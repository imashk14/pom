use anyhow::Result;
use clap::Parser;

/// Top-level parsed CLI command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Sync / install / sysupgrade (`-S`, `-Sy`, `-Su`, `-Syu`, or default no-args).
    Sync(SyncArgs),
    /// Search official repositories and AUR (`-Ss`, `-Ssq`).
    Search(SearchArgs),
    /// Remove installed packages (`-R`, `-Rs`, `-Rns`, etc.).
    Remove(RemoveArgs),
    /// Forward raw arguments directly to `pacman`.
    Passthrough {
        raw_args: Vec<String>,
        needs_root: bool,
    },
}

/// Arguments for `-S` / `--sync` operations (excluding search and passthrough-only modes).
#[derive(Debug, Clone, Default, PartialEq, Eq, Parser)]
pub struct SyncArgs {
    /// Refresh package databases (-y, -yy)
    #[arg(short = 'y', long = "refresh", action = clap::ArgAction::Count)]
    pub refresh: u8,
    /// Upgrade installed packages (-u, -uu)
    #[arg(short = 'u', long = "sysupgrade", action = clap::ArgAction::Count)]
    pub sysupgrade: u8,
    /// Skip up-to-date targets
    #[arg(long = "needed")]
    pub needed: bool,
    /// Do not ask for any confirmation (also skips PKGBUILD diff review)
    #[arg(long = "noconfirm")]
    pub noconfirm: bool,
    /// Install packages as dependencies
    #[arg(long = "asdeps")]
    pub asdeps: bool,
    /// Install packages as explicitly installed
    #[arg(long = "asexplicit")]
    pub asexplicit: bool,
    /// Only operate on AUR targets
    #[arg(short = 'a', long = "aur")]
    pub aur_only: bool,
    /// Only operate on official repository targets
    #[arg(long = "repo")]
    pub repo_only: bool,
    /// Ignore a package upgrade (can be comma-separated)
    #[arg(long = "ignore", value_delimiter = ',')]
    pub ignore: Vec<String>,
    /// Target packages (e.g. "ripgrep", "extra/fd", "aur/yay-bin")
    pub targets: Vec<String>,
}

/// Arguments for `-Ss` / `--sync --search` operations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchArgs {
    pub quiet: bool,
    pub aur_only: bool,
    pub repo_only: bool,
    pub terms: Vec<String>,
}

/// Arguments for `-R` / `--remove` operations (excluding print/help passthrough modes).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoveArgs {
    pub noconfirm: bool,
    pub targets: Vec<String>,
    pub raw_args: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TopOp {
    Sync,
    Query,
    Remove,
    Upgrade,
    Database,
    Files,
    Deptest,
    Version,
    Help,
}

#[derive(Debug, Default)]
struct FlagSummary {
    op: Option<TopOp>,
    has_help: bool,
    has_refresh: bool,
    has_sysupgrade: bool,
    has_search: bool,
    has_quiet: bool,
    has_aur: bool,
    has_repo: bool,
    has_info: bool,
    has_list: bool,
    has_groups: bool,
    has_clean: bool,
    has_print: bool,
    has_downloadonly: bool,
    has_noconfirm: bool,
    positionals: Vec<String>,
}

fn long_flag_takes_value(flag: &str) -> bool {
    matches!(
        flag,
        "--ignore"
            | "--ignoregroup"
            | "--dbpath"
            | "--root"
            | "--sysroot"
            | "--config"
            | "--arch"
            | "--cachedir"
            | "--hookdir"
            | "--gpgdir"
            | "--logfile"
            | "--color"
            | "--assume-installed"
            | "--overwrite"
    )
}

fn short_flag_takes_value(ch: char) -> bool {
    matches!(ch, 'b' | 'r')
}

