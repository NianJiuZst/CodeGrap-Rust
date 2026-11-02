//! Swift facts: imports, scopes and typed bindings (see `facts/swift.rs`).

use cg_core::{Lang, NONE};
use cg_extract::*;

fn run(src: &str) -> Facts {
    let b = extract_bytes("Sources/App/a.swift", src.as_bytes(), 0, Lang::Swift, &ExtractConfig::new("."));
    assert_eq!(b.flags & fileflags::PANICKED, 0, "{:?}", b.errors);
    assert!(b.errors.iter().all(|e| !e.contains("facts")), "{:?}", b.errors);
    b.facts
}

fn imp<'a>(f: &'a Facts, spec: &str) -> &'a ImportFact {
    f.imports.iter().find(|i| i.spec == spec).unwrap_or_else(|| panic!("no import {spec}: {:?}", f.imports))
}

fn binds<'a>(f: &'a Facts, name: &str) -> Vec<&'a BindingFact> {
    f.bindings.iter().filter(|b| b.name == name).collect()
}

/// The only binding called `name`.
fn bind<'a>(f: &'a Facts, name: &str) -> &'a BindingFact {
    match binds(f, name).as_slice() {
        [one] => one,
        other => panic!("expected one binding {name}, got {other:?}\nall: {:#?}", f.bindings),
    }
}

/// (decl_type, init_ctor) of the only binding called `name`.
fn ty<'a>(f: &'a Facts, name: &str) -> (&'a str, &'a str) {
    let b = bind(f, name);
    (b.decl_type.as_str(), b.init_ctor.as_str())
}

fn start_line(s: &ScopeFact) -> u32 {
    (s.start >> 32) as u32
}

fn end_line(s: &ScopeFact) -> u32 {
    (s.end >> 32) as u32
}

/// Kinds of the scopes that start on `line`, in order.
fn scopes_at(f: &Facts, line: u32) -> Vec<ScopeKind> {
    f.scopes.iter().skip(1).filter(|s| start_line(s) == line).map(|s| s.kind).collect()
}

fn scope_kind(f: &Facts, b: &BindingFact) -> ScopeKind {
    f.scopes[b.scope as usize].kind
}

