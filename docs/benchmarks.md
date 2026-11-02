# Continuous editing benchmarks

[Back to documentation](README.md)

`benchmarks/editing.py` uses the Python 3 standard library to start a real release server and play back the editor session with the LSP stdio protocol. No need to install Python packages, TeX distributions or editors.

```sh
cargo build --release --locked
python3 benchmarks/editing.py --output target/benchmarks/latest.json
```

## Comparing an existing project

`benchmarks/project_latency.py` compares two release binaries on a temporary copy
of a self-contained project. It preserves local class/package files and excludes
build outputs. The original files are never edited or compiled.

```sh
python3 benchmarks/project_latency.py /path/to/project \
  --before target/benchmarks/before --after target/release/texquill \
  --document main.tex --needle '\maketitle' --offset 4 \
  --output target/benchmarks/project-latency.json
```

The script alternates request order and process startup order, measures warmed
queries, single edits and an edit burst, and checks that both versions return the
same responses after each operation (excluding opaque `resultId` values). It
disables distribution discovery, save-triggered builds and ChkTeX to isolate
source analysis. JSON includes all samples, medians and binary hashes. This is a
server protocol measurement, not an editor UI latency measurement.

`benchmarks/completion_load.py` adds the concurrent load of an editing session:
it types `\`, `\i`, `\in`, `\inp` at 350 ms intervals, cancels superseded
completions, and sends automatic linked editing, tokens, inlay hints, colors,
highlights, symbols, diagnostics and code actions after each edit. Diagnostic
refreshes trigger both workspace and attached-document pulls. The main source
and one BibTeX buffer are open, with snippet completion enabled.

```sh
python3 benchmarks/completion_load.py /path/to/project \
  --before target/benchmarks/before --after target/release/texquill \
  --output target/benchmarks/completion-load.json
```

The script alternates startup order over three paired runs, measures the final
keystroke-to-completion response, and checks the complete completion edits and
all final background view responses against the baseline. Superseded views may
be cancelled, but current views must finish successfully. It copies the project
and disables builds and distribution discovery, as above. JSON includes binary
hashes and request timings/errors. Editor menu timing needs a separate real
Neovim check; this protocol workload does not measure rendering.

## Generated editing sessions

By default, three sizes of deterministically generated projects are measured: small is 1 chapter and 16 sections per chapter; medium is 4 chapters and 32 sections per chapter; large is 8 chapters and 64 sections per chapter. Each project contains the main document, chapter files and BibTeX database. The text contains Chinese, emoji, mathematical environment, user macros, cross-file labels and citations. Projects are all below the default index budget; the test rejects index truncation.

Start 3 independent servers at each level, warm up for 3 cycles each time, and then measure for 20 cycles. Execute every cycle:

1. Insert/delete Chinese text, or paste/delete multi-line text with new labels, using UTF-16 increment range.
2. Modify the number of parameters of the user macro in the unsaved main document, and check the latest signature help in the chapter.
3. Query label completion, citation completion, command completion, hover, definition, citation, cross-file renaming, signature help, outline, exact/all/no match search for workspace symbols, folding, and full semantic tokens and cached delta.
4. Introduce/repair undefined references, query the complete pull diagnostics, and then use `previousResultId` to query the cache report.
5. Burst 8 incremental edits, followed immediately by requesting completion, and measure response time under bursts of input.

Renaming only measures the time it takes to generate workspace edits, renaming is not applied. Each response is checked for content, including expected completion candidates, precise definition locations, the complete set of files and number of entries referenced and renamed across chapters, main document parameter changes, and the appearance/disappearance of errors and diagnostic cache hits. CI runs small's short session verification protocol and scripts without setting a time-consuming threshold that is affected by machine load.

`--hook-labels` Adds explicit `contribution` code labels to each section and adds interference tags for another hook and completion fields with full suffixes. Each cycle additionally verifies the precise definition position of all code blocks, all references registered and used, hover and complete suffix completion filtered by hook; the three levels have 17/129/513 definitions and 18/133/521 references respectively. Can be combined with other hook options. `--query hook-definition` of the paired script automatically enables this load, alternately measuring the same code label query, and comparing the defined responses item by item; `--query definition --hook-labels` is used to observe the indexing cost of ordinary queries within the same project.

```sh
python3 benchmarks/editing.py --hook-labels --output target/benchmarks/hook-labels.json
python3 benchmarks/rename_pairs.py --before target/benchmarks/before --after target/benchmarks/after \
  --query hook-definition --output target/benchmarks/hook-label-pairs.json
```

`--lifecycle-labels` implies `--hook-labels`, changes the registration of each section to `AtBeginDocument[contribution]`, mixes a `AddToHook{begindocument}[contribution]` in the main document, and adds the `AtEndDocument[contrast]` interference in the end stage. The definition/reference is calculated independently according to the `begindocument` combination identity, and the hook name is also checked on hover; the completion must exclude the end stage and the same prefix tags of other hooks. All 17/129/513 code blocks and 18/133/521 references in the three project sizes are still verified one by one. Both scripts record `lifecycle_labels`, and the comparison requires the options to be consistent.

```sh
python3 benchmarks/editing.py --lifecycle-labels --output target/benchmarks/lifecycle-labels.json
python3 benchmarks/rename_pairs.py --before target/benchmarks/before --after target/benchmarks/after \
  --query hook-definition --lifecycle-labels --output target/benchmarks/lifecycle-pairs.json
```

`--global-hook-rules` implies `--hook-labels`. Each section adds the registration of other hooks with the same name. Each chapter adds global default rules and complete suffix completion detection. Can be combined with `--lifecycle-labels`; full session verifies both local and global definitions/references, hover scope, and exact completion. Global completion must include cross-hook `contrast` and `contribution`. Local completion still only includes the registration of its own hook. The global definition of the three project sizes is 33/257/1025 places, and the global reference is 35/265/1041 places; the local definition remains at 17/129/513 places, and the local reference is increased to 19/137/529 places. `--query global-hook-definition` of the paired script automatically enables this load and verifies all global definitions one by one, `--query hook-definition` continues to verify only the definition of the current hook, and `--query hook-name-definition` verifies the declaration of the independent hook name as a control without code label indexing.

```sh
python3 benchmarks/editing.py --global-hook-rules --lifecycle-labels \
  --output target/benchmarks/global-hook-rules.json
python3 benchmarks/rename_pairs.py --before target/benchmarks/before --after target/benchmarks/after \
  --query global-hook-definition --lifecycle-labels --output target/benchmarks/global-hook-pairs.json
```

`--default-hook-labels` implies `--hook-labels`, adds Push/Set/omitted tag registration/Pop in each section, and adds the definition, reference, hover and complete suffix completion query of the default tag setting at the end of the chapter. The definition only counts currently authenticated explicit registrations, omitting registrations does not count towards the expected results. The definition and completion of setting fields span hooks, and local definitions are still filtered by their own hooks; both types of references contain corresponding setting fields. When combined with `--global-hook-rules --lifecycle-labels`, the three gears are locally defined/referenced as 17/36, 129/269, 513/1049, and the global and default settings are 33/52, 257/397, 1025/1561. The pairing script's `--query default-hook-definition` automatically enables new payloads and verifies all explicit definitions in turn; other options can still be combined independently.

```sh
python3 benchmarks/editing.py --default-hook-labels --global-hook-rules --lifecycle-labels \
  --output target/benchmarks/default-hook-labels.json
python3 benchmarks/rename_pairs.py --before target/benchmarks/before --after target/benchmarks/after \
  --query default-hook-definition --global-hook-rules --lifecycle-labels \
  --output target/benchmarks/default-hook-pairs.json
```

`--document-hook-slots` implies `--hook-labels`, add explicit `top-level` registration in the main document/each section of the current hook, put another hook with the same name to interfere, and add and remove the suffix completion field with `toFAKE` in each chapter. Only all definitions/references of this hook, document-level slot hovers, and complete suffix substitutions are verified item by item. Sorting rules or default settings are not incorporated into the slot. Level three is 17/129/513 definitions and 18/133/521 references; can be combined with lifecycle, global rules, and default settings loads. `--query document-hook-definition` automatically enables new loads and verifies definitions one after another.

```sh
python3 benchmarks/editing.py --document-hook-slots --lifecycle-labels \
  --output target/benchmarks/document-hook-slots.json
python3 benchmarks/rename_pairs.py --before target/benchmarks/before --after target/benchmarks/after \
  --query document-hook-definition --lifecycle-labels \
  --output target/benchmarks/document-hook-pairs.json
```

`--relative-hook-labels` implies `--hook-labels`, the main document explicitly registers `relative/tail`, each section adds the relative `./tail` registration and Pop next to `PushDefaultHookLabel{relative}`, another hook puts the same name interference, and each chapter adds relative removal and `./taFAKE` completion fields. Verify the complete definition/reference corresponding to the parsed identity, the original relative field scope, the hook scope, and the complete suffix replacement that preserves the relative spelling one by one. The third level has 17/129/513 definitions and 18/133/521 references, which can be combined with all existing loads. `--query relative-hook-definition` automatically enables this load.

```sh
python3 benchmarks/editing.py --relative-hook-labels --lifecycle-labels \
  --output target/benchmarks/relative-hook-labels.json
python3 benchmarks/rename_pairs.py --before target/benchmarks/before --after target/benchmarks/after \
  --query relative-hook-definition --lifecycle-labels \
  --output target/benchmarks/relative-hook-pairs.json
