use std::ffi::OsStr;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{SystemTime, UNIX_EPOCH};

use futures::stream::{self, StreamExt};
use owo_colors::OwoColorize;
use srcinfo::Srcinfo;
use thiserror::Error;
use tokio::process::Command;
use tokio::sync::mpsc;

/// Git ref inside each AUR clone recording the last user-reviewed commit SHA.
pub const REVIEWED_REF: &str = "refs/pom/reviewed";

/// Default maximum number of concurrent `git clone` / `git fetch` operations.
pub const DEFAULT_GIT_CONCURRENCY: usize = 6;

/// Errors returned by AUR git synchronization, `.SRCINFO` parsing, and review flows.
#[derive(Debug, Error)]
pub enum AurGitError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("git {command} failed for '{pkgbase}' (exit {status:?}): {stderr}")]
    Git {
        pkgbase: String,
        command: String,
        status: Option<i32>,
        stderr: String,
    },

    #[error("missing .SRCINFO in '{pkgbase}' at {path}")]
    MissingSrcinfo { pkgbase: String, path: PathBuf },

    #[error("failed to parse .SRCINFO for '{pkgbase}': {source}")]
    Srcinfo {
        pkgbase: String,
        #[source]
        source: srcinfo::Error,
    },

    #[error("pager '{pager}' exited with status {status:?}")]
    PagerFailed { pager: String, status: Option<i32> },
}

/// Target AUR package base to clone or fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AurRepoTarget {
    pub pkgbase: String,
    /// Optional git remote URL override (defaults to `https://aur.archlinux.org/<pkgbase>.git`).
    pub remote_url: Option<String>,
}

impl AurRepoTarget {
    /// Creates a target pointing at the official AUR git remote for `pkgbase`.
    pub fn new(pkgbase: impl Into<String>) -> Self {
        Self {
            pkgbase: pkgbase.into(),
            remote_url: None,
        }
    }

    /// Creates a target with an explicit git remote URL (useful for mirrors or tests).
    #[cfg(test)]
    pub fn with_remote(pkgbase: impl Into<String>, remote_url: impl Into<String>) -> Self {
        Self {
            pkgbase: pkgbase.into(),
            remote_url: Some(remote_url.into()),
        }
    }

    fn effective_url(&self) -> String {
        self.remote_url
            .clone()
            .unwrap_or_else(|| format!("https://aur.archlinux.org/{}.git", self.pkgbase))
    }
}

/// Progress events emitted during concurrent AUR git synchronization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitSyncProgress {
    Cloning { pkgbase: String },
    Synced { pkgbase: String },
    Failed { pkgbase: String, error: String },
}

/// Synchronized local AUR git repository and its parsed `.SRCINFO` from `origin/HEAD`.
#[derive(Debug, Clone)]
pub struct SyncedRepo {
    pub pkgbase: String,
    pub dir: PathBuf,
    pub head_commit: String,
    #[allow(dead_code)]
    pub srcinfo: Srcinfo,
    #[allow(dead_code)]
    pub is_new_clone: bool,
}

/// Kind of unreviewed change detected for an AUR repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    New,
    Updated,
}

/// Unreviewed changes for a single AUR repository awaiting user inspection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoDiff {
    pub pkgbase: String,
    pub repo_dir: PathBuf,
    pub target_commit: String,
    pub kind: DiffKind,
    pub diff_text: String,
}

/// Async hook allowing callers to suspend an active inline TUI around `$PAGER` and prompts.
#[async_trait::async_trait]
pub trait ReviewUiSuspender: Send + Sync {
    type Guard: Send;
    async fn suspend(&self) -> Self::Guard;
}

/// Returns `$XDG_CACHE_HOME/pom/clone` (falling back to `~/.cache/pom/clone`).
pub fn aur_cache_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME")
        && !xdg.is_empty()
    {
        return PathBuf::from(xdg).join("pom").join("clone");
    }
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("pom")
        .join("clone")
}

/// Extracts all package names declared in a parsed [`Srcinfo`] (including split packages).
#[cfg(test)]
pub fn srcinfo_pkgnames(info: &Srcinfo) -> Vec<String> {
    info.pkgs.iter().map(|pkg| pkg.pkgname.clone()).collect()
}

