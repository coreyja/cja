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

// Walk CSS bytes rather than searching lines: comments and quoted content can
// contain inert @rules, while an actual rule may span several lines.
fn directives(css: &str, file: &Path) -> Result<Directives, BuildError> {
    let bytes = css.as_bytes();
    let mut pos = 0;
    let mut result = Directives::default();
    while pos < bytes.len() {
        if bytes[pos..].starts_with(b"/*") {
            pos = css[pos + 2..]
                .find("*/")
                .map(|end| pos + 2 + end + 2)
                .ok_or_else(|| error(format!("unterminated CSS comment in {}", file.display())))?;
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
                cursor = css[cursor + 2..]
                    .find("*/")
                    .map(|n| cursor + 2 + n + 2)
                    .ok_or_else(|| {
                        error(format!("unterminated CSS comment in {}", file.display()))
                    })?;
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
            "refusing Tailwind CSS path inside target/: {}",
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
    let mut seen = HashSet::new();
    let mut chain = Vec::new();
    let mut sources = HashSet::new();
    visit(
        &input,
        &root,
        out_dir,
        true,
        &mut seen,
        &mut chain,
        &mut sources,
    )?;
    Ok(input)
}

fn visit(
    file: &Path,
    root: &Path,
    out_dir: &Path,
    is_root: bool,
    seen: &mut HashSet<PathBuf>,
    chain: &mut Vec<PathBuf>,
    sources: &mut HashSet<PathBuf>,
) -> Result<(), BuildError> {
    if !seen.insert(file.to_path_buf()) {
        return Err(error(format!(
            "CSS import cycle or duplicate: {} -> {}",
            chain
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(" -> "),
            file.display()
        )));
    }
    chain.push(file.to_path_buf());
    watch_dir(file, out_dir)?;
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
            if !sources.insert(directory.clone()) {
                return Err(error(format!(
                    "duplicate @source directory {}",
                    directory.display()
                )));
            }
            watch_dir(&directory, out_dir)?;
            reject_source_symlinks(&directory)?;
            watch_ignore_files(&directory, out_dir)?;
        }
    } else if rules.tailwind_imports != 0 || !rules.sources.is_empty() {
        return Err(error(format!(
            "imported CSS {} cannot declare @source or import tailwindcss",
            file.display()
        )));
    }
    for import in rules.imports {
        let path = confined(&parent.join(import), root, false)?;
        visit(&path, root, out_dir, false, seen, chain, sources)?;
    }
    chain.pop();
    Ok(())
}

fn reject_source_symlinks(root: &Path) -> Result<(), BuildError> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
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

fn watch_ignore_files(root: &Path, out_dir: &Path) -> Result<(), BuildError> {
    for path in existing_ignore_files(root) {
        watch_dir(&path, out_dir)?;
    }
    Ok(())
}

fn existing_ignore_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for ancestor in root.ancestors() {
        for relative in [".gitignore", ".ignore", ".git/info/exclude"] {
            let path = ancestor.join(relative);
            if path.is_file() {
                files.push(path);
            }
        }
    }
    files
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
        let root = fixture();
        let message = check(
            root.path(),
            &format!("{ROOT} .a\\\"{{}} @source \"../../secret\"; /* \" */"),
        )
        .unwrap_err()
        .to_string();
        assert!(message.contains("manifest directory"), "{message}");

        fs::write(root.path().join("frontend/hidden.css"), ".hidden{}").unwrap();
        check(
            root.path(),
            &format!("{ROOT} .a\\'{{}} @import \"./hidden.css\"; /* ' */"),
        )
        .unwrap();
        fs::remove_file(root.path().join("frontend/hidden.css")).unwrap();
        let message = check(
            root.path(),
            &format!("{ROOT} .a\\'{{}} @import \"./hidden.css\"; /* ' */"),
        )
        .unwrap_err()
        .to_string();
        assert!(message.contains("hidden.css"), "{message}");
        check(root.path(), &format!("{ROOT} .md\\:x{{}} .a\\\"{{}}")).unwrap();
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
        assert!(message.contains("target/"), "{message}");
        fs::write(root.path().join("target/gen.css"), ".x{}").unwrap();
        let message = check(
            root.path(),
            &format!("{ROOT} @import \"../target/gen.css\";"),
        )
        .unwrap_err()
        .to_string();
        assert!(message.contains("target/"), "{message}");
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
    }

    #[test]
    fn watches_existing_ancestor_ignore_files() {
        let root = fixture();
        let ignore = root.path().join(".gitignore");
        fs::write(&ignore, "ignored.rs\n").unwrap();
        let out = root.path().join("out");
        watch_ignore_files(&root.path().join("src"), &out).unwrap();
        assert!(existing_ignore_files(&root.path().join("src")).contains(&ignore));
    }
}
