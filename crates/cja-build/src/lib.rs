//! Build static and bundled frontend files into a content-hashed cja manifest.

mod bundler;
mod tool_cache;

#[cfg(test)]
pub(crate) static SPAWN: std::sync::Mutex<()> = std::sync::Mutex::new(());

use sha2::{Digest, Sha256};
use std::{
    env, fmt, fs, io,
    path::{Component, Path, PathBuf},
};

#[non_exhaustive]
#[derive(thiserror::Error)]
pub enum BuildError {
    #[error("assets path is not a directory: {}", path.display())]
    NotADirectory { path: PathBuf },
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
    #[error("frontend build failed: {0}")]
    Frontend(String),
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
/// Dot-prefixed files and directories at any depth are ignored.
pub struct AssetsBuilder {
    assets_dir: Option<PathBuf>,
    frontend_dir: PathBuf,
    entries: Vec<Entry>,
    outputs: Vec<AssetRecord>,
}

/// A TypeScript or JavaScript entry and its logical output name.
#[derive(Clone)]
pub struct Entry {
    source: PathBuf,
    logical_name: String,
    format: EntryFormat,
}

/// The output format for one entry. ESM must remain a single self-contained chunk.
#[derive(Clone)]
pub enum EntryFormat {
    Iife { global_name: Option<String> },
    Esm,
}

impl Entry {
    pub fn new(source: impl Into<PathBuf>, logical_name: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            logical_name: logical_name.into(),
            format: EntryFormat::Iife { global_name: None },
        }
    }

    #[must_use]
    pub fn format(mut self, format: EntryFormat) -> Self {
        self.format = format;
        self
    }
}

impl AssetsBuilder {
    pub fn new() -> Self {
        Self {
            assets_dir: None,
            frontend_dir: PathBuf::from("frontend"),
            entries: Vec::new(),
            outputs: Vec::new(),
        }
    }

