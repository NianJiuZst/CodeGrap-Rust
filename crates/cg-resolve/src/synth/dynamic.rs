//! Dynamic dispatch and cross-boundary channels (upstream `callback-synthesizer.ts`,
//! `tier-synthesizer.ts`): calls that only exist at run time, closed by the string or field
//! both sides name.
//!
//! * `callback` - a registrar (`onChange(cb)`) stores the callback in a field
//!   (`this.listeners.push(cb)`) that a dispatcher of the same file iterates
//!   (`this.listeners.forEach(..)`): the dispatcher calls every named function registered.
//! * `object-registry` - `handlers[action](..)` dispatches through an object literal of
//!   functions (`const handlers = { create: createUser, remove: removeUser }`).
//! * `http-client` - `fetch('/api/users')`, `api.post('/users')` reaches the one route that
//!   serves the method and path.
//! * `queue-job` - `queue.add('welcome')` reaches the consumer of that job / queue.
//! * `event-bus` - `eventEmitter.emit('user.created')` reaches `@OnEvent('user.created')`, and
//!   socket events cross between client and server.
//! * `celery-dispatch` - `task.delay()` reaches the `@shared_task` function.
//! * `spring-event` - `publishEvent(new E(..))` reaches every `@EventListener` of `E`.
//!
//! Extraction records the sites while the source is in memory; linking reads only sites and
//! the view. Every pass is capped per key, and an ambiguous name links nothing.

use super::scan::{call_args, enclosing_row, is_minified, mid_chain, LineIndex, FN_KINDS};
use super::sites::{PASS_BUS, PASS_CALLBACK, PASS_CELERY, PASS_HTTP, PASS_MEDIATR, PASS_QUEUE, PASS_REGISTRY, PASS_SPRING};
use super::{order_of, Out};
use crate::engine::{Bound, Engine};
use crate::frameworks::href::{index_at_depth0, match_bracket, read_string_at, HOLE};
use crate::resolve::{Accept, FileCx, NameRes, WorkerCx};
use cg_core::{EdgeKind, FileId, Lang, LangFamily, NodeIdx, NodeKind, RefKind, StrId, NONE};
use cg_extract::{FileHook, HookCtx, HookOut, SiteFact};
use cg_store::{status, View};
use regex::Regex;
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::OnceLock;

macro_rules! re {
    ($name:ident, $pat:expr) => {
        fn $name() -> &'static Regex {
            static R: OnceLock<Regex> = OnceLock::new();
            R.get_or_init(|| Regex::new($pat).expect("static regex"))
        }
    };
}

const MAX_CALLBACKS_PER_CHANNEL: usize = 40;
const REGISTRY_FANOUT_CAP: usize = 40;
const EVENT_FANOUT_CAP: usize = 6;
const CELERY_FANOUT_CAP: usize = 80;
const SPRING_FANOUT_CAP: usize = 80;
const MEDIATR_FANOUT_CAP: usize = 80;

fn site(pass: u16, role: u8, key_a: impl Into<String>, key_b: impl Into<String>, node_row: Option<u32>, line: u32, payload: impl Into<String>) -> SiteFact {
    SiteFact { pass, role, key_kind: 0, key_a: key_a.into(), key_b: key_b.into(), node_row: node_row.unwrap_or(NONE), line, payload: payload.into() }
}

fn text(view: &View, id: u32) -> &str {
    view.text_str(StrId(id))
}

fn registered(view: &View, f: FileId, line: u32) -> String {
    format!("{}:{}", view.file(f).path(), line)
}

/// Whether the site's node is the file itself (no function encloses it).
fn is_file_node(view: &View, n: NodeIdx) -> bool {
    view.node(n).kind() == NodeKind::File
}

// =================================================================== callback channels

pub const ROLE_REGISTRAR: u8 = 0;
pub const ROLE_DISPATCHER: u8 = 1;
pub const ROLE_REGISTRATION: u8 = 2;

re!(registrar_name_re, r"^(?:on[A-Z]\w*|subscribe|addListener|addEventListener|register|watch|listen|addCallback)$");
re!(dispatcher_name_re, r"(?i)(?:emit|trigger|notify|dispatch|fire|publish|flush)");
re!(registrar_field_re, r"this\.(\w+)\.(?:add|push|set)\(");
re!(dispatch_for_of_re, r"\bof\s+(?:Array\.from\(\s*)?this\.(\w+)");
re!(dispatch_call_re, r"\b\w+\s*\(");
re!(dispatch_for_each_re, r"this\.(\w+)\.forEach\(");
re!(registration_call_re, r"\.\s*(on[A-Z]\w*|subscribe|addListener|addEventListener|register|watch|listen|addCallback)\s*\(\s*(this\.\w+|\w+)\s*[,)]");

/// Registrars, dispatchers and the calls that register a named callback.
pub struct CallbackScanner;

impl FileHook for CallbackScanner {
    fn name(&self) -> &'static str {
        "callback-sites"
    }
    fn applies(&self, lang: Lang, _rel: &str) -> bool {
        lang.family() == LangFamily::Js || matches!(lang, Lang::Java | Lang::Kotlin)
    }
    fn triggers(&self) -> &'static [&'static str] {
        &["this.", "on", "subscribe", "register", "watch", "listen"]
    }
    fn run(&self, ctx: &HookCtx<'_>, out: &mut HookOut) {
        if is_minified(ctx.src) {
            return;
        }
        let lines = LineIndex::new(ctx.src);
        if ctx.src.contains("this.") {
            for (row, n) in ctx.rows.nodes().enumerate() {
                if !matches!(n.kind(), NodeKind::Method | NodeKind::Function) {
                    continue;
                }
                let is_reg = registrar_name_re().is_match(n.name());
                let is_disp = dispatcher_name_re().is_match(n.name());
                if !is_reg && !is_disp {
                    continue;
                }
                let body = lines.lines(n.start_line(), n.end_line());
                if is_reg {
                    if let Some(c) = registrar_field_re().captures(body) {
                        out.sites.push(site(PASS_CALLBACK, ROLE_REGISTRAR, &c[1], "", Some(row as u32), n.start_line(), ""));
                    }
                }
                if is_disp {
                    let field = match dispatch_for_of_re().captures(body) {
                        Some(c) if dispatch_call_re().is_match(body) => Some(c[1].to_owned()),
                        _ => dispatch_for_each_re().captures(body).map(|c| c[1].to_owned()),
                    };
                    if let Some(field) = field {
                        out.sites.push(site(PASS_CALLBACK, ROLE_DISPATCHER, field, "", Some(row as u32), n.start_line(), ""));
                    }
                }
            }
        }
        for c in registration_call_re().captures_iter(ctx.src) {
            let at = c.get(0).unwrap().start();
            let line = lines.line(at);
            let Some(row) = enclosing_row(ctx, line, FN_KINDS) else { continue };
            out.sites.push(site(PASS_CALLBACK, ROLE_REGISTRATION, &c[1], "", Some(row), line, &c[2]));
        }
    }
}

/// The resolved target of the reference of `file` at `line` whose name ends in `tail` and
/// whose kind is `kind`, resolved on the spot (synthesis runs before resolution is stored).
fn resolved_at<'e, 'v>(
    cx: &FileCx<'e, 'v>,
    wcx: &mut WorkerCx<'e, 'v>,
    view: &'v View,
    from: NodeIdx,
    line: u32,
    tail: &str,
    want: &[RefKind],
) -> Option<NodeIdx> {
    let mut hit = None;
    for r in view.refs_of_file(cx.file) {
        if r.line != line || r.from != from {
            continue;
        }
        let Some(kind) = RefKind::from_wire(r.kind) else { continue };
        if !want.contains(&kind) {
            continue;
        }
        let name = view.text_str(r.name);
        if name != tail && !name.ends_with(&format!(".{tail}")) {
            continue;
        }
        let o = cx.resolve_ref(wcx, r);
        if o.status != status::RESOLVED {
            continue;
        }
        if hit.is_some_and(|h| h != NodeIdx(o.target)) {
            return None;
        }
        hit = Some(NodeIdx(o.target));
    }
    hit
}

pub(crate) fn callbacks<'e, 'v>(e: &'e Engine<'v>, out: &mut Out, wcx: &mut WorkerCx<'e, 'v>) {
    let view = e.view;
    // (file, field) -> dispatchers; registrar node -> field
    let mut dispatchers: FxHashMap<(FileId, &str), Vec<NodeIdx>> = FxHashMap::default();
    let mut registrars: FxHashMap<NodeIdx, &str> = FxHashMap::default();
    let mut registrations: Vec<(FileId, NodeIdx, u32, &str, &str)> = Vec::new();
    for f in view.files() {
        for s in view.sites_of(f).iter().filter(|s| s.pass == PASS_CALLBACK) {
            match s.role {
                ROLE_REGISTRAR => {
                    registrars.insert(s.node, text(view, s.key_a));
                }
                ROLE_DISPATCHER => dispatchers.entry((f, text(view, s.key_a))).or_default().push(s.node),
                _ => registrations.push((f, s.node, s.line, text(view, s.key_a), view.text_str(s.payload))),
            }
        }
    }
    if registrars.is_empty() || dispatchers.is_empty() {
        return;
    }
    let mut added: FxHashMap<NodeIdx, usize> = FxHashMap::default();
    for (f, caller, line, reg_name, arg) in registrations {
        let cx = wcx.file_cx(e, f);
        // the call must reach a registrar whose field a dispatcher of its file iterates
        let Some(reg) = resolved_at(&cx, wcx, view, caller, line, reg_name, &[RefKind::Edge(EdgeKind::Calls)]) else { continue };
        let Some(field) = registrars.get(&reg) else { continue };
        let Some(disps) = dispatchers.get(&(view.hot(reg).file, *field)) else { continue };
        // the callback: the function value passed at that line
        let bare = arg.strip_prefix("this.").unwrap_or(arg);
        let Some(callback) = resolved_at(&cx, wcx, view, caller, line, bare, &[RefKind::FunctionRef]) else { continue };
        if !matches!(view.node(callback).kind(), NodeKind::Method | NodeKind::Function) {
            continue;
        }
        if !arg.starts_with("this.") && view.hot(callback).file != f && !imported_into(e, f, callback) {
            continue;
        }
        for &d in disps {
            let n = added.entry(reg).or_default();
            if *n >= MAX_CALLBACKS_PER_CHANNEL || d == callback {
                continue;
            }
            *n += 1;
            let meta =
                serde_json::json!({ "synthesizedBy": "callback", "via": view.node(reg).name(), "field": field, "registeredAt": registered(view, f, line) });
            out.push_meta(EdgeKind::Calls, d, callback, view.node(d).start_line(), NONE, meta, order_of("callback"));
        }
    }
}

