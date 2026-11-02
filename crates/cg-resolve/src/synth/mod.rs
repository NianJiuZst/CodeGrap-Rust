//! Synthesised edges for dynamic dispatch (design doc 7). Every pass is a pure function of the
//! view (hierarchy, members, persisted sites), so the same code serves full builds and the
//! incremental sync, which re-runs it and rewrites the files whose synthesised edges changed.

pub mod bridges;
pub mod dynamic;
pub mod gin;
pub mod go;
pub mod go_fields;
pub mod go_std;
pub mod js_alias;
pub mod js_proto;
pub mod jvm;
pub mod redux;
pub mod scan;
pub mod sites;

use crate::engine::Engine;
use crate::resolve::{Accept, NameRes, WorkerCx};
use cg_core::{EdgeKind, FileId, Lang, LangFamily, NodeIdx, NodeKind, Provenance, NONE};
use cg_store::{outkind, EdgeBuild, View};
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use sites::{PASS_EMITTER, PASS_JSX, PASS_REACT_RENDER, ROLE_EMIT, ROLE_ON};
use std::collections::BTreeMap;

#[derive(Default)]
pub struct SynthOutput {
    pub edges: Vec<EdgeBuild>,
    pub emeta: Vec<String>,
}

/// Fan-out ceilings: a key with more targets is skipped entirely (precision first, design 7.2).
/// Overrides linked per (type, supertype) pair (upstream `MAX_CALLBACKS_PER_CHANNEL`).
const MAX_IMPLS_PER_PAIR: usize = 40;
const MAX_EMITTERS: usize = 8;
const MAX_HANDLERS: usize = 8;
/// Implementers linked per Go interface, in file order (upstream `MAX_CALLBACKS_PER_CHANNEL`).
const MAX_GO_IMPLEMENTERS: usize = 40;

#[derive(Default)]
pub(crate) struct Out {
    edges: Vec<EdgeBuild>,
    emeta: Vec<String>,
    meta_ids: FxHashMap<String, u32>,
}

impl Out {
    fn append(&mut self, other: Out) {
        let remap: Vec<_> = other
            .emeta
            .into_iter()
            .map(|meta| {
                *self.meta_ids.entry(meta.clone()).or_insert_with(|| {
                    self.emeta.push(meta);
                    (self.emeta.len() - 1) as u32
                })
            })
            .collect();
        self.edges.extend(other.edges.into_iter().map(|mut edge| {
            if edge.meta != NONE {
                edge.meta = remap[edge.meta as usize];
            }
            edge
        }));
    }
    /// An edge with its metadata object as is (the passes that carry more than one extra key).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn push_meta(&mut self, kind: EdgeKind, src: NodeIdx, dst: NodeIdx, line: u32, col: u32, meta: serde_json::Value, order: u16) {
        if src == dst {
            return;
        }
        let json = meta.to_string();
        let id = *self.meta_ids.entry(json.clone()).or_insert_with(|| {
            self.emeta.push(json);
            (self.emeta.len() - 1) as u32
        });
        self.edges.push(EdgeBuild {
            src: src.0,
            dst: dst.0,
            kind: outkind::pack(kind, Provenance::Heuristic, outkind::CLASS_SYNTH),
            line: if line == 0 { NONE } else { line },
            col,
            res: 0,
            ref_idx: NONE,
            meta: id,
            order,
        });
    }

    #[allow(clippy::too_many_arguments)] // one call site per synthesis pass; a builder would only add noise
    pub(crate) fn push(
        &mut self,
        kind: EdgeKind,
        src: NodeIdx,
        dst: NodeIdx,
        line: u32,
        by: &str,
        registered_at: Option<String>,
        extra: Option<(&str, &str)>,
        order: u16,
    ) {
        if src == dst {
            return;
        }
        let mut m = serde_json::Map::new();
        m.insert("synthesizedBy".into(), by.into());
        if let Some(r) = registered_at {
            m.insert("registeredAt".into(), r.into());
        }
        if let Some((k, v)) = extra {
            m.insert(k.into(), v.into());
        }
        let json = serde_json::Value::Object(m).to_string();
        let id = *self.meta_ids.entry(json.clone()).or_insert_with(|| {
            self.emeta.push(json);
            (self.emeta.len() - 1) as u32
        });
        self.edges.push(EdgeBuild {
            src: src.0,
            dst: dst.0,
            kind: outkind::pack(kind, Provenance::Heuristic, outkind::CLASS_SYNTH),
            line: if line == 0 { NONE } else { line },
            col: NONE,
            res: 0,
            ref_idx: NONE,
            meta: id,
            order,
        });
    }
}

