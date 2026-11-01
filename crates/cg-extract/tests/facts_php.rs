//! PHP facts: namespace package, `use` imports, scopes and typed bindings.

use cg_core::Lang;
use cg_extract::*;

fn run(src: &str) -> Facts {
    let b = extract_bytes("a.php", src.as_bytes(), 0, Lang::Php, &ExtractConfig::new("."));
    assert_eq!(b.flags & fileflags::PANICKED, 0, "{:?}", b.errors);
    b.facts
}

fn imp<'a>(f: &'a Facts, local: &str) -> &'a ImportFact {
    f.imports.iter().find(|i| i.local == local).unwrap_or_else(|| panic!("no import {local}: {:?}", f.imports))
}

fn binds<'a>(f: &'a Facts, name: &str) -> Vec<&'a BindingFact> {
    f.bindings.iter().filter(|b| b.name == name).collect()
}

fn bind<'a>(f: &'a Facts, name: &str) -> &'a BindingFact {
    f.bindings.iter().find(|b| b.name == name).unwrap_or_else(|| panic!("no binding {name}: {:?}", f.bindings))
}

fn bind_kind<'a>(f: &'a Facts, name: &str, kind: BindKind) -> &'a BindingFact {
    f.bindings.iter().find(|b| b.name == name && b.kind == kind).unwrap_or_else(|| panic!("no {kind:?} binding {name}: {:?}", f.bindings))
}

fn package(f: &Facts) -> Vec<&str> {
    f.bindings.iter().filter(|b| b.kind == BindKind::Package).map(|b| b.name.as_str()).collect()
}

fn kind_of(f: &Facts, scope: u32) -> ScopeKind {
    f.scopes[scope as usize].kind
}

#[test]
fn namespace_statement_form() {
    let f = run("<?php\nnamespace App\\Http;\n\nclass A {}\n");
    assert_eq!(package(&f), ["App\\Http"]);
    let p = bind_kind(&f, "App\\Http", BindKind::Package);
    assert_eq!((p.scope, p.line), (0, 2));
}

#[test]
fn namespace_braced_form_first_only() {
    let f = run("<?php\nnamespace App\\Braced {\n    use App\\X;\n    function f() {}\n}\nnamespace Other {\n}\n");
    assert_eq!(package(&f), ["App\\Braced"]);
    // imports inside the braces are still seen
    assert_eq!(imp(&f, "X").spec, "App\\X");
}

#[test]
fn use_plain_and_alias() {
    let f = run("<?php\nnamespace App;\nuse App\\Models\\User;\nuse App\\Models\\User as U;\nuse \\Lead\\Slash;\nuse A\\B, C\\D as E;\n");
    let u = imp(&f, "User");
    assert_eq!((u.imported.as_str(), u.spec.as_str(), u.kind), ("User", "App\\Models\\User", ImportKind::Named));
    assert_eq!((u.scope, u.line, u.type_only), (0, 3, false));
    let a = imp(&f, "U");
    assert_eq!((a.imported.as_str(), a.spec.as_str(), a.kind), ("User", "App\\Models\\User", ImportKind::Named));
    // leading backslash stripped
    assert_eq!(imp(&f, "Slash").spec, "Lead\\Slash");
    // several clauses in one statement
    assert_eq!(imp(&f, "B").spec, "A\\B");
    assert_eq!((imp(&f, "E").spec.as_str(), imp(&f, "E").imported.as_str()), ("C\\D", "D"));
    assert_eq!(f.imports.len(), 5, "{:?}", f.imports);
}

#[test]
fn use_group() {
    let f = run("<?php\nuse App\\Models\\{User, Post as P};\nuse Lib\\{Sub\\Thing};\n");
    let u = imp(&f, "User");
    assert_eq!((u.imported.as_str(), u.spec.as_str(), u.kind), ("User", "App\\Models\\User", ImportKind::Named));
    let p = imp(&f, "P");
    assert_eq!((p.imported.as_str(), p.spec.as_str(), p.kind), ("Post", "App\\Models\\Post", ImportKind::Named));
    assert_eq!(imp(&f, "Thing").spec, "Lib\\Sub\\Thing");
    assert_eq!(f.imports.len(), 3, "{:?}", f.imports);
}

