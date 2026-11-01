//! Go structural interfaces (upstream `goImplementsEdges`, with #3185's type check): a type
//! implements an interface when its method set, embedded types' methods included, has every
//! method the interface asks for with the same parameter and result counts and types. An
//! unexported method name only matches within its package. A type is compared by its name with
//! the package qualifier left off (the two declarations may sit in different packages); a true
//! alias, a type parameter and a `func`/`struct`/`interface` literal type rule nothing out.
//!
//! A type embedded from outside the index brings what `go_std` knows of it (`io.Reader`'s
//! `Read`, `error`'s `Error`), by name and counts; any other outside type is opaque, and an
//! interface embedding one is satisfied only by a type that embeds the same one.

use super::sites::{PASS_GO_EMBEDS, PASS_GO_TYPES, ROLE_GO_ALIAS, ROLE_GO_GENERIC};
use super::{Out, MAX_GO_IMPLEMENTERS};
use crate::engine::Engine;
use crate::project::parent_dir;
use cg_core::{EdgeKind, Lang, NodeIdx, NodeKind};
use rustc_hash::{FxHashMap, FxHashSet};

/// A method's parameter and result types, each comparable or `None` (open).
type Sig = Vec<Option<String>>;

/// The top-level items of the bracketed list opening at `text[open]`, and where it closes.
fn list_items(text: &str, open: usize) -> Option<(Vec<&str>, usize)> {
    let b = text.as_bytes();
    let mut items = Vec::new();
    let (mut depth, mut start) = (0i32, open + 1);
    let mut i = open;
    while i < b.len() {
        match b[i] {
            b'"' | b'`' | b'\'' => {
                let q = b[i];
                i += 1;
                while i < b.len() && b[i] != q {
                    if q != b'`' && b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    let last = text[start..i].trim();
                    if !last.is_empty() {
                        items.push(last);
                    }
                    return Some((items, i));
                }
            }
            b',' if depth == 1 => {
                let item = text[start..i].trim();
                if !item.is_empty() {
                    items.push(item);
                }
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// `(ctx context.Context, a, b int) (int, error)` -> parameters and results.
fn signature_parts(sig: &str) -> Option<(Vec<&str>, Vec<&str>)> {
    let sig = sig.trim();
    if !sig.starts_with('(') {
        return None;
    }
    let (params, end) = list_items(sig, 0)?;
    let rest = sig[end + 1..].trim();
    if !rest.starts_with('(') {
        return Some((params, if rest.is_empty() { vec![] } else { vec![rest] }));
    }
    let (results, end) = list_items(rest, 0)?;
    rest[end + 1..].trim().is_empty().then_some((params, results))
}

const TYPE_WORDS: &[&str] = &["func", "chan", "map", "struct", "interface"];

/// The types of a list's items: `a, b int` gives `a` the type `int`; a list with no names is
/// a list of types.
fn item_types<'a>(items: &[&'a str]) -> Vec<&'a str> {
    let named: Vec<Option<&str>> = items
        .iter()
        .map(|item| {
            let (name, ty) = item.split_once(char::is_whitespace)?;
            let ok = name.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_') && name.chars().all(|c| c.is_alphanumeric() || c == '_');
            (ok && !TYPE_WORDS.contains(&name)).then(|| ty.trim())
        })
        .collect();
    if named.iter().all(Option::is_none) {
        return items.to_vec();
    }
    let mut out = vec![""; items.len()];
    let mut ty = "";
    for i in (0..items.len()).rev() {
        if let Some(t) = named[i] {
            ty = t;
        }
        out[i] = ty;
    }
    out
}

/// A type as two declarations can compare it, or `None` for one that holds a list of its own.
fn comparable(written: &str) -> Option<String> {
    let flat: String = written.chars().filter(|c| !c.is_whitespace()).collect();
    let flat = flat.replace("interface{}", "any");
    if flat.contains("func(") || flat.contains("struct{") || flat.contains("interface{") {
        return None;
    }
    // drop package qualifiers: `*http.Request` -> `*Request`
    let mut out = String::with_capacity(flat.len());
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String, next: Option<char>| {
        if next == Some('.') {
            word.clear();
        } else {
            out.push_str(match word.as_str() {
                "uint8" => "byte",
                "int32" => "rune",
                w => w,
            });
            word.clear();
        }
    };
    let chars: Vec<char> = flat.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_alphanumeric() || c == '_' {
            word.push(c);
        } else {
            let qualifier = c == '.' && !word.is_empty();
            flush(&mut word, &mut out, Some(c));
            if !qualifier {
                out.push(c);
            }
        }
        i += 1;
    }
    flush(&mut word, &mut out, None);
    Some(out)
}

