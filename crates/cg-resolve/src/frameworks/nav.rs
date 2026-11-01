//! Navigation between pages (upstream `nextjs.ts`, `react-router.ts`, `tanstack-router.ts`
//! resolvers and their link synthesizers) and the Next.js proxy (`next-proxy.ts`).
//!
//! A navigation is a string: `router.push('/users')`, `navigate({ to: '/posts/$id' })`,
//! `redirect('/login')`, `<Link href="/x">`, `<NavLink to="/x">`. Extraction records each call
//! and link with its destination expression (a bare identifier is read through its local
//! `const`); the link step matches the destination against the router's own route table and
//! draws a `navigates` edge from the function it is written in to the page. A computed
//! destination, a path no page serves, or a tie between routes draws nothing.
//!
//! A Next proxy (`middleware.ts`, `proxy.ts`) is the one file nothing imports: each route its
//! `config.matcher` accepts gets a `calls` edge to what runs in it.

use super::href::{self, app_root_for, match_bracket, object_field, parse_href_expression, read_string_at, tanstack_path, Href, RootedTables, RouteTable};
use super::jsroutes::next_route_of;
use crate::engine::Engine;
use crate::synth::scan::{call_args, enclosing_row, is_minified, local_initializer, mid_chain, LineIndex, FN_KINDS};
use crate::synth::sites::{PASS_NAV, PASS_PROXY, PASS_REACT_TABLE};
use crate::synth::Out;
use cg_core::{EdgeKind, FileId, Lang, LangFamily, NodeIdx, NodeKind, NONE};
use cg_extract::{FileHook, HookCtx, HookOut, SiteFact};
use regex::Regex;
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::OnceLock;

pub const ROLE_NAV_CALL: u8 = 0;
pub const ROLE_NAV_LINK: u8 = 1;

/// Links a single component may carry before it is a navigation menu, not a decision.
const MAX_LINKS_PER_COMPONENT: usize = 24;

macro_rules! re {
    ($name:ident, $pat:expr) => {
        fn $name() -> &'static Regex {
            static R: OnceLock<Regex> = OnceLock::new();
            R.get_or_init(|| Regex::new($pat).expect("static regex"))
        }
    };
}

re!(nav_call_re, r"((?:[A-Za-z_$][\w$]*\s*\??\.\s*)*)(push|replace|prefetch|navigate|redirect|permanentRedirect)\s*\(");
re!(link_tag_re, r#"<(Link|NextLink|a|NavLink|Navigate|Redirect|LinkContainer|IndexLinkContainer)\b((?:[^>]|=>)*?)\b(href|to)\s*=\s*"#);
re!(next_nav_re, r"(?:^|\.)(push|replace|prefetch)$|^(redirect|permanentRedirect)$|^(?:NextResponse|Response)\.(redirect)$");
re!(router_word_re, r"(?i)(?:router|navigation|history)$");
re!(rr_nav_re, r"^(?:history|navigate|router)\.(?:push|replace|navigate)$|^(?:navigate|redirect)$");
re!(tanstack_nav_re, r"^(?:navigate|redirect)$|^(?:router|Route)\.navigate$");

/// Destination text of a call's first argument, a bare identifier read through its local
/// declaration in the enclosing function.
fn destination_text<'a>(lines: &LineIndex<'a>, arg: &'a str, line: u32, floor: u32) -> &'a str {
    let ident = arg.trim();
    if ident.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$') && !ident.is_empty() {
        if let Some(init) = local_initializer(lines, ident, line, floor) {
            return init;
        }
    }
    arg
}

/// Records navigation calls and link markup of JS / TS files.
pub struct NavScanner;

