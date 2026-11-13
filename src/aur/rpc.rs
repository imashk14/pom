//! Async AUR RPC v5 client helpers and search types.

use crate::alpm_ctx::PackageDatabase;
use raur::{Handle, Raur, SearchBy};
use std::fmt::Display;

/// Errors returned by AUR RPC operations.
#[derive(Debug, thiserror::Error)]
pub enum AurRpcError {
    /// An error returned by the underlying `raur` client or AUR RPC endpoint.
    #[error("AUR RPC request failed: {0}")]
    Rpc(String),
}

impl From<raur::Error> for AurRpcError {
    fn from(err: raur::Error) -> Self {
        Self::Rpc(err.to_string())
    }
}

/// A single AUR package search result annotated with local installation state.
#[derive(Debug, Clone, PartialEq)]
pub struct AurSearchHit {
    pub name: String,
    pub version: String,
    pub description: Option<String>,
    pub votes: u32,
    pub popularity: f64,
    pub maintainer: Option<String>,
    pub out_of_date: Option<i64>,
    pub installed_version: Option<String>,
}

/// Default AUR RPC v5 endpoint URL (without trailing slash so POST requests are not redirected to GET).
pub const DEFAULT_AUR_RPC_URL: &str = "https://aur.archlinux.org/rpc";

/// Creates a default [`raur::Handle`] for querying the AUR RPC v5 API.
pub fn new_handle() -> Handle {
    let client = reqwest::Client::builder()
        .user_agent(concat!(
            env!("CARGO_PKG_NAME"),
            "/",
            env!("CARGO_PKG_VERSION")
        ))
        .build()
        .unwrap_or_default();
    Handle::new_with_settings(client, DEFAULT_AUR_RPC_URL)
}