/// Whether `f` imports the declaration `n` (or the module that declares it).
fn imported_into(e: &Engine<'_>, f: FileId, n: NodeIdx) -> bool {
    let target = e.view.hot(n).file;
    (0..e.view.imports_of(f).len()).any(|i| match e.bind_import(f, i) {
        Bound::Node(b) => b == n,
        Bound::Module(m) => m == target,
        _ => false,
    })
}

// =================================================================== object registries

pub const ROLE_REGISTRY: u8 = 0;
pub const ROLE_REGISTRY_DISPATCH: u8 = 1;

re!(registry_dispatch_re, r"(?:\bnew\s+)?((?:this\.)?[A-Za-z_$][\w$]*)\s*\[\s*([A-Za-z_$][\w$.]*)\s*\]\s*(?:\(|\.[A-Za-z_$])");
re!(registry_assign_re, r"(?:(?:const|let|var)\s+([A-Za-z_$][\w$]*)|((?:this\.)?[A-Za-z_$][\w$]*))\s*=\s*\{");
re!(chained_re, r"\]\s*\([^)]*\)\s*\.\s*([A-Za-z_$][\w$]*)|\]\s*\.\s*([A-Za-z_$][\w$]*)");
re!(registry_entry_re, r#"^\s*(?:\[[^\]]+\]|['"]?[\w$]+['"]?)\s*:\s*([A-Za-z_$][\w$]*)\s*$"#);

const REGISTRY_CLASS_ENTRY: &[&str] = &["execute", "run", "handle", "perform", "process", "call", "apply", "dispatch"];

/// `key: Identifier` entries at the top level of an object literal's body.
fn registry_entries(body: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut start = 0;
    loop {
        let stop = index_at_depth0(body, b',', start).unwrap_or(body.len());
        if let Some(c) = registry_entry_re().captures(&body[start..stop]) {
            let n = c[1].to_owned();
            if n.len() >= 3 && !names.contains(&n) {
                names.push(n);
            }
        }
        if stop >= body.len() {
            break;
        }
        start = stop + 1;
    }
    names
}

pub struct RegistryScanner;

impl FileHook for RegistryScanner {
    fn name(&self) -> &'static str {
        "registry-sites"
    }
    fn applies(&self, lang: Lang, _rel: &str) -> bool {
        lang.family() == LangFamily::Js
    }
    fn triggers(&self) -> &'static [&'static str] {
        &["["]
    }
    fn run(&self, ctx: &HookCtx<'_>, out: &mut HookOut) {
        let src = ctx.src;
        if is_minified(src) {
            return;
        }
        let safe = cg_extract::post::strip_js_comments(src);
        let lines = LineIndex::new(&safe);
        let mut dispatches: Vec<(String, u32, String)> = Vec::new();
        for c in registry_dispatch_re().captures_iter(&safe) {
            let m = c.get(0).unwrap();
            let start = c.get(1).unwrap().start();
            if mid_chain(&safe, start) && !safe[..start].ends_with("this.") {
                continue;
            }
            let win = &safe[m.start()..(m.start() + 160).min(safe.len())];
            let win = &win[..(0..=win.len()).rev().find(|i| win.is_char_boundary(*i)).unwrap_or(0)];
            let chained = chained_re().captures(win).and_then(|cc| cc.get(1).or_else(|| cc.get(2))).map(|g| g.as_str().to_owned()).unwrap_or_default();
            let reg = c[1].trim_start_matches("this.").to_owned();
            dispatches.push((reg, lines.line(m.start()), chained));
        }
        if dispatches.is_empty() {
            return;
        }
        let wanted: FxHashSet<&str> = dispatches.iter().map(|d| d.0.as_str()).collect();
        let mut registries: FxHashMap<String, (Vec<String>, u32)> = FxHashMap::default();
        for c in registry_assign_re().captures_iter(&safe) {
            let lhs = c.get(1).or_else(|| c.get(2)).unwrap().as_str().trim_start_matches("this.");
            if !wanted.contains(lhs) || registries.contains_key(lhs) {
                continue;
            }
            let open = c.get(0).unwrap().end() - 1;
            let Some(close) = match_bracket(&safe, open) else { continue };
            let names = registry_entries(&safe[open + 1..close]);
            if names.len() >= 2 {
                registries.insert(lhs.to_owned(), (names, lines.line(c.get(0).unwrap().start())));
            }
        }
        for (reg, line, chained) in dispatches {
            let Some((names, reg_line)) = registries.get(&reg) else { continue };
            let Some(row) = enclosing_row(ctx, line, FN_KINDS) else { continue };
            out.sites.push(site(PASS_REGISTRY, ROLE_REGISTRY_DISPATCH, &reg, chained, Some(row), line, format!("{reg_line}|{}", names.join(","))));
        }
    }
}

/// A registered handler's callable entry: the function, or a class's `execute`-like method.
fn registry_target<'e, 'v>(e: &'e Engine<'v>, cx: &FileCx<'e, 'v>, wcx: &mut WorkerCx<'e, 'v>, name: &str, chained: &str) -> Option<NodeIdx> {
    let view = e.view;
    let n = match cx.resolve_name(wcx, name, Accept::Any, 0, u32::MAX, None, true) {
        NameRes::Found(n, ..) => n,
        _ => return None,
    };
    match view.node(n).kind() {
        NodeKind::Function | NodeKind::Method => Some(n),
        NodeKind::Class | NodeKind::Struct => {
            let want = REGISTRY_CLASS_ENTRY.contains(&chained).then_some(chained);
            let members: Vec<NodeIdx> =
                view.out_edges(n).filter(|ed| ed.kind == EdgeKind::Contains && view.node(ed.dst).kind() == NodeKind::Method).map(|ed| ed.dst).collect();
            let by = |p: &dyn Fn(&str) -> bool| members.iter().copied().find(|m| p(view.node(*m).name()));
            Some(want.and_then(|w| by(&|m| m == w)).or_else(|| by(&|m| REGISTRY_CLASS_ENTRY.contains(&m))).or_else(|| by(&|m| m == "constructor")).unwrap_or(n))
        }
        _ => None,
    }
}

pub(crate) fn registries<'e, 'v>(e: &'e Engine<'v>, out: &mut Out, wcx: &mut WorkerCx<'e, 'v>) {
    let view = e.view;
    for f in view.files() {
        let sites: Vec<_> = view.sites_of(f).iter().filter(|s| s.pass == PASS_REGISTRY).collect();
        if sites.is_empty() {
            continue;
        }
        let cx = wcx.file_cx(e, f);
        let mut seen: FxHashSet<(NodeIdx, NodeIdx)> = FxHashSet::default();
        for s in sites {
            let payload = view.text_str(s.payload);
            let Some((reg_line, names)) = payload.split_once('|') else { continue };
            let chained = text(view, s.key_b);
            let mut added = 0;
            for name in names.split(',') {
                if added >= REGISTRY_FANOUT_CAP {
                    break;
                }
                let Some(target) = registry_target(e, &cx, wcx, name, chained) else { continue };
                if target == s.node || !seen.insert((s.node, target)) {
                    continue;
                }
                added += 1;
                let meta =
                    serde_json::json!({ "synthesizedBy": "object-registry", "via": name, "registeredAt": format!("{}:{reg_line}", view.file(f).path()) });
                out.push_meta(EdgeKind::Calls, s.node, target, s.line, NONE, meta, order_of("object-registry"));
            }
        }
    }
}

// =================================================================== cross-tier channels

pub const ROLE_HTTP_CALL: u8 = 0;
pub const ROLE_HTTP_CLIENT: u8 = 1;
/// `app.use('/prefix', router)` (recorded by the Express route hook).
pub const ROLE_HTTP_MOUNT: u8 = 2;
pub const ROLE_QUEUE_BINDING: u8 = 0;
pub const ROLE_QUEUE_PRODUCER: u8 = 1;
pub const ROLE_QUEUE_CONSUMER: u8 = 2;
pub const ROLE_BUS_DISPATCH: u8 = 0;
pub const ROLE_BUS_HANDLER: u8 = 1;