/// Parses `.SRCINFO` bytes using [`srcinfo::Srcinfo::from_buf`] (never executes `PKGBUILD`).
pub fn parse_srcinfo_bytes(pkgbase: &str, bytes: &[u8]) -> Result<Srcinfo, AurGitError> {
    Srcinfo::from_buf(BufReader::new(bytes)).map_err(|source| AurGitError::Srcinfo {
        pkgbase: pkgbase.to_string(),
        source,
    })
}

/// Concurrently clones or fetches all `targets` under `$XDG_CACHE_HOME/pom/clone/<pkgbase>`.
pub async fn sync_aur_repos(
    targets: &[AurRepoTarget],
    progress_tx: Option<&mpsc::UnboundedSender<GitSyncProgress>>,
) -> Result<Vec<SyncedRepo>, AurGitError> {
    let cache_dir = aur_cache_dir();
    sync_aur_repos_in_dir(targets, &cache_dir, progress_tx).await
}

/// Concurrently clones or fetches all `targets` into `cache_dir/<pkgbase>`.
pub async fn sync_aur_repos_in_dir(
    targets: &[AurRepoTarget],
    cache_dir: &Path,
    progress_tx: Option<&mpsc::UnboundedSender<GitSyncProgress>>,
) -> Result<Vec<SyncedRepo>, AurGitError> {
    tokio::fs::create_dir_all(cache_dir).await?;

    let results: Vec<Result<SyncedRepo, AurGitError>> = stream::iter(targets.iter().cloned())
        .map(|target| async move {
            if let Some(tx) = progress_tx {
                let _ = tx.send(GitSyncProgress::Cloning {
                    pkgbase: target.pkgbase.clone(),
                });
            }

            let res = sync_single_repo(&target, cache_dir).await;
            if let Some(tx) = progress_tx {
                match &res {
                    Ok(_) => {
                        let _ = tx.send(GitSyncProgress::Synced {
                            pkgbase: target.pkgbase.clone(),
                        });
                    }
                    Err(err) => {
                        let _ = tx.send(GitSyncProgress::Failed {
                            pkgbase: target.pkgbase.clone(),
                            error: err.to_string(),
                        });
                    }
                }
            }
            res
        })
        .buffer_unordered(DEFAULT_GIT_CONCURRENCY)
        .collect()
        .await;

    let mut synced = Vec::with_capacity(results.len());
    for res in results {
        synced.push(res?);
    }
    Ok(synced)
}

async fn sync_single_repo(
    target: &AurRepoTarget,
    cache_dir: &Path,
) -> Result<SyncedRepo, AurGitError> {
    let repo_dir = cache_dir.join(&target.pkgbase);
    let is_new_clone = !repo_dir.join(".git").exists();

    if is_new_clone {
        if repo_dir.exists() {
            tokio::fs::remove_dir_all(&repo_dir).await?;
        }
        let url = target.effective_url();
        run_git(
            &target.pkgbase,
            "clone",
            &[
                OsStr::new("clone"),
                OsStr::new("--no-progress"),
                OsStr::new(&url),
                repo_dir.as_os_str(),
            ],
            None,
        )
        .await?;
    } else {
        run_git(
            &target.pkgbase,
            "fetch",
            &[
                OsStr::new("-C"),
                repo_dir.as_os_str(),
                OsStr::new("fetch"),
                OsStr::new("--no-progress"),
                OsStr::new("origin"),
            ],
            None,
        )
        .await?;
    }

    ensure_origin_head(&target.pkgbase, &repo_dir).await?;
    let head_commit = rev_parse(&target.pkgbase, &repo_dir, "origin/HEAD").await?;

    let srcinfo_bytes = git_show_bytes(&target.pkgbase, &repo_dir, "origin/HEAD:.SRCINFO").await?;
    let srcinfo = parse_srcinfo_bytes(&target.pkgbase, &srcinfo_bytes)?;

    Ok(SyncedRepo {
        pkgbase: target.pkgbase.clone(),
        dir: repo_dir,
        head_commit,
        srcinfo,
        is_new_clone,
    })
}

async fn ensure_origin_head(pkgbase: &str, repo_dir: &Path) -> Result<(), AurGitError> {
    if rev_parse_optional(repo_dir, "origin/HEAD").await?.is_some() {
        return Ok(());
    }
    let _ = run_git(
        pkgbase,
        "remote set-head",
        &[
            OsStr::new("-C"),
            repo_dir.as_os_str(),
            OsStr::new("remote"),
            OsStr::new("set-head"),
            OsStr::new("origin"),
            OsStr::new("--auto"),
        ],
        None,
    )
    .await;
    Ok(())
}

