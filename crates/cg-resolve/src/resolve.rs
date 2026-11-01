//! Reference resolution (design doc 6.3-6.5): the strategy order is
//! framework rules, lexical scope, import binding, receiver type, qualified name,
//! same package / module, global name match. A reference is resolved by the first strategy that
//! gives an answer, and "better unresolved than wrong": ambiguity produces no edge.

use crate::builtins::{is_builtin, is_common_method};
use crate::engine::{Bound, Engine};
use crate::modules::{ImportTarget, PkgId};
use crate::overload::{self, ArgTy, CallArgs};
use crate::shape::{parse_shape, split_head, Shape};
use cg_core::{text::pack_pos, EdgeKind, FileId, Lang, LangFamily, NodeIdx, NodeKind, RefKind, Visibility, NONE};
use cg_extract::facts::COMMONJS_MODULE;
use cg_extract::{BindKind, ImportKind, ScopeKind};
use cg_store::{status, BindingRec, ImportRec, RefRec, ScopeRec, View};
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;
use std::rc::Rc;

/// Which strategy produced an edge (stored in `out_res`, design doc 5.3).
pub mod by {
    pub const FRAMEWORK: u8 = 1;
    pub const LEXICAL: u8 = 2;
    pub const IMPORT: u8 = 3;
    pub const RECEIVER: u8 = 4;
    pub const QUALIFIED: u8 = 5;
    pub const PACKAGE: u8 = 6;
    pub const GLOBAL: u8 = 7;
    pub const EXTRACT: u8 = 8;