    #[must_use]
    pub fn static_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.assets_dir = Some(path.into());
        self
    }

    #[must_use]
    pub fn frontend_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.frontend_dir = path.into();
        self
    }

    #[must_use]
    pub fn entry(mut self, entry: Entry) -> Self {
        self.entries.push(entry);
        self
    }

    /// Called from the consuming crate's `build.rs`.
    pub fn build(self) -> Result<(), BuildError> {
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
        let tool_env = tool_cache::ToolEnv::capture(&manifest_dir);
        self.build_in(&manifest_dir, &out_dir, &tool_env)
    }

    #[cfg(test)]
    fn build_test(self, manifest_dir: &Path, out_dir: &Path) -> Result<(), BuildError> {
        self.build_in(
            manifest_dir,
            out_dir,
            &tool_cache::ToolEnv::capture(manifest_dir),
        )
    }

    fn build_in(
        mut self,
        manifest_dir: &Path,
        out_dir: &Path,
        tool_env: &tool_cache::ToolEnv,
    ) -> Result<(), BuildError> {
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
        let manifest_path = out_dir.join("cja_assets.rs");
        if manifest_path.exists() {
            fs::remove_file(&manifest_path).map_err(|source| BuildError::Io {
                path: manifest_path.clone(),
                source,
            })?;
        }
        for name in [
            "TSC",
            "TAILWIND_CLI",
            "XDG_CACHE_HOME",
            "CARGO_NET_OFFLINE",
            "HOME",
            "PATH",
            "HTTPS_PROXY",
            "HTTP_PROXY",
            "ALL_PROXY",
            "NO_PROXY",
            "https_proxy",
            "http_proxy",
            "all_proxy",
            "no_proxy",
        ] {
            println!("cargo::rerun-if-env-changed={name}");
        }
        if let Some(assets_dir) = self.assets_dir.clone() {
            let source_dir = manifest_dir.join(assets_dir);
            watch_dir(&source_dir, out_dir)?;
            let metadata = fs::symlink_metadata(&source_dir).map_err(|source| BuildError::Io {
                path: source_dir.clone(),
                source,
            })?;
            if metadata.file_type().is_symlink() {
                return Err(BuildError::InvalidPath {
                    path: source_dir,
                    reason: "symlinks are not supported",
                });
            }
            if !metadata.is_dir() {
                return Err(BuildError::NotADirectory { path: source_dir });
            }
            let mut files = Vec::new();
            collect_files(&source_dir, "", &mut files)?;
            files.sort();
            for (logical_name, path) in files {
                let bytes = fs::read(&path).map_err(|source| BuildError::Io {
                    path: path.clone(),
                    source,
                })?;
                self.add_output(&logical_name, bytes, &path)?;
            }
        }

        if !self.entries.is_empty() {
            self.build_entries(manifest_dir, out_dir, tool_env)?;
        }

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
            use fmt::Write as _;
            // Writing to a String is infallible; discard only fmt::Result.
            let _ = writeln!(
                manifest,
                "Asset {{ logical_name: {:?}, hashed_name: {:?}, content_type: {:?}, bytes: include_bytes!(concat!(env!(\"OUT_DIR\"), {:?})) }},",
                record.logical_name, record.hashed_name, record.content_type, relative
            );
        }
        manifest.push_str("]\n");
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
        let hashed_name = match logical_name.rsplit_once('/') {
            Some((parent, _)) => format!("{parent}/{hashed_file}"),
            None => hashed_file,
        };
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
            content_type: if Path::new(logical_name)
                .extension()
                .is_some_and(|ext| ext == "map")
            {
                "application/json".to_owned()
            } else {
                mime_guess::from_path(logical_name)
                    .first_or_octet_stream()
                    .to_string()
            },
            source_path: source_path.to_path_buf(),
            bytes,
        });
        Ok(hashed_name)
    }

    fn validate_entries(&self) -> Result<(), BuildError> {
        for entry in &self.entries {
            validate_name(&entry.logical_name, &entry.source)?;
            if Path::new(&entry.logical_name)
                .extension()
                .is_none_or(|ext| ext != "js")
            {
                return Err(BuildError::Frontend(format!(
                    "logical entry name must end in .js: {}",
                    entry.logical_name
                )));
            }
            validate_name(&entry.source.to_string_lossy(), &entry.source)?;
            if !matches!(
                entry.source.extension().and_then(|x| x.to_str()),
                Some("ts" | "js")
            ) {
                return Err(BuildError::Frontend(format!(
                    "entry source must end in .ts or .js: {}",
                    entry.source.display()
                )));
            }
            if let EntryFormat::Iife {
                global_name: Some(name),
            } = &entry.format
                && !valid_global(name)
            {
                return Err(BuildError::Frontend(format!(
                    "invalid IIFE global name {name:?}"
                )));
            }
            let map_name = format!("{}.map", entry.logical_name);
            if self
                .outputs
                .iter()
                .any(|r| r.logical_name == entry.logical_name || r.logical_name == map_name)
                || self
                    .entries
                    .iter()
                    .filter(|candidate| {
                        candidate.logical_name == entry.logical_name
                            || candidate.logical_name == map_name
                    })
                    .count()
                    != 1
            {
                return Err(BuildError::Frontend(format!(
                    "entry output collision: {} or {map_name}",
                    entry.logical_name
                )));
            }
        }
        Ok(())
    }

    fn check_types(
        &self,
        frontend: &Path,
        config: &Path,
        app_root: &Path,
        out_dir: &Path,
        tool_env: &tool_cache::ToolEnv,
    ) -> Result<(), BuildError> {
        let executable = tool_cache::resolve_tsc(tool_env, tool_cache::fetch_https)
            .map_err(BuildError::Frontend)?;
        watch_dir(&executable, out_dir)?;
        if tool_env.override_path.is_none() {
            watch_dir(
                &executable
                    .ancestors()
                    .nth(3)
                    .unwrap_or(&executable)
                    .join(".sha256"),
                out_dir,
            )?;
        }
        let tsc_result = tool_cache::run_tsc(&executable, frontend, config);
        let listed = match tsc_result {
            Ok(files) => files,
            Err(diagnostics) => {
                let mut detail = diagnostics;
                for (index, entry) in self.entries.iter().enumerate() {
                    let scratch = out_dir.join(format!("cja-bundle-diagnostics-{index}"));
                    if let Err(error) =
                        bundler::bundle_entry(entry, frontend, app_root, &scratch, false)
                    {
                        use fmt::Write as _;
                        let _ = write!(detail, "\nRolldown: {error}");
                    }
                }
                return Err(BuildError::Frontend(detail));
            }
        };
        let tool_lib = executable.parent().unwrap_or(&executable);
        for file in listed {
            if !file.starts_with(tool_lib) {
                watch_dir(&file, out_dir)?;
                if let Some(parent) = file.parent() {
                    watch_dir(parent, out_dir)?;
                }
            }
        }
        Ok(())
    }

    fn build_entries(
        &mut self,
        manifest_dir: &Path,
        out_dir: &Path,
        tool_env: &tool_cache::ToolEnv,
    ) -> Result<(), BuildError> {
        let frontend = manifest_dir.join(&self.frontend_dir);
        watch_dir(&frontend, out_dir)?;
        let frontend = frontend.canonicalize().map_err(|e| {
            BuildError::Frontend(format!("frontend root {}: {e}", frontend.display()))
        })?;
        let app_root = manifest_dir
            .canonicalize()
            .map_err(|e| BuildError::Frontend(e.to_string()))?;
        if !frontend.starts_with(&app_root) {
            return Err(BuildError::Frontend(format!(
                "frontend root {} escapes app root",
                frontend.display()
            )));
        }
        let config = frontend.join("tsconfig.json");
        if !config.is_file() {
            return Err(BuildError::Frontend(format!(
                "{} is required for TypeScript entries; add a strict tsconfig.json with noEmit, ES2022 and DOM libs",
                config.display()
            )));
        }
        watch_config(&config, out_dir, &mut Vec::new())?;
        self.validate_entries()?;
        self.check_types(&frontend, &config, &app_root, out_dir, tool_env)?;
        for (index, entry) in self.entries.clone().iter().enumerate() {
            let scratch = out_dir.join(format!("cja-bundle-{index}"));
            let bundle = bundler::bundle_entry(entry, &frontend, &app_root, &scratch, true)
                .map_err(BuildError::Frontend)?;
            for file in bundle.watch_files {
                watch_dir(&file, out_dir)?;
            }
            let map_logical = format!("{}.map", entry.logical_name);
            let mut map: serde_json::Value = serde_json::from_slice(&bundle.map).map_err(|e| {
                BuildError::Frontend(format!(
                    "invalid Rolldown source map for {}: {e}",
                    entry.logical_name
                ))
            })?;
            if map.get("version").and_then(serde_json::Value::as_u64) != Some(3)
                || !map.get("sources").is_some_and(serde_json::Value::is_array)
                || !map
                    .get("mappings")
                    .is_some_and(serde_json::Value::is_string)
                || !map
                    .get("sourcesContent")
                    .is_some_and(serde_json::Value::is_array)
            {
                return Err(BuildError::Frontend(format!(
                    "incomplete Rolldown source map for {}",
                    entry.logical_name
                )));
            }
            let object = map
                .as_object_mut()
                .ok_or_else(|| BuildError::Frontend("source map is not an object".to_owned()))?;
            object.insert(
                "file".to_owned(),
                serde_json::Value::String(entry.logical_name.clone()),
            );
            let map_bytes =
                serde_json::to_vec(&map).map_err(|e| BuildError::Frontend(e.to_string()))?;
            let map_hashed =
                self.add_output(&map_logical, map_bytes, &frontend.join(&entry.source))?;
            let map_basename = Path::new(&map_hashed)
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| BuildError::Frontend("invalid hashed map name".to_owned()))?;
            let mut js =
                String::from_utf8(bundle.js).map_err(|e| BuildError::Frontend(e.to_string()))?;
            let old = "sourceMappingURL=entry.js.map";
            if js.matches(old).count() != 1 {
                return Err(BuildError::Frontend(format!(
                    "expected one Rolldown sourceMappingURL for {}",
                    entry.logical_name
                )));
            }
            js = js.replace(old, &format!("sourceMappingURL={map_basename}"));
            self.add_output(
                &entry.logical_name,
                js.into_bytes(),
                &frontend.join(&entry.source),
            )?;
        }
        Ok(())
    }
}

