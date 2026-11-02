//! Redux Toolkit chains the graph cannot see.
//!
//! * `redux-thunk` (upstream `reduxThunkEdges`): `export const X = createAsyncThunk(prefix, async
//!   (a, api) => { ... dispatch(next(...)) })` passes its body as an argument, so the calls that
//!   drive a thunk chain forward hang off no function. Each thunk constant links to what its body
//!   dispatches: the name as the file binds it (an import or a declaration of its own), else a
//!   thunk constant, then any constant, of that name when only one fits.
//! * `rtk-query` (upstream `rtkQueryEdges`): a generated hook (`useGetPostsQuery`, extracted with
//!   a sentinel signature from `export const { ... } = api`) runs the endpoint its name derives
//!   from (`getPosts`), declared in the same file.

use super::scan::{is_minified, LineIndex};
use super::sites::PASS_THUNK;
use super::{order_of, Out};
use crate::engine::Engine;
use crate::resolve::{Accept, NameRes, WorkerCx};
use cg_core::{EdgeKind, Lang, LangFamily, NodeIdx, NodeKind, StrId, NONE};
use cg_extract::{FileHook, HookCtx, HookOut, SiteFact};
use regex::Regex;
use std::sync::OnceLock;

const THUNK_FANOUT_CAP: usize = 24;
/// The signature the kernel gives a generated RTK Query hook.
const RTK_GENERATED_HOOK: &str = "= RTK Query generated hook";

fn thunk_decl_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"create(?:Async)?Thunk").expect("static regex"))
}

fn dispatch_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\bdispatch\s*\(\s*([A-Za-z_]\w*)\s*[(),]").expect("static regex"))
}

/// The names a thunk constant's body dispatches.
pub struct ThunkScanner;

impl FileHook for ThunkScanner {
    fn name(&self) -> &'static str {
        "redux-thunk-sites"
    }
    fn applies(&self, lang: Lang, _rel: &str) -> bool {
        lang.family() == LangFamily::Js
    }
    fn triggers(&self) -> &'static [&'static str] {
        &["Thunk"]
    }
    fn run(&self, ctx: &HookCtx<'_>, out: &mut HookOut) {
        if is_minified(ctx.src) {
            return;
        }
        let safe = cg_extract::post::strip_js_comments(ctx.src);
        let lines = LineIndex::new(&safe);
        for (row, n) in ctx.rows.nodes().enumerate() {
            if n.kind() != NodeKind::Constant || !n.signature().is_some_and(|s| thunk_decl_re().is_match(s)) {
                continue;
            }
            let body = lines.lines(n.start_line(), n.end_line());
            for (i, m) in dispatch_re().captures_iter(body).enumerate() {
                if i >= THUNK_FANOUT_CAP * 4 {
                    break;
                }
                let name = &m[1];
                if name == n.name() {
                    continue; // a recursive thunk
                }
                let line = n.start_line() + body[..m.get(0).unwrap().start()].matches('\n').count() as u32;
                out.sites.push(SiteFact {
                    pass: PASS_THUNK,
                    role: 0,
                    key_kind: 0,
                    key_a: name.to_owned(),
                    key_b: String::new(),
                    node_row: row as u32,
                    line,
                    payload: String::new(),
                });
            }
        }
    }
}

