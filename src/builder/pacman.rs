use std::ffi::{OsStr, OsString};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};

use nix::unistd::Uid;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Duration, interval};

use crate::ui::app::TuiHandle;

/// Interval between `sudo -n -v` credential keepalive refreshes.
const SUDO_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(50);

/// Errors returned by privilege-escalation, root-guard, and `pacman` operations.
#[derive(Debug, Error)]
pub enum PacmanError {
    #[error(
        "refusing to run as root (euid 0); run pom as a normal user — it will invoke sudo/doas when needed"
    )]
    RunningAsRoot,

    #[error("no privilege escalation tool found (tried POM_SUDO, sudo, doas)")]
    NoEscalationTool,

    #[error("configured escalation tool '{0}' was not found or is not executable")]
    EscalationToolNotFound(String),

    #[error("failed to spawn '{program}': {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },

    #[error("'{command}' exited with status {code:?}")]
    CommandFailed { command: String, code: Option<i32> },

    #[error("sudo credential validation failed (exit status {code:?})")]
    SudoValidationFailed { code: Option<i32> },
}

/// Privilege escalation binary used for mutating `pacman` operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Escalation {
    Sudo,
    Doas,
    Custom(String),
}

impl Escalation {
    /// Returns the executable name or path to invoke.
    pub fn program(&self) -> &str {
        match self {
            Self::Sudo => "sudo",
            Self::Doas => "doas",
            Self::Custom(cmd) => cmd.as_str(),
        }
    }

    /// Returns `true` if this escalation tool supports `sudo -v` / `sudo -n -v`
    /// credential caching.
    pub fn supports_keepalive(&self) -> bool {
        match self {
            Self::Sudo => true,
            Self::Doas => false,
            Self::Custom(cmd) => Path::new(cmd).file_name() == Some(OsStr::new("sudo")),
        }
    }
}

/// Ensures `pom` was not started as `root` (e.g. via `sudo pom ...`).
///
/// Running `makepkg`, `git`, or AUR network calls as root is forbidden.
pub fn ensure_not_root() -> Result<(), PacmanError> {
    ensure_not_root_for_uid(nix::unistd::geteuid())
}

/// Testable helper for [`ensure_not_root`] taking an explicit effective UID.
pub fn ensure_not_root_for_uid(euid: Uid) -> Result<(), PacmanError> {
    if euid.is_root() {
        Err(PacmanError::RunningAsRoot)
    } else {
        Ok(())
    }
}

/// Detects the privilege escalation binary to use.
///
/// Checks `POM_SUDO` first, then falls back to `sudo` and `doas` in `PATH`.
pub fn detect_escalation() -> Result<Escalation, PacmanError> {
    let pom_sudo = std::env::var("POM_SUDO").ok();
    let path_var = std::env::var_os("PATH");
    detect_escalation_with(pom_sudo.as_deref(), |cmd| {
        command_exists_in_path(cmd, path_var.as_deref())
    })
}

/// Testable helper for [`detect_escalation`] with injected `POM_SUDO` and
/// executable lookup.
pub fn detect_escalation_with<F>(
    pom_sudo: Option<&str>,
    mut exists: F,
) -> Result<Escalation, PacmanError>
where
    F: FnMut(&str) -> bool,
{
    if let Some(override_cmd) = pom_sudo.map(str::trim).filter(|s| !s.is_empty()) {
        if !exists(override_cmd) {
            return Err(PacmanError::EscalationToolNotFound(
                override_cmd.to_string(),
            ));
        }
        return Ok(match override_cmd {
            "sudo" => Escalation::Sudo,
            "doas" => Escalation::Doas,
            other => Escalation::Custom(other.to_string()),
        });
    }

    if exists("sudo") {
        Ok(Escalation::Sudo)
    } else if exists("doas") {
        Ok(Escalation::Doas)
    } else {
        Err(PacmanError::NoEscalationTool)
    }
}

fn command_exists_in_path(cmd: &str, path_var: Option<&OsStr>) -> bool {
    if cmd.contains('/') {
        return is_executable_file(Path::new(cmd));
    }
    let Some(paths) = path_var else {
        return false;
    };
    std::env::split_paths(paths).any(|dir| is_executable_file(&dir.join(cmd)))
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    meta.is_file() && (meta.permissions().mode() & 0o111 != 0)
}

/// Guard that aborts the background `sudo -n -v` keepalive task when dropped.
pub struct SudoKeepaliveGuard {
    handle: Option<JoinHandle<()>>,
}

