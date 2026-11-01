//! Facts for C#: the first namespace as the package, `using` directives (namespace, static,
//! alias), type / member / lambda scopes, and parameters, locals, fields and properties with
//! their declared types. Shaped after the Java extractor.

use super::util::*;
use super::*;

pub fn extract(cx: &mut Cx, root: Node) {
    walk(cx, root, 0);
}

fn walk_children(cx: &mut Cx, n: Node, scope: u32) {
    for ch in kids(n) {
        walk(cx, ch, scope);
    }
}

/// Walk the children of `n` except `skip` (a node already handled by the caller).
fn walk_children_except(cx: &mut Cx, n: Node, scope: u32, skip: Option<Node>) {
    for ch in kids(n) {
        if Some(ch) != skip {
            walk(cx, ch, scope);
        }
    }
}

fn walk(cx: &mut Cx, n: Node, scope: u32) {
    match n.kind() {
        "namespace_declaration" | "file_scoped_namespace_declaration" => {
            package(cx, n);
            walk_children(cx, n, scope);
        }
        "using_directive" => import(cx, n, scope),
        "class_declaration" | "struct_declaration" | "interface_declaration" | "record_declaration" | "record_struct_declaration" | "enum_declaration" => {
            class(cx, n, scope)
        }
        "method_declaration" | "constructor_declaration" | "destructor_declaration" | "operator_declaration" | "conversion_operator_declaration" => {
            function(cx, n, scope)
        }
        "local_function_statement" => {
            // Local functions are visible throughout the enclosing body (callable before their
            // declaration), so they are hoisted: line 0. The kernel mints no node for them.
            if let Some(nm) = n.child_by_field_name("name") {
                let t = ident(cx, nm);
                cx.add_binding(scope, &t, BindKind::LocalFn, "", "", 0);
            }
            function(cx, n, scope);
        }
        "lambda_expression" | "anonymous_method_expression" => lambda(cx, n, scope),
        "property_declaration" => property(cx, n, scope),
        "indexer_declaration" => indexer(cx, n, scope),
        "event_declaration" => {
            // `event EventHandler E { add {...} remove {...} }`
            let ty = n.child_by_field_name("type").map(|t| type_name(cx, t)).unwrap_or_default();
            if let Some(nm) = n.child_by_field_name("name") {
                bind(cx, scope, nm, BindKind::Field, &ty, "");
            }
            if let Some(acc) = n.child_by_field_name("accessors") {
                accessors(cx, acc, scope, None, &ty);
            }
        }
        // locals, `using (...)`, `for (...)`, `fixed (...)`; fields and events in a type body
        "variable_declaration" => {
            declare(cx, n, scope);
            walk_children(cx, n, scope);
        }
        "foreach_statement" => {
            let ty = n.child_by_field_name("type").map(|t| type_name(cx, t)).unwrap_or_default();
            if let Some(left) = n.child_by_field_name("left") {
                if left.kind() == "identifier" {
                    bind(cx, scope, left, BindKind::Local, &ty, "");
                } else {
                    // `foreach (var (a, b) in pairs)`
                    bind_designation(cx, scope, left);
                }
            }
            walk_children(cx, n, scope);
        }
        // `out Foo x` / `out var x` / `(var a, Foo b) = ...`
        "declaration_expression" => {
            if let Some(nm) = n.child_by_field_name("name") {
                if nm.kind() == "identifier" {
                    let ty = n.child_by_field_name("type").map(|t| type_name(cx, t)).unwrap_or_default();
                    bind(cx, scope, nm, BindKind::Local, &ty, "");
                } else {
                    bind_designation(cx, scope, nm);
                }
            }
        }
        // `x is Foo f`, `case Foo f:`, `x is var v`, `x is Foo { P: 1 } f`
        "declaration_pattern" | "recursive_pattern" => {
            if let Some(nm) = n.child_by_field_name("name") {
                if nm.kind() == "identifier" {
                    let ty = n.child_by_field_name("type").map(|t| type_name(cx, t)).unwrap_or_default();
                    bind(cx, scope, nm, BindKind::Local, &ty, "");
                } else {
                    bind_designation(cx, scope, nm);
                }
            }
            walk_children(cx, n, scope);
        }
        "var_pattern" => bind_designation(cx, scope, n),
        "catch_declaration" => {
            let ty = n.child_by_field_name("type").map(|t| type_name(cx, t)).unwrap_or_default();
            if let Some(nm) = n.child_by_field_name("name") {
                bind(cx, scope, nm, BindKind::Local, &ty, "");
            }
        }
        // LINQ range variables
        "from_clause" => {
            let ty = n.child_by_field_name("type").map(|t| type_name(cx, t)).unwrap_or_default();
            if let Some(nm) = n.child_by_field_name("name") {
                bind(cx, scope, nm, BindKind::Local, &ty, "");
            }
            walk_children(cx, n, scope);
        }
        "let_clause" => {
            if let Some(nm) = n.named_child(0).filter(|c| c.kind() == "identifier") {
                bind(cx, scope, nm, BindKind::Local, "", "");
            }
            walk_children(cx, n, scope);
        }
        "join_clause" | "query_expression" => {
            query_names(cx, n, scope);
            walk_children(cx, n, scope);
        }
        _ => walk_children(cx, n, scope),
    }
}

