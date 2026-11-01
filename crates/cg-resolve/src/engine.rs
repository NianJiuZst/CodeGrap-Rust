//! Resolution tables (design doc 6.2, round 0): import targets, export tables with re-export
//! closure, package definition maps, member lookup and the type hierarchy. Everything here is
//! built once, then shared read-only by the parallel rounds.

use crate::modules::{ImportTarget, Modules, PkgId};
use crate::project::ProjectInfo;
use cg_core::{EdgeKind, FileId, Lang, LangFamily, NodeIdx, NodeKind, StrId, NONE};
use cg_extract::facts::COMMONJS_MODULE;
use cg_extract::{BindKind, ImportKind};
use cg_store::{outkind, status, View};
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use smallvec::SmallVec;

#[derive(Clone, Copy, Debug)]
pub enum ExportEntry {
    Node(NodeIdx),
    /// `export { name as x } from './f'`: look `name` up in file.
    Re(FileId, StrId),
    /// `export * as ns from './f'` or a child module: the module itself.
    Module(FileId),
}

/// Result of binding a local name through an import.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bound {
    Node(NodeIdx),
    Module(FileId),
    Pkg(PkgId),
    External,
    Unknown,
}

pub struct Engine<'v> {
    pub budget: cg_core::budget::MemBudget,
    pub view: &'v View,
    pub project: &'v ProjectInfo,
    pub modules: Modules<'v>,
    /// Per file (indexed by `FileId`): targets aligned with `view.imports_of(file)`.
    pub imp_targets: Vec<Vec<ImportTarget>>,
    pub exports: Vec<FxHashMap<&'v str, ExportEntry>>,
    pub stars: Vec<Vec<FileId>>,
    /// Per package: top-level definitions by name.
    pub pkg_defs: Vec<FxHashMap<&'v str, SmallVec<[NodeIdx; 2]>>>,
    /// Direct supertypes (`extends` / `implements` resolved in round 1).
    pub supers: FxHashMap<NodeIdx, SmallVec<[NodeIdx; 3]>>,
    /// JavaScript functions with syntax evidence that they implement an object member.
    pub member_functions: FxHashMap<&'v str, SmallVec<[NodeIdx; 2]>>,
    pub return_types: FxHashMap<NodeIdx, &'v cg_store::BindingRec>,
    pub django_model: Option<NodeIdx>,
    pub n_files: usize,
}

fn is_top_level(view: &View, n: NodeIdx) -> bool {
    match view.node(n).parent() {
        Some(p) => view.hot(p).kind == NodeKind::File as u8,
        None => false,
    }
}

