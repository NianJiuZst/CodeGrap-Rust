use cg_resolve::{build_seg, IndexOptions};
use cg_store::{dump, Seg, View};
use std::path::Path;
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
    let root = Path::new(dir.path()).to_path_buf();
    (dir, View::new(seg).with_root(root))
}

fn has(edges: &[String], needle: &str) -> bool {
    edges.iter().any(|e| e == needle)
}

#[test]
fn parallel_event_keys_and_resolution_are_byte_deterministic() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..80 {
        let source = format!("export function handler{i}() {{}}\nexport function fire{i}() {{ bus.emit('event{i}'); }}\nbus.on('event{i}', handler{i});\n");
        std::fs::write(dir.path().join(format!("file{i}.ts")), source).unwrap();
    }
    let build = |threads| build_seg(dir.path(), &IndexOptions { threads: Some(threads), memory_mb: Some(2048), ..Default::default() }).unwrap();
    let sequential = build(1);
    let parallel = build(4);
    assert_eq!(sequential.seg.to_bytes(), parallel.seg.to_bytes(), "all partitions and metadata must be independent of scheduling");
    let view = View::new(Arc::new(parallel.seg));
    assert_eq!(dump::canonical(&view).iter().filter(|line| line.contains("event-emitter")).count(), 80);
}

#[test]
fn typescript_imports_classes_and_receivers() {
    let (_d, view) = project(&[
        ("src/service.ts", "export class Service { run(): void {} }\nexport function helper() {}\n"),
        ("src/index.ts", "export * from './service';\n"),
        (
            "src/widget.ts",
            r#"import { Service, helper } from './index';
import * as svc from './service';
export class Base { base() {} }
export class Widget extends Base {
  private svc: Service;
  constructor() { super(); this.svc = new Service(); }
  render(): void {
    this.svc.run();
    helper();
    svc.helper();
    this.base();
    const s = new Service();
    s.run();
    localFn();
    function localFn() {}
  }
}
"#,
        ),
    ]);
    let mut edges = dump::dump_edges(&view);
    edges.dedup();
    for e in &edges {
        eprintln!("{e}");
    }
    assert!(has(&edges, "src/widget.ts::Widget -extends-> src/widget.ts::Base"));
    assert!(has(&edges, "src/widget.ts::Widget::render -calls-> src/service.ts::Service::run"), "this.svc.run through the re-export barrel");
    assert!(has(&edges, "src/widget.ts::Widget::render -calls-> src/service.ts::helper"));
    assert!(has(&edges, "src/widget.ts::Widget::render -calls-> src/widget.ts::Base::base"), "inherited method");
    assert!(has(&edges, "src/widget.ts::Widget::render -instantiates-> src/service.ts::Service"));
    assert!(has(&edges, "src/widget.ts::Widget::render -calls-> src/widget.ts::Widget::render::localFn"));
    assert!(has(&edges, "src/widget.ts -imports-> src/index.ts"));
    assert!(has(&edges, "src/index.ts -imports-> src/service.ts"));
}

#[test]
fn typescript_satisfies_keeps_concrete_call_targets() {
    let (_d, view) = project(&[
        ("service.ts", "export interface Runner { run(): void }\nexport class Service { run() {} extra() {} }\nexport function makeService(): Service { return new Service(); }\n"),
        (
            "app.ts",
            r#"import { Runner, Service, makeService } from './service';
export function direct() {
    const service = new Service() satisfies Runner;
    service.run();
    service.extra();
}
export function factory() {
    const service = makeService() satisfies Runner;
    service.run();
    service.extra();
}
export function plainFactory() {
    const service = makeService();
    service.run();
    service.extra();
}
export class Holder {
    service = new Service() satisfies Runner;
    use() { this.service.run(); this.service.extra(); }
}
export function asserted() {
    const service = (new Service() as Runner) satisfies object;
    service.run();
}
"#,
        ),
    ]);
    let edges = dump::dump_edges(&view);
    for caller in ["app.ts::direct", "app.ts::Holder::use"] {
        for method in ["run", "extra"] {
            assert!(
                has(&edges, &format!("{caller} -calls-> service.ts::Service::{method}")),
                "{caller} must retain the initializer's concrete receiver: {edges:#?}"
            );
        }
        assert!(!has(&edges, &format!("{caller} -calls-> service.ts::Runner::run")), "satisfies must not replace the receiver with its constraint");
    }
    for caller in ["factory", "plainFactory"] {
        assert!(has(&edges, &format!("app.ts::{caller} -calls-> service.ts::makeService")), "the factory call itself must be preserved");
    }
    assert!(!has(&edges, "app.ts::factory -calls-> service.ts::Runner::run"), "satisfies must not replace an unknown receiver with its constraint");
    assert!(has(&edges, "app.ts::asserted -calls-> service.ts::Runner::run"), "an inner as assertion still supplies its explicit type");
}

