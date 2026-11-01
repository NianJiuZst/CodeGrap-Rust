//! Calls that cross from JavaScript into another language of the same app.
//!
//! * `tauri-command` (upstream `tauri-synthesizer.ts`): the page calls `invoke('save_file', ...)`
//!   and the Rust `#[tauri::command] fn save_file` answers. A command answers only when a handler
//!   list registers it (`generate_handler![...]`, tauri-specta's `collect_commands![...]`), and
//!   names are looked up per app (the directory holding `src-tauri/`): a monorepo's two `greet`s
//!   stay apart, and a tie is nothing.
//! * `rn-native-module` (upstream `rnXPlatEdges`, the Android half): `NativeModules.Calendar
//!   .createEvent(...)` (or a module destructured from `NativeModules`) reaches the
//!   `@ReactMethod createEvent` of the Java / Kotlin module whose `getName()` returns `Calendar`.
//!
//! Event channels across the bridges and the iOS side are not read.

use super::scan::{enclosing_row, is_minified, LineIndex, FN_KINDS};
use super::sites::PASS_BRIDGE;
use super::{order_of, Out};
use crate::engine::Engine;
use cg_core::{EdgeKind, FileId, Lang, LangFamily, NodeIdx, NodeKind, StrId, NONE};
use cg_extract::{FileHook, HookCtx, HookOut, SiteFact};
use regex::Regex;
use rustc_hash::FxHashMap;
use std::sync::OnceLock;

pub const ROLE_TAURI_COMMAND: u8 = 0;
pub const ROLE_TAURI_REGISTERED: u8 = 1;
pub const ROLE_TAURI_INVOKE: u8 = 2;
pub const ROLE_RN_METHOD: u8 = 3;
pub const ROLE_RN_CALL: u8 = 4;

macro_rules! re {
    ($name:ident, $pat:expr) => {
        fn $name() -> &'static Regex {
            static R: OnceLock<Regex> = OnceLock::new();
            R.get_or_init(|| Regex::new($pat).expect("static regex"))
        }
    };
}

re!(tauri_command_re, r"#\[\s*(?:tauri\s*::\s*)?command\b[^\]]*\]\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\s*\([^)]*\))?\s+)?(?:async\s+)?fn\s+([A-Za-z_]\w*)");
re!(tauri_handlers_re, r"\b(?:generate_handler|collect_commands)\s*!\s*\[([^\]]*)\]");
re!(invoke_re, r#"(?:^|[^\w$.])(?:[\w$]+\s*\.\s*)*invoke\s*(?:<[^>()]*>)?\s*\(\s*(['"`])([A-Za-z_][\w]*)['"`]"#);
re!(rn_get_name_re, r#"\bgetName\s*\(\s*\)[^{=]*(?:\{\s*return\s+|=\s*)"([A-Za-z_][\w]*)""#);
re!(
    rn_method_re,
    r"@ReactMethod(?:\s*\([^)]*\))?\s*(?:(?:public|private|protected|internal|override|suspend|final)\s+)*(?:fun\s+|[\w<>\[\],\s]+?\s+)([A-Za-z_]\w*)\s*\("
);
re!(rn_call_re, r"\bNativeModules\s*\.\s*([A-Za-z_$][\w$]*)\s*\.\s*([A-Za-z_$][\w$]*)\s*\(");
re!(rn_destructure_re, r"\{([^{}]*)\}\s*=\s*NativeModules\b");

fn site(role: u8, key_a: &str, key_b: &str, node_row: u32, line: u32) -> SiteFact {
    SiteFact { pass: PASS_BRIDGE, role, key_kind: 0, key_a: key_a.to_owned(), key_b: key_b.to_owned(), node_row, line, payload: String::new() }
}

/// The function node called `name` declared on `line` or just below it.
fn fn_row_at(ctx: &HookCtx<'_>, name: &str, line: u32) -> Option<u32> {
    ctx.rows
        .nodes()
        .enumerate()
        .find(|(_, n)| matches!(n.kind(), NodeKind::Function | NodeKind::Method) && n.name() == name && n.start_line() >= line && n.start_line() <= line + 3)
        .map(|(i, _)| i as u32)
}

pub struct BridgeScanner;

impl FileHook for BridgeScanner {
    fn name(&self) -> &'static str {
        "bridge-sites"
    }
    fn applies(&self, lang: Lang, _rel: &str) -> bool {
        matches!(lang.family(), LangFamily::Js | LangFamily::Rust | LangFamily::Jvm)
    }
    fn triggers(&self) -> &'static [&'static str] {
        &["command", "generate_handler", "collect_commands", "invoke", "ReactMethod", "NativeModules"]
    }
    fn run(&self, ctx: &HookCtx<'_>, out: &mut HookOut) {
        if is_minified(ctx.src) {
            return;
        }
        let safe = cg_extract::post::strip_js_comments(ctx.src);
        let lines = LineIndex::new(&safe);
        match ctx.lang.family() {
            LangFamily::Rust => {
                if safe.contains("command") && safe.contains("tauri") {
                    for c in tauri_command_re().captures_iter(&safe) {
                        let name = c.get(1).unwrap();
                        let line = lines.line(name.start());
                        if let Some(row) = fn_row_at(ctx, name.as_str(), line.saturating_sub(1)) {
                            out.sites.push(site(ROLE_TAURI_COMMAND, name.as_str(), "", row, line));
                        }
                    }
                }
                for c in tauri_handlers_re().captures_iter(&safe) {
                    let line = lines.line(c.get(0).unwrap().start());
                    for item in c[1].split(',') {
                        let name = item.trim().rsplit("::").next().unwrap_or("").trim();
                        if !name.is_empty() {
                            out.sites.push(site(ROLE_TAURI_REGISTERED, name, "", NONE, line));
                        }
                    }
                }
            }
            LangFamily::Jvm => {
                let Some(module) = rn_get_name_re().captures(&safe).map(|c| c[1].to_owned()) else { return };
                for c in rn_method_re().captures_iter(&safe) {
                    let name = c.get(1).unwrap();
                    let line = lines.line(name.start());
                    if let Some(row) = fn_row_at(ctx, name.as_str(), line.saturating_sub(1)) {
                        out.sites.push(site(ROLE_RN_METHOD, &module, name.as_str(), row, line));
                    }
                }
            }
            _ => {
                for c in invoke_re().captures_iter(&safe) {
                    let at = c.get(2).unwrap().start();
                    let line = lines.line(at);
                    let row = enclosing_row(ctx, line, FN_KINDS).unwrap_or(NONE);
                    out.sites.push(site(ROLE_TAURI_INVOKE, &c[2], "", row, line));
                }
                for c in rn_call_re().captures_iter(&safe) {
                    let line = lines.line(c.get(0).unwrap().start());
                    let row = enclosing_row(ctx, line, FN_KINDS).unwrap_or(NONE);
                    out.sites.push(site(ROLE_RN_CALL, &c[1], &c[2], row, line));
                }
                // `const { Calendar } = NativeModules;` then `Calendar.createEvent(...)`
                for d in rn_destructure_re().captures_iter(&safe) {
                    for module in d[1].split(',').filter_map(|m| m.split(':').next_back()).map(str::trim).filter(|m| !m.is_empty()) {
                        let pat = format!(r"\b{}\s*\.\s*([A-Za-z_$][\w$]*)\s*\(", regex::escape(module));
                        let Ok(re) = Regex::new(&pat) else { continue };
                        for c in re.captures_iter(&safe) {
                            let line = lines.line(c.get(0).unwrap().start());
                            let row = enclosing_row(ctx, line, FN_KINDS).unwrap_or(NONE);
                            out.sites.push(site(ROLE_RN_CALL, module, &c[1], row, line));
                        }
                    }
                }
            }
        }
    }
}