fn path_line(view: &View, f: FileId, line: u32) -> String {
    format!("{}:{}", view.file(f).path(), line)
}

/// Languages whose supertype methods dispatch to a subtype's same-named methods.
fn overrides_lang(l: Lang) -> bool {
    matches!(l.family(), LangFamily::Jvm | LangFamily::Js | LangFamily::CSharp | LangFamily::Swift) || matches!(l, Lang::Go | Lang::Rust)
}

/// The methods of `t` called `name`: its body's, or for Go / Rust the receiver and impl
/// methods declared outside it.
fn own_methods(e: &Engine<'_>, t: NodeIdx, name: &str) -> Vec<NodeIdx> {
    let view = e.view;
    let mut v: Vec<NodeIdx> = view
        .out_edges(t)
        .filter(|ed| ed.kind == EdgeKind::Contains)
        .map(|ed| ed.dst)
        .filter(|m| view.node(*m).kind() == NodeKind::Method && view.node(*m).name() == name)
        .collect();
    if v.is_empty() && matches!(view.file(view.node(t).file()).lang(), Lang::Go | Lang::Rust) {
        v.extend(e.direct_member(t, name).filter(|m| view.node(*m).kind().is_callable()));
    }
    v
}

/// Of an overloaded name, the methods that can override `base`: Java, Kotlin and TypeScript
/// make an override repeat the parameters it overrides. Unreadable signatures stay in.
fn overrides_of(e: &Engine<'_>, base: NodeIdx, impls: &[NodeIdx]) -> Vec<NodeIdx> {
    let view = e.view;
    let params = |n: NodeIdx| -> Option<Vec<String>> {
        let lang = view.file(view.node(n).file()).lang();
        crate::overload::parse_params(lang, view.node(n).signature()?).map(|p| p.items.iter().map(|i| i.ty.clone()).collect())
    };
    let Some(want) = params(base) else { return impls.to_vec() };
    let (mut same, mut maybe) = (Vec::new(), Vec::new());
    for &m in impls {
        match params(m) {
            None => maybe.push(m),
            Some(have) if have.len() != want.len() => {}
            Some(have) => {
                if have == want {
                    same.push(m);
                } else if have.iter().zip(&want).all(|(a, b)| a == b || a.is_empty() || b.is_empty() || a.len() == 1 || b.len() == 1) {
                    maybe.push(m); // a type parameter on either side
                }
            }
        }
    }
    if same.is_empty() {
        maybe
    } else {
        same
    }
}

