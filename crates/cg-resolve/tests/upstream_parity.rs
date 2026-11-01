//! Capabilities ported from upstream CodeGraph: overload selection by arguments, Go structural
//! interfaces by signature, framework routes and navigation, and dynamic / cross-tier edges.
//! Every positive case has a negative twin that must stay unlinked.

use cg_core::{EdgeKind, NodeIdx};
use cg_resolve::{build_seg, IndexOptions};
use cg_store::{dump, Seg, View};
use std::sync::Arc;

fn project(files: &[(&str, &str)]) -> (tempfile::TempDir, View) {
    let dir = tempfile::tempdir().unwrap();
    for (p, c) in files {
        let path = dir.path().join(p);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, c).unwrap();
    }
    let built = build_seg(dir.path(), &IndexOptions { threads: Some(2), ..Default::default() }).unwrap();
    let seg: Arc<Seg> = Arc::new(built.seg);
    let root = dir.path().to_path_buf();
    (dir, View::new(seg).with_root(root))
}

/// Edges as `from -kind-> to@line` where `to@line` is the target's start line, so overloads
/// sharing a qualified name stay apart.
fn edges_with_lines(v: &View) -> Vec<String> {
    let mut out = Vec::new();
    for f in v.files() {
        for n in v.nodes_in_file(f) {
            let n = NodeIdx(n);
            for e in v.out_edges(n) {
                if e.kind == EdgeKind::Contains {
                    continue;
                }
                out.push(format!("{} -{}-> {}@{}", dump::node_label(v, n), e.kind, dump::node_label(v, e.dst), v.node(e.dst).start_line()));
            }
        }
    }
    out.sort();
    out
}

fn meta_of(v: &View, from: &str, to: &str) -> Vec<String> {
    let mut out = Vec::new();
    for f in v.files() {
        for n in v.nodes_in_file(f) {
            let n = NodeIdx(n);
            if dump::node_label(v, n) != from {
                continue;
            }
            for e in v.out_edges(n) {
                if dump::node_label(v, e.dst) == to {
                    out.push(v.edge_meta(e.eid).unwrap_or("").to_owned());
                }
            }
        }
    }
    out
}

#[test]
fn java_overloads_follow_argument_count_and_types() {
    let (_d, v) = project(&[
        ("src/p/Node.java", "package p;\npublic class Node {}\n"),
        ("src/p/Element.java", "package p;\npublic class Element extends Node {}\n"),
        (
            "src/p/Api.java",
            r#"package p;
import java.util.function.Function;
public class Api {
    public void put(String key) {}
    public void put(int key) {}
    public void put(String key, int value) {}
    public void add(Node n) {}
    public void add(String s) {}
    public void on(Runnable r) {}
    public void on(Function<String, String> f) {}
    public void log(String fmt, Object... args) {}
    public void log(String msg) {}
    public void size(long n) {}
    public void size(Object o) {}
}
"#,
        ),
        (
            "src/p/Use.java",
            r#"package p;
public class Use {
    private final Api api = new Api();
    private int count;
    void run(Element el, String label) {
        api.put("k");
        api.put(1);
        api.put("k", 2);
        api.add(el);
        api.add(label);
        api.on(() -> {});
        api.on(s -> s);
        api.log("x", 1, 2);
        api.log("x");
        api.size(count);
    }
}
"#,
        ),
    ]);
    let edges = edges_with_lines(&v);
    let run = "src/p/Use.java::p::Use::run -calls-> src/p/Api.java::p::Api";
    for (method, line) in [("put", 4), ("put", 5), ("put", 6), ("add", 7), ("add", 8), ("on", 9), ("on", 10), ("log", 11), ("log", 12), ("size", 13)] {
        let needle = format!("{run}::{method}@{line}");
        assert!(edges.contains(&needle), "missing {needle}: {edges:#?}");
    }
    // the primitive field widens to `long` and is no `Object` only by boxing: `size(long)` wins
    assert!(!edges.contains(&format!("{run}::size@14")), "{edges:#?}");
}

