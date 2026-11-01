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

fn edges_of(view: &View) -> Vec<String> {
    let mut edges = dump::dump_edges(view);
    edges.dedup();
    for e in &edges {
        eprintln!("{e}");
    }
    edges
}

#[test]
fn php_parent_calls_namespaces_and_qualified_names() {
    let (_d, view) = project(&[
        ("src/Base.php", "<?php\nnamespace App;\nclass Base {\n  public function __construct() {}\n  public function go() {}\n}\n"),
        (
            "src/Child.php",
            "<?php\nnamespace App;\nclass Child extends Base {\n  public function __construct() { parent::__construct(); }\n  public function go() { parent::go(); $this->run(); }\n  public function run() {}\n}\n",
        ),
        ("src/Logger.php", "<?php\nnamespace App;\nclass Logger {\n  public static function make() {}\n}\n"),
        ("src/Http/Svc.php", "<?php\nnamespace App\\Http;\nclass Svc {\n  public function run() { \\Vendor\\Lib\\Logger::make(); }\n}\n"),
        ("src/Http/Other.php", "<?php\nnamespace App\\Http;\nuse App\\Logger;\nclass Other {\n  public function run() { \\App\\Logger::make(); Logger::make(); }\n}\n"),
    ]);
    let edges = edges_of(&view);
    assert!(has(&edges, "src/Child.php::App::Child -extends-> src/Base.php::App::Base"));
    assert!(has(&edges, "src/Child.php::App::Child::__construct -calls-> src/Base.php::App::Base::__construct"), "parent::__construct()");
    assert!(!has(&edges, "src/Child.php::App::Child::__construct -calls-> src/Child.php::App::Child::__construct"), "no self-loop constructor");
    assert!(has(&edges, "src/Child.php::App::Child::go -calls-> src/Base.php::App::Base::go"), "parent::go() from go()");
    assert!(!has(&edges, "src/Child.php::App::Child::go -calls-> src/Child.php::App::Child::go"), "bare go inside go is parent::go, not recursion");
    assert!(has(&edges, "src/Child.php::App::Child::go -calls-> src/Child.php::App::Child::run"), "$this->run()");
    assert!(!edges.iter().any(|e| e.starts_with("src/Http/Svc.php::App\\Http::Svc::run -calls->")), "a vendor class is not a project class: {edges:?}");
    assert!(
        has(&edges, "src/Http/Other.php::App\\Http::Other::run -calls-> src/Logger.php::App::Logger::make"),
        "\\App\\Logger::make() and the imported Logger"
    );
}

#[test]
fn csharp_namespaces_partial_classes_and_determinism() {
    let files: &[(&str, &str)] = &[
        ("src/Util1.cs", "namespace A {\n  public partial class Util {\n    public static void Go() {}\n  }\n}\n"),
        ("src/Util2.cs", "namespace A {\n  public partial class Util {\n    public static void Stop() { Go(); }\n  }\n}\n"),
        ("src/Helper.cs", "namespace A {\n  public class Helper {\n    public void Help() {}\n  }\n}\n"),
        (
            "src/Sub/User.cs",
            "namespace A.B {\n  public class User {\n    public void Run() {\n      Util.Go();\n      var h = new Helper();\n      h.Help();\n    }\n  }\n}\n",
        ),
    ];
    let (_d, view) = project(files);
    let edges = edges_of(&view);
    assert!(has(&edges, "src/Util2.cs::A::Util::Stop -calls-> src/Util1.cs::A::Util::Go"), "member of the other partial declaration");
    assert!(has(&edges, "src/Sub/User.cs::A.B::User::Run -calls-> src/Util1.cs::A::Util::Go"), "type of the enclosing namespace without a using");
    assert!(has(&edges, "src/Sub/User.cs::A.B::User::Run -instantiates-> src/Helper.cs::A::Helper"));
    assert!(has(&edges, "src/Sub/User.cs::A.B::User::Run -calls-> src/Helper.cs::A::Helper::Help"));
    // the pick among cross-file candidates must not depend on scheduling
    let dir = tempfile::tempdir().unwrap();
    for (p, c) in files {
        let path = dir.path().join(p);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, c).unwrap();
    }
    let a = build_seg(dir.path(), &IndexOptions { threads: Some(1), ..Default::default() }).unwrap();
    let b = build_seg(dir.path(), &IndexOptions { threads: Some(4), ..Default::default() }).unwrap();
    assert_eq!(a.seg.to_bytes(), b.seg.to_bytes());
}

