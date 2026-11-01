//! Cross-file NestJS module registrations and bootstrap prefixes. The original route nodes
//! retain their controller-local identity; HTTP matching uses their effective mounted paths.

use super::href::{index_at_depth0, match_bracket, object_field, read_string_at, HOLE};
use crate::engine::Engine;
use crate::synth::scan::call_args;
use crate::synth::sites::PASS_NEST;
use cg_core::{FileId, Lang, LangFamily, NodeIdx, NodeKind, NONE};
use cg_extract::{FileHook, HookCtx, HookOut, SiteFact};
use regex::Regex;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

const MODULE: u8 = 0;
const MOUNT: u8 = 1;
const BOOT: u8 = 2;

#[derive(Default, Serialize, Deserialize)]
struct Module {
    name: String,
    controllers: Vec<String>,
    imports: Vec<String>,
}
#[derive(Serialize, Deserialize)]
struct Mount {
    module: String,
    path: String,
}
#[derive(Serialize, Deserialize)]
struct Exclusion {
    path: String,
    method: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct Boot {
    root: String,
    prefix: String,
    exclude: Vec<Exclusion>,
}

macro_rules! re {
    ($name:ident, $pat:expr) => {
        fn $name() -> &'static Regex {
            static R: OnceLock<Regex> = OnceLock::new();
            R.get_or_init(|| Regex::new($pat).expect("static regex"))
        }
    };
}
re!(module_re, r"@Module\s*\(");
re!(class_re, r"\bclass\s+([A-Za-z_$][\w$]*)\b");
re!(mount_re, r"\b([A-Za-z_$][\w$]*)\s*\.\s*register\s*\(");
re!(boot_re, r"\b(?:const|let|var)\s+([A-Za-z_$][\w$]*)(?:\s*:\s*[^=;]+)?\s*=\s*(?:await\s+)?([A-Za-z_$][\w$]*)\s*\.\s*create(?:\s*<[^>]+>)?\s*\(");
re!(prefix_re, r"\b([A-Za-z_$][\w$]*)\s*\.\s*setGlobalPrefix\s*\(");
re!(ident_re, r"^[A-Za-z_$][\w$]*(?:\.[A-Za-z_$][\w$]*)?$");

fn ident(s: &str) -> Option<String> {
    let s = s.trim();
    ident_re().is_match(s).then(|| s.to_owned())
}

fn literal(s: &str) -> Option<String> {
    let s = s.trim();
    let value = read_string_at(s, 0)?;
    let end = match s.as_bytes().first()? {
        b'`' => super::href::template_end(s, 0)?,
        _ => super::href::string_end(s, 0)?,
    };
    (end + 1 == s.len() && !value.contains(HOLE)).then_some(value)
}

fn items(s: &str) -> Option<Vec<&str>> {
    let s = s.trim();
    if !s.starts_with('[') || match_bracket(s, 0)? + 1 != s.len() {
        return None;
    }
    let body = &s[1..s.len() - 1];
    let mut out = Vec::new();
    let mut at = 0;
    while at < body.len() {
        let end = index_at_depth0(body, b',', at).unwrap_or(body.len());
        let item = body[at..end].trim();
        if !item.is_empty() {
            out.push(item);
        }
        at = end + 1;
    }
    Some(out)
}

fn names(value: Option<&str>) -> Vec<String> {
    value.and_then(items).unwrap_or_default().into_iter().filter_map(ident).collect()
}

fn join(prefix: &str, path: &str) -> String {
    let parts: Vec<_> = [prefix, path].iter().map(|s| s.trim_matches('/')).filter(|s| !s.is_empty()).collect();
    format!("/{}", parts.join("/"))
}