const GENERIC: &str = r"(?:<[^()<>]*(?:<[^()<>]*>[^()<>]*)*>)?";
re!(bare_client_re, &format!(r"(?:(?:window|globalThis|global)\s*\.\s*)?\b(fetch|\$fetch|ofetch|axios|ky|got|useFetch|useSWR)\s*{GENERIC}\s*\("));
re!(
    member_client_re,
    &format!(
        r"((?:this\s*\.\s*)?[A-Za-z_$][\w$]*(?:\s*\.\s*[A-Za-z_$][\w$]*)*)\s*\.\s*(get|post|put|patch|delete|head|options|request|\$get|\$post|\$put|\$patch|\$delete)\s*{GENERIC}\s*\("
    )
);
re!(
    client_factory_re,
    r"\b(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*(?::[^=;]*?)?=\s*(?:await\s+)?(?:(?:axios|ky|got|ofetch|\$fetch|wretch|redaxios)\s*\.\s*(?:create|extend)|new\s+Axios|wretch)\s*\("
);
re!(default_client_re, r"\bexport\s+default\s+(?:(?:axios|ky|got|ofetch|\$fetch|wretch|redaxios)\s*\.\s*(?:create|extend)|new\s+Axios|wretch)\s*\(");
re!(
    client_names_re,
    r"^(?:axios|ky|got|superagent|http|https|httpClient|httpService|api|apiClient|client|restClient|request|agent|fetcher|instance|\$api|\$http|\$axios|axiosInstance|Axios|HttpClient|backend|server)$"
);
re!(server_names_re, r"^(?:app|router|route|routes|express|fastify|koa|hono|elysia|apiRouter|v1|v2|r)$");
re!(method_option_re, r#"\bmethod\s*:\s*(['"`])([A-Za-z]+)['"`]"#);
re!(base_url_re, r"\b(?:baseURL|baseUrl|prefixUrl|baseURI)\s*:\s*");
re!(url_key_re, r"\burl\s*:\s*");
re!(queue_add_re, r#"((?:this\s*\.\s*)?[A-Za-z_$][\w$]*(?:\s*\.\s*[A-Za-z_$][\w$]*)*)\s*\.\s*add\s*\(\s*(['"`])([^'"`]+)['"`]"#);
re!(queue_shaped_re, r"(?i)queue|jobs?$|worker|bull|flow|producer");
re!(new_worker_re, r#"\bnew\s+Worker\s*(?:<[^>]*>)?\s*\(\s*(['"`])([^'"`]+)['"`]\s*,\s*"#);
re!(
    queue_process_re,
    r#"((?:this\s*\.\s*)?[A-Za-z_$][\w$]*(?:\s*\.\s*[A-Za-z_$][\w$]*)*)\s*\.\s*process\s*\(\s*(?:(['"`])([^'"`]+)['"`]\s*,\s*)?(?:\d+\s*,\s*)?"#
);
re!(
    queue_binding_re,
    r#"\b(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*(?::[^=;]*?)?=\s*(?:new\s+)?(?:Queue|Bull|BullQueue)\s*(?:<[^>]*>)?\s*\(\s*(['"`])([^'"`]+)['"`]"#
);
re!(inject_queue_re, r#"@InjectQueue\s*\(\s*(['"`])([^'"`]+)['"`]\s*\)\s*(?:(?:private|public|protected|readonly)\s+)*([A-Za-z_$][\w$]*)"#);
re!(handler_arg_re, r"^(?:(?:async\s+)?([A-Za-z_$][\w$.]*)\s*(?:[,)]|$)|(?:async\s*)?(?:\(|function\b|[A-Za-z_$][\w$]*\s*=>))");
re!(emit_re, r#"((?:[\w$]+(?:\([^()]*\))?\s*\.\s*)*[\w$]+)\s*\.\s*(emit|emitAsync)\s*\(\s*(['"`])([^'"`\n]+)['"`]"#);
re!(socket_on_re, r#"((?:[\w$]+(?:\([^()]*\))?\s*\.\s*)*[\w$]+)\s*\.\s*(?:on|once)\s*\(\s*(['"`])([^'"`\n]+)['"`]\s*,\s*"#);
re!(
    socket_words_re,
    r"^(?:socket|io|ws|wss|client|server|namespace|nsp|conn|connection|gateway|broadcast|to|in|of|except|volatile|local|sockets|socketServer|wsServer|room|channel|pusher|ably|ioClient|socketClient|sock)$"
);
re!(
    bus_words_re,
    r"^(?:eventEmitter|emitter|events|eventBus|bus|dispatcher|pubsub|publisher|eventPublisher|ee|hub|mediator|broker|messageBus|appEvents|domainEvents|eventsService|eventService)$"
);
re!(
    generic_event_re,
    r"^(?:error|connect|connect_error|connect_failed|connection|disconnect|disconnecting|reconnect|reconnect_attempt|reconnecting|reconnect_error|reconnect_failed|close|open|end|data|ready|drain|finish|pipe|unpipe|listening|timeout|ping|pong|upgrade|newListener|removeListener)$"
);
re!(
    socket_server_re,
    r#"@WebSocketGateway\s*\(|@SubscribeMessage\s*\(|@WebSocketServer\s*\(|\bnew\s+(?:Server|SocketIOServer|WebSocketServer|WebSocket\.Server|WSServer)\b|\.on\s*\(\s*['"]connection['"]"#
);
re!(class_after_re, r"\bclass\s+([A-Za-z_$][\w$]*)");
re!(literal_re, r#"(['"`])([^'"`]+)['"`]"#);

fn first_literal(args: &str) -> Option<String> {
    literal_re().captures(args).map(|c| c[2].to_owned())
}

fn all_literals(args: &str) -> Vec<String> {
    literal_re().captures_iter(args).map(|c| c[2].to_owned()).collect()
}

/// Every `@Name(...)` decorator: (args, start, end).
fn decorators<'s>(src: &'s str, name: &str) -> Vec<(&'s str, usize, usize)> {
    let pat = format!("@{name}");
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(rel) = src[from..].find(&pat) {
        let at = from + rel;
        from = at + pat.len();
        let rest = &src[from..];
        if rest.starts_with(|c: char| c.is_alphanumeric() || c == '_') {
            continue;
        }
        let open = from + (rest.len() - rest.trim_start().len());
        if src.as_bytes().get(open) != Some(&b'(') {
            continue;
        }
        if let Some(close) = match_bracket(src, open) {
            out.push((&src[open + 1..close], at, close + 1));
            from = close + 1;
        }
    }
    out
}

/// The method a decorator ending at `end` sits on (stacked decorators and modifiers skipped).
fn decorated_method(ctx: &HookCtx<'_>, lines: &LineIndex<'_>, src: &str, end: usize, within: Option<(u32, u32)>) -> Option<u32> {
    let mut i = end;
    let b = src.as_bytes();
    loop {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if b.get(i) == Some(&b'@') {
            i += 1;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'.') {
                i += 1;
            }
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if b.get(i) == Some(&b'(') {
                i = match_bracket(src, i)? + 1;
            }
            continue;
        }
        let rest = &src[i..];
        let kw = ["public", "private", "protected", "async", "static", "readonly", "override"]
            .iter()
            .find(|k| rest.starts_with(**k) && rest[k.len()..].starts_with(char::is_whitespace));
        match kw {
            Some(k) => i += k.len(),
            None => break,
        }
    }
    let rest = &src[i..];
    let len = rest.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$')).unwrap_or(rest.len());
    if len == 0 {
        return None;
    }
    let name = &rest[..len];
    let line = lines.line(i);
    ctx.rows.nodes().enumerate().find_map(|(row, n)| {
        let inside = within.is_none_or(|(s, e)| n.start_line() >= s && n.end_line() <= e);
        (matches!(n.kind(), NodeKind::Method | NodeKind::Function) && n.name() == name && n.start_line() + 1 >= line && n.start_line() <= line + 1 && inside)
            .then_some(row as u32)
    })
}

/// The function a handler argument names (resolved at link time) or the inline function it is.
fn handler_site(ctx: &HookCtx<'_>, text: &str, line: u32) -> Option<(Option<u32>, String)> {
    let c = handler_arg_re().captures(text.trim_start())?;
    match c.get(1) {
        Some(name) => Some((enclosing_row(ctx, line, FN_KINDS), name.as_str().to_owned())),
        None => {
            // an inline function: the function (or value) it is written in
            let row = enclosing_row(ctx, line, FN_KINDS).or_else(|| enclosing_row(ctx, line, &[NodeKind::Constant, NodeKind::Variable]))?;
            Some((Some(row), String::new()))
        }
    }
}

fn shape_of(receiver: &str) -> Option<&'static str> {
    let mut flat = String::new();
    let mut depth = 0;
    for c in receiver.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ if depth == 0 && !c.is_whitespace() => flat.push(c),
            _ => {}
        }
    }
    let segs: Vec<&str> = flat.split('.').filter(|s| *s != "this").collect();
    if segs.iter().any(|s| socket_words_re().is_match(s)) {
        Some("socket")
    } else if segs.iter().any(|s| bus_words_re().is_match(s)) {
        Some("bus")
    } else {
        None
    }
}

/// HTTP client calls, queue producers / consumers and event dispatches / handlers.
pub struct TierScanner;

impl FileHook for TierScanner {
    fn name(&self) -> &'static str {
        "tier-sites"
    }
    fn applies(&self, lang: Lang, rel: &str) -> bool {
        lang.family() == LangFamily::Js && !cg_scan::testpath::is_test_path(rel)
    }
    fn triggers(&self) -> &'static [&'static str] {
        &[
            "fetch",
            "axios",
            "ky",
            "got",
            "useSWR",
            ".get",
            ".post",
            ".put",
            ".patch",
            ".delete",
            ".head",
            ".options",
            ".request",
            ".$",
            ".add(",
            "Processor",
            "Worker",
            ".process(",
            ".emit",
            ".on(",
            ".once(",
            "OnEvent",
            "SubscribeMessage",
        ]
    }
    fn run(&self, ctx: &HookCtx<'_>, out: &mut HookOut) {
        if is_minified(ctx.src) {
            return;
        }
        let safe = cg_extract::post::strip_js_comments(ctx.src);
        let src = safe.as_str();
        let lines = LineIndex::new(src);
        // client instances this file makes: `const api = axios.create({ baseURL })`
        let base_url = |open: usize| -> String {
            call_args(src, open).and_then(|a| a.first().and_then(|c| base_url_re().find(c).and_then(|m| read_string_at(c, m.end())))).unwrap_or_default()
        };
        for c in client_factory_re().captures_iter(src) {
            let open = c.get(0).unwrap().end() - 1;
            out.sites.push(site(PASS_HTTP, ROLE_HTTP_CLIENT, &c[1], "", None, lines.line(c.get(0).unwrap().start()), base_url(open)));
        }
        if let Some(m) = default_client_re().find(src) {
            out.sites.push(site(PASS_HTTP, ROLE_HTTP_CLIENT, "default", "", None, lines.line(m.start()), base_url(m.end() - 1)));
        }
        let mut http = |at: usize, open: usize, verb: Option<&str>, receiver: &str| {
            let line = lines.line(at);
            let Some(row) = enclosing_row(ctx, line, FN_KINDS) else { return };
            let Some(args) = call_args(src, open) else { return };
            let Some(mut first) = args.first().copied().filter(|a| !a.is_empty()) else { return };
            let mut method = verb.map(str::to_owned);
            if first.starts_with('{') {
                // `axios({ url, method })`, `.request({ url, method })`
                let Some(m) = url_key_re().find(first) else { return };
                method = method.or_else(|| method_option_re().captures(first).map(|c| c[2].to_ascii_uppercase())).or(Some("GET".into()));
                first = &first[m.end()..];
            } else if method.is_none() {
                method = Some(method_option_re().captures(&args[1..].join(",")).map(|c| c[2].to_ascii_uppercase()).unwrap_or_else(|| "GET".into()));
            }
            let first = first.trim_start();
            let first = first.strip_prefix("new URL(").map_or(first, |r| r.trim_start());
            let Some(literal) = read_string_at(first, 0) else { return };
            let callee: String = src[at..open].chars().filter(|c| !c.is_whitespace()).collect();
            let callee = callee.split('<').next().unwrap_or("").to_owned();
            out.sites.push(site(
                PASS_HTTP,
                ROLE_HTTP_CALL,
                method.unwrap_or_default(),
                format!("{}|{receiver}|{callee}", lines.col(at)),
                Some(row),
                line,
                literal,
            ));
        };
        for m in bare_client_re().find_iter(src) {
            let lead = &src[..m.start()];
            if lead.ends_with('.') && !m.as_str().starts_with(['w', 'g']) {
                continue; // `this.fetch()` is a project method
            }
            if mid_chain(src, m.start()) && !lead.ends_with('.') {
                continue;
            }
            http(m.start(), m.end() - 1, None, "");
        }
        for c in member_client_re().captures_iter(src) {
            let recv = c.get(1).unwrap();
            if mid_chain(src, recv.start()) {
                continue;
            }
            let verb = c[2].trim_start_matches('$').to_ascii_uppercase();
            let receiver: String = recv.as_str().chars().filter(|ch| !ch.is_whitespace()).collect();
            http(recv.start(), c.get(0).unwrap().end() - 1, (verb != "REQUEST").then_some(verb.as_str()), &receiver);
        }

        // queues: bindings, producers, consumers
        for c in queue_binding_re().captures_iter(src) {
            out.sites.push(site(PASS_QUEUE, ROLE_QUEUE_BINDING, &c[1], "", None, lines.line(c.get(0).unwrap().start()), &c[3]));
        }
        for c in inject_queue_re().captures_iter(src) {
            out.sites.push(site(PASS_QUEUE, ROLE_QUEUE_BINDING, &c[3], "", None, lines.line(c.get(0).unwrap().start()), &c[2]));
        }
        for c in queue_add_re().captures_iter(src) {
            let recv = c.get(1).unwrap();
            if mid_chain(src, recv.start()) {
                continue;
            }
            let line = lines.line(recv.start());
            let Some(row) = enclosing_row(ctx, line, FN_KINDS) else { continue };
            let receiver: String = recv.as_str().chars().filter(|ch| !ch.is_whitespace()).collect();
            out.sites.push(site(PASS_QUEUE, ROLE_QUEUE_PRODUCER, &c[3], format!("{}|{receiver}", lines.col(recv.start())), Some(row), line, ""));
        }
        // `@Processor('email')` on a class: its `@Process('job')` methods, or its `process`
        for (args, _, end) in decorators(src, "Processor") {
            let queue = first_literal(args).unwrap_or_default();
            let Some(cm) = class_after_re().captures(&src[end..]) else { continue };
            let cls_line = lines.line(end + cm.get(0).unwrap().start());
            let Some(cls) = ctx.rows.nodes().find(|n| n.kind() == NodeKind::Class && n.name() == &cm[1] && n.start_line() + 2 >= cls_line) else { continue };
            let span = (cls.start_line(), cls.end_line());
            let mut any = false;
            for (jargs, jat, jend) in decorators(src, "Process") {
                let line = lines.line(jat);
                if line < span.0 || line > span.1 {
                    continue;
                }
                let Some(row) = decorated_method(ctx, &lines, src, jend, Some(span)) else { continue };
                any = true;
                out.sites.push(site(PASS_QUEUE, ROLE_QUEUE_CONSUMER, &queue, first_literal(jargs).unwrap_or_default(), Some(row), line, ""));
            }
            if !any {
                if let Some((row, n)) = ctx
                    .rows
                    .nodes()
                    .enumerate()
                    .find(|(_, n)| n.kind() == NodeKind::Method && n.name() == "process" && n.start_line() >= span.0 && n.end_line() <= span.1)
                {
                    out.sites.push(site(PASS_QUEUE, ROLE_QUEUE_CONSUMER, &queue, "", Some(row as u32), n.start_line(), ""));
                }
            }
        }
        // BullMQ `new Worker('email', handler)`, Bull `queue.process('job', handler)`
        for c in new_worker_re().captures_iter(src) {
            let m = c.get(0).unwrap();
            let line = lines.line(m.start());
            let tail = &src[m.end()..(m.end() + 200).min(src.len())];
            let tail = &tail[..(0..=tail.len()).rev().find(|i| tail.is_char_boundary(*i)).unwrap_or(0)];
            let Some((row, name)) = handler_site(ctx, tail, line) else { continue };
            out.sites.push(site(PASS_QUEUE, ROLE_QUEUE_CONSUMER, &c[2], "", row, line, name));
        }
        for c in queue_process_re().captures_iter(src) {
            let recv = c.get(1).unwrap();
            if mid_chain(src, recv.start()) {
                continue;
            }
            let receiver: String = recv.as_str().chars().filter(|ch| !ch.is_whitespace()).collect();
            let m = c.get(0).unwrap();
            let line = lines.line(m.start());
            let tail = &src[m.end()..(m.end() + 200).min(src.len())];
            let tail = &tail[..(0..=tail.len()).rev().find(|i| tail.is_char_boundary(*i)).unwrap_or(0)];
            let Some((row, name)) = handler_site(ctx, tail, line) else { continue };
            // the queue is the receiver's binding, resolved at link time: `@receiver` keys it
            let job = c.get(3).map_or("", |j| j.as_str());
            out.sites.push(site(PASS_QUEUE, ROLE_QUEUE_CONSUMER, format!("@{receiver}"), job, row, line, name));
        }

        // events: a bus, and sockets both ways
        let side = if socket_server_re().is_match(src) { "server" } else { "client" };
        for c in emit_re().captures_iter(src) {
            let recv = c.get(1).unwrap();
            if mid_chain(src, recv.start()) {
                continue;
            }
            let Some(shape) = shape_of(recv.as_str()) else { continue };
            let event = &c[4];
            if generic_event_re().is_match(event) {
                continue;
            }
            let line = lines.line(recv.start());
            let Some(row) = enclosing_row(ctx, line, FN_KINDS) else { continue };
            let callee: String = format!("{}.{}", recv.as_str().chars().filter(|ch| !ch.is_whitespace()).collect::<String>(), &c[2]);
            out.sites.push(site(PASS_BUS, ROLE_BUS_DISPATCH, event, format!("{shape}|{side}|{}|{callee}", lines.col(recv.start())), Some(row), line, ""));
        }
        for (args, at, end) in decorators(src, "OnEvent") {
            let Some(row) = decorated_method(ctx, &lines, src, end, None) else { continue };
            let patterns = if args.trim_start().starts_with('[') { all_literals(args) } else { first_literal(args).into_iter().collect() };
            for p in patterns {
                out.sites.push(site(PASS_BUS, ROLE_BUS_HANDLER, p, format!("bus|{side}"), Some(row), lines.line(at), ""));
            }
        }
        for (args, at, end) in decorators(src, "SubscribeMessage") {
            let (Some(row), Some(event)) = (decorated_method(ctx, &lines, src, end, None), first_literal(args)) else { continue };
            out.sites.push(site(PASS_BUS, ROLE_BUS_HANDLER, event, "socket|server", Some(row), lines.line(at), ""));
        }
        for c in socket_on_re().captures_iter(src) {
            let recv = c.get(1).unwrap();
            if mid_chain(src, recv.start()) || shape_of(recv.as_str()) != Some("socket") || generic_event_re().is_match(&c[3]) {
                continue;
            }
            let m = c.get(0).unwrap();
            let line = lines.line(m.start());
            let tail = &src[m.end()..(m.end() + 200).min(src.len())];
            let tail = &tail[..(0..=tail.len()).rev().find(|i| tail.is_char_boundary(*i)).unwrap_or(0)];
            let Some((row, name)) = handler_site(ctx, tail, line) else { continue };
            out.sites.push(site(PASS_BUS, ROLE_BUS_HANDLER, &c[3], format!("socket|{side}"), row, line, name));
        }
    }
}

