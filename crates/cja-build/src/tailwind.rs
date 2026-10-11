//! Validate the explicit Tailwind scan graph before invoking the CLI.
use std::{
    collections::HashSet,
    fs,
    path::{Component, Path, PathBuf},
};

use crate::{BuildError, watch_dir};

fn error(message: impl Into<String>) -> BuildError {
    BuildError::Frontend(message.into())
}

#[derive(Default)]
struct Directives {
    imports: Vec<String>,
    sources: Vec<String>,
    tailwind_imports: usize,
}

#[derive(Default)]
struct VisitState {
    seen: HashSet<PathBuf>,
    chain: Vec<PathBuf>,
    sources: HashSet<PathBuf>,
    watches: Vec<PathBuf>,
}

// Walk CSS bytes rather than searching lines: comments and quoted content can
// contain inert @rules, while an actual rule may span several lines.
fn directives(css: &str, file: &Path) -> Result<Directives, BuildError> {
    let bytes = css.as_bytes();
    let mut pos = 0;
    let mut result = Directives::default();
    while pos < bytes.len() {
        if bytes[pos..].starts_with(b"/*") {
            pos = skip_comment(bytes, pos, file)?;
            continue;
        }
        if matches!(bytes[pos], b'\'' | b'"') {
            pos = skip_string(bytes, pos, file)?;
            continue;
        }
        if bytes[pos] == b'\\' {
            pos = (pos + 2).min(bytes.len());
            continue;
        }
        if bytes[pos] != b'@' {
            pos += 1;
            continue;
        }
        let start = pos + 1;
        let mut end = start;
        while end < bytes.len() && (bytes[end].is_ascii_alphabetic() || bytes[end] == b'-') {
            end += 1;
        }
        let name = &css[start..end];
        check_rule_case(name, file)?;
        if !matches!(
            name,
            "import" | "source" | "config" | "plugin" | "reference" | "tailwind"
        ) {
            pos = end;
            continue;
        }
        let mut cursor = end;
        while cursor < bytes.len() && bytes[cursor] != b';' {
            if bytes[cursor..].starts_with(b"/*") {
                cursor = skip_comment(bytes, cursor, file)?;
            } else if matches!(bytes[cursor], b'\'' | b'"') {
                cursor = skip_string(bytes, cursor, file)?;
            } else if bytes[cursor] == b'\\' {
                cursor = (cursor + 2).min(bytes.len());
            } else {
                cursor += 1;
            }
        }
        if cursor == bytes.len() {
            return Err(error(format!("unterminated @{name} in {}", file.display())));
        }
        let argument = css[end..cursor].trim();
        match name {
            "import" => {
                if argument == "\"tailwindcss\" source(none)"
                    || argument == "'tailwindcss' source(none)"
                {
                    result.tailwind_imports += 1;
                } else {
                    reject_near_miss_root_import(argument, file)?;
                    let path = quoted_only(argument).ok_or_else(|| {
                        error(format!(
                            "unsupported @import {argument:?} in {}: use a local quoted .css path",
                            file.display()
                        ))
                    })?;
                    if !relative_css(path) {
                        return Err(error(format!(
                            "unsupported @import {argument:?} in {}: use ./ or ../ and a .css file without qualifiers",
                            file.display()
                        )));
                    }
                    result.imports.push(path.to_owned());
                }
            }
            "source" => result.sources.push(source_path(argument, file)?.to_owned()),
            _ => {
                return Err(error(format!(
                    "@{name} is unsupported in {}: use Tailwind v4 CSS-first configuration",
                    file.display()
                )));
            }
        }
        pos = cursor + 1;
    }
    Ok(result)
}

fn skip_comment(bytes: &[u8], mut pos: usize, file: &Path) -> Result<usize, BuildError> {
    pos += 2;
    while pos < bytes.len() {
        if bytes[pos] == b'\\' {
            pos = (pos + 2).min(bytes.len());
        } else if bytes[pos..].starts_with(b"*/") {
            return Ok(pos + 2);
        } else {
            pos += 1;
        }
    }
    Err(error(format!(
        "unterminated CSS comment in {}",
        file.display()
    )))
}

fn check_rule_case(name: &str, file: &Path) -> Result<(), BuildError> {
    if [
        "import",
        "source",
        "config",
        "plugin",
        "reference",
        "tailwind",
    ]
    .iter()
    .any(|known| name.eq_ignore_ascii_case(known) && name != *known)
    {
        return Err(error(format!(
            "at-rule names must be lowercase in {}: Tailwind ignores @{name}",
            file.display()
        )));
    }
    Ok(())
}