#[test]
fn kotlin_and_typescript_overloads() {
    let (_d, v) = project(&[
        (
            "src/k/K.kt",
            "package k\nclass K {\n  fun f(s: String) {}\n  fun f(n: Int, m: Long = 0L) {}\n  fun g(cb: (Int) -> Unit) {}\n  fun g(cb: (Int, Int) -> Unit) {}\n  fun run() {\n    f(\"x\")\n    f(1)\n    g { a -> }\n    g { a, b -> }\n  }\n}\n",
        ),
        (
            "src/t.ts",
            "export class T {\n  f(s: string): void;\n  f(n: number, m?: number): void;\n  f(a: any, b?: any): void {}\n  run(n: number) {\n    this.f('x');\n    this.f(n, 2);\n  }\n}\n",
        ),
    ]);
    let edges = edges_with_lines(&v);
    let k = "src/k/K.kt::k::K::run -calls-> src/k/K.kt::k::K";
    for needle in [format!("{k}::f@3"), format!("{k}::f@4"), format!("{k}::g@5"), format!("{k}::g@6")] {
        assert!(edges.contains(&needle), "missing {needle}: {edges:#?}");
    }
    let t = "src/t.ts::T::run -calls-> src/t.ts::T::f";
    assert!(edges.contains(&format!("{t}@2")), "{edges:#?}");
    assert!(edges.contains(&format!("{t}@3")), "{edges:#?}");
}

#[test]
fn go_interfaces_need_matching_signatures() {
    let (_d, v) = project(&[
        ("go.mod", "module example.com/m\n\ngo 1.21\n"),
        (
            "httpm/m.go",
            "package httpm\n\nimport \"net/http\"\n\ntype RequestMatcher interface {\n\tMatch(r *http.Request) bool\n}\n\ntype PathMatcher struct{}\n\nfunc (m PathMatcher) Match(r *http.Request) bool { return true }\n",
        ),
        (
            "tlsm/m.go",
            "package tlsm\n\nimport \"crypto/tls\"\n\ntype SNI struct{}\n\nfunc (m SNI) Match(hello *tls.ClientHelloInfo) bool { return true }\n\ntype Counter struct{}\n\nfunc (c Counter) Match(r *Request, n int) bool { return true }\n",
        ),
        (
            "cache/c.go",
            "package cache\n\ntype Getter[K comparable, V any] interface {\n\tGet(k K) (V, bool)\n}\n\ntype Map[K comparable, V any] struct{ m map[K]V }\n\nfunc (c *Map[K, V]) Get(k K) (V, bool) { var v V; return v, false }\n\ntype Base struct{}\n\nfunc (b *Base) Close() error { return nil }\n\ntype Closer interface {\n\tClose() error\n}\n\ntype Wrapped struct {\n\tBase\n}\n\ntype refresher interface {\n\trefresh() error\n}\n\ntype local struct{}\n\nfunc (l local) refresh() error { return nil }\n",
        ),
        ("other/o.go", "package other\n\ntype remote struct{}\n\nfunc (r remote) refresh() error { return nil }\n"),
    ]);
    let edges = dump::dump_edges(&v);
    let has = |needle: &str| edges.iter().any(|e| e == needle);
    assert!(has("httpm/m.go::PathMatcher -implements-> httpm/m.go::RequestMatcher"), "{edges:#?}");
    assert!(!has("tlsm/m.go::SNI -implements-> httpm/m.go::RequestMatcher"), "another parameter type: {edges:#?}");
    assert!(!has("tlsm/m.go::Counter -implements-> httpm/m.go::RequestMatcher"), "another parameter count: {edges:#?}");
    assert!(has("cache/c.go::Map -implements-> cache/c.go::Getter"), "type parameters stay open: {edges:#?}");
    assert!(has("cache/c.go::Wrapped -implements-> cache/c.go::Closer"), "promoted through embedding: {edges:#?}");
    assert!(has("cache/c.go::local -implements-> cache/c.go::refresher"), "{edges:#?}");
    assert!(!has("other/o.go::remote -implements-> cache/c.go::refresher"), "an unexported method of another package: {edges:#?}");
}

