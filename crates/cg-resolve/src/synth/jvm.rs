//! Java / Kotlin links no call writes.
//!
//! * `test-request` (upstream `testRequestEdges`): a Spring test reaches a controller by URL -
//!   MockMvc's `perform(post("/owners/new"))`, Kotlin's `mockMvc.get("/x")`, WebTestClient's
//!   `.get().uri("/x")`, TestRestTemplate and RestAssured. The test method links to the route
//!   the method and path match; the route's own edge reaches the handler.
//! * `kotlin-expect-actual` (upstream `kotlinExpectActualEdges`): a Multiplatform `expect`
//!   declaration links to each `actual` of the same qualified name in another file, so editing
//!   a platform implementation surfaces the common declaration and its callers.

use super::dynamic::{client_path, http_routes, match_http};
use super::scan::{enclosing_row, is_minified, LineIndex, FN_KINDS};
use super::sites::PASS_TEST_REQUEST;
use super::{order_of, Out};
use crate::engine::Engine;
use crate::frameworks::href::{match_bracket, HOLE};
use cg_core::{EdgeKind, Lang, NodeIdx, NodeKind, StrId, NONE};
use cg_extract::{FileHook, HookCtx, HookOut, SiteFact};
use regex::Regex;
use std::sync::OnceLock;

const MAX_ACTUALS: usize = 40;

macro_rules! re {
    ($name:ident, $pat:expr) => {
        fn $name() -> &'static Regex {
            static R: OnceLock<Regex> = OnceLock::new();
            R.get_or_init(|| Regex::new($pat).expect("static regex"))
        }
    };
}

const VERBS: &str = "get|post|put|patch|delete|head|options";
re!(gate_re, r"[mM]ockMvc|[wW]ebTestClient\b|[rR]estTemplate\b|\bRestAssured\b|\.\s*when\s*\(\s*\)");
re!(mockmvc_builder_re, &format!(r"(?:^|[^\w$.])(?:MockMvcRequestBuilders\s*\.\s*)?({VERBS}|multipart)\s*\("));
re!(mockmvc_request_re, r"(?:^|[^\w$.])(?:MockMvcRequestBuilders\s*\.\s*)?request\s*\(\s*HttpMethod\s*\.\s*([A-Z]+)\s*,");
re!(kotlin_mockmvc_re, &format!(r"\b\w*[mM]ockMvc\s*\.\s*({VERBS})\s*\("));
re!(web_test_client_re, &format!(r"\.\s*({VERBS})\s*\(\s*\)\s*\.\s*uri\s*\("));
re!(
    rest_template_re,
    r"\b\w*[Rr]estTemplate\s*\.\s*(getForEntity|getForObject|postForEntity|postForObject|postForLocation|put|delete|patchForObject|exchange)\s*\("
);
re!(rest_assured_re, &format!(r"(?:\.\s*when\s*\(\s*\)\s*|\bRestAssured\s*)\.\s*({VERBS})\s*\("));
re!(http_method_re, r"HttpMethod\s*\.\s*([A-Z]+)");