fn method_types(sig: &str) -> Option<(usize, usize, Sig)> {
    let (params, results) = signature_parts(sig)?;
    let types = item_types(&params).into_iter().chain(item_types(&results)).map(comparable).collect();
    Some((params.len(), results.len(), types))
}

/// What a method matches an interface method by: its name, plus its package when unexported.
fn method_key(view: &cg_store::View, m: NodeIdx) -> String {
    let node = view.node(m);
    name_key(view, node.file(), node.name())
}

fn name_key(view: &cg_store::View, file: cg_core::FileId, name: &str) -> String {
    if name.chars().next().is_some_and(char::is_uppercase) {
        name.to_owned()
    } else {
        format!("{}\0{name}", parent_dir(view.file(file).path()))
    }
}

/// [`method_key`] of a standard-library method: an unexported one is spelled so that no
/// project directory matches it.
fn std_key(path: &str, name: &str) -> String {
    if name.chars().next().is_some_and(char::is_uppercase) {
        name.to_owned()
    } else {
        format!("std:{path}\0{name}")
    }
}

/// What the types a Go type embeds from outside the index bring.
#[derive(Default)]
struct Outside {
    /// Standard-library methods: key, parameter count, result count.
    std: Vec<(String, usize, usize)>,
    /// Outside types the table does not know, by import path (or directory) and name.
    opaque: Vec<String>,
}

/// One member of a method set: a declaration, or a standard-library method with no node.
type Member = (Option<NodeIdx>, Option<(usize, usize, Sig)>);