/// An HTTP route: its node, method and path segments.
pub(crate) struct HttpRoute {
    pub(crate) node: NodeIdx,
    method: String,
    segs: Vec<String>,
    excluded: Vec<(Option<String>, Vec<String>)>,
}

const HTTP_VERBS: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS", "ALL", "ANY"];

/// `/api` + `/users` -> `/api/users`; `/api/` + `/` -> `/api`.
fn join_paths(prefix: &str, path: &str) -> String {
    let a = prefix.trim_end_matches('/');
    let b = path.trim_start_matches('/');
    let joined = if b.is_empty() { a.to_owned() } else { format!("{a}/{b}") };
    if joined.is_empty() {
        "/".to_owned()
    } else {
        joined
    }
}

/// The prefix each mounted router file serves under (upstream Express `postExtract`):
/// `app.use('/api', routes)` + `router.use('/users', users)` puts `users`'s `POST /` at
/// `POST /api/users`. A file mounted at two different paths is left alone.
fn mount_prefixes(e: &Engine<'_>) -> FxHashMap<FileId, String> {
    let view = e.view;
    let mut mounts: Vec<(FileId, String, FileId)> = Vec::new();
    for f in view.files() {
        for s in view.sites_of(f).iter().filter(|s| s.pass == PASS_HTTP && s.role == ROLE_HTTP_MOUNT) {
            let target = view.text_str(s.payload).trim();
            let lang = view.file(f).lang();
            let file = if let Some(spec) = target.strip_prefix("require(").map(|r| r.trim_start()).and_then(|r| read_string_at(r, 0)) {
                e.modules.target_file(e.modules.resolve_import(lang, &spec, f))
            } else {
                let ident = target.split('.').next().unwrap_or("");
                let suffix_ok = target.split_once('.').is_none_or(|(_, s)| matches!(s, "default" | "router" | "routes"));
                let i = suffix_ok.then(|| view.imports_of(f).iter().position(|imp| view.text_str(imp.local) == ident)).flatten();
                i.and_then(|i| e.modules.target_file(e.imp_targets[f.idx()].get(i).copied()?))
            };
            if let Some(t) = file.filter(|t| *t != f) {
                mounts.push((f, text(view, s.key_a).to_owned(), t));
            }
        }
    }
    let mut prefix_of: FxHashMap<FileId, String> = FxHashMap::default();
    for _ in 0..8 {
        let mut next: FxHashMap<FileId, String> = FxHashMap::default();
        let mut ambiguous: FxHashSet<FileId> = FxHashSet::default();
        for (f, prefix, target) in &mounts {
            let full = join_paths(prefix_of.get(f).map_or("", |p| p.as_str()), prefix);
            match next.get(target) {
                Some(seen) if *seen != full => {
                    ambiguous.insert(*target);
                }
                _ => {
                    next.insert(*target, full);
                }
            }
        }
        for a in ambiguous {
            next.remove(&a);
        }
        if next == prefix_of {
            break;
        }
        prefix_of = next;
    }
    prefix_of
}

