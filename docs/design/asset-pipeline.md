# cja frontend asset pipeline: node-free spike (DEV-1652)

This records a throwaway experiment on the Lima VM on 2026-10-10. No framework or app code was changed. The scratch crate and raw logs are at `/tmp/mull-scratch/cja-DEV-1652/`; its dedicated Cargo build directory is removed after this evidence is captured.

## Settled product decisions

* **Two crates.** `cja-build` is a new workspace crate in `coreyja/cja` that apps add under `[build-dependencies]`. It owns bundling, type checking, Tailwind, static-file hashing and manifest generation. `rolldown` is pinned `=1.2.13` and appears only in `cja-build`, never in `cja`'s normal dependency tree. The runtime half is `cja::assets`, an always-on module in the `cja` crate (no feature flag): `asset_url`, the asset router, and a macro that includes the generated manifest.
* **TS/JS output.** Bundled with rolldown as a Rust library. One **IIFE per entry** by default; ESM output is an optional builder setting. Always minified, debug builds included. External source maps are always emitted and served next to the bundle. **Relative imports only**: a bare specifier (`import x from "pkg"`) fails the build with an error naming the import and the file it appears in.
* **Type checking** runs in build.rs with **TypeScript 7 native** (`tsc` 7.0.2 from the `microsoft/typescript-go` GitHub release tarballs, ~9 MB per platform). It runs `tsc --noEmit` against the app's `tsconfig.json`, and a type error fails `cargo build` with the diagnostics printed. tsc checks; rolldown emits.
* **Tailwind** is the standalone CLI **v4.3.3**, v4 only (CSS-first config, no `tailwind.config.js`).
* **External tool binaries (tsc, tailwindcss)** resolve in this order:
  1. An env override (`TSC`, `TAILWIND_CLI`).
  2. A shared user cache, `$XDG_CACHE_HOME/cja/<tool>-<version>-<platform>` (default `~/.cache/cja/...`), so every worktree and `cargo clean` reuse one copy.
  3. An HTTPS download of the pinned version, verified against a sha256 hardcoded per platform in `cja-build`.

  A hash mismatch is a build error. When offline (`CARGO_NET_OFFLINE=true`) with no binary, the build fails with a message that says how to install the tool or set the env var. The download is the default; no opt-in flag. Supported platforms: linux x64/arm64 (glibc and musl) and macOS x64/arm64.
* **Loud failure, always.** No placeholder output, no `*_STRICT_*` / `*_PREBUILT_*` env flags. Any frontend failure fails the cargo build.
* **Hashing and serving.**
  * Every emitted file is named `name.<8-hex content hash>.ext` and served at `/assets/<hashed name>` with `Cache-Control: public, max-age=31536000, immutable` and the correct `Content-Type`. `asset_url("app.js")` returns `/assets/app.3f9a1c2b.js`.
  * Unknown paths return 404, with no fallback to the logical name. A small allowlist of stable unhashed paths (`favicon.ico`, `robots.txt`) is served at fixed paths.
  * Plain static files (svg, png, fonts, hand-written css) go through the same hashing and embedding, replacing each app's `include_dir!` + `static_assets.rs`.
* **Embedding.** Always `include_bytes!` into the binary; there is no read-from-disk dev mode. Rebuilds happen via `cargo:rerun-if-changed` only. No live reload in v1.
* **Out of scope for the whole delivery:** npm packages, Svelte (mull and chess keep vite), live reload, and Tailwind v3.
* **cja conventions.** Edition 2024, workspace clippy pedantic = deny, `unsafe_code = forbid`. Clean breaks: no back-compat shims and no `_with_*` variants. Opinionated defaults.

## Method

The experiment is a standalone edition-2024 crate outside the cja workspace. `src/main.ts` imports `./util.ts`, uses an interface and typed `HTMLElement`/`document`, and prints an observable `asset-spike:cja-asset-spike` string. `src/main.rs` embeds the current JS with `include_bytes!(env!("ASSET_JS_PATH"))` and prints its byte length and first 200 bytes. The TypeScript config uses ES2020, ESNext, Bundler resolution, strict mode, DOM and ES2020 libraries, `types: []`, `noEmit: true`, and `allowImportingTsExtensions: true`. `css/input.css` contains `@import "tailwindcss";` and `@source "../src";` on separate lines. `src/maud_fixture.rs` contains a literal `class="bg-red-500 md:flex"`.

