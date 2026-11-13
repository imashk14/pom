use std::collections::VecDeque;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::Command;
use tokio::sync::mpsc;

/// Maximum number of recent log lines retained in the live UI ring buffer per package.
pub const LOG_RING_CAPACITY: usize = 5;

/// Deterministic build phase for an AUR package base.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuildPhase {
    Pending,
    Cloning,
    Verifying,
    Extracting,
    Building,
    Packaging,
    Installing,
    Done,
    Failed,
}

impl BuildPhase {
    /// Returns a concise lowercase label suitable for inline UI badges.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Cloning => "cloning",
            Self::Verifying => "verifying",
            Self::Extracting => "extracting",
            Self::Building => "building",
            Self::Packaging => "packaging",
            Self::Installing => "installing",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}

impl fmt::Display for BuildPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Events emitted by [`build_package_base`] for live UI or logging consumers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MakepkgEvent {
    PhaseChanged { pkgbase: String, phase: BuildPhase },
    LogLine { pkgbase: String, line: String },
}

/// Fixed-capacity rolling ring buffer for the most recent `makepkg` output lines.
#[derive(Debug, Clone)]
pub struct LogRingBuffer {
    capacity: usize,
    lines: VecDeque<String>,
}

impl Default for LogRingBuffer {
    fn default() -> Self {
        Self::new(LOG_RING_CAPACITY)
    }
}

impl LogRingBuffer {
    /// Creates a new ring buffer retaining at most `capacity` lines.
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            lines: VecDeque::with_capacity(capacity),
        }
    }

    /// Pushes a line, evicting the oldest line if the buffer is at capacity.
    pub fn push(&mut self, line: impl Into<String>) {
        if self.capacity == 0 {
            return;
        }
        if self.lines.len() == self.capacity {
            self.lines.pop_front();
        }
        self.lines.push_back(line.into());
    }

    /// Collects the retained lines into an owned `Vec<String>`.
    pub fn to_vec(&self) -> Vec<String> {
        self.lines.iter().cloned().collect()
    }

    /// Returns the number of currently retained lines.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.lines.len()
    }
}

/// Options controlling `makepkg` execution and output artifact filtering.
#[derive(Debug, Clone, Default)]
pub struct BuildOptions {
    /// Expected package names from `PlannedAurBase::packages`.
    /// When non-empty, `--packagelist` output is filtered to these package names.
    pub target_packages: Vec<String>,
    /// Optional override for the log output directory (defaults to `$XDG_STATE_HOME/pom/logs`).
    pub log_dir: Option<PathBuf>,
}

/// Successful result of building an AUR package base with `makepkg`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildResult {
    pub pkgbase: String,
    pub packages: Vec<PathBuf>,
    pub log_path: PathBuf,
    pub last_phase: BuildPhase,
    pub tail: Vec<String>,
}

/// Errors produced during `makepkg` execution or package artifact resolution.
#[derive(Debug, thiserror::Error)]
pub enum MakepkgError {
    #[error("failed to create log directory {path}: {source}")]
    LogDirCreate {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to create log file {path}: {source}")]
    LogFileCreate {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to spawn makepkg in {dir}: {source}")]
    Spawn {
        dir: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "makepkg failed for {pkgbase} (exit code: {exit_code:?}); full log saved at {log_path}"
    )]
    BuildFailed {
        pkgbase: String,
        exit_code: Option<i32>,
        log_path: PathBuf,
        full_log: String,
        tail: Vec<String>,
    },
    #[error("makepkg --packagelist failed in {dir} (exit code: {exit_code:?}): {stderr}")]
    PackageListFailed {
        dir: PathBuf,
        exit_code: Option<i32>,
        stderr: String,
    },
    #[error("expected built package archive not found on disk: {path}")]
    MissingBuiltPackage { path: PathBuf },
    #[error("no built packages from makepkg --packagelist matched targets for {pkgbase}")]
    NoMatchingPackages { pkgbase: String },
    #[error("I/O error while processing makepkg output: {0}")]
    Io(#[from] std::io::Error),
}

/// Strips ANSI CSI/OSC/ESC sequences and carriage returns from a line of terminal output.
pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            match chars.peek().copied() {
                // CSI sequence: ESC [ <params/intermediates> <final byte 0x40..=0x7E>
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC sequence: ESC ] ... (BEL or ST)
                Some(']') => {
                    chars.next();
                    while let Some(c) = chars.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                // Other 2-byte escape sequence
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
        } else if ch != '\r' {
            out.push(ch);
        }
    }

    out
}

