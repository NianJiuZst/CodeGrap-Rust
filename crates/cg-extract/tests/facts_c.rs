//! Facts for C and C++ (`facts/c.rs`): includes, using-directives, scopes and typed bindings.

use cg_core::Lang;
use cg_extract::*;

fn run(rel: &str, src: &str, lang: Lang) -> Facts {
    let b = extract_bytes(rel, src.as_bytes(), 0, lang, &ExtractConfig::new("."));
    assert_eq!(b.flags & fileflags::PANICKED, 0, "{:?}", b.errors);
    // a panic inside the facts walker is caught and reported here, with empty facts
    assert!(!b.errors.iter().any(|e| e.contains("panic")), "{:?}", b.errors);
    assert!(!b.facts.scopes.is_empty(), "facts were not built");
    b.facts
}

fn cpp(src: &str) -> Facts {
    run("a.cpp", src, Lang::Cpp)
}

fn c(src: &str) -> Facts {
    run("a.c", src, Lang::C)
}

fn binds<'a>(f: &'a Facts, name: &str) -> Vec<&'a BindingFact> {
    f.bindings.iter().filter(|b| b.name == name).collect()
}

fn bind<'a>(f: &'a Facts, name: &str) -> &'a BindingFact {
    let v = binds(f, name);
    assert_eq!(v.len(), 1, "expected one binding {name}: {:?}", f.bindings);
    v[0]
}

fn no_bind(f: &Facts, name: &str) {
    assert!(binds(f, name).is_empty(), "unexpected binding {name}: {:?}", f.bindings);
}

/// (kind, decl_type, init_ctor) of the single binding `name`.
fn info<'a>(f: &'a Facts, name: &str) -> (BindKind, &'a str, &'a str) {
    let b = bind(f, name);
    (b.kind, b.decl_type.as_str(), b.init_ctor.as_str())
}

fn imp<'a>(f: &'a Facts, spec: &str) -> &'a ImportFact {
    f.imports.iter().find(|i| i.spec == spec).unwrap_or_else(|| panic!("no import {spec}: {:?}", f.imports))
}

fn scope_kind(f: &Facts, b: &BindingFact) -> ScopeKind {
    f.scopes[b.scope as usize].kind
}

fn count(f: &Facts, kind: ScopeKind) -> usize {
    f.scopes.iter().filter(|s| s.kind == kind).count()
}

// ---------------------------------------------------------------------------------------------
// includes

const INCLUDES: &str = r#"#include "foo/bar.h"
#include <vector>
#include <fmt/core.h>
#ifdef USE_X
#include "x/inside_ifdef.h"
#elif defined(USE_Y)
#include <inside_elif.h>
#else
#include "inside_else.h"
#endif
#if FOO
#  include "inside_if.h"
#endif
#include MACRO_HEADER
#include CONCAT(a, b)
"#;

fn check_includes(f: &Facts) {
    let specs: Vec<&str> = f.imports.iter().map(|i| i.spec.as_str()).collect();
    assert_eq!(specs, ["foo/bar.h", "<vector>", "<fmt/core.h>", "x/inside_ifdef.h", "<inside_elif.h>", "inside_else.h", "inside_if.h"], "{:?}", f.imports);
    for i in &f.imports {
        assert_eq!((i.local.as_str(), i.imported.as_str(), i.kind, i.scope, i.type_only), ("", "", ImportKind::SideEffect, 0, false), "{i:?}");
    }
    assert_eq!(imp(f, "foo/bar.h").line, 1);
    assert_eq!(imp(f, "<vector>").line, 2);
    assert_eq!(imp(f, "x/inside_ifdef.h").line, 5);
    assert_eq!(imp(f, "inside_if.h").line, 12);
}

#[test]
fn c_includes() {
    check_includes(&c(INCLUDES));
}

#[test]
fn cpp_includes() {
    check_includes(&cpp(INCLUDES));
}

#[test]
fn headers() {
    // C++ header content: `.hpp` uses the C++ grammar; a `.h` may get the C grammar, where only
    // the includes are certain to survive
    let src = "#pragma once\n#include <memory>\n#include \"detail/x.hpp\"\nnamespace n {\nclass Y {\n  Foo* p;\n};\n}\n";
    for rel in ["a.h", "a.hpp"] {
        let f = run(rel, src, Lang::Cpp);
        assert_eq!(imp(&f, "<memory>").kind, ImportKind::SideEffect, "{rel}");
        assert_eq!(imp(&f, "detail/x.hpp").line, 3, "{rel}");
    }
    let f = run("a.hpp", src, Lang::Cpp);
    assert_eq!(info(&f, "p"), (BindKind::Field, "Foo", ""));
    assert_eq!(count(&f, ScopeKind::Block), 1);
    let f = run("b.h", "#ifndef B_H\n#define B_H\n#include <stdio.h>\nstruct b { struct b *next; };\nint b_fn(struct b *self);\n#endif\n", Lang::C);
    assert_eq!(imp(&f, "<stdio.h>").line, 3);
    assert_eq!(info(&f, "next"), (BindKind::Field, "b", ""));
    no_bind(&f, "self");
    no_bind(&f, "B_H");
}

// ---------------------------------------------------------------------------------------------
// using / namespace aliases / type aliases

