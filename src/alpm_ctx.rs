//! Safe wrapper around `libalpm` and `PackageDatabase` abstraction.
//!
//! Decouples `libalpm` queries behind [`PackageDatabase`] so downstream modules
//! (`aur::rpc`, `ui::summary`, and unit tests) can operate without host
//! `/var/lib/pacman` state, while [`AlpmCtx`] provides the production
//! `pacmanconf` + `alpm_utils::alpm_with_conf` implementation.

use std::cmp::Ordering;
#[cfg(test)]
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::ops::Deref;
use std::sync::{Mutex, MutexGuard};

use alpm::{Alpm, Package};
use pacmanconf::Config;

/// Errors returned when initializing or reloading [`AlpmCtx`].
#[derive(Debug, thiserror::Error)]
pub enum AlpmCtxError {
    /// Failed to read or parse `/etc/pacman.conf`.
    #[error("failed to parse pacman.conf: {0}")]
    PacmanConf(#[from] pacmanconf::Error),

    /// `libalpm` failed to initialize or register databases.
    #[error("libalpm error: {0}")]
    Alpm(#[from] alpm::Error),
}

/// Summary metadata for a package in a sync repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkgSummary {
    pub repo: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub download_size: i64,
    pub installed_size: i64,
    pub installed_version: Option<String>,
}

/// An available official-repository upgrade (or downgrade) for an installed package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoUpgrade {
    pub repo: String,
    pub name: String,
    pub old_version: String,
    pub new_version: String,
    pub download_size: i64,
    pub net_size_delta: i64,
}

/// Abstraction over local and sync package databases.
pub trait PackageDatabase: Send + Sync {
    /// Returns the installed version of `name`, if present in the local database.
    fn local_version(&self, name: &str) -> Option<String>;

    /// Looks up `name` (supporting optional `repo/pkg` prefix) in sync databases.
    #[allow(dead_code)]
    fn sync_pkg(&self, name: &str) -> Option<PkgSummary>;

    /// Searches sync databases for packages matching all `terms` (case-insensitive).
    fn search_sync(&self, terms: &[String]) -> Vec<PkgSummary>;

    /// Computes official-repository upgrades (and downgrades if `allow_downgrade` is true),
    /// excluding any packages listed in `ignore` or configured as ignored.
    fn repo_upgrades(&self, ignore: &[String], allow_downgrade: bool) -> Vec<RepoUpgrade>;

    /// Returns `(name, version)` pairs for locally installed packages not found in any sync repo.
    #[allow(dead_code)]
    fn foreign_packages(&self) -> Vec<(String, String)>;

    /// Compares two package version strings using `libalpm`'s `vercmp` algorithm.
    #[allow(dead_code)]
    fn cmp_versions(&self, a: &str, b: &str) -> Ordering;
}

/// Wrapper around `alpm::Alpm` allowing thread-safe access when guarded by a [`Mutex`].
///
/// `libalpm` handles have no thread-local state; all state lives inside `alpm_handle_t`,
/// and the surrounding [`Mutex`] ensures exclusive access across threads.
struct SendAlpm(Alpm);

// SAFETY: Access to the inner `Alpm` handle is serialized by `AlpmCtx::alpm`'s `Mutex`.
unsafe impl Send for SendAlpm {}

/// RAII guard providing shared access to the underlying [`alpm::Alpm`] handle.
pub struct AlpmGuard<'a>(MutexGuard<'a, SendAlpm>);

impl Deref for AlpmGuard<'_> {
    type Target = Alpm;

    fn deref(&self) -> &Self::Target {
        &self.0.0
    }
}

/// Production [`PackageDatabase`] backed by `pacmanconf::Config` and `alpm::Alpm`.
pub struct AlpmCtx {
    conf: Config,
    alpm: Mutex<SendAlpm>,
}

impl AlpmCtx {
    /// Loads `/etc/pacman.conf` and initializes a `libalpm` handle.
    pub fn from_pacman_conf() -> Result<Self, AlpmCtxError> {
        let conf = Config::new()?;
        Self::from_config(conf)
    }

