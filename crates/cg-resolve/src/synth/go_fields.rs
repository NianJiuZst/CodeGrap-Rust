//! Go func-field dispatch (upstream `go-func-field-synthesizer.ts`): a struct holds a function
//! in a field and calls it through the field (`d.refreshf(ctx)`), which no method answers. The
//! call reaches every function or method value the field is given:
//!
//! * a composite literal, keyed `S{F: v}` or positional `S{a, v}`;
//! * an assignment `x.F = v` / `x.y.F = v`;
//! * one level of field-to-field copy: `S{F: o.G}` gives `S.F` what `T.G` is given directly.
//!
//! A field is a func field when its type is a literal `func(...)` or a project type declared as
//! one (`type HandlerFunc func(...)`). A call or an assignment counts only where the receiver's
//! type is known; one field hop is allowed, and a field one embedded struct promotes. A value is
//! what the reference written there resolves to, else the method `m` of `x.m`'s type (an
//! interface's, whose interface-impl edges carry on). A conversion to a func type is the value it
//! converts. Generated files are not read, and a value a package's test puts in a field links no
//! call outside that package's directory.

use super::sites::{PASS_GO_FIELDS, ROLE_GO_FIELD_ASSIGN, ROLE_GO_FIELD_LITERAL, ROLE_GO_FUNC_FIELD, ROLE_GO_FUNC_TYPE};
use super::{order_of, Out};
use crate::engine::Engine;
use crate::project::parent_dir;
use crate::resolve::{Accept, FileCx, NameRes, TypeRes, WorkerCx};
use crate::shape::{parse_shape, Shape};
use cg_core::{EdgeKind, FileId, Lang, NodeIdx, NodeKind, RefKind, StrId, NONE};
use cg_extract::fileflags;
use cg_store::{status, BindingRec, SiteRec, View};
use rustc_hash::{FxHashMap, FxHashSet};

const MAX_VALUES_PER_FIELD: usize = 40;

/// A function put in a field.
#[derive(Clone)]
struct Value {
    target: NodeIdx,
    file: FileId,
    line: u32,
    test: bool,
    /// The field the value was copied from (`Options.RefreshF`).
    via: Option<String>,
}

type Channel = (NodeIdx, String);

fn text(view: &View, id: u32) -> &str {
    view.text_str(StrId(id))
}

fn is_fn(view: &View, n: NodeIdx) -> bool {
    matches!(view.node(n).kind(), NodeKind::Function | NodeKind::Method)
}

/// The type `x` (or `x.y`, one field hop) holds where it is written.
fn receiver_type<'e, 'v>(cx: &FileCx<'e, 'v>, wcx: &mut WorkerCx<'e, 'v>, receiver: &str, scope: u32, line: u32) -> Option<NodeIdx> {
    let (root, hop) = match receiver.split_once('.') {
        Some((a, b)) => (a, Some(b)),
        None => (receiver, None),
    };
    let b = cx.lookup_binding(root, scope, line, false).or_else(|| cx.file_binding(root))?;
    let mut t = cx.binding_type(wcx, b)?;
    if let Some(h) = hop {
        t = cx.field_type(wcx, t, h)?;
    }
    Some(t)
}

/// What the value written at `line` names: the resolved reference there, else the method of
/// `x.m`'s type, a package's function `pkg.F` or a function in scope.
fn value_target<'e, 'v>(e: &'e Engine<'v>, cx: &FileCx<'e, 'v>, wcx: &mut WorkerCx<'e, 'v>, value: &str, scope: u32, line: u32) -> Option<NodeIdx> {
    let view = e.view;
    let tail = value.rsplit('.').next().unwrap_or(value);
    let mut hit = None;
    for r in view.refs_of_file(cx.file) {
        if r.line != line {
            continue;
        }
        if !matches!(RefKind::from_wire(r.kind), Some(RefKind::FunctionRef | RefKind::Edge(EdgeKind::References))) {
            continue;
        }
        let name = view.text_str(r.name);
        if name != value && name != tail {
            continue;
        }
        let o = cx.resolve_ref(wcx, r);
        if o.status != status::RESOLVED {
            continue;
        }
        let t = NodeIdx(o.target);
        if hit.is_some_and(|h| h != t) {
            return None;
        }
        hit = Some(t);
    }
    if let Some(t) = hit.filter(|t| is_fn(view, *t)) {
        return Some(t);
    }
    match value.split_once('.') {
        Some((root, m)) if !m.contains('.') => {
            if let Some(b) = cx.lookup_binding(root, scope, line, false).or_else(|| cx.file_binding(root)) {
                let t = cx.binding_type(wcx, b)?;
                return e.find_member(t, m).filter(|n| is_fn(view, *n));
            }
            match cx.resolve_name(wcx, root, Accept::Any, scope, line, None, false) {
                NameRes::Pkg(p) => e.pkg_defs.get(p.0 as usize)?.get(m)?.iter().copied().find(|n| is_fn(view, *n)),
                _ => None,
            }
        }
        None => match cx.resolve_name(wcx, value, Accept::Callable, scope, line, None, true) {
            NameRes::Found(n, ..) if is_fn(view, n) => Some(n),
            _ => None,
        },
        _ => None,
    }
}