The lockfile was generated before timing (`cargo generate-lockfile`, 281 packages; crates.io index updated). “Cold” means an absent `/tmp/mull-scratch/cja-DEV-1652-build`, a generated lockfile, and the host's warm Cargo registry/index cache. The first compiling command was `cargo build`. Every Cargo invocation set `CARGO_BUILD_BUILD_DIR=/tmp/mull-scratch/cja-DEV-1652-build`; final executable was in the scratch crate's `target/debug/`. A `systemd-run --user --scope -p MemoryAccounting=yes` dry run successfully read its own cgroup `memory.peak` before the measured build. No other Cargo/rustc process was running at start. The build was not retried or reconfigured.

Reproduction from the retained scratch sources:

```sh
cd /tmp/mull-scratch/cja-DEV-1652
CARGO_BUILD_BUILD_DIR=/tmp/mull-scratch/cja-DEV-1652-build cargo generate-lockfile
# Ensure the dedicated build dir is absent; this was checked before the measured run.
systemd-run --user --scope -p MemoryAccounting=yes -- ./logs/cold-run.sh
# cold-run.sh runs /usr/bin/time -v -o logs/cold.time cargo build,
# saves Cargo output/status, then reads its scope's memory.peak before exit.
du -sb /tmp/mull-scratch/cja-DEV-1652-build
du -sB1 /tmp/mull-scratch/cja-DEV-1652-build
du -sh /tmp/mull-scratch/cja-DEV-1652-build
touch src/util.ts
CARGO_BUILD_BUILD_DIR=/tmp/mull-scratch/cja-DEV-1652-build /usr/bin/time -v cargo build
```

VM: Ubuntu 24.04.3, Linux 6.8.0-106-generic, aarch64 glibc, 8 vCPU, 15 GiB RAM, 8 GiB swap (3.2 GiB already used), 32 GiB available on `/` before the run. Rust/Cargo 1.99.0. `~/.cargo/config.toml` sets `jobs = 4`, `build-dir = "/home/coreyja.linux/.cache/cargo-builds"` (overridden for this experiment), and aarch64 linker `clang` with `-fuse-ld=mold`. No `RUSTFLAGS` or `CARGO_INCREMENTAL` environment override. The default dev profile is unoptimized with debuginfo and default incremental compilation. These figures are VM- and profile-specific.

## Working build.rs code

This is the compiling final scratch implementation. Rolldown's fixed `app.js` and `app.js.map` outputs are selected from the **current `BundleOutput.assets`**, then SHA-256 is truncated to eight lowercase hex characters for both final files. The source map's optional `file` property is removed to avoid a filename/hash cycle; its `sources` and mappings remain intact. The same hash function can name static files in DEV-1653. The build fails on any rolldown warning, including failed-clean warnings. `clean_dir` applies only to `OUT_DIR/assets`.