pub(crate) fn http_routes(e: &Engine<'_>) -> Vec<HttpRoute> {
    let view = e.view;
    let prefixes = mount_prefixes(e);
    let nest_paths = crate::frameworks::nest::effective_paths(e);
    let mut out = Vec::new();
    for f in view.files() {
        let prefix = prefixes.get(&f).map(String::as_str).filter(|p| *p != "/");
        for n in view.nodes_in_file(f) {
            let n = NodeIdx(n);
            let node = view.node(n);
            if node.kind() != NodeKind::Route {
                continue;
            }
            let Some((method, path)) = node.name().split_once(' ') else { continue };
            let method = method.to_ascii_uppercase();
            if !HTTP_VERBS.contains(&method.as_str()) || !path.trim().starts_with('/') {
                continue;
            }
            let paths = nest_paths.get(&n).cloned().unwrap_or_else(|| {
                let path = match prefix {
                    Some(p) => join_paths(p, path.trim()),
                    None => path.trim().to_owned(),
                };
                vec![crate::frameworks::nest::EffectivePath { path, method: None, excluded: Vec::new() }]
            });
            for path in paths {
                let segments = |p: &str| p.split('/').filter(|s| !s.is_empty()).map(str::to_owned).collect();
                out.push(HttpRoute {
                    node: n,
                    method: path.method.unwrap_or_else(|| method.clone()),
                    segs: segments(&path.path),
                    excluded: path.excluded.into_iter().map(|(method, path)| (method, segments(&path))).collect(),
                });
            }
        }
    }
    out
}

fn param_seg(s: &str) -> bool {
    s.starts_with([':', '{', '[', '<', '*']) || s.ends_with('?')
}

fn catch_all_seg(s: &str) -> bool {
    s == "*"
        || s.starts_with("[...")
        || s.starts_with("{*")
        || (s.starts_with(':') && s.ends_with('*'))
        || (s.starts_with('{') && s.ends_with(":*}"))
        || (s.starts_with('*') && s[1..].chars().all(|c| c.is_alphanumeric() || c == '_'))
}

/// A literal match beats a parameter's; a hole fills a parameter, never a literal segment.
fn score_path(client: &[String], route: &[String]) -> Option<u32> {
    let mut score = 0;
    let mut i = 0;
    for seg in route {
        if catch_all_seg(seg) {
            if i >= client.len() {
                return None;
            }
            score += (client.len() - i) as u32;
            i = client.len();
            continue;
        }
        let c = client.get(i)?;
        if param_seg(seg) {
            score += 2;
        } else if c == seg {
            score += 3;
        } else {
            return None;
        }
        i += 1;
    }
    (i == client.len()).then_some(score)
}

/// Segments of a client path (`${…}` as `*`), and whether a base URL hides its prefix.
pub(crate) fn client_path(raw: &str, base: Option<&str>) -> Option<(Vec<String>, bool)> {
    let mut p = &raw[..raw.find(['?', '#']).unwrap_or(raw.len())];
    let mut suffix = false;
    let owned;
    if let Some(i) = p.find("//") {
        let scheme = &p[..i];
        if scheme.is_empty() || (scheme.ends_with(':') && scheme[..scheme.len() - 1].chars().all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c))) {
            let rest = &p[i + 2..];
            p = rest.find('/').map_or("/", |j| &rest[j..]);
        }
    }
    if !p.starts_with('/') {
        if let Some(rest) = p.strip_prefix(HOLE) {
            if !rest.starts_with('/') {
                return None;
            }
            p = rest;
            suffix = true;
        } else {
            let base = base?;
            match client_path(base, None) {
                Some((segs, s)) => {
                    suffix = s;
                    owned = format!("/{}/{}", segs.join("/"), p);
                    p = &owned;
                }
                None if base.contains(HOLE) => {
                    suffix = true;
                    owned = format!("/{p}");
                    p = &owned;
                }
                None => return None,
            }
        }
    } else if let Some(base) = base {
        match client_path(base, None) {
            Some((segs, s)) if !segs.is_empty() => {
                suffix = s;
                owned = format!("/{}{}", segs.join("/"), p);
                p = &owned;
            }
            None if base.contains(HOLE) => suffix = true,
            _ => {}
        }
    }
    let segs: Vec<String> = p.split('/').filter(|s| !s.is_empty()).map(|s| if s.contains(HOLE) { "*".to_owned() } else { s.to_owned() }).collect();
    if !segs.is_empty() && segs.iter().all(|s| s == "*") {
        return None;
    }
    Some((segs, suffix))
}

pub(crate) fn match_http<'r>(method: &str, segs: &[String], suffix: bool, routes: &'r [HttpRoute]) -> Option<&'r HttpRoute> {
    let mut best: Option<(&HttpRoute, u32)> = None;
    let mut tied = false;
    for r in routes {
        if r.method != "ALL" && r.method != "ANY" && r.method != method {
            continue;
        }
        if r.excluded.iter().any(|(m, path)| {
            if m.as_ref().is_some_and(|m| m != method) {
                return false;
            }
            if suffix {
                path.len() >= segs.len() && score_path(segs, &path[path.len() - segs.len()..]).is_some()
            } else {
                score_path(segs, path).is_some()
            }
        }) {
            continue;
        }
        let score = if suffix {
            // a base URL hides a prefix the route may spell out: two segments of tail at least,
            // two of hidden prefix at most
            if segs.len() < 2 || r.segs.len() < segs.len() || r.segs.len() - segs.len() > 2 {
                continue;
            }
            score_path(segs, &r.segs[r.segs.len() - segs.len()..])
        } else {
            score_path(segs, &r.segs)
        };
        let Some(score) = score else { continue };
        match best {
            Some((_, b)) if score < b => {}
            Some((current, b)) if score == b && current.node != r.node => tied = true,
            Some((_, b)) if score == b => {}
            _ => {
                best = Some((r, score));
                tied = false;
            }
        }
    }
    best.filter(|_| !tied).map(|b| b.0)
}

/// Sites of `pass` in the module an import of `f` binds `local` to: (module, exported name).
fn import_source<'v>(e: &Engine<'v>, f: FileId, local: &str) -> Option<(FileId, &'v str, bool)> {
    let view = e.view;
    let i = view.imports_of(f).iter().position(|imp| view.text_str(imp.local) == local)?;
    let imp = &view.imports_of(f)[i];
    let target = e.modules.target_file(e.imp_targets[f.idx()].get(i).copied()?)?;
    let imported = view.text_str(imp.imported);
    Some((target, imported, imported == "default"))
}

