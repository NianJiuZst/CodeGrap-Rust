//! Facts for Swift: module imports, type / function / accessor / closure scopes, parameters
//! (internal names), stored and computed properties, and local bindings from `let` / `var`,
//! `if let` / `guard let` / `while let`, `case let` patterns and `for ... in` loops.
//! The grammar (alex-pinkus tree-sitter-swift) reuses the `name` field for both names and
//! types, so names are told apart from types by node kind.

use super::util::*;
use super::*;

pub fn extract(cx: &mut Cx, root: Node) {
    walk(cx, root, 0);
}

/// Children of `n` with their field names (anonymous tokens included).
fn fields<'t>(n: Node<'t>) -> Vec<(Option<&'static str>, Node<'t>)> {
    let mut c = n.walk();
    let mut v = Vec::new();
    if c.goto_first_child() {
        loop {
            v.push((c.field_name(), c.node()));
            if !c.goto_next_sibling() {
                break;
            }
        }
    }
    v
}

fn first<'t>(n: Node<'t>, kind: &str) -> Option<Node<'t>> {
    kids(n).find(|c| c.kind() == kind)
}

fn is_comment(n: Node) -> bool {
    matches!(n.kind(), "comment" | "multiline_comment")
}

fn walk_children(cx: &mut Cx, n: Node, scope: u32) {
    for ch in kids(n) {
        walk(cx, ch, scope);
    }
}

fn walk(cx: &mut Cx, n: Node, scope: u32) {
    match n.kind() {
        "import_declaration" => import(cx, n),
        // class / struct / enum / actor / extension share `class_declaration`
        "class_declaration" | "protocol_declaration" => {
            let sc = cx.push_scope(ScopeKind::Class, scope, n);
            walk_children(cx, n, sc);
        }
        "function_declaration" | "init_declaration" | "deinit_declaration" | "subscript_declaration" => function(cx, n, scope),
        "lambda_literal" => lambda(cx, n, scope),
        "property_declaration" | "protocol_property_declaration" => property(cx, n, scope),
        "if_statement" | "guard_statement" | "while_statement" => conditional(cx, n, scope),
        "for_statement" => for_in(cx, n, scope),
        // `case let .x(a)`, `case .x(let a)`, `catch let e`, `if case let ...`
        "pattern" => {
            let mut names = Vec::new();
            pattern_names(cx, n, false, &mut names);
            add_locals(cx, scope, names, BindKind::Local);
        }
        _ => walk_children(cx, n, scope),
    }
}

fn binding_kind(cx: &Cx, scope: u32) -> BindKind {
    if cx.facts.scopes[scope as usize].kind == ScopeKind::Class {
        BindKind::Field
    } else {
        BindKind::Local
    }
}

fn add_locals(cx: &mut Cx, scope: u32, names: Vec<(Node, String)>, kind: BindKind) {
    for (id, ty) in names {
        add_named(cx, scope, id, kind, &ty, "");
    }
}

/// Bind the identifier node `id`. Skips the `_` wildcard and `guard let self = self`, which
/// must not shadow the receiver.
fn add_named(cx: &mut Cx, scope: u32, id: Node, kind: BindKind, ty: &str, init: &str) {
    let name = cx.text(id);
    if name == "_" || name == "self" {
        return;
    }
    let line = cx.line(id);
    cx.add_binding(scope, name, kind, ty, init, line);
}

// ---------------------------------------------------------------- imports

/// `import Foundation`, `@testable import MyLib`, `import struct Foo.Bar` all bind the module.
fn import(cx: &mut Cx, n: Node) {
    let Some(id) = first(n, "identifier") else { return };
    let Some(module) = first(id, "simple_identifier") else { return };
    let spec = cx.text(module).to_owned();
    let line = cx.line(id);
    cx.add_import(ImportFact { local: "*".into(), imported: "*".into(), spec, kind: ImportKind::Namespace, type_only: false, scope: 0, line });
}

// ---------------------------------------------------------------- types