/// Collects unreviewed diffs for `repos` by comparing `refs/pom/reviewed` against `origin/HEAD`.
///
/// - If `refs/pom/reviewed` does not exist, formats a full listing of tracked files in
///   `origin/HEAD` (excluding `.SRCINFO`).
/// - If `refs/pom/reviewed != origin/HEAD`, runs
///   `git diff --color=always refs/pom/reviewed..origin/HEAD -- . ':!.SRCINFO'`.
/// - If `refs/pom/reviewed == origin/HEAD`, the repository is already reviewed and omitted.
pub async fn collect_unreviewed_diffs(repos: &[SyncedRepo]) -> Result<Vec<RepoDiff>, AurGitError> {
    let mut diffs = Vec::new();

    for repo in repos {
        let reviewed = rev_parse_optional(&repo.dir, REVIEWED_REF).await?;
        match reviewed {
            None => {
                let diff_text = format_new_package_listing(repo).await?;
                diffs.push(RepoDiff {
                    pkgbase: repo.pkgbase.clone(),
                    repo_dir: repo.dir.clone(),
                    target_commit: repo.head_commit.clone(),
                    kind: DiffKind::New,
                    diff_text,
                });
            }
            Some(reviewed_sha) if reviewed_sha == repo.head_commit => {}
            Some(_) => {
                let color_arg = if use_diff_color() {
                    "--color=always"
                } else {
                    "--color=never"
                };
                let diff_range = format!("{REVIEWED_REF}..origin/HEAD");
                let out = run_git(
                    &repo.pkgbase,
                    "diff",
                    &[
                        OsStr::new("-C"),
                        repo.dir.as_os_str(),
                        OsStr::new("diff"),
                        OsStr::new(color_arg),
                        OsStr::new(&diff_range),
                        OsStr::new("--"),
                        OsStr::new("."),
                        OsStr::new(":!.SRCINFO"),
                    ],
                    None,
                )
                .await?;

                let diff_text = String::from_utf8_lossy(&out).into_owned();
                if !diff_text.trim().is_empty() {
                    let header = format!(
                        "=== UPDATED AUR PACKAGE: {} ({}) ===\n",
                        repo.pkgbase, repo.head_commit
                    );
                    diffs.push(RepoDiff {
                        pkgbase: repo.pkgbase.clone(),
                        repo_dir: repo.dir.clone(),
                        target_commit: repo.head_commit.clone(),
                        kind: DiffKind::Updated,
                        diff_text: format!("{header}{diff_text}"),
                    });
                }
            }
        }
    }

    Ok(diffs)
}

async fn format_new_package_listing(repo: &SyncedRepo) -> Result<String, AurGitError> {
    let files_raw = run_git(
        &repo.pkgbase,
        "ls-tree",
        &[
            OsStr::new("-C"),
            repo.dir.as_os_str(),
            OsStr::new("ls-tree"),
            OsStr::new("-r"),
            OsStr::new("--name-only"),
            OsStr::new("origin/HEAD"),
        ],
        None,
    )
    .await?;

    let files_str = String::from_utf8_lossy(&files_raw);
    let mut output = format!(
        "=== NEW AUR PACKAGE: {} ({}) ===\n",
        repo.pkgbase, repo.head_commit
    );

    for file in files_str
        .lines()
        .filter(|f| !f.is_empty() && *f != ".SRCINFO")
    {
        let spec = format!("origin/HEAD:{file}");
        let content = git_show_bytes(&repo.pkgbase, &repo.dir, &spec).await?;
        output.push_str(&format!("\n--- {file} ---\n"));
        output.push_str(&String::from_utf8_lossy(&content));
        if !output.ends_with('\n') {
            output.push('\n');
        }
    }

    Ok(output)
}

/// Records `commit` in `refs/pom/reviewed` and checks it out into `repo_dir`.
pub async fn mark_reviewed(repo_dir: &Path, commit: &str) -> Result<(), AurGitError> {
    let pkgbase = repo_dir
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("unknown");

    run_git(
        pkgbase,
        "update-ref",
        &[
            OsStr::new("-C"),
            repo_dir.as_os_str(),
            OsStr::new("update-ref"),
            OsStr::new(REVIEWED_REF),
            OsStr::new(commit),
        ],
        None,
    )
    .await?;

    checkout_commit(pkgbase, repo_dir, commit).await
}