/// A call through a supertype's method lands on what a subtype declares for it (upstream
/// `interfaceOverrideEdges`): per concrete type and each direct supertype, every base method
/// links to the type's own methods of that name, overloads paired by their parameters,
/// a JS static only to a static. Capped per (type, supertype) pair. Go's structural and
/// promoted method dispatch is handled together by `go_implements`.
fn interface_impl(e: &Engine<'_>, out: &mut Out) {
    let view = e.view;
    let mut sups: FxHashMap<NodeIdx, Vec<NodeIdx>> = FxHashMap::default();
    for (sub, list) in &e.supers {
        sups.entry(*sub).or_default().extend(list.iter().copied());
    }
    let mut types: Vec<NodeIdx> = sups.keys().copied().collect();
    types.sort();
    for cls in types {
        let cn = view.node(cls);
        let lang = view.file(cn.file()).lang();
        // Go structural matching and dispatch share the same promoted method set.
        if lang == Lang::Go {
            continue;
        }
        let concrete = matches!(cn.kind(), NodeKind::Class | NodeKind::Struct | NodeKind::Union)
            || (lang == Lang::Go && cn.kind() == NodeKind::TypeAlias)
            || (lang == Lang::Rust && cn.kind() == NodeKind::Enum);
        if !concrete || !overrides_lang(lang) {
            continue;
        }
        let mut bases = sups[&cls].clone();
        bases.sort();
        bases.dedup();
        for base in bases {
            let bn = view.node(base);
            let blang = view.file(bn.file()).lang();
            if base == cls || !overrides_lang(blang) || (lang == Lang::Go && bn.kind() != NodeKind::Interface) {
                continue;
            }
            let methods_of = |t: NodeIdx| {
                let mut ms: Vec<NodeIdx> = view
                    .out_edges(t)
                    .filter(|ed| ed.kind == EdgeKind::Contains)
                    .map(|ed| ed.dst)
                    .filter(|m| view.node(*m).kind() == NodeKind::Method)
                    .collect();
                // The cap below keeps a slice of these: upstream's slice is in id order.
                if matches!(blang.family(), LangFamily::CSharp | LangFamily::Swift) {
                    ms.sort_by_key(|m| view.cold(*m).digest);
                }
                ms
            };
            let mut base_methods: Vec<NodeIdx> = methods_of(base);
            // A Swift protocol's requirements are not methods: its default implementations live
            // in its extensions, the class-kind nodes that share its name.
            if blang == Lang::Swift && bn.kind() == NodeKind::Interface {
                let exts = view.by_name(bn.name()).filter(|n| view.node(*n).kind() == NodeKind::Class && view.file(view.node(*n).file()).lang() == Lang::Swift);
                base_methods.extend(exts.flat_map(methods_of));
            }
            let mut added = 0;
            for &bm in &base_methods {
                if added >= MAX_IMPLS_PER_PAIR {
                    break;
                }
                let bmn = view.node(bm);
                let mut impls = own_methods(e, cls, bmn.name());
                if lang == Lang::Go && !bmn.name().starts_with(|c: char| c.is_uppercase()) {
                    // an unexported method matches only in the interface's package
                    let dir = crate::project::parent_dir(view.file(bmn.file()).path());
                    impls.retain(|m| crate::project::parent_dir(view.file(view.node(*m).file()).path()) == dir);
                }
                if impls.is_empty() {
                    continue;
                }
                let overloaded = impls.len() > 1 || base_methods.iter().filter(|o| view.node(**o).name() == bmn.name()).count() > 1;
                let chosen = if overloaded { overrides_of(e, bm, &impls) } else { impls };
                for m in chosen {
                    if added >= MAX_IMPLS_PER_PAIR || m == bm {
                        continue;
                    }
                    let mn = view.node(m);
                    if lang.family() == LangFamily::Js && (bmn.is_static() == Some(true)) != (mn.is_static() == Some(true)) {
                        continue;
                    }
                    let at = format!("{}:{}", view.file(mn.file()).path(), mn.start_line());
                    out.push(EdgeKind::Calls, bm, m, bmn.start_line(), "interface-impl", Some(at), Some(("via", mn.name())), order_of("interface-impl"));
                    added += 1;
                }
            }
        }
    }
}