```

## Indicator meaning

- `initialize`: Initialize request to response, reported separately, not mixed with functional samples after warm-up; `open` includes opening two buffers to ordered barrier response.
- `edit.*`: Send a single edit notification and return it to the subsequent `latex.indexingStatus` request. This lightweight, ordered request is a notification processing barrier; the time consumption includes text update, parsing, dependency refresh, transmission and the barrier itself, not pure parsing time.
- Other features: After the pre-order editing is completed, the wall clock time from a single request to the response, including stdio, JSON encoding and decoding, queuing and function calculations; measured in session order, previous queries may warm up subsequent queries.
- `semantic.full` queries after text modification; `semantic.delta.cached` requests delta when there are no subsequent modifications. The two are clearly distinguished.
- `diagnostic` is recalculated after a new reference is edited; `diagnostic.cached` must return `unchanged`.
- `burst8.to_completion`: The processing time from the first edit notification to completion return, including 8 edits; this is the end-to-end delay in the backlog scenario.

The output JSON saves all raw millisecond samples, p50, p95 (nearest rank) and maximum values, as well as project bytes, project and script fingerprints, binary fingerprints, platform and Python version. The warmed up samples of each process are combined into statistics; one sample is initialized and opened for each process.

This test turns off distro discovery and uses pull diagnostics to avoid the TeX installation on the machine and push diagnostic anti-shake effect comparison. It measures language functionality; PDF compilation, external formatters, ChkTeX, and disk saving are not part of this load. A reproducible synthetic multi-file paper is used here, which does not represent all real user projects, especially does not cover complex macro expansion, extremely large single lines, and large number of conditional loads.

`--hooks` Add a kernel hook with parameters to the main document and use it in the body. An additional measurement of `signature.hook` is performed every cycle to verify that the declared 1 parameter covers the 0 written at the call; other references, renaming and editing verification remain intact. There is no hook call for chapters, which is used to observe whether project-level parameter analysis adds repeated scanning to irrelevant chapters. Output parameter record `hooks`, the same options must be used for comparison before and after.

```sh
python3 benchmarks/editing.py --hooks --output target/benchmarks/hooks.json
```

`--hook-wrappers` contains the above hook payload, and defines two layers of parameterized packaging macros in the main document, adding a packaging call to each section. It measures the full parsing cost when passing call arguments as hook literals; cross-section references and renames of independent tags are still subject to quantity and file collection verification. The output record `hook_wrappers` must also be consistent during comparison.

```sh
python3 benchmarks/editing.py --hook-wrappers --output target/benchmarks/hook-wrappers.json
```

`--hook-environments` contains the hook payload, and adds an xparse environment call for each section. The environment end text passes the start parameters to the hook. Each file defines 16/32/64 environments respectively, and each chapter reuses these definitions to measure the cost of environment parameter binding as the number of definitions and calls increases. Can be combined with `--hook-wrappers`; output record `hook_environments`, the before and after comparison requires the same options.

```sh
python3 benchmarks/editing.py --hook-wrappers --hook-environments \
  --output target/benchmarks/hook-environments.json
```

`--hook-renames` contains the hook payload and adds `rename.hook`. It requests name changes from explicit declarations in the main file, compares the complete name edit range and replacement text in all source code, covering registration, usage, wrapper text and chapter forwarding calls, excluding ordinary missing references. Pair detection uses `--query hook-rename`, also checking the full edit set.

`--hook-interfaces` contains the hook payload. The zero-parameter environment forwarder `benchinterface` is defined in the main file. Each section directly calls the start control sequence generated by it, and passes the following name and literal data to `UseHook`. It covers the security analysis cost of the environment generation interface and can be combined with the above two packaging payloads; output record `hook_interfaces`.

```sh
python3 benchmarks/editing.py --hook-wrappers --hook-environments --hook-interfaces \
  --output target/benchmarks/hook-interfaces.json
```

## Compare before and after optimization

First keep the pre-optimized binary, then build the optimized version, and measure it in sequence to avoid building or running other benchmarks at the same time:

```sh
cargo build --release --locked
mkdir -p target/benchmarks
cp target/release/texquill target/benchmarks/texquill-before
# Rebuild after changing the source。
cargo build --release --locked
python3 benchmarks/editing.py --server target/benchmarks/texquill-before \
  --output target/benchmarks/before.json
python3 benchmarks/editing.py --baseline target/benchmarks/before.json \
  --output target/benchmarks/after.json
```

The comparison requires that the script fingerprint, number of warming up times, number of sampling times, number of runs and project fingerprints are consistent; use the same machine, the same build parameters, and keep the load stable. The speed multiplier is p50 before optimization / p50 after optimization; less than 1 means slower. Don't take a sub-millisecond difference in one run as evidence of regression.

Generate standalone HTML comparison pages for switchable project sizes and p50/p95:

```sh
python3 benchmarks/report.py target/benchmarks/before.json target/benchmarks/after.json \
  --output target/benchmarks/report.html
```

The page is self-contained and can be opened or shared offline; the complete original sample is retained in JSON.

A quick check can be done using:

```sh
python3 benchmarks/editing.py --scales small --iterations 2 --warmup 1 \
  --runs 1 --output target/benchmarks/smoke.json
```

`--timeout` sets the upper limit of waiting for each request, the default is 120 seconds; service exit, timeout, protocol error and function verification failure will cause the script to exit non-zero.

## 2026-10-26 The fourth round: repeated scanning of hook projects

After completing the static variable parameter hook, measure the regular project and the `--hooks` project respectively. 3 independent processes per stage, each process warms up for 3 cycles, measures for 12 cycles, 36 samples each; build, test, and measure run separately.

The review found that name change protection will first construct a complete reference context for each chapter, and then confirm whether the file contains relevant calls; parameter scanning will also repeatedly detect four hook interfaces for ordinary commands. Now filter the chapters based on possible direct/named/forwarded/copied names and check the parsed leaf commands directly. Analysis budgets, data boundaries, and ambiguity protection remain the same.

Below are the end-to-end milliseconds for the large project. The running order of the two versions of the retest is exchanged, saved separately, and not mixed with the first round of samples; the two sets of speed magnifications are not confidence intervals.

| Load and index | p50 before optimization | p50 after optimization | Speed multiplier | p95 before optimization | p95 after optimization |
| --- | ---: | ---: | ---: | ---: | ---: |
| With hook: cross-file rename | 24.89 | 21.68 | 1.15× | 41.25 | 39.18 |
| With hook: Exchange order retest renaming | 26.37 | 20.85 | 1.27× | 41.82 | 37.32 |
| With hook: tag completion | 30.93 | 28.43 | 1.09× | 48.42 | 52.94 |

The main repeatable benefit is the renaming of large projects with hooks. Tag completion's p50 dropped slightly, but p95 got worse on the first run; there were no consistent broad improvements in general items and other features, and no overall speedup can be claimed on that basis. Added `signature.hook` to verify the real quantity coverage at the same time, which is not only time-consuming to record.

Original samples: [Before normal optimization](../benchmarks/results/2026-10-26/round4-before.json), [After normal optimization](../benchmarks/results/2026-10-26/round4-after.json), [Before hook optimization](../benchmarks/results/2026-10-26/round4-hooks-before.json), [hook After optimization](../benchmarks/results/2026-10-26/round4-hooks-after.json), [Exchange sequence before retest version](../benchmarks/results/2026-10-26/round4-hooks-repeat-before.json), [Exchange sequence after retest version](../benchmarks/results/2026-10-26/round4-hooks-repeat-after.json). Pass to `benchmarks/report.py` to reconstruct a stand-alone HTML comparison page. Results only apply to native synthetic paper loads.

## Optimize content

Navigation, referencing, renaming, hover, and signature help now use the same in-query source graph cache as completion and diagnostics. The same query also reuses up to 16 groups of dependency traversal results. The key includes the workspace, starting document, and root document to avoid repeated file parsing and path checking; it is released when the query is completed, and subsequent edits or disk changes are recalculated.

The non-execution data analysis of tag declarations originally reanalyzed the command execution location of each source code in the project for each consumption document. Chapters without macro declarations will not contribute any data readers. This scan is now skipped directly to reduce repeated work in multi-chapter projects. The semantics of declarations, references, and unsaved content remain the same.

Named reference analysis is also reused by target identification, scope validation, and reference collection in the same request. References, renames, and their prechecks reuse these results through an immutable shared index; these queries and diagnostics also share tag declaration analysis by precise project scope. Completion, definition, hover, and signature help only use the source graph cache to avoid retaining extra indexes for analyzes that are only used once. Each of the two types of symbolic analysis retains a maximum of 4 items, sharing an estimated byte budget of approximately 8 MiB per query; if the limit is exceeded, the results will still be calculated normally without additional caching. The cache key differentiates between workspaces and original query document/project scopes, and document partitioning analysis that provides separate resource context continues to use its own interpretation to avoid confusion of meanings.

## 2026-10-26 First round of measurement results

Environment: Linux x86_64, Intel Xeon Gold 6146, default release configuration (thin LTO). Each stage has 3 independent processes, each process has 3 cycles of warming up and 12 cycles of measurement; 36 samples per function. Both versions were run sequentially on the same machine, with no parallel builds or tests during the measurement period.

The large project contains 10 files and 196343 bytes; the following is the end-to-end p50 in milliseconds:

| Function | Before optimization | After optimization | Speed multiplier |
| --- | ---: | ---: | ---: |
| Tag completion | 48.25 | 22.11 | 2.18× |
| Hover | 67.17 | 15.43 | 4.35× |
| Definition jump | 91.91 | 25.13 | 3.66× |
| Find citations | 118.07 | 36.54 | 3.23× |
| Cross-file rename | 143.10 | 53.43 | 2.68× |
| Recalculate diagnostics | 111.47 | 37.37 | 2.98× |
| 8 consecutive edits to completion | 61.76 | 34.18 | 1.81× |

Optimization focuses on dependency and tag analysis. The medians for single text edits, folds, semantic tokens, and cache diagnostics remain essentially the same; the small changes in these metrics are not enough to account for the performance differences.

The complete indicators, environment and original samples of the three-tier projects are retained: [JSON before optimization](../benchmarks/results/2026-10-26/before.json), [JSON after optimization](../benchmarks/results/2026-10-26/after.json). They can be passed to `benchmarks/report.py` to reconstruct the HTML comparison page. This is a measurement of a single-machine synthetic paper load and cannot be extrapolated to a fixed speedup factor for all projects.

## 2026-10-26 Second round of measurement results

The baseline of this round is the version after the first round of optimization, and the optimized version is the final version for new reference/tag analysis and reuse. The environment, build configuration, and sampling parameters are the same as in the table above; the two versions were measured sequentially, with no parallel builds or tests during the period. The end-to-end latency of the large project is as follows, in milliseconds:

| Function | Baseline p50 | This round p50 | p50 speed multiplication | Baseline p95 | This round p95 |
| --- | ---: | ---: | ---: | ---: | ---: |
| Find References | 36.34 | 22.49 | 1.62× | 44.35 | 29.44 |
| Cross-file rename | 53.71 | 18.55 | 2.90× | 92.41 | 33.60 |
| Recalculate diagnostics | 37.72 | 30.23 | 1.25× | 61.25 | 51.24 |

The renaming p50 of small/medium dropped from 2.62/14.20 ms to 1.17/5.04 ms respectively. The label completion of large is 22.40 → 22.57 ms, the completion of eight consecutive edits is 35.02 → 35.28 ms, and the text editing process is 2.06 → 2.07 ms; these indicators have not been significantly improved in this round. p95 is more susceptible to load, and the full data preserves fluctuations in other metrics without interpreting them as full acceleration.

The new symbol cache is only used for queries with repeated analysis. Expanding the experiment to all queries resulted in an approximately 10% increase in the median number of completion paths, which persisted after reversing the measurement order; re-measurement after reducing the cache range yielded the final result above.

This round also has enhanced validation of defining exact ranges, full number of cross-file references, and rename edits, so script fingerprints are different from the first round. Please pair reports within the same round and do not cross-compare JSON from two rounds. Final original samples: [Baseline JSON of this round](../benchmarks/results/2026-10-26/round2-before.json), [JSON after optimization of this round](../benchmarks/results/2026-10-26/round2-after.json).

```sh
python3 benchmarks/report.py benchmarks/results/2026-10-26/round2-before.json \
  benchmarks/results/2026-10-26/round2-after.json \
  --output target/benchmarks/round2-report.html
