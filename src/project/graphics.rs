//! Statically declared graphicx search directories, relative to the TeX root.
use crate::{
    parser::syntax,
    project::workspace::{normalize_path, resolve_path, Workspace},
};
use lsp_types::Url;
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    ops::Range,
    path::{Path, PathBuf},
};

pub fn is_image(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            syntax::file_extensions("includegraphics")
                .unwrap()
                .iter()
                .any(|supported| extension.eq_ignore_ascii_case(supported))
        })
}

/// graphicx removes balanced filename quotes, including quoted segments.
/// The source span retains those quotes so edits can replace them together.
pub(crate) fn unquote(value: &str) -> Option<Cow<'_, str>> {
    let quotes = value.bytes().filter(|byte| *byte == b'"').count();
    if quotes % 2 != 0 {
        return None;
    }
    Some(normalize_name(value, false))
}

pub(crate) fn unquote_prefix(value: &str) -> Cow<'_, str> {
    normalize_name(value, true)
}

fn normalize_name(value: &str, prefix: bool) -> Cow<'_, str> {
    if !value.contains('"') && !value.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return Cow::Borrowed(value);
    }
    let mut normalized = String::new();
    let mut space = false;
    for character in value.chars().filter(|character| *character != '"') {
        if matches!(character, ' ' | '\t' | '\r' | '\n') {
            space = !normalized.is_empty();
        } else {
            if space {
                normalized.push(' ');
                space = false;
            }
            normalized.push(character);
        }
    }
    if prefix && space {
        normalized.push(' ');
    }
    if normalized == value {
        Cow::Borrowed(value)
    } else {
        Cow::Owned(normalized)
    }
}

fn has_paragraph(value: &str) -> bool {
    let lines: Vec<_> = value.split('\n').collect();
    lines.len() > 2
        && lines[1..lines.len() - 1]
            .iter()
            .any(|line| line.trim().is_empty())
}

pub(crate) fn argument(source: &str, command: &syntax::Command) -> Option<syntax::Argument> {
    let start = syntax::file_name_argument_start(source, "includegraphics", command.span.end)?;
    let (mut argument, _) = syntax::group(source, start, b'{', b'}')?;
    let raw = &source[argument.span.clone()];
    if has_paragraph(raw)
        || raw
            .bytes()
            .enumerate()
            .any(|(index, byte)| byte == b'%' && (index == 0 || raw.as_bytes()[index - 1] != b'\\'))
    {
        return None;
    }
    argument.span = syntax::trimmed_span(source, argument.span);
    argument.value = unquote(&source[argument.span.clone()])?.into_owned();
    Some(argument)
}

pub(crate) fn prefix(
    source: &str,
    command: &syntax::Command,
    cursor: usize,
) -> Option<Range<usize>> {
    let start = syntax::file_name_argument_start(source, "includegraphics", command.span.end)?;
    let raw = source.get(start..cursor)?.strip_prefix('{')?;
    if has_paragraph(raw) {
        return None;
    }
    let raw = raw.trim_start();
    let value = unquote_prefix(raw);
    // A trailing space can be the start of the next word in a filename.
    syntax::is_literal_file_path(&format!("{value}x")).then_some(cursor - raw.len()..cursor)
}

pub(crate) fn quoted_replacement(value: String, original: &str, following_path: bool) -> String {
    if !original.contains('"') {
        value
    } else if following_path && original.bytes().filter(|byte| *byte == b'"').count() % 2 != 0 {
        // The matching closing quote remains after an existing slash.
        format!("\"{value}")
    } else {
        format!("\"{value}\"")
    }
}

/// Normalize image calls in the full project, including recorder-only sources.
/// This must remain separate from static input discovery to avoid recursion.
pub(crate) fn calls(
    workspace: &Workspace,
    root: &Url,
) -> BTreeMap<Url, Vec<crate::symbols::commands::SourceCall>> {
    let inputs = crate::project::source_inputs::Index::new(workspace, root);
    if inputs.incomplete {
        return BTreeMap::new();
    }
    calls_in(
        workspace,
        &workspace.project_documents_with_inputs(root, root, &inputs),
    )
}

pub(crate) fn calls_in(
    workspace: &Workspace,
    sources: &BTreeSet<Url>,
) -> BTreeMap<Url, Vec<crate::symbols::commands::SourceCall>> {
    let Some(documents) = crate::project::source_inputs::bounded_latex_sources(workspace, sources)
    else {
        return BTreeMap::new();
    };
    // Every supported alias chain must reach the builtin, either as a control
    // word or a literal name. Named controls can normalize source spelling, so
    // retain full analysis whenever they occur, even without the literal text.
    // Check all bounded sources, including recorder-only definitions, before
    // deciding that no document can contain an image call.
    if !documents.iter().any(|(_, doc)| {
        doc.text.source.contains("includegraphics")
            || doc
                .syntax
                .commands
                .iter()
                .any(|command| matches!(command.name.as_str(), "csuse" | "csname" | "endcsname"))
    }) {
        return documents
            .iter()
            .map(|(uri, _)| (uri.clone(), Vec::new()))
            .collect();
    }
    let signatures = crate::symbols::query_cache::source_signatures(workspace, sources);
    documents
        .iter()
        .map(|(uri, doc)| {
            let positions = signatures.source_command_positions(&doc.text.source, &doc.syntax);
            (
                uri.clone(),
                signatures.graphics_invocations(&doc.text.source, &doc.syntax, &positions),
            )
        })
        .collect()
}

