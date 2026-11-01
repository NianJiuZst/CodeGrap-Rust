//! Facts for PHP: the first namespace (as the file's package), `use` imports (plain, aliased,
//! grouped, `use function`), class / function / closure / arrow-function scopes, parameters
//! (including constructor property promotion), properties, `$this->x = ...` fields and locals
//! with declared types.
//!
//! Variable names are stored without their `$` (the kernel emits `$repo->find()` as
//! `repo.find`) and `$this` is never a binding. Type names keep their namespace backslashes
//! but lose a leading one; scalar and pseudo types become empty except `self` / `static`,
//! which both become `self` for the resolver to map onto the enclosing class.

use super::util::*;
use super::*;
use rustc_hash::{FxHashMap, FxHashSet};

pub fn extract(cx: &mut Cx, root: Node) {
    let mut st = St::default();
    walk(cx, &mut st, root, 0);
}

#[derive(Default)]
struct St {
    /// `(scope, name)` of every variable bound so far: later plain assignments don't rebind.
    bound: FxHashSet<(u32, String)>,
    /// `(class scope, name)` -> index of the first field binding of that name.
    fields: FxHashMap<(u32, String), usize>,
    package: bool,
}

fn walk_children(cx: &mut Cx, st: &mut St, n: Node, scope: u32) {
    for ch in kids(n) {
        walk(cx, st, ch, scope);
    }
}

fn walk(cx: &mut Cx, st: &mut St, n: Node, scope: u32) {
    match n.kind() {
        "namespace_definition" => {
            namespace(cx, st, n);
            if let Some(body) = n.child_by_field_name("body") {
                walk(cx, st, body, scope);
            }
        }
        "namespace_use_declaration" => use_declaration(cx, n, scope),
        // trait `use Foo;` inside a class body: not an import
        "use_declaration" => {}
        "class_declaration" | "interface_declaration" | "trait_declaration" | "enum_declaration" | "anonymous_class" => class(cx, st, n, scope),
        "function_definition" | "method_declaration" => function(cx, st, n, scope),
        "anonymous_function" | "arrow_function" => closure(cx, st, n, scope),
        "assignment_expression" | "reference_assignment_expression" => {
            assignment(cx, st, n, scope);
            walk_children(cx, st, n, scope);
        }
        "augmented_assignment_expression" => {
            // `$this->cache ??= new Cache()` / `$x ??= make()` initialise lazily
            if n.child_by_field_name("operator").is_some_and(|o| cx.text(o) == "??=") {
                assignment(cx, st, n, scope);
            }
            walk_children(cx, st, n, scope);
        }
        "foreach_statement" => {
            let body = n.child_by_field_name("body").map(|b| b.id());
            // `foreach (<subject> as <target>)`: everything named after the subject binds
            for t in kids(n).filter(|c| c.is_named() && Some(c.id()) != body && c.kind() != "comment").skip(1) {
                bind_targets(cx, st, t, scope);
            }
            walk_children(cx, st, n, scope);
        }
        "catch_clause" => {
            let ty = n.child_by_field_name("type").map(|tl| single_type(cx, tl)).unwrap_or_default();
            if let Some(v) = n.child_by_field_name("name") {
                // every catch rebinds: `catch (A $e) {} catch (B $e) {}` are distinct types
                let name = var_name(cx, v);
                let line = cx.line(v);
                bind(cx, st, scope, &name, BindKind::Local, &ty, "", line);
            }
            walk_children(cx, st, n, scope);
        }
        "global_declaration" => {
            for v in kids(n).filter(|c| c.kind() == "variable_name") {
                local(cx, st, scope, v, "", "");
            }
        }
        "static_variable_declaration" => {
            if let Some(v) = n.child_by_field_name("name") {
                let (ty, init) = n.child_by_field_name("value").map(|r| value_info(cx, r, scope)).unwrap_or_default();
                local(cx, st, scope, v, &ty, &init);
            }
            walk_children(cx, st, n, scope);
        }
        _ => walk_children(cx, st, n, scope),
    }
}

// ---------------------------------------------------------------------------- names and types

/// `$repo` -> `repo` (`variable_name` or `by_ref` around one).
fn var_name(cx: &Cx, v: Node) -> String {
    let v = if v.kind() == "by_ref" { kids(v).find(|c| c.kind() == "variable_name").unwrap_or(v) } else { v };
    if v.kind() != "variable_name" {
        return String::new();
    }
    cx.text(v).trim_start_matches('$').trim().to_owned()
}