#[test]
fn cpp_using_directives_and_aliases() {
    let f = cpp(r#"using namespace a::b;
using namespace std;
using a::b::Foo;
using ::Bar;
namespace x = a::b;
namespace y = c;
using X = a::Foo<int>;
using P = Foo*;
using Fn = void(int);
using N = int;
typedef a::Foo Y;
typedef std::vector<Foo> Vec;
typedef unsigned long ulong;
typedef void (*callback)(int);
"#);
    // the first `a::b` import is the using-directive (the namespace alias below shares the spec)
    let ns = imp(&f, "a::b");
    assert_eq!((ns.local.as_str(), ns.imported.as_str(), ns.kind, ns.scope, ns.line), ("*", "*", ImportKind::Namespace, 0, 1));
    let std = imp(&f, "std");
    assert_eq!((std.local.as_str(), std.imported.as_str(), std.kind), ("*", "*", ImportKind::Namespace));
    let foo = imp(&f, "a::b::Foo");
    assert_eq!((foo.local.as_str(), foo.imported.as_str(), foo.kind, foo.line), ("Foo", "Foo", ImportKind::Named, 3));
    let bar = imp(&f, "Bar");
    assert_eq!((bar.local.as_str(), bar.kind), ("Bar", ImportKind::Named));
    let x = f.imports.iter().find(|i| i.local == "x").expect("namespace alias x");
    assert_eq!((x.imported.as_str(), x.spec.as_str(), x.kind, x.line), ("*", "a::b", ImportKind::Named, 5));
    let y = f.imports.iter().find(|i| i.local == "y").expect("namespace alias y");
    assert_eq!((y.imported.as_str(), y.spec.as_str()), ("*", "c"));
    assert_eq!(f.imports.len(), 6, "{:?}", f.imports);

    assert_eq!(info(&f, "X"), (BindKind::Local, "a::Foo", ""));
    assert_eq!(bind(&f, "X").scope, 0);
    assert_eq!(bind(&f, "X").line, 7);
    assert_eq!(info(&f, "P"), (BindKind::Local, "Foo", ""));
    assert_eq!(info(&f, "Y"), (BindKind::Local, "a::Foo", ""));
    assert_eq!(info(&f, "Vec"), (BindKind::Local, "std::vector", ""));
    no_bind(&f, "Fn");
    no_bind(&f, "N");
    no_bind(&f, "ulong");
    no_bind(&f, "callback");
    // no package binding for C / C++
    assert!(f.bindings.iter().all(|b| b.kind != BindKind::Package));
}

#[test]
fn cpp_aliases_in_scopes() {
    let f = cpp(r#"namespace outer {
using namespace inner;
using Alias = lib::Thing;
}
class C {
  using value_type = Foo;
};
void f() {
  using namespace detail;
  using L = Local<int>;
  L l;
}
"#);
    let outer = imp(&f, "inner");
    assert_eq!(f.scopes[outer.scope as usize].kind, ScopeKind::Block);
    let alias = bind(&f, "Alias");
    assert_eq!((alias.decl_type.as_str(), scope_kind(&f, alias)), ("lib::Thing", ScopeKind::Block));
    let vt = bind(&f, "value_type");
    assert_eq!((vt.kind, vt.decl_type.as_str(), scope_kind(&f, vt)), (BindKind::Local, "Foo", ScopeKind::Class));
    let d = imp(&f, "detail");
    assert_eq!(f.scopes[d.scope as usize].kind, ScopeKind::Function);
    let l = bind(&f, "L");
    assert_eq!((l.decl_type.as_str(), scope_kind(&f, l)), ("Local", ScopeKind::Function));
    assert_eq!(info(&f, "l"), (BindKind::Local, "L", ""));
}

#[test]
fn c_typedefs() {
    let f = c(r#"typedef struct node node_t;
typedef struct { int a; Foo *p; } anon_t;
typedef Foo *FooPtr;
typedef int myint;
typedef int (*cmp_fn)(const void *, const void *);
"#);
    assert_eq!(info(&f, "node_t"), (BindKind::Local, "node", ""));
    assert_eq!(info(&f, "FooPtr"), (BindKind::Local, "Foo", ""));
    no_bind(&f, "anon_t");
    no_bind(&f, "myint");
    no_bind(&f, "cmp_fn");
    // the anonymous struct body is still a class scope with its fields
    let p = bind(&f, "p");
    assert_eq!((p.kind, p.decl_type.as_str(), scope_kind(&f, p)), (BindKind::Field, "Foo", ScopeKind::Class));
    assert_eq!(info(&f, "a"), (BindKind::Field, "", ""));
}

// ---------------------------------------------------------------------------------------------
// scopes

#[test]
fn cpp_scopes() {
    let f = cpp(r#"namespace ns {
class Widget : public Base {
public:
  Widget(int a) : a_(a) {}
  ~Widget() {}
  void inl(Bar b) { Foo z; }
  bool operator==(const Widget& o) const { return true; }
  void decl_only(Foo f);
  int a_;
};
}
void ns::Widget::decl_only(Foo f) {
  auto fn = [](int x) { return x; };
}
static int free_fn(Foo* p) { return 0; }
template <typename T> T tf(T t) { T loc; return loc; }
template <typename T> class TC { T val; };
"#);
    // Widget, TC
    assert_eq!(count(&f, ScopeKind::Class), 2, "{:?}", f.scopes);
    // ctor, dtor, inl, operator==, out-of-line decl_only, lambda, free_fn, tf
    assert_eq!(count(&f, ScopeKind::Function), 8, "{:?}", f.scopes);
    assert_eq!(count(&f, ScopeKind::Block), 1, "{:?}", f.scopes);

    // nesting: method in class in namespace in file
    let b = bind(&f, "b");
    assert_eq!((b.kind, b.decl_type.as_str()), (BindKind::Param, "Bar"));
    let m = &f.scopes[b.scope as usize];
    assert_eq!(m.kind, ScopeKind::Function);
    let cls = &f.scopes[m.parent as usize];
    assert_eq!(cls.kind, ScopeKind::Class);
    let nsb = &f.scopes[cls.parent as usize];
    assert_eq!((nsb.kind, nsb.parent), (ScopeKind::Block, 0));
    assert_eq!(bind(&f, "z").scope, b.scope);

    // operator / ctor params
    assert_eq!(info(&f, "o"), (BindKind::Param, "Widget", ""));
    assert_eq!(info(&f, "a"), (BindKind::Param, "", ""));
    // field in class
    let a_ = bind(&f, "a_");
    assert_eq!((a_.kind, scope_kind(&f, a_)), (BindKind::Field, ScopeKind::Class));

    // out-of-line method: one Param `f` (the in-class prototype binds nothing), parent = file
    let fp = bind(&f, "f");
    assert_eq!((fp.kind, fp.decl_type.as_str()), (BindKind::Param, "Foo"));
    assert_eq!(f.scopes[fp.scope as usize].parent, 0);
    // lambda nested in the out-of-line method
    let x = bind(&f, "x");
    assert_eq!(x.kind, BindKind::Param);
    let lam = &f.scopes[x.scope as usize];
    assert_eq!((lam.kind, lam.parent), (ScopeKind::Function, fp.scope));
    let fnb = bind(&f, "fn");
    assert_eq!((fnb.kind, fnb.scope, fnb.decl_type.as_str(), fnb.init_ctor.as_str()), (BindKind::Local, fp.scope, "", ""));

    // template function / class
    assert_eq!(info(&f, "t"), (BindKind::Param, "T", ""));
    assert_eq!(info(&f, "loc"), (BindKind::Local, "T", ""));
    let val = bind(&f, "val");
    assert_eq!((val.kind, val.decl_type.as_str(), scope_kind(&f, val)), (BindKind::Field, "T", ScopeKind::Class));
    assert_eq!(info(&f, "p"), (BindKind::Param, "Foo", ""));

    // scope ranges are ordered and nested
    for s in &f.scopes[1..] {
        let p = &f.scopes[s.parent as usize];
        assert!(p.start <= s.start && s.end <= p.end, "{s:?} not inside {p:?}");
    }
}

#[test]
fn c_scopes() {
    let f = c(r#"struct node {
  struct node *next;
};
union u { int i; float f; };
struct node *fwd;
int f(struct node *p) {
  return 0;
}
static void g(void) {}
"#);
    assert_eq!(count(&f, ScopeKind::Class), 2, "{:?}", f.scopes);
    assert_eq!(count(&f, ScopeKind::Function), 2, "{:?}", f.scopes);
    let p = bind(&f, "p");
    assert_eq!((p.kind, p.decl_type.as_str(), scope_kind(&f, p)), (BindKind::Param, "node", ScopeKind::Function));
    assert_eq!(f.scopes[p.scope as usize].parent, 0);
}

// ---------------------------------------------------------------------------------------------
// parameters

#[test]
fn cpp_params() {
    let f = cpp(r#"void run(const Foo& a, Foo* b, Foo** c, std::shared_ptr<Foo> d, const ns::Bar<int>& e,
         int g, unsigned long h, auto i, size_t j, Foo&& k, Foo l[], int, Foo*, const Foo&,
         Baz m = Baz(), void (*cb)(int), ::Glob n, typename T::type o, ...) {}
template <typename... Args> void var(Args&&... args) {}
"#);
    assert_eq!(info(&f, "a"), (BindKind::Param, "Foo", ""));
    assert_eq!(info(&f, "b"), (BindKind::Param, "Foo", ""));
    assert_eq!(info(&f, "c"), (BindKind::Param, "Foo", ""));
    assert_eq!(info(&f, "d"), (BindKind::Param, "std::shared_ptr", ""));
    assert_eq!(info(&f, "e"), (BindKind::Param, "ns::Bar", ""));
    assert_eq!(info(&f, "g"), (BindKind::Param, "", ""));
    assert_eq!(info(&f, "h"), (BindKind::Param, "", ""));
    assert_eq!(info(&f, "i"), (BindKind::Param, "", ""));
    assert_eq!(info(&f, "j"), (BindKind::Param, "", ""));
    assert_eq!(info(&f, "k"), (BindKind::Param, "Foo", ""));
    assert_eq!(info(&f, "l"), (BindKind::Param, "Foo", ""));
    assert_eq!(info(&f, "m"), (BindKind::Param, "Baz", ""));
    assert_eq!(info(&f, "cb"), (BindKind::Param, "", ""));
    assert_eq!(info(&f, "n"), (BindKind::Param, "Glob", ""));
    assert_eq!(info(&f, "o"), (BindKind::Param, "T::type", ""));
    no_bind(&f, "args");
    // unnamed params bind nothing: a..o only
    let params: Vec<&str> = f.bindings.iter().filter(|b| b.kind == BindKind::Param).map(|b| b.name.as_str()).collect();
    assert_eq!(params, ["a", "b", "c", "d", "e", "g", "h", "i", "j", "k", "l", "m", "cb", "n", "o"]);
    assert_eq!(bind(&f, "a").line, 1);
    assert_eq!(bind(&f, "m").line, 3);
}

#[test]
fn c_params() {
    let f = c(r#"int f(struct node *p, const char *s, size_t n, Foo f, unsigned int u, enum color col,
      const struct node *const q, Foo arr[], int (*cmp)(int, int), int, ...) {
  return 0;
}
int knr(a, b) int a; char *b; { return a; }
"#);
    assert_eq!(info(&f, "p"), (BindKind::Param, "node", ""));
    assert_eq!(info(&f, "s"), (BindKind::Param, "", ""));
    assert_eq!(info(&f, "n"), (BindKind::Param, "", ""));
    assert_eq!(info(&f, "f"), (BindKind::Param, "Foo", ""));
    assert_eq!(info(&f, "u"), (BindKind::Param, "", ""));
    assert_eq!(info(&f, "col"), (BindKind::Param, "color", ""));
    assert_eq!(info(&f, "q"), (BindKind::Param, "node", ""));
    assert_eq!(info(&f, "arr"), (BindKind::Param, "Foo", ""));
    assert_eq!(info(&f, "cmp"), (BindKind::Param, "", ""));
    assert_eq!(binds(&f, "a").iter().filter(|b| b.kind == BindKind::Param).count(), 1);
}

#[test]
fn prototypes_bind_nothing() {
    for (rel, lang) in [("a.c", Lang::C), ("a.cpp", Lang::Cpp)] {
        let f = run(
            rel,
            r#"int proto(Foo f, int x);
Foo *make_foo(Bar b);
extern void cb_proto(void (*cb)(int y));
#define MAX(a, b) ((a) > (b) ? (a) : (b))
#define LIMIT 10
"#,
            lang,
        );
        assert!(f.bindings.is_empty(), "{rel}: {:?}", f.bindings);
        assert_eq!(f.scopes.len(), 1, "{rel}: {:?}", f.scopes);
    }
}

// ---------------------------------------------------------------------------------------------
// locals

#[test]
fn cpp_locals() {
    let f = cpp(r#"void Widget::run() {
  Foo a;
  Foo b(1, 2);
  Foo c{1};
  Foo* d = new Foo(1);
  auto e = Foo(1);
  auto f = Foo{1};
  auto g = new Foo();
  auto h = make_foo();
  auto i = Foo::create();
  auto j = other.build();
  auto k = other->build();
  auto l = std::make_unique<Foo>(1);
  auto m = std::make_shared<Foo>();
  Foo n, *o;
  const Foo& ref = other;
  ns::Foo qq;
  unsigned long ul = 3;
  auto* ap = new ns::Bar;
  auto ns_ctor = ns::Thing(1);
  auto chain = this->repo.find(1).first();
  auto lit = 42;
  auto cast = static_cast<Foo*>(ptr);
  std::lock_guard<std::mutex> lock(mu_);
  std::unique_ptr<Foo> up = std::make_unique<Foo>();
  Foo x(a, b);
  auto paren = (Foo(2));
}
"#);
    assert_eq!(info(&f, "a"), (BindKind::Local, "Foo", ""));
    assert_eq!(info(&f, "b"), (BindKind::Local, "Foo", ""));
    assert_eq!(info(&f, "c"), (BindKind::Local, "Foo", ""));
    assert_eq!(info(&f, "d"), (BindKind::Local, "Foo", ""));
    assert_eq!(info(&f, "e"), (BindKind::Local, "Foo", ""));
    assert_eq!(info(&f, "f"), (BindKind::Local, "Foo", ""));
    assert_eq!(info(&f, "g"), (BindKind::Local, "Foo", ""));
    assert_eq!(info(&f, "h"), (BindKind::Local, "", "make_foo()"));
    assert_eq!(info(&f, "i"), (BindKind::Local, "", "Foo::create()"));
    assert_eq!(info(&f, "j"), (BindKind::Local, "", "other.build()"));
    assert_eq!(info(&f, "k"), (BindKind::Local, "", "other.build()"));
    assert_eq!(info(&f, "l"), (BindKind::Local, "Foo", ""));
    assert_eq!(info(&f, "m"), (BindKind::Local, "Foo", ""));
    assert_eq!(info(&f, "n"), (BindKind::Local, "Foo", ""));
    assert_eq!(info(&f, "o"), (BindKind::Local, "Foo", ""));
    assert_eq!(info(&f, "ref"), (BindKind::Local, "Foo", ""));
    assert_eq!(info(&f, "qq"), (BindKind::Local, "ns::Foo", ""));
    assert_eq!(info(&f, "ul"), (BindKind::Local, "", ""));
    assert_eq!(info(&f, "ap"), (BindKind::Local, "ns::Bar", ""));
    assert_eq!(info(&f, "ns_ctor"), (BindKind::Local, "ns::Thing", ""));
    assert_eq!(info(&f, "chain"), (BindKind::Local, "", "this.repo.find().first()"));
    assert_eq!(info(&f, "lit"), (BindKind::Local, "", ""));
    assert_eq!(info(&f, "cast"), (BindKind::Local, "Foo", ""));
    assert_eq!(info(&f, "lock"), (BindKind::Local, "std::lock_guard", ""));
    assert_eq!(info(&f, "up"), (BindKind::Local, "std::unique_ptr", ""));
    assert_eq!(info(&f, "x"), (BindKind::Local, "Foo", ""));
    assert_eq!(info(&f, "paren"), (BindKind::Local, "Foo", ""));
    // lines are 1-based and taken from the declarator name
    assert_eq!(bind(&f, "a").line, 2);
    assert_eq!(bind(&f, "o").line, 15);
    // all in the function scope
    let fs = bind(&f, "a").scope;
    assert_eq!(scope_kind(&f, bind(&f, "a")), ScopeKind::Function);
    assert!(f.bindings.iter().all(|b| b.scope == fs), "{:?}", f.bindings);
}

#[test]
fn cpp_auto_initialiser_shapes() {
    let f = cpp(r#"void run() {
  auto w = CreateWidget();
  auto gw = GetWidget(1);
  auto span = absl::MakeSpan(v);
  auto mac = MAKE_FOO(Foo);
  auto builder = Builder();
  auto factory = Widget::Create();
  auto tmpl = make_thing<Foo>();
  auto ctor_t = Box<int>(3);
  auto vec = std::vector<int>();
  auto deep = q->a.b->c();
  auto self = this->make();
  auto dc = dynamic_cast<const Foo&>(base);
  decltype(auto) da = get_foo();
  auto lam = [cap = make_cap(), &r = other]() {};
  Foo plain = make_foo();
  int n = obj.size();
}
"#);
    assert_eq!(info(&f, "w"), (BindKind::Local, "", "CreateWidget()"));
    assert_eq!(info(&f, "gw"), (BindKind::Local, "", "GetWidget()"));
    assert_eq!(info(&f, "span"), (BindKind::Local, "", "absl::MakeSpan()"));
    assert_eq!(info(&f, "mac"), (BindKind::Local, "", ""));
    assert_eq!(info(&f, "builder"), (BindKind::Local, "Builder", ""));
    assert_eq!(info(&f, "factory"), (BindKind::Local, "", "Widget::Create()"));
    assert_eq!(info(&f, "tmpl"), (BindKind::Local, "", "make_thing()"));
    assert_eq!(info(&f, "ctor_t"), (BindKind::Local, "Box", ""));
    assert_eq!(info(&f, "vec"), (BindKind::Local, "", "std::vector()"));
    assert_eq!(info(&f, "deep"), (BindKind::Local, "", "q.a.b.c()"));
    assert_eq!(info(&f, "self"), (BindKind::Local, "", "this.make()"));
    assert_eq!(info(&f, "dc"), (BindKind::Local, "Foo", ""));
    assert_eq!(info(&f, "da"), (BindKind::Local, "", "get_foo()"));
    assert_eq!(info(&f, "lam"), (BindKind::Local, "", ""));
    // init-captures are locals of the lambda
    let cap = bind(&f, "cap");
    assert_eq!((cap.kind, cap.init_ctor.as_str(), scope_kind(&f, cap)), (BindKind::Local, "make_cap()", ScopeKind::Function));
    assert_ne!(cap.scope, bind(&f, "lam").scope);
    assert_eq!(bind(&f, "r").scope, cap.scope);
    // an explicit type wins; initialisers of typed declarations are not kept
    assert_eq!(info(&f, "plain"), (BindKind::Local, "Foo", ""));
    assert_eq!(info(&f, "n"), (BindKind::Local, "", ""));
}

#[test]
fn cpp_function_declarator_shapes() {
    let f = cpp(r#"class Q {
 public:
  Q() = default;
  explicit Q(Bar b);
  virtual void pure(Foo f) = 0;
  static Q* instance;
  template <typename U> void tm(U u) {}
  friend void fr(Q& q) {}
  Foo operator+(const Foo& rhs) const;
};
Q::Q(Bar b2) : inner_([](int k) { return k; }) {}
int (*getfp(Foo xx))(int) { return 0; }
Foo* make(Foo mm) { return nullptr; }
Foo& ref_ret(Foo rr) { return rr; }
auto trailing(Foo tt) -> Foo { return {}; }
void vex() {
  Foo none();
  Foo (*fp)(int) = nullptr;
}
"#);
    no_bind(&f, "b");
    no_bind(&f, "f");
    no_bind(&f, "rhs");
    no_bind(&f, "none");
    no_bind(&f, "pure");
    assert_eq!(info(&f, "instance"), (BindKind::Field, "Q", ""));
    assert_eq!(info(&f, "u"), (BindKind::Param, "U", ""));
    assert_eq!(info(&f, "q"), (BindKind::Param, "Q", ""));
    assert_eq!(info(&f, "b2"), (BindKind::Param, "Bar", ""));
    assert_eq!(info(&f, "k"), (BindKind::Param, "", ""));
    for nm in ["xx", "mm", "rr", "tt"] {
        assert_eq!(info(&f, nm), (BindKind::Param, "Foo", ""), "{nm}");
    }
    assert_eq!(info(&f, "fp"), (BindKind::Local, "", ""));
    // the lambda in the member initialiser list nests in the constructor's scope
    let k = bind(&f, "k");
    assert_eq!(f.scopes[k.scope as usize].parent, bind(&f, "b2").scope);
    // Q() = default, tm, fr inside the class; ctor, lambda, getfp, make, ref_ret, trailing, vex outside
    let class_scope = bind(&f, "instance").scope;
    let in_class = f.scopes.iter().filter(|s| s.kind == ScopeKind::Function && s.parent == class_scope).count();
    assert_eq!(in_class, 3, "{:?}", f.scopes);
    assert_eq!(count(&f, ScopeKind::Function), 10, "{:?}", f.scopes);
}

#[test]
fn cpp_control_flow_locals() {
    let f = cpp(r#"void run(std::vector<Foo>& xs) {
  for (auto& q : xs) {}
  for (const Foo& r : xs) { r.go(); }
  for (Foo* rp : ptrs) {}
  for (auto& [key, value] : map) {}
  auto [s, t] = pair();
  const auto& [u, v] = get_pair();
  if (Foo* cond = get()) { cond->go(); }
  while (Bar* w = next()) {}
  if (auto it = m.find(1); it != m.end()) {}
  for (int i = 0; i < 3; ++i) {}
  for (auto iter = xs.begin(); iter != xs.end(); ++iter) {}
  switch (Kind k = kind()) { default: break; }
  try { risky(); } catch (const std::exception& ex) { log(ex); } catch (...) {}
}
"#);
    assert_eq!(info(&f, "q"), (BindKind::Local, "", ""));
    assert_eq!(info(&f, "r"), (BindKind::Local, "Foo", ""));
    assert_eq!(info(&f, "rp"), (BindKind::Local, "Foo", ""));
    for nm in ["key", "value", "s", "t", "u", "v"] {
        assert_eq!(info(&f, nm), (BindKind::Local, "", ""), "{nm}");
    }
    assert_eq!(info(&f, "cond"), (BindKind::Local, "Foo", ""));
    assert_eq!(info(&f, "w"), (BindKind::Local, "Bar", ""));
    assert_eq!(info(&f, "it"), (BindKind::Local, "", "m.find()"));
    assert_eq!(info(&f, "i"), (BindKind::Local, "", ""));
    assert_eq!(info(&f, "iter"), (BindKind::Local, "", "xs.begin()"));
    assert_eq!(info(&f, "k"), (BindKind::Local, "Kind", ""));
    assert_eq!(info(&f, "ex"), (BindKind::Local, "std::exception", ""));
    assert_eq!(info(&f, "xs"), (BindKind::Param, "std::vector", ""));
    assert_eq!(bind(&f, "s").line, 6);
    assert_eq!(bind(&f, "cond").line, 8);
}

#[test]
fn cpp_lambdas() {
    let f = cpp(r#"void run() {
  auto f = [](int x, Foo y) { return x; };
  auto g = [this, &z, w = make_w()](const Bar& b) mutable { Baz inner; };
  call([&](Qux* q) { q->go(); });
}
"#);
    let fs = bind(&f, "f");
    assert_eq!((fs.kind, fs.decl_type.as_str(), fs.init_ctor.as_str()), (BindKind::Local, "", ""));
    assert_eq!(info(&f, "g"), (BindKind::Local, "", ""));
    let x = bind(&f, "x");
    let y = bind(&f, "y");
    assert_eq!((x.kind, x.decl_type.as_str()), (BindKind::Param, ""));
    assert_eq!((y.kind, y.decl_type.as_str()), (BindKind::Param, "Foo"));
    assert_eq!(x.scope, y.scope);
    let lam = &f.scopes[x.scope as usize];
    assert_eq!((lam.kind, lam.parent), (ScopeKind::Function, fs.scope));
    let b = bind(&f, "b");
    assert_eq!((b.kind, b.decl_type.as_str()), (BindKind::Param, "Bar"));
    assert_eq!(bind(&f, "inner").scope, b.scope);
    assert_eq!(info(&f, "inner"), (BindKind::Local, "Baz", ""));
    // init-capture
    let w = bind(&f, "w");
    assert_eq!((w.kind, w.init_ctor.as_str(), w.scope), (BindKind::Local, "make_w()", b.scope));
    // lambda passed as an argument
    assert_eq!(info(&f, "q"), (BindKind::Param, "Qux", ""));
    assert_eq!(count(&f, ScopeKind::Function), 4);
}

#[test]
fn c_locals() {
    let f = c(r#"int f(struct node *p) {
  struct node *cur = p->next;
  node_t local;
  int i = 0, j;
  Foo *a = make(), b;
  struct node arr[4];
  for (int k = 0; k < 3; k++) {}
  if (1) { Bar nested; }
  return 0;
}
"#);
    assert_eq!(info(&f, "cur"), (BindKind::Local, "node", ""));
    assert_eq!(info(&f, "local"), (BindKind::Local, "node_t", ""));
    assert_eq!(info(&f, "i"), (BindKind::Local, "", ""));
    assert_eq!(info(&f, "j"), (BindKind::Local, "", ""));
    assert_eq!(info(&f, "a"), (BindKind::Local, "Foo", ""));
    assert_eq!(info(&f, "b"), (BindKind::Local, "Foo", ""));
    assert_eq!(info(&f, "arr"), (BindKind::Local, "node", ""));
    assert_eq!(info(&f, "k"), (BindKind::Local, "", ""));
    assert_eq!(info(&f, "nested"), (BindKind::Local, "Bar", ""));
    assert_eq!(bind(&f, "cur").line, 2);
    assert_eq!(bind(&f, "b").line, 5);
}

// ---------------------------------------------------------------------------------------------
// fields and globals

#[test]
fn cpp_fields() {
    let f = cpp(r#"class Node : public Base {
public:
  Node* next;
  std::vector<Foo> v, *w;
  static int count;
  const ns::Bar& bar_;
  Baz baz_ = Baz();
  std::function<void()> cb_ = [](int arg) {};
  void method(Foo f);
  Node();
  struct Inner { Qux q; } inner_;
  union { int as_int; Flt as_flt; };
private:
  int bits : 3;
};
struct Plain { Foo a; };
"#);
    let cls = |name: &str| scope_kind(&f, bind(&f, name));
    assert_eq!(info(&f, "next"), (BindKind::Field, "Node", ""));
    assert_eq!(info(&f, "v"), (BindKind::Field, "std::vector", ""));
    assert_eq!(info(&f, "w"), (BindKind::Field, "std::vector", ""));
    assert_eq!(info(&f, "count"), (BindKind::Field, "", ""));
    assert_eq!(info(&f, "bar_"), (BindKind::Field, "ns::Bar", ""));
    assert_eq!(info(&f, "baz_"), (BindKind::Field, "Baz", ""));
    assert_eq!(info(&f, "cb_"), (BindKind::Field, "std::function", ""));
    assert_eq!(info(&f, "inner_"), (BindKind::Field, "Inner", ""));
    assert_eq!(info(&f, "q"), (BindKind::Field, "Qux", ""));
    assert_eq!(info(&f, "bits"), (BindKind::Field, "", ""));
    assert_eq!(info(&f, "a"), (BindKind::Field, "Foo", ""));
    for nm in ["next", "v", "count", "inner_", "q", "as_int", "a"] {
        assert_eq!(cls(nm), ScopeKind::Class, "{nm}");
    }
    // anonymous union members are fields of the enclosing class
    assert_eq!(bind(&f, "as_flt").scope, bind(&f, "next").scope);
    assert_eq!(info(&f, "as_flt"), (BindKind::Field, "Flt", ""));
    // nested struct is its own class scope inside Node's
    let q = bind(&f, "q");
    assert_eq!(f.scopes[q.scope as usize].parent, bind(&f, "next").scope);
    // method / ctor declarations are not fields
    no_bind(&f, "method");
    no_bind(&f, "Node");
    no_bind(&f, "f");
    // lambda in a default member initialiser
    assert_eq!(info(&f, "arg"), (BindKind::Param, "", ""));
    assert_eq!(bind(&f, "next").line, 3);
}

#[test]
fn c_struct_fields() {
    let f = c(r#"struct node {
  struct node *next;
  int val;
  Foo *foo;
  char name[16];
  void (*cb)(struct node *);
  struct { int x; Bar bar; } pos;
  union { long l; Dbl d; };
  unsigned flag : 1;
};
"#);
    assert_eq!(info(&f, "next"), (BindKind::Field, "node", ""));
    assert_eq!(info(&f, "val"), (BindKind::Field, "", ""));
    assert_eq!(info(&f, "foo"), (BindKind::Field, "Foo", ""));
    assert_eq!(info(&f, "name"), (BindKind::Field, "", ""));
    assert_eq!(info(&f, "cb"), (BindKind::Field, "", ""));
    assert_eq!(info(&f, "pos"), (BindKind::Field, "", ""));
    assert_eq!(info(&f, "bar"), (BindKind::Field, "Bar", ""));
    assert_eq!(info(&f, "d"), (BindKind::Field, "Dbl", ""));
    assert_eq!(info(&f, "flag"), (BindKind::Field, "", ""));
    let outer = bind(&f, "next").scope;
    assert_eq!(scope_kind(&f, bind(&f, "next")), ScopeKind::Class);
    assert_eq!(bind(&f, "l").scope, outer);
    assert_ne!(bind(&f, "bar").scope, outer);
    assert_eq!(f.scopes[bind(&f, "bar").scope as usize].parent, outer);
}

#[test]
fn globals() {
    let f = cpp(r#"static Foo g_foo;
Foo* g = nullptr;
extern Bar g_bar;
const int kMax = 3;
auto g_auto = make_global();
namespace ns { Baz in_ns; }
int Widget::count = 0;
class Cfg { int x; } g_cfg;
"#);
    for (nm, ty) in [("g_foo", "Foo"), ("g", "Foo"), ("g_bar", "Bar"), ("kMax", ""), ("g_cfg", "Cfg")] {
        let b = bind(&f, nm);
        assert_eq!((b.kind, b.decl_type.as_str(), b.scope), (BindKind::Local, ty, 0), "{nm}");
    }
    assert_eq!(info(&f, "g_auto"), (BindKind::Local, "", "make_global()"));
    let in_ns = bind(&f, "in_ns");
    assert_eq!((in_ns.decl_type.as_str(), scope_kind(&f, in_ns)), ("Baz", ScopeKind::Block));
    // out-of-line static member definitions declare nothing new
    no_bind(&f, "count");
    no_bind(&f, "Widget");

    let f = c("static struct node g_head;\nFoo *g_ptr = 0;\nint counter;\n");
    for (nm, ty) in [("g_head", "node"), ("g_ptr", "Foo"), ("counter", "")] {
        let b = bind(&f, nm);
        assert_eq!((b.kind, b.decl_type.as_str(), b.scope), (BindKind::Local, ty, 0), "{nm}");
    }
}

#[test]
fn extern_c_and_preproc_blocks() {
    let f = cpp(r#"extern "C" {
#include "c_api.h"
Foo* c_global;
int c_fn(Bar* b) { return 0; }
}
#ifdef DEBUG
static Dbg g_dbg;
void dbg(Dbg* d) {}
#endif
class K {
#if X
  Foo guarded;
#endif
};
"#);
    assert_eq!(imp(&f, "c_api.h").kind, ImportKind::SideEffect);
    assert_eq!(info(&f, "c_global"), (BindKind::Local, "Foo", ""));
    assert_eq!(info(&f, "b"), (BindKind::Param, "Bar", ""));
    assert_eq!(info(&f, "g_dbg"), (BindKind::Local, "Dbg", ""));
    assert_eq!(info(&f, "d"), (BindKind::Param, "Dbg", ""));
    let g = bind(&f, "guarded");
    assert_eq!((g.kind, g.decl_type.as_str(), scope_kind(&f, g)), (BindKind::Field, "Foo", ScopeKind::Class));
}

// ---------------------------------------------------------------------------------------------
// robustness

#[test]
fn empty_and_trivial_files() {
    for (rel, lang) in [("a.c", Lang::C), ("a.cpp", Lang::Cpp), ("a.h", Lang::C), ("a.hpp", Lang::Cpp)] {
        for src in ["", "\n", "// only a comment\n", ";", "#", "#include", "#include \"", "{", "}", "int", "class", "template <"] {
            let f = run(rel, src, lang);
            assert!(!f.scopes.is_empty());
            assert_eq!(f.scopes[0].kind, ScopeKind::File);
        }
    }
}

#[test]
fn macro_heavy_code_with_errors() {
    let src = r#"#include "base.h"
#define DECLARE(x) int x##_v
BEGIN_NAMESPACE(foo)
DECLARE_CLASS(Widget) : PUBLIC_BASE(Base) {
  FIELD(int, a) = 3;
  Foo* ok_field;
  METHOD(void, run)(Foo* p) LOCKS_EXCLUDED(mu_) {
    FOR_EACH(x, xs) { Bar b; }
    auto y = MAKE(Foo);
    Foo* z = new Foo(;
  }
};
END_NAMESPACE(foo)
void after(Qux q) {
  Qux local;
  @@@ $$$ ;;; ))) }}
}
TEST_F(MyTest, Works) {
  Foo t;
  EXPECT_EQ(t.v(), 1);
}
int tail_fn(Baz* last) { return 0; }
"#;
    for (rel, lang) in [("a.cpp", Lang::Cpp), ("a.c", Lang::C)] {
        let f = run(rel, src, lang);
        assert_eq!(imp(&f, "base.h").kind, ImportKind::SideEffect);
        // the well-formed function after the junk is still found
        let last = bind(&f, "last");
        assert_eq!((last.kind, last.decl_type.as_str()), (BindKind::Param, "Baz"), "{rel}");
        for b in &f.bindings {
            assert!((b.scope as usize) < f.scopes.len());
            assert!(b.line >= 1 && b.line <= 23, "{b:?}");
        }
        for s in &f.scopes[1..] {
            assert!((s.parent as usize) < f.scopes.len());
        }
    }
}