#[test]
fn use_group_with_leading_backslash_binds_nothing_wrong() {
    // tree-sitter-php 0.24.2 cannot parse a leading `\` before a group: whatever it
    // recovers must not import the bare prefix
    let f = run("<?php\nuse App\\Models\\{User};\nuse \\Lib\\{Sub\\Thing};\nuse App\\After;\n");
    assert!(f.imports.iter().all(|i| i.spec != "Lib" && i.local != "Lib"), "{:?}", f.imports);
    assert_eq!(imp(&f, "User").spec, "App\\Models\\User");
    if let Some(t) = f.imports.iter().find(|i| i.local == "Thing") {
        assert_eq!(t.spec, "Lib\\Sub\\Thing");
    }
}

#[test]
fn use_function_and_const() {
    let f =
        run("<?php\nuse function App\\helper;\nuse function App\\other as o;\nuse const App\\FOO;\nuse App\\{function grouped, const BAR, Models\\Thing};\n");
    let h = imp(&f, "helper");
    assert_eq!((h.imported.as_str(), h.spec.as_str(), h.kind), ("helper", "App\\helper", ImportKind::Named));
    assert_eq!((imp(&f, "o").imported.as_str(), imp(&f, "o").spec.as_str()), ("other", "App\\other"));
    assert_eq!(imp(&f, "grouped").spec, "App\\grouped");
    assert_eq!(imp(&f, "Thing").spec, "App\\Models\\Thing");
    assert!(f.imports.iter().all(|i| i.local != "FOO" && i.local != "BAR"), "{:?}", f.imports);
    assert_eq!(f.imports.len(), 4, "{:?}", f.imports);
}

#[test]
fn trait_use_is_not_an_import() {
    let f = run("<?php\nuse App\\Real;\nclass A {\n    use SomeTrait;\n    use Other\\Trait2, Third { foo as bar; }\n}\n");
    assert_eq!(f.imports.len(), 1, "{:?}", f.imports);
    assert_eq!(imp(&f, "Real").spec, "App\\Real");
}