#[test]
fn typescript_satisfies_preserves_bound_aliases() {
    for ext in ["ts", "tsx"] {
        let app = format!("app.{ext}");
        let (_d, view) = project(&[
            ("service.ts", "export interface Runner { run(): void }\nexport class Service { run() {} extra() {} }\nexport class Other { run() {} }\nexport const shared = new Service();\n"),
            (
                &app,
                r#"import { Other, Runner, Service, shared as importedShared } from './service';
export function direct(original: Service) {
    const alias = original satisfies Runner;
    alias.run();
    alias.extra();
}
export function chained(original: Service) {
    const first = original satisfies Runner;
    const second = (first! satisfies Runner) satisfies object;
    second.run();
}
export function outer(original: Service) {
    function inner() {
        const alias = original satisfies Runner;
        alias.run();
    }
    inner();
}
export function imported() {
    const alias = importedShared satisfies Runner;
    alias.run();
}
export function shadowed(importedShared: Other) {
    const alias = importedShared satisfies Runner;
    alias.run();
}
export function shadowedUnknown(importedShared: any) {
    const alias = importedShared satisfies Runner;
    alias.run();
}
const localShared = new Service();
export class Holder {
    service = localShared satisfies Runner;
    use() { this.service.run(); }
}
"#,
            ),
        ]);
        let edges = dump::dump_edges(&view);
        for caller in ["direct", "chained", "outer::inner", "imported", "Holder::use"] {
            let expected = format!("{app}::{caller} -calls-> service.ts::Service::run");
            assert!(has(&edges, &expected), "{expected}: {edges:#?}");
        }
        assert!(has(&edges, &format!("{app}::direct -calls-> service.ts::Service::extra")), "{edges:#?}");
        assert!(has(&edges, &format!("{app}::shadowed -calls-> service.ts::Other::run")), "{edges:#?}");
        assert!(!has(&edges, &format!("{app}::shadowedUnknown -calls-> service.ts::Service::run")), "{edges:#?}");
    }
}

#[test]
fn typescript_satisfies_does_not_guess_unresolved_aliases() {
    for ext in ["ts", "tsx"] {
        let app = format!("app.{ext}");
        let (_d, view) = project(&[(
            &app,
            r#"import type { Service } from 'external-package';
export class Unrelated { run() {} }
export function external(original: Service) {
    const alias = original satisfies Service;
    alias.run();
}
export function untyped(original: any) {
    const alias = original satisfies { run(): void };
    alias.run();
}
function make(): any { return {}; }
export function factory() {
    const alias = make() satisfies Service;
    alias.run();
}
export function field(original: { service: Service }) {
    const alias = original.service satisfies Service;
    alias.run();
}
"#,
        )]);
        let edges = dump::dump_edges(&view);
        for caller in ["external", "untyped", "factory", "field"] {
            let unrelated = format!("{app}::{caller} -calls-> {app}::Unrelated::run");
            assert!(!has(&edges, &unrelated), "{unrelated}: {edges:#?}");
        }
    }
}