fn source_path<'a>(argument: &'a str, file: &Path) -> Result<&'a str, BuildError> {
    let path = quoted_only(argument).ok_or_else(|| {
        error(format!(
            "unsupported @source {argument:?} in {}: use a quoted positive directory",
            file.display()
        ))
    })?;
    if !relative(path) || path.contains(['*', '?', '[', ']']) || path.starts_with('!') {
        return Err(error(format!(
            "unsupported @source {argument:?} in {}: directory must start with ./ or ../ and contain no globs",
            file.display()
        )));
    }
    Ok(path)
}

fn reject_near_miss_root_import(argument: &str, file: &Path) -> Result<(), BuildError> {
    if argument.starts_with("\"tailwindcss\"") || argument.starts_with("'tailwindcss'") {
        return Err(error(format!(
            "unsupported @import {argument:?} in {}: root import must be exactly @import \"tailwindcss\" source(none);",
            file.display()
        )));
    }
    Ok(())
}

fn skip_string(bytes: &[u8], mut pos: usize, file: &Path) -> Result<usize, BuildError> {
    let quote = bytes[pos];
    pos += 1;
    while pos < bytes.len() {
        if bytes[pos] == b'\\' {
            pos += 2;
        } else if bytes[pos] == quote {
            return Ok(pos + 1);
        } else {
            pos += 1;
        }
    }
    Err(error(format!(
        "unterminated CSS string in {}",
        file.display()
    )))
}

fn quoted_only(text: &str) -> Option<&str> {
    let first = text.as_bytes().first()?;
    if !matches!(first, b'\'' | b'"') || text.as_bytes().last()? != first || text.len() < 3 {
        return None;
    }
    let path = &text[1..text.len() - 1];
    if path.contains(['\'', '"', '\\', '\n', '\r']) {
        return None;
    }
    Some(path)
}

fn relative(path: &str) -> bool {
    path.starts_with("./") || path.starts_with("../")
}
fn relative_css(path: &str) -> bool {
    relative(path) && Path::new(path).extension().is_some_and(|ext| ext == "css")
}

fn confined(path: &Path, root: &Path, directory: bool) -> Result<PathBuf, BuildError> {
    // Inspect every lexical component before canonicalizing, so a symlink that
    // ultimately resolves back inside the app is still rejected.
    let mut current = PathBuf::new();
    for part in path.components() {
        match part {
            Component::Prefix(_) | Component::RootDir => current.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                current.pop();
            }
            Component::Normal(name) => {
                current.push(name);
                if fs::symlink_metadata(&current).is_ok_and(|meta| meta.file_type().is_symlink()) {
                    return Err(error(format!(
                        "symlink in CSS path {} (manifest directory limit {})",
                        path.display(),
                        root.display()
                    )));
                }
            }
        }
    }
    let canonical = current.canonicalize().map_err(|e| {
        error(format!(
            "CSS path {}: {e} (manifest directory limit {})",
            path.display(),
            root.display()
        ))
    })?;
    if !canonical.starts_with(root) {
        return Err(error(format!(
            "CSS path {} escapes manifest directory limit {}",
            path.display(),
            root.display()
        )));
    }
    if canonical.strip_prefix(root).is_ok_and(|relative| {
        relative
            .components()
            .any(|part| part.as_os_str() == "target")
    }) {
        return Err(error(format!(
            "refusing Tailwind path inside target/ (reserved for Cargo output): {}",
            canonical.display()
        )));
    }
    if (directory && !canonical.is_dir()) || (!directory && !canonical.is_file()) {
        return Err(error(format!(
            "CSS path {} must be a {} under manifest directory {}",
            canonical.display(),
            if directory {
                "directory"
            } else {
                "regular file"
            },
            root.display()
        )));
    }
    Ok(canonical)
}

pub(super) fn validate_css(
    input: &Path,
    manifest_dir: &Path,
    out_dir: &Path,
) -> Result<PathBuf, BuildError> {
    validate_css_with_watches(input, manifest_dir, out_dir).map(|(input, _)| input)
}

