use std::cmp::Ordering;
use std::collections::HashSet;

use anyhow::Result;
use aur_depends::{Actions, Base, Flags, Resolver};
use raur::{Cache, Raur};

#[cfg(test)]
use crate::alpm_ctx::PackageDatabase;
use crate::alpm_ctx::RepoUpgrade;

/// Kind of version change for a planned package operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Install,
    Upgrade,
    Reinstall,
    Downgrade,
}

/// Role of a package within the resolved transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallRole {
    ExplicitTarget,
    Dependency,
    MakeDependency,
}

/// A single planned package installation or upgrade (either official repo or AUR).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedPkg {
    pub name: String,
    pub source_repo: String,
    pub old_version: Option<String>,
    pub new_version: String,
    pub change: ChangeKind,
    pub role: InstallRole,
    pub download_size: i64,
    pub net_size_delta: i64,
}

/// An AUR `pkgbase` unit to be cloned, reviewed, built once, and installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedAurBase {
    pub pkgbase: String,
    pub version: String,
    pub packages: Vec<PlannedPkg>,
    pub make_only: bool,
    pub is_explicit_target: bool,
}

/// A dependency that could not be satisfied during resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingDepError {
    pub dep: String,
    pub dep_chain: Vec<String>,
}

/// A package conflict detected in the planned transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictEntry {
    pub pkg: String,
    pub conflicting_pkg: String,
    pub reason: Option<String>,
}

/// Owned, `libalpm`-lifetime-free transaction plan consumed by UI and builder layers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransactionPlan {
    pub repo_packages: Vec<PlannedPkg>,
    pub aur_bases: Vec<PlannedAurBase>,
    pub unneeded: Vec<(String, String)>,
    pub missing: Vec<MissingDepError>,
    pub conflicts: Vec<ConflictEntry>,
}

/// Options controlling dependency resolution behavior.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolveOptions {
    pub needed: bool,
    pub aur_only: bool,
    pub repo_only: bool,
    pub allow_downgrade: bool,
    pub ignore: Vec<String>,
}

fn build_flags(opts: &ResolveOptions) -> Flags {
    let mut flags = Flags::new();
    if opts.needed {
        flags |= Flags::NEEDED;
    }
    if opts.allow_downgrade {
        flags |= Flags::ENABLE_DOWNGRADE;
    }
    if opts.aur_only {
        flags &= !Flags::REPO;
    }
    if opts.repo_only {
        flags &= !(Flags::AUR | Flags::PKGBUILDS);
    }
    flags
}

fn classify_change(old_version: Option<&str>, new_version: &str) -> ChangeKind {
    match old_version {
        None => ChangeKind::Install,
        Some(old) => match alpm::vercmp(new_version, old) {
            Ordering::Greater => ChangeKind::Upgrade,
            Ordering::Less => ChangeKind::Downgrade,
            Ordering::Equal => ChangeKind::Reinstall,
        },
    }
}

fn compute_runtime_needed(actions: &Actions<'_>) -> HashSet<String> {
    let mut deps_by_pkg: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    let mut queue: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    let mut visited: HashSet<String> = HashSet::new();

    for repo_pkg in &actions.install {
        let name = repo_pkg.pkg.name().to_string();
        let deps: Vec<String> = repo_pkg
            .pkg
            .depends()
            .iter()
            .map(|d| d.name().to_string())
            .collect();
        deps_by_pkg.insert(name.clone(), deps);
        if repo_pkg.target && visited.insert(name.clone()) {
            queue.push_back(name);
        }
    }

    for aur_pkg in actions.iter_aur_pkgs() {
        let name = aur_pkg.pkg.name.clone();
        let deps: Vec<String> = aur_pkg
            .pkg
            .depends
            .iter()
            .map(|d| alpm::Depend::new(d.as_str()).name().to_string())
            .collect();
        deps_by_pkg.insert(name.clone(), deps);
        if aur_pkg.target && visited.insert(name.clone()) {
            queue.push_back(name);
        }
    }

    for (_, pb_pkg) in actions.iter_pkgbuilds() {
        let name = pb_pkg.pkg.pkgname.clone();
        let deps: Vec<String> = pb_pkg
            .pkg
            .depends
            .iter()
            .flat_map(|av| av.values().iter())
            .map(|d| alpm::Depend::new(d.as_str()).name().to_string())
            .collect();
        deps_by_pkg.insert(name.clone(), deps);
        if pb_pkg.target && visited.insert(name.clone()) {
            queue.push_back(name);
        }
    }

    while let Some(pkg) = queue.pop_front() {
        if let Some(deps) = deps_by_pkg.get(&pkg) {
            for dep in deps {
                if visited.insert(dep.clone()) {
                    queue.push_back(dep.clone());
                }
            }
        }
    }

    visited
}