/// Parses a single `LC_ALL=C` `makepkg` (7.1.0) output line into a [`BuildPhase`] transition.
///
/// Strips ANSI escape sequences before checking for `==>` status markers.
pub fn parse_makepkg_line(line: &str) -> Option<BuildPhase> {
    let cleaned = strip_ansi(line);
    let trimmed = cleaned.trim();
    let rest = trimmed.strip_prefix("==>")?.trim_start();

    if rest.starts_with("ERROR:") {
        return Some(BuildPhase::Failed);
    }

    if rest.starts_with("Retrieving sources...")
        || (rest.starts_with("Validating ") && rest.contains(" files with "))
        || rest.starts_with("Verifying source file signatures")
    {
        return Some(BuildPhase::Verifying);
    }

    if rest.starts_with("Extracting sources...")
        || rest.starts_with("Starting prepare()...")
        || rest.starts_with("Starting pkgver()...")
        || rest.starts_with("Sources are ready.")
    {
        return Some(BuildPhase::Extracting);
    }

    if rest.starts_with("Starting build()...") || rest.starts_with("Starting check()...") {
        return Some(BuildPhase::Building);
    }

    if rest.starts_with("Entering fakeroot environment...")
        || rest.starts_with("Starting package")
        || rest.starts_with("Tidying install...")
        || rest.starts_with("Creating package ")
        || rest.starts_with("Checking for packaging issues...")
        || rest.starts_with("Signing package")
    {
        return Some(BuildPhase::Packaging);
    }

    None
}

/// Resolves the persistent log directory (`$XDG_STATE_HOME/pom/logs/`).
pub fn resolve_log_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(xdg).join("pom").join("logs");
    }
    if let Some(state_dir) = dirs::state_dir() {
        return state_dir.join("pom").join("logs");
    }
    std::env::temp_dir().join("pom").join("logs")
}

/// Extracts `<pkgname>` from a `.pkg.tar.*` archive path produced by `makepkg --packagelist`.
///
/// Expected filename format: `<pkgname>-<pkgver>-<pkgrel>-<arch>.pkg.tar.<ext>`.
pub fn extract_pkgname_from_archive_path(path: &Path) -> Option<String> {
    let filename = path.file_name()?.to_str()?;
    let stem = filename.split(".pkg.tar").next()?;
    let mut parts = stem.rsplitn(4, '-');
    let _arch = parts.next()?;
    let _pkgrel = parts.next()?;
    let _pkgver = parts.next()?;
    let pkgname = parts.next()?;
    if pkgname.is_empty() {
        return None;
    }
    Some(pkgname.to_owned())
}

/// Filters `makepkg --packagelist` lines to the requested `target_packages` (if non-empty).
pub fn filter_packagelist_lines<'a>(
    lines: impl IntoIterator<Item = &'a str>,
    target_packages: &[String],
) -> Vec<PathBuf> {
    lines
        .into_iter()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .filter(|path| {
            if target_packages.is_empty() {
                return true;
            }
            extract_pkgname_from_archive_path(path)
                .is_some_and(|name| target_packages.iter().any(|t| t == &name))
        })
        .collect()
}