    /// Loads a `pacman.conf` from `path` and initializes a `libalpm` handle.
    #[allow(dead_code)]
    pub fn from_conf_file<P: AsRef<OsStr>>(path: P) -> Result<Self, AlpmCtxError> {
        let conf = Config::from_file(path)?;
        Self::from_config(conf)
    }

    /// Initializes a `libalpm` handle from an already-parsed [`pacmanconf::Config`].
    pub fn from_config(conf: Config) -> Result<Self, AlpmCtxError> {
        let alpm = alpm_utils::alpm_with_conf(&conf)?;
        Ok(Self {
            conf,
            alpm: Mutex::new(SendAlpm(alpm)),
        })
    }

    /// Re-initializes the underlying `libalpm` handle so newly synced `.db` files are visible.
    #[allow(dead_code)]
    pub fn reopen(&mut self) -> Result<(), AlpmCtxError> {
        let new_alpm = alpm_utils::alpm_with_conf(&self.conf)?;
        *self.alpm.get_mut().unwrap_or_else(|e| e.into_inner()) = SendAlpm(new_alpm);
        Ok(())
    }

    /// Returns the parsed `pacman.conf` configuration.
    #[allow(dead_code)]
    pub fn config(&self) -> &Config {
        &self.conf
    }

    /// Acquires exclusive access to the underlying [`alpm::Alpm`] handle.
    pub fn alpm(&self) -> AlpmGuard<'_> {
        AlpmGuard(self.alpm.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn lock_alpm(&self) -> MutexGuard<'_, SendAlpm> {
        self.alpm.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn pkg_to_summary(repo: &str, pkg: &Package, localdb: &alpm::Db) -> PkgSummary {
    let installed_version = localdb
        .pkg(pkg.name())
        .ok()
        .map(|p| p.version().to_string());
    PkgSummary {
        repo: repo.to_string(),
        name: pkg.name().to_string(),
        version: pkg.version().to_string(),
        description: pkg.desc().unwrap_or_default().to_string(),
        download_size: pkg.download_size(),
        installed_size: pkg.isize(),
        installed_version,
    }
}

fn is_ignored(pkg: &Package, cli_ignore: &[String], conf: &Config) -> bool {
    let name = pkg.name();
    if cli_ignore.iter().any(|i| i == name) || conf.ignore_pkg.iter().any(|i| i == name) {
        return true;
    }
    pkg.groups()
        .iter()
        .any(|g| conf.ignore_group.iter().any(|ig| ig == g))
}

impl PackageDatabase for AlpmCtx {
    fn local_version(&self, name: &str) -> Option<String> {
        let guard = self.lock_alpm();
        guard
            .0
            .localdb()
            .pkg(name)
            .ok()
            .map(|p| p.version().to_string())
    }

    fn sync_pkg(&self, name: &str) -> Option<PkgSummary> {
        let guard = self.lock_alpm();
        let alpm = &guard.0;
        let localdb = alpm.localdb();

        if let Some((repo_name, pkg_name)) = name.split_once('/') {
            let db = alpm.syncdbs().iter().find(|db| db.name() == repo_name)?;
            let pkg = db.pkg(pkg_name).ok()?;
            return Some(pkg_to_summary(db.name(), pkg, localdb));
        }

        for db in alpm.syncdbs() {
            if let Ok(pkg) = db.pkg(name) {
                return Some(pkg_to_summary(db.name(), pkg, localdb));
            }
        }
        None
    }

    fn search_sync(&self, terms: &[String]) -> Vec<PkgSummary> {
        if terms.is_empty() {
            return Vec::new();
        }

        let guard = self.lock_alpm();
        let alpm = &guard.0;
        let localdb = alpm.localdb();
        let term_refs: Vec<&str> = terms.iter().map(String::as_str).collect();

        let mut results = Vec::new();
        for db in alpm.syncdbs() {
            if let Ok(pkgs) = db.search(term_refs.iter().copied()) {
                for pkg in pkgs {
                    results.push(pkg_to_summary(db.name(), pkg, localdb));
                }
            }
        }
        results
    }

    fn repo_upgrades(&self, ignore: &[String], allow_downgrade: bool) -> Vec<RepoUpgrade> {
        let guard = self.lock_alpm();
        let alpm = &guard.0;
        let syncdbs = alpm.syncdbs();
        let mut upgrades = Vec::new();

        for local_pkg in alpm.localdb().pkgs() {
            if is_ignored(local_pkg, ignore, &self.conf) {
                continue;
            }

            // First sync repository containing the package takes precedence (pacman repo order).
            let Some((repo_name, sync_pkg)) = syncdbs
                .iter()
                .find_map(|db| db.pkg(local_pkg.name()).ok().map(|p| (db.name(), p)))
            else {
                continue;
            };

            let cmp = alpm::vercmp(sync_pkg.version().as_str(), local_pkg.version().as_str());
            let include = match cmp {
                Ordering::Greater => true,
                Ordering::Less => allow_downgrade,
                Ordering::Equal => false,
            };

            if include {
                upgrades.push(RepoUpgrade {
                    repo: repo_name.to_string(),
                    name: local_pkg.name().to_string(),
                    old_version: local_pkg.version().to_string(),
                    new_version: sync_pkg.version().to_string(),
                    download_size: sync_pkg.download_size(),
                    net_size_delta: sync_pkg.isize() - local_pkg.isize(),
                });
            }
        }

        upgrades
    }

    fn foreign_packages(&self) -> Vec<(String, String)> {
        let guard = self.lock_alpm();
        let alpm = &guard.0;
        let syncdbs = alpm.syncdbs();

        alpm.localdb()
            .pkgs()
            .iter()
            .filter(|pkg| syncdbs.iter().all(|db| db.pkg(pkg.name()).is_err()))
            .map(|pkg| (pkg.name().to_string(), pkg.version().to_string()))
            .collect()
    }

    fn cmp_versions(&self, a: &str, b: &str) -> Ordering {
        alpm::vercmp(a, b)
    }
}

/// In-memory mock implementation of [`PackageDatabase`] for unit testing.
#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub struct MockDatabase {
    local_pkgs: BTreeMap<String, (String, i64)>,
    sync_pkgs: Vec<PkgSummary>,
    ignore_pkgs: Vec<String>,
}

#[cfg(test)]
impl MockDatabase {
    /// Creates an empty [`MockDatabase`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder method adding an installed local package with zero installed size.
    pub fn with_local(mut self, name: impl Into<String>, version: impl Into<String>) -> Self {
        self.insert_local(name, version, 0);
        self
    }