/// Searches the AUR using the first term via `SearchBy::NameDesc`, filters any
/// remaining terms case-insensitively across package name and description, and
/// annotates each hit with its locally installed version from `db`.
pub async fn search_aur<R>(
    raur: &R,
    db: &dyn PackageDatabase,
    terms: &[String],
) -> Result<Vec<AurSearchHit>, AurRpcError>
where
    R: Raur + Sync,
    R::Err: Display,
{
    let Some((first_term, rest_terms)) = terms.split_first() else {
        return Ok(Vec::new());
    };

    let packages = raur
        .search_by(first_term, SearchBy::NameDesc)
        .await
        .map_err(|err| AurRpcError::Rpc(err.to_string()))?;

    let lowered_filters: Vec<String> = rest_terms.iter().map(|t| t.to_lowercase()).collect();

    let hits = packages
        .into_iter()
        .filter(|pkg| {
            if lowered_filters.is_empty() {
                return true;
            }
            let name_lower = pkg.name.to_lowercase();
            let desc_lower = pkg
                .description
                .as_deref()
                .unwrap_or_default()
                .to_lowercase();
            lowered_filters
                .iter()
                .all(|needle| name_lower.contains(needle) || desc_lower.contains(needle))
        })
        .map(|pkg| {
            let installed_version = db.local_version(&pkg.name);
            AurSearchHit {
                name: pkg.name,
                version: pkg.version,
                description: pkg.description,
                votes: pkg.num_votes,
                popularity: pkg.popularity,
                maintainer: pkg.maintainer,
                out_of_date: pkg.out_of_date,
                installed_version,
            }
        })
        .collect();

    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alpm_ctx::{PkgSummary, RepoUpgrade};
    use async_trait::async_trait;
    use raur::Package;
    use std::cmp::Ordering;
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct MockRaur {
        results: Result<Vec<Package>, String>,
        recorded_queries: Mutex<Vec<(String, SearchBy)>>,
    }

    impl MockRaur {
        fn with_packages(packages: Vec<Package>) -> Self {
            Self {
                results: Ok(packages),
                recorded_queries: Mutex::new(Vec::new()),
            }
        }

        fn with_error(msg: &str) -> Self {
            Self {
                results: Err(msg.to_string()),
                recorded_queries: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl Raur for MockRaur {
        type Err = String;

        async fn raw_info<S: AsRef<str> + Send + Sync>(
            &self,
            _pkg_names: &[S],
        ) -> Result<Vec<Package>, Self::Err> {
            self.results.clone()
        }

        async fn search_by<S: AsRef<str> + Send + Sync>(
            &self,
            query: S,
            strategy: SearchBy,
        ) -> Result<Vec<Package>, Self::Err> {
            self.recorded_queries
                .lock()
                .expect("mutex poisoned")
                .push((query.as_ref().to_string(), strategy));
            self.results.clone()
        }
    }

    struct MockLocalDb {
        local: HashMap<String, String>,
    }

    impl MockLocalDb {
        fn new(entries: &[(&str, &str)]) -> Self {
            Self {
                local: entries
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                    .collect(),
            }
        }
    }

    impl PackageDatabase for MockLocalDb {
        fn local_version(&self, name: &str) -> Option<String> {
            self.local.get(name).cloned()
        }

        fn sync_pkg(&self, _name: &str) -> Option<PkgSummary> {
            None
        }

        fn search_sync(&self, _terms: &[String]) -> Vec<PkgSummary> {
            Vec::new()
        }

        fn repo_upgrades(&self, _ignore: &[String], _allow_downgrade: bool) -> Vec<RepoUpgrade> {
            Vec::new()
        }

        fn foreign_packages(&self) -> Vec<(String, String)> {
            Vec::new()
        }

        fn cmp_versions(&self, a: &str, b: &str) -> Ordering {
            alpm::vercmp(a, b)
        }
    }

    fn make_pkg(
        name: &str,
        version: &str,
        description: Option<&str>,
        votes: u32,
        popularity: f64,
        maintainer: Option<&str>,
        out_of_date: Option<i64>,
    ) -> Package {
        Package {
            name: name.to_string(),
            version: version.to_string(),
            description: description.map(str::to_string),
            num_votes: votes,
            popularity,
            maintainer: maintainer.map(str::to_string),
            out_of_date,
            ..Package::default()
        }
    }

    #[test]
    fn new_handle_uses_default_aur_url() {
        let handle = new_handle();
        assert_eq!(handle.url(), DEFAULT_AUR_RPC_URL);
    }

    #[tokio::test]
    async fn search_aur_empty_terms_returns_empty_without_rpc() {
        let raur = MockRaur::with_packages(vec![make_pkg(
            "paru-bin",
            "2.0.0-1",
            Some("Feature packed AUR helper"),
            100,
            1.5,
            Some("Morganamilo"),
            None,
        )]);
        let db = MockLocalDb::new(&[]);

        let hits = search_aur(&raur, &db, &[]).await.unwrap();
        assert!(hits.is_empty());
        assert!(raur.recorded_queries.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn search_aur_single_term_annotates_installed_and_fields() {
        let raur = MockRaur::with_packages(vec![
            make_pkg(
                "paru-bin",
                "2.0.4-1",
                Some("Feature packed AUR helper"),
                512,
                12.34,
                Some("Morganamilo"),
                None,
            ),
            make_pkg(
                "paru-git",
                "2.0.4.r10-1",
                None,
                42,
                0.75,
                None,
                Some(1_700_000_000),
            ),
        ]);
        let db = MockLocalDb::new(&[("paru-bin", "2.0.3-1")]);

        let terms = vec!["paru".to_string()];
        let hits = search_aur(&raur, &db, &terms).await.unwrap();

        let queries = raur.recorded_queries.lock().unwrap();
        assert_eq!(
            queries.as_slice(),
            &[("paru".to_string(), SearchBy::NameDesc)]
        );

        assert_eq!(hits.len(), 2);
        assert_eq!(
            hits[0],
            AurSearchHit {
                name: "paru-bin".to_string(),
                version: "2.0.4-1".to_string(),
                description: Some("Feature packed AUR helper".to_string()),
                votes: 512,
                popularity: 12.34,
                maintainer: Some("Morganamilo".to_string()),
                out_of_date: None,
                installed_version: Some("2.0.3-1".to_string()),
            }
        );
        assert_eq!(
            hits[1],
            AurSearchHit {
                name: "paru-git".to_string(),
                version: "2.0.4.r10-1".to_string(),
                description: None,
                votes: 42,
                popularity: 0.75,
                maintainer: None,
                out_of_date: Some(1_700_000_000),
                installed_version: None,
            }
        );
    }

    #[tokio::test]
    async fn search_aur_multi_term_filters_case_insensitively() {
        let raur = MockRaur::with_packages(vec![
            make_pkg(
                "paru-bin",
                "2.0.4-1",
                Some("Feature packed AUR helper written in Rust"),
                512,
                12.34,
                Some("Morganamilo"),
                None,
            ),
            make_pkg(
                "yay-bin",
                "12.3.5-1",
                Some("Yet another yogurt - AUR helper written in Go"),
                900,
                20.0,
                Some("jguer"),
                None,
            ),
            make_pkg(
                "rust-analyzer-bin",
                "2024.01-1",
                Some("Rust LSP server"),
                50,
                1.0,
                Some("someone"),
                None,
            ),
        ]);
        let db = MockLocalDb::new(&[]);

        let terms = vec!["helper".to_string(), "RUST".to_string(), "AUR".to_string()];
        let hits = search_aur(&raur, &db, &terms).await.unwrap();

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "paru-bin");
    }

    #[tokio::test]
    async fn search_aur_propagates_rpc_error() {
        let raur = MockRaur::with_error("Query arg too small");
        let db = MockLocalDb::new(&[]);

        let terms = vec!["a".to_string()];
        let err = search_aur(&raur, &db, &terms).await.unwrap_err();
        assert!(matches!(err, AurRpcError::Rpc(msg) if msg.contains("Query arg too small")));
    }
}
