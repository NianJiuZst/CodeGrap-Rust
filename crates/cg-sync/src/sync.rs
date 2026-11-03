//! The sync algorithm (design doc 9.3). A delta layer is always computed against the *base*
//! segment and the current source tree, so deltas never stack and "incremental equals full" is a
//! statement about one function:
//!
//!   View(base, delta(base, tree)) == fresh index of tree
//!
//! 1. diff the tree against the base (changed files C: replaced / new / removed / touched);
//! 2. re-extract C, build a provisional delta, and from it a resolution engine over the new state;
//! 3. find the references whose result may differ (affected refs) and resolve exactly those;
//! 4. write the delta: nodes/refs/facts of replaced and new files, rewritten refs and edges of
//!    unchanged files whose references changed.
//!
//! When too much is affected, or the configuration changed, the base is rebuilt instead.

use crate::detect::{diff_view, Diff};
use cg_core::budget::MemBudget;
use cg_core::config::ProjectConfig;
use cg_core::{brand, CgError, EdgeKind, FileId, LangFamily, NodeIdx, NodeKind, RefKind, Result, StrId, NONE};
use cg_extract::{fileflags, try_extract_stream, BindKind, ExtractConfig, FileBatch};
use cg_resolve::driver::{make_edge, round_of, update_of, FNREF_META};
use cg_resolve::{hooks_for, index_with_guard, resolve_refs, Engine, IndexOptions, Outcome, ProjectInfo, Round_};
use cg_scan::{scan, ScanOptions};
use cg_store::{
    assemble_delta, base_offsets, build_draft_at, outkind, status, DeltaParts, DeltaState, EdgeBuild, EdgesOnlyFile, FileRec, ProjectMeta, RefRec, RefUpdate,
    Seg, Store, View, WriteGuard,
};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default)]
pub struct SyncOptions {
    /// Rebuild the base instead of writing a delta.
    pub force_full: bool,
    pub index: IndexOptions,
    /// Exact watcher paths. `None` performs a full scan; directory/config/burst events
    /// conservatively fall back to one. Periodic full scans recover dropped events.
    pub paths: Option<Vec<PathBuf>>,
}