/// Reduce a type node to a nominal name: optionals unwrapped, generics reduced to their head,
/// `some P` / `any P` to `P`; collections, tuples, functions and metatypes give "".
fn type_name(cx: &Cx, t: Node) -> String {
    match t.kind() {
        "user_type" => {
            let parts: Vec<&str> = kids(t).filter(|c| c.kind() == "type_identifier").map(|c| cx.text(c)).collect();
            if parts.len() > 1 && matches!(parts.last(), Some(&"Type") | Some(&"Protocol")) {
                return String::new();
            }
            let name = parts.join(".");
            if name == "Optional" || name == "Swift.Optional" {
                // `Optional<Foo>` is `Foo?`
                let args: Vec<Node> = first(t, "type_arguments").map(|a| kids(a).filter(|c| c.is_named() && !is_comment(*c)).collect()).unwrap_or_default();
                return match args.as_slice() {
                    [one] => type_name(cx, *one),
                    _ => String::new(),
                };
            }
            name
        }
        "optional_type" => t.child_by_field_name("wrapped").map(|w| type_name(cx, w)).unwrap_or_default(),
        "opaque_type" | "existential_type" => kids(t).find(|c| c.is_named() && !is_comment(*c)).map(|c| type_name(cx, c)).unwrap_or_default(),
        // `(Foo)` is a parenthesized `Foo`
        "tuple_type" => {
            let items: Vec<Node> = kids(t).filter(|c| c.kind() == "tuple_type_item").collect();
            match items.as_slice() {
                [item] => {
                    let named: Vec<Node> = kids(*item).filter(|c| c.is_named() && !is_comment(*c)).collect();
                    match named.as_slice() {
                        [one] => type_name(cx, *one),
                        _ => String::new(),
                    }
                }
                _ => String::new(),
            }
        }
        "type_identifier" => cx.text(t).to_owned(),
        _ => String::new(),
    }
}

/// Type written in a `type_annotation` (`: Foo`).
fn annotation_type(cx: &Cx, ta: Node) -> String {
    let t = ta.child_by_field_name("name").or_else(|| kids(ta).find(|c| c.is_named() && !is_comment(*c)));
    t.map(|t| type_name(cx, t)).unwrap_or_default()
}

// ---------------------------------------------------------------- initialisers

/// Strip `try` / `try?` / `try!` / `await` wrappers from an expression.
fn strip_effects(mut v: Node) -> Node {
    while matches!(v.kind(), "try_expression" | "await_expression") {
        match v.child_by_field_name("expr") {
            Some(e) => v = e,
            None => break,
        }
    }
    v
}

/// (cast type, constructor path) of an initialiser: `x as? Foo` gives the type `Foo`, a single
/// call gives its path (`Foo()`, `Foo.make()`, `x.build()`).
fn value_info(cx: &Cx, v: Node) -> (String, String) {
    let v = strip_effects(v);
    if v.kind() == "as_expression" {
        let t = v.child_by_field_name("name").map(|t| type_name(cx, t)).unwrap_or_default();
        return (t, String::new());
    }
    (String::new(), ctor(cx, v).unwrap_or_default())
}

fn ctor(cx: &Cx, v: Node) -> Option<String> {
    match v.kind() {
        "call_expression" => {
            let callee = v.named_child(0)?;
            if callee.kind() == "call_expression" {
                // `Foo(a) { ... }`: a trailing closure on a call; `Foo()()` is not a constructor
                let suffix = first(v, "call_suffix")?;
                if first(suffix, "value_arguments").is_some() {
                    return None;
                }
                return ctor(cx, callee);
            }
            let path = callee_path(cx, callee)?;
            let path = path.strip_suffix(".init").unwrap_or(&path);
            Some(format!("{path}()"))
        }
        // `Foo<Int>()`
        "constructor_expression" => {
            let t = v.child_by_field_name("constructed_type")?;
            if t.kind() != "user_type" {
                return None;
            }
            let name = type_name(cx, t);
            (!name.is_empty()).then(|| format!("{name}()"))
        }
        _ => None,
    }
}

