//! Module and package indexes (design doc 6.2 `ModuleIndex`) and import-specifier resolution
//! for every MVP language. Built once per run from the file list and the project manifests.

use crate::project::{normalize, parent_dir, ProjectInfo};
use cg_core::{FileId, Lang, LangFamily, NodeIdx, NONE};
use cg_extract::BindKind;
use cg_store::View;
use rustc_hash::FxHashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PkgId(pub u32);

impl PkgId {
    pub const NONE: PkgId = PkgId(NONE);
    pub fn is_none(self) -> bool {
        self.0 == NONE
    }
}

/// What an import specifier points at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportTarget {
    /// A module file (JS/TS/Python/Rust).
    File(FileId),
    /// A package: Go directory, Java/Kotlin package.
    Pkg(PkgId),
    /// A specific declaration (Java/Kotlin class or member named by its fully qualified name).
    Node(NodeIdx),
    /// Outside the project: npm package, standard library, external crate.
    External,
    /// Looks internal but nothing matched.
    NotFound,
}

pub struct Pkg {
    pub key: String,
    pub files: Vec<FileId>,
}

pub struct Modules<'v> {
    pub view: &'v View,
    pub project: &'v ProjectInfo,
    pub path_to_file: FxHashMap<&'v str, FileId>,
    pub dir_files: FxHashMap<&'v str, Vec<FileId>>,
    py_modules: FxHashMap<String, Vec<FileId>>,
    pub pkgs: Vec<Pkg>,
    pkg_by_key: FxHashMap<String, PkgId>,
    pub file_pkg: Vec<PkgId>,
    go_dir_pkgs: FxHashMap<&'v str, Vec<PkgId>>,
    /// Rust: (crate index, module path "a::b") -> file.
    rs_mod_to_file: FxHashMap<(u32, String), FileId>,
    /// Rust: per file (crate index, module path).
    rs_file_mod: FxHashMap<FileId, (u32, String)>,
    /// TypeScript: the declaration file a `declare module 'name' { ... }` block declares each
    /// module in (upstream `ambient-modules.ts`), first by path.
    ambient: FxHashMap<&'v str, FileId>,
}

fn dir_of(p: &str) -> &str {
    parent_dir(p)
}

/// A TypeScript declaration file: `.d.ts`, `.d.mts`, `.d.cts`.
fn is_declaration_file(path: &str) -> bool {
    path.ends_with(".d.ts") || path.ends_with(".d.mts") || path.ends_with(".d.cts")
}

