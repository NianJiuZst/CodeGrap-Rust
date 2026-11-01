//! Destinations written as strings (upstream `expo-router.ts` / `nextjs.ts` readers): a
//! navigation's href, a client's request path. A literal, a template (its `${…}` parts become
//! holes), an object's `pathname` / `to`, or a conditional whose every arm is one of those. The
//! matcher pairs an href with the one route of a table that serves it: a literal segment beats
//! a parameter, which beats a hole; a tie is no answer.

use cg_core::NodeIdx;
use rustc_hash::FxHashMap;

/// Stands for a `${…}` hole inside a stored path.
pub const HOLE: char = '\u{1}';

/// One destination: the path (query and hash removed, holes kept) and the text as written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Href {
    pub path: String,
}

impl Href {
    pub fn display(&self) -> String {
        self.path.replace(HOLE, "${…}")
    }
}

/// Byte offset of the `)`/`]`/`}` closing the bracket at `open`, skipping strings and
/// templates; `None` when unbalanced.
pub fn match_bracket(s: &str, open: usize) -> Option<usize> {
    let b = s.as_bytes();
    let mut depth = 0i32;
    let mut i = open;
    while i < b.len() {
        match b[i] {
            b'"' | b'\'' => i = string_end(s, i)?,
            b'`' => i = template_end(s, i)?,
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
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

/// Offset of the closing quote of the string opening at `at`.
pub fn string_end(s: &str, at: usize) -> Option<usize> {
    let b = s.as_bytes();
    let q = b[at];
    if q == b'`' {
        return template_end(s, at);
    }
    let mut i = at + 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1,
            c if c == q => return Some(i),
            b'\n' => return None,
            _ => {}
        }
        i += 1;
    }
    None
}

/// Offset of the backtick closing the template opening at `open`.
pub fn template_end(s: &str, open: usize) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = open + 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1,
            b'`' => return Some(i),
            b'$' if b.get(i + 1) == Some(&b'{') => {
                let mut depth = 0;
                i += 1;
                while i < b.len() {
                    match b[i] {
                        b'{' => depth += 1,
                        b'}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        b'`' => i = template_end(s, i)?,
                        b'"' | b'\'' => i = string_end(s, i)?,
                        _ => {}
                    }
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// The string literal starting at `at`, with each `${…}` replaced by [`HOLE`]; `None` when
/// `at` holds no string.
pub fn read_string_at(s: &str, at: usize) -> Option<String> {
    let b = s.as_bytes();
    let q = *b.get(at)?;
    match q {
        b'"' | b'\'' => {
            let end = string_end(s, at)?;
            Some(unescape(&s[at + 1..end]))
        }
        b'`' => {
            let end = template_end(s, at)?;
            let body = &s[at + 1..end];
            let mut out = String::new();
            let bb = body.as_bytes();
            let mut i = 0;
            let mut lit = 0;
            while i < bb.len() {
                if bb[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if bb[i] == b'$' && bb.get(i + 1) == Some(&b'{') {
                    out.push_str(&unescape(&body[lit..i]));
                    let close = match_bracket(body, i + 1)?;
                    out.push(HOLE);
                    i = close + 1;
                    lit = i;
                    continue;
                }
                i += 1;
            }
            out.push_str(&unescape(&body[lit.min(body.len())..]));
            Some(out)
        }
        _ => None,
    }
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Offset of the first `ch` at bracket depth 0 and outside strings at or after `from`.
pub fn index_at_depth0(s: &str, ch: u8, from: usize) -> Option<usize> {
    let b = s.as_bytes();
    let mut depth = 0i32;
    let mut i = from;
    while i < b.len() {
        match b[i] {
            b'"' | b'\'' => i = string_end(s, i).unwrap_or(b.len()),
            b'`' => i = template_end(s, i).unwrap_or(b.len()),
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b'?' if matches!(b.get(i + 1), Some(b'.' | b'?')) && ch == b'?' => i += 1,
            c if depth == 0 && c == ch => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

/// The `:` closing the ternary opened at `q`, nested ternaries paired.
fn ternary_colon(s: &str, q: usize) -> Option<usize> {
    let mut depth = 0;
    let mut i = q + 1;
    for _ in 0..64 {
        let next_q = index_at_depth0(s, b'?', i);
        let colon = index_at_depth0(s, b':', i)?;
        if let Some(nq) = next_q.filter(|nq| *nq < colon) {
            depth += 1;
            i = nq + 1;
            continue;
        }
        if depth == 0 {
            return Some(colon);
        }
        depth -= 1;
        i = colon + 1;
    }
    None
}

/// An href from a literal: the path before `?` / `#`.
pub fn to_href(literal: &str) -> Option<Href> {
    let cut = literal.find(['?', '#']).unwrap_or(literal.len());
    let path = &literal[..cut];
    (!path.is_empty()).then(|| Href { path: path.to_owned() })
}

fn strip_cast(s: &str) -> &str {
    // `expr as any`, `expr satisfies Href`
    for kw in [" as ", " satisfies "] {
        if let Some(i) = s.rfind(kw) {
            let tail = &s[i + kw.len()..];
            if tail.chars().all(|c| c.is_alphanumeric() || "_$.<>[]|& ".contains(c)) {
                return s[..i].trim_end();
            }
        }
    }
    s
}

/// Every destination an argument expression names (`key` reads an object's field: `pathname`
/// for an `Href` object, `to` for TanStack's): one per arm of a conditional.
pub fn parse_href_expression(expr: &str, key: &str) -> Vec<Href> {
    let mut args = strip_cast(expr.trim()).trim();
    while args.starts_with('(') && match_bracket(args, 0) == Some(args.len() - 1) {
        args = strip_cast(args[1..args.len() - 1].trim()).trim();
    }
    if args.is_empty() {
        return Vec::new();
    }
    if let Some(q) = index_at_depth0(args, b'?', 0).filter(|q| *q > 0) {
        if let Some(colon) = ternary_colon(args, q) {
            let mut arms = parse_href_expression(&args[q + 1..colon], key);
            for arm in parse_href_expression(&args[colon + 1..], key) {
                if !arms.contains(&arm) {
                    arms.push(arm);
                }
            }
            return arms;
        }
    }
    if args.starts_with('{') {
        return object_field(args, key).and_then(|v| read_string_at(v, 0)).and_then(|l| to_href(&l)).into_iter().collect();
    }
    read_string_at(args, 0).and_then(|l| to_href(&l)).into_iter().collect()
}

/// The text of field `key`'s value in the object literal `obj` (`{ a: 1, key: 'x' }`).
pub fn object_field<'a>(obj: &'a str, key: &str) -> Option<&'a str> {
    let end = match_bracket(obj, 0)?;
    let body = &obj[1..end];
    let mut i = 0;
    let b = body.as_bytes();
    while i < b.len() {
        let rest = &body[i..];
        let trimmed = rest.trim_start();
        let at = i + (rest.len() - trimmed.len());
        let name_len = trimmed.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$')).unwrap_or(trimmed.len());
        let name = &trimmed[..name_len];
        let after = trimmed[name_len..].trim_start();
        if name == key && after.starts_with(':') {
            let value = after[1..].trim_start();
            let start = body.len() - value.len();
            let stop = index_at_depth0(body, b',', start).unwrap_or(body.len());
            return Some(body[start..stop].trim());
        }
        // skip to the next entry
        match index_at_depth0(body, b',', at.max(i)) {
            Some(c) => i = c + 1,
            None => break,
        }
    }
    None
}

/// The directory of the app owning a file: everything before the first conventional source
/// directory (upstream `appRootFor`).
pub fn app_root_for(path: &str) -> &str {
    let mut at = 0;
    for seg in path.split('/') {
        if matches!(seg, "src" | "pages" | "app" | "routes") && at + seg.len() < path.len() {
            return &path[..at];
        }
        at += seg.len() + 1;
    }
    match path.rfind('/') {
        Some(i) => &path[..=i],
        None => "",
    }
}

fn is_param(seg: &str) -> bool {
    (seg.starts_with('[') && seg.ends_with(']')) || (seg.starts_with(':') && !seg.ends_with('*'))
}

fn is_catch_all(seg: &str) -> bool {
    (seg.starts_with("[...") && seg.ends_with(']')) || (seg.starts_with(':') && seg.ends_with('*'))
}

/// Segments of an href path: `/users/${id}` -> `["users", "*"]`; an absolute URL keeps its
/// path; a relative href is nothing.
pub fn href_segments(path: &str) -> Option<Vec<String>> {
    let mut p = path;
    if let Some(i) = p.find("//") {
        let scheme = &p[..i];
        if scheme.is_empty() || scheme.ends_with(':') && scheme[..scheme.len() - 1].chars().all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c)) {
            let rest = &p[i + 2..];
            p = rest.find('/').map_or("/", |j| &rest[j..]);
        }
    }
    if !p.starts_with('/') {
        return None;
    }
    Some(p.split('/').filter(|s| !s.is_empty()).map(|s| if s.contains(HOLE) { "*".to_owned() } else { s.to_owned() }).collect())
}

/// Score of an href's segments against a route's, `None` when it does not match.
fn score(href: &[String], route: &[&str]) -> Option<u32> {
    let mut score = 0;
    let mut i = 0;
    for seg in route {
        if is_catch_all(seg) {
            if i >= href.len() {
                return None;
            }
            score += (href.len() - i) as u32;
            i = href.len();
            continue;
        }
        let h = href.get(i)?;
        if is_param(seg) {
            score += 2;
        } else if h == seg {
            score += 3;
        } else if h == "*" {
            score += 1;
        } else {
            return None;
        }
        i += 1;
    }
    (i == href.len()).then_some(score)
}

/// Routes by path; dynamic ones are matched segment by segment.
#[derive(Default)]
pub struct RouteTable {
    pub exact: FxHashMap<String, NodeIdx>,
    pub dynamic: Vec<(NodeIdx, Vec<String>)>,
}

impl RouteTable {
    /// The first route to claim an address keeps it.
    pub fn add(&mut self, path: &str, node: NodeIdx) {
        self.exact.entry(path.to_owned()).or_insert(node);
        if path.contains(':') || path.contains('[') {
            self.dynamic.push((node, path.split('/').skip(1).map(str::to_owned).collect()));
        }
    }

    pub fn is_empty(&self) -> bool {
        self.exact.is_empty()
    }

    /// The one route the segments denote.
    pub fn match_segments(&self, segs: &[String]) -> Option<NodeIdx> {
        if let Some(n) = self.exact.get(&format!("/{}", segs.join("/"))) {
            return Some(*n);
        }
        let mut best: Option<(NodeIdx, u32)> = None;
        let mut tied = false;
        for (node, route) in &self.dynamic {
            let route: Vec<&str> = route.iter().map(String::as_str).collect();
            let Some(s) = score(segs, &route) else { continue };
            match best {
                Some((_, b)) if s < b => {}
                Some((_, b)) if s == b => tied = true,
                _ => {
                    best = Some((*node, s));
                    tied = false;
                }
            }
        }
        best.filter(|_| !tied).map(|b| b.0)
    }

    /// The routes an href's arms name, each once, with the arm that named it.
    pub fn destinations<'h>(&self, arms: &'h [Href]) -> Vec<(NodeIdx, &'h Href)> {
        let mut out: Vec<(NodeIdx, &Href)> = Vec::new();
        for arm in arms {
            let Some(segs) = href_segments(&arm.path) else { continue };
            if let Some(n) = self.match_segments(&segs) {
                if !out.iter().any(|(m, _)| *m == n) {
                    out.push((n, arm));
                }
            }
        }
        out
    }
}

/// Route tables split by the app each route belongs to (a monorepo has a `/` per app).
#[derive(Default)]
pub struct RootedTables {
    pub by_root: Vec<(String, RouteTable)>,
}

impl RootedTables {
    pub fn add(&mut self, root: &str, path: &str, node: NodeIdx) {
        match self.by_root.iter_mut().find(|(r, _)| r == root) {
            Some((_, t)) => t.add(path, node),
            None => {
                let mut t = RouteTable::default();
                t.add(path, node);
                self.by_root.push((root.to_owned(), t));
            }
        }
    }

    /// The table of the app `path` belongs to: the longest root it is under.
    pub fn for_file(&self, path: &str) -> Option<&RouteTable> {
        self.by_root.iter().filter(|(r, _)| path.starts_with(r.as_str())).max_by_key(|(r, _)| r.len()).map(|(_, t)| t)
    }

    pub fn is_empty(&self) -> bool {
        self.by_root.is_empty()
    }
}

/// Encode an href's arms for a site payload, one per line.
pub fn encode_arms(arms: &[Href]) -> String {
    arms.iter().map(|a| a.path.as_str()).collect::<Vec<_>>().join("\n")
}

pub fn decode_arms(payload: &str) -> Vec<Href> {
    payload.split('\n').filter(|p| !p.is_empty()).map(|p| Href { path: p.to_owned() }).collect()
}

/// React Router's nesting: a child path is relative unless it starts with `/`.
pub fn compose_route_path(parts: &[&str]) -> String {
    let mut path = String::new();
    for part in parts {
        if part.starts_with('/') {
            path = (*part).to_owned();
        } else if !part.is_empty() {
            path = format!("{}/{part}", path.trim_end_matches('/'));
        }
    }
    let mut out = format!("/{}", path.trim_start_matches('/'));
    while out.contains("//") {
        out = out.replace("//", "/");
    }
    if out.len() > 1 && out.ends_with('/') {
        out.pop();
    }
    out
}

/// A TanStack path in the form the other routers use: `$id` -> `:id`, `$` -> a splat; `_auth`
/// (pathless layout) and `(group)` segments are not in the URL; a trailing `_` un-nests.
pub fn tanstack_path(raw: &str) -> Option<String> {
    if !raw.starts_with('/') {
        return None;
    }
    let mut segs = Vec::new();
    for seg in raw.split('/') {
        if seg.is_empty() || seg.starts_with('_') || (seg.starts_with('(') && seg.ends_with(')')) {
            continue;
        }
        let bare = seg.strip_suffix('_').unwrap_or(seg);
        if bare == "$" {
            segs.push(":splat*".to_owned());
        } else if let Some(p) = bare.strip_prefix('$') {
            segs.push(format!(":{p}"));
        } else {
            segs.push(bare.to_owned());
        }
    }
    Some(format!("/{}", segs.join("/")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(v: &[Href]) -> Vec<String> {
        v.iter().map(Href::display).collect()
    }

    #[test]
    fn reads_destinations() {
        assert_eq!(paths(&parse_href_expression("'/users?tab=1'", "pathname")), vec!["/users"]);
        assert_eq!(paths(&parse_href_expression("`/users/${u.id}/edit`", "pathname")), vec!["/users/${…}/edit"]);
        assert_eq!(paths(&parse_href_expression("{ pathname: '/p/[id]', query: { id } }", "pathname")), vec!["/p/[id]"]);
        assert_eq!(paths(&parse_href_expression("isAdmin ? '/admin' : ok ? '/a' : '/b'", "pathname")), vec!["/admin", "/a", "/b"]);
        assert_eq!(paths(&parse_href_expression("{ to: '/posts/$postId', params }", "to")), vec!["/posts/$postId"]);
        assert!(parse_href_expression("next", "pathname").is_empty());
        assert_eq!(paths(&parse_href_expression("('/x' as any)", "pathname")), vec!["/x"]);
    }

    #[test]
    fn matches_routes() {
        let mut t = RouteTable::default();
        t.add("/users", NodeIdx(1));
        t.add("/users/:id", NodeIdx(2));
        t.add("/users/top", NodeIdx(3));
        t.add("/docs/:all*", NodeIdx(4));
        let m = |p: &str| href_segments(p).and_then(|s| t.match_segments(&s));
        assert_eq!(m("/users"), Some(NodeIdx(1)));
        assert_eq!(m("/users/5"), Some(NodeIdx(2)));
        assert_eq!(m("/users/top"), Some(NodeIdx(3)));
        assert_eq!(m(&format!("/users/{HOLE}")), Some(NodeIdx(2)));
        assert_eq!(m("/docs/a/b"), Some(NodeIdx(4)));
        assert_eq!(m("https://x.com/users"), Some(NodeIdx(1)));
        assert_eq!(m("users"), None);
        assert_eq!(m("/nope"), None);
    }

    #[test]
    fn paths_and_roots() {
        assert_eq!(compose_route_path(&["/dashboard", "team", ""]), "/dashboard/team");
        assert_eq!(compose_route_path(&["/a", "/b"]), "/b");
        assert_eq!(compose_route_path(&["", ""]), "/");
        assert_eq!(tanstack_path("/posts/$postId").as_deref(), Some("/posts/:postId"));
        assert_eq!(tanstack_path("/_auth/invoices/").as_deref(), Some("/invoices"));
        assert_eq!(app_root_for("apps/web/src/App.tsx"), "apps/web/");
        assert_eq!(app_root_for("src/App.tsx"), "");
        assert_eq!(app_root_for("lib/x.ts"), "lib/");
    }
}
