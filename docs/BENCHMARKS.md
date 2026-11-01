# Main performance and upstream graph oracle retest

A newer [six-repository retest](BENCHMARKS-SIX-REPO-2026-11-01.md) measures
`edb403e` and adds Zod and OkHttp. This page retains the older `b827b1d` results,
including sync and query measurements not completed in the newer retest.

This report measures merged `main` on **2026-11-01**. It replaces the headline measurements
from the [historical four-worker comparison](BENCHMARKS-HISTORICAL.md). Performance and graph
agreement are separate results: a faster engine does not by itself establish equivalent graphs.
Exact timing samples, graph intersections, definitions, query lists and exclusions are in the
[machine-readable evidence](benchmarks/2026-11-01-main-retest.json).

The subsequent [resolution-quality investigation and fixes](RESOLUTION-QUALITY.md) report
new graph comparisons for engine version 3. The timing results below remain tied to `b827b1d`.

## Revisions and environment

| Component | Pinned version / setting |
|---|---|
| `cgr` | [`b827b1d5bc6d90eaf7e6810e9effcc6dfe42eba4`](https://github.com/NianJiuZst/CodeGraph-Rust/tree/b827b1d5bc6d90eaf7e6810e9effcc6dfe42eba4), merged main |
| Upstream | [`d2e0389b519be398f9bc616b6f7bea8a7edfbc6a`](https://github.com/colbymchenry/codegraph/tree/d2e0389b519be398f9bc616b6f7bea8a7edfbc6a), package.json 1.6.2 |
| Vendored kernel base | `9d3abb843a84d7610bf987bda7a1db4a6fe3eee3` plus the project's recorded adaptation patches |
| Machine | Apple M5, 10 cores, 16 GiB RAM, arm64, macOS 26.5.2 |
| Toolchains | Rust 1.94.1; Node 24.18.0 |
| Builds | `cargo build --release --locked -p cgr`, opt-level 3 and fat LTO; upstream production JS and release native kernel |
| Restricted tier | `CGR_THREADS=2`, `CGR_MEMORY_MB=2048`; upstream parse/resolve workers both 2 |
| Upstream launcher | `node --liftoff-only --disable-warning=ExperimentalWarning` |
| Persistence | Production defaults; `CGR_FSYNC` unset, upstream SQLite WAL and synchronous NORMAL |
| Power / scheduling | AC checked before and after each recorded command; no CPU pinning or frequency lock |

The source paths implementing JavaScript, Go, Rust and Python extraction, node IDs and buffers
are unchanged between the vendored upstream base and the upstream revision under test. The
upstream native binary was retained from the previous build: its entire kernel source is
unchanged between that build's commit `087de4c` and `d2e0389`. Native loading, ABI and default
language routing were checked. No WASM-forcing override or disabled analysis pass was used;
upstream retains its production per-file fallback policy for erroring syntax trees.

Both programs ran on independent scratch copies. Identical `codegraph.json` exclusions remove
YAML/XML files indexed only by upstream. Indexed file paths and SHA-256 source-byte digests
were compared after full indexing and after edit restoration. Source repositories and builds
were prepared outside timing; the corpus checkouts were clean at their pinned commits.

| Repository | Commit | Indexed files |
|---|---|---:|
| [Express](https://github.com/expressjs/express/tree/1c6f1b0fe4000977b9a7b4f34097e4d527dd9824) | `1c6f1b0fe4000977b9a7b4f34097e4d527dd9824` | 142 |
| [Gin](https://github.com/gin-gonic/gin/tree/0f09c3a9b4626d9fe9979cf4d0521d6ae32ab646) | `0f09c3a9b4626d9fe9979cf4d0521d6ae32ab646` | 99 |
| [Tokio](https://github.com/tokio-rs/tokio/tree/b499257ef62bd631b83a988d371f4330f545670c) | `b499257ef62bd631b83a988d371f4330f545670c` | 814 |
| [Django](https://github.com/django/django/tree/a05e16a7230fe33d3f43f4a3aaed103cebb42efc) | `a05e16a7230fe33d3f43f4a3aaed103cebb42efc` | 2,984 |

## Resource protection and timing

The repository's unmodified `scripts/bench-local.sh` ran an external paired-command adapter.
The adapter invokes production CLIs/handlers and provides their individual measurements;
the wrapper's group wall times include cooling and must not be used as engine timings.

- Preflight requires at least 6,144 MiB available memory, swap below 1,024 MiB and disk
  space at least 20,480 MiB. On macOS the current wrapper computes available memory as
  free + inactive + speculative pages. The full-index phase passed at 6,258 / 969 / 35,296 MiB.
  The sync/query phase initially refused at 5,855 MiB available memory, and later passed
  at 6,236 / 969 / 34,394 MiB after memory was released. Thresholds were unchanged.
- `caffeinate -i` prevents sleep. Process-tree RSS is sampled every 200 ms, with a
  3,072 MiB kill limit and a 1,024 MiB swap-growth limit. Only one tested command runs
  at a time. The same conservative RSS guard applies to both implementations.
- Every tested command is separated from its predecessor by at least 30 seconds.
  No performance result is taken from a failed preflight or aborted setup attempt.
- Full-index, sync and new-process CLI times include process startup and shutdown.
  They are measured by a monotonic wall clock. `/usr/bin/time -l` supplies each run's
  maximum RSS; tables report the median of those peaks. Sampled tree peaks are retained
  separately for the guard and are not substituted for the resource measurement.
  UTC timestamps are recorded after resource/output bookkeeping; the optional command-gap
  estimate in the JSON is approximate, while cooling uses its own saved completion time.
- File caches remain warm. Other desktop applications were running. Three samples
  support medians and ranges, not confidence intervals or a universal speedup promise.

## Full indexing

Each implementation receives one untimed warmup and three timed complete CLI rebuilds.
Execution order alternates. Upstream's CLI requires initialization; an empty database was
created before warmup using its production `CodeGraph.initSync` entry point. Its `index`
command recreates the full database, and `cgr index` rebuilds the complete snapshot.

| Repository | Files | Upstream full index | `cgr` full index | Speedup | Peak RSS: upstream → `cgr` |
|---|---:|---:|---:|---:|---:|
| Express | 142 | 532.7 ms | 121.9 ms | **4.37×** | 225.7 → 27.9 MiB |
| Gin | 99 | 636.6 ms | 129.6 ms | **4.91×** | 246.3 → 34.8 MiB |
| Tokio | 814 | 2,599.3 ms | 504.6 ms | **5.15×** | 736.1 → 73.4 MiB |
| Django | 2,984 | 10,878.0 ms | 2,002.1 ms | **5.43×** | 1,909.2 → 229.6 MiB |

| Repository | Upstream min–max (ms) | `cgr` min–max (ms) |
|---|---:|---:|
| Express | 531.5–548.7 | 111.7–143.9 |
| Gin | 631.7–640.2 | 128.3–136.0 |
| Tokio | 2584.1–2650.4 | 482.1–546.2 |
| Django | 10690.2–10895.7 | 1916.6–2094.0 |

Speedup = upstream median / `cgr` median, before rounding. The equal-weight geometric mean
across the four repositories is **4.95×**. Median peak RSS is lower by
**85.9%–90.0%**. This is an end-to-end pipeline comparison with the graph
coverage differences below; it does not isolate the contribution of each optimization.

## Incremental CLI synchronization

Each category runs three times per implementation in alternating order. Both copies receive
the same change, with original bytes restored and synced after each edit category.

| Repository | No changes | Comment edit | Single-file symbol rename |
|---|---:|---:|---:|
| Express | 183.2 → 37.6 ms (4.87×) | 311.8 → 56.7 ms (5.50×) | 312.5 → 53.0 ms (5.89×) |
| Gin | 165.6 → 32.1 ms (5.16×) | 317.3 → 54.7 ms (5.80×) | 327.5 → 157.4 ms (2.08×) |
| Tokio | 196.8 → 50.4 ms (3.90×) | 492.8 → 70.4 ms (7.00×) | 499.4 → 71.4 ms (7.00×) |
| Django | 451.7 → 235.5 ms (1.92×) | 1925.9 → 270.2 ms (7.13×) | 1951.0 → 279.5 ms (6.98×) |

The edited file and symbol are `lib/application.js` / `handle`, `gin.go` / `Default`,
`tokio/src/runtime/builder.rs` / `new_current_thread`, and
`django/core/handlers/base.py` / `get_response`. Comment edits append a distinct comment;
rename edits alternate an existing symbol and its references within the same file.
These measurements do not represent repository-wide refactoring or watcher notification latency.
No-change CLI sync includes filesystem scanning; it does not exercise the event-path shortcut.

## Explore queries

| Repository | Warm P50: upstream → `cgr` | P50 speedup | Warm P95: upstream → `cgr` | New-process CLI median |
|---|---:|---:|---:|---:|
| Express | 3.74 → 1.15 ms | 3.25× | 9.76 → 2.81 ms | 132.2 → 21.5 ms (6.14×) |
| Gin | 8.47 → 0.95 ms | 8.92× | 11.73 → 3.08 ms | 140.3 → 20.5 ms (6.84×) |
| Tokio | 16.49 → 3.27 ms | 5.03× | 21.78 → 5.99 ms | 162.6 → 17.9 ms (9.06×) |
| Django | 33.25 → 4.50 ms | 7.39× | 51.87 → 11.30 ms | 301.0 → 14.8 ms (20.30×) |

Both sides use the same 20 fixed-seed production-symbol queries retained from the earlier
comparison, including single names, pairs and questions containing names. Eight queries warm
the production handler; 20 queries × 3 passes provide 60 latency observations per repository.
`cgr bench explore` and upstream `ToolHandler.executeReadTool` exclude MCP transport,
admission queueing and network latency. The equal-weight warm-P50 speedup mean is
**5.73×**. P99 and output-size distributions are available in the data file.

New-process CLI results repeat the first fixed query three times on each side, including
startup with warm OS caches. They are a single-query sample for each repository.
Outputs are not equalized: upstream counts UTF-16 JS string units, while `cgr` counts UTF-8
bytes. Mostly ASCII code still represents different returned content; output size and graph
coverage can contribute to latency differences. No agent A/B, answer-quality or token-savings
claim follows from these timings.

| Repository | Upstream average JS string length | `cgr` average UTF-8 bytes |
|---|---:|---:|
| Express | 12,028 | 6,371 |
| Gin | 12,919 | 11,402 |
| Tokio | 24,810 | 16,710 |
| Django | 24,434 | 23,544 |

## Oracle definition and graph coverage

The upstream graph is the full-index result of its production CLI. Its
[official dump script](https://github.com/colbymchenry/codegraph/blob/d2e0389b519be398f9bc616b6f7bea8a7edfbc6a/scripts/dump-graph.mjs)
was also run and retained independently of the custom comparison. `cgr export --sqlite`
provides the live snapshot's rows. An external read-only exporter calls the existing
`cg_store::node_id_string` on the **persisted digest and collision-column suffix** to recover
actual stable external IDs. IDs are not guessed from names or matched to whichever oracle
endpoint gives the highest score.

The primary key follows design §13.2 exactly:

```text
(source stable ID, target stable ID, edge kind, line, UTF-16 column)
precision = |cgr ∩ oracle| / |cgr|
recall    = |cgr ∩ oracle| / |oracle|
Jaccard   = |cgr ∩ oracle| / |cgr ∪ oracle|
```

NULL remains distinct from zero. Confidence, resolution strategy, metadata and provenance
are excluded from the match key. All routes, containment edges and syntax-error files remain
in the primary denominator. Identical natural keys are counted once; all four samples have
zero duplicate edge rows and zero duplicate node IDs. An independent SQLite `INTERSECT`
query reproduced every reported intersection count.

| Repository | Node rows: upstream / `cgr` | Route nodes: upstream / `cgr` | Edge rows: upstream / `cgr` |
|---|---:|---:|---:|
| Express | 1,143 / 866 | 277 / 0 | 2,334 / 2,527 |
| Gin | 2,667 / 2,436 | 231 / 0 | 8,073 / 7,921 |
| Tokio | 14,460 / 14,460 | 0 / 0 | 39,682 / 36,511 |
| Django | 62,755 / 62,660 | 840 / 745 | 194,105 / 185,506 |

The non-route node identity keys `(relative file, kind, name, start line, start UTF-16 column)`
match **100% in all four samples**. This does not compare all node attributes or establish
resolved-call, route or synthesized-edge agreement.

### Primary strict result

| Repository | Matching edges | Unique edges: `cgr` / oracle | Precision | Recall | Jaccard |
|---|---:|---:|---:|---:|---:|
| Express | 1,384 | 2,527 / 2,334 | 54.77% | 59.30% | 39.80% |
| Gin | 7,626 | 7,921 / 8,073 | 96.28% | 94.46% | 91.13% |
| Tokio | 31,541 | 36,511 / 39,682 | 86.39% | 79.48% | 70.64% |
| Django | 173,752 | 185,506 / 194,105 | 93.66% | 89.51% | 84.40% |

Across repository-prefixed keys, the micro total is 214,303 matches / 232,465 `cgr` edges / 244,194 oracle edges: precision **92.19%**, recall **87.76%**, Jaccard **81.68%**. This total is dominated by Django and does not replace the per-repository gate.

**None of the four repositories passes the design's relative precision ≥97% and recall ≥95%
gate.** The result establishes a speed advantage for these workloads, while the goal of
upstream-level whole-graph agreement remains open. In particular, total node counts or edge
counts alone would conceal important relationship differences.

### Non-containment and calls

Containment is structurally easy to match and can dominate a graph. The following views keep
the same strict stable-ID/location key and expose the relationships used for navigation.

| Repository | Matches | Unique edges: `cgr` / oracle | Precision | Recall | Jaccard |
|---|---:|---:|---:|---:|---:|
| Express | 660 | 1,803 / 1,610 | 36.61% | 40.99% | 23.97% |
| Gin | 4,895 | 5,190 / 5,306 | 94.32% | 92.25% | 87.40% |
| Tokio | 14,488 | 19,458 / 22,629 | 74.46% | 64.02% | 52.49% |
| Django | 114,821 | 125,830 / 135,174 | 91.25% | 84.94% | 78.55% |

| Repository — calls only | Matches | Unique edges: `cgr` / oracle | Precision | Recall | Jaccard |
|---|---:|---:|---:|---:|---:|
| Express | 419 | 1,550 / 1,280 | 27.03% | 32.73% | 17.38% |
| Gin | 3,430 | 3,450 / 3,807 | 99.42% | 90.10% | 89.63% |
| Tokio | 8,345 | 9,252 / 15,605 | 90.20% | 53.48% | 50.54% |
| Django | 43,533 | 48,315 / 56,826 | 90.10% | 76.61% | 70.66% |

The evidence file also reports every edge kind, source language, provenance distribution,
error-file list, per-side difference counts and both engines' row counts. Django includes three
`cgr` files flagged for syntax/extraction errors; they were retained, not excluded to improve
the score. There are no reported error files in the other three `cgr` samples.

### Secondary diagnostics

The normalized view replaces each endpoint ID with its exact node identity tuple. The topology
view additionally drops edge line/column, merging repeated call sites. These change the metric
and denominator; they are **not substitutes for the primary gate**.

| Repository | Normalized + site precision | Normalized + site recall | Topology precision | Topology recall |
|---|---:|---:|---:|---:|
| Express | 54.77% | 59.30% | 81.41% | 82.58% |
| Gin | 96.28% | 94.46% | 95.63% | 93.79% |
| Tokio | 86.39% | 79.48% | 88.03% | 80.64% |
| Django | 93.66% | 89.51% | 93.66% | 90.07% |

For these samples, endpoint normalization alone leaves the strict site-aware scores unchanged.
Ignoring call sites still does not yield the required precision/recall across the corpus.
The differences therefore require resolution, synthesis and framework work, beyond formatting.

| Repository | Side only | Automatic diagnostic bucket | Unique edges |
|---|---:|---:|---:|
| Express | rust | resolution_or_synthesis_needs_review | 1,142 |
| Express | rust | edge_location_difference | 1 |
| Express | upstream | resolution_or_synthesis_needs_review | 930 |
| Express | upstream | framework_route_coverage | 20 |
| Gin | rust | resolution_or_synthesis_needs_review | 293 |
| Gin | rust | edge_location_difference | 2 |
| Gin | upstream | edge_location_difference | 14 |
| Gin | upstream | resolution_or_synthesis_needs_review | 382 |
| Gin | upstream | framework_route_coverage | 15 |
| Gin | upstream | containment_topology | 36 |
| Tokio | rust | resolution_or_synthesis_needs_review | 4,891 |
| Tokio | rust | edge_location_difference | 79 |
| Tokio | upstream | resolution_or_synthesis_needs_review | 8,088 |
| Tokio | upstream | edge_location_difference | 53 |
| Django | rust | resolution_or_synthesis_needs_review | 10,131 |
| Django | rust | framework_route_coverage | 1,451 |
| Django | rust | edge_location_difference | 172 |
| Django | upstream | resolution_or_synthesis_needs_review | 19,569 |
| Django | upstream | framework_route_coverage | 709 |
| Django | upstream | edge_location_difference | 75 |

### Reviewed examples and follow-up

The buckets above are automatic structural diagnostics, not a correctness verdict on every
edge. Each bucket's first 20 examples and source windows, plus the complete differing keys,
are retained locally for review. Most unresolved/synthesis differences still need adjudication.

- **Upstream can be wrong.** At `examples/auth/index.js:21`, upstream links
  `express.urlencoded()` to `createApplication` in `lib/express.js`. The latter explicitly
  exports `urlencoded` from `bodyParser`, so this target is not the called function.
  `cgr` omits that target and separately links `app.use` to `lib/application.js::use`.
  The source factory mixes the application prototype into `app`, supporting that edge.
- **`cgr` can be wrong.** At `examples/content-negotiation/index.js:36`, `res.format(obj)`
  is linked to the enclosing local wrapper `format(path)` as a self-call. That wrapper is
  not the receiver's method. Upstream leaves this call unlinked.
- **Route identity also differs.** In `django/conf/urls/i18n.py:38`, `cgr` names a route
  `ANY /setlang/`, while upstream names it `setlang/`. Express/Gin have 277/231 upstream
  route nodes and no `cgr` route nodes in these self-repository samples. Dependency-manifest
  framework detection, route naming and extraction coverage need explicit compatibility rules.

Follow-up should prioritize JavaScript receiver/factory/callback resolution, Rust calls and
imports, Python calls/decorators/imports, then Gin's smaller remaining gap. Add focused semantic
expectations for reviewed examples before changing rules. Record intentional deviations from
known upstream mistakes separately, while always retaining unadjusted oracle scores. The
current result covers one repository per primary language, not the design's three repositories
per MVP language or a complete synthesis-pass acceptance matrix.

## Restoration and correctness checks

After all edits were restored, `cgr` retained exactly the initial strict edge set in **4/4** repositories; upstream did so in **4/4**. Both source-byte digests returned to their initial values. The final `cgr status --verify --strict` and `cgr debug diff` checks passed for all four repositories. These compare partition integrity and incremental/full equivalence, separately from upstream agreement.

The existing cross-platform implementation CI remains linked in the README; this retest adds
real-repository measurements and oracle comparison. It does not reclassify hosted CI timing
as performance evidence. Snapshot format remains 2; old indexes need a full `cgr index` rebuild.

## Reproduction and retained evidence

Use clean, separate copies of the pinned source commits and the published identical exclusions.
Build both production variants and verify native kernel loading before collecting any samples.
The exact command vectors, query strings and all 152 benchmark/warmup/restore observations
are in the JSON evidence. Compilation, installation and copying must remain outside timing.

```sh
export CGR_THREADS=2 CGR_MEMORY_MB=2048
export CODEGRAPH_PARSE_WORKERS=2 CODEGRAPH_RESOLVE_WORKERS=2
export CODEGRAPH_TELEMETRY=0 DO_NOT_TRACK=1
unset CGR_FSYNC CODEGRAPH_KERNEL CODEGRAPH_KERNEL_LANGS CODEGRAPH_KERNEL_PATH
NODE24=/path/to/node-v24.18.0/bin/node

# Representative production commands, with your own paths:
cgr --path "$CGR_COPY" index --quiet --report-json
"$NODE24" --liftoff-only --disable-warning=ExperimentalWarning \
  "$UPSTREAM/dist/bin/codegraph.js" index --quiet "$UPSTREAM_COPY"
cgr --path "$CGR_COPY" sync --quiet
"$NODE24" --liftoff-only --disable-warning=ExperimentalWarning \
  "$UPSTREAM/dist/bin/codegraph.js" sync --quiet "$UPSTREAM_COPY"
cgr bench explore "$CGR_COPY" --queries "$QUERIES" --runs 3 --json

# The stock wrapper measures cgr directly; its JSON is not an upstream comparison:
scripts/bench-local.sh --tier restricted --cgr /path/to/cgr \
  --repos-dir /path/to/copies --kinds index,sync,explore \
  --runs 3 --edits 200 --out /path/to/results --history /path/to/history \
  express gin tokio django
```

The paired comparison used the stock wrapper with an external adapter implementing its
`bench index/sync/explore/compare` invocation interface. The adapter ran the individual CLI
commands above sequentially, saved `/usr/bin/time -l` results, enforced per-command AC checks
and cooling, and summarized them without altering either production engine. The cgr-only 10%
historical regression gate was not evaluated: the earlier series used different worker settings
and an older upstream revision. The raw scripts,
databases, official dumps, source digests, key differences and logs remain outside the source
checkout; compact measurement data is published with this report. This is a comparative
restricted-tier run, not a completed standard-tier, random-200-edit, with-readers or large-repo
RSS acceptance matrix. Kernel spill was not exercised by these four corpus sizes.

## 中文说明

本轮在同一台 Apple M5、16 GiB 内存的电脑上，复测已合并的 `main`（`b827b1d`），
对照固定的上游提交 `d2e0389`。双方提取/解析线程参数均为 2，`cgr` 内存预算为 2 GiB；
全量重建预热一次、计时三次，交替运行、每次间隔至少 30 秒。保护阈值未降低，原版原生 Rust 内核已启用。

全量索引加速比几何平均为 **4.95×**，峰值 RSS 降低 **85.9%–90.0%**。
本轮与历史 6.40× 的线程参数、上游提交和计时条件不同，不能直接当作优化前后的收益。
同步、查询、输出量及各次原始采样见上表和 JSON 数据文件；输出内容未强行截成相同长度。

**图谱基本一致的目标尚未达到。** 主指标逐条比较来源稳定 ID、目标稳定 ID、边类型、行号和 UTF-16 列号，
不把 NULL 改成 0，不删除 contains、路由或语法错误文件。四个仓库均未达到设计要求的
精确率 ≥97%、召回率 ≥95%。非路由节点的身份键虽然 100% 一致，解析后的关系仍有实质差异；
忽略位置的拓扑分数也不能代替主指标或证明达到门槛。

差异不能一概当作 `cgr` 错误：已经核查到原版将外部的 `express.urlencoded` 错连到应用创建函数，
也核查到 `cgr` 把 `res.format` 错连到同名本地包装函数。Django 的路由命名和两边的路由覆盖还不一致。
自动分桶仅用于定位；大量解析/合成差异仍需逐类审阅。下一步应优先校准 JavaScript 接收者、工厂和回调，
Rust 调用与导入、Python 调用/装饰器/导入，以及 Gin 剩余差异；对已确认的上游错误保留单独的语义预期，
不能为了提高分数复刻错误或静默删除分母。

本轮性能、oracle 原始结果和恢复后检查均单独留证。标准档、大仓库内存上限、并发读写、完整 MVP 多仓库
验收及 Agent A/B 仍属于其他测试范围，本报告不将它们标为已通过。
