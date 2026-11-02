# CodeGraph Rust (`cgr`)

**A local code knowledge graph for AI coding agents, built in Rust.**

[English](README.md) · [简体中文](README.zh-CN.md) · [Architecture](docs/ARCHITECTURE.md) · [Benchmarks](docs/BENCHMARKS.md) · [Changelog](CHANGELOG.md)

CodeGraph Rust indexes a repository into a persistent graph of symbols, references and
relationships. It gives developers and coding agents a way to retrieve relevant source,
follow calls across files, inspect dependencies and estimate the impact of a change through
one command-line executable or an MCP connection.

The engine runs locally and builds its graph from source syntax and deterministic analysis.
Indexing and querying require no hosted service, embedding API, model credentials, Node.js
runtime or database server. The normal index is stored in the project's `.cgr/` directory;
query results include source windows from the working tree.

This is an independent Rust re-implementation of the engine behind
[colbymchenry/codegraph](https://github.com/colbymchenry/codegraph). It reuses the upstream
Tree-sitter extraction kernel and adapts the agent-facing tool schemas and guidance, with
`codegraph-rust_` as the MCP tool prefix. Storage, resolution, incremental synchronization,
queries and service integration are implemented in this Rust workspace. Current coverage
differs from upstream; see [current limitations](#current-limitations).

## Contents

- [Capabilities](#capabilities)
- [Measured performance](#measured-performance)
- [Installation](#installation)
- [Quick start](#quick-start)
- [Language and framework support](#language-and-framework-support)
- [Command-line reference](#command-line-reference)
- [Agent and MCP integration](#agent-and-mcp-integration)
- [Configuration](#configuration)
- [Architecture](#architecture)
- [Index lifecycle](#index-lifecycle)
- [Development and verification](#development-and-verification)
- [Current limitations](#current-limitations)
- [Contributing](#contributing)
- [Acknowledgements and license](#acknowledgements-and-license)

## Capabilities

- **Source-backed exploration.** `explore` combines symbol search, call paths, related source
  and change impact in one bounded response. Queries can contain natural-language questions,
  symbol names, qualified names or file names; specific names help identify a flow.
- **Cross-file code navigation.** Inspect callers, callees, implementations, imports and
  dependencies, including supported framework routes and heuristic dynamic-dispatch links.
- **Incremental indexing.** Re-extract changed files and re-resolve affected references.
  Larger changes and configuration changes can trigger a full rebuild.
- **Efficient persistent storage.** Immutable memory-mapped segments, dense integer IDs,
  string pools, bidirectional CSR edges, and prebuilt FST, trigram and BM25 search indexes.
- **CLI and MCP access.** Shared query handlers serve terminal workflows and agent clients.
  Built-in installers support Claude Code, Cursor and Codex.
- **Updates during agent sessions.** The MCP server can watch indexed projects, debounce
  file changes and publish new snapshots. Run `cgr sync` manually when no watcher is running.
- **Inspectable results.** Index health and checksums, unresolved references, incremental/full
  graph comparisons and optional SQLite exports are available from the CLI.

## Measured performance

The **2026-11-01 six-repository retest** of `cgr` **`edb403e`** and upstream
**`d2e0389`** measured a **4.77× geometric-mean full-index speedup** and
**78.3%–88.7% lower median peak RSS** on this machine. The corpus adds TypeScript
Zod and Kotlin/Java OkHttp to the previous four repositories. Both sides indexed the same
files and bytes, and upstream's native kernel was enabled.

| Repository | Files | Upstream full index | `cgr` full index | Speedup | Peak RSS: upstream → `cgr` |
|---|---:|---:|---:|---:|---:|
| Express | 142 | 536.2 ms | 136.0 ms | **3.94×** | 225.6 → 48.9 MiB |
| Gin | 99 | 620.8 ms | 130.5 ms | **4.76×** | 246.9 → 37.0 MiB |
| Tokio | 814 | 2,592.2 ms | 555.3 ms | **4.67×** | 733.6 → 83.3 MiB |
| Django | 2,984 | 10,597.8 ms | 1,934.0 ms | **5.48×** | 1,896.8 → 237.4 MiB |
| Zod | 523 | 2,287.0 ms | 474.1 ms | **4.82×** | 627.2 → 93.8 MiB |
| OkHttp | 690 | 3,136.6 ms | 613.9 ms | **5.11×** | 814.2 → 109.0 MiB |

Three timed CLI rebuilds follow one warmup; the restricted tier uses two workers and
a 2 GiB `cgr` memory budget. Commands run on AC power with at least 30 seconds between
runs and warm OS caches. Full-graph precision and recall against upstream do not both meet
the 97% / 95% gate in any of the six repositories; graph outputs differ and upstream is not
semantic ground truth. See the [six-repository report](docs/BENCHMARKS-SIX-REPO-2026-11-01.md)
and [structured evidence](docs/benchmarks/2026-11-01-six-repo-retest.json).

The sections below retain the earlier **`b827b1d`** sync and query measurements. The new
retest completed only Express sync and did not complete query latency tests.

### Incremental CLI synchronization

CLI medians below are upstream → `cgr` (speedup), with three timed commands per category and
implementation. Both sides receive the same edit; original source is restored after each category.
Symbol renames are within one file and do not measure repository-wide refactoring.

| Repository | No changes | Comment edit | Single-file symbol rename |
|---|---:|---:|---:|
| Express | 183.2 → 37.6 ms (4.87×) | 311.8 → 56.7 ms (5.50×) | 312.5 → 53.0 ms (5.89×) |
| Gin | 165.6 → 32.1 ms (5.16×) | 317.3 → 54.7 ms (5.80×) | 327.5 → 157.4 ms (2.08×) |
| Tokio | 196.8 → 50.4 ms (3.90×) | 492.8 → 70.4 ms (7.00×) | 499.4 → 71.4 ms (7.00×) |
| Django | 451.7 → 235.5 ms (1.92×) | 1925.9 → 270.2 ms (7.13×) | 1951.0 → 279.5 ms (6.98×) |


### Query latency

| Repository | Warm P50: upstream → `cgr` | P50 speedup | Warm P95: upstream → `cgr` | New-process CLI median |
|---|---:|---:|---:|---:|
| Express | 3.74 → 1.15 ms | 3.25× | 9.76 → 2.81 ms | 132.2 → 21.5 ms (6.14×) |
| Gin | 8.47 → 0.95 ms | 8.92× | 11.73 → 3.08 ms | 140.3 → 20.5 ms (6.84×) |
| Tokio | 16.49 → 3.27 ms | 5.03× | 21.78 → 5.99 ms | 162.6 → 17.9 ms (9.06×) |
| Django | 33.25 → 4.50 ms | 7.39× | 51.87 → 11.30 ms | 301.0 → 14.8 ms (20.30×) |

Warm measurements use the same 20 fixed queries × 3 passes after warming eight queries, calling
production in-process handlers. They exclude MCP transport and queueing; CLI medians include
startup for one fixed query repeated three times. Warm P50 speedup geometric mean: **5.73×**.
Returned content and output lengths differ. No LLM or token-savings benchmark was run.

### Performance engineering update

Main includes event-path sync, source-free snapshot compaction after a quiet period, extraction
buffer spill, preallocated resolution outcomes, fused hook scanning, selective/parallel event
synthesis, compressed BM25 postings, adaptive radix sorting and bounded source windows. MCP has
bounded admission, cooperative cancellation and project/watcher cleanup.

These changes use **snapshot format 2**; run `cgr index /path/to/project` to rebuild an old index.
The new full-index retest is revision-pinned above. The earlier **6.40×** result used different
worker settings and an older upstream revision; it is [archived separately](docs/BENCHMARKS-HISTORICAL.md),
and is not a before/after estimate for this optimization. Implementation boundaries are recorded
in the bilingual [engineering record](docs/PERFORMANCE-IMPLEMENTATION.md).

## Installation

### Build and install from source

Source installation is the currently available path. Requirements are **Rust 1.89 or newer**,
Cargo, Git and a C compiler toolchain for the Tree-sitter grammars.

```sh
git clone https://github.com/NianJiuZst/CodeGraph-Rust.git
cd CodeGraph-Rust
cargo install --path crates/cgr --locked
cgr version --verbose
```

Cargo installs `cgr` into its binary directory, normally `~/.cargo/bin`. Make sure that directory
is on your `PATH`. Rust and the C compiler are build-time requirements; the resulting executable
runs without a Rust toolchain or Node.js.

For a local build without installing:

```sh
cargo build --release --locked -p cgr
./target/release/cgr --help
```

On Windows, the executable is `target\release\cgr.exe`.

### Release installers

The repository includes release packaging and checksum-verifying download scripts. **As of
2026-11-01, no GitHub Release has been published.** The following commands require published
archives and checksum files on the [Releases page](https://github.com/NianJiuZst/CodeGraph-Rust/releases).
Until those assets are available, use the source installation above.

```sh
# Linux / macOS — requires a published release
curl -fsSL https://raw.githubusercontent.com/NianJiuZst/CodeGraph-Rust/main/scripts/install.sh | sh
```

```powershell
# Windows, PowerShell 7+ — requires a published release
irm https://raw.githubusercontent.com/NianJiuZst/CodeGraph-Rust/main/scripts/install.ps1 | iex
```

Release automation defines these targets:

| Platform | Architectures | Packaging |
|---|---|---|
| Linux | x86_64, ARM64 | Static musl executable, `.tar.gz` |
| macOS | Intel x86_64, Apple Silicon ARM64 | Executable, `.tar.gz` |
| Windows | x86_64, ARM64 | MSVC executable, `.zip` |

The scripts accept `CGR_VERSION` to select a version and `CGR_INSTALL_DIR` to choose the install
location. Defaults are `~/.local/bin` on Linux/macOS and `%LOCALAPPDATA%\Programs\cgr` on Windows.

## Quick start

Run these commands in the repository you want to analyze:

```sh
cd /path/to/your-project
cgr init
cgr status --verify
```

Explore an area using names from that project's source:

```sh
cgr explore "AuthService loginUser SessionStore"
cgr query loginUser --json
cgr callers loginUser
cgr callees loginUser
cgr impact loginUser --depth 2
```

`AuthService`, `loginUser` and `SessionStore` are illustrative names; replace them with your
own symbols. You can also ask a question such as `cgr explore "how does login reach the database"`.
The response uses the indexed graph and relevant source, with line numbers and an output budget.

Register the MCP server with supported agents:

```sh
cgr install --dry-run
cgr install
```

The installer connects the agent; `cgr init` builds each project's index. When you use only
CLI commands, update the graph after edits with `cgr sync`. While an MCP server is running
with watching enabled, it performs that synchronization automatically after a debounce period.

## Language and framework support

### Source languages

| Language | Recognized supported extensions |
|---|---|
| TypeScript / TSX | `.ts`, `.mts`, `.cts`, `.tsx` |
| JavaScript / JSX | `.js`, `.mjs`, `.cjs`, `.jsx`, `.xsjs`, `.xsjslib` |
| Python | `.py`, `.pyw` |
| Go | `.go` |
| Java | `.java` |
| Rust | `.rs` |
| Kotlin | `.kt`, `.kts` |

Extraction records source structure, symbols and references. Resolution uses available scope,
import, receiver-type, module and package information. Coverage depends on language constructs
and project configuration; this is a static code graph rather than a full compiler type checker.

TypeScript/TSX factory calls propagate explicit named return types from function declarations,
variable-initialized arrow/function expressions and methods, using the factory's enclosing
declarations and imports. Types declared inside the factory body do not shadow its return
annotation. Union/intersection types, generic substitution, arrays and async/generator wrappers
remain unresolved; an unsupported annotation does not justify guessing a same-named method.
Block-level type shadowing is not modeled. These return rules use engine version 10, so syncing
an older index refreshes its facts and call edges.

### Framework and dynamic-dispatch rules

Route hooks are implemented for **Express, Flask, FastAPI, Django, Gin, Spring, Axum, Next.js,
NestJS, React Router and TanStack Router**.
Framework detection uses project manifests, so route coverage can differ when dependencies are
absent or when indexing a framework's own repository. The benchmark documents observed gaps.

Supported synthesis passes include interface/implementation dispatch, Go structural interfaces,
event-emitter handlers, React re-render links and JSX parent/child links. These are bounded
heuristics with recorded provenance; they cover specific patterns rather than arbitrary
runtime behavior.

For NestJS, literal `RouterModule.register` paths and a literal `setGlobalPrefix` on a
recognizable `NestFactory.create` application are composed for HTTP-client matching, including
statically readable exclusions. Route node names remain controller-local. For React Router,
exported route-object arrays can be consumed across imports and re-exports, including literal
parent routes and `children` arrays. Candidate route nodes are retained for exported arrays;
only arrays reached from a recognized router call participate in navigation matching. Dynamic
route configuration remains outside this cross-file rule. Go interface dispatch includes unambiguous promoted methods;
method-set inference remains a bounded static approximation.

Engine rule version 7 adds these resolution rules and synthesis passes, all bounded static rules
that link nothing when a name is ambiguous:

- **Go**: embedded standard-library types take part in interface satisfaction (the `io.Reader`,
  `*os.File`, `error`, ... method table is ported from upstream) and other outside embeds are
  opaque; a project embedding the kernel writes without its package binds through the written
  qualifier; func-field dispatch (`d.refreshf()` reaches the functions put in the field, through one
  field-to-field copy); protoc `Unimplemented*Server` stubs reach hand-written servers; Gin's
  `Context.Next` reaches registered middleware.
- **TS/JS**: calls through `const local = other` and `export const alias = realImpl` reach the
  real function, as do object-literal members (`api.run()`); conditional / `??` / `||` arms of
  module-level values (`const-alias`); ES5 constructors with `prototype` methods and
  `util.inherits` / `Object.create` inheritance; ambient modules declared with `declare module 'x'`
  in `.d.ts` files; Redux thunk `dispatch` chains and RTK Query generated hooks.
- **Rust**: generic parameters resolve through their trait bounds (an unbounded parameter no longer
  matches a project type of the same name); `impl Deref` targets, `type X = Y` aliases and
  `Mutex` / `RwLock` lock guards are looked through; `self.field` call chains are evaluated.
- **Python**: factories returning different classes in different branches are typed by the nearest
  common base; `self.x = self.make()` types a field across methods; pytest fixture parameters (same
  file or a `conftest.py` above) take the fixture's return type.
- **Cross-language / JVM**: Spring test requests (MockMvc, WebTestClient, TestRestTemplate,
  RestAssured) reach routes; Kotlin Multiplatform `expect` reaches each platform's `actual`; a Tauri
  frontend's `invoke('cmd')` reaches the registered `#[tauri::command]`; React Native
  `NativeModules.X.m()` reaches the Java / Kotlin module's `@ReactMethod`.

Not covered: MyBatis XML, Angular templates, Vue / Svelte ecosystem stores and routers, React Native
events and the Fabric / Expo native side, Tauri events, GoFrame, and Python parameter-type flow
(upstream implements it for Ruby only).

C/C++, C#, PHP, Ruby, Swift and the other upstream language families are outside this version's
supported extraction set. Vue/Svelte-specific extraction is also outside the current scope.

## Command-line reference

Use `cgr --help` or `cgr <command> --help` for the full option list. `--path` / `-p` selects a
project explicitly, for example `cgr --path /path/to/project explore "UserService save"`.
Query commands can locate an index in the current directory or an ancestor.

| Command | Purpose |
|---|---|
| `cgr init [path]` | Create `.cgr/` and build a full index |
| `cgr index [path] --timings` | Rebuild the full graph and report phase timings |
| `cgr sync [path]` | Reconcile additions, edits and deletions with the index |
| `cgr status --json --verify` | Report counts, changes and snapshot checksum verification |
| `cgr status --strict` | Return a nonzero status when the index has pending changes |
| `cgr explore <query…>` | Retrieve related source, call paths and impact |
| `cgr query <text> --kind function --json` | Search symbols, optionally filtering by kind |
| `cgr node <symbol-or-file> --code` | Inspect a symbol or file and its source |
| `cgr files` | List indexed files and structure |
| `cgr callers <symbol>` / `cgr callees <symbol>` | Follow incoming or outgoing calls |
| `cgr impact <symbol> --depth 2` | Inspect the graph neighborhood affected by a change |
| `cgr implementations <symbol>` | Find implementations or subtypes |
| `cgr affected <files…> --json` | Find test files connected to changed files |
| `cgr cycles --json` | Report import cycles; exit 1 when cycles are found |
| `cgr export --sqlite graph.sqlite` | Export a queryable SQLite graph |
| `cgr serve --mcp` / `cgr serve --http ADDR` | Start an MCP service |
| `cgr install` / `cgr uninstall` | Add or remove agent integration |
| `cgr uninit [path] --yes` | Remove the project's index |
| `cgr version --verbose` | Show build, format and kernel information |

Normal success returns 0; operational failures and conditions such as detected cycles return 1;
invalid command-line usage returns 2. Source windows are read from the current working tree,
so keeping the graph synchronized matters after edits.

## Agent and MCP integration

### Automatic configuration

`cgr install` detects supported agents and edits their existing configuration while preserving
comments and unrelated entries. Existing user-pinned commands and settings are retained;
missing integration keys are added. `uninstall` removes the owned registration and marked
instruction blocks.

```sh
cgr install --agent claude-code
cgr install --agent cursor
cgr install --agent codex
cgr install --local --agent claude-code
cgr install --no-hook
cgr uninstall --agent cursor
```

| Agent | Global registration | Project-local behavior |
|---|---|---|
| Claude Code | MCP registration, instructions, optional prompt hook and explore permission | `.mcp.json`, `CLAUDE.md`, `.claude/settings.json` |
| Cursor | `~/.cursor/mcp.json`, with workspace path passed to the server | `.cursor/mcp.json` with the project path |
| Codex | MCP registration in `~/.codex/config.toml` and instructions | `--local` edits project `AGENTS.md`; register the server globally separately |

Claude Code honors `CLAUDE_CONFIG_DIR`; Codex honors `CODEX_HOME`. Use `--dry-run` to inspect
changes and `--bin` to specify a different executable path. `--no-hook` disables registration
of the optional Claude Code prompt hook; `--no-permissions` skips the explore permission entry.

### Manual stdio configuration

For an MCP client with an `mcpServers` JSON configuration, use absolute paths appropriate to
that machine:

```json
{
  "mcpServers": {
    "codegraph-rust": {
      "command": "/absolute/path/to/cgr",
      "args": ["serve", "--mcp", "--path", "/absolute/path/to/project"]
    }
  }
}
```

Initialize that project with `cgr init` first. Clients can supply MCP roots; tools also accept
`projectPath` for selecting another indexed project. Use `--path` for clients that do not
provide a project root. Add `--no-watch` when the server should serve snapshots without
watching source changes.

By default, the tool list exposes **`codegraph-rust_explore`**. Set `CGR_MCP_TOOLS` in the server's
environment to enable additional tools:

```sh
CGR_MCP_TOOLS=explore,node,callers,callees,impact,files,search,status cgr serve --mcp
```

| Tool | Purpose |
|---|---|
| `codegraph-rust_explore` | Relevant source, named-symbol flows and impact |
| `codegraph-rust_node` | Symbol or file details and source |
| `codegraph-rust_callers` / `codegraph-rust_callees` | Call relationships |
| `codegraph-rust_impact` | Change impact neighborhood |
| `codegraph-rust_files` | Indexed file tree |
| `codegraph-rust_search` | Symbol locations |
| `codegraph-rust_status` | Index health and pending changes |

### HTTP transport

HTTP-capable clients can connect to `/mcp`:

```sh
export CGR_HTTP_TOKEN="replace-with-your-token"
cgr serve --http 127.0.0.1:7878 --path /absolute/path/to/project
```

This listens at `http://127.0.0.1:7878/mcp` and requires `Authorization: Bearer <token>` when a
token is configured. The token can also be supplied with `--token`. Keep the binding and token
appropriate for the clients that should access the project's source.

## Configuration

### Project settings

Place `cgr.json` at the project root. The upstream filename `codegraph.json` is also read;
`cgr.json` takes precedence for fields it supplies.

```json
{
  "exclude": ["**/generated/**", "**/fixtures/**"],
  "include": ["packages/shared/src/**"],
  "deprioritize": ["third_party/**"],
  "includeIgnored": false,
  "extensions": {
    ".customts": "typescript"
  }
}
```

| Field | Meaning |
|---|---|
| `exclude` | Gitignore-style patterns excluded from indexing, including tracked files |
| `include` | Re-include user-ignored paths; this is not a source-file whitelist |
| `deprioritize` | Keep matching paths indexed but rank them below first-party code |
| `includeIgnored` | Include embedded Git repositories during discovery |
| `extensions` | Map extra extensions to existing language parsers |

Explicit `exclude` rules take precedence over `include`; built-in directory skips still apply
when re-including paths. Invalid configuration produces warnings. Extension mappings select
an existing parser and do not add support for a new language.

Discovery applies ignore rules and skips common dependency/build directories. Source files
larger than 1 MiB are excluded from extraction. Generated files and test paths are recorded as
flags for downstream ranking and analysis.

### Environment variables

| Variable | Behavior |
|---|---|
| `CGR_THREADS` | Indexing worker count; the detected memory budget can reduce it |
| `CGR_MEMORY_MB` | Tuning budget for workers, batches, kernel spill and resolver caches; not an RSS cap |
| `CGR_MCP_TOOLS` | Comma-separated list of exposed MCP tools; `CODEGRAPH_MCP_TOOLS` is a fallback |
| `CGR_EXPLORE_DEDUP=1` | Skip source already returned in the same durable MCP conversation |
| `CGR_NO_PROMPT_HOOK=1` | Disable the optional Claude Code prompt hook at runtime |
| `CGR_HTTP_TOKEN` | Require a bearer token on HTTP requests |
| `CGR_LOG` | Service log filter, default `warn`; logs go to stderr |
| `CGR_TIMINGS` | Enable CLI indexing phase timing output |
| `CGR_FSYNC` | Default platform syncing; `full` selects strongest syncing, `off` skips syncing |

Defaults derive indexing resources from the host's available memory and CPU parallelism.
For example, `CGR_THREADS=4 CGR_MEMORY_MB=2048 cgr index --timings` requests a constrained
indexing configuration. Actual memory use also depends on graph size and intermediate data.

## Architecture

```text
source files → scan → parallel extraction + facts → merge → resolve + synthesize
                                                                    ↓
CLI / MCP ← query handlers ← base + delta view ← mmap segments ← publish snapshot
```

The snapshot format stores hot/cold node records, string pools, bidirectional CSR edges,
reference outcomes and search indexes. Publishing writes new immutable files and switches
the manifest; readers retain a stable snapshot, with shared locks protecting files from GC.

Incremental updates compare timestamps/sizes and content hashes, extract changed files, then
re-resolve references affected by changed definitions, imports, exports or hierarchy. One delta
is computed against the base; deltas do not accumulate as an unbounded stack. Quiet-period
compaction rewrites the live snapshot without re-extraction; broad reference changes or config
changes can still rebuild the graph. The correctness target is that the
incremental view equals a fresh full build, checked by canonical graph comparisons.

| Crate | Responsibility |
|---|---|
| `cg-core` | IDs, node/edge kinds, configuration, resource tuning and shared errors |
| `cg-kernel` | Vendored upstream extraction kernel and adaptation patches |
| `cg-scan` | File discovery, language detection and ignore rules |
| `cg-extract` | Parallel extraction, facts, error isolation and hooks |
| `cg-store` | Segment format, indexes, snapshots, publishing and garbage collection |
| `cg-resolve` | Module/export tables, reference resolution, route hooks and synthesis |
| `cg-query` | Retrieval, traversal, source windows and shared tool handlers |
| `cg-sync` | Change detection, incremental updates and watcher coordination |
| `cg-mcp` | JSON-RPC, stdio and HTTP service transports |
| `cg-install` | Agent configuration and instruction-block editing |
| `cgr` | CLI, prompt hook, SQLite export and benchmark commands |

See [Architecture](docs/ARCHITECTURE.md) for storage and algorithm details.

## Index lifecycle

- `cgr init` creates `.cgr/` and indexes the project. Its internal `.gitignore` keeps index
  contents out of version control. `init --local-only` additionally uses `.git/info/exclude`
  when available.
- CLI queries open existing snapshots directly. The MCP process stays alive for its client
  and can watch source changes; it does not require a separate database service.
- `cgr sync` updates the graph after edits. `cgr index` performs a full rebuild, including after
  changes that require refreshed engine analysis. `status` reports index version and changes.
- `cgr export --sqlite graph.sqlite` writes a separate SQLite file for analysis; it does not
  replace the native snapshot backend. Add `--force` to replace an existing export.
- `cgr uninstall` removes agent integration. `cgr uninit --yes` separately removes the project's
  index. The source tree is retained.

The index and exported graphs contain code-derived information. Keep them with the access and
retention policy appropriate to the repository.

## Development and verification

```sh
cargo test --workspace --locked
cargo clippy --workspace --exclude cg-kernel --all-targets -- -D warnings
cargo build --release --locked -p cgr
```

For faster optimized development builds, use `cargo build --profile fastrel -p cgr`. The
release profile uses fat LTO; `fastrel` reduces link work and is a separate performance profile.

Focused correctness and measurement commands include:

```sh
cgr status --verify --strict
cgr debug unresolved
cgr debug diff
cgr bench index /path/to/project --runs 3 --json
cgr bench sync /path/to/project --edits 30 --seed 1 --verify --json
cgr bench explore /path/to/project --count 100 --runs 3 --json
scripts/bench-local.sh --help
```

`bench sync` edits a temporary copy by default; `--in-place` changes that behavior. `debug diff`
compares the current graph with a fresh full build. The benchmark wrapper adds memory, disk
and swap preflight checks and process-tree monitoring; its prerequisites may require closing
other applications before a run.

The performance implementation update passed **137 tests, with one ignored**, on macOS,
plus release compilation, Clippy, formatting and the CLI/MCP/hook smoke test. All four real
projects passed full/edit/restore graph equivalence and matched the prior engine's graph dumps.
The historical benchmark revision passed 116 tests. Implementation checks are recorded in
[the engineering record](docs/PERFORMANCE-IMPLEMENTATION.md). The subsequent main retest added
128 timed commands and successful checksum/incremental-full checks in all four repositories;
see [the performance report](docs/BENCHMARKS.md#full-indexing).
Local measurements and hosted CI results are separate evidence.

Code revision `f82506e` passed all nine [hosted CI checks](https://github.com/NianJiuZst/CodeGraph-Rust/actions/runs/41286327676),
including Linux/macOS/Windows tests and end-to-end smoke, lint, kernel reproducibility and the
static Linux build. Hosted correctness results are not used as performance measurements.

The CI configuration includes formatting, Clippy, kernel reproducibility, multi-platform tests
and release-build checks. The smoke helper covers the CLI, MCP handshake and prompt hook.
To refresh the kernel, use `scripts/sync-kernel.sh <upstream-checkout> <commit>` and verify the
resulting patches and extraction tests. Vendored formatting is kept separate from workspace
formatting checks.

## Current limitations

- **Coverage parity:** supported language families, framework routes, synthesized edges and
  explore responses differ from upstream. The benchmark has a per-repository coverage table.
- **Static-analysis limits:** ambiguity can leave references unresolved; dynamic behavior is
  covered only by implemented patterns. Inspect important call paths before relying on them.
- **Memory tuning:** kernel-buffer spill and resolver cache limits release heap memory, but
  facts, graph builders and shared tables still scale with repository size. The tuning budget
  does not enforce a process RSS cap.
- **Incremental scope:** filesystem event probing is local, but inventory assembly and the
  base-relative delta still do broader in-memory work. Structural changes can trigger wider
  resolution/synthesis; background compaction with delta rebasing remains M6 work.
- **Execution deadlines:** query/hook cancellation is cooperative; a blocking filesystem call
  cannot be forcibly interrupted by the worker deadline.
- **Performance scope:** current timings cover four restricted-tier repositories. Standard-tier
  workloads, concurrent-reader latency, large-repository RSS and agent-task outcomes remain
  outside this measurement.

## Contributing

Start with [Architecture](docs/ARCHITECTURE.md) and the owning crate. Keep graph changes focused,
include a regression case for changed resolution or synchronization behavior, and compare
incremental output with a full rebuild. Document supported constructs and observed coverage
when extending a language or framework rule. Run the relevant tests and formatting/Clippy
checks before submitting changes; kernel updates should remain reproducible from their pinned
upstream commit and patches.

## Acknowledgements and license

CodeGraph Rust builds on [Colby Mchenry's CodeGraph](https://github.com/colbymchenry/codegraph)
and its Tree-sitter extraction work. The vendored upstream kernel is MIT-licensed; its pinned
revision is recorded in `crates/cg-kernel/UPSTREAM_COMMIT`.

This project is distributed under the [MIT License](LICENSE). See
[THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES) for upstream attribution and vendored components.