#[test]
fn imports_bind_the_module() {
    let f = run(r#"import Foundation
@testable import MyLib
import struct Foo.Bar
import UIKit.UIView
@_exported import Shared

let x = 1
"#);
    assert_eq!(f.imports.len(), 5, "{:?}", f.imports);
    for (spec, line) in [("Foundation", 1), ("MyLib", 2), ("Foo", 3), ("UIKit", 4), ("Shared", 5)] {
        let i = imp(&f, spec);
        assert_eq!((i.local.as_str(), i.imported.as_str(), i.kind, i.type_only, i.scope, i.line), ("*", "*", ImportKind::Namespace, false, 0, line), "{spec}");
    }
    assert!(f.bindings.iter().all(|b| b.kind != BindKind::Package), "Swift has no package binding: {:?}", f.bindings);
}

#[test]
fn type_declarations_open_class_scopes() {
    let f = run(r#"class C {
}
struct S {
}
enum E {
    case a, b
}
actor A {
}
protocol P {
    var p: Int { get }
    func f()
}
extension C {
}
extension Array where Element == C {
}
final class Outer {
    struct Inner {
    }
}
"#);
    for line in [1, 3, 5, 8, 10, 14, 16, 18, 19] {
        assert_eq!(scopes_at(&f, line), vec![ScopeKind::Class], "line {line}: {:?}", f.scopes);
    }
    let class_scopes = f.scopes.iter().filter(|s| s.kind == ScopeKind::Class).count();
    assert_eq!(class_scopes, 9);
    // bodyless protocol requirements open no function scope
    assert!(f.scopes.iter().all(|s| s.kind != ScopeKind::Function), "{:?}", f.scopes);
    // nesting
    let inner = f.scopes.iter().position(|s| start_line(s) == 19).unwrap();
    let outer = f.scopes.iter().position(|s| start_line(s) == 18).unwrap();
    assert_eq!(f.scopes[inner].parent, outer as u32);
    assert_eq!(f.scopes[outer].parent, 0);
    let c = &f.scopes[f.scopes.iter().position(|s| start_line(s) == 1).unwrap()];
    assert_eq!(end_line(c), 2);
    // enum cases are not bindings
    assert!(binds(&f, "a").is_empty() && binds(&f, "b").is_empty(), "{:?}", f.bindings);
    // a protocol property requirement is a field of the protocol
    assert_eq!((bind(&f, "p").kind, ty(&f, "p").0), (BindKind::Field, "Int"));
}

#[test]
fn functions_accessors_and_closures_open_function_scopes() {
    let f = run(r#"class C {
    init(a: Int) {
    }
    deinit {
    }
    func run() {
        xs.map { x in x }
    }
    subscript(i: Int) -> Int {
        return i
    }
    var computed: Int {
        return 1
    }
    var both: Int {
        get { 1 }
        set { print(newValue) }
    }
    var observed: Int = 0 {
        willSet { print(newValue) }
        didSet { print(oldValue) }
    }
    static func make() -> C { C() }
}
func free() {
}
"#);
    let func = vec![ScopeKind::Function];
    for line in [2, 4, 6, 7, 9, 12, 16, 17, 20, 21, 23, 25] {
        assert_eq!(scopes_at(&f, line), func, "line {line}: {:?}", f.scopes);
    }
    // the accessor-list braces themselves are not a scope
    assert!(scopes_at(&f, 15).is_empty() && scopes_at(&f, 19).is_empty(), "{:?}", f.scopes);
    let class = f.scopes.iter().position(|s| s.kind == ScopeKind::Class).unwrap() as u32;
    for line in [2, 4, 6, 9, 12, 16, 17, 20, 21, 23] {
        let s = f.scopes.iter().find(|s| start_line(s) == line).unwrap();
        assert_eq!(s.parent, class, "line {line}");
    }
    // the closure nests in the method
    let run = f.scopes.iter().position(|s| start_line(s) == 6).unwrap() as u32;
    assert_eq!(f.scopes.iter().find(|s| start_line(s) == 7).unwrap().parent, run);
    assert_eq!(f.scopes.iter().find(|s| start_line(s) == 25).unwrap().parent, 0);
    // a subscript's getter shorthand stays in the subscript's own scope
    assert_eq!(scopes_at(&f, 10), vec![]);
}

#[test]
fn parameters_bind_internal_names() {
    let f = run(r#"class Client {
    init(session: Session, _ y: Foo?) {
    }
    func f(for request: Request, _ x: Foo, with opt: Bar? = Bar(), _ items: Foo..., into out: inout Out) {
    }
    func g(cb: @escaping (Int) -> Void, list: [Foo], map: [String: Foo], gen: Array<Foo>, iuo: Foo!, some: some Proto) {
    }
    subscript(index i: Index) -> Element {
        return i
    }
}
"#);
    for (name, t) in [
        ("session", "Session"),
        ("y", "Foo"),
        ("request", "Request"),
        ("x", "Foo"),
        ("opt", "Bar"),
        ("items", ""),
        ("out", "Out"),
        ("cb", ""),
        ("list", ""),
        ("map", ""),
        ("gen", "Array"),
        ("iuo", "Foo"),
        ("some", "Proto"),
        ("i", "Index"),
    ] {
        let b = bind(&f, name);
        assert_eq!((b.kind, b.decl_type.as_str(), b.init_ctor.as_str()), (BindKind::Param, t, ""), "{name}");
        assert_eq!(scope_kind(&f, b), ScopeKind::Function, "{name}");
    }
    // external labels are not bindings
    for label in ["for", "with", "into", "index", "_"] {
        assert!(binds(&f, label).is_empty(), "{label}: {:?}", f.bindings);
    }
    assert_eq!(bind(&f, "request").line, 4);
    // each function has its own scope
    assert_ne!(bind(&f, "session").scope, bind(&f, "request").scope);
}

#[test]
fn closure_parameters() {
    let f = run(r#"func f() {
    xs.map { (a: Foo, b) in a }
    xs.map { c, d in c }
    xs.map { (e) -> Int in e }
    xs.map { _ in 1 }
    xs.map { $0 + 1 }
    xs.forEach { [weak self] item in self?.use(item) }
    let cb = { [client = makeClient()] (r: Request) in client.send(r) }
}
"#);
    let a = bind(&f, "a");
    assert_eq!((a.kind, a.decl_type.as_str(), a.line), (BindKind::Param, "Foo", 2));
    assert_eq!(scopes_at(&f, 2), vec![ScopeKind::Function]);
    assert_eq!(f.scopes[a.scope as usize].kind, ScopeKind::Function);
    assert_ne!(a.scope, 1, "closure params live in the closure scope");
    for (name, line) in [("b", 2), ("c", 3), ("d", 3), ("e", 4), ("item", 7)] {
        let b = bind(&f, name);
        assert_eq!((b.kind, b.decl_type.as_str(), b.line), (BindKind::Param, "", line), "{name}");
    }
    assert_eq!(ty(&f, "r"), ("Request", ""));
    assert!(binds(&f, "$0").is_empty() && binds(&f, "_").is_empty() && binds(&f, "self").is_empty(), "{:?}", f.bindings);
    // a capture with an initialiser binds inside the closure
    let client = bind(&f, "client");
    assert_eq!((client.kind, client.init_ctor.as_str()), (BindKind::Local, "makeClient()"));
    assert_eq!(client.scope, bind(&f, "r").scope);
    assert_eq!(f.scopes.iter().filter(|s| s.kind == ScopeKind::Function).count(), 8);
}

#[test]
fn stored_and_computed_properties_are_fields() {
    let f = run(r#"import Alamofire

final class Client {
    let session: Session
    var x = Foo()
    lazy var q = Queue()
    static let shared = Client()
    @Published var state = State.initial()
    private(set) var count: Int = 0
    weak var delegate: ClientDelegate?
    var computed: Renderer { Renderer() }
    var gs: Cache {
        get { cache }
        set { cache = newValue }
    }
    var a = 1, b = Foo()
    func run() {}
}
struct S { let id: ID }
enum E {
    case one
    var label: Label { Label() }
}
"#);
    for (name, t, init) in [
        ("session", "Session", ""),
        ("x", "", "Foo()"),
        ("q", "", "Queue()"),
        ("shared", "", "Client()"),
        ("state", "", "State.initial()"),
        ("count", "Int", ""),
        ("delegate", "ClientDelegate", ""),
        ("computed", "Renderer", ""),
        ("gs", "Cache", ""),
        ("a", "", ""),
        ("b", "", "Foo()"),
        ("id", "ID", ""),
        ("label", "Label", ""),
    ] {
        let b = bind(&f, name);
        assert_eq!((b.kind, b.decl_type.as_str(), b.init_ctor.as_str()), (BindKind::Field, t, init), "{name}");
        assert_eq!(scope_kind(&f, b), ScopeKind::Class, "{name}");
        assert_eq!(b.node_row, NONE);
    }
    assert_eq!(bind(&f, "session").line, 4);
    assert_eq!(bind(&f, "b").line, 16);
    assert!(binds(&f, "one").is_empty() && binds(&f, "run").is_empty(), "{:?}", f.bindings);
    // top-level and class scopes differ per type
    assert_ne!(bind(&f, "session").scope, bind(&f, "id").scope);
}

#[test]
fn declared_type_reduction() {
    let f = run(r#"class T {
    var opt: Foo?
    var iuo: Foo!
    var arr: [Foo]
    var dict: [K: V]
    var gen: Array<Foo>
    var head: Foo<T>
    var nested: Outer<T>.Inner
    var someP: some P
    var anyP: any P
    var qual: Alamofire.Session
    var tup: (Int, Int)
    var fn: (Int) -> Void
    var paren: (Foo)
    var wrapped: Optional<Foo>
    var meta: Foo.Type
    var composed: A & B
    var optArr: [Foo]?
    var doubleOpt: Foo??
}
"#);
    for (name, t) in [
        ("opt", "Foo"),
        ("iuo", "Foo"),
        ("arr", ""),
        ("dict", ""),
        ("gen", "Array"),
        ("head", "Foo"),
        ("nested", "Outer.Inner"),
        ("someP", "P"),
        ("anyP", "P"),
        ("qual", "Alamofire.Session"),
        ("tup", ""),
        ("fn", ""),
        ("paren", "Foo"),
        ("wrapped", "Foo"),
        ("meta", ""),
        ("composed", ""),
        ("optArr", ""),
        ("doubleOpt", "Foo"),
    ] {
        assert_eq!(ty(&f, name), (t, ""), "{name}");
    }
}

#[test]
fn initialiser_forms() {
    let f = run(r#"func f() {
    let a = Foo()
    let b = Foo.make(1)
    let c = x.build()
    let d = a.b().c()
    let e = Foo { 1 }
    let g = Foo(a: 1) { 2 }
    let h = Foo<Int>()
    let i = Foo.init(x: 1)
    let j: Foo = .init()
    let k = Foo()()
    let l = self.make()
    let m = make()
    let n = Foo.shared
    let o = try Foo.make()
    let p = try? Foo()
    let q = try! await Loader.load()
    let r = await client.fetch()
    let s = Foo(x: 1).build()
    let t = { 1 }()
    let u = x?.build()
    let v = y as? Bar
    let w = try y as! Baz
    let z: [Foo] = makeFoos()
    let typed: Foo = make()
}
"#);
    for (name, t, init) in [
        ("a", "", "Foo()"),
        ("b", "", "Foo.make()"),
        ("c", "", "x.build()"),
        ("d", "", ""),
        ("e", "", "Foo()"),
        ("g", "", "Foo()"),
        ("h", "", "Foo()"),
        ("i", "", "Foo()"),
        ("j", "Foo", ""),
        ("k", "", ""),
        ("l", "", "self.make()"),
        ("m", "", "make()"),
        ("n", "", ""),
        ("o", "", "Foo.make()"),
        ("p", "", "Foo()"),
        ("q", "", "Loader.load()"),
        ("r", "", "client.fetch()"),
        ("s", "", ""),
        ("t", "", ""),
        ("u", "", "x.build()"),
        ("v", "Bar", ""),
        ("w", "Baz", ""),
        ("z", "", ""),
        ("typed", "Foo", "make()"),
    ] {
        let b = bind(&f, name);
        assert_eq!((b.kind, b.decl_type.as_str(), b.init_ctor.as_str()), (BindKind::Local, t, init), "{name}");
    }
    // closures in initialisers still open scopes
    for line in [6, 7, 20] {
        assert_eq!(scopes_at(&f, line), vec![ScopeKind::Function], "line {line}");
    }
}

#[test]
fn local_bindings() {
    let f = run(r#"func f(y: Foo?, xs: [Foo], d: [K: V]) -> Int {
    let a: Foo = make()
    var b = Foo()
    let (c, e) = pair()
    var (g, h): (Int, Int) = (1, 2)
    guard let i = y else { return 0 }
    guard let j: Foo = y, var k = try? Foo.make() else { return 0 }
    if let l = y { print(l) }
    if let m { print(m) }
    if let n = y, let o = n.next(), n.isValid { }
    if case .some(let p) = y { print(p) }
    if case let .some(q) = y { print(q) }
    while let r = it.next() { }
    for s in xs { print(s) }
    for (t, u) in d { print(t, u) }
    for v: Foo in xs { print(v) }
    for case let w? in xs { }
    for _ in xs { }
    if let cell = view.dequeue() as? Cell { }
    let _ = discard()
    return 0
}
"#);
    let fs = f.scopes.iter().position(|s| start_line(s) == 1).unwrap() as u32;
    assert_eq!(f.scopes[fs as usize].kind, ScopeKind::Function);
    for (name, line, t, init) in [
        ("a", 2, "Foo", "make()"),
        ("b", 3, "", "Foo()"),
        ("c", 4, "", ""),
        ("e", 4, "", ""),
        ("g", 5, "", ""),
        ("h", 5, "", ""),
        ("i", 6, "Foo", ""),
        ("j", 7, "Foo", ""),
        ("k", 7, "", "Foo.make()"),
        ("l", 8, "Foo", ""),
        ("m", 9, "", ""),
        ("n", 10, "Foo", ""),
        ("o", 10, "", "n.next()"),
        ("p", 11, "", ""),
        ("q", 12, "", ""),
        ("r", 13, "", "it.next()"),
        ("s", 14, "", ""),
        ("t", 15, "", ""),
        ("u", 15, "", ""),
        ("v", 16, "Foo", ""),
        ("w", 17, "", ""),
        ("cell", 19, "Cell", ""),
    ] {
        let b = bind(&f, name);
        assert_eq!((b.kind, b.line, b.decl_type.as_str(), b.init_ctor.as_str(), b.scope), (BindKind::Local, line, t, init, fs), "{name}");
    }
    // enum case names in patterns and wildcards are not bindings
    assert!(binds(&f, "some").is_empty() && binds(&f, "_").is_empty(), "{:?}", f.bindings);
    // parameters, then locals; nothing else
    let locals = f.bindings.iter().filter(|b| b.kind == BindKind::Local).count();
    assert_eq!(locals, 22, "{:#?}", f.bindings);
}

#[test]
fn optional_rebinding_keeps_the_type() {
    let f = run(r#"class C {
    var session: Session?
    var cache = Cache()
    func f(req: Request?, opt: Foo?) {
        guard let req = req else { return }
        if let opt { print(opt) }
        guard let session = session, let c = self.cache else { return }
        guard let self = self else { return }
        let local: Bar? = nil
        if let l2 = local, let unknown = nowhere, let typed: Baz = local { }
        xs.forEach { [weak self] in
            guard let self else { return }
        }
    }
}
"#);
    let local = |name: &str| -> (String, String) {
        let b = f.bindings.iter().find(|b| b.name == name && b.kind == BindKind::Local).unwrap_or_else(|| panic!("no local {name}: {:?}", f.bindings));
        (b.decl_type.clone(), b.init_ctor.clone())
    };
    assert_eq!(local("req"), ("Request".into(), "".into()));
    assert_eq!(local("opt"), ("Foo".into(), "".into()));
    assert_eq!(local("session"), ("Session".into(), "".into()));
    assert_eq!(local("c"), ("".into(), "Cache()".into()));
    assert_eq!(local("l2"), ("Bar".into(), "".into()));
    assert_eq!(local("unknown"), ("".into(), "".into()));
    assert_eq!(local("typed"), ("Baz".into(), "".into()));
    // `guard let self` never shadows the receiver
    assert!(binds(&f, "self").is_empty(), "{:?}", f.bindings);
}

#[test]
fn switch_and_catch_patterns_bind_locals() {
    let f = run(r#"func f(e: Event) {
    switch e {
    case .a(let x):
        print(x)
    case let .b(y, z):
        print(y, z)
    case let w as Foo:
        print(w)
    case .c(v):
        break
    default:
        break
    }
    do {
        try run()
    } catch let err {
        print(err)
    }
}
"#);
    for (name, t) in [("x", ""), ("y", ""), ("z", ""), ("w", "Foo"), ("err", "")] {
        let b = bind(&f, name);
        assert_eq!((b.kind, b.decl_type.as_str()), (BindKind::Local, t), "{name}");
    }
    // case names and expression patterns bind nothing
    for name in ["a", "b", "c", "v"] {
        assert!(binds(&f, name).is_empty(), "{name}: {:?}", f.bindings);
    }
}

#[test]
fn enum_case_names_in_payload_less_patterns_bind_nothing() {
    let f = run(r#"func f(xs: [E], e: E) {
    let a = Foo()
    for case .a in xs { a.go() }
    switch e {
    case let .a:
        a.go()
    case .b:
        break
    case let .c?:
        break
    case let .d(x):
        print(x)
    case let .a(y):
        print(y)
    default:
        break
    }
    if case .g = e { }
    if case let .h = e { }
    for case let .k? in xs { }
    for case let z in xs { print(z) }
}
"#);
    // `.a` is the enum case, not a rebinding of the local `a`
    assert_eq!(ty(&f, "a"), ("", "Foo()"));
    assert_eq!(bind(&f, "a").line, 2);
    for name in ["b", "c", "d", "g", "h", "k"] {
        assert!(binds(&f, name).is_empty(), "{name}: {:?}", f.bindings);
    }
    // payloads and plain `case let` patterns still bind
    for name in ["x", "y", "z"] {
        assert_eq!(bind(&f, name).kind, BindKind::Local, "{name}");
    }
}

#[test]
fn deeply_nested_closures_types_and_patterns() {
    let depth = 3000;
    let mut src = String::from("import Foundation\nfunc deep(top: Foo) {\n    let x = ");
    src.push_str(&"{ ".repeat(depth));
    src.push('1');
    src.push_str(&" }".repeat(depth));
    src.push_str("\n    let y = ");
    src.push_str(&"(".repeat(depth));
    src.push('1');
    src.push_str(&")".repeat(depth));
    src.push_str("\n    let z: ");
    src.push_str(&"(".repeat(depth));
    src.push_str("Foo");
    src.push_str(&")".repeat(depth));
    src.push_str(" = make()\n    let ");
    src.push_str(&"(a, ".repeat(depth));
    src.push_str("last");
    src.push_str(&")".repeat(depth));
    src.push_str(" = t\n");
    for i in 0..depth {
        src.push_str(&format!("    if let v{i} = o {{\n"));
    }
    src.push_str(&"    }\n".repeat(depth));
    src.push_str("}\nfunc after(q: Bar) { let w = Baz() }\n");
    // run on a thread with a modest stack: the walker must bound its recursion
    let f = std::thread::Builder::new().stack_size(2 << 20).spawn(move || run(&src)).unwrap().join().unwrap();
    assert_eq!(imp(&f, "Foundation").kind, ImportKind::Namespace);
    assert_eq!(ty(&f, "top"), ("Foo", ""));
    assert_eq!(bind(&f, "x").kind, BindKind::Local);
    assert_eq!(bind(&f, "y").kind, BindKind::Local);
    // a type nested past the chain bound reduces to nothing rather than crashing
    assert_eq!(bind(&f, "z").kind, BindKind::Local);
    assert_eq!(bind(&f, "v0").kind, BindKind::Local);
    assert_eq!(ty(&f, "q"), ("Bar", ""));
    assert_eq!(ty(&f, "w"), ("", "Baz()"));
    // the deepest nodes are skipped, not crashed on
    assert!(binds(&f, "last").is_empty() && binds(&f, &format!("v{}", depth - 1)).is_empty());
    assert!(!binds(&f, "a").is_empty());
}

#[test]
fn nested_functions_are_local_fns() {
    let f = run(r#"class C {
    func outer() {
        func helper(a: Int) -> Int { return a }
        xs.map { v in
            func inClosure() {}
        }
    }
    var p: Int {
        func inGetter() {}
        return 1
    }
}
func top() {
    @discardableResult
    func inner() -> Int { 1 }
}
"#);
    for (name, line) in [("helper", 3), ("inClosure", 5), ("inGetter", 9), ("inner", 15)] {
        let b = bind(&f, name);
        assert_eq!((b.kind, b.line), (BindKind::LocalFn, line), "{name}");
        assert_eq!(scope_kind(&f, b), ScopeKind::Function, "{name}");
        assert_ne!(b.node_row, NONE, "{name} has a kernel node");
    }
    // methods and free functions are not bindings
    assert!(binds(&f, "outer").is_empty() && binds(&f, "top").is_empty(), "{:?}", f.bindings);
    // helper binds in outer's scope, its parameter in its own
    let outer = f.scopes.iter().position(|s| start_line(s) == 2).unwrap() as u32;
    assert_eq!(bind(&f, "helper").scope, outer);
    let a = bind(&f, "a");
    assert_eq!((a.kind, f.scopes[a.scope as usize].parent), (BindKind::Param, outer));
}

#[test]
fn accessor_parameters() {
    let f = run(r#"class C {
    var named: Foo {
        get { store }
        set(v) { store = v }
    }
    var observed: Bar = Bar() {
        willSet { print(newValue) }
        didSet(previous) { print(previous) }
    }
    subscript(i: Int) -> Elem {
        get { items[i] }
        set { items[i] = newValue }
    }
}
"#);
    assert_eq!(ty(&f, "v"), ("Foo", ""));
    assert_eq!(bind(&f, "v").kind, BindKind::Param);
    assert_eq!(ty(&f, "previous"), ("Bar", ""));
    assert_eq!(ty(&f, "observed"), ("Bar", "Bar()"));
    let nv = binds(&f, "newValue");
    assert_eq!(nv.len(), 2, "{nv:?}");
    assert!(nv.iter().any(|b| b.decl_type == "Bar" && b.line == 7), "{nv:?}");
    assert!(nv.iter().any(|b| b.decl_type == "Elem" && b.line == 12), "{nv:?}");
    assert!(nv.iter().all(|b| b.kind == BindKind::Param && scope_kind(&f, b) == ScopeKind::Function));
    // `get` has no implicit parameter
    assert!(binds(&f, "oldValue").is_empty(), "explicit didSet name replaces oldValue");
    // getter / setter scopes nest in the subscript's scope
    let sub = f.scopes.iter().position(|s| start_line(s) == 10).unwrap() as u32;
    for line in [11, 12] {
        assert_eq!(f.scopes.iter().find(|s| start_line(s) == line).unwrap().parent, sub, "line {line}");
    }
    assert_eq!(bind(&f, "i").scope, sub);
}

#[test]
fn top_level_code_binds_in_file_scope() {
    let f = run(r#"import Foundation

let app = App()
var config: Config = load()
func main() {}
"#);
    for name in ["app", "config"] {
        let b = bind(&f, name);
        assert_eq!((b.kind, b.scope), (BindKind::Local, 0), "{name}");
    }
    assert_eq!(ty(&f, "app"), ("", "App()"));
    assert_eq!(ty(&f, "config"), ("Config", "load()"));
}

#[test]
fn robustness_on_broken_and_empty_input() {
    let inputs = [
        "",
        "\n\n",
        "class Broken {\n  func f( {\n    let x = \n  }\n  var = \n",
        "import\nimport .\n@testable\n",
        "func f(_ : ) { let (a, = b; if let { } guard let else { } for in { } }",
        "xs.map { (a: , ) in }\nlet = Foo(\nvar y: = \nswitch { case let: }",
        "extension { var x: Int { get set } }\nprotocol { func }\nsubscript() -> { }",
        "let x = try? await\nlet y = Foo<\nlet z: [Foo: = [:]\nenum E { case let x }",
        "}}}{{{ let a = 1 }}}\u{0}\u{feff}",
        "class C { var p: Int { willSet(nv) didSet { } } init?(a b c: Int) { } deinit }",
    ];
    for src in inputs {
        // through the pipeline
        let _ = run(src);
        // and the extractor directly on the raw tree, whatever the kernel decided
        let mut p = tree_sitter::Parser::new();
        p.set_language(&cg_kernel::hooks::language("swift").unwrap()).unwrap();
        let tree = p.parse(src, None).unwrap();
        let opts = cg_kernel::ExtractOptions::default();
        // kernel rows only feed `row_at`; fall back to an empty file's rows if the kernel declines
        let emit = cg_kernel::extract_file("a.swift", src, "swift", &opts).or_else(|_| cg_kernel::extract_file("a.swift", "", "swift", &opts)).unwrap();
        let facts = cg_extract::facts::extract(Lang::Swift, &tree, src, FileRows::new(&emit));
        assert_eq!(facts.scopes[0].kind, ScopeKind::File);
        assert!(facts.scopes.iter().skip(1).all(|s| s.start <= s.end && (s.parent as usize) < facts.scopes.len()), "{src:?}");
        assert!(facts.bindings.iter().all(|b| !b.name.is_empty() && b.line >= 1 && (b.scope as usize) < facts.scopes.len()), "{src:?}");
    }
    let empty = run("");
    assert!(empty.imports.is_empty() && empty.bindings.is_empty() && empty.scopes.len() == 1);
}