pub(crate) fn func_fields<'e, 'v>(e: &'e Engine<'v>, out: &mut Out, wcx: &mut WorkerCx<'e, 'v>) {
    let view = e.view;
    let go_files: Vec<FileId> = view.files().filter(|f| view.file(*f).lang() == Lang::Go).collect();
    let generated = |f: FileId| view.file(f).flags() & fileflags::GENERATED != 0;
    let mut func_types: FxHashSet<(&str, &str)> = FxHashSet::default();
    let mut fields: FxHashMap<NodeIdx, FxHashSet<&str>> = FxHashMap::default();
    let mut writes: Vec<(FileId, &SiteRec)> = Vec::new();
    for &f in &go_files {
        let dir = parent_dir(view.file(f).path());
        for s in view.sites_of(f).iter().filter(|s| s.pass == PASS_GO_FIELDS) {
            match s.role {
                ROLE_GO_FUNC_TYPE => {
                    func_types.insert((dir, text(view, s.key_a)));
                }
                ROLE_GO_FUNC_FIELD if s.node.0 != NONE => {
                    fields.entry(s.node).or_default().insert(text(view, s.key_a));
                }
                ROLE_GO_FIELD_LITERAL | ROLE_GO_FIELD_ASSIGN if !generated(f) => writes.push((f, s)),
                _ => {}
            }
        }
    }
    if writes.is_empty() {
        return;
    }
    let is_func_type = |n: NodeIdx| -> bool {
        let node = view.node(n);
        func_types.contains(&(parent_dir(view.file(node.file()).path()), node.name()))
    };
    // fields declared with a project func type (`h HandlerFunc`)
    let mut declared: FxHashMap<NodeIdx, Vec<&'v BindingRec>> = FxHashMap::default();
    let type_names: FxHashSet<&str> = func_types.iter().map(|(_, n)| *n).collect();
    for &f in &go_files {
        let structs: Vec<NodeIdx> = view.nodes_in_file(f).map(NodeIdx).filter(|n| view.node(*n).kind() == NodeKind::Struct).collect();
        if structs.is_empty() {
            continue;
        }
        let cx = wcx.file_cx(e, f);
        for t in structs {
            let list = cx.declared_fields(t);
            for b in &list {
                let dt = view.text_str(b.decl_type);
                if dt.is_empty() || !type_names.contains(dt.rsplit('.').next().unwrap_or(dt)) {
                    continue;
                }
                if let TypeRes::Node(ft) = cx.lookup_type(wcx, dt) {
                    if is_func_type(ft) {
                        fields.entry(t).or_default().insert(view.text_str(b.name));
                    }
                }
            }
            declared.insert(t, list);
        }
    }
    // the field a write or a call reaches: the holder's own, or one an embedded struct promotes
    let channel = |holder: NodeIdx, field: &str| -> Option<Channel> {
        if fields.get(&holder).is_some_and(|s| s.contains(field)) {
            return Some((holder, field.to_owned()));
        }
        if declared.get(&holder).is_some_and(|l| l.iter().any(|b| view.text_str(b.name) == field)) {
            return None; // a field of its own shadows a promoted one
        }
        let mut hits = e.direct_supers(holder).iter().copied().filter(|s| fields.get(s).is_some_and(|set| set.contains(field)));
        let first = hits.next()?;
        hits.next().is_none().then(|| (first, field.to_owned()))
    };

    let mut direct: FxHashMap<Channel, Vec<Value>> = FxHashMap::default();
    let mut copies: Vec<(Channel, Channel, FileId, u32)> = Vec::new();
    for (f, s) in writes {
        let cx = wcx.file_cx(e, f);
        let payload = view.text_str(s.payload);
        let Some((col, value)) = payload.split_once('|') else { continue };
        let scope = cx.scope_at(s.line, col.parse().unwrap_or(0));
        let holder = match s.role {
            ROLE_GO_FIELD_LITERAL => match cx.lookup_type(wcx, text(view, s.key_a)) {
                TypeRes::Node(t) if view.node(t).kind() == NodeKind::Struct => Some(t),
                _ => None,
            },
            _ => receiver_type(&cx, wcx, text(view, s.key_a), scope, s.line),
        };
        let Some(holder) = holder else { continue };
        let mut field = text(view, s.key_b).to_owned();
        if let Some(i) = field.strip_prefix('#').and_then(|i| i.parse::<usize>().ok()) {
            match declared.get(&holder).and_then(|l| l.get(i)) {
                Some(b) => field = view.text_str(b.name).to_owned(),
                None => continue,
            }
        }
        let Some(ch) = channel(holder, &field) else { continue };
        // `HandlerFunc(h.serve)` is the value it converts
        let value = match value.split_once('(') {
            Some((callee, rest)) => match cx.lookup_type(wcx, callee) {
                TypeRes::Node(t) if is_func_type(t) => rest.strip_suffix(')').unwrap_or(rest),
                _ => continue,
            },
            None => value,
        };
        // a value copied from another struct's func field: `S{F: o.G}`
        if let Some((root, g)) = value.split_once('.').filter(|(_, g)| !g.contains('.')) {
            if let Some(src) = receiver_type(&cx, wcx, root, scope, s.line).and_then(|t| channel(t, g)) {
                copies.push((ch, src, f, s.line));
                continue;
            }
        }
        if let Some(target) = value_target(e, &cx, wcx, value, scope, s.line) {
            direct.entry(ch).or_default().push(Value { target, file: f, line: s.line, test: cx.is_test, via: None });
        }
    }
    let mut values = direct.clone();
    for (dst, src, f, line) in copies {
        let Some(given) = direct.get(&src) else { continue };
        let via = format!("{}.{}", view.node(src.0).name(), src.1);
        let test = view.file(f).flags() & fileflags::TEST != 0;
        let list = values.entry(dst).or_default();
        for v in given {
            // the copy's own wiring is where the value reaches this field
            list.push(Value { target: v.target, file: f, line, test: test || v.test, via: Some(via.clone()) });
        }
    }
    // production wiring before test wiring, then by place; one value per target
    for list in values.values_mut() {
        list.sort_by(|a, b| (a.test, view.file(a.file).path(), a.line, a.target).cmp(&(b.test, view.file(b.file).path(), b.line, b.target)));
        let mut seen = FxHashSet::default();
        list.retain(|v| seen.insert(v.target));
        list.truncate(MAX_VALUES_PER_FIELD);
    }
    if values.is_empty() {
        return;
    }
    let names: FxHashSet<&str> = values.keys().map(|(_, f)| f.as_str()).collect();
    for &f in &go_files {
        if generated(f) {
            continue;
        }
        let dir = parent_dir(view.file(f).path());
        for r in view.refs_of_file(f) {
            if RefKind::from_wire(r.kind) != Some(RefKind::Edge(EdgeKind::Calls)) {
                continue;
            }
            let Shape::Member { root, path, method } = parse_shape(view.text_str(r.name)) else { continue };
            if path.len() > 1 || !names.contains(method) {
                continue;
            }
            let cx = wcx.file_cx(e, f);
            let scope = cx.scope_at(r.line, r.col);
            let receiver = match path.first() {
                Some(hop) => format!("{root}.{hop}"),
                None => root.to_owned(),
            };
            let Some(t) = receiver_type(&cx, wcx, &receiver, scope, r.line) else { continue };
            let Some(ch) = channel(t, method) else { continue };
            let Some(list) = values.get(&ch) else { continue };
            let field = format!("{}.{}", view.node(ch.0).name(), ch.1);
            for v in list {
                if v.target == r.from || (v.test && parent_dir(view.file(v.file).path()) != dir) {
                    continue;
                }
                let mut meta = serde_json::json!({
                    "synthesizedBy": "go-func-field",
                    "field": field,
                    "registeredAt": format!("{}:{}", view.file(v.file).path(), v.line),
                });
                if let Some(via) = &v.via {
                    meta["via"] = via.as_str().into();
                }
                out.push_meta(EdgeKind::Calls, r.from, v.target, r.line, NONE, meta, order_of("go-func-field"));
            }
        }
    }
}
