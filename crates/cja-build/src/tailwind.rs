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

// This allow-list grammar is a strict subset of Tailwind CSS, not a mirror of
// its parser. Only import and source may add input-graph edges; all other
// allowed rules must be file-free and cannot take source(...).
// at-rule        consumer or reason allowed
// import         cja.app/eyes; confined CSS or exact Tailwind root import
// source         cja.app/eyes; explicit top-level directories
// media          cja.app/eyes; no file/path input
// supports       no file/path input
// layer          no file/path input
// keyframes      cja.app; no file/path input
// font-face      no file/path input
// theme          no file/path input
// utility        no file/path input
// variant        no file/path input
// custom-variant no file/path input
// apply          no file/path input
// container      no file/path input
// property       no file/path input
// starting-style no file/path input
// page           no file/path input
// charset        no file/path input
const ALLOWED_RULES: &[&str] = &[
    "import",
    "source",
    "media",
    "supports",
    "layer",
    "keyframes",
    "font-face",
    "theme",
    "utility",
    "variant",
    "custom-variant",
    "apply",
    "container",
    "property",
    "starting-style",
    "page",
    "charset",
];

const EXCLUDED_RULES: &[&str] = &["plugin", "config", "reference", "tailwind"];

fn ascii_trim(text: &str) -> &str {
    text.trim_matches([' ', '\t', '\n', '\r', '\u{000c}'])
}

fn non_ascii(css: &str, pos: usize, file: &Path) -> BuildError {
    let token = css
        .get(pos..)
        .and_then(|tail| tail.chars().next())
        .map_or_else(
            || format!("0x{:02X}", css.as_bytes()[pos]),
            |ch| format!("U+{:04X}", u32::from(ch)),
        );
    error(format!(
        "non-ASCII {token} outside CSS string/comment in {}",
        file.display()
    ))
}

fn skip_escape(css: &str, pos: usize, file: &Path) -> Result<usize, BuildError> {
    let bytes = css.as_bytes();
    if bytes.get(pos + 1).is_some_and(|byte| !byte.is_ascii()) {
        return Err(non_ascii(css, pos + 1, file));
    }
    Ok((pos + 2).min(bytes.len()))
}

// Tailwind elides comments between tokens, including inside source/**/(.
fn source_token(bytes: &[u8], pos: usize, file: &Path) -> Result<bool, BuildError> {
    if !bytes[pos].eq_ignore_ascii_case(&b's') {
        return Ok(false);
    }
    let mut cursor = pos;
    for letter in b"source(" {
        while bytes[cursor..].starts_with(b"/*") {
            cursor = skip_comment(bytes, cursor, file)?;
        }
        if !bytes
            .get(cursor)
            .is_some_and(|byte| byte.eq_ignore_ascii_case(letter))
        {
            return Ok(false);
        }
        cursor += 1;
    }
    Ok(true)
}

fn reject_source_token(bytes: &[u8], pos: usize, file: &Path) -> Result<(), BuildError> {
    if source_token(bytes, pos, file)? {
        return Err(error(format!(
            "unsupported source( in {}: only the root Tailwind import may use source(none)",
            file.display()
        )));
    }
    Ok(())
}

fn reject_source_in_statement(
    bytes: &[u8],
    mut pos: usize,
    end: usize,
    file: &Path,
) -> Result<(), BuildError> {
    while pos < end {
        if bytes[pos..].starts_with(b"/*") {
            pos = skip_comment(bytes, pos, file)?;
        } else if matches!(bytes[pos], b'\'' | b'"') {
            pos = skip_string(bytes, pos, file)?;
        } else if bytes[pos] == b'\\' {
            pos = (pos + 2).min(end);
        } else {
            reject_source_token(bytes, pos, file)?;
            pos += 1;
        }
    }
    Ok(())
}