/// A written class name reduced to what resolution looks up: no whitespace, no leading `\`,
/// no `namespace\` prefix; scalar and pseudo types are empty, `self` / `static` are `self`.
fn class_name(raw: &str) -> String {
    let s: String = raw.split_whitespace().collect();
    let s = s.trim_start_matches('\\');
    let s = s.strip_prefix("namespace\\").unwrap_or(s);
    if s.is_empty() || !s.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '\\') {
        return String::new();
    }
    match s.to_ascii_lowercase().as_str() {
        "self" | "static" => "self".to_owned(),
        "int" | "integer" | "string" | "array" | "mixed" | "void" | "callable" | "iterable" | "bool" | "boolean" | "float" | "double" | "object" | "null"
        | "never" | "false" | "true" | "parent" | "resource" | "list" => String::new(),
        _ => s.to_owned(),
    }
}

fn is_null_type(cx: &Cx, t: Node) -> bool {
    cx.text(t).trim().eq_ignore_ascii_case("null")
}

/// Declared type of a `type` node: a class name, `?Foo` / `Foo|null` -> `Foo`, anything else
/// (scalars, other unions, intersections, DNF types) -> "".
fn type_name(cx: &Cx, t: Node) -> String {
    match t.kind() {
        "named_type" | "primitive_type" | "name" | "qualified_name" | "relative_name" => class_name(cx.text(t)),
        "optional_type" => t.named_child(0).map(|c| type_name(cx, c)).unwrap_or_default(),
        "union_type" => {
            let parts: Vec<Node> = kids(t).filter(|c| c.is_named() && c.kind() != "comment").collect();
            let rest: Vec<Node> = parts.iter().copied().filter(|c| !is_null_type(cx, *c)).collect();
            if parts.len() == 2 && rest.len() == 1 {
                type_name(cx, rest[0])
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}

/// `catch (FooException $e)` -> `FooException`; several alternatives -> "".
fn single_type(cx: &Cx, list: Node) -> String {
    let ts: Vec<Node> = kids(list).filter(|c| c.is_named() && c.kind() != "comment").collect();
    match ts.as_slice() {
        [t] => type_name(cx, *t),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------- docblocks

/// The `/** ... */` docblock directly above `n`, if any.
fn doc_comment<'a>(cx: &Cx<'a>, n: Node) -> Option<&'a str> {
    let p = n.prev_named_sibling()?;
    if p.kind() != "comment" || p.end_position().row + 1 < n.start_position().row {
        return None;
    }
    let t = cx.text(p);
    t.starts_with("/**").then_some(t)
}

/// Type named by `@var T [$name]` / `@param T $name` in a docblock, when it is one plain class
/// (`?T`, `T|null` allowed). `name` must match when the tag names a variable (always for
/// `@param`).
fn doc_tag_type(doc: &str, tag: &str, name: &str) -> Option<String> {
    let mut rest = doc;
    while let Some(i) = rest.find(tag) {
        let after = &rest[i + tag.len()..];
        rest = after;
        if !after.starts_with(|c: char| c.is_whitespace()) {
            continue; // `@variable`, `@param-out`
        }
        let mut words = after.split_whitespace().take_while(|w| *w != "*/" && !w.starts_with('@'));
        let Some(ty) = words.next() else { continue };
        let var = words.next().filter(|w| w.starts_with('$') || w.starts_with("...$"));
        match var {
            Some(v) if v.trim_start_matches("...").trim_start_matches('$') != name => continue,
            None if tag == "@param" => continue,
            _ => {}
        }
        let ty = ty.trim_start_matches('?');
        let parts: Vec<&str> = ty.split('|').filter(|p| !p.eq_ignore_ascii_case("null")).collect();
        let [one] = parts.as_slice() else { return None };
        let t = class_name(one);
        return (!t.is_empty()).then_some(t);
    }
    None
}

// ---------------------------------------------------------------------------- bindings

/// Add a binding and remember the name as bound in `scope`.
#[allow(clippy::too_many_arguments)]
fn bind(cx: &mut Cx, st: &mut St, scope: u32, name: &str, kind: BindKind, ty: &str, init: &str, line: u32) {
    if name.is_empty() || name == "this" {
        return;
    }
    st.bound.insert((scope, name.to_owned()));
    cx.add_binding(scope, name, kind, ty, init, line);
}

/// A local variable; only the first binding of a name in a scope counts.
fn local(cx: &mut Cx, st: &mut St, scope: u32, v: Node, ty: &str, init: &str) {
    let name = var_name(cx, v);
    if st.bound.contains(&(scope, name.clone())) {
        return;
    }
    let line = cx.line(v);
    bind(cx, st, scope, &name, BindKind::Local, ty, init, line);
}

/// A field of class scope `cs`. The first field of a name stays the one the resolver reads;
/// an untyped one picks up the type of a later typed assignment.
fn field(cx: &mut Cx, st: &mut St, cs: u32, name: &str, ty: &str, init: &str, line: u32) {
    if name.is_empty() {
        return;
    }
    if let Some(&i) = st.fields.get(&(cs, name.to_owned())) {
        let b = &mut cx.facts.bindings[i];
        if b.decl_type.is_empty() && b.init_ctor.is_empty() {
            b.decl_type = ty.to_owned();
            b.init_ctor = init.to_owned();
        }
        return;
    }
    st.fields.insert((cs, name.to_owned()), cx.facts.bindings.len());
    cx.add_binding(cs, name, BindKind::Field, ty, init, line);
}

/// Variables bound by a `foreach` target or a `list(...)` / `[...]` destructuring.
fn bind_targets(cx: &mut Cx, st: &mut St, n: Node, scope: u32) {
    match n.kind() {
        "variable_name" => local(cx, st, scope, n, "", ""),
        "by_ref" | "pair" | "list_literal" | "array_creation_expression" | "array_element_initializer" => {
            for ch in kids(n) {
                bind_targets(cx, st, ch, scope);
            }
        }
        _ => {}
    }
}

/// Type and bounded initializer path of an assigned value.
fn value_info(cx: &Cx, v: Node, scope: u32) -> (String, String) {
    match v.kind() {
        "object_creation_expression" => {
            let t = kids(v).find(|c| matches!(c.kind(), "name" | "qualified_name" | "relative_name")).map(|c| class_name(cx.text(c))).unwrap_or_default();
            (t, String::new())
        }
        "scoped_call_expression" => {
            let (Some(s), Some(m)) = (v.child_by_field_name("scope"), v.child_by_field_name("name")) else { return Default::default() };
            if m.kind() != "name" || !matches!(s.kind(), "name" | "qualified_name" | "relative_name" | "relative_scope") {
                return Default::default();
            }
            let owner = class_name(cx.text(s));
            if owner.is_empty() {
                return Default::default(); // `parent::make()`
            }
            (String::new(), format!("{owner}::{}()", cx.text(m)))
        }
        "function_call_expression" => match v.child_by_field_name("function") {
            Some(f) if matches!(f.kind(), "name" | "qualified_name" | "relative_name") => {
                let path: String = cx.text(f).split_whitespace().collect();
                let path = path.trim_start_matches('\\');
                let path = path.strip_prefix("namespace\\").unwrap_or(path);
                (String::new(), format!("{path}()"))
            }
            _ => Default::default(),
        },
        "member_call_expression" => {
            let (Some(o), Some(m)) = (v.child_by_field_name("object"), v.child_by_field_name("name")) else { return Default::default() };
            if o.kind() != "variable_name" || m.kind() != "name" {
                return Default::default(); // `$this->repo->find()`, `$a->$m()`: too deep / dynamic
            }
            (String::new(), format!("{}.{}()", var_name(cx, o), cx.text(m)))
        }
        "parenthesized_expression" => v.named_child(0).map(|c| value_info(cx, c, scope)).unwrap_or_default(),
        // `$a = $b = new Foo()`
        "assignment_expression" => v.child_by_field_name("right").map(|c| value_info(cx, c, scope)).unwrap_or_default(),
        // alias of another variable: copy what we know about it
        "variable_name" => {
            let name = var_name(cx, v);
            if name == "this" {
                return Default::default();
            }
            cx.lookup_binding(scope, &name).unwrap_or_default()
        }
        _ => Default::default(),
    }
}

fn assignment(cx: &mut Cx, st: &mut St, n: Node, scope: u32) {
    let Some(left) = n.child_by_field_name("left") else { return };
    match left.kind() {
        "variable_name" => {
            let name = var_name(cx, left);
            if name == "this" || st.bound.contains(&(scope, name.clone())) {
                return;
            }
            let (mut ty, init) = n.child_by_field_name("right").map(|r| value_info(cx, r, scope)).unwrap_or_default();
            // `/** @var Foo $x */ $x = $container->get(...);`
            if let Some(d) =
                n.parent().filter(|p| p.kind() == "expression_statement").and_then(|p| doc_comment(cx, p)).and_then(|d| doc_tag_type(d, "@var", &name))
            {
                ty = d;
            }
            let line = cx.line(left);
            bind(cx, st, scope, &name, BindKind::Local, &ty, &init, line);
        }
        "list_literal" | "array_creation_expression" => bind_targets(cx, st, left, scope),
        "member_access_expression" => {
            // `$this->repo = new Repo()` declares a field of the enclosing class
            let (Some(o), Some(m)) = (left.child_by_field_name("object"), left.child_by_field_name("name")) else { return };
            if o.kind() != "variable_name" || var_name(cx, o) != "this" || m.kind() != "name" {
                return;
            }
            let Some(cs) = cx.class_scope_of(scope) else { return };
            let (ty, mut init) = n.child_by_field_name("right").map(|r| value_info(cx, r, scope)).unwrap_or_default();
            // the field lives in the class scope: a call on a method's local means nothing there
            if init.split('(').next().is_some_and(|p| p.contains('.') && !p.starts_with("this.")) {
                init.clear();
            }
            let name = cx.text(m).to_owned();
            let line = cx.line(m);
            field(cx, st, cs, &name, &ty, &init, line);
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------- declarations

fn namespace(cx: &mut Cx, st: &mut St, n: Node) {
    if st.package {
        return;
    }
    let Some(nm) = n.child_by_field_name("name") else { return };
    st.package = true;
    let name: String = cx.text(nm).split_whitespace().collect();
    let name = name.trim_matches('\\').to_owned();
    let line = cx.line(n);
    cx.add_binding(0, &name, BindKind::Package, "", "", line);
}

/// `use A\B [as C], D\E;`, `use A\{B, C as D};`, `use function A\f;` (`use const` is skipped).
fn use_declaration(cx: &mut Cx, n: Node, scope: u32) {
    let is_const = |cx: &Cx, n: Node| n.child_by_field_name("type").is_some_and(|t| cx.text(t).eq_ignore_ascii_case("const"));
    // A broken statement recovers into misleading clauses (the grammar reads the unsupported
    // `use \A\{B};` as `use \A;` plus an error): bind nothing rather than a wrong name.
    if is_const(cx, n) || kids(n).any(|c| c.is_error()) {
        return;
    }
    let path = |cx: &Cx, p: Node| -> String { cx.text(p).split_whitespace().collect::<String>().trim_matches('\\').to_owned() };
    let prefix = kids(n).find(|c| c.kind() == "namespace_name").map(|p| path(cx, p));
    let clauses: Vec<Node> = match n.child_by_field_name("body") {
        Some(group) => kids(group).filter(|c| c.kind() == "namespace_use_clause").collect(),
        None => kids(n).filter(|c| c.kind() == "namespace_use_clause").collect(),
    };
    for c in clauses {
        if is_const(cx, c) {
            continue;
        }
        let alias = c.child_by_field_name("alias");
        let Some(p) = kids(c).find(|k| matches!(k.kind(), "name" | "qualified_name") && Some(k.id()) != alias.map(|a| a.id())) else { continue };
        let tail = path(cx, p);
        let spec = match &prefix {
            Some(pre) if !pre.is_empty() => format!("{pre}\\{tail}"),
            _ => tail,
        };
        let imported = spec.rsplit('\\').next().unwrap_or("").to_owned();
        if imported.is_empty() {
            continue;
        }
        let local = alias.map(|a| cx.text(a).trim().to_owned()).filter(|a| !a.is_empty()).unwrap_or_else(|| imported.clone());
        let line = cx.line(c);
        cx.add_import(ImportFact { local, imported, spec, kind: ImportKind::Named, type_only: false, scope, line });
    }
}

fn class(cx: &mut Cx, st: &mut St, n: Node, scope: u32) {
    // `new class($a) { ... }`: the constructor arguments belong to the outer scope
    for a in kids(n).filter(|c| c.kind() == "arguments") {
        walk(cx, st, a, scope);
    }
    let sc = cx.push_scope(ScopeKind::Class, scope, n);
    let Some(body) = n.child_by_field_name("body") else { return };
    // Declared and promoted properties first, so `$this->x = ...` in any method knows which
    // fields already carry a type.
    for m in kids(body) {
        match m.kind() {
            "property_declaration" => property(cx, st, m, sc),
            "method_declaration" if m.child_by_field_name("name").is_some_and(|nm| cx.text(nm).eq_ignore_ascii_case("__construct")) => {
                let doc = doc_comment(cx, m);
                if let Some(ps) = m.child_by_field_name("parameters") {
                    for p in kids(ps).filter(|p| p.kind() == "property_promotion_parameter") {
                        let Some(v) = p.child_by_field_name("name") else { continue };
                        let name = var_name(cx, v);
                        let ty = param_type(cx, p, &name, doc);
                        let line = cx.line(v);
                        field(cx, st, sc, &name, &ty, "", line);
                    }
                }
            }
            _ => {}
        }
    }
    for m in kids(body) {
        match m.kind() {
            "property_declaration" | "use_declaration" | "const_declaration" | "enum_case" => {}
            _ => walk(cx, st, m, sc),
        }
    }
}

/// `private ?Repo $repo = null, $other;` (type from the declaration or its `@var` docblock).
fn property(cx: &mut Cx, st: &mut St, n: Node, sc: u32) {
    let native = n.child_by_field_name("type").map(|t| type_name(cx, t));
    let doc = if native.is_none() { doc_comment(cx, n) } else { None };
    for el in kids(n).filter(|c| c.kind() == "property_element") {
        let Some(v) = el.child_by_field_name("name") else { continue };
        let name = var_name(cx, v);
        let ty = match &native {
            Some(t) => t.clone(),
            None => doc.and_then(|d| doc_tag_type(d, "@var", &name)).unwrap_or_default(),
        };
        let line = cx.line(v);
        field(cx, st, sc, &name, &ty, "", line);
    }
}

/// Declared type of a parameter, falling back to the function's `@param` docblock.
fn param_type(cx: &Cx, p: Node, name: &str, doc: Option<&str>) -> String {
    if p.kind() == "variadic_parameter" {
        return String::new(); // `Foo ...$xs` is an array
    }
    match p.child_by_field_name("type") {
        Some(t) => type_name(cx, t),
        None => doc.and_then(|d| doc_tag_type(d, "@param", name)).unwrap_or_default(),
    }
}

fn params(cx: &mut Cx, st: &mut St, f: Node, sc: u32, doc: Option<&str>) {
    let Some(ps) = f.child_by_field_name("parameters") else { return };
    for p in kids(ps) {
        if !matches!(p.kind(), "simple_parameter" | "property_promotion_parameter" | "variadic_parameter") {
            continue;
        }
        let Some(v) = p.child_by_field_name("name") else { continue };
        let name = var_name(cx, v);
        let ty = param_type(cx, p, &name, doc);
        let line = cx.line(v);
        bind(cx, st, sc, &name, BindKind::Param, &ty, "", line);
    }
}

fn function(cx: &mut Cx, st: &mut St, n: Node, scope: u32) {
    if n.kind() == "function_definition" && cx.facts.scopes[scope as usize].kind == ScopeKind::Function {
        if let Some(name) = n.child_by_field_name("name") {
            let nm = cx.text(name).trim().to_owned();
            if !nm.is_empty() {
                let row = cx.row_at(cx.start(n));
                let line = cx.line(name);
                cx.add_binding(scope, &nm, BindKind::LocalFn, "", "", line);
                if let Some(b) = cx.facts.bindings.last_mut() {
                    b.node_row = row.unwrap_or(NONE);
                }
            }
        }
    }
    let sc = cx.push_scope(ScopeKind::Function, scope, n);
    let doc = doc_comment(cx, n);
    params(cx, st, n, sc, doc);
    if let Some(body) = n.child_by_field_name("body") {
        walk(cx, st, body, sc);
    }
}

/// `function ($a) use ($b, &$c) { ... }` / `fn($a) => ...`.
fn closure(cx: &mut Cx, st: &mut St, n: Node, scope: u32) {
    let sc = cx.push_scope(ScopeKind::Function, scope, n);
    params(cx, st, n, sc, None);
    for u in kids(n).filter(|c| c.kind() == "anonymous_function_use_clause") {
        for v in kids(u).filter(|c| matches!(c.kind(), "variable_name" | "by_ref")) {
            let name = var_name(cx, v);
            // captured by value or reference: it carries what the outer variable is known to be
            let (ty, init) = if name.is_empty() { Default::default() } else { cx.lookup_binding(scope, &name).unwrap_or_default() };
            let line = cx.line(v);
            bind(cx, st, sc, &name, BindKind::Param, &ty, &init, line);
        }
    }
    if let Some(body) = n.child_by_field_name("body") {
        walk(cx, st, body, sc);
    }
}