pub(crate) fn thunks<'e, 'v>(e: &'e Engine<'v>, out: &mut Out, wcx: &mut WorkerCx<'e, 'v>) {
    let view = e.view;
    let is_thunk = |n: NodeIdx| view.node(n).kind() == NodeKind::Constant && view.node(n).signature().is_some_and(|s| thunk_decl_re().is_match(s));
    for f in view.files() {
        let sites: Vec<_> = view.sites_of(f).iter().filter(|s| s.pass == PASS_THUNK && s.node.0 != NONE).collect();
        if sites.is_empty() {
            continue;
        }
        let cx = wcx.file_cx(e, f);
        let mut added: rustc_hash::FxHashMap<NodeIdx, usize> = rustc_hash::FxHashMap::default();
        let mut seen = rustc_hash::FxHashSet::default();
        for s in sites {
            let name = view.text_str(StrId(s.key_a));
            let count = added.entry(s.node).or_default();
            if *count >= THUNK_FANOUT_CAP {
                continue;
            }
            // what the file binds the name to, else the one thunk / constant of that name
            let target = match cx.resolve_name(wcx, name, Accept::Callable, 0, u32::MAX, None, false) {
                NameRes::Found(n, ..) => Some(n),
                _ => {
                    let cands: Vec<NodeIdx> = view
                        .by_name(name)
                        .filter(|n| matches!(view.node(*n).kind(), NodeKind::Constant | NodeKind::Function | NodeKind::Method))
                        .filter(|n| view.file(view.node(*n).file()).lang().family() == LangFamily::Js)
                        .collect();
                    let thunks: Vec<NodeIdx> = cands.iter().copied().filter(|n| is_thunk(*n)).collect();
                    let consts: Vec<NodeIdx> = cands.iter().copied().filter(|n| view.node(*n).kind() == NodeKind::Constant).collect();
                    match (thunks.as_slice(), consts.as_slice(), cands.as_slice()) {
                        ([one], ..) | ([], [one], _) | ([], [], [one]) => Some(*one),
                        _ => None,
                    }
                }
            };
            let Some(target) = target.filter(|t| *t != s.node && matches!(view.node(*t).kind(), NodeKind::Constant | NodeKind::Function | NodeKind::Method))
            else {
                continue;
            };
            if !seen.insert((s.node, target)) {
                continue;
            }
            *count += 1;
            let meta = serde_json::json!({
                "synthesizedBy": "redux-thunk",
                "via": name,
                "registeredAt": format!("{}:{}", view.file(f).path(), s.line),
            });
            out.push_meta(EdgeKind::Calls, s.node, target, s.line, NONE, meta, order_of("redux-thunk"));
        }
    }
}

/// `useLazyGetRecordsQuery` -> `getRecords`.
fn endpoint_of(hook: &str) -> Option<String> {
    let mid = hook.strip_prefix("use")?;
    let mid = mid.strip_suffix("Query").or_else(|| mid.strip_suffix("Mutation"))?;
    let mid = mid.strip_prefix("Lazy").unwrap_or(mid);
    let mut chars = mid.chars();
    let first = chars.next().filter(|c| c.is_ascii_uppercase())?;
    Some(format!("{}{}", first.to_ascii_lowercase(), chars.as_str()))
}

pub(crate) fn rtk_query(e: &Engine<'_>, out: &mut Out) {
    let view = e.view;
    for f in view.files() {
        if view.file(f).lang().family() != LangFamily::Js {
            continue;
        }
        let nodes: Vec<NodeIdx> = view.nodes_in_file(f).map(NodeIdx).collect();
        for &hook in &nodes {
            let node = view.node(hook);
            if node.kind() != NodeKind::Function || node.signature() != Some(RTK_GENERATED_HOOK) {
                continue;
            }
            let Some(endpoint) = endpoint_of(node.name()) else { continue };
            let Some(&target) = nodes.iter().find(|n| **n != hook && view.node(**n).kind() == NodeKind::Function && view.node(**n).name() == endpoint) else {
                continue;
            };
            out.push(
                EdgeKind::Calls,
                hook,
                target,
                node.start_line(),
                "rtk-query",
                Some(format!("{}:{}", view.file(f).path(), node.start_line())),
                Some(("via", &endpoint)),
                order_of("rtk-query"),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn endpoint_names() {
        assert_eq!(super::endpoint_of("useGetPostsQuery").as_deref(), Some("getPosts"));
        assert_eq!(super::endpoint_of("useLazyGetRecordsQuery").as_deref(), Some("getRecords"));
        assert_eq!(super::endpoint_of("useUpdateUserMutation").as_deref(), Some("updateUser"));
        assert_eq!(super::endpoint_of("useQuery"), None);
        assert_eq!(super::endpoint_of("useThing"), None);
    }
}