/// The first namespace declared in the file names the package.
fn package(cx: &mut Cx, n: Node) {
    if cx.facts.bindings.iter().any(|b| b.kind == BindKind::Package) {
        return;
    }
    let Some(nm) = n.child_by_field_name("name") else { return };
    if let Some(p) = qualified(cx, nm, 0) {
        let line = cx.line(nm);
        cx.add_binding(0, &p, BindKind::Package, "", "", line);
    }
}

/// `using A.B;`, `global using A.B;`, `using static A.B.C;`, `using X = A.B.C;`.
fn import(cx: &mut Cx, n: Node, scope: u32) {
    let alias = n.child_by_field_name("name");
    let is_static = kids(n).any(|c| c.kind() == "static");
    let Some(target) = kids(n).filter(|c| c.is_named() && c.kind() != "comment" && Some(*c) != alias).last() else { return };
    // aliases to tuples, pointers, arrays and predefined types import nothing
    let Some(spec) = qualified(cx, target, 0) else { return };
    let line = cx.line(n);
    let fact = if let Some(a) = alias {
        let imported = spec.rsplit('.').next().unwrap_or("").to_owned();
        ImportFact { local: ident(cx, a), imported, spec, kind: ImportKind::Named, type_only: false, scope, line }
    } else {
        let kind = if is_static { ImportKind::Static } else { ImportKind::Namespace };
        ImportFact { local: "*".into(), imported: "*".into(), spec, kind, type_only: false, scope, line }
    };
    cx.add_import(fact);
}

fn class(cx: &mut Cx, n: Node, scope: u32) {
    let sc = cx.push_scope(ScopeKind::Class, scope, n);
    // primary constructor: `record R(Foo A)` (parameters are also properties), `class C(Foo a)`
    let primary = kids(n).find(|c| c.kind() == "parameter_list");
    if let Some(list) = primary {
        let is_record = matches!(n.kind(), "record_declaration" | "record_struct_declaration");
        for (nm, ty) in param_list(cx, list) {
            bind(cx, sc, nm, BindKind::Param, &ty, "");
            if is_record {
                bind(cx, sc, nm, BindKind::Field, &ty, "");
            }
        }
    }
    walk_children_except(cx, n, sc, primary);
}

/// Methods, constructors, destructors, operators and local functions.
fn function(cx: &mut Cx, n: Node, scope: u32) {
    let sc = cx.push_scope(ScopeKind::Function, scope, n);
    let params = n.child_by_field_name("parameters");
    if let Some(list) = params {
        for (nm, ty) in param_list(cx, list) {
            bind(cx, sc, nm, BindKind::Param, &ty, "");
        }
    }
    walk_children_except(cx, n, sc, params);
}

/// `x => ...`, `(Foo x, int y) => ...`, `(a, b) => ...`, `delegate (Foo x) { ... }`.
fn lambda(cx: &mut Cx, n: Node, scope: u32) {
    let sc = cx.push_scope(ScopeKind::Function, scope, n);
    let params = n.child_by_field_name("parameters").or_else(|| kids(n).find(|c| c.kind() == "parameter_list"));
    if let Some(p) = params {
        match p.kind() {
            "implicit_parameter" | "identifier" => bind(cx, sc, p, BindKind::Param, "", ""),
            _ => {
                for (nm, ty) in param_list(cx, p) {
                    bind(cx, sc, nm, BindKind::Param, &ty, "");
                }
            }
        }
    }
    walk_children_except(cx, n, sc, params);
}

/// `public Foo Bar { get; set; }`, `Foo Bar => expr;`, `Foo Bar { get; } = new();`.
fn property(cx: &mut Cx, n: Node, scope: u32) {
    let ty = n.child_by_field_name("type").map(|t| type_name(cx, t)).unwrap_or_default();
    if let Some(nm) = n.child_by_field_name("name") {
        bind(cx, scope, nm, BindKind::Field, &ty, "");
    }
    if let Some(acc) = n.child_by_field_name("accessors") {
        accessors(cx, acc, scope, None, &ty);
    }
    if let Some(v) = n.child_by_field_name("value") {
        if v.kind() == "arrow_expression_clause" {
            // expression-bodied getter
            let sc = cx.push_scope(ScopeKind::Function, scope, v);
            walk(cx, v, sc);
        } else {
            // initializer
            walk(cx, v, scope);
        }
    }
}