fn scan_flags(raw_args: &[String]) -> FlagSummary {
    let mut summary = FlagSummary::default();
    let mut after_double_dash = false;
    let mut skip_next_value = false;

    for arg in raw_args {
        if skip_next_value {
            skip_next_value = false;
            continue;
        }

        if after_double_dash {
            summary.positionals.push(arg.clone());
            continue;
        }

        if arg == "--" {
            after_double_dash = true;
            continue;
        }

        if let Some(long) = arg.strip_prefix("--") {
            let flag_name = long.split('=').next().unwrap_or(long);
            let full_flag = format!("--{flag_name}");
            if !long.contains('=') && long_flag_takes_value(&full_flag) {
                skip_next_value = true;
            }

            match flag_name {
                "sync" => summary.op.get_or_insert(TopOp::Sync),
                "query" => summary.op.get_or_insert(TopOp::Query),
                "remove" => summary.op.get_or_insert(TopOp::Remove),
                "upgrade" => summary.op.get_or_insert(TopOp::Upgrade),
                "database" => summary.op.get_or_insert(TopOp::Database),
                "files" => summary.op.get_or_insert(TopOp::Files),
                "deptest" => summary.op.get_or_insert(TopOp::Deptest),
                "version" => summary.op.get_or_insert(TopOp::Version),
                "help" => {
                    summary.has_help = true;
                    summary.op.get_or_insert(TopOp::Help)
                }
                "refresh" => {
                    summary.has_refresh = true;
                    continue;
                }
                "sysupgrade" => {
                    summary.has_sysupgrade = true;
                    continue;
                }
                "search" => {
                    summary.has_search = true;
                    continue;
                }
                "quiet" => {
                    summary.has_quiet = true;
                    continue;
                }
                "aur" => {
                    summary.has_aur = true;
                    continue;
                }
                "repo" => {
                    summary.has_repo = true;
                    continue;
                }
                "info" => {
                    summary.has_info = true;
                    continue;
                }
                "list" => {
                    summary.has_list = true;
                    continue;
                }
                "groups" => {
                    summary.has_groups = true;
                    continue;
                }
                "clean" => {
                    summary.has_clean = true;
                    continue;
                }
                "print" => {
                    summary.has_print = true;
                    continue;
                }
                "downloadonly" => {
                    summary.has_downloadonly = true;
                    continue;
                }
                "noconfirm" => {
                    summary.has_noconfirm = true;
                    continue;
                }
                _ => continue,
            };
            continue;
        }

        if arg.starts_with('-') && arg.len() > 1 {
            let chars: Vec<char> = arg[1..].chars().collect();
            for (idx, &ch) in chars.iter().enumerate() {
                if idx == chars.len() - 1 && short_flag_takes_value(ch) {
                    skip_next_value = true;
                }
                match ch {
                    'S' => {
                        summary.op.get_or_insert(TopOp::Sync);
                    }
                    'Q' => {
                        summary.op.get_or_insert(TopOp::Query);
                    }
                    'R' => {
                        summary.op.get_or_insert(TopOp::Remove);
                    }
                    'U' => {
                        summary.op.get_or_insert(TopOp::Upgrade);
                    }
                    'D' => {
                        summary.op.get_or_insert(TopOp::Database);
                    }
                    'F' => {
                        summary.op.get_or_insert(TopOp::Files);
                    }
                    'T' => {
                        summary.op.get_or_insert(TopOp::Deptest);
                    }
                    'V' => {
                        summary.op.get_or_insert(TopOp::Version);
                    }
                    'h' => {
                        summary.has_help = true;
                        summary.op.get_or_insert(TopOp::Help);
                    }
                    'y' => summary.has_refresh = true,
                    'u' => summary.has_sysupgrade = true,
                    's' => summary.has_search = true,
                    'q' => summary.has_quiet = true,
                    'a' => summary.has_aur = true,
                    'i' => summary.has_info = true,
                    'l' => summary.has_list = true,
                    'g' => summary.has_groups = true,
                    'c' => summary.has_clean = true,
                    'p' => summary.has_print = true,
                    'w' => summary.has_downloadonly = true,
                    _ => {}
                }
            }
            continue;
        }

        summary.positionals.push(arg.clone());
    }

    summary
}