pub(crate) fn cross_tier<'e, 'v>(e: &'e Engine<'v>, out: &mut Out, wcx: &mut WorkerCx<'e, 'v>) {
    let view = e.view;
    let order = order_of("http-client");
    let mut seen: FxHashSet<(NodeIdx, NodeIdx)> = FxHashSet::default();

    // ---- 1. HTTP client -> route
    let routes = http_routes(e);
    let clients = |f: FileId| -> FxHashMap<&str, &str> {
        view.sites_of(f).iter().filter(|s| s.pass == PASS_HTTP && s.role == ROLE_HTTP_CLIENT).map(|s| (text(view, s.key_a), view.text_str(s.payload))).collect()
    };
    if !routes.is_empty() {
        for f in view.files() {
            let calls: Vec<_> = view.sites_of(f).iter().filter(|s| s.pass == PASS_HTTP && s.role == ROLE_HTTP_CALL).collect();
            if calls.is_empty() {
                continue;
            }
            let own = clients(f);
            // a registration the route extractor read is no client call
            let route_lines: FxHashSet<u32> =
                view.nodes_in_file(f).map(NodeIdx).filter(|n| view.node(*n).kind() == NodeKind::Route).map(|n| view.node(n).start_line()).collect();
            for s in calls {
                if route_lines.contains(&s.line) {
                    continue;
                }
                let key_b = text(view, s.key_b);
                let mut parts = key_b.splitn(3, '|');
                let (col, receiver, callee) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                // what the receiver is: a client this file or an import makes, or a client by name
                let base: Option<Option<&str>> = if receiver.is_empty() {
                    Some(None)
                } else {
                    let chain: Vec<&str> = receiver.trim_start_matches("this.").split('.').collect();
                    let (head, last) = (chain[0], chain[chain.len() - 1]);
                    if let Some(b) = own.get(head) {
                        Some(Some(*b).filter(|b| !b.is_empty()))
                    } else if let Some((m, name, default)) = import_source(e, f, head) {
                        let theirs = clients(m);
                        match theirs.get(if default { "default" } else { name }) {
                            Some(b) => Some(Some(*b).filter(|b| !b.is_empty())),
                            None if server_names_re().is_match(last) || server_names_re().is_match(head) => None,
                            None => client_names_re().is_match(last).then_some(None),
                        }
                    } else if server_names_re().is_match(last) || server_names_re().is_match(head) {
                        None
                    } else {
                        client_names_re().is_match(last).then_some(None)
                    }
                };
                let Some(base) = base else { continue };
                let raw = view.text_str(s.payload);
                let Some((segs, suffix)) = client_path(raw, base) else { continue };
                let method = text(view, s.key_a);
                let Some(route) = match_http(method, &segs, suffix, &routes) else { continue };
                if route.node == s.node || is_file_node(view, s.node) || !seen.insert((s.node, route.node)) {
                    continue;
                }
                let display = format!("/{}", segs.iter().map(|s| if s == "*" { "${…}" } else { s.as_str() }).collect::<Vec<_>>().join("/"));
                let rn = view.node(route.node);
                let meta = serde_json::json!({
                    "synthesizedBy": "http-client", "channel": "http", "callee": callee, "tier": "client→server",
                    "method": method, "href": display, "registeredAt": registered(view, rn.file(), rn.start_line()),
                });
                out.push_meta(EdgeKind::Calls, s.node, route.node, s.line, col.parse().unwrap_or(NONE), meta, order);
            }
        }
    }

    // ---- 2. queue job -> consumer
    struct Producer {
        f: FileId,
        node: NodeIdx,
        line: u32,
        col: u32,
        callee: String,
        queue: Option<String>,
        job: String,
    }
    struct Consumer {
        node: NodeIdx,
        f: FileId,
        line: u32,
        queue: Option<String>,
        job: Option<String>,
    }
    let bindings = |f: FileId| -> FxHashMap<&str, &str> {
        view.sites_of(f)
            .iter()
            .filter(|s| s.pass == PASS_QUEUE && s.role == ROLE_QUEUE_BINDING)
            .map(|s| (text(view, s.key_a), view.text_str(s.payload)))
            .collect()
    };
    let queue_for = |f: FileId, receiver: &str| -> Option<String> {
        let head = receiver.trim_start_matches("this.").split('.').next().unwrap_or("");
        if let Some(q) = bindings(f).get(head) {
            return Some(q.to_string());
        }
        let (m, name, _) = import_source(e, f, head)?;
        bindings(m).get(name).map(|q| q.to_string())
    };
    let mut producers = Vec::new();
    let mut consumers = Vec::new();
    for f in view.files() {
        for s in view.sites_of(f).iter().filter(|s| s.pass == PASS_QUEUE) {
            match s.role {
                ROLE_QUEUE_PRODUCER => {
                    let key_b = text(view, s.key_b);
                    let (col, receiver) = key_b.split_once('|').unwrap_or(("", key_b));
                    let queue = queue_for(f, receiver);
                    let last = receiver.rsplit('.').next().unwrap_or(receiver);
                    if queue.is_none() && !queue_shaped_re().is_match(last) {
                        continue;
                    }
                    producers.push(Producer {
                        f,
                        node: s.node,
                        line: s.line,
                        col: col.parse().unwrap_or(NONE),
                        callee: format!("{receiver}.add"),
                        queue,
                        job: text(view, s.key_a).to_owned(),
                    });
                }
                ROLE_QUEUE_CONSUMER => {
                    let key_a = text(view, s.key_a);
                    let queue = match key_a.strip_prefix('@') {
                        Some(receiver) => {
                            let q = queue_for(f, receiver);
                            let last = receiver.rsplit('.').next().unwrap_or(receiver);
                            if q.is_none() && !queue_shaped_re().is_match(last) {
                                continue;
                            }
                            q
                        }
                        None => Some(key_a.to_owned()).filter(|q| !q.is_empty()),
                    };
                    let job = Some(text(view, s.key_b).to_owned()).filter(|j| !j.is_empty());
                    let handler = view.text_str(s.payload);
                    let node = if handler.is_empty() { Some(s.node).filter(|n| !is_file_node(view, *n)) } else { handler_node(e, wcx, f, handler) };
                    let Some(node) = node else { continue };
                    consumers.push(Consumer { node, f, line: s.line, queue, job });
                }
                _ => {}
            }
        }
    }
    for p in &producers {
        let mut cands: Vec<&Consumer> =
            consumers.iter().filter(|c| (p.queue.is_none() || c.queue.is_none() || c.queue == p.queue) && c.job.as_ref().is_none_or(|j| *j == p.job)).collect();
        let exact: Vec<&Consumer> =
            cands.iter().copied().filter(|c| c.job.as_deref() == Some(p.job.as_str()) && c.queue == p.queue && p.queue.is_some()).collect();
        if !exact.is_empty() {
            cands = exact;
        } else {
            let by_job: Vec<&Consumer> = cands.iter().copied().filter(|c| c.job.as_deref() == Some(p.job.as_str())).collect();
            if !by_job.is_empty() {
                cands = by_job;
            } else if p.queue.is_none() {
                continue;
            } else {
                cands.retain(|c| c.queue == p.queue);
            }
        }
        if cands.is_empty() || cands.len() > EVENT_FANOUT_CAP {
            continue;
        }
        for c in cands {
            if c.node == p.node || !seen.insert((p.node, c.node)) {
                continue;
            }
            let mut meta = serde_json::json!({ "synthesizedBy": "queue-job", "channel": "queue", "callee": p.callee, "event": p.job, "registeredAt": registered(view, c.f, c.line) });
            if let Some(q) = p.queue.as_ref().or(c.queue.as_ref()) {
                meta["queue"] = q.clone().into();
            }
            let _ = p.f;
            out.push_meta(EdgeKind::Calls, p.node, c.node, p.line, p.col, meta, order);
        }
    }

    // ---- 3. events: a bus, and sockets both ways
    struct Dispatch<'a> {
        node: NodeIdx,
        line: u32,
        col: u32,
        callee: &'a str,
        event: &'a str,
        shape: &'a str,
        side: &'a str,
    }
    struct Handler<'a> {
        node: NodeIdx,
        f: FileId,
        line: u32,
        pattern: &'a str,
        kind: &'a str,
        side: &'a str,
    }
    let mut dispatches: Vec<Dispatch> = Vec::new();
    let mut handlers: Vec<Handler> = Vec::new();
    for f in view.files() {
        for s in view.sites_of(f).iter().filter(|s| s.pass == PASS_BUS) {
            let key_b = text(view, s.key_b);
            let mut parts = key_b.splitn(4, '|');
            let shape = parts.next().unwrap_or("");
            let side = parts.next().unwrap_or("");
            if s.role == ROLE_BUS_DISPATCH {
                let col = parts.next().and_then(|c| c.parse().ok()).unwrap_or(NONE);
                dispatches.push(Dispatch { node: s.node, line: s.line, col, callee: parts.next().unwrap_or(""), event: text(view, s.key_a), shape, side });
            } else {
                let handler = view.text_str(s.payload);
                let node = if handler.is_empty() { Some(s.node).filter(|n| !is_file_node(view, *n)) } else { handler_node(e, wcx, f, handler) };
                let Some(node) = node else { continue };
                handlers.push(Handler { node, f, line: s.line, pattern: text(view, s.key_a), kind: shape, side });
            }
        }
    }
    let mut by_event: FxHashMap<(&str, &str), usize> = FxHashMap::default();
    for d in &dispatches {
        *by_event.entry((d.shape, d.event)).or_default() += 1;
    }
    for d in &dispatches {
        if by_event[&(d.shape, d.event)] > EVENT_FANOUT_CAP {
            continue;
        }
        let (matched, tier): (Vec<&Handler>, Option<&str>) = if d.shape == "bus" {
            (handlers.iter().filter(|h| h.kind == "bus" && event_matches(h.pattern, d.event)).collect(), None)
        } else if d.side == "client" {
            (handlers.iter().filter(|h| h.kind == "socket" && h.side == "server" && h.pattern == d.event).collect(), Some("client→server"))
        } else {
            (handlers.iter().filter(|h| h.kind == "socket" && h.side == "client" && h.pattern == d.event).collect(), Some("server→client"))
        };
        if matched.is_empty() || matched.len() > EVENT_FANOUT_CAP {
            continue;
        }
        for h in matched {
            if h.node == d.node || !seen.insert((d.node, h.node)) {
                continue;
            }
            let mut meta = serde_json::json!({
                "synthesizedBy": "event-bus", "channel": if d.shape == "bus" { "event" } else { "socket" },
                "callee": d.callee, "event": d.event, "registeredAt": registered(view, h.f, h.line),
            });
            if let Some(t) = tier {
                meta["tier"] = t.into();
            }
            out.push_meta(EdgeKind::Calls, d.node, h.node, d.line, d.col, meta, order);
        }
    }
}

/// `user.*` matches one segment, `**` any number; anything else is exact.
fn event_matches(pattern: &str, event: &str) -> bool {
    if pattern == event {
        return true;
    }
    if !pattern.contains('*') {
        return false;
    }
    fn go(p: &[&str], e: &[&str]) -> bool {
        match (p.first(), e.first()) {
            (None, None) => true,
            (Some(&"**"), _) => (0..=e.len()).any(|k| go(&p[1..], &e[k..])),
            (Some(seg), Some(ev)) => {
                let ok = if seg.contains('*') {
                    let (pre, post) = seg.split_once('*').unwrap_or((seg, ""));
                    ev.len() > pre.len() + post.len() && ev.starts_with(pre) && ev.ends_with(post)
                } else {
                    seg == ev
                };
                ok && go(&p[1..], &e[1..])
            }
            _ => false,
        }
    }
    let p: Vec<&str> = pattern.split('.').collect();
    let e: Vec<&str> = event.split('.').collect();
    go(&p, &e)
}

/// The function a handler name denotes, from the file it is written in.
fn handler_node<'e, 'v>(e: &'e Engine<'v>, wcx: &mut WorkerCx<'e, 'v>, f: FileId, name: &str) -> Option<NodeIdx> {
    let view = e.view;
    let last = name.rsplit('.').next().unwrap_or(name);
    let cx = wcx.file_cx(e, f);
    let n = if name.contains('.') {
        // `this.handle` / `handlers.onJob`: a same-file declaration of that name, unique
        let mut local = view.by_name(last).filter(|n| view.hot(*n).file == f && view.node(*n).kind().is_callable());
        let first = local.next();
        if local.next().is_some() {
            return None;
        }
        first?
    } else {
        match cx.resolve_name(wcx, last, Accept::Callable, 0, u32::MAX, None, true) {
            NameRes::Found(n, ..) => n,
            _ => return None,
        }
    };
    matches!(view.node(n).kind(), NodeKind::Function | NodeKind::Method).then_some(n)
}

// =================================================================== Celery

pub const ROLE_CELERY_TASK: u8 = 0;
pub const ROLE_CELERY_DISPATCH: u8 = 1;