/// `public Foo this[int i] { get { ... } set { ... } }` / `public Foo this[int i] => ...;`
fn indexer(cx: &mut Cx, n: Node, scope: u32) {
    let ty = n.child_by_field_name("type").map(|t| type_name(cx, t)).unwrap_or_default();
    let params = n.child_by_field_name("parameters");
    if let Some(acc) = n.child_by_field_name("accessors") {
        accessors(cx, acc, scope, params, &ty);
    }
    if let Some(v) = n.child_by_field_name("value").filter(|v| v.kind() == "arrow_expression_clause") {
        let sc = cx.push_scope(ScopeKind::Function, scope, v);
        if let Some(list) = params {
            for (nm, pty) in param_list(cx, list) {
                bind(cx, sc, nm, BindKind::Param, &pty, "");
            }
        }
        walk(cx, v, sc);
    }
}

/// Accessors with bodies get a function scope holding the indexer parameters and, for
/// `set` / `init` / `add` / `remove`, the implicit `value` parameter.
fn accessors(cx: &mut Cx, list: Node, scope: u32, params: Option<Node>, ty: &str) {
    for a in kids(list) {
        if a.kind() != "accessor_declaration" {
            continue;
        }
        let Some(body) = a.child_by_field_name("body") else { continue };
        let sc = cx.push_scope(ScopeKind::Function, scope, a);
        if let Some(list) = params {
            for (nm, pty) in param_list(cx, list) {
                bind(cx, sc, nm, BindKind::Param, &pty, "");
            }
        }
        if kids(a).any(|c| matches!(c.kind(), "set" | "init" | "add" | "remove")) {
            let line = cx.line(a);
            cx.add_binding(sc, "value", BindKind::Param, ty, "", line);
        }
        walk(cx, body, sc);
    }
}

/// Parameters of a `parameter_list` / `bracketed_parameter_list` as (name node, type head).
/// `params Foo[] xs` sits directly in the list as `type` + `name` fields.
fn param_list<'t>(cx: &Cx, list: Node<'t>) -> Vec<(Node<'t>, String)> {
    let mut out = Vec::new();
    let mut pending = String::new();
    let mut c = list.walk();
    if !c.goto_first_child() {
        return out;
    }
    loop {
        let node = c.node();
        match (c.field_name(), node.kind()) {
            (_, "parameter") => {
                if let Some(nm) = node.child_by_field_name("name") {
                    let ty = node.child_by_field_name("type").map(|t| type_name(cx, t)).unwrap_or_default();
                    out.push((nm, ty));
                }
            }
            (Some("type"), _) => pending = type_name(cx, node),
            (Some("name"), "identifier") => out.push((node, std::mem::take(&mut pending))),
            _ => {}
        }
        if !c.goto_next_sibling() {
            break;
        }
    }
    out
}

/// `Foo x = ...;`, `var x = new Foo();`, `int a = 1, b = 2;`, `var (a, b) = ...;`. Fields and
/// events when the scope is a type body, locals otherwise.
fn declare(cx: &mut Cx, n: Node, scope: u32) {
    let kind = if cx.facts.scopes[scope as usize].kind == ScopeKind::Class { BindKind::Field } else { BindKind::Local };
    let ty = n.child_by_field_name("type").map(|t| type_name(cx, t)).unwrap_or_default();
    for d in kids(n) {
        if d.kind() != "variable_declarator" {
            continue;
        }
        let (vt, init) = initializer(d).map(|v| value_info(cx, v)).unwrap_or_default();
        let decl = if ty.is_empty() { vt } else { ty.clone() };
        if let Some(nm) = d.child_by_field_name("name") {
            bind(cx, scope, nm, kind, &decl, &init);
        } else if let Some(tp) = kids(d).find(|c| c.kind() == "tuple_pattern") {
            bind_designation(cx, scope, tp);
        }
    }
}

/// The expression after `=` in a `variable_declarator`.
fn initializer(d: Node) -> Option<Node> {
    let mut seen_eq = false;
    for c in kids(d) {
        if seen_eq && c.is_named() && c.kind() != "comment" {
            return Some(c);
        }
        if c.kind() == "=" {
            seen_eq = true;
        }
    }
    None
}

/// (declared type, initializer call path) an initializer contributes.
fn value_info(cx: &Cx, v: Node) -> (String, String) {
    match v.kind() {
        "object_creation_expression" => (v.child_by_field_name("type").map(|t| type_name(cx, t)).unwrap_or_default(), String::new()),
        "invocation_expression" => {
            let path = v.child_by_field_name("function").and_then(|f| call_path(cx, f, 0));
            (String::new(), path.map(|p| format!("{p}()")).unwrap_or_default())
        }
        "cast_expression" => (v.child_by_field_name("type").map(|t| type_name(cx, t)).unwrap_or_default(), String::new()),
        "as_expression" => (v.child_by_field_name("right").map(|t| type_name(cx, t)).unwrap_or_default(), String::new()),
        "parenthesized_expression" => v.named_child(0).map(|c| value_info(cx, c)).unwrap_or_default(),
        _ => (String::new(), String::new()),
    }
}