fn classify_role(
    name: &str,
    target: bool,
    make: bool,
    runtime_needed: &HashSet<String>,
) -> InstallRole {
    if target {
        InstallRole::ExplicitTarget
    } else if make || !runtime_needed.contains(name) {
        InstallRole::MakeDependency
    } else {
        InstallRole::Dependency
    }
}

fn actions_to_plan(alpm: &alpm::Alpm, actions: Actions<'_>) -> TransactionPlan {
    let runtime_needed = compute_runtime_needed(&actions);
    let mut conflicts = Vec::new();
    for conflict in actions.calculate_conflicts(true) {
        for entry in conflict.conflicting {
            conflicts.push(ConflictEntry {
                pkg: conflict.pkg.clone(),
                conflicting_pkg: entry.pkg,
                reason: entry.conflict,
            });
        }
    }

    let localdb = alpm.localdb();
    let repo_packages = actions
        .install
        .iter()
        .map(|repo_pkg| {
            let name = repo_pkg.pkg.name().to_string();
            let source_repo = repo_pkg
                .pkg
                .db()
                .map(|d| d.name().to_string())
                .unwrap_or_else(|| "sync".to_string());
            let local_pkg = localdb.pkg(name.as_str()).ok();
            let old_version = local_pkg.map(|p| p.version().to_string());
            let new_version = repo_pkg.pkg.version().as_str().to_string();
            let change = classify_change(old_version.as_deref(), &new_version);
            let role = classify_role(&name, repo_pkg.target, repo_pkg.make, &runtime_needed);
            let old_isize = local_pkg.map(|p| p.isize()).unwrap_or(0);
            PlannedPkg {
                name,
                source_repo,
                old_version,
                new_version,
                change,
                role,
                download_size: repo_pkg.pkg.download_size(),
                net_size_delta: repo_pkg.pkg.isize() - old_isize,
            }
        })
        .collect();

    let aur_bases = actions
        .build
        .iter()
        .map(|base| match base {
            Base::Aur(aur_base) => {
                let pkgbase = aur_base.package_base().to_string();
                let version = aur_base.version();
                let packages: Vec<PlannedPkg> = aur_base
                    .pkgs
                    .iter()
                    .map(|aur_pkg| {
                        let name = aur_pkg.pkg.name.clone();
                        let old_version = localdb
                            .pkg(name.as_str())
                            .ok()
                            .map(|p| p.version().to_string());
                        let new_version = aur_pkg.pkg.version.clone();
                        let change = classify_change(old_version.as_deref(), &new_version);
                        let role =
                            classify_role(&name, aur_pkg.target, aur_pkg.make, &runtime_needed);
                        PlannedPkg {
                            name,
                            source_repo: "aur".to_string(),
                            old_version,
                            new_version,
                            change,
                            role,
                            download_size: 0,
                            net_size_delta: 0,
                        }
                    })
                    .collect();
                let make_only = packages
                    .iter()
                    .all(|p| p.role == InstallRole::MakeDependency);
                let is_explicit_target = packages
                    .iter()
                    .any(|p| p.role == InstallRole::ExplicitTarget);
                PlannedAurBase {
                    pkgbase,
                    version,
                    packages,
                    make_only,
                    is_explicit_target,
                }
            }
            Base::Pkgbuild(pb) => {
                let pkgbase = pb.package_base().to_string();
                let version = pb.version();
                let packages: Vec<PlannedPkg> = pb
                    .pkgs
                    .iter()
                    .map(|pkg| {
                        let name = pkg.pkg.pkgname.clone();
                        let old_version = localdb
                            .pkg(name.as_str())
                            .ok()
                            .map(|p| p.version().to_string());
                        let change = classify_change(old_version.as_deref(), &version);
                        let role = classify_role(&name, pkg.target, pkg.make, &runtime_needed);
                        PlannedPkg {
                            name,
                            source_repo: pb.repo.clone(),
                            old_version,
                            new_version: version.clone(),
                            change,
                            role,
                            download_size: 0,
                            net_size_delta: 0,
                        }
                    })
                    .collect();
                let make_only = packages
                    .iter()
                    .all(|p| p.role == InstallRole::MakeDependency);
                let is_explicit_target = packages
                    .iter()
                    .any(|p| p.role == InstallRole::ExplicitTarget);
                PlannedAurBase {
                    pkgbase,
                    version,
                    packages,
                    make_only,
                    is_explicit_target,
                }
            }
        })
        .collect();

    let unneeded = actions
        .unneeded
        .into_iter()
        .map(|u| (u.name, u.version))
        .collect();

    let missing = actions
        .missing
        .into_iter()
        .map(|m| MissingDepError {
            dep: m.dep,
            dep_chain: m.stack.into_iter().map(|s| s.pkg).collect(),
        })
        .collect();

    TransactionPlan {
        repo_packages,
        aur_bases,
        unneeded,
        missing,
        conflicts,
    }
}

