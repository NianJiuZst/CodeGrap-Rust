//! "Incremental equals full" for the capabilities ported from upstream: overloads picked by
//! arguments, Go interfaces by signature, framework routes and navigation, and the dynamic /
//! cross-tier passes. Each targeted edit changes one side of a link those passes draw; the
//! random edits mix them. After every step the synced view must equal a fresh index.

use cg_resolve::{index_project, IndexOptions};
use cg_store::{dump, Store};
use cg_sync::{sync, SyncOptions};
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

const FILES: &[(&str, &str)] = &[
    (
        "package.json",
        r#"{"name":"parity","dependencies":{"next":"15.0.0","react":"19","react-router-dom":"6","@tanstack/react-router":"1","@nestjs/core":"10","express":"4","axios":"1"}}"#,
    ),
    ("app/page.tsx", "import Link from 'next/link';\nexport default function Home() {\n  return <Link href=\"/about\">about</Link>;\n}\n"),
    ("app/about/page.tsx", "export default function About() { return null }\n"),
    ("app/users/[id]/page.tsx", "export default function User() { return null }\n"),
    ("app/api/items/route.ts", "export async function GET() { return Response.json([]) }\n"),
    ("middleware.ts", "export function middleware(req) { return req; }\nexport const config = { matcher: ['/about/:path*', '/users/:id'] };\n"),
    ("web/Nav.tsx", "import { useRouter } from 'next/navigation';\nexport function Nav() {\n  const router = useRouter();\n  const go = () => router.push('/users/7');\n  return null;\n}\n"),
    (
        "spa/src/App.tsx",
        "import { Routes, Route } from 'react-router-dom';\nimport { Home, Team } from './pages';\nexport function App() {\n  return (\n    <Routes>\n      <Route path=\"/\" element={<Home />} />\n      <Route path=\"/teams/:id\" element={<Team />} />\n    </Routes>\n  );\n}\n",
    ),
    ("spa/src/pages.tsx", "import { Link, useNavigate } from 'react-router-dom';\nexport function Home() {\n  const navigate = useNavigate();\n  const open = () => navigate('/teams/1');\n  return <Link to=\"/teams/2\">t</Link>;\n}\nexport function Team() { return null; }\n"),
    ("nest/items.controller.ts", "import { Controller, Get } from '@nestjs/common';\n@Controller('items')\nexport class ItemsController {\n  @Get(':id')\n  one() { return 1; }\n}\n"),
    ("server/app.js", "const app = require('express')();\nfunction listItems(req, res) {}\napp.get('/api/list', listItems);\n"),
    ("client/http.ts", "import axios from 'axios';\nexport const api = axios.create({ baseURL: '/api' });\n"),
    ("client/items.ts", "import { api } from './http';\nexport function load() {\n  return fetch('/api/list');\n}\nexport function one() {\n  return api.get('/items');\n}\n"),
    (
        "store/store.ts",
        "export class Store {\n  private listeners: Array<() => void> = [];\n  onChange(cb: () => void) {\n    this.listeners.push(cb);\n  }\n  notify() {\n    this.listeners.forEach((l) => l());\n  }\n}\n",
    ),
    ("store/view.ts", "import { Store } from './store';\nexport class View {\n  constructor(private store: Store) {}\n  mount() {\n    this.store.onChange(this.paint);\n  }\n  paint() {}\n}\n"),
    ("reg/commands.ts", "export function createOne() {}\nexport function removeOne() {}\nconst handlers = { create: createOne, remove: removeOne };\nexport function run(a: string) {\n  handlers[a]();\n}\n"),
    ("q/producer.ts", "export class Producer {\n  constructor(private readonly mailQueue: any) {}\n  async welcome() {\n    await this.mailQueue.add('welcome', {});\n  }\n}\n"),
    ("q/processor.ts", "@Processor('email')\nexport class MailProcessor {\n  @Process('welcome')\n  async sendWelcome(job) {}\n}\n"),
    ("bus/svc.ts", "export class Svc {\n  constructor(private eventEmitter: any) {}\n  create() {\n    this.eventEmitter.emit('order.created', {});\n  }\n}\nexport class Audit {\n  @OnEvent('order.created')\n  onOrder(p) {}\n}\n"),
    (
        "java/p/Api.java",
        "package p;\npublic class Api {\n    public void put(String key) {}\n    public void put(int key) {}\n    public void put(String key, int value) {}\n}\n",
    ),
    ("java/p/Use.java", "package p;\npublic class Use {\n    private final Api api = new Api();\n    void run() {\n        api.put(\"k\");\n        api.put(1);\n        api.put(\"k\", 2);\n    }\n}\n"),
    ("java/p/Created.java", "package p;\npublic class Created {}\n"),
    ("java/p/Pub.java", "package p;\npublic class Pub {\n    private Publisher publisher;\n    public void fire() {\n        publisher.publishEvent(new Created());\n    }\n}\n"),
    ("java/p/Listener.java", "package p;\npublic class Listener {\n    @EventListener\n    public void on(Created e) {}\n}\n"),
    ("go/go.mod", "module example.com/m\n\ngo 1.21\n"),
    (
        "go/x/x.go",
        "package x\n\ntype Matcher interface {\n\tMatch(path string) bool\n}\n\ntype Prefix struct{}\n\nfunc (p Prefix) Match(path string) bool { return true }\n\ntype Count struct{}\n\nfunc (c Count) Match(n int) bool { return false }\n",
    ),
    ("py/tasks.py", "from celery import shared_task\n\n@shared_task\ndef send(to):\n    pass\n"),
    ("py/views.py", "from .tasks import send\n\ndef signup():\n    send.delay('a')\n"),
    ("cjs/utils.js", "exports.compileETag = function(val) {\n  return val;\n};\nexports.compileTrust = function(val) {\n  return val;\n};\n"),
    ("cjs/app.js", "var compileETag = require('./utils').compileETag;\nfunction setup(v) {\n  compileETag(v);\n}\nmodule.exports = setup;\n"),
    ("barrel/core.ts", "export function _lt() {}\nexport function _gte() {}\n"),
    ("barrel/checks.ts", "export {\n  _lt as lt,\n  _gte as gte,\n} from './core';\n"),
    ("twin/schema.ts", "export interface Shape {\n  kind: string;\n}\nexport const Shape: any = null;\n"),
    ("twin/use.ts", "import { Shape } from './schema';\nexport function make() {\n  return new Shape();\n}\n"),
    ("win/filter.js", "window.Filter = {\n  init: function () {}\n};\n"),
    ("win/use.js", "function setup() {\n  Filter.init();\n}\n"),
    ("esm/anon.ts", "export default function () {\n  return 1;\n}\n"),
    ("esm/use.ts", "import anon from './anon';\nexport function run() {\n  return anon();\n}\n"),
    ("ov/doc.ts", "export class Doc {\n  write(fn: () => void): void;\n  write(line: string): void;\n  write(arg: any) {}\n}\nexport function emit(doc: Doc) {\n  doc.write('a');\n}\n"),
    ("fwd/express.js", "var Router = require('router');\nexports = module.exports = createApplication;\nfunction createApplication() {}\nexports.Router = Router;\n"),
    ("fwd/use.js", "var express = require('./express');\nfunction make() {\n  return express.Router();\n}\n"),
];

