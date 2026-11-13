mod alpm_ctx;
mod aur;
mod builder;
mod cli;
mod ui;

use anyhow::{Context, Result, bail};
use tokio::sync::mpsc;

use crate::alpm_ctx::{AlpmCtx, PackageDatabase};
use crate::aur::git::{
    AurRepoTarget, GitSyncProgress, ReviewUiSuspender, collect_unreviewed_diffs,
    present_pkgbuild_review_with_tui, sync_aur_repos,
};
use crate::aur::resolver::{ResolveOptions, TransactionPlan, resolve_install, resolve_sysupgrade};
use crate::builder::makepkg::{BuildPhase, MakepkgError};
use crate::builder::{BuildError, RunOptions};
use crate::cli::{Command, RemoveArgs, SearchArgs, SyncArgs};
use crate::ui::app::{SuspendGuard, TuiHandle};
use crate::ui::summary::RemovalPkg;

struct TuiSuspender<'a>(&'a TuiHandle);

#[async_trait::async_trait]
impl ReviewUiSuspender for TuiSuspender<'_> {
    type Guard = SuspendGuard;

    async fn suspend(&self) -> Self::Guard {
        self.0.suspend().await
    }
}

fn install_terminal_guards() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::terminal::disable_raw_mode();
        default_hook(info);
    }));

    tokio::spawn(async {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = crossterm::terminal::disable_raw_mode();
            std::process::exit(130);
        }
    });
}

#[tokio::main]
async fn main() -> Result<()> {
    install_terminal_guards();
    builder::pacman::ensure_not_root()?;

    match cli::parse_cli(std::env::args())? {
        Command::Passthrough {
            raw_args,
            needs_root,
        } => {
            let status = builder::pacman::passthrough(&raw_args, needs_root).await?;
            if !status.success() {
                std::process::exit(status.code().unwrap_or(1));
            }
            Ok(())
        }
        Command::Search(args) => run_search(args).await,
        Command::Sync(args) => run_sync(args).await,
        Command::Remove(args) => run_remove(args).await,
    }
}

async fn run_search(args: SearchArgs) -> Result<()> {
    let db = AlpmCtx::from_pacman_conf().context("failed to initialize libalpm")?;
    let raur = aur::rpc::new_handle();

    let repo_hits = if args.aur_only {
        Vec::new()
    } else {
        db.search_sync(&args.terms)
    };

    let aur_hits = if args.repo_only {
        Vec::new()
    } else {
        aur::rpc::search_aur(&raur, &db, &args.terms).await?
    };

    ui::summary::print_search_results(&repo_hits, &aur_hits, args.quiet);
    Ok(())
}