fn mounts(value: &str, parent: &str, out: &mut Vec<Mount>, depth: usize) {
    if depth > 16 {
        return;
    }
    for item in items(value).unwrap_or_default() {
        if !item.starts_with('{') || match_bracket(item, 0) != Some(item.len() - 1) {
            continue;
        }
        // Unknown paths must not be treated as an empty prefix.
        let Some(path) = object_field(item, "path").and_then(literal) else { continue };
        let path = join(parent, &path);
        if let Some(module) = object_field(item, "module").and_then(ident) {
            out.push(Mount { module, path: path.clone() });
        }
        if let Some(children) = object_field(item, "children") {
            mounts(children, &path, out, depth + 1);
        }
    }
}

fn exclusions(options: &str) -> Option<Vec<Exclusion>> {
    let options = options.trim();
    if !options.starts_with('{') || match_bracket(options, 0)? + 1 != options.len() {
        return None;
    }
    let Some(exclude) = object_field(options, "exclude") else { return Some(Vec::new()) };
    let mut out = Vec::new();
    for item in items(exclude)? {
        if let Some(path) = literal(item) {
            out.push(Exclusion { path, method: None });
        } else {
            let path = object_field(item, "path").and_then(literal)?;
            let method = object_field(item, "method").map(|m| m.trim().strip_prefix("RequestMethod.").map(str::to_owned)).transpose_option()?;
            out.push(Exclusion { path, method });
        }
    }
    Some(out)
}

// Like Option::transpose, but an absent field is valid and a present unknown field is not.
trait TransposeOption<T> {
    fn transpose_option(self) -> Option<Option<T>>;
}
impl<T> TransposeOption<T> for Option<Option<T>> {
    fn transpose_option(self) -> Option<Option<T>> {
        match self {
            None => Some(None),
            Some(Some(x)) => Some(Some(x)),
            Some(None) => None,
        }
    }
}