/// C++: a virtual call stops at the base method; link it to the same-named method of each class
/// that extends the base. Over-approximates, as upstream's `cppOverrideEdges` does.
fn cpp_override(e: &Engine<'_>, out: &mut Out) {
    let view = e.view;
    let is_cpp = |t: NodeIdx| view.file(view.node(t).file()).lang() == Lang::Cpp;
    // the cap keeps a slice of these and a repeated name keeps the last: upstream's are in id order
    let methods_of = |t: NodeIdx| {
        let mut ms: Vec<NodeIdx> =
            view.out_edges(t).filter(|ed| ed.kind == EdgeKind::Contains).map(|ed| ed.dst).filter(|m| view.node(*m).kind() == NodeKind::Method).collect();
        ms.sort_by_key(|m| view.cold(*m).digest);
        ms
    };
    let mut types: Vec<NodeIdx> = e.supers.keys().copied().filter(|t| view.node(*t).kind() == NodeKind::Class && is_cpp(*t)).collect();
    types.sort();
    for cls in types {
        let subs = methods_of(cls);
        let mut bases: Vec<NodeIdx> = e.supers[&cls].iter().copied().filter(|b| *b != cls && is_cpp(*b)).collect();
        bases.sort();
        bases.dedup();
        for base in bases {
            let by_name: FxHashMap<&str, NodeIdx> = methods_of(base).into_iter().map(|m| (view.node(m).name(), m)).collect();
            let mut added = 0;
            for &m in &subs {
                if added >= MAX_IMPLS_PER_PAIR {
                    break;
                }
                let mn = view.node(m);
                let Some(&bm) = by_name.get(mn.name()).filter(|bm| **bm != m) else { continue };
                let at = format!("{}:{}", view.file(mn.file()).path(), mn.start_line());
                out.push(EdgeKind::Calls, bm, m, view.node(bm).start_line(), "cpp-override", Some(at), Some(("via", mn.name())), order_of("cpp-override"));
                added += 1;
            }
        }
    }
}

/// `.emit('evt')` in a method -> the named handlers registered with `.on('evt', handler)`.
fn event_emitter<'e, 'v>(e: &'e Engine<'v>, out: &mut Out, wcx: &mut WorkerCx<'e, 'v>, selected: Option<&FxHashSet<String>>) {
    if selected.is_some_and(|keys| keys.is_empty()) {
        return;
    }
    let view = e.view;
    #[derive(Default)]
    struct Key<'a> {
        emitters: Vec<NodeIdx>,
        handlers: Vec<(NodeIdx, &'a str, u32, FileId)>,
    }
    let mut keys: BTreeMap<&str, Key<'_>> = BTreeMap::new();
    for f in view.files() {
        for s in view.sites_of(f) {
            if s.pass != PASS_EMITTER {
                continue;
            }
            let event = view.text_str(cg_core::StrId(s.key_a));
            if selected.is_some_and(|keys| !keys.contains(event)) {
                continue;
            }
            let k = keys.entry(event).or_default();
            if s.role == ROLE_EMIT {
                k.emitters.push(s.node);
            } else if s.role == ROLE_ON {
                k.handlers.push((s.node, view.text_str(s.payload), s.line, f));
            }
        }
    }
    for k in keys.values_mut() {
        k.emitters.sort();
        k.emitters.dedup();
    }
    let connect = |event: &str, k: &Key<'_>, wcx: &mut WorkerCx<'e, 'v>| {
        let mut out = Out::default();
        if k.emitters.is_empty() || k.emitters.len() > MAX_EMITTERS {
            return out;
        }
        let mut targets: Vec<(NodeIdx, u32, FileId)> = Vec::new();
        for (reg, handler, line, f) in &k.handlers {
            // `this.onFoo` / `obj.onFoo` / `onFoo`: resolve the last segment where it was written
            let name = handler.rsplit('.').next().unwrap_or(handler);
            let cx = wcx.file_cx(e, *f);
            let t = if handler.starts_with("this.") {
                view.node(*reg)
                    .parent()
                    .and_then(|c| e.direct_member(c, name))
                    .or_else(|| view.node(*reg).parent().and_then(|c| view.node(c).parent()).and_then(|c| e.direct_member(c, name)))
            } else if !handler.contains('.') {
                match cx.resolve_name(wcx, name, Accept::Callable, 0, u32::MAX, None, false) {
                    NameRes::Found(n, ..) => Some(n),
                    _ => None,
                }
            } else {
                None
            };
            if let Some(t) = t {
                if view.node(t).kind().is_callable() {
                    targets.push((t, *line, *f));
                }
            }
        }
        targets.sort();
        targets.dedup_by_key(|t| t.0);
        if targets.is_empty() || targets.len() > MAX_HANDLERS {
            return out;
        }
        for em in &k.emitters {
            for (t, line, f) in &targets {
                out.push(EdgeKind::Calls, *em, *t, 0, "event-emitter", Some(path_line(view, *f, *line)), Some(("event", event)), order_of("event-emitter"));
            }
        }
        out
    };
    // Indexed collection and ordered merging preserve pass/key precedence regardless of
    // scheduling. Bound in-flight link outputs to 256 keys; small edits stay sequential.
    if keys.len() < 32 || rayon::current_num_threads() == 1 {
        for (event, key) in &keys {
            out.append(connect(event, key, wcx));
        }
    } else {
        let keys: Vec<_> = keys.into_iter().collect();
        let per_key = (MAX_EMITTERS * MAX_HANDLERS * (std::mem::size_of::<EdgeBuild>() + 256)) as u64;
        let keys_per_chunk = (e.budget.synth / 4 / per_key).clamp(1, 256) as usize;
        for chunk in keys.chunks(keys_per_chunk) {
            let parts: Vec<_> = chunk.par_iter().map_init(WorkerCx::new, |cx, (event, key)| connect(event, key, cx)).collect();
            for part in parts {
                out.append(part);
            }
        }
    }
}

