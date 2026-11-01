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
fn only_first_assignment_binds() {
    let f = run(r#"def a
  x = Foo.new
  x = Bar.new
  x ||= Baz.new
end
def b(p)
  x = Bar.new
  p = Foo.new
end
"#);
    let xs = binds(&f, "x");
    assert_eq!(xs.len(), 2, "one per method: {xs:?}");
    assert_eq!((xs[0].decl_type.as_str(), xs[0].line), ("Foo", 2));
    assert_eq!((xs[1].decl_type.as_str(), xs[1].line), ("Bar", 7));
    assert_ne!(xs[0].scope, xs[1].scope);
    // reassigning a parameter does not create a second variable
    assert_eq!(info(&f, "p"), (BindKind::Param, "", ""));
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
    assert_eq!(info(&f, "i"), (BindKind::Param, "", ""));
    let ys = binds(&f, "y");
    assert_eq!(ys.len(), 2, "block-local y and method-local y are distinct: {ys:?}");
    assert_eq!(ys[0].decl_type, "Baz");
    assert_eq!(f.scopes[ys[0].scope as usize].parent, ys[1].scope);
    assert_eq!(ys[1].decl_type, "Other");
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
  end
end
def n
  work
rescue Err => err
end
"#);
    assert_eq!(info(&f, "e"), (BindKind::Local, "Foo", ""));
    assert_eq!(bind(&f, "e").line, 4);
    assert_eq!(info(&f, "e2"), (BindKind::Local, "Foo::Bar", ""));
    assert_eq!(info(&f, "e3"), (BindKind::Local, "Top", ""));
    assert_eq!(info(&f, "e4"), (BindKind::Local, "", ""));
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
fn first_instance_variable_assignment_wins_per_class() {
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
    assert_eq!((vs[0].decl_type.as_str(), vs[0].line), ("", 2));
    assert_eq!((vs[1].decl_type.as_str(), vs[1].line), ("Bar", 6));
    assert_ne!(vs[0].scope, vs[1].scope);
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
    let foo = bind(&f, "FOO");
    assert_eq!((foo.kind, foo.decl_type.as_str(), foo.scope, foo.line), (BindKind::Local, "Bar", 0, 1));
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
    let depth = 200;
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