/// Parses CLI arguments (`argv`, including `argv[0]`) into a `Command`.
///
/// Defaults to `-Syu` when invoked with no arguments.
pub fn parse_cli<I, S>(argv: I) -> Result<Command>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut raw_args: Vec<String> = argv
        .into_iter()
        .skip(1)
        .map(|s| s.as_ref().to_string())
        .collect();

    if raw_args.is_empty() {
        raw_args.push("-Syu".to_string());
    }

    let summary = scan_flags(&raw_args);

    match summary.op {
        Some(TopOp::Sync) => {
            if summary.has_help
                || summary.has_info
                || summary.has_list
                || summary.has_groups
                || summary.has_clean
                || summary.has_print
                || summary.has_downloadonly
            {
                let needs_root = operation_needs_root(&raw_args);
                return Ok(Command::Passthrough {
                    raw_args,
                    needs_root,
                });
            }

            if summary.has_search {
                return Ok(Command::Search(SearchArgs {
                    quiet: summary.has_quiet,
                    aur_only: summary.has_aur,
                    repo_only: summary.has_repo,
                    terms: summary.positionals,
                }));
            }

            let sync_args = parse_sync_args(&raw_args)?;
            Ok(Command::Sync(sync_args))
        }
        Some(TopOp::Remove) => {
            if summary.has_help || summary.has_print {
                let needs_root = operation_needs_root(&raw_args);
                return Ok(Command::Passthrough {
                    raw_args,
                    needs_root,
                });
            }

            Ok(Command::Remove(RemoveArgs {
                noconfirm: summary.has_noconfirm,
                targets: summary.positionals,
                raw_args,
            }))
        }
        _ => {
            let needs_root = operation_needs_root(&raw_args);
            Ok(Command::Passthrough {
                raw_args,
                needs_root,
            })
        }
    }
}

fn parse_sync_args(raw_args: &[String]) -> Result<SyncArgs> {
    let mut clap_args = Vec::with_capacity(raw_args.len() + 1);
    clap_args.push("pom".to_string());

    let mut after_double_dash = false;
    for arg in raw_args {
        if after_double_dash {
            clap_args.push(arg.clone());
            continue;
        }
        if arg == "--" {
            after_double_dash = true;
            clap_args.push(arg.clone());
            continue;
        }
        if arg == "--sync" {
            continue;
        }
        if arg.starts_with('-') && !arg.starts_with("--") && arg.len() > 1 {
            let remaining: String = arg[1..].chars().filter(|&c| c != 'S').collect();
            if !remaining.is_empty() {
                clap_args.push(format!("-{remaining}"));
            }
            continue;
        }
        clap_args.push(arg.clone());
    }

    SyncArgs::try_parse_from(clap_args).map_err(anyhow::Error::from)
}