re!(celery_dispatch_re, r"\b([A-Za-z_]\w*)\s*\.\s*(?:delay|apply_async)\s*\(");
re!(celery_decorator_re, r"@\s*(?:[A-Za-z_][\w.]*\.)?(?:shared_task|task)\b");
re!(py_decl_re, r"^(?:async\s+def|def|class)\b");

pub struct CeleryScanner;

impl FileHook for CeleryScanner {
    fn name(&self) -> &'static str {
        "celery-sites"
    }
    fn applies(&self, lang: Lang, _rel: &str) -> bool {
        lang == Lang::Python
    }
    fn triggers(&self) -> &'static [&'static str] {
        &["task", ".delay(", ".apply_async("]
    }
    fn run(&self, ctx: &HookCtx<'_>, out: &mut HookOut) {
        let src = ctx.src;
        let lines: Vec<&str> = src.split('\n').collect();
        // tasks: functions with a celery decorator above their `def`
        if src.contains("task") {
            for (row, n) in ctx.rows.nodes().enumerate() {
                if n.kind() != NodeKind::Function {
                    continue;
                }
                let start = n.start_line() as usize;
                for i in (start.saturating_sub(13)..start.saturating_sub(1)).rev() {
                    let t = lines.get(i).map_or("", |l| l.trim());
                    if py_decl_re().is_match(t) {
                        break;
                    }
                    if celery_decorator_re().is_match(t) {
                        out.sites.push(site(PASS_CELERY, ROLE_CELERY_TASK, n.name(), "", Some(row as u32), n.start_line(), ""));
                        break;
                    }
                }
            }
        }
        if src.contains(".delay(") || src.contains(".apply_async(") {
            let safe = crate::frameworks::routes::strip_python_noise(src);
            let li = LineIndex::new(&safe);
            for c in celery_dispatch_re().captures_iter(&safe) {
                let line = li.line(c.get(0).unwrap().start());
                let Some(row) = enclosing_row(ctx, line, FN_KINDS) else { continue };
                out.sites.push(site(PASS_CELERY, ROLE_CELERY_DISPATCH, &c[1], "", Some(row), line, ""));
            }
        }
    }
}

pub(crate) fn celery(e: &Engine<'_>, out: &mut Out) {
    let view = e.view;
    let mut tasks: FxHashMap<&str, Vec<(NodeIdx, FileId)>> = FxHashMap::default();
    let mut dispatches = Vec::new();
    for f in view.files() {
        for s in view.sites_of(f).iter().filter(|s| s.pass == PASS_CELERY) {
            if s.role == ROLE_CELERY_TASK {
                tasks.entry(text(view, s.key_a)).or_default().push((s.node, f));
            } else {
                dispatches.push((f, s.node, s.line, text(view, s.key_a)));
            }
        }
    }
    if tasks.is_empty() {
        return;
    }
    let mut seen: FxHashSet<(NodeIdx, NodeIdx)> = FxHashSet::default();
    let mut per_file: FxHashMap<FileId, usize> = FxHashMap::default();
    for (f, disp, line, name) in dispatches {
        let Some(cands) = tasks.get(name) else { continue };
        // a name two modules declare as a task is ambiguous unless one is the caller's own
        let target = if cands.len() == 1 { Some(cands[0].0) } else { cands.iter().find(|c| c.1 == f).map(|c| c.0) };
        let Some(target) = target else { continue };
        let n = per_file.entry(f).or_default();
        if target == disp || *n >= CELERY_FANOUT_CAP || !seen.insert((disp, target)) {
            continue;
        }
        *n += 1;
        let meta = serde_json::json!({ "synthesizedBy": "celery-dispatch", "via": name, "registeredAt": registered(view, f, line) });
        out.push_meta(EdgeKind::Calls, disp, target, line, NONE, meta, order_of("celery-dispatch"));
    }
}

// =================================================================== Spring events

pub const ROLE_SPRING_LISTENER: u8 = 0;
pub const ROLE_SPRING_PUBLISH: u8 = 1;

re!(spring_publish_re, r"\.publishEvent\s*\(\s*new\s+([A-Z][A-Za-z0-9_]*)");
re!(spring_listener_re, r"@(?:EventListener|TransactionalEventListener)\b");
re!(spring_anno_type_re, r"@(?:EventListener|TransactionalEventListener)\s*\(\s*(?:classes\s*=\s*)?([A-Z][A-Za-z0-9_]*)\.class");
re!(spring_app_listener_re, r"\bApplicationListener\s*<\s*([A-Z][A-Za-z0-9_]*)");

/// First parameter type of a Java method signature (`void (XEvent e)` -> `XEvent`).
fn first_param_type(sig: &str) -> Option<String> {
    let open = sig.find('(')?;
    let close = sig[open..].find(')').map_or(sig.len(), |c| open + c);
    let inner = sig[open + 1..close].trim();
    let first = inner.split(',').next()?.trim();
    let toks: Vec<&str> = first.split_whitespace().filter(|t| *t != "final" && !t.starts_with('@')).collect();
    if toks.len() < 2 {
        return None;
    }
    let ty = toks[toks.len() - 2].split('<').next()?;
    (ty.starts_with(|c: char| c.is_ascii_uppercase()) && ty.chars().all(|c| c.is_alphanumeric() || c == '_')).then(|| ty.to_owned())
}

pub struct SpringEventScanner;

impl FileHook for SpringEventScanner {
    fn name(&self) -> &'static str {
        "spring-event-sites"
    }
    fn applies(&self, lang: Lang, _rel: &str) -> bool {
        lang == Lang::Java
    }
    fn triggers(&self) -> &'static [&'static str] {
        &["publishEvent", "EventListener", "ApplicationListener"]
    }
    fn run(&self, ctx: &HookCtx<'_>, out: &mut HookOut) {
        let src = ctx.src;
        let lines: Vec<&str> = src.split('\n').collect();
        let has_anno = src.contains("@EventListener") || src.contains("@TransactionalEventListener");
        let app_listener = spring_app_listener_re().captures(src).map(|c| c[1].to_owned());
        for (row, n) in ctx.rows.nodes().enumerate() {
            if n.kind() != NodeKind::Method {
                continue;
            }
            // the method's own annotation block: consecutive `@` lines from its first line
            let mut head = String::new();
            let start = n.start_line() as usize;
            for line in lines.iter().take((start + 7).min(lines.len())).skip(start.saturating_sub(1)) {
                let t = line.trim();
                if !t.starts_with('@') {
                    break;
                }
                head.push_str(t);
                head.push('\n');
            }
            let annotated = has_anno && spring_listener_re().is_match(&head);
            let is_app = app_listener.is_some() && n.name() == "onApplicationEvent";
            if !annotated && !is_app {
                continue;
            }
            let ty = n
                .signature()
                .and_then(first_param_type)
                .or_else(|| spring_anno_type_re().captures(&head).map(|c| c[1].to_owned()))
                .or_else(|| app_listener.clone().filter(|_| is_app));
            if let Some(ty) = ty {
                out.sites.push(site(PASS_SPRING, ROLE_SPRING_LISTENER, ty, "", Some(row as u32), n.start_line(), ""));
            }
        }
        if src.contains(".publishEvent(") {
            let safe = cg_extract::post::strip_js_comments(src);
            let li = LineIndex::new(&safe);
            for c in spring_publish_re().captures_iter(&safe) {
                let line = li.line(c.get(0).unwrap().start());
                let Some(row) = enclosing_row(ctx, line, &[NodeKind::Method, NodeKind::Function]) else { continue };
                out.sites.push(site(PASS_SPRING, ROLE_SPRING_PUBLISH, &c[1], "", Some(row), line, ""));
            }
        }
    }
}

pub(crate) fn spring_events(e: &Engine<'_>, out: &mut Out) {
    let view = e.view;
    let mut listeners: FxHashMap<&str, Vec<NodeIdx>> = FxHashMap::default();
    let mut publishes = Vec::new();
    for f in view.files() {
        for s in view.sites_of(f).iter().filter(|s| s.pass == PASS_SPRING) {
            if s.role == ROLE_SPRING_LISTENER {
                listeners.entry(text(view, s.key_a)).or_default().push(s.node);
            } else {
                publishes.push((f, s.node, s.line, text(view, s.key_a)));
            }
        }
    }
    let mut seen: FxHashSet<(NodeIdx, NodeIdx)> = FxHashSet::default();
    let mut per_file: FxHashMap<FileId, usize> = FxHashMap::default();
    for (f, disp, line, ty) in publishes {
        let Some(targets) = listeners.get(ty) else { continue };
        for &t in targets {
            let n = per_file.entry(f).or_default();
            if t == disp || *n >= SPRING_FANOUT_CAP || !seen.insert((disp, t)) {
                continue;
            }
            *n += 1;
            let meta = serde_json::json!({ "synthesizedBy": "spring-event", "via": ty, "registeredAt": registered(view, f, line) });
            out.push_meta(EdgeKind::Calls, disp, t, line, NONE, meta, order_of("spring-event"));
        }
    }
}

// =================================================================== MediatR dispatch

pub const ROLE_MEDIATR_HANDLER: u8 = 0;
pub const ROLE_MEDIATR_SEND: u8 = 1;

re!(mediatr_base_re, r"(?:IRequestHandler|INotificationHandler)\s*<\s*([A-Za-z_]\w*)");
re!(mediatr_dispatch_re, r"([A-Za-z_][\w.]*)\s*\.\s*(Send|Publish)\s*\(\s*(new\s+[A-Z]\w*|[A-Za-z_]\w*)");
re!(mediatr_receiver_re, r"(?i)mediator|sender|publisher");
re!(mediatr_handle_param_re, r"\bHandle\s*\(\s*(?:\[[^\]]*\]\s*)?(?:[A-Za-z_]\w*\.)*([A-Za-z_]\w*)");
re!(mediatr_parens_re, r"\([^()]*(?:\([^()]*\)[^()]*)*\)");
re!(mediatr_generic_arg_re, r"[<,]\s*(?:[A-Za-z_]\w*\.)*([A-Za-z_]\w*)");
re!(mediatr_own_params_re, r"<([^<>]*)>");
re!(mediatr_new_re, r"^new\s+([A-Z]\w*)");

