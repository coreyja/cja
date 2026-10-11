//! Content-hashed assets embedded by `cja-build`.

use axum::{
    Router,
    http::StatusCode,
    http::header::{CACHE_CONTROL, CONTENT_TYPE},
    routing::{any, get},
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
    pub owner: &'static str,
    pub entries: &'static [Asset],
}

impl Manifest {
    pub const fn new(owner: &'static str, entries: &'static [Asset]) -> Self {
        Self { owner, entries }
    }

    /// Resolve only within this manifest's logical namespace.
    #[track_caller]
    pub fn asset_url(&self, logical_name: &str) -> String {
        let Some(asset) = self
            .entries
            .iter()
            .find(|asset| asset.logical_name == logical_name)
        else {
            panic!("unknown asset: {logical_name}");
        };
        format!("/assets/{}", asset.hashed_name)
    }
}

/// A duplicate served URL discovered before router startup.
#[derive(Debug, PartialEq, Eq)]
pub enum AssetError {
    Collision {
        url: String,
        first_owner: &'static str,
        first_logical_name: &'static str,
        second_owner: &'static str,
        second_logical_name: &'static str,
    },
}

impl fmt::Display for AssetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Collision {
                url,
                first_owner,
                first_logical_name,
                second_owner,
                second_logical_name,
            } => {
                write!(
                    f,
                    "asset URL collision at {url}: {first_owner} ({first_logical_name}) and {second_owner} ({second_logical_name})"
                )
            }
        }
    }
}

impl std::error::Error for AssetError {}

/// Build a root-mounted Axum router for all manifests in one call.
///
/// The `/assets`, `/assets/`, and `/assets/{*rest}` routes force unknown asset
/// URLs to return 404 even when the app has a fallback or root wildcard route.
/// Calling this more than once, or adding another `/assets` wildcard, conflicts
/// at startup.
pub fn router<S>(manifests: &[&'static Manifest]) -> Result<Router<S>, AssetError>
where
    S: Clone + Send + Sync + 'static,
{
    let mut paths = HashMap::new();
    let mut registrations = Vec::new();
    for manifest in manifests {
        for asset in manifest.entries {
            let hashed_url = format!("/assets/{}", asset.hashed_name);
            register(&mut paths, &hashed_url, manifest.owner, asset.logical_name)?;
            registrations.push((hashed_url, asset, IMMUTABLE_CACHE));
            if STABLE_ROOT_FILES.contains(&asset.logical_name) {
                let stable_url = format!("/{}", asset.logical_name);
                register(&mut paths, &stable_url, manifest.owner, asset.logical_name)?;
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
    Ok(router
        .route("/assets", any(|| async { StatusCode::NOT_FOUND }))
        .route("/assets/", any(|| async { StatusCode::NOT_FOUND }))
        .route("/assets/{*rest}", any(|| async { StatusCode::NOT_FOUND })))
}

fn register(
    paths: &mut HashMap<String, (&'static str, &'static str)>,
    url: &str,
    owner: &'static str,
    logical_name: &'static str,
) -> Result<(), AssetError> {
    if let Some((first_owner, first_logical_name)) =
        paths.insert(url.to_owned(), (owner, logical_name))
    {
        return Err(AssetError::Collision {
            url: url.to_owned(),
            first_owner,
            first_logical_name,
            second_owner: owner,
            second_logical_name: logical_name,
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

            pub static MANIFEST: Manifest = Manifest::new(
                env!("CARGO_PKG_NAME"),
                &include!(concat!(env!("OUT_DIR"), "/cja_assets.rs")),
            );

            #[track_caller]
            pub fn asset_url(logical_name: &str) -> String {
                MANIFEST.asset_url(logical_name)
            }
        }
    };
}