impl<'v> Engine<'v> {
    pub fn build(view: &'v View, project: &'v ProjectInfo) -> Engine<'v> {
        Self::build_with_budget(view, project, cg_core::budget::MemBudget::detect())
    }

    pub fn build_with_budget(view: &'v View, project: &'v ProjectInfo, budget: cg_core::budget::MemBudget) -> Engine<'v> {
        let modules = Modules::build(view, project);
        let n_files = modules.file_pkg.len().max(view.n_files());
        let files: Vec<FileId> = view.files().collect();

        // ---- import targets (parallel by file) ----
        let mut imp_targets: Vec<Vec<ImportTarget>> = vec![Vec::new(); n_files];
        let computed: Vec<(usize, Vec<ImportTarget>)> = files
            .par_iter()
            .map(|&f| {
                let lang = view.file(f).lang();
                let targets = view
                    .imports_of(f)
                    .iter()
                    .map(|imp| {
                        if imp.kind == ImportKind::SideEffect as u8 && imp.spec.0 == NONE {
                            return ImportTarget::External;
                        }
                        modules.resolve_import(lang, view.text_str(imp.spec), f)
                    })
                    .collect();
                (f.idx(), targets)
            })
            .collect();
        for (i, t) in computed {
            imp_targets[i] = t;
        }
        let mut member_functions: FxHashMap<&'v str, SmallVec<[NodeIdx; 2]>> = FxHashMap::default();
        for &f in &files {
            for b in view.bindings_of(f).iter().filter(|b| b.kind == BindKind::MemberFn as u8 && b.node.is_some()) {
                member_functions.entry(view.text_str(b.name)).or_default().push(b.node);
            }
        }

        let mut e = Engine {
            budget,
            view,
            project,
            modules,
            imp_targets,
            exports: vec![FxHashMap::default(); n_files],
            stars: vec![Vec::new(); n_files],
            pkg_defs: Vec::new(),
            supers: FxHashMap::default(),
            member_functions,
            return_types: files
                .iter()
                .flat_map(|&f| view.bindings_of(f))
                .filter(|b| b.kind == BindKind::ReturnType as u8 && b.node.is_some())
                .map(|b| (b.node, b))
                .collect(),
            django_model: None,
            n_files,
        };
        e.build_exports(&files);
        if project.py_deps.contains("django") {
            if let Some(f) = files.iter().copied().find(|f| view.file(*f).lang() == Lang::Python) {
                if let ImportTarget::File(base) = e.modules.resolve_import(Lang::Python, "django.db.models.base", f) {
                    if let Some(Bound::Node(n)) = e.lookup_export(base, "Model", 0) {
                        e.django_model = Some(n);
                    }
                }
            }
        }
        e.build_pkg_defs(&files);
        e
    }

    fn top_level_nodes(&self, f: FileId) -> impl Iterator<Item = NodeIdx> + '_ {
        let range = self.view.nodes_in_file(f);
        range.map(NodeIdx).filter(move |n| {
            let h = self.view.hot(*n);
            let k = NodeKind::from_u8(h.kind);
            k.is_some_and(|k| k.is_definition()) && is_top_level(self.view, *n)
        })
    }

    fn build_exports(&mut self, files: &[FileId]) {
        let view = self.view;
        let results: Vec<(usize, FxHashMap<&'v str, ExportEntry>, Vec<FileId>)> = files
            .par_iter()
            .map(|&f| {
                let lang = view.file(f).lang();
                let mut map: FxHashMap<&'v str, ExportEntry> = FxHashMap::default();
                let mut stars = Vec::new();
                let family = lang.family();
                // Local definitions
                for n in self.top_level_nodes(f) {
                    let node = view.node(n);
                    let exported = match family {
                        LangFamily::Js => node.is_exported() == Some(true),
                        _ => true,
                    };
                    if exported {
                        let name = node.name();
                        match map.get(name) {
                            // class beats interface/type alias for merged declarations
                            Some(ExportEntry::Node(prev)) if view.node(*prev).kind().is_type() && !node.kind().is_type() => {}
                            Some(ExportEntry::Node(prev)) if !(node.kind() == NodeKind::Class && view.node(*prev).kind() != NodeKind::Class) => {}
                            _ => {
                                map.insert(name, ExportEntry::Node(n));
                            }
                        }
                    }
                }
                // Explicit export facts (JS re-exports, `pub use`) and default exports
                for ex in view.exports_of(f) {
                    let name = view.text_str(ex.name);
                    let spec = view.text_str(ex.from_spec);
                    if name == COMMONJS_MODULE && spec.is_empty() && view.text_str(ex.from_name) == "*" {
                        map.insert(name, ExportEntry::Module(f));
                        continue;
                    }
                    if ex.star != 0 {
                        if let Some(ImportTarget::File(tf)) = Some(self.modules.resolve_import(lang, spec, f)) {
                            stars.push(tf);
                        }
                        continue;
                    }
                    if !spec.is_empty() {
                        if let ImportTarget::File(tf) = self.modules.resolve_import(lang, spec, f) {
                            if view.text_str(ex.from_name) == "*" {
                                map.insert(name, ExportEntry::Module(tf));
                            } else {
                                map.insert(name, ExportEntry::Re(tf, ex.from_name));
                            }
                        }
                    } else if ex.node.is_some() {
                        map.insert(name, ExportEntry::Node(ex.node));
                    } else {
                        // `export { a as b }` / `export default a`: alias of a local definition
                        let local = view.text_str(ex.from_name);
                        if let Some(n) = self.top_level_nodes(f).find(|n| view.node(*n).name() == local) {
                            map.insert(name, ExportEntry::Node(n));
                        }
                    }
                }
                // Python packages re-export what their `__init__` imports
                if family == LangFamily::Python {
                    let targets = &self.imp_targets[f.idx()];
                    for (i, imp) in view.imports_of(f).iter().enumerate() {
                        let local = view.text_str(imp.local);
                        if local.is_empty() || local == "*" {
                            if local == "*" {
                                if let Some(ImportTarget::File(tf)) = targets.get(i).copied() {
                                    stars.push(tf);
                                }
                            }
                            continue;
                        }
                        if imp.kind == ImportKind::Named as u8 {
                            if let Some(ImportTarget::File(tf)) = targets.get(i).copied() {
                                map.entry(local).or_insert(ExportEntry::Re(tf, imp.imported));
                            }
                        }
                    }
                }
                (f.idx(), map, stars)
            })
            .collect();
        for (i, m, s) in results {
            self.exports[i] = m;
            self.stars[i] = s;
        }
        // Rust child modules (`mod x;` -> x.rs) are exports of their parent module
        for &f in files {
            if self.view.file(f).lang() != Lang::Rust {
                continue;
            }
            let Some((crate_idx, modpath)) = self.modules.rust_module_of(f).cloned() else { continue };
            if modpath.is_empty() && self.modules.rust_module_file(crate_idx, "") != Some(f) {
                continue;
            }
            // children are discovered from the other direction: register each file under its parent
            let _ = (crate_idx, modpath);
        }
        let rust_children: Vec<(FileId, &'v str, FileId)> = files
            .iter()
            .filter_map(|&f| {
                if self.view.file(f).lang() != Lang::Rust {
                    return None;
                }
                let (ci, mp) = self.modules.rust_module_of(f)?;
                if mp.is_empty() {
                    return None;
                }
                let (parent, last) = match mp.rfind("::") {
                    Some(i) => (&mp[..i], &mp[i + 2..]),
                    None => ("", mp.as_str()),
                };
                let pf = self.modules.rust_module_file(*ci, parent)?;
                // the &str must live as long as the view: take it from the path's own text
                let path = self.view.file(f).path();
                let stem = path.rsplit('/').next().unwrap_or("");
                let name: &'v str = if stem == "mod.rs" { path.rsplit('/').nth(1).unwrap_or("") } else { stem.strip_suffix(".rs").unwrap_or(stem) };
                let _ = last;
                Some((pf, name, f))
            })
            .collect();
        for (pf, name, f) in rust_children {
            self.exports[pf.idx()].entry(name).or_insert(ExportEntry::Module(f));
        }
    }

    fn build_pkg_defs(&mut self, files: &[FileId]) {
        let view = self.view;
        let mut defs: Vec<FxHashMap<&'v str, SmallVec<[NodeIdx; 2]>>> = (0..self.modules.pkgs.len()).map(|_| FxHashMap::default()).collect();
        for &f in files {
            let p = self.modules.pkg_of(f);
            if p.is_none() {
                continue;
            }
            for n in self.top_level_nodes(f) {
                let node = view.node(n);
                // Go methods are not package-level names; Java/Kotlin nest members under classes
                if node.kind() == NodeKind::Method {
                    continue;
                }
                defs[p.0 as usize].entry(node.name()).or_default().push(n);
            }
        }
        self.pkg_defs = defs;
    }

    /// Module file a re-export (`export ... from 'x'`) points to, or NONE.
    pub fn export_target(&self, f: FileId, ex: &cg_store::ExportRec) -> u32 {
        if ex.from_spec.0 == NONE {
            return NONE;
        }
        let spec = self.view.text_str(ex.from_spec);
        if spec.is_empty() {
            return NONE;
        }
        match self.modules.resolve_import(self.view.file(f).lang(), spec, f) {
            crate::modules::ImportTarget::File(t) => t.0,
            _ => NONE,
        }
    }

    // ---------------------------------------------------------------- lookups
    pub fn lookup_export(&self, f: FileId, name: &str, depth: u8) -> Option<Bound> {
        if depth > 8 || f.idx() >= self.exports.len() {
            return None;
        }
        if let Some(e) = self.exports[f.idx()].get(name) {
            match *e {
                ExportEntry::Node(n) => return Some(Bound::Node(n)),
                ExportEntry::Module(m) => return Some(Bound::Module(m)),
                ExportEntry::Re(tf, from) => {
                    if let Some(b) = self.lookup_export(tf, self.view.text_str(from), depth + 1) {
                        return Some(b);
                    }
                }
            }
        }
        if name != "default" {
            for &tf in &self.stars[f.idx()] {
                if let Some(b) = self.lookup_export(tf, name, depth + 1) {
                    return Some(b);
                }
            }
        }
        None
    }

    /// Any top-level definition called `name` in the file, exported or not (CommonJS).
    pub fn lookup_top_level(&self, f: FileId, name: &str) -> Option<NodeIdx> {
        self.top_level_nodes(f).find(|n| self.view.node(*n).name() == name)
    }

    /// Python / Rust: sibling submodule `name` of the package or module file `f`.
    pub fn submodule(&self, f: FileId, name: &str) -> Option<FileId> {
        let path = self.view.file(f).path();
        match self.view.file(f).lang() {
            Lang::Python => {
                let dir = crate::project::parent_dir(path);
                for cand in [format!("{dir}/{name}.py"), format!("{dir}/{name}/__init__.py")] {
                    if let Some(sf) = self.modules.file_by_path(cand.trim_start_matches('/')) {
                        return Some(sf);
                    }
                }
                None
            }
            Lang::Rust => match self.exports.get(f.idx())?.get(name)? {
                ExportEntry::Module(m) => Some(*m),
                _ => None,
            },
            _ => None,
        }
    }

    /// Bind the name introduced by an import record.
    pub fn bind_import(&self, f: FileId, idx: usize) -> Bound {
        let imp = &self.view.imports_of(f)[idx];
        let tgt = self.imp_targets[f.idx()].get(idx).copied().unwrap_or(ImportTarget::External);
        let imported = self.view.text_str(imp.imported);
        match tgt {
            ImportTarget::External | ImportTarget::NotFound => Bound::External,
            ImportTarget::Node(n) => Bound::Node(n),
            ImportTarget::Pkg(p) => Bound::Pkg(p),
            ImportTarget::File(tf) => {
                if imp.kind == ImportKind::Default as u8 && imported == "default" {
                    if let Some(value) = self.lookup_export(tf, COMMONJS_MODULE, 0) {
                        return value;
                    }
                }
                if imp.kind == ImportKind::Namespace as u8 || imported == "*" {
                    return Bound::Module(tf);
                }
                if imp.kind == ImportKind::Require as u8 {
                    if let Some(b) = self.lookup_export(tf, imported, 0) {
                        return b;
                    }
                    return Bound::Unknown;
                }
                if let Some(b) = self.lookup_export(tf, imported, 0) {
                    return b;
                }
                if let Some(sm) = self.submodule(tf, imported) {
                    return Bound::Module(sm);
                }
                // Python `import mod` binds the module; `from pkg import name` may name a submodule
                Bound::Unknown
            }
        }
    }

    // --------------------------------------------------------------- hierarchy
    pub fn direct_supers(&self, n: NodeIdx) -> &[NodeIdx] {
        self.supers.get(&n).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Breadth-first ancestors of a type (excluding itself), cycle-safe, depth-limited.
    pub fn ancestors(&self, n: NodeIdx) -> Vec<NodeIdx> {
        let mut out = Vec::new();
        let mut frontier = vec![n];
        let mut seen = rustc_hash::FxHashSet::default();
        seen.insert(n);
        for _ in 0..10 {
            let mut next = Vec::new();
            for t in &frontier {
                for s in self.direct_supers(*t) {
                    if seen.insert(*s) {
                        out.push(*s);
                        next.push(*s);
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        out
    }

    /// Install the hierarchy from resolved `extends` / `implements` outcomes.
    pub fn set_hierarchy(&mut self, edges: impl Iterator<Item = (NodeIdx, NodeIdx)>) {
        for (sub, sup) in edges {
            let v = self.supers.entry(sub).or_default();
            if !v.contains(&sup) && sub != sup {
                v.push(sup);
            }
        }
        for v in self.supers.values_mut() {
            v.sort();
        }
    }

    /// Hierarchy from refs already resolved in the view (incremental runs).
    pub fn hierarchy_from_view(&mut self) {
        let mut pairs = Vec::new();
        for f in self.view.files() {
            for r in self.view.refs_of_file(f) {
                if r.status == status::RESOLVED
                    && matches!(cg_core::RefKind::from_wire(r.kind), Some(cg_core::RefKind::Edge(EdgeKind::Extends | EdgeKind::Implements)))
                {
                    pairs.push((r.from, r.target));
                }
            }
        }
        self.set_hierarchy(pairs.into_iter());
    }

    // ------------------------------------------------------------------ members
    /// Member `name` declared directly by type `t` (contained child, or `Type::name` by
    /// qualified name for methods declared outside the type body: Go receivers, Rust impls).
    pub fn direct_member(&self, t: NodeIdx, name: &str) -> Option<NodeIdx> {
        let tn = self.view.node(t);
        for e in self.view.out_edges(t) {
            if e.kind == EdgeKind::Contains {
                let c = self.view.node(e.dst);
                if c.name() == name && c.kind() != NodeKind::File {
                    return Some(e.dst);
                }
            }
        }
        let q = format!("{}::{}", tn.qname(), name);
        let tfile = tn.file();
        let tpkg = self.modules.pkg_of(tfile);
        let mut best = None;
        for cand in self.view.by_qname(&q) {
            let cf = self.view.hot(cand).file;
            // Go: same package; Rust: the owning module or a proved type import.
            if cf == tfile || (!tpkg.is_none() && self.modules.pkg_of(cf) == tpkg) || self.rust_type_owner(t, cf) {
                return Some(cand);
            }
            best.get_or_insert(cand);
        }
        // Java/Kotlin nested types; C# partial classes, Ruby reopened classes, Swift
        // extensions and C++ out-of-line definitions declare members in other files.
        best.filter(|_| match self.view.file(tfile).lang().family() {
            LangFamily::Jvm => tn.qname().contains("::"),
            LangFamily::CSharp | LangFamily::Ruby | LangFamily::Swift | LangFamily::C => true,
            _ => false,
        })
    }

    /// Equal unqualified type names in different Rust modules are distinct.
    /// An out-of-module impl needs an import that identifies this exact type.
    pub fn rust_type_owner(&self, t: NodeIdx, file: FileId) -> bool {
        let node = self.view.node(t);
        match (self.modules.rust_module_of(node.file()), self.modules.rust_module_of(file)) {
            (Some(a), Some(b)) if a == b => return true,
            (Some((a, _)), Some((b, _))) if a == b => {}
            _ => return false,
        }
        self.view
            .imports_of(file)
            .iter()
            .enumerate()
            .any(|(i, import)| self.view.text_str(import.local) == node.qname() && self.bind_import(file, i) == Bound::Node(t))
    }

    /// Member lookup through the hierarchy.
    pub fn find_member(&self, t: NodeIdx, name: &str) -> Option<NodeIdx> {
        if let Some(m) = self.direct_member(t, name) {
            return Some(m);
        }
        for a in self.ancestors(t) {
            if let Some(m) = self.direct_member(a, name) {
                return Some(m);
            }
        }
        None
    }

    /// Go: promoted members through embedded structs are covered by `ancestors`
    /// (embedding is recorded as `extends`).
    pub fn node_is_type(&self, n: NodeIdx) -> bool {
        self.view.node(n).kind().is_type()
    }
}

#[allow(dead_code)]
fn _unused() -> (BindKind, u8) {
    (BindKind::Local, outkind::CLASS_RESOLVE)
}