pub(super) fn go_implements(e: &Engine<'_>, out: &mut Out) {
    let view = e.view;
    let mut implementers: Vec<NodeIdx> = Vec::new();
    let mut interfaces: Vec<NodeIdx> = Vec::new();
    let mut is_go_type: FxHashSet<NodeIdx> = FxHashSet::default();
    // type name -> declaring nodes, per package directory, for receiver methods of other files
    let mut types_in_dir: FxHashMap<(&str, &str), NodeIdx> = FxHashMap::default();
    let mut methods_of: FxHashMap<NodeIdx, Vec<NodeIdx>> = FxHashMap::default();
    // names standing for some other type: true aliases and type parameters
    let mut open_names: FxHashSet<&str> = FxHashSet::default();
    let mut receiver_methods: Vec<NodeIdx> = Vec::new();
    for f in view.files() {
        if view.file(f).lang() != Lang::Go {
            continue;
        }
        let dir = parent_dir(view.file(f).path());
        for s in view.sites_of(f).iter().filter(|s| s.pass == PASS_GO_TYPES) {
            match s.role {
                ROLE_GO_ALIAS => {
                    open_names.insert(view.text_str(cg_core::StrId(s.key_a)));
                }
                ROLE_GO_GENERIC => open_names.extend(view.text_str(s.payload).split(',').filter(|p| !p.is_empty())),
                _ => {}
            }
        }
        for n in view.nodes_in_file(f) {
            let n = NodeIdx(n);
            let node = view.node(n);
            match node.kind() {
                NodeKind::Interface => {
                    interfaces.push(n);
                    is_go_type.insert(n);
                    types_in_dir.entry((dir, node.name())).or_insert(n);
                }
                NodeKind::Struct | NodeKind::Class | NodeKind::TypeAlias => {
                    implementers.push(n);
                    is_go_type.insert(n);
                    types_in_dir.entry((dir, node.name())).or_insert(n);
                }
                NodeKind::Method | NodeKind::Function => {
                    if let Some((tq, name)) = node.qname().rsplit_once("::") {
                        if name == node.name() && !tq.contains("::") {
                            receiver_methods.push(n);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let stable_key = |n: &NodeIdx| {
        let node = view.node(*n);
        (view.file(node.file()).path(), node.start_line(), node.start_col(), node.name())
    };
    implementers.sort_by_key(stable_key);
    interfaces.sort_by_key(stable_key);
    // a type's own methods: those in its body (interfaces) and the receiver methods its
    // package declares anywhere
    for &t in &interfaces {
        let own: Vec<NodeIdx> =
            view.out_edges(t).filter(|ed| ed.kind == EdgeKind::Contains && view.node(ed.dst).kind() == NodeKind::Method).map(|ed| ed.dst).collect();
        methods_of.insert(t, own);
    }
    for &m in &receiver_methods {
        let node = view.node(m);
        let tq = node.qname().rsplit_once("::").map_or("", |x| x.0);
        let dir = parent_dir(view.file(node.file()).path());
        if let Some(&t) = types_in_dir.get(&(dir, tq)) {
            if view.node(t).kind() != NodeKind::Interface {
                methods_of.entry(t).or_default().push(m);
            }
        }
    }
    // A field at the same or a shallower embedding depth also blocks a promoted method.
    let mut fields_of: FxHashMap<NodeIdx, Vec<String>> = FxHashMap::default();
    for &t in &implementers {
        let node = view.node(t);
        let f = node.file();
        let start = cg_core::text::pack_pos(node.start_line(), node.start_col());
        let Some((scope, _)) = view.scopes_of(f).iter().enumerate().find(|(_, s)| s.kind == cg_extract::ScopeKind::Class as u8 && s.start == start) else {
            continue;
        };
        let scope = view.scope_base(f) + scope as u32;
        for b in view.bindings_of(f).iter().filter(|b| b.scope.0 == scope && b.kind == cg_extract::BindKind::Field as u8) {
            fields_of.entry(t).or_default().push(name_key(view, f, view.text_str(b.name)));
        }
    }
    // Embedded types the resolver did not bind. The kernel writes an embedding without its
    // package (`sandbox.Manager` is `Manager`), so the written qualifier finds a project type the
    // name alone could not; a type from outside the project brings what the standard-library table
    // knows of it, or stays opaque.
    let mut extra: FxHashMap<NodeIdx, Vec<NodeIdx>> = FxHashMap::default();
    let mut outside: FxHashMap<NodeIdx, Outside> = FxHashMap::default();
    for f in view.files() {
        if view.file(f).lang() != Lang::Go {
            continue;
        }
        let dir = parent_dir(view.file(f).path());
        for s in view.sites_of(f).iter().filter(|s| s.pass == PASS_GO_EMBEDS && s.node.0 != cg_core::NONE) {
            let (qualifier, name) = (view.text_str(cg_core::StrId(s.key_a)), view.text_str(cg_core::StrId(s.key_b)));
            if e.direct_supers(s.node).iter().any(|t| is_go_type.contains(t) && view.node(*t).name() == name) {
                continue; // bound: its methods come through the closure
            }
            let path = view.text_str(s.payload);
            let target = if qualifier.is_empty() { crate::modules::ImportTarget::NotFound } else { e.modules.resolve_import(Lang::Go, path, f) };
            let project = match target {
                _ if qualifier.is_empty() => types_in_dir.get(&(dir, name)).copied(),
                crate::modules::ImportTarget::Pkg(p) => {
                    e.pkg_defs.get(p.0 as usize).and_then(|m| m.get(name)).and_then(|l| l.iter().copied().find(|n| is_go_type.contains(n)))
                }
                _ => None,
            };
            if let Some(t) = project.filter(|t| *t != s.node) {
                extra.entry(s.node).or_default().push(t);
                continue;
            }
            if !qualifier.is_empty() && !matches!(target, crate::modules::ImportTarget::External) {
                continue; // a project package without the type: nothing is known of it
            }
            let entry = outside.entry(s.node).or_default();
            match super::go_std::methods(path, name) {
                Some(ms) => entry.std.extend(ms.into_iter().map(|(m, p, r)| (std_key(path, m), p, r))),
                None if !path.is_empty() => entry.opaque.push(format!("{path}.{name}")),
                None => entry.opaque.push(format!("{dir}#{name}")),
            }
        }
    }
    // embedding: Go records it as `extends`
    let embeds = |t: NodeIdx| -> Vec<NodeIdx> {
        let mut v: Vec<NodeIdx> = e.direct_supers(t).iter().copied().filter(|s| is_go_type.contains(s)).collect();
        for x in extra.get(&t).into_iter().flatten() {
            if !v.contains(x) {
                v.push(*x);
            }
        }
        v
    };
    let closure = |t: NodeIdx| -> Vec<NodeIdx> {
        let mut seen = vec![t];
        let mut i = 0;
        while i < seen.len() && seen.len() < 64 {
            for s in embeds(seen[i]) {
                if !seen.contains(&s) {
                    seen.push(s);
                }
            }
            i += 1;
        }
        seen
    };
    let std_member = |p: usize, r: usize| -> Member { (None, Some((p, r, vec![None; p + r]))) };
    let opaque_of =
        |t: NodeIdx| -> FxHashSet<&str> { closure(t).iter().filter_map(|ty| outside.get(ty)).flat_map(|o| o.opaque.iter().map(String::as_str)).collect() };
    // For interfaces retain declarations from embedded interfaces. For concrete types, Go
    // selects the shallowest embedded method; two paths at that depth are ambiguous even if
    // they eventually lead to the same declaration (a diamond).
    type MethodSet = FxHashMap<String, Vec<Member>>;
    let interface_methods = |t: NodeIdx| -> MethodSet {
        let mut set: MethodSet = FxHashMap::default();
        for ty in closure(t) {
            for &m in methods_of.get(&ty).map(Vec::as_slice).unwrap_or(&[]) {
                set.entry(method_key(view, m)).or_default().push((Some(m), view.node(m).signature().and_then(method_types)));
            }
            for (key, p, r) in outside.get(&ty).map(|o| o.std.as_slice()).unwrap_or(&[]) {
                set.entry(key.clone()).or_default().push(std_member(*p, *r));
            }
        }
        set
    };
    let concrete_methods = |t: NodeIdx| -> MethodSet {
        // key -> (depth, declarations reached there, standard-library methods reached there, a field)
        type Selected = FxHashMap<String, (usize, Vec<NodeIdx>, Vec<(usize, usize)>, bool)>;
        let mut selected: Selected = FxHashMap::default();
        let mut frontier = vec![(t, vec![t])];
        // standard-library types embedded one level below the current frontier
        let mut std_next: Vec<&(String, usize, usize)> = Vec::new();
        for depth in 0..10 {
            if frontier.is_empty() && std_next.is_empty() {
                break;
            }
            if frontier.len() > 1024 {
                return MethodSet::default();
            }
            for (key, p, r) in std::mem::take(&mut std_next) {
                let entry = selected.entry(key.clone()).or_insert_with(|| (depth, Vec::new(), Vec::new(), false));
                if entry.0 == depth {
                    entry.2.push((*p, *r));
                }
            }
            let mut next = Vec::new();
            for (ty, path) in frontier {
                for &m in methods_of.get(&ty).map(Vec::as_slice).unwrap_or(&[]) {
                    let entry = selected.entry(method_key(view, m)).or_insert_with(|| (depth, Vec::new(), Vec::new(), false));
                    if entry.0 == depth {
                        entry.1.push(m);
                    }
                }
                for key in fields_of.get(&ty).into_iter().flatten() {
                    let entry = selected.entry(key.clone()).or_insert_with(|| (depth, Vec::new(), Vec::new(), false));
                    if entry.0 == depth {
                        entry.3 = true;
                    }
                }
                std_next.extend(outside.get(&ty).map(|o| o.std.as_slice()).unwrap_or(&[]));
                for embedded in embeds(ty) {
                    if !path.contains(&embedded) {
                        let mut child = path.clone();
                        child.push(embedded);
                        next.push((embedded, child));
                    }
                }
            }
            frontier = next;
        }
        selected
            .into_iter()
            .filter_map(|(k, (_, ms, std, field))| {
                if field || ms.len() + std.len() != 1 {
                    return None;
                }
                let member = match (ms.first(), std.first()) {
                    (Some(m), _) => (Some(*m), view.node(*m).signature().and_then(method_types)),
                    (None, Some((p, r))) => std_member(*p, *r),
                    _ => return None,
                };
                Some((k, vec![member]))
            })
            .collect()
    };
    let open = |t: &Option<String>| -> bool {
        match t {
            None => true,
            Some(t) => t
                .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .any(|w| open_names.contains(w) || (w.len() == 1 && w.chars().all(|c| c.is_ascii_uppercase()))),
        }
    };
    let fits = |have: &Option<(usize, usize, Sig)>, want: &Option<(usize, usize, Sig)>| -> bool {
        let (Some(h), Some(w)) = (have, want) else { return true };
        h.0 == w.0 && h.1 == w.1 && h.2.iter().zip(&w.2).all(|(a, b)| a == b || open(a) || open(b))
    };
    let sets: Vec<(NodeIdx, MethodSet, FxHashSet<&str>)> =
        implementers.iter().map(|&t| (t, concrete_methods(t), opaque_of(t))).filter(|(_, s, o)| !s.is_empty() || !o.is_empty()).collect();
    for &iface in &interfaces {
        let want = interface_methods(iface);
        let want_opaque = opaque_of(iface);
        if want.is_empty() && want_opaque.is_empty() {
            continue; // `any` would match everything
        }
        let mut direct = Vec::new();
        let mut promoted = Vec::new();
        for (t, have, have_opaque) in &sets {
            let covers = want.iter().all(|(key, wanted)| have.get(key).is_some_and(|got| wanted.iter().all(|(_, w)| got.iter().any(|(_, g)| fits(g, w)))));
            if !covers || !want_opaque.is_subset(have_opaque) || embeds(*t).contains(&iface) {
                continue;
            }
            let own_covers = want.keys().all(|key| methods_of.get(t).is_some_and(|ms| ms.iter().any(|m| &method_key(view, *m) == key)));
            if own_covers {
                direct.push(*t);
            } else {
                promoted.push(*t);
            }
        }
        for t in direct.into_iter().chain(promoted).take(MAX_GO_IMPLEMENTERS) {
            let node = view.node(t);
            let at = format!("{}:{}", view.file(node.file()).path(), node.start_line());
            out.push(
                EdgeKind::Implements,
                t,
                iface,
                node.start_line(),
                "go-implements",
                Some(at),
                Some(("via", view.node(iface).name())),
                super::order_of("go-implements"),
            );
            // Link the interface method to the actual declaration selected by Go's embedded
            // method set, including methods declared in another file or embedded type.
            if let Some((_, have, _)) = sets.iter().find(|(ty, ..)| *ty == t) {
                let mut added = 0;
                let mut wanted_methods: Vec<_> = want.iter().collect();
                wanted_methods.sort_by_key(|(key, _)| *key);
                for (key, wanted) in wanted_methods {
                    for (bm, _) in wanted {
                        let Some(bm) = bm else { continue };
                        for (m, _) in have.get(key).into_iter().flatten() {
                            let Some(m) = m else { continue };
                            if added >= super::MAX_IMPLS_PER_PAIR || bm == m {
                                continue;
                            }
                            let method = view.node(*m);
                            let at = format!("{}:{}", view.file(method.file()).path(), method.start_line());
                            out.push(
                                EdgeKind::Calls,
                                *bm,
                                *m,
                                view.node(*bm).start_line(),
                                "interface-impl",
                                Some(at),
                                Some(("via", method.name())),
                                super::order_of("interface-impl"),
                            );
                            added += 1;
                        }
                    }
                }
            }
        }
    }
}

/// The receiver methods of each Go struct, by the struct's directory and name.
fn struct_methods(e: &Engine<'_>) -> (Vec<NodeIdx>, FxHashMap<NodeIdx, Vec<NodeIdx>>) {
    let view = e.view;
    let mut structs = Vec::new();
    let mut by_name: FxHashMap<(&str, &str), NodeIdx> = FxHashMap::default();
    let mut receiver_methods = Vec::new();
    for f in view.files() {
        if view.file(f).lang() != Lang::Go {
            continue;
        }
        let dir = parent_dir(view.file(f).path());
        for n in view.nodes_in_file(f).map(NodeIdx) {
            let node = view.node(n);
            match node.kind() {
                NodeKind::Struct => {
                    structs.push(n);
                    by_name.entry((dir, node.name())).or_insert(n);
                }
                NodeKind::Method => receiver_methods.push(n),
                _ => {}
            }
        }
    }
    let mut methods: FxHashMap<NodeIdx, Vec<NodeIdx>> = FxHashMap::default();
    for m in receiver_methods {
        let node = view.node(m);
        let Some((owner, name)) = node.qname().rsplit_once("::") else { continue };
        if name != node.name() || owner.contains("::") {
            continue;
        }
        if let Some(&t) = by_name.get(&(parent_dir(view.file(node.file()).path()), owner)) {
            methods.entry(t).or_default().push(m);
        }
    }
    (structs, methods)
}

/// A protoc-generated `Unimplemented*Server` stub stands for the service the project implements
/// by hand (upstream `goGrpcStubImplEdges`): each stub method links to the same-named method of
/// every hand-written struct that has all of the stub's methods. The stub must sit in a
/// generated file, and generated siblings (`UnsafeFooServer`, clients) are never candidates.
pub(super) fn grpc_stubs(e: &Engine<'_>, out: &mut Out) {
    let view = e.view;
    let generated = |n: NodeIdx| view.file(view.node(n).file()).flags() & cg_extract::fileflags::GENERATED != 0;
    let (mut structs, methods) = struct_methods(e);
    let stubs: Vec<NodeIdx> = structs
        .iter()
        .copied()
        .filter(|s| {
            let name = view.node(*s).name();
            name.starts_with("Unimplemented") && name.ends_with("Server") && name.len() > "UnimplementedServer".len() && generated(*s)
        })
        .collect();
    if stubs.is_empty() {
        return;
    }
    let stable = |n: &NodeIdx| (view.file(view.node(*n).file()).path(), view.node(*n).start_line(), *n);
    structs.sort_by_key(stable);
    let names = |t: NodeIdx| -> FxHashSet<&str> { methods.get(&t).into_iter().flatten().map(|m| view.node(*m).name()).collect() };
    for stub in stubs {
        let mut rpcs: Vec<NodeIdx> = methods
            .get(&stub)
            .into_iter()
            .flatten()
            .copied()
            .filter(|m| {
                let n = view.node(*m).name();
                !n.starts_with("mustEmbed") && n != "testEmbeddedByValue"
            })
            .collect();
        if rpcs.is_empty() {
            continue;
        }
        rpcs.sort_by_key(stable);
        for &cand in &structs {
            if cand == stub || generated(cand) {
                continue;
            }
            let have = names(cand);
            if !rpcs.iter().all(|m| have.contains(view.node(*m).name())) {
                continue;
            }
            let mut impls: Vec<NodeIdx> = methods.get(&cand).cloned().unwrap_or_default();
            impls.sort_by_key(stable);
            let mut added = 0;
            for &sm in &rpcs {
                for &cm in impls.iter().filter(|cm| view.node(**cm).name() == view.node(sm).name()) {
                    if added >= MAX_GO_IMPLEMENTERS {
                        break;
                    }
                    let method = view.node(cm);
                    let at = format!("{}:{}", view.file(method.file()).path(), method.start_line());
                    out.push(
                        EdgeKind::Calls,
                        sm,
                        cm,
                        view.node(sm).start_line(),
                        "go-grpc-stub-impl",
                        Some(at),
                        Some(("via", method.name())),
                        super::order_of("go-grpc-stub-impl"),
                    );
                    added += 1;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_compare_by_count_and_type() {
        let (p, r, t) = method_types("(ctx context.Context, a, b int, rest ...string) (int, error)").unwrap();
        assert_eq!((p, r), (4, 2));
        assert_eq!(t, vec![Some("Context".into()), Some("int".into()), Some("int".into()), Some("...string".into()), Some("int".into()), Some("error".into())]);
        assert_eq!(method_types("() error").unwrap().2, vec![Some("error".into())]);
        assert_eq!(method_types("(*http.Request) bool").unwrap().2, vec![Some("*Request".into()), Some("bool".into())]);
        assert_eq!(method_types("(f func(int) error)").unwrap().2, vec![None]);
        assert_eq!(comparable("map[string]uint8").as_deref(), Some("map[string]byte"));
        assert_eq!(comparable("interface{}").as_deref(), Some("any"));
    }
}