#[test]
fn python_modules_and_self() {
    let (_d, view) = project(&[
        ("pkg/__init__.py", "from .core import Engine\n"),
        ("pkg/core.py", "class Engine:\n    def start(self):\n        self.warm()\n    def warm(self):\n        pass\n\ndef make():\n    return Engine()\n"),
        ("app.py", "from pkg import Engine, core\nimport pkg.core as pc\n\ndef main():\n    e = Engine()\n    e.start()\n    pc.make()\n    core.make()\n    print('x')\n"),
    ]);
    let mut edges = dump::dump_edges(&view);
    edges.dedup();
    for e in &edges {
        eprintln!("{e}");
    }
    assert!(has(&edges, "app.py::main -calls-> pkg/core.py::Engine::start"), "typed local from constructor call through the package re-export");
    assert!(has(&edges, "pkg/core.py::Engine::start -calls-> pkg/core.py::Engine::warm"));
    assert!(has(&edges, "app.py::main -calls-> pkg/core.py::make"));
    assert!(has(&edges, "app.py::main -instantiates-> pkg/core.py::Engine"));
}

#[test]
fn go_packages_and_interfaces() {
    let (_d, view) = project(&[
        ("go.mod", "module example.com/proj\n"),
        ("pkg/util/util.go", "package util\n\nfunc Do(n int) int { return n }\n\nfunc helper() {}\n"),
        (
            "cmd/main.go",
            r#"package main

import "example.com/proj/pkg/util"

type Repo interface { Find(id int) string }
type Server struct { repo Repo }

func (s *Server) Run() {
	s.repo.Find(1)
	util.Do(3)
	helper := func() {}
	helper()
	other()
}

func other() {}
func main() { (&Server{}).Run() }
"#,
        ),
    ]);
    let mut edges = dump::dump_edges(&view);
    edges.dedup();
    for e in &edges {
        eprintln!("{e}");
    }
    assert!(has(&edges, "cmd/main.go::Server::Run -calls-> cmd/main.go::Repo::Find"));
    assert!(has(&edges, "cmd/main.go::Server::Run -calls-> pkg/util/util.go::Do"));
    assert!(has(&edges, "cmd/main.go::Server::Run -calls-> cmd/main.go::other"));
    assert!(!edges.iter().any(|e| e.contains("util.go::helper")), "the local closure must shadow the package function");
}

#[test]
fn java_packages_imports_and_members() {
    let (_d, view) = project(&[
        ("src/com/x/util/Helper.java", "package com.x.util;\npublic class Helper { public void assist() {} public static void s() {} }\n"),
        ("src/com/x/util/Statics.java", "package com.x.util;\npublic class Statics { public static void go() {} }\n"),
        (
            "src/com/x/svc/A.java",
            r#"package com.x.svc;
import com.x.util.Helper;
import static com.x.util.Statics.go;
public class A extends Base {
    private final Helper repo;
    public void run() {
        repo.assist();
        Helper h = new Helper();
        h.assist();
        go();
        this.other();
        Helper.s();
    }
    private void other() {}
}
class Base {}
"#,
        ),
    ]);
    let mut edges = dump::dump_edges(&view);
    edges.dedup();
    for e in &edges {
        eprintln!("{e}");
    }
    let run = "src/com/x/svc/A.java::com.x.svc::A::run";
    assert!(has(&edges, &format!("{run} -calls-> src/com/x/util/Helper.java::com.x.util::Helper::assist")));
    assert!(has(&edges, &format!("{run} -calls-> src/com/x/util/Statics.java::com.x.util::Statics::go")), "static import");
    assert!(has(&edges, &format!("{run} -calls-> src/com/x/svc/A.java::com.x.svc::A::other")));
    assert!(has(&edges, &format!("{run} -calls-> src/com/x/util/Helper.java::com.x.util::Helper::s")));
    assert!(has(&edges, &format!("{run} -instantiates-> src/com/x/util/Helper.java::com.x.util::Helper")));
}