/// The types a class's base list names as generic arguments, its own type parameters and
/// primary constructor aside (16 lines: a primary constructor can push the list down).
fn mediatr_base_types(lines: &[&str], class_start: usize) -> Vec<String> {
    let head = lines.iter().skip(class_start.saturating_sub(1)).take(16).copied().collect::<Vec<_>>().join("\n");
    let head = head.split('{').next().unwrap_or("");
    let text = mediatr_parens_re().replace_all(head, " ");
    let Some(colon) = text.find(':') else { return Vec::new() };
    let own: Vec<&str> =
        mediatr_own_params_re().captures(&text[..colon]).map(|c| c.get(1).unwrap().as_str().split(',').map(str::trim).collect()).unwrap_or_default();
    mediatr_generic_arg_re().captures_iter(&text[colon + 1..]).map(|c| c[1].to_owned()).filter(|t| !own.contains(&t.as_str())).collect()
}

/// The type sent at a dispatch: an inline `new X(..)`, else within the enclosing method an
/// `arg = new X(..)` or a mapper's `arg = m.Map<X>(..)` (either wins), else a declared `X arg`.
fn mediatr_arg_type(arg: &str, lines: &[&str], method_start: u32, line: u32) -> Option<String> {
    if let Some(c) = mediatr_new_re().captures(arg) {
        return Some(c[1].to_owned());
    }
    if !arg.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    let assign = Regex::new(&format!(r"\b{arg}\b\s*=\s*new\s+([A-Z]\w*)")).ok()?;
    let mapped = Regex::new(&format!(r"\b{arg}\b\s*=[^;]*?\.\s*\w+<(?:[^<>;]*,\s*)?(?:\w+\.)*([A-Z]\w*)>\s*\(")).ok()?;
    let decl = Regex::new(&format!(r"\b([A-Z]\w*)\b\s+{arg}\b")).ok()?;
    let mut declared = None;
    for l in lines.iter().take(line as usize).skip(method_start.saturating_sub(1) as usize) {
        if let Some(c) = assign.captures(l).or_else(|| mapped.captures(l)) {
            return Some(c[1].to_owned());
        }
        if declared.is_none() {
            declared = decl.captures(l).map(|c| c[1].to_owned());
        }
    }
    declared
}

/// MediatR decouples `.Send(x)` / `.Publish(x)` from the `Handle` method through the type of x
/// (upstream `mediatrDispatchEdges`). The receiver must be mediator-ish and the type a known
/// handler's. A handler is what names the interface, or any class whose base list repeats its
/// `Handle`'s first parameter: projects derive their own `ICommandHandler<T>`.
pub struct MediatrScanner;

impl FileHook for MediatrScanner {
    fn name(&self) -> &'static str {
        "mediatr-sites"
    }
    fn applies(&self, lang: Lang, _rel: &str) -> bool {
        lang == Lang::CSharp
    }
    fn triggers(&self) -> &'static [&'static str] {
        &[".Send(", ".Publish(", "Handle("]
    }
    fn run(&self, ctx: &HookCtx<'_>, out: &mut HookOut) {
        let src = ctx.src;
        if src.contains(".Send(") || src.contains(".Publish(") {
            let safe = cg_extract::post::strip_js_comments(src);
            let li = LineIndex::new(&safe);
            let lines: Vec<&str> = safe.split('\n').collect();
            for c in mediatr_dispatch_re().captures_iter(&safe) {
                if !mediatr_receiver_re().is_match(&c[1]) {
                    continue;
                }
                let line = li.line(c.get(0).unwrap().start());
                let Some(row) = enclosing_row(ctx, line, FN_KINDS) else { continue };
                let Some(from) = ctx.rows.nodes().nth(row as usize) else { continue };
                if let Some(ty) = mediatr_arg_type(&c[3], &lines, from.start_line(), line) {
                    out.sites.push(site(PASS_MEDIATR, ROLE_MEDIATR_SEND, ty, &c[2], Some(row), line, ""));
                }
            }
        }
        if !src.contains("Handle(") {
            return;
        }
        let lines: Vec<&str> = src.split('\n').collect();
        let handles: Vec<(u32, u32)> = ctx
            .rows
            .nodes()
            .enumerate()
            .filter(|(_, n)| n.kind() == NodeKind::Method && n.name() == "Handle")
            .map(|(row, n)| (row as u32, n.start_line()))
            .collect();
        for cls in ctx.rows.nodes().filter(|n| n.kind() == NodeKind::Class) {
            let inside: Vec<(u32, u32)> = handles.iter().copied().filter(|(_, l)| (cls.start_line()..=cls.end_line()).contains(l)).collect();
            let Some(&(first, _)) = inside.first() else { continue };
            let start = cls.start_line() as usize;
            let decl = lines.iter().skip(start - 1).take(4).copied().collect::<Vec<_>>().join("\n");
            let mut add = |ty: &str, row: u32, line: u32| out.sites.push(site(PASS_MEDIATR, ROLE_MEDIATR_HANDLER, ty, "", Some(row), line, ""));
            if let Some(c) = mediatr_base_re().captures(decl.split('{').next().unwrap_or("")) {
                add(&c[1], first, cls.start_line());
            }
            let bases = mediatr_base_types(&lines, start);
            for &(row, line) in &inside {
                let head = lines.iter().skip(line as usize - 1).take(3).copied().collect::<Vec<_>>().join(" ");
                if let Some(c) = mediatr_handle_param_re().captures(&head).filter(|c| bases.iter().any(|b| *b == c[1])) {
                    add(&c[1], row, line);
                }
            }
        }
    }
}

/// How many leading folders two paths share.
fn shared_folders(a: &str, b: &str) -> usize {
    let (a, b): (Vec<&str>, Vec<&str>) = (a.split('/').collect(), b.split('/').collect());
    a.iter().zip(&b).take(a.len().min(b.len()) - 1).take_while(|(x, y)| x == y).count()
}

pub(crate) fn mediatr_dispatch(e: &Engine<'_>, out: &mut Out) {
    let view = e.view;
    let mut handlers: FxHashMap<&str, Vec<NodeIdx>> = FxHashMap::default();
    let mut sends = Vec::new();
    for f in view.files() {
        for s in view.sites_of(f).iter().filter(|s| s.pass == PASS_MEDIATR) {
            if s.role == ROLE_MEDIATR_HANDLER {
                let known = handlers.entry(text(view, s.key_a)).or_default();
                if !known.contains(&s.node) {
                    known.push(s.node);
                }
            } else {
                sends.push((f, s.node, s.line, text(view, s.key_a), text(view, s.key_b)));
            }
        }
    }
    let mut seen: FxHashSet<(NodeIdx, NodeIdx)> = FxHashSet::default();
    let mut per_file: FxHashMap<FileId, usize> = FxHashMap::default();
    for (f, disp, line, ty, verb) in sends {
        let Some(targets) = handlers.get(ty) else { continue };
        let path = view.file(f).path();
        let shared = |t: NodeIdx| shared_folders(path, view.file(view.node(t).file()).path());
        // Solutions sit side by side with same-named requests: a Send has one handler, the
        // nearest; a Publish fans out, but stays in this top-level folder when any is there.
        let best = targets.iter().map(|&t| shared(t)).max().unwrap_or(0);
        let n = per_file.entry(f).or_default();
        if *n >= MEDIATR_FANOUT_CAP {
            continue;
        }
        for &t in targets {
            let reachable = targets.len() < 2 || if verb == "Send" { shared(t) == best } else { best == 0 || shared(t) > 0 };
            if !reachable || t == disp || !seen.insert((disp, t)) {
                continue;
            }
            *n += 1;
            let meta = serde_json::json!({ "synthesizedBy": "mediatr-dispatch", "via": ty, "registeredAt": registered(view, f, line) });
            out.push_meta(EdgeKind::Calls, disp, t, line, NONE, meta, order_of("mediatr-dispatch"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_paths() {
        let routes = vec![
            HttpRoute { node: NodeIdx(1), method: "GET".into(), segs: vec!["api".into(), "users".into()], excluded: vec![] },
            HttpRoute { node: NodeIdx(2), method: "GET".into(), segs: vec!["api".into(), "users".into(), ":id".into()], excluded: vec![] },
            HttpRoute { node: NodeIdx(3), method: "GET".into(), segs: vec!["api".into(), "users".into(), "top".into()], excluded: vec![] },
            HttpRoute { node: NodeIdx(4), method: "POST".into(), segs: vec!["api".into(), "users".into()], excluded: vec![] },
        ];
        let m = |method: &str, raw: &str, base: Option<&str>| {
            client_path(raw, base).and_then(|(s, suffix)| match_http(method, &s, suffix, &routes)).map(|r| r.node)
        };
        assert_eq!(m("GET", "/api/users", None), Some(NodeIdx(1)));
        assert_eq!(m("POST", "/api/users", None), Some(NodeIdx(4)));
        assert_eq!(m("GET", &format!("/api/users/{HOLE}"), None), Some(NodeIdx(2)));
        assert_eq!(m("GET", "/api/users/top", None), Some(NodeIdx(3)));
        assert_eq!(m("GET", "users", Some("/api")), Some(NodeIdx(1)));
        assert_eq!(m("GET", &format!("{HOLE}/users/7"), None), Some(NodeIdx(2)));
        assert_eq!(m("GET", "https://x.io/api/users?x=1", None), Some(NodeIdx(1)));
        assert_eq!(m("DELETE", "/api/users", None), None);
        assert_eq!(m("GET", "users", None), None);
    }

    #[test]
    fn events_and_params() {
        assert!(event_matches("user.*", "user.created"));
        assert!(!event_matches("user.*", "user.created.x"));
        assert!(event_matches("user.**", "user.created.x"));
        assert!(!event_matches("order.*", "user.created"));
        assert_eq!(first_param_type("void (final UserCreated e)").as_deref(), Some("UserCreated"));
        assert_eq!(first_param_type("void ()"), None);
        assert_eq!(shape_of("this.eventEmitter"), Some("bus"));
        assert_eq!(shape_of("this.server.to(room)"), Some("socket"));
        assert_eq!(shape_of("foo"), None);
        assert_eq!(registry_entries(" create: createUser, remove: removeUser, x: 5, y: () => 1 "), vec!["createUser", "removeUser"]);
    }
}