/// Keep binary dependencies out of the text index while sharing link resolution.
pub fn images(workspace: &Workspace, uri: &Url) -> BTreeSet<Url> {
    collect_images(
        workspace,
        uri,
        crate::language::navigation::document_links(workspace, uri),
        &workspace.source_roots(uri),
    )
}

pub(crate) fn images_in(workspace: &Workspace, uri: &Url, root: &Url) -> BTreeSet<Url> {
    collect_images(
        workspace,
        uri,
        crate::language::navigation::document_links_in(workspace, uri, root),
        std::slice::from_ref(root),
    )
}

fn collect_images(
    workspace: &Workspace,
    uri: &Url,
    links: Vec<lsp_types::DocumentLink>,
    roots: &[Url],
) -> BTreeSet<Url> {
    let Some(doc) = workspace.documents.get(uri) else {
        return BTreeSet::new();
    };
    let ranges: Vec<_> = roots
        .iter()
        .flat_map(|root| calls(workspace, root).remove(uri).unwrap_or_default())
        .filter(|call| call.certain)
        .filter_map(|call| call.command.argument)
        .map(|argument| doc.text.range(argument.span.start, argument.span.end))
        .collect();
    if ranges.is_empty() {
        return BTreeSet::new();
    }
    links
        .into_iter()
        .filter(|link| {
            ranges
                .iter()
                .any(|range| range.start <= link.range.start && range.end >= link.range.end)
        })
        .filter_map(|link| link.target)
        .collect()
}

pub struct Directory {
    pub path: PathBuf,
    pub span: Range<usize>,
    pub origin: Url,
}

pub fn declarations(workspace: &Workspace, uri: &Url) -> Vec<Directory> {
    let origins = workspace.source_roots(uri);
    origins
        .iter()
        .flat_map(|origin| declarations_in(workspace, uri, origin))
        .collect()
}

pub(crate) fn declarations_in(workspace: &Workspace, uri: &Url, origin: &Url) -> Vec<Directory> {
    declarations_with(workspace, uri, origin)
}

/// Literal entries retain source spans so directory moves can update aliases too.
/// Only pdfTeX confirms quoted directories: other graphicx drivers can check
/// literal quotes during existence tests before stripping them for rendering.
pub(crate) fn path_entries(
    source: &str,
    command: &syntax::Command,
    quoted_directories: bool,
) -> (Vec<(Range<usize>, String)>, bool) {
    let Some(argument) = &command.argument else {
        return (vec![], false);
    };
    if argument.outer_span.start != syntax::skip_space(source, command.span.end) {
        return (vec![], false);
    }
    let mut result = Vec::new();
    let mut complete = true;
    let mut at = 0;
    while at < argument.value.len() {
        at = syntax::skip_space(&argument.value, at);
        if at == argument.value.len() {
            break;
        }
        let Some((directory, end)) = syntax::group(&argument.value, at, b'{', b'}') else {
            return (result, false);
        };
        at = end;
        let raw_span =
            argument.span.start + directory.span.start..argument.span.start + directory.span.end;
        let raw = &source[raw_span.clone()];
        // The directory is concatenated with the filename. Trailing space
        // therefore belongs between the slash and that filename, even inside
        // quotes; trimming it would infer a directory TeX does not search.
        if !quoted_directories && raw.contains('"')
            || has_paragraph(raw)
            || raw
                .chars()
                .rev()
                .find(|character| *character != '"')
                .is_some_and(|character| character.is_ascii_whitespace())
        {
            complete = false;
            continue;
        }
        let Some(value) = unquote(raw) else {
            complete = false;
            continue;
        };
        if !value.ends_with('/')
            || value.contains(['\\', '{', '}', '%', '#', '$', '&', '^', '~', '\n', '\r'])
        {
            complete = false;
            continue;
        }
        result.push((syntax::trimmed_span(source, raw_span), value.into_owned()));
        if result.len() >= 256 {
            return (result, false);
        }
    }
    (result, complete)
}