```

The final code passed all tests: 3341 passed, 490 tests that required external tools were skipped; additionally passed format checking, strict Clippy, and a real stdio short session with CI parameters. Regression covers unsaved dependency/macro meaning changes after query, workspace and project scope isolation, nested query exception recovery, and continued calculation when cache capacity is exceeded.


## 2026-10-26 Round 3: Workspace symbol search

The baseline is the release version of `cca0e35`. Exact, all and no matching queries for workspace symbols are added in this round, and the original document outline, editing, completion, diagnostics and other indicators are retained for comparison. Each of the three projects starts 3 independent processes, each process is warmed up for 3 cycles, measured for 8 cycles, and each process contains 24 samples; the script and project fingerprint are consistent. The final pairing tests the optimized version first and then the baseline version. There are no builds, tests, or other benchmarks running simultaneously during the measurement period. The large project is still 10 files, 196343 bytes.

An initial analysis confirmed that each search reconstructed the outline of all documents, distributed the full flat results, and then sorted and intercepted the top 256 items. Bounded outline caches are now shared by source version and candidates are retained with a ranked queue of up to 256 items, avoiding copying all matches and retaining outlines for over-limit documents at the same time. The cache does not save URIs, and the total retention budget is about 4 MiB; it is regenerated when editing, language role changes, and moves, and calculation continues when the limit is exceeded. For details, see [Indexing and Caching](indexing.md).

End-to-end latency of large, in milliseconds:

| Function | Baseline p50 | This round p50 | p50 speed multiplication | Baseline p95 | This round p95 |
| --- | ---: | ---: | ---: | ---: | ---: |
| Exact search | 1.996 | 0.585 | 3.41× | 3.289 | 0.947 |
| Search all | 4.116 | 2.579 | 1.60× | 6.111 | 3.268 |
| No matching search | 1.758 | 0.404 | 4.36× | 2.555 | 0.510 |
| Document Outline (Contrast) | 2.811 | 2.751 | 1.02× | 3.350 | 3.847 |

The precise search p50 of small/medium is accelerated by 1.57×/2.12× respectively, and the unmatched search is accelerated by 1.74×/2.81×. The cache mainly reduces duplicate symbol analysis and does not improve other language features. The 8 consecutive edits to completion for large are 36.534 → 36.553 ms, tag completion is 22.547 → 23.570 ms, and diagnostics is 30.859 → 32.491 ms; retaining these fluctuations and not interpreting this round as a general acceleration, p95 is also affected by machine load.

Initially, the cache was also created for the first query of the hierarchical outline, which resulted in an increase of about 10% of the outline time. Eventually, the new tree was returned directly for the first time, and only the existing cache was reused. After the measurement order was reversed, the outline p50 returned to near the baseline.

Complete samples and environments: [Baseline JSON for this round](../benchmarks/results/2026-10-26/round3-before.json), JSON after optimization for this round](../benchmarks/results/2026-10-26/round3-after.json). The new indicator changes the script fingerprint, please only match within this round.

```sh
python3 benchmarks/report.py benchmarks/results/2026-10-26/round3-before.json \
  benchmarks/results/2026-10-26/round3-after.json \
  --output target/benchmarks/round3-report.html
```

In this round of consolidation, 3344 items passed, and 490 external tool tests were skipped by default; after the final cache scope was narrowed, 26 items of navigation/document symbols/language roles returned, strict full-target Clippy, release build, and three levels of real stdio benchmark were passed again. Regression covers unsaved updates, old snapshots, UTF-16, containers, sorting and truncation of over 300 candidates, language switching, and complete results after cache overrun.

## 2026-10-26 Round 5: Data flow protection of wrapped macros

After `17c9233` completes the hook data protection of command packaging parameters, a new `--hook-wrappers` load is added: each section calls two layers of packaging macros and passes the actual parameters into the hook. The review found that the protection only requires the literal range and declaration position, but constructs a reference context with complete operand analysis; each round of data flow propagation also performs name and context checks for unrelated commands. Now reuse the smaller source context and filter possible data reader names first; static `csuse`/`csname`, analysis budget and data boundary preservation.

Continuous editing sessions still run in three gears, 3 new sessions x 12 measurement cycles, and 3 warm-up cycles. The wall clock showed large fluctuations: the first large-scale packaging project renamed p50 was 36.30→38.40 ms (0.95×), and the independent retest after the order was swapped was 37.51→30.37 ms (1.24×). The early intermediate version that only filtered names also experienced slowdowns in multiple unchanged features at the same time, so a single group of sessions or name filtering alone was not used as a basis for stable revenue. All original samples are retained.

In order to further compare the rename calculation, `benchmarks/rename_pairs.py` is added: the two versions of the server read the same fixed project snapshot, send the same rename request each time, and exchange the order pair by pair; 3 groups of new processes, each group warms up 5 pairs and measures 20 pairs, for a total of 60 pairs. Under Linux, bind the same allowed CPU before starting the server, and verify the affinity of all threads after opening the file to ensure that Tokio worker threads are also bound. This probe does not include editing and other features and is not a replacement for a continuous session; it records both the wall clock raw sample and the server `/proc` CPU delta. The CPU single sample resolution is 10 ms, and the table is the average of 60 samples, especially if it is not claimed to be optimized based on a small number of CPU ticks for small projects.

Each line below is arranged according to "original protection implementation → after optimization"; the reverse retest JSON exchanges the two versions of `before`/`after` parameters.

| Fixed snapshot | Renamed p50 before/after ms | p50 multiplier | Server CPU average before/after ms |
| --- | ---: | ---: | ---: |
| small packaging | 2.44／2.00 | 1.22× | 2.33／2.33 |
| medium packaging | 11.19／8.51 | 1.31× | 11.50／8.50 |
| large packaging | 33.15／25.95 | 1.28× | 34.17／26.50 |
| large wrapper, swap binary order | 38.19/30.45 | 1.25× | 39.33/30.33 |
| large regular | 23.51／22.59 | 1.04× | 24.33／23.50 |

The median wall clock magnification of the two sets of large packages is about 1.26×, 1.25×, and the average server CPU decreases by about 22%–23%. Conclusions are limited to the cost of renaming this data protection; regular load changes are small, and no overall acceleration of other functions is claimed.

The original data is located at `benchmarks/results/2026-10-26/round5*.json`. `guard` is the earliest baseline, `filter` is the intermediate version that only filters the name; `before`/`after` and `wrappers` save the final continuous session, `wrappers-repeat` saves the reverse session, and `pairs` saves the formal pairing detection. `pairs-*-main-thread` and `pairs-main-thread` are early detections that are only bound to the main thread after startup. The Tokio worker thread is not verified. They are reserved separately but are not used for the above same-core conclusion.

```sh
PYTHONDONTWRITEBYTECODE=1 python3 benchmarks/rename_pairs.py \
  --before target/benchmarks/texquill-round5-guard \
  --after target/benchmarks/texquill-round5-final --hook-wrappers \
  --output target/benchmarks/pairs.json
python3 benchmarks/report.py \
  benchmarks/results/2026-10-26/round5-wrappers-before.json \
  benchmarks/results/2026-10-26/round5-wrappers-after.json \
  --paired benchmarks/results/2026-10-26/round5-pairs.json \
  --output target/benchmarks/round5-wrappers-report.html