async fn checkout_commit(pkgbase: &str, repo_dir: &Path, commit: &str) -> Result<(), AurGitError> {
    run_git(
        pkgbase,
        "checkout",
        &[
            OsStr::new("-C"),
            repo_dir.as_os_str(),
            OsStr::new("checkout"),
            OsStr::new("--force"),
            OsStr::new(commit),
        ],
        None,
    )
    .await?;
    Ok(())
}

/// Presents unreviewed diffs in `$PAGER` (fallback `less -R`) and prompts `[y/N]`.
///
/// On `--noconfirm`, logs a warning and returns `Ok(true)` without advancing `refs/pom/reviewed`
/// (while checking out `target_commit` so `makepkg` builds the fetched revision).
#[cfg(test)]
pub async fn present_pkgbuild_review(
    diffs: &[RepoDiff],
    noconfirm: bool,
) -> Result<bool, AurGitError> {
    present_pkgbuild_review_impl(diffs, noconfirm, None::<&NoopSuspender>).await
}

/// Variant of [`present_pkgbuild_review`] that suspends an active TUI around interactive I/O.
pub async fn present_pkgbuild_review_with_tui<S: ReviewUiSuspender>(
    diffs: &[RepoDiff],
    noconfirm: bool,
    suspender: &S,
) -> Result<bool, AurGitError> {
    present_pkgbuild_review_impl(diffs, noconfirm, Some(suspender)).await
}

#[cfg(test)]
struct NoopSuspender;

#[cfg(test)]
#[async_trait::async_trait]
impl ReviewUiSuspender for NoopSuspender {
    type Guard = ();
    async fn suspend(&self) -> Self::Guard {}
}

async fn present_pkgbuild_review_impl<S: ReviewUiSuspender>(
    diffs: &[RepoDiff],
    noconfirm: bool,
    suspender: Option<&S>,
) -> Result<bool, AurGitError> {
    if diffs.is_empty() {
        return Ok(true);
    }

    if noconfirm {
        let names: Vec<&str> = diffs.iter().map(|d| d.pkgbase.as_str()).collect();
        eprintln!(
            "{} skipping PKGBUILD review for {} (--noconfirm)",
            "warning:".yellow().bold(),
            names.join(", ")
        );
        for diff in diffs {
            checkout_commit(&diff.pkgbase, &diff.repo_dir, &diff.target_commit).await?;
        }
        return Ok(true);
    }

    let _guard = match suspender {
        Some(s) => Some(s.suspend().await),
        None => None,
    };

    let temp_diff = write_temp_diff_file(diffs)?;
    spawn_pager(&temp_diff.path).await?;
    drop(temp_diff);

    let approved = tokio::task::spawn_blocking(prompt_review_confirmation)
        .await
        .map_err(io::Error::other)??;

    if approved {
        for diff in diffs {
            mark_reviewed(&diff.repo_dir, &diff.target_commit).await?;
        }
    }

    Ok(approved)
}

struct TempDiffFile {
    path: PathBuf,
}

impl Drop for TempDiffFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn write_temp_diff_file(diffs: &[RepoDiff]) -> Result<TempDiffFile, AurGitError> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!("pom-review-{}-{nanos}.diff", std::process::id()));

    let mut file = std::fs::File::create(&path)?;
    for (idx, diff) in diffs.iter().enumerate() {
        if idx > 0 {
            writeln!(file)?;
        }
        write!(file, "{}", diff.diff_text)?;
        if !diff.diff_text.ends_with('\n') {
            writeln!(file)?;
        }
    }
    file.flush()?;

    Ok(TempDiffFile { path })
}

async fn spawn_pager(diff_path: &Path) -> Result<(), AurGitError> {
    let pager_cmd = std::env::var("PAGER")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "less -R".to_string());

    let parts: Vec<&str> = pager_cmd.split_whitespace().collect();
    let (program, args) = parts
        .split_first()
        .ok_or_else(|| AurGitError::PagerFailed {
            pager: pager_cmd.clone(),
            status: None,
        })?;

    let status = Command::new(program)
        .args(args)
        .arg(diff_path)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await?;

    if !status.success() {
        return Err(AurGitError::PagerFailed {
            pager: pager_cmd,
            status: status.code(),
        });
    }
    Ok(())
}

fn prompt_review_confirmation() -> io::Result<bool> {
    let mut stderr = io::stderr().lock();
    write!(stderr, "Proceed with building reviewed PKGBUILDs? [y/N] ")?;
    stderr.flush()?;

    let mut line = String::new();
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    reader.read_line(&mut line)?;
    Ok(parse_confirmation(&line))
}

