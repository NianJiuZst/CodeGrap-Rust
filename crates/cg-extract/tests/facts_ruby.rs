//! Ruby facts: side-effect imports, scopes, params, locals, fields and constants.

use cg_core::Lang;
use cg_extract::*;

fn run(src: &str) -> Facts {
    let b = extract_bytes("lib/a.rb", src.as_bytes(), 0, Lang::Ruby, &ExtractConfig::new("."));
    assert_eq!(b.flags & fileflags::PANICKED, 0, "{:?}", b.errors);
    assert!(!b.errors.iter().any(|e| e.contains("facts")), "{:?}", b.errors);
    b.facts
}

/// All bindings called `name`.
fn binds<'a>(f: &'a Facts, name: &str) -> Vec<&'a BindingFact> {
    f.bindings.iter().filter(|b| b.name == name).collect()
}

/// The single binding called `name`.
fn bind<'a>(f: &'a Facts, name: &str) -> &'a BindingFact {
    let all = binds(f, name);
    assert_eq!(all.len(), 1, "expected one binding {name}: {:?}", f.bindings);
    all[0]
}

fn no_bind(f: &Facts, name: &str) {
    assert!(binds(f, name).is_empty(), "unexpected binding {name}: {:?}", f.bindings);
}

/// (kind, decl_type, init_ctor) of the single binding `name`.
fn info<'a>(f: &'a Facts, name: &str) -> (BindKind, &'a str, &'a str) {
    let b = bind(f, name);
    (b.kind, b.decl_type.as_str(), b.init_ctor.as_str())
}

/// (kind, decl_type, init_ctor, line) of every binding `name`, in order.
fn infos<'a>(f: &'a Facts, name: &str) -> Vec<(BindKind, &'a str, &'a str, u32)> {
    binds(f, name).into_iter().map(|b| (b.kind, b.decl_type.as_str(), b.init_ctor.as_str(), b.line)).collect()
}

/// Run on a thread with a modest stack: the walker must bound its own recursion.
fn run_small_stack(src: String) -> Facts {
    std::thread::Builder::new().stack_size(2 << 20).spawn(move || run(&src)).unwrap().join().unwrap()
}

fn scope_kind(f: &Facts, scope: u32) -> ScopeKind {
    f.scopes[scope as usize].kind
}

fn start_line(f: &Facts, scope: u32) -> u32 {
    (f.scopes[scope as usize].start >> 32) as u32
}

fn specs(f: &Facts) -> Vec<&str> {
    f.imports.iter().map(|i| i.spec.as_str()).collect()
}

// ------------------------------------------------------------------ imports

