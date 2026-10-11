//! All Rolldown API use is confined to this module.

use crate::{Entry, EntryFormat};
use rolldown::plugin::{
    HookResolveIdArgs, HookResolveIdOutput, HookResolveIdReturn, HookUsage, Plugin, PluginContext,
    PluginContextResolveOptions,
};
use rolldown::{
    Bundler, BundlerOptions, InputItem, OutputFormat, Platform, RawMinifyOptions,
    SourceMapPathTransform, SourceMapType,
};
use rolldown_common::Output;
use std::{
    borrow::Cow,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

#[derive(Debug)]
struct RelativeImports {
    app_root: PathBuf,
}

impl Plugin for RelativeImports {
    fn name(&self) -> Cow<'static, str> {
        "cja-relative-imports".into()
    }

    async fn resolve_id(
        &self,
        ctx: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> HookResolveIdReturn {
        if args.is_entry {
            return Ok(None);
        }
        let importer = args.importer.unwrap_or("<unknown>");
        let specifier = args.specifier;
        let location = source_location(importer, specifier);
        if args.kind.is_dynamic() {
            return Err(anyhow::anyhow!(
                "dynamic import {specifier:?} in {location} is unsupported"
            ));
        }
        if !specifier.starts_with("./") && !specifier.starts_with("../") {
            return Err(anyhow::anyhow!(
                "non-relative import {specifier:?} in {location}; use a relative import"
            ));
        }
        if Path::new(specifier)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("css"))
        {
            return Err(anyhow::anyhow!(
                "CSS import {specifier:?} in {location} is unsupported"
            ));
        }
        let resolved = ctx
            .resolve(
                specifier,
                args.importer,
                Some(PluginContextResolveOptions {
                    import_kind: args.kind,
                    skip_self: true,
                    ..Default::default()
                }),
            )
            .await?;
        let resolved = resolved.map_err(|error| {
            anyhow::anyhow!("unresolved import {specifier:?} in {location}: {error}")
        })?;
        let id = Path::new(resolved.id.as_str());
        if id
            .components()
            .any(|part| part.as_os_str() == "node_modules")
        {
            return Err(anyhow::anyhow!(
                "node_modules import {specifier:?} in {location} is forbidden"
            ));
        }
        let canonical = id
            .canonicalize()
            .map_err(|error| anyhow::anyhow!("resolve {specifier:?} in {location}: {error}"))?;
        if !canonical.starts_with(&self.app_root) {
            return Err(anyhow::anyhow!(
                "import {specifier:?} in {location} escapes app root {}",
                self.app_root.display()
            ));
        }
        Ok(Some(HookResolveIdOutput::from_resolved_id(resolved)))
    }

    fn register_hook_usage(&self) -> HookUsage {
        HookUsage::ResolveId
    }
}

fn source_location(importer: &str, specifier: &str) -> String {
    let Some((line, column)) = std::fs::read_to_string(importer).ok().and_then(|source| {
        source
            .lines()
            .enumerate()
            .find_map(|(index, text)| text.find(specifier).map(|column| (index + 1, column + 1)))
    }) else {
        return importer.to_owned();
    };
    format!("{importer}:{line}:{column}")
}

pub(crate) struct Bundle {
    pub js: Vec<u8>,
    pub map: Vec<u8>,
    pub watch_files: Vec<PathBuf>,
}

pub(crate) fn bundle_entry(
    entry: &Entry,
    frontend: &Path,
    app_root: &Path,
    scratch: &Path,
    write: bool,
) -> Result<Bundle, String> {
    let frontend = frontend.canonicalize().map_err(|e| e.to_string())?;
    let app_root = app_root.canonicalize().map_err(|e| e.to_string())?;
    let source = frontend
        .join(&entry.source)
        .canonicalize()
        .map_err(|e| format!("entry {}: {e}", entry.source.display()))?;
    if !source.starts_with(&app_root) {
        return Err(format!("entry {} escapes app root", entry.source.display()));
    }
    std::fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
    let scratch = scratch.canonicalize().map_err(|e| e.to_string())?;
    let options = make_options(entry, &source, frontend, &scratch);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    runtime.block_on(async {
        let mut bundler = Bundler::with_plugins(
            options,
            vec![RelativeImports::new_shared(RelativeImports { app_root })],
        )
        .map_err(|e| e.to_string())?;
        let output = if write {
            bundler.write().await
        } else {
            bundler.generate().await
        }
        .map_err(|e| {
            e.iter()
                .map(|diagnostic| diagnostic.to_diagnostic().to_string())
                .collect::<Vec<_>>()
                .join("\n")
        })?;
        if !output.warnings.is_empty() {
            return Err(output
                .warnings
                .iter()
                .map(|diagnostic| diagnostic.to_diagnostic().to_string())
                .collect::<Vec<_>>()
                .join("\n"));
        }
        let (js, map) = parse_outputs(output.assets.iter().map(|asset| {
            (
                asset.filename().to_owned(),
                matches!(asset, Output::Chunk(_)),
                asset.content_as_bytes().to_vec(),
            )
        }))?;
        let watch_files = bundler
            .watch_files()
            .iter()
            .map(|path| PathBuf::from(path.as_str()))
            .collect();
        bundler.close().await.map_err(|e| e.to_string())?;
        Ok(Bundle {
            js,
            map,
            watch_files,
        })
    })
}