/// The path a request's first argument builds from `at` (just past its `(`): string literals
/// verbatim, each computed piece and each Spring `{var}` as a hole. `None` unless it starts with
/// a literal.
fn request_path(src: &str, at: usize) -> Option<String> {
    let b = src.as_bytes();
    let mut i = at;
    let skip = |i: &mut usize| {
        while *i < b.len() && b[*i].is_ascii_whitespace() {
            *i += 1;
        }
    };
    skip(&mut i);
    let mut out = String::new();
    loop {
        if b.get(i) == Some(&b'"') {
            let start = i + 1;
            let mut j = start;
            while j < b.len() && b[j] != b'"' && b[j] != b'\n' {
                j += if b[j] == b'\\' { 2 } else { 1 };
            }
            let lit = src.get(start..j.min(b.len()))?;
            // `{id}` placeholders and Kotlin `$id` / `${id}` templates are holes
            let mut depth = 0;
            for c in lit.chars() {
                match c {
                    '{' => {
                        depth += 1;
                        if depth == 1 {
                            out.push(HOLE);
                        }
                    }
                    '}' => depth -= 1,
                    '$' if depth == 0 => out.push(HOLE),
                    _ if depth == 0 && !(out.ends_with(HOLE) && (c.is_alphanumeric() || c == '_')) => out.push(c),
                    _ => {}
                }
            }
            i = j + 1;
        } else if out.is_empty() {
            return None;
        } else {
            // a computed operand
            let rest = &src[i..];
            let end = rest.find(['+', ',', ')']).unwrap_or(rest.len());
            if end == 0 {
                return None;
            }
            out.push(HOLE);
            i += end;
        }
        skip(&mut i);
        if b.get(i) == Some(&b'+') {
            i += 1;
            skip(&mut i);
            continue;
        }
        return Some(out);
    }
}

/// Requests Spring tests make, with the method and the path they name.
pub struct TestRequestScanner;

impl FileHook for TestRequestScanner {
    fn name(&self) -> &'static str {
        "test-request-sites"
    }
    fn applies(&self, lang: Lang, rel: &str) -> bool {
        matches!(lang, Lang::Java | Lang::Kotlin) && cg_scan::testpath::is_test_path(rel)
    }
    fn triggers(&self) -> &'static [&'static str] {
        &["ockMvc", "ebTestClient", "estTemplate", "RestAssured", "when("]
    }
    fn run(&self, ctx: &HookCtx<'_>, out: &mut HookOut) {
        if is_minified(ctx.src) || !gate_re().is_match(ctx.src) {
            return;
        }
        let safe = cg_extract::post::strip_js_comments(ctx.src);
        let lines = LineIndex::new(&safe);
        let mut add = |at: usize, arg: usize, method: &str, callee: &str| {
            let Some(path) = request_path(&safe, arg) else { return };
            let line = lines.line(at);
            let row = enclosing_row(ctx, line, FN_KINDS).unwrap_or(NONE);
            out.sites.push(SiteFact {
                pass: PASS_TEST_REQUEST,
                role: 0,
                key_kind: 0,
                key_a: method.to_ascii_uppercase(),
                key_b: path,
                node_row: row,
                line,
                payload: callee.to_owned(),
            });
        };
        if safe.contains("MockMvcRequestBuilders") {
            for c in mockmvc_builder_re().captures_iter(&safe) {
                let m = c.get(1).unwrap();
                let method = if &c[1] == "multipart" { "post" } else { &c[1] };
                add(m.start(), c.get(0).unwrap().end(), method, &c[1]);
            }
            for c in mockmvc_request_re().captures_iter(&safe) {
                add(c.get(0).unwrap().start(), c.get(0).unwrap().end(), &c[1], "request");
            }
        }
        for (re, callee) in [(kotlin_mockmvc_re(), "mockMvc."), (web_test_client_re(), "uri:"), (rest_assured_re(), "")] {
            for c in re.captures_iter(&safe) {
                add(c.get(0).unwrap().start(), c.get(0).unwrap().end(), &c[1], &format!("{callee}{}", &c[1]));
            }
        }
        for c in rest_template_re().captures_iter(&safe) {
            let name = &c[1];
            let open = c.get(0).unwrap().end() - 1;
            let method = match name {
                n if n.starts_with("get") => Some("GET".to_owned()),
                n if n.starts_with("post") => Some("POST".to_owned()),
                "put" => Some("PUT".to_owned()),
                "delete" => Some("DELETE".to_owned()),
                n if n.starts_with("patch") => Some("PATCH".to_owned()),
                _ => match_bracket(&safe, open).and_then(|close| http_method_re().captures(&safe[open..close]).map(|m| m[1].to_owned())),
            };
            if let Some(method) = method {
                add(c.get(0).unwrap().start(), open + 1, &method, &format!("restTemplate.{name}"));
            }
        }
    }
}