#[test]
fn require_forms() {
    let f = run(r#"require 'foo/bar'
require "foo"
require('paren')
require("paren2")
load 'x.rb'
load('y.rb', true)
require %q(pct)
"#);
    assert_eq!(specs(&f), ["foo/bar", "foo", "paren", "paren2", "x.rb", "y.rb", "pct"]);
    for (i, imp) in f.imports.iter().enumerate() {
        assert_eq!(imp.kind, ImportKind::SideEffect);
        assert_eq!((imp.local.as_str(), imp.imported.as_str()), ("", ""));
        assert_eq!(imp.scope, 0);
        assert!(!imp.type_only);
        assert_eq!(imp.line, i as u32 + 1);
    }
}

#[test]
fn require_relative_specs_start_with_dot() {
    let f = run(r#"require_relative '../lib/x'
require_relative './y'
require_relative "sib"
require_relative('nested/z')
require_relative "../../up"
"#);
    assert_eq!(specs(&f), ["../lib/x", "./y", "./sib", "./nested/z", "../../up"]);
    assert!(f.imports.iter().all(|i| i.kind == ImportKind::SideEffect && i.spec.starts_with('.')));
}

#[test]
fn require_rejects_dynamic_arguments() {
    let f = run(r#"require "a#{b}"
require_relative "x/#{name}"
require ENV['X']
require name
require_relative File.join('a', 'b')
require ''
require :sym
require
Kernel.require 'k'
obj.load 'l'
"#);
    assert!(f.imports.is_empty(), "{:?}", f.imports);
}

#[test]
fn require_inside_method_and_modifier() {
    let f = run(r#"require 'top' if defined?(X)
def setup
  require 'inner'
end
"#);
    assert_eq!(specs(&f), ["top", "inner"]);
    assert_eq!(f.imports[0].scope, 0);
    let s = f.imports[1].scope;
    assert_eq!((scope_kind(&f, s), f.imports[1].line), (ScopeKind::Function, 3));
}

#[test]
fn no_package_binding() {
    let f = run("module A\n  class B\n  end\nend\n");
    assert!(f.bindings.iter().all(|b| b.kind != BindKind::Package), "{:?}", f.bindings);
}

// ------------------------------------------------------------------ scopes

#[test]
fn scopes_for_classes_methods_blocks_and_lambdas() {
    let f = run(r#"module M
  class C < Base
    class << self
      def s; end
    end
    def m
      [1].each do |x| end
      [2].map { |y| y }
      l = ->(z) { z }
      k = lambda { |w| w }
    end
    def self.t; end
  end
end
"#);
    use ScopeKind::*;
    let got: Vec<(ScopeKind, u32, u32)> = f.scopes.iter().enumerate().map(|(i, s)| (s.kind, s.parent, start_line(&f, i as u32))).collect();
    let none = cg_core::NONE;
    assert_eq!(
        got,
        [
            (File, none, 0),
            (Class, 0, 1),     // module M
            (Class, 1, 2),     // class C
            (Class, 2, 3),     // class << self
            (Function, 3, 4),  // def s
            (Function, 2, 6),  // def m
            (Function, 5, 7),  // do |x| ... end
            (Function, 5, 8),  // { |y| y }
            (Function, 5, 9),  // ->(z) { z }: a single scope
            (Function, 5, 10), // lambda { |w| w }
            (Function, 2, 12), // def self.t
        ]
    );
    let end_line = (f.scopes[2].end >> 32) as u32;
    assert_eq!(end_line, 13);
}

#[test]
fn nested_def_is_a_scope_without_local_fn() {
    let f = run(r#"def outer
  def inner(v)
  end
end
"#);
    assert!(f.bindings.iter().all(|b| b.kind != BindKind::LocalFn), "{:?}", f.bindings);
    let v = bind(&f, "v");
    assert_eq!(scope_kind(&f, v.scope), ScopeKind::Function);
    let parent = f.scopes[v.scope as usize].parent;
    assert_eq!((scope_kind(&f, parent), f.scopes[parent as usize].parent), (ScopeKind::Function, 0));
}

// ------------------------------------------------------------------ params

#[test]
fn method_params_of_every_kind() {
    let f = run(r#"class C
  def run(a, b = 1, *rest, k:, j: 2, **opts, &blk)
  end
  def self.build(x, (p, q))
  end
  def anon(*, **, &)
  end
  def fwd(...)
  end
end
"#);
    for name in ["a", "b", "rest", "k", "j", "opts", "blk", "x", "p", "q"] {
        let b = bind(&f, name);
        assert_eq!((b.kind, b.decl_type.as_str(), b.init_ctor.as_str()), (BindKind::Param, "", ""), "{name}");
        assert_eq!(scope_kind(&f, b.scope), ScopeKind::Function, "{name}");
        assert_eq!(scope_kind(&f, f.scopes[b.scope as usize].parent), ScopeKind::Class, "{name}");
    }
    assert_eq!(bind(&f, "a").line, 2);
    assert_eq!(bind(&f, "x").line, 4);
    assert_eq!(bind(&f, "a").scope, bind(&f, "blk").scope);
    assert_ne!(bind(&f, "a").scope, bind(&f, "x").scope);
    // anonymous and forwarding params bind nothing; no sigils leak into names
    assert_eq!(f.bindings.iter().filter(|b| b.kind == BindKind::Param).count(), 10, "{:?}", f.bindings);
    assert!(f.bindings.iter().all(|b| !b.name.starts_with(['*', '&', ':'])));
}

#[test]
fn method_params_without_parens() {
    let f = run("def go a, b = 2\n  a\nend\n");
    assert_eq!(info(&f, "a"), (BindKind::Param, "", ""));
    assert_eq!(info(&f, "b"), (BindKind::Param, "", ""));
}

#[test]
fn block_params_with_destructuring() {
    let f = run(r#"def m
  pairs.each do |a, (b, c)|
  end
  xs.map { |x, *ys, k: 1, &f; loc| x }
end
"#);
    for name in ["a", "b", "c"] {
        let b = bind(&f, name);
        assert_eq!((b.kind, b.line), (BindKind::Param, 2), "{name}");
        assert_eq!(scope_kind(&f, b.scope), ScopeKind::Function);
        assert_ne!(b.scope, 1, "block params live in the block scope, not the method");
        assert_eq!(f.scopes[b.scope as usize].parent, 1);
    }
    for name in ["x", "ys", "k", "f"] {
        assert_eq!(info(&f, name), (BindKind::Param, "", ""), "{name}");
    }
    // `; loc` declares a block-local variable
    assert_eq!(info(&f, "loc"), (BindKind::Local, "", ""));
    assert_eq!(bind(&f, "loc").scope, bind(&f, "x").scope);
}

#[test]
fn lambda_params() {
    let f = run(r#"l = ->(m, n = 1) { m }
d = -> o do o end
"#);
    assert_eq!(info(&f, "m"), (BindKind::Param, "", ""));
    assert_eq!(info(&f, "n"), (BindKind::Param, "", ""));
    assert_eq!(info(&f, "o"), (BindKind::Param, "", ""));
    assert_eq!(scope_kind(&f, bind(&f, "m").scope), ScopeKind::Function);
    assert_eq!(f.scopes.len(), 3, "one scope per lambda: {:?}", f.scopes);
}

// ------------------------------------------------------------------ locals

#[test]
fn local_types_and_initialisers() {
    let f = run(r#"def build
  y = Foo::Bar.new
  z = ::Top.new
  zz = ::A::B.new(1)
  n = Foo.new(1, 2)
  w = Foo.create(1)
  ww = Foo::Bar.create
  v = make
  u = make(1)
  e = make()
  s = make 1
  t = other.build
  c = Foo.new.build
  i = @repo.find(1)
  p = (Foo.new)
  r = Foo.new rescue nil
  lit = 1
  str = "s"
  arr = [Foo.new]
end
"#);
    assert_eq!(info(&f, "y"), (BindKind::Local, "Foo::Bar", ""));
    assert_eq!(info(&f, "z"), (BindKind::Local, "Top", ""));
    assert_eq!(info(&f, "zz"), (BindKind::Local, "A::B", ""));
    assert_eq!(info(&f, "n"), (BindKind::Local, "Foo", ""));
    assert_eq!(info(&f, "w"), (BindKind::Local, "", "Foo.create()"));
    assert_eq!(info(&f, "ww"), (BindKind::Local, "", "Foo::Bar.create()"));
    assert_eq!(info(&f, "v"), (BindKind::Local, "", ""), "a bare identifier may be a variable");
    assert_eq!(info(&f, "u"), (BindKind::Local, "", "make()"));
    assert_eq!(info(&f, "e"), (BindKind::Local, "", "make()"));
    assert_eq!(info(&f, "s"), (BindKind::Local, "", "make()"));
    assert_eq!(info(&f, "t"), (BindKind::Local, "", "other.build()"));
    assert_eq!(info(&f, "c"), (BindKind::Local, "", "Foo.new().build()"));
    assert_eq!(info(&f, "i"), (BindKind::Local, "", "@repo.find()"));
    assert_eq!(info(&f, "p"), (BindKind::Local, "Foo", ""));
    assert_eq!(info(&f, "r"), (BindKind::Local, "Foo", ""));
    for name in ["lit", "str", "arr"] {
        assert_eq!(info(&f, name), (BindKind::Local, "", ""), "{name}");
    }
    assert_eq!(bind(&f, "y").line, 2);
    assert_eq!(bind(&f, "arr").line, 19);
    assert!(f.bindings.iter().filter(|b| b.kind == BindKind::Local).all(|b| scope_kind(&f, b.scope) == ScopeKind::Function));
}

#[test]
fn local_binding_line_is_the_name_line() {
    let f = run("def m\n  x =\n    Foo.new\nend\n");
    assert_eq!((bind(&f, "x").line, bind(&f, "x").decl_type.as_str()), (2, "Foo"));
}

#[test]
fn typed_reassignment_adds_a_binding_at_its_line() {
    use BindKind::*;
    let f = run(r#"def a
  x = Foo.new
  x = Bar.new
  x ||= Svc.create(1)
  x.run
end
def b
  y = nil
  y = compute
  y = Foo.new
  y.run
end
def c(p)
  p = Foo.new
end
def d
  x = Bar.new
end
"#);
    // the resolver takes the latest binding before a use, so `x.run` on line 5 sees `Svc.create()`
    // and `y.run` on line 11 sees `Foo`
    let xs = binds(&f, "x");
    assert_eq!(infos(&f, "x"), [(Local, "Foo", "", 2), (Local, "Bar", "", 3), (Local, "", "Svc.create()", 4), (Local, "Bar", "", 17)]);
    assert_eq!(xs[0].scope, xs[2].scope);
    assert_ne!(xs[0].scope, xs[3].scope, "one variable per method");
    // the untyped first assignment still creates the variable; `y = compute` adds nothing
    assert_eq!(infos(&f, "y"), [(Local, "", "", 8), (Local, "Foo", "", 10)]);
    // a parameter re-assigned with evidence gets a typed binding from that line on
    let ps = binds(&f, "p");
    assert_eq!(infos(&f, "p"), [(Param, "", "", 13), (Local, "Foo", "", 14)]);
    assert_eq!(ps[0].scope, ps[1].scope);
}

#[test]
fn untyped_reassignment_and_block_assignment_to_outer_variable_add_nothing() {
    let f = run(r#"def m
  x = Foo.new
  x = compute
  x = 1
  x = [Bar.new]
  x, y = Bar.new, Baz.new
  x += 1
  items.each do |i|
    x = Bar.new
    x = make(1)
    x ||= Qux.new
    [1].map { x = Other.new }
  end
  for x in xs do end
  x = nil
end
"#);
    // the block assignments change the method's `x`, not a variable of the block
    assert_eq!(info(&f, "x"), (BindKind::Local, "Foo", ""));
    assert_eq!(bind(&f, "x").line, 2);
    assert_eq!(info(&f, "y"), (BindKind::Local, "", ""));
}

#[test]
fn blocks_close_over_outer_locals() {
    let f = run(r#"def m
  x = Foo.new
  items.each do |i|
    x = Bar.new
    y = Baz.new
    i = Qux.new
  end
  y = Other.new
end
"#);
    assert_eq!(info(&f, "x"), (BindKind::Local, "Foo", ""));
    // the block's own `i` is re-bound with a type inside the block
    let is = binds(&f, "i");
    assert_eq!(infos(&f, "i"), [(BindKind::Param, "", "", 3), (BindKind::Local, "Qux", "", 6)]);
    assert_eq!(is[0].scope, is[1].scope);
    let ys = binds(&f, "y");
    assert_eq!(ys.len(), 2, "block-local y and method-local y are distinct: {ys:?}");
    assert_eq!(ys[0].decl_type, "Baz");
    assert_eq!(f.scopes[ys[0].scope as usize].parent, ys[1].scope);
    assert_eq!(ys[1].decl_type, "Other");
}

#[test]
fn block_and_lambda_params_shadow_outer_variables() {
    use BindKind::*;
    let f = run(r#"def m
  i = Foo.new
  items.each { |i| i.bar }
  l = ->(i) { i }
  xs.each { |a; i| i }
  pairs.each { |(i, j)| i }
  items.each { i = Bar.new }
  ys.each { |a, a| a }
end
"#);
    // the method's `i`, then one fresh untyped variable per block / lambda that names it;
    // the plain assignment on line 7 is to the method's `i` and adds nothing
    let is = binds(&f, "i");
    assert_eq!(infos(&f, "i"), [(Local, "Foo", "", 2), (Param, "", "", 3), (Param, "", "", 4), (Local, "", "", 5), (Param, "", "", 6)]);
    assert_eq!(is[0].scope, 1);
    for b in &is[1..] {
        assert_ne!(b.scope, 1, "{b:?}");
        assert_eq!(f.scopes[b.scope as usize].parent, 1, "{b:?}");
    }
    assert!(is.iter().map(|b| b.scope).collect::<std::collections::HashSet<_>>().len() == is.len(), "one scope each: {is:?}");
    // a name repeated in one parameter list binds once
    assert_eq!(infos(&f, "a"), [(Param, "", "", 5), (Param, "", "", 8)]);
}

#[test]
fn block_params_shadow_file_and_class_locals() {
    let f = run(r#"i = Foo.new
items.each { |i| i.bar }
class C
  i = Bar.new
  items.each { |i| i.bar }
  def m(i)
    i.baz
  end
end
"#);
    let is = infos(&f, "i");
    assert_eq!(is.len(), 5, "{is:?}");
    assert_eq!(is[1], (BindKind::Param, "", "", 2));
    assert_eq!(is[3], (BindKind::Param, "", "", 5));
    assert_eq!(is[4], (BindKind::Param, "", "", 6));
}

#[test]
fn or_assign_and_operator_assign() {
    let f = run(r#"def m
  q ||= Q.new
  r ||= Svc.create(1)
  c &&= C.new
  n += 1
end
"#);
    assert_eq!(info(&f, "q"), (BindKind::Local, "Q", ""));
    assert_eq!(info(&f, "r"), (BindKind::Local, "", "Svc.create()"));
    assert_eq!(info(&f, "c"), (BindKind::Local, "C", ""));
    assert_eq!(info(&f, "n"), (BindKind::Local, "", ""));
}

#[test]
fn multiple_assignment_has_no_types() {
    let f = run(r#"def m
  a, b = Foo.new, Bar.new
  c, *d = list
  (e, f), g = pair
  h, = Foo.new
end
"#);
    for name in ["a", "b", "c", "d", "e", "f", "g", "h"] {
        assert_eq!(info(&f, name), (BindKind::Local, "", ""), "{name}");
    }
    assert_eq!(bind(&f, "g").line, 4);
}

#[test]
fn chained_assignment() {
    let f = run("x = y = Foo.new\n");
    assert_eq!(info(&f, "x"), (BindKind::Local, "Foo", ""));
    assert_eq!(info(&f, "y"), (BindKind::Local, "Foo", ""));
}

#[test]
fn rescue_binds_exception_variable() {
    let f = run(r#"def m
  begin
    work
  rescue Foo => e
  rescue Foo::Bar, Baz => e2
  rescue ::Top => e3
  rescue => e4
  rescue Other => e
  rescue *LIST => e5
  rescue Foo::Bar => e6
  rescue Foo, Bar => @e7
  end
end
def n
  work
rescue Err => err
end
"#);
    // a second `rescue` re-binds `e` with its own class from that line on
    assert_eq!(infos(&f, "e"), [(BindKind::Local, "Foo", "", 4), (BindKind::Local, "Other", "", 8)]);
    // several exception classes are ambiguous: the variable gets no type
    assert_eq!(info(&f, "e2"), (BindKind::Local, "", ""));
    assert_eq!(info(&f, "e3"), (BindKind::Local, "Top", ""));
    assert_eq!(info(&f, "e4"), (BindKind::Local, "", ""));
    assert_eq!(info(&f, "e5"), (BindKind::Local, "", ""), "a splat is not a class");
    assert_eq!(info(&f, "e6"), (BindKind::Local, "Foo::Bar", ""));
    no_bind(&f, "@e7");
    assert_eq!(info(&f, "err"), (BindKind::Local, "Err", ""));
    assert_eq!(scope_kind(&f, bind(&f, "err").scope), ScopeKind::Function);
}

#[test]
fn for_loop_binds_locals() {
    let f = run(r#"def m
  for r in xs
    r.go
  end
  for k, v in h do end
end
"#);
    assert_eq!(info(&f, "r"), (BindKind::Local, "", ""));
    assert_eq!(bind(&f, "r").line, 2);
    assert_eq!(info(&f, "k"), (BindKind::Local, "", ""));
    assert_eq!(info(&f, "v"), (BindKind::Local, "", ""));
    // `for` opens no scope of its own
    assert_eq!(bind(&f, "r").scope, 1);
}

// ------------------------------------------------------------------ fields

#[test]
fn instance_variables_are_class_fields() {
    let f = run(r#"class Service
  def initialize
    @repo = Repo.new
    @svc ||= Svc.create(1)
  end
  def reset
    @repo = Other.new
    @cache = nil
    items.each { @seen = Seen.new }
  end
  def self.boot
    @inst = Foo::Bar.new
  end
end
def top
  @orphan = Foo.new
end
"#);
    let class_scope = 1;
    assert_eq!(scope_kind(&f, class_scope), ScopeKind::Class);
    let repo = bind(&f, "@repo");
    assert_eq!((repo.kind, repo.decl_type.as_str(), repo.scope, repo.line), (BindKind::Field, "Repo", class_scope, 3));
    assert_eq!(info(&f, "@svc"), (BindKind::Field, "", "Svc.create()"));
    assert_eq!(info(&f, "@cache"), (BindKind::Field, "", ""));
    assert_eq!(info(&f, "@seen"), (BindKind::Field, "Seen", ""));
    assert_eq!(info(&f, "@inst"), (BindKind::Field, "Foo::Bar", ""));
    for name in ["@svc", "@cache", "@seen", "@inst"] {
        assert_eq!(bind(&f, name).scope, class_scope, "{name}");
    }
    no_bind(&f, "repo");
    no_bind(&f, "@orphan");
}

#[test]
fn one_instance_variable_binding_per_class() {
    let f = run(r#"class A
  def x; @v = nil; end
  def y; @v = Foo.new; end
end
class B
  def y; @v = Bar.new; end
end
"#);
    let vs = binds(&f, "@v");
    assert_eq!(vs.len(), 2, "{vs:?}");
    // the binding stays at the first assignment; the later one supplies the type it lacked
    assert_eq!((vs[0].decl_type.as_str(), vs[0].line), ("Foo", 2));
    assert_eq!((vs[1].decl_type.as_str(), vs[1].line), ("Bar", 6));
    assert_ne!(vs[0].scope, vs[1].scope);
}

#[test]
fn later_evidence_fills_an_untyped_field_once() {
    let f = run(r#"class Service
  attr_reader :cache, :v
  def initialize
    @v = nil
    @w = Repo.new
    @u = compute
    @cache = nil
    @@n = 0
  end
  def setup
    @v = Foo.new
    @w = Other.new
    @u = build(1)
    @v = Bar.new
    items.each { @cache = Cache.new }
    @@n = Counter.new
    @u = Third.new
  end
end
class Other
  def setup
    @w = nil
  end
end
"#);
    let v = bind(&f, "@v");
    assert_eq!(
        (v.kind, v.decl_type.as_str(), v.init_ctor.as_str(), v.line),
        (BindKind::Field, "Foo", "", 4),
        "first evidence fills the binding, later evidence does not replace it"
    );
    let u = bind(&f, "@u");
    assert_eq!((u.decl_type.as_str(), u.init_ctor.as_str(), u.line), ("", "build()", 6), "a call path is evidence too");
    let c = bind(&f, "@cache");
    assert_eq!((c.decl_type.as_str(), c.line), ("Cache", 7), "attr_reader adds no second binding");
    assert_eq!(info(&f, "@@n"), (BindKind::Field, "Counter", ""));
    // a typed binding keeps its type; the fill-in is per class, so `Other` has its own untyped `@w`
    let ws = binds(&f, "@w");
    assert_eq!(infos(&f, "@w"), [(BindKind::Field, "Repo", "", 5), (BindKind::Field, "", "", 22)]);
    assert_eq!((ws[0].scope, scope_kind(&f, ws[1].scope)), (1, ScopeKind::Class));
    assert_ne!(ws[0].scope, ws[1].scope);
    // ... in either order
    let f2 = run("class Other\n  def setup\n    @w = nil\n  end\nend\nclass Service\n  def setup\n    @w = Repo.new\n  end\nend\n");
    assert_eq!(infos(&f2, "@w"), [(BindKind::Field, "", "", 3), (BindKind::Field, "Repo", "", 8)]);
}

#[test]
fn instance_variables_in_modules_and_singleton_classes() {
    let f = run(r#"module Cache
  def store
    @store ||= Store.new
  end
  class << self
    def conn
      @conn = Conn.new
    end
  end
end
"#);
    let store = bind(&f, "@store");
    assert_eq!((store.decl_type.as_str(), store.scope), ("Store", 1));
    let conn = bind(&f, "@conn");
    assert_eq!(conn.decl_type, "Conn");
    assert_eq!(scope_kind(&f, conn.scope), ScopeKind::Class);
    assert_eq!(f.scopes[conn.scope as usize].parent, 1, "bound in the `class << self` scope");
}

#[test]
fn attr_accessors_declare_fields() {
    let f = run(r#"class C
  attr_accessor :repo, :name
  attr_reader :x
  attr_writer "w"
  private attr_reader :p
  attr_accessor(:q)
  def initialize
    @repo = Repo.new
  end
end
attr_reader :outside
"#);
    let repo = bind(&f, "@repo");
    assert_eq!((repo.kind, repo.decl_type.as_str(), repo.line), (BindKind::Field, "Repo", 8));
    for (name, line) in [("@name", 2), ("@x", 3), ("@w", 4), ("@p", 5), ("@q", 6)] {
        let b = bind(&f, name);
        assert_eq!((b.kind, b.decl_type.as_str(), b.init_ctor.as_str(), b.scope, b.line), (BindKind::Field, "", "", 1, line), "{name}");
    }
    no_bind(&f, "@outside");
    no_bind(&f, "name");
}

#[test]
fn attr_before_untyped_assignment_keeps_single_binding() {
    let f = run(r#"class C
  attr_reader :repo
  def initialize(repo)
    @repo = repo
  end
end
"#);
    let b = bind(&f, "@repo");
    assert_eq!((b.kind, b.decl_type.as_str(), b.line), (BindKind::Field, "", 4));
}

#[test]
fn class_variables_are_fields() {
    let f = run(r#"class C
  @@count = Counter.new
  def self.cache
    @@cache ||= Cache.new
    @@count = Other.new
  end
end
"#);
    let count = bind(&f, "@@count");
    assert_eq!((count.kind, count.decl_type.as_str(), count.scope, count.line), (BindKind::Field, "Counter", 1, 2));
    assert_eq!(info(&f, "@@cache"), (BindKind::Field, "Cache", ""));
    assert_eq!(bind(&f, "@@cache").scope, 1);
}

#[test]
fn constants_are_locals_of_their_scope() {
    let f = run(r#"FOO = Bar.new
module M
  LIMIT = Limit::Max.new(1)
  NAME = "n"
  DEFAULT = Factory.build
  class C
    INNER = ::Inner.new
  end
end
M::OTHER = 1
FOO = Baz.new
"#);
    // a constant re-assigned with evidence is re-bound like a local
    let foos = binds(&f, "FOO");
    assert_eq!(infos(&f, "FOO"), [(BindKind::Local, "Bar", "", 1), (BindKind::Local, "Baz", "", 11)]);
    assert_eq!((foos[0].scope, foos[1].scope), (0, 0));
    let limit = bind(&f, "LIMIT");
    assert_eq!((limit.kind, limit.decl_type.as_str(), limit.scope), (BindKind::Local, "Limit::Max", 1));
    assert_eq!(info(&f, "NAME"), (BindKind::Local, "", ""));
    assert_eq!(info(&f, "DEFAULT"), (BindKind::Local, "", "Factory.build()"));
    let inner = bind(&f, "INNER");
    assert_eq!((inner.decl_type.as_str(), inner.scope), ("Inner", 2));
    no_bind(&f, "OTHER");
    no_bind(&f, "M::OTHER");
}

#[test]
fn class_body_locals_stay_in_class_scope() {
    let f = run("class C\n  helper = Helper.new\n  def m\n    helper = Other.new\n  end\nend\n");
    let hs = binds(&f, "helper");
    assert_eq!(hs.len(), 2, "def is a hard scope boundary: {hs:?}");
    assert_eq!((hs[0].kind, hs[0].decl_type.as_str(), hs[0].scope), (BindKind::Local, "Helper", 1));
    assert_eq!((hs[1].decl_type.as_str(), scope_kind(&f, hs[1].scope)), ("Other", ScopeKind::Function));
}

// ------------------------------------------------------------------ robustness

#[test]
fn empty_and_comment_only_files() {
    for src in ["", "\n\n", "# just a comment\n", "=begin\ndoc\n=end\n", "__END__\nrequire 'x'\n"] {
        let f = run(src);
        assert!(f.imports.is_empty(), "{src:?}: {:?}", f.imports);
        assert!(f.bindings.is_empty(), "{src:?}: {:?}", f.bindings);
        assert_eq!(f.scopes.len(), 1, "{src:?}");
        assert_eq!(f.scopes[0].kind, ScopeKind::File);
    }
}

#[test]
fn syntax_errors_do_not_panic() {
    let broken = [
        "class",
        "class Foo < \n def",
        "def m(a, b = , *) \n x = Foo.new(\n",
        "require\nrequire_relative(\nload 'x",
        "x = = Foo.new\n@ = 1\n@@ = 2\n",
        "items.each do |a, (b, | \n @x = \nend end end",
        "a, b, = \nfor in xs\nrescue => \n",
        "->(x { x }\nlambda { |y \n",
        "class << \n attr_accessor :, 'x\n",
        "module M; class C; def m; @a = A.new; end",
        "\u{feff}require 'bom'\nx = \"unterminated\n",
        "def é(ü = Ü.new); ä = Ö.new; end",
    ];
    for src in broken {
        let f = run(src);
        assert!(!f.scopes.is_empty(), "{src:?}");
        assert_eq!(f.scopes[0].kind, ScopeKind::File);
        for s in &f.scopes[1..] {
            assert!((s.parent as usize) < f.scopes.len(), "{src:?}: {:?}", f.scopes);
            assert!(s.start <= s.end);
        }
        for b in &f.bindings {
            assert!((b.scope as usize) < f.scopes.len() && !b.name.is_empty(), "{src:?}: {b:?}");
        }
    }
}

#[test]
fn partial_file_still_yields_facts() {
    let f = run("require 'ok'\nclass Svc\n  def initialize\n    @repo = Repo.new\n  end\nend\nfoo(1, \n");
    assert_eq!(specs(&f), ["ok"]);
    assert_eq!(info(&f, "@repo"), (BindKind::Field, "Repo", ""));
    // an error inside the class swallows the `class` node; what survives is still reported
    let f = run("require 'ok'\nclass Svc\n  def initialize(a)\n    @repo = Repo.new\n  end\n  def broken(\nend\n");
    assert_eq!(specs(&f), ["ok"]);
    assert_eq!(info(&f, "a"), (BindKind::Param, "", ""));
}

#[test]
fn deeply_nested_blocks() {
    // each block level is three tree levels (call, do_block, body); 100 stay within the
    // walker's recursion bound
    let depth = 100;
    let mut src = String::new();
    for i in 0..depth {
        src.push_str(&format!("a.each do |v{i}|\n"));
    }
    src.push_str("x = Foo.new\n");
    for _ in 0..depth {
        src.push_str("end\n");
    }
    let f = run(&src);
    assert_eq!(f.scopes.len(), depth + 1);
    assert_eq!(info(&f, "x"), (BindKind::Local, "Foo", ""));
    assert_eq!(bind(&f, "x").scope, depth as u32);
}

#[test]
fn extreme_expression_nesting_is_skipped_not_fatal() {
    // 5000 nested parentheses: the walker and the value unwrapping both stop at their bounds
    let mut src = String::from("require 'before'\ndef m\n  x = ");
    src.push_str(&"(".repeat(5000));
    src.push('1');
    src.push_str(&")".repeat(5000));
    src.push_str("\n  y = Foo.new\nend\ndef after(p)\n  z = Bar.new\nend\n");
    let f = run_small_stack(src);
    assert_eq!(specs(&f), ["before"]);
    assert_eq!(info(&f, "x"), (BindKind::Local, "", ""));
    assert_eq!(bind(&f, "x").line, 3);
    assert_eq!(info(&f, "y"), (BindKind::Local, "Foo", ""));
    assert_eq!(info(&f, "p"), (BindKind::Param, "", ""));
    assert_eq!(info(&f, "z"), (BindKind::Local, "Bar", ""));
    // a constant path too long to be evidence is dropped, not recursed into
    let mut src = String::from("def m\n  x = ");
    src.push_str(&"A::".repeat(3000));
    src.push_str("B.new\n  y = Foo.new\nend\n");
    let f = run_small_stack(src);
    assert_eq!(info(&f, "x"), (BindKind::Local, "", ""));
    assert_eq!(info(&f, "y"), (BindKind::Local, "Foo", ""));
}

#[test]
fn extreme_block_nesting_is_skipped_not_fatal() {
    let depth = 4000;
    let mut src = String::from("require 'before'\n");
    for i in 0..depth {
        src.push_str(&format!("a.each do |v{i}|\n"));
    }
    src.push_str("x = Foo.new\n");
    for _ in 0..depth {
        src.push_str("end\n");
    }
    src.push_str("def after(q)\n  z = Bar.new\nend\n");
    let f = run_small_stack(src);
    assert_eq!(specs(&f), ["before"]);
    assert_eq!(info(&f, "v0"), (BindKind::Param, "", ""));
    assert_eq!(bind(&f, "v0").scope, 1);
    // the deepest blocks are skipped, not crashed on
    no_bind(&f, &format!("v{}", depth - 1));
    no_bind(&f, "x");
    assert!(f.scopes.len() < depth, "{} scopes", f.scopes.len());
    // what follows the deep region is still reported
    assert_eq!(info(&f, "q"), (BindKind::Param, "", ""));
    assert_eq!(info(&f, "z"), (BindKind::Local, "Bar", ""));
    assert_eq!(f.scopes[bind(&f, "q").scope as usize].parent, 0);
}