/// Dotted path of a callee made only of identifiers (`Foo`, `Foo.make`, `self.x.build`).
fn callee_path(cx: &Cx, n: Node) -> Option<String> {
    match n.kind() {
        "simple_identifier" => Some(cx.text(n).to_owned()),
        "self_expression" => Some("self".to_owned()),
        "navigation_expression" => {
            let target = n.child_by_field_name("target")?;
            let suffix = n.child_by_field_name("suffix")?;
            let name = suffix.child_by_field_name("suffix")?;
            if name.kind() != "simple_identifier" {
                return None;
            }
            Some(format!("{}.{}", callee_path(cx, target)?, cx.text(name)))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------- patterns

/// Names a pattern binds. `bind` is set inside a `let` / `var` context (declarations,
/// `for` items, or after a `let` within a `case` pattern). `case let x as Foo` types `x`.
fn pattern_names<'t>(cx: &Cx, p: Node<'t>, mut bind: bool, out: &mut Vec<(Node<'t>, String)>) {
    let ch = fields(p);
    let named = ch.iter().filter(|(_, c)| c.is_named() && !is_comment(*c) && c.kind() != "value_binding_pattern").count();
    let start = out.len();
    let mut saw_as = false;
    let mut cast: Option<String> = None;
    for (f, c) in &ch {
        match c.kind() {
            "value_binding_pattern" => bind = true,
            "simple_identifier" if *f == Some("bound_identifier") || (bind && named == 1) => out.push((*c, String::new())),
            "pattern" => pattern_names(cx, *c, bind, out),
            "as" => saw_as = true,
            _ if saw_as && cast.is_none() && c.is_named() && !is_comment(*c) => cast = Some(type_name(cx, *c)),
            _ => {}
        }
    }
    if let Some(t) = cast {
        if out.len() == start + 1 {
            out[start].1 = t;
        }
    }
}

// ---------------------------------------------------------------- declarations

/// `let a: Foo = x, b = Foo()`, stored / computed / observed properties, protocol requirements.
fn property(cx: &mut Cx, n: Node, scope: u32) {
    let kind = binding_kind(cx, scope);
    let ch = fields(n);
    // one declarator per `name:` pattern; its type and value follow it
    let mut decls: Vec<(Node, String, String, bool)> = Vec::new();
    for (f, c) in &ch {
        match c.kind() {
            "pattern" if *f == Some("name") => decls.push((*c, String::new(), String::new(), false)),
            "type_annotation" => {
                if let Some(d) = decls.last_mut() {
                    d.1 = annotation_type(cx, *c);
                    d.3 = true;
                }
            }
            _ if *f == Some("value") => {
                if let Some(d) = decls.last_mut() {
                    let (cast, init) = value_info(cx, *c);
                    if !d.3 {
                        d.1 = cast;
                    }
                    // a written non-nominal type (`[Foo]`, `(A, B)`) is authoritative
                    if !(d.3 && d.1.is_empty()) {
                        d.2 = init;
                    }
                }
            }
            _ => {}
        }
    }
    for (pat, ty, init, _) in &decls {
        let mut names = Vec::new();
        pattern_names(cx, *pat, true, &mut names);
        if let [(id, _)] = names.as_slice() {
            add_named(cx, scope, *id, kind, ty, init);
        } else {
            // tuple destructuring: no per-element types
            for (id, _) in names {
                add_named(cx, scope, id, kind, "", "");
            }
        }
    }
    let mut cur = 0usize;
    let mut seen = false;
    for (f, c) in &ch {
        match c.kind() {
            "pattern" if *f == Some("name") => {
                if seen {
                    cur += 1;
                }
                seen = true;
            }
            "pattern" | "type_annotation" | "value_binding_pattern" => {}
            "computed_property" => {
                let ty = decls.get(cur).map(|d| d.1.clone()).unwrap_or_default();
                computed(cx, *c, scope, &ty, true);
            }
            "willset_didset_block" => {
                let ty = decls.get(cur).map(|d| d.1.clone()).unwrap_or_default();
                for cl in kids(*c) {
                    match cl.kind() {
                        "willset_clause" => accessor(cx, cl, scope, &ty, Some("newValue")),
                        "didset_clause" => accessor(cx, cl, scope, &ty, Some("oldValue")),
                        _ => walk(cx, cl, scope),
                    }
                }
            }
            _ => walk(cx, *c, scope),
        }
    }
}

/// A computed property body: either bare statements (a getter) or explicit accessors.
/// `own_scope` is false for a subscript body, which already has the subscript's scope.
fn computed(cx: &mut Cx, cp: Node, scope: u32, ty: &str, own_scope: bool) {
    let has_accessors = kids(cp).any(|c| matches!(c.kind(), "computed_getter" | "computed_setter" | "computed_modify"));
    if has_accessors {
        for c in kids(cp) {
            match c.kind() {
                "computed_getter" | "computed_modify" => accessor(cx, c, scope, ty, None),
                "computed_setter" => accessor(cx, c, scope, ty, Some("newValue")),
                _ => walk(cx, c, scope),
            }
        }
    } else if own_scope {
        let sc = cx.push_scope(ScopeKind::Function, scope, cp);
        walk_children(cx, cp, sc);
    } else {
        walk_children(cx, cp, scope);
    }
}

/// `get { }`, `set(v) { }`, `willSet { }`, `didSet { }`: a function scope when it has a body.
/// `implicit` names the parameter Swift provides when none is written.
fn accessor(cx: &mut Cx, a: Node, scope: u32, ty: &str, implicit: Option<&str>) {
    if !kids(a).any(|c| c.kind() == "{" || c.kind() == "statements") {
        return;
    }
    let sc = cx.push_scope(ScopeKind::Function, scope, a);
    if let Some(imp) = implicit {
        match first(a, "simple_identifier") {
            Some(id) => add_named(cx, sc, id, BindKind::Param, ty, ""),
            None => {
                let line = cx.line(a);
                cx.add_binding(sc, imp, BindKind::Param, ty, "", line);
            }
        }
    }
    for c in kids(a) {
        if c.kind() != "simple_identifier" {
            walk(cx, c, sc);
        }
    }
}

/// `func`, `init`, `deinit`, `subscript`.
fn function(cx: &mut Cx, n: Node, scope: u32) {
    let ch = fields(n);
    if n.kind() == "function_declaration" && cx.facts.scopes[scope as usize].kind == ScopeKind::Function {
        if let Some((_, id)) = ch.iter().find(|(f, c)| *f == Some("name") && c.kind() == "simple_identifier") {
            let before = cx.facts.bindings.len();
            add_named(cx, scope, *id, BindKind::LocalFn, "", "");
            if cx.facts.bindings.len() > before {
                // the kernel mints a node for the local function at the declaration's start
                let row = cx.row_at(cx.start(n)).unwrap_or(NONE);
                if let Some(b) = cx.facts.bindings.last_mut() {
                    b.node_row = row;
                }
            }
        }
    }
    let sc = cx.push_scope(ScopeKind::Function, scope, n);
    // the subscript's element type, for its setter's `newValue`
    let ret =
        ch.iter().find(|(f, c)| *f == Some("name") && c.is_named() && c.kind() != "simple_identifier").map(|(_, t)| type_name(cx, *t)).unwrap_or_default();
    for (_, c) in &ch {
        if c.kind() == "parameter" {
            param(cx, *c, sc);
        }
    }
    for (_, c) in &ch {
        match c.kind() {
            "parameter" => {}
            "computed_property" => computed(cx, *c, sc, &ret, false),
            _ => walk(cx, *c, sc),
        }
    }
}

/// A function or closure parameter: binds the internal name with its written type.
fn param(cx: &mut Cx, p: Node, sc: u32) {
    let ch = fields(p);
    let Some((_, id)) = ch.iter().find(|(f, c)| *f == Some("name") && c.kind() == "simple_identifier") else { return };
    let variadic = ch.iter().any(|(_, c)| c.kind() == "...");
    let ty = match ch.iter().find(|(f, c)| *f == Some("name") && c.is_named() && c.kind() != "simple_identifier") {
        Some((_, t)) if !variadic => type_name(cx, *t),
        _ => String::new(),
    };
    add_named(cx, sc, *id, BindKind::Param, &ty, "");
}

/// `{ (a: Foo, b) in ... }`, `{ a, b in ... }`, `{ [x = y] in ... }`; `$0` binds nothing.
fn lambda(cx: &mut Cx, n: Node, scope: u32) {
    let sc = cx.push_scope(ScopeKind::Function, scope, n);
    for c in kids(n) {
        match c.kind() {
            "lambda_function_type" => {
                for ps in kids(c).filter(|k| k.kind() == "lambda_function_type_parameters") {
                    for lp in kids(ps).filter(|k| k.kind() == "lambda_parameter") {
                        param(cx, lp, sc);
                    }
                }
            }
            "capture_list" => {
                for item in kids(c).filter(|k| k.kind() == "capture_list_item") {
                    let Some(value) = item.child_by_field_name("value") else { continue };
                    if let Some(id) = item.child_by_field_name("name").filter(|i| i.kind() == "simple_identifier") {
                        let (ty, init) = value_info(cx, value);
                        add_named(cx, sc, id, BindKind::Local, &ty, &init);
                    }
                    // captures are evaluated where the closure is written
                    walk(cx, value, scope);
                }
            }
            _ => walk(cx, c, sc),
        }
    }
}

// ---------------------------------------------------------------- statements

/// `if let x = y`, `if let x`, `guard let x: Foo = y else`, `while let x = it.next()`,
/// `if case .some(let x) = y`, `if case let .some(x) = y`. The grammar flattens each
/// condition into the statement, so a leading `let` / `var` applies to the next pattern.
fn conditional(cx: &mut Cx, n: Node, scope: u32) {
    let ch = fields(n);
    let mut let_ctx = false;
    for (i, (f, c)) in ch.iter().enumerate() {
        match c.kind() {
            "value_binding_pattern" => {
                let_ctx = true;
                continue;
            }
            "," => let_ctx = false,
            "pattern" => {
                let mut names = Vec::new();
                pattern_names(cx, *c, let_ctx, &mut names);
                add_locals(cx, scope, names, BindKind::Local);
                continue;
            }
            _ => {}
        }
        if *f != Some("bound_identifier") {
            walk(cx, *c, scope);
            continue;
        }
        let (mut ty, mut init, mut typed, mut eq) = (String::new(), String::new(), false, false);
        // shorthand `if let x` unwraps `x`
        let mut source = Some((cx.text(*c), false));
        for (_, s) in &ch[i + 1..] {
            if is_comment(*s) {
                continue;
            }
            match s.kind() {
                "=" => {
                    eq = true;
                    source = None;
                }
                "type_annotation" if !eq => {
                    ty = annotation_type(cx, *s);
                    typed = true;
                }
                _ if eq && s.is_named() => {
                    let (cast, ctor) = value_info(cx, *s);
                    if !typed {
                        ty = cast;
                    }
                    init = ctor;
                    source = unwrapped_name(cx, *s);
                    break;
                }
                _ => break,
            }
        }
        // `guard let x = x`, `if let x = self.x`, `if let x`: the unwrapped binding keeps the
        // optional's type (types are already reduced past `?`) instead of shadowing it untyped
        if !typed && ty.is_empty() && init.is_empty() {
            if let Some((t, i)) = source.and_then(|(name, field)| visible_type(cx, scope, name, field)) {
                (ty, init) = (t, i);
            }
        }
        add_named(cx, scope, *c, BindKind::Local, &ty, &init);
    }
}

/// The binding an `if let` value names: `y` (any visible binding) or `self.y` (a field).
fn unwrapped_name<'a>(cx: &Cx<'a>, v: Node) -> Option<(&'a str, bool)> {
    match v.kind() {
        "simple_identifier" => Some((cx.text(v), false)),
        "navigation_expression" => {
            let target = v.child_by_field_name("target")?;
            let name = v.child_by_field_name("suffix")?.child_by_field_name("suffix")?;
            (target.kind() == "self_expression" && name.kind() == "simple_identifier").then(|| (cx.text(name), true))
        }
        _ => None,
    }
}

/// (decl_type, init_ctor) of the binding `name` visible from `scope`: the nearest local /
/// parameter (unless `field_only`), else a field of the enclosing type declared so far.
fn visible_type(cx: &Cx, scope: u32, name: &str, field_only: bool) -> Option<(String, String)> {
    if !field_only {
        if let Some(t) = cx.lookup_binding(scope, name) {
            return Some(t);
        }
    }
    let cs = cx.class_scope_of(scope)?;
    cx.facts.bindings.iter().rev().find(|b| b.scope == cs && b.name == name && b.kind == BindKind::Field).map(|b| (b.decl_type.clone(), b.init_ctor.clone()))
}

/// `for x in xs`, `for (k, v) in d`, `for x: Foo in xs`, `for case let x? in xs`.
fn for_in(cx: &mut Cx, n: Node, scope: u32) {
    let ch = fields(n);
    let ty = ch.iter().find(|(_, c)| c.kind() == "type_annotation").map(|(_, t)| annotation_type(cx, *t)).unwrap_or_default();
    for (f, c) in &ch {
        match c.kind() {
            "pattern" if *f == Some("item") => {
                let mut names = Vec::new();
                pattern_names(cx, *c, true, &mut names);
                if let [(id, cast)] = names.as_slice() {
                    let t = if ty.is_empty() { cast.clone() } else { ty.clone() };
                    add_named(cx, scope, *id, BindKind::Local, &t, "");
                } else {
                    add_locals(cx, scope, names, BindKind::Local);
                }
            }
            "type_annotation" => {}
            _ => walk(cx, *c, scope),
        }
    }
}
