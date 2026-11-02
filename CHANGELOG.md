# Changelog

## 0.1.0 — first release

### Language coverage and Rust macro items

- Vendor the upstream C/C++, C#, PHP, Ruby and Swift walkers and the Swift grammar through
  kernel patch P5 (`scripts/sync-kernel.sh` re-vendors them; each language has a cargo feature).
  A `.h` header with C++ constructs is parsed with the C++ grammar, as upstream does.
- Facts extractors for the five languages (scopes, typed parameters/locals/fields, imports,
  namespaces), and resolution rules: `#include` by relative path and path suffix, C# namespaces
  including the enclosing ones, PHP namespaced `use` names, Ruby `require`/`require_relative`,
  Swift modules per SwiftPM target; implicit-`this` member calls, call-name normalisation
  (PHP `->`/`parent::`/`self::`, C# `?.`/`base.`, Ruby `@ivar`, C++ arity suffixes), nested type
  paths, qualified callables, and members declared in other files (partial classes, extensions,
  reopened classes, out-of-line C++ definitions). Per-family builtin and common-method lists
  keep untyped receivers from linking to same-named project methods.
- Rust: items inside conditional wrapper macros (`cfg_rt! { pub struct X; impl X { .. } }`,
  Tokio's `cfg_*!` family) are extracted from a position-preserving view with only the wrapper
  tokens blanked. Node identities and positions of everything else are unchanged; the original
  text is used again if blanking introduces a syntax error. On Tokio this adds ~740 nodes and
  ~2,200 resolved call references.
- Self-review fixes: depth bounds (iterative helpers) in the five extractors; deterministic
  tie-breaking for cross-file members and qualified names; C++ `::a::f` / `::f()` resolved in the
  global namespace only (no self-links from `test::open` wrappers), `::std::..` as builtin, and
  unqualified `get`/`swap`/`move`/.. in C++ files as standard functions; PHP `parent::__construct()`
  and namespaced names; Ruby single-segment `require` limited to load-path roots; an explicit Swift
  standard-type list with nested-type shadowing; Objective-C headers recorded as file nodes; the
  `cfg_*!` pre-pass only blanks closed wrappers and only when it does not add syntax errors.
- Fix `file_node_only` building a node row 8 bytes larger than the kernel ABI row (a debug
  assertion panic on any kernel failure in debug/test builds).
- **Engine rules version 5**: indexes built by an older engine are rebuilt on the next sync.

### Engine performance follow-through

- Add event-path sync, bounded watcher events and source-free snapshot compaction after a
  quiet period. Keep full scanning/rebuild fallbacks for broad or structural changes.
- Spill accumulated kernel buffers to immutable temporary mappings; cap resolver caches and
  indexing workers by the memory tuning budget. Use the configured pool for table construction
  and lower indexing-worker priority on Linux/macOS/Windows.
- Preallocate resolution outcomes, fuse hook trigger scans, and link event keys in bounded,
  deterministic parallel chunks with selective incremental reconnection.
- Keep package/module representatives ordered by source path and track package/declaration
  import dependencies, preserving incremental/full equality when package membership changes.
  Engine revision 2 refreshes older dependency records on rebuild.
- Compress BM25 postings, persist global document lengths, and add adaptive radix edge sorting.
  **Snapshot format 2 requires rebuilding old indexes with `cgr index`.**
- Use bounded source text/line-offset views; add MCP backpressure and cooperative cancellation,
  coalesce background rebuilds, release watcher resources on LRU eviction, and bound shutdown.
- Bound prompt-hook discovery; repair smoke-helper stdin and timer cleanup on macOS Bash 3.2.
- Document implementation, correctness validation and remaining boundaries in
  [English and Chinese](docs/PERFORMANCE-IMPLEMENTATION.md). The historical 6.40× comparison is
  unchanged; a new benchmark was refused by the resource preflight, so no additional speedup
  is claimed.

### Initial milestones M0–M4

**Engine**
- Static single binary `cgr`; vendored upstream tree-sitter kernel (TypeScript/JavaScript, Python,
  Go, Java, Rust, Kotlin, C/C++, C#, PHP, Ruby, Swift), re-vendorable with `scripts/sync-kernel.sh`.
- Immutable, memory-mapped snapshot format (header, checksummed partitions, CSR edges, FST name
  indexes, trigram and BM25 indexes), published atomically; readers never block the writer.
- Reference resolution in rounds (module tables, export closure, scope/import/receiver/qualified
  strategies, "better unresolved than wrong"); route extraction for Express, Flask, FastAPI,
  Django, Gin, Spring, Axum, Next.js; synthesised dynamic-dispatch edges for interfaces and
  implementations, Go structural interfaces, event emitters, React render and JSX children.
- Incremental sync: delta layer over a base segment, re-resolution of only the affected
  references, thresholds that fall back to a full rebuild, automatic merge; the result equals a
  full rebuild (property test with random edit sequences, plus sabotage-checked rule coverage).
- File watcher with a lock-based election (one watcher per project across sessions).

**Agent surface**
- MCP server over stdio and HTTP: upstream tool schemas/descriptions/instructions with the
  `codegraph-rust_` prefix, explore-only default surface, `roots`/`--path`/`projectPath`
  precedence, success-shaped errors, 30 s call budget, in-flight calls answered on EOF.
- `cgr install` / `uninstall` for Claude Code, Cursor and Codex (comment-preserving JSONC/TOML
  edits, marker blocks, idempotent and reversible), Claude Code `UserPromptSubmit` hook.
- CLI: init, index, sync, status, query, explore, node, files, callers, callees, impact,
  implementations, affected, cycles, serve, install, uninstall, uninit, export --sqlite, version.
- Development tools: `debug dump|unresolved|diff`, `bench index|sync|explore|compare`,
  `scripts/bench-local.sh` (memory-guarded), `scripts/smoke.sh`.

**Release engineering**
- CI: rustfmt, clippy `-D warnings`, kernel re-vendoring check, tests on Linux/macOS/Windows,
  smoke test, static musl build. Release workflow for six targets with checksums; install scripts.

Not in this release: other upstream languages; Vue/Svelte;
code signing and notarisation; npm/PyPI wrappers; Homebrew tap. See the README's current limitations.
