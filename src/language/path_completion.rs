use crate::{parser::syntax, project::workspace::Workspace};
use lsp_types::*;
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    ops::Range,
    path::Path,
};

/// Retain only the best paths while preserving folder/file/distribution order.
struct Candidates {
    items: BTreeMap<String, CompletionItem>,
    ranked: BTreeSet<(u8, String)>,
    limit: usize,
}

impl Candidates {
    fn accepts(&self, label: &str, group: u8) -> bool {
        !self.items.contains_key(label)
            && (self.ranked.len() < self.limit
                || self.ranked.last().is_some_and(|(last_group, last)| {
                    (group, label) < (*last_group, last.as_str())
                }))
    }

    fn insert(&mut self, label: String, group: u8, item: CompletionItem) {
        self.ranked.insert((group, label.clone()));
        self.items.insert(label, item);
        if self.ranked.len() > self.limit {
            let (_, label) = self.ranked.pop_last().unwrap();
            self.items.remove(&label);
        }
    }

    fn finish(mut self) -> Vec<CompletionItem> {
        self.ranked
            .into_iter()
            .map(|(_, label)| self.items.remove(&label).unwrap())
            .collect()
    }
}

pub fn complete(
    workspace: &Workspace,
    uri: &Url,
    command: &str,
    span: Range<usize>,
) -> Vec<CompletionItem> {
    complete_with_argument(workspace, uri, command, span, None, None, None)
}

/// The caller has certified the native input meaning and its compilation root.
pub(crate) fn complete_prepared_input(
    workspace: &Workspace,
    uri: &Url,
    span: Range<usize>,
) -> Vec<CompletionItem> {
    complete_with_argument(
        workspace,
        uri,
        "input",
        span,
        None,
        None,
        Some(vec![uri.clone()]),
    )
}

pub(crate) fn complete_source(
    workspace: &Workspace,
    uri: &Url,
    command: &syntax::Command,
    span: Range<usize>,
) -> Vec<CompletionItem> {
    if crate::bibliography::bib_resources::references_file(&command.name) {
        let Some(doc) = workspace.documents.get(uri) else {
            return vec![];
        };
        if crate::bibliography::bib_resources::read_argument(
            &doc.text.source,
            command.span.end,
            false,
        )
        .is_some_and(|argument| {
            !crate::bibliography::bib_resources::insertion_preserves_filename(
                workspace,
                uri,
                &workspace.source_roots(uri),
                Some(command.span.start),
                &argument.value,
            )
        }) {
            return vec![];
        }
    }
    // Forwarding stores options in command.argument; its filename follows in
    // the second group. An unfinished filename uses the suffix scanner below.
    let argument = if syntax::passes_options(&command.name) {
        workspace
            .documents
            .get(uri)
            .and_then(|doc| syntax::file_argument(&doc.text.source, command))
    } else {
        command.argument.as_ref().map(Cow::Borrowed)
    };
    complete_with_argument(
        workspace,
        uri,
        &command.name,
        span,
        argument.as_deref(),
        Some(command.span.start),
        None,
    )
}