async fn run_sync(args: SyncArgs) -> Result<()> {
    if args.refresh > 0 {
        let escalation = builder::pacman::detect_escalation()?;
        builder::pacman::refresh(&escalation, args.refresh, args.noconfirm, None).await?;
    }

    if args.sysupgrade == 0 && args.targets.is_empty() {
        return Ok(());
    }

    let ctx = AlpmCtx::from_pacman_conf().context("failed to initialize libalpm")?;
    let raur = aur::rpc::new_handle();
    let resolve_opts = ResolveOptions {
        needed: args.needed,
        aur_only: args.aur_only,
        repo_only: args.repo_only,
        allow_downgrade: args.sysupgrade > 1,
        ignore: args.ignore.clone(),
    };

    let repo_upgrades = if args.sysupgrade > 0 && !resolve_opts.aur_only {
        ctx.repo_upgrades(&resolve_opts.ignore, resolve_opts.allow_downgrade)
    } else {
        Vec::new()
    };

    let plan = {
        let alpm_guard = ctx.alpm();
        match (args.sysupgrade > 0, args.targets.is_empty()) {
            (true, true) => {
                resolve_sysupgrade(&alpm_guard, &repo_upgrades, &raur, &resolve_opts).await?
            }
            (false, false) => {
                resolve_install(&alpm_guard, &raur, &args.targets, &resolve_opts).await?
            }
            (true, false) => {
                let mut sys_plan =
                    resolve_sysupgrade(&alpm_guard, &repo_upgrades, &raur, &resolve_opts).await?;
                let inst_plan =
                    resolve_install(&alpm_guard, &raur, &args.targets, &resolve_opts).await?;
                merge_plans(&mut sys_plan, inst_plan);
                sys_plan
            }
            (false, true) => unreachable!(),
        }
    };

    if !plan.missing.is_empty() {
        for missing in &plan.missing {
            eprintln!(
                "error: could not satisfy dependency '{}' (required by: {})",
                missing.dep,
                missing.dep_chain.join(" -> ")
            );
        }
        bail!("missing dependencies prevented transaction resolution");
    }

    if plan.repo_packages.is_empty() && plan.aur_bases.is_empty() {
        println!(" there is nothing to do");
        return Ok(());
    }

    ui::summary::print_summary(&plan);
    if !ui::summary::confirm_prompt("Proceed with transaction?", true, args.noconfirm)? {
        println!("Aborted.");
        return Ok(());
    }

    let (tui, ui_task) = ui::app::spawn_ui();
    for base in &plan.aur_bases {
        tui.register_base(&base.pkgbase, &base.version);
    }

    let synced_repos = if plan.aur_bases.is_empty() {
        Vec::new()
    } else {
        let targets: Vec<AurRepoTarget> = plan
            .aur_bases
            .iter()
            .map(|b| AurRepoTarget::new(&b.pkgbase))
            .collect();

        let (git_tx, mut git_rx) = mpsc::unbounded_channel::<GitSyncProgress>();
        let tui_clone = tui.clone();
        let git_fwd = tokio::spawn(async move {
            while let Some(ev) = git_rx.recv().await {
                match ev {
                    GitSyncProgress::Cloning { pkgbase } => {
                        tui_clone.phase_changed(pkgbase, BuildPhase::Cloning);
                    }
                    GitSyncProgress::Synced { .. } => {}
                    GitSyncProgress::Failed { pkgbase, error } => {
                        tui_clone.log_line(&pkgbase, error);
                        tui_clone.phase_changed(pkgbase, BuildPhase::Failed);
                    }
                }
            }
        });

        let sync_res = sync_aur_repos(&targets, Some(&git_tx)).await;
        drop(git_tx);
        let _ = git_fwd.await;

        let repos = match sync_res {
            Ok(repos) => repos,
            Err(err) => {
                tui.shutdown();
                let _ = ui_task.await;
                return Err(err.into());
            }
        };

        let diffs = match collect_unreviewed_diffs(&repos).await {
            Ok(diffs) => diffs,
            Err(err) => {
                tui.shutdown();
                let _ = ui_task.await;
                return Err(err.into());
            }
        };

        let approved =
            match present_pkgbuild_review_with_tui(&diffs, args.noconfirm, &TuiSuspender(&tui))
                .await
            {
                Ok(approved) => approved,
                Err(err) => {
                    tui.shutdown();
                    let _ = ui_task.await;
                    return Err(err.into());
                }
            };

        if !approved {
            tui.shutdown();
            let _ = ui_task.await;
            println!("Aborted.");
            return Ok(());
        }

        repos
    };

    let run_opts = RunOptions {
        noconfirm: args.noconfirm,
        asdeps: args.asdeps,
        asexplicit: args.asexplicit,
    };
    let tx_res = builder::run_transaction(&plan, &synced_repos, &tui, &run_opts).await;

    tui.shutdown();
    let _ = ui_task.await;

    match tx_res {
        Ok(()) => Ok(()),
        Err(BuildError::Makepkg(MakepkgError::BuildFailed {
            pkgbase,
            exit_code,
            log_path,
            full_log,
            ..
        })) => {
            eprintln!("\n==> Build failed for {pkgbase} (exit code: {exit_code:?})");
            if !full_log.is_empty() {
                eprintln!("{full_log}");
            }
            eprintln!("==> Full build log saved at: {}", log_path.display());
            std::process::exit(exit_code.unwrap_or(1));
        }
        Err(err) => Err(err.into()),
    }
}