```

The stable consolidation baseline achieved by the protection was 319 targets, 3359 passes, and 0 failures. After optimization, it once again passed 521 units and related regressions, 8 hook protocols, 6 explicit native tests and full-target strict Clippy; native data protection covers 21 direct/wrapped/named calls, verifying the complete AUX after capturing text, macro execution times and actual independent label editing. CI joins the short test of packaging sessions and pairing detection, with no machine-related performance thresholds. The dev cleanup freed 9.1 GiB, leaving approximately 20 GiB free at the end of the measurement.

## 2026-10-26 Round 6: Environment load and hook index for irrelevant queries

The baseline is `0f26cef`, and the hook data in environment packaging and extra parameter forwarding has been protected. Added `--hook-environments`, measured in combination with `--hook-wrappers`: large contains 10 files, 240046 bytes, 64 xparse environments and 512 environment calls. End the text and pass the start parameters to the hook. All edits and file collections of independent tags are still verified.

Initially attempts to index body-owned definitions in Data Flow Analysis by document. The renamed small/medium/large p50 with homonuclear pairing has a magnification ratio of approximately 0.99×/0.96×/0.99×. It does not show stable returns and is therefore withdrawn; `round6-owner-index-pairs.json` retains this intermediate experiment. Later, I discovered that ordinary tag navigation would also create a hook name index for all source codes. Now parse the document where the cursor is located. If there is no target hook name under the consumer's interpretation, this analysis will end directly; the real hook name query continues to scan the complete project and retains the consistency check of all shared consumers. No cross-request caching is added.

Continuous sessions are 3 independent processes per gear, each process has 3 cycles of warm-up and 12 cycles of measurement. The definition p50 of large combined load is 79.95→76.46 ms, hovering 51.00→45.36 ms, renamed to 47.85→48.81 ms; conventional load is defined as 26.77→25.89 ms, renamed 19.40→20.30 ms. There are still noticeable fluctuations in sessions, don't interpret these changes as a general speedup across all functions.

`rename_pairs.py` Added `--query definition` to check the exact declaration position, and continue to use same-core binding before startup, all thread affinity checks and alternate requests after startup. Each level has 3 sets of processes, 5 pairs of warming up and 20 pairs of measurements. The following table is always arranged according to "Original → Optimized Version", and the `before`/`after` of the reverse retest file has the binary exchanged.

| fixed snapshot | define p50 before/after ms | p50 multiplier | CPU mean before/after ms |
| --- | ---: | ---: | ---: |
| small combination | 4.51／4.34 | 1.04× | 4.83／4.67 |
| medium combination | 20.03／18.32 | 1.10× | 21.33／18.50 |
| large combination | 77.31／69.55 | 1.11× | 77.83／70.83 |
| large combination, exchange order | 77.40／69.21 | 1.12× | 77.00／70.50 |

The average CPU drop for both sets of large probes is about 8%–9%; the CPU tick is 10 ms, and benefits cannot be claimed based on a small number of ticks for a small project. Conclusions are limited to avoiding the navigation costs of irrelevant hook indexes; paired probes do not have continuous editing and cannot replace a full session. No builds, tests, or browser rendering were run in parallel during the measurement.

All raw data is `benchmarks/results/2026-10-26/round6*.json`, totaling about 290 KiB: `environments-before/after` and `regular-before/after` are continuous sessions, `definition-pairs` and its `reversed` are official pairs, and `owner-index-pairs` is a withdrawn experiment.

```sh
PYTHONDONTWRITEBYTECODE=1 python3 benchmarks/rename_pairs.py \
  --before target/benchmarks/texquill-round6-guard \
  --after target/benchmarks/texquill-round6-origin-first \
  --query definition --hook-wrappers --hook-environments \
  --output target/benchmarks/definition-pairs.json
python3 benchmarks/report.py \
  benchmarks/results/2026-10-26/round6-environments-before.json \
  benchmarks/results/2026-10-26/round6-environments-after.json \
  --paired benchmarks/results/2026-10-26/round6-definition-pairs.json \
  --output target/benchmarks/round6-environments-report.html
```

Smoke passes for both pairing queries and environment combination sessions, and CI is covered synchronously. 44 hook-related tests, 8 explicit native tests, all-target strict Clippy, documentation, format and difference checks passed. The total warehouse return after freezing the source code was 320 targets, 3365 passed, 0 failed, and 494 were skipped by default, of which the complete serial protocol was 406 passed and 29 were skipped by default. The independent HTML pairing table and item/quantile/indicator filtering interaction, all original sample numbers and numerical checks passed; the preview service still returned `Headless Chrome could not render the page: the browser exited unexpectedly.`, which was not counted as a visual verification pass.

## 2026-10-26 Round 7: Environmental Control Interface and No Picture Project

The baseline is `b41daa5`, and the data when the start/end control sequence generated by the protected environment is directly forwarded to the hook. Added `--hook-interfaces`, each section directly calls the hook through the control sequence generated by the environment; it can be combined with `--hook-wrappers --hook-environments`, and the CI session is covered simultaneously with the two pairing queries. Measurements continue to verify the complete edited set of individual tags and their precise declared location.

Try to skip the unused hook prompt index of the security check, and filter the candidate field where the cursor is located before the hook name index. Large-scale same-core pairings only yielded about 1%–3%, and have been withdrawn; `round7-without-hint-index-pairs.json` and `round7-definition-pairs.json` are retained and are not counted as final optimizations.

Temporary staged diagnostics show that document link analysis in large definition requests takes approximately 21–22 ms, even if the project does not have any images, building full-project image signatures and operand analysis. Formal optimization first checks all bounded LaTeX sources, including recorder-only files: if the source code does not contain `includegraphics`, and there is no `csuse`/`csname`/`endcsname`, then directly retain the empty call list of each document. Static alias chains and command replication must have final built-in names, and named constructs always fall back to full analysis; query files cannot be checked only, and cross-request caching is not added. Projects containing picture words or naming structures will continue to undergo original analysis, so the income in this round is limited to projects that meet this condition. The official binary has all diagnostic timing removed.

The same core alternating definition detects 3 groups of processes in each stage, warms up 5 pairs, and measures 20 pairs; binds the CPU before starting, and verifies all threads after opening. The following tables are all arranged by "Baseline → After Optimization"; the reverse sample file has the binary parameters exchanged.

| fixed snapshot | define p50 before/after ms | p50 multiplier | CPU mean before/after ms |
| --- | ---: | ---: | ---: |
| small combination | 4.70／3.78 | 1.24× | 4.67／3.67 |
| medium combination | 20.65／15.83 | 1.30× | 20.17／15.67 |
| large combination | 78.08／59.41 | 1.31× | 77.67／58.83 |
| large combination, exchange order | 78.02／60.03 | 1.30× | 77.83／59.67 |

The average CPU value of the two large groups dropped by about 23%–24%; the single-sample tick is still 10 ms, and the benefits are not extrapolated from the small number of ticks for small projects. There are no parallel builds, tests, or browser rendering during measurements.

The continuous session is still a three-level project, with 3 independent processes per level, 3 cycles of warm-up, and 12 cycles of measurement. The large combination is 10 files and 261082 bytes: p50 is defined as 80.36→62.34 ms, hovering is 48.23→32.66 ms, editing to completion 8 times continuously is 86.57→69.40 ms; renaming is 52.50→55.08 ms, and the hook signature help is 22.39→23.21 ms. Regular large items are defined as 27.64→19.95 ms, hover 16.94→10.23 ms, rename 19.75→18.86 ms. The renaming and hook signature helping algorithms have not been changed in this round. The complete session still fluctuates, and we cannot claim that all operations are accelerated.

The original data is `benchmarks/results/2026-10-26/round7*.json`: `interfaces-before/after` and `regular-before/after` save continuous sessions, `no-images-definition-pairs` and `reversed` save formal pairing; the other two pairing files are withdrawn attempts; 8 JSONs total 317212 bytes, 10560 The number, finite value and median verification of the original samples of the wall clock passed. The HTML pairing table, item/quantile/indicator filter has been verified using JS VM; the preview still returns `Headless Chrome could not render the page: the browser exited unexpectedly.`, which is not counted as a visual verification pass.

```sh
PYTHONDONTWRITEBYTECODE=1 python3 benchmarks/rename_pairs.py \
  --before target/benchmarks/texquill-round7-guard \
  --after target/benchmarks/texquill-round7-no-images \
  --query definition --hook-wrappers --hook-environments --hook-interfaces \
  --output target/benchmarks/definition-pairs.json
python3 benchmarks/report.py \
  benchmarks/results/2026-10-26/round7-interfaces-before.json \
  benchmarks/results/2026-10-26/round7-interfaces-after.json \
  --paired benchmarks/results/2026-10-26/round7-no-images-definition-pairs.json \
  --output target/benchmarks/round7-interfaces-report.html
```

The full regression after freezing Rust was 320 targets, 3370 passes, 0 failures, 495 default skips, including 407 passes, 29 default skips for the full serial protocol. Added regression verification for unsaved files from no image definition to new alias, deletion, and recovery. Image navigation and file rename are refreshed simultaneously; cross-file, naming, recorder-only, shared consumer, and incomplete source regressions are passed. 12 explicit native tests, full target rigorous Clippy, documentation, formatting and diff checks passed. The dev product is cleaned after validation, retaining the release baseline and all raw measurements.

## 2026-10-26 Round 8: Custom hook name rename

The functional baseline is `78b9374`, which supports complete custom hook name editing, independent checking of shared consumers, dynamic calls, sources and reader identities. Added `--hook-renames` and pairing `--query hook-rename`; master file query explicit declaration, comparing all name-edited files, UTF-16 ranges, and replacement text on an item-by-item basis. The combined load enables command packaging, environment packaging and environment generation interfaces at the same time, and the three levels check 36/164/580 name edits respectively.

Measurement found that the full rename check duplicated the construction of `named_references::Context`, again analyzing the operands, expl3 data and body. Name indexing already excludes operands in each consumer interpretation of the same item; verification still requires an exact match of the full index entry. Therefore, the verification uses the smaller `LiteralContext`, retaining the literal area, dynamic calls, interface identities, related modifications, sources and conflict checks, and reusing the operand exclusion results of existing entries. There is no new cache or cross-request state.

Formal pairing uses three groups of new processes, with each group warming up 5 pairs and measuring 40 pairs; the same core is bound before starting, and the CPU affinity of all threads is verified after opening the project. The two versions are requested alternately, and all editors are fully verified. CPU is the average of 120 requests per version, with a single-sample timing granularity of 10 ms.

| Portfolio project | hook renamed p50: original → optimized version | p50 acceleration | CPU average: original → optimized version |
| --- | ---: | ---: | ---: |
| small | 2.906 → 2.516 ms | 1.15× | 2.750 → 2.417 ms |
| medium | 13.577 → 10.952 ms | 1.24× | 13.000 → 9.917 ms |
| large | 52.221 → 41.304 ms | 1.26× | 49.333 → 38.750 ms |
| large, after exchanging binary parameters, convert according to the original → optimized version | 51.940 → 41.003 ms | 1.27× | 49.667 → 38.833 ms |

The average CPU drop for both large pairings is about 21%–22%. The full editing session uses three new groups of processes, each group warms up for 3 cycles and measures 20 cycles; the hook name renamed small/medium/large p50 is reduced from 2.953/13.105/55.001 to 2.552/10.844/43.607 ms respectively. The large common tag is renamed to 59.625 → 59.854 ms, and the hook signature help is 23.811 → 25.906 ms; it is not claimed that all functions are accelerated based on this. The session also verifies the unsaved main document, Chinese/emoji incremental editing, diagnostic repair, and other functional results.

The review results of the 11 real release edits of the two versions are consistent: 5 sets of complete edits are equal to manually marked correct edits item by item, and events, capture data and AUX are retained; 6 sets of dynamic or incomplete scenes are rejected for editing. Added regression to catch hook tokens: naive name substitution changes the data text, but the execution count and AUX remain the same, and the index entry exclusion check continues to reject such edits.

The original report totaled 250657 bytes (approximately 245 KiB), and the number of 9276 wall clock samples, p50, p95, maximum and paired CPU averages have been recalculated:

- [Combo Session: Original](../benchmarks/results/2026-10-26/round8-hook-renames-before.json)
- [Combined Session: Optimized Version](../benchmarks/results/2026-10-26/round8-hook-renames-after.json)
- [Same core pairing: third level](../benchmarks/results/2026-10-26/round8-hook-rename-pairs.json)
- [Same core pairing: exchange binary parameters](../benchmarks/results/2026-10-26/round8-hook-rename-pairs-reversed.json)

```sh
python3 benchmarks/rename_pairs.py \
  --before target/benchmarks/texquill-round8-guard \
  --after target/benchmarks/texquill-round8-index-proof \
  --query hook-rename --hook-wrappers --hook-environments --hook-interfaces \
  --iterations 40 --warmup 5 --runs 3 --output target/benchmarks/round8-hook-rename-pairs.json