fn complete_with_argument(
    workspace: &Workspace,
    uri: &Url,
    command: &str,
    span: Range<usize>,
    argument: Option<&syntax::Argument>,
    call_start: Option<usize>,
    prepared_roots: Option<Vec<Url>>,
) -> Vec<CompletionItem> {
    let Some(doc) = workspace.documents.get(uri) else {
        return vec![];
    };
    let Some(extensions) = syntax::file_extensions(command) else {
        return vec![];
    };
    let Some(raw_prefix) = doc.text.source.get(span.clone()) else {
        return vec![];
    };
    let compact = syntax::file_name_ignores_spaces(command);
    let normalized_prefix = if command == "includegraphics" {
        Some(crate::project::graphics::unquote_prefix(raw_prefix).into_owned())
    } else {
        compact.then(|| syntax::loader_name(raw_prefix))
    };
    let prefix = normalized_prefix.as_deref().unwrap_or(raw_prefix);
    if prefix.contains(['\\', '{', '}'])
        || !crate::bibliography::bib_resources::references_file(command)
            && prefix.contains(['%', '#', '$', '&', '^', '~'])
    {
        return vec![];
    }
    let separator = prefix.rfind('/').map_or(0, |index| index + 1);
    let directory = &prefix[..separator];
    let name_prefix = &prefix[separator..];
    let input_argument = argument.or_else(|| {
        matches!(command, "input" | "InputIfFileExists" | "IfFileExists")
            .then(|| {
                doc.syntax
                    .commands
                    .iter()
                    .filter(|candidate| candidate.name == command)
                    .filter_map(|command| command.argument.as_ref())
                    .find(|argument| {
                        argument.span.start == span.start && span.end <= argument.span.end
                    })
            })
            .flatten()
    });
    let argument_end = input_argument.map_or(doc.text.source.len(), |argument| argument.span.end);
    let Some(suffix) = doc.text.source.get(span.end..argument_end) else {
        return vec![];
    };
    let suffix_length = suffix
        .find(|ch| {
            matches!(ch, '/' | '{' | '}' | '\\')
                || ch == '%' && !crate::bibliography::bib_resources::references_file(command)
                || ch == ',' && syntax::file_list(command)
                || crate::parser::fancyvrb::is_input(command) && ch == '"'
                || !compact
                    && !(command == "includegraphics" && input_argument.is_some())
                    && matches!(ch, '\n' | '\r')
        })
        .unwrap_or(suffix.len());
    let suffix_length = suffix[..suffix_length].trim_end().len();
    let existing_slash = suffix.as_bytes().get(suffix_length) == Some(&b'/');
    // Normalization changes byte lengths; edits and suffix inspection still
    // use offsets in the original document, including UTF-8 and CRLF.
    let raw_separator = raw_prefix.rfind('/').map_or(0, |index| index + 1);
    let raw_name = &doc.text.source[span.start + raw_separator..span.end + suffix_length];
    let normalized_name = compact.then(|| syntax::loader_name(raw_name));
    let existing_name = normalized_name.as_deref().unwrap_or(raw_name);
    let Some((range, mut additional_edits)) =
        syntax::name_completion_ranges(&doc.text, span.start..span.end + suffix_length, span.end)
    else {
        return vec![];
    };
    let filter_prefix = raw_prefix.rsplit('\n').next().unwrap_or(raw_prefix);
    let roots = prepared_roots.unwrap_or_else(|| {
        workspace
            .root_document(uri)
            .map(|root| vec![root])
            .unwrap_or_else(|_| workspace.source_roots(uri))
    });
    let raw_resource_insertion = std::cell::OnceCell::new();
    if syntax::code_input_command(command)
        && !doc.syntax.commands.iter().any(|candidate| {
            candidate.name == command
                && (if crate::project::code_inputs::wrapped_filename(command) {
                    crate::project::code_inputs::prefix(&doc.text.source, candidate, span.end)
                        == Some(span.clone())
                } else {
                    syntax::file_name_argument_start(&doc.text.source, command, candidate.span.end)
                        == Some(span.start.saturating_sub(1))
                })
                && roots.iter().all(|root| {
                    crate::project::code_inputs::supported(workspace, uri, root, candidate)
                })
        })
    {
        return vec![];
    }
    let section_labels: BTreeSet<_> =
        if crate::bibliography::bib_section_files::file_command(command) {
            roots
                .iter()
                .flat_map(|root| {
                    crate::project::source_inputs::Index::new(workspace, root).section_labels
                })
                .collect()
        } else {
            BTreeSet::new()
        };
    let mut candidates = Candidates {
        items: BTreeMap::new(),
        ranked: BTreeSet::new(),
        limit: workspace.completion_budget(),
    };
    let allow_spaces = command != "inputminted"
        || roots
            .iter()
            .all(|root| crate::project::code_inputs::allows_spaces(workspace, uri, root));
    let mut insertions = BTreeSet::new();
    let mut directories = roots
        .iter()
        .chain(std::iter::once(uri).filter(|uri| {
            command != "includegraphics"
                && !syntax::code_input_command(command)
                && !roots.contains(uri)
        }))
        .filter_map(|origin| {
            origin
                .to_file_path()
                .ok()?
                .parent()
                .map(|path| path.to_path_buf())
        })
        .collect::<Vec<_>>();
    if command == "includegraphics" {
        if let Some(start) = call_start.or_else(|| {
            doc.syntax
                .commands
                .iter()
                .rev()
                .find(|candidate| {
                    candidate.name == "includegraphics" && candidate.span.end <= span.start
                })
                .map(|command| command.span.start)
        }) {
            for root in &roots {
                if let Some(paths) =
                    crate::project::graphics_settings::Index::directories(workspace, root)
                        .paths_at(root, uri, start, true)
                {
                    directories.extend(paths);
                }
            }
        }
    }
    for parent in directories {
        let Ok(origin) = Url::from_file_path(parent.join("__texquill_completion__.tex")) else {
            continue;
        };
        let Ok(entries) = std::fs::read_dir(parent.join(directory)) else {
            continue;
        };
        for entry in entries.take(4096).flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if !name.starts_with(name_prefix)
                || crate::bibliography::bib_section_files::file_command(command)
                    && name.contains([',', ']'])
                || !allow_spaces && name.chars().any(char::is_whitespace)
                || name.starts_with('.') && !name_prefix.starts_with('.')
                || (compact || syntax::passes_options(command) || command == "bibliographystyle")
                    && name.contains([' ', '\t', '\r', '\n'])
                || name.contains(['\\', '{', '}', '"', '\n', '\r'])
                || !crate::bibliography::bib_resources::references_file(command)
                    && name.contains(['%', '#', '$', '&', '^', '~'])
            {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let folder = metadata.is_dir();
            if !folder && (!metadata.is_file() || existing_slash) {
                continue;
            }
            let extension = Path::new(&name)
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or("");
            if !folder
                && command != "IfFileExists"
                && !syntax::code_input_command(command)
                && !extensions
                    .iter()
                    .any(|ext| extension.eq_ignore_ascii_case(ext))
            {
                continue;
            }
            let label = format!("{directory}{name}{}", if folder { "/" } else { "" });
            if !candidates.accepts(&label, if folder { 0 } else { 1 }) {
                continue;
            }
            let mut new_text = format!("{directory}{name}");
            if folder {
                if !existing_slash {
                    new_text.push('/');
                }
            } else if command != "includegraphics"
                && !crate::bibliography::bib_resources::references_file(command)
                && !syntax::code_input_command(command)
            {
                let explicit_extension = existing_name
                    .rsplit_once('.')
                    .is_some_and(|(_, typed)| extension.starts_with(typed));
                // Uppercase extensions cannot be inferred by TeX on a case-sensitive filesystem.
                if (syntax::package_class_extension(command).is_some() || !explicit_extension)
                    && extensions.first() == Some(&extension)
                {
                    new_text.truncate(new_text.len() - extension.len() - 1);
                }
            }
            if crate::bibliography::bib_section_files::file_command(command)
                && (!crate::bibliography::bib_section_files::editable_item(&new_text)
                    || section_labels.contains(&new_text))
            {
                continue;
            }
            if crate::bibliography::bib_resources::references_file(command)
                && crate::bibliography::bib_resources::changes_when_tokenized(&new_text)
                && !raw_resource_insertion.get_or_init(|| {
                    crate::bibliography::bib_resources::insertion_preserves_filename(
                        workspace,
                        uri,
                        &workspace.source_roots(uri),
                        call_start,
                        "%",
                    )
                })
            {
                continue;
            }
            if !folder {
                let resolved = if command == "includegraphics" {
                    crate::project::graphics::resolve_in(
                        &origin,
                        &new_text,
                        syntax::inferred_file_extensions(command).unwrap(),
                        &[],
                    )
                } else {
                    crate::project::workspace::resolve_file_path(
                        &origin,
                        command,
                        &new_text,
                        syntax::inferred_file_extensions(command).unwrap(),
                    )
                };
                let candidate = entry
                    .path()
                    .canonicalize()
                    .ok()
                    .and_then(|path| Url::from_file_path(path).ok());
                if resolved != candidate {
                    continue;
                }
            }
            if !insertions.insert(new_text.clone()) {
                continue;
            }
            let mut edit_range = range;
            let mut filter_start = "";
            if command == "input" {
                if let Some(argument) = input_argument {
                    let completed = format!(
                        "{new_text}{}",
                        &doc.text.source[span.end + suffix_length..argument_end]
                    );
                    if syntax::input_filename_needs_braces(&doc.text.source, argument, &completed) {
                        let edit = syntax::input_filename_edit(&doc.text, argument, completed);
                        edit_range = edit.range;
                        new_text = edit.new_text;
                        filter_start = &doc.text.source[argument.outer_span.start..span.start];
                    }
                }
            }
            if crate::project::code_inputs::wrapped_filename(command) {
                let candidate = doc.syntax.commands.iter().find(|candidate| {
                    candidate.name == command
                        && crate::project::code_inputs::prefix(
                            &doc.text.source,
                            candidate,
                            span.end,
                        ) == Some(span.clone())
                });
                let Some(edit) = candidate.and_then(|candidate| {
                    crate::project::code_inputs::completion_edit(
                        &doc.text,
                        candidate,
                        new_text.clone(),
                        span.end + suffix_length,
                    )
                }) else {
                    continue;
                };
                let Some((range, extra)) = syntax::name_completion_ranges(
                    &doc.text,
                    doc.text.offset(edit.range.start).unwrap()
                        ..doc.text.offset(edit.range.end).unwrap(),
                    span.end,
                ) else {
                    continue;
                };
                edit_range = range;
                additional_edits = extra;
                new_text = edit.new_text;
                let from = doc.text.offset(edit_range.start).unwrap();
                filter_start = &doc.text.source[from..span.start];
            }
            if command == "includegraphics" {
                new_text = crate::project::graphics::quoted_replacement(
                    new_text,
                    &doc.text.source[span.start..span.end + suffix_length],
                    folder && existing_slash,
                );
            }
            candidates.insert(
                label.clone(),
                if folder { 0 } else { 1 },
                CompletionItem {
                    label: label.clone(),
                    // Clients filter against the text before the cursor. Keep
                    // its spelling while displaying the real target path.
                    filter_text: Some(format!(
                        "{filter_start}{filter_prefix}{}",
                        &label[prefix.len()..]
                    )),
                    sort_text: Some(format!("{}{label}", if folder { "0" } else { "1" })),
                    detail: Some(entry.path().display().to_string()),
                    kind: Some(if folder {
                        CompletionItemKind::FOLDER
                    } else {
                        CompletionItemKind::FILE
                    }),
                    text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                        range: edit_range,
                        new_text,
                    })),
                    additional_text_edits: (!additional_edits.is_empty())
                        .then(|| additional_edits.clone()),
                    ..Default::default()
                },
            );
        }
    }
    if directory.is_empty() {
        if let Some(extension) = syntax::package_class_extension(command)
            .or_else(|| (command == "bibliographystyle").then_some("bst"))
        {
            for (file, uri) in workspace.distribution.files.iter() {
                let Some(stem) = file.strip_suffix(&format!(".{extension}")) else {
                    continue;
                };
                let new_text = if extension == "bst" && existing_name.contains('.') {
                    file
                } else {
                    stem
                };
                if !file.starts_with(name_prefix)
                    || !candidates.accepts(file, 2)
                    || !insertions.insert(new_text.to_owned())
                    || !uri.to_file_path().is_ok_and(|path| path.is_file())
                {
                    continue;
                }
                candidates.insert(
                    file.clone(),
                    2,
                    CompletionItem {
                        label: file.clone(),
                        kind: Some(CompletionItemKind::MODULE),
                        detail: Some(format!("TeX distribution — {}", uri.path())),
                        filter_text: Some(format!("{filter_prefix}{}", &file[prefix.len()..])),
                        sort_text: Some(format!("2{file}")),
                        text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                            range,
                            new_text: new_text.to_owned(),
                        })),
                        additional_text_edits: (!additional_edits.is_empty())
                            .then(|| additional_edits.clone()),
                        ..Default::default()
                    },
                );
            }
        }
    }
    candidates.finish()
}