pub(crate) fn test_requests(e: &Engine<'_>, out: &mut Out) {
    let view = e.view;
    let mut sites = Vec::new();
    for f in view.files() {
        if matches!(view.file(f).lang(), Lang::Java | Lang::Kotlin) {
            sites.extend(view.sites_of(f).iter().filter(|s| s.pass == PASS_TEST_REQUEST).map(|s| (f, s)));
        }
    }
    if sites.is_empty() {
        return;
    }
    let routes = http_routes(e);
    if routes.is_empty() {
        return;
    }
    let mut seen = rustc_hash::FxHashSet::default();
    for (f, s) in sites {
        let from = if s.node.0 == NONE { view.file(f).rec.node_start } else { s.node };
        let Some((segs, suffix)) = client_path(view.text_str(StrId(s.key_b)), None) else { continue };
        let Some(route) = match_http(view.text_str(StrId(s.key_a)), &segs, suffix, &routes) else { continue };
        if !seen.insert((from, route.node)) {
            continue;
        }
        out.push(
            EdgeKind::Calls,
            from,
            route.node,
            s.line,
            "test-request",
            Some(format!("{}:{}", view.file(f).path(), s.line)),
            Some(("via", view.text_str(s.payload))),
            order_of("test-request"),
        );
    }
}

/// Kinds an `expect` / `actual` pair may straddle (`expect class` fulfilled by an `actual
/// typealias`).
fn kmp_kinds_compatible(a: NodeKind, b: NodeKind) -> bool {
    let typey = |k: NodeKind| matches!(k, NodeKind::Class | NodeKind::Interface | NodeKind::Struct | NodeKind::Enum | NodeKind::TypeAlias);
    a == b || (typey(a) && typey(b))
}

pub(crate) fn kotlin_expect_actual(e: &Engine<'_>, out: &mut Out) {
    let view = e.view;
    let marked = |n: NodeIdx, word: &str| view.node(n).decorators().iter().any(|d| d.split_whitespace().any(|w| w == word));
    for f in view.files() {
        if view.file(f).lang() != Lang::Kotlin {
            continue;
        }
        for act in view.nodes_in_file(f).map(NodeIdx) {
            if !marked(act, "actual") {
                continue;
            }
            let an = view.node(act);
            let mut decls: Vec<NodeIdx> = view
                .by_qname(an.qname())
                .filter(|c| {
                    let cn = view.node(*c);
                    *c != act
                        && cn.file() != an.file()
                        && view.file(cn.file()).lang() == Lang::Kotlin
                        && kmp_kinds_compatible(cn.kind(), an.kind())
                        && !marked(*c, "actual")
                })
                .collect();
            decls.sort_by_key(|n| (view.file(view.node(*n).file()).path(), view.node(*n).start_line()));
            for decl in decls.into_iter().take(MAX_ACTUALS) {
                out.push(
                    EdgeKind::Calls,
                    decl,
                    act,
                    view.node(decl).start_line(),
                    "kotlin-expect-actual",
                    Some(format!("{}:{}", view.file(f).path(), an.start_line())),
                    Some(("via", an.name())),
                    order_of("kotlin-expect-actual"),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::request_path;
    use crate::frameworks::href::HOLE;

    #[test]
    fn request_paths() {
        assert_eq!(request_path("\"/owners/new\")", 0).as_deref(), Some("/owners/new"));
        assert_eq!(request_path(" \"/owners/{id}/edit\", 1)", 0), Some(format!("/owners/{HOLE}/edit")));
        assert_eq!(request_path("\"/owners/\" + id)", 0), Some(format!("/owners/{HOLE}")));
        assert_eq!(request_path("\"/pets/$id\")", 0), Some(format!("/pets/{HOLE}")));
        assert_eq!(request_path("url)", 0), None);
    }
}