impl FileHook for NavScanner {
    fn name(&self) -> &'static str {
        "navigation-sites"
    }
    fn applies(&self, lang: Lang, rel: &str) -> bool {
        lang.family() == LangFamily::Js && !cg_scan::testpath::is_test_path(rel)
    }
    fn triggers(&self) -> &'static [&'static str] {
        &["push(", "replace(", "prefetch(", "navigate(", "redirect(", "Redirect(", "href", "to="]
    }
    fn run(&self, ctx: &HookCtx<'_>, out: &mut HookOut) {
        if is_minified(ctx.src) {
            return;
        }
        let safe = cg_extract::post::strip_js_comments(ctx.src);
        let src = safe.as_str();
        let lines = LineIndex::new(src);
        for c in nav_call_re().captures_iter(src) {
            let whole = c.get(0).unwrap();
            if mid_chain(src, whole.start()) {
                continue;
            }
            let callee: String = whole.as_str()[..whole.as_str().len() - 1].chars().filter(|ch| !ch.is_whitespace() && *ch != '?').collect();
            let open = whole.end() - 1;
            let Some(args) = call_args(src, open) else { continue };
            let Some(first) = args.first().copied().filter(|a| !a.is_empty()) else { continue };
            let line = lines.line(whole.start());
            let row = enclosing_row(ctx, line, FN_KINDS);
            let floor = row.and_then(|r| ctx.rows.nodes().nth(r as usize)).map_or(line.saturating_sub(40), |n| n.start_line());
            let dest = destination_text(&lines, first, line, floor);
            if dest.len() > 600 {
                continue;
            }
            out.sites.push(SiteFact {
                pass: PASS_NAV,
                role: ROLE_NAV_CALL,
                key_kind: 0,
                key_a: c[2].to_owned(),
                key_b: format!("{}|{callee}", lines.col(whole.start())),
                node_row: row.unwrap_or(NONE),
                line,
                payload: dest.to_owned(),
            });
        }
        for c in link_tag_re().captures_iter(src) {
            let whole = c.get(0).unwrap();
            let at = whole.end();
            let value = match src.as_bytes().get(at) {
                Some(b'"' | b'\'') => href::string_end(src, at).map(|e| &src[at..=e]),
                Some(b'{') => match_bracket(src, at).map(|e| src[at + 1..e].trim()),
                _ => None,
            };
            let Some(value) = value.filter(|v| v.len() <= 600) else { continue };
            let line = lines.line(whole.start());
            let Some(row) = enclosing_row(ctx, line, FN_KINDS) else { continue };
            out.sites.push(SiteFact {
                pass: PASS_NAV,
                role: ROLE_NAV_LINK,
                key_kind: 0,
                key_a: c[1].to_owned(),
                key_b: c[3].to_owned(),
                node_row: row,
                line,
                payload: value.to_owned(),
            });
        }
    }
}

/// A TanStack destination: `{ to: '/posts/$id' }` or a string, normalised as a route name.
fn tanstack_destination(expr: &str) -> Vec<Href> {
    let t = expr.trim();
    let literal = if t.starts_with('{') { object_field(t, "to").and_then(|v| read_string_at(v, 0)) } else { read_string_at(t, 0) };
    match literal.and_then(|l| tanstack_path(&l)) {
        Some(p) => href::to_href(&p).into_iter().collect(),
        None => parse_href_expression(t, "to"),
    }
}

/// `new URL('/login', req.url)` -> `'/login'`.
fn unwrap_url(expr: &str) -> &str {
    let t = expr.trim();
    if let Some(rest) = t.strip_prefix("new URL") {
        let open = t.len() - rest.trim_start().len();
        if t.as_bytes().get(open) == Some(&b'(') {
            if let Some(args) = call_args(t, open) {
                if let Some(first) = args.first() {
                    return first;
                }
            }
        }
    }
    t
}

struct Tables {
    next: RouteTable,
    next_roots: Vec<String>,
    react: RootedTables,
    tanstack: RootedTables,
}

/// An anonymous `export default [...]` has no kernel symbol. Follow export aliases to its
/// persisted table sites instead of guessing a same-named declaration in another file.
fn exported_react_table(e: &Engine<'_>, f: FileId, name: &str, depth: u8) -> Option<(FileId, String)> {
    use crate::engine::{Bound, ExportEntry};
    if depth > 8 {
        return None;
    }
    let view = e.view;
    if let Some(Bound::Node(n)) = e.lookup_export(f, name, 0) {
        let node = view.node(n);
        return Some((node.file(), node.name().to_owned()));
    }
    if let Some(ExportEntry::Re(target, from)) = e.exports.get(f.idx())?.get(name) {
        return exported_react_table(e, *target, view.text_str(*from), depth + 1);
    }
    if name == "default"
        && view.sites_of(f).iter().any(|s| {
            s.pass == PASS_REACT_TABLE
                && ((s.role == 0 && view.text_str(cg_core::StrId(s.key_a)) == "#default")
                    || (s.role == 1 && view.text_str(cg_core::StrId(s.key_b)) == "#default"))
        })
    {
        return Some((f, "#default".to_owned()));
    }
    None
}

fn react_table_binding(e: &Engine<'_>, f: FileId, name: &str) -> Option<(FileId, String)> {
    if let Some(n) = super::js_symbol(e, f, name) {
        let node = e.view.node(n);
        return Some((node.file(), node.name().to_owned()));
    }
    let (head, member) = name.split_once('.').map_or((name, None), |(a, b)| (a, Some(b)));
    let (index, import) = e.view.imports_of(f).iter().enumerate().find(|(_, imp)| e.view.text_str(imp.local) == head)?;
    let target = e.modules.target_file(*e.imp_targets.get(f.idx())?.get(index)?)?;
    let imported = member.unwrap_or_else(|| e.view.text_str(import.imported));
    exported_react_table(e, target, imported, 0)
}