/// Returns `true` if the `pacman` operation represented by `raw_args` (excluding `argv[0]`)
/// modifies system state and therefore requires root privileges.
pub fn operation_needs_root(raw_args: &[String]) -> bool {
    let summary = scan_flags(raw_args);

    if summary.has_help {
        return false;
    }

    match summary.op {
        Some(TopOp::Remove) => !summary.has_print,
        Some(TopOp::Upgrade) | Some(TopOp::Database) => true,
        Some(TopOp::Files) => summary.has_refresh,
        Some(TopOp::Query) | Some(TopOp::Deptest) | Some(TopOp::Version) | Some(TopOp::Help)
        | None => false,
        Some(TopOp::Sync) => {
            if summary.has_search
                || summary.has_info
                || summary.has_list
                || summary.has_groups
                || summary.has_print
            {
                return false;
            }
            summary.has_clean
                || summary.has_refresh
                || summary.has_sysupgrade
                || summary.has_downloadonly
                || !summary.positionals.is_empty()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(slice: &[&str]) -> Vec<String> {
        slice.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn default_no_args_is_syu() {
        let cmd = parse_cli(["pom"]).unwrap();
        assert_eq!(
            cmd,
            Command::Sync(SyncArgs {
                refresh: 1,
                sysupgrade: 1,
                ..Default::default()
            })
        );
    }

    #[test]
    fn parse_syu_and_syyu() {
        assert_eq!(
            parse_cli(["pom", "-Syu"]).unwrap(),
            Command::Sync(SyncArgs {
                refresh: 1,
                sysupgrade: 1,
                ..Default::default()
            })
        );

        assert_eq!(
            parse_cli(["pom", "-Syyu"]).unwrap(),
            Command::Sync(SyncArgs {
                refresh: 2,
                sysupgrade: 1,
                ..Default::default()
            })
        );

        assert_eq!(
            parse_cli(["pom", "--sync", "--refresh", "--refresh", "--sysupgrade"]).unwrap(),
            Command::Sync(SyncArgs {
                refresh: 2,
                sysupgrade: 1,
                ..Default::default()
            })
        );
    }

    #[test]
    fn parse_sync_with_flags_and_targets() {
        let cmd = parse_cli([
            "pom",
            "-S",
            "--needed",
            "--noconfirm",
            "--asdeps",
            "--ignore",
            "linux,linux-headers",
            "yay-bin",
            "extra/fd",
        ])
        .unwrap();

        assert_eq!(
            cmd,
            Command::Sync(SyncArgs {
                needed: true,
                noconfirm: true,
                asdeps: true,
                ignore: vec!["linux".to_string(), "linux-headers".to_string()],
                targets: vec!["yay-bin".to_string(), "extra/fd".to_string()],
                ..Default::default()
            })
        );
    }

    #[test]
    fn parse_sync_aur_and_repo_only() {
        let aur = parse_cli(["pom", "-Sayu", "paru-bin"]).unwrap();
        assert_eq!(
            aur,
            Command::Sync(SyncArgs {
                refresh: 1,
                sysupgrade: 1,
                aur_only: true,
                targets: vec!["paru-bin".to_string()],
                ..Default::default()
            })
        );

        let repo = parse_cli(["pom", "-Syu", "--repo"]).unwrap();
        assert_eq!(
            repo,
            Command::Sync(SyncArgs {
                refresh: 1,
                sysupgrade: 1,
                repo_only: true,
                ..Default::default()
            })
        );
    }

    #[test]
    fn parse_search_ss_and_ssq() {
        assert_eq!(
            parse_cli(["pom", "-Ss", "ripgrep", "fd"]).unwrap(),
            Command::Search(SearchArgs {
                quiet: false,
                aur_only: false,
                repo_only: false,
                terms: vec!["ripgrep".to_string(), "fd".to_string()],
            })
        );

        assert_eq!(
            parse_cli(["pom", "-Ssq", "--aur", "yay"]).unwrap(),
            Command::Search(SearchArgs {
                quiet: true,
                aur_only: true,
                repo_only: false,
                terms: vec!["yay".to_string()],
            })
        );
    }

    #[test]
    fn parse_remove_modes() {
        assert_eq!(
            parse_cli(["pom", "-R", "gedit"]).unwrap(),
            Command::Remove(RemoveArgs {
                noconfirm: false,
                targets: args(&["gedit"]),
                raw_args: args(&["-R", "gedit"]),
            })
        );

        assert_eq!(
            parse_cli(["pom", "-Rns", "foo", "bar"]).unwrap(),
            Command::Remove(RemoveArgs {
                noconfirm: false,
                targets: args(&["foo", "bar"]),
                raw_args: args(&["-Rns", "foo", "bar"]),
            })
        );

        assert_eq!(
            parse_cli(["pom", "-R", "--noconfirm", "foo"]).unwrap(),
            Command::Remove(RemoveArgs {
                noconfirm: true,
                targets: args(&["foo"]),
                raw_args: args(&["-R", "--noconfirm", "foo"]),
            })
        );

        assert_eq!(
            parse_cli(["pom", "-Rp", "foo"]).unwrap(),
            Command::Passthrough {
                raw_args: args(&["-Rp", "foo"]),
                needs_root: false,
            }
        );
    }

    #[test]
    fn parse_passthrough_modes() {
        assert_eq!(
            parse_cli(["pom", "-Qi", "pacman"]).unwrap(),
            Command::Passthrough {
                raw_args: args(&["-Qi", "pacman"]),
                needs_root: false,
            }
        );

        assert_eq!(
            parse_cli(["pom", "-Rp", "foo"]).unwrap(),
            Command::Passthrough {
                raw_args: args(&["-Rp", "foo"]),
                needs_root: false,
            }
        );

        assert_eq!(
            parse_cli(["pom", "-Rh"]).unwrap(),
            Command::Passthrough {
                raw_args: args(&["-Rh"]),
                needs_root: false,
            }
        );

        assert_eq!(
            parse_cli(["pom", "-Si", "ripgrep"]).unwrap(),
            Command::Passthrough {
                raw_args: args(&["-Si", "ripgrep"]),
                needs_root: false,
            }
        );

        assert_eq!(
            parse_cli(["pom", "-Sc"]).unwrap(),
            Command::Passthrough {
                raw_args: args(&["-Sc"]),
                needs_root: true,
            }
        );

        assert_eq!(
            parse_cli(["pom", "-Sl", "core"]).unwrap(),
            Command::Passthrough {
                raw_args: args(&["-Sl", "core"]),
                needs_root: false,
            }
        );

        assert_eq!(
            parse_cli(["pom", "-Sp", "pacman"]).unwrap(),
            Command::Passthrough {
                raw_args: args(&["-Sp", "pacman"]),
                needs_root: false,
            }
        );

        assert_eq!(
            parse_cli(["pom", "-Sw", "pacman"]).unwrap(),
            Command::Passthrough {
                raw_args: args(&["-Sw", "pacman"]),
                needs_root: true,
            }
        );
    }

    #[test]
    fn operation_needs_root_classification() {
        // Mutating operations -> true
        assert!(operation_needs_root(&args(&["-Rns", "foo"])));
        assert!(operation_needs_root(&args(&["-U", "pkg.tar.zst"])));
        assert!(operation_needs_root(&args(&["-D", "--asexplicit", "foo"])));
        assert!(operation_needs_root(&args(&["-Fy"])));
        assert!(operation_needs_root(&args(&["-Sc"])));
        assert!(operation_needs_root(&args(&["-Scc"])));
        assert!(operation_needs_root(&args(&["-Sy"])));
        assert!(operation_needs_root(&args(&["-Su"])));
        assert!(operation_needs_root(&args(&["-Syu"])));
        assert!(operation_needs_root(&args(&["-S", "ripgrep"])));
        assert!(operation_needs_root(&args(&["-Sw", "ripgrep"])));

        // Read-only operations -> false
        assert!(!operation_needs_root(&args(&["-Q"])));
        assert!(!operation_needs_root(&args(&["-Qi", "pacman"])));
        assert!(!operation_needs_root(&args(&["-Qs", "rust"])));
        assert!(!operation_needs_root(&args(&["-T", "glibc"])));
        assert!(!operation_needs_root(&args(&["-V"])));
        assert!(!operation_needs_root(&args(&["-h"])));
        assert!(!operation_needs_root(&args(&["--help"])));
        assert!(!operation_needs_root(&args(&["-Sh"])));
        assert!(!operation_needs_root(&args(&["-Ss", "ripgrep"])));
        assert!(!operation_needs_root(&args(&["-Si", "ripgrep"])));
        assert!(!operation_needs_root(&args(&["-Sl", "extra"])));
        assert!(!operation_needs_root(&args(&["-Sg", "base-devel"])));
        assert!(!operation_needs_root(&args(&["-Sp", "ripgrep"])));
        assert!(!operation_needs_root(&args(&["-Fl", "pacman"])));
        assert!(!operation_needs_root(&args(&["-Fs", "libalpm.so"])));
    }
}