/// A React class component's method that calls `setState` re-renders the component.
fn react_render(e: &Engine<'_>, out: &mut Out, selected: Option<&FxHashSet<u32>>) {
    let view = e.view;
    for f in view.files() {
        if selected.is_some_and(|files| !files.contains(&f.0)) {
            continue;
        }
        let sites: Vec<_> = view.sites_of(f).iter().filter(|s| s.pass == PASS_REACT_RENDER).collect();
        if sites.is_empty() {
            continue;
        }
        let components: FxHashSet<NodeIdx> = sites.iter().filter(|s| s.role == 1).map(|s| s.node).collect();
        for s in sites.iter().filter(|s| s.role == 0) {
            let m = s.node;
            let Some(class) = view.node(m).parent() else { continue };
            if !components.contains(&class) {
                continue;
            }
            if let Some(render) = e.direct_member(class, "render") {
                if render != m {
                    out.push(EdgeKind::Calls, m, render, 0, "react-render", Some(path_line(view, f, s.line)), None, order_of("react-render"));
                }
            }
        }
    }
}

/// A component's JSX tag renders the child component it names.
fn jsx_render<'e, 'v>(e: &'e Engine<'v>, out: &mut Out, wcx: &mut WorkerCx<'e, 'v>, selected: Option<&FxHashSet<u32>>) {
    let view = e.view;
    for f in view.files() {
        if selected.is_some_and(|files| !files.contains(&f.0)) {
            continue;
        }
        let sites: Vec<_> = view.sites_of(f).iter().filter(|s| s.pass == PASS_JSX).collect();
        if sites.is_empty() {
            continue;
        }
        let cx = wcx.file_cx(e, f);
        for s in sites {
            let tag = view.text_str(cg_core::StrId(s.key_a));
            if let NameRes::Found(t, ..) = cx.resolve_name(wcx, tag, Accept::Callable, 0, u32::MAX, None, false) {
                let k = view.node(t).kind();
                if matches!(k, NodeKind::Function | NodeKind::Class | NodeKind::Component | NodeKind::Variable | NodeKind::Constant) && t != s.node {
                    out.push(EdgeKind::Calls, s.node, t, 0, "jsx-render", Some(path_line(view, f, s.line)), Some(("via", tag)), order_of("jsx-render"));
                }
            }
        }
    }
}

/// Run every pass over the engine's view (hierarchy installed).
pub fn run_all(e: &Engine<'_>) -> SynthOutput {
    let mut out = Out { edges: Vec::new(), emeta: Vec::new(), meta_ids: FxHashMap::default() };
    let mut wcx = WorkerCx::new();
    go::go_implements(e, &mut out);
    interface_impl(e, &mut out);
    cpp_override(e, &mut out);
    event_emitter(e, &mut out, &mut wcx, None);
    react_render(e, &mut out, None);
    jsx_render(e, &mut out, &mut wcx, None);
    run_global(e, &mut out, &mut wcx);
    SynthOutput { edges: out.edges, emeta: out.emeta }
}