/// Resolves explicit install/upgrade targets across official repositories and the AUR.
pub async fn resolve_install<R>(
    alpm: &alpm::Alpm,
    raur: &R,
    targets: &[String],
    opts: &ResolveOptions,
) -> Result<TransactionPlan>
where
    R: Raur + Sync,
    R::Err: std::error::Error + Send + Sync + 'static,
{
    let flags = build_flags(opts);
    let mut cache = Cache::new();
    let resolver = Resolver::new(alpm, &mut cache, raur, flags).aur_namespace(true);
    let actions = resolver.resolve_targets(targets).await?;
    Ok(actions_to_plan(alpm, actions))
}

/// Resolves a full system upgrade combining official repository and AUR updates.
pub async fn resolve_sysupgrade<R>(
    alpm: &alpm::Alpm,
    repo_upgrades: &[RepoUpgrade],
    raur: &R,
    opts: &ResolveOptions,
) -> Result<TransactionPlan>
where
    R: Raur + Sync,
    R::Err: std::error::Error + Send + Sync + 'static,
{
    let mut plan = TransactionPlan::default();
    let mut seen_repo: HashSet<String> = HashSet::new();

    if !opts.aur_only {
        for up in repo_upgrades {
            let change = if alpm::vercmp(up.new_version.as_str(), up.old_version.as_str())
                == Ordering::Less
            {
                ChangeKind::Downgrade
            } else {
                ChangeKind::Upgrade
            };
            seen_repo.insert(up.name.clone());
            plan.repo_packages.push(PlannedPkg {
                name: up.name.clone(),
                source_repo: up.repo.clone(),
                old_version: Some(up.old_version.clone()),
                new_version: up.new_version.clone(),
                change,
                role: InstallRole::ExplicitTarget,
                download_size: up.download_size,
                net_size_delta: up.net_size_delta,
            });
        }
    }

    if !opts.repo_only {
        let mut flags = build_flags(opts);
        // `updates()` inspects foreign/AUR packages; keep `AUR` enabled.
        flags |= Flags::AUR;

        let mut cache = Cache::new();
        let updates = {
            let mut resolver = Resolver::new(alpm, &mut cache, raur, flags).aur_namespace(true);
            resolver.updates(None).await?
        };

        let aur_targets: Vec<String> = updates
            .aur_updates
            .into_iter()
            .filter(|u| !opts.ignore.iter().any(|ig| ig == u.local.name()))
            .map(|u| format!("aur/{}", u.remote.name))
            .collect();

        if !aur_targets.is_empty() {
            let aur_opts = ResolveOptions {
                needed: opts.needed,
                aur_only: opts.aur_only,
                repo_only: false,
                allow_downgrade: opts.allow_downgrade,
                ignore: opts.ignore.clone(),
            };
            let aur_plan = resolve_install(alpm, raur, &aur_targets, &aur_opts).await?;
            for pkg in aur_plan.repo_packages {
                if seen_repo.insert(pkg.name.clone()) {
                    plan.repo_packages.push(pkg);
                }
            }
            plan.aur_bases.extend(aur_plan.aur_bases);
            plan.unneeded.extend(aur_plan.unneeded);
            plan.missing.extend(aur_plan.missing);
            plan.conflicts.extend(aur_plan.conflicts);
        }
    }

    Ok(plan)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::path::Path;

    use alpm::{Alpm, SigLevel};
    use raur::{Package, SearchBy};
    use tempfile::TempDir;

    use super::*;
    use crate::alpm_ctx::{MockDatabase, PkgSummary};

    #[derive(Debug, thiserror::Error)]
    #[error("mock raur error: {0}")]
    struct MockRaurErr(String);

    #[derive(Default)]
    struct MockRaur {
        packages: HashMap<String, Package>,
    }

    impl MockRaur {
        fn insert(&mut self, pkg: Package) {
            self.packages.insert(pkg.name.clone(), pkg);
        }
    }

    #[async_trait::async_trait]
    impl Raur for MockRaur {
        type Err = MockRaurErr;

        async fn raw_info<S: AsRef<str> + Send + Sync>(
            &self,
            pkg_names: &[S],
        ) -> std::result::Result<Vec<Package>, Self::Err> {
            Ok(pkg_names
                .iter()
                .filter_map(|name| self.packages.get(name.as_ref()).cloned())
                .collect())
        }

        async fn search_by<S: AsRef<str> + Send + Sync>(
            &self,
            _query: S,
            _strategy: SearchBy,
        ) -> std::result::Result<Vec<Package>, Self::Err> {
            Ok(Vec::new())
        }
    }

    fn make_aur_pkg(
        name: &str,
        pkgbase: &str,
        version: &str,
        depends: &[&str],
        make_depends: &[&str],
    ) -> Package {
        Package {
            id: 1,
            name: name.to_string(),
            package_base_id: 1,
            package_base: pkgbase.to_string(),
            version: version.to_string(),
            description: Some(format!("{name} description")),
            num_votes: 10,
            popularity: 1.5,
            maintainer: Some("tester".to_string()),
            url_path: format!("/cgit/aur.git/snapshot/{pkgbase}.tar.gz"),
            depends: depends.iter().map(|s| (*s).to_string()).collect(),
            make_depends: make_depends.iter().map(|s| (*s).to_string()).collect(),
            ..Package::default()
        }
    }

    struct TestSyncPkg<'a> {
        name: &'a str,
        version: &'a str,
        depends: &'a [&'a str],
    }

    struct TestAlpmDb {
        dir: TempDir,
        alpm: Alpm,
    }

    impl TestAlpmDb {
        fn new(
            local: &[(&str, &str)],
            core: &[TestSyncPkg<'_>],
            extra: &[TestSyncPkg<'_>],
        ) -> Self {
            let dir = TempDir::new().unwrap();
            let root = dir.path();
            let dbpath = root.join("var/lib/pacman");
            let local_dir = dbpath.join("local");
            let sync_dir = dbpath.join("sync");
            fs::create_dir_all(&local_dir).unwrap();
            fs::create_dir_all(&sync_dir).unwrap();
            fs::write(local_dir.join("ALPM_DB_VERSION"), "9\n").unwrap();

            for (name, version) in local {
                let pkg_dir = local_dir.join(format!("{name}-{version}"));
                fs::create_dir_all(&pkg_dir).unwrap();
                let desc = format!(
                    "%NAME%\n{name}\n\n%VERSION%\n{version}\n\n%BASE%\n{name}\n\n%ARCH%\nx86_64\n\n%SIZE%\n1024\n"
                );
                fs::write(pkg_dir.join("desc"), desc).unwrap();
            }

            write_sync_db(&sync_dir.join("core.db"), core);
            write_sync_db(&sync_dir.join("extra.db"), extra);

            let mut alpm = Alpm::new(root.to_str().unwrap(), dbpath.to_str().unwrap()).unwrap();
            alpm.add_architecture("x86_64").unwrap();
            alpm.register_syncdb("core", SigLevel::NONE).unwrap();
            alpm.register_syncdb("extra", SigLevel::NONE).unwrap();

            Self { dir, alpm }
        }
    }

    fn write_sync_db(path: &Path, pkgs: &[TestSyncPkg<'_>]) {
        let mut tar_bytes = Vec::new();
        for pkg in pkgs {
            let entry_name = format!("{}-{}/desc", pkg.name, pkg.version);
            let mut desc = format!(
                "%FILENAME%\n{0}-{1}-x86_64.pkg.tar.zst\n\n%NAME%\n{0}\n\n%BASE%\n{0}\n\n%VERSION%\n{1}\n\n%DESC%\n{0} desc\n\n%CSIZE%\n1024\n\n%ISIZE%\n2048\n\n%ARCH%\nx86_64\n",
                pkg.name, pkg.version
            );
            if !pkg.depends.is_empty() {
                desc.push_str("\n%DEPENDS%\n");
                for dep in pkg.depends {
                    desc.push_str(dep);
                    desc.push('\n');
                }
            }
            append_tar_file(&mut tar_bytes, &entry_name, desc.as_bytes());
        }
        // Two 512-byte zero blocks terminate a POSIX tar archive.
        tar_bytes.extend_from_slice(&[0u8; 1024]);
        fs::write(path, tar_bytes).unwrap();
    }

    fn append_tar_file(buf: &mut Vec<u8>, name: &str, data: &[u8]) {
        let mut header = [0u8; 512];
        let name_bytes = name.as_bytes();
        assert!(name_bytes.len() <= 100);
        header[..name_bytes.len()].copy_from_slice(name_bytes);
        header[100..108].copy_from_slice(b"0000644\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        let size_oct = format!("{:011o}\0", data.len());
        header[124..136].copy_from_slice(size_oct.as_bytes());
        header[136..148].copy_from_slice(b"00000000000\0");
        header[148..156].copy_from_slice(b"        ");
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");

        let checksum: u32 = header.iter().map(|&b| b as u32).sum();
        let chksum_str = format!("{checksum:06o}\0 ");
        header[148..156].copy_from_slice(chksum_str.as_bytes());

        buf.extend_from_slice(&header);
        buf.extend_from_slice(data);
        let rem = data.len() % 512;
        if rem != 0 {
            buf.extend_from_slice(&vec![0u8; 512 - rem]);
        }
    }

    #[tokio::test]
    async fn resolve_aur_target_with_aur_makedep_and_repo_dep() {
        let test_db = TestAlpmDb::new(
            &[],
            &[],
            &[TestSyncPkg {
                name: "git",
                version: "2.48.0-1",
                depends: &[],
            }],
        );
        let _keep_dir = &test_db.dir;

        let mut raur = MockRaur::default();
        raur.insert(make_aur_pkg(
            "my-app",
            "my-app",
            "1.0.0-1",
            &["git"],
            &["aur-maker"],
        ));
        raur.insert(make_aur_pkg("aur-maker", "aur-maker", "0.2.0-1", &[], &[]));

        let plan = resolve_install(
            &test_db.alpm,
            &raur,
            &["my-app".to_string()],
            &ResolveOptions::default(),
        )
        .await
        .unwrap();

        assert!(plan.missing.is_empty());
        assert_eq!(plan.repo_packages.len(), 1);
        assert_eq!(plan.repo_packages[0].name, "git");
        assert_eq!(plan.repo_packages[0].source_repo, "extra");
        assert_eq!(plan.repo_packages[0].role, InstallRole::Dependency);

        // Topological order: aur-maker (makedep) must be built before my-app.
        assert_eq!(plan.aur_bases.len(), 2);
        assert_eq!(plan.aur_bases[0].pkgbase, "aur-maker");
        assert!(plan.aur_bases[0].make_only);
        assert!(!plan.aur_bases[0].is_explicit_target);

        assert_eq!(plan.aur_bases[1].pkgbase, "my-app");
        assert!(!plan.aur_bases[1].make_only);
        assert!(plan.aur_bases[1].is_explicit_target);
    }

    #[tokio::test]
    async fn resolve_split_package_built_once() {
        let test_db = TestAlpmDb::new(&[], &[], &[]);
        let mut raur = MockRaur::default();
        raur.insert(make_aur_pkg("foo-bin", "foo", "1.0-1", &[], &[]));
        raur.insert(make_aur_pkg("foo-doc", "foo", "1.0-1", &[], &[]));

        let plan = resolve_install(
            &test_db.alpm,
            &raur,
            &["foo-bin".to_string(), "foo-doc".to_string()],
            &ResolveOptions::default(),
        )
        .await
        .unwrap();

        assert_eq!(plan.aur_bases.len(), 1);
        assert_eq!(plan.aur_bases[0].pkgbase, "foo");
        let names: Vec<&str> = plan.aur_bases[0]
            .packages
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, vec!["foo-bin", "foo-doc"]);
    }

    #[tokio::test]
    async fn resolve_missing_dependency_reports_chain() {
        let test_db = TestAlpmDb::new(&[], &[], &[]);
        let mut raur = MockRaur::default();
        raur.insert(make_aur_pkg(
            "top-pkg",
            "top-pkg",
            "1.0-1",
            &["nonexistent-lib>=2.0"],
            &[],
        ));

        let plan = resolve_install(
            &test_db.alpm,
            &raur,
            &["top-pkg".to_string()],
            &ResolveOptions::default(),
        )
        .await
        .unwrap();

        assert_eq!(plan.missing.len(), 1);
        assert_eq!(plan.missing[0].dep, "nonexistent-lib>=2.0");
        assert!(plan.missing[0].dep_chain.contains(&"top-pkg".to_string()));
    }

    #[tokio::test]
    async fn resolve_needed_skips_up_to_date_targets() {
        let test_db = TestAlpmDb::new(&[("yay-bin", "12.4.2-1")], &[], &[]);
        let mut raur = MockRaur::default();
        raur.insert(make_aur_pkg("yay-bin", "yay-bin", "12.4.2-1", &[], &[]));

        let plan = resolve_install(
            &test_db.alpm,
            &raur,
            &["yay-bin".to_string()],
            &ResolveOptions {
                needed: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert!(plan.aur_bases.is_empty());
        assert_eq!(
            plan.unneeded,
            vec![("yay-bin".to_string(), "12.4.2-1".to_string())]
        );
    }

    #[tokio::test]
    async fn resolve_sysupgrade_combines_repo_and_aur_updates() {
        let test_db = TestAlpmDb::new(
            &[("bash", "5.2.037-1"), ("yay-bin", "12.4.1-1")],
            &[TestSyncPkg {
                name: "bash",
                version: "5.2.037-2",
                depends: &[],
            }],
            &[TestSyncPkg {
                name: "jq",
                version: "1.7.1-2",
                depends: &[],
            }],
        );
        let mut raur = MockRaur::default();
        raur.insert(make_aur_pkg("yay-bin", "yay-bin", "12.4.2-1", &["jq"], &[]));

        let mock_db = MockDatabase::new()
            .with_local_sized("bash", "5.2.037-1", 490)
            .with_local("yay-bin", "12.4.1-1")
            .with_sync(PkgSummary {
                repo: "core".to_string(),
                name: "bash".to_string(),
                version: "5.2.037-2".to_string(),
                description: "bash".to_string(),
                download_size: 500,
                installed_size: 500,
                installed_version: None,
            })
            .with_sync(PkgSummary {
                repo: "extra".to_string(),
                name: "jq".to_string(),
                version: "1.7.1-2".to_string(),
                description: "jq".to_string(),
                download_size: 100,
                installed_size: 200,
                installed_version: None,
            });

        let upgrades = mock_db.repo_upgrades(&[], false);
        let plan = resolve_sysupgrade(&test_db.alpm, &upgrades, &raur, &ResolveOptions::default())
            .await
            .unwrap();

        let repo_names: Vec<&str> = plan.repo_packages.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(repo_names, vec!["bash", "jq"]);
        assert_eq!(plan.aur_bases.len(), 1);
        assert_eq!(plan.aur_bases[0].pkgbase, "yay-bin");
        assert_eq!(
            plan.aur_bases[0].packages[0].old_version.as_deref(),
            Some("12.4.1-1")
        );
        assert_eq!(plan.aur_bases[0].packages[0].new_version, "12.4.2-1");
        assert_eq!(plan.aur_bases[0].packages[0].change, ChangeKind::Upgrade);
    }

    #[tokio::test]
    async fn resolve_install_and_sysupgrade_with_alpm_ctx_do_not_deadlock() {
        use std::time::Duration;

        use crate::alpm_ctx::AlpmCtx;

        let test_db = TestAlpmDb::new(
            &[("bash", "5.2.037-1")],
            &[TestSyncPkg {
                name: "bash",
                version: "5.2.037-2",
                depends: &[],
            }],
            &[TestSyncPkg {
                name: "gedit",
                version: "48.1-1",
                depends: &[],
            }],
        );
        let root = test_db.dir.path();
        let dbpath = root.join("var/lib/pacman");
        let conf_path = root.join("pacman.conf");
        let conf_contents = format!(
            "[options]\nRootDir = {}\nDBPath = {}\nArchitecture = x86_64\nSigLevel = Never\n\n[core]\nServer = file://{}\n\n[extra]\nServer = file://{}\n",
            root.display(),
            dbpath.display(),
            dbpath.join("sync").display(),
            dbpath.join("sync").display(),
        );
        fs::write(&conf_path, conf_contents).unwrap();

        let ctx = AlpmCtx::from_conf_file(&conf_path).unwrap();
        let raur = MockRaur::default();
        let opts = ResolveOptions::default();

        let upgrades = ctx.repo_upgrades(&opts.ignore, opts.allow_downgrade);
        let alpm_guard = ctx.alpm();

        let install_plan = tokio::time::timeout(
            Duration::from_secs(2),
            resolve_install(&alpm_guard, &raur, &["gedit".to_string()], &opts),
        )
        .await
        .expect("resolve_install should not deadlock while AlpmGuard is held")
        .unwrap();

        assert_eq!(install_plan.repo_packages.len(), 1);
        assert_eq!(install_plan.repo_packages[0].name, "gedit");
        assert_eq!(install_plan.repo_packages[0].source_repo, "extra");
        assert_eq!(install_plan.repo_packages[0].change, ChangeKind::Install);
        assert!(install_plan.aur_bases.is_empty());

        let sysupgrade_plan = tokio::time::timeout(
            Duration::from_secs(2),
            resolve_sysupgrade(&alpm_guard, &upgrades, &raur, &opts),
        )
        .await
        .expect("resolve_sysupgrade should not deadlock while AlpmGuard is held")
        .unwrap();

        assert_eq!(sysupgrade_plan.repo_packages.len(), 1);
        assert_eq!(sysupgrade_plan.repo_packages[0].name, "bash");
        assert_eq!(sysupgrade_plan.repo_packages[0].change, ChangeKind::Upgrade);
    }
}