#[test]
fn scopes() {
    let f = run(r#"<?php
class C { public function m() { $f = function () use ($x) { return 1; }; $g = fn($y) => $y; } }
interface I { public function i(); }
trait T { function t() {} }
enum E: string { case A = 'a'; public function e() {} }
function top() {}
"#);
    let kinds: Vec<ScopeKind> = f.scopes.iter().map(|s| s.kind).collect();
    assert_eq!(kinds.iter().filter(|k| **k == ScopeKind::Class).count(), 4, "{kinds:?}");
    // m, closure, arrow fn, i, t, e, top
    assert_eq!(kinds.iter().filter(|k| **k == ScopeKind::Function).count(), 7, "{kinds:?}");
    assert_eq!(kinds[0], ScopeKind::File);
    // closure and arrow fn nest in the method, which nests in the class
    let closure_param = bind(&f, "y");
    let arrow = &f.scopes[closure_param.scope as usize];
    assert_eq!(arrow.kind, ScopeKind::Function);
    let method = arrow.parent;
    assert_eq!(kind_of(&f, method), ScopeKind::Function);
    assert_eq!(kind_of(&f, f.scopes[method as usize].parent), ScopeKind::Class);
    // the class scope spans the declaration
    let class = &f.scopes[f.scopes[method as usize].parent as usize];
    assert_eq!((class.start >> 32, class.end >> 32), (2, 2));
}

#[test]
fn param_types() {
    let f = run(r#"<?php
function f(Foo $a, ?Foo $b, \App\Foo $c, int $d, A|B $e, A|null $g, null|A $h, Foo&Bar $i, ?int $j, self $k, static|null $l, $m, Foo ...$rest, &$ref) {}
"#);
    let ty = |n: &str| {
        let b = bind(&f, n);
        assert_eq!(b.kind, BindKind::Param, "{n}");
        b.decl_type.clone()
    };
    assert_eq!(ty("a"), "Foo");
    assert_eq!(ty("b"), "Foo");
    assert_eq!(ty("c"), "App\\Foo");
    assert_eq!(ty("d"), "");
    assert_eq!(ty("e"), "");
    assert_eq!(ty("g"), "A");
    assert_eq!(ty("h"), "A");
    assert_eq!(ty("i"), "");
    assert_eq!(ty("j"), "");
    assert_eq!(ty("k"), "self");
    assert_eq!(ty("l"), "self");
    assert_eq!(ty("m"), "");
    assert_eq!(ty("rest"), "");
    assert_eq!(ty("ref"), "");
    assert_eq!(bind(&f, "a").line, 2);
    assert_eq!(kind_of(&f, bind(&f, "a").scope), ScopeKind::Function);
}

#[test]
fn scalar_types_are_empty() {
    let f =
        run("<?php\nfunction f(int $a, string $b, array $c, mixed $d, callable $e, iterable $g, bool $h, float $i, object $j, ?string $k, false|null $l) {}\n");
    for n in ["a", "b", "c", "d", "e", "g", "h", "i", "j", "k", "l"] {
        assert_eq!(bind(&f, n).decl_type, "", "{n}");
    }
}

#[test]
fn promoted_constructor_params() {
    let f = run(r#"<?php
class S {
    public function __construct(private Repo $repo, protected readonly ?\App\Log $log, int $n) {}
}
"#);
    let p = bind_kind(&f, "repo", BindKind::Param);
    assert_eq!(p.decl_type, "Repo");
    assert_eq!(kind_of(&f, p.scope), ScopeKind::Function);
    let fl = bind_kind(&f, "repo", BindKind::Field);
    assert_eq!((fl.decl_type.as_str(), fl.line), ("Repo", 3));
    assert_eq!(kind_of(&f, fl.scope), ScopeKind::Class);
    assert_eq!(bind_kind(&f, "log", BindKind::Field).decl_type, "App\\Log");
    assert_eq!(bind_kind(&f, "log", BindKind::Param).decl_type, "App\\Log");
    // ordinary parameters are not fields
    assert!(binds(&f, "n").iter().all(|b| b.kind == BindKind::Param), "{:?}", binds(&f, "n"));
}

#[test]
fn properties() {
    let f = run(r#"<?php
class P {
    private Repo $repo;
    private ?Repo $r2 = null;
    public $untyped;
    public static ?\App\Foo $sf;
    public int|string $u;
    var $old, $older;
    const X = 1;
}
"#);
    let field = |n: &str| {
        let b = bind(&f, n);
        assert_eq!(b.kind, BindKind::Field, "{n}");
        assert_eq!(kind_of(&f, b.scope), ScopeKind::Class, "{n}");
        (b.decl_type.clone(), b.line)
    };
    assert_eq!(field("repo"), ("Repo".to_owned(), 3));
    assert_eq!(field("r2"), ("Repo".to_owned(), 4));
    assert_eq!(field("untyped"), (String::new(), 5));
    assert_eq!(field("sf"), ("App\\Foo".to_owned(), 6));
    assert_eq!(field("u").0, "");
    assert_eq!(field("old").0, "");
    assert_eq!(field("older").0, "");
    assert!(binds(&f, "X").is_empty());
}

#[test]
fn docblock_types() {
    let f = run(r#"<?php
class D {
    /** @var Logger */
    private $log;
    /**
     * @var \App\Cache|null
     */
    private $cache;
    /** @var Foo[] */
    private $many;
    /** @var Native */
    private Repo $native;
    /** @var int */
    private $count;

    /**
     * @param Repo $repo the repository
     * @param string $name
     * @param ?Mailer $mailer
     */
    public function __construct($repo, $name, $mailer, private $promo) {}

    /** @param Item $other */
    public function m($item) {
        /** @var User $u */
        $u = $this->repo->find(1);
    }
}
"#);
    assert_eq!(bind_kind(&f, "log", BindKind::Field).decl_type, "Logger");
    assert_eq!(bind_kind(&f, "cache", BindKind::Field).decl_type, "App\\Cache");
    assert_eq!(bind_kind(&f, "many", BindKind::Field).decl_type, "");
    assert_eq!(bind_kind(&f, "native", BindKind::Field).decl_type, "Repo");
    assert_eq!(bind_kind(&f, "count", BindKind::Field).decl_type, "");
    assert_eq!(bind_kind(&f, "repo", BindKind::Param).decl_type, "Repo");
    assert_eq!(bind_kind(&f, "name", BindKind::Param).decl_type, "");
    assert_eq!(bind_kind(&f, "mailer", BindKind::Param).decl_type, "Mailer");
    assert_eq!(bind_kind(&f, "promo", BindKind::Field).decl_type, "");
    // `@param` must name the parameter
    assert_eq!(bind_kind(&f, "item", BindKind::Param).decl_type, "");
    assert_eq!(bind_kind(&f, "u", BindKind::Local).decl_type, "User");
}

#[test]
fn locals_from_initialisers() {
    let f = run(r#"<?php
function m($other) {
    $a = new Foo(1);
    $b = Foo::create();
    $c = make();
    $d = $other->build();
    $e = $this->repo->find();
    $g = new \App\Foo;
    $h = \App\Factory::make();
    $i = \App\helper();
    $j = self::instance();
    $k = static::instance();
    $l = parent::instance();
    $n = new static();
    $o = ($p = new Bar());
    $q = $a;
    $r = $this->make();
    $s = 42;
    $a = new Other();
}
"#);
    let local = |n: &str| {
        let b = bind(&f, n);
        assert_eq!(b.kind, BindKind::Local, "{n}");
        assert_eq!(kind_of(&f, b.scope), ScopeKind::Function, "{n}");
        (b.decl_type.as_str(), b.init_ctor.as_str())
    };
    assert_eq!(local("a"), ("Foo", ""));
    assert_eq!(bind(&f, "a").line, 3);
    assert_eq!(local("b"), ("", "Foo::create()"));
    assert_eq!(local("c"), ("", "make()"));
    assert_eq!(local("d"), ("", "other.build()"));
    assert_eq!(local("e"), ("", ""));
    assert_eq!(local("g"), ("App\\Foo", ""));
    assert_eq!(local("h"), ("", "App\\Factory::make()"));
    assert_eq!(local("i"), ("", "App\\helper()"));
    assert_eq!(local("j"), ("", "self::instance()"));
    assert_eq!(local("k"), ("", "self::instance()"));
    assert_eq!(local("l"), ("", ""));
    assert_eq!(local("n"), ("self", ""));
    assert_eq!(local("o"), ("Bar", ""));
    assert_eq!(local("p"), ("Bar", ""));
    assert_eq!(local("q"), ("Foo", ""));
    assert_eq!(local("r"), ("", "this.make()"));
    assert_eq!(local("s"), ("", ""));
    // re-assignment in the same scope is ignored
    assert_eq!(binds(&f, "a").len(), 1, "{:?}", binds(&f, "a"));
    // `$this` is never a binding, re-assigning a parameter adds nothing
    assert!(binds(&f, "this").is_empty());
    assert_eq!(binds(&f, "other").len(), 1);
}

#[test]
fn locals_are_per_scope() {
    let f = run("<?php\n$app = new App();\nfunction f() {\n    $app = new Other();\n}\n");
    let apps = binds(&f, "app");
    assert_eq!(apps.len(), 2, "{apps:?}");
    assert_eq!((apps[0].scope, apps[0].decl_type.as_str()), (0, "App"));
    assert_eq!((kind_of(&f, apps[1].scope), apps[1].decl_type.as_str()), (ScopeKind::Function, "Other"));
}

#[test]
fn foreach_catch_list_global_static() {
    let f = run(r#"<?php
function m($xs) {
    foreach ($xs as $k => $v) {}
    foreach ($xs as $only) {}
    foreach ($xs as [$r1, $r2]) {}
    foreach ($xs as &$ref) {}
    try {} catch (FooException $e) {} catch (A | B $e2) {} catch (\App\Err) {}
    list($l1, $l2) = f();
    [$l3, $l4] = f();
    ['id' => $id] = $row;
    global $g, $h;
    static $st = 1, $inst = null;
}
"#);
    for n in ["k", "v", "only", "r1", "r2", "ref", "l1", "l2", "l3", "l4", "id", "g", "h", "st", "inst"] {
        let b = bind(&f, n);
        assert_eq!((b.kind, b.decl_type.as_str()), (BindKind::Local, ""), "{n}");
        assert_eq!(kind_of(&f, b.scope), ScopeKind::Function, "{n}");
    }
    // the foreach subject is not bound
    assert_eq!(binds(&f, "xs").len(), 1);
    assert_eq!(bind(&f, "v").line, 3);
    let e = bind(&f, "e");
    assert_eq!((e.kind, e.decl_type.as_str(), e.line), (BindKind::Local, "FooException", 7));
    assert_eq!(bind(&f, "e2").decl_type, "");
}

#[test]
fn closure_use_vars() {
    let f = run(r#"<?php
function m() {
    $repo = new Repo();
    $count = 0;
    $cb = function (Item $item) use ($repo, &$count, $missing) {
        $local = new Bar();
        return $repo->save($item);
    };
    $arrow = fn(Item $i) => $repo->save($i);
}
"#);
    let outer = bind_kind(&f, "repo", BindKind::Local);
    let captured = bind_kind(&f, "repo", BindKind::Param);
    assert_eq!(kind_of(&f, captured.scope), ScopeKind::Function);
    assert_eq!(f.scopes[captured.scope as usize].parent, outer.scope);
    assert_eq!((captured.decl_type.as_str(), captured.line), ("Repo", 5));
    let count = bind_kind(&f, "count", BindKind::Param);
    assert_eq!(count.scope, captured.scope);
    assert_eq!(bind_kind(&f, "missing", BindKind::Param).scope, captured.scope);
    assert_eq!(bind_kind(&f, "item", BindKind::Param).decl_type, "Item");
    let local = bind_kind(&f, "local", BindKind::Local);
    assert_eq!((local.scope, local.decl_type.as_str()), (captured.scope, "Bar"));
    let i = bind_kind(&f, "i", BindKind::Param);
    assert_eq!(i.decl_type, "Item");
    assert_ne!(i.scope, captured.scope);
    assert_eq!(f.scopes[i.scope as usize].parent, outer.scope);
}

#[test]
fn this_assignments_declare_fields() {
    let f = run(r#"<?php
class S {
    private Typed $typed;
    public $loose;
    public function __construct(Repo $repo) {
        $this->svc = new \App\Svc();
        $this->typed = new Other();
        $this->loose = new Loose();
        $this->repo = $repo;
        $this->made = Factory::make();
        $this->count = 0;
        $this->cache ??= new Cache();
        $this->svc = new Replaced();
        $fn = function () { $this->inner = new Inner(); };
    }
}
function free() { $this->nope = new X(); }
"#);
    let field = |n: &str| {
        let all: Vec<_> = binds(&f, n).into_iter().filter(|b| b.kind == BindKind::Field).collect();
        assert_eq!(all.len(), 1, "{n}: {all:?}");
        assert_eq!(kind_of(&f, all[0].scope), ScopeKind::Class, "{n}");
        (all[0].decl_type.as_str(), all[0].init_ctor.as_str())
    };
    assert_eq!(field("svc"), ("App\\Svc", ""));
    assert_eq!(bind_kind(&f, "svc", BindKind::Field).line, 6);
    // a typed property keeps its declared type
    assert_eq!(field("typed"), ("Typed", ""));
    // an untyped property learns the assigned type
    assert_eq!(field("loose"), ("Loose", ""));
    assert_eq!(bind_kind(&f, "loose", BindKind::Field).line, 4);
    // copied from the typed parameter
    assert_eq!(field("repo"), ("Repo", ""));
    assert_eq!(field("made"), ("", "Factory::make()"));
    assert_eq!(field("count"), ("", ""));
    assert_eq!(field("cache"), ("Cache", ""));
    // `$this` in a closure inside a method still refers to the class
    assert_eq!(field("inner"), ("Inner", ""));
    // outside any class nothing is declared
    assert!(binds(&f, "nope").is_empty());
}

#[test]
fn nested_functions_are_local_fns() {
    let f = run("<?php\nfunction outer() {\n    function inner() {}\n}\nfunction top() {}\nclass C { function m() {} }\n");
    let inner = bind(&f, "inner");
    assert_eq!((inner.kind, inner.line), (BindKind::LocalFn, 3));
    assert_eq!(kind_of(&f, inner.scope), ScopeKind::Function);
    assert!(binds(&f, "top").is_empty());
    assert!(binds(&f, "m").is_empty());
    assert!(binds(&f, "outer").is_empty());
}

#[test]
fn anonymous_class_methods_have_their_own_class() {
    let f = run(r#"<?php
class Outer {
    public function m($dep) {
        return new class($dep) extends Base {
            public function run() { $this->q = new Q(); }
        };
    }
}
"#);
    let q = bind_kind(&f, "q", BindKind::Field);
    assert_eq!(kind_of(&f, q.scope), ScopeKind::Class);
    // the anonymous class sits inside the method, not directly in `Outer`
    let parent = f.scopes[q.scope as usize].parent;
    assert_eq!(kind_of(&f, parent), ScopeKind::Function);
}

#[test]
fn leading_html() {
    let f = run(
        "<html>\n<body>\n<?php\nnamespace App;\nuse App\\Models\\User;\n$u = new User();\n?>\n<p><?= $u->name ?></p>\n<?php function f(Repo $r) {} ?>\n</body>\n",
    );
    assert_eq!(package(&f), ["App"]);
    assert_eq!((imp(&f, "User").spec.as_str(), imp(&f, "User").line), ("App\\Models\\User", 5));
    let u = bind(&f, "u");
    assert_eq!((u.kind, u.decl_type.as_str(), u.line, u.scope), (BindKind::Local, "User", 6, 0));
    assert_eq!(bind(&f, "r").decl_type, "Repo");
}

#[test]
fn syntax_errors_do_not_panic() {
    let f = run(r#"<?php
namespace App;
use App\Models\User;
class Broken {
    private Repo $repo
    public function m(Foo $x {
        $a = new Foo(;
        $this-> = 3;
        foreach ($xs as) {}
        catch (
"#);
    assert_eq!(package(&f), ["App"]);
    assert_eq!(imp(&f, "User").spec, "App\\Models\\User");
    for src in [
        "<?php use ;",
        "<?php use App\\{;",
        "<?php namespace",
        "<?php function (",
        "<?php $ = new;",
        "<?php class { use",
        "<?php [$a, = 1;",
        "<?php fn(",
        "<?php /** @var */ $x = 1;",
    ] {
        run(src);
    }
}

#[test]
fn empty_and_trivial_files() {
    for src in ["", "<?php", "<?php\n", "just text, no php", "<?php ?>", "<?= 1 ?>"] {
        let f = run(src);
        assert!(f.imports.is_empty() && f.bindings.is_empty(), "{src:?}: {f:?}");
        assert_eq!(f.scopes.len(), 1, "{src:?}");
    }
}
