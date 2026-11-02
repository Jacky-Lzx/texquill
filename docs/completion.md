# Completion

[Back to documentation](README.md) · [Editing features](editing.md) · [Editor configuration](editors.md)

Supports common LaTeX commands, environments, in-project user commands, cross-file labels, BibTeX entry types, field names, string abbreviations and citation keys, as well as paths to input files, images and local packages. Supports mid-word substitution; when optional parameters of references and paths contain nested commands, the current key name or file parameter can still be found.

Ordinary command candidates are prepared in the background when a LaTeX buffer
opens. Eligible prose, comment and command-prefix edits reuse those templates;
replacement ranges and snippet decisions still use the current text. Macro,
dependency, documentation and settings changes invalidate affected preparation.
Cold requests, complex parameter readers, literal regions and unsupported
contexts use the full contextual analysis. Label, citation and path providers
retain their existing context checks. No editor configuration change is needed.

Math-only commands from the built-in catalog and loaded packages (such as `alpha`, `frac`, and `dfrac`) are completed only in math mode. The server recognizes `$…$`, `$$…$$`, `\(…\)`, `\[…\]`, `ensuremath`, and common math environments; text arguments such as `text` and `mbox` inside formulas restore text mode. Comments, verbatim content, escaped symbols, and unexpanded command-name operands do not change the mode. User definitions retain priority, and math commands remain available when declaring, copying, or inspecting command names. This check uses static syntax in the current file; it does not expand macros or infer math mode for custom environments.

biblatex's `citefield` / `printfield`, `citelist` / `printlist`, `citename` / `printnames` also completes common output fields, literal lists or name list names by parameter type, and provides hover instructions; see [biblatex Data Access](biblatex.md#fields-lists-and-name-references).