fn route_tables(e: &Engine<'_>) -> Tables {
    let view = e.view;
    let mut t = Tables { next: RouteTable::default(), next_roots: Vec::new(), react: RootedTables::default(), tanstack: RootedTables::default() };
    let mut optional: Vec<(String, String, NodeIdx)> = Vec::new();
    for f in view.files() {
        if view.file(f).lang().family() != LangFamily::Js {
            continue;
        }
        let path = view.file(f).path();
        for n in view.nodes_in_file(f) {
            let n = NodeIdx(n);
            let node = view.node(n);
            if node.kind() != NodeKind::Route || !node.name().starts_with('/') {
                continue;
            }
            match node.signature() {
                Some("next") => {
                    if let Some(r) = next_route_of(path) {
                        t.next.add(node.name(), n);
                        if !t.next_roots.contains(&r.root) {
                            t.next_roots.push(r.root);
                        }
                    }
                }
                Some("react-router") if !node.name().ends_with('*') => {
                    let root = app_root_for(path);
                    let name = node.name();
                    let p = if name.len() > 1 { name.trim_end_matches('/') } else { name };
                    t.react.add(root, p, n);
                    // `/cart/:id?` serves a bare `/cart` too, unless a route claims it
                    let mut segs: Vec<&str> = p.split('/').skip(1).collect();
                    while segs.len() > 1 && segs.last().is_some_and(|s| s.starts_with(':') && s.ends_with('?')) {
                        segs.pop();
                        optional.push((root.to_owned(), format!("/{}", segs.join("/")), n));
                    }
                }
                Some("tanstack-router") => t.tanstack.add(app_root_for(path), node.name(), n),
                _ => {}
            }
        }
    }
    // Resolve the table argument through imports and re-exports. Candidate arrays that are
    // never passed to a router do not become navigation targets.
    let mut queue = Vec::new();
    for f in view.files() {
        for site in view.sites_of(f).iter().filter(|s| s.pass == PASS_REACT_TABLE && s.role == 1 && view.text_str(cg_core::StrId(s.key_b)).is_empty()) {
            let name = view.text_str(cg_core::StrId(site.key_a));
            let payload = view.text_str(site.payload);
            queue.push((f, name.to_owned(), app_root_for(view.file(f).path()).to_owned(), payload.get(1..).unwrap_or("").to_owned(), 0u8));
        }
    }
    let mut activated = FxHashSet::default();
    while let Some((f, name, root, prefix, depth)) = queue.pop() {
        if depth > 16 {
            continue;
        }
        let Some((decl_file, decl_name)) = react_table_binding(e, f, &name) else { continue };
        if !activated.insert((decl_file, decl_name.clone(), root.clone(), prefix.clone())) {
            continue;
        }
        for site in view.sites_of(decl_file).iter().filter(|s| s.pass == PASS_REACT_TABLE) {
            let name = view.text_str(cg_core::StrId(site.key_a));
            let payload = view.text_str(site.payload);
            if site.role == 1 && view.text_str(cg_core::StrId(site.key_b)) == decl_name {
                let sub = payload.get(1..).unwrap_or("");
                let path = if payload.starts_with('a') { sub.to_owned() } else { href::compose_route_path(&[&prefix, sub.trim_start_matches('/')]) };
                queue.push((decl_file, name.to_owned(), root.clone(), path, depth + 1));
            } else if site.role == 0 && name == decl_name {
                let node = view.node(site.node);
                if node.kind() != NodeKind::Route || node.name().ends_with('*') {
                    continue;
                }
                let path = if payload == "a" { node.name().to_owned() } else { href::compose_route_path(&[&prefix, node.name().trim_start_matches('/')]) };
                let p = if path.len() > 1 { path.trim_end_matches('/') } else { &path };
                t.react.add(&root, p, site.node);
                let mut segs: Vec<&str> = p.split('/').skip(1).collect();
                while segs.len() > 1 && segs.last().is_some_and(|s| s.starts_with(':') && s.ends_with('?')) {
                    segs.pop();
                    optional.push((root.clone(), format!("/{}", segs.join("/")), site.node));
                }
            }
        }
    }
    for (root, path, n) in optional {
        if t.react.for_file(&root).is_some_and(|tb| !tb.exact.contains_key(&path)) {
            t.react.add(&root, &path, n);
        }
    }
    t
}

fn registered(e: &Engine<'_>, f: FileId, line: u32) -> String {
    format!("{}:{}", e.view.file(f).path(), line)
}