impl<'v> Modules<'v> {
    pub fn build(view: &'v View, project: &'v ProjectInfo) -> Modules<'v> {
        let n = view.base.p.files.len() + view.n_files().saturating_sub(view.base.p.files.len());
        let mut m = Modules {
            view,
            project,
            path_to_file: FxHashMap::default(),
            dir_files: FxHashMap::default(),
            py_modules: FxHashMap::default(),
            pkgs: Vec::new(),
            pkg_by_key: FxHashMap::default(),
            file_pkg: vec![PkgId::NONE; n.max(view.n_files())],
            go_dir_pkgs: FxHashMap::default(),
            rs_mod_to_file: FxHashMap::default(),
            rs_file_mod: FxHashMap::default(),
            ambient: FxHashMap::default(),
        };
        let mut files: Vec<FileId> = view.files().collect();
        // Delta file ids are appended, unlike a fresh build's path-ordered ids. Package
        // representatives and first-match module aliases must use the same path order.
        files.sort_unstable_by(|a, b| view.file(*a).path().cmp(view.file(*b).path()));
        for &f in &files {
            let fr = view.file(f);
            let path = fr.path();
            m.path_to_file.insert(path, f);
            m.dir_files.entry(dir_of(path)).or_default().push(f);
        }
        for &f in &files {
            let fr = view.file(f);
            let path = fr.path();
            match fr.lang() {
                Lang::Python => m.index_python(f, path),
                Lang::Java | Lang::Kotlin => m.index_jvm(f),
                Lang::Go => m.index_go(f, path),
                Lang::Rust => m.index_rust(f, path),
                Lang::TypeScript if is_declaration_file(path) => {
                    // a script declaration file (no top-level import or export) declares its modules
                    for ex in view.exports_of(f) {
                        if view.text_str(ex.name) == cg_extract::facts::js::AMBIENT_MODULE && view.text_str(ex.from_name) == "script" {
                            m.ambient.entry(view.text_str(ex.from_spec)).or_insert(f);
                        }
                    }
                }
                _ => {}
            }
        }
        m
    }

    pub fn path_of(&self, f: FileId) -> &'v str {
        self.view.file(f).path()
    }

    /// Stable file dependency recorded for an import, including package representatives
    /// and declarations. A package's representative may change when a file is added.
    pub fn target_file(&self, target: ImportTarget) -> Option<FileId> {
        match target {
            ImportTarget::File(file) => Some(file),
            ImportTarget::Node(node) => Some(self.view.hot(node).file),
            ImportTarget::Pkg(pkg) => self.pkgs[pkg.0 as usize].files.first().copied(),
            ImportTarget::External | ImportTarget::NotFound => None,
        }
    }

    // ----------------------------------------------------------------- indexing
    fn index_python(&mut self, f: FileId, path: &str) {
        // Absolute imports name a module relative to its *package root*: climb while the
        // directory is a package (has `__init__.py`). A script outside any package is importable
        // by its stem (its directory is on the path) and by its project-relative dotted path.
        let stem = path.strip_suffix(".py").or_else(|| path.strip_suffix(".pyw")).unwrap_or(path);
        let comps: Vec<&str> = stem.split('/').collect();
        let dir_comps = &comps[..comps.len() - 1];
        let mut root_idx = dir_comps.len();
        while root_idx > 0 && self.path_to_file.contains_key(format!("{}/__init__.py", dir_comps[..root_idx].join("/")).as_str()) {
            root_idx -= 1;
        }
        let mut names: Vec<String> = Vec::new();
        let mut rel: Vec<&str> = comps[root_idx..].to_vec();
        if rel.last() == Some(&"__init__") {
            rel.pop();
        }
        names.push(rel.join("."));
        if root_idx == dir_comps.len() && root_idx > 0 {
            let mut full: Vec<&str> = comps.clone();
            if full.last() == Some(&"__init__") {
                full.pop();
            }
            names.push(full.join("."));
        }
        for key in names {
            if !key.is_empty() {
                let e = self.py_modules.entry(key).or_default();
                if !e.contains(&f) {
                    e.push(f);
                }
            }
        }
    }

    fn package_name(&self, f: FileId) -> Option<String> {
        let ff = self.view.bindings_of(f);
        ff.iter().find(|b| b.kind == BindKind::Package as u8).map(|b| self.view.text_str(b.name).to_owned())
    }

    fn add_pkg(&mut self, key: String, f: FileId) -> PkgId {
        let id = match self.pkg_by_key.get(&key) {
            Some(id) => *id,
            None => {
                let id = PkgId(self.pkgs.len() as u32);
                self.pkgs.push(Pkg { key: key.clone(), files: Vec::new() });
                self.pkg_by_key.insert(key, id);
                id
            }
        };
        self.pkgs[id.0 as usize].files.push(f);
        if f.idx() >= self.file_pkg.len() {
            self.file_pkg.resize(f.idx() + 1, PkgId::NONE);
        }
        self.file_pkg[f.idx()] = id;
        id
    }

    fn index_jvm(&mut self, f: FileId) {
        let name = self.package_name(f).unwrap_or_default();
        self.add_pkg(name, f);
    }

    fn index_go(&mut self, f: FileId, path: &str) {
        let pkg = self.package_name(f).unwrap_or_else(|| "main".to_owned());
        let dir = dir_of(path);
        let key = format!("{dir}#{pkg}");
        let id = self.add_pkg(key, f);
        // `dir_of` borrows from the view's strings, which outlive `self`
        let dir_static: &'v str = dir_of(self.view.file(f).path());
        let list = self.go_dir_pkgs.entry(dir_static).or_default();
        if !list.contains(&id) {
            list.push(id);
        }
    }

    fn index_rust(&mut self, f: FileId, path: &str) {
        // Auxiliary Cargo targets are independent crates. In particular, a bench
        // sorted before src/lib.rs must never become the library's root module.
        let separate = self.project.crates.len() as u32 + f.0;
        let (crate_idx, src_dir, root): (u32, String, Option<&str>) =
            match self.project.crates.iter().position(|c| path.starts_with(&format!("{}/", c.dir)) || c.dir.is_empty()) {
                Some(i) => {
                    let c = &self.project.crates[i];
                    let root = c.roots.iter().find(|r| self.path_to_file.contains_key(r.as_str())).map(String::as_str);
                    let src_dir =
                        root.map(dir_of).map(str::to_owned).unwrap_or_else(|| if c.dir.is_empty() { "src".to_owned() } else { format!("{}/src", c.dir) });
                    let rel = path.strip_prefix(&format!("{}/", c.dir)).unwrap_or(path);
                    let auxiliary = rel.starts_with("tests/")
                        || rel.starts_with("examples/")
                        || rel.starts_with("benches/")
                        || rel.starts_with("src/bin/")
                        || rel == "build.rs";
                    let in_source = src_dir.is_empty() || path.starts_with(&format!("{src_dir}/"));
                    if !in_source || auxiliary || (c.roots.iter().any(|r| r == path) && root != Some(path)) {
                        (separate, dir_of(path).to_owned(), Some(path))
                    } else {
                        (i as u32, src_dir, root)
                    }
                }
                None => match path.rfind("/src/") {
                    Some(i) => (NONE, path[..i + 4].to_owned(), None),
                    None if path.starts_with("src/") => (NONE, "src".to_owned(), None),
                    None => (separate, dir_of(path).to_owned(), Some(path)),
                },
            };
        let rel = if src_dir.is_empty() { Some(path) } else { path.strip_prefix(&format!("{src_dir}/")) };
        let modpath = match rel {
            _ if root == Some(path) => String::new(),
            Some(rel) => {
                let stem = rel.strip_suffix(".rs").unwrap_or(rel);
                let mut segs: Vec<&str> = stem.split('/').collect();
                match segs.last().copied() {
                    Some("mod") => {
                        segs.pop();
                    }
                    Some("lib") | Some("main") if segs.len() == 1 => {
                        segs.pop();
                    }
                    _ => {}
                }
                // src/bin/x.rs and src/bin/x/main.rs are their own crate roots
                if segs.first() == Some(&"bin") {
                    String::new()
                } else {
                    segs.join("::")
                }
            }
            None => String::new(), // tests/, examples/, benches/, build.rs: separate roots
        };
        if root == Some(path) {
            self.rs_mod_to_file.insert((crate_idx, modpath.clone()), f);
        } else {
            self.rs_mod_to_file.entry((crate_idx, modpath.clone())).or_insert(f);
        }
        self.rs_file_mod.insert(f, (crate_idx, modpath));
    }

    // ------------------------------------------------------------ lookups
    pub fn file_by_path(&self, p: &str) -> Option<FileId> {
        self.path_to_file.get(p).copied()
    }

    pub fn pkg_of(&self, f: FileId) -> PkgId {
        self.file_pkg.get(f.idx()).copied().unwrap_or(PkgId::NONE)
    }

    pub fn pkg_by_key(&self, key: &str) -> Option<PkgId> {
        self.pkg_by_key.get(key).copied()
    }

    /// Rust: (crate, module path) of a file.
    pub fn rust_module_of(&self, f: FileId) -> Option<&(u32, String)> {
        self.rs_file_mod.get(&f)
    }

    pub fn rust_module_file(&self, crate_idx: u32, modpath: &str) -> Option<FileId> {
        self.rs_mod_to_file.get(&(crate_idx, modpath.to_owned())).copied()
    }

    // ------------------------------------------------------------ resolution
    pub fn resolve_import(&self, lang: Lang, spec: &str, from: FileId) -> ImportTarget {
        let from_path = self.path_of(from);
        match lang.family() {
            LangFamily::Js => self.resolve_js(spec, from_path, lang),
            LangFamily::Python => self.resolve_python(spec, from_path),
            LangFamily::Go => self.resolve_go(spec, from_path),
            LangFamily::Jvm => self.resolve_jvm(spec),
            LangFamily::Rust => self.resolve_rust(spec, from),
            _ => ImportTarget::External,
        }
    }

    // ---- JS / TS
    fn probe_js(&self, base: &str, from_lang: Lang) -> Option<FileId> {
        if let Some(f) = self.path_to_file.get(base) {
            if self.view.file(*f).lang().is_js_family() {
                return Some(*f);
            }
        }
        // `import './a.js'` written for the TypeScript file `./a.ts`
        for (from_ext, to) in [(".js", &[".ts", ".tsx"][..]), (".jsx", &[".tsx"][..]), (".mjs", &[".mts"][..]), (".cjs", &[".cts"][..])] {
            if let Some(stem) = base.strip_suffix(from_ext) {
                for t in to {
                    if let Some(f) = self.path_to_file.get(format!("{stem}{t}").as_str()) {
                        return Some(*f);
                    }
                }
            }
        }
        let exts: &[&str] = if from_lang.is_typescript() {
            &[".ts", ".tsx", ".d.ts", ".js", ".jsx", "/index.ts", "/index.tsx", "/index.js", "/index.jsx"]
        } else {
            &[".js", ".jsx", ".mjs", ".cjs", ".ts", ".tsx", "/index.js", "/index.jsx", "/index.ts", "/index.tsx"]
        };
        for e in exts {
            if from_lang.family() == LangFamily::Js && !from_lang.is_typescript() && *e == ".d.ts" {
                continue;
            }
            // the project root itself is the empty path: `require('..')` -> `index.js`
            let cand = if base.is_empty() { e.trim_start_matches('/').to_owned() } else { format!("{base}{e}") };
            if cand.is_empty() || cand.starts_with('.') {
                continue;
            }
            if let Some(f) = self.path_to_file.get(cand.as_str()) {
                return Some(*f);
            }
        }
        None
    }

    fn resolve_js(&self, spec: &str, from_path: &str, from_lang: Lang) -> ImportTarget {
        let relative = spec == "." || spec == ".." || spec.starts_with("./") || spec.starts_with("../");
        if relative {
            let base = normalize(&format!("{}/{}", dir_of(from_path), spec));
            return match self.probe_js(&base, from_lang) {
                Some(f) => ImportTarget::File(f),
                None => ImportTarget::NotFound,
            };
        }
        if let Some(rest) = spec.strip_prefix('/') {
            return match self.probe_js(&normalize(rest), from_lang) {
                Some(f) => ImportTarget::File(f),
                None => ImportTarget::NotFound,
            };
        }
        if let Some(tc) = self.project.tsconfig_for(from_path) {
            for (pat, targets) in &tc.paths {
                let rest = match pat.find('*') {
                    Some(star) => {
                        let (pre, post) = (&pat[..star], &pat[star + 1..]);
                        if spec.len() >= pre.len() + post.len() && spec.starts_with(pre) && spec.ends_with(post) {
                            Some(&spec[pre.len()..spec.len() - post.len()])
                        } else {
                            None
                        }
                    }
                    None => (pat == spec).then_some(""),
                };
                if let Some(rest) = rest {
                    for t in targets {
                        let cand = normalize(&t.replace('*', rest));
                        if let Some(f) = self.probe_js(&cand, from_lang) {
                            return ImportTarget::File(f);
                        }
                    }
                }
            }
            if let Some(b) = &tc.base_url {
                if let Some(f) = self.probe_js(&normalize(&format!("{b}/{spec}")), from_lang) {
                    return ImportTarget::File(f);
                }
            }
        }
        // workspace packages
        let (name, sub) = {
            let mut it = spec.splitn(if spec.starts_with('@') { 3 } else { 2 }, '/');
            if spec.starts_with('@') {
                let scope = it.next().unwrap_or("");
                let n = it.next().unwrap_or("");
                (format!("{scope}/{n}"), it.next().unwrap_or("").to_owned())
            } else {
                (it.next().unwrap_or("").to_owned(), it.next().unwrap_or("").to_owned())
            }
        };
        if let Some(pkg) = self.project.packages.iter().find(|p| p.name.as_deref() == Some(name.as_str())) {
            if sub.is_empty() {
                for e in &pkg.entries {
                    if let Some(f) = self.probe_js(e.trim_end_matches(".js").trim_end_matches(".d.ts"), from_lang).or_else(|| self.probe_js(e, from_lang)) {
                        return ImportTarget::File(f);
                    }
                }
                for cand in [format!("{}/src/index", pkg.dir), format!("{}/index", pkg.dir)] {
                    if let Some(f) = self.probe_js(&normalize(&cand), from_lang) {
                        return ImportTarget::File(f);
                    }
                }
            } else {
                for cand in [format!("{}/{sub}", pkg.dir), format!("{}/src/{sub}", pkg.dir)] {
                    if let Some(f) = self.probe_js(&normalize(&cand), from_lang) {
                        return ImportTarget::File(f);
                    }
                }
            }
        }
        // a module the repository declares itself (`declare module 'mammoth' { ... }`): the
        // checker reads it before the package, and only TypeScript reads it at all
        if from_lang.is_typescript() {
            if let Some(f) = self.ambient.get(spec) {
                return ImportTarget::File(*f);
            }
        }
        ImportTarget::External
    }

    // ---- Python
    fn best_by_proximity(&self, cands: &[FileId], from_path: &str) -> Option<FileId> {
        let from_dir = dir_of(from_path);
        cands.iter().copied().max_by_key(|f| {
            let p = self.path_of(*f);
            let common = from_dir.split('/').zip(dir_of(p).split('/')).take_while(|(a, b)| a == b).count();
            (common, std::cmp::Reverse(p.len()))
        })
    }

    fn resolve_python(&self, spec: &str, from_path: &str) -> ImportTarget {
        let dots = spec.chars().take_while(|c| *c == '.').count();
        let rest = &spec[dots..];
        if dots > 0 {
            let mut base = dir_of(from_path).to_owned();
            for _ in 1..dots {
                base = parent_dir(&base).to_owned();
            }
            let rel = rest.replace('.', "/");
            let candidates =
                if rel.is_empty() { vec![format!("{base}/__init__.py")] } else { vec![format!("{base}/{rel}.py"), format!("{base}/{rel}/__init__.py")] };
            for c in candidates {
                let c = c.trim_start_matches('/').to_owned();
                if let Some(f) = self.path_to_file.get(c.as_str()) {
                    return ImportTarget::File(*f);
                }
            }
            return ImportTarget::NotFound;
        }
        match self.py_modules.get(rest) {
            Some(c) if !c.is_empty() => match self.best_by_proximity(c, from_path) {
                Some(f) => ImportTarget::File(f),
                None => ImportTarget::External,
            },
            _ => ImportTarget::External,
        }
    }

    // ---- Go
    fn resolve_go(&self, spec: &str, from_path: &str) -> ImportTarget {
        let _ = from_path;
        for gm in &self.project.go_mods {
            let rest = if spec == gm.module { Some("") } else { spec.strip_prefix(gm.module.as_str()).and_then(|r| r.strip_prefix('/')) };
            if let Some(rest) = rest {
                let dir = normalize(&format!("{}/{rest}", gm.dir));
                if let Some(pkgs) = self.go_dir_pkgs.get(dir.as_str()) {
                    // prefer the non-test package of the directory
                    let pick = pkgs.iter().find(|p| !self.pkgs[p.0 as usize].key.ends_with("_test")).or(pkgs.first());
                    if let Some(p) = pick {
                        return ImportTarget::Pkg(*p);
                    }
                }
                return ImportTarget::NotFound;
            }
        }
        ImportTarget::External
    }

    // ---- Java / Kotlin
    fn resolve_jvm(&self, spec: &str) -> ImportTarget {
        let segs: Vec<&str> = spec.split('.').collect();
        for i in (1..segs.len()).rev() {
            let qname = format!("{}::{}", segs[..i].join("."), segs[i..].join("::"));
            if let Some(n) = self.view.by_qname(&qname).next() {
                return ImportTarget::Node(n);
            }
        }
        // whole package (wildcard import)
        if let Some(p) = self.pkg_by_key.get(spec) {
            return ImportTarget::Pkg(*p);
        }
        // Kotlin default-package top-level declarations and unknown project packages
        let first = segs.first().copied().unwrap_or("");
        if matches!(first, "java" | "javax" | "kotlin" | "kotlinx" | "android" | "androidx" | "jakarta" | "org" | "com" | "io" | "net")
            && !self.pkg_by_key.keys().any(|k| k == spec || k.starts_with(&format!("{spec}.")) || spec.starts_with(&format!("{k}.")))
        {
            return ImportTarget::External;
        }
        if self.pkg_by_key.keys().any(|k| spec.starts_with(&format!("{k}.")) || k.starts_with(&format!("{spec}."))) {
            ImportTarget::NotFound
        } else {
            ImportTarget::External
        }
    }

    // ---- Rust
    fn resolve_rust(&self, spec: &str, from: FileId) -> ImportTarget {
        self.resolve_rust_path(spec, from).0
    }

    /// The longest module prefix and the number of trailing declaration/member
    /// segments. Returning the tail prevents `crate::api::Type` being mistaken
    /// for the module itself when resolving a qualified type or return value.
    pub fn resolve_rust_path(&self, spec: &str, from: FileId) -> (ImportTarget, usize) {
        let Some((crate_idx, modpath)) = self.rs_file_mod.get(&from) else { return (ImportTarget::External, 0) };
        let segs: Vec<&str> = spec.split("::").filter(|s| !s.is_empty()).collect();
        let Some(first) = segs.first().copied() else { return (ImportTarget::External, 0) };
        let (target_crate, mut path): (u32, Vec<String>) = match first {
            "crate" => (*crate_idx, segs[1..].iter().map(|s| s.to_string()).collect()),
            "self" => (*crate_idx, modpath.split("::").filter(|s| !s.is_empty()).map(str::to_owned).chain(segs[1..].iter().map(|s| s.to_string())).collect()),
            "super" => {
                let mut base: Vec<String> = modpath.split("::").filter(|s| !s.is_empty()).map(str::to_owned).collect();
                let mut i = 0;
                while i < segs.len() && segs[i] == "super" {
                    base.pop();
                    i += 1;
                }
                base.extend(segs[i..].iter().map(|s| s.to_string()));
                (*crate_idx, base)
            }
            other => {
                // another workspace crate by name?
                if let Some(ci) = self.project.crates.iter().position(|c| c.name == other.replace('-', "_")) {
                    (ci as u32, segs[1..].iter().map(|s| s.to_string()).collect())
                } else {
                    // 2018-style relative path: first segment names a child module
                    let mut rel: Vec<String> = modpath.split("::").filter(|s| !s.is_empty()).map(str::to_owned).collect();
                    rel.push(first.to_owned());
                    if self.rs_mod_to_file.contains_key(&(*crate_idx, rel.join("::"))) {
                        let mut full: Vec<String> = modpath.split("::").filter(|s| !s.is_empty()).map(str::to_owned).collect();
                        full.extend(segs.iter().map(|s| s.to_string()));
                        (*crate_idx, full)
                    } else {
                        return (ImportTarget::External, 0);
                    }
                }
            }
        };
        let full_len = path.len();
        // longest module prefix that exists
        while !path.is_empty() || self.rs_mod_to_file.contains_key(&(target_crate, String::new())) {
            if let Some(f) = self.rs_mod_to_file.get(&(target_crate, path.join("::"))) {
                return (ImportTarget::File(*f), full_len - path.len());
            }
            if path.pop().is_none() {
                break;
            }
        }
        (ImportTarget::NotFound, 0)
    }
}