/// Runs `makepkg --packagelist` with `LC_ALL=C` in `build_dir` and verifies built archives exist.
pub async fn resolve_package_list(
    pkgbase: &str,
    build_dir: &Path,
    target_packages: &[String],
) -> Result<Vec<PathBuf>, MakepkgError> {
    let output = Command::new("makepkg")
        .arg("--packagelist")
        .current_dir(build_dir)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|source| MakepkgError::Spawn {
            dir: build_dir.to_path_buf(),
            source,
        })?;

    if !output.status.success() {
        return Err(MakepkgError::PackageListFailed {
            dir: build_dir.to_path_buf(),
            exit_code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let paths = filter_packagelist_lines(stdout.lines(), target_packages);

    if paths.is_empty() {
        return Err(MakepkgError::NoMatchingPackages {
            pkgbase: pkgbase.to_owned(),
        });
    }

    for path in &paths {
        if !tokio::fs::try_exists(path).await.unwrap_or(false) {
            return Err(MakepkgError::MissingBuiltPackage { path: path.clone() });
        }
    }

    Ok(paths)
}

/// Spawns `makepkg --noconfirm --noprogressbar --nocolor -f` unprivileged with `LC_ALL=C`,
/// persisting all output to `$XDG_STATE_HOME/pom/logs/<pkgbase>-<ts>.log` and streaming
/// phase transitions and log lines over `event_tx`.
pub async fn build_package_base(
    pkgbase: &str,
    build_dir: &Path,
    opts: &BuildOptions,
    event_tx: Option<&mpsc::UnboundedSender<MakepkgEvent>>,
) -> Result<BuildResult, MakepkgError> {
    let log_dir = opts.log_dir.clone().unwrap_or_else(resolve_log_dir);
    tokio::fs::create_dir_all(&log_dir)
        .await
        .map_err(|source| MakepkgError::LogDirCreate {
            path: log_dir.clone(),
            source,
        })?;

    let unix_ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let log_path = log_dir.join(format!("{pkgbase}-{unix_ts}.log"));

    let log_file =
        tokio::fs::File::create(&log_path)
            .await
            .map_err(|source| MakepkgError::LogFileCreate {
                path: log_path.clone(),
                source,
            })?;
    let mut log_writer = BufWriter::new(log_file);

    let mut child = Command::new("makepkg")
        .args(["--noconfirm", "--noprogressbar", "--nocolor", "-f"])
        .current_dir(build_dir)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| MakepkgError::Spawn {
            dir: build_dir.to_path_buf(),
            source,
        })?;

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let mut stdout_lines = BufReader::new(stdout).lines();
    let mut stderr_lines = BufReader::new(stderr).lines();
    let mut stdout_done = false;
    let mut stderr_done = false;

    let mut full_log = String::new();
    let mut ring = LogRingBuffer::new(LOG_RING_CAPACITY);
    let mut current_phase = BuildPhase::Verifying;

    while !stdout_done || !stderr_done {
        let maybe_line = tokio::select! {
            res = stdout_lines.next_line(), if !stdout_done => {
                match res? {
                    Some(line) => Some(line),
                    None => {
                        stdout_done = true;
                        None
                    }
                }
            }
            res = stderr_lines.next_line(), if !stderr_done => {
                match res? {
                    Some(line) => Some(line),
                    None => {
                        stderr_done = true;
                        None
                    }
                }
            }
        };

        if let Some(raw_line) = maybe_line {
            let clean_line = strip_ansi(&raw_line);
            log_writer.write_all(clean_line.as_bytes()).await?;
            log_writer.write_all(b"\n").await?;

            full_log.push_str(&clean_line);
            full_log.push('\n');
            ring.push(clean_line.clone());

            if let Some(tx) = event_tx {
                let _ = tx.send(MakepkgEvent::LogLine {
                    pkgbase: pkgbase.to_owned(),
                    line: clean_line.clone(),
                });
            }

            if let Some(next_phase) = parse_makepkg_line(&clean_line)
                && next_phase != current_phase
            {
                current_phase = next_phase;
                if let Some(tx) = event_tx {
                    let _ = tx.send(MakepkgEvent::PhaseChanged {
                        pkgbase: pkgbase.to_owned(),
                        phase: next_phase,
                    });
                }
            }
        }
    }

    log_writer.flush().await?;
    let status = child.wait().await?;

    if !status.success() || current_phase == BuildPhase::Failed {
        if current_phase != BuildPhase::Failed
            && let Some(tx) = event_tx
        {
            let _ = tx.send(MakepkgEvent::PhaseChanged {
                pkgbase: pkgbase.to_owned(),
                phase: BuildPhase::Failed,
            });
        }
        return Err(MakepkgError::BuildFailed {
            pkgbase: pkgbase.to_owned(),
            exit_code: status.code(),
            log_path,
            full_log,
            tail: ring.to_vec(),
        });
    }

    let packages = resolve_package_list(pkgbase, build_dir, &opts.target_packages).await?;

    Ok(BuildResult {
        pkgbase: pkgbase.to_owned(),
        packages,
        log_path,
        last_phase: current_phase,
        tail: ring.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const STANDARD_BUILD_LOG: &str = "\
==> Making package: ripgrep-git 14.1.0.r12.gabcdef-1 (Thu Nov 13 10:00:00 UTC 2026)
==> Checking runtime dependencies...
==> Checking buildtime dependencies...
==> Retrieving sources...
  -> Updating ripgrep git repo...
==> Validating source files with sha256sums...
    ripgrep ... Passed
==> Extracting sources...
  -> Creating working copy of ripgrep git repo...
==> Starting pkgver()...
==> Updated version: ripgrep-git 14.1.0.r15.g123456-1
==> Starting build()...
   Compiling ripgrep v14.1.0
    Finished `release` profile [optimized] target(s) in 18.42s
==> Starting check()...
test result: ok. 42 passed; 0 failed
==> Entering fakeroot environment...
==> Starting package()...
==> Tidying install...
  -> Removing libtool files...
  -> Stripping unneeded symbols from binaries and libraries...
==> Checking for packaging issues...
==> Creating package \"ripgrep-git\"...
  -> Generating .PKGINFO file...
  -> Compressing package...
==> Leaving fakeroot environment.
==> Finished making: ripgrep-git 14.1.0.r15.g123456-1 (Thu Nov 13 10:00:25 UTC 2026)
";

    const SPLIT_PACKAGE_LOG: &str = "\
\x1b[1;32m==>\x1b[0m\x1b[1m Making package: foo-split 1.2.3-1 (Thu Nov 13 10:05:00 UTC 2026)\x1b[0m
==> Retrieving sources...
==> Validating source_x86_64 files with b2sums...
    foo-1.2.3-x86_64.tar.gz ... Passed
==> Verifying source file signatures with gpg...
    foo-1.2.3-x86_64.tar.gz ... Passed
==> Extracting sources...
==> Starting prepare()...
==> Sources are ready.
==> Starting build()...
cc -O2 -o foo main.c
==> Entering fakeroot environment...
==> Starting package_foo-lib()...
==> Tidying install...
==> Creating package \"foo-lib\"...
==> Starting package_foo-bin()...
==> Tidying install...
==> Creating package \"foo-bin\"...
==> Signing package(s)...
==> Finished making: foo-split 1.2.3-1 (Thu Nov 13 10:05:10 UTC 2026)
";

    const FAILED_BUILD_LOG: &str = "\
==> Making package: broken-pkg 0.1.0-1 (Thu Nov 13 10:10:00 UTC 2026)
==> Retrieving sources...
==> Validating source files with sha256sums...
    broken-0.1.0.tar.gz ... Passed
==> Extracting sources...
==> Starting build()...
line 1: configuring
line 2: compiling foo.c
line 3: compiling bar.c
line 4: linking
line 5: undefined reference to `missing_symbol`
line 6: collect2: error: ld returned 1 exit status
==> ERROR: A failure occurred in build().
    Aborting...
";

    fn collect_transitions(log: &str) -> Vec<BuildPhase> {
        let mut transitions = Vec::new();
        for line in log.lines() {
            if let Some(phase) = parse_makepkg_line(line)
                && transitions.last() != Some(&phase)
            {
                transitions.push(phase);
            }
        }
        transitions
    }

    #[test]
    fn parses_standard_makepkg_7_1_0_build() {
        let phases = collect_transitions(STANDARD_BUILD_LOG);
        assert_eq!(
            phases,
            vec![
                BuildPhase::Verifying,
                BuildPhase::Extracting,
                BuildPhase::Building,
                BuildPhase::Packaging,
            ]
        );
    }

    #[test]
    fn parses_split_package_and_ansi_and_arch_checksums() {
        let phases = collect_transitions(SPLIT_PACKAGE_LOG);
        assert_eq!(
            phases,
            vec![
                BuildPhase::Verifying,
                BuildPhase::Extracting,
                BuildPhase::Building,
                BuildPhase::Packaging,
            ]
        );

        assert_eq!(
            parse_makepkg_line("\x1b[1;32m==>\x1b[0m Starting package_foo-lib()..."),
            Some(BuildPhase::Packaging)
        );
        assert_eq!(
            parse_makepkg_line("==> Validating source_aarch64 files with sha512sums..."),
            Some(BuildPhase::Verifying)
        );
    }

    #[test]
    fn parses_failed_build_and_caps_ring_buffer_at_five() {
        let phases = collect_transitions(FAILED_BUILD_LOG);
        assert_eq!(
            phases,
            vec![
                BuildPhase::Verifying,
                BuildPhase::Extracting,
                BuildPhase::Building,
                BuildPhase::Failed,
            ]
        );

        let mut ring = LogRingBuffer::default();
        for line in FAILED_BUILD_LOG.lines() {
            ring.push(strip_ansi(line));
        }

        assert_eq!(ring.len(), LOG_RING_CAPACITY);
        assert_eq!(
            ring.to_vec(),
            vec![
                "line 4: linking",
                "line 5: undefined reference to `missing_symbol`",
                "line 6: collect2: error: ld returned 1 exit status",
                "==> ERROR: A failure occurred in build().",
                "    Aborting...",
            ]
        );
    }

    #[test]
    fn strips_ansi_escapes_cleanly() {
        let raw = "\x1b[1;34m  ->\x1b[0m\x1b[1m Downloading foo.tar.gz...\x1b[0m\r";
        assert_eq!(strip_ansi(raw), "  -> Downloading foo.tar.gz...");
    }

    #[test]
    fn filters_packagelist_including_hyphenated_names_and_debug_splits() {
        let raw = "\
/var/cache/pom/foo-bar-git-1.2.3-1-x86_64.pkg.tar.zst
/var/cache/pom/foo-bar-git-debug-1.2.3-1-x86_64.pkg.tar.zst
/var/cache/pom/foo-bar-docs-1.2.3-1-any.pkg.tar.zst
";
        let targets = vec!["foo-bar-git".to_owned(), "foo-bar-docs".to_owned()];
        let selected = filter_packagelist_lines(raw.lines(), &targets);
        assert_eq!(
            selected,
            vec![
                PathBuf::from("/var/cache/pom/foo-bar-git-1.2.3-1-x86_64.pkg.tar.zst"),
                PathBuf::from("/var/cache/pom/foo-bar-docs-1.2.3-1-any.pkg.tar.zst"),
            ]
        );
    }
}