#[test]
fn cpp_namespace_order_and_out_of_line_members() {
    let (_d, view) = project(&[
        (
            "src/a.cpp",
            "namespace util { void log(int) {} }\nnamespace app {\n  namespace util { void log(int) {} }\n  struct S { void run(); };\n}\nvoid app::S::run() { util::log(1); ::util::log(2); }\n",
        ),
        ("src/foo.h", "struct Foo {\n  void bar();\n  void baz();\n};\n"),
        ("src/foo.cpp", "#include \"foo.h\"\nvoid Foo::bar() {}\nvoid Foo::baz() { bar(); }\n"),
        ("src/use.cpp", "#include \"foo.h\"\nvoid g() { Foo f; f.bar(); }\n"),
        ("src/other/foo2.h", "struct Foo {\n  void bar();\n};\n"),
        ("src/other/foo2.cpp", "#include \"foo2.h\"\nvoid Foo::bar() {}\n"),
    ]);
    let edges = edges_of(&view);
    assert!(has(&edges, "src/a.cpp::app::S::run -calls-> src/a.cpp::app::util::log"), "innermost namespace first");
    assert!(has(&edges, "src/a.cpp::app::S::run -calls-> src/a.cpp::util::log"), "::util::log is absolute");
    assert!(has(&edges, "src/foo.cpp -imports-> src/foo.h"));
    assert!(has(&edges, "src/use.cpp::g -calls-> src/foo.cpp::Foo::bar"), "out-of-line definition in the file that includes the header");
    assert!(!has(&edges, "src/use.cpp::g -calls-> src/other/foo2.cpp::Foo::bar"), "an unrelated Foo");
}

#[test]
fn cpp_declarations_definitions_and_unknown_namespaces() {
    let (_d, view) = project(&[
        ("include/lib/core.h", "namespace lib { namespace detail { void vprint(int); } }\n"),
        ("src/core.cc", "#include \"lib/core.h\"\nnamespace lib { namespace detail { void vprint(int) {} } }\n"),
        ("src/app.cc", "#include \"lib/core.h\"\nnamespace lib { void print() { detail::vprint(1); } }\n"),
        ("src/abort.cc", "struct Death { void Abort() {} };\nvoid g() { posix::Abort(); }\n"),
    ]);
    let edges = edges_of(&view);
    assert!(
        edges.iter().any(|e| e.starts_with("src/app.cc::lib::print -calls-> ") && e.ends_with("::lib::detail::vprint")),
        "a function declared in one file and defined in another resolves: {edges:?}"
    );
    assert!(!has(&edges, "src/abort.cc::g -calls-> src/abort.cc::Death::Abort"), "an unknown namespace is not a unique-name guess");
}

#[test]
fn cpp_global_namespace_and_standard_free_functions() {
    let (_d, view) = project(&[
        ("test/util.h", "inline int helper() { return 1; }\n"),
        ("test/mock.cc", "#include \"util.h\"\nnamespace test { int open(const char* p); }\nint test::open(const char* p) {\n  return ::open(p);\n}\nvoid use() {\n  ::helper();\n  auto s = ::std::string(\"x\");\n}\n"),
        ("test/ranges.cc", "template <int N> int get(int t) { return t; }\n"),
        ("test/other.cc", "struct File { int get() { return 0; } };\nvoid run() {\n  get();\n}\n"),
        ("test/old.c", "int max(int a, int b) { return a > b ? a : b; }\n"),
        ("test/use.c", "int pick(int a, int b) { return max(a, b); }\n"),
    ]);
    let edges = edges_of(&view);
    assert!(
        !edges.iter().any(|e| e.starts_with("test/mock.cc::test::open -calls-> ")),
        "::open is the global POSIX function, not the wrapper itself: {edges:?}"
    );
    assert!(has(&edges, "test/mock.cc::use -calls-> test/util.h::helper"), "::helper() declared in an included header: {edges:?}");
    assert!(!edges.iter().any(|e| e.starts_with("test/mock.cc::use -") && e.contains("string")), "::std::string is the standard library: {edges:?}");
    assert!(!has(&edges, "test/other.cc::run -calls-> test/ranges.cc::get"), "an unqualified get() in C++ is not another file's template: {edges:?}");
    assert!(has(&edges, "test/use.c::pick -calls-> test/old.c::max"), "C keeps project functions named like std ones: {edges:?}");
}