    /// Builder method adding an installed local package with an explicit installed size.
    pub fn with_local_sized(
        mut self,
        name: impl Into<String>,
        version: impl Into<String>,
        installed_size: i64,
    ) -> Self {
        self.insert_local(name, version, installed_size);
        self
    }

    /// Builder method adding a sync repository package.
    pub fn with_sync(mut self, pkg: PkgSummary) -> Self {
        self.insert_sync(pkg);
        self
    }

    /// Builder method adding a package name to the configured ignore list.
    pub fn with_ignore(mut self, name: impl Into<String>) -> Self {
        self.ignore_pkgs.push(name.into());
        self
    }

    /// Inserts or updates a local package entry.
    pub fn insert_local(
        &mut self,
        name: impl Into<String>,
        version: impl Into<String>,
        installed_size: i64,
    ) {
        self.local_pkgs
            .insert(name.into(), (version.into(), installed_size));
    }

    /// Appends a sync package entry (preserving repository priority order).
    pub fn insert_sync(&mut self, pkg: PkgSummary) {
        self.sync_pkgs.push(pkg);
    }

    fn annotate_installed(&self, mut pkg: PkgSummary) -> PkgSummary {
        pkg.installed_version = self.local_version(&pkg.name);
        pkg
    }
}

#[cfg(test)]
impl PackageDatabase for MockDatabase {
    fn local_version(&self, name: &str) -> Option<String> {
        self.local_pkgs.get(name).map(|(v, _)| v.clone())
    }