impl Drop for SudoKeepaliveGuard {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

/// Suspends the inline TUI (if active) while an interactive subprocess owns the TTY,
/// and resumes it automatically when the returned guard drops.
pub async fn with_tty_handoff<F, Fut, T>(tui: Option<&TuiHandle>, f: F) -> Result<T, PacmanError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T, PacmanError>>,
{
    let guard = match tui {
        Some(handle) => Some(handle.suspend().await),
        None => None,
    };
    with_tty_handoff_guard(guard, f).await
}

async fn with_tty_handoff_guard<G, F, Fut, T>(guard: Option<G>, f: F) -> Result<T, PacmanError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T, PacmanError>>,
{
    let _guard = guard;
    f().await
}

/// Forwards raw arguments directly to `pacman`, inheriting the current TTY.
///
/// Escalates via [`detect_escalation`] when `needs_root` is `true`.
pub async fn passthrough(raw_args: &[String], needs_root: bool) -> Result<ExitStatus, PacmanError> {
    let (program, args): (String, Vec<OsString>) = if needs_root {
        let esc = detect_escalation()?;
        let mut all = Vec::with_capacity(raw_args.len() + 1);
        all.push(OsString::from("pacman"));
        all.extend(raw_args.iter().map(OsString::from));
        (esc.program().to_string(), all)
    } else {
        (
            "pacman".to_string(),
            raw_args.iter().map(OsString::from).collect(),
        )
    };

    Command::new(&program)
        .args(&args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .map_err(|source| PacmanError::Spawn { program, source })
}

/// Validates privilege escalation credentials (`sudo -v`) with TTY handoff.
pub async fn sudo_validate(
    escalation: &Escalation,
    tui: Option<&TuiHandle>,
) -> Result<(), PacmanError> {
    if !escalation.supports_keepalive() {
        return Ok(());
    }

    with_tty_handoff(tui, || async {
        let program = escalation.program().to_string();
        let status = Command::new(&program)
            .arg("-v")
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .await
            .map_err(|source| PacmanError::Spawn {
                program: program.clone(),
                source,
            })?;

        if !status.success() {
            return Err(PacmanError::SudoValidationFailed {
                code: status.code(),
            });
        }
        Ok(())
    })
    .await
}

/// Spawns a background task that runs `sudo -n -v` every 50 seconds until the
/// returned [`SudoKeepaliveGuard`] is dropped.
pub fn spawn_keepalive(escalation: &Escalation) -> SudoKeepaliveGuard {
    if !escalation.supports_keepalive() {
        return SudoKeepaliveGuard { handle: None };
    }

    let program = escalation.program().to_string();
    let handle = tokio::spawn(async move {
        let mut ticker = interval(SUDO_KEEPALIVE_INTERVAL);
        // Consume the immediate initial tick; sudo_validate just refreshed credentials.
        ticker.tick().await;

        loop {
            ticker.tick().await;
            let status = Command::new(&program)
                .args(["-n", "-v"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;

            match status {
                Ok(s) if s.success() => {}
                _ => break,
            }
        }
    });

    SudoKeepaliveGuard {
        handle: Some(handle),
    }
}

/// Refreshes sync databases via `sudo pacman -Sy` (or `-Syy` when `refresh_count >= 2`).
pub async fn refresh(
    escalation: &Escalation,
    refresh_count: u8,
    noconfirm: bool,
    tui: Option<&TuiHandle>,
) -> Result<(), PacmanError> {
    if refresh_count == 0 {
        return Ok(());
    }
    let args = build_refresh_args(refresh_count, noconfirm);
    run_privileged_pacman(escalation, &args, tui, !noconfirm, 0).await
}

/// Installs official repository packages via `sudo pacman -S --needed`.
pub async fn install_repo(
    escalation: &Escalation,
    targets: &[String],
    asdeps: bool,
    noconfirm: bool,
    tui: Option<&TuiHandle>,
) -> Result<(), PacmanError> {
    if targets.is_empty() {
        return Ok(());
    }
    let args = build_install_repo_args(targets, asdeps, noconfirm);
    run_privileged_pacman(escalation, &args, tui, !noconfirm, targets.len()).await
}

/// Installs built `.pkg.tar.*` package archives via `sudo pacman -U`.
pub async fn install_files(
    escalation: &Escalation,
    pkg_files: &[PathBuf],
    asdeps: bool,
    asexplicit: bool,
    noconfirm: bool,
    tui: Option<&TuiHandle>,
) -> Result<(), PacmanError> {
    if pkg_files.is_empty() {
        return Ok(());
    }
    let args = build_install_files_args(pkg_files, asdeps, asexplicit, noconfirm);
    run_privileged_pacman(escalation, &args, tui, !noconfirm, pkg_files.len()).await
}

/// Removes packages via `sudo pacman` using the forwarded removal arguments.
pub async fn remove_packages(
    escalation: &Escalation,
    raw_args: &[String],
    noconfirm: bool,
    tui: Option<&TuiHandle>,
) -> Result<(), PacmanError> {
    let args = build_remove_args(raw_args, noconfirm);
    let total_hint = raw_args.iter().filter(|a| !a.starts_with('-')).count();
    run_privileged_pacman(escalation, &args, tui, !noconfirm, total_hint).await
}

fn build_refresh_args(refresh_count: u8, noconfirm: bool) -> Vec<OsString> {
    let mut args = Vec::with_capacity(2);
    if refresh_count >= 2 {
        args.push(OsString::from("-Syy"));
    } else {
        args.push(OsString::from("-Sy"));
    }
    if noconfirm {
        args.push(OsString::from("--noconfirm"));
    }
    args
}

fn build_install_repo_args(targets: &[String], asdeps: bool, noconfirm: bool) -> Vec<OsString> {
    let mut args = Vec::with_capacity(targets.len() + 4);
    args.push(OsString::from("-S"));
    args.push(OsString::from("--needed"));
    if asdeps {
        args.push(OsString::from("--asdeps"));
    }
    if noconfirm {
        args.push(OsString::from("--noconfirm"));
    }
    args.extend(targets.iter().map(OsString::from));
    args
}

fn build_install_files_args(
    pkg_files: &[PathBuf],
    asdeps: bool,
    asexplicit: bool,
    noconfirm: bool,
) -> Vec<OsString> {
    let mut args = Vec::with_capacity(pkg_files.len() + 3);
    args.push(OsString::from("-U"));
    if asdeps {
        args.push(OsString::from("--asdeps"));
    } else if asexplicit {
        args.push(OsString::from("--asexplicit"));
    }
    if noconfirm {
        args.push(OsString::from("--noconfirm"));
    }
    args.extend(pkg_files.iter().map(|p| p.as_os_str().to_os_string()));
    args
}

fn build_remove_args(raw_args: &[String], noconfirm: bool) -> Vec<OsString> {
    let mut args: Vec<OsString> = raw_args.iter().map(OsString::from).collect();
    if noconfirm && !raw_args.iter().any(|a| a == "--noconfirm") {
        args.push(OsString::from("--noconfirm"));
    }
    args
}

/// Structured events parsed from `LC_ALL=C` `pacman` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PacmanEvent {
    Section(String),
    Downloading {
        package: String,
    },
    Checking {
        step: String,
        current: Option<usize>,
        total: Option<usize>,
    },
    Installing {
        current: usize,
        total: usize,
        package: String,
    },
    Upgrading {
        current: usize,
        total: usize,
        package: String,
    },
    Reinstalling {
        current: usize,
        total: usize,
        package: String,
    },
    Removing {
        current: usize,
        total: usize,
        package: String,
    },
    Hook {
        current: usize,
        total: usize,
        name: String,
    },
    Other(String),
}

const CHECKING_STEPS: &[&str] = &[
    "checking keys in keyring",
    "checking package integrity",
    "loading package files",
    "checking for file conflicts",
    "checking available disk space",
];

/// Parses a single line of `LC_ALL=C` `pacman` output into a [`PacmanEvent`].
pub fn parse_pacman_line(line: &str) -> Option<PacmanEvent> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(section) = trimmed.strip_prefix(":: ") {
        return Some(PacmanEvent::Section(section.trim().to_string()));
    }

    if let Some(rest) = trimmed.strip_prefix("downloading ") {
        let pkg = rest.trim_end_matches("...").trim();
        if !pkg.is_empty() {
            return Some(PacmanEvent::Downloading {
                package: pkg.to_string(),
            });
        }
    }

    if let Some((current, total, after)) = parse_counter_prefix(trimmed) {
        let normalized = after.trim_end_matches("...").trim();
        if CHECKING_STEPS.contains(&normalized) {
            return Some(PacmanEvent::Checking {
                step: normalized.to_string(),
                current: Some(current),
                total: Some(total),
            });
        }
        if let Some(pkg) = after.strip_prefix("installing ") {
            return Some(PacmanEvent::Installing {
                current,
                total,
                package: pkg.trim_end_matches("...").trim().to_string(),
            });
        }
        if let Some(pkg) = after.strip_prefix("upgrading ") {
            return Some(PacmanEvent::Upgrading {
                current,
                total,
                package: pkg.trim_end_matches("...").trim().to_string(),
            });
        }
        if let Some(pkg) = after.strip_prefix("reinstalling ") {
            return Some(PacmanEvent::Reinstalling {
                current,
                total,
                package: pkg.trim_end_matches("...").trim().to_string(),
            });
        }
        if let Some(pkg) = after.strip_prefix("removing ") {
            return Some(PacmanEvent::Removing {
                current,
                total,
                package: pkg.trim_end_matches("...").trim().to_string(),
            });
        }
        if !normalized.is_empty() {
            return Some(PacmanEvent::Hook {
                current,
                total,
                name: normalized.to_string(),
            });
        }
    }

    let normalized = trimmed.trim_end_matches("...").trim();
    if CHECKING_STEPS.contains(&normalized) {
        return Some(PacmanEvent::Checking {
            step: normalized.to_string(),
            current: None,
            total: None,
        });
    }

    Some(PacmanEvent::Other(trimmed.to_string()))
}

fn parse_counter_prefix(s: &str) -> Option<(usize, usize, &str)> {
    let rest = s.strip_prefix('(')?;
    let close_idx = rest.find(')')?;
    let counter = &rest[..close_idx];
    let (cur_str, tot_str) = counter.split_once('/')?;
    let current = cur_str.trim().parse::<usize>().ok()?;
    let total = tot_str.trim().parse::<usize>().ok()?;
    let after = rest[close_idx + 1..].trim();
    Some((current, total, after))
}

struct PacmanStreamState {
    active_pkg: Option<String>,
    used_tx_node: bool,
    used_hooks_node: bool,
    total_hint: usize,
}

impl PacmanStreamState {
    fn new(total_hint: usize) -> Self {
        Self {
            active_pkg: None,
            used_tx_node: false,
            used_hooks_node: false,
            total_hint,
        }
    }

    fn finish_active_pkg(&mut self, tui: &TuiHandle) {
        if let Some(pkg) = self.active_pkg.take() {
            tui.repo_phase(pkg, "done", None);
        }
    }

    fn finish_tx_node(&mut self, tui: &TuiHandle) {
        if self.used_tx_node {
            self.used_tx_node = false;
            tui.repo_phase("pacman", "done", None);
        }
    }

    fn finish_hooks_node(&mut self, tui: &TuiHandle) {
        if self.used_hooks_node {
            self.used_hooks_node = false;
            tui.repo_phase("hooks", "done", None);
        }
    }

    fn handle_line(&mut self, line: &str, tui: &TuiHandle) {
        let Some(event) = parse_pacman_line(line) else {
            return;
        };
        let raw = line.trim().to_string();

        match event {
            PacmanEvent::Section(sec) => {
                if sec.starts_with("Running post-transaction hooks") {
                    self.finish_active_pkg(tui);
                    self.finish_tx_node(tui);
                    tui.set_progress("Running post-transaction hooks", 0, 0, None);
                } else if sec.starts_with("Retrieving packages") {
                    tui.set_progress("Retrieving packages", 0, self.total_hint, None);
                } else if sec.starts_with("Processing package changes") {
                    self.finish_tx_node(tui);
                    tui.set_progress("Processing package changes", 0, self.total_hint, None);
                } else {
                    tui.set_progress(sec.trim_end_matches("..."), 0, self.total_hint, None);
                }
            }
            PacmanEvent::Downloading { package } => {
                self.used_tx_node = true;
                tui.repo_phase("pacman", "downloading", Some(raw));
                tui.set_progress(
                    "Retrieving packages",
                    0,
                    self.total_hint,
                    Some(format!("downloading {package}")),
                );
            }
            PacmanEvent::Checking {
                step,
                current,
                total,
            } => {
                self.used_tx_node = true;
                tui.repo_phase("pacman", "checking", Some(raw));
                tui.set_progress(
                    "Checking packages",
                    current.unwrap_or(0),
                    total.unwrap_or(0),
                    Some(step),
                );
            }
            PacmanEvent::Installing {
                current,
                total,
                package,
            } => {
                self.handle_pkg_change("installing", current, total, package, raw, tui);
            }
            PacmanEvent::Upgrading {
                current,
                total,
                package,
            } => {
                self.handle_pkg_change("upgrading", current, total, package, raw, tui);
            }
            PacmanEvent::Reinstalling {
                current,
                total,
                package,
            } => {
                self.handle_pkg_change("reinstalling", current, total, package, raw, tui);
            }
            PacmanEvent::Removing {
                current,
                total,
                package,
            } => {
                self.handle_pkg_change("removing", current, total, package, raw, tui);
            }
            PacmanEvent::Hook {
                current,
                total,
                name,
            } => {
                self.finish_active_pkg(tui);
                self.finish_tx_node(tui);
                self.used_hooks_node = true;
                tui.repo_phase("hooks", "hooks", Some(raw));
                tui.set_progress("Running post-transaction hooks", current, total, Some(name));
            }
            PacmanEvent::Other(text) => {
                if let Some(pkg) = self.active_pkg.as_deref() {
                    tui.log_line(pkg, text);
                } else if self.used_hooks_node {
                    tui.log_line("hooks", text);
                } else if self.used_tx_node {
                    tui.log_line("pacman", text);
                }
            }
        }
    }

    fn handle_pkg_change(
        &mut self,
        verb: &str,
        current: usize,
        total: usize,
        package: String,
        raw: String,
        tui: &TuiHandle,
    ) {
        self.finish_tx_node(tui);
        if self.active_pkg.as_deref() != Some(package.as_str()) {
            self.finish_active_pkg(tui);
            self.active_pkg = Some(package.clone());
        }
        tui.repo_phase(&package, verb, Some(raw));
        tui.set_progress(
            "Processing package changes",
            current,
            total,
            Some(format!("{verb} {package}")),
        );
    }

    fn finish(mut self, success: bool, tui: &TuiHandle) {
        if success {
            self.finish_active_pkg(tui);
            self.finish_tx_node(tui);
            self.finish_hooks_node(tui);
        } else if let Some(pkg) = self.active_pkg.take() {
            tui.repo_phase(pkg, "failed", None);
        } else if self.used_hooks_node {
            self.used_hooks_node = false;
            tui.repo_phase("hooks", "failed", None);
        } else if self.used_tx_node {
            self.used_tx_node = false;
            tui.repo_phase("pacman", "failed", None);
        }
        tui.clear_progress();
    }
}

async fn can_run_noninteractive_sudo(escalation: &Escalation) -> bool {
    if !escalation.supports_keepalive() {
        return false;
    }
    Command::new(escalation.program())
        .args(["-n", "-v"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success())
}

async fn run_privileged_pacman(
    escalation: &Escalation,
    pacman_args: &[OsString],
    tui: Option<&TuiHandle>,
    interactive: bool,
    total_hint: usize,
) -> Result<(), PacmanError> {
    if !interactive
        && let Some(tui_handle) = tui
        && can_run_noninteractive_sudo(escalation).await
    {
        return run_streaming_pacman(escalation, pacman_args, tui_handle, total_hint).await;
    }

    with_tty_handoff(tui, || async {
        let program = escalation.program().to_string();
        let status = Command::new(&program)
            .arg("pacman")
            .args(pacman_args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .await
            .map_err(|source| PacmanError::Spawn {
                program: program.clone(),
                source,
            })?;

        if !status.success() {
            return Err(PacmanError::CommandFailed {
                command: format!("{program} pacman"),
                code: status.code(),
            });
        }
        Ok(())
    })
    .await
}

async fn run_streaming_pacman(
    escalation: &Escalation,
    pacman_args: &[OsString],
    tui: &TuiHandle,
    total_hint: usize,
) -> Result<(), PacmanError> {
    let program = escalation.program().to_string();
    let mut cmd = Command::new(&program);
    cmd.arg("-n")
        .arg("pacman")
        .arg("--noprogressbar")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if !pacman_args.iter().any(|a| a == "--noconfirm") {
        cmd.arg("--noconfirm");
    }
    cmd.args(pacman_args);

    let mut child = cmd.spawn().map_err(|source| PacmanError::Spawn {
        program: program.clone(),
        source,
    })?;

    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let stdout_task = child.stdout.take().map(|out| {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(out).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if tx.send(line).is_err() {
                    break;
                }
            }
        })
    });
    let stderr_task = child.stderr.take().map(|err| {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(err).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if tx.send(line).is_err() {
                    break;
                }
            }
        })
    });
    drop(tx);

    let mut state = PacmanStreamState::new(total_hint);
    while let Some(line) = rx.recv().await {
        state.handle_line(&line, tui);
    }

    if let Some(task) = stdout_task {
        let _ = task.await;
    }
    if let Some(task) = stderr_task {
        let _ = task.await;
    }

    let status = child.wait().await.map_err(|source| PacmanError::Spawn {
        program: program.clone(),
        source,
    })?;

    state.finish(status.success(), tui);

    if !status.success() {
        return Err(PacmanError::CommandFailed {
            command: format!("{program} pacman"),
            code: status.code(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn root_guard_rejects_euid_zero_and_allows_unprivileged() {
        assert!(matches!(
            ensure_not_root_for_uid(Uid::from_raw(0)),
            Err(PacmanError::RunningAsRoot)
        ));
        assert!(ensure_not_root_for_uid(Uid::from_raw(1000)).is_ok());
    }

    #[test]
    fn escalation_detection_prefers_pom_sudo_override() {
        let esc = detect_escalation_with(Some("doas"), |cmd| cmd == "doas" || cmd == "sudo")
            .expect("override should succeed");
        assert_eq!(esc, Escalation::Doas);

        let custom = detect_escalation_with(Some("/usr/local/bin/mysudo"), |cmd| {
            cmd == "/usr/local/bin/mysudo"
        })
        .expect("custom override should succeed");
        assert_eq!(
            custom,
            Escalation::Custom("/usr/local/bin/mysudo".to_string())
        );

        let missing = detect_escalation_with(Some("missing-sudo"), |_| false);
        assert!(matches!(
            missing,
            Err(PacmanError::EscalationToolNotFound(ref s)) if s == "missing-sudo"
        ));
    }

    #[test]
    fn escalation_detection_falls_back_sudo_then_doas() {
        let sudo = detect_escalation_with(None, |cmd| cmd == "sudo" || cmd == "doas").unwrap();
        assert_eq!(sudo, Escalation::Sudo);

        let doas = detect_escalation_with(Some("   "), |cmd| cmd == "doas").unwrap();
        assert_eq!(doas, Escalation::Doas);

        let none = detect_escalation_with(None, |_| false);
        assert!(matches!(none, Err(PacmanError::NoEscalationTool)));
    }

    #[test]
    fn keepalive_support_matches_sudo_only() {
        assert!(Escalation::Sudo.supports_keepalive());
        assert!(!Escalation::Doas.supports_keepalive());
        assert!(Escalation::Custom("/usr/bin/sudo".to_string()).supports_keepalive());
        assert!(!Escalation::Custom("pkexec".to_string()).supports_keepalive());
    }

    #[test]
    fn command_exists_checks_executable_bit() {
        let dir = tempfile::tempdir().unwrap();
        let exec_path = dir.path().join("my-sudo");
        let non_exec_path = dir.path().join("not-exec");

        std::fs::write(&exec_path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&exec_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        std::fs::write(&non_exec_path, "plain file").unwrap();
        std::fs::set_permissions(&non_exec_path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let path_env = dir.path().as_os_str();
        assert!(command_exists_in_path("my-sudo", Some(path_env)));
        assert!(!command_exists_in_path("not-exec", Some(path_env)));
        assert!(!command_exists_in_path("absent", Some(path_env)));
        assert!(command_exists_in_path(exec_path.to_str().unwrap(), None));
        assert!(!command_exists_in_path(
            non_exec_path.to_str().unwrap(),
            None
        ));
    }

    #[test]
    fn pacman_arg_builders_produce_expected_flags() {
        assert_eq!(build_refresh_args(1, false), vec![OsString::from("-Sy")]);
        assert_eq!(
            build_refresh_args(2, true),
            vec![OsString::from("-Syy"), OsString::from("--noconfirm")]
        );

        assert_eq!(
            build_install_repo_args(&["ripgrep".into(), "fd".into()], true, false),
            vec![
                OsString::from("-S"),
                OsString::from("--needed"),
                OsString::from("--asdeps"),
                OsString::from("ripgrep"),
                OsString::from("fd"),
            ]
        );

        assert_eq!(
            build_install_files_args(&[PathBuf::from("/tmp/foo.pkg.tar.zst")], false, true, true),
            vec![
                OsString::from("-U"),
                OsString::from("--asexplicit"),
                OsString::from("--noconfirm"),
                OsString::from("/tmp/foo.pkg.tar.zst"),
            ]
        );

        assert_eq!(
            build_remove_args(&["-Rns".into(), "foo".into()], false),
            vec![OsString::from("-Rns"), OsString::from("foo")]
        );
        assert_eq!(
            build_remove_args(&["-Rns".into(), "foo".into()], true),
            vec![
                OsString::from("-Rns"),
                OsString::from("foo"),
                OsString::from("--noconfirm"),
            ]
        );
        assert_eq!(
            build_remove_args(&["-R".into(), "--noconfirm".into(), "foo".into()], true),
            vec![
                OsString::from("-R"),
                OsString::from("--noconfirm"),
                OsString::from("foo"),
            ]
        );
    }

    #[tokio::test]
    async fn tty_handoff_guard_drops_after_closure_on_ok_and_err() {
        struct DropTracker(Arc<AtomicBool>);
        impl Drop for DropTracker {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let tracker = DropTracker(Arc::clone(&dropped));
        let res = with_tty_handoff_guard(Some(tracker), || {
            let dropped_inner = Arc::clone(&dropped);
            async move {
                assert!(
                    !dropped_inner.load(Ordering::SeqCst),
                    "guard must stay alive during subprocess"
                );
                Ok(42)
            }
        })
        .await
        .unwrap();
        assert_eq!(res, 42);
        assert!(dropped.load(Ordering::SeqCst), "guard must drop on return");

        let dropped_err = Arc::new(AtomicBool::new(false));
        let tracker_err = DropTracker(Arc::clone(&dropped_err));
        let err_res: Result<(), PacmanError> =
            with_tty_handoff_guard(Some(tracker_err), || async {
                Err(PacmanError::CommandFailed {
                    command: "test".into(),
                    code: Some(1),
                })
            })
            .await;
        assert!(err_res.is_err());
        assert!(
            dropped_err.load(Ordering::SeqCst),
            "guard must drop even when closure returns Err"
        );
    }

    #[test]
    fn parse_pacman_line_handles_lc_all_c_output() {
        assert_eq!(parse_pacman_line("   "), None);

        assert_eq!(
            parse_pacman_line(":: Retrieving packages..."),
            Some(PacmanEvent::Section("Retrieving packages...".into()))
        );
        assert_eq!(
            parse_pacman_line(" downloading gedit-48.1-1-x86_64.pkg.tar.zst..."),
            Some(PacmanEvent::Downloading {
                package: "gedit-48.1-1-x86_64.pkg.tar.zst".into()
            })
        );

        assert_eq!(
            parse_pacman_line("(6/6) checking keys in keyring"),
            Some(PacmanEvent::Checking {
                step: "checking keys in keyring".into(),
                current: Some(6),
                total: Some(6),
            })
        );
        assert_eq!(
            parse_pacman_line("checking available disk space"),
            Some(PacmanEvent::Checking {
                step: "checking available disk space".into(),
                current: None,
                total: None,
            })
        );

        assert_eq!(
            parse_pacman_line(":: Processing package changes..."),
            Some(PacmanEvent::Section("Processing package changes...".into()))
        );
        assert_eq!(
            parse_pacman_line("(1/6) installing libpeas..."),
            Some(PacmanEvent::Installing {
                current: 1,
                total: 6,
                package: "libpeas".into(),
            })
        );
        assert_eq!(
            parse_pacman_line("(2/6) upgrading gedit..."),
            Some(PacmanEvent::Upgrading {
                current: 2,
                total: 6,
                package: "gedit".into(),
            })
        );
        assert_eq!(
            parse_pacman_line("(1/2) removing foo..."),
            Some(PacmanEvent::Removing {
                current: 1,
                total: 2,
                package: "foo".into(),
            })
        );

        assert_eq!(
            parse_pacman_line(":: Running post-transaction hooks..."),
            Some(PacmanEvent::Section(
                "Running post-transaction hooks...".into()
            ))
        );
        assert_eq!(
            parse_pacman_line("(1/3) Arming ConditionNeedsUpdate..."),
            Some(PacmanEvent::Hook {
                current: 1,
                total: 3,
                name: "Arming ConditionNeedsUpdate".into(),
            })
        );
    }
}
