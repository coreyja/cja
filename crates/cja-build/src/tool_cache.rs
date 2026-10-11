//! Shared, verified cache for build-time native tools.

use sha2::{Digest as _, Sha256};
use std::{
    env, fs,
    io::{self, Cursor, Read as _, Write as _},
    path::{Component, Path, PathBuf},
    process::Command,
};

const TSC_VERSION: &str = "7.0.2";
const MAX_DOWNLOAD: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct ToolEnv {
    pub manifest_dir: PathBuf,
    pub override_path: Option<String>,
    pub xdg_cache_home: Option<PathBuf>,
    pub home: Option<PathBuf>,
    pub path: Option<String>,
    pub offline: bool,
    pub host: String,
}

impl ToolEnv {
    pub fn capture(manifest_dir: &Path) -> Self {
        Self {
            manifest_dir: manifest_dir.to_path_buf(),
            override_path: env::var("TSC").ok(),
            xdg_cache_home: env::var_os("XDG_CACHE_HOME").map(PathBuf::from),
            home: env::var_os("HOME").map(PathBuf::from),
            path: env::var("PATH").ok(),
            offline: env::var("CARGO_NET_OFFLINE").is_ok_and(|v| v == "true"),
            host: host_platform(),
        }
    }
}

fn host_platform() -> String {
    let os = if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        std::env::consts::OS
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    };
    if cfg!(all(target_os = "linux", target_env = "musl")) {
        format!("{os}-{arch}-musl")
    } else {
        format!("{os}-{arch}")
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ToolSpec {
    pub name: &'static str,
    pub version: &'static str,
    pub platform: String,
    pub url: String,
    pub digest: &'static str,
    pub archive_executable: Option<&'static str>,
}

impl ToolSpec {
    pub fn tsc(host: &str) -> Result<Self, String> {
        let archive_platform = host.strip_suffix("-musl").unwrap_or(host);
        let digest = match archive_platform {
            "linux-arm64" => "c83d931ac9dd7549cde6e71246aa9d6a9812843023df3e277fe3b5dcf41dd0ea",
            "linux-x64" => "7ecad6f67377e831856367ab062ef394f21506a611405bf8ac0ff039348637d3",
            "darwin-arm64" => "902e2fe1cf0799198ef902c6b8c310a450fef629a6baba41d45641ef75c04ebd",
            "darwin-x64" => "eba158cb54050f723d5ff781438f33de5640054440bb4f2bd170cfe9bc2eb551",
            _ => return Err(format!("unsupported native tsc build host: {host}")),
        };
        Ok(Self {
            name: "tsc",
            version: TSC_VERSION,
            platform: host.to_owned(),
            url: format!(
                "https://github.com/microsoft/typescript-go/releases/download/typescript/v{TSC_VERSION}/typescript-{archive_platform}.tgz"
            ),
            digest,
            archive_executable: Some("package/lib/tsc"),
        })
    }

    fn key(&self) -> String {
        format!("{}-{}-{}", self.name, self.version, self.platform)
    }

    fn executable(&self, root: &Path) -> PathBuf {
        match self.archive_executable {
            Some(executable) => root.join(executable),
            None => root.to_path_buf(),
        }
    }
}

pub(crate) fn resolve_tsc(
    env: &ToolEnv,
    fetch: impl Fn(&str) -> Result<Vec<u8>, String>,
) -> Result<PathBuf, String> {
    if let Some(value) = &env.override_path {
        return resolve_override(value, env);
    }
    let spec = ToolSpec::tsc(&env.host)?;
    resolve(&spec, env, fetch)
}

pub(crate) fn resolve(
    spec: &ToolSpec,
    env: &ToolEnv,
    fetch: impl Fn(&str) -> Result<Vec<u8>, String>,
) -> Result<PathBuf, String> {
    if let Some(override_path) = &env.override_path {
        return resolve_override(override_path, env);
    }
    let base = env
        .xdg_cache_home
        .clone()
        .or_else(|| env.home.as_ref().map(|p| p.join(".cache")))
        .ok_or("no tool cache base: set XDG_CACHE_HOME or HOME, or set TSC")?;
    let cache_parent = base.join("cja");
    fs::create_dir_all(&cache_parent)
        .map_err(|e| format!("create {}: {e}", cache_parent.display()))?;
    let root = cache_parent.join(spec.key());
    let lock_path = cache_parent.join(format!("{}.lock", spec.key()));
    let lock =
        fs::File::create(&lock_path).map_err(|e| format!("open {}: {e}", lock_path.display()))?;
    lock.lock()
        .map_err(|e| format!("lock {}: {e}", lock_path.display()))?;
    let result = resolve_locked(spec, env, &root, fetch);
    let _ = lock.unlock();
    result
}

fn resolve_override(value: &str, env: &ToolEnv) -> Result<PathBuf, String> {
    let path = Path::new(value);
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else if value.contains('/') {
        env.manifest_dir.join(path)
    } else {
        env.path
            .as_deref()
            .unwrap_or("")
            .split(':')
            .map(Path::new)
            .map(|dir| dir.join(value))
            .find(|path| path.is_file())
            .ok_or_else(|| format!("TSC command {value:?} not found on captured PATH"))?
    };
    let candidate = candidate
        .canonicalize()
        .map_err(|e| format!("TSC override {}: {e}", candidate.display()))?;
    if !candidate.is_file() {
        return Err(format!(
            "TSC override is not a regular file: {}",
            candidate.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if fs::metadata(&candidate)
            .map_err(|e| e.to_string())?
            .permissions()
            .mode()
            & 0o111
            == 0
        {
            return Err(format!(
                "TSC override is not executable: {}",
                candidate.display()
            ));
        }
    }
    Ok(candidate)
}

fn resolve_locked(
    spec: &ToolSpec,
    env: &ToolEnv,
    root: &Path,
    fetch: impl Fn(&str) -> Result<Vec<u8>, String>,
) -> Result<PathBuf, String> {
    if valid_cache(spec, root)? {
        return Ok(spec.executable(root));
    }
    if env.offline {
        if root.exists() {
            return Err(format!(
                "{} {} has an invalid cache entry at {}; remove it and reinstall online, or set TSC",
                spec.name,
                spec.version,
                root.display()
            ));
        }
        return Err(format!(
            "{} {} unavailable offline at {}; install it in the cache or set TSC",
            spec.name,
            spec.version,
            root.display()
        ));
    }
    let mut stale = None;
    if root.exists() {
        let path = root.with_extension(format!(
            "stale-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| e.to_string())?
                .as_nanos()
        ));
        fs::rename(root, &path).map_err(|e| {
            format!(
                "move corrupt cache {}: {e}; remove it and retry",
                root.display()
            )
        })?;
        stale = Some(path);
    }
    let result = install(spec, root, fetch);
    match result {
        Ok(()) => {
            if let Some(path) = stale {
                let result = if path.is_dir() {
                    fs::remove_dir_all(&path)
                } else {
                    fs::remove_file(&path)
                };
                result.map_err(|e| format!("remove stale cache {}: {e}", path.display()))?;
            }
            Ok(spec.executable(root))
        }
        Err(error) => Err(match stale {
            Some(path) => format!("{error}; previous cache preserved at {}", path.display()),
            None => error,
        }),
    }
}

fn valid_cache(spec: &ToolSpec, root: &Path) -> Result<bool, String> {
    if !root.exists() {
        return Ok(false);
    }
    match spec.archive_executable {
        Some(_) => {
            let marker = fs::read_to_string(root.join(".sha256")).ok();
            Ok(marker.as_deref().is_some_and(|m| m.trim() == spec.digest)
                && spec.executable(root).is_file()
                && root.join("package/lib/lib.d.ts").is_file())
        }
        None => Ok(root.is_file()
            && format!(
                "{:x}",
                Sha256::digest(fs::read(root).map_err(|e| e.to_string())?)
            ) == spec.digest),
    }
}

fn install(
    spec: &ToolSpec,
    root: &Path,
    fetch: impl Fn(&str) -> Result<Vec<u8>, String>,
) -> Result<(), String> {
    let bytes = fetch(&spec.url)?;
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if actual != spec.digest {
        return Err(format!(
            "{} SHA-256 mismatch: expected {}, got {actual}",
            spec.name, spec.digest
        ));
    }
    let parent = root.parent().ok_or("cache entry has no parent")?;
    let stage = tempfile::Builder::new()
        .prefix(".cja-tool-")
        .tempdir_in(parent)
        .map_err(|e| e.to_string())?;
    if let Some(executable) = spec.archive_executable {
        let decoder = flate2::read::GzDecoder::new(Cursor::new(bytes));
        let mut archive = tar::Archive::new(decoder);
        for entry in archive.entries().map_err(|e| e.to_string())? {
            let mut entry = entry.map_err(|e| e.to_string())?;
            let path = entry.path().map_err(|e| e.to_string())?;
            if path.is_absolute()
                || path
                    .components()
                    .any(|c| !matches!(c, Component::Normal(_)))
            {
                return Err(format!("unsafe tar path: {}", path.display()));
            }
            let kind = entry.header().entry_type();
            if !kind.is_file() && !kind.is_dir() {
                return Err(format!("unsafe tar entry: {}", path.display()));
            }
            entry.unpack_in(stage.path()).map_err(|e| e.to_string())?;
        }
        if !stage.path().join(executable).is_file()
            || !stage.path().join("package/lib/lib.d.ts").is_file()
        {
            return Err("tsc archive lacks executable or declaration libraries".to_owned());
        }
        fs::write(stage.path().join(".sha256"), spec.digest).map_err(|e| e.to_string())?;
        let staged = stage.into_path();
        if let Err(e) = fs::rename(&staged, root) {
            fs::remove_dir_all(&staged).map_err(|cleanup| cleanup.to_string())?;
            if matches!(e.kind(), io::ErrorKind::AlreadyExists)
                || matches!(e.raw_os_error(), Some(17 | 39))
            {
                if valid_cache(spec, root)? {
                    return Ok(());
                }
                return Err(format!(
                    "concurrent cache install left invalid entry at {}; remove it and retry",
                    root.display()
                ));
            }
            return Err(format!("install {}: {e}", root.display()));
        }
    } else {
        let staged = stage.path().join("binary");
        let mut file = fs::File::create(&staged).map_err(|e| e.to_string())?;
        file.write_all(&bytes).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&staged, fs::Permissions::from_mode(0o755))
                .map_err(|e| e.to_string())?;
        }
        fs::rename(staged, root).map_err(|e| format!("install {}: {e}", root.display()))?;
    }
    Ok(())
}

pub(crate) fn fetch_https(url: &str) -> Result<Vec<u8>, String> {
    if !url.starts_with("https://") {
        return Err(format!("tool download requires HTTPS: {url}"));
    }
    let response = ureq::get(url)
        .call()
        .map_err(|e| format!("download {url}: {e}"))?;
    if !response.get_url().starts_with("https://") {
        return Err(format!(
            "tool download redirected away from HTTPS: {}",
            response.get_url()
        ));
    }
    if response
        .header("Content-Length")
        .and_then(|s| s.parse::<u64>().ok())
        .is_some_and(|n| n > MAX_DOWNLOAD)
    {
        return Err("tool download exceeds 64 MiB".to_owned());
    }
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(MAX_DOWNLOAD + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_DOWNLOAD {
        return Err("tool download exceeds 64 MiB".to_owned());
    }
    Ok(bytes)
}

pub(crate) fn run_tsc(
    executable: &Path,
    frontend: &Path,
    config: &Path,
) -> Result<Vec<PathBuf>, String> {
    let output = Command::new(executable)
        .current_dir(frontend)
        .args(["--noEmit", "--listFiles", "--project"])
        .arg(config)
        .output()
        .map_err(|e| format!("run {}: {e}", executable.display()))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(format!("tsc failed ({}):\n{stdout}{stderr}", output.status));
    }
    let mut files = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let path = Path::new(line);
        if !path.is_absolute() || !path.is_file() {
            return Err(format!(
                "tsc --listFiles contained a non-file line: {line:?}\n{stderr}"
            ));
        }
        files.push(path.to_path_buf());
    }
    if files.is_empty() {
        return Err(format!(
            "tsc --listFiles returned no readable files:\n{stdout}{stderr}"
        ));
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(root: &Path) -> ToolEnv {
        ToolEnv {
            manifest_dir: root.to_path_buf(),
            override_path: None,
            xdg_cache_home: Some(root.join("cache")),
            home: None,
            path: None,
            offline: false,
            host: "linux-arm64".to_owned(),
        }
    }

    fn file_spec(bytes: &[u8]) -> ToolSpec {
        ToolSpec {
            name: "tailwindcss",
            version: "4.3.3",
            platform: "linux-arm64".to_owned(),
            url: "https://example.test/tool".to_owned(),
            digest: Box::leak(format!("{:x}", Sha256::digest(bytes)).into_boxed_str()),
            archive_executable: None,
        }
    }

    fn tar_bytes(link: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let encoder = flate2::write::GzEncoder::new(&mut bytes, flate2::Compression::default());
            let mut tar = tar::Builder::new(encoder);
            for (name, content) in [
                ("package/lib/tsc", b"fake tsc".as_slice()),
                ("package/lib/lib.d.ts", b"types".as_slice()),
            ] {
                let mut header = tar::Header::new_gnu();
                header.set_size(content.len() as u64);
                header.set_mode(0o755);
                header.set_cksum();
                tar.append_data(&mut header, name, content).unwrap();
            }
            if link {
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_size(0);
                header.set_mode(0o777);
                header.set_cksum();
                tar.append_link(&mut header, "package/lib/escape", "../../outside")
                    .unwrap();
            }
            tar.finish().unwrap();
        }
        bytes
    }

    fn tar_spec(bytes: &[u8]) -> ToolSpec {
        ToolSpec {
            name: "tsc",
            version: "7.0.2",
            platform: "linux-arm64".to_owned(),
            url: "https://example.test/tsc.tgz".to_owned(),
            digest: Box::leak(format!("{:x}", Sha256::digest(bytes)).into_boxed_str()),
            archive_executable: Some("package/lib/tsc"),
        }
    }

    #[test]
    fn override_precedes_cache_and_offline() {
        let _guard = crate::SPAWN.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fake-tsc");
        fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut env = env(dir.path());
        env.override_path = Some("fake-tsc".to_owned());
        env.path = Some(dir.path().display().to_string());
        env.offline = true;
        let spec = file_spec(b"expected");
        assert_eq!(
            resolve(&spec, &env, |_| panic!("no download")).unwrap(),
            path
        );
        env.host = "unsupported-arch".to_owned();
        assert_eq!(resolve_tsc(&env, |_| panic!("no download")).unwrap(), path);
        env.override_path = Some("./fake-tsc".to_owned());
        assert_eq!(
            resolve(&spec, &env, |_| panic!("no download")).unwrap(),
            path
        );
    }

    #[test]
    fn checksum_mismatch_and_offline_empty_cache_fail() {
        let dir = tempfile::tempdir().unwrap();
        let spec = file_spec(b"expected");
        let mut env = env(dir.path());
        let error = resolve(&spec, &env, |_| Ok(b"wrong".to_vec())).unwrap_err();
        assert!(error.contains("SHA-256 mismatch"), "{error}");
        env.offline = true;
        let error = resolve(&spec, &env, |_| panic!("no download")).unwrap_err();
        assert!(error.contains("set TSC"), "{error}");
    }

    #[test]
    fn file_cache_rehashes_every_hit() {
        let dir = tempfile::tempdir().unwrap();
        let spec = file_spec(b"good");
        let env = env(dir.path());
        let path = resolve(&spec, &env, |_| Ok(b"good".to_vec())).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"good");
        fs::write(&path, b"corrupt").unwrap();
        let error = resolve(&spec, &env, |_| Ok(b"bad".to_vec())).unwrap_err();
        assert!(error.contains("SHA-256 mismatch"), "{error}");
    }

    #[test]
    fn missing_cache_base_is_actionable() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = env(dir.path());
        env.xdg_cache_home = None;
        env.home = None;
        let error = resolve(&file_spec(b"x"), &env, |_| panic!("no download")).unwrap_err();
        assert!(error.contains("XDG_CACHE_HOME or HOME"), "{error}");
    }

    #[test]
    fn markerless_directory_is_replaced_and_failure_preserves_it() {
        let dir = tempfile::tempdir().unwrap();
        let spec = ToolSpec::tsc("linux-arm64").unwrap();
        let env = env(dir.path());
        let root = dir.path().join("cache/cja").join(spec.key());
        fs::create_dir_all(root.join("package/lib")).unwrap();
        fs::write(root.join("package/lib/tsc"), b"old").unwrap();
        let error = resolve(&spec, &env, |_| Err("fetch failed".to_owned())).unwrap_err();
        assert!(error.contains("previous cache preserved at"), "{error}");
        assert!(!root.exists());
        assert!(
            dir.path()
                .join("cache/cja")
                .read_dir()
                .unwrap()
                .any(|entry| entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains("stale-"))
        );
    }

    #[test]
    fn offline_corrupt_cache_names_the_remedy() {
        let dir = tempfile::tempdir().unwrap();
        let spec = ToolSpec::tsc("linux-arm64").unwrap();
        let mut env = env(dir.path());
        env.offline = true;
        let root = dir.path().join("cache/cja").join(spec.key());
        fs::create_dir_all(&root).unwrap();
        let error = resolve(&spec, &env, |_| panic!("no download")).unwrap_err();
        assert!(error.contains("invalid cache entry"), "{error}");
        assert!(
            error.contains("remove it") && error.contains("TSC"),
            "{error}"
        );
    }

    #[test]
    fn markerless_and_wrong_marker_cache_are_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let archive = tar_bytes(false);
        let spec = tar_spec(&archive);
        let env = env(dir.path());
        let root = dir.path().join("cache/cja").join(spec.key());
        fs::create_dir_all(root.join("package/lib")).unwrap();
        fs::write(root.join("package/lib/tsc"), b"old").unwrap();
        let path = resolve(&spec, &env, |_| Ok(archive.clone())).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"fake tsc");
        assert_eq!(
            fs::read_to_string(root.join(".sha256")).unwrap(),
            spec.digest
        );
        fs::write(root.join(".sha256"), "wrong").unwrap();
        let path = resolve(&spec, &env, |_| Ok(archive.clone())).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"fake tsc");
        assert_eq!(
            fs::read_to_string(root.join(".sha256")).unwrap(),
            spec.digest
        );
        let hit = resolve(&spec, &env, |_| panic!("valid cache must not download")).unwrap();
        assert_eq!(hit, path);
    }

    #[test]
    fn malicious_archive_and_lost_rename_fail_safely() {
        let dir = tempfile::tempdir().unwrap();
        let archive = tar_bytes(true);
        let spec = tar_spec(&archive);
        let env = env(dir.path());
        let error = resolve(&spec, &env, |_| Ok(archive.clone())).unwrap_err();
        assert!(error.contains("unsafe tar entry"), "{error}");
        let good = tar_bytes(false);
        let good_spec = tar_spec(&good);
        let root = dir.path().join("cache/cja").join(good_spec.key());
        let path = resolve(&good_spec, &env, |_| Ok(good.clone())).unwrap();
        assert!(path.is_file());
        // Bypass the ordinary cache-hit shortcut to exercise the rename loser.
        install(&good_spec, &root, |_| Ok(good.clone())).unwrap();
        assert_eq!(fs::read(path).unwrap(), b"fake tsc");
    }

    #[test]
    fn concurrent_resolve_installs_one_valid_entry() {
        let dir = tempfile::tempdir().unwrap();
        let archive = tar_bytes(false);
        let spec = tar_spec(&archive);
        let env = env(dir.path());
        std::thread::scope(|scope| {
            let first = scope.spawn(|| resolve(&spec, &env, |_| Ok(archive.clone())));
            let second = scope.spawn(|| resolve(&spec, &env, |_| Ok(archive.clone())));
            assert_eq!(
                first.join().unwrap().unwrap(),
                second.join().unwrap().unwrap()
            );
        });
    }

    #[test]
    fn tsc_stdout_and_stderr_are_both_preserved() {
        let _guard = crate::SPAWN.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("tsc");
        fs::write(&script, "#!/bin/sh\necho 'src/main.ts(1,1): error TS2322'\necho 'secondary stderr' >&2\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let error = run_tsc(&script, dir.path(), dir.path()).unwrap_err();
        assert!(error.contains("TS2322"));
        assert!(error.contains("secondary stderr"));
    }

    #[test]
    #[ignore = "requires a native TypeScript 7 download"]
    fn real_tsc_valid_fixture() {
        let _guard = crate::SPAWN.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let frontend = dir.path().join("frontend");
        fs::create_dir(&frontend).unwrap();
        fs::write(
            frontend.join("tsconfig.json"),
            r#"{"compilerOptions":{"noEmit":true,"strict":true,"allowImportingTsExtensions":true,"module":"ESNext","moduleResolution":"Bundler"},"include":["*.ts"]}"#,
        )
        .unwrap();
        fs::write(frontend.join("main.ts"), "import { answer } from './util.ts';\nconst result: number = answer;\nconsole.log(result);\n").unwrap();
        fs::write(
            frontend.join("util.ts"),
            "export const answer: number = 42;\n",
        )
        .unwrap();
        let env = ToolEnv::capture(dir.path());
        let spec = ToolSpec::tsc(&env.host).unwrap();
        let exe = resolve(&spec, &env, fetch_https).unwrap();
        let files = run_tsc(&exe, &frontend, &frontend.join("tsconfig.json")).unwrap();
        assert!(files.iter().any(|path| path.ends_with("main.ts")));
        assert!(files.iter().any(|path| path.ends_with("util.ts")));
    }

    #[test]
    #[ignore = "requires a native TypeScript 7 download"]
    fn real_tsc_type_error() {
        let _guard = crate::SPAWN.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let frontend = dir.path().join("frontend");
        fs::create_dir(&frontend).unwrap();
        fs::write(
            frontend.join("tsconfig.json"),
            r#"{"compilerOptions":{"noEmit":true,"strict":true},"include":["*.ts"]}"#,
        )
        .unwrap();
        fs::write(
            frontend.join("main.ts"),
            "const answer: number = 'wrong';\n",
        )
        .unwrap();
        let env = ToolEnv::capture(dir.path());
        let spec = ToolSpec::tsc(&env.host).unwrap();
        let exe = resolve(&spec, &env, fetch_https).unwrap();
        let error = run_tsc(&exe, &frontend, &frontend.join("tsconfig.json")).unwrap_err();
        assert!(error.contains("TS2322"), "{error}");
        assert!(error.contains("main.ts(1,"), "{error}");
    }
}