fn parse_outputs(
    files: impl IntoIterator<Item = (String, bool, Vec<u8>)>,
) -> Result<(Vec<u8>, Vec<u8>), String> {
    let mut js = None;
    let mut map = None;
    for (filename, is_chunk, bytes) in files {
        let path = Path::new(&filename);
        if path.components().count() != 1
            || !matches!(path.components().next(), Some(Component::Normal(_)))
        {
            return Err(format!("unsafe rolldown output filename: {filename}"));
        }
        match filename.as_str() {
            "entry.js" if is_chunk => {
                if js.replace(bytes).is_some() {
                    return Err("duplicate JS output".to_owned());
                }
            }
            "entry.js.map" if !is_chunk => {
                if map.replace(bytes).is_some() {
                    return Err("duplicate map output".to_owned());
                }
            }
            "entry.js" | "entry.js.map" => {
                return Err(format!("wrong rolldown output type: {filename}"));
            }
            _ => {
                return Err(format!(
                    "unexpected rolldown output: {filename}; only one self-contained chunk is supported"
                ));
            }
        }
    }
    Ok((
        js.ok_or("rolldown did not emit entry.js")?,
        map.ok_or("rolldown did not emit entry.js.map")?,
    ))
}

fn make_options(entry: &Entry, source: &Path, frontend: PathBuf, scratch: &Path) -> BundlerOptions {
    let source_root = frontend.clone();
    let transform = SourceMapPathTransform::new(Arc::new(move |sources, map_path| {
        let root = source_root.clone();
        let map_dir = PathBuf::from(map_path)
            .parent()
            .unwrap_or(Path::new(""))
            .to_path_buf();
        Box::pin(async move {
            sources
                .into_iter()
                .map(|source| {
                    let path = map_dir
                        .join(&source)
                        .canonicalize()
                        .map_err(|e| anyhow::anyhow!("source map source {source:?}: {e}"))?;
                    let relative = path.strip_prefix(&root).map_err(|_| {
                        anyhow::anyhow!(
                            "source map source {} escapes frontend root {}",
                            path.display(),
                            root.display()
                        )
                    })?;
                    Ok(relative.to_string_lossy().replace('\\', "/"))
                })
                .collect()
        })
    }));
    let format = match entry.format {
        EntryFormat::Iife { .. } => OutputFormat::Iife,
        EntryFormat::Esm => OutputFormat::Esm,
    };
    let name = match &entry.format {
        EntryFormat::Iife { global_name } => global_name.clone(),
        EntryFormat::Esm => None,
    };
    BundlerOptions {
        input: Some(vec![InputItem {
            name: Some("entry".into()),
            import: source.to_string_lossy().into_owned(),
        }]),
        cwd: Some(frontend),
        dir: Some(scratch.to_string_lossy().into_owned()),
        clean_dir: Some(true),
        format: Some(format),
        name,
        platform: Some(Platform::Browser),
        minify: Some(RawMinifyOptions::Bool(true)),
        sourcemap: Some(SourceMapType::File),
        sourcemap_path_transform: Some(transform),
        entry_filenames: Some("[name].js".to_string().into()),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AssetsBuilder, short_hash};

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    fn run(name: &str, format: EntryFormat) -> Result<Bundle, String> {
        let temp = tempfile::tempdir().unwrap();
        let root = fixture(name);
        let entry = Entry::new("src/main.ts", "studio.js").format(format);
        bundle_entry(&entry, &root, &root, temp.path(), false)
    }

    #[test]
    fn rejects_malformed_missing_and_unsafe_outputs() {
        let js = ("entry.js".to_owned(), true, b"code".to_vec());
        let map = ("entry.js.map".to_owned(), false, b"{}".to_vec());
        assert!(parse_outputs([js.clone()]).unwrap_err().contains("map"));
        assert!(
            parse_outputs([map.clone()])
                .unwrap_err()
                .contains("entry.js")
        );
        assert!(
            parse_outputs([js.clone(), js.clone(), map.clone()])
                .unwrap_err()
                .contains("duplicate")
        );
        assert!(
            parse_outputs([
                js.clone(),
                map.clone(),
                ("../escape.js".to_owned(), true, vec![])
            ])
            .unwrap_err()
            .contains("unsafe")
        );
        assert!(
            parse_outputs([
                js.clone(),
                map.clone(),
                ("chunk.js".to_owned(), true, vec![])
            ])
            .unwrap_err()
            .contains("self-contained")
        );
        assert!(
            parse_outputs([("entry.js".to_owned(), false, vec![]), map])
                .unwrap_err()
                .contains("type")
        );
    }

    #[test]
    fn two_file_iife_global_and_single_chunk_esm() {
        let iife = run("basic", EntryFormat::Iife { global_name: None }).unwrap();
        let js = String::from_utf8(iife.js).unwrap();
        assert!(
            js.contains("bundle-one-") && js.contains("bundle-two"),
            "{js}"
        );
        assert!(js.starts_with("(function"), "{js}");
        let global = run(
            "global",
            EntryFormat::Iife {
                global_name: Some("cjaPasskey".to_owned()),
            },
        )
        .unwrap();
        let global_js = String::from_utf8_lossy(&global.js);
        assert!(global_js.contains("var cjaPasskey="), "{global_js}");
        let esm = run("esm", EntryFormat::Esm).unwrap();
        let js = String::from_utf8(esm.js).unwrap();
        assert!(
            js.contains("esm-marker") && !js.contains("./util.ts"),
            "{js}"
        );
    }

    #[test]
    fn rejects_non_relative_node_modules_dynamic_and_css() {
        for (fixture, fragment) in [
            ("bare", "missing-package"),
            ("bare_resolvable", "fake-package"),
            ("relative_node_modules", "node_modules"),
            ("dynamic", "dynamic import"),
            ("css", "CSS import"),
            ("split", "dynamic import"),
        ] {
            let Err(error) = run(fixture, EntryFormat::Esm) else {
                panic!("{fixture} bundled")
            };
            assert!(error.contains(fragment), "{fixture}: {error}");
            assert!(error.contains("main.ts"), "{fixture}: {error}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_import_outside_app_is_rejected() {
        use std::os::unix::fs::symlink;
        let app = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(app.path().join("src")).unwrap();
        std::fs::write(
            app.path().join("src/main.ts"),
            "import { secret } from './escape.ts';\nconsole.log(secret);\n",
        )
        .unwrap();
        std::fs::write(
            outside.path().join("secret.ts"),
            "export const secret = 42;\n",
        )
        .unwrap();
        symlink(
            outside.path().join("secret.ts"),
            app.path().join("src/escape.ts"),
        )
        .unwrap();
        let output = tempfile::tempdir().unwrap();
        let Err(error) = bundle_entry(
            &Entry::new("src/main.ts", "studio.js"),
            app.path(),
            app.path(),
            output.path(),
            false,
        ) else {
            panic!("escaped import bundled")
        };
        assert!(error.contains("escapes app root"), "{error}");
        assert!(error.contains("main.ts:1:"), "{error}");
    }

    #[test]
    fn syntax_and_missing_relative_import_report_source() {
        for fixture in ["syntax", "missing_relative"] {
            let Err(error) = run(fixture, EntryFormat::Iife { global_name: None }) else {
                panic!("{fixture} bundled")
            };
            assert!(error.contains("main.ts"), "{fixture}: {error}");
            assert!(error.contains("main.ts:1:"), "{fixture}: {error}");
        }
    }

    #[test]
    fn final_map_and_js_hashes_are_content_derived() {
        let _guard = crate::SPAWN.lock().unwrap();
        let root = fixture("basic");
        let out = tempfile::tempdir().unwrap();
        let mut env = crate::tool_cache::ToolEnv::capture(&root);
        env.override_path = Some(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/bin/fake-tsc")
                .display()
                .to_string(),
        );
        AssetsBuilder::new()
            .frontend_dir(&root)
            .entry(Entry::new("src/main.ts", "studio.js"))
            .build_in(&root, out.path(), &env)
            .unwrap();
        let dir = out.path().join("cja-assets");
        let js = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|s| s == "js"))
            .unwrap();
        let map = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|s| s == "map"))
            .unwrap();
        let js_bytes = std::fs::read(&js).unwrap();
        let map_bytes = std::fs::read(&map).unwrap();
        assert!(
            js.file_name()
                .unwrap()
                .to_string_lossy()
                .contains(&short_hash(&js_bytes))
        );
        assert!(
            map.file_name()
                .unwrap()
                .to_string_lossy()
                .contains(&short_hash(&map_bytes))
        );
        assert!(String::from_utf8_lossy(&js_bytes).contains(&format!(
            "sourceMappingURL={}",
            map.file_name().unwrap().to_string_lossy()
        )));
        let json: serde_json::Value = serde_json::from_slice(&map_bytes).unwrap();
        let sources = json["sources"].as_array().unwrap();
        assert!(sources.contains(&serde_json::json!("src/main.ts")));
        assert!(sources.contains(&serde_json::json!("src/util.ts")));
        assert_eq!(json["sourcesContent"].as_array().unwrap().len(), 2);
    }
}
