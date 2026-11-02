# Builds

[Back to README](../README.md)

Background compilation command, configuration, status and product consistency check. See [Build diagnostics](diagnostics.md) for specific location rules for errors and warnings, and see [SyncTeX](synctex.md) for source code and PDF query.

- [Tools and Compilation Entry](#tools-and-compilation-entrance)
- [Commands, Status and Notifications](#commands-status-and-notifications)
- [Recompile from Diagnostics](#recompile-from-diagnostics)
- [Result Analysis](#result-analysis)
- [Compile configuration](#compile-configuration)
- [Save, Cancel and Import Snapshot](#save-cancel-and-import-snapshots)
- [Log size and analysis budget](#log-size-and-analysis-budget)

## Tools and compilation entrance

- Background compilation: supports latexmk, pdfLaTeX, XeLaTeX, LuaLaTeX, Tectonic and custom tools; root file identification, cancellation, timeout, status query, editor native progress display and optional post-save compilation.

- In-document compilation entry: supports standard CodeLens, and compiles projects directly in the main file or associated chapters, packages and other TeX sources. Comply with the root file configuration and `% !TeX root` directive; the sharing source can select each main document separately, and the main files with the same name in the multi-workspace folder are distinguished by full paths. Each request to recheck the root file returns up to 32 entries; unsaved standalone documents, fragments that do not belong to a project, and unavailable explicit roots do not provide entries.

The editing function does not depend on TeX; compilation requires the corresponding tools in `PATH` of the language server. `latexmk` is used by default, and the engine sequentially uses explicit `build.engine`, the main file program directive, and the statically recognized `$pdf_mode` in the project rc, otherwise pdfLaTeX is used. The output directory preferentially uses `$out_dir` that can be statically identified in `.latexmkrc` (or `latexmkrc`) next to the root document, otherwise `build/texquill/` is used; the explicit `build.outputDirectory` takes precedence. For the recognition range, see [Compile Configuration](build-configuration.md). Save the project file before compiling. The server will refuse to compile dependencies with unsaved content to avoid inconsistency between diagnostics and editor content.

Build preparation captures the ordered editor state, then scans dependencies,
resolves tool configuration and checks saved inputs on a background workspace.
Editing and completion can continue during preparation. Source reloads are
merged only when their captured document revision is still current; they never
replace a newer buffer edit. Preparing builds participate in cancellation,
shutdown and rebuild queueing. Saves received during preparation are checked
against the prepared inputs before deciding whether a follow-up is necessary.

Compilation can be triggered in Neovim like this:

```lua
vim.lsp.buf_request(0, 'workspace/executeCommand', {
  command = 'latex.build',
  arguments = { vim.uri_from_bufnr(0) },
}, function(err, result)
  if err then vim.notify(err.message, vim.log.levels.ERROR)
  else vim.notify('LaTeX: ' .. result.status) end
end)

vim.lsp.handlers['latex/buildFinished'] = function(_, result)
  if not result or result.stale or result.status == 'cancelled' then return end
  local level = result.status == 'success' and vim.log.levels.INFO or vim.log.levels.ERROR
  vim.notify(result.message, level)
end
```

## Commands, status and notifications

Call the following command via `workspace/executeCommand`, with the first parameter being the current document URI (`{ "uri": "file:///..." }` can also be used):

- `latex.build`: Return `{status: "started", buildId, rootUri}` immediately, compilation runs in the background. When the same root file already has a task, `running` and the original task ID are returned.

- `latex.rebuild`: Force recompile; default latexmk adds `-g`, direct TeX engine and Tectonic run directly every time. Used to restore modified products or when latexmk incremental judgment needs to be skipped. When the same root file is being compiled, duplicate requests are merged into one subsequent forced compilation, which will be executed even if `onSave` is `false`; the save event retains the queued forced requests, and subsequent tasks use the latest configuration and saved content. Explicit cancellation, file create/delete/rename operations, and server shutdown clear the queue. File watcher notifications preserve an existing queued save or forced rebuild and resume it after refreshing the project.

- `latex.cancelBuild`: Cancel the compilation corresponding to this document; cancel all compilation when the parameter is omitted. Also terminate child processes started by latexmk on Unix.

- `latex.buildStatus`: Query the current task or the latest compilation result. If it is not compiled, `idle` is returned; `queued` in operation indicates whether there is subsequent compilation, and `queuedRebuild` indicates whether the subsequent task is forced compilation. `rebuild` in startup response, running status and start/completion notification indicates whether the current task is forced compilation.

When a task is canceled before the compilation worker thread is started, the compiler is not invoked, the output directory is created, or old logs are replaced, and existing PDFs, logs, and auxiliary files are retained. The cancellation result will not return these old files as the products of this compilation.

The server issues `latex/buildStarted` and `latex/buildFinished` notifications, both containing the actual `executable` and the inferred `engine` (`null` when using custom parameters or an unrecognized tool). The completion notice also contains `status` (`success`/`error`/`cancelled`/ `timeout`), `buildId`, `rootUri`, `pdfUri`, `logUri`, `exitCode`, `diagnosticsCount`, `errorsCount`, `warningsCount`, `dependenciesCount` (number of sources including root files), `durationMs`, `message` and length-limited `output` abstract. [`pages`](build-analysis.md#pdf-pages) of the successful result is the page number of the final TeX log report (`null` when unknown), `pdfSize` is the number of bytes of the actual PDF; both of the failed results are `null`. `latex.buildStatus` retains the same digest. If the document or known input selection changes during compilation, `stale` becomes `true`, the old diagnostics for this compilation will not be released, and the diagnostics for the last compilation of the project will be cleared to prevent old errors from remaining in the editor. Failed compilation does not mark potentially incomplete PDFs as successful results.

## Recompile from diagnostics

When the compiler explicitly requires compilation again, you can request a quick fix at the corresponding warning and select `Rebuild …`. Supports LaTeX cross-references, [last page changed](last-page.md), natbib documents, biblatex's LaTeX rerun prompt, longtable table column width, hyperref page tags and rerunfilecheck file change warning; undefined references themselves will not generate this operation.

See [Recompilation Warning for Line Wrapping](build-analysis.md#recompilation-warning-for-line-breaks) for the standard request for word wrapping, continuation lines after the first sentence, and natbib brief prompts.

For the request format of rerunfilecheck, hyperref reverse literature reference, file name wrapping and recompilation mark removal rules, see [File Changes and Recompilation](build-analysis.md#file-changes-and-recompilation).

When biblatex explicitly requires running BibTeX or Biber first, latexmk with default parameters also provides this operation, and latexmk schedules document processing and subsequent LaTeX compilation. Direct TeX engine or custom process configurable [`build.bibliographyTool`](bibliography-processing.md); also provides action when backend and root task names match, otherwise retains prompts and tool summary. Old menu operations will no longer work after switching to a configuration that cannot handle the requested document.

Explicit makeindex requests for indexes and glossaries provide a [Summary of required tools](index-processing.md) and mark the need for subsequent runs of LaTeX; `build.indexTools` can be configured to allow the compilation task to perform tool and subsequent LaTeX passes. When the input name and type of the warning match the current root configuration task, the above menu operations are also provided; the configuration is rechecked during execution.

This operation calls `latex.rebuild`, and multiple warnings in the same main document are merged into one entry. Shared files list the main document that generated the diagnostic separately, avoiding guessing the compilation root from the shared source; only matching currently saved compilation diagnostics and request scopes. The entry is no longer provided after diagnostic clearing, nor is it provided when the root file is missing, the root configuration has been changed to another project, or the custom tool has not been configured `rebuildArguments`.

Supports standard CodeAction and Command responses for legacy clients and does not require the client to support workspace editing. Execution inherits existing compiled save checking, queuing, cancellation and timeout rules.

Menu operations recheck the original root document, source version, and current diagnostics when executed. After the menu is generated, if you modify the root file configuration, edit the source, or clear the corresponding warning, the old operation will report an error and require you to re-request the menu, and will not switch to compiling other projects. The queued compilation that has been accepted also retains the original project ownership; when the subsequent root selection changes, the subsequent task will be canceled. Manually call the URI of `latex.rebuild` or the form of `{uri}` to continue selecting the root file according to the current configuration.

## Result analysis

For the recompilation request, unresolved references, documentation tools and typesetting, fonts and character summaries of the final log, see [Compilation Result Analysis](build-analysis.md).

## Compile configuration

For settings such as compiler, engine, root file, output directory, parameter template, and compilation after saving, see [Compilation Configuration](build-configuration.md).

## Save, cancel and import snapshots

Please save project dependencies before compiling. When the input changes during compilation, the result will be marked as `stale`, and the server discards the old diagnostics; after turning on `onSave`, saves with changed inputs or build settings are merged into one subsequent compilation. Saving identical content during an active build does not queue another compilation, even if the editor rewrites the file or advances its document version. Existing queued saves and explicit forced rebuilds are retained.

See [Compilation Input and Expiration Results](build-inputs.md) for the validation rules for save checking, cancellation, recorder, document resources, file monitoring, and PDF / SyncTeX.

## Log size and analysis budget

When the compile pipeline output exceeds 4 MiB and the log exceeds 8 MiB, the analysis buffer retains the beginning and end and omits the middle; the full log remains on disk. When omitted, the file stack is cleared, explicit file locations and newly emerged file contexts can still be located, and other diagnostics fall back to the main document. `diagnosticsTruncated` of the completion notification indicates that TeX or literature diagnostics uses the omitted content. At this time, the number of diagnoses only counts the analyzed part. A partial recorder or document dependency list will not replace the last complete record.