Field reads, undef checks, and field/list comparisons share name candidates. Both field parameters of `iffieldsequal` can be completed; the comparison string, command content and ordinary text in the execution branch remain independent. For details, see [Field and List Conditions](biblatex.md#field-and-list-conditions).

The first parameter of `ifentrytype` recommends commonly used biblatex output entry types, using ordinary name editing and type description; its two branches remain independent, see [Entry Condition Check](biblatex.md#entry-condition-check) for details.

## BibTeX entries and fields

Complete common BibTeX / biblatex entry types after `@`, and also provide custom types that already appear in the current project library. Enter `@art` to select `article`, which is not case-sensitive. In-word completion will replace the complete type name and retain `@`. When there is already a curly or parenthesis body, only the name is replaced, the entry key, fields, and comments are retained.

Clients that support snippets can directly insert the field template of a new entry, for example, select `article` and fill in the citation key, author, title, journal, and year. The template takes the LF/CRLF of the current document; `comment`, `string` and `preamble` use their respective syntax. The template provides common fields and is not used to determine style requirements or verify required items; the custom type generates a general entry skeleton.

Provides common field completion at commas after entry keys, spaces between fields, or incomplete field names. Candidates include field descriptions, which prioritize commonly used fields of the current type and exclude field names that have already been filled in in the same entry; comparisons are not case-sensitive. When editing an existing field name, its assignment, field value, and comments are retained. For example, completing `AUTHbad` in `AUTHbad = {Writer}` to `author` will not modify `{Writer}`.

Candidates also contain custom fields that have values assigned to them in the current project library. The original name is retained and deduplicated according to ASCII case; fields with the same name are given priority to use the spelling in the current file, and built-in fields use standard lowercase names. Commonly used fields of the current type are listed first, and custom fields are displayed after the other built-in fields. Names that have not yet been assigned will not become project candidates, and unfinished assignments that have entered `=` can still contribute field names. Examples from other projects, notes, and special entries are not included in the collection.

Field names adopt BibTeX identifier rules, supporting legal names such as `custom:field`, `field$`, and non-ASCII characters; their subsequent fields continue to participate in outline, navigation, completion, and semantic highlighting. Entering `AUTH` in `AUTH:bad` and then selecting `author` will replace the entire `AUTH:bad` name. Field names beginning with a number are not parsed as valid assignments.

When fragments are supported, the new field inserts a value placeholder, such as `author = {…},`, and retains the existing commas; when `=` already exists, only the field name is changed. `$` and backslashes in custom field names are escaped per snippet syntax and the original characters are retained after expansion. Clients without snippet support only insert type names or field names. Built-in item types and field names also provide hover descriptions; item keys and inter-item references continue to use document navigation and completion.

Contextual reuse of entry and field parsing does not suggest types or field names in entry keys, titles, URLs, other field values, comments, and special entry bodies. Subsequent fields are not guessed when a comma after a key is missing or an unparsable field is encountered. Candidates for custom types and fields are isolated by project and names from independent papers are not introduced; unsaved incremental edits update candidates immediately, restoring current disk contents after closing the file.

verify:

```sh
cargo test --locked --test bibliography_completion --test bibliography
cargo test --locked --test protocol bibliography_type_and_field_
cargo test --locked --test protocol custom_bibliography_field_completions
cargo test --locked --test bibliography_completion real_bibtex_reads_completed -- --include-ignored
cargo test --locked --test bibliography_completion bibliography_snippets -- --include-ignored
```

The last item is expanded by the `vim.snippet` check snippet of real Neovim 0.10+; the native BibTeX regression uses a custom style to read the completed field, verify that the assigned value and subsequent title are preserved.

BibTeX field values, string assignments, and expressions for `@preamble` also provide in-project [string abbreviation](bibliography.md#string-abbreviations) and common month completions. Candidates can replace the complete naked identifier, or fill in the empty value position after `=`, `#`, preserving the literal text and splicing structure.

## Call snippets and existing arguments

The completion description of the unique user macro and environment includes the original declaration information and the consecutive [Document Comments](command-documentation.md) before the declaration, and is displayed as plain text; when there is ambiguity in the declaration with the same name, the original brief information is retained. Environment name completion and normal control word completion reuse the project declaration index built once to avoid scanning the entire project for each candidate. The declarations of macros and environments are judged separately, and the respective descriptions of declarations with the same name are retained.

The server provides standard `completionItem/resolve`. When the client contains `documentation` in `textDocument.completion.completionItem.resolveSupport.properties`, the user macro and environment document are deferred until the parsing request is returned; other clients receive the inline description directly. Fragment, name substitution, additional editing and sorting fields retain their original values during parsing. Candidates that do not need to be parsed, such as ordinary built-ins, are returned unchanged.

Parse data binding raw documents, declaration namespaces, unique declarations, project file contents and versions. `-32801` is returned when the relevant buffer changes, the declaration disappears, the scope changes, or the data is rewritten, and the client should request completion again; data with illegal structure returns `InvalidParams`. Modifications to independent projects will not invalidate candidates for the current project. This delay mechanism covers the normal user-controlled word and environment name branches, and dedicated expl3, path, literature, and other name parameter completions continue to use their respective on-the-fly instructions.

When the client declares snippet support, command completion can insert parameter placeholders; otherwise, only the command name is inserted. User commands generate fragments based on static signatures within the project, omitting optional parameters and flags, and retaining the default behavior.

See [Signature help](signatures.md) for the comment rules for cross-file declarations and number of parameters.

If there are already parameters, only the names will be replaced and parameters will not be added repeatedly. Built-in commands also retain existing braceless ASCII arguments, such as `\frac12` and `\textbf\payload`; parameter placeholders are still inserted before math, alignment, or environment terminators. The same rules apply to user macros, with explicitly declared flags and delimiters taking precedence.

## Command-name operands

Declaring the target, command copy source, and `meaning` view location only reads the command name and does not invoke the command. When you enter part of a command name and accept completion, only the name is replaced and the parameters of the named command are not inserted. This rule is also followed when only backslashes are entered and no letters are entered, and it is compatible with unclosed name groups. For example, when there is `\newcommand{\ordinary}[1]{#1}` in the project:

| Input | After selecting `ordinary` |
| --- | --- |
| `\meaning\` | `\meaning\ordinary` |
| `\meaning\ord` | `\meaning\ordinary` |
| `\let\saved=\ord` | `\let\saved=\ordinary` |
| `\newcommand{\ord}` | `\newcommand{\ordinary}` |
| `\futurelet\ord\relax x` | `\futurelet\ordinary\relax x` |

The same rules apply to built-in commands as to cross-file user commands. In-word completion replaces the remaining command letters, retaining backslashes, comments, line breaks, and subsequent text; the position uses UTF-16 and is compatible with Chinese and emoji.

The actual call still provides the argument snippet, including the subsequent execution command of `futurelet`, and the real command that appears again after the operand is viewed or copied. The operand judgment follows the static parsing boundary; when the relevant keywords have been explicitly redefined in the previous section of this document, they are processed as ordinary calls. Does not perform arbitrary macro expansion, nor does it fully infer cross-file overwrites or group recovery.

For related semantics, see [Command-name operands and declaration](declaration-operands.md), [meaning](meaning.md) and [futurelet](futurelet.md).

The grouped text of `detokenize` and the token converted to text by `string` do not provide ordinary command completion; these wrappers in the file name parameter can still be used for path completion. The conversion command viewed or copied by `meaning` does not start text conversion, and subsequent real commands continue to provide completion.

## Name replacement

When single-label reference, environment, or counter name completion is selected, the current name parameter is replaced with the full name of the candidate, including leading and trailing whitespace within the old name, to avoid still referencing another name after completion. Preserves parameter separators, values, formatting, and typesetting outside parameters.

Cross-line names use the current line's main edit and non-overlapping additional delete edits, which conforms to LSP's single-line requirements for completion edits. No suggestions are provided for dynamic names and empty lines where the main editor cannot be safely placed.