```

New indicators, three paired queries, report interactions, and two versions of release passed native review. The final verification of the entire frozen source code was 321 targets, 3377 passed, 0 failed, and 497 were skipped by default. The complete serial protocol was 408 passed and 29 were skipped by default. 64 special hook items (including native), strict full-target Clippy, document, format and difference checks passed. 478 copies of the Rust source code are consistent with the checksum when frozen, and the SHA of the two versions of the measured binary is consistent with the report. The HTML interactive script check passed; both headless previews failed due to abnormal browser exit, regardless of visual verification.

## 2026-10-26 The ninth round of explicit code label and field candidate screening

The functional baseline is `513ab8a`, and the definition, reference, hover and completion with the identity of `(hook, code label)` are added. The load also enables `--hook-labels --hook-wrappers --hook-environments --hook-interfaces --hook-renames`, retaining all edit verifications for both types of name changes. The three project sizes has 17/129/513 code block definitions and 18/133/521 references; the code label responds to the precise range of item-by-item verification, complete verification hook isolation and replacement of the entire suffix.

Source code review found that the hook name and code label index will first construct the complete operand context for the cursor document, and then determine whether the cursor is located in the relevant field. The optimization first parses the candidate fields; returns early when the cursor is not hit, only constructs the context for documents with candidates, and calls the check operand according to the candidate. Full call protection of literal areas, declaration locations, shared consumer consistency, source budgets, and renames are preserved, and no new caches are added. Existing data arguments, unsaved provision files, and shared consumer regressions are used to check for changes in filter order.

Formal pairing starts two resident release processes on CPU 0. After opening, the affinity of all server threads is verified; the request order and the start order of each group of processes are alternated. There are 3 groups of processes in each gear, each group warms up 5 pairs, measures 30 pairs, and then exchanges `--before`/`--after` for complete retest. The following table is uniformly listed in the order of functional baseline → optimized version, and the reverse report itself retains the actual parameter order.

| Query/Project | First group p50 · ms | After exchange p50 · ms | Two sets of magnifications |
| --- | ---: | ---: | ---: |
| Common definition · small | 4.562 → 3.919 | 4.572 → 3.925 | 1.16×／1.16× |
| Common definition · medium | 18.854 → 17.327 | 18.679 → 17.090 | 1.09×／1.09× |
| Common definition · large | 69.861 → 66.269 | 70.783 → 67.401 | 1.05×／1.05× |
| Code label definition · small | 2.796 → 2.432 | 2.686 → 2.364 | 1.15×／1.14× |
| Code label definition · medium | 11.294 → 10.609 | 11.508 → 10.570 | 1.06×／1.09× |
| Code label definition · large | 42.971 → 41.159 | 43.257 → 41.398 | 1.04×／1.04× |

The average server CPU values of large generally defined are 70.222 → 66.667, 70.444 → 66.889 ms, both decreased by approximately 5.1%; the code label definitions are 41.222 → 39.778, 42.778 → 39.889 ms, decreased by approximately 3.5%/6.8%. The single-sample accuracy of the CPU is 10 ms. It only compares the mean of 90 samples in each level and does not draw conclusions based on a single sample.

A full continuous session is 3 new sessions per run, each with 3 cycles of warm-up and 20 cycles of measurement. The results for large are as follows:

| Operation | Before optimization p50 · ms | After optimization p50 · ms |
| --- | ---: | ---: |
| Code label definition | 43.941 | 41.660 |
| Code Tag Reference | 45.412 | 42.848 |
| Code label hover | 44.771 | 39.019 |
| Code tag completion | 46.763 | 40.795 |
| Common definition | 72.854 | 73.899 |
| Ordinary name change | 63.614 | 61.826 |
| hook name renamed | 55.230 | 54.016 |
| hook signature help | 25.365 | 25.243 |
| 8 consecutive edits to completion | 81.895 | 75.165 |

Paired queries confirm small, stable gains; there are still fluctuations in consecutive sessions, and ordinary definitions are slightly slower in this full replay, which cannot be used to claim that all functions are accelerated. Tested using synthetic papers, turning off release discovery and using pull diagnostics, does not include PDF compilation or external formatting.

The six official original reports total 350040 bytes (approximately 342 KiB), and the statistics, CPU averages, fixtures, scripts and binary SHA of 11916 wall clock samples have been recalculated:

- [Full session before optimization](../benchmarks/results/2026-10-26/round9-code-labels-before.json), [After optimization](../benchmarks/results/2026-10-26/round9-code-labels-after.json)
- [Normal definition pairing](../benchmarks/results/2026-10-26/round9-field-candidates-definition-pairs.json), [Exchange order](../benchmarks/results/2026-10-26/round9-field-candidates-definition-pairs-reversed.json)
- [Code Tag Definition Pairing](../benchmarks/results/2026-10-26/round9-field-candidates-code-label-pairs.json), [Exchange Order](../benchmarks/results/2026-10-26/round9-field-candidates-code-label-pairs-reversed.json)

```sh
python3 benchmarks/editing.py --server target/benchmarks/texquill-round9-labels \
  --hook-labels --hook-wrappers --hook-environments --hook-interfaces --hook-renames \
  --runs 3 --iterations 20 --warmup 3 --output target/benchmarks/round9-before.json
python3 benchmarks/rename_pairs.py \
  --before target/benchmarks/texquill-round9-labels \
  --after target/benchmarks/texquill-round9-field-candidates \
  --query hook-definition --hook-labels --hook-wrappers --hook-environments --hook-interfaces \
  --runs 3 --iterations 30 --warmup 5 --output target/benchmarks/round9-pairs.json
```

Both the functional version and the optimized version have passed the four navigation and native editing reviews of the actual release: direct, static naming, pure forwarding and precise definition/reference/hover of boundary comments are consistent; the actual editing application of independent ordinary tags maintains the code event sequence and complete AUX. The strict full-target Clippy has been passed, and the optimized version’s final frozen full-scale return is in progress.

The JS VM verification of the interactive report has passed, including three-speed switching, p50/p95, all indicators and matching tables. `html_preview` still returns `Headless Chrome could not render the page: the browser exited unexpectedly.`; the preview is not marked as a visual verification pass.

The optimized version of the final freeze verification passed: 322 targets, 3385 passes, 0 failures, 499 default skips, of which the full serial stdio is 409 passes, 29 default skips. 73 projects including native hooks, strict full-target Clippy, documentation, format and difference checks passed; 481 Rust source code collections and verification values were consistent. Normal/hook session smoke and four paired query modes passed; six official JSONs and two versions of release SHA were verified again to be consistent.

## 2026-10-26 Round 10: Lifecycle labels and withdrawn commands pre-screening

This round adds `--lifecycle-labels`, changes the section-by-section registration to `AtBeginDocument`, and mixes the interference of direct `AddToHook{begindocument}`, the end stage and other hooks. Each query checks for complete definitions/references and exact completion ranges. The three load levels are 3/6/10 files and 16400/80871/290107 bytes respectively; the code label is defined at 17/129/513 and the reference is 18/133/521.

The functional baseline is from `b244dcd`, saved as `target/benchmarks/texquill-round10-lifecycle`, and SHA-256 is `a4daa8704e0585d1cdc543b5c7df1afb274788d02d4b6cb5a43408704114bf0a`. A complete session starts 3 processes at each stage, each with 3 cycles of warm-up and 20 cycles of measurement:

| Operation p50 · ms | small | medium | large |
| --- | ---: | ---: | ---: |
| Common definition | 3.943 | 17.414 | 68.709 |
| Lifecycle code label definition | 2.429 | 10.562 | 41.865 |
| Lifecycle Code Label Reference | 2.516 | 10.771 | 41.736 |
| Lifecycle Code Label Hover | 2.346 | 9.791 | 37.820 |
| Lifecycle code label completion | 2.694 | 10.794 | 40.196 |
| Custom hook name renamed | 2.766 | 11.860 | 46.330 |
| 8 consecutive edits to completion | 9.618 | 27.542 | 72.698 |

The point to be optimized is that the hook field reader repeatedly checks the literal area and forwarding identity of unrelated commands. Three candidates were tested: pre-collecting confirmed interface aliases from the current signature; excluding impossible built-in names on this basis; and removing the alias set and only moving forward the existing forwarding judgment. Subsequent named calls, current kernel leaf interfaces, data roles, and shared consumer checks are all preserved. The first version passed 80 special projects with native hooks and strict all-target Clippy, but the performance probe did not show any benefit.

The detection of large-scale projects is requested alternately in two resident processes of the same core, with 2 rounds, 4 warm-ups and 15 measurements per round. There are 30 samples per query per version; the following are probe results, not full dual-sequential performance certification:

| candidate | query | baseline p50 → candidate p50 · ms | median paired odds |
| --- | --- | ---: | ---: |
| Pre-collected alias set | Common definition | 65.797 → 67.136 | 0.98× |
| Alias collection plus name pre-filtering | Common definition | 66.934 → 68.001 | 0.98× |
| Alias collection plus name pre-filtering | Code label definition | 40.201 → 40.639 | 0.99× |
| Direct forward forwarding judgment | Ordinary definition | 66.673 → 67.782 | 0.99× |
| Direct forward forwarding judgment | Code label definition | 40.681 → 42.288 | 0.97× |

These small-scale detections are not enough to prove widespread degradation, but they do not meet the criteria to retain optimization; all three candidates have been withdrawn, and the Rust source code is completely consistent with the functional submission that has completed the whole warehouse verification. There is no new cache, nor is there any claim to speed up this round of queries. The next round continues to measure actual functional load, giving priority to duplicate signature constructs rather than adding unproven filtering layers. The CPU clock has an accuracy of 10 ms and does not judge small changes based on individual samples.

The six original reports total 141993 bytes, and the statistics, fixtures, CPU averages and script/binary SHA of 5178 wall clock samples have been recalculated:

- [Full Lifecycle Session](../benchmarks/results/2026-10-26/round10-lifecycle-before.json)
- [Alias Collection: Common Definition](../benchmarks/results/2026-10-26/round10-filter-definition-rejected.json)
- [Name pre-filter: normal definition](../benchmarks/results/2026-10-26/round10-uppercase-definition-rejected.json), [Code label definition](../benchmarks/results/2026-10-26/round10-uppercase-label-rejected.json)
- [Forward judgment: normal definition](../benchmarks/results/2026-10-26/round10-early-meaning-definition-rejected.json), [Code label definition](../benchmarks/results/2026-10-26/round10-early-meaning-label-rejected.json)

```sh
python3 benchmarks/editing.py --server target/benchmarks/texquill-round10-lifecycle \
  --lifecycle-labels --hook-wrappers --hook-environments --hook-interfaces --hook-renames \
  --runs 3 --iterations 20 --warmup 3 --output target/benchmarks/round10-lifecycle.json
