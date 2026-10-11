//! Build static files into a content-hashed, embedded cja manifest.

use sha2::{Digest, Sha256};
use std::{
    env, fmt, fs, io,
    path::{Component, Path, PathBuf},
};

#[derive(thiserror::Error)]
pub enum BuildError {
    #[error("assets directory is missing or is not a directory: {}", path.display())]
    MissingAssetsDir { path: PathBuf },
    #[error("asset I/O failed at {}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid asset path {}: {reason}", path.display())]
    InvalidPath { path: PathBuf, reason: &'static str },
    #[error("asset output collision for {hashed_name}: {} and {}", first.display(), second.display())]
    Collision {
        first: PathBuf,
        second: PathBuf,
        hashed_name: String,
    },
    #[error("missing or invalid environment variable {name}")]
    Environment {
        name: &'static str,
        #[source]
        source: env::VarError,
    },
}

impl fmt::Debug for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use std::error::Error as _;
        write!(f, "{self}")?;
        let mut cause = self.source();
        while let Some(error) = cause {
            write!(f, ": {error}")?;
            cause = error.source();
        }
        Ok(())
    }
}

struct AssetRecord {
    logical_name: String,
    hashed_name: String,
    content_type: String,
    source_path: PathBuf,
    bytes: Vec<u8>,
}

/// Collects all outputs for one crate and owns `OUT_DIR/cja-assets`.
pub struct AssetsBuilder {
    assets_dir: PathBuf,
    outputs: Vec<AssetRecord>,
}

impl AssetsBuilder {
    pub fn new(assets_dir: impl Into<PathBuf>) -> Self {
        Self {
            assets_dir: assets_dir.into(),
            outputs: Vec::new(),
        }
    }

    /// Called from the consuming crate's `build.rs`.
    pub fn build(self) -> Result<(), BuildError> {
        println!("cargo::rerun-if-env-changed=CARGO_MANIFEST_DIR");
        println!("cargo::rerun-if-env-changed=OUT_DIR");
        println!("cargo::rerun-if-changed=build.rs");
        println!("cargo::rerun-if-changed={}", self.assets_dir.display());
        let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").map_err(|source| {
            BuildError::Environment {
                name: "CARGO_MANIFEST_DIR",
                source,
            }
        })?);
        let out_dir =
            PathBuf::from(
                env::var("OUT_DIR").map_err(|source| BuildError::Environment {
                    name: "OUT_DIR",
                    source,
                })?,
            );
        self.build_in(&manifest_dir, &out_dir)
    }

    fn build_in(mut self, manifest_dir: &Path, out_dir: &Path) -> Result<(), BuildError> {
        let source_dir = manifest_dir.join(&self.assets_dir);
        if !source_dir.is_dir()
            || fs::symlink_metadata(&source_dir)
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(BuildError::MissingAssetsDir { path: source_dir });
        }
        let mut files = Vec::new();
        collect_files(&source_dir, &source_dir, &mut files)?;
        files.sort();
        for (logical_name, path) in files {
            let bytes = fs::read(&path).map_err(|source| BuildError::Io {
                path: path.clone(),
                source,
            })?;
            self.add_output(&logical_name, bytes, &path)?;
        }

        let final_dir = out_dir.join("cja-assets");
        if final_dir.exists() {
            fs::remove_dir_all(&final_dir).map_err(|source| BuildError::Io {
                path: final_dir.clone(),
                source,
            })?;
        }
        fs::create_dir_all(&final_dir).map_err(|source| BuildError::Io {
            path: final_dir.clone(),
            source,
        })?;
        let mut manifest = String::from("[\n");
        for record in &self.outputs {
            let path = final_dir.join(&record.hashed_name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|source| BuildError::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
            fs::write(&path, &record.bytes).map_err(|source| BuildError::Io {
                path: path.clone(),
                source,
            })?;
            let relative = format!("/cja-assets/{}", record.hashed_name);
            use std::fmt::Write as _;
            writeln!(
                manifest,
                "Asset {{ logical_name: {:?}, hashed_name: {:?}, content_type: {:?}, bytes: include_bytes!(concat!(env!(\"OUT_DIR\"), {:?})) }},",
                record.logical_name, record.hashed_name, record.content_type, relative
            )
            .expect("writing to a String cannot fail");
        }
        manifest.push_str("]\n");
        let manifest_path = out_dir.join("cja_assets.rs");
        let pending_path = out_dir.join("cja_assets.rs.tmp");
        fs::write(&pending_path, manifest).map_err(|source| BuildError::Io {
            path: pending_path.clone(),
            source,
        })?;
        fs::rename(&pending_path, &manifest_path).map_err(|source| BuildError::Io {
            path: manifest_path,
            source,
        })?;
        Ok(())
    }

    fn add_output(
        &mut self,
        logical_name: &str,
        bytes: Vec<u8>,
        source_path: &Path,
    ) -> Result<String, BuildError> {
        validate_name(logical_name, source_path)?;
        let path = Path::new(logical_name);
        let hash = short_hash(&bytes);
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| BuildError::InvalidPath {
                path: source_path.to_path_buf(),
                reason: "missing filename",
            })?;
        let hashed_file = match file_name.rsplit_once('.') {
            Some((stem, extension)) if !stem.is_empty() => {
                format!("{stem}.{hash}.{extension}")
            }
            _ => format!("{file_name}.{hash}"),
        };
        let hashed_name = path
            .with_file_name(hashed_file)
            .to_string_lossy()
            .replace('\\', "/");
        if let Some(first) = self
            .outputs
            .iter()
            .find(|record| record.logical_name == logical_name || record.hashed_name == hashed_name)
        {
            return Err(BuildError::Collision {
                first: first.source_path.clone(),
                second: source_path.to_path_buf(),
                hashed_name,
            });
        }
        self.outputs.push(AssetRecord {
            logical_name: logical_name.to_owned(),
            hashed_name: hashed_name.clone(),
            content_type: mime_guess::from_path(logical_name)
                .first_or_octet_stream()
                .to_string(),
            source_path: source_path.to_path_buf(),
            bytes,
        });
        Ok(hashed_name)
    }
}