#[test]
fn ruby_requires_and_module_methods() {
    let (_d, view) = project(&[
        ("lib/mygem.rb", "require 'set'\nrequire 'logger'\nrequire_relative 'mygem/util'\nrequire 'mygem/set'\n"),
        ("lib/mygem/util.rb", "module Mygem\n  module Util\n    def self.helper\n    end\n  end\nend\n"),
        ("lib/mygem/set.rb", "module Mygem\n  class Set\n  end\nend\n"),
        ("lib/mygem/logger.rb", "module Mygem\n  class Logger\n  end\nend\n"),
        ("lib/app.rb", "require_relative 'mygem/util'\nclass App\n  def run\n    Mygem::Util.helper\n  end\nend\n"),
    ]);
    let edges = edges_of(&view);
    assert!(has(&edges, "lib/mygem.rb -imports-> lib/mygem/util.rb"), "require_relative");
    assert!(has(&edges, "lib/mygem.rb -imports-> lib/mygem/set.rb"), "require 'mygem/set'");
    assert!(!has(&edges, "lib/mygem.rb -imports-> lib/mygem/logger.rb"), "require 'logger' is the standard library, not lib/mygem/logger.rb");
    assert!(has(&edges, "lib/app.rb::App::run -calls-> lib/mygem/util.rb::Mygem::Util::helper"));
}

#[test]
fn swift_standard_types_versus_project_types() {
    let (_d, view) = project(&[
        ("Sources/App/Task.swift", "struct Task {\n  let title: String\n}\n"),
        ("Sources/App/Ext.swift", "import Foundation\nextension Data {\n  func asString() -> String { \"\" }\n}\n"),
        ("Sources/App/Res.swift", "extension Result {\n  var isSuccess: Bool { false }\n}\n"),
        ("Sources/App/Monitor.swift", "struct PathMonitor {\n  enum Result {\n    case ok\n  }\n  func check() {\n    let r = Result.ok\n    _ = r\n  }\n}\nstruct Throwing {\n  struct Error: Swift.Error {}\n  func fail() throws {\n    throw Error()\n  }\n}\n"),
        ("Sources/App/Main.swift", "import Foundation\nfunc make() {\n  let t = Task(title: \"x\")\n  let d = Data()\n  d.asString()\n  let r = Result { try load() }\n  _ = (t, r)\n}\n"),
    ]);
    let edges = edges_of(&view);
    assert!(
        edges.iter().any(|e| e.starts_with("Sources/App/Main.swift::make -") && e.ends_with("-> Sources/App/Task.swift::Task")),
        "a project struct named like a standard type is still the project's: {edges:?}"
    );
    assert!(
        !edges.iter().any(|e| e.starts_with("Sources/App/Main.swift::make -") && e.ends_with("-> Sources/App/Ext.swift::Data")),
        "Data() is Foundation's, not the extension"
    );
    assert!(
        !edges.iter().any(|e| e.starts_with("Sources/App/Main.swift::make -") && e.ends_with("-> Sources/App/Res.swift::Result")),
        "Result {{ .. }} is the standard library's init(catching:), not the extension, even with a nested enum Result elsewhere: {edges:?}"
    );
    assert!(
        has(&edges, "Sources/App/Monitor.swift::Throwing::fail -instantiates-> Sources/App/Monitor.swift::Throwing::Error"),
        "a nested type shadows the standard type inside its enclosing declaration: {edges:?}"
    );
    assert!(has(&edges, "Sources/App/Main.swift::make -calls-> Sources/App/Ext.swift::Data::asString"), "members added by the extension are reachable");
}