```

The functional freeze regression was 322 targets, 3391 passes, 0 failures, 501 default skips, with the full serial stdio at 410 passes, 29 default skips. Passed 80 special projects including native hooks, nine groups of actual release native/accurate navigation reviews, and strict all-target Clippy, document, format, and difference checks. After the measurement, the functional version release has been restored; the source code of failed candidates will not be retained.

## 2026-10-26 Round 11: Global sorting rules and intra-request signature reuse

The functional baseline is `723f611`. Added `--global-hook-rules`, which can be combined with `--lifecycle-labels` to cover the definition, reference, hover and completion of local and global rules. The third gear is locally defined as 17/129/513 places and cited as 19/137/529 places; globally defined as 33/257/1025 places and cited as 35/265/1041 places. Calculate all positions independently, verify that global completion includes `contrast` and `contribution`, and local definition and completion only include this hook. Normal and hook name renames still check for full editing.

Source code review found that the hook name index and code label index in the same request repeatedly constructed the project command signature. Optimized to save owned signatures in the existing request-level cache, distinguished by workspace identity and precise source code scope; each category has a maximum of four items, sharing the 8 MiB metering budget with existing analysis. If the limit is exceeded, it will only be recalculated and will not be retained. The cache is destroyed when the request ends and is not reused across unsaved edits. Definitions, hovers, and completions also go into the existing symbol query cache, so existing named/label analyzes are reused at the same time; the results of this round cannot all be attributed to signature reuse. Literal data, interface meanings, and all shared consumer checks are maintained.

The formal pairing uses two resident release processes of the same core to verify the affinity of all threads and alternate the order of requests. There are three groups of processes in each stage, each of which is warmed up with five pairs, measured with thirty pairs, and then the process startup order is exchanged for complete retesting. The table is uniformly displayed according to functional baseline → optimized version, unit ms:

| Query/Project | First set of p50 | After exchange p50 |
| --- | ---: | ---: |
| Common definition · small | 4.166 → 4.045 | 4.026 → 3.869 |
| Common definition · medium | 18.388 → 18.156 | 18.244 → 18.068 |
| Common definition · large | 72.781 → 72.305 | 73.570 → 72.865 |
| Local label definition · small | 2.488 → 2.372 | 2.510 → 2.362 |
| Local label definition · medium | 11.588 → 11.571 | 11.302 → 11.136 |
| Local label definition · large | 45.007 → 43.635 | 45.609 → 44.099 |
| Global label definition · small | 3.017 → 2.859 | 2.645 → 2.479 |
| Global label definition · medium | 12.438 → 12.054 | 12.192 → 12.083 |
| Global label definition · large | 50.043 → 50.096 | 50.703 → 49.496 |

The p50 of both groups improved by about 3% for large local definition; about 1% for normal definition of large. The large global definition is that one group is flat/slightly slower, the other group improves by about 2.4%, and a stable improvement of 5% cannot be considered. The server CPU averages are 47.667 → 46.889, 47.222 → 45.889 ms respectively, a decrease of approximately 1.6%/2.8%; the CPU single sample accuracy is 10 ms, and only the entire average is compared. The common renaming of a large control group is 64.291 → 64.344, 63.961 → 64.664 ms, and there is no stable benefit; the hook name is defined as 39.252 → 38.574, 39.395 → 38.857 ms, and no obvious new overhead is found.

A complete continuous session has three new processes per session, each with three cycles of warm-up and twenty cycles of measurement. The large p50 is as follows; the session still has timing fluctuations, and does not replace the pairing conclusion, nor does it claim full acceleration:

| Operation | Before optimization p50 · ms | After optimization p50 · ms |
| --- | ---: | ---: |
| Common definition | 77.861 | 75.167 |
| Local label definition | 46.114 | 45.197 |
| Global label definition | 55.005 | 49.577 |
| Global label reference | 54.619 | 51.330 |
| Global Label Hover | 43.381 | 42.294 |
| Global tag completion | 48.821 | 43.494 |
| Ordinary name change | 67.656 | 66.727 |
| Hook name changed | 54.797 | 51.845 |
| hook signature help | 27.634 | 27.325 |
| Eight consecutive edits to completion | 84.101 | 82.091 |

The twelve formal reports total 487,300 bytes, and ten preliminary detections are retained; all twenty-two are 562,096 bytes, 16,236 wall clock samples, and statistics, CPU averages, fixtures, scripts, and two versions of release SHA have been recalculated. Tested using synthetic papers, pull diagnostics, closed release discovery, and does not include PDF compilation or external formatting.

- [Full session before optimization](../benchmarks/results/2026-10-26/round11-global-rules-before.json), [After optimization](../benchmarks/results/2026-10-26/round11-global-rules-after.json)
- [Normal definition pairing](../benchmarks/results/2026-10-26/round11-query-signatures-definition-pairs.json), [Exchange order](../benchmarks/results/2026-10-26/round11-query-signatures-definition-pairs-reversed.json)
- [Partial label definition pairing](../benchmarks/results/2026-10-26/round11-query-signatures-hook-definition-pairs.json), [Exchange order](../benchmarks/results/2026-10-26/round11-query-signatures-hook-definition-pairs-reversed.json)
- [Global label definition pairing](../benchmarks/results/2026-10-26/round11-query-signatures-global-hook-definition-pairs.json), [Exchange order](../benchmarks/results/2026-10-26/round11-query-signatures-global-hook-definition-pairs-reversed.json)
- [Normal name change control](../benchmarks/results/2026-10-26/round11-query-signatures-rename-pairs.json), [Exchange sequence](../benchmarks/results/2026-10-26/round11-query-signatures-rename-pairs-reversed.json)
- [hook name definition control](../benchmarks/results/2026-10-26/round11-query-signatures-hook-name-definition-pairs.json), [exchange sequence](../benchmarks/results/2026-10-26/round11-query-signatures-hook-name-definition-pairs-reversed.json)

```sh
python3 benchmarks/editing.py --server target/benchmarks/texquill-round11-global-rules \
  --global-hook-rules --lifecycle-labels --hook-wrappers --hook-environments \
  --hook-interfaces --hook-renames --runs 3 --iterations 20 --warmup 3 \
  --output target/benchmarks/round11-before.json
python3 benchmarks/rename_pairs.py \
  --before target/benchmarks/texquill-round11-global-rules \
  --after target/benchmarks/texquill-round11-query-signatures \
  --query global-hook-definition --lifecycle-labels --hook-wrappers \
  --hook-environments --hook-interfaces --runs 3 --iterations 30 --warmup 5 \
  --output target/benchmarks/round11-pairs.json