fn valid_global(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_' || b == b'$')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'$')
}

fn watch_config(path: &Path, out_dir: &Path, seen: &mut Vec<PathBuf>) -> Result<(), BuildError> {
    let path = path
        .canonicalize()
        .map_err(|e| BuildError::Frontend(format!("tsconfig {}: {e}", path.display())))?;
    if seen.contains(&path) {
        return Ok(());
    }
    seen.push(path.clone());
    watch_dir(&path, out_dir)?;
    if let Some(parent) = path.parent() {
        watch_dir(parent, out_dir)?;
    }
    let text = fs::read_to_string(&path)
        .map_err(|e| BuildError::Frontend(format!("tsconfig {}: {e}", path.display())))?;
    let value: serde_json::Value = json5::from_str(&text)
        .map_err(|e| BuildError::Frontend(format!("tsconfig {}: {e}", path.display())))?;
    for key in ["include", "files"] {
        if let Some(patterns) = value.get(key) {
            let patterns = patterns.as_array().ok_or_else(|| {
                BuildError::Frontend(format!("tsconfig {} has non-array {key}", path.display()))
            })?;
            for pattern in patterns {
                let pattern = pattern.as_str().ok_or_else(|| {
                    BuildError::Frontend(format!(
                        "tsconfig {} has non-string {key} entry",
                        path.display()
                    ))
                })?;
                watch_pattern_root(&path, pattern, out_dir)?;
            }
        }
    }
    if let Some(extends) = value.get("extends") {
        let values: Vec<_> = match extends {
            serde_json::Value::String(raw) => vec![raw.as_str()],
            serde_json::Value::Array(values) => values
                .iter()
                .map(|value| {
                    value.as_str().ok_or_else(|| {
                        BuildError::Frontend(format!("unsupported extends in {}", path.display()))
                    })
                })
                .collect::<Result<_, _>>()?,
            _ => {
                return Err(BuildError::Frontend(format!(
                    "unsupported extends in {}",
                    path.display()
                )));
            }
        };
        for raw in values {
            if !raw.starts_with("./") && !raw.starts_with("../") {
                return Err(BuildError::Frontend(format!(
                    "unsupported extends {raw:?} in {}: use a relative config path",
                    path.display()
                )));
            }
            let parent = path
                .parent()
                .ok_or_else(|| BuildError::Frontend("tsconfig has no parent".to_owned()))?;
            let target = parent.join(raw);
            let target = if target.extension().is_some() {
                target
            } else {
                target.with_extension("json")
            };
            watch_config(&target, out_dir, seen)?;
        }
    }
    Ok(())
}