#[test]
fn rust_modules_use_and_impl() {
    let (_d, view) = project(&[
        ("Cargo.toml", "[package]\nname = \"demo\"\n"),
        ("src/lib.rs", "pub mod util;\npub mod server;\n"),
        ("src/util.rs", "pub fn helper(n: u32) -> u32 { n }\npub struct Thing;\nimpl Thing { pub fn new() -> Thing { Thing } pub fn go(&self) {} }\n"),
        (
            "src/server.rs",
            r#"use crate::util::{helper, Thing as T};
pub trait Repo { fn find(&self, id: u32) -> u32; }
pub struct Server { repo: Box<dyn Repo> }
impl Server {
    pub fn run(&self) {
        helper(3);
        let t = T::new();
        t.go();
        Self::other();
    }
    fn other() {}
}
"#,
        ),
    ]);
    let mut edges = dump::dump_edges(&view);
    edges.dedup();
    for e in &edges {
        eprintln!("{e}");
    }
    assert!(has(&edges, "src/server.rs::Server::run -calls-> src/util.rs::helper"));
    assert!(has(&edges, "src/server.rs::Server::run -calls-> src/util.rs::Thing::new"));
    assert!(has(&edges, "src/server.rs::Server::run -calls-> src/util.rs::Thing::go"), "type of `t` comes from T::new()");
    assert!(has(&edges, "src/server.rs::Server::run -calls-> src/server.rs::Server::other"));
}

#[test]
fn kotlin_members_and_imports() {
    let (_d, view) = project(&[
        ("src/com/x/util/Helper.kt", "package com.x.util\nclass Helper { fun assist() {} }\n"),
        (
            "src/com/x/svc/A.kt",
            "package com.x.svc\nimport com.x.util.Helper\nclass A(private val repo: Helper) {\n    fun run() {\n        repo.assist()\n        val h = Helper()\n        h.assist()\n        other()\n    }\n    private fun other() {}\n}\n",
        ),
    ]);
    let mut edges = dump::dump_edges(&view);
    edges.dedup();
    for e in &edges {
        eprintln!("{e}");
    }
    let run = "src/com/x/svc/A.kt::com.x.svc::A::run";
    assert!(has(&edges, &format!("{run} -calls-> src/com/x/util/Helper.kt::com.x.util::Helper::assist")));
    assert!(has(&edges, &format!("{run} -calls-> src/com/x/svc/A.kt::com.x.svc::A::other")));
}

#[test]
fn debug_ts_unresolved() {
    let (_d, view) = project(&[
        ("src/service.ts", "export class Service { run(): void {} }\nexport function helper() {}\n"),
        (
            "src/widget.ts",
            "import { Service } from './service';\nexport class Base { base() {} }\nexport class Widget extends Base {\n  render(): void {\n    this.base();\n    this.render();\n  }\n}\n",
        ),
    ]);
    for u in dump::dump_unresolved(&view) {
        eprintln!("UNRESOLVED {u}");
    }
    for e in dump::dump_edges(&view) {
        eprintln!("EDGE {e}");
    }
}

#[test]
fn debug_python_typing_and_package() {
    let (_d, view) = project(&[
        ("src/flask/__init__.py", "from .app import Flask as Flask\n"),
        ("src/flask/app.py", "import typing as t\nT = t.TypeVar('T')\nclass Flask:\n    def route(self): pass\n"),
        ("tests/test_a.py", "import flask\nimport typing as t\n\ndef test():\n    app = flask.Flask('x')\n    app.route()\n    x = t.cast(int, 1)\n"),
    ]);
    for u in dump::dump_unresolved(&view) {
        eprintln!("UNRESOLVED {u}");
    }
    for e in dump::dump_edges(&view) {
        eprintln!("EDGE {e}");
    }
    for f in view.files() {
        for i in view.imports_of(f) {
            eprintln!(
                "IMPORT {} local={} imported={} spec={} kind={} scope={} line={}",
                view.file(f).path(),
                view.text_str(i.local),
                view.text_str(i.imported),
                view.text_str(i.spec),
                i.kind,
                i.scope.0,
                i.line
            );
        }
    }
}