```

The eight sets of global rules and nine sets of life cycle reviews of the actual optimized version of the release have been approved, including precise navigation, completion, common references, actual edited native events/complete AUX, and reserved name rejection. The interaction report has been released, and the JS VM verification covers three levels, p50/p95, all indicators and global pairing labels; `html_preview` returns `Headless Chrome could not render the page: the browser exited unexpectedly.`, excluding visual verification. Old lifecycle reports can also continue to be generated.

Optimized final freeze regression passes: 322 targets, 3401 passes, 0 failures, 504 default skips, including 411 passes, 29 default skips for full serial stdio. 90 special projects with native hooks, strict full-target Clippy, documentation, format and difference checks passed; 481 Rust source code collections and hashes are consistent with the frozen list, and the two versions of release SHA are consistent with the original report. Added ownership, scope/workspace isolation, post-request refresh, nested panic recovery, and cache budget fallback pass. After measurement, clean the dev products of this package and retain the release baseline, dependencies and original samples.

## 2026-10-26 Round 12: Default label settings and cross-analysis signature reuse

The functional baseline is `ee618a4`. The new payload executes Push/Set/omitted tag registration/Pop in each section, and queries the definition, reference, hover and complete suffix completion of the set field; combined with global rules, life cycle, command/environment packaging and environment generation interface. The three project sizes are 3/6/10 files and 20208/110247/405435 bytes respectively. Local definitions/references are 17/36, 129/269, 513/1049, global and default settings are 33/52, 257/397, 1025/1561. All positions are counted independently; only explicit registrations that are currently authenticated are counted, omitted registrations are not counted as supported definitions. Complete edits of common references and hook names continue to be checked item by item.

Source code review confirmed that the hook index has shared the command signature, but the named reference analysis and label declaration data of the same request still prove that the signature is repeatedly constructed in the same scope. The optimization uniformly selects LaTeX source code according to precise scope and URI order, and the three types of analysis share the existing in-request signature cache. Borrowed keys are retained when there is no query cache to avoid forced copying; results retained within the query continue to have all keys. Respective source/command budgets, data roles, and shared consumer checks remain, caches still have a maximum of four items per category, a shared 8 MiB metered budget, and are not reused across requests.

Formal pairing binds two resident release processes to the same CPU and verifies all threads; alternate requests, three groups of processes in each stage, warm up five pairs each and measure thirty pairs, and then exchange the startup sequence for complete retest. Normal definitions/renames query common reference keys within the same composite project, with default settings and hook name definitions as controls. The following table is uniformly listed by functional baseline → optimized version, in ms; the reverse JSON retains its actual parameter order.

| Query/Project | First set of p50 | After exchange p50 |
| --- | ---: | ---: |
| Common definition · small | 4.453 → 4.016 | 4.379 → 4.044 |
| Common definition · medium | 22.040 → 21.413 | 21.764 → 21.183 |
| Common definition · large | 88.565 → 83.916 | 87.936 → 83.216 |
| Default label setting definition · small | 2.832 → 2.817 | 2.724 → 2.708 |
| Default label setting definition · medium | 14.695 → 14.557 | 14.325 → 13.929 |
| Default label setting definition · large | 59.237 → 58.318 | 57.713 → 57.061 |
| Ordinary name change · large | 76.919 → 74.244 | 76.998 → 74.586 |
| hook name definition · large | 47.737 → 46.987 | 47.037 → 46.685 |

The p50 of the two groups of large ordinary definitions improved by about 5.2%/5.4% respectively, and the average server CPU was 87.778 → 83.111, 87.889 → 83.000 ms, a decrease of about 5.3%/5.6%. Large normal renamed p50 improved by about 3.5%/3.1%, with average CPU values of 75.333 → 71.889, 75.000 → 72.667 ms. The changes to default field and hook name definitions are minor and do not extend the benefits of ordinary definitions to all hook queries. The CPU single-sample accuracy is 10 ms, and only the mean of the entire range is compared.

A complete continuous session starts three new processes at each stage, each of which is warmed up for three cycles and measured for twenty cycles. The large p50 is as follows. Global/default definitions, default references, hook name renames, and input bursts still fluctuate, and full acceleration cannot be claimed based on the benefits of pairing ordinary definitions:

| Operation | Before optimization p50 · ms | After optimization p50 · ms |
| --- | ---: | ---: |
| Common definition | 91.309 | 87.321 |
| Common Quotes | 94.126 | 92.173 |
| Common tag completion | 69.195 | 67.228 |
| Local hook tag definition | 53.510 | 53.446 |
| Global hook tag definition | 60.883 | 62.348 |
| Default label settings definition | 61.784 | 62.238 |
| Default label settings reference | 67.547 | 67.914 |
| Default label settings on hover | 51.748 | 50.662 |
| Default label setting completion | 55.508 | 53.724 |
| Ordinary name change | 78.838 | 75.812 |
| hook name renamed | 61.988 | 62.763 |
| hook signature help | 32.391 | 31.923 |
| Eight consecutive edits to completion | 97.531 | 97.869 |

Ten official reports are 465282 bytes and 15516 wall clock samples; two preliminary reports are retained, and all twelve are 475482 bytes and 15636 samples. Statistics, CPU averages, fixtures, scripts and two versions of release SHA have been recalculated. Complete normal/combined smoke and seven paired query modes passed. Tests close distribution discovery, uses pull diagnostics, and does not include PDF compilation or external formatting; results are dependent on synthetic load and machine.

- [Full session before optimization](../benchmarks/results/2026-10-26/round12-default-settings-before.json), [After optimization](../benchmarks/results/2026-10-26/round12-default-settings-after.json)
- [Normal definition pairing](../benchmarks/results/2026-10-26/round12-project-signatures-definition-pairs.json), [Exchange order](../benchmarks/results/2026-10-26/round12-project-signatures-definition-pairs-reversed.json)
- [Default setting definition pairing](../benchmarks/results/2026-10-26/round12-project-signatures-default-hook-definition-pairs.json), [Exchange order](../benchmarks/results/2026-10-26/round12-project-signatures-default-hook-definition-pairs-reversed.json)
- [Normal name change control](../benchmarks/results/2026-10-26/round12-project-signatures-rename-pairs.json), [Exchange sequence](../benchmarks/results/2026-10-26/round12-project-signatures-rename-pairs-reversed.json)
- [hook name definition control](../benchmarks/results/2026-10-26/round12-project-signatures-hook-name-definition-pairs.json), [exchange sequence](../benchmarks/results/2026-10-26/round12-project-signatures-hook-name-definition-pairs-reversed.json)
- [Initial exploration of common definitions](../benchmarks/results/2026-10-26/round12-project-signatures-definition-probe.json), [Initial exploration of default settings](../benchmarks/results/2026-10-26/round12-project-signatures-default-hook-definition-probe.json)

```sh
python3 benchmarks/editing.py --server target/benchmarks/texquill-round12-default-settings \
  --default-hook-labels --global-hook-rules --lifecycle-labels --hook-wrappers \
  --hook-environments --hook-interfaces --hook-renames --runs 3 --iterations 20 \
  --warmup 3 --output target/benchmarks/round12-before.json
python3 benchmarks/rename_pairs.py \
  --before target/benchmarks/texquill-round12-default-settings \
  --after target/benchmarks/texquill-round12-project-signatures \
  --query definition --default-hook-labels --global-hook-rules --lifecycle-labels \
  --hook-wrappers --hook-environments --hook-interfaces --runs 3 --iterations 30 \
  --warmup 5 --output target/benchmarks/round12-pairs.json
