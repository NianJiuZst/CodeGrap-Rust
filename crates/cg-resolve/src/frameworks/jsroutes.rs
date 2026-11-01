//! Route tables of the JavaScript frameworks (upstream `frameworks/{nextjs,nestjs,react,
//! tanstack-router}.ts`): Next.js file routes, NestJS decorators across HTTP, GraphQL,
//! microservice and WebSocket transports, React Router's `<Route>` markup and route objects,
//! and TanStack Router's file and code routes. A page is named by its path alone and reaches
//! the component it renders; a server route is `METHOD path` and references its handler.

use super::href::{compose_route_path, match_bracket, object_field, read_string_at, tanstack_path};
use super::routes::{Handler, Route};
use cg_core::{EdgeKind, RefKind};
use regex::Regex;
use std::sync::OnceLock;

macro_rules! re {
    ($name:ident, $pat:expr) => {
        fn $name() -> &'static Regex {
            static R: OnceLock<Regex> = OnceLock::new();
            R.get_or_init(|| Regex::new($pat).expect("static regex"))
        }
    };
}

/// 1-based line of a byte offset.
fn line_at(src: &str, off: usize) -> u32 {
    src.as_bytes()[..off.min(src.len())].iter().filter(|b| **b == b'\n').count() as u32 + 1
}

// ------------------------------------------------------------------- Next.js

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NextKind {
    /// `app/**/page.tsx`, `pages/**`: a page component.
    Page,
    /// `app/**/route.ts`: one route per exported HTTP method.
    Handler,
    /// `pages/api/**`: `ANY /api/...` bound to the default export.
    Api,
}

#[derive(Debug, PartialEq, Eq)]
pub struct NextRoute {
    pub kind: NextKind,
    /// `/blog/:slug`
    pub path: String,
    /// The directory the Next app lives in (`""`, `apps/web/`).
    pub root: String,
}

/// `[slug]` -> `:slug`, `[...all]` / `[[...all]]` -> `:all*`.
fn next_segment(seg: &str) -> String {
    if let Some(inner) = seg.strip_prefix("[[...").and_then(|s| s.strip_suffix("]]")) {
        return format!(":{inner}*");
    }
    if let Some(inner) = seg.strip_prefix("[...").and_then(|s| s.strip_suffix(']')) {
        return format!(":{inner}*");
    }
    if let Some(inner) = seg.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        return format!(":{inner}");
    }
    seg.to_owned()
}

/// The prefix before the first `src/<dir>/` or `<dir>/` directory, and the rest.
fn split_at_dir<'a>(rel: &'a str, dir: &str) -> Option<(&'a str, &'a str)> {
    let mut at = 0;
    let segs: Vec<&str> = rel.split('/').collect();
    for (i, seg) in segs.iter().enumerate() {
        if i + 1 < segs.len() {
            if *seg == dir {
                return Some((&rel[..at], &rel[at + seg.len() + 1..]));
            }
            if *seg == "src" && segs.get(i + 1) == Some(&dir) && i + 2 < segs.len() {
                return Some((&rel[..at], &rel[at + 4 + dir.len() + 1..]));
            }
        }
        at += seg.len() + 1;
    }
    None
}

const NEXT_EXTS: &[&str] = &["tsx", "ts", "jsx", "js", "mjs", "cjs", "md", "mdx"];

/// What a file is to the Next router, or `None` for a file that is no route.
pub fn next_route_of(rel: &str) -> Option<NextRoute> {
    if rel.split('/').any(|s| matches!(s, "__tests__" | "__mocks__" | "node_modules")) {
        return None;
    }
    if let Some((root, rest)) = split_at_dir(rel, "app") {
        let (dir, file) = rest.rsplit_once('/').unwrap_or(("", rest));
        let (stem, ext) = file.rsplit_once('.')?;
        if !NEXT_EXTS.contains(&ext) || !matches!(stem, "page" | "route") {
            return None;
        }
        let segs: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
        // parallel (`@slot`) and intercepting (`(.)photo`) routes are not modelled
        if segs.iter().any(|s| s.starts_with('@') || s.starts_with("(.)") || s.starts_with("(..)") || s.starts_with("(...)")) {
            return None;
        }
        let kept: Vec<String> = segs.iter().filter(|s| !(s.starts_with('(') && s.ends_with(')'))).map(|s| next_segment(s)).collect();
        let kind = if stem == "page" { NextKind::Page } else { NextKind::Handler };
        return Some(NextRoute { kind, path: format!("/{}", kept.join("/")), root: root.to_owned() });
    }
    let (root, rest) = split_at_dir(rel, "pages")?;
    let (bare, ext) = rest.rsplit_once('.')?;
    if !NEXT_EXTS.contains(&ext) {
        return None;
    }
    let mut segs: Vec<&str> = bare.split('/').collect();
    let base = *segs.last()?;
    if base.starts_with('_') || [".test", ".spec", ".stories", ".config", ".d"].iter().any(|s| bare.ends_with(s)) {
        return None;
    }
    if segs.last() == Some(&"index") {
        segs.pop();
    }
    let kind = if segs.first() == Some(&"api") { NextKind::Api } else { NextKind::Page };
    let path = format!("/{}", segs.iter().map(|s| next_segment(s)).collect::<Vec<_>>().join("/"));
    Some(NextRoute { kind, path, root: root.to_owned() })
}