#[test]
fn annotation_macros_and_misparsed_access_labels() {
    // a semicolon-less macro before an access label swallows the label into a bogus field
    let qt = cpp("class W {\n  Q_OBJECT\n public:\n  Foo* after_;\n};\nclass V {\n  Foo* ok_;\n};\n");
    no_bind(&qt, "public");
    assert_eq!(info(&qt, "ok_"), (BindKind::Field, "Foo", ""));

    // stacked attribute macros read as `MACRO_A MACRO_B;`; ALL_CAPS constants with real types stay
    let pair = cpp("_GLIBCXX_NODISCARD _GLIBCXX20_CONSTEXPR\nbool empty() const;\nstatic const int MAX_SIZE = 4;\nFOO_T FOO_INSTANCE;\nHANDLE h;\n");
    no_bind(&pair, "_GLIBCXX20_CONSTEXPR");
    no_bind(&pair, "FOO_INSTANCE");
    assert_eq!(info(&pair, "MAX_SIZE"), (BindKind::Local, "", ""));
    assert_eq!(info(&pair, "h"), (BindKind::Local, "HANDLE", ""));

    let f = cpp(r#"class S {
 public:
  std::unique_ptr<Repo> repo_ ABSL_GUARDED_BY(mu_);
  Foo* ptr_ PT_GUARDED_BY(mu_);
  int count_ GUARDED_BY(mu_) = 0;
  Bar bar_ ABSL_GUARDED_BY(mu_) ABSL_ACQUIRED_AFTER(x);
  void Run() ABSL_LOCKS_EXCLUDED(mu_);
  DISALLOW_COPY_AND_ASSIGN(S);
};
Foo g_foo GUARDED_BY(g_mu);
void f() {
  Foo local GUARDED_BY(mu);
}
"#);
    assert_eq!(info(&f, "repo_"), (BindKind::Field, "std::unique_ptr", ""));
    assert_eq!(info(&f, "ptr_"), (BindKind::Field, "Foo", ""));
    assert_eq!(info(&f, "count_"), (BindKind::Field, "", ""));
    assert_eq!(info(&f, "bar_"), (BindKind::Field, "Bar", ""));
    assert_eq!(info(&f, "g_foo"), (BindKind::Local, "Foo", ""));
    assert_eq!(bind(&f, "g_foo").scope, 0);
    assert_eq!(info(&f, "local"), (BindKind::Local, "Foo", ""));
    for nm in ["public", "Run", "ABSL_LOCKS_EXCLUDED", "DISALLOW_COPY_AND_ASSIGN", "GUARDED_BY", "PT_GUARDED_BY", "mu_", "S"] {
        no_bind(&f, nm);
    }
}