fn declarations_with(workspace: &Workspace, uri: &Url, origin: &Url) -> Vec<Directory> {
    let Some(doc) = workspace.documents.get(uri) else {
        return vec![];
    };
    let Some(parent) = origin
        .to_file_path()
        .ok()
        .and_then(|path| path.parent().map(Path::to_owned))
    else {
        return vec![];
    };
    let mut scope = workspace.project_documents(origin);
    scope.insert(uri.clone());
    let signatures = crate::symbols::commands::CommandSignatures::source_context(
        scope.iter().filter_map(|uri| {
            workspace
                .documents
                .get(uri)
                .map(|doc| (doc.text.source.as_str(), &doc.syntax))
        }),
    );
    if !signatures.command_unchanged("graphicspath") {
        return vec![];
    }
    let positions = signatures.source_command_positions(&doc.text.source, &doc.syntax);
    let indirect =
        signatures.indirect_graphics_path_invocations(&doc.text.source, &doc.syntax, &positions);
    let mut directories = Vec::new();
    for command in doc
        .syntax
        .commands
        .iter()
        .filter(|command| command.name == "graphicspath" && positions.contains(&command.span.start))
        .chain(
            indirect
                .iter()
                .filter(|call| call.certain)
                .map(|call| &call.command),
        )
    {
        let (entries, _) = path_entries(
            &doc.text.source,
            command,
            workspace.build_config(origin).effective_engine()
                == Some(crate::config::Engine::PdfLatex),
        );
        for (span, value) in entries {
            let path = parent.join(value);
            let path = crate::diagnostics::diagnostic_inputs::canonicalize(&path)
                .unwrap_or_else(|_| normalize_path(&path));
            directories.push(Directory {
                path,
                span,
                origin: origin.clone(),
            });
            if directories.len() >= 256 {
                return directories;
            }
        }
    }
    directories
}

/// Potential declared directories for browsing; image resolution uses settings at the call position.
pub fn directories(workspace: &Workspace, uri: &Url) -> Vec<PathBuf> {
    if let Ok(root) = workspace.root_document(uri) {
        return directories_in(workspace, uri, &root);
    }
    let mut result = Vec::new();
    for root in workspace.source_roots(uri) {
        for directory in directories_in(workspace, uri, &root) {
            if !result.contains(&directory) {
                result.push(directory);
            }
            if result.len() >= 256 {
                return result;
            }
        }
    }
    result
}

pub(crate) fn directories_in(workspace: &Workspace, uri: &Url, root: &Url) -> Vec<PathBuf> {
    let project = workspace.project_documents(root);
    let mut seen = BTreeSet::new();
    let mut result = Vec::new();
    for document in std::iter::once(uri)
        .chain(std::iter::once(root))
        .chain(project.iter())
    {
        if !seen.insert(document.clone()) {
            continue;
        }
        for directory in declarations_in(workspace, document, root) {
            if directory.path.is_dir() && !result.contains(&directory.path) {
                result.push(directory.path);
            }
            if result.len() >= 256 {
                return result;
            }
        }
    }
    result
}

pub fn resolve(directory: &std::path::Path, name: &str, extension: &str) -> Option<Url> {
    let origin = Url::from_file_path(directory.join("__texquill_graphics__.tex")).ok()?;
    resolve_path(&origin, &unquote(name)?, extension)
}

/// graphicx searches one extension across the root and its declared directories
/// before trying the next extension. Included sources do not change TeX's cwd.
pub(crate) fn resolve_in(
    root: &Url,
    name: &str,
    extensions: &[&str],
    directories: &[PathBuf],
) -> Option<Url> {
    resolve_with(root, name, extensions, directories, |path| {
        let path = crate::diagnostics::diagnostic_inputs::canonicalize(path).ok()?;
        path.is_file().then_some(path)
    })
}

/// The same search can inspect the filesystem after a proposed rename batch
/// without moving files or constructing temporary copies of binary assets.
pub(crate) fn resolve_with(
    root: &Url,
    name: &str,
    extensions: &[&str],
    directories: &[PathBuf],
    file: impl Fn(&Path) -> Option<PathBuf>,
) -> Option<Url> {
    let mut literal = unquote(name)?.into_owned();
    for character in ['_', '%', '#', '&', '$', ' '] {
        literal = literal.replace(&format!("\\{character}"), &character.to_string());
    }
    let name = literal.as_str();
    if name.is_empty() || name.contains(['\\', '{', '}']) {
        return None;
    }
    let parent = root.to_file_path().ok()?.parent()?.to_owned();
    let exact = |name: &str| {
        file(&parent.join(name)).or_else(|| {
            directories
                .iter()
                .find_map(|directory| file(&directory.join(name)))
        })
    };
    let explicit = std::path::Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str());
    let direct = explicit.and_then(|_| exact(name));
    let selected = if direct.is_some()
        && explicit.is_some_and(|extension| {
            syntax::file_extensions("includegraphics")
                .unwrap()
                .iter()
                .any(|supported| extension.eq_ignore_ascii_case(supported))
        }) {
        direct
    } else {
        // graphicx also tries appending its extensions to a dotted filename
        // when its explicit name is missing or has no known graphics rule.
        extensions
            .iter()
            .find_map(|extension| exact(&format!("{name}.{extension}")))
            .or(direct)
    };
    selected.and_then(|path| Url::from_file_path(path).ok())
}