```

Optimized version of six request cache tests, strict full-target Clippy and seven sets of actual release/native review passes; the latter checks for precise fields, completion, code block removal and common references after actual editing of events and complete AUX. The final freeze return was 322 targets, 3408 passes, 0 failures, and 505 default skips, of which the full serial stdio was 412 passes and 29 default skips. 96 projects including native hooks, documentation, format, and difference checks passed; 481 Rust source code collections and hash matching frozen lists, and the two versions of release SHA are consistent with all original reports. After measurement, clean up the dev products of this package and retain dependencies, release baselines and original samples.

The interactive report has been released; the normal definition, default field pairing and the last round of global reports each passed JS VM verification of eighteen combinations of gears/statistics/indicators. `html_preview` still returns `Headless Chrome could not render the page: the browser exited unexpectedly.`, and the visual verification is not passed.

## 2026-10-26 Round 13: Document-level code slots and partial reader location analysis

Feature Baseline `af73876` Added explicit `top-level` document-level code slot definition, referencing, hovering, highlighting and completion. Registration/removal maintains the independent identity of this hook. The sorting rules and default settings are not associated with or recommended for this slot. The reserved slot does not provide name changes. `--document-hook-slots` is added in this round, combined with default settings, global rules, life cycle, packaging macros, environment and interface loads; a complete session also verifies all edits to hook names. The three project sizes is 3/6/10 files, 22236/125286/464574 bytes, independently verified 17/129/513 slot definitions and 18/133/521 references. Smoke passes for both normal and combined sessions, and for all eight paired queries.

Review found that `label_declarations::readers` recalculates the execution position of all commands with definition files for each consumer, and then partial reader authentication requires that the source is the current consumer. Optimization only constructs these locations for the current file; definitions for other files still go into the conflict set, and the job budget and full source checks remain intact. No new caches were added, no readers were expanded, or authentication was edited. Two types of declaration protection projects (including native) and seven groups of actual releases have been reviewed and approved.

Formal pairing uses three groups of new processes in each gear, each group warms up five pairs and measures thirty pairs, and requests are executed alternately; all server threads are bound to the same allowed CPU before starting, and thread affinity is verified after opening. Swap the startup sequence of the two versions and test again; the following table lists p50 (ms) according to functional baseline → optimized version.

| Query | First group | After exchange |
| --- | ---: | ---: |
| Common definition · small | 4.569 → 4.465 | 4.551 → 4.488 |
| Common definition · medium | 24.539 → 23.950 | 24.524 → 24.033 |
| Common definition · large | 97.765 → 95.253 | 99.048 → 96.812 |
| Document-level slot definition · large | 59.341 → 59.463 | 61.342 → 61.365 |
| Ordinary name change · large | 85.143 → 85.015 | 86.239 → 85.648 |
| hook name definition · large | 54.412 → 54.612 | 54.955 → 54.517 |

Large common definitions improved by about 2.6%/2.3% respectively, and the server CPU averages were 97.778 → 95.556, 103.222 → 98.444 ms, a decrease of about 2.3%/4.6%. A single CPU sample has only 10 ms accuracy. Here we compare the average of ninety samples in each file. The common definitions of the four forward probes are also improved by about 2%–3% in both orders, and are all retained; the document-level slots are not analyzed by the reader, and the definition time is basically the same. Ordinary name changes and hook name definitions have no stable and substantial benefits.

A full edit session is still three gears, three new sets of processes per gear, three warm-ups, and twenty measurement cycles, with no CPU binding. The common definition of large improves slightly, but multiple metrics fall back and no overall acceleration is claimed:

| large full session | feature baseline p50 ms | optimized version p50 ms |
| --- | ---: | ---: |
| Common definition | 98.992 | 98.566 |
| Ordinary name change | 85.088 | 87.020 |
| Document-level slot definition | 60.266 | 61.902 |
| Document-level slot reference | 61.111 | 61.957 |
| Document-level slot completion | 59.315 | 59.724 |
| Default settings definition | 64.606 | 68.013 |
| Global Rule Definition | 66.485 | 65.790 |
| hook name renamed | 67.203 | 67.583 |
| Eight consecutive edits to completion | 104.136 | 105.833 |

Measurements are not done in parallel with builds, tests, or browser rendering. Fourteen original JSONs totaling 533447 bytes, 17436 wall clock samples, all statistics, CPU averages, fixtures, scripts and binary SHA have been recalculated. The two versions of release SHA-256 are `9a7a54430b5e5b2ff4ca02c0cf7b0d65cb26a2a8d0bb8ebf419a00aa9e058076` and `01e97f0b65d3b16a42d8ac73640cc29f63c509d1ee462e5d967779b58a895b0d`. Eighteen interaction states each of the main report, document-level slot report, and twelfth-round old report compatibility are checked through JS. The headless preview still returns `Headless Chrome could not render the page: the browser exited unexpectedly.`, without visual verification passing.

Original report:

- [Functional Baseline Full Session](../benchmarks/results/2026-10-26/round13-document-slots-baseline.json), [Optimized Version Full Session](../benchmarks/results/2026-10-26/round13-local-reader-positions-editing.json)
- [Normal definition pairing](../benchmarks/results/2026-10-26/round13-local-reader-positions-definition-pairs.json), [Exchange order](../benchmarks/results/2026-10-26/round13-local-reader-positions-definition-pairs-reversed.json)
- [Document-level slot definition pairing](../benchmarks/results/2026-10-26/round13-local-reader-positions-document-hook-definition-pairs.json), [Swap order](../benchmarks/results/2026-10-26/round13-local-reader-positions-document-hook-definition-pairs-reversed.json)
- [Normal name change pairing](../benchmarks/results/2026-10-26/round13-local-reader-positions-rename-pairs.json), [Swap order](../benchmarks/results/2026-10-26/round13-local-reader-positions-rename-pairs-reversed.json)
- [hook name definition pairing](../benchmarks/results/2026-10-26/round13-local-reader-positions-hook-name-definition-pairs.json), [exchange order](../benchmarks/results/2026-10-26/round13-local-reader-positions-hook-name-definition-pairs-reversed.json)
- [Normal definition pre-detection](../benchmarks/results/2026-10-26/round13-local-reader-positions-definition-probe.json), [Exchange sequence](../benchmarks/results/2026-10-26/round13-local-reader-positions-definition-probe-reversed.json)
- [Document level slot pre-detection](../benchmarks/results/2026-10-26/round13-local-reader-positions-document-hook-definition-probe.json), [Exchange sequence](../benchmarks/results/2026-10-26/round13-local-reader-positions-document-hook-definition-probe-reversed.json)


## 2026-10-26 Round 14: Relative tags and shared reader context

The functional baseline is `9f7c3cc`. Added `--relative-hook-labels`, combined with document-level slots, default settings, global rules, lifecycle, wrapper macros, environments and interfaces; the full session also checks all edits to hook names one by one. The main document explicitly registers `relative/tail`, and each section registers `./tail` immediately after the literal Push. Relative removal and complete suffix completion are added to each chapter, and another hook is placed to interfere with the same name. The three project sizes are 3/6/10 documents, 24301/140105/521977 bytes, and independently verified 17/129/513 definitions and 18/133/521 citations. Normal/combined full sessions and all nine paired query smokes pass; existing fixture bytes remain consistent when the new option is turned off.

Review found that `label_declarations::readers` repeatedly scans the same source code scope for each consumer, checking for modified interfaces, dynamic naming/loading, and conditions/grouping. The optimization shares three boolean facts in an immutable item analysis: whether the two native readers remain literal, and whether the custom reader proves to be available. Each consumer's own definition conflicts, job budgets, local execution locations, and alias fixed points are still independently verified; local single file paths use the same context. There is no new cache or expansion of certification scope. Two categories of claim protection special items (including native) and seven groups of candidate release/native relative labels were reviewed and approved.

The formal pairing is three groups of new processes in each stage, five pairs are warmed up and thirty pairs are measured; all server threads in the two versions are bound to the same allowed CPU, alternate requests, exchange the startup sequence and test again. The following table lists p50 (ms) by functional baseline → optimized version, and the reverse JSON retains the actual parameter order.

| Query | First group | After exchange |
| --- | ---: | ---: |
| Common definition · small | 4.838 → 4.727 | 4.933 → 4.874 |
| Common definition · medium | 26.283 → 25.285 | 26.079 → 25.273 |
| Common definition · large | 107.404 → 99.148 | 106.189 → 98.717 |
| Relative label definition · small | 3.101 → 3.081 | 3.094 → 3.052 |
| Relative label definition · medium | 16.780 → 16.614 | 16.779 → 16.477 |
| Relative label definition · large | 67.517 → 66.117 | 66.413 → 65.374 |
| Ordinary name change · large | 92.423 → 91.479 | 93.324 → 91.154 |
| hook name definition · large | 59.620 → 58.758 | 59.489 → 58.893 |

The p50 of the two groups of large ordinary definitions improved by about 7.7%/7.0% respectively, and the average server CPU was 111.222 → 102.556, 108.333 → 101.000 ms, a decrease of about 7.8%/6.8%. The CPU single-sample accuracy is 10 ms, and only the mean of the entire range is compared. The four forward detections are also retained, and the normal definition is improved by about 5.3%/7.5% in the two orders respectively. Relative tag queries are not analyzed by this reader, and although the wall clock changes slightly, the CPU does not drop uniformly, which is not attributed to this optimization; ordinary renames and hook name definition changes are not enough to claim full acceleration.

- [Full session function baseline](../benchmarks/results/2026-10-26/round14-relative-fields-baseline.json), [Optimized version](../benchmarks/results/2026-10-26/round14-reader-context-editing.json)
- [Normal definition pairing](../benchmarks/results/2026-10-26/round14-reader-context-definition-pairs.json), [Exchange order](../benchmarks/results/2026-10-26/round14-reader-context-definition-pairs-reversed.json)
- [Relative tag definition pairing](../benchmarks/results/2026-10-26/round14-reader-context-relative-hook-definition-pairs.json), [Exchange order](../benchmarks/results/2026-10-26/round14-reader-context-relative-hook-definition-pairs-reversed.json)
- [Normal name change control](../benchmarks/results/2026-10-26/round14-reader-context-rename-pairs.json), [Exchange sequence](../benchmarks/results/2026-10-26/round14-reader-context-rename-pairs-reversed.json)
- [hook name definition control](../benchmarks/results/2026-10-26/round14-reader-context-hook-name-definition-pairs.json), [exchange sequence](../benchmarks/results/2026-10-26/round14-reader-context-hook-name-definition-pairs-reversed.json)
- [Preliminary exploration of common definitions](../benchmarks/results/2026-10-26/round14-reader-context-definition-probe.json), [Exchange order](../benchmarks/results/2026-10-26/round14-reader-context-definition-probe-reversed.json)
- [Initial exploration of relative tags](../benchmarks/results/2026-10-26/round14-reader-context-relative-hook-definition-probe.json), [Exchange order](../benchmarks/results/2026-10-26/round14-reader-context-relative-hook-definition-probe-reversed.json)

Full edit session with three new sets of processes per run, warmed up three times each and measured for twenty cycles, no CPU binding. The full report was not written for the large file at the time of the interruption, so the candidate full session reruns the three project sizes separately; the paired report does not need to be rerun. The p50 of large is as follows. Including queries that have not been analyzed by this reader, many indicators have improved, indicating that there are differences in overall operating conditions between sessions, and all changes cannot be attributed to this optimization; causal judgment is still based on same-core dual-sequence pairing.

| large full session | feature baseline p50 ms | optimized version p50 ms |
| --- | ---: | ---: |
| Common definition | 117.478 | 101.917 |
| Ordinary name change | 101.747 | 93.266 |
| Relative label definition | 74.659 | 67.595 |
| Relative label reference | 75.666 | 68.492 |
| relative label hover | 70.818 | 64.487 |
| Relative tag completion | 71.435 | 66.856 |
| Default settings definition | 82.416 | 72.888 |
| Global Rule Definition | 78.811 | 72.413 |
| Hook name renamed | 79.460 | 73.595 |
| Eight consecutive edits to completion | 125.726 | 109.789 |

Measurements are not done in parallel with builds, tests, or browser rendering. Fourteen original JSONs totaling 570256 bytes and 18876 wall clock samples; statistics, CPU averages, fixtures, scripts and binary SHA all passed the recalculation. The functional version and optimized version release SHA-256 are `e306aaa41754e5b5281f0665912482d180c64aa10febb26f97c867323d2f776f` and `9463007f8640a6fcf57636a0775bd27eeda84e119d1e6bcf52fda0ceb46f90e2` respectively. Tests off distribution discovery, uses pull diagnostics, and does not include PDF compilation or external formatting; results depend on synthetic load and machine.

```sh
python3 benchmarks/editing.py --server target/benchmarks/texquill-round14-reader-context \
  --relative-hook-labels --document-hook-slots --default-hook-labels \
  --global-hook-rules --lifecycle-labels --hook-wrappers --hook-environments \
  --hook-interfaces --hook-renames --runs 3 --iterations 20 --warmup 3 \
  --baseline benchmarks/results/2026-10-26/round14-relative-fields-baseline.json \
  --output target/benchmarks/round14-editing.json
python3 benchmarks/rename_pairs.py \
  --before target/benchmarks/texquill-round14-relative-fields \
  --after target/benchmarks/texquill-round14-reader-context \
  --query definition --relative-hook-labels --document-hook-slots \
  --default-hook-labels --global-hook-rules --lifecycle-labels --hook-wrappers \
  --hook-environments --hook-interfaces --runs 3 --iterations 30 --warmup 5 \
  --output target/benchmarks/round14-definition-pairs.json
```

Interaction report published. Ordinary definitions, relative label definitions, and the compatibility of the thirteenth round of old reports each pass JS VM verification of eighteen combinations of gears/statistics/indicators. `html_preview` returns `Headless Chrome could not render the page: the browser exited unexpectedly.`, excluding visual verification.

The optimized version of the final freeze verification passes: 322 targets, 3421 passes, 0 failures, 507 default skips, of which the full serial stdio is 414 passes, 29 default skips. 109 projects including native hooks, strict full-target Clippy, documentation, format and difference checks passed; 481 Rust source code collections and hashes are consistent with the frozen list. Seven sets of actual optimized versions release/native review passed, precise navigation, completion and common reference actual editing to maintain events and complete AUX; the release SHA of the two versions is consistent with the original report.

After the final verification, the dev product of this package was cleaned up, 9.4 GiB was released, and the available disk space was restored to about 20 GiB; the release baseline, dependencies, and all original measurement data were retained.