/// Passes whose keys span files (routes against navigations, a proxy against the routes it
/// covers, a channel's two ends): cheap over persisted sites, so an incremental run
/// recomputes them whole.
const GLOBAL_PASSES: &[&str] = &[
    "next-navigation",
    "react-router-navigation",
    "tanstack-navigation",
    "next-link",
    "react-router-link",
    "tanstack-link",
    "next-proxy",
    "callback",
    "object-registry",
    "http-client",
    "queue-job",
    "event-bus",
    "celery-dispatch",
    "spring-event",
    "mediatr-dispatch",
    "go-func-field",
    "go-grpc-stub-impl",
    "gin-middleware-chain",
    "const-alias",
    "redux-thunk",
    "rtk-query",
    "test-request",
    "kotlin-expect-actual",
    "tauri-command",
    "rn-native-module",
];

/// Merge order of a synthesis pass (upstream `SYNTH_PASSES`): of two synthesized edges
/// between the same two symbols the earlier pass's stays. Go's fixed steps and navigation
/// calls (resolved references upstream) stand outside the merge.
pub(crate) fn order_of(by: &str) -> u16 {
    match by {
        "go-implements" | "go-method-contains" => 10,
        "callback" => 100,
        "http-client" | "queue-job" | "event-bus" => 120,
        "tauri-command" => 125,
        "test-request" => 130,
        "event-emitter" => 150,
        "react-render" => 160,
        "jsx-render" => 170,
        "cpp-override" => 255,
        "interface-impl" => 260,
        "kotlin-expect-actual" => 270,
        "go-grpc-stub-impl" => 280,
        "rn-native-module" => 320,
        "gin-middleware-chain" => 340,
        "redux-thunk" => 350,
        "const-alias" => 360,
        "rtk-query" => 400,
        "object-registry" => 390,
        "celery-dispatch" => 430,
        "spring-event" => 440,
        "mediatr-dispatch" => 445,
        "go-func-field" => 480,
        "next-link" => 510,
        "next-proxy" => 520,
        "react-router-link" => 530,
        "tanstack-link" => 540,
        _ => 600,
    }
}

/// Keep the first synthesized edge per (source, target) in merge order.
fn dedupe_pairs(out: &mut Out) {
    let in_merge = |o: u16| (100..600).contains(&o);
    let mut idx: Vec<usize> = (0..out.edges.len()).filter(|i| in_merge(out.edges[*i].order)).collect();
    idx.sort_by_key(|i| {
        let e = &out.edges[*i];
        (e.order, e.src, e.dst, e.line, e.col, out.emeta.get(e.meta as usize).cloned().unwrap_or_default())
    });
    let mut seen: FxHashSet<(u32, u32)> = FxHashSet::default();
    let mut drop = vec![false; out.edges.len()];
    for i in idx {
        let e = &out.edges[i];
        if !seen.insert((e.src, e.dst)) {
            drop[i] = true;
        }
    }
    let mut k = 0;
    out.edges.retain(|_| {
        let keep = !drop[k];
        k += 1;
        keep
    });
}

fn run_global<'e, 'v>(e: &'e Engine<'v>, out: &mut Out, wcx: &mut WorkerCx<'e, 'v>) {
    dynamic::callbacks(e, out, wcx);
    dynamic::cross_tier(e, out, wcx);
    redux::thunks(e, out, wcx);
    js_alias::const_aliases(e, out, wcx);
    dynamic::registries(e, out, wcx);
    redux::rtk_query(e, out);
    dynamic::celery(e, out);
    dynamic::spring_events(e, out);
    dynamic::mediatr_dispatch(e, out);
    bridges::link(e, out);
    jvm::test_requests(e, out);
    jvm::kotlin_expect_actual(e, out);
    go_fields::func_fields(e, out, wcx);
    go::grpc_stubs(e, out);
    gin::middleware_chain(e, out);
    crate::frameworks::nav::navigation(e, out);
    crate::frameworks::nav::next_proxy(e, out);
    dedupe_pairs(out);
}