pub struct NestScanner;
impl FileHook for NestScanner {
    fn name(&self) -> &'static str {
        "nest-module-sites"
    }
    fn applies(&self, lang: Lang, _: &str) -> bool {
        lang.family() == LangFamily::Js
    }
    fn triggers(&self) -> &'static [&'static str] {
        &["@Module", "RouterModule", "setGlobalPrefix"]
    }
    fn run(&self, ctx: &HookCtx<'_>, out: &mut HookOut) {
        let source = cg_extract::post::strip_js_comments(ctx.src);
        let src = source.as_str();
        let aliases = |original: &str| -> Vec<&str> {
            ctx.facts.imports.iter().filter(|i| i.spec == "@nestjs/core" && i.imported == original).map(|i| i.local.as_str()).collect()
        };
        let routers = aliases("RouterModule");
        let factories = aliases("NestFactory");
        let mut add = |role, payload: String| {
            out.sites.push(SiteFact { pass: PASS_NEST, role, key_kind: 0, key_a: String::new(), key_b: String::new(), node_row: NONE, line: 0, payload })
        };
        for hit in module_re().find_iter(src) {
            let Some(end) = match_bracket(src, hit.end() - 1) else { continue };
            let body = src[hit.end()..end].trim();
            if !body.starts_with('{') || match_bracket(body, 0).is_none_or(|i| i + 1 != body.len()) {
                continue;
            }
            let rest = &src[end + 1..];
            let Some(class) = class_re().captures(rest).filter(|c| c.get(0).unwrap().start() < 512) else { continue };
            let module =
                Module { name: class[1].to_owned(), controllers: names(object_field(body, "controllers")), imports: names(object_field(body, "imports")) };
            add(MODULE, serde_json::to_string(&module).unwrap());
        }
        for hit in mount_re().captures_iter(src).filter(|hit| &hit[1] == "RouterModule" || routers.contains(&&hit[1])) {
            let Some(args) = call_args(src, hit.get(0).unwrap().end() - 1) else { continue };
            let mut found = Vec::new();
            if let Some(first) = args.first() {
                mounts(first, "", &mut found, 0);
            }
            for mount in found {
                add(MOUNT, serde_json::to_string(&mount).unwrap());
            }
        }
        let roots: Vec<_> = boot_re()
            .captures_iter(src)
            .filter(|hit| &hit[2] == "NestFactory" || factories.contains(&&hit[2]))
            .filter_map(|hit| {
                let args = call_args(src, hit.get(0)?.end() - 1)?;
                Some((ident(args.first()?)?, hit[1].to_owned()))
            })
            .collect();
        // If multiple apps are bootstrapped in one file, do not guess which receives a prefix.
        if roots.len() == 1 {
            for hit in prefix_re().captures_iter(src).filter(|hit| hit[1] == roots[0].1) {
                let Some(args) = call_args(src, hit.get(0).unwrap().end() - 1) else { continue };
                let Some(prefix) = args.first().and_then(|s| literal(s)) else { continue };
                let exclude = match args.get(1) {
                    Some(options) => match exclusions(options) {
                        Some(v) => v,
                        None => continue,
                    },
                    None => Vec::new(),
                };
                let boot = Boot { root: roots[0].0.clone(), prefix, exclude };
                add(BOOT, serde_json::to_string(&boot).unwrap());
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct EffectivePath {
    pub path: String,
    pub method: Option<String>,
    /// (optional method, path) excluded from this effective path.
    pub excluded: Vec<(Option<String>, String)>,
}

/// Exclusions can cover only one value or method of a route. Retain the normal prefixed
/// route for other requests, but remove the intersection from it and add it unprefixed.
fn boot_paths(boot: &Boot, method: &str, path: &str) -> Vec<EffectivePath> {
    let mut out = Vec::new();
    let mut fully_excluded = false;
    let mut denied = Vec::new();
    for x in &boot.exclude {
        let method_filter = x.method.as_ref().filter(|m| *m != "ALL").cloned();
        if method != "ALL" && method != "ANY" && method_filter.as_ref().is_some_and(|m| m != method) {
            continue;
        }
        let a: Vec<_> = x.path.trim_matches('/').split('/').collect();
        let b: Vec<_> = path.trim_matches('/').split('/').collect();
        if a.len() != b.len() {
            continue;
        }
        let mut partial = false;
        let mut intersection = Vec::new();
        for (excluded, route) in a.iter().zip(&b) {
            if excluded == route || excluded.starts_with(':') {
                intersection.push(*route);
            } else if route.starts_with(':') {
                intersection.push(*excluded);
                partial = true;
            } else {
                intersection.clear();
                break;
            }
        }
        if intersection.len() == b.len() {
            let intersection = format!("/{}", intersection.join("/"));
            let applies_to_all = method_filter.is_none() || (method != "ALL" && method != "ANY");
            fully_excluded |= !partial && applies_to_all;
            denied.push((method_filter.clone(), join(&boot.prefix, &intersection)));
            out.push(EffectivePath { path: intersection, method: if method == "ALL" || method == "ANY" { method_filter } else { None }, excluded: Vec::new() });
        }
    }
    if !fully_excluded {
        out.push(EffectivePath { path: join(&boot.prefix, path), method: None, excluded: denied });
    }
    out
}

/// Effective HTTP paths for Nest route nodes. Unresolved symbols are left unmounted rather
/// than connected by matching class names in unrelated files.
pub(crate) fn effective_paths(e: &Engine<'_>) -> FxHashMap<NodeIdx, Vec<EffectivePath>> {
    let v = e.view;
    let mut modules: FxHashMap<NodeIdx, (FileId, Module)> = FxHashMap::default();
    let mut pending_mounts: Vec<(FileId, Mount)> = Vec::new();
    let mut boots: Vec<(FileId, Boot)> = Vec::new();
    for f in v.files() {
        for site in v.sites_of(f).iter().filter(|s| s.pass == PASS_NEST) {
            let data = v.text_str(site.payload);
            match site.role {
                MODULE => {
                    if let Ok(m) = serde_json::from_str::<Module>(data) {
                        if let Some(n) = super::js_symbol(e, f, &m.name) {
                            modules.insert(n, (f, m));
                        }
                    }
                }
                MOUNT => {
                    if let Ok(m) = serde_json::from_str::<Mount>(data) {
                        pending_mounts.push((f, m));
                    }
                }
                BOOT => {
                    if let Ok(b) = serde_json::from_str::<Boot>(data) {
                        boots.push((f, b));
                    }
                }
                _ => {}
            }
        }
    }
    let mut mounts_by_module: FxHashMap<NodeIdx, Vec<(Option<NodeIdx>, String)>> = FxHashMap::default();
    for (file, mount) in pending_mounts {
        let Some(target) = super::js_symbol(e, file, &mount.module) else { continue };
        let mut owners = modules.iter().filter(|(_, (f, _))| *f == file).map(|(n, _)| *n);
        let first = owners.next();
        let origin = if owners.next().is_none() { first } else { None };
        mounts_by_module.entry(target).or_default().push((origin, mount.path));
    }
    let mut contexts: FxHashMap<NodeIdx, Vec<usize>> = FxHashMap::default();
    let mut reachable = Vec::new();
    for (index, (file, boot)) in boots.iter().enumerate() {
        let mut visited = FxHashSet::default();
        let Some(root) = super::js_symbol(e, *file, &boot.root) else { continue };
        let mut queue = vec![root];
        while let Some(module) = queue.pop() {
            if !visited.insert(module) {
                continue;
            }
            contexts.entry(module).or_default().push(index);
            if let Some((f, m)) = modules.get(&module) {
                queue.extend(m.imports.iter().filter_map(|name| super::js_symbol(e, *f, name)));
            }
        }
        reachable.push((index, visited));
    }
    let mut controller_modules: FxHashMap<NodeIdx, Vec<NodeIdx>> = FxHashMap::default();
    for (module, (file, m)) in &modules {
        for controller in m.controllers.iter().filter_map(|name| super::js_symbol(e, *file, name)) {
            controller_modules.entry(controller).or_default().push(*module);
        }
    }
    let mut result = FxHashMap::default();
    for f in v.files() {
        for n in v.nodes_in_file(f).map(NodeIdx) {
            let route = v.node(n);
            if route.kind() != NodeKind::Route || route.signature() != Some("nestjs") {
                continue;
            }
            let Some((method, local_path)) = route.name().split_once(' ') else { continue };
            if !local_path.starts_with('/') {
                continue;
            }
            let class = v
                .nodes_in_file(f)
                .map(NodeIdx)
                .filter(|c| {
                    let c = v.node(*c);
                    c.kind() == NodeKind::Class && c.start_line() <= route.start_line() && c.end_line() >= route.start_line()
                })
                .min_by_key(|c| v.node(*c).end_line() - v.node(*c).start_line());
            let Some(owners) = class.and_then(|c| controller_modules.get(&c)) else { continue };
            let mut paths = Vec::new();
            for owner in owners {
                if let Some(indices) = contexts.get(owner) {
                    for index in indices {
                        let boot = &boots[*index].1;
                        let reaches = reachable.iter().find(|(i, _)| i == index).map(|(_, set)| set);
                        let mut prefixes: Vec<&str> = mounts_by_module
                            .get(owner)
                            .into_iter()
                            .flatten()
                            .filter_map(|(origin, path)| {
                                (origin.is_some_and(|n| reaches.is_some_and(|set| set.contains(&n))) || (origin.is_none() && boots.len() == 1))
                                    .then_some(path.as_str())
                            })
                            .collect();
                        if prefixes.is_empty() {
                            prefixes.push("");
                        }
                        for mount in prefixes {
                            for full in boot_paths(boot, method, &join(mount, local_path)) {
                                if !paths.contains(&full) {
                                    paths.push(full);
                                }
                            }
                        }
                    }
                } else {
                    let mut prefixes: Vec<&str> = mounts_by_module.get(owner).into_iter().flatten().map(|(_, path)| path.as_str()).collect();
                    if prefixes.is_empty() {
                        prefixes.push("");
                    }
                    for mount in prefixes {
                        let path = EffectivePath { path: join(mount, local_path), method: None, excluded: Vec::new() };
                        if !paths.contains(&path) {
                            paths.push(path);
                        }
                    }
                }
            }
            paths.sort();
            result.insert(n, paths);
        }
    }
    result
}