    fn sync_pkg(&self, name: &str) -> Option<PkgSummary> {
        if let Some((repo_name, pkg_name)) = name.split_once('/') {
            return self
                .sync_pkgs
                .iter()
                .find(|p| p.repo == repo_name && p.name == pkg_name)
                .cloned()
                .map(|p| self.annotate_installed(p));
        }

        self.sync_pkgs
            .iter()
            .find(|p| p.name == name)
            .cloned()
            .map(|p| self.annotate_installed(p))
    }

    fn search_sync(&self, terms: &[String]) -> Vec<PkgSummary> {
        if terms.is_empty() {
            return Vec::new();
        }

        let lowered_terms: Vec<String> = terms.iter().map(|t| t.to_lowercase()).collect();
        self.sync_pkgs
            .iter()
            .filter(|pkg| {
                let name = pkg.name.to_lowercase();
                let desc = pkg.description.to_lowercase();
                lowered_terms
                    .iter()
                    .all(|term| name.contains(term) || desc.contains(term))
            })
            .cloned()
            .map(|p| self.annotate_installed(p))
            .collect()
    }

    fn repo_upgrades(&self, ignore: &[String], allow_downgrade: bool) -> Vec<RepoUpgrade> {
        let mut upgrades = Vec::new();

        for (name, (local_ver, local_size)) in &self.local_pkgs {
            if ignore.iter().any(|i| i == name) || self.ignore_pkgs.iter().any(|i| i == name) {
                continue;
            }

            let Some(sync_pkg) = self.sync_pkgs.iter().find(|p| &p.name == name) else {
                continue;
            };

            let cmp = alpm::vercmp(sync_pkg.version.as_str(), local_ver.as_str());
            let include = match cmp {
                Ordering::Greater => true,
                Ordering::Less => allow_downgrade,
                Ordering::Equal => false,
            };

            if include {
                upgrades.push(RepoUpgrade {
                    repo: sync_pkg.repo.clone(),
                    name: name.clone(),
                    old_version: local_ver.clone(),
                    new_version: sync_pkg.version.clone(),
                    download_size: sync_pkg.download_size,
                    net_size_delta: sync_pkg.installed_size - *local_size,
                });
            }
        }

        upgrades
    }

    fn foreign_packages(&self) -> Vec<(String, String)> {
        self.local_pkgs
            .iter()
            .filter(|(name, _)| !self.sync_pkgs.iter().any(|s| &s.name == *name))
            .map(|(name, (ver, _))| (name.clone(), ver.clone()))
            .collect()
    }

    fn cmp_versions(&self, a: &str, b: &str) -> Ordering {
        alpm::vercmp(a, b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_sync_pkg(repo: &str, name: &str, version: &str, desc: &str) -> PkgSummary {
        PkgSummary {
            repo: repo.to_string(),
            name: name.to_string(),
            version: version.to_string(),
            description: desc.to_string(),
            download_size: 1024,
            installed_size: 4096,
            installed_version: None,
        }
    }

    #[test]
    fn search_sync_matches_multi_term_and_annotates_installed() {
        let db = MockDatabase::new()
            .with_local("ripgrep", "14.1.0-1")
            .with_sync(sample_sync_pkg(
                "extra",
                "ripgrep",
                "14.1.1-1",
                "A search tool that combines the usability of ag with the raw speed of grep",
            ))
            .with_sync(sample_sync_pkg(
                "extra",
                "fd",
                "10.2.0-1",
                "Simple, fast and user-friendly alternative to find",
            ))
            .with_sync(sample_sync_pkg(
                "core",
                "grep",
                "3.11-1",
                "A string search utility",
            ));

        let hits = db.search_sync(&["Search".to_string(), "GREP".to_string()]);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].name, "ripgrep");
        assert_eq!(hits[0].installed_version.as_deref(), Some("14.1.0-1"));
        assert_eq!(hits[1].name, "grep");
        assert_eq!(hits[1].installed_version, None);

        let narrow = db.search_sync(&["ag".to_string(), "GREP".to_string()]);
        assert_eq!(narrow.len(), 1);
        assert_eq!(narrow[0].name, "ripgrep");

        assert!(db.search_sync(&[]).is_empty());

        let fd = db.sync_pkg("extra/fd").expect("extra/fd should resolve");
        assert_eq!(fd.name, "fd");
        assert_eq!(fd.installed_version, None);
        assert!(db.sync_pkg("core/fd").is_none());
    }