#[derive(Clone, Debug, Default)]
pub struct SyncReport {
    pub up_to_date: bool,
    /// A base + delta snapshot was repacked without extracting or resolving source.
    pub compacted: bool,
    /// Why the base was rebuilt instead of writing a delta.
    pub full_rebuild: Option<String>,
    pub added: usize,
    pub modified: usize,
    pub removed: usize,
    pub touched: usize,
    pub affected_files: usize,
    pub affected_refs: usize,
    pub timings: Vec<(&'static str, Duration)>,
}

impl SyncReport {
    pub fn total(&self) -> Duration {
        self.timings.iter().map(|t| t.1).sum()
    }
    pub fn summary(&self) -> String {
        if self.up_to_date {
            return "index is up to date".into();
        }
        let how = match &self.full_rebuild {
            Some(why) => format!("rebuilt the index ({why})"),
            None => format!("{} file(s) re-resolved", self.affected_files),
        };
        format!(
            "synced: {} added, {} modified, {} removed, {} touched; {how} in {:.0} ms",
            self.added,
            self.modified,
            self.removed,
            self.touched,
            self.total().as_secs_f64() * 1e3
        )
    }
}

struct Phase {
    t: Instant,
    timings: Vec<(&'static str, Duration)>,
}

impl Phase {
    fn new() -> Phase {
        Phase { t: Instant::now(), timings: Vec::new() }
    }
    fn lap(&mut self, name: &'static str) {
        let now = Instant::now();
        self.timings.push((name, now - self.t));
        self.t = now;
    }
}

/// Fraction limits from the design (9.3 step 9).
const MAX_AFFECTED_REF_PCT: usize = 30;
const MAX_AFFECTED_FILE_PCT: usize = 20;
/// Merge when the delta covers this much of the project, or after this many syncs (5.7).
const MERGE_FILE_PCT: usize = 5;
const MERGE_AFTER_SYNCS: u32 = 200;
const MERGE_BYTES: u64 = 64 << 20;
const MERGE_IDLE_MS: u64 = 10 * 60 * 1000;
const MERGE_QUIET_MS: u64 = 30 * 1000;

fn merge_due(view: &View, manifest: &cg_store::Manifest) -> bool {
    let Some(delta) = &view.delta else {
        return false;
    };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
    let idle = now.saturating_sub(manifest.updated_ms);
    let files = view.base.p.files.len();
    idle >= MERGE_QUIET_MS
        && ((files >= 40 && delta.seg.p.delta_files.len() / 3 * 100 > files * MERGE_FILE_PCT)
            || manifest.syncs_since_merge >= MERGE_AFTER_SYNCS
            || delta.seg.file_len >= MERGE_BYTES
            || idle >= MERGE_IDLE_MS)
}

fn compact_snapshot(store: &Store, guard: &WriteGuard, view: &View, rep: &mut SyncReport, ph: &mut Phase, budget: MemBudget) -> Result<()> {
    let pool = cg_resolve::index::indexing_pool(budget, true)?;
    let seg = pool.install(|| cg_store::compact::compact(view));
    ph.lap("compact");
    store.publish_base(&seg, guard)?;
    ph.lap("write");
    rep.compacted = true;
    Ok(())
}

/// Idle maintenance, called by the elected watcher. This never scans, stats or extracts
/// source files. An active writer wins immediately; maintenance retries on the next tick.
pub fn compact_if_due(root: &Path) -> Result<SyncReport> {
    let store = Store::at(root);
    let mut rep = SyncReport { up_to_date: true, ..Default::default() };
    let mut phase = Phase::new();
    let guard = match store.write_lock(Duration::ZERO) {
        Ok(guard) => guard,
        Err(CgError::Lock(_)) => return Ok(rep),
        Err(e) => return Err(e),
    };
    let snapshot = store.open_snapshot()?;
    if merge_due(&snapshot.view, &snapshot.manifest) {
        compact_snapshot(&store, &guard, &snapshot.view, &mut rep, &mut phase, MemBudget::detect())?;
        rep.timings = phase.timings;
    }
    Ok(rep)
}

fn event_paths(root: &Path, view: &View, paths: Option<&[PathBuf]>) -> Option<Vec<String>> {
    let paths = paths?;
    if paths.len() > 500 {
        return None;
    }
    let mut rels = Vec::new();
    for p in paths {
        let p = if p.is_absolute() { p.strip_prefix(root).ok()?.to_path_buf() } else { p.clone() };
        if p.components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
            return None;
        }
        let rel = p.to_str()?.replace('\\', "/");
        if rel.is_empty() || crate::watch::is_config_name(rel.rsplit('/').next()?) || root.join(&p).is_dir() {
            return None;
        }
        // A removed directory is no longer identifiable with stat.
        let prefix = format!("{rel}/");
        if view.files().any(|f| view.file(f).path().starts_with(&prefix)) {
            return None;
        }
        rels.push(rel);
    }
    rels.sort();
    rels.dedup();
    Some(rels)
}

fn scan_events(root: &Path, config: &ProjectConfig, view: &View, paths: &[String]) -> Option<Vec<cg_scan::FileEntry>> {
    let rules = cg_scan::IgnoreRules::new(root, config);
    let mut entries: BTreeMap<String, cg_scan::FileEntry> = view
        .files()
        .map(|f| {
            let file = view.file(f);
            let e = cg_scan::FileEntry { rel: file.path().to_owned(), lang: file.lang(), size: file.rec.size as u64, mtime_ns: file.rec.mtime_ns };
            (e.rel.clone(), e)
        })
        .collect();
    for rel in paths {
        match cg_scan::stat_entry(&rules, config, rel) {
            Some(e) => {
                entries.insert(rel.clone(), e);
            }
            None => {
                // A tracked-but-ignored file can be included by the full scanner's git
                // supplement; use that scanner rather than silently dropping the file.
                if entries.contains_key(rel) && root.join(rel).is_file() && rules.is_ignored(rel, false) {
                    return None;
                }
                entries.remove(rel);
            }
        }
    }
    Some(entries.into_values().collect())
}

/// Bring the index of `root` up to date.
pub fn sync(root: &Path, opts: &SyncOptions) -> Result<SyncReport> {
    let store = Store::at(root);
    if store.read_manifest()?.is_none() {
        return Err(CgError::NoIndex(root.to_path_buf()));
    }
    let guard = store.write_lock(Duration::from_secs(30))?;
    store.cleanup_tmp();
    sync_locked(&store, &guard, root, opts)
}

fn full_rebuild(store: &Store, guard: &WriteGuard, root: &Path, opts: &SyncOptions, mut rep: SyncReport, why: String) -> Result<SyncReport> {
    let r = index_with_guard(store, guard, root, &opts.index)?;
    rep.full_rebuild = Some(why);
    rep.timings.extend(r.timings.iter().copied());
    Ok(rep)
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct DefKey {
    kind: u8,
    name: String,
    qname: String,
}

#[derive(Clone, PartialEq, Eq)]
struct DefDetail {
    flags: u16,
    ret: Option<String>,
    sig: Option<String>,
    vis: u8,
}

fn defs_of(view: &View, f: FileId) -> BTreeMap<DefKey, Vec<DefDetail>> {
    let mut m: BTreeMap<DefKey, Vec<DefDetail>> = BTreeMap::new();
    for n in view.nodes_in_file(f) {
        let n = NodeIdx(n);
        let node = view.node(n);
        if !node.kind().is_definition() {
            continue;
        }
        m.entry(DefKey { kind: node.kind() as u8, name: node.name().to_owned(), qname: node.qname().to_owned() }).or_default().push(DefDetail {
            flags: view.hot(n).flags,
            ret: node.return_type().map(str::to_owned),
            sig: node.signature().map(str::to_owned),
            vis: node.visibility() as u8,
        });
    }
    m
}

type ExportSig = Vec<(String, String, String, u8)>;

type ResolverFactSig = BTreeSet<(u8, String, String, String, String)>;
type JsValueFactSig = Vec<(u8, String, String, String)>;

/// Module-level value evidence can be followed through imported aliases.
fn js_value_fact_sig(view: &View, f: FileId) -> JsValueFactSig {
    if view.file(f).lang().family() != LangFamily::Js {
        return Vec::new();
    }
    let scope = view.scope_base(f);
    view.bindings_of(f)
        .iter()
        .filter(|b| b.scope.0 == scope && (b.kind == BindKind::Local as u8 || b.kind == BindKind::Alias as u8))
        .map(|b| (b.kind, view.text_str(b.name).to_owned(), view.text_str(b.decl_type).to_owned(), view.text_str(b.init_ctor).to_owned()))
        .collect()
}

/// JS `interface X` beside `const X`: importing `X` names whichever comes first, so swapping
/// same-named declarations changes what importers link.
fn js_twin_order_sig(view: &View, f: FileId) -> Vec<(String, u8)> {
    if view.file(f).lang().family() != LangFamily::Js {
        return Vec::new();
    }
    let mut count: FxHashMap<&str, u32> = FxHashMap::default();
    for n in view.nodes_in_file(f) {
        *count.entry(view.node(NodeIdx(n)).qname()).or_default() += 1;
    }
    let mut twins: Vec<_> = view
        .nodes_in_file(f)
        .map(|n| view.node(NodeIdx(n)))
        .filter(|n| count[n.qname()] > 1)
        .map(|n| (n.start_line(), n.start_col(), n.qname().to_owned(), n.kind() as u8))
        .collect();
    twins.sort();
    twins.into_iter().map(|(_, _, q, k)| (q, k)).collect()
}

/// These facts change resolution even when the kernel's declaration identity
/// stays the same (e.g. `obj.old = function internal` -> `obj.new = ...`, or the target of a
/// Rust `impl Deref`, which has no node of its own).
fn resolver_fact_sig(view: &View, f: FileId) -> ResolverFactSig {
    let field_owners: FxHashMap<_, _> = if view.file(f).lang().family() == LangFamily::Rust {
        view.nodes_in_file(f)
            .map(|n| view.node(NodeIdx(n)))
            .filter(|n| n.kind().is_type())
            .map(|n| (cg_core::text::pack_pos(n.start_line(), n.start_col()), n.qname().to_owned()))
            .collect()
    } else {
        FxHashMap::default()
    };
    view.bindings_of(f)
        .iter()
        .filter(|b| {
            ((b.kind == BindKind::MemberFn as u8
                || b.kind == BindKind::ReturnType as u8
                || b.kind == BindKind::Fixture as u8
                || b.kind == BindKind::TypeAlias as u8
                || b.kind == BindKind::FactoryMixin as u8)
                && b.node.is_some())
                || b.kind == BindKind::Deref as u8
                || b.kind == BindKind::Mutation as u8
                || b.kind == BindKind::CommonJsObject as u8
                || b.kind == BindKind::MacroNamespace as u8
                || (b.kind == BindKind::Field as u8 && view.file(f).lang().family() == LangFamily::Rust)
        })
        .map(|b| {
            // A Rust field's declared type can redirect a call in an unchanged
            // importer. Include its owner: two structs may have the same field.
            let owner = if b.kind == BindKind::Field as u8 {
                let scope = &view.scopes_of(f)[b.scope.0.wrapping_sub(view.scope_base(f)) as usize];
                field_owners.get(&scope.start).cloned().unwrap_or_default()
            } else if b.node.is_some() {
                view.node(b.node).qname().to_owned()
            } else {
                String::new()
            };
            (b.kind, view.text_str(b.name).to_owned(), owner, view.text_str(b.decl_type).to_owned(), view.text_str(b.init_ctor).to_owned())
        })
        .collect()
}

/// What a file declares as exports (re-exports, aliases, default exports, star exports). Exported
/// definitions are covered by the definition diff and by dead-target handling.
fn export_sig(view: &View, f: FileId) -> ExportSig {
    let mut v: ExportSig = view
        .exports_of(f)
        .iter()
        .map(|e| (view.text_str(e.name).to_owned(), view.text_str(e.from_spec).to_owned(), view.text_str(e.from_name).to_owned(), e.star))
        .collect();
    v.sort();
    v
}

/// A local export can forward an imported binding (`import * as z ...; export { z }`).
/// Changing that import changes the exported module even when the export syntax is unchanged.
fn reexported_import_sig(view: &View, f: FileId) -> BTreeSet<(String, String, String, u8, u8)> {
    let locals: FxHashSet<&str> =
        view.exports_of(f).iter().filter(|e| view.text_str(e.from_spec).is_empty() && e.node.is_none()).map(|e| view.text_str(e.from_name)).collect();
    view.imports_of(f)
        .iter()
        .filter(|i| i.scope.0 == view.scope_base(f) && locals.contains(view.text_str(i.local)))
        .map(|i| (view.text_str(i.local).to_owned(), view.text_str(i.spec).to_owned(), view.text_str(i.imported).to_owned(), i.kind, i.type_only))
        .collect()
}

/// Pair the nodes of a replaced file before and after the edit by identity (kind + qualified
/// name + occurrence). A pair whose definition details are unchanged keeps its meaning, so
/// references that resolved to the old node can simply point at the new one.
fn pair_nodes(old_view: &View, new_view: &View, f: FileId, remap: &mut FxHashMap<u32, u32>) {
    type Group = Vec<(u32, Option<DefDetail>)>;
    fn groups(view: &View, f: FileId) -> BTreeMap<(u8, String), Group> {
        let mut m: BTreeMap<(u8, String), Group> = BTreeMap::new();
        for n in view.nodes_in_file(f) {
            let n = NodeIdx(n);
            let node = view.node(n);
            let detail = node.kind().is_definition().then(|| DefDetail {
                flags: view.hot(n).flags,
                ret: node.return_type().map(str::to_owned),
                sig: node.signature().map(str::to_owned),
                vis: node.visibility() as u8,
            });
            m.entry((node.kind() as u8, node.qname().to_owned())).or_default().push((n.0, detail));
        }
        m
    }
    let old = groups(old_view, f);
    let new = groups(new_view, f);
    for (k, olds) in &old {
        let Some(news) = new.get(k) else { continue };
        if olds.len() != news.len() {
            continue;
        }
        for ((o, od), (n, nd)) in olds.iter().zip(news) {
            if od == nd {
                remap.insert(*o, *n);
            }
        }
    }
}

fn is_hierarchy_kind(kind: u8) -> bool {
    matches!(RefKind::from_wire(kind), Some(RefKind::Edge(EdgeKind::Extends | EdgeKind::Implements)))
}

/// Hierarchy as a set of id- and position-independent (sub, super) keys.
///
/// A key is `path#kind@qualified-name` plus the occurrence number among nodes of the same file
/// that share kind and name, so that inserting a line above a class does not look like a change
/// of the class hierarchy (which would widen the re-resolution to every reference in the
/// project), while a renamed class, a moved class or a changed parent still does.
fn hierarchy_keys(view: &View, pairs: impl Iterator<Item = (NodeIdx, NodeIdx)>) -> BTreeSet<(String, String)> {
    let mut ordinals: FxHashMap<u32, FxHashMap<u32, u32>> = FxHashMap::default();
    let mut key = |n: NodeIdx| -> String {
        let node = view.node(n);
        let file = node.file();
        let ord = ordinals.entry(file.0).or_insert_with(|| {
            let mut seen: FxHashMap<(u8, &str), u32> = FxHashMap::default();
            let mut out: FxHashMap<u32, u32> = FxHashMap::default();
            for m in view.nodes_in_file(file) {
                let nm = view.node(NodeIdx(m));
                let c = seen.entry((nm.kind() as u8, nm.qname())).or_insert(0);
                out.insert(m, *c);
                *c += 1;
            }
            out
        });
        let dead = if view.is_alive(n) { "" } else { "!dead" };
        format!("{}#{}@{}~{}{dead}", view.file(file).path(), node.kind(), node.qname(), ord.get(&n.0).copied().unwrap_or(0))
    };
    pairs.map(|(a, b)| (key(a), key(b))).collect()
}

pub fn sync_locked(store: &Store, guard: &WriteGuard, root: &Path, opts: &SyncOptions) -> Result<SyncReport> {
    let mut rep = SyncReport::default();
    let mut ph = Phase::new();
    let budget = MemBudget::configured(opts.index.memory_mb, opts.index.threads);

    let snap = match store.open_snapshot() {
        Ok(s) => s,
        Err(CgError::NoIndex(p)) => return Err(CgError::NoIndex(p)),
        Err(e) => return full_rebuild(store, guard, root, opts, rep, format!("snapshot unreadable: {e}")),
    };
    let manifest = snap.manifest.clone();
    let base: Arc<Seg> = snap.view.base.clone();
    if opts.force_full {
        return full_rebuild(store, guard, root, opts, rep, "requested".into());
    }
    if base.meta.engine_version != brand::ENGINE_VERSION {
        return full_rebuild(store, guard, root, opts, rep, "index built by an older engine".into());
    }

    let config = ProjectConfig::load(root);
    let paths = event_paths(root, &snap.view, opts.paths.as_deref());
    let project = match &paths {
        Some(_) => ProjectInfo::load_known(root, &base.meta.config_digests.keys().cloned().collect::<Vec<_>>()),
        None => ProjectInfo::load(root, &config),
    };
    if base.meta.config_digest != config.digest() {
        return full_rebuild(store, guard, root, opts, rep, "project configuration changed".into());
    }
    if base.meta.config_digests != project.config_hashes {
        return full_rebuild(store, guard, root, opts, rep, "a build/config file (tsconfig, package.json, go.mod, Cargo.toml, ...) changed".into());
    }

    // ---- 1. diff ----
    let mut sopts = ScanOptions::new(root);
    sopts.config = config.clone();
    let entries = paths.as_ref().and_then(|p| scan_events(root, &config, &snap.view, p)).unwrap_or_else(|| scan(&sopts).files);
    ph.lap("scan");
    let current = diff_view(&snap.view, root, &entries);
    if current.is_empty() {
        if merge_due(&snap.view, &manifest) {
            compact_snapshot(store, guard, &snap.view, &mut rep, &mut ph, budget)?;
        }
        rep.up_to_date = true;
        rep.timings = ph.timings;
        return Ok(rep);
    }
    let base_view = View::new(base.clone()).with_root(root.to_path_buf());
    let d: Diff = diff_view(&base_view, root, &entries);
    ph.lap("diff");
    if d.is_empty() {
        // the tree equals the base again
        store.clear_delta(guard)?;
        rep.up_to_date = true;
        rep.timings = ph.timings;
        return Ok(rep);
    }
    rep.added = d.added.len();
    rep.modified = d.replaced.len();
    rep.removed = d.removed.len();
    rep.touched = d.touched.len();
    let n_base_files = base.p.files.len();

    // ---- 2. re-extract the changed files ----
    let pool = cg_resolve::index::indexing_pool(budget, opts.index.low_priority)?;
    let replaced_ids: FxHashMap<&str, FileId> = d.replaced.iter().map(|(f, e)| (e.rel.as_str(), *f)).collect();
    let mut entries: Vec<_> = d.replaced.iter().map(|x| x.1.clone()).chain(d.added.iter().cloned()).collect();
    entries.sort_by(|a, b| a.rel.as_bytes().cmp(b.rel.as_bytes()));
    let mut xcfg = ExtractConfig::new(root);
    xcfg.threads = budget.threads;
    xcfg.batch_bytes = budget.extract_batch_bytes();
    xcfg.overrides = config.extensions.clone();
    xcfg.low_priority = opts.index.low_priority;
    xcfg.set_hooks(hooks_for(&project));
    let deprio = cg_resolve::index::deprioritize_matcher(root, &config);
    let mut extracted: Vec<Option<FileBatch>> = (0..entries.len()).map(|_| None).collect();
    let mut resident = 0u64;
    let spill_dir = root.join(brand::DATA_DIR).join("tmp");
    try_extract_stream(&entries, &xcfg, |i, b| {
        if let Some(mut b) = b {
            if deprio.as_ref().is_some_and(|g| g.matched_path_or_any_parents(&entries[i].rel, false).is_ignore()) {
                b.flags |= fileflags::DEPRIORITIZED;
            }
            resident += cg_extract::spill::resident_bytes(&b);
            extracted[i] = Some(b);
            if resident >= budget.extract / 2 {
                cg_extract::spill::spill(extracted.iter_mut().filter_map(Option::as_mut), &spill_dir)?;
                resident = 0;
            }
        }
        Ok::<_, std::io::Error>(())
    })?;
    ph.lap("extract");

    let mut batches: Vec<FileBatch> = Vec::new();
    let mut states: Vec<DeltaState> = Vec::new();
    let mut ids: Vec<FileId> = Vec::new();
    let mut removed: Vec<FileId> = d.removed.clone();
    let mut next_new = n_base_files as u32;
    for (i, e) in entries.iter().enumerate() {
        match (extracted[i].take(), replaced_ids.get(e.rel.as_str())) {
            (Some(b), Some(id)) => {
                batches.push(b);
                states.push(DeltaState::Replaced);
                ids.push(*id);
            }
            (Some(b), None) => {
                batches.push(b);
                states.push(DeltaState::New);
                ids.push(FileId(next_new));
                next_new += 1;
            }
            (None, Some(id)) => removed.push(*id), // no longer extractable
            (None, None) => {}
        }
    }
    removed.sort();
    removed.dedup();
    let replaced: Vec<FileId> = ids.iter().zip(&states).filter(|(_, s)| **s == DeltaState::Replaced).map(|(f, _)| *f).collect();
    let mut dead_nodes: Vec<u32> = Vec::new();
    for f in replaced.iter().chain(removed.iter()) {
        let r = &base.p.files[f.idx()];
        dead_nodes.extend(r.node_start.0..r.node_start.0 + r.node_count);
    }
    let c_files: FxHashSet<u32> = ids.iter().chain(removed.iter()).map(|f| f.0).collect();

    // ---- provisional delta -> resolution engine over the new state ----
    let bb = base_offsets(&base);
    let meta =
        ProjectMeta { config_digest: base.meta.config_digest, frameworks: base.meta.frameworks.clone(), config_digests: base.meta.config_digests.clone() };
    let draft = pool.install(|| build_draft_at(&batches, Some((&ids, bb)), &meta));
    drop(batches);
    let removed_recs: Vec<(FileId, FileRec)> = removed.iter().map(|f| (*f, base.p.files[f.idx()])).collect();
    let provisional = |draft: &cg_store::Draft, indexes: bool| DeltaParts {
        draft: draft.duplicate(),
        states: states.clone(),
        ids: ids.clone(),
        removed: removed_recs.clone(),
        edges_only: Vec::new(),
        ref_updates: Vec::new(),
        edges: Vec::new(),
        emeta: Vec::new(),
        import_targets: Vec::new(),
        export_targets: Vec::new(),
        sites: Vec::new(),
        site_ranges: Vec::new(),
        dead_base_nodes: dead_nodes.clone(),
        total_files: 0,
        total_nodes: 0,
        total_edges: 0,
        base_gen: manifest.base_gen,
        indexes,
    };
    let v1_seg = assemble_delta(&base, provisional(&draft, false));
    let view1 = base_view.with_delta(Arc::new(v1_seg))?;
    ph.lap("draft");
    let mut engine = pool.install(|| Engine::build_with_budget(&view1, &project, budget));
    ph.lap("tables");

    // ---- 3. affected references ----
    let n_base_refs = base.p.refs.len() as u32;
    let n_draft_refs = draft.seg.p.refs.len() as u32;
    let mut affected: FxHashSet<u32> = FxHashSet::default();
    for f in ids.iter() {
        for id in view1.ref_range(*f) {
            affected.insert(id);
        }
    }
    // definition / export differences of replaced, new and removed files
    let mut d_names: BTreeSet<String> = BTreeSet::new();
    let mut definitions_changed = false;
    let mut export_changed: Vec<FileId> = Vec::new();
    let mut remap: FxHashMap<u32, u32> = FxHashMap::default();
    // pytest injects a fixture by parameter name, with no import: a changed fixture re-resolves
    // the Python files of its directory and below
    let mut fixture_dirs: Vec<String> = Vec::new();
    let fixture_dir = |view: &View, f: FileId, facts: &ResolverFactSig| -> Option<String> {
        facts.iter().any(|fact| fact.0 == BindKind::Fixture as u8).then(|| cg_resolve::project::parent_dir(view.file(f).path()).to_owned())
    };
    for (i, f) in ids.iter().enumerate() {
        let old = if states[i] == DeltaState::Replaced { defs_of(&base_view, *f) } else { BTreeMap::new() };
        let new = defs_of(&view1, *f);
        definitions_changed |= old != new;
        if view1.file(*f).lang().family() == LangFamily::Js
            && old.keys().chain(new.keys()).any(|key| {
                matches!(NodeKind::from_u8(key.kind), Some(NodeKind::Function | NodeKind::Method | NodeKind::Component)) && old.get(key) != new.get(key)
            })
        {
            // An exported value may depend on a function's return annotation.
            export_changed.push(*f);
        }
        let old_facts = if states[i] == DeltaState::Replaced { resolver_fact_sig(&base_view, *f) } else { BTreeSet::new() };
        let new_facts = resolver_fact_sig(&view1, *f);
        let old_values = if states[i] == DeltaState::Replaced { js_value_fact_sig(&base_view, *f) } else { Vec::new() };
        if old_values != js_value_fact_sig(&view1, *f) {
            export_changed.push(*f);
        }
        if old_facts != new_facts {
            definitions_changed = true;
            d_names.extend(old_facts.iter().chain(&new_facts).map(|fact| fact.1.clone()));
            export_changed.push(*f);
            fixture_dirs.extend(fixture_dir(&view1, *f, &new_facts).or_else(|| fixture_dir(&view1, *f, &old_facts)));
        }
        for (k, details) in &new {
            // a changed signature can move a call to another overload of the name, even one
            // whose target kept its identity
            if old.get(k) != Some(details) {
                d_names.insert(k.name.clone());
            }
        }
        for k in old.keys() {
            if !new.contains_key(k) {
                d_names.insert(k.name.clone());
            }
        }
        if states[i] == DeltaState::Replaced {
            pair_nodes(&base_view, &view1, *f, &mut remap);
            if export_sig(&base_view, *f) != export_sig(&view1, *f)
                || reexported_import_sig(&base_view, *f) != reexported_import_sig(&view1, *f)
                || js_twin_order_sig(&base_view, *f) != js_twin_order_sig(&view1, *f)
            {
                export_changed.push(*f);
            }
        }
    }
    for f in &removed {
        let facts = resolver_fact_sig(&base_view, *f);
        fixture_dirs.extend(fixture_dir(&base_view, *f, &facts));
        d_names.extend(facts.into_iter().map(|fact| fact.1));
        for k in defs_of(&base_view, *f).keys() {
            d_names.insert(k.name.clone());
        }
        export_changed.push(*f);
    }
    let tail_ids: FxHashSet<u32> = d_names.iter().filter_map(|n| base.names.find(n.as_bytes())).map(|s| s.0).collect();
    let is_dead = {
        let mut bits = vec![0u64; base.p.nodes_hot.len().div_ceil(64)];
        for n in &dead_nodes {
            bits[*n as usize / 64] |= 1u64 << (*n % 64);
        }
        move |n: u32| (n as usize / 64) < bits.len() && bits[n as usize / 64] >> (n % 64) & 1 == 1
    };
    // consumers (direct importers, through re-exporting barrels) of files whose exports changed
    let mut consumer_files: FxHashSet<u32> = FxHashSet::default();
    if !export_changed.is_empty() {
        let live: Vec<FileId> = view1.files().collect();
        let mut importers: FxHashMap<u32, Vec<u32>> = FxHashMap::default();
        let mut value_forwarders: FxHashSet<u32> = FxHashSet::default();
        for g in &live {
            if view1.file(*g).lang().family() == LangFamily::Js {
                let scope = view1.scope_base(*g);
                let values: FxHashSet<&str> = view1
                    .bindings_of(*g)
                    .iter()
                    .filter(|b| {
                        b.scope.0 == scope
                            && (b.kind == BindKind::Local as u8 || b.kind == BindKind::Alias as u8)
                            && (!view1.text_str(b.decl_type).is_empty() || !view1.text_str(b.init_ctor).is_empty())
                    })
                    .map(|b| view1.text_str(b.name))
                    .collect();
                if engine.exports[g.idx()].values().any(|e| match e {
                    cg_resolve::ExportEntry::Node(n) if view1.hot(*n).file == *g => values.contains(view1.node(*n).name()),
                    _ => false,
                }) {
                    value_forwarders.insert(g.0);
                }
            }
            let mut srcs: FxHashSet<u32> = FxHashSet::default();
            for t in &engine.imp_targets[g.idx()] {
                match t {
                    cg_resolve::ImportTarget::Pkg(pkg) => srcs.extend(engine.modules.pkgs[pkg.0 as usize].files.iter().map(|file| file.0)),
                    _ => {
                        if let Some(file) = engine.modules.target_file(*t) {
                            srcs.insert(file.0);
                        }
                    }
                }
            }
            for x in &engine.stars[g.idx()] {
                srcs.insert(x.0);
            }
            for e in engine.exports[g.idx()].values() {
                match e {
                    cg_resolve::ExportEntry::Re(x, _) | cg_resolve::ExportEntry::Module(x) => {
                        srcs.insert(x.0);
                    }
                    _ => {}
                }
            }
            for x in srcs {
                importers.entry(x).or_default().push(g.0);
            }
        }
        // removed files are not in `live`: their importers come from the base facts
        for f in &removed {
            for g in base_view.files() {
                if base_view.imports_of(g).iter().any(|i| i.target == *f) || base_view.exports_of(g).iter().any(|x| x.target == *f) {
                    importers.entry(f.0).or_default().push(g.0);
                }
            }
        }
        let mut frontier: Vec<u32> = export_changed.iter().map(|f| f.0).collect();
        let mut seen: FxHashSet<u32> = frontier.iter().copied().collect();
        while let Some(x) = frontier.pop() {
            for g in importers.get(&x).into_iter().flatten() {
                consumer_files.insert(*g);
                // a barrel passes the change on
                let base_reexports = |g: u32, x: u32| -> bool {
                    (g as usize) < base.p.files.len() && {
                        let ff = base.p.file_facts[g as usize];
                        base.p.exports[ff.exp_start as usize..(ff.exp_start + ff.exp_count) as usize].iter().any(|e| e.target.0 == x)
                    }
                };
                let reexports = engine.stars[*g as usize].iter().any(|y| y.0 == x)
                    || engine.exports[*g as usize]
                        .values()
                        .any(|e| matches!(e, cg_resolve::ExportEntry::Re(y, _) | cg_resolve::ExportEntry::Module(y) if y.0 == x))
                    || base_reexports(*g, x);
                if (reexports || value_forwarders.contains(g)) && seen.insert(*g) {
                    frontier.push(*g);
                }
            }
        }
    }
    for dir in &fixture_dirs {
        for (fi, rec) in base.p.files.iter().enumerate() {
            let path = base.paths.get(rec.path);
            if path.ends_with(".py") && (dir.is_empty() || path.starts_with(&format!("{dir}/"))) {
                consumer_files.insert(fi as u32);
            }
        }
    }
    let all_refs_of_file = |f: u32, affected: &mut FxHashSet<u32>| {
        let rec = &base.p.files[f as usize];
        for id in rec.ref_start..rec.ref_start + rec.ref_count {
            affected.insert(id);
        }
    };
    let mut retarget: FxHashMap<u32, u32> = FxHashMap::default();
    let debug = std::env::var_os("CGR_SYNC_DEBUG").is_some();
    let (mut hit_imports, mut hit_tail, mut hit_dead, mut hit_consumer) = (0usize, 0usize, 0usize, 0usize);
    for (fi, rec) in base.p.files.iter().enumerate() {
        if c_files.contains(&(fi as u32)) {
            continue;
        }
        if consumer_files.contains(&(fi as u32)) {
            hit_consumer += 1;
            all_refs_of_file(fi as u32, &mut affected);
            continue;
        }
        // A file with one affected reference is re-resolved as a whole: references depend on
        // each other through local bindings and receiver types (`x = f(); x.m()`).
        let mut file_hit = false;
        // an import now resolving to another file (or to none) changes what the file binds
        let facts = base.p.file_facts[fi];
        for (i, old) in base.p.imports[facts.imp_start as usize..(facts.imp_start + facts.imp_count) as usize].iter().enumerate() {
            let new_t = engine.imp_targets[fi].get(i).and_then(|t| engine.modules.target_file(*t)).map(|file| view1.file(file).path());
            let old_t = old.target.opt().map(|t| base.paths.get(base.p.files[t.idx()].path));
            if new_t != old_t {
                file_hit = true;
                hit_imports += 1;
                if debug {
                    eprintln!("sync: import #{i} of {} now {:?} (was {:?})", base.paths.get(rec.path), new_t, old_t);
                }
                break;
            }
        }
        if !file_hit {
            for i in 0..rec.ref_count {
                let r = &base.p.refs[(rec.ref_start + i) as usize];
                let by_tail = tail_ids.contains(&r.tail.0);
                let by_dead = r.status == status::RESOLVED && is_dead(r.target.0) && !remap.contains_key(&r.target.0);
                if by_tail || by_dead {
                    file_hit = true;
                    hit_tail += by_tail as usize;
                    hit_dead += by_dead as usize;
                    if debug {
                        eprintln!("sync: {} hit by {} ref {:?}", base.paths.get(rec.path), if by_tail { "tail" } else { "dead target" }, base.text.get(r.name));
                    }
                    break;
                }
            }
        }
        if file_hit {
            all_refs_of_file(fi as u32, &mut affected);
        } else {
            for i in 0..rec.ref_count {
                let id = rec.ref_start + i;
                let r = &base.p.refs[id as usize];
                if r.status == status::RESOLVED {
                    if let Some(n) = remap.get(&r.target.0) {
                        retarget.insert(id, *n);
                    }
                }
            }
        }
    }
    if debug {
        eprintln!(
            "sync: files hit: imports {hit_imports}, tail {hit_tail}, dead {hit_dead}, consumers {hit_consumer}; remapped nodes {}; d_names {:?}",
            remap.len(),
            d_names
        );
    }
    // (the files' own refs of C files are in `affected` already via view1.ref_range)

    // ---- 3b. resolve, widening when imports or the hierarchy change ----
    let all_files: Vec<FileId> = view1.files().collect();
    let file_of = |id: u32| -> FileId { view1.hot(view1.reference(id).from).file };
    let mut outcomes: FxHashMap<u32, Outcome> = FxHashMap::default();
    {
        // round 1: imports, supertypes, type references
        let todo: FxHashSet<u32> = affected.iter().copied().filter(|id| round_of(view1.reference(*id).kind) == Round_::One).collect();
        if !todo.is_empty() {
            let mut files: Vec<FileId> = todo.iter().map(|id| file_of(*id)).collect();
            files.sort();
            files.dedup();
            let res = pool.install(|| resolve_refs(&engine, &files, Round_::One, Some(&todo)));
            for (id, o) in res {
                outcomes.insert(id, o);
            }
        }
    }
    // hierarchy from all resolved extends / implements refs
    let mut pairs: Vec<(NodeIdx, NodeIdx)> = Vec::new();
    for &f in &all_files {
        let range = view1.ref_range(f);
        for (i, r) in view1.refs_of_file(f).iter().enumerate() {
            if !is_hierarchy_kind(r.kind) {
                continue;
            }
            let id = range.start + i as u32;
            let (st, tg) = match outcomes.get(&id) {
                Some(o) => (o.status, o.target),
                None => (r.status, retarget.get(&id).copied().unwrap_or(r.target.0)),
            };
            if st == status::RESOLVED {
                pairs.push((r.from, NodeIdx(tg)));
            }
        }
    }
    let old_pairs = {
        let mut v = Vec::new();
        for r in base.p.refs.iter() {
            if is_hierarchy_kind(r.kind) && r.status == status::RESOLVED {
                v.push((r.from, r.target));
            }
        }
        hierarchy_keys(&base_view, v.into_iter())
    };
    let new_pairs = hierarchy_keys(&view1, pairs.iter().copied());
    let hierarchy_changed = old_pairs != new_pairs;
    if debug && hierarchy_changed {
        for p in old_pairs.difference(&new_pairs).take(5) {
            eprintln!("sync: hierarchy only before: {p:?}");
        }
        for p in new_pairs.difference(&old_pairs).take(5) {
            eprintln!("sync: hierarchy only after: {p:?}");
        }
    }
    engine.set_hierarchy(pairs.into_iter());
    if hierarchy_changed {
        for &f in &all_files {
            let range = view1.ref_range(f);
            for (i, r) in view1.refs_of_file(f).iter().enumerate() {
                if round_of(r.kind) == Round_::Two {
                    affected.insert(range.start + i as u32);
                }
            }
        }
    }
    let todo2: FxHashSet<u32> = affected.iter().copied().filter(|id| round_of(view1.reference(*id).kind) == Round_::Two).collect();
    if !todo2.is_empty() {
        let mut files: Vec<FileId> = todo2.iter().map(|id| file_of(*id)).collect();
        files.sort();
        files.dedup();
        let res = pool.install(|| resolve_refs(&engine, &files, Round_::Two, Some(&todo2)));
        for (id, o) in res {
            outcomes.insert(id, o);
        }
    }
    ph.lap("resolve");

    // ---- synthesised edges: recomputed over the new state, compared with the base ----
    let synth = pool.install(|| {
        cg_resolve::synth::run_incremental(
            &engine,
            &base_view,
            &c_files,
            &remap,
            hierarchy_changed || definitions_changed || !export_changed.is_empty() || hit_imports > 0 || !removed.is_empty(),
        )
    });
    let mut synth_by_file: FxHashMap<u32, Vec<usize>> = FxHashMap::default();
    for (i, e) in synth.edges.iter().enumerate() {
        synth_by_file.entry(view1.hot(NodeIdx(e.src)).file.0).or_default().push(i);
    }
    let mut synth_dirty: BTreeSet<u32> = BTreeSet::new();
    {
        // synthesised edges of the base, grouped by the file of their source
        let mut base_synth: FxHashMap<u32, Vec<(u32, u32, usize)>> = FxHashMap::default(); // file -> (src, dst, eid)
        for (eid, kb) in base.p.out_kind.iter().enumerate() {
            if outkind::class(*kb) == outkind::CLASS_SYNTH {
                let src = base.p.out_off.partition_point(|o| *o as usize <= eid) as u32 - 1;
                base_synth.entry(base.p.nodes_hot[src as usize].file.0).or_default().push((src, base.p.out_dst[eid], eid));
            }
        }
        let new_key = |e: &EdgeBuild| -> String {
            format!(
                "{}>{}|{}|{}|{}|{}",
                cg_store::dump::node_key(&view1, NodeIdx(e.src)),
                cg_store::dump::node_key(&view1, NodeIdx(e.dst)),
                e.kind,
                e.line,
                e.col,
                synth.emeta.get(e.meta as usize).map_or("", |m| m.as_str())
            )
        };
        let mut files: BTreeSet<u32> = synth_by_file.keys().copied().collect();
        files.extend(base_synth.keys().copied());
        for f in files {
            if f as usize >= n_base_files || c_files.contains(&f) {
                continue; // draft file, replaced or removed: their edges are rewritten anyway
            }
            let old = base_synth.get(&f).map(|v| v.as_slice()).unwrap_or(&[]);
            let mut old_keys: Vec<String> = Vec::with_capacity(old.len());
            let mut dirty = false;
            for (src, dst, eid) in old {
                if is_dead(*dst) {
                    dirty = true;
                    break;
                }
                let kb = base.p.out_kind[*eid];
                let site = base.p.out_site[*eid];
                let meta = base_view.edge_meta(*eid as u32).unwrap_or("");
                old_keys.push(format!(
                    "{}>{}|{}|{}|{}|{}",
                    cg_store::dump::node_key(&base_view, NodeIdx(*src)),
                    cg_store::dump::node_key(&base_view, NodeIdx(*dst)),
                    outkind::kind(kb) as u8,
                    site.line,
                    site.col,
                    meta
                ));
            }
            if !dirty {
                let mut new_keys: Vec<String> = synth_by_file.get(&f).map(|v| v.iter().map(|i| new_key(&synth.edges[*i])).collect()).unwrap_or_default();
                old_keys.sort();
                new_keys.sort();
                dirty = old_keys != new_keys;
            }
            if dirty {
                synth_dirty.insert(f);
            }
        }
    }

    // ---- thresholds ----
    let affected_base_files: BTreeSet<u32> =
        affected.iter().filter(|id| **id < n_base_refs).map(|id| file_of(*id).0).filter(|f| !c_files.contains(f)).collect();
    rep.affected_refs = affected.len();
    rep.affected_files = affected_base_files.len() + ids.len();
    if affected.len() * 100 > (n_base_refs as usize).max(1) * MAX_AFFECTED_REF_PCT && n_base_refs > 2000 {
        let why = format!("{} of {} references affected", affected.len(), n_base_refs);
        return full_rebuild(store, guard, root, opts, rep, why);
    }
    if rep.affected_files * 100 > n_base_files.max(1) * MAX_AFFECTED_FILE_PCT && n_base_files >= 40 {
        let why = format!("{} of {} files affected", rep.affected_files, n_base_files);
        return full_rebuild(store, guard, root, opts, rep, why);
    }

    // ---- 4. assemble the delta ----
    // edges-only files: unchanged content, rewritten refs/edges (plus touched files)
    let mut eo_set: BTreeSet<u32> = affected_base_files.clone();
    eo_set.extend(synth_dirty.iter().copied());
    for id in retarget.keys() {
        let f = base.p.files.partition_point(|r| r.ref_start + r.ref_count <= *id);
        eo_set.insert(f as u32);
    }
    for (f, _) in &d.touched {
        if !c_files.contains(&f.0) {
            eo_set.insert(f.0);
        }
    }
    let touched_recs: FxHashMap<u32, &cg_scan::FileEntry> = d.touched.iter().map(|(f, e)| (f.0, e)).collect();
    let mut eo_start: FxHashMap<u32, u32> = FxHashMap::default();
    let mut next = n_base_refs + n_draft_refs;
    for f in &eo_set {
        eo_start.insert(*f, next);
        next += base.p.files[*f as usize].ref_count;
    }
    let final_ref = |id: u32| -> u32 {
        if id >= n_base_refs {
            return id;
        }
        let f = base.p.files.partition_point(|r| r.ref_start + r.ref_count <= id);
        // files with no refs share a ref_start; `partition_point` lands on the file owning `id`
        let rec = &base.p.files[f];
        eo_start[&(f as u32)] + (id - rec.ref_start)
    };
    let mut emeta: Vec<String> = vec![FNREF_META.to_owned()];
    let mut emeta_map: FxHashMap<String, u32> = FxHashMap::default();
    emeta_map.insert(FNREF_META.to_owned(), 0);
    let mut edges: Vec<EdgeBuild> = Vec::new();

    // draft files: one edge per resolved reference
    for id in n_base_refs..n_base_refs + n_draft_refs {
        let o = outcomes.get(&id).copied().unwrap_or(Outcome::PENDING);
        if let Some(e) = make_edge(&engine, id, &o, 0) {
            edges.push(e);
        }
    }
    let ref_updates: Vec<RefUpdate> =
        (n_base_refs..n_base_refs + n_draft_refs).map(|id| update_of(&outcomes.get(&id).copied().unwrap_or(Outcome::PENDING))).collect();
    let import_targets: Vec<u32> =
        ids.iter().flat_map(|f| engine.imp_targets[f.idx()].iter().map(|t| engine.modules.target_file(*t).map_or(NONE, |file| file.0))).collect();

    let export_targets: Vec<u32> = ids.iter().flat_map(|f| view1.exports_of(*f).iter().map(|ex| engine.export_target(*f, ex)).collect::<Vec<_>>()).collect();
    let mut edges_only: Vec<EdgesOnlyFile> = Vec::new();
    for &f in &eo_set {
        let rec = base.p.files[f as usize];
        let mut refs: Vec<RefRec> = base.p.refs[rec.ref_start as usize..(rec.ref_start + rec.ref_count) as usize].to_vec();
        for (i, r) in refs.iter_mut().enumerate() {
            let id = rec.ref_start + i as u32;
            if let Some(o) = outcomes.get(&id) {
                let u = update_of(o);
                r.status = u.status;
                r.by = u.by;
                r.target = NodeIdx(u.target);
            } else if let Some(n) = retarget.get(&id) {
                r.target = NodeIdx(*n);
            }
        }
        // out-edges: extraction edges and untouched resolution edges are copied
        for n in rec.node_start.0..rec.node_start.0 + rec.node_count {
            for e in base_view.out_edges(NodeIdx(n)) {
                let keep = match e.class {
                    outkind::CLASS_EXTRACT => true,
                    outkind::CLASS_RESOLVE => e.ref_idx.is_some_and(|r| !outcomes.contains_key(&r)),
                    _ => false,
                };
                if !keep {
                    continue;
                }
                let meta = match base_view.edge_meta(e.eid) {
                    Some(m) => *emeta_map.entry(m.to_owned()).or_insert_with(|| {
                        emeta.push(m.to_owned());
                        (emeta.len() - 1) as u32
                    }),
                    None => NONE,
                };
                let dst = match e.ref_idx.and_then(|r| retarget.get(&r)) {
                    Some(nt) => *nt,
                    None => e.dst.0,
                };
                edges.push(EdgeBuild {
                    src: n,
                    dst,
                    kind: outkind::pack(e.kind, e.provenance, e.class),
                    line: e.line.unwrap_or(NONE),
                    col: e.col.unwrap_or(NONE),
                    res: e.res,
                    ref_idx: e.ref_idx.map_or(NONE, final_ref),
                    meta,
                    order: 0,
                });
            }
        }
        for i in 0..rec.ref_count {
            let id = rec.ref_start + i;
            if let Some(o) = outcomes.get(&id) {
                if let Some(mut e) = make_edge(&engine, id, o, 0) {
                    e.ref_idx = final_ref(id);
                    edges.push(e);
                }
            }
        }
        let mut new_rec = rec;
        if let Some(entry) = touched_recs.get(&f) {
            new_rec.mtime_ns = entry.mtime_ns;
            new_rec.size = entry.size as u32;
        }
        edges_only.push(EdgesOnlyFile { file: FileId(f), rec: new_rec, refs });
    }

    // synthesised edges: those whose source file is rewritten in this delta
    let synth_meta_base = emeta.len() as u32;
    emeta.extend(synth.emeta.iter().cloned());
    let rewritten: FxHashSet<u32> = ids.iter().map(|f| f.0).chain(eo_set.iter().copied()).collect();
    for (f, idxs) in &synth_by_file {
        if !rewritten.contains(f) {
            continue;
        }
        for i in idxs {
            let mut e = synth.edges[*i];
            if e.meta != NONE {
                e.meta += synth_meta_base;
            }
            edges.push(e);
        }
    }

    // totals for the manifest
    let dead_count = dead_nodes.len() as u32;
    let total_nodes = base.p.nodes_hot.len() as u32 - dead_count + draft.seg.p.nodes_hot.len() as u32;
    let total_files = view1.files().count() as u32;
    let mut dropped_edges = 0u32;
    for f in replaced.iter().map(|f| f.0).chain(removed.iter().map(|f| f.0)).chain(eo_set.iter().copied()) {
        let rec = &base.p.files[f as usize];
        for n in rec.node_start.0..rec.node_start.0 + rec.node_count {
            dropped_edges += base.p.out_off[n as usize + 1] - base.p.out_off[n as usize];
        }
    }
    let parts = DeltaParts {
        draft: draft.duplicate(),
        states: states.clone(),
        ids: ids.clone(),
        removed: removed_recs.clone(),
        edges_only,
        ref_updates,
        edges,
        emeta,
        import_targets,
        export_targets,
        sites: Vec::new(),
        site_ranges: Vec::new(),
        dead_base_nodes: dead_nodes.clone(),
        total_files,
        total_nodes,
        total_edges: 0,
        base_gen: manifest.base_gen,
        indexes: true,
    };
    let mut delta = assemble_delta(&base, parts);
    delta.meta.n_edges = base.meta.n_edges - dropped_edges + delta.p.out_dst.len() as u32;
    ph.lap("assemble");
    // Publishing a source edit never waits for compaction. The quiet-period maintenance
    // repacks this snapshot later, under the same writer lock.
    store.publish_delta(&delta, guard)?;
    ph.lap("write");
    rep.timings = ph.timings;
    Ok(rep)
}

#[allow(dead_code)]
fn _unused(_: StrId) {}