re!(
    next_handler_re,
    r"\bexport\s+(?:async\s+)?function\s+(GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS)\b|\bexport\s+(?:const|let)\s+(GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS)\s*="
);
re!(default_fn_re, r"export\s+default\s+(?:async\s+)?function\s*\*?\s*([A-Za-z_$][\w$]*)");
re!(default_class_re, r"export\s+default\s+class\s+([A-Za-z_$][\w$]*)");
re!(default_wrapped_re, r"export\s+default\s+(?:[A-Za-z_$][\w$]*\.)?[A-Za-z_$][\w$]*\s*\(\s*([A-Za-z_$][\w$]*)\s*[,)]");
re!(default_ident_re, r"(?m)export\s+default\s+([A-Za-z_$][\w$]*)\s*;?\s*$");
re!(default_as_re, r"export\s*\{\s*([A-Za-z_$][\w$]*)\s+as\s+default\s*\}");

/// The name the module exports as its default (a wrapper such as `memo(Page)` is looked
/// through), with its offset.
pub fn default_export_name(src: &str) -> Option<(String, usize)> {
    for re in [default_fn_re(), default_class_re(), default_wrapped_re(), default_ident_re(), default_as_re()] {
        if let Some(c) = re.captures(src) {
            let g = c.get(1)?;
            if g.as_str() != "default" {
                return Some((g.as_str().to_owned(), c.get(0)?.start()));
            }
        }
    }
    None
}

pub fn scan_next(rel: &str, src: &str) -> Vec<Route> {
    let Some(file) = next_route_of(rel) else { return Vec::new() };
    let mut out = Vec::new();
    match file.kind {
        NextKind::Handler => {
            let mut seen = Vec::new();
            for c in next_handler_re().captures_iter(src) {
                let Some(m) = c.get(1).or_else(|| c.get(2)) else { continue };
                if seen.contains(&m.as_str()) {
                    continue;
                }
                seen.push(m.as_str());
                let line = line_at(src, c.get(0).unwrap().start());
                out.push(Route {
                    method: m.as_str().to_owned(),
                    path: file.path.clone(),
                    line,
                    handlers: vec![Handler { name: m.as_str().to_owned(), line, col: 0 }],
                    router: Some("next"),
                    ..Default::default()
                });
            }
        }
        NextKind::Page | NextKind::Api => {
            let handlers = default_export_name(src).map(|(name, at)| Handler { name, line: line_at(src, at), col: 0 }).into_iter().collect();
            let api = file.kind == NextKind::Api;
            out.push(Route {
                method: if api { "ANY".into() } else { String::new() },
                path: file.path,
                line: 1,
                handlers,
                ref_kind: if api { RefKind::FunctionRef } else { RefKind::Edge(EdgeKind::Calls) },
                router: Some("next"),
            });
        }
    }
    out
}

// ------------------------------------------------------------------- NestJS

/// `@Name(` at `at`: the arguments' text and the offset after `)`.
fn decorator_args(src: &str, open: usize) -> Option<(&str, usize)> {
    let close = match_bracket(src, open)?;
    Some((&src[open + 1..close], close + 1))
}

/// Every `@Name(...)` decorator named in `names`: (name, args, start, end).
fn decorators<'s>(src: &'s str, names: &[&str]) -> Vec<(&'s str, &'s str, usize, usize)> {
    let mut out = Vec::new();
    let b = src.as_bytes();
    let mut i = 0;
    while let Some(rel) = src[i..].find('@') {
        let at = i + rel;
        i = at + 1;
        let name_end = src[at + 1..].find(|c: char| !(c.is_alphanumeric() || c == '_')).map_or(src.len(), |e| at + 1 + e);
        let name = &src[at + 1..name_end];
        if !names.contains(&name) {
            continue;
        }
        let mut open = name_end;
        while open < b.len() && b[open].is_ascii_whitespace() {
            open += 1;
        }
        if b.get(open) != Some(&b'(') {
            continue;
        }
        if let Some((args, end)) = decorator_args(src, open) {
            out.push((name, args, at, end));
            i = end;
        }
    }
    out
}

/// The method a decorator ending at `from` sits on: skip stacked decorators and modifiers.
fn method_after(src: &str, from: usize) -> Option<(String, usize)> {
    let b = src.as_bytes();
    let mut i = from;
    let ws = |i: &mut usize| {
        while *i < b.len() && b[*i].is_ascii_whitespace() {
            *i += 1;
        }
    };
    loop {
        ws(&mut i);
        if b.get(i) != Some(&b'@') {
            break;
        }
        i += 1;
        while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'.') {
            i += 1;
        }
        ws(&mut i);
        if b.get(i) == Some(&b'(') {
            i = match_bracket(src, i)? + 1;
        }
    }
    loop {
        ws(&mut i);
        let rest = &src[i..];
        let m = ["public", "private", "protected", "async", "static", "readonly", "override"]
            .iter()
            .find(|m| rest.starts_with(**m) && rest[m.len()..].starts_with(|c: char| !(c.is_alphanumeric() || c == '_')));
        match m {
            Some(m) => i += m.len(),
            None => break,
        }
    }
    let rest = &src[i..];
    let end = rest.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$')).unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    let after = rest[end..].trim_start();
    (after.starts_with('(') || after.starts_with('<')).then(|| (rest[..end].to_owned(), i))
}

