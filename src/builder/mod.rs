//! Transaction build pipeline orchestrator and privileged `pacman` handoff.

pub mod makepkg;
pub mod pacman;

use tokio::sync::mpsc;

use crate::aur::git::SyncedRepo;
use crate::aur::resolver::{InstallRole, TransactionPlan};
use crate::ui::app::TuiHandle;
use makepkg::{BuildOptions, BuildPhase, MakepkgError, MakepkgEvent};
use pacman::PacmanError;

/// Options controlling transaction execution behavior.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunOptions {
    pub noconfirm: bool,
    pub asdeps: bool,
    pub asexplicit: bool,
}

/// Errors produced while executing a resolved [`TransactionPlan`].
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error(transparent)]
    Pacman(#[from] PacmanError),

    #[error(transparent)]
    Makepkg(#[from] MakepkgError),

    #[error("missing synchronized AUR repository for pkgbase '{0}'")]
    MissingSyncedRepo(String),
}

/// Executes a resolved [`TransactionPlan`]:
/// 1. Validates `sudo` credentials and spawns a 50-second keepalive task.
/// 2. Installs official repository dependencies/targets via `pacman -S --needed`.
/// 3. Builds each AUR `pkgbase` in topological order with `makepkg` and installs
///    resulting `.pkg.tar.*` archives via `pacman -U` with TTY handoff.
pub async fn run_transaction(
    plan: &TransactionPlan,
    synced_repos: &[SyncedRepo],
    tui: &TuiHandle,
    opts: &RunOptions,
) -> Result<(), BuildError> {
    if plan.repo_packages.is_empty() && plan.aur_bases.is_empty() {
        return Ok(());
    }

    for base in &plan.aur_bases {
        tui.register_base(&base.pkgbase, &base.version);
    }

    let escalation = pacman::detect_escalation()?;
    pacman::sudo_validate(&escalation, Some(tui)).await?;
    let _keepalive = pacman::spawn_keepalive(&escalation);

    if !plan.repo_packages.is_empty() {
        tui.set_header("Installing repository packages");
        let dep_targets: Vec<String> = plan
            .repo_packages
            .iter()
            .filter(|p| p.role != InstallRole::ExplicitTarget)
            .map(|p| p.name.clone())
            .collect();
        let explicit_targets: Vec<String> = plan
            .repo_packages
            .iter()
            .filter(|p| p.role == InstallRole::ExplicitTarget)
            .map(|p| p.name.clone())
            .collect();

        // Keep pacman interactive when conflicts exist so the user can resolve them;
        // otherwise pass --noconfirm since the user already confirmed pom's summary.
        let repo_noconfirm = plan.conflicts.is_empty();

        if !dep_targets.is_empty() {
            pacman::install_repo(&escalation, &dep_targets, true, repo_noconfirm, Some(tui))
                .await?;
        }
        if !explicit_targets.is_empty() {
            pacman::install_repo(
                &escalation,
                &explicit_targets,
                opts.asdeps,
                repo_noconfirm,
                Some(tui),
            )
            .await?;
        }
    }

    if !plan.aur_bases.is_empty() {
        tui.set_header("Building AUR packages");
    }

    let install_noconfirm = plan.conflicts.is_empty();
    for base in &plan.aur_bases {
        let synced = synced_repos
            .iter()
            .find(|r| r.pkgbase == base.pkgbase)
            .ok_or_else(|| BuildError::MissingSyncedRepo(base.pkgbase.clone()))?;

        let build_opts = BuildOptions {
            target_packages: base.packages.iter().map(|p| p.name.clone()).collect(),
            log_dir: None,
        };

        let (mk_tx, mut mk_rx) = mpsc::unbounded_channel::<MakepkgEvent>();
        let tui_clone = tui.clone();
        let forwarder = tokio::spawn(async move {
            while let Some(ev) = mk_rx.recv().await {
                match ev {
                    MakepkgEvent::PhaseChanged { pkgbase, phase } => {
                        tui_clone.phase_changed(pkgbase, phase);
                    }
                    MakepkgEvent::LogLine { pkgbase, line } => {
                        tui_clone.log_line(pkgbase, line);
                    }
                }
            }
        });

        let build_res =
            makepkg::build_package_base(&base.pkgbase, &synced.dir, &build_opts, Some(&mk_tx))
                .await;
        drop(mk_tx);
        let _ = forwarder.await;

        let build_res = match build_res {
            Ok(res) => res,
            Err(err) => {
                tui.phase_changed(&base.pkgbase, BuildPhase::Failed);
                return Err(BuildError::Makepkg(err));
            }
        };

        tui.phase_changed(&base.pkgbase, BuildPhase::Installing);

        let install_asdeps = base.make_only || !base.is_explicit_target || opts.asdeps;
        let install_asexplicit = !install_asdeps && (opts.asexplicit || base.is_explicit_target);

        if let Err(err) = pacman::install_files(
            &escalation,
            &build_res.packages,
            install_asdeps,
            install_asexplicit,
            install_noconfirm,
            Some(tui),
        )
        .await
        {
            tui.phase_changed(&base.pkgbase, BuildPhase::Failed);
            return Err(BuildError::Pacman(err));
        }

        tui.phase_changed(&base.pkgbase, BuildPhase::Done);
    }

    Ok(())
}

/// Executes a package removal transaction via `pacman`:
/// 1. Validates `sudo` credentials and spawns a 50-second keepalive task.
/// 2. Streams `(N/M) removing <pkg>...` and post-transaction hooks into the TUI.
pub async fn run_removal(
    raw_args: &[String],
    noconfirm: bool,
    tui: &TuiHandle,
) -> Result<(), BuildError> {
    let _ = noconfirm;
    let escalation = pacman::detect_escalation()?;
    pacman::sudo_validate(&escalation, Some(tui)).await?;
    let _keepalive = pacman::spawn_keepalive(&escalation);

    tui.set_header("Removing packages");
    pacman::remove_packages(&escalation, raw_args, true, Some(tui)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_role_flags_match_expected_defaults() {
        let opts = RunOptions::default();
        assert!(!opts.noconfirm);
        assert!(!opts.asdeps);
        assert!(!opts.asexplicit);
    }
}