/// Parses a `[y/N]` confirmation response (`y` / `yes` case-insensitive -> `true`).
pub fn parse_confirmation(input: &str) -> bool {
    matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn use_diff_color() -> bool {
    std::env::var_os("NO_COLOR").is_none()
}

async fn run_git(
    pkgbase: &str,
    command_name: &str,
    args: &[&OsStr],
    cwd: Option<&Path>,
) -> Result<Vec<u8>, AurGitError> {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    let output = cmd.output().await?;
    if !output.status.success() {
        return Err(AurGitError::Git {
            pkgbase: pkgbase.to_string(),
            command: command_name.to_string(),
            status: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(output.stdout)
}

async fn rev_parse(pkgbase: &str, repo_dir: &Path, rev: &str) -> Result<String, AurGitError> {
    let out = run_git(
        pkgbase,
        "rev-parse",
        &[
            OsStr::new("-C"),
            repo_dir.as_os_str(),
            OsStr::new("rev-parse"),
            OsStr::new("--verify"),
            OsStr::new(rev),
        ],
        None,
    )
    .await?;
    Ok(String::from_utf8_lossy(&out).trim().to_string())
}

async fn rev_parse_optional(repo_dir: &Path, rev: &str) -> Result<Option<String>, AurGitError> {
    let output = Command::new("git")
        .args([
            OsStr::new("-C"),
            repo_dir.as_os_str(),
            OsStr::new("rev-parse"),
            OsStr::new("--verify"),
            OsStr::new("--quiet"),
            OsStr::new(rev),
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await?;

    if output.status.success() {
        Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        ))
    } else {
        Ok(None)
    }
}

async fn git_show_bytes(
    pkgbase: &str,
    repo_dir: &Path,
    spec: &str,
) -> Result<Vec<u8>, AurGitError> {
    run_git(
        pkgbase,
        "show",
        &[
            OsStr::new("-C"),
            repo_dir.as_os_str(),
            OsStr::new("show"),
            OsStr::new(spec),
        ],
        None,
    )
    .await
    .map_err(|err| match err {
        AurGitError::Git { .. } if spec.ends_with(":.SRCINFO") => AurGitError::MissingSrcinfo {
            pkgbase: pkgbase.to_string(),
            path: repo_dir.join(".SRCINFO"),
        },
        other => other,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;
    use tempfile::TempDir;

    fn run_std_git(dir: &Path, args: &[&str]) {
        let output = StdCommand::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .output()
            .expect("failed to execute git");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn setup_bare_aur_repo(tmp: &Path, pkgbase: &str) -> (PathBuf, PathBuf) {
        let bare_dir = tmp.join(format!("{pkgbase}.git"));
        let seed_dir = tmp.join(format!("{pkgbase}-seed"));
        std::fs::create_dir_all(&bare_dir).unwrap();
        std::fs::create_dir_all(&seed_dir).unwrap();

        run_std_git(&bare_dir, &["init", "--bare", "--initial-branch=master"]);
        run_std_git(&seed_dir, &["init", "--initial-branch=master"]);
        run_std_git(&seed_dir, &["config", "user.email", "test@example.com"]);
        run_std_git(&seed_dir, &["config", "user.name", "Test User"]);

        std::fs::write(
            seed_dir.join("PKGBUILD"),
            "pkgname=demo-pkg\npkgver=1.0.0\npkgrel=1\narch=('any')\n",
        )
        .unwrap();
        std::fs::write(
            seed_dir.join(".SRCINFO"),
            "pkgbase = demo-pkg\n\tpkgver = 1.0.0\n\tpkgrel = 1\n\tarch = any\n\npkgname = demo-pkg\n",
        )
        .unwrap();

        run_std_git(&seed_dir, &["add", "PKGBUILD", ".SRCINFO"]);
        run_std_git(&seed_dir, &["commit", "-m", "initial import"]);
        run_std_git(
            &seed_dir,
            &["remote", "add", "origin", bare_dir.to_str().unwrap()],
        );
        run_std_git(&seed_dir, &["push", "-u", "origin", "master"]);

        (bare_dir, seed_dir)
    }

    #[test]
    fn parse_confirmation_handles_yes_and_default_no() {
        assert!(parse_confirmation("y"));
        assert!(parse_confirmation("Y"));
        assert!(parse_confirmation("yes"));
        assert!(parse_confirmation("  YES \n"));
        assert!(!parse_confirmation(""));
        assert!(!parse_confirmation("\n"));
        assert!(!parse_confirmation("n"));
        assert!(!parse_confirmation("no"));
    }

    #[test]
    fn parse_srcinfo_extracts_split_pkgnames() {
        let raw = b"\
pkgbase = my-split
\tpkgver = 2.0.0
\tpkgrel = 1
\tarch = x86_64

pkgname = my-split-bin

pkgname = my-split-docs
";
        let info = parse_srcinfo_bytes("my-split", raw).unwrap();
        assert_eq!(info.base.pkgbase, "my-split");
        assert_eq!(
            srcinfo_pkgnames(&info),
            vec!["my-split-bin".to_string(), "my-split-docs".to_string()]
        );
    }

    #[tokio::test]
    async fn clone_review_and_update_diff_lifecycle() {
        let tmp = TempDir::new().unwrap();
        let (bare_dir, seed_dir) = setup_bare_aur_repo(tmp.path(), "demo-pkg");
        let cache_dir = tmp.path().join("cache");

        let targets = vec![AurRepoTarget::with_remote(
            "demo-pkg",
            bare_dir.to_str().unwrap(),
        )];

        let (tx, mut rx) = mpsc::unbounded_channel();
        let synced = sync_aur_repos_in_dir(&targets, &cache_dir, Some(&tx))
            .await
            .unwrap();
        assert_eq!(synced.len(), 1);
        assert!(synced[0].is_new_clone);
        assert_eq!(synced[0].srcinfo.base.pkgbase, "demo-pkg");
        assert_eq!(srcinfo_pkgnames(&synced[0].srcinfo), vec!["demo-pkg"]);

        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        assert!(matches!(
            events.as_slice(),
            [
                GitSyncProgress::Cloning { .. },
                GitSyncProgress::Synced { .. }
            ]
        ));

        // Initial state: no refs/pom/reviewed -> New diff with PKGBUILD and no .SRCINFO.
        let diffs = collect_unreviewed_diffs(&synced).await.unwrap();
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].kind, DiffKind::New);
        assert!(diffs[0].diff_text.contains("=== NEW AUR PACKAGE: demo-pkg"));
        assert!(diffs[0].diff_text.contains("pkgver=1.0.0"));
        assert!(!diffs[0].diff_text.contains("pkgbase = demo-pkg"));

        // --noconfirm skips review without advancing refs/pom/reviewed.
        let approved_noconfirm = present_pkgbuild_review(&diffs, true).await.unwrap();
        assert!(approved_noconfirm);
        assert!(
            rev_parse_optional(&synced[0].dir, REVIEWED_REF)
                .await
                .unwrap()
                .is_none()
        );

        // Marking reviewed updates refs/pom/reviewed and clears unreviewed diffs.
        mark_reviewed(&synced[0].dir, &synced[0].head_commit)
            .await
            .unwrap();
        let after_review = collect_unreviewed_diffs(&synced).await.unwrap();
        assert!(after_review.is_empty());

        // Push an upstream update modifying both PKGBUILD and .SRCINFO.
        std::fs::write(
            seed_dir.join("PKGBUILD"),
            "pkgname=demo-pkg\npkgver=1.1.0\npkgrel=1\narch=('any')\n",
        )
        .unwrap();
        std::fs::write(
            seed_dir.join(".SRCINFO"),
            "pkgbase = demo-pkg\n\tpkgver = 1.1.0\n\tpkgrel = 1\n\tarch = any\n\npkgname = demo-pkg\n",
        )
        .unwrap();
        run_std_git(&seed_dir, &["add", "PKGBUILD", ".SRCINFO"]);
        run_std_git(&seed_dir, &["commit", "-m", "bump to 1.1.0"]);
        run_std_git(&seed_dir, &["push", "origin", "master"]);

        let synced_updated = sync_aur_repos_in_dir(&targets, &cache_dir, None)
            .await
            .unwrap();
        assert!(!synced_updated[0].is_new_clone);
        assert_eq!(synced_updated[0].srcinfo.base.pkgver, "1.1.0");

        let update_diffs = collect_unreviewed_diffs(&synced_updated).await.unwrap();
        assert_eq!(update_diffs.len(), 1);
        assert_eq!(update_diffs[0].kind, DiffKind::Updated);
        assert!(update_diffs[0].diff_text.contains("pkgver=1.1.0"));
        assert!(!update_diffs[0].diff_text.contains("pkgbase = demo-pkg"));
    }
}