/// The Tauri app a file belongs to: the directory holding the `src-tauri/` its path runs
/// through, or the longest such directory above a frontend file. `""` with no `src-tauri/`.
fn app_of<'a>(path: &str, roots: &[&'a str]) -> &'a str {
    if let Some(i) = path.find("src-tauri/") {
        if let Some(r) = roots.iter().find(|r| **r == &path[..i]) {
            return r;
        }
    }
    roots.iter().filter(|r| path.starts_with(**r)).max_by_key(|r| r.len()).copied().unwrap_or("")
}

pub(crate) fn link(e: &Engine<'_>, out: &mut Out) {
    let view = e.view;
    let mut commands: Vec<(FileId, NodeIdx, &str)> = Vec::new();
    let mut registered: Vec<(FileId, &str)> = Vec::new();
    let mut invokes: Vec<(FileId, NodeIdx, u32, &str)> = Vec::new();
    let mut rn_methods: FxHashMap<(&str, &str), Vec<NodeIdx>> = FxHashMap::default();
    let mut rn_calls: Vec<(FileId, NodeIdx, u32, &str, &str)> = Vec::new();
    for f in view.files() {
        let file_node = view.file(f).rec.node_start;
        for s in view.sites_of(f).iter().filter(|s| s.pass == PASS_BRIDGE) {
            let (a, b) = (view.text_str(StrId(s.key_a)), view.text_str(StrId(s.key_b)));
            let from = if s.node.0 == NONE { file_node } else { s.node };
            match s.role {
                ROLE_TAURI_COMMAND => commands.push((f, s.node, a)),
                ROLE_TAURI_REGISTERED => registered.push((f, a)),
                ROLE_TAURI_INVOKE => invokes.push((f, from, s.line, a)),
                ROLE_RN_METHOD => rn_methods.entry((a, b)).or_default().push(s.node),
                ROLE_RN_CALL => rn_calls.push((f, from, s.line, a, b)),
                _ => {}
            }
        }
    }
    if !invokes.is_empty() && !commands.is_empty() {
        let mut roots: Vec<&str> = commands.iter().filter_map(|(f, ..)| view.file(*f).path().find("src-tauri/").map(|i| &view.file(*f).path()[..i])).collect();
        roots.sort_unstable();
        roots.dedup();
        let app = |f: FileId| app_of(view.file(f).path(), &roots);
        let mut answers: FxHashMap<(&str, &str), Vec<NodeIdx>> = FxHashMap::default();
        for &(f, node, name) in &commands {
            if registered.iter().any(|(rf, r)| *r == name && app(*rf) == app(f)) {
                answers.entry((app(f), name)).or_default().push(node);
            }
        }
        for (f, from, line, name) in invokes {
            let Some([target]) = answers.get(&(app(f), name)).map(Vec::as_slice) else { continue };
            out.push(
                EdgeKind::Calls,
                from,
                *target,
                line,
                "tauri-command",
                Some(format!("{}:{}", view.file(f).path(), line)),
                Some(("via", name)),
                order_of("tauri-command"),
            );
        }
    }
    for (f, from, line, module, method) in rn_calls {
        let Some([target]) = rn_methods.get(&(module, method)).map(Vec::as_slice) else { continue };
        out.push(
            EdgeKind::Calls,
            from,
            *target,
            line,
            "rn-native-module",
            Some(format!("{}:{}", view.file(f).path(), line)),
            Some(("via", &format!("{module}.{method}"))),
            order_of("rn-native-module"),
        );
    }
}