/// Reconnect only changed event keys / per-file render sites when the definition and type
/// tables did not change. Structural changes deliberately use the full pass set.
pub fn run_incremental(e: &Engine<'_>, previous: &View, changed: &FxHashSet<u32>, remap: &FxHashMap<u32, u32>, structural: bool) -> SynthOutput {
    // Go aliases and type parameters change which implementers fit without changing a node
    let go_types = |view: &View| view.files().filter(|f| changed.contains(&f.0)).any(|f| view.sites_of(f).iter().any(|s| s.pass == sites::PASS_GO_TYPES));
    if structural || go_types(previous) || go_types(e.view) {
        return run_all(e);
    }
    let mut keys = FxHashSet::default();
    for view in [previous, e.view] {
        for f in view.files().filter(|f| changed.contains(&f.0)) {
            for s in view.sites_of(f).iter().filter(|s| s.pass == PASS_EMITTER) {
                keys.insert(view.text_str(cg_core::StrId(s.key_a)).to_owned());
            }
        }
    }
    let mut out = Out { edges: Vec::new(), emeta: Vec::new(), meta_ids: FxHashMap::default() };
    // Unchanged keys keep their existing edges, including all provenance and registration
    // locations. Ids of replaced nodes are remapped even when their meaning did not change.
    for f in previous.files() {
        for n in previous.nodes_in_file(f) {
            for edge in previous.out_edges(NodeIdx(n)).filter(|edge| edge.class == outkind::CLASS_SYNTH) {
                let meta = previous.edge_meta(edge.eid).unwrap_or("{}");
                let value: serde_json::Value = serde_json::from_str(meta).unwrap_or_default();
                let by = value.get("synthesizedBy").and_then(|v| v.as_str()).unwrap_or("");
                let recompute = match by {
                    "event-emitter" => value.get("event").and_then(|v| v.as_str()).is_some_and(|key| keys.contains(key)),
                    "react-render" | "jsx-render" => changed.contains(&f.0),
                    // registration sites name lines of other files: recomputed whole, as cheap
                    "interface-impl" | "cpp-override" | "go-implements" => true,
                    by => GLOBAL_PASSES.contains(&by),
                };
                if recompute {
                    continue;
                }
                let src = remap.get(&edge.src.0).copied().unwrap_or(edge.src.0);
                let dst = remap.get(&edge.dst.0).copied().unwrap_or(edge.dst.0);
                if !e.view.is_alive(NodeIdx(src)) || !e.view.is_alive(NodeIdx(dst)) {
                    continue;
                }
                let mid = *out.meta_ids.entry(meta.to_owned()).or_insert_with(|| {
                    out.emeta.push(meta.to_owned());
                    (out.emeta.len() - 1) as u32
                });
                out.edges.push(EdgeBuild {
                    src,
                    dst,
                    kind: outkind::pack(edge.kind, edge.provenance, edge.class),
                    line: if changed.contains(&f.0) && matches!(by, "interface-impl" | "cpp-override" | "go-implements" | "go-method-contains") {
                        e.view.hot(NodeIdx(src)).start_line
                    } else {
                        edge.line.unwrap_or(NONE)
                    },
                    col: edge.col.unwrap_or(NONE),
                    res: edge.res,
                    ref_idx: NONE,
                    meta: mid,
                    order: order_of(by),
                });
            }
        }
    }
    let mut wcx = WorkerCx::new();
    go::go_implements(e, &mut out);
    interface_impl(e, &mut out);
    cpp_override(e, &mut out);
    event_emitter(e, &mut out, &mut wcx, Some(&keys));
    react_render(e, &mut out, Some(changed));
    jsx_render(e, &mut out, &mut wcx, Some(changed));
    run_global(e, &mut out, &mut wcx);
    SynthOutput { edges: out.edges, emeta: out.emeta }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cg_core::Lang;

    #[test]
    fn jsx_regex_finds_capitalised_tags_only() {
        let re = sites::SiteScanner;
        let _ = (&re, Lang::Jsx);
    }
}