fn watch_pattern_root(config: &Path, pattern: &str, out_dir: &Path) -> Result<(), BuildError> {
    let base = config
        .parent()
        .ok_or_else(|| BuildError::Frontend("tsconfig has no parent".to_owned()))?;
    let prefix = pattern.split(['*', '?']).next().unwrap_or("");
    let mut path = base.join(prefix);
    while !path.is_dir() {
        path = path
            .parent()
            .ok_or_else(|| {
                BuildError::Frontend(format!(
                    "cannot watch tsconfig pattern {pattern:?} from {}",
                    config.display()
                ))
            })?
            .to_path_buf();
    }
    watch_dir(&path, out_dir)
}

impl Default for AssetsBuilder {
    fn default() -> Self {
        Self::new()
    }
}

fn watch_dir(path: &Path, out_dir: &Path) -> Result<(), BuildError> {
    let out = out_dir
        .canonicalize()
        .unwrap_or_else(|_| out_dir.to_path_buf());
    let watched = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if out.starts_with(&watched) || watched.starts_with(&out) {
        return Err(BuildError::Frontend(format!(
            "refusing Cargo watch of {} because it contains or is inside OUT_DIR {}",
            watched.display(),
            out.display()
        )));
    }
    if watched.is_dir() && watched.join("target").is_dir() {
        return Err(BuildError::Frontend(format!(
            "refusing Cargo watch of {} because it contains target/",
            watched.display()
        )));
    }
    println!("cargo::rerun-if-changed={}", watched.display());
    Ok(())
}

/// The shared content hash for static files and later generated assets.
pub(crate) fn short_hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))[..8].to_owned()
}