fn validate_css_with_watches(
    input: &Path,
    manifest_dir: &Path,
    out_dir: &Path,
) -> Result<(PathBuf, Vec<PathBuf>), BuildError> {
    let root = manifest_dir.canonicalize().map_err(|e| {
        error(format!(
            "manifest directory {}: {e}",
            manifest_dir.display()
        ))
    })?;
    // CARGO_MANIFEST_DIR can itself contain a symlinked ancestor (macOS /var,
    // for example). Check symlinks only in the path owned by this app.
    let relative = input.strip_prefix(manifest_dir).map_err(|_| {
        error(format!(
            "CSS input {} is outside manifest directory limit {}",
            input.display(),
            manifest_dir.display()
        ))
    })?;
    let input = confined(&root.join(relative), &root, false)?;
    let parent = input
        .parent()
        .ok_or_else(|| error(format!("CSS input {} has no parent", input.display())))?;
    for name in [
        "tailwind.config.js",
        "tailwind.config.ts",
        "tailwind.config.cjs",
        "tailwind.config.mjs",
    ] {
        if parent.join(name).exists() {
            return Err(error(format!(
                "{} is unsupported: Tailwind v4 uses CSS-first configuration",
                parent.join(name).display()
            )));
        }
    }
    let mut state = VisitState::default();
    visit(&input, &root, out_dir, true, &mut state)?;
    Ok((input, state.watches))
}

fn visit(
    file: &Path,
    root: &Path,
    out_dir: &Path,
    is_root: bool,
    state: &mut VisitState,
) -> Result<(), BuildError> {
    if !state.seen.insert(file.to_path_buf()) {
        return Err(error(format!(
            "CSS import cycle or duplicate: {} -> {}",
            state
                .chain
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(" -> "),
            file.display()
        )));
    }
    state.chain.push(file.to_path_buf());
    watch_dir(file, out_dir)?;
    state.watches.push(file.to_path_buf());
    let css =
        fs::read_to_string(file).map_err(|e| error(format!("read CSS {}: {e}", file.display())))?;
    let rules = directives(&css, file)?;
    let parent = file
        .parent()
        .ok_or_else(|| error(format!("CSS file {} has no parent", file.display())))?;
    if is_root {
        if rules.tailwind_imports != 1 || rules.sources.is_empty() {
            return Err(error(format!(
                "{} requires exactly one @import \"tailwindcss\" source(none); and at least one @source directory",
                file.display()
            )));
        }
        for source in rules.sources {
            let directory = confined(&parent.join(source), root, true)?;
            if !state.sources.insert(directory.clone()) {
                return Err(error(format!(
                    "duplicate @source directory {}",
                    directory.display()
                )));
            }
            watch_dir(&directory, out_dir)?;
            state.watches.push(directory.clone());
            reject_source_symlinks(&directory)?;
            watch_ignore_files(&directory, out_dir, &mut state.watches)?;
        }
    } else if rules.tailwind_imports != 0 || !rules.sources.is_empty() {
        return Err(error(format!(
            "imported CSS {} cannot declare @source or import tailwindcss",
            file.display()
        )));
    }
    for import in rules.imports {
        let path = confined(&parent.join(import), root, false)?;
        visit(&path, root, out_dir, false, state)?;
    }
    state.chain.pop();
    Ok(())
}

fn reject_source_symlinks(root: &Path) -> Result<(), BuildError> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        if dir.file_name().is_some_and(|name| name == "node_modules") {
            continue;
        }
        for entry in fs::read_dir(&dir)
            .map_err(|e| error(format!("read Tailwind source {}: {e}", dir.display())))?
        {
            let entry =
                entry.map_err(|e| error(format!("read Tailwind source {}: {e}", dir.display())))?;
            let path = entry.path();
            let kind = entry
                .file_type()
                .map_err(|e| error(format!("inspect Tailwind source {}: {e}", path.display())))?;
            if kind.is_symlink() {
                if fs::metadata(&path).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) {
                    continue;
                }
                return Err(error(format!(
                    "symlink in Tailwind @source directory: {}",
                    path.display()
                )));
            }
            if kind.is_dir() {
                pending.push(path);
            }
        }
    }
    Ok(())
}

fn watch_ignore_files(
    root: &Path,
    out_dir: &Path,
    watches: &mut Vec<PathBuf>,
) -> Result<(), BuildError> {
    for path in existing_ignore_files(root)? {
        watch_dir(&path, out_dir)?;
        watches.push(path);
    }
    Ok(())
}