#[test]
fn c_identifiers_that_are_cpp_keywords() {
    let f = c(r#"struct list { struct list *new; void *private; int class; };
void add(struct list *new, struct list *this) {
  struct list *template = new;
}
"#);
    assert_eq!(binds(&f, "new").len(), 2, "{:?}", f.bindings);
    assert!(binds(&f, "new").iter().any(|b| b.kind == BindKind::Field && b.decl_type == "list"));
    assert!(binds(&f, "new").iter().any(|b| b.kind == BindKind::Param && b.decl_type == "list"));
    assert_eq!(info(&f, "private"), (BindKind::Field, "", ""));
    assert_eq!(info(&f, "class"), (BindKind::Field, "", ""));
    assert_eq!(info(&f, "this"), (BindKind::Param, "list", ""));
    assert_eq!(info(&f, "template"), (BindKind::Local, "list", ""));
}

#[test]
fn deeply_nested_blocks() {
    let depth = 2000;
    let mut src = String::from("void deep(Foo* top) {\n");
    for i in 0..depth {
        src.push_str(&format!("{{ Foo v{i};\n"));
    }
    for _ in 0..depth {
        src.push('}');
    }
    src.push_str("\n}\nint after(Bar* b) { return 0; }\n");
    for (rel, lang) in [("a.cpp", Lang::Cpp), ("a.c", Lang::C)] {
        let src = src.clone();
        // run on a thread with a modest stack: the walker must bound its recursion
        let f = std::thread::Builder::new().stack_size(2 << 20).spawn(move || run(rel, &src, lang)).unwrap().join().unwrap();
        assert_eq!(info(&f, "top"), (BindKind::Param, "Foo", ""));
        assert_eq!(info(&f, "v0"), (BindKind::Local, "Foo", ""));
        assert_eq!(info(&f, "b"), (BindKind::Param, "Bar", ""));
        // the deepest declarations are skipped, not crashed on
        assert!(binds(&f, &format!("v{}", depth - 1)).is_empty());
    }
}

#[test]
fn deeply_nested_expressions_and_lambdas() {
    let mut src = String::from("void f() {\n  auto x = ");
    for _ in 0..1500 {
        src.push_str("[](){ return ");
    }
    src.push('1');
    for _ in 0..1500 {
        src.push_str("; }()");
    }
    src.push_str(";\n  int y = ");
    src.push_str(&"(".repeat(3000));
    src.push('1');
    src.push_str(&")".repeat(3000));
    src.push_str(";\n}\n");
    let f = std::thread::Builder::new().stack_size(2 << 20).spawn(move || run("a.cpp", &src, Lang::Cpp)).unwrap().join().unwrap();
    assert_eq!(bind(&f, "x").kind, BindKind::Local);
    assert_eq!(bind(&f, "y").kind, BindKind::Local);
}