fn merge_plans(base: &mut TransactionPlan, extra: TransactionPlan) {
    for pkg in extra.repo_packages {
        if !base.repo_packages.iter().any(|p| p.name == pkg.name) {
            base.repo_packages.push(pkg);
        }
    }
    for aur_base in extra.aur_bases {
        if !base.aur_bases.iter().any(|b| b.pkgbase == aur_base.pkgbase) {
            base.aur_bases.push(aur_base);
        }
    }
    base.unneeded.extend(extra.unneeded);
    base.missing.extend(extra.missing);
    base.conflicts.extend(extra.conflicts);
}

async fn run_remove(args: RemoveArgs) -> Result<()> {
    let mut preview_args = vec![
        "--print".to_string(),
        "--print-format".to_string(),
        "%n\t%v\t%s".to_string(),
    ];
    preview_args.extend(args.raw_args.iter().cloned());

    let output = tokio::process::Command::new("pacman")
        .env("LC_ALL", "C")
        .args(&preview_args)
        .output()
        .await
        .context("failed to run pacman --print for removal preview")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let msg = stderr.trim();
        if msg.is_empty() {
            bail!(
                "pacman removal preview failed with exit status {}",
                output.status
            );
        }
        bail!("{msg}");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let removals = parse_removal_preview(&stdout);
    if removals.is_empty() {
        println!("Nothing to do.");
        return Ok(());
    }

    ui::summary::print_removal_summary(&removals);
    if !ui::summary::confirm_prompt("Proceed with removal?", true, args.noconfirm)? {
        println!("Aborted.");
        return Ok(());
    }

    let (tui, ui_task) = ui::app::spawn_ui();
    let res = builder::run_removal(&args.raw_args, args.noconfirm, &tui).await;
    tui.shutdown();
    let _ = ui_task.await;
    res?;
    Ok(())
}

fn parse_removal_preview(output: &str) -> Vec<RemovalPkg> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let mut parts = line.splitn(3, '\t');
            let name = parts.next()?.trim();
            let version = parts.next()?.trim();
            if name.is_empty() || version.is_empty() {
                return None;
            }
            let installed_size = parts
                .next()
                .and_then(|s| s.trim().parse::<i64>().ok())
                .unwrap_or(0);
            Some(RemovalPkg {
                name: name.to_string(),
                version: version.to_string(),
                installed_size,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_removal_preview_parses_tab_delimited_lines() {
        let input = "paru\t2.2.0-1\t8518637\n\nfoo-lib\t1.0.0-2\t1024\n";
        let parsed = parse_removal_preview(input);
        assert_eq!(
            parsed,
            vec![
                RemovalPkg {
                    name: "paru".to_string(),
                    version: "2.2.0-1".to_string(),
                    installed_size: 8518637,
                },
                RemovalPkg {
                    name: "foo-lib".to_string(),
                    version: "1.0.0-2".to_string(),
                    installed_size: 1024,
                },
            ]
        );
    }

    #[test]
    fn parse_removal_preview_handles_empty_and_malformed_lines() {
        assert!(parse_removal_preview("").is_empty());
        assert!(parse_removal_preview("\n   \n").is_empty());

        let parsed = parse_removal_preview("pkg-a\t1.0-1\tnot-a-number\nbadline\n");
        assert_eq!(
            parsed,
            vec![RemovalPkg {
                name: "pkg-a".to_string(),
                version: "1.0-1".to_string(),
                installed_size: 0,
            }]
        );
    }
}
