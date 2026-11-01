//! Gin's middleware chain (upstream `ginMiddlewareChainEdges`). Gin runs every handler through
//! one computed call in `(*Context).Next` (`c.handlers[c.index](c)`), which names nothing, so a
//! flow from `ServeHTTP` dead-ends there. The dispatcher - a Go method whose body calls a
//! `handlers` slice by index - links to every named handler a registration call (`.Use`, `.GET`,
//! ..., `.Handle`) passes: `gin.Logger()` is `Logger`, a closure is anonymous and skipped. A
//! deliberate, capped over-approximation (any registered handler can run for some route), gated
//! on the dispatcher existing. A handler name must reach one function: the only one in the
//! project, or the only one in the registering file's directory.

use super::scan::{enclosing_row, is_minified, LineIndex};
use super::sites::PASS_GIN;
use super::{order_of, Out};
use crate::engine::Engine;
use crate::frameworks::href::match_bracket;
use crate::project::parent_dir;
use cg_core::{EdgeKind, Lang, NodeIdx, NodeKind, StrId, NONE};
use cg_extract::{FileHook, HookCtx, HookOut, SiteFact};
use regex::Regex;
use std::sync::OnceLock;

pub const ROLE_GIN_DISPATCHER: u8 = 0;
pub const ROLE_GIN_HANDLER: u8 = 1;

const MAX_HANDLERS: usize = 40;

fn dispatch_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\.handlers\s*\[[^\]]*\]\s*\(").expect("static regex"))
}

fn registration_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\.(?:Use|GET|POST|PUT|PATCH|DELETE|OPTIONS|HEAD|Any|Handle)\s*\(").expect("static regex"))
}

/// Top-level comma-separated items of a call's arguments.
fn split_args(args: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut start) = (0i32, 0);
    for (i, c) in args.char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                out.push(&args[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if !args[start..].trim().is_empty() {
        out.push(&args[start..]);
    }
    out
}

/// The name a handler argument ends in: `gin.Logger()` -> `Logger`, `mw` -> `mw`; nothing for a
/// path string or an inline closure.
fn handler_ident(arg: &str) -> Option<&str> {
    let a = arg.trim();
    let a = a.strip_suffix("()").map(str::trim_end).unwrap_or(a);
    if a.is_empty() || a.starts_with(['"', '`']) || a.starts_with("func") {
        return None;
    }
    let tail = a.rsplit('.').next().unwrap_or(a);
    let ok = tail.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_') && tail.chars().all(|c| c.is_alphanumeric() || c == '_');
    ok.then_some(tail)
}

/// Gin's dispatcher methods and the handlers registration calls pass.
pub struct GinScanner;

impl FileHook for GinScanner {
    fn name(&self) -> &'static str {
        "gin-sites"
    }
    fn applies(&self, lang: Lang, _rel: &str) -> bool {
        lang == Lang::Go
    }
    fn triggers(&self) -> &'static [&'static str] {
        &[".handlers", ".Use(", ".GET(", ".POST(", ".PUT(", ".PATCH(", ".DELETE(", ".OPTIONS(", ".HEAD(", ".Any(", ".Handle("]
    }
    fn run(&self, ctx: &HookCtx<'_>, out: &mut HookOut) {
        if is_minified(ctx.src) {
            return;
        }
        let safe = cg_extract::post::strip_js_comments(ctx.src);
        let lines = LineIndex::new(&safe);
        let site = |role: u8, key_a: &str, node_row: u32, line: u32| SiteFact {
            pass: PASS_GIN,
            role,
            key_kind: 0,
            key_a: key_a.to_owned(),
            key_b: String::new(),
            node_row,
            line,
            payload: String::new(),
        };
        if safe.contains(".handlers") {
            for (row, n) in ctx.rows.nodes().enumerate() {
                if n.kind() == NodeKind::Method && dispatch_re().is_match(lines.lines(n.start_line(), n.end_line())) {
                    out.sites.push(site(ROLE_GIN_DISPATCHER, "", row as u32, n.start_line()));
                }
            }
        }
        for m in registration_re().find_iter(&safe) {
            let open = m.end() - 1;
            let Some(close) = match_bracket(&safe, open) else { continue };
            let line = lines.line(m.start());
            let row = enclosing_row(ctx, line, super::scan::FN_KINDS).unwrap_or(NONE);
            for arg in split_args(&safe[open + 1..close]) {
                if let Some(name) = handler_ident(arg) {
                    out.sites.push(site(ROLE_GIN_HANDLER, name, row, line));
                }
            }
        }
    }
}

pub(crate) fn middleware_chain(e: &Engine<'_>, out: &mut Out) {
    let view = e.view;
    let mut dispatchers: Vec<NodeIdx> = Vec::new();
    // (name, file path, line), first registration per name in path order
    let mut registered: Vec<(&str, &str, u32)> = Vec::new();
    for f in view.files() {
        if view.file(f).lang() != Lang::Go {
            continue;
        }
        let path = view.file(f).path();
        for s in view.sites_of(f).iter().filter(|s| s.pass == PASS_GIN) {
            match s.role {
                ROLE_GIN_DISPATCHER if s.node.0 != NONE => dispatchers.push(s.node),
                ROLE_GIN_HANDLER => registered.push((view.text_str(StrId(s.key_a)), path, s.line)),
                _ => {}
            }
        }
    }
    if dispatchers.is_empty() || registered.is_empty() {
        return;
    }
    registered.sort_by(|a, b| (a.1, a.2, a.0).cmp(&(b.1, b.2, b.0)));
    let mut seen = rustc_hash::FxHashSet::default();
    registered.retain(|r| seen.insert(r.0));
    let stable = |n: &NodeIdx| (view.file(view.node(*n).file()).path(), view.node(*n).start_line(), *n);
    dispatchers.sort_by_key(stable);
    dispatchers.dedup();
    // the one function a name reaches: unique in the project, else in the registering directory
    let handler = |name: &str, at: &str| -> Option<NodeIdx> {
        let cands: Vec<NodeIdx> = view
            .by_name(name)
            .filter(|n| matches!(view.node(*n).kind(), NodeKind::Function | NodeKind::Method) && view.file(view.node(*n).file()).lang() == Lang::Go)
            .collect();
        if cands.len() == 1 {
            return Some(cands[0]);
        }
        let dir = parent_dir(at);
        let mut local = cands.into_iter().filter(|n| parent_dir(view.file(view.node(*n).file()).path()) == dir);
        let first = local.next()?;
        local.next().is_none().then_some(first)
    };
    for disp in dispatchers {
        let mut added = 0;
        for &(name, path, line) in &registered {
            if added >= MAX_HANDLERS {
                break;
            }
            let Some(h) = handler(name, path) else { continue };
            if h == disp {
                continue;
            }
            out.push(
                EdgeKind::Calls,
                disp,
                h,
                view.node(disp).start_line(),
                "gin-middleware-chain",
                Some(format!("{path}:{line}")),
                Some(("via", name)),
                order_of("gin-middleware-chain"),
            );
            added += 1;
        }
    }
}