/// One-sided edits of the links above: (file, from, to).
const EDITS: &[(&str, &str, &str)] = &[
    ("app/page.tsx", "href=\"/about\"", "href=\"/users/3\""),
    ("app/page.tsx", "href=\"/users/3\"", "href=\"/about\""),
    ("middleware.ts", "'/about/:path*', ", ""),
    ("middleware.ts", "matcher: [", "matcher: ['/about/:path*', "),
    ("web/Nav.tsx", "'/users/7'", "'/about'"),
    ("spa/src/pages.tsx", "'/teams/1'", "'/nowhere'"),
    ("spa/src/App.tsx", "/teams/:id", "/team/:id"),
    ("nest/items.controller.ts", "@Controller('items')", "@Controller('things')"),
    ("server/app.js", "'/api/list'", "'/api/all'"),
    ("client/items.ts", "fetch('/api/list')", "fetch('/api/all')"),
    ("store/store.ts", "this.listeners.push(cb)", "this.others.push(cb)"),
    ("store/view.ts", "this.paint", "this.repaint"),
    ("reg/commands.ts", "remove: removeOne", "remove: createOne"),
    ("q/processor.ts", "@Process('welcome')", "@Process('reset')"),
    ("bus/svc.ts", "@OnEvent('order.created')", "@OnEvent('order.*')"),
    ("java/p/Api.java", "public void put(int key) {}", "public void put(long key) {}"),
    ("java/p/Api.java", "public void put(String key) {}\n", ""),
    ("java/p/Listener.java", "on(Created e)", "on(Object e)"),
    ("go/x/x.go", "Match(n int) bool", "Match(path string) bool"),
    ("go/x/x.go", "func (p Prefix) Match(path string)", "func (p Prefix) Match(path []byte)"),
    ("py/tasks.py", "@shared_task\n", ""),
    ("app/about/page.tsx", "About", "AboutPage"),
    ("cjs/utils.js", "exports.compileETag =", "exports.compileETagX ="),
    ("cjs/utils.js", "exports.compileETagX =", "exports.compileETag ="),
    ("cjs/app.js", ").compileETag;", ").compileTrust;"),
    ("barrel/core.ts", "export function _lt()", "export function _ltX()"),
    ("barrel/core.ts", "export function _ltX()", "export function _lt()"),
    ("barrel/checks.ts", "_gte as gte,\n", ""),
    // `import { Shape }` names whichever of the twins comes first
    (
        "twin/schema.ts",
        "export interface Shape {\n  kind: string;\n}\nexport const Shape: any = null;\n",
        "export const Shape: any = null;\nexport interface Shape {\n  kind: string;\n}\n",
    ),
    (
        "twin/schema.ts",
        "export const Shape: any = null;\nexport interface Shape {\n  kind: string;\n}\n",
        "export interface Shape {\n  kind: string;\n}\nexport const Shape: any = null;\n",
    ),
    // `Filter.init()` reaches the `window.Filter` global of another script
    ("win/filter.js", "window.Filter =", "window.Filtr ="),
    ("win/filter.js", "window.Filtr =", "window.Filter ="),
    ("win/filter.js", "  init: function", "  start: function"),
    ("win/filter.js", "  start: function", "  init: function"),
    // `import anon` names the file whatever its default export is called
    ("esm/anon.ts", "export default function ()", "export default function named()"),
    ("esm/anon.ts", "export default function named()", "export default function ()"),
    // `doc.write(..)` names the first overload
    ("ov/doc.ts", "  write(fn: () => void): void;\n  write(line: string): void;\n", "  write(line: string): void;\n  write(fn: () => void): void;\n"),
    ("ov/doc.ts", "  write(line: string): void;\n  write(fn: () => void): void;\n", "  write(fn: () => void): void;\n  write(line: string): void;\n"),
    // `express.Router()` reaches the variable `exports.Router` forwards
    ("fwd/express.js", "var Router = require('router');", "var Rtr = require('router');"),
    ("fwd/express.js", "var Rtr = require('router');", "var Router = require('router');"),
    ("fwd/express.js", "exports.Router = Router;", "exports.Router = createApplication;"),
    ("fwd/express.js", "exports.Router = createApplication;", "exports.Router = Router;"),
];