    #[test]
    fn repo_upgrades_handles_upgrades_downgrades_and_repo_priority() {
        let db = MockDatabase::new()
            .with_local_sized("linux", "6.11.1-1", 1000)
            .with_local_sized("mesa", "24.2.0-1", 2000)
            .with_local_sized("downgrade-me", "2.0.0-1", 500)
            // testing repo appears before extra, so testing shadows extra
            .with_sync(PkgSummary {
                repo: "testing".to_string(),
                name: "linux".to_string(),
                version: "6.12.0-1".to_string(),
                description: String::new(),
                download_size: 120_000,
                installed_size: 1500,
                installed_version: None,
            })
            .with_sync(sample_sync_pkg("core", "linux", "6.11.2-1", ""))
            .with_sync(sample_sync_pkg("extra", "mesa", "24.2.0-1", ""))
            .with_sync(PkgSummary {
                repo: "extra".to_string(),
                name: "downgrade-me".to_string(),
                version: "1.9.0-1".to_string(),
                description: String::new(),
                download_size: 300,
                installed_size: 400,
                installed_version: None,
            });

        let upgrades = db.repo_upgrades(&[], false);
        assert_eq!(upgrades.len(), 1);
        assert_eq!(upgrades[0].repo, "testing");
        assert_eq!(upgrades[0].name, "linux");
        assert_eq!(upgrades[0].old_version, "6.11.1-1");
        assert_eq!(upgrades[0].new_version, "6.12.0-1");
        assert_eq!(upgrades[0].net_size_delta, 500);

        let with_downgrade = db.repo_upgrades(&[], true);
        assert_eq!(with_downgrade.len(), 2);
        assert_eq!(with_downgrade[0].name, "downgrade-me");
        assert_eq!(with_downgrade[0].net_size_delta, -100);
        assert_eq!(with_downgrade[1].name, "linux");
    }

    #[test]
    fn repo_upgrades_respects_ignore_lists() {
        let db = MockDatabase::new()
            .with_local("linux", "6.11.0-1")
            .with_local("nvidia", "560.0-1")
            .with_local("vim", "9.1.0-1")
            .with_ignore("nvidia")
            .with_sync(sample_sync_pkg("core", "linux", "6.12.0-1", ""))
            .with_sync(sample_sync_pkg("extra", "nvidia", "565.0-1", ""))
            .with_sync(sample_sync_pkg("extra", "vim", "9.1.1-1", ""));

        let upgrades = db.repo_upgrades(&["linux".to_string()], false);
        assert_eq!(upgrades.len(), 1);
        assert_eq!(upgrades[0].name, "vim");
    }

    #[test]
    fn foreign_packages_excludes_sync_packages() {
        let db = MockDatabase::new()
            .with_local("pacman", "7.1.0-1")
            .with_local("yay-bin", "12.4.2-1")
            .with_sync(sample_sync_pkg("core", "pacman", "7.1.0-1", ""));

        assert_eq!(
            db.foreign_packages(),
            vec![("yay-bin".to_string(), "12.4.2-1".to_string())]
        );
    }

    #[test]
    fn cmp_versions_uses_alpm_vercmp() {
        let db = MockDatabase::new();
        assert_eq!(db.cmp_versions("1.2.0-1", "1.10.0-1"), Ordering::Less);
        assert_eq!(db.cmp_versions("1.0-2", "1.0-1"), Ordering::Greater);
        assert_eq!(db.cmp_versions("1:1.0-1", "2.0-1"), Ordering::Greater);
        assert_eq!(db.cmp_versions("1.0.0-1", "1.0.0-1"), Ordering::Equal);
    }
}