re!(first_string_re, r#"['"`]([^'"`]*)['"`]"#);
re!(path_field_re, r#"path\s*:\s*['"`]([^'"`]*)['"`]"#);
re!(namespace_re, r#"namespace\s*:\s*['"`]([^'"`]*)['"`]"#);
re!(name_field_re, r#"name\s*:\s*['"`]([^'"`]*)['"`]"#);
re!(lead_string_re, r#"^\s*['"`]([^'"`]*)['"`]"#);

fn first_string(args: &str) -> String {
    first_string_re().captures(args).map(|c| c[1].to_owned()).unwrap_or_default()
}

fn join_http(prefix: &str, sub: &str) -> String {
    let parts: Vec<&str> = [prefix, sub].iter().map(|p| p.trim().trim_matches('/')).filter(|p| !p.is_empty()).collect();
    format!("/{}", parts.join("/"))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NestClass {
    Controller,
    Resolver,
    Gateway,
    Other,
}

pub fn scan_nest(src: &str) -> Vec<Route> {
    // class scopes: each runs from its decorator to the next class decorator
    let mut raw: Vec<(NestClass, String, usize)> = Vec::new();
    for (name, args, at, _) in decorators(src, &["Controller", "Resolver", "WebSocketGateway", "Injectable", "Module", "Catch"]) {
        let (kind, prefix) = match name {
            "Controller" => (NestClass::Controller, path_field_re().captures(args).map(|c| c[1].to_owned()).unwrap_or_else(|| first_string(args))),
            "Resolver" => (NestClass::Resolver, String::new()),
            "WebSocketGateway" => (NestClass::Gateway, namespace_re().captures(args).map(|c| c[1].to_owned()).unwrap_or_default()),
            _ => (NestClass::Other, String::new()),
        };
        raw.push((kind, prefix, at));
    }
    raw.sort_by_key(|r| r.2);
    let scope_for = |at: usize| -> Option<(NestClass, &str)> {
        let i = raw.partition_point(|r| r.2 <= at);
        (i > 0).then(|| (raw[i - 1].0, raw[i - 1].1.as_str()))
    };
    let mut out = Vec::new();
    let mut add = |at: usize, method: String, path: String, handler: Option<(String, usize)>| {
        let line = line_at(src, at);
        let handlers = handler.map(|(h, _)| Handler { name: h, line, col: 0 }).into_iter().collect();
        out.push(Route { method, path, line, handlers, ..Default::default() });
    };
    for (name, args, at, end) in decorators(src, &["Get", "Post", "Put", "Patch", "Delete", "Head", "Options", "All"]) {
        let prefix = match scope_for(at) {
            Some((NestClass::Controller, p)) => p,
            _ => "",
        };
        add(at, name.to_ascii_uppercase(), join_http(prefix, &first_string(args)), method_after(src, end));
    }
    for (name, args, at, end) in decorators(src, &["Query", "Mutation", "Subscription"]) {
        // `@Query()` is also REST's parameter decorator: only a resolver's is GraphQL
        if !matches!(scope_for(at), Some((NestClass::Resolver, _))) {
            continue;
        }
        let handler = method_after(src, end);
        let op = name_field_re()
            .captures(args)
            .or_else(|| lead_string_re().captures(args))
            .map(|c| c[1].to_owned())
            .or_else(|| handler.as_ref().map(|h| h.0.clone()))
            .unwrap_or_default();
        add(at, name.to_ascii_uppercase(), op, handler);
    }
    for (name, args, at, end) in decorators(src, &["MessagePattern", "EventPattern"]) {
        let handler = method_after(src, end);
        let verb = if name == "EventPattern" { "EVENT" } else { "MESSAGE" };
        let pattern = Some(first_string(args)).filter(|s| !s.is_empty()).or_else(|| handler.as_ref().map(|h| h.0.clone())).unwrap_or_default();
        add(at, verb.into(), pattern, handler);
    }
    for (_, args, at, end) in decorators(src, &["SubscribeMessage"]) {
        let ns = match scope_for(at) {
            Some((NestClass::Gateway, p)) => p,
            _ => "",
        };
        let handler = method_after(src, end);
        let event = Some(first_string(args)).filter(|s| !s.is_empty()).or_else(|| handler.as_ref().map(|h| h.0.clone())).unwrap_or_default();
        add(at, "WS".into(), if ns.is_empty() { event } else { format!("{ns}:{event}") }, handler);
    }
    out.sort_by_key(|r| r.line);
    out
}

// ------------------------------------------------------------------- React Router

/// A route-bearing `<Route>` or route object, with the span its children sit in.
#[derive(Clone, Debug)]
struct RrScope {
    /// The path as written (`None`: a path the file does not spell out).
    part: Option<String>,
    at: usize,
    start: usize,
    end: usize,
    component: Option<String>,
    jsx: bool,
    index: bool,
    pathless: bool,
    outlet: bool,
}

re!(capital_tag_re, r"<\s*([A-Z][\w.]*)");
re!(wrapper_re, r"^(?:Suspense|ErrorBoundary|\w*(?:Guard|Provider|Wrapper|Route)|(?:Protected|Private|Auth|Require\w*)\w*)$");
re!(router_element_re, r"^(?:Browser|Hash|Memory|Native|Static|History|unstable_History)?Router$");
re!(router_call_re, r"\b(?:createBrowserRouter|createHashRouter|createMemoryRouter|createRoutesFromElements|useRoutes)\b");
re!(route_array_re, r"\b(?:const|let|var)\s+([A-Za-z_$][\w$]*)(?:\s*:\s*[^=;\n]+)?\s*=\s*\[");
re!(default_route_array_re, r"\bexport\s+default\s*\[");
re!(route_table_call_re, r"\b(?:createBrowserRouter|createHashRouter|createMemoryRouter|useRoutes)\s*\(");
re!(route_table_name_re, r"^[A-Za-z_$][\w$]*(?:\.[A-Za-z_$][\w$]*)?$");

/// The component an `element` renders: the first tag that is no guard, provider or outlet.
fn element_component(value: &str) -> Option<String> {
    let inner = value.trim().trim_start_matches('(').trim_start();
    if !inner.starts_with('<') {
        return None;
    }
    let tags: Vec<&str> = capital_tag_re().captures_iter(inner).filter_map(|c| c.get(1)).map(|m| m.as_str()).collect();
    let content = |t: &&&str| !wrapper_re().is_match(t) && **t != "Outlet";
    tags.iter().find(content).or_else(|| tags.iter().rev().find(|t| **t != "Outlet")).or(tags.first()).map(|t| t.to_string())
}

fn literal(value: &str) -> Option<String> {
    let v = value.trim();
    let v = v.strip_prefix('{').and_then(|x| x.strip_suffix('}')).map(str::trim).unwrap_or(v);
    if (v.starts_with('"') || v.starts_with('\'')) && v.len() >= 2 && v.ends_with(&v[..1]) {
        return Some(v[1..v.len() - 1].to_owned());
    }
    None
}

struct RrScan<'a> {
    s: &'a str,
    b: &'a [u8],
    jsx_ok: bool,
    objects: bool,
    scopes: Vec<RrScope>,
    routers: Vec<(usize, usize)>,
    route_tags: usize,
    depth: u32,
}

impl<'a> RrScan<'a> {
    fn trivia(&self, mut at: usize) -> usize {
        while at < self.b.len() && self.b[at].is_ascii_whitespace() {
            at += 1;
        }
        at
    }

    fn token_before(&self, at: usize) -> &'a str {
        let mut j = at;
        while j > 0 && self.b[j - 1].is_ascii_whitespace() {
            j -= 1;
        }
        let mut start = j.saturating_sub(12);
        while !self.s.is_char_boundary(start) {
            start += 1;
        }
        &self.s[start..j]
    }

    /// One syntactic unit starting at `at`: a string, a JSX element, a bracketed group or one
    /// byte. Returns the offset after it.
    fn unit(&mut self, at: usize) -> usize {
        if self.depth > 200 {
            return self.b.len();
        }
        let Some(&c) = self.b.get(at) else { return at };
        match c {
            b'"' | b'\'' => return super::href::string_end(self.s, at).map_or(at + 1, |e| e + 1),
            b'`' => return super::href::template_end(self.s, at).map_or(self.b.len(), |e| e + 1),
            b'<' if self.jsx_ok => {
                let next = self.b.get(at + 1).copied().unwrap_or(0);
                let before = self.token_before(at);
                let opens = next.is_ascii_alphabetic() || next == b'>';
                let after_value = before.ends_with(|ch: char| ch.is_alphanumeric() || "_$)]'\"".contains(ch)) && !before.ends_with("return");
                if opens && !after_value {
                    self.depth += 1;
                    let e = self.jsx(at);
                    self.depth -= 1;
                    return e;
                }
                return at + 1;
            }
            b'{' | b'[' | b'(' => {}
            _ => return at + self.s[at..].chars().next().map_or(1, char::len_utf8),
        }
        let close = match c {
            b'{' => b'}',
            b'[' => b']',
            _ => b')',
        };
        let mut before_start = at.saturating_sub(48);
        while !self.s.is_char_boundary(before_start) {
            before_start += 1;
        }
        let routers_call = c == b'(' && self.s[before_start..at].trim_end().ends_with(|ch: char| ch.is_alphanumeric()) && {
            let before = self.s[before_start..at].trim_end();
            ["createRoutesFromElements", "createRoutesFromChildren", "createBrowserRouter", "createHashRouter", "createMemoryRouter", "useRoutes"]
                .iter()
                .any(|name| before.ends_with(name))
        };
        let mut fields: Vec<(&'a str, &'a str, usize)> = Vec::new();
        let mut i = at + 1;
        self.depth += 1;
        loop {
            i = self.trivia(i);
            if i >= self.b.len() || self.b[i] == close {
                break;
            }
            let entry = i;
            let key = if c == b'{' {
                let rest = &self.s[i..];
                let name_len = rest.find(|ch: char| !(ch.is_alphanumeric() || ch == '_' || ch == '$')).unwrap_or(0);
                (name_len > 0 && rest[name_len..].trim_start().starts_with(':')).then(|| &rest[..name_len])
            } else {
                None
            };
            if let Some(name) = key {
                i = self.s[i..].find(':').map_or(self.b.len(), |k| i + k + 1);
                let value_at = self.trivia(i);
                i = value_at;
                let mut value_end = i;
                loop {
                    i = self.trivia(i);
                    if i >= self.b.len() || self.b[i] == b',' || self.b[i] == close {
                        break;
                    }
                    i = self.unit(i);
                    value_end = i;
                }
                fields.push((name, self.s[value_at..value_end.max(value_at)].trim(), entry));
            } else {
                loop {
                    i = self.trivia(i);
                    if i >= self.b.len() || self.b[i] == b',' || self.b[i] == close {
                        break;
                    }
                    i = self.unit(i);
                }
            }
            if self.b.get(i) == Some(&b',') {
                i += 1;
            }
        }
        self.depth -= 1;
        if self.objects && c == b'{' && i < self.b.len() {
            let get = |k: &str| fields.iter().find(|f| f.0 == k);
            let path = get("path").and_then(|f| literal(f.1));
            let shown = get("element")
                .and_then(|f| element_component(f.1))
                .or_else(|| get("Component").map(|f| f.1.trim().to_owned()).filter(|v| v.starts_with(|c: char| c.is_ascii_uppercase())));
            let outlet = shown.as_deref() == Some("Outlet") && path.as_deref() != Some("");
            let component = shown.filter(|s| s != "Outlet");
            if let Some(p) = get("path") {
                let opaque = path.is_none();
                self.scopes.push(RrScope {
                    part: path,
                    at: p.2,
                    start: at,
                    end: i,
                    component,
                    jsx: false,
                    index: false,
                    pathless: false,
                    outlet: outlet && !opaque,
                });
            } else if get("index").is_some_and(|f| f.1 == "true") {
                let at_index = get("index").map_or(at, |f| f.2);
                self.scopes.push(RrScope {
                    part: Some(String::new()),
                    at: at_index,
                    start: at,
                    end: i,
                    component,
                    jsx: false,
                    index: true,
                    pathless: false,
                    outlet: false,
                });
            } else if component.is_some() && get("children").is_some() {
                self.scopes.push(RrScope {
                    part: Some(String::new()),
                    at,
                    start: at,
                    end: i,
                    component,
                    jsx: false,
                    index: false,
                    pathless: true,
                    outlet: false,
                });
            }
        }
        if routers_call {
            self.routers.push((at, i));
        }
        (i + 1).min(self.b.len())
    }

    fn jsx(&mut self, at: usize) -> usize {
        let rest = &self.s[at + 1..];
        let tag_len = rest.find(|c: char| !(c.is_alphanumeric() || "_.:-".contains(c))).unwrap_or(rest.len());
        let tag = &rest[..tag_len];
        let mut i = at + 1 + tag_len;
        let mut attrs: Vec<(&'a str, &'a str)> = Vec::new();
        loop {
            i = self.trivia(i);
            if i >= self.b.len() || self.b[i] == b'>' || self.s[i..].starts_with("/>") {
                break;
            }
            let rest = &self.s[i..];
            let name_len = rest.find(|c: char| !(c.is_alphanumeric() || "_:-".contains(c))).unwrap_or(0);
            if name_len == 0 {
                i = self.unit(i);
                continue;
            }
            let name = &rest[..name_len];
            i = self.trivia(i + name_len);
            if self.b.get(i) != Some(&b'=') {
                attrs.push((name, "{true}"));
                continue;
            }
            i = self.trivia(i + 1);
            let start = i;
            i = self.unit(i);
            attrs.push((name, &self.s[start..i.min(self.b.len())]));
        }
        let get = |k: &str| attrs.iter().find(|a| a.0 == k).map(|a| a.1);
        let mut scope: Option<usize> = None;
        let mut addressless: Option<(RrScope, usize, bool)> = None;
        let router = router_element_re().is_match(tag).then(|| {
            self.routers.push((at, self.b.len()));
            self.routers.len() - 1
        });
        if tag == "Route" && i < self.b.len() {
            let path_text = get("path");
            let path = path_text.and_then(literal);
            self.route_tags += 1;
            let expr = |k: &str| get(k).map(|v| v.trim().trim_start_matches('{').trim_end_matches('}').trim());
            let element = expr("element").and_then(element_component).filter(|c| c != "Outlet");
            let component = expr("component")
                .filter(|v| v.starts_with(|c: char| c.is_ascii_uppercase()) && v.chars().all(|c| c.is_alphanumeric() || c == '_'))
                .map(str::to_owned)
                .or(element.clone());
            let route = RrScope {
                part: Some(path.clone().unwrap_or_default()),
                at,
                start: at,
                end: self.b.len(),
                component,
                jsx: true,
                index: false,
                pathless: false,
                outlet: false,
            };
            match (&path, path_text) {
                (Some(p), _) if !p.is_empty() => {
                    self.scopes.push(route);
                    scope = Some(self.scopes.len() - 1);
                }
                (_, None) | (Some(_), Some(_)) => {
                    if get("index").is_some_and(|v| v.trim_matches(['{', '}', ' ']) == "true") {
                        if route.component.is_some() {
                            self.scopes.push(RrScope { index: true, ..route });
                            scope = Some(self.scopes.len() - 1);
                        }
                    } else {
                        let page = path.as_deref() == Some("") && element.is_some();
                        addressless = Some((route, self.route_tags, page));
                    }
                }
                (None, Some(_)) => {
                    // `path={paths.home}`: an address the file does not spell out
                    self.scopes.push(RrScope { part: None, ..route });
                    scope = Some(self.scopes.len() - 1);
                }
            }
        }
        let mut close = |me: &mut Self, end: usize| -> usize {
            if let Some(s) = scope {
                me.scopes[s].end = end;
            }
            if let Some(r) = router {
                me.routers[r].1 = end;
            }
            if let Some((mut route, tags, page)) = addressless.take() {
                route.end = end;
                if me.route_tags > tags {
                    if route.component.is_some() {
                        me.scopes.push(RrScope { pathless: true, ..route });
                    }
                } else if page {
                    me.scopes.push(RrScope { index: true, ..route });
                }
            }
            end
        };
        if self.s[i..].starts_with("/>") {
            return close(self, i + 2);
        }
        i += 1;
        while i < self.b.len() {
            if !self.s.is_char_boundary(i) {
                i += 1;
                continue;
            }
            if self.s[i..].starts_with("</") {
                let end = self.s[i + 2..].find('>').map_or(self.b.len(), |e| i + 2 + e + 1);
                return close(self, end);
            }
            match self.b[i] {
                b'<' if self.b.get(i + 1).is_some_and(|c| c.is_ascii_alphabetic() || *c == b'>') => {
                    self.depth += 1;
                    i = self.jsx(i);
                    self.depth -= 1;
                }
                b'{' => i = self.unit(i),
                _ => i += 1,
            }
        }
        close(self, i)
    }
}

/// The routes a file's own text declares (upstream `scanRoutes` for one file).
pub fn scan_react_router(rel: &str, src: &str) -> Vec<Route> {
    scan_react_router_detailed(rel, src).into_iter().map(|(route, _)| route).collect()
}

fn scan_react_router_detailed(rel: &str, src: &str) -> Vec<(Route, bool)> {
    let objects = router_call_re().is_match(src);
    if !objects && !src.contains("<Route") {
        return Vec::new();
    }
    let jsx_ok = !rel.ends_with(".ts") && !rel.ends_with(".mts") && !rel.ends_with(".cts");
    let mut scan = RrScan { s: src, b: src.as_bytes(), jsx_ok, objects, scopes: Vec::new(), routers: Vec::new(), route_tags: 0, depth: 0 };
    let mut at = 0;
    while at < src.len() {
        at = scan.trivia(at);
        at = scan.unit(at);
    }
    let scopes = scan.scopes;
    let routers = scan.routers;
    let holds = |o: &RrScope, start: usize, end: usize| o.start < start && o.end >= end;
    let around = |start: usize, end: usize| -> Vec<&RrScope> {
        let mut v: Vec<&RrScope> = scopes.iter().filter(|o| holds(o, start, end)).collect();
        v.sort_by_key(|o| o.start);
        v
    };
    let path_of = |s: &RrScope| -> Option<String> {
        let mut parts: Vec<&str> = Vec::new();
        for o in around(s.start, s.end) {
            parts.push(o.part.as_deref()?);
        }
        parts.push(s.part.as_deref()?);
        Some(compose_route_path(&parts))
    };
    let in_router = |s: &RrScope| routers.iter().any(|r| r.0 < s.start && r.1 >= s.end);
    let addressed = |s: &RrScope| {
        if !s.index {
            return true;
        }
        let outer = around(s.start, s.end);
        !outer.iter().any(|c| c.part.is_none()) && (outer.iter().any(|c| c.part.as_deref().is_some_and(|p| !p.is_empty())) || in_router(s))
    };
    let pages: Vec<&RrScope> =
        scopes.iter().filter(|s| !s.pathless && s.part.is_some() && (s.jsx || s.outlet || s.component.is_some()) && addressed(s)).collect();
    let mut out = Vec::new();
    for s in &pages {
        let Some(path) = path_of(s) else { continue };
        // a route around others is their layout when one of them claims its address
        let claimed = pages.iter().any(|inner| {
            !std::ptr::eq(*inner, *s)
                && inner.jsx == s.jsx
                && inner.component.is_some()
                && holds(s, inner.start, inner.end)
                && path_of(inner).as_deref() == Some(path.as_str())
        });
        if claimed || path.ends_with('*') {
            continue;
        }
        let line = line_at(src, s.at);
        let handlers = s.component.iter().map(|c| Handler { name: c.clone(), line, col: 0 }).collect();
        let absolute = around(s.start, s.end).into_iter().chain(std::iter::once(*s)).any(|o| o.part.as_deref().is_some_and(|p| p.starts_with('/')));
        out.push((
            Route { method: String::new(), path, line, handlers, ref_kind: RefKind::Edge(EdgeKind::References), router: Some("react-router") },
            absolute,
        ));
    }
    out.sort_by_key(|r| r.0.line);
    out.dedup_by(|a, b| a.0.path == b.0.path && a.0.line == b.0.line);
    out
}

/// Route-object arrays declared separately from the router that consumes them. The nodes are
/// candidates: navigation uses them only after an actual router call resolves to this binding.
fn route_arrays(src: &str) -> Vec<(String, usize, usize)> {
    let mut arrays = Vec::new();
    for cap in route_array_re().captures_iter(src) {
        let open = cap.get(0).unwrap().end() - 1;
        if let Some(close) = match_bracket(src, open) {
            arrays.push((cap[1].to_owned(), open, close));
        }
    }
    for hit in default_route_array_re().find_iter(src) {
        let open = hit.end() - 1;
        if let Some(close) = match_bracket(src, open) {
            arrays.push(("#default".to_owned(), open, close));
        }
    }
    arrays
}

pub fn scan_react_tables(rel: &str, src: &str) -> Vec<(String, Route, bool)> {
    if !src.contains("export") {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (name, open, close) in route_arrays(src) {
        let wrapped = format!("createBrowserRouter({})", &src[open..=close]);
        let line_offset = line_at(src, open) - 1;
        for (mut route, absolute) in scan_react_router_detailed(rel, &wrapped) {
            route.line += line_offset;
            for handler in &mut route.handlers {
                handler.line += line_offset;
            }
            route.router = Some("react-router-table");
            out.push((name.clone(), route, absolute));
        }
    }
    out
}

/// A reference to an array used as the router's input or a route object's children.
pub struct ReactTableUse {
    pub owner: String,
    pub name: String,
    pub prefix: String,
    pub absolute: bool,
}

fn react_refs(expr: &str, owner: &str, prefix: &str, absolute: bool, out: &mut Vec<ReactTableUse>, depth: u8) {
    if depth > 16 {
        return;
    }
    let expr = expr.trim();
    let name = expr.strip_prefix("...").unwrap_or(expr).trim();
    if route_table_name_re().is_match(name) {
        out.push(ReactTableUse { owner: owner.to_owned(), name: name.to_owned(), prefix: prefix.to_owned(), absolute });
        return;
    }
    if expr.starts_with('[') && match_bracket(expr, 0) == Some(expr.len() - 1) {
        let body = &expr[1..expr.len() - 1];
        let mut start = 0;
        while start < body.len() {
            let end = super::href::index_at_depth0(body, b',', start).unwrap_or(body.len());
            react_refs(&body[start..end], owner, prefix, absolute, out, depth + 1);
            start = end + 1;
        }
    } else if expr.starts_with('{') && match_bracket(expr, 0) == Some(expr.len() - 1) {
        let part = match object_field(expr, "path") {
            Some(p) => match literal(p) {
                Some(p) => p,
                None => return,
            },
            None => String::new(),
        };
        let is_absolute = absolute || part.starts_with('/');
        let path = compose_route_path(&[prefix, &part]);
        if let Some(children) = object_field(expr, "children") {
            react_refs(children, owner, &path, is_absolute, out, depth + 1);
        }
    }
}

pub fn react_table_uses(src: &str) -> Vec<ReactTableUse> {
    let mut out = Vec::new();
    for m in route_table_call_re().find_iter(src) {
        if let Some(first) = crate::synth::scan::call_args(src, m.end() - 1).and_then(|args| args.first().copied()) {
            react_refs(first, "", "", false, &mut out, 0);
        }
    }
    if src.contains("export") {
        for (name, open, close) in route_arrays(src) {
            react_refs(&src[open..=close], &name, "", false, &mut out, 0);
        }
    }
    out
}

// ------------------------------------------------------------------- TanStack Router

re!(tanstack_factory_re, r"\bcreate(?:File|Lazy(?:File)?|Root)?Route\s*\(");
re!(tanstack_file_re, r"\bcreate(?:Lazy)?FileRoute\s*\(");
re!(tanstack_named_re, r"\b(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*(?::[^=]*?)?=\s*create(Root)?Route\s*\(\s*\{");
re!(tanstack_lazy_import_re, r#"\bimport\s*\(\s*['"`]([^'"`]+)['"`]"#);
re!(tanstack_component_re, r"(?:^|[^\w$])component\s*:\s*([A-Z][A-Za-z0-9_]*)");
re!(parent_arrow_re, r"=>\s*([A-Za-z_$][\w$]*)");

/// The component a route names: an identifier, or the file a lazy import names.
fn tanstack_component(text: &str) -> Option<String> {
    if let Some(c) = tanstack_lazy_import_re().captures(text) {
        let file = c[1].rsplit('/').next().unwrap_or("");
        let stem = file.rsplit_once('.').map_or(file, |x| x.0);
        return (!stem.is_empty()).then(|| stem.to_owned());
    }
    if let Some(c) = tanstack_component_re().captures(text) {
        return Some(c[1].to_owned());
    }
    let t = text.trim();
    (t.starts_with(|c: char| c.is_ascii_uppercase()) && t.chars().all(|c| c.is_alphanumeric() || c == '_')).then(|| t.to_owned())
}

/// The call chain after a factory call: `({ … })` and any `.update({ … })` / `.lazy(…)`.
fn chain_after(s: &str, at: usize) -> &str {
    let b = s.as_bytes();
    let mut i = at;
    for _ in 0..8 {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if b.get(i) == Some(&b'.') {
            i += 1;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$') {
                i += 1;
            }
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
        }
        if b.get(i) != Some(&b'(') {
            break;
        }
        match match_bracket(s, i) {
            Some(c) => i = c + 1,
            None => break,
        }
    }
    &s[at..i.min(s.len())]
}

fn is_pathless_layout(raw: &str) -> bool {
    if raw.len() > 1 && raw.ends_with('/') {
        return false; // the index route inside the layout
    }
    raw.split('/').rfind(|s| !s.is_empty()).is_some_and(|l| l.starts_with('_'))
}

pub fn scan_tanstack(rel: &str, src: &str) -> Vec<Route> {
    if !tanstack_factory_re().is_match(src) {
        return Vec::new();
    }
    let base = rel.rsplit('/').next().unwrap_or(rel);
    let layout_file =
        base == "route.tsx" || base == "route.ts" || base == "route.jsx" || base == "route.js" || base.contains(".route.") || src.contains("<Outlet");
    // (path, component, index, line)
    let mut entries: Vec<(String, Option<String>, bool, u32)> = Vec::new();
    for m in tanstack_file_re().find_iter(src) {
        let open = m.end() - 1;
        let Some(close) = match_bracket(src, open) else { continue };
        let inner = src[open + 1..close].trim_start();
        let Some(raw) = read_string_at(inner, 0) else { continue };
        let Some(path) = tanstack_path(&raw) else { continue };
        if is_pathless_layout(&raw) || layout_file {
            continue;
        }
        let index = raw.len() > 1 && raw.ends_with('/');
        entries.push((path, tanstack_component(chain_after(src, close + 1)), index, line_at(src, m.start())));
    }
    // code-based routes: a path per route, composed through `getParentRoute`
    struct Decl {
        name: String,
        path: Option<String>,
        parent: Option<String>,
        component: Option<String>,
        root: bool,
        at: usize,
    }
    let mut decls: Vec<Decl> = Vec::new();
    for c in tanstack_named_re().captures_iter(src) {
        let whole = c.get(0).unwrap();
        let brace = whole.end() - 1;
        let Some(end) = match_bracket(src, brace) else { continue };
        let obj = &src[brace..=end];
        let root = c.get(2).is_some();
        let path = if root { Some("/".to_owned()) } else { object_field(obj, "path").and_then(|v| read_string_at(v, 0)) };
        let parent = object_field(obj, "getParentRoute").and_then(|v| parent_arrow_re().captures(v)).map(|p| p[1].to_owned());
        let component = object_field(obj, "component").and_then(tanstack_component);
        decls.push(Decl { name: c[1].to_owned(), path, parent, component, root, at: whole.start() });
    }
    let wraps_index: Vec<&str> = decls.iter().filter(|d| d.path.as_deref() == Some("/") && d.parent.is_some()).filter_map(|d| d.parent.as_deref()).collect();
    for d in &decls {
        let Some(own) = &d.path else { continue };
        if d.root || wraps_index.contains(&d.name.as_str()) {
            continue;
        }
        // compose through the parents declared in this file
        let mut segs: Vec<String> = Vec::new();
        let mut cur = Some(d.name.as_str());
        let mut ok = true;
        for _ in 0..24 {
            let Some(name) = cur else { break };
            let Some(decl) = decls.iter().find(|x| x.name == name) else {
                ok = false;
                break;
            };
            if let Some(p) = &decl.path {
                let mut own: Vec<String> = p.split('/').filter(|s| !s.is_empty()).map(str::to_owned).collect();
                own.extend(segs);
                segs = own;
            }
            cur = decl.parent.as_deref();
        }
        if !ok {
            continue;
        }
        let Some(path) = tanstack_path(&format!("/{}", segs.join("/"))) else { continue };
        entries.push((path, d.component.clone(), own == "/", line_at(src, d.at)));
    }
    // one route per address: an index route outranks the layout that wraps it
    let mut by_path: Vec<(String, Option<String>, bool, u32)> = Vec::new();
    for e in entries {
        match by_path.iter_mut().find(|x| x.0 == e.0) {
            Some(held) if e.2 && !held.2 => *held = e,
            Some(_) => {}
            None => by_path.push(e),
        }
    }
    by_path
        .into_iter()
        .map(|(path, component, _, line)| Route {
            method: String::new(),
            path,
            line,
            handlers: component.map(|c| Handler { name: c, line, col: 0 }).into_iter().collect(),
            ref_kind: RefKind::Edge(EdgeKind::Calls),
            router: Some("tanstack-router"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(r: &[Route]) -> Vec<String> {
        r.iter()
            .map(|r| {
                format!(
                    "{}{} -> {}",
                    if r.method.is_empty() { String::new() } else { format!("{} ", r.method) },
                    r.path,
                    r.handlers.iter().map(|h| h.name.as_str()).collect::<Vec<_>>().join("|")
                )
            })
            .collect()
    }

    #[test]
    fn next_files() {
        let r = |p: &str| next_route_of(p).map(|n| (n.kind, n.path, n.root));
        assert_eq!(r("app/users/[id]/page.tsx"), Some((NextKind::Page, "/users/:id".into(), "".into())));
        assert_eq!(r("apps/web/src/app/(shop)/cart/page.tsx"), Some((NextKind::Page, "/cart".into(), "apps/web/".into())));
        assert_eq!(r("app/docs/[...all]/page.mdx"), Some((NextKind::Page, "/docs/:all*".into(), "".into())));
        assert_eq!(r("app/api/x/route.ts"), Some((NextKind::Handler, "/api/x".into(), "".into())));
        assert_eq!(r("app/@modal/x/page.tsx"), None);
        assert_eq!(r("app/layout.tsx"), None);
        assert_eq!(r("pages/index.tsx"), Some((NextKind::Page, "/".into(), "".into())));
        assert_eq!(r("pages/api/users.ts"), Some((NextKind::Api, "/api/users".into(), "".into())));
        assert_eq!(r("pages/_app.tsx"), None);
        assert_eq!(r("src/pages/blog/[slug].tsx"), Some((NextKind::Page, "/blog/:slug".into(), "".into())));
        assert_eq!(names(&scan_next("app/cart/page.tsx", "export default function Cart() { return null }")), vec!["/cart -> Cart"]);
        assert_eq!(names(&scan_next("app/a/page.tsx", "function A() {}\nexport default memo(A);")), vec!["/a -> A"]);
        assert_eq!(
            names(&scan_next("app/api/x/route.ts", "export async function GET(req) {}\nexport const POST = async () => {}")),
            vec!["GET /api/x -> GET", "POST /api/x -> POST"]
        );
    }

    #[test]
    fn nest_decorators() {
        let src = r#"
@Controller('users')
export class UsersController {
  constructor(private readonly svc: UsersService) {}
  @Get(':id')
  @UseGuards(AuthGuard)
  async findOne(@Param('id') id: string) {}
  @Post()
  create(@Body() dto: Dto) {}
}
@Resolver(() => User)
export class UsersResolver {
  @Query(() => [User], { name: 'users' })
  all() {}
}
@WebSocketGateway({ namespace: 'chat' })
export class ChatGateway {
  @SubscribeMessage('message')
  handle() {}
}
@Controller()
export class Jobs {
  @EventPattern('user.created')
  onCreated() {}
}
"#;
        assert_eq!(
            names(&scan_nest(src)),
            vec!["GET /users/:id -> findOne", "POST /users -> create", "QUERY users -> all", "WS chat:message -> handle", "EVENT user.created -> onCreated"]
        );
    }

    #[test]
    fn react_router_markup_and_objects() {
        let src = r#"
export function App() {
  return (
    <BrowserRouter>
      <Routes>
        <Route path="/" element={<Layout />}>
          <Route index element={<Home />} />
          <Route path="dashboard" element={<RequireAuth><Dashboard /></RequireAuth>}>
            <Route path="team" element={<Team />} />
          </Route>
          <Route path="*" element={<NotFound />} />
        </Route>
        <Route path="/payment" component={PaymentScreen} />
      </Routes>
    </BrowserRouter>
  );
}
"#;
        assert_eq!(
            names(&scan_react_router("src/App.tsx", src)),
            vec!["/ -> Home", "/dashboard -> Dashboard", "/dashboard/team -> Team", "/payment -> PaymentScreen"]
        );
        let objects = "const router = createBrowserRouter([\n  { path: '/', element: <Root />, children: [\n    { index: true, element: <Index /> },\n    { path: 'posts/:id', element: <Post /> },\n  ] },\n]);\n";
        assert_eq!(names(&scan_react_router("src/router.tsx", objects)), vec!["/ -> Index", "/posts/:id -> Post"]);
        // a menu list that never reaches the router is no route table
        assert!(scan_react_router("src/menu.ts", "export const menu = [{ path: '/x', element: Foo }];").is_empty());
    }

    #[test]
    fn tanstack_routes() {
        let file = "export const Route = createFileRoute('/posts/$postId')({\n  component: PostComponent,\n})\n";
        assert_eq!(names(&scan_tanstack("src/routes/posts.$postId.tsx", file)), vec!["/posts/:postId -> PostComponent"]);
        assert!(scan_tanstack("src/routes/_auth.tsx", "export const Route = createFileRoute('/_auth')({ component: A })").is_empty());
        let code = "const rootRoute = createRootRoute({ component: Root })\nconst postsRoute = createRoute({ getParentRoute: () => rootRoute, path: 'posts', component: Posts })\nconst postRoute = createRoute({ getParentRoute: () => postsRoute, path: '$postId', component: Post })\n";
        assert_eq!(names(&scan_tanstack("src/main.tsx", code)), vec!["/posts -> Posts", "/posts/:postId -> Post"]);
    }
}