/// Shadowing a sync must follow: each edit changes which declaration a call or `new` reaches.
const SHADOW_FILES: &[(&str, &str)] = &[
    ("Cargo.toml", "[package]\nname = \"shadow\"\n"),
    (
        "src/main.rs",
        "mod a_tr;\nmod z_imp;\npub trait Action { fn run(&self) -> u8; }\npub struct Worker;\npub struct Owner { worker: std::sync::Arc<Worker> }\nimpl Owner { pub fn go(&self) -> u8 { self.worker.run() } }\npub fn direct(w: &Worker) -> u8 { w.run() }\n",
    ),
    ("src/a_tr.rs", "use crate::{Action, Worker};\nimpl Action for Worker { fn run(&self) -> u8 { 1 } }\n"),
    ("src/z_imp.rs", "use crate::Worker;\nimpl Worker { pub fn run(&self) -> u8 { 2 } }\n"),
    ("py/base.py", "class Left:\n    def run(self):\n        pass\n\nclass Right:\n    def run(self):\n        pass\n\nclass Derived(Right, Left):\n    pass\n"),
    ("py/use.py", "from .base import Derived\n\ndef caller():\n    Derived().run()\n"),
    ("js/app.ts", "class Factory {\n  run() {}\n}\nexport function nested() {\n  class Factory {\n    run() {}\n  }\n  return new Factory().run();\n}\n"),
];