```rust
use rolldown::plugin::{HookResolveIdArgs, HookResolveIdReturn, HookUsage, Plugin, PluginContext};
use rolldown::{
    Bundler, BundlerOptions, HashCharacters, InputItem, OutputFormat, Platform, RawMinifyOptions,
    SourceMapType,
};
use sha2::{Digest, Sha256};
use std::{
    borrow::Cow,
    env, fs,
    path::{Component, PathBuf},
};

#[derive(Debug)]
struct RelativeImportsOnly;

impl Plugin for RelativeImportsOnly {
    fn name(&self) -> Cow<'static, str> {
        "relative-imports-only".into()
    }
    async fn resolve_id(
        &self,
        _ctx: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> HookResolveIdReturn {
        if !args.is_entry && !args.specifier.starts_with("./") && !args.specifier.starts_with("../")
        {
            return Err(anyhow::anyhow!(
                "bare import {:?} from {:?} is forbidden; use a relative import",
                args.specifier,
                args.importer.unwrap_or("<unknown>")
            ));
        }
        Ok(None)
    }
    fn register_hook_usage(&self) -> HookUsage {
        HookUsage::ResolveId
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let out_dir = PathBuf::from(env::var("OUT_DIR")?);
    let assets_dir = out_dir.join("assets");
    fs::create_dir_all(&assets_dir)?;
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=tsconfig.json");
    let options = BundlerOptions {
        input: Some(vec![InputItem {
            name: Some("app".into()),
            import: "./src/main.ts".into(),
        }]),
        cwd: Some(manifest_dir),
        dir: Some(assets_dir.to_string_lossy().into_owned()),
        clean_dir: Some(true),
        format: Some(OutputFormat::Iife),
        platform: Some(Platform::Browser),
        minify: Some(RawMinifyOptions::Bool(true)),
        sourcemap: Some(SourceMapType::File),
        hash_characters: Some(HashCharacters::Hex),
        entry_filenames: Some("[name].js".to_string().into()),
        ..Default::default()
    };
    let mut bundler = Bundler::with_plugins(
        options,
        vec![RelativeImportsOnly::new_shared(RelativeImportsOnly)],
    )?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let output = runtime.block_on(bundler.write())?;
    if !output.warnings.is_empty() {
        for warning in &output.warnings {
            eprintln!("rolldown warning: {warning}");
        }
        return Err("rolldown emitted warnings".into());
    }
    let mut js = None;
    let mut map = None;
    for asset in &output.assets {
        let filename = asset.filename();
        let path = std::path::Path::new(filename);
        if path.components().count() != 1
            || !matches!(path.components().next(), Some(Component::Normal(_)))
        {
            return Err(format!("unsafe rolldown output filename: {filename}").into());
        }
        let output_path = assets_dir.join(filename);
        if !output_path.is_file() {
            return Err(format!("returned asset missing: {filename}").into());
        }
        if filename.ends_with(".js") {
            if js.replace(output_path).is_some() {
                return Err("duplicate JS output".into());
            }
        } else if filename.ends_with(".map") {
            if map.replace(output_path).is_some() {
                return Err("duplicate map output".into());
            }
        } else {
            return Err(format!("unexpected output: {filename}").into());
        }
    }
    let js = js.ok_or("missing JS output")?;
    let map = map.ok_or("missing map output")?;
    let map_text = fs::read_to_string(&map)?;
    let map_text = map_text.replace("\"file\":\"app.js\",", "");
    let map_hash = short_hash(map_text.as_bytes());
    let map_name = format!("app.js.{map_hash}.map");
    let final_map = assets_dir.join(&map_name);
    fs::write(&final_map, map_text)?;
    fs::remove_file(&map)?;

    let js_text = fs::read_to_string(&js)?;
    let old_comment = "//# sourceMappingURL=app.js.map";
    if js_text.matches(old_comment).count() != 1 {
        return Err("expected exactly one sourceMappingURL comment".into());
    }
    let js_text = js_text.replace(old_comment, &format!("//# sourceMappingURL={map_name}"));
    let js_hash = short_hash(js_text.as_bytes());
    let final_js = assets_dir.join(format!("app.{js_hash}.js"));
    fs::write(&final_js, js_text)?;
    fs::remove_file(&js)?;
    eprintln!(
        "SPIKE_OUTPUT js={} map={}",
        final_js.display(),
        final_map.display()
    );
    println!("cargo:rustc-env=ASSET_JS_PATH={}", final_js.display());
    Ok(())
}

fn short_hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))[..8].to_owned()
}
```

`main.rs` embeds the returned current JS path:

```rust
const JS: &[u8] = include_bytes!(env!("ASSET_JS_PATH"));
fn main() {
    println!("len={} first={}", JS.len(), String::from_utf8_lossy(&JS[..JS.len().min(200)]));
}
```

## Results