fn directives(css: &str, file: &Path) -> Result<Directives, BuildError> {
    let bytes = css.as_bytes();
    let mut pos = if bytes.starts_with(b"\xef\xbb\xbf") {
        3
    } else {
        0
    };
    let mut depth = 0usize;
    let mut paren_depth = 0usize;
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
            pos = skip_escape(css, pos, file)?;
            continue;
        }
        if !bytes[pos].is_ascii() {
            return Err(non_ascii(css, pos, file));
        }
        reject_source_token(bytes, pos, file)?;
        if bytes[pos] == b'(' {
            paren_depth += 1;
            pos += 1;
            continue;
        }
        if bytes[pos] == b')' {
            if paren_depth == 0 {
                return Err(error(format!("unmatched ) in {}", file.display())));
            }
            paren_depth -= 1;
            pos += 1;
            continue;
        }
        // Tailwind does not treat these as structure inside parentheses.
        // Reject them so our brace depth cannot diverge from Tailwind's.
        if paren_depth != 0 && matches!(bytes[pos], b'{' | b'}' | b';') {
            return Err(error(format!(
                "unsupported {} inside parentheses in {}",
                char::from(bytes[pos]),
                file.display()
            )));
        }
        if bytes[pos] == b'{' {
            depth += 1;
            pos += 1;
            continue;
        }
        if bytes[pos] == b'}' {
            if depth == 0 {
                return Err(error(format!("unmatched }} in {}", file.display())));
            }
            depth -= 1;
            pos += 1;
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
        if bytes[end..].starts_with(b"/*") {
            return Err(error(format!(
                "comment inside or directly after an at-rule name is unsupported in {}: @{}/*",
                file.display(),
                &css[start..end],
            )));
        }
        if bytes.get(start).is_some_and(|byte| !byte.is_ascii()) {
            return Err(non_ascii(css, start, file));
        }
        let name = &css[start..end];
        check_rule_case(name, file)?;
        if name.is_empty()
            || bytes
                .get(end)
                .is_some_and(|byte| byte.is_ascii_digit() || *byte == b'_')
        {
            return Err(error(format!(
                "malformed at-rule @{} in {}: expected ASCII letters or hyphens",
                css.get(start..=end).unwrap_or(name),
                file.display()
            )));
        }
        if !ALLOWED_RULES.contains(&name) {
            return Err(error(format!(
                "unsupported @{name} in {}: at-rule is not in cja-build's Tailwind allow-list",
                file.display()
            )));
        }
        if !matches!(name, "import" | "source") {
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
                cursor = skip_escape(css, cursor, file)?;
            } else if !bytes[cursor].is_ascii() {
                return Err(non_ascii(css, cursor, file));
            } else if matches!(bytes[cursor], b'{' | b'}') {
                return Err(error(format!(
                    "unsupported {} in @{name} in {}: expected semicolon before brace",
                    char::from(bytes[cursor]),
                    file.display()
                )));
            } else {
                cursor += 1;
            }
        }
        if cursor == bytes.len() {
            return Err(error(format!("unterminated @{name} in {}", file.display())));
        }
        let argument = ascii_trim(&css[end..cursor]);
        match name {
            "import" => {
                if argument == "\"tailwindcss\" source(none)"
                    || argument == "'tailwindcss' source(none)"
                {
                    if depth != 0 {
                        return Err(error(format!(
                            "@import \"tailwindcss\" source(none) must be at depth 0 in {}",
                            file.display()
                        )));
                    }
                    result.tailwind_imports += 1;
                } else {
                    reject_near_miss_root_import(argument, file)?;
                    reject_source_in_statement(bytes, end, cursor, file)?;
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
            "source" => {
                reject_source_in_statement(bytes, end, cursor, file)?;
                if depth != 0 {
                    return Err(error(format!(
                        "@source must be at depth 0 in {}",
                        file.display()
                    )));
                }
                result.sources.push(source_path(argument, file)?.to_owned());
            }
            _ => {}
        }
        pos = cursor + 1;
    }
    if depth != 0 {
        return Err(error(format!("unmatched {{ in {}", file.display())));
    }
    if paren_depth != 0 {
        return Err(error(format!("unmatched ( in {}", file.display())));
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
    if ALLOWED_RULES
        .iter()
        .chain(EXCLUDED_RULES.iter())
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
    if argument.contains(['*', '?', '[', ']', '{', '}', ',', '!', '(', ')', '\\']) {
        return Err(error(format!(
            "unsupported @source {argument} in {}: directory must start with ./ or ../ and contain no globs",
            file.display()
        )));
    }
    let path = quoted_only(argument).ok_or_else(|| {
        error(format!(
            "unsupported @source {argument} in {}: use a quoted positive directory",
            file.display()
        ))
    })?;
    if !relative(path) {
        return Err(error(format!(
            "unsupported @source {argument} in {}: directory must start with ./ or ../ and contain no globs",
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
    let root_in_node_modules = root
        .components()
        .any(|component| component.as_os_str() == "node_modules");
    while let Some(dir) = pending.pop() {
        if !root_in_node_modules
            && dir != root
            && dir.file_name().is_some_and(|name| name == "node_modules")
        {
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
            let gitdir = match ancestor.join(gitdir).canonicalize() {
                Ok(path) => path,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(error(format!(
                        "resolve gitdir in {}: {e}",
                        dot_git.display()
                    )));
                }
            };
            let common = gitdir.join("commondir");
            let base = if common.is_file() {
                let relative = fs::read_to_string(&common)
                    .map_err(|e| error(format!("read {}: {e}", common.display())))?;
                match gitdir.join(relative.trim()).canonicalize() {
                    Ok(path) => path,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(error(format!("resolve {}: {e}", common.display()))),
                }
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
    use sha2::{Digest, Sha256};

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

    fn rejected(root: &Path, css: &str, expected: &str, token: &str) {
        let message = check(root, css).unwrap_err().to_string();
        assert!(message.contains(expected), "{css}: {message}");
        assert!(message.contains(token), "{css}: {message}");
        assert!(message.contains("app.css"), "{css}: {message}");
    }

    #[test]
    fn nested_root_import_rejected() {
        let root = fixture();
        rejected(
            root.path(),
            "@media screen { @import \"tailwindcss\" source(none); } @source \"../src\";",
            "must be at depth 0",
            "@import \"tailwindcss\" source(none)",
        );
    }

    #[test]
    fn source_function_is_rejected_everywhere_outside_strings_and_comments() {
        let root = fixture();
        rejected(
            root.path(),
            &format!("{ROOT} .x {{ color: source(../../secret); }}"),
            "only the root Tailwind import may use source(none)",
            "source(",
        );
        rejected(
            root.path(),
            "@media source(\"../../secret\") { @import \"tailwindcss\" source(none); } @source \"../src\";",
            "only the root Tailwind import may use source(none)",
            "source(",
        );
        rejected(
            root.path(),
            &format!("{ROOT} @source source(../src);"),
            "only the root Tailwind import may use source(none)",
            "source(",
        );
    }

    #[test]
    fn plain_source_function_in_media_is_rejected() {
        let root = fixture();
        rejected(
            root.path(),
            &format!("{ROOT} @media source(\"../../secret\") {{ .x{{}} }}"),
            "only the root Tailwind import may use source(none)",
            "source(",
        );
    }

    #[test]
    fn comment_split_source_function_in_media_is_rejected() {
        let root = fixture();
        rejected(
            root.path(),
            &format!("{ROOT} @media source/**/(\"../../secret\") {{ .x{{}} }}"),
            "only the root Tailwind import may use source(none)",
            "source(",
        );
    }

    #[test]
    fn comment_split_source_name_in_media_is_rejected() {
        let root = fixture();
        rejected(
            root.path(),
            &format!("{ROOT} @media sou/**/rce(\"../../secret\") {{ .x{{}} }}"),
            "only the root Tailwind import may use source(none)",
            "source(",
        );
    }

    #[test]
    fn unknown_at_rule_is_rejected() {
        let root = fixture();
        rejected(
            root.path(),
            &format!("{ROOT} @mystery x;"),
            "Tailwind allow-list",
            "@mystery",
        );
    }

    #[test]
    fn unicode_outside_strings_comments_is_rejected_before_import_resolution() {
        let outer = tempfile::tempdir().unwrap();
        let app = outer.path().join("app");
        fs::create_dir_all(app.join("frontend")).unwrap();
        fs::create_dir(app.join("src")).unwrap();
        fs::create_dir(app.join("out")).unwrap();
        fs::write(app.join("frontend/a.css"), ".safe{}").unwrap();
        fs::write(
            app.join("frontend/a.css\""),
            "@import \"../../secret.css\";",
        )
        .unwrap();
        fs::write(outer.path().join("secret.css"), ".cja-outside-marker{}").unwrap();
        rejected(
            &app,
            &format!("{ROOT} @import \"./a.css\"\u{0085};"),
            "non-ASCII U+0085",
            "U+0085",
        );
        rejected(
            app.as_path(),
            &format!("{ROOT} @media (width > 1px)\u{2212} {{}}"),
            "non-ASCII U+2212",
            "U+2212",
        );
        rejected(
            app.as_path(),
            &format!("{ROOT} @é x;"),
            "non-ASCII U+00E9",
            "U+00E9",
        );
        rejected(
            app.as_path(),
            &format!("{ROOT} \u{feff}.x{{}}"),
            "non-ASCII U+FEFF",
            "U+FEFF",
        );
    }

    #[test]
    fn source_glob_metacharacters_are_rejected_before_confinement() {
        let root = fixture();
        for ch in ['*', '?', '[', ']', '{', '}', ',', '!', '(', ')', '\\'] {
            let argument = format!("\"../sr{ch}c\"");
            rejected(
                root.path(),
                &format!("@import \"tailwindcss\" source(none); @source {argument};"),
                "contain no globs",
                &argument,
            );
        }
    }

    #[test]
    fn literal_brace_source_is_rejected_even_when_directory_exists() {
        let root = fixture();
        let odd = root.path().join("{src,..").join("secret}");
        fs::create_dir_all(&odd).unwrap();
        assert!(odd.canonicalize().unwrap().starts_with(root.path()));
        rejected(
            root.path(),
            "@import \"tailwindcss\" source(none); @source \"../{src,../secret}\";",
            "contain no globs",
            "../{src,../secret}",
        );
    }

    #[test]
    fn unknown_and_malformed_at_rules_are_rejected_at_any_depth() {
        let root = fixture();
        for rule in ["@mystery x;", "@media screen { @mystery x; }"] {
            rejected(
                root.path(),
                &format!("{ROOT} {rule}"),
                "Tailwind allow-list",
                "@mystery",
            );
        }
        for name in ["plugin", "config", "reference", "tailwind"] {
            rejected(
                root.path(),
                &format!("{ROOT} @{name} \"./x.css\";"),
                "Tailwind allow-list",
                &format!("@{name}"),
            );
        }
        rejected(
            root.path(),
            &format!("{ROOT} @1bad x;"),
            "malformed at-rule",
            "@1",
        );
        rejected(
            root.path(),
            &format!("{ROOT} @media_foo x;"),
            "malformed at-rule",
            "@media_",
        );
    }

    #[test]
    fn braces_and_nested_source_are_rejected() {
        let root = fixture();
        rejected(
            root.path(),
            &format!("{ROOT} @media screen {{ @source \"../src\"; }}"),
            "@source must be at depth 0",
            "@source",
        );
        rejected(root.path(), &format!("{ROOT} }}"), "unmatched }", "}");
        rejected(root.path(), &format!("{ROOT} {{"), "unmatched {", "{");
        rejected(
            root.path(),
            "@import \"tailwindcss\" source(none) { @source \"../src\"; }",
            "semicolon before brace",
            "{",
        );
    }

    // Tailwind 4.3.3's css-parser ignores `{` and `}` while a `(` is open, so
    // here it nests the root import inside `.p { ... }` (it still honours
    // source(none) there). The task requires the root import at depth 0.
    #[test]
    fn root_import_nested_via_braces_inside_parens_is_rejected() {
        let root = fixture();
        let css =
            ".p { a: (}); @import \"tailwindcss\" source(none); a: ({); } @source \"../src\";";
        let message = check(root.path(), css)
            .expect_err("Tailwind nests this root import; validation must reject it")
            .to_string();
        assert!(message.contains("app.css"), "{css}: {message}");
    }

    #[test]
    fn structural_tokens_inside_parentheses_are_rejected() {
        let root = fixture();
        for token in ['{', '}', ';'] {
            rejected(
                root.path(),
                &format!("{ROOT} .x {{ a: (foo{token}bar); }}"),
                "inside parentheses",
                &token.to_string(),
            );
        }
        rejected(
            root.path(),
            &format!("{ROOT} .x {{ a: (foo; }}"),
            "inside parentheses",
            ";",
        );
        rejected(
            root.path(),
            &format!("{ROOT} .x {{ a: foo); }}"),
            "unmatched )",
            ")",
        );
        rejected(
            root.path(),
            &format!("{ROOT} .x {{ a: (foo }}"),
            "inside parentheses",
            "}",
        );
        rejected(root.path(), &format!("{ROOT} a: (foo"), "unmatched (", "(");
    }

    #[test]
    fn allowed_at_rules_and_unicode_literals_pass() {
        let root = fixture();
        fs::create_dir_all(root.path().join("node_modules/@scope/pkg")).unwrap();
        let css = format!(
            "\u{feff}{ROOT} @source \"../node_modules/@scope/pkg\"; /* — · source( */ .x {{ content: \"− source(\"; }} @media screen {{}} @supports (display: grid) {{}} @layer components {{}} @keyframes fade {{ from {{ opacity: 0; }} }} @font-face {{}} @theme {{}} @utility foo {{}} @variant foo {{}} @custom-variant foo (&:hover); @apply foo; @container (width > 1px) {{}} @property --foo {{}} @starting-style {{}} @page {{}} @charset \"UTF-8\";"
        );
        check(root.path(), &css).unwrap();
    }

    #[test]
    fn eyes_css_fixture_accepts_allow_list() {
        let portfolio = include_bytes!("../tests/fixtures/eyes/portfolio.css");
        assert_eq!(portfolio.len(), 16_392);
        assert_eq!(
            format!("{:x}", Sha256::digest(portfolio)),
            "caaf47e1ae2d809e1a7b10c04a2b5d97258d164f211662a9cbc631a538553f18"
        );
        let root = tempfile::tempdir().unwrap();
        let app = root.path().join("app");
        fs::create_dir_all(app.join("assets")).unwrap();
        fs::create_dir(app.join("src")).unwrap();
        fs::create_dir(app.join("out")).unwrap();
        fs::write(
            app.join("assets/input.css"),
            include_bytes!("../tests/fixtures/eyes/input.css"),
        )
        .unwrap();
        fs::write(app.join("assets/portfolio.css"), portfolio).unwrap();
        validate_css(&app.join("assets/input.css"), &app, &app.join("out")).unwrap();
    }

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
            ("@config \"./tailwind.config.js\";", "allow-list"),
            ("@plugin \"typography\";", "allow-list"),
            ("@reference \"./a.css\";", "allow-list"),
            ("@tailwind utilities;", "allow-list"),
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
        let canonical = root.path().canonicalize().unwrap();
        for (name, target) in [("file.rs", "/etc/hosts"), ("linked", "/tmp")] {
            let link = canonical.join("src").join(name);
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
        let canonical = root.path().canonicalize().unwrap();
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
            &canonical.join("wt/frontend/app.css"),
            &canonical.join("wt/frontend/extra.css"),
            &canonical.join("wt/src"),
            &canonical.join("wt/.gitignore"),
            &canonical.join("repo/.git/info/exclude"),
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
                .contains(&canonical.join("repo/.git/info/exclude"))
        );
    }

    #[test]
    fn missing_gitdir_is_ignored() {
        let root = fixture();
        fs::write(root.path().join(".git"), "gitdir: missing\n").unwrap();
        check(root.path(), ROOT).unwrap();
        assert_eq!(
            existing_ignore_files(&root.path().join("src")).unwrap(),
            Vec::<PathBuf>::new()
        );
    }

    #[test]
    fn comments_inside_at_rule_names_fail_closed() {
        let outer = tempfile::tempdir().unwrap();
        let app = outer.path().join("app");
        fs::create_dir_all(app.join("frontend")).unwrap();
        fs::create_dir(app.join("src")).unwrap();
        fs::create_dir(app.join("out")).unwrap();
        fs::create_dir(outer.path().join("secret")).unwrap();
        fs::write(outer.path().join("secret.css"), ".secret{}").unwrap();
        fs::write(app.join("frontend/x.css"), ".x{}").unwrap();
        for rule in [
            "@im/**/port \"../../secret.css\";",
            "@sou/**/rce \"../../secret\";",
            "@/**/import \"./x.css\";",
            "@plu/**/gin \"x\";",
            "@im/*! x */port \"./x.css\";",
            "@media screen { @im/**/port \"../../secret.css\"; }",
            ".x { @im/**/port \"../../secret.css\"; }",
        ] {
            let message = check(&app, &format!("{ROOT} {rule}"))
                .unwrap_err()
                .to_string();
            assert!(
                message.contains("comment inside or directly after an at-rule name"),
                "{rule}: {message}"
            );
            assert!(message.contains("app.css"), "{message}");
        }
        check(&app, &format!("{ROOT} @media screen {{ .x{{}} }}")).unwrap();
        check(
            &app,
            "@import \"tailwindcss\" source(none); /* note */ @source \"../src\";",
        )
        .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn explicit_node_modules_roots_reject_resolving_symlinks() {
        use std::os::unix::fs::symlink;
        let root = fixture();
        let canonical = root.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let modules = canonical.join("node_modules");
        fs::create_dir_all(modules.join("ui-lib/node_modules")).unwrap();
        symlink(outside.path(), modules.join("out")).unwrap();
        symlink(outside.path(), modules.join("ui-lib/node_modules/dep")).unwrap();
        for (source, link) in [
            ("../node_modules", modules.join("out")),
            (
                "../node_modules/ui-lib",
                modules.join("ui-lib/node_modules/dep"),
            ),
        ] {
            let message = check(
                root.path(),
                &format!("@import \"tailwindcss\" source(none); @source \"{source}\";"),
            )
            .unwrap_err()
            .to_string();
            assert!(message.contains(&link.display().to_string()), "{message}");
        }
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
                .contains("unsupported @plugin")
        );
        check(&app, &format!("{ROOT} /* C:\\dir */ .x{{}} ")).unwrap();
    }
}