#[test]
fn express_flask_and_gin_routes_link_to_their_handlers() {
    let (_d, v) = project(&[
        ("package.json", r#"{"name":"a","dependencies":{"express":"4"}}"#),
        ("web/routes.js", "const app = require('express')();\nfunction list(req, res) {}\nconst ctrl = { show(req, res) {} };\napp.get('/users', list);\napp.get('/users/:id', ctrl.show);\n"),
        ("requirements.txt", "flask\n"),
        ("py/views.py", "from flask import Flask\napp = Flask(__name__)\n\n@app.route('/hello')\ndef hello():\n    return 'x'\n"),
        ("go.mod", "module example.com/m\n\nrequire github.com/gin-gonic/gin v1.9.0\n"),
        ("srv/main.go", "package main\n\nfunc ping() {}\n\nfunc setup(r *Engine) {\n\tr.GET(\"/ping\", ping)\n}\n"),
    ]);
    let edges = cg_store::dump::dump_edges(&v);
    let has = |needle: &str| edges.iter().any(|e| e.contains(needle));
    assert!(has("web/routes.js::web/routes.js::GET:/users -references-> web/routes.js::list"), "{edges:#?}");
    assert!(has("GET:/users/:id -references-> web/routes.js::ctrl::show"), "{edges:#?}");
    assert!(has("py/views.py::py/views.py::GET:/hello -references-> py/views.py::hello"), "{edges:#?}");
    assert!(has("srv/main.go::srv/main.go::GET:/ping -references-> srv/main.go::ping"), "{edges:#?}");
}

fn synth_meta(v: &View, from: &str, to: &str) -> Option<String> {
    for f in v.files() {
        for n in v.nodes_in_file(f) {
            let n = cg_core::NodeIdx(n);
            if cg_store::dump::node_label(v, n) != from {
                continue;
            }
            for e in v.out_edges(n) {
                if cg_store::dump::node_label(v, e.dst) == to {
                    return v.edge_meta(e.eid).map(str::to_owned);
                }
            }
        }
    }
    None
}

#[test]
fn synthesised_edges_for_dynamic_dispatch() {
    let (_d, v) = project(&[
        ("ui/App.tsx", "import { Child } from './Child';\nexport function App() {\n  return <div><Child /></div>;\n}\n"),
        ("ui/Child.tsx", "export function Child() {\n  return <span />;\n}\n"),
        ("ui/Comp.jsx", "import React from 'react';\nexport class Comp extends React.Component {\n  handle() {\n    this.setState({ a: 1 });\n  }\n  render() {\n    return null;\n  }\n}\n"),
        ("bus/bus.ts", "import { EventEmitter } from 'events';\nconst bus = new EventEmitter();\nexport function onTick() {}\nbus.on('tick', onTick);\nexport function fire() {\n  bus.emit('tick');\n}\n"),
        ("j/Shape.java", "package j;\npublic interface Shape {\n    double area();\n}\n"),
        ("j/Circle.java", "package j;\npublic class Circle implements Shape {\n    public double area() { return 1; }\n}\n"),
        ("go.mod", "module example.com/m\n\ngo 1.21\n"),
        ("g/g.go", "package g\n\ntype Greeter interface {\n\tGreet() string\n\tName() string\n}\n\ntype En struct{}\n\nfunc (e *En) Greet() string { return \"hi\" }\nfunc (e *En) Name() string { return \"en\" }\n"),
    ]);
    let edges = cg_store::dump::dump_edges(&v);
    let has = |needle: &str| edges.iter().any(|e| e == needle);
    assert!(has("ui/App.tsx::App -calls-> ui/Child.tsx::Child"), "jsx-render: {edges:#?}");
    assert!(synth_meta(&v, "ui/App.tsx::App", "ui/Child.tsx::Child").unwrap().contains("jsx-render"));
    assert!(has("ui/Comp.jsx::Comp::handle -calls-> ui/Comp.jsx::Comp::render"), "react-render: {edges:#?}");
    assert!(has("bus/bus.ts::fire -calls-> bus/bus.ts::onTick"), "event-emitter: {edges:#?}");
    assert!(synth_meta(&v, "bus/bus.ts::fire", "bus/bus.ts::onTick").unwrap().contains("\"event\":\"tick\""));
    assert!(has("j/Shape.java::j::Shape::area -calls-> j/Circle.java::j::Circle::area"), "interface-impl: {edges:#?}");
    assert!(has("g/g.go::En -implements-> g/g.go::Greeter"), "go-implements: {edges:#?}");
    assert!(has("g/g.go::Greeter::Greet -calls-> g/g.go::En::Greet"), "go interface-impl: {edges:#?}");
}