const SHADOW_EDITS: &[(&str, &str, &str)] = &[
    // an inherent `Worker::run` shadows the trait's
    ("src/z_imp.rs", "pub fn run(&self)", "pub fn act(&self)"),
    ("src/z_imp.rs", "pub fn act(&self)", "pub fn run(&self)"),
    // the same method, at the same place, now inherent
    ("src/a_tr.rs", "impl Action for Worker", "impl /*       */Worker"),
    ("src/a_tr.rs", "impl /*       */Worker", "impl Action for Worker"),
    // a field behind `Arc` and a plain one
    ("src/main.rs", "worker: std::sync::Arc<Worker>", "worker: Worker"),
    ("src/main.rs", "worker: Worker", "worker: std::sync::Arc<Worker>"),
    // Python's method resolution order follows the bases as written
    ("py/base.py", "class Derived(Right, Left):", "class Derived(Left, Right):"),
    ("py/base.py", "class Derived(Left, Right):", "class Derived(Right, Left):"),
    // a function's own class shadows the file's
    ("js/app.ts", "  class Factory {\n    run() {}\n  }\n", ""),
    ("js/app.ts", "export function nested() {\n", "export function nested() {\n  class Factory {\n    run() {}\n  }\n"),
];

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn write(root: &Path, rel: &str, content: &str, step: u64) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, content).unwrap();
    let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + step * 7) + Duration::from_nanos(step * 1013);
    fs::OpenOptions::new().write(true).open(&p).unwrap().set_modified(t).unwrap();
}

fn copy_tree(src: &Path, dst: &Path) {
    for e in fs::read_dir(src).unwrap().flatten() {
        if e.file_name() == ".cgr" {
            continue;
        }
        let (s, d) = (e.path(), dst.join(e.file_name()));
        if s.is_dir() {
            fs::create_dir_all(&d).unwrap();
            copy_tree(&s, &d);
        } else {
            fs::copy(&s, &d).unwrap();
        }
    }
}

fn opts() -> IndexOptions {
    IndexOptions { threads: Some(2), ..Default::default() }
}

fn canonical_of(root: &Path) -> Vec<String> {
    let store = Store::locate(root).expect("index");
    let snap = store.open_snapshot().expect("snapshot");
    dump::canonical(&snap.view)
}

fn full_canonical(src: &Path) -> Vec<String> {
    let oracle = tempfile::tempdir().unwrap();
    copy_tree(src, oracle.path());
    index_project(oracle.path(), &opts()).unwrap();
    canonical_of(oracle.path())
}

fn first_diff(a: &[String], b: &[String]) -> String {
    let (sa, sb): (std::collections::BTreeSet<&String>, std::collections::BTreeSet<&String>) = (a.iter().collect(), b.iter().collect());
    let mut out = format!("{} vs {} lines\n-- only incremental:\n", a.len(), b.len());
    for l in sa.difference(&sb).take(8) {
        out.push_str(&format!("  {l}\n"));
    }
    out.push_str("-- only full:\n");
    for l in sb.difference(&sa).take(8) {
        out.push_str(&format!("  {l}\n"));
    }
    out
}

fn setup() -> tempfile::TempDir {
    setup_with(FILES)
}

fn setup_with(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (i, (f, c)) in files.iter().enumerate() {
        write(dir.path(), f, c, i as u64);
    }
    index_project(dir.path(), &opts()).unwrap();
    dir
}

#[test]
fn fixture_draws_every_ported_link() {
    let dir = setup();
    let lines = canonical_of(dir.path());
    let text = lines.join("\n");
    for by in [
        "next-link",
        "next-navigation",
        "next-proxy",
        "react-router-link",
        "react-router-navigation",
        "http-client",
        "callback",
        "object-registry",
        "queue-job",
        "event-bus",
        "spring-event",
        "celery-dispatch",
        "go-implements",
    ] {
        assert!(text.contains(by), "no {by} edge in the fixture:\n{text}");
    }
}

#[test]
fn targeted_edits_match_full_rebuild() {
    edits_match_full_rebuild(FILES, EDITS);
}