fn existing_ignore_files(root: &Path) -> Result<Vec<PathBuf>, BuildError> {
    let mut files = Vec::new();
    for ancestor in root.ancestors() {
        for relative in [".gitignore", ".ignore", ".git/info/exclude"] {
            let path = ancestor.join(relative);
            if path.is_file() {
                files.push(path);
            }
        }
        let dot_git = ancestor.join(".git");
        if dot_git.is_file() {
            let content = fs::read_to_string(&dot_git)
                .map_err(|e| error(format!("read {}: {e}", dot_git.display())))?;
            let gitdir = content
                .lines()
                .find_map(|line| line.strip_prefix("gitdir: "))
                .ok_or_else(|| error(format!("missing gitdir: line in {}", dot_git.display())))?;
            let gitdir = ancestor
                .join(gitdir)
                .canonicalize()
                .map_err(|e| error(format!("resolve gitdir in {}: {e}", dot_git.display())))?;
            let common = gitdir.join("commondir");
            let base = if common.is_file() {
                let relative = fs::read_to_string(&common)
                    .map_err(|e| error(format!("read {}: {e}", common.display())))?;
                gitdir
                    .join(relative.trim())
                    .canonicalize()
                    .map_err(|e| error(format!("resolve {}: {e}", common.display())))?
            } else {
                gitdir
            };
            let exclude = base.join("info/exclude");
            if exclude.is_file() {
                files.push(exclude);
            }
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("frontend/nested")).unwrap();
        fs::create_dir(root.path().join("src")).unwrap();
        fs::create_dir(root.path().join("out")).unwrap();
        root
    }

    fn check(root: &Path, css: &str) -> Result<(), BuildError> {
        let input = root.join("frontend/app.css");
        fs::write(&input, css).unwrap();
        validate_css(&input, root, &root.join("out")).map(|_| ())
    }

    const ROOT: &str = "@import \"tailwindcss\" source(none); @source \"../src\";";

    #[test]
    fn comments_strings_and_nested_imports() {
        let root = fixture();
        fs::write(
            root.path().join("frontend/nested/last.css"),
            ".last { color: red; }",
        )
        .unwrap();
        fs::write(
            root.path().join("frontend/extra.css"),
            "@import \"./nested/last.css\"; .extra { content: '@source \\\"../bad\\\";'; }",
        )
        .unwrap();
        check(root.path(), &format!("/* @config './bad'; @source '../bad'; */ {ROOT} @import \"./extra.css\"; .x {{ content: '@plugin foo;'; }}")).unwrap();
    }

    #[test]
    fn source_rules_are_explicit_and_confined() {
        let root = fixture();
        for (css, expected) in [
            ("@source \"../src\";", "exactly one"),
            ("@import \"tailwindcss\" source(none);", "at least one"),
            (
                "@import \"tailwindcss\" source(none); @import \"tailwindcss\" source(none); @source \"../src\";",
                "exactly one",
            ),
            (
                "@import \"tailwindcss\" source(none); @source \"../src\"; @source \"../src\";",
                "duplicate",
            ),
            (
                "@import \"tailwindcss\" source(none); @source not inline;",
                "unsupported @source",
            ),
            (
                "@import \"tailwindcss\" source(none); @source \"../src/*.rs\";",
                "contain no globs",
            ),
            (
                "@import \"tailwindcss\" source(none); @source \"src\";",
                "start with ./ or ../",
            ),
            (
                "@import \"tailwindcss\" source(none); @source \"../../outside\";",
                "manifest directory",
            ),
            (
                "@import \"tailwindcss\" source(none); @source \"../frontend/app.css\";",
                "directory",
            ),
            ("@config \"./tailwind.config.js\";", "CSS-first"),
            ("@plugin \"typography\";", "CSS-first"),
            ("@reference \"./a.css\";", "CSS-first"),
            ("@tailwind utilities;", "CSS-first"),
        ] {
            let error = check(root.path(), css).unwrap_err().to_string();
            assert!(error.contains(expected), "{css}: {error}");
        }
    }

    #[test]
    fn near_miss_tailwind_import_names_required_root_form() {
        let root = fixture();
        for rule in [
            "@import \"tailwindcss\"  source(none);",
            "@import \"tailwindcss\" source(none) prefix(tw);",
        ] {
            let error = check(root.path(), &format!("{rule} @source \"../src\";"))
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("root import must be exactly @import \"tailwindcss\" source(none);"),
                "{error}"
            );
        }
    }

    #[test]
    fn import_grammar_and_graph_rejections() {
        let root = fixture();
        fs::write(root.path().join("frontend/other.css"), ".other{}").unwrap();
        for (rule, expected) in [
            ("@import \"package\";", "unsupported @import"),
            ("@import url(./other.css);", "unsupported @import"),
            ("@import \"./other.css\" layer(foo);", "unsupported @import"),
            ("@import \"./other.txt\";", "unsupported @import"),
            ("@import \"../../outside.css\";", "manifest directory"),
            ("@import \"./missing.css\";", "CSS path"),
            ("@import \"./app.css\";", "cycle or duplicate"),
            (
                "@import \"./other.css\"; @import \"./other.css\";",
                "cycle or duplicate",
            ),
        ] {
            let error = check(root.path(), &format!("{ROOT} {rule}"))
                .unwrap_err()
                .to_string();
            assert!(error.contains(expected), "{rule}: {error}");
        }
        for imported in [
            "@source \"../src\";",
            "@import \"tailwindcss\" source(none);",
        ] {
            fs::write(root.path().join("frontend/other.css"), imported).unwrap();
            let error = check(root.path(), &format!("{ROOT} @import \"./other.css\";"))
                .unwrap_err()
                .to_string();
            assert!(error.contains("imported CSS"), "{error}");
        }
    }

    #[test]
    fn out_and_target_roots_are_rejected() {
        let root = fixture();
        fs::create_dir(root.path().join("target")).unwrap();
        let error = check(
            root.path(),
            "@import \"tailwindcss\" source(none); @source \"../\";",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("OUT_DIR"), "{error}");
        let separate_out = tempfile::tempdir().unwrap();
        let error = validate_css(
            &root.path().join("frontend/app.css"),
            root.path(),
            separate_out.path(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("target/"), "{error}");
        let error = check(
            root.path(),
            "@import \"tailwindcss\" source(none); @source \"../target\";",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("inside target/"), "{error}");
        let error = check(
            root.path(),
            "@import \"tailwindcss\" source(none); @source \"../out\";",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("OUT_DIR"), "{error}");
    }

    #[test]
    fn mis_cased_tailwind_rules_fail() {
        let root = fixture();
        for rule in [
            "@SOURCE \"../src\";",
            "@Import \"./x.css\";",
            "@Plugin \"x\";",
        ] {
            let message = check(root.path(), &format!("{ROOT} {rule}"))
                .unwrap_err()
                .to_string();
            assert!(message.contains("must be lowercase"), "{message}");
        }
    }

    #[test]
    fn escaped_selectors_cannot_hide_rules() {
        let outer = tempfile::tempdir().unwrap();
        let app = outer.path().join("app");
        fs::create_dir_all(app.join("frontend")).unwrap();
        fs::create_dir(app.join("src")).unwrap();
        fs::create_dir(app.join("out")).unwrap();
        let root = app.as_path();
        let secret = outer.path().join("secret");
        fs::create_dir_all(&secret).unwrap();
        let message = check(
            root,
            &format!("{ROOT} .a\\\"{{}} @source \"../../secret\"; /* \" */"),
        )
        .unwrap_err()
        .to_string();
        assert!(
            message.contains("escapes manifest directory limit"),
            "{message}"
        );

        fs::write(root.join("frontend/hidden.css"), ".hidden{}").unwrap();
        check(
            root,
            &format!("{ROOT} .a\\'{{}} @import \"./hidden.css\"; /* ' */"),
        )
        .unwrap();
        fs::remove_file(root.join("frontend/hidden.css")).unwrap();
        let message = check(
            root,
            &format!("{ROOT} .a\\'{{}} @import \"./hidden.css\"; /* ' */"),
        )
        .unwrap_err()
        .to_string();
        assert!(message.contains("hidden.css"), "{message}");
        check(root, &format!("{ROOT} .md\\:x{{}} .a\\\"{{}}")).unwrap();
    }

    #[test]
    fn css_files_cannot_live_under_target() {
        let root = fixture();
        fs::create_dir_all(root.path().join("target")).unwrap();
        fs::write(root.path().join("target/app.css"), ROOT).unwrap();
        let message = validate_css(
            &root.path().join("target/app.css"),
            root.path(),
            &root.path().join("out"),
        )
        .unwrap_err()
        .to_string();
        assert!(
            message.contains("refusing Tailwind path inside target/ (reserved for Cargo output):"),
            "{message}"
        );
        fs::write(root.path().join("target/gen.css"), ".x{}").unwrap();
        let message = check(
            root.path(),
            &format!("{ROOT} @import \"../target/gen.css\";"),
        )
        .unwrap_err()
        .to_string();
        assert!(
            message.contains("refusing Tailwind path inside target/ (reserved for Cargo output):"),
            "{message}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn source_tree_rejects_symlinked_files_and_directories() {
        use std::os::unix::fs::symlink;
        let root = fixture();
        for (name, target) in [("file.rs", "/etc/hosts"), ("linked", "/tmp")] {
            let link = root.path().join("src").join(name);
            symlink(target, &link).unwrap();
            let message = check(root.path(), ROOT).unwrap_err().to_string();
            assert!(message.contains(&link.display().to_string()), "{message}");
            fs::remove_file(link).unwrap();
        }
        symlink("missing.rs", root.path().join("src/.#a.rs")).unwrap();
        fs::create_dir(root.path().join("src/node_modules")).unwrap();
        symlink("/etc/hosts", root.path().join("src/node_modules/tool")).unwrap();
        check(root.path(), ROOT).unwrap();
    }

    #[test]
    fn watches_existing_ancestor_ignore_files() {
        let root = fixture();
        let gitdir = root.path().join("repo/.git/worktrees/wt");
        fs::create_dir_all(&gitdir).unwrap();
        fs::create_dir_all(root.path().join("repo/.git/info")).unwrap();
        fs::write(gitdir.join("commondir"), "../..").unwrap();
        let exclude = root.path().join("repo/.git/info/exclude");
        fs::write(&exclude, "g.rs\n").unwrap();
        let wt = root.path().join("wt");
        fs::create_dir_all(wt.join("frontend")).unwrap();
        fs::create_dir(wt.join("src")).unwrap();
        fs::create_dir(wt.join("out")).unwrap();
        fs::write(wt.join(".git"), "gitdir: ../repo/.git/worktrees/wt\n").unwrap();
        fs::write(wt.join(".gitignore"), "ignored.rs\n").unwrap();
        fs::write(wt.join("frontend/extra.css"), ".extra{}").unwrap();
        let input = wt.join("frontend/app.css");
        fs::write(&input, format!("{ROOT} @import \"./extra.css\";")).unwrap();
        let (_, emitted) = validate_css_with_watches(&input, &wt, &wt.join("out")).unwrap();
        for expected in [
            &input,
            &wt.join("frontend/extra.css"),
            &wt.join("src"),
            &wt.join(".gitignore"),
            &exclude,
        ] {
            assert!(
                emitted.contains(expected),
                "missing {} from {emitted:?}",
                expected.display()
            );
        }
        assert!(
            existing_ignore_files(&wt.join("src"))
                .unwrap()
                .contains(&exclude)
        );
    }

    #[test]
    fn malformed_worktree_gitdir_is_loud() {
        let root = fixture();
        fs::write(root.path().join(".git"), "broken\n").unwrap();
        let message = check(root.path(), ROOT).unwrap_err().to_string();
        assert!(
            message.contains("missing gitdir: line") && message.contains(".git"),
            "{message}"
        );
    }

    #[test]
    fn escaped_comment_close_cannot_hide_rules() {
        let outer = tempfile::tempdir().unwrap();
        let app = outer.path().join("app");
        fs::create_dir_all(app.join("frontend")).unwrap();
        fs::create_dir(app.join("src")).unwrap();
        fs::create_dir(app.join("out")).unwrap();
        fs::create_dir(outer.path().join("secret")).unwrap();
        for quote in ['"', '\''] {
            let css = format!(
                "{ROOT} /* \\*/ {quote} */ @source {quote}../../secret{quote}; /* {quote} */"
            );
            let message = check(&app, &css).unwrap_err().to_string();
            assert!(
                message.contains("escapes manifest directory limit"),
                "{message}"
            );
        }
        let hidden = app.join("frontend/hidden.css");
        let css = format!("{ROOT} /* \\*/ \" */ @import \"./hidden.css\"; /* \" */");
        fs::write(&hidden, ".hidden{}").unwrap();
        check(&app, &css).unwrap();
        fs::remove_file(hidden).unwrap();
        assert!(
            check(&app, &css)
                .unwrap_err()
                .to_string()
                .contains("hidden.css")
        );
        let css = format!("{ROOT} /* \\*/ \" */ @plugin \"x\"; /* \" */");
        assert!(
            check(&app, &css)
                .unwrap_err()
                .to_string()
                .contains("@plugin is unsupported")
        );
        check(&app, &format!("{ROOT} /* C:\\dir */ .x{{}} ")).unwrap();
    }
}