/// Callee of `X()`, `X.Y()`, `x.y()`, `this.x.Y()`, `Make<T>()` without type arguments;
/// `None` for anything else (chained calls, conditional access, indexers, ...).
fn call_path(cx: &Cx, f: Node, depth: u8) -> Option<String> {
    if depth >= 32 {
        return None;
    }
    match f.kind() {
        "identifier" | "generic_name" => simple_name(cx, f),
        "this" if depth > 0 => Some("this".to_owned()),
        "member_access_expression" => {
            let recv = call_path(cx, f.child_by_field_name("expression")?, depth + 1)?;
            let name = simple_name(cx, f.child_by_field_name("name")?)?;
            Some(format!("{recv}.{name}"))
        }
        _ => None,
    }
}

/// Type head for `decl_type`: generic arguments, nullability and pointers dropped,
/// qualification kept (`global::` removed); arrays, tuples and predefined types give "".
fn type_name(cx: &Cx, n: Node) -> String {
    match n.kind() {
        "nullable_type" | "pointer_type" | "ref_type" | "scoped_type" => n.child_by_field_name("type").map(|t| type_name(cx, t)).unwrap_or_default(),
        _ => match qualified(cx, n, 0) {
            Some(t) if !matches!(t.as_str(), "var" | "dynamic") => t,
            _ => String::new(),
        },
    }
}

/// Dotted path of a (possibly qualified / generic / alias-qualified) name; `None` for any other node.
fn qualified(cx: &Cx, n: Node, depth: u8) -> Option<String> {
    if depth >= 32 {
        return None;
    }
    match n.kind() {
        "identifier" | "generic_name" => simple_name(cx, n),
        // `global::A` -> `A`
        "alias_qualified_name" => qualified(cx, n.child_by_field_name("name")?, depth + 1),
        "qualified_name" => {
            let q = qualified(cx, n.child_by_field_name("qualifier")?, depth + 1)?;
            let name = qualified(cx, n.child_by_field_name("name")?, depth + 1)?;
            Some(format!("{q}.{name}"))
        }
        _ => None,
    }
}

/// `Foo` / `@Foo` / `Foo<T>` -> `Foo`.
fn simple_name(cx: &Cx, n: Node) -> Option<String> {
    let id = match n.kind() {
        "identifier" => n,
        "generic_name" => kids(n).find(|c| c.kind() == "identifier")?,
        _ => return None,
    };
    let t = ident(cx, id);
    (!t.is_empty()).then_some(t)
}

/// Identifier text without the `@` verbatim prefix.
fn ident(cx: &Cx, n: Node) -> String {
    cx.text(n).trim_start_matches('@').to_owned()
}

fn bind(cx: &mut Cx, scope: u32, nm: Node, kind: BindKind, ty: &str, init: &str) {
    let t = ident(cx, nm);
    let line = cx.line(nm);
    cx.add_binding(scope, &t, kind, ty, init, line);
}

/// Untyped locals of a designation: `x`, `(a, b)`, `var (a, (b, c))`, tuple patterns.
fn bind_designation(cx: &mut Cx, scope: u32, n: Node) {
    match n.kind() {
        "identifier" => bind(cx, scope, n, BindKind::Local, "", ""),
        "tuple_pattern" | "parenthesized_variable_designation" | "var_pattern" => {
            for c in kids(n) {
                if c.is_named() {
                    bind_designation(cx, scope, c);
                }
            }
        }
        // discards; anything else (recovered errors, nested declarations) is walked on its own
        _ => {}
    }
}

/// `join Foo x in ...` and `... into y` range variables of a query.
fn query_names(cx: &mut Cx, n: Node, scope: u32) {
    // `join [Type] name in ...`: the first non-type named child is the range variable
    let mut join_name = n.kind() == "join_clause";
    let mut after_into = false;
    let mut c = n.walk();
    if !c.goto_first_child() {
        return;
    }
    loop {
        let node = c.node();
        let is_type = c.field_name() == Some("type");
        if node.kind() == "identifier" && !is_type && (after_into || join_name) {
            let ty = if join_name { n.child_by_field_name("type").map(|t| type_name(cx, t)).unwrap_or_default() } else { String::new() };
            bind(cx, scope, node, BindKind::Local, &ty, "");
        }
        if node.is_named() && !is_type {
            join_name = false;
        }
        after_into = node.kind() == "into";
        if !c.goto_next_sibling() {
            break;
        }
    }
}