    pub fn name(b: u8) -> &'static str {
        match b {
            FRAMEWORK => "framework",
            LEXICAL => "scope",
            IMPORT => "import",
            RECEIVER => "receiver-type",
            QUALIFIED => "qualified-name",
            PACKAGE => "package",
            GLOBAL => "global-name",
            EXTRACT => "extraction",
            _ => "",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub status: u8,
    pub by: u8,
    /// Target node, or the candidate count for `Ambiguous`.
    pub target: u32,
    /// Confidence 0-100.
    pub conf: u8,
}

impl Outcome {
    pub const PENDING: Outcome = Outcome { status: status::PENDING, by: 0, target: NONE, conf: 0 };
    pub fn found(n: NodeIdx, by: u8, conf: u8) -> Outcome {
        Outcome { status: status::RESOLVED, by, target: n.0, conf }
    }
    pub fn st(s: u8) -> Outcome {
        Outcome { status: s, by: 0, target: NONE, conf: 0 }
    }
    pub fn ambiguous(k: u32) -> Outcome {
        Outcome { status: status::AMBIGUOUS, by: 0, target: k, conf: 0 }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Accept {
    Type,
    Callable,
    Any,
}

impl Accept {
    fn ok(self, k: NodeKind) -> bool {
        match self {
            Accept::Type => k.is_type() || matches!(k, NodeKind::Namespace | NodeKind::Module),
            Accept::Callable => k.is_callable() || k.is_type() || matches!(k, NodeKind::Variable | NodeKind::Constant | NodeKind::Property | NodeKind::Field),
            Accept::Any => k.is_definition(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameRes {
    Found(NodeIdx, u8, u8),
    Module(FileId),
    Pkg(PkgId),
    External,
    Local,
    Builtin,
    Ambiguous(u32),
    NotFound,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypeRes {
    Node(NodeIdx),
    External,
    Builtin,
    Unknown,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StdWrapper {
    Result,
    Option,
    Pointer,
}

/// A project type, possibly still inside a standard Rust container, or an
/// explicit return annotation whose receiver we cannot model.
/// Keep the container until an operation such as `unwrap` actually removes it.
struct ValueType {
    node: Option<NodeIdx>,
    wrappers: SmallVec<[StdWrapper; 2]>,
}

impl ValueType {
    fn plain(node: NodeIdx) -> Self {
        Self { node: Some(node), wrappers: SmallVec::new() }
    }

    fn unknown() -> Self {
        Self { node: None, wrappers: SmallVec::new() }
    }

    fn receiver(self) -> Option<NodeIdx> {
        // The field cache stores one node, so it cannot preserve wrapper
        // methods such as Arc::clone. Only method_receiver may autoderef.
        self.wrappers.is_empty().then_some(self.node).flatten()
    }

    fn method_receiver(&self, method: &str) -> Option<NodeIdx> {
        if self.wrappers.contains(&StdWrapper::Pointer) && pointer_method(method) {
            return None;
        }
        self.wrappers.iter().all(|w| *w == StdWrapper::Pointer).then_some(self.node).flatten()
    }
}

fn pointer_method(method: &str) -> bool {
    matches!(
        method,
        "clone"
            | "clone_from"
            | "as_ref"
            | "as_mut"
            | "deref"
            | "deref_mut"
            | "get_mut"
            | "make_mut"
            | "into_inner"
            | "try_unwrap"
            | "downgrade"
            | "strong_count"
            | "weak_count"
            | "ptr_eq"
    )
}

/// Paths here contain no source arguments except bounded constructor paths.
/// Find the final complete call, allowing earlier calls in its receiver.
fn split_call(value: &str) -> Option<(&str, &str)> {
    let (mut depth, mut open) = (0u32, 0usize);
    for (i, byte) in value.bytes().enumerate() {
        match byte {
            b'(' => {
                if depth == 0 {
                    open = i;
                }
                depth += 1;
            }
            b')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 && i + 1 == value.len() {
                    return Some((&value[..open], &value[open + 1..i]));
                }
            }
            _ => {}
        }
    }
    None
}

/// Per-file lookup structures, built once per file and reused for all its references.
pub struct FileCx<'e, 'v> {
    pub e: &'e Engine<'v>,
    pub view: &'v View,
    pub file: FileId,
    pub lang: Lang,
    pub family: LangFamily,
    pub path: &'v str,
    pub pkg: PkgId,
    pub is_test: bool,
    scopes: &'v [ScopeRec],
    scope_base: u32,
    bindings: FxHashMap<(u32, &'v str), SmallVec<[&'v BindingRec; 2]>>,
    pub imports: &'v [ImportRec],
    import_by_local: FxHashMap<&'v str, SmallVec<[u32; 2]>>,
    imports_by_line: FxHashMap<u32, SmallVec<[u32; 4]>>,
    wildcards: SmallVec<[u32; 2]>,
    imported_files: FxHashSet<FileId>,
    top_dir: &'v str,
}

/// Fields of a class: name and declared type (when it resolves).
type FieldList = Vec<(Box<str>, Option<NodeIdx>)>;

pub struct WorkerCx<'e, 'v> {
    cache: Vec<(FileId, Rc<FileCx<'e, 'v>>, usize)>,
    cache_bytes: usize,
    field_cache: FxHashMap<NodeIdx, FieldList>,
    field_cache_bytes: usize,
}

impl<'e, 'v> WorkerCx<'e, 'v> {
    pub fn new() -> Self {
        WorkerCx { cache: Vec::new(), cache_bytes: 0, field_cache: FxHashMap::default(), field_cache_bytes: 0 }
    }

    pub fn file_cx(&mut self, e: &'e Engine<'v>, f: FileId) -> Rc<FileCx<'e, 'v>> {
        if let Some(pos) = self.cache.iter().position(|(id, _, _)| *id == f) {
            let item = self.cache.remove(pos);
            let rc = item.1.clone();
            self.cache.push(item);
            return rc;
        }
        let rc = Rc::new(FileCx::new(e, f));
        let bytes = rc.heap_bytes();
        let limit = e.budget.worker_cache_bytes();
        if bytes > limit {
            return rc;
        }
        while self.cache.len() >= 6 || self.cache_bytes.saturating_add(bytes) > limit {
            let (_, _, evicted) = self.cache.remove(0);
            self.cache_bytes -= evicted;
        }
        self.cache.push((f, rc.clone(), bytes));
        self.cache_bytes += bytes;
        rc
    }

    fn cache_field(&mut self, t: NodeIdx, name: &str, value: Option<NodeIdx>, limit: usize) {
        if name.len().saturating_add(256) > limit {
            return;
        }
        if self.field_cache_bytes.saturating_add(name.len() + 256) > limit {
            self.field_cache = FxHashMap::default();
            self.field_cache_bytes = 0;
        }
        let old_map_capacity = self.field_cache.capacity();
        let list = self.field_cache.entry(t).or_default();
        let old_capacity = list.capacity();
        list.push((name.into(), value));
        self.field_cache_bytes += name.len()
            + (list.capacity() - old_capacity) * std::mem::size_of::<(Box<str>, Option<NodeIdx>)>()
            + (self.field_cache.capacity() - old_map_capacity) * (std::mem::size_of::<(NodeIdx, FieldList)>() + 8);
        if self.field_cache_bytes > limit {
            self.field_cache = FxHashMap::default();
            self.field_cache_bytes = 0;
        }
    }
}

impl<'e, 'v> Default for WorkerCx<'e, 'v> {
    fn default() -> Self {
        Self::new()
    }
}

fn kind_of(view: &View, n: NodeIdx) -> NodeKind {
    NodeKind::from_u8(view.hot(n).kind).unwrap_or(NodeKind::Variable)
}

impl<'e, 'v> FileCx<'e, 'v> {
    fn heap_bytes(&self) -> usize {
        fn table<K, V>(map: &FxHashMap<K, V>) -> usize {
            map.capacity() * (std::mem::size_of::<(K, V)>() + 8)
        }
        let extra_bindings = self.bindings.values().filter(|v| v.spilled()).map(|v| v.capacity() * std::mem::size_of::<&BindingRec>()).sum::<usize>();
        let extra_imports = self.import_by_local.values().filter(|v| v.spilled()).map(|v| v.capacity() * 4).sum::<usize>()
            + self.imports_by_line.values().filter(|v| v.spilled()).map(|v| v.capacity() * 4).sum::<usize>();
        table(&self.bindings)
            + table(&self.import_by_local)
            + table(&self.imports_by_line)
            + extra_bindings
            + extra_imports
            + self.imported_files.capacity() * (std::mem::size_of::<FileId>() + 8)
            + if self.wildcards.spilled() { self.wildcards.capacity() * 4 } else { 0 }
    }
    pub fn new(e: &'e Engine<'v>, file: FileId) -> Self {
        let view = e.view;
        let fr = view.file(file);
        let path = fr.path();
        let lang = fr.lang();
        let scopes = view.scopes_of(file);
        let scope_base = view.scope_base(file);
        let mut bindings: FxHashMap<(u32, &'v str), SmallVec<[&'v BindingRec; 2]>> = FxHashMap::default();
        for b in view.bindings_of(file) {
            bindings.entry((b.scope.0.wrapping_sub(scope_base), view.text_str(b.name))).or_default().push(b);
        }
        let imports = view.imports_of(file);
        let mut import_by_local: FxHashMap<&'v str, SmallVec<[u32; 2]>> = FxHashMap::default();
        let mut imports_by_line: FxHashMap<u32, SmallVec<[u32; 4]>> = FxHashMap::default();
        let mut wildcards = SmallVec::new();
        let mut imported_files = FxHashSet::default();
        for (i, imp) in imports.iter().enumerate() {
            let local = view.text_str(imp.local);
            if local == "*" {
                wildcards.push(i as u32);
            } else if !local.is_empty() {
                import_by_local.entry(local).or_default().push(i as u32);
            }
            imports_by_line.entry(imp.line).or_default().push(i as u32);
            if let Some(ImportTarget::File(tf)) = e.imp_targets[file.idx()].get(i) {
                imported_files.insert(*tf);
            }
        }
        let top_dir = path.split('/').next().unwrap_or("");
        FileCx {
            e,
            view,
            file,
            lang,
            family: lang.family(),
            path,
            pkg: e.modules.pkg_of(file),
            is_test: fr.flags() & cg_extract::fileflags::TEST != 0,
            scopes,
            scope_base,
            bindings,
            imports,
            import_by_local,
            imports_by_line,
            wildcards,
            imported_files,
            top_dir,
        }
    }

    // ------------------------------------------------------------------ scopes
    pub fn scope_at(&self, line: u32, col: u32) -> u32 {
        if self.scopes.is_empty() {
            return 0;
        }
        let pos = pack_pos(line, col);
        let i = self.scopes.partition_point(|s| s.start <= pos);
        if i == 0 {
            return 0;
        }
        let mut cur = (i - 1) as u32;
        loop {
            let s = &self.scopes[cur as usize];
            if s.start <= pos && pos <= s.end {
                return cur;
            }
            if s.parent.0 == NONE {
                return 0;
            }
            cur = s.parent.0.wrapping_sub(self.scope_base);
            if cur as usize >= self.scopes.len() {
                return 0;
            }
        }
    }

    fn scope_parent(&self, s: u32) -> Option<u32> {
        let p = self.scopes.get(s as usize)?.parent.0;
        (p != NONE).then(|| p.wrapping_sub(self.scope_base))
    }

    fn scope_kind(&self, s: u32) -> u8 {
        self.scopes.get(s as usize).map_or(ScopeKind::File as u8, |x| x.kind)
    }

    fn scope_within(&self, mut inner: u32, outer: u32) -> bool {
        loop {
            if inner == outer {
                return true;
            }
            match self.scope_parent(inner) {
                Some(p) => inner = p,
                None => return outer == 0,
            }
        }
    }

    pub fn lookup_binding(&self, name: &str, scope: u32, line: u32, allow_class: bool) -> Option<&'v BindingRec> {
        let mut s = scope;
        loop {
            if allow_class || self.scope_kind(s) != ScopeKind::Class as u8 {
                if let Some(list) = self.bindings.get(&(s, name)) {
                    let mut best = None;
                    for b in list {
                        if b.kind == BindKind::Package as u8 || b.kind == BindKind::MemberFn as u8 || b.kind == BindKind::ReturnType as u8 {
                            continue;
                        }
                        if b.line == 0 || b.line <= line {
                            best = Some(*b);
                        }
                    }
                    if best.is_some() {
                        return best;
                    }
                }
            }
            s = self.scope_parent(s)?;
        }
    }

    fn visible_import(&self, name: &str, scope: u32) -> Option<u32> {
        let list = self.import_by_local.get(name)?;
        list.iter().copied().find(|i| {
            let imp = &self.imports[*i as usize];
            imp.scope.0 == NONE || self.scope_within(scope, imp.scope.0.wrapping_sub(self.scope_base))
        })
    }

    // ----------------------------------------------------------- type context
    pub fn enclosing_type(&self, from: NodeIdx) -> Option<NodeIdx> {
        let view = self.view;
        let mut cur = from;
        for _ in 0..24 {
            let n = view.node(cur);
            if n.kind().is_type() {
                return Some(cur);
            }
            match n.parent() {
                Some(p) if kind_of(view, p) != NodeKind::File => cur = p,
                _ => break,
            }
        }
        // methods declared outside their type's body (Go receivers, Rust impls): `Type::method`
        let q = view.node(from).qname();
        let prefix = &q[..q.rfind("::")?];
        let mut best = None;
        for cand in view.by_qname(prefix) {
            if !view.node(cand).kind().is_type() {
                continue;
            }
            let cf = view.hot(cand).file;
            if cf == self.file {
                return Some(cand);
            }
            if self.family == LangFamily::Rust {
                if self.e.rust_type_owner(cand, self.file) {
                    return Some(cand);
                }
                continue;
            }
            // a candidate in the caller's package wins over earlier ones; otherwise the first stays
            if (self.pkg.0 != NONE && self.e.modules.pkg_of(cf) == self.pkg) || best.is_none() {
                best = Some(cand);
            }
        }
        best
    }

    /// Node of the class scope that corresponds to type node `t` (same start position).
    fn class_scope_for(&self, t: NodeIdx) -> Option<u32> {
        let n = self.view.node(t);
        let pos = pack_pos(n.start_line(), n.start_col());
        let end = pack_pos(n.end_line(), n.end_col());
        let mut best: Option<(u32, u64)> = None;
        for (i, s) in self.scopes.iter().enumerate() {
            if s.kind != ScopeKind::Class as u8 {
                continue;
            }
            if s.start == pos {
                return Some(i as u32);
            }
            if s.start <= pos && end <= s.end {
                let size = s.end - s.start;
                if best.is_none_or(|(_, b)| size < b) {
                    best = Some((i as u32, size));
                }
            }
        }
        best.map(|b| b.0)
    }

    /// Binding of a file-level variable called `name` (module singletons such as `app = Flask()`).
    pub fn file_binding(&self, name: &str) -> Option<&'v BindingRec> {
        self.bindings.get(&(0, name)).and_then(|l| l.iter().copied().find(|b| b.kind == BindKind::Local as u8 || b.kind == BindKind::Field as u8))
    }

    /// Follow `segs` from a module / type: exports, top-level definitions, submodules, members.
    fn walk_path(&self, start: Bound, segs: &[&str]) -> Option<Bound> {
        let mut cur = start;
        for seg in segs {
            cur = match cur {
                Bound::Module(m) => match self.e.lookup_export(m, seg, 0) {
                    Some(b) => b,
                    None => match (self.view.file(m).lang().family() != LangFamily::Js).then(|| self.e.lookup_top_level(m, seg)).flatten() {
                        Some(n) => Bound::Node(n),
                        None => Bound::Module(self.e.submodule(m, seg)?),
                    },
                },
                Bound::Node(n) => Bound::Node(self.e.direct_member(n, seg).or_else(|| self.e.find_member(n, seg))?),
                Bound::Pkg(p) => {
                    let n = self.e.pkg_defs.get(p.0 as usize).and_then(|m| m.get(*seg)).and_then(|l| l.iter().find(|n| self.visible(**n, seg)))?;
                    Bound::Node(*n)
                }
                _ => return None,
            };
        }
        Some(cur)
    }

    // ------------------------------------------------------------ type lookup
    /// Type (or namespace) denoted by `name` in this file's context.
    pub fn lookup_type(&self, wcx: &mut WorkerCx<'e, 'v>, name: &str) -> TypeRes {
        let name = name.trim();
        if name.is_empty() {
            return TypeRes::Unknown;
        }
        if let Some((head, rest)) = split_head(name) {
            return self.qualified_type(wcx, head, rest, name);
        }
        match self.resolve_name(wcx, name, Accept::Type, 0, u32::MAX, None, true) {
            NameRes::Found(n, ..) => TypeRes::Node(n),
            NameRes::External => TypeRes::External,
            NameRes::Builtin => TypeRes::Builtin,
            _ => TypeRes::Unknown,
        }
    }

    fn qualified_type(&self, wcx: &mut WorkerCx<'e, 'v>, head: &str, rest: &str, full: &str) -> TypeRes {
        if self.family == LangFamily::Rust {
            if let Some(Bound::Node(n)) = self.rust_path_bound(full) {
                if kind_of(self.view, n).is_type() {
                    return TypeRes::Node(n);
                }
            }
        }
        let segs: Vec<&str> = rest.split(['.', ':']).filter(|s| !s.is_empty()).collect();
        match self.resolve_name(wcx, head, Accept::Any, 0, u32::MAX, None, false) {
            NameRes::Module(tf) => match self.walk_path(Bound::Module(tf), &segs) {
                Some(Bound::Node(n)) => TypeRes::Node(n),
                _ => TypeRes::Unknown,
            },
            NameRes::Pkg(p) => match self.walk_path(Bound::Pkg(p), &segs) {
                Some(Bound::Node(n)) => TypeRes::Node(n),
                _ => TypeRes::Unknown,
            },
            NameRes::Found(n, ..) => self.nested_type(wcx, n, rest),
            NameRes::External => TypeRes::External,
            _ => {
                // fully qualified Java / Kotlin / Rust name
                let q = full.replace('.', "::");
                if let Some(n) = self.view.by_qname(&q).find(|n| kind_of(self.view, *n).is_type()) {
                    return TypeRes::Node(n);
                }
                let segs: Vec<&str> = full.split(['.', ':']).filter(|s| !s.is_empty()).collect();
                for i in (1..segs.len()).rev() {
                    let q = format!("{}::{}", segs[..i].join("."), segs[i..].join("::"));
                    if let Some(n) = self.view.by_qname(&q).find(|n| kind_of(self.view, *n).is_type()) {
                        return TypeRes::Node(n);
                    }
                }
                TypeRes::Unknown
            }
        }
    }

    fn nested_type(&self, _wcx: &mut WorkerCx<'e, 'v>, mut n: NodeIdx, rest: &str) -> TypeRes {
        for seg in rest.split(['.', ':']).filter(|s| !s.is_empty()) {
            match self.e.direct_member(n, seg) {
                Some(m) if self.view.node(m).kind().is_type() => n = m,
                _ => return TypeRes::Unknown,
            }
        }
        TypeRes::Node(n)
    }

    fn rust_path_bound(&self, name: &str) -> Option<Bound> {
        // A module and a value can share a Rust name (`mod once; pub use
        // once::once`). Resolve the parent, then consult its exports before
        // treating the final segment as a module filename.
        let (prefix, last) = name.rsplit_once("::").unwrap_or((name, ""));
        let (target, tail) = self.e.modules.resolve_rust_path(prefix, self.file);
        let ImportTarget::File(file) = target else { return None };
        let mut segs: Vec<_> = prefix.split("::").filter(|s| !s.is_empty()).collect();
        let start = segs.len().checked_sub(tail)?;
        if !last.is_empty() {
            segs.push(last);
        }
        self.walk_path(Bound::Module(file), &segs[start..])
    }

    /// The type a binding holds, from its declared type or initialiser.
    pub fn binding_type(&self, wcx: &mut WorkerCx<'e, 'v>, b: &BindingRec) -> Option<NodeIdx> {
        self.binding_value(wcx, b, 0)?.receiver()
    }

    fn binding_value(&self, wcx: &mut WorkerCx<'e, 'v>, b: &BindingRec, depth: u8) -> Option<ValueType> {
        if depth >= 16 {
            return None;
        }
        let dt = self.view.text_str(b.decl_type);
        if !dt.is_empty() {
            return self.type_value(wcx, dt, None, depth + 1);
        }
        let init = self.view.text_str(b.init_ctor);
        if init.is_empty() || init == "embedded" {
            return None;
        }
        let scope = b.scope.0.wrapping_sub(self.scope_base);
        // The new variable is not in scope in its own initializer (`let rt = rt()`).
        let shadows = init.split(['.', ':', '(']).next() == Some(self.view.text_str(b.name));
        let line = if shadows { b.line.saturating_sub(1) } else { b.line };
        self.expression_value(wcx, init, scope, line, depth + 1)
    }

    fn expression_value(&self, wcx: &mut WorkerCx<'e, 'v>, init: &str, scope: u32, line: u32, depth: u8) -> Option<ValueType> {
        if depth >= 16 {
            return None;
        }
        if let Some(inner) = init.strip_suffix('?') {
            let mut value = self.expression_value(wcx, inner, scope, line, depth + 1)?;
            if !matches!(value.wrappers.pop()?, StdWrapper::Result | StdWrapper::Option) {
                return None;
            }
            return Some(value);
        }
        if self.family == LangFamily::Rust {
            if let Some((callee, args)) = split_call(init) {
                if let Some(owner) = callee.strip_suffix("::new") {
                    if self.std_wrapper(wcx, owner) == Some(StdWrapper::Pointer) {
                        let mut value = self.expression_value(wcx, args, scope, line, depth + 1)?;
                        value.wrappers.push(StdWrapper::Pointer);
                        return Some(value);
                    }
                }
                return self.call_value(wcx, callee, scope, line, depth + 1);
            }
        }
        if let Some(call) = init.strip_suffix("()") {
            return self.call_value(wcx, call, scope, line, depth + 1);
        }
        if self.family == LangFamily::Rust {
            if let Some(other) = self.lookup_binding(init, scope, line, false) {
                return self.binding_value(wcx, other, depth + 1);
            }
        }
        self.type_value(wcx, init, None, depth + 1)
    }

    fn std_wrapper(&self, wcx: &mut WorkerCx<'e, 'v>, name: &str) -> Option<StdWrapper> {
        let (prefix, leaf) = name.rsplit_once("::").unwrap_or(("", name));
        if prefix.is_empty() {
            if let Some(i) = self.visible_import(name, 0) {
                let imp = &self.imports[i as usize];
                let spec = self.view.text_str(imp.spec);
                let imported = self.view.text_str(imp.imported);
                return match (spec, imported) {
                    ("std::sync" | "alloc::sync", "Arc") | ("std::rc" | "alloc::rc", "Rc") | ("std::boxed" | "alloc::boxed", "Box") => {
                        Some(StdWrapper::Pointer)
                    }
                    ("std::result" | "core::result" | "std::io", "Result") => Some(StdWrapper::Result),
                    ("std::option" | "core::option", "Option") => Some(StdWrapper::Option),
                    _ => None,
                };
            }
        }
        let wrapper = match leaf {
            "Result" => StdWrapper::Result,
            "Option" => StdWrapper::Option,
            "Arc" | "Rc" | "Box" => StdWrapper::Pointer,
            _ => return None,
        };
        let standard = match prefix {
            "std::result" | "core::result" | "std::io" => wrapper == StdWrapper::Result,
            "std::option" | "core::option" => wrapper == StdWrapper::Option,
            "std::sync" | "alloc::sync" => leaf == "Arc",
            "std::rc" | "alloc::rc" => leaf == "Rc",
            "std::boxed" | "alloc::boxed" => leaf == "Box",
            "" => matches!(self.lookup_type(wcx, name), TypeRes::Builtin),
            "io" if wrapper == StdWrapper::Result => self.visible_import("io", 0).is_some_and(|i| {
                let imp = &self.imports[i as usize];
                self.view.text_str(imp.spec) == "std" && self.view.text_str(imp.imported) == "io"
            }),
            _ => false,
        };
        standard.then_some(wrapper)
    }

    fn type_value(&self, wcx: &mut WorkerCx<'e, 'v>, name: &str, receiver: Option<NodeIdx>, depth: u8) -> Option<ValueType> {
        if depth >= 16 || name.len() > 4096 {
            return None;
        }
        let mut name = name.trim();
        while let Some(rest) = name.strip_prefix('&') {
            name = rest.trim_start();
            if name.starts_with('\'') {
                name = name.split_once(char::is_whitespace)?.1.trim_start();
            }
            name = name.strip_prefix("mut ").unwrap_or(name).trim_start();
        }
        if matches!(name, "Self" | "self" | "this") {
            return receiver.map(ValueType::plain);
        }
        if self.family == LangFamily::Rust {
            if let Some(open) = name.find('<') {
                let base = name[..open].trim();
                let Some(wrapper) = self.std_wrapper(wcx, base) else {
                    return match self.lookup_type(wcx, base) {
                        TypeRes::Node(t) => Some(ValueType::plain(t)),
                        _ => None,
                    };
                };
                let args = name[open + 1..].strip_suffix('>')?;
                let mut nesting = 0u32;
                let mut end = args.len();
                for (i, c) in args.char_indices() {
                    match c {
                        '<' | '(' | '[' => nesting += 1,
                        '>' | ')' | ']' => nesting = nesting.checked_sub(1)?,
                        ',' if nesting == 0 => {
                            end = i;
                            break;
                        }
                        _ => {}
                    }
                }
                let mut inner = self.type_value(wcx, &args[..end], receiver, depth + 1)?;
                inner.wrappers.push(wrapper);
                return Some(inner);
            }
        }
        match self.lookup_type(wcx, name) {
            TypeRes::Node(t) => Some(ValueType::plain(t)),
            _ => None,
        }
    }

    fn returned_value(&self, wcx: &mut WorkerCx<'e, 'v>, n: NodeIdx, receiver: Option<NodeIdx>, depth: u8) -> Option<ValueType> {
        let node = self.view.node(n);
        if node.kind().is_type() {
            return Some(ValueType::plain(n));
        }
        let cx = wcx.file_cx(self.e, node.file());
        if let Some(b) = self.e.return_types.get(&n) {
            let rt = self.view.text_str(b.decl_type);
            if cx.family == LangFamily::Js {
                // TS facts also retain unsupported annotations. Failure to
                // resolve one must not enable the unique-member fallback.
                return Some(cx.ts_return_value(n, rt).unwrap_or_else(ValueType::unknown));
            }
            if !rt.is_empty() {
                return cx.type_value(wcx, rt, receiver, depth + 1);
            }
            return cx.expression_value(wcx, self.view.text_str(b.init_ctor), b.scope.0.wrapping_sub(cx.scope_base), u32::MAX, depth + 1);
        }
        cx.type_value(wcx, node.return_type()?, receiver, depth + 1)
    }

    /// TS return facts contain only nominal names. Look in the declaration's
    /// enclosing scopes and explicit imports, never another file's same-named
    /// class or an unrelated nested declaration.
    fn ts_return_value(&self, function: NodeIdx, name: &str) -> Option<ValueType> {
        if name.is_empty() || name.len() > 4096 {
            return None;
        }
        let (head, tail) = name.split_once('.').unwrap_or((name, ""));
        let mut owner = Some(function);
        let mut bound = None;
        while let Some(scope) = owner {
            let mut candidates = self.view.by_name(head).filter(|n| {
                let node = self.view.node(*n);
                node.file() == self.file && node.parent() == Some(scope) && Accept::Type.ok(node.kind())
            });
            if let Some(n) = candidates.next() {
                if candidates.next().is_some() {
                    return None;
                }
                bound = Some(Bound::Node(n));
                break;
            }
            owner = self.view.node(scope).parent();
        }
        let bound = bound.or_else(|| self.visible_import(head, 0).map(|i| self.e.bind_import(self.file, i as usize)))?;
        let target = if tail.is_empty() { bound } else { self.walk_path(bound, &tail.split('.').collect::<Vec<_>>())? };
        match target {
            Bound::Node(n) if self.view.node(n).kind().is_type() => Some(ValueType::plain(n)),
            _ => None,
        }
    }

    /// Follow declared return types through a bounded chain. Static methods do
    /// not implicitly return their owner, and Result<T> is not treated as T.
    fn call_value(&self, wcx: &mut WorkerCx<'e, 'v>, callee: &str, scope: u32, line: u32, depth: u8) -> Option<ValueType> {
        if depth >= 16 || callee.len() > 4096 {
            return None;
        }
        if let Some((inner, method)) = callee.rsplit_once('.') {
            if inner.ends_with(['?', ')']) {
                let receiver = self.expression_value(wcx, inner, scope, line, depth + 1)?;
                return self.member_return(wcx, receiver, method, depth + 1);
            }
        }
        let split = callee.rsplit_once("::").or_else(|| callee.rsplit_once('.'));
        if let Some((owner, method)) = split {
            // Django's default manager returns the concrete model for these
            // operations. Require the actual Model base in the resolved
            // hierarchy; a coincidentally named `.objects.create` is insufficient.
            if self.family == LangFamily::Python && matches!(method, "create" | "get" | "first" | "last" | "earliest" | "latest") {
                if let (Some(base), Some(model)) = (self.e.django_model, owner.strip_suffix(".objects")) {
                    if let TypeRes::Node(t) = self.lookup_type(wcx, model) {
                        if t == base || self.e.ancestors(t).contains(&base) {
                            return Some(ValueType::plain(t));
                        }
                    }
                }
            }
            let receiver = if let Some(b) = self.lookup_binding(owner, scope, line, false) {
                self.binding_value(wcx, b, depth + 1)
            } else {
                self.type_value(wcx, owner, None, depth + 1)
            };
            if let Some(t) = receiver {
                return self.member_return(wcx, t, method, depth + 1);
            }
        }
        match self.resolve_name(wcx, callee, Accept::Callable, scope, line, None, true) {
            NameRes::Found(n, ..) => self.returned_value(wcx, n, self.enclosing_type(n), depth + 1),
            _ => None,
        }
    }

    fn member_return(&self, wcx: &mut WorkerCx<'e, 'v>, mut receiver: ValueType, method: &str, depth: u8) -> Option<ValueType> {
        if receiver.wrappers.last().is_some_and(|w| matches!(w, StdWrapper::Result | StdWrapper::Option)) {
            return match method {
                "unwrap" | "expect" | "unwrap_or" | "unwrap_or_else" | "unwrap_or_default" => {
                    receiver.wrappers.pop();
                    Some(receiver)
                }
                "as_ref" | "as_mut" => Some(receiver),
                _ => None,
            };
        }
        if receiver.wrappers.last() == Some(&StdWrapper::Pointer) {
            match method {
                "clone" => return Some(receiver),
                "as_ref" | "as_mut" | "deref" | "deref_mut" => {
                    receiver.wrappers.pop();
                    return Some(receiver);
                }
                _ => {}
            }
        }
        let node = receiver.method_receiver(method)?;
        let m = self.e.find_member(node, method)?;
        self.returned_value(wcx, m, Some(node), depth + 1)
    }

    /// Declared type of field `name` of type `t` (through the hierarchy).
    pub fn field_type(&self, wcx: &mut WorkerCx<'e, 'v>, t: NodeIdx, name: &str) -> Option<NodeIdx> {
        if let Some(list) = wcx.field_cache.get(&t) {
            if let Some((_, v)) = list.iter().find(|(n, _)| &**n == name) {
                return *v;
            }
        }
        let mut result = None;
        let mut chain = vec![t];
        chain.extend(self.e.ancestors(t));
        for ty in chain {
            let owner = self.view.hot(ty).file;
            let cx = wcx.file_cx(self.e, owner);
            if let Some(cs) = cx.class_scope_for(ty) {
                if let Some(list) = cx.bindings.get(&(cs, name)) {
                    if let Some(b) = list.iter().find(|b| b.kind == BindKind::Field as u8) {
                        if let Some(ft) = cx.binding_type(wcx, b) {
                            result = Some(ft);
                            break;
                        }
                    }
                }
            }
        }
        wcx.cache_field(t, name, result, self.e.budget.worker_cache_bytes());
        result
    }

    // ------------------------------------------------------- overload selection
    /// What each argument of the call `r` is, from its shape and the bindings in scope.
    fn arg_types(&self, wcx: &mut WorkerCx<'e, 'v>, args: &CallArgs<'v>, r: &RefRec) -> SmallVec<[ArgTy; 6]> {
        let scope = self.scope_at(r.line, r.col);
        args.shapes.iter().map(|s| self.arg_type(wcx, s, scope, r)).collect()
    }

    fn named_arg(&self, wcx: &mut WorkerCx<'e, 'v>, written: &str) -> ArgTy {
        let simple = overload::simplify_type(written);
        if simple.is_empty() {
            return ArgTy::Unknown;
        }
        let node = if overload::is_value_type(&simple) || simple.ends_with("[]") {
            None
        } else {
            match self.lookup_type(wcx, written) {
                TypeRes::Node(n) => Some(n),
                _ => None,
            }
        };
        ArgTy::Named(simple, node)
    }

    fn node_arg(&self, t: NodeIdx) -> ArgTy {
        ArgTy::Named(self.view.node(t).name().to_owned(), Some(t))
    }

    fn arg_type(&self, wcx: &mut WorkerCx<'e, 'v>, shape: &str, scope: u32, r: &RefRec) -> ArgTy {
        match shape {
            "?" => ArgTy::Unknown,
            "N" => ArgTy::Null,
            "L" => ArgTy::Lambda(None),
            "#this" => self.enclosing_type(r.from).map_or(ArgTy::Unknown, |t| self.node_arg(t)),
            _ => {
                if let Some(t) = shape.strip_prefix("T:") {
                    return self.named_arg(wcx, t);
                }
                if let Some(v) = shape.strip_prefix("V:") {
                    if let Some(b) = self.lookup_binding(v, scope, r.line, false) {
                        if b.kind == BindKind::LocalFn as u8 {
                            return ArgTy::Lambda(None);
                        }
                        let dt = self.view.text_str(b.decl_type);
                        if !dt.is_empty() {
                            return self.named_arg(wcx, dt);
                        }
                        return self.binding_type(wcx, b).map_or(ArgTy::Unknown, |t| self.node_arg(t));
                    }
                    // a bare field: Java / Kotlin only (JS reads fields through `this.`)
                    return if self.family == LangFamily::Jvm { self.field_arg(wcx, r, v) } else { ArgTy::Unknown };
                }
                if let Some(f) = shape.strip_prefix("F:") {
                    return self.field_arg(wcx, r, f);
                }
                match shape.strip_prefix('L').map(str::parse::<u8>) {
                    Some(Ok(k)) => ArgTy::Lambda(Some(k)),
                    _ => ArgTy::Unknown,
                }
            }
        }
    }

    /// Field `name` of the type enclosing the reference, by its declared type (primitives too).
    fn field_arg(&self, wcx: &mut WorkerCx<'e, 'v>, r: &RefRec, name: &str) -> ArgTy {
        let Some(t) = self.enclosing_type(r.from) else { return ArgTy::Unknown };
        for ty in std::iter::once(t).chain(self.e.ancestors(t)) {
            let cx = wcx.file_cx(self.e, self.view.hot(ty).file);
            let Some(cs) = cx.class_scope_for(ty) else { continue };
            let Some(b) = cx.bindings.get(&(cs, name)).and_then(|l| l.iter().copied().find(|b| b.kind == BindKind::Field as u8)) else { continue };
            let dt = self.view.text_str(b.decl_type);
            if !dt.is_empty() {
                return cx.named_arg(wcx, dt);
            }
            return cx.binding_type(wcx, b).map_or(ArgTy::Unknown, |n| cx.node_arg(n));
        }
        ArgTy::Unknown
    }

    /// The overload of `cands` (same name, preference order) the call's arguments reach:
    /// by count first, then by type. `None` when nothing is known or nothing fits.
    fn pick_overload(&self, wcx: &mut WorkerCx<'e, 'v>, cands: &[NodeIdx], args: &CallArgs<'v>, r: &RefRec) -> Option<NodeIdx> {
        if cands.len() < 2 {
            return None;
        }
        let view = self.view;
        let n = args.shapes.len();
        let lang_of = |c: NodeIdx| view.file(view.hot(c).file).lang();
        // arity settles most sets without typing a single argument
        let by_count: SmallVec<[NodeIdx; 4]> =
            cands.iter().copied().filter(|c| overload::takes(lang_of(*c), view.node(*c).signature(), n) != Some(false)).collect();
        match by_count.len() {
            0 => return None,
            1 => return Some(by_count[0]),
            _ => {}
        }
        let types = self.arg_types(wcx, args, r);
        let mut sigs = Vec::with_capacity(by_count.len());
        let mut tparams: Vec<SmallVec<[&str; 2]>> = Vec::with_capacity(by_count.len());
        let mut param_nodes: Vec<FxHashMap<String, Option<NodeIdx>>> = Vec::with_capacity(by_count.len());
        for &c in &by_count {
            let node = view.node(c);
            let params = node.signature().and_then(|s| overload::parse_params(lang_of(c), s));
            let mut map = FxHashMap::default();
            if let Some(p) = &params {
                let cx = wcx.file_cx(self.e, node.file());
                for item in &p.items {
                    let base = item.ty.trim_end_matches("[]");
                    if base.is_empty() || map.contains_key(&item.ty) || overload::is_value_type(base) || !base.starts_with(|ch: char| ch.is_ascii_uppercase()) {
                        continue;
                    }
                    let t = match cx.lookup_type(wcx, base) {
                        TypeRes::Node(t) => Some(t),
                        _ => None,
                    };
                    map.insert(item.ty.clone(), t);
                }
            }
            sigs.push(params);
            tparams.push(node.type_params().into_iter().filter_map(|t| t.split(|c: char| !(c.is_alphanumeric() || c == '_')).next()).collect());
            param_nodes.push(map);
        }
        let e = self.e;
        let relate = |i: usize, a: &ArgTy, pt: &str| -> Option<bool> {
            let p = (*param_nodes[i].get(pt)?)?;
            match a {
                ArgTy::Named(_, Some(at)) => Some(*at == p || e.ancestors(*at).contains(&p)),
                ArgTy::Named(name, None) => overload::is_value_type(name).then_some(false),
                ArgTy::Lambda(_) => {
                    (!matches!(view.node(p).kind(), NodeKind::Interface | NodeKind::Trait | NodeKind::Protocol | NodeKind::TypeAlias)).then_some(false)
                }
                _ => None,
            }
        };
        overload::choose(self.family, &sigs, &types, &tparams, &relate).map(|i| by_count[i])
    }

    /// The overload of member `name` of `t` (or its ancestors) a call reaches, when the type
    /// declares several.
    fn overloaded_member(&self, wcx: &mut WorkerCx<'e, 'v>, t: NodeIdx, name: &str, args: Option<&CallArgs<'v>>, r: &RefRec) -> Option<NodeIdx> {
        let args = args?;
        if !matches!(self.family, LangFamily::Jvm | LangFamily::Js) {
            return None;
        }
        let cands = self.e.member_overloads(t, name);
        self.pick_overload(wcx, &cands, args, r)
    }

    /// Same-named siblings of `pick` in its file (functions declared several times).
    fn sibling_overloads(&self, pick: NodeIdx) -> SmallVec<[NodeIdx; 4]> {
        let view = self.view;
        let h = view.hot(pick);
        let name = view.name_str(h.name);
        let mut out: SmallVec<[NodeIdx; 4]> = view
            .by_name(name)
            .filter(|c| {
                let ch = view.hot(*c);
                ch.file == h.file && ch.parent == h.parent && view.node(*c).kind().is_callable()
            })
            .collect();
        out.sort();
        out
    }

    // ------------------------------------------------------------ name lookup
    fn visible(&self, cand: NodeIdx, name: &str) -> bool {
        let h = self.view.hot(cand);
        if h.file == self.file {
            return true;
        }
        match self.family {
            LangFamily::Go => {
                let same_pkg = self.pkg.0 != NONE && self.e.modules.pkg_of(h.file) == self.pkg;
                same_pkg || name.chars().next().is_some_and(|c| c.is_uppercase())
            }
            LangFamily::Jvm => h.vis != Visibility::Private as u8,
            _ => true,
        }
    }

    fn score(&self, cand: NodeIdx) -> u32 {
        let h = self.view.hot(cand);
        let cf = h.file;
        let cpath = self.view.file(cf).path();
        let mut s = if cf == self.file {
            100
        } else if crate::project::parent_dir(cpath) == crate::project::parent_dir(self.path) || (self.pkg.0 != NONE && self.e.modules.pkg_of(cf) == self.pkg) {
            80
        } else if self.imported_files.contains(&cf) {
            60
        } else if cpath.split('/').next().unwrap_or("") == self.top_dir {
            40
        } else {
            20
        };
        let node = self.view.node(cand);
        if node.is_exported() == Some(true) {
            s += 5;
        }
        let cflags = self.view.file(cf).flags();
        if (cflags & cg_extract::fileflags::TEST != 0) == self.is_test {
            s += 5;
        }
        if cflags & cg_extract::fileflags::GENERATED == 0 {
            s += 3;
        }
        s
    }

    /// Name match over the whole project (design doc 6.5).
    fn global_match(&self, wcx: &mut WorkerCx<'e, 'v>, name: &str, accept: Accept, bare_callable: bool, args: Option<(&CallArgs<'v>, &RefRec)>) -> NameRes {
        let mut best: SmallVec<[NodeIdx; 4]> = SmallVec::new();
        let mut best_score = 0u32;
        let mut seen = 0usize;
        for cand in self.view.by_name(name) {
            seen += 1;
            if seen > 4000 {
                break;
            }
            let node = self.view.node(cand);
            let k = node.kind();
            if !accept.ok(k) || (bare_callable && matches!(k, NodeKind::Method | NodeKind::Property | NodeKind::Field)) {
                continue;
            }
            let cf = node.file();
            if self.view.file(cf).lang().family() != self.family {
                continue;
            }
            if !self.visible(cand, name) {
                continue;
            }
            let sc = self.score(cand);
            if sc > best_score {
                best_score = sc;
                best.clear();
                best.push(cand);
            } else if sc == best_score {
                best.push(cand);
            }
        }
        match best.len() {
            0 => NameRes::NotFound,
            1 => {
                let (by_, conf) = if best_score >= 180 {
                    (by::LEXICAL, 98)
                } else if best_score >= 80 {
                    (by::PACKAGE, 85)
                } else if best_score >= 60 {
                    (by::GLOBAL, 70)
                } else if best_score >= 40 {
                    (by::GLOBAL, 55)
                } else {
                    (by::GLOBAL, 40)
                };
                NameRes::Found(best[0], by_, conf)
            }
            k => {
                // overloads under one parent: the arguments pick one, else the first declared
                let p0 = self.view.hot(best[0]).parent;
                if best.iter().all(|c| self.view.hot(*c).parent == p0 && self.view.node(*c).file() == self.view.node(best[0]).file()) {
                    best.sort();
                    let pick = args.and_then(|(a, r)| self.pick_overload(wcx, &best, a, r)).unwrap_or(best[0]);
                    return NameRes::Found(pick, by::PACKAGE, 80);
                }
                NameRes::Ambiguous(k as u32)
            }
        }
    }

    /// Member `name` of the type enclosing `from` (and of its outer types for Java / Kotlin).
    fn enclosing_member(&self, wcx: &mut WorkerCx<'e, 'v>, from: Option<NodeIdx>, name: &str, args: Option<(&CallArgs<'v>, &RefRec)>) -> Option<NodeIdx> {
        let view = self.view;
        let t = self.enclosing_type(from?)?;
        if let Some((a, r)) = args {
            if let Some(m) = self.overloaded_member(wcx, t, name, Some(a), r) {
                return Some(m);
            }
        }
        if let Some(m) = self.e.find_member(t, name) {
            return Some(m);
        }
        if self.family == LangFamily::Jvm {
            let mut outer = view.node(t).parent();
            while let Some(o) = outer {
                if kind_of(view, o) == NodeKind::File {
                    break;
                }
                if kind_of(view, o).is_type() {
                    if let Some(m) = self.e.find_member(o, name) {
                        return Some(m);
                    }
                }
                outer = view.node(o).parent();
            }
        }
        None
    }

    /// Resolve a simple name (design doc 6.3 steps 2, 3, 6, 7).
    #[allow(clippy::too_many_arguments)]
    pub fn resolve_name(
        &self,
        wcx: &mut WorkerCx<'e, 'v>,
        name: &str,
        accept: Accept,
        scope: u32,
        line: u32,
        from: Option<NodeIdx>,
        allow_global: bool,
    ) -> NameRes {
        self.resolve_name_args(wcx, name, accept, scope, line, from, allow_global, None)
    }

    /// [`Self::resolve_name`] for a call whose arguments may pick among overloads.
    #[allow(clippy::too_many_arguments)]
    fn resolve_name_args(
        &self,
        wcx: &mut WorkerCx<'e, 'v>,
        name: &str,
        accept: Accept,
        scope: u32,
        line: u32,
        from: Option<NodeIdx>,
        allow_global: bool,
        args: Option<(&CallArgs<'v>, &RefRec)>,
    ) -> NameRes {
        let view = self.view;
        let e = self.e;
        // 2. lexical scope
        if accept != Accept::Type {
            if let Some(b) = self.lookup_binding(name, scope, line, false) {
                if b.kind == BindKind::LocalFn as u8 && b.node.0 != NONE {
                    return NameRes::Found(b.node, by::LEXICAL, 98);
                }
                if b.kind == BindKind::LocalFn as u8 {
                    // a local function declared in this scope: its node is the same-named nested definition
                    let hit = view.by_name(name).find(|n| view.hot(*n).file == self.file && matches!(kind_of(view, *n), NodeKind::Function | NodeKind::Method));
                    if let Some(n) = hit {
                        return NameRes::Found(n, by::LEXICAL, 95);
                    }
                }
                return NameRes::Local;
            }
            // members of the enclosing class are visible by bare name in these languages
            if matches!(self.family, LangFamily::Jvm | LangFamily::Python) {
                if let Some(m) = self.enclosing_member(wcx, from, name, args) {
                    return NameRes::Found(m, by::RECEIVER, 90);
                }
            }
        }
        // 3. import binding
        if let Some(i) = self.visible_import(name, scope) {
            match e.bind_import(self.file, i as usize) {
                Bound::Node(n) => {
                    if accept.ok(kind_of(view, n)) || accept == Accept::Any {
                        return NameRes::Found(n, by::IMPORT, 95);
                    }
                    // imported value used as a namespace (rare)
                    return NameRes::Found(n, by::IMPORT, 80);
                }
                Bound::Module(m) => {
                    // `require('./factory')()` invokes module.exports. Keep the
                    // module namespace for member calls; never substitute its
                    // default function for a missing property such as .static.
                    let imp = &self.imports[i as usize];
                    if self.family == LangFamily::Js && imp.kind == ImportKind::Require as u8 {
                        if let Some(Bound::Node(n)) = e.lookup_export(m, COMMONJS_MODULE, 0) {
                            if accept.ok(kind_of(view, n)) {
                                return NameRes::Found(n, by::IMPORT, 95);
                            }
                        }
                    }
                    return NameRes::Module(m);
                }
                Bound::Pkg(p) => return NameRes::Pkg(p),
                Bound::External => return NameRes::External,
                Bound::Unknown if self.family == LangFamily::Js => return NameRes::NotFound,
                Bound::Unknown => {}
            }
        }
        // JS / TS: `this.method()` is emitted as a bare `method`; an explicit import wins over it
        if accept != Accept::Type && self.family == LangFamily::Js {
            if let Some(m) = self.enclosing_member(wcx, from, name, args) {
                return NameRes::Found(m, by::RECEIVER, 88);
            }
        }
        // 4. same file
        let bare_callable = accept == Accept::Callable;
        let mut same: SmallVec<[NodeIdx; 4]> = SmallVec::new();
        for cand in view.by_name(name) {
            let h = view.hot(cand);
            if h.file != self.file {
                continue;
            }
            let k = NodeKind::from_u8(h.kind).unwrap_or(NodeKind::Variable);
            if !accept.ok(k) || (bare_callable && matches!(k, NodeKind::Method | NodeKind::Property | NodeKind::Field)) {
                continue;
            }
            same.push(cand);
        }
        if !same.is_empty() {
            // prefer top-level definitions, then the first declared; overloads by the arguments
            let mut pick = same.iter().copied().find(|n| view.node(*n).parent().is_some_and(|p| kind_of(view, p) == NodeKind::File)).unwrap_or(same[0]);
            if let Some((a, r)) = args.filter(|_| same.len() > 1 && matches!(self.family, LangFamily::Jvm | LangFamily::Js)) {
                let set = self.sibling_overloads(pick);
                if let Some(m) = self.pick_overload(wcx, &set, a, r) {
                    pick = m;
                }
            }
            return NameRes::Found(pick, by::LEXICAL, 92);
        }
        // 5. same package / module
        if self.pkg.0 != NONE {
            if let Some(list) = e.pkg_defs.get(self.pkg.0 as usize).and_then(|m| m.get(name)) {
                let hits: SmallVec<[NodeIdx; 2]> = list.iter().copied().filter(|n| accept.ok(kind_of(view, *n)) && view.hot(*n).file != self.file).collect();
                if let Some(n) = hits.first() {
                    return NameRes::Found(*n, by::PACKAGE, 85);
                }
            }
        }
        // Rust: a child module is visible by name
        if self.family == LangFamily::Rust && accept != Accept::Callable {
            if let Some(m) = e.submodule(self.file, name) {
                return NameRes::Module(m);
            }
        }
        // 6. wildcard imports
        for &wi in &self.wildcards {
            let imp = &self.imports[wi as usize];
            let tgt = e.imp_targets[self.file.idx()].get(wi as usize).copied().unwrap_or(ImportTarget::External);
            match tgt {
                ImportTarget::Pkg(p) => {
                    if let Some(list) = e.pkg_defs.get(p.0 as usize).and_then(|m| m.get(name)) {
                        if let Some(n) = list.iter().find(|n| accept.ok(kind_of(view, **n))) {
                            return NameRes::Found(*n, by::IMPORT, 85);
                        }
                    }
                }
                ImportTarget::File(tf) => {
                    if let Some(Bound::Node(n)) = e.lookup_export(tf, name, 0) {
                        if accept.ok(kind_of(view, n)) {
                            return NameRes::Found(n, by::IMPORT, 85);
                        }
                    }
                }
                ImportTarget::Node(c) if imp.kind == ImportKind::Static as u8 => {
                    if let Some(m) = e.find_member(c, name) {
                        return NameRes::Found(m, by::IMPORT, 85);
                    }
                }
                _ => {}
            }
        }
        // 7. global name match
        if is_builtin(self.family, name) {
            return NameRes::Builtin;
        }
        if allow_global {
            return self.global_match(wcx, name, accept, bare_callable, args);
        }
        NameRes::NotFound
    }

    // ------------------------------------------------------------ call forms
    fn member_of(&self, wcx: &mut WorkerCx<'e, 'v>, t: NodeIdx, path: &[&str], method: &str, conf: u8) -> Outcome {
        self.member_call(wcx, t, path, method, conf, None)
    }

    /// `member_of` for a call: its arguments pick among overloads of the final member.
    fn member_call(
        &self,
        wcx: &mut WorkerCx<'e, 'v>,
        mut t: NodeIdx,
        path: &[&str],
        method: &str,
        conf: u8,
        args: Option<(&CallArgs<'v>, &RefRec)>,
    ) -> Outcome {
        for f in path {
            match self.field_type(wcx, t, f) {
                Some(ft) => t = ft,
                None => return Outcome::st(status::NO_CANDIDATE),
            }
        }
        if let Some((a, r)) = args {
            if let Some(m) = self.overloaded_member(wcx, t, method, Some(a), r) {
                return Outcome::found(m, by::RECEIVER, conf);
            }
        }
        match self.e.find_member(t, method) {
            Some(m) => Outcome::found(m, by::RECEIVER, conf),
            None => Outcome::st(status::NO_CANDIDATE),
        }
    }

    /// Method of an untyped receiver: only when the name is unique in the language family.
    fn fallback_method(&self, method: &str) -> Outcome {
        if is_common_method(self.family, method) || is_builtin(self.family, method) {
            return Outcome::st(status::NO_CANDIDATE);
        }
        let mut found: SmallVec<[NodeIdx; 2]> = SmallVec::new();
        let assigned = self.e.member_functions.get(method).map(|v| v.as_slice()).unwrap_or_default();
        for cand in self.view.by_name(method).chain(assigned.iter().copied()) {
            if found.contains(&cand) {
                continue;
            }
            let node = self.view.node(cand);
            if !matches!(node.kind(), NodeKind::Method | NodeKind::Function | NodeKind::Property | NodeKind::Field) {
                continue;
            }
            if node.kind() == NodeKind::Function && !assigned.contains(&cand) {
                continue;
            }
            if self.view.file(node.file()).lang().family() != self.family || !self.visible(cand, method) {
                continue;
            }
            found.push(cand);
            if found.len() > 1 {
                break;
            }
        }
        match found.len() {
            1 => Outcome::found(found[0], by::GLOBAL, 45),
            0 => Outcome::st(status::NO_CANDIDATE),
            _ => Outcome::ambiguous(2),
        }
    }

    fn in_module(&self, wcx: &mut WorkerCx<'e, 'v>, tf: FileId, path: &[&str], method: &str) -> Outcome {
        // walk module / type segments; the last segment may be a member of a type reached on the way
        let mut segs: Vec<&str> = path.to_vec();
        let mut cur = Bound::Module(tf);
        let mut consumed = 0;
        for (i, seg) in segs.iter().enumerate() {
            match self.walk_path(cur, &[seg]) {
                Some(next) => {
                    cur = next;
                    consumed = i + 1;
                }
                None => return Outcome::st(status::NO_CANDIDATE),
            }
        }
        segs.truncate(consumed);
        match cur {
            Bound::Module(m) => match self.walk_path(Bound::Module(m), &[method]) {
                Some(Bound::Node(n)) => Outcome::found(n, by::IMPORT, 92),
                _ => Outcome::st(status::NO_CANDIDATE),
            },
            Bound::Node(n) => {
                if self.view.node(n).kind().is_type() {
                    self.member_of(wcx, n, &[], method, 90)
                } else {
                    // a variable holding an instance: use the type of its initialiser
                    let owner = self.view.hot(n).file;
                    let cx = wcx.file_cx(self.e, owner);
                    let nm = self.view.node(n).name();
                    if let Some(b) = cx.file_binding(nm) {
                        if let Some(t) = cx.binding_type(wcx, b) {
                            return self.member_of(wcx, t, &[], method, 88);
                        }
                    }
                    Outcome::st(status::NO_CANDIDATE)
                }
            }
            _ => Outcome::st(status::NO_CANDIDATE),
        }
    }

    fn call_bare(&self, wcx: &mut WorkerCx<'e, 'v>, r: &RefRec, name: &str, accept: Accept, args: Option<&CallArgs<'v>>) -> Outcome {
        let scope = self.scope_at(r.line, r.col);
        match self.resolve_name_args(wcx, name, accept, scope, r.line, Some(r.from), true, args.map(|a| (a, r))) {
            NameRes::Found(n, b, c) => Outcome::found(n, b, c),
            NameRes::Local => Outcome::st(status::LOCAL_BINDING),
            NameRes::External | NameRes::Module(_) | NameRes::Pkg(_) => Outcome::st(status::EXTERNAL),
            NameRes::Builtin => Outcome::st(status::BUILTIN),
            NameRes::Ambiguous(k) => Outcome::ambiguous(k),
            NameRes::NotFound => Outcome::st(status::NO_CANDIDATE),
        }
    }

    fn call_member(&self, wcx: &mut WorkerCx<'e, 'v>, r: &RefRec, root: &str, path: &[&str], method: &str, args: Option<&CallArgs<'v>>) -> Outcome {
        let scope = self.scope_at(r.line, r.col);
        let view = self.view;
        let call = args.map(|a| (a, r));
        // (a) a local variable, parameter or field
        if let Some(b) = self.lookup_binding(root, scope, r.line, false) {
            if b.kind != BindKind::LocalFn as u8 {
                return match self.binding_value(wcx, b, 0) {
                    Some(t) if t.method_receiver(method).is_some() => {
                        let o = self.member_call(wcx, t.method_receiver(method).unwrap(), path, method, 90, call);
                        if o.status == status::RESOLVED {
                            o
                        } else {
                            Outcome::st(status::NO_CANDIDATE)
                        }
                    }
                    Some(_) => Outcome::st(status::NO_CANDIDATE),
                    None => {
                        if !self.view.text_str(b.decl_type).is_empty() {
                            return Outcome::st(status::NO_CANDIDATE);
                        }
                        // An initializer rooted in an explicit external import
                        // does not justify guessing an unrelated project member.
                        let init = self.view.text_str(b.init_ctor);
                        let head = init.split(['.', ':', '(']).next().unwrap_or("");
                        if let Some(i) = self.visible_import(head, scope).filter(|_| self.lookup_binding(head, scope, r.line, false).is_none()) {
                            if self.e.bind_import(self.file, i as usize) == Bound::External {
                                return Outcome::st(status::EXTERNAL);
                            }
                        }
                        let o = self.fallback_method(method);
                        if o.status == status::RESOLVED && path.is_empty() {
                            o
                        } else {
                            Outcome::st(status::LOCAL_BINDING)
                        }
                    }
                };
            }
        }
        // (a2) Java / Kotlin: a field of the enclosing type or an outer one, `this.` implied
        // (the kernel writes `this.repo.find()` as `repo.find`)
        if self.family == LangFamily::Jvm {
            if let Some(o) = self.call_on_field(wcx, r, root, path, method, call) {
                return o;
            }
        }
        // (b) imported root
        if let Some(i) = self.visible_import(root, scope) {
            match self.e.bind_import(self.file, i as usize) {
                Bound::Module(tf) => return self.in_module(wcx, tf, path, method),
                Bound::Pkg(p) => {
                    let first = path.first().copied().unwrap_or(method);
                    let list = self.e.pkg_defs.get(p.0 as usize).and_then(|m| m.get(first));
                    let hit = list.and_then(|l| l.iter().find(|n| self.visible(**n, first)).copied());
                    return match hit {
                        Some(n) if path.is_empty() => Outcome::found(n, by::IMPORT, 92),
                        Some(n) => self.member_of(wcx, n, &path[1..], method, 90),
                        None => Outcome::st(status::NO_CANDIDATE),
                    };
                }
                Bound::Node(n) => {
                    let k = kind_of(view, n);
                    if k.is_type() {
                        return self.member_call(wcx, n, path, method, 92, call);
                    }
                    // an imported module-level variable: the type of its initialiser
                    {
                        let owner = view.hot(n).file;
                        let cx = wcx.file_cx(self.e, owner);
                        let nm = view.node(n).name();
                        if let Some(b) = cx.file_binding(nm) {
                            if let Some(t) = cx.binding_type(wcx, b) {
                                return self.member_of(wcx, t, path, method, 88);
                            }
                        }
                    }
                    if let Some(rt) = view.node(n).return_type() {
                        let owner = view.hot(n).file;
                        let cx = wcx.file_cx(self.e, owner);
                        if let TypeRes::Node(t) = cx.lookup_type(wcx, rt) {
                            return self.member_of(wcx, t, path, method, 85);
                        }
                    }
                    return Outcome::st(status::NO_CANDIDATE);
                }
                Bound::External => return Outcome::st(status::EXTERNAL),
                Bound::Unknown if self.family == LangFamily::Js => return Outcome::st(status::NO_CANDIDATE),
                Bound::Unknown => {}
            }
        }
        // (c) a type (static member) or, in Rust, a child module
        if self.family == LangFamily::Rust {
            let mut qualified = root.to_owned();
            for seg in path.iter().copied().chain(std::iter::once(method)) {
                qualified.push_str("::");
                qualified.push_str(seg);
            }
            if let Some(Bound::Node(n)) = self.rust_path_bound(&qualified) {
                if Accept::Callable.ok(kind_of(view, n)) {
                    return Outcome::found(n, by::IMPORT, 92);
                }
            }
            if let Some(mf) = self.e.submodule(self.file, root) {
                return self.in_module(wcx, mf, path, method);
            }
        }
        match self.lookup_type(wcx, root) {
            TypeRes::Node(t) => {
                if view.node(t).kind().is_type() {
                    return self.member_call(wcx, t, path, method, 90, call);
                }
            }
            TypeRes::External => return Outcome::st(status::EXTERNAL),
            TypeRes::Builtin => return Outcome::st(status::BUILTIN),
            TypeRes::Unknown => {}
        }
        if is_builtin(self.family, root) {
            return Outcome::st(status::BUILTIN);
        }
        // (d) nothing known about the receiver
        if path.is_empty() {
            self.fallback_method(method)
        } else {
            Outcome::st(status::NO_CANDIDATE)
        }
    }

    /// A call on field `root` of the type enclosing `r` (or of an outer type). `None` when no
    /// such field is declared; a field whose declared type is outside the project ends here.
    fn call_on_field(
        &self,
        wcx: &mut WorkerCx<'e, 'v>,
        r: &RefRec,
        root: &str,
        path: &[&str],
        method: &str,
        call: Option<(&CallArgs<'v>, &RefRec)>,
    ) -> Option<Outcome> {
        let view = self.view;
        let mut t = self.enclosing_type(r.from);
        for _ in 0..8 {
            let ty = t?;
            if let Some(ft) = self.field_type(wcx, ty, root) {
                let o = self.member_call(wcx, ft, path, method, 90, call);
                return Some(if o.status == status::RESOLVED { o } else { Outcome::st(status::NO_CANDIDATE) });
            }
            if let Some(declared) = self.field_decl_type(wcx, ty, root) {
                // declared, not a project type: nothing in the project is its member
                return (!declared.is_empty()).then(|| Outcome::st(status::EXTERNAL));
            }
            t = view.node(ty).parent().filter(|p| kind_of(view, *p).is_type());
        }
        None
    }

    /// Declared type text of field `name` of `t` or an ancestor (`Some("")`: untyped).
    fn field_decl_type(&self, wcx: &mut WorkerCx<'e, 'v>, t: NodeIdx, name: &str) -> Option<String> {
        for ty in std::iter::once(t).chain(self.e.ancestors(t)) {
            let cx = wcx.file_cx(self.e, self.view.hot(ty).file);
            let Some(cs) = cx.class_scope_for(ty) else { continue };
            if let Some(b) = cx.bindings.get(&(cs, name)).and_then(|l| l.iter().copied().find(|b| b.kind == BindKind::Field as u8)) {
                return Some(self.view.text_str(b.decl_type).to_owned());
            }
        }
        None
    }

    fn call_this(&self, wcx: &mut WorkerCx<'e, 'v>, r: &RefRec, kw: &str, fields: &[&str], method: &str, args: Option<&CallArgs<'v>>) -> Outcome {
        let Some(t) = self.enclosing_type(r.from) else {
            return if fields.is_empty() { self.fallback_method(method) } else { Outcome::st(status::NO_CANDIDATE) };
        };
        let _ = kw;
        let o = self.member_call(wcx, t, fields, method, 95, args.map(|a| (a, r)));
        if o.status == status::RESOLVED {
            return o;
        }
        if fields.is_empty() {
            // a callable stored in a field of this type
            let tf = self.view.hot(t).file;
            let cx = wcx.file_cx(self.e, tf);
            if let Some(cs) = cx.class_scope_for(t) {
                if let Some(list) = cx.bindings.get(&(cs, self.view.name_str(self.view.hot(t).name))) {
                    let _ = list;
                }
                for key in cx.bindings.keys() {
                    if key.0 == cs && key.1 == method {
                        return Outcome::st(status::LOCAL_BINDING);
                    }
                }
            }
            // A known receiver constrains member lookup to its own hierarchy.
            return Outcome::st(status::NO_CANDIDATE);
        }
        Outcome::st(status::NO_CANDIDATE)
    }

    fn call_super(&self, wcx: &mut WorkerCx<'e, 'v>, r: &RefRec, method: &str, args: Option<&CallArgs<'v>>) -> Outcome {
        let Some(t) = self.enclosing_type(r.from) else { return Outcome::st(status::NO_CANDIDATE) };
        for a in self.e.ancestors(t) {
            if let Some(m) = self.e.direct_member(a, method) {
                // the overloads the nearest declaring supertype sees, its own and inherited
                let pick = self.overloaded_member(wcx, a, method, args, r).unwrap_or(m);
                return Outcome::found(pick, by::RECEIVER, 90);
            }
        }
        Outcome::st(status::NO_CANDIDATE)
    }

    fn call_result(&self, wcx: &mut WorkerCx<'e, 'v>, r: &RefRec, inner: &str, method: &str, args: Option<&CallArgs<'v>>) -> Outcome {
        // type produced by calling `inner`
        let value = match self.call_value(wcx, inner, self.scope_at(r.line, r.col), r.line, 0) {
            Some(value) => Some(value),
            None => {
                // `inner` may be a method of a typed receiver: resolve it as a call first (its
                // own arguments are not this reference's)
                let o = self.call_name_with(wcx, r, inner, None);
                if o.status != status::RESOLVED {
                    return Outcome::st(status::NO_CANDIDATE);
                }
                let n = NodeIdx(o.target);
                self.returned_value(wcx, n, self.enclosing_type(n), 0)
            }
        };
        let Some(t) = value.and_then(|v| v.method_receiver(method)) else { return Outcome::st(status::NO_CANDIDATE) };
        self.member_call(wcx, t, &[], method, 85, args.map(|a| (a, r)))
    }

    pub fn resolve_call_name(&self, wcx: &mut WorkerCx<'e, 'v>, r: &RefRec, name: &str) -> Outcome {
        let args = CallArgs::of(self.view, r);
        self.call_name_with(wcx, r, name, args.as_ref())
    }

    fn call_name_with(&self, wcx: &mut WorkerCx<'e, 'v>, r: &RefRec, name: &str, args: Option<&CallArgs<'v>>) -> Outcome {
        match parse_shape(name) {
            Shape::Bare(n) => self.call_bare(wcx, r, n, Accept::Callable, args),
            Shape::Member { root, path, method } => self.call_member(wcx, r, root, &path, method, args),
            Shape::This { kw, fields, method } => self.call_this(wcx, r, kw, &fields, method, args),
            Shape::Super { method } => self.call_super(wcx, r, method, args),
            Shape::New { class, method } => match self.lookup_type(wcx, class) {
                TypeRes::Node(t) => self.member_call(wcx, t, &[], method, 90, args.map(|a| (a, r))),
                TypeRes::External => Outcome::st(status::EXTERNAL),
                _ => Outcome::st(status::NO_CANDIDATE),
            },
            Shape::CallResult { inner, method } => self.call_result(wcx, r, inner, method, args),
            Shape::ValueResult { inner, method } => {
                let value = self.expression_value(wcx, inner, self.scope_at(r.line, r.col), r.line, 0);
                match value.and_then(|v| v.method_receiver(method)) {
                    Some(t) => self.member_call(wcx, t, &[], method, 85, args.map(|a| (a, r))),
                    None => Outcome::st(status::NO_CANDIDATE),
                }
            }
        }
    }

    // ------------------------------------------------------------ reference kinds
    fn resolve_type_ref(&self, wcx: &mut WorkerCx<'e, 'v>, r: &RefRec, name: &str, accept: Accept) -> Outcome {
        if let Some(c) = self.view.list(r.candidates).first() {
            // walkers sometimes hand over qualified candidates
            if let Some(n) = self.view.by_qname(c).next() {
                return Outcome::found(n, by::QUALIFIED, 90);
            }
        }
        let scope = self.scope_at(r.line, r.col);
        let res = if let Some((head, rest)) = split_head(name) {
            match self.qualified_type(wcx, head, rest, name) {
                TypeRes::Node(n) => NameRes::Found(n, by::QUALIFIED, 90),
                TypeRes::External => NameRes::External,
                TypeRes::Builtin => NameRes::Builtin,
                TypeRes::Unknown => NameRes::NotFound,
            }
        } else {
            let first = self.resolve_name(wcx, name, accept, scope, r.line, Some(r.from), true);
            // `Self` and friends
            match (first, name) {
                (NameRes::NotFound | NameRes::Builtin, "Self") => match self.enclosing_type(r.from) {
                    Some(t) => NameRes::Found(t, by::RECEIVER, 95),
                    None => first,
                },
                _ => first,
            }
        };
        match res {
            NameRes::Found(n, b, c) => Outcome::found(n, b, c),
            NameRes::External | NameRes::Module(_) | NameRes::Pkg(_) => Outcome::st(status::EXTERNAL),
            NameRes::Builtin => Outcome::st(status::BUILTIN),
            NameRes::Local => Outcome::st(status::LOCAL_BINDING),
            NameRes::Ambiguous(k) => Outcome::ambiguous(k),
            NameRes::NotFound => Outcome::st(status::NO_CANDIDATE),
        }
    }

    /// Framework rule for a route's handler that no scope or import names: the method the
    /// route decorates (`@Get() list()`, `@GetMapping get()`), declared right after the
    /// decorator in the route's file. A bare method name is invisible to lexical lookup, but a
    /// decorator route sits on the method it routes to.
    fn route_handler(&self, r: &RefRec, name: &str, kind: RefKind) -> Option<Outcome> {
        let view = self.view;
        if kind_of(view, r.from) != NodeKind::Route || !matches!(kind, RefKind::FunctionRef | RefKind::Edge(EdgeKind::Calls | EdgeKind::References)) {
            return None;
        }
        let Shape::Bare(name) = parse_shape(name) else { return None };
        let mut after: Option<(u32, NodeIdx)> = None;
        for c in view.by_name(name) {
            let h = view.hot(c);
            let k = NodeKind::from_u8(h.kind).unwrap_or(NodeKind::Variable);
            // stacked decorators and annotations sit between the route and its method
            if h.file != self.file || !matches!(k, NodeKind::Method | NodeKind::Function) || h.start_line < r.line || h.start_line > r.line + 12 {
                continue;
            }
            if after.is_none_or(|(l, n)| (h.start_line, c) < (l, n)) {
                after = Some((h.start_line, c));
            }
        }
        Some(Outcome::found(after?.1, by::FRAMEWORK, 92))
    }

    fn file_node(&self, f: FileId) -> NodeIdx {
        self.view.file(f).rec.node_start
    }

    fn bound_outcome(&self, b: Bound) -> Outcome {
        match b {
            Bound::Node(n) => Outcome::found(n, by::IMPORT, 95),
            Bound::Module(m) => Outcome::found(self.file_node(m), by::IMPORT, 95),
            Bound::Pkg(p) => {
                let pk = &self.e.modules.pkgs[p.0 as usize];
                match pk.files.first() {
                    Some(f) => Outcome::found(self.file_node(*f), by::IMPORT, 70),
                    None => Outcome::st(status::NO_CANDIDATE),
                }
            }
            Bound::External => Outcome::st(status::EXTERNAL),
            Bound::Unknown => Outcome::st(status::NO_CANDIDATE),
        }
    }

    fn resolve_import_ref(&self, name: &str, r: &RefRec) -> Outcome {
        let Some(idxs) = self.imports_by_line.get(&r.line) else { return Outcome::st(status::NO_CANDIDATE) };
        let view = self.view;
        let tgt = |i: u32| self.e.imp_targets[self.file.idx()].get(i as usize).copied().unwrap_or(ImportTarget::External);
        // 1. the module specifier itself
        for &i in idxs {
            let imp = &self.imports[i as usize];
            if view.text_str(imp.spec) == name {
                return match tgt(i) {
                    ImportTarget::File(tf) => Outcome::found(self.file_node(tf), by::IMPORT, 95),
                    ImportTarget::Node(n) => Outcome::found(n, by::IMPORT, 95),
                    ImportTarget::Pkg(p) => self.bound_outcome(Bound::Pkg(p)),
                    ImportTarget::External => Outcome::st(status::EXTERNAL),
                    ImportTarget::NotFound => Outcome::st(status::NO_CANDIDATE),
                };
            }
        }
        // 2. a name bound by the import
        for &i in idxs {
            let imp = &self.imports[i as usize];
            let local = view.text_str(imp.local);
            let spec = view.text_str(imp.spec);
            let imported = view.text_str(imp.imported);
            let full_rs = format!("{spec}::{imported}");
            let full_dot = format!("{spec}.{imported}");
            if (!local.is_empty() && local == name) || full_rs == name || full_dot == name {
                return self.bound_outcome(self.e.bind_import(self.file, i as usize));
            }
        }
        // 3. Rust: the root crate / module segment of a `use` path
        if self.family == LangFamily::Rust {
            for &i in idxs {
                let imp = &self.imports[i as usize];
                let spec = view.text_str(imp.spec);
                if spec.split("::").next() == Some(name) {
                    return match tgt(i) {
                        ImportTarget::External => Outcome::st(status::EXTERNAL),
                        _ => {
                            let first = self.e.modules.resolve_import(self.lang, name, self.file);
                            match first {
                                ImportTarget::File(tf) => Outcome::found(self.file_node(tf), by::IMPORT, 90),
                                _ => Outcome::st(status::EXTERNAL),
                            }
                        }
                    };
                }
            }
        }
        Outcome::st(status::NO_CANDIDATE)
    }

    /// Resolve one reference. `name` is the reference's text.
    pub fn resolve_ref(&self, wcx: &mut WorkerCx<'e, 'v>, r: &RefRec) -> Outcome {
        let name = self.view.text_str(r.name);
        let Some(kind) = RefKind::from_wire(r.kind) else { return Outcome::st(status::NO_CANDIDATE) };
        let o = self.resolve_kind(wcx, r, name, kind);
        if o.status == status::NO_CANDIDATE {
            if let Some(found) = self.route_handler(r, name, kind) {
                return found;
            }
        }
        o
    }

    fn resolve_kind(&self, wcx: &mut WorkerCx<'e, 'v>, r: &RefRec, name: &str, kind: RefKind) -> Outcome {
        match kind {
            RefKind::Edge(EdgeKind::Imports) => self.resolve_import_ref(name, r),
            RefKind::Edge(EdgeKind::Extends | EdgeKind::Implements | EdgeKind::TypeOf | EdgeKind::Returns) => self.resolve_type_ref(wcx, r, name, Accept::Type),
            RefKind::Edge(EdgeKind::Instantiates) => {
                let o = self.resolve_type_ref(wcx, r, name, Accept::Type);
                if o.status == status::NO_CANDIDATE {
                    self.resolve_type_ref(wcx, r, name, Accept::Callable)
                } else {
                    o
                }
            }
            RefKind::Edge(EdgeKind::References) => {
                let o = self.resolve_type_ref(wcx, r, name, Accept::Type);
                if matches!(o.status, status::NO_CANDIDATE) {
                    self.resolve_type_ref(wcx, r, name, Accept::Any)
                } else {
                    o
                }
            }
            RefKind::FunctionRef => self.resolve_call_name(wcx, r, name),
            RefKind::Edge(EdgeKind::Calls | EdgeKind::Decorates | EdgeKind::Navigates | EdgeKind::Overrides | EdgeKind::Exports | EdgeKind::Contains) => {
                self.resolve_call_name(wcx, r, name)
            }
        }
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    #[test]
    fn optional_field_cache_releases_capacity_at_its_budget() {
        let mut worker = WorkerCx::new();
        for i in 0..1000 {
            worker.cache_field(NodeIdx(i / 3), &format!("field{i}"), Some(NodeIdx(i)), 1024);
            assert!(worker.field_cache_bytes <= 1024);
        }
        let before = worker.field_cache_bytes;
        worker.cache_field(NodeIdx(0), &"x".repeat(2048), None, 1024);
        assert_eq!(worker.field_cache_bytes, before);
    }
}