#[test]
fn next_pages_navigation_links_and_proxy() {
    let (_d, v) = project(&[
        ("package.json", r#"{"name":"web","dependencies":{"next":"15.0.0","react":"19"}}"#),
        ("app/page.tsx", "export default function Home() { return <a href=\"/about\">about</a> }\n"),
        ("app/about/page.tsx", "export default function About() { return null }\n"),
        ("app/users/[id]/page.tsx", "import Link from 'next/link';\nexport default function User({ id }) {\n  return <Link href={`/users/${id}/settings`}>x</Link>;\n}\n"),
        ("app/users/[id]/settings/page.tsx", "export default function Settings() { return null }\n"),
        ("app/dashboard/page.tsx", "export default function Dashboard() { return null }\n"),
        ("app/api/users/route.ts", "export async function GET() { return Response.json([]) }\nexport async function POST(req: Request) { return Response.json({}) }\n"),
        (
            "app/login/actions.ts",
            "import { redirect } from 'next/navigation';\nexport async function login(ok: boolean) {\n  const next = ok ? '/dashboard' : '/about';\n  redirect(next);\n}\n",
        ),
        (
            "components/Nav.tsx",
            "'use client';\nimport { useRouter } from 'next/navigation';\nexport function Nav() {\n  const router = useRouter();\n  const go = () => router.push('/users/42');\n  const nowhere = () => router.push('/missing');\n  const list = [];\n  list.push('/about');\n  return null;\n}\n",
        ),
        (
            "middleware.ts",
            "import { NextResponse } from 'next/server';\nexport function middleware(req) { return NextResponse.next(); }\nexport const config = {\n  matcher: ['/dashboard/:path*', '/((?!api|_next/static).*)'],\n};\n",
        ),
    ]);
    let edges = dump::dump_edges(&v);
    let has = |needle: &str| edges.iter().any(|e| e == needle);
    // pages are named by their path and render their default export
    assert!(has("app/about/page.tsx::app/about/page.tsx::route:/about -calls-> app/about/page.tsx::About"), "{edges:#?}");
    assert!(has("app/api/users/route.ts::app/api/users/route.ts::GET:/api/users -references-> app/api/users/route.ts::GET"), "{edges:#?}");
    // links and navigation calls reach the page
    assert!(has("app/page.tsx::Home -navigates-> app/about/page.tsx::app/about/page.tsx::route:/about"), "{edges:#?}");
    assert!(
        has("app/users/[id]/page.tsx::User -navigates-> app/users/[id]/settings/page.tsx::app/users/[id]/settings/page.tsx::route:/users/:id/settings"),
        "{edges:#?}"
    );
    assert!(has("components/Nav.tsx::Nav::go -navigates-> app/users/[id]/page.tsx::app/users/[id]/page.tsx::route:/users/:id"), "{edges:#?}");
    assert!(
        has("app/login/actions.ts::login -navigates-> app/dashboard/page.tsx::app/dashboard/page.tsx::route:/dashboard"),
        "both arms of the local: {edges:#?}"
    );
    assert!(has("app/login/actions.ts::login -navigates-> app/about/page.tsx::app/about/page.tsx::route:/about"), "{edges:#?}");
    assert!(!edges.iter().any(|e| e.starts_with("components/Nav.tsx::Nav::nowhere -navigates->")), "no page serves /missing: {edges:#?}");
    assert!(!edges.iter().any(|e| e.starts_with("components/Nav.tsx::Nav -navigates->")), "an array push is no navigation: {edges:#?}");
    // the proxy runs before every route its matcher covers
    assert!(has("app/dashboard/page.tsx::app/dashboard/page.tsx::route:/dashboard -calls-> middleware.ts::middleware"), "{edges:#?}");
    assert!(has("app/about/page.tsx::app/about/page.tsx::route:/about -calls-> middleware.ts::middleware"), "{edges:#?}");
    assert!(!has("app/api/users/route.ts::app/api/users/route.ts::GET:/api/users -calls-> middleware.ts::middleware"), "excluded by the matcher: {edges:#?}");
    let meta = meta_of(&v, "components/Nav.tsx::Nav::go", "app/users/[id]/page.tsx::app/users/[id]/page.tsx::route:/users/:id");
    assert!(meta.iter().any(|m| m.contains("\"next-navigation\"") && m.contains("\"href\":\"/users/42\"")), "{meta:?}");
}

#[test]
fn react_router_routes_links_and_navigate() {
    let (_d, v) = project(&[
        ("package.json", r#"{"name":"spa","dependencies":{"react":"18","react-router-dom":"6"}}"#),
        (
            "src/App.tsx",
            r#"import { BrowserRouter, Routes, Route } from 'react-router-dom';
import { Home, Profile, Team, Login } from './pages';
export function App() {
  return (
    <BrowserRouter>
      <Routes>
        <Route path="/" element={<Home />} />
        <Route path="/profile/:id" element={<Profile />} />
        <Route path="/teams" element={<Layout />}>
          <Route path=":teamId" element={<Team />} />
        </Route>
        <Route path="/login" element={<Login />} />
      </Routes>
    </BrowserRouter>
  );
}
function Layout() { return null; }
"#,
        ),
        (
            "src/pages.tsx",
            r#"import { Link, useNavigate } from 'react-router-dom';
export function Home() {
  const navigate = useNavigate();
  const onLogout = () => navigate('/login');
  return <Link to="/profile/7">me</Link>;
}
export function Profile() { return <NavLink to={`/teams/${'x'}`}>t</NavLink>; }
export function Team() { return null; }
export function Login() { return <Link to="/nowhere">x</Link>; }
"#,
        ),
    ]);
    let edges = dump::dump_edges(&v);
    let has = |needle: &str| edges.iter().any(|e| e == needle);
    assert!(has("src/App.tsx::src/App.tsx::route:/profile/:id -references-> src/pages.tsx::Profile"), "{edges:#?}");
    assert!(has("src/App.tsx::src/App.tsx::route:/teams/:teamId -references-> src/pages.tsx::Team"), "nested path: {edges:#?}");
    assert!(has("src/pages.tsx::Home -navigates-> src/App.tsx::src/App.tsx::route:/profile/:id"), "{edges:#?}");
    assert!(has("src/pages.tsx::Home::onLogout -navigates-> src/App.tsx::src/App.tsx::route:/login"), "{edges:#?}");
    assert!(has("src/pages.tsx::Profile -navigates-> src/App.tsx::src/App.tsx::route:/teams/:teamId"), "{edges:#?}");
    assert!(!edges.iter().any(|e| e.starts_with("src/pages.tsx::Login -navigates->")), "{edges:#?}");
}

#[test]
fn tanstack_routes_and_links() {
    let (_d, v) = project(&[
        ("package.json", r#"{"name":"spa","dependencies":{"react":"18","@tanstack/react-router":"1"}}"#),
        ("src/routes/posts.$postId.tsx", "import { createFileRoute } from '@tanstack/react-router';\nexport const Route = createFileRoute('/posts/$postId')({ component: PostComponent });\nfunction PostComponent() { return null; }\n"),
        (
            "src/routes/index.tsx",
            "import { createFileRoute, Link, useNavigate } from '@tanstack/react-router';\nexport const Route = createFileRoute('/')({ component: Index });\nfunction Index() {\n  const navigate = useNavigate();\n  const open = () => navigate({ to: '/posts/$postId', params: { postId: '1' } });\n  return <Link to=\"/posts/$postId\" params={{ postId: '2' }}>p</Link>;\n}\n",
        ),
    ]);
    let edges = dump::dump_edges(&v);
    let has = |needle: &str| edges.iter().any(|e| e == needle);
    let post = "src/routes/posts.$postId.tsx::src/routes/posts.$postId.tsx::route:/posts/:postId";
    assert!(has(&format!("{post} -calls-> src/routes/posts.$postId.tsx::PostComponent")), "{edges:#?}");
    assert!(has(&format!("src/routes/index.tsx::Index -navigates-> {post}")), "{edges:#?}");
    assert!(has(&format!("src/routes/index.tsx::Index::open -navigates-> {post}")), "{edges:#?}");
}

#[test]
fn nestjs_routes_and_injected_services() {
    let (_d, v) = project(&[
        ("package.json", r#"{"name":"api","dependencies":{"@nestjs/core":"10","@nestjs/common":"10"}}"#),
        ("src/users/users.service.ts", "import { Injectable } from '@nestjs/common';\n@Injectable()\nexport class UsersService {\n  findAll() { return []; }\n  findOne(id: string) { return id; }\n}\n"),
        (
            "src/users/users.controller.ts",
            "import { Controller, Get, Param } from '@nestjs/common';\nimport { UsersService } from './users.service';\n@Controller('users')\nexport class UsersController {\n  constructor(private readonly users: UsersService) {}\n  @Get()\n  list() { return this.users.findAll(); }\n  @Get(':id')\n  one(@Param('id') id: string) { return this.users.findOne(id); }\n}\n",
        ),
    ]);
    let edges = dump::dump_edges(&v);
    let has = |needle: &str| edges.iter().any(|e| e == needle);
    let ctl = "src/users/users.controller.ts";
    assert!(has(&format!("{ctl}::{ctl}::GET:/users -references-> {ctl}::UsersController::list")), "{edges:#?}");
    assert!(has(&format!("{ctl}::{ctl}::GET:/users/:id -references-> {ctl}::UsersController::one")), "{edges:#?}");
    assert!(has(&format!("{ctl}::UsersController::list -calls-> src/users/users.service.ts::UsersService::findAll")), "{edges:#?}");
}

#[test]
fn callback_collections_and_object_registries() {
    let (_d, v) = project(&[
        (
            "src/store.ts",
            "export class Store {\n  private listeners: Array<() => void> = [];\n  onChange(cb: () => void) {\n    this.listeners.push(cb);\n  }\n  notify() {\n    this.listeners.forEach((l) => l());\n  }\n}\n",
        ),
        (
            "src/view.ts",
            "import { Store } from './store';\nexport class View {\n  constructor(private store: Store) {}\n  mount() {\n    this.store.onChange(this.render);\n  }\n  render() {}\n}\n",
        ),
        (
            "src/commands.ts",
            "export function createUser() {}\nexport function removeUser() {}\nexport function audit() {}\nconst handlers = {\n  create: createUser,\n  remove: removeUser,\n};\nconst config = { width: 5, height: 6 };\nexport function run(action: string) {\n  handlers[action]();\n  config[action];\n}\n",
        ),
    ]);
    let edges = dump::dump_edges(&v);
    let has = |needle: &str| edges.iter().any(|e| e == needle);
    assert!(has("src/store.ts::Store::notify -calls-> src/view.ts::View::render"), "callback: {edges:#?}");
    let meta = meta_of(&v, "src/store.ts::Store::notify", "src/view.ts::View::render");
    assert!(meta.iter().any(|m| m.contains("\"callback\"") && m.contains("\"field\":\"listeners\"")), "{meta:?}");
    assert!(has("src/commands.ts::run -calls-> src/commands.ts::createUser"), "registry: {edges:#?}");
    assert!(has("src/commands.ts::run -calls-> src/commands.ts::removeUser"), "{edges:#?}");
    assert!(!has("src/commands.ts::run -calls-> src/commands.ts::audit"), "{edges:#?}");
}

#[test]
fn http_clients_reach_their_routes() {
    let (_d, v) = project(&[
        ("package.json", r#"{"name":"full","dependencies":{"express":"4","axios":"1","react":"18"}}"#),
        (
            "server/app.js",
            "const express = require('express');\nconst app = express();\nfunction listUsers(req, res) {}\nfunction getUser(req, res) {}\nfunction createUser(req, res) {}\napp.get('/api/users', listUsers);\napp.get('/api/users/:id', getUser);\napp.post('/api/users', createUser);\n",
        ),
        ("web/api.ts", "import axios from 'axios';\nexport const api = axios.create({ baseURL: '/api' });\n"),
        (
            "web/users.ts",
            "import { api } from './api';\nexport async function loadUsers() {\n  return fetch('/api/users');\n}\nexport async function loadUser(id: string) {\n  return api.get(`/users/${id}`);\n}\nexport async function addUser(u: object) {\n  return fetch('/api/users', { method: 'POST', body: JSON.stringify(u) });\n}\nexport async function missing() {\n  return fetch('/api/nothing');\n}\n",
        ),
    ]);
    let edges = dump::dump_edges(&v);
    let has = |needle: &str| edges.iter().any(|e| e == needle);
    assert!(has("web/users.ts::loadUsers -calls-> server/app.js::server/app.js::GET:/api/users"), "{edges:#?}");
    assert!(has("web/users.ts::loadUser -calls-> server/app.js::server/app.js::GET:/api/users/:id"), "base URL of the imported client: {edges:#?}");
    assert!(has("web/users.ts::addUser -calls-> server/app.js::server/app.js::POST:/api/users"), "method option: {edges:#?}");
    assert!(!edges.iter().any(|e| e.starts_with("web/users.ts::missing -calls->")), "{edges:#?}");
    // the route still reaches its handler, so the flow runs client -> route -> handler
    assert!(has("server/app.js::server/app.js::GET:/api/users -references-> server/app.js::listUsers"), "{edges:#?}");
}

#[test]
fn queues_buses_celery_and_spring_events() {
    let (_d, v) = project(&[
        ("package.json", r#"{"name":"svc","dependencies":{"@nestjs/core":"10","@nestjs/bull":"10"}}"#),
        (
            "src/mail.producer.ts",
            "import { InjectQueue } from '@nestjs/bull';\nimport { Queue } from 'bull';\nexport class MailProducer {\n  constructor(@InjectQueue('email') private readonly mailQueue: Queue) {}\n  async welcome() {\n    await this.mailQueue.add('welcome', {});\n  }\n}\n",
        ),
        (
            "src/mail.processor.ts",
            "import { Processor, Process } from '@nestjs/bull';\n@Processor('email')\nexport class MailProcessor {\n  @Process('welcome')\n  async sendWelcome(job) {}\n  @Process('reset')\n  async sendReset(job) {}\n}\n",
        ),
        (
            "src/users.service.ts",
            "import { EventEmitter2, OnEvent } from '@nestjs/event-emitter';\nexport class UsersService {\n  constructor(private eventEmitter: EventEmitter2) {}\n  create() {\n    this.eventEmitter.emit('user.created', {});\n  }\n}\nexport class Audit {\n  @OnEvent('user.*')\n  onUser(payload) {}\n}\n",
        ),
        ("app/tasks.py", "from celery import shared_task\n\n@shared_task\ndef send_email(to):\n    pass\n\ndef not_a_task():\n    pass\n"),
        ("app/views.py", "from .tasks import send_email, not_a_task\n\ndef signup(request):\n    send_email.delay('a@b.c')\n    not_a_task.delay()\n"),
        ("src/main/java/p/UserCreated.java", "package p;\npublic class UserCreated {}\n"),
        (
            "src/main/java/p/UserService.java",
            "package p;\npublic class UserService {\n    private final Publisher publisher;\n    public void create() {\n        publisher.publishEvent(new UserCreated());\n    }\n}\n",
        ),
        (
            "src/main/java/p/Welcome.java",
            "package p;\npublic class Welcome {\n    @EventListener\n    public void on(UserCreated e) {}\n    @EventListener\n    public void other(OtherEvent e) {}\n}\n",
        ),
    ]);
    let edges = dump::dump_edges(&v);
    let has = |needle: &str| edges.iter().any(|e| e == needle);
    assert!(has("src/mail.producer.ts::MailProducer::welcome -calls-> src/mail.processor.ts::MailProcessor::sendWelcome"), "queue: {edges:#?}");
    assert!(!has("src/mail.producer.ts::MailProducer::welcome -calls-> src/mail.processor.ts::MailProcessor::sendReset"), "{edges:#?}");
    assert!(has("src/users.service.ts::UsersService::create -calls-> src/users.service.ts::Audit::onUser"), "event bus glob: {edges:#?}");
    assert!(has("app/views.py::signup -calls-> app/tasks.py::send_email"), "celery: {edges:#?}");
    assert!(
        !edges.iter().any(|e| e.starts_with("app/views.py::signup -calls-> app/tasks.py::not_a_task")
            && meta_of(&v, "app/views.py::signup", "app/tasks.py::not_a_task").iter().any(|m| m.contains("celery"))),
        "{edges:#?}"
    );
    assert!(has("src/main/java/p/UserService.java::p::UserService::create -calls-> src/main/java/p/Welcome.java::p::Welcome::on"), "spring: {edges:#?}");
    assert!(!has("src/main/java/p/UserService.java::p::UserService::create -calls-> src/main/java/p/Welcome.java::p::Welcome::other"), "{edges:#?}");
}
