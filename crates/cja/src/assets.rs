//! Content-hashed assets embedded by `cja-build`.

use axum::{
    Router,
    http::header::{CACHE_CONTROL, CONTENT_TYPE},
    routing::get,
};
use std::{collections::HashMap, fmt};

const STABLE_ROOT_FILES: &[&str] = &["favicon.ico", "robots.txt"];
const IMMUTABLE_CACHE: &str = "public, max-age=31536000, immutable";
const STABLE_CACHE: &str = "public, max-age=3600";

/// One generated output embedded in the consuming binary.
pub struct Asset {
    pub logical_name: &'static str,
    pub hashed_name: &'static str,
    pub content_type: &'static str,
    pub bytes: &'static [u8],
}

/// Assets owned by a single app or library crate.
pub struct Manifest {
    pub entries: &'static [Asset],
}

impl Manifest {
    pub const fn new(entries: &'static [Asset]) -> Self {
        Self { entries }
    }

    /// Resolve only within this manifest's logical namespace.
    pub fn asset_url(&self, logical_name: &str) -> String {
        let asset = self
            .entries
            .iter()
            .find(|asset| asset.logical_name == logical_name)
            .unwrap_or_else(|| panic!("unknown asset: {logical_name}"));
        format!("/assets/{}", asset.hashed_name)
    }
}

/// A duplicate served URL discovered before router startup.
#[derive(Debug, PartialEq, Eq)]
pub enum AssetError {
    Collision {
        url: String,
        first: &'static str,
        second: &'static str,
    },
}

impl fmt::Display for AssetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Collision { url, first, second } => {
                write!(f, "asset URL collision at {url}: {first} and {second}")
            }
        }
    }
}

impl std::error::Error for AssetError {}

/// Build a root-mounted Axum router for one or more manifests.
pub fn router<S>(manifests: &[&'static Manifest]) -> Result<Router<S>, AssetError>
where
    S: Clone + Send + Sync + 'static,
{
    let mut paths = HashMap::new();
    let mut registrations = Vec::new();
    for manifest in manifests {
        for asset in manifest.entries {
            let hashed_url = format!("/assets/{}", asset.hashed_name);
            register(&mut paths, &hashed_url, asset.logical_name)?;
            registrations.push((hashed_url, asset, IMMUTABLE_CACHE));
            if STABLE_ROOT_FILES.contains(&asset.logical_name) {
                let stable_url = format!("/{}", asset.logical_name);
                register(&mut paths, &stable_url, asset.logical_name)?;
                registrations.push((stable_url, asset, STABLE_CACHE));
            }
        }
    }
    let mut router = Router::new();
    for (url, asset, cache) in registrations {
        router = router.route(
            &url,
            get(move || async move {
                (
                    [(CONTENT_TYPE, asset.content_type), (CACHE_CONTROL, cache)],
                    asset.bytes,
                )
            }),
        );
    }
    Ok(router)
}

fn register(
    paths: &mut HashMap<String, &'static str>,
    url: &str,
    logical_name: &'static str,
) -> Result<(), AssetError> {
    if let Some(first) = paths.insert(url.to_owned(), logical_name) {
        return Err(AssetError::Collision {
            url: url.to_owned(),
            first,
            second: logical_name,
        });
    }
    Ok(())
}

/// Define a named module with this crate's generated manifest and scoped URL lookup.
#[macro_export]
macro_rules! include_assets {
    ($name:ident) => {
        pub mod $name {
            use $crate::assets::{Asset, Manifest};

            pub static MANIFEST: Manifest =
                Manifest::new(&include!(concat!(env!("OUT_DIR"), "/cja_assets.rs")));

            pub fn asset_url(logical_name: &str) -> String {
                MANIFEST.asset_url(logical_name)
            }
        }
    };
}