| Check | Result |
| --- | --- |
| Cold Cargo build | **29.12 s**, exit 0; rolldown dependencies compiled into isolated build dir |
| GNU time maximum RSS | **985,100 KiB** = **1,008,742,400 bytes** = **1.008742 GB** = **0.939465 GiB**; largest single process, not aggregate concurrency |
| Scoped cgroup memory peak | **1,882,210,304 bytes** = 1.882210 GB = 1.752945 GiB; aggregate scope context, not the RSS gate |
| Cold build dir immediately after build | Apparent **1,106,248,791 bytes** (1.106249 GB, 1.030274 GiB); allocated **1,115,881,472 bytes** (1.115881 GB, 1.039246 GiB); `du -sh` **1.1G**. Gate uses the larger allocated count. |
| Touch-only warm rebuild | **0.32 s**; Cargo recompiled `cja-asset-spike`; output remained 209-byte JS and 768-byte map under the same `app.17571f0d` rolldown names and identical SHA-256 bytes. |
| Visible `util.ts` edit and restore | Changed string yielded `app.91e3dc39.js` (217 bytes), its map, and a 217-byte embedded binary output. Restore yielded only the original two files and 209-byte embedded output. `clean_dir` removed stale files. |
| IIFE / external map / embedding | Pass: minified `(function(){...})();`, imported `asset-spike:` behavior, no TS syntax/import left. Rolldown output was `app.17571f0d.js` (209 bytes) and `app.17571f0d.js.map` (768 bytes), with a resolving `sourceMappingURL`; binary printed `len=209`. |
| Final content hashes | Pass after post-process: `app.f0a80ffc.js` (209 bytes, SHA-256 starts `f0a80ffc`) and `app.js.69e14a70.map` (743 bytes, SHA-256 starts `69e14a70`); source-map URL resolves. The map is valid JSON with two sources and no optional `file` field. |
| Bare imports | Pass: the `resolve_id` plugin fails both absent and fake-resolvable `left-pad` before ordinary resolution, with specifier and importing `src/main.ts` path in the diagnostic. Both builds exited 101; restored source built cleanly. |
| TypeScript 7.0.2 | Pass: verified 8,900,516-byte archive; extracted whole `package/lib/` tree to shared cache. Clean `--noEmit -p tsconfig.json` exited 0 in **0.18 s** with no `node_modules`; `document` and `HTMLElement` resolved. Injected `string` to `number` mismatch exited 1 with TS2322 and file/line; restored source passed. |
| Tailwind CLI v4.3.3 | Pass: verified 109,881,488-byte binary; `--minify` exited 0 in **0.42 s**; CSS **69,175 bytes**; `.bg-red-500{` and escaped `.md\\:flex{` selectors both present; first CSS line is `/*! tailwindcss v4.3.3 | MIT License | https://tailwindcss.com */`. No `node_modules`. |

### Bare-import mechanism

Rolldown's native `UNRESOLVED_IMPORT` is only a warning/externalization and cannot enforce relative imports when a package resolves. The `resolve_id` hook with `HookUsage::ResolveId` rejects every non-entry specifier not beginning `./` or `../`. Both the absent and fake-resolvable package produced the plugin error shown verbatim in the appendix. The fixture was removed, and the clean source rebuilt. Production code should retain the plugin gate; no package manager or install is involved.

### Hash and source-map behavior

Rolldown's `[hash:8]` is its chunk hash (xxh3_128 with hex characters), **not** SHA-256 of either final emitted file. The native `app.17571f0d.js` had SHA-256 `bf2ff44a...`; its `app.17571f0d.js.map` had SHA-256 `e88dd585...`. Thus the map's name does not hash its own bytes. The JS name also does not hash the final bytes containing its `sourceMappingURL` comment. For the settled every-file content-hash rule, the working post-process emits fixed names from rolldown, removes the map's optional `file` field, hashes/renames the map, rewrites the JS comment to that returned map name, then hashes/renames the final JS. A final on-disk SHA-256 recomputation verified both names. `app.js.<hash>.map` treats `app.js` as the logical name and `.map` as the extension.

### Native tool provenance and cache