fn collect_files(
    dir: &Path,
    relative_dir: &str,
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
        let file_name = entry.file_name();
        let path = entry.path();
        if file_name.as_encoded_bytes().first() == Some(&b'.') {
            continue;
        }
        let component = file_name.to_str().ok_or_else(|| BuildError::InvalidPath {
            path: path.clone(),
            reason: "non-UTF-8 name",
        })?;
        let name = if relative_dir.is_empty() {
            component.to_owned()
        } else {
            format!("{relative_dir}/{component}")
        };
        validate_name(&name, &path)?;
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
            collect_files(&path, &name, files)?;
        } else if kind.is_file() {
            files.push((name, path));
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

    fn fixture_path(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    fn fake_env(root: &Path) -> tool_cache::ToolEnv {
        let mut env = tool_cache::ToolEnv::capture(root);
        env.override_path = Some(fixture_path("bin/fake-tsc").display().to_string());
        env
    }

    fn fixture() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp directory")
    }

    #[test]
    fn hash_is_content_derived() {
        assert_eq!(short_hash(b"abc"), "ba7816bf");
        assert_eq!(short_hash(b"abc"), short_hash(b"abc"));
        assert_ne!(short_hash(b"abc"), short_hash(b"abd"));
        let mut builder = AssetsBuilder::new().static_dir("assets");
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
    fn empty_builder_emits_empty_manifest() {
        let temp = fixture();
        let out = temp.path().join("out");
        AssetsBuilder::new().build_test(temp.path(), &out).unwrap();
        assert_eq!(
            fs::read_to_string(out.join("cja_assets.rs")).unwrap(),
            "[\n]\n"
        );
    }

    #[test]
    fn entry_validation_and_missing_tsconfig_fail() {
        let temp = fixture();
        let out = temp.path().join("out");
        fs::create_dir(temp.path().join("frontend")).unwrap();
        let error = AssetsBuilder::new()
            .entry(Entry::new("src/main.ts", "studio.js"))
            .build_test(temp.path(), &out)
            .unwrap_err();
        assert!(error.to_string().contains("tsconfig.json is required"));
        fs::write(temp.path().join("frontend/tsconfig.json"), "{}").unwrap();
        for (source, name, expected) in [
            ("src/main.css", "studio.js", "must end in .ts or .js"),
            ("src/main.ts", "studio.css", "must end in .js"),
            ("src/main.ts", "bad name.js", "invalid asset path"),
        ] {
            let error = AssetsBuilder::new()
                .entry(Entry::new(source, name))
                .build_test(temp.path(), &out)
                .unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
        }
        let error = AssetsBuilder::new()
            .entry(
                Entry::new("src/main.ts", "studio.js").format(EntryFormat::Iife {
                    global_name: Some("bad.name".to_owned()),
                }),
            )
            .build_test(temp.path(), &out)
            .unwrap_err();
        assert!(error.to_string().contains("invalid IIFE global"));
    }

    #[test]
    fn type_error_keeps_tsc_diagnostics_and_no_manifest() {
        let _guard = crate::SPAWN.lock().unwrap();
        let root = fixture_path("type_error");
        let out = tempfile::tempdir().unwrap();
        let error = AssetsBuilder::new()
            .frontend_dir(&root)
            .entry(Entry::new("src/main.ts", "studio.js"))
            .build_in(&root, out.path(), &fake_env(&root))
            .unwrap_err();
        assert!(error.to_string().contains("TS2322"), "{error}");
        assert!(error.to_string().contains("main.ts(1,7)"), "{error}");
        assert!(!out.path().join("cja_assets.rs").exists());
    }

    #[test]
    fn tsc_diagnostics_precede_rolldown_syntax_diagnostics() {
        let _guard = crate::SPAWN.lock().unwrap();
        let root = fixture_path("syntax");
        let out = tempfile::tempdir().unwrap();
        let error = AssetsBuilder::new()
            .frontend_dir(&root)
            .entry(Entry::new("src/main.ts", "studio.js"))
            .build_in(&root, out.path(), &fake_env(&root))
            .unwrap_err()
            .to_string();
        let tsc = error.find("TS2322").expect("tsc diagnostics");
        let rolldown = error.find("Rolldown:").expect("Rolldown diagnostics");
        assert!(tsc < rolldown, "{error}");
        assert!(error.contains("main.ts:1:"), "{error}");
        assert!(!out.path().join("cja_assets.rs").exists());
    }

    #[test]
    fn entry_outside_tsconfig_program_is_not_bundled_unchecked() {
        // tsc --listFiles names every file it checked. An entry absent from that list was never
        // type checked, so bundling it would ship unchecked TypeScript.
        let root = fixture();
        let frontend = root.path().join("frontend");
        fs::create_dir_all(frontend.join("src")).unwrap();
        fs::copy(
            fixture_path("basic/tsconfig.json"),
            frontend.join("tsconfig.json"),
        )
        .unwrap();
        fs::write(frontend.join("src/util.ts"), "export const suffix = 'x';\n").unwrap();
        // Outside `include: ["src/**/*.ts"]`; real tsc would never see this type error.
        fs::write(
            frontend.join("main.ts"),
            "const answer: number = 'not checked';\nconsole.log(answer);\n",
        )
        .unwrap();
        let out = root.path().join("out");
        let result = {
            // Release SPAWN before asserting so this pin cannot poison other tests.
            let _guard = crate::SPAWN
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            AssetsBuilder::new()
                .entry(Entry::new("main.ts", "studio.js"))
                .build_in(root.path(), &out, &fake_env(root.path()))
        };
        assert!(
            result.is_err(),
            "entry main.ts was bundled although tsc never listed it"
        );
        assert!(!out.join("cja_assets.rs").exists());
    }

    #[test]
    fn duplicate_entries_and_static_collision_fail() {
        let root = fixture_path("basic");
        let out = tempfile::tempdir().unwrap();
        let error = AssetsBuilder::new()
            .frontend_dir(&root)
            .entry(Entry::new("src/main.ts", "studio.js"))
            .entry(Entry::new("src/util.ts", "studio.js"))
            .build_in(&root, out.path(), &fake_env(&root))
            .unwrap_err();
        assert!(error.to_string().contains("collision"));
        assert!(!out.path().join("cja_assets.rs").exists());
    }

    #[test]
    fn two_entries_stale_cleanup_and_failure_leave_no_manifest() {
        let _guard = crate::SPAWN.lock().unwrap();
        let root = fixture();
        let frontend = root.path().join("frontend");
        fs::create_dir_all(frontend.join("src")).unwrap();
        for file in ["tsconfig.json", "src/main.ts", "src/util.ts"] {
            let original = fixture_path("basic").join(file);
            let target = frontend.join(file);
            fs::copy(original, target).unwrap();
        }
        fs::write(
            frontend.join("src/other.ts"),
            "document.body.dataset.other = 'other-entry';\n",
        )
        .unwrap();
        let out = root.path().join("out");
        fs::create_dir_all(&out).unwrap();
        fs::write(out.join("unrelated"), b"keep").unwrap();
        let env = fake_env(root.path());
        let builder = || {
            AssetsBuilder::new()
                .entry(Entry::new("src/main.ts", "studio.js"))
                .entry(Entry::new("src/other.ts", "other.js"))
        };
        builder().build_in(root.path(), &out, &env).unwrap();
        let manifest = fs::read_to_string(out.join("cja_assets.rs")).unwrap();
        assert!(manifest.contains("studio.js") && manifest.contains("other.js"));
        assert_eq!(fs::read(out.join("unrelated")).unwrap(), b"keep");
        AssetsBuilder::new()
            .entry(Entry::new("src/main.ts", "studio.js"))
            .build_in(root.path(), &out, &env)
            .unwrap();
        assert!(
            !fs::read_to_string(out.join("cja_assets.rs"))
                .unwrap()
                .contains("other.js")
        );
        assert_eq!(fs::read_dir(out.join("cja-assets")).unwrap().count(), 2);
        fs::write(
            frontend.join("src/main.ts"),
            "import value from 'bare-package';\nconsole.log(value);\n",
        )
        .unwrap();
        let error = AssetsBuilder::new()
            .entry(Entry::new("src/main.ts", "studio.js"))
            .build_in(root.path(), &out, &env)
            .unwrap_err();
        assert!(error.to_string().contains("bare-package"), "{error}");
        assert!(!out.join("cja_assets.rs").exists());
        assert_eq!(fs::read_dir(out.join("cja-assets")).unwrap().count(), 0);
        assert_eq!(fs::read(out.join("unrelated")).unwrap(), b"keep");
    }

    #[test]
    fn static_js_and_map_names_collide_with_entries() {
        let root = fixture();
        let frontend = root.path().join("frontend");
        fs::create_dir_all(frontend.join("src")).unwrap();
        fs::copy(
            fixture_path("basic/tsconfig.json"),
            frontend.join("tsconfig.json"),
        )
        .unwrap();
        let assets = root.path().join("assets");
        fs::create_dir(&assets).unwrap();
        for name in ["studio.js", "studio.js.map"] {
            fs::write(assets.join(name), b"collision").unwrap();
            let error = AssetsBuilder::new()
                .static_dir("assets")
                .entry(Entry::new("src/main.ts", "studio.js"))
                .build_in(
                    root.path(),
                    &root.path().join("out"),
                    &fake_env(root.path()),
                )
                .unwrap_err();
            assert!(error.to_string().contains("collision"), "{error}");
            fs::remove_file(assets.join(name)).unwrap();
        }
    }

    #[test]
    fn map_mime_is_json() {
        let mut builder = AssetsBuilder::new();
        builder
            .add_output("studio.js.map", b"{}".to_vec(), Path::new("studio.js.map"))
            .unwrap();
        assert_eq!(builder.outputs[0].content_type, "application/json");
    }

    #[test]
    fn watching_out_dir_fails() {
        let root = tempfile::tempdir().unwrap();
        let frontend = root.path().join("frontend");
        let out = frontend.join("target/debug/out");
        fs::create_dir_all(&out).unwrap();
        let error = watch_dir(&frontend, &out).unwrap_err();
        assert!(error.to_string().contains("OUT_DIR"));
    }

    #[test]
    fn frontend_containing_target_is_not_watched() {
        let root = fixture();
        let frontend = root.path().join("frontend");
        fs::create_dir_all(frontend.join("target")).unwrap();
        let error = watch_dir(&frontend, &root.path().join("out")).unwrap_err();
        assert!(error.to_string().contains("target/"));
    }

    #[test]
    fn relative_tsconfig_extends_are_watched_and_package_extends_fail() {
        let root = fixture();
        let frontend = root.path().join("frontend");
        fs::create_dir(&frontend).unwrap();
        fs::write(root.path().join("shared.json"), "{}").unwrap();
        fs::write(frontend.join("base.json"), "{}").unwrap();
        let config = frontend.join("tsconfig.json");
        fs::write(&config, "{extends: ['./base.json', '../shared.json']}").unwrap();
        let mut seen = Vec::new();
        let out = fixture();
        watch_config(&config, out.path(), &mut seen).unwrap();
        assert_eq!(seen.len(), 3);
        fs::write(&config, "{extends: 'a-package'}").unwrap();
        let error = watch_config(&config, out.path(), &mut Vec::new()).unwrap_err();
        assert!(error.to_string().contains("a-package"));
        assert!(error.to_string().contains("tsconfig.json"));
        let shared = root.path().join("shared");
        fs::create_dir(&shared).unwrap();
        fs::write(&config, "{include: ['../shared/**/*.d.ts']}").unwrap();
        let error = watch_config(&config, &shared.join("target/out"), &mut Vec::new()).unwrap_err();
        assert!(error.to_string().contains("shared"));
        assert!(error.to_string().contains("OUT_DIR"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn invalid_utf8_dot_entries_are_ignored() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt as _};
        let temp = fixture();
        let assets = temp.path().join("assets");
        fs::create_dir(&assets).unwrap();
        fs::write(
            assets.join(OsString::from_vec(vec![b'.', 0xff])),
            b"ignored",
        )
        .unwrap();
        let hidden = assets.join(OsString::from_vec(vec![b'.', 0xfe]));
        fs::create_dir(&hidden).unwrap();
        fs::write(hidden.join("file"), b"ignored").unwrap();
        AssetsBuilder::new()
            .static_dir("assets")
            .build_test(temp.path(), &temp.path().join("out"))
            .unwrap();
    }

    #[test]
    fn nested_manifest_is_deterministic_and_owned_output_is_cleaned() {
        let temp = fixture();
        let assets = temp.path().join("assets");
        let out = temp.path().join("out");
        fs::create_dir_all(assets.join("img")).unwrap();
        fs::create_dir_all(&out).unwrap();
        for name in ["z.css", "a.css", "m.css", "b.css"] {
            fs::write(assets.join(name), name.as_bytes()).unwrap();
        }
        fs::write(assets.join("img/logo.svg"), b"<svg/>").unwrap();
        fs::write(assets.join("empty"), b"").unwrap();
        fs::write(assets.join(".hidden"), b"secret").unwrap();
        fs::create_dir(assets.join(".dir")).unwrap();
        fs::write(assets.join(".dir/inner.css"), b"secret").unwrap();
        fs::write(out.join("unrelated"), b"keep").unwrap();
        AssetsBuilder::new()
            .static_dir("assets")
            .build_test(temp.path(), &out)
            .unwrap();
        let generated = fs::read_to_string(out.join("cja_assets.rs")).unwrap();
        let empty_name = format!("empty.{}", short_hash(b""));
        let logo_name = format!("img/logo.{}.svg", short_hash(b"<svg/>"));
        assert!(generated.contains(&logo_name));
        assert!(generated.contains(&empty_name));
        assert!(generated.contains("image/svg+xml"));
        assert!(generated.contains("include_bytes!"));
        assert!(!generated.contains(".hidden"));
        assert!(!generated.contains("inner.css"));
        let logical_names: Vec<_> = generated
            .lines()
            .filter_map(|line| line.split("logical_name: \"").nth(1))
            .map(|tail| tail.split('"').next().unwrap())
            .collect();
        assert_eq!(
            logical_names,
            ["a.css", "b.css", "empty", "img/logo.svg", "m.css", "z.css"]
        );
        assert_eq!(
            fs::read(out.join("cja-assets").join(&logo_name)).unwrap(),
            b"<svg/>"
        );
        fs::write(assets.join("img/logo.svg"), b"<svg>changed</svg>").unwrap();
        AssetsBuilder::new()
            .static_dir("assets")
            .build_test(temp.path(), &out)
            .unwrap();
        assert!(!out.join("cja-assets").join(logo_name).exists());
        assert_eq!(fs::read(out.join("unrelated")).unwrap(), b"keep");
    }

    #[test]
    fn missing_directory_names_the_path() {
        let temp = fixture();
        let err = AssetsBuilder::new()
            .static_dir("no-such-assets")
            .build_test(temp.path(), &temp.path().join("out"))
            .unwrap_err();
        assert!(matches!(err, BuildError::Io { .. }));
        assert!(format!("{err:?}").contains("no-such-assets"));
        assert!(format!("{err:?}").contains("No such file"));
    }

    #[test]
    fn file_as_root_is_not_a_directory() {
        let temp = fixture();
        fs::write(temp.path().join("assets"), b"not a directory").unwrap();
        let err = AssetsBuilder::new()
            .static_dir("assets")
            .build_test(temp.path(), &temp.path().join("out"))
            .unwrap_err();
        assert!(matches!(err, BuildError::NotADirectory { .. }));
        assert!(err.to_string().contains("assets"));
        assert!(err.to_string().contains("not a directory"));
    }

    #[test]
    fn invalid_name_and_collision_name_both_sources() {
        let mut builder = AssetsBuilder::new().static_dir("assets");
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

    #[cfg(target_os = "linux")]
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
        let err = AssetsBuilder::new()
            .static_dir("assets")
            .build_test(temp.path(), &temp.path().join("out"))
            .unwrap_err();
        assert!(err.to_string().contains("link"));
        fs::remove_file(assets.join("link")).unwrap();
        let invalid = assets.join(OsString::from_vec(vec![0xff]));
        fs::write(&invalid, b"x").unwrap();
        let err = AssetsBuilder::new()
            .static_dir("assets")
            .build_test(temp.path(), &temp.path().join("out"))
            .unwrap_err();
        assert!(matches!(err, BuildError::InvalidPath { .. }));
        assert!(err.to_string().contains("non-UTF-8"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_root_fails_but_dot_prefixed_symlink_is_skipped() {
        use std::os::unix::fs::symlink;
        let temp = fixture();
        fs::create_dir(temp.path().join("real-assets")).unwrap();
        symlink("real-assets", temp.path().join("assets")).unwrap();
        let err = AssetsBuilder::new()
            .static_dir("assets")
            .build_test(temp.path(), &temp.path().join("out"))
            .unwrap_err();
        assert!(matches!(err, BuildError::InvalidPath { .. }));
        assert!(err.to_string().contains("symlinks are not supported"));
        fs::remove_file(temp.path().join("assets")).unwrap();
        fs::create_dir(temp.path().join("assets")).unwrap();
        symlink("missing", temp.path().join("assets/.#lock")).unwrap();
        AssetsBuilder::new()
            .static_dir("assets")
            .build_test(temp.path(), &temp.path().join("out"))
            .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn non_regular_file_names_the_socket() {
        use std::os::unix::net::UnixListener;
        let temp = fixture();
        fs::create_dir(temp.path().join("assets")).unwrap();
        let socket = temp.path().join("assets/socket");
        let _listener = UnixListener::bind(&socket).unwrap();
        let err = AssetsBuilder::new()
            .static_dir("assets")
            .build_test(temp.path(), &temp.path().join("out"))
            .unwrap_err();
        assert!(matches!(err, BuildError::InvalidPath { .. }));
        assert!(err.to_string().contains("socket"));
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_subdirectory_names_the_path() {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = fixture();
        let assets = temp.path().join("assets");
        let secret = assets.join("secret-subdirectory");
        fs::create_dir_all(&secret).unwrap();
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o000)).unwrap();
        let direct = fs::read_dir(&secret).expect_err("this test must run as a non-root user");
        assert_eq!(direct.kind(), io::ErrorKind::PermissionDenied);
        let err = AssetsBuilder::new()
            .static_dir("assets")
            .build_test(temp.path(), &temp.path().join("out"))
            .unwrap_err();
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(err, BuildError::Io { .. }));
        assert!(format!("{err:?}").contains("secret-subdirectory"));
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
        let err = AssetsBuilder::new()
            .static_dir("assets")
            .build_test(temp.path(), &temp.path().join("out"))
            .unwrap_err();
        assert!(matches!(err, BuildError::Io { .. }));
        assert!(err.to_string().contains("secret.css"));
        assert!(format!("{err:?}").contains(&direct.to_string()));
    }
}