/// The shared content hash for static files and later generated assets.
pub(crate) fn short_hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))[..8].to_owned()
}

fn collect_files(
    root: &Path,
    dir: &Path,
    files: &mut Vec<(String, PathBuf)>,
) -> Result<(), BuildError> {
    let entries = fs::read_dir(dir).map_err(|source| BuildError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| BuildError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let relative = path.strip_prefix(root).expect("entry is below root");
        let name = relative.to_str().ok_or_else(|| BuildError::InvalidPath {
            path: path.clone(),
            reason: "non-UTF-8 name",
        })?;
        validate_name(name, &path)?;
        let kind = entry.file_type().map_err(|source| BuildError::Io {
            path: path.clone(),
            source,
        })?;
        if kind.is_symlink() {
            return Err(BuildError::InvalidPath {
                path,
                reason: "symlinks are not supported",
            });
        }
        if kind.is_dir() {
            collect_files(root, &path, files)?;
        } else if kind.is_file() {
            files.push((name.replace('\\', "/"), path));
        } else {
            return Err(BuildError::InvalidPath {
                path,
                reason: "not a regular file",
            });
        }
    }
    Ok(())
}

fn validate_name(name: &str, path: &Path) -> Result<(), BuildError> {
    if name.is_empty()
        || Path::new(name)
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
        || name.split('/').any(|part| {
            part.is_empty()
                || part == "."
                || part == ".."
                || !part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._~@+-".contains(&b))
        })
    {
        return Err(BuildError::InvalidPath {
            path: path.to_path_buf(),
            reason: "only ASCII letters, digits, . _ ~ @ + - and / are allowed",
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp directory")
    }

    #[test]
    fn hash_is_content_derived() {
        assert_eq!(short_hash(b"abc"), "ba7816bf");
        assert_eq!(short_hash(b"abc"), short_hash(b"abc"));
        assert_ne!(short_hash(b"abc"), short_hash(b"abd"));
        let mut builder = AssetsBuilder::new("assets");
        assert_eq!(
            builder
                .add_output("one.svg", b"abc".to_vec(), Path::new("one.svg"))
                .unwrap(),
            "one.ba7816bf.svg"
        );
        assert_eq!(
            builder
                .add_output("two.svg", b"abc".to_vec(), Path::new("two.svg"))
                .unwrap(),
            "two.ba7816bf.svg"
        );
    }

    #[test]
    fn nested_manifest_is_deterministic_and_owned_output_is_cleaned() {
        let temp = fixture();
        let assets = temp.path().join("assets");
        let out = temp.path().join("out");
        fs::create_dir_all(assets.join("img")).unwrap();
        fs::create_dir_all(&out).unwrap();
        fs::write(assets.join("img/logo.svg"), b"<svg/>").unwrap();
        fs::write(assets.join("empty"), b"").unwrap();
        fs::write(assets.join(".hidden"), b"secret").unwrap();
        fs::write(out.join("unrelated"), b"keep").unwrap();
        AssetsBuilder::new("assets")
            .build_in(temp.path(), &out)
            .unwrap();
        let generated = fs::read_to_string(out.join("cja_assets.rs")).unwrap();
        let empty_name = format!("empty.{}", short_hash(b""));
        let logo_name = format!("img/logo.{}.svg", short_hash(b"<svg/>"));
        assert!(generated.contains(&logo_name));
        assert!(generated.contains(&empty_name));
        assert!(generated.contains("image/svg+xml"));
        assert!(generated.contains("include_bytes!"));
        assert!(generated.find(".hidden").unwrap() < generated.find("empty.").unwrap());
        assert!(generated.find("empty.").unwrap() < generated.find("img/logo.").unwrap());
        assert_eq!(
            fs::read(out.join("cja-assets").join(&logo_name)).unwrap(),
            b"<svg/>"
        );
        fs::write(assets.join("img/logo.svg"), b"<svg>changed</svg>").unwrap();
        AssetsBuilder::new("assets")
            .build_in(temp.path(), &out)
            .unwrap();
        assert!(!out.join("cja-assets").join(logo_name).exists());
        assert_eq!(fs::read(out.join("unrelated")).unwrap(), b"keep");
    }

    #[test]
    fn missing_directory_names_the_path() {
        let temp = fixture();
        let err = AssetsBuilder::new("missing")
            .build_in(temp.path(), temp.path())
            .unwrap_err();
        assert!(matches!(err, BuildError::MissingAssetsDir { .. }));
        assert!(err.to_string().contains("missing"));
    }

    #[test]
    fn invalid_name_and_collision_name_both_sources() {
        let mut builder = AssetsBuilder::new("assets");
        let err = builder
            .add_output("my logo.svg", vec![], Path::new("assets/my logo.svg"))
            .unwrap_err();
        assert!(err.to_string().contains("my logo.svg"));
        assert!(matches!(err, BuildError::InvalidPath { .. }));
        let err = builder
            .add_output("img/./logo.svg", vec![], Path::new("assets/img/./logo.svg"))
            .unwrap_err();
        assert!(matches!(err, BuildError::InvalidPath { .. }));

        // An eight-hex collision is improbable in a small fixture; inject its output key.
        builder.outputs.push(AssetRecord {
            logical_name: "first.svg".to_owned(),
            hashed_name: format!("second.{}.svg", short_hash(b"same")),
            content_type: "image/svg+xml".to_owned(),
            source_path: PathBuf::from("assets/first.svg"),
            bytes: vec![],
        });
        let err = builder
            .add_output(
                "second.svg",
                b"same".to_vec(),
                Path::new("assets/second.svg"),
            )
            .unwrap_err();
        assert!(matches!(err, BuildError::Collision { .. }));
        assert!(err.to_string().contains("first.svg"));
        assert!(err.to_string().contains("second.svg"));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_and_non_utf8_names_fail() {
        use std::{
            ffi::OsString,
            os::unix::{ffi::OsStringExt as _, fs::symlink},
        };
        let temp = fixture();
        let assets = temp.path().join("assets");
        fs::create_dir(&assets).unwrap();
        symlink("missing", assets.join("link")).unwrap();
        let err = AssetsBuilder::new("assets")
            .build_in(temp.path(), temp.path())
            .unwrap_err();
        assert!(err.to_string().contains("link"));
        fs::remove_file(assets.join("link")).unwrap();
        let invalid = assets.join(OsString::from_vec(vec![0xff]));
        fs::write(&invalid, b"x").unwrap();
        let err = AssetsBuilder::new("assets")
            .build_in(temp.path(), temp.path())
            .unwrap_err();
        assert!(matches!(err, BuildError::InvalidPath { .. }));
        assert!(err.to_string().contains("non-UTF-8"));
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_file_is_a_real_permission_error() {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = fixture();
        let assets = temp.path().join("assets");
        fs::create_dir(&assets).unwrap();
        let path = assets.join("secret.css");
        fs::write(&path, b"secret").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o0)).unwrap();
        let direct = fs::read(&path).expect_err("this test must run as a non-root user");
        assert_eq!(direct.kind(), io::ErrorKind::PermissionDenied);
        let err = AssetsBuilder::new("assets")
            .build_in(temp.path(), temp.path())
            .unwrap_err();
        assert!(matches!(err, BuildError::Io { .. }));
        assert!(err.to_string().contains("secret.css"));
        assert!(format!("{err:?}").contains(&direct.to_string()));
    }
}