/// `navigates` edges for every recorded navigation call and link.
pub(crate) fn navigation(e: &Engine<'_>, out: &mut Out) {
    let view = e.view;
    let tables = route_tables(e);
    if tables.next.is_empty() && tables.react.is_empty() && tables.tanstack.is_empty() {
        return;
    }
    let mut per_component: FxHashMap<(NodeIdx, &'static str), usize> = FxHashMap::default();
    let mut seen: FxHashSet<(NodeIdx, NodeIdx, u32, &'static str)> = FxHashSet::default();
    for f in view.files() {
        let sites: Vec<_> = view.sites_of(f).iter().filter(|s| s.pass == PASS_NAV).collect();
        if sites.is_empty() {
            continue;
        }
        let path = view.file(f).path();
        let in_next = !tables.next.is_empty() && tables.next_roots.iter().any(|r| path.starts_with(r.as_str()));
        for s in sites {
            let payload = view.text_str(s.payload);
            let verb = view.text_str(cg_core::StrId(s.key_a));
            let key_b = view.text_str(cg_core::StrId(s.key_b));
            // (router, its routes, the arms, how it navigates)
            let mut tries: Vec<(&'static str, &RouteTable, Vec<Href>, String)> = Vec::new();
            if s.role == ROLE_NAV_CALL {
                let (col, callee) = key_b.split_once('|').unwrap_or(("", key_b));
                let _ = col;
                // upstream takes any `x.push('/page')`; a receiver that is no router (an
                // array, a queue) is not navigating, so it has to say so
                let router_receiver = callee.rsplit_once('.').is_none_or(|(recv, _)| router_word_re().is_match(recv));
                if in_next && router_receiver {
                    if let Some(c) = next_nav_re().captures(callee) {
                        let how = if c.get(3).is_some() { "response.redirect".to_owned() } else { verb.to_owned() };
                        tries.push(("next-navigation", &tables.next, parse_href_expression(unwrap_url(payload), "pathname"), how));
                    }
                }
                if rr_nav_re().is_match(callee) {
                    if let Some(t) = tables.react.for_file(path) {
                        tries.push(("react-router-navigation", t, parse_href_expression(payload, "pathname"), verb.to_owned()));
                    }
                }
                if tanstack_nav_re().is_match(callee) {
                    if let Some(t) = tables.tanstack.for_file(path) {
                        tries.push(("tanstack-navigation", t, tanstack_destination(payload), verb.to_owned()));
                    }
                }
            } else {
                let how = match verb {
                    "a" => "a",
                    "Navigate" => "navigate",
                    "Redirect" => "redirect",
                    _ => "link",
                };
                match key_b {
                    "href" if in_next && matches!(verb, "Link" | "NextLink" | "a") => {
                        let arms: Vec<Href> =
                            parse_href_expression(payload, "pathname").into_iter().filter(|a| verb != "a" || a.path.starts_with('/')).collect();
                        tries.push(("next-link", &tables.next, arms, how.to_owned()));
                    }
                    "to" => {
                        if let Some(t) = tables.react.for_file(path) {
                            let arms = parse_href_expression(payload, "pathname").into_iter().filter(|a| a.path.starts_with('/')).collect();
                            tries.push(("react-router-link", t, arms, how.to_owned()));
                        }
                        if matches!(verb, "Link" | "Navigate") {
                            if let Some(t) = tables.tanstack.for_file(path) {
                                tries.push(("tanstack-link", t, tanstack_destination(payload), how.to_owned()));
                            }
                        }
                    }
                    _ => {}
                }
            }
            let src = s.node;
            for (by, table, arms, how) in tries {
                let dests = table.destinations(&arms);
                if dests.is_empty() {
                    continue;
                }
                let col = if s.role == ROLE_NAV_CALL { key_b.split_once('|').and_then(|(c, _)| c.parse::<u32>().ok()) } else { None };
                for (route, arm) in dests {
                    if route == src || !seen.insert((src, route, s.line, by)) {
                        continue;
                    }
                    if s.role == ROLE_NAV_LINK {
                        let count = per_component.entry((src, by)).or_default();
                        *count += 1;
                        if *count > MAX_LINKS_PER_COMPONENT {
                            continue;
                        }
                    }
                    let meta = serde_json::json!({ "synthesizedBy": by, "href": arm.display(), "navMethod": how, "registeredAt": registered(e, f, s.line) });
                    out.push_meta(EdgeKind::Navigates, src, route, s.line, col.unwrap_or(NONE), meta, crate::synth::order_of(by));
                }
                // the first router that names a page answers the navigation
                break;
            }
        }
    }
}

// ------------------------------------------------------------------- Next proxy

const PROXY_EXTS: &[&str] = &["ts", "tsx", "js", "jsx", "mjs", "cjs"];

/// `middleware.ts` / `proxy.ts`: what runs in it and what its `config.matcher` accepts.
pub struct ProxyScanner;

re!(export_config_re, r"\bexport\s+const\s+config\b[^=;]*=\s*");
re!(default_fn_re, r"\bexport\s+default\s+(?:async\s+)?function\s*\*?\s*([A-Za-z_$][\w$]*)");
re!(default_ident_re, r"(?m)\bexport\s+default\s+([A-Za-z_$][\w$]*)\s*;?\s*$");
re!(export_as_re, r"\bexport\s*\{[^}]*?\b([A-Za-z_$][\w$]*)\s+as\s+(?:default|middleware|proxy)\b[^}]*\}");

fn proxy_file_kind(rel: &str) -> Option<&'static str> {
    let base = rel.rsplit('/').next()?;
    let (stem, ext) = base.rsplit_once('.')?;
    if !PROXY_EXTS.contains(&ext) {
        return None;
    }
    match stem {
        "middleware" => Some("middleware"),
        "proxy" => Some("proxy"),
        _ => None,
    }
}

impl FileHook for ProxyScanner {
    fn name(&self) -> &'static str {
        "next-proxy-sites"
    }
    fn applies(&self, lang: Lang, rel: &str) -> bool {
        lang.family() == LangFamily::Js && proxy_file_kind(rel).is_some()
    }
    fn run(&self, ctx: &HookCtx<'_>, out: &mut HookOut) {
        let Some(kind) = proxy_file_kind(ctx.rel) else { return };
        let safe = cg_extract::post::strip_js_comments(ctx.src);
        let src = safe.as_str();
        let lines = LineIndex::new(src);
        // what runs: the export named after the file, else the default export's function
        let named = ctx
            .rows
            .nodes()
            .any(|n| n.name() == kind && n.is_exported() == Some(true) && matches!(n.kind(), NodeKind::Function | NodeKind::Constant | NodeKind::Variable));
        let entry = if named {
            kind.to_owned()
        } else {
            default_fn_re()
                .captures(src)
                .or_else(|| default_ident_re().captures(src))
                .or_else(|| export_as_re().captures(src).filter(|c| !src[c.get(0).unwrap().end()..].trim_start().starts_with("from")))
                .map(|c| c[1].to_owned())
                .unwrap_or_default()
        };
        // matcher entries: `line\tsource` per entry; `*` for no matcher (every request)
        let payload = match export_config_re().find(src) {
            None => "*".to_owned(),
            Some(m) if src.as_bytes().get(m.end()) == Some(&b'{') => match match_bracket(src, m.end()) {
                Some(close) => {
                    let obj = &src[m.end()..=close];
                    match object_field(obj, "matcher") {
                        None if !obj.contains("matcher") => "*".to_owned(),
                        None => String::new(),
                        Some(value) => {
                            let base = m.end() + (value.as_ptr() as usize - obj.as_ptr() as usize);
                            matcher_entries(value).into_iter().map(|(off, s)| format!("{}\t{s}", lines.line(base + off))).collect::<Vec<_>>().join("\n")
                        }
                    }
                }
                None => String::new(),
            },
            Some(_) => String::new(),
        };
        out.sites.push(SiteFact { pass: PASS_PROXY, role: 0, key_kind: 0, key_a: entry, key_b: kind.to_owned(), node_row: NONE, line: 1, payload });
    }
}

/// The strings a matcher value names, with their offsets: a string, or an array of strings
/// and `{ source }` objects. A value it cannot read names nothing.
fn matcher_entries(value: &str) -> Vec<(usize, String)> {
    let v = value.trim();
    let lead = value.len() - value.trim_start().len();
    if !v.starts_with('[') {
        return read_string_at(v, 0).filter(|s| !s.contains(href::HOLE)).map(|s| vec![(lead, s)]).unwrap_or_default();
    }
    let Some(end) = match_bracket(v, 0) else { return Vec::new() };
    let mut out = Vec::new();
    let body = &v[1..end];
    let mut start = 0;
    loop {
        let stop = href::index_at_depth0(body, b',', start).unwrap_or(body.len());
        let item = body[start..stop].trim();
        let item_at = lead + 1 + start + (body[start..stop].len() - body[start..stop].trim_start().len());
        if !item.is_empty() {
            let s = if item.starts_with('{') { object_field(item, "source").and_then(|x| read_string_at(x, 0)) } else { read_string_at(item, 0) };
            match s.filter(|s| !s.contains(href::HOLE)) {
                Some(s) => out.push((item_at, s)),
                None => return Vec::new(),
            }
        }
        if stop >= body.len() {
            break;
        }
        start = stop + 1;
    }
    out
}

/// A matcher entry compiled the way path-to-regexp reads it: anchored at both ends, `/:p*`
/// any number of segments, `(?!api|_next).*` everything but the excluded prefixes. An entry
/// this does not read matches nothing.
#[derive(Debug, PartialEq)]
enum Pat {
    Lit(String),
    /// `/:name` with an optional `(…)` body and a `?` / `*` / `+` modifier.
    Param(Body, Option<char>),
    Group(Body),
}

#[derive(Debug, PartialEq)]
enum Body {
    Segment,
    Any {
        plus: bool,
    },
    Options(Vec<String>),
    /// `(?!a|b).*`: the rest must not start with one of the exclusions.
    Except(Vec<Excl>, bool),
}

#[derive(Debug, PartialEq)]
enum Excl {
    Prefix(String, Anchor),
    /// A path whose last segment has a dot (a file with an extension).
    FileExtension,
}

#[derive(Debug, PartialEq)]
enum Anchor {
    None,
    End,
    SegmentOrEnd,
}

fn closing_paren(s: &str, open: usize) -> Option<usize> {
    let b = s.as_bytes();
    let mut depth = 0;
    let mut i = open;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1,
            b'[' => {
                while i < b.len() && b[i] != b']' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn top_level_bars(s: &str) -> Vec<&str> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let (mut from, mut i) = (0, 0);
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1,
            b'(' => match closing_paren(s, i) {
                Some(c) => i = c,
                None => return vec![s],
            },
            b'|' => {
                out.push(&s[from..i]);
                from = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    out.push(&s[from..]);
    out
}

fn read_exclusion(branch: &str) -> Option<Excl> {
    for lead in ["[^?]*\\.", ".*\\.", ".+\\."] {
        if branch.starts_with(lead) {
            return Some(Excl::FileExtension);
        }
    }
    if branch.starts_with('[') && branch.contains("]+\\.") {
        return Some(Excl::FileExtension);
    }
    let (body, anchor) = if let Some(b) = branch.strip_suffix("(?:/|$)") {
        (b, Anchor::SegmentOrEnd)
    } else if let Some(b) = branch.strip_suffix('$') {
        (b, Anchor::End)
    } else {
        (branch, Anchor::None)
    };
    let lit = body.replace("\\.", ".");
    let ok = lit.chars().all(|c| c.is_alphanumeric() || "_/~.-".contains(c));
    ok.then_some(Excl::Prefix(lit, anchor))
}

fn read_group(body: &str) -> Option<Body> {
    match body {
        ".*" => return Some(Body::Any { plus: false }),
        ".+" => return Some(Body::Any { plus: true }),
        _ => {}
    }
    if body.starts_with("(?!") {
        let close = closing_paren(body, 0)?;
        let tail = &body[close + 1..];
        let plus = match tail {
            ".*" => false,
            ".+" => true,
            _ => return None,
        };
        let excls = top_level_bars(&body[3..close]).into_iter().map(read_exclusion).collect::<Option<Vec<_>>>()?;
        return Some(Body::Except(excls, plus));
    }
    let opts = top_level_bars(body.strip_prefix("?:").unwrap_or(body));
    opts.iter()
        .all(|o| !o.is_empty() && o.chars().all(|c| c.is_alphanumeric() || "_./~-".contains(c)))
        .then(|| Body::Options(opts.iter().map(|o| o.to_string()).collect()))
}

fn compile_matcher(source: &str) -> Option<Vec<Pat>> {
    if !source.starts_with('/') || source.len() > 2048 {
        return None;
    }
    let mut out = Vec::new();
    let b = source.as_bytes();
    let mut i = 0;
    let mut lit = String::new();
    while i < b.len() {
        if b[i] == b'/' && b.get(i + 1) == Some(&b':') {
            let name_end = source[i + 2..].find(|c: char| !(c.is_alphanumeric() || c == '_')).map_or(source.len(), |e| i + 2 + e);
            if name_end == i + 2 {
                return None;
            }
            if !lit.is_empty() {
                out.push(Pat::Lit(std::mem::take(&mut lit)));
            }
            i = name_end;
            let mut body = Body::Segment;
            if b.get(i) == Some(&b'(') {
                let close = closing_paren(source, i)?;
                body = read_group(&source[i + 1..close])?;
                i = close + 1;
            }
            let modifier = b.get(i).filter(|c| matches!(c, b'?' | b'*' | b'+')).map(|c| *c as char);
            if modifier.is_some() {
                i += 1;
            }
            out.push(Pat::Param(body, modifier));
            continue;
        }
        if b[i] == b'(' {
            let close = closing_paren(source, i)?;
            if b.get(close + 1).is_some_and(|c| matches!(c, b'?' | b'*' | b'+')) {
                return None;
            }
            if !lit.is_empty() {
                out.push(Pat::Lit(std::mem::take(&mut lit)));
            }
            out.push(Pat::Group(read_group(&source[i + 1..close])?));
            i = close + 1;
            continue;
        }
        let c = source[i..].chars().next()?;
        if !(c.is_alphanumeric() || "_./~-".contains(c)) {
            return None;
        }
        lit.push(c);
        i += c.len_utf8();
    }
    if !lit.is_empty() {
        out.push(Pat::Lit(lit));
    }
    Some(out)
}

fn excluded(excls: &[Excl], rest: &str) -> bool {
    excls.iter().any(|e| match e {
        Excl::FileExtension => rest.rsplit('/').next().is_some_and(|last| last.contains('.')),
        Excl::Prefix(p, anchor) => {
            rest.starts_with(p.as_str())
                && match anchor {
                    Anchor::None => true,
                    Anchor::End => rest.len() == p.len(),
                    Anchor::SegmentOrEnd => rest.len() == p.len() || rest[p.len()..].starts_with('/'),
                }
        }
    })
}

/// Lengths `body` can consume of `s` (shortest first).
fn body_spans(body: &Body, s: &str) -> Vec<usize> {
    match body {
        Body::Segment => {
            let n = s.find('/').unwrap_or(s.len());
            if n > 0 {
                vec![n]
            } else {
                vec![]
            }
        }
        Body::Any { plus } => (if *plus { 1 } else { 0 }..=s.len()).filter(|n| s.is_char_boundary(*n)).collect(),
        Body::Options(opts) => opts.iter().filter(|o| s.starts_with(o.as_str())).map(|o| o.len()).collect(),
        Body::Except(excls, plus) => {
            if excluded(excls, s) {
                vec![]
            } else {
                (if *plus { 1 } else { 0 }..=s.len()).filter(|n| s.is_char_boundary(*n)).collect()
            }
        }
    }
}

fn matches_at(pats: &[Pat], s: &str, budget: &mut u32) -> bool {
    if *budget == 0 {
        return false;
    }
    *budget -= 1;
    let Some((first, rest)) = pats.split_first() else {
        return s.is_empty() || s == "/";
    };
    match first {
        Pat::Lit(l) => s.strip_prefix(l.as_str()).is_some_and(|r| matches_at(rest, r, budget)),
        Pat::Group(body) => body_spans(body, s).into_iter().any(|n| matches_at(rest, &s[n..], budget)),
        Pat::Param(body, modifier) => {
            // one `/<body>` occurrence, repeated as the modifier allows
            let (min, max) = match modifier {
                None => (1, 1),
                Some('?') => (0, 1),
                Some('*') => (0, 64),
                _ => (1, 64),
            };
            fn rep(body: &Body, rest: &[Pat], s: &str, left: (usize, usize), budget: &mut u32) -> bool {
                if left.0 == 0 && matches_at(rest, s, budget) {
                    return true;
                }
                if left.1 == 0 {
                    return false;
                }
                let Some(after) = s.strip_prefix('/') else { return false };
                body_spans(body, after).into_iter().any(|n| n > 0 && rep(body, rest, &after[n..], (left.0.saturating_sub(1), left.1 - 1), budget))
            }
            rep(body, rest, s, (min, max), budget)
        }
    }
}

/// Addresses a route serves, one per shape: a parameter is a segment no literal stands for.
fn route_addresses(path: &str) -> Vec<String> {
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let filled: Vec<&str> = segs.iter().map(|s| if s.starts_with(':') { "\u{1}" } else { s }).collect();
    let mut out = vec![format!("/{}", filled.join("/"))];
    if segs.last().is_some_and(|l| l.ends_with('*')) {
        out.push(format!("/{}", filled[..filled.len() - 1].join("/")));
    }
    out
}

fn covers(pats: &[Pat], route_path: &str) -> bool {
    route_addresses(route_path).iter().any(|a| {
        let mut budget = 20_000;
        matches_at(pats, a, &mut budget)
    })
}

/// The directory an app's proxy must sit in: beside `pages`, else beside `app`, at the app's
/// root before its `src/`.
fn proxy_dir(root: &str, route_files: &[&str]) -> String {
    for dir in ["pages/", "app/"] {
        if route_files.iter().any(|f| f.starts_with(&format!("{root}{dir}"))) {
            return root.to_owned();
        }
        if route_files.iter().any(|f| f.starts_with(&format!("{root}src/{dir}"))) {
            return format!("{root}src/");
        }
    }
    root.to_owned()
}

/// A Next route of an app: its node, its path and its file.
type AppRoute<'v> = (NodeIdx, String, &'v str);
/// A matcher entry: the line it is written on, its source and its compiled form.
type Matcher = (u32, String, Option<Vec<Pat>>);

/// `calls` edges from every Next route a proxy runs before to what runs in it.
pub(crate) fn next_proxy(e: &Engine<'_>, out: &mut Out) {
    let view = e.view;
    // app root -> (route node, its path, its file)
    let mut apps: Vec<(String, Vec<AppRoute>)> = Vec::new();
    for f in view.files() {
        let path = view.file(f).path();
        let Some(r) = next_route_of(path) else { continue };
        for n in view.nodes_in_file(f) {
            let n = NodeIdx(n);
            let node = view.node(n);
            if node.kind() != NodeKind::Route || node.signature() != Some("next") {
                continue;
            }
            match apps.iter_mut().find(|(root, _)| *root == r.root) {
                Some((_, v)) => v.push((n, r.path.clone(), path)),
                None => apps.push((r.root.clone(), vec![(n, r.path.clone(), path)])),
            }
        }
    }
    for (root, routes) in &apps {
        let files: Vec<&str> = routes.iter().map(|r| r.2).collect();
        let dir = proxy_dir(root, &files);
        let find = |stem: &str| PROXY_EXTS.iter().find_map(|ext| view.file_by_path(&format!("{dir}{stem}.{ext}")));
        let Some(file) = find("middleware").or_else(|| find("proxy")) else { continue };
        let Some(site) = view.sites_of(file).iter().find(|s| s.pass == PASS_PROXY) else { continue };
        let entry_name = view.text_str(cg_core::StrId(site.key_a));
        let entry = view
            .nodes_in_file(file)
            .map(NodeIdx)
            .find(|n| {
                !entry_name.is_empty()
                    && view.node(*n).name() == entry_name
                    && matches!(view.node(*n).kind(), NodeKind::Function | NodeKind::Constant | NodeKind::Variable)
            })
            .unwrap_or(view.file(file).rec.node_start);
        let payload = view.text_str(site.payload);
        let matchers: Option<Vec<Matcher>> = (payload != "*").then(|| {
            payload
                .lines()
                .filter_map(|l| l.split_once('\t'))
                .map(|(line, source)| (line.parse().unwrap_or(1), source.to_owned(), compile_matcher(source)))
                .collect()
        });
        let file_path = view.file(file).path();
        for (route, path, _) in routes {
            let hit = match &matchers {
                None => None,
                Some(list) => match list.iter().find(|(_, _, p)| p.as_ref().is_some_and(|p| covers(p, path))) {
                    Some(h) => Some(h),
                    None => continue,
                },
            };
            let mut meta = serde_json::Map::new();
            meta.insert("synthesizedBy".into(), "next-proxy".into());
            if let Some((_, source, _)) = hit {
                meta.insert("matcher".into(), source.clone().into());
            }
            let line = hit.map_or(view.node(entry).start_line().max(1), |h| h.0);
            meta.insert("registeredAt".into(), format!("{file_path}:{line}").into());
            out.push_meta(
                EdgeKind::Calls,
                *route,
                entry,
                view.node(*route).start_line(),
                NONE,
                serde_json::Value::Object(meta),
                crate::synth::order_of("next-proxy"),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cov(m: &str, path: &str) -> bool {
        covers(&compile_matcher(m).unwrap(), path)
    }

    #[test]
    fn proxy_matchers() {
        assert!(cov("/about", "/about"));
        assert!(!cov("/about", "/about/team"));
        assert!(cov("/about/:path*", "/about"));
        assert!(cov("/about/:path*", "/about/team/x"));
        assert!(cov("/dashboard/:path+", "/dashboard/a"));
        assert!(!cov("/dashboard/:path+", "/dashboard"));
        let m = "/((?!api|_next/static|_next/image|favicon.ico).*)";
        assert!(cov(m, "/"));
        assert!(cov(m, "/dashboard/:id"));
        assert!(!cov(m, "/api/users"));
        assert!(!cov("/((?!.*\\..*).*)", "/robots.txt"));
        assert!(cov("/((?!.*\\..*).*)", "/blog"));
        assert!(cov("/(en|fr)/:path*", "/fr/docs"));
        assert!(compile_matcher("/x/(.*)+").is_none());
        assert_eq!(matcher_entries(" ['/a', { source: '/b/:p*' }]").into_iter().map(|e| e.1).collect::<Vec<_>>(), vec!["/a", "/b/:p*"]);
        assert!(matcher_entries("[`/a/${x}`]").is_empty());
    }

    #[test]
    fn destinations() {
        assert_eq!(tanstack_destination("{ to: '/posts/$postId', params }").iter().map(Href::display).collect::<Vec<_>>(), vec!["/posts/:postId"]);
        assert_eq!(unwrap_url("new URL('/login', req.url)"), "'/login'");
    }
}