#[test]
fn shadowing_edits_match_full_rebuild() {
    edits_match_full_rebuild(SHADOW_FILES, SHADOW_EDITS);
}

fn edits_match_full_rebuild(files: &[(&str, &str)], edits: &[(&str, &str, &str)]) {
    let dir = setup_with(files);
    let root = dir.path();
    for (step, (f, from, to)) in edits.iter().enumerate() {
        let cur = fs::read_to_string(root.join(f)).unwrap();
        if !cur.contains(from) {
            continue;
        }
        write(root, f, &cur.replacen(from, to, 1), 200 + step as u64);
        let rep = sync(root, &SyncOptions { index: opts(), ..Default::default() }).unwrap();
        assert!(rep.full_rebuild.is_none(), "edit {step} fell back to a full rebuild: {:?}", rep.full_rebuild);
        let (inc, full) = (canonical_of(root), full_canonical(root));
        assert!(inc == full, "edit {step} ({f}: {from:?} -> {to:?}) diverged:\n{}", first_diff(&inc, &full));
    }
}

fn random_sequence(seed: u64, steps: usize) {
    let dir = setup();
    let root = dir.path();
    let mut rng = Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1);
    let mut log = Vec::new();
    let mut deltas = 0;
    for step in 1..=steps as u64 {
        let stamp = 500 + step;
        match rng.below(6) {
            0 | 1 => {
                let (f, from, to) = EDITS[rng.below(EDITS.len())];
                if let Ok(cur) = fs::read_to_string(root.join(f)) {
                    if cur.contains(from) {
                        log.push(format!("edit {f}: {from:?} -> {to:?}"));
                        write(root, f, &cur.replacen(from, to, 1), stamp);
                    }
                }
            }
            2 => {
                let (f, c) = FILES[rng.below(FILES.len())];
                if f != "package.json" && f != "go/go.mod" {
                    log.push(format!("restore {f}"));
                    write(root, f, c, stamp);
                }
            }
            3 => {
                let (f, _) = FILES[rng.below(FILES.len())];
                if f != "package.json" && f != "go/go.mod" && root.join(f).exists() {
                    log.push(format!("remove {f}"));
                    fs::remove_file(root.join(f)).unwrap();
                }
            }
            4 => {
                let (f, _) = FILES[rng.below(FILES.len())];
                if let Ok(cur) = fs::read_to_string(root.join(f)) {
                    if f.ends_with(".ts") || f.ends_with(".tsx") || f.ends_with(".java") || f.ends_with(".go") || f.ends_with(".js") {
                        log.push(format!("comment {f}"));
                        write(root, f, &format!("// note\n{cur}"), stamp);
                    }
                }
            }
            _ => {
                let n = rng.below(3);
                let (rel, content) = match n {
                    0 => (
                        format!("app/gen{step}/page.tsx"),
                        format!("import Link from 'next/link';\nexport default function Gen{step}() {{ return <Link href=\"/about\">a</Link>; }}\n"),
                    ),
                    1 => (format!("client/gen{step}.ts"), "export function gen() { return fetch('/api/list'); }\n".to_owned()),
                    _ => (format!("java/p/Gen{step}.java"), format!("package p;\npublic class Gen{step} {{\n    void go(Api a) {{ a.put(\"x\", 1); }}\n}}\n")),
                };
                log.push(format!("add {rel}"));
                write(root, &rel, &content, stamp);
            }
        }
        let rep = sync(root, &SyncOptions { index: opts(), ..Default::default() }).unwrap_or_else(|e| panic!("sync failed at step {step}: {e}\n{log:#?}"));
        if rep.full_rebuild.is_none() && !rep.up_to_date {
            deltas += 1;
        }
        let (inc, full) = (canonical_of(root), full_canonical(root));
        assert!(inc == full, "seed {seed} step {step} diverged:\n{}\nlog: {log:#?}", first_diff(&inc, &full));
    }
    assert!(deltas > steps / 3, "most steps should have used a delta layer ({deltas}/{steps})");
}

#[test]
fn random_edits_match_full_rebuild_a() {
    random_sequence(11, 40);
}

#[test]
fn random_edits_match_full_rebuild_b() {
    random_sequence(29, 40);
}