| Tool | HTTPS source and checksum | Verified cache |
| --- | --- | --- |
| TypeScript-go 7.0.2 | [linux-arm64 archive](https://github.com/microsoft/typescript-go/releases/download/typescript/v7.0.2/typescript-linux-arm64.tgz); SHA-256 `c83d931ac9dd7549cde6e71246aa9d6a9812843023df3e277fe3b5dcf41dd0ea` matches the [GitHub release API asset digest](https://api.github.com/repos/microsoft/typescript-go/releases/tags/typescript%2Fv7.0.2). This release has no publisher checksum-file asset; `cja-build` must hardcode a verified digest. | `$XDG_CACHE_HOME/cja/tsc-7.0.2-linux-arm64/package/lib/tsc` (default `~/.cache/cja/...`), beside `lib.dom.d.ts` and the other declaration libraries. |
| Tailwind CLI 4.3.3 | [linux-arm64 binary](https://github.com/tailwindlabs/tailwindcss/releases/download/v4.3.3/tailwindcss-linux-arm64); SHA-256 `55fd0b241214eff3de1e8ee4f22796662f2d2e7a49bcfca7477cfd0bac398195` matches the exact line in publisher [sha256sums.txt](https://github.com/tailwindlabs/tailwindcss/releases/download/v4.3.3/sha256sums.txt) and the [release API asset digest](https://api.github.com/repos/tailwindlabs/tailwindcss/releases/tags/v4.3.3). | `$XDG_CACHE_HOME/cja/tailwindcss-4.3.3-linux-arm64` (default `~/.cache/cja/...`). |

Only this VM's linux-arm64 glibc assets were executed. linux x64, linux musl, and macOS x64/arm64 remain unverified on this VM. Tailwind publishes a distinct `tailwindcss-linux-arm64-musl` asset; its key and digest must differ. TypeScript-go 7.0.2 musl compatibility is unverified. These are later platform checks, not failures of this VM spike.

### Surprises

- TS5097 rejected the required `./util.ts` import under `moduleResolution: Bundler` until the scratch `tsconfig.json` added `allowImportingTsExtensions: true`; the option works with `noEmit: true`.
- Tailwind's `--version` generated **83,921 bytes of CSS on stdout**, rather than a simple version string. `--help` displayed `≈ tailwindcss v4.3.3`; the generated CSS banner and release checksum independently identify the binary. `--minify` **preserved** the banner in the output CSS.
- Rolldown's native map has the same chunk hash as its JS, not a hash of the map's own content. Post-processing is required for the settled rule.
- The TypeScript-go release repository was archived by Microsoft in September 2026; the pinned 7.0.2 asset remains downloadable and passed this spike. No version or product decision was changed.

## Go/No-Go

**Go on this VM.** Cold build **29.12 s ≤ 180 s**; maximum single-process RSS **1,008,742,400 ≤ 6,000,000,000 bytes**; larger exact build-dir size **1,115,881,472 ≤ 5,000,000,000 bytes**. Bundle, external map, embedding, relative-import rejection, tsc, Tailwind selectors/banner/checksums, and own-final-content hash checks all passed. `oxc_transformer` per-file type stripping without bundling remains the considered fallback if a later platform or rollout misses a gate; this spike does not invoke it.

## Appendix: raw evidence

### Cold `/usr/bin/time -v` (verbatim)

```text
	Command being timed: "cargo build"
	User time (seconds): 76.78
	System time (seconds): 13.20
	Percent of CPU this job got: 308%
	Elapsed (wall clock) time (h:mm:ss or m:ss): 0:29.12
	Average shared text size (kbytes): 0
	Average unshared data size (kbytes): 0
	Average stack size (kbytes): 0
	Average total size (kbytes): 0
	Maximum resident set size (kbytes): 985100
	Average resident set size (kbytes): 0
	Major (requiring I/O) page faults: 628
	Minor (reclaiming a frame) page faults: 6319162
	Voluntary context switches: 17531
	Involuntary context switches: 6654
	Swaps: 0
	File system inputs: 256936
	File system outputs: 2377600
	Socket messages sent: 0
	Socket messages received: 0
	Signals delivered: 0
	Page size (bytes): 4096
	Exit status: 0
```

### Warm `/usr/bin/time -v` (verbatim)

```text
	Command being timed: "cargo build"
	User time (seconds): 0.11
	System time (seconds): 0.11
	Percent of CPU this job got: 69%
	Elapsed (wall clock) time (h:mm:ss or m:ss): 0:00.32
	Average shared text size (kbytes): 0
	Average unshared data size (kbytes): 0
	Average stack size (kbytes): 0
	Average total size (kbytes): 0
	Maximum resident set size (kbytes): 91636
	Average resident set size (kbytes): 0
	Major (requiring I/O) page faults: 4
	Minor (reclaiming a frame) page faults: 33934
	Voluntary context switches: 1421
	Involuntary context switches: 898
	Swaps: 0
	File system inputs: 27712
	File system outputs: 608
	Socket messages sent: 0
	Socket messages received: 0
	Signals delivered: 0
	Page size (bytes): 4096
	Exit status: 0
```

### Build-dir and scope measurements (verbatim)

```text
1106248791	/tmp/mull-scratch/cja-DEV-1652-build
1115881472	/tmp/mull-scratch/cja-DEV-1652-build
1.1G	/tmp/mull-scratch/cja-DEV-1652-build
memory.peak: 1882210304
```

`df -B1G /` before:

```text
Filesystem     1G-blocks  Used Available Use% Mounted on
/dev/vda1            309   278        32  90% /
```

`df -B1G /` immediately after:

```text
Filesystem     1G-blocks  Used Available Use% Mounted on
/dev/vda1            309   279        31  91% /
```

### Tool checksums and compared digests (verbatim)

```text
c83d931ac9dd7549cde6e71246aa9d6a9812843023df3e277fe3b5dcf41dd0ea  logs/typescript-linux-arm64.tgz
typescript-linux-arm64.tgz	8900516	sha256:c83d931ac9dd7549cde6e71246aa9d6a9812843023df3e277fe3b5dcf41dd0ea
55fd0b241214eff3de1e8ee4f22796662f2d2e7a49bcfca7477cfd0bac398195  logs/tailwindcss-linux-arm64
55fd0b241214eff3de1e8ee4f22796662f2d2e7a49bcfca7477cfd0bac398195  ./tailwindcss-linux-arm64
tailwindcss-linux-arm64	109881488	sha256:55fd0b241214eff3de1e8ee4f22796662f2d2e7a49bcfca7477cfd0bac398195
```

### Plugin errors, absent and fake-resolvable package (verbatim stderr excerpts)

Absent package (`cargo build` exit 101):

```text
   Compiling cja-asset-spike v0.1.0 (/tmp/mull-scratch/cja-DEV-1652)
error: failed to run custom build command for `cja-asset-spike v0.1.0 (/tmp/mull-scratch/cja-DEV-1652)`

Caused by:
  process didn't exit successfully: `/tmp/mull-scratch/cja-DEV-1652-build/debug/build/cja-asset-spike-571b7cd23991dd14/build-script-build` (exit status: 1)
  --- stdout
  cargo:rerun-if-changed=src
  cargo:rerun-if-changed=tsconfig.json

  --- stderr
  Error: BatchedBuildDiagnostic([BuildDiagnostic { severity: Error, kind: "relative-imports-only", message: "plugin `relative-imports-only` threw an error\n\nCaused by:\n    bare import \"left-pad\" from \"/tmp/mull-scratch/cja-DEV-1652/src/main.ts\" is forbidden; use a relative import", .. }])
```

Fake-resolvable `node_modules/left-pad` fixture (`cargo build` exit 101):

```text
   Compiling cja-asset-spike v0.1.0 (/tmp/mull-scratch/cja-DEV-1652)
error: failed to run custom build command for `cja-asset-spike v0.1.0 (/tmp/mull-scratch/cja-DEV-1652)`

Caused by:
  process didn't exit successfully: `/tmp/mull-scratch/cja-DEV-1652-build/debug/build/cja-asset-spike-571b7cd23991dd14/build-script-build` (exit status: 1)
  --- stdout
  cargo:rerun-if-changed=src
  cargo:rerun-if-changed=tsconfig.json

  --- stderr
  Error: BatchedBuildDiagnostic([BuildDiagnostic { severity: Error, kind: "relative-imports-only", message: "plugin `relative-imports-only` threw an error\n\nCaused by:\n    bare import \"left-pad\" from \"/tmp/mull-scratch/cja-DEV-1652/src/main.ts\" is forbidden; use a relative import", .. }])
```

### TypeScript injected error (verbatim)

```text
src/main.ts(9,7): error TS2322: Type 'string' is not assignable to type 'number'.
```

### Tailwind selector extraction and banner (verbatim)

`grep -oF '.bg-red-500{'` and `grep -oF '.md\:flex{'` on the minified CSS:

```text
.bg-red-500{
.md\:flex{
```

```text
.bg-red-500{ 36129 .bg-red-500{background-color:var(--color-red-500)}.bg-transp
.md\:flex{ 63740 .md\:flex{display:flex}.md\:text-sm{font-size:var(--text-sm)
banner= /*! tailwindcss v4.3.3 | MIT License | https://tailwindcss.com */
```

### Final output hashes and validation (verbatim)

```text
f0a80ffc81f6e10484236a7e0fbece2dea7f4671825531ddf5cc3e9a286147c4  /tmp/mull-scratch/cja-DEV-1652-build/debug/build/cja-asset-spike-6c8364eecd75a3dc/out/assets/app.f0a80ffc.js
69e14a701dba3b7b603eb6669854513c5cb76e469a5a7fc124708d563400b2f4  /tmp/mull-scratch/cja-DEV-1652-build/debug/build/cja-asset-spike-6c8364eecd75a3dc/out/assets/app.js.69e14a70.map
app.js.69e14a70.map 743 69e14a70 hash_matches=True
app.f0a80ffc.js 209 f0a80ffc hash_matches=True
iife=true imported_behavior=true external_map=true embedded_path=true
source_map_file_field= <omitted>
```
