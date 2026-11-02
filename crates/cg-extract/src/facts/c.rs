//! Facts for C and C++ (one walker: the C++ grammar is a superset of C's node kinds).
//!
//! - `#include` directives (also inside `#if` / `#ifdef` blocks) as side-effect imports; system
//!   headers keep their angle brackets (`<vector>`).
//! - `using namespace a::b;`, `using a::b::Foo;` and `namespace x = a::b;` as imports; type
//!   aliases (`using X = a::Foo<int>;`, `typedef a::Foo X;`) as typed locals.
//! - Function / method / constructor / operator / lambda scopes, class / struct / union bodies
//!   and namespace blocks.
//! - Parameters, locals (typed, `auto` with an initialiser, range-for, structured bindings,
//!   condition declarations, catch parameters), fields and file-scope globals.
//!
//! Declared types are reduced to the qualified head of the named type: pointers, references,
//! cv-qualifiers and template arguments are dropped (`const std::shared_ptr<Foo>&` ->
//! `std::shared_ptr`); primitive types and `auto` give an empty type.

use super::util::*;
use super::*;

/// Recursion bound: deeper subtrees are skipped rather than risking the stack.
const MAX_DEPTH: u32 = 400;
/// Bound for the iterative descents through declarator / expression chains.
const MAX_CHAIN: usize = 64;

pub fn extract(cx: &mut Cx, root: Node) {
    walk(cx, root, 0, 0);
}

fn walk_children(cx: &mut Cx, n: Node, scope: u32, depth: u32) {
    for ch in kids(n) {
        walk(cx, ch, scope, depth + 1);
    }
}

fn walk(cx: &mut Cx, n: Node, scope: u32, depth: u32) {
    if depth > MAX_DEPTH {
        return;
    }
    match n.kind() {
        "preproc_include" => include(cx, n),
        // Macro bodies are opaque text; their names are not bindings.
        "preproc_def" => macro_namespaces(cx, n, scope),
        "preproc_function_def" | "preproc_call" | "comment" => {}
        "using_declaration" => using(cx, n, scope),
        "namespace_alias_definition" => namespace_alias(cx, n, scope),
        "alias_declaration" => {
            if let (Some(nm), Some(ty)) = (n.child_by_field_name("name"), n.child_by_field_name("type")) {
                let t = type_name(cx, ty);
                if !t.is_empty() {
                    bind(cx, scope, nm, BindKind::Local, &t, "");
                    owned_fact(cx, scope, n, nm, BindKind::TypeAlias, &t);
                }
            }
        }
        "type_definition" => type_definition(cx, n, scope, depth),
        "namespace_definition" => {
            let sc = cx.push_scope(ScopeKind::Block, scope, n);
            if let Some(body) = n.child_by_field_name("body") {
                walk_children(cx, body, sc, depth + 1);
            }
        }
        "class_specifier" | "struct_specifier" | "union_specifier" => class(cx, n, scope, depth),
        "function_definition" => function(cx, n, scope, depth),
        "lambda_expression" => lambda(cx, n, scope, depth),
        "declaration" => declaration(cx, n, scope, depth),
        "field_declaration" => field_declaration(cx, n, scope, depth),
        "for_range_loop" => {
            let (base, is_auto) = decl_base(cx, n);
            if let Some(d) = n.child_by_field_name("declarator") {
                bind_declarator(cx, d, scope, BindKind::Local, if is_auto { "" } else { &base });
            }
            walk_children(cx, n, scope, depth);
        }
        "catch_clause" => {
            if let Some(params) = n.child_by_field_name("parameters") {
                parameters(cx, params, scope, BindKind::Local);
            }
            if let Some(body) = n.child_by_field_name("body") {
                walk(cx, body, scope, depth + 1);
            }
        }
        _ => walk_children(cx, n, scope, depth),
    }
}

// ---------------------------------------------------------------------------------------------
// imports

/// The namespaces a `#define` body opens.
fn macro_namespaces(cx: &mut Cx, n: Node, scope: u32) {
    let Some(value) = n.child_by_field_name("value") else { return };
    let words: Vec<String> = cx.text(value).split_whitespace().map(str::to_owned).collect();
    let line = cx.line(n);
    for pair in words.windows(2).filter(|w| w[0] == "namespace") {
        let name = pair[1].split("::").next().unwrap_or("").trim_end_matches('{');
        if !name.is_empty() && name != "{" {
            cx.add_binding(scope, name, BindKind::MacroNamespace, "", "", line);
        }
    }
}

fn include(cx: &mut Cx, n: Node) {
    let Some(path) = n.child_by_field_name("path") else { return };
    let spec = match path.kind() {
        "string_literal" => unquote(cx.text(path)).to_owned(),
        "system_lib_string" => cx.text(path).trim().to_owned(),
        // `#include MACRO` / `#include MACRO(x)`: not resolvable syntactically
        _ => return,
    };
    if spec.is_empty() || spec == "<>" {
        return;
    }
    let line = cx.line(n);
    cx.add_import(ImportFact { local: String::new(), imported: String::new(), spec, kind: ImportKind::SideEffect, type_only: false, scope: 0, line });
}

/// `using namespace a::b;` (wildcard) and `using a::b::Foo;` (one name).
fn using(cx: &mut Cx, n: Node, scope: u32) {
    let is_namespace = kids(n).any(|c| c.kind() == "namespace");
    let Some(target) = kids(n).find(|c| matches!(c.kind(), "identifier" | "qualified_identifier" | "type_identifier" | "namespace_identifier")) else {
        return;
    };
    let path = path_text(cx.text(target));
    if path.is_empty() {
        return;
    }
    let line = cx.line(n);
    let fact = if is_namespace {
        ImportFact { local: "*".into(), imported: "*".into(), spec: path, kind: ImportKind::Namespace, type_only: false, scope, line }
    } else {
        let last = path.rsplit("::").next().unwrap_or("").to_owned();
        if last.is_empty() {
            return;
        }
        ImportFact { local: last.clone(), imported: last, spec: path, kind: ImportKind::Named, type_only: false, scope, line }
    };
    cx.add_import(fact);
}

/// `namespace x = a::b;`
fn namespace_alias(cx: &mut Cx, n: Node, scope: u32) {
    let Some(name) = n.child_by_field_name("name") else { return };
    let Some(target) = kids(n).filter(|c| matches!(c.kind(), "namespace_identifier" | "nested_namespace_specifier") && *c != name).last() else {
        return;
    };
    let (local, spec) = (cx.text(name).to_owned(), path_text(cx.text(target)));
    if local.is_empty() || spec.is_empty() {
        return;
    }
    let line = cx.line(n);
    cx.add_import(ImportFact { local, imported: "*".into(), spec, kind: ImportKind::Named, type_only: false, scope, line });
}

// ---------------------------------------------------------------------------------------------
// scopes

fn class(cx: &mut Cx, n: Node, scope: u32, depth: u32) {
    let Some(body) = n.child_by_field_name("body") else { return };
    let sc = cx.push_scope(ScopeKind::Class, scope, n);
    walk_children(cx, body, sc, depth + 1);
}

fn function(cx: &mut Cx, n: Node, scope: u32, depth: u32) {
    let sc = cx.push_scope(ScopeKind::Function, scope, n);
    if let Some(fd) = n.child_by_field_name("declarator").and_then(own_function_declarator) {
        if let Some(params) = fd.child_by_field_name("parameters") {
            parameters(cx, params, sc, BindKind::Param);
        }
        // `auto f() -> T` says it after the parameters, `T f()` before the name
        let trailing = kids(fd).find(|c| c.kind() == "trailing_return_type").and_then(|t| kids(t).find(|c| c.kind() == "type_descriptor"));
        if let (Some(t), Some(nm)) = (trailing.or(n.child_by_field_name("type")), fd.child_by_field_name("declarator")) {
            let base = type_name(cx, t);
            if !base.is_empty() {
                owned_fact(cx, scope, n, nm, BindKind::ReturnType, &base);
            }
        }
    }
    for ch in kids(n) {
        match ch.kind() {
            // member initialisers may hold lambdas
            "field_initializer_list" => walk(cx, ch, sc, depth + 1),
            "compound_statement" | "try_statement" => walk(cx, ch, sc, depth + 1),
            _ => {}
        }
    }
}

fn lambda(cx: &mut Cx, n: Node, scope: u32, depth: u32) {
    let sc = cx.push_scope(ScopeKind::Function, scope, n);
    if let Some(caps) = n.child_by_field_name("captures") {
        for c in kids(caps) {
            if c.kind() == "lambda_capture_initializer" {
                if let Some(nm) = c.child_by_field_name("left") {
                    let (ty, init) = c.child_by_field_name("right").map(|v| value_info(cx, v)).unwrap_or_default();
                    bind(cx, sc, nm, BindKind::Local, &ty, &init);
                }
                if let Some(v) = c.child_by_field_name("right") {
                    walk(cx, v, scope, depth + 1);
                }
            }
        }
    }
    if let Some(params) = n.child_by_field_name("declarator").and_then(|d| d.child_by_field_name("parameters")) {
        parameters(cx, params, sc, BindKind::Param);
    }
    if let Some(body) = n.child_by_field_name("body") {
        walk(cx, body, sc, depth + 1);
    }
}

/// The function declarator that owns the definition's parameters: the innermost one on the
/// declarator chain (`int (*getfp(int x))(int)` -> the one with `int x`).
fn own_function_declarator(d: Node) -> Option<Node> {
    let mut cur = d;
    let mut found = None;
    for _ in 0..MAX_CHAIN {
        if cur.kind() == "function_declarator" {
            found = Some(cur);
        }
        match inner_declarator(cur) {
            Some(next) => cur = next,
            None => break,
        }
    }
    found
}

/// Parameters (or catch parameters) of a parameter list.
fn parameters(cx: &mut Cx, params: Node, scope: u32, kind: BindKind) {
    for p in kids(params) {
        match p.kind() {
            "parameter_declaration" | "optional_parameter_declaration" => {
                let Some(d) = p.child_by_field_name("declarator") else { continue };
                let base = p.child_by_field_name("type").map(|t| type_name(cx, t)).unwrap_or_default();
                bind_declarator(cx, d, scope, kind, &base);
            }
            // K&R `int f(a, b) int a; ...`
            "identifier" => {
                bind(cx, scope, p, kind, "", "");
            }
            // `...`, `Args... args`: no usable binding
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------------------------
// declarations

/// Declared base type of a declaration-like node and whether it is a placeholder (`auto`).
fn decl_base(cx: &Cx, n: Node) -> (String, bool) {
    match n.child_by_field_name("type") {
        Some(t) => (type_name(cx, t), is_placeholder(cx, t)),
        None => (String::new(), false),
    }
}

fn is_placeholder(cx: &Cx, t: Node) -> bool {
    matches!(t.kind(), "placeholder_type_specifier" | "auto") || (t.kind() == "decltype" && cx.text(t).contains("auto"))
}

/// What a declarator declares.
enum Declared<'t> {
    /// A variable named by the node; `true` when it is a function pointer (no class type).
    Var(Node<'t>, bool),
    /// A function (prototype): `name(params)`.
    Func(Node<'t>),
    /// `auto [a, b]`
    Structured(Node<'t>),
    None,
}

fn inner_declarator(n: Node) -> Option<Node> {
    if let Some(d) = n.child_by_field_name("declarator") {
        return Some(d);
    }
    match n.kind() {
        "reference_declarator" | "attributed_declarator" | "parenthesized_declarator" | "abstract_reference_declarator" => kids(n).find(|c| {
            c.is_named()
                && !matches!(
                    c.kind(),
                    "type_qualifier"
                        | "attribute_declaration"
                        | "attribute_specifier"
                        | "ms_call_modifier"
                        | "ms_pointer_modifier"
                        | "ms_based_modifier"
                        | "comment"
                )
        }),
        _ => None,
    }
}

fn classify<'t>(cx: &Cx, d: Node<'t>) -> Declared<'t> {
    let mut cur = d;
    let mut fn_pending: Option<Node> = None;
    let mut fn_ptr = false;
    for _ in 0..MAX_CHAIN {
        match cur.kind() {
            "identifier" | "field_identifier" | "type_identifier" => {
                return match fn_pending {
                    Some(f) => Declared::Func(f),
                    None => Declared::Var(cur, fn_ptr),
                };
            }
            "structured_binding_declarator" => return if fn_pending.is_some() { Declared::None } else { Declared::Structured(cur) },
            "function_declarator" => {
                if fn_pending.is_none() {
                    if let Some(nm) = annotated_name(cx, cur) {
                        return Declared::Var(nm, false);
                    }
                }
                // a second function declarator below a pending one: `int (*f(int))(int)`
                fn_pending = Some(cur);
            }
            "parenthesized_declarator" => {
                if fn_pending.take().is_some() {
                    fn_ptr = true;
                }
            }
            "pointer_declarator" | "reference_declarator" | "array_declarator" | "attributed_declarator" | "init_declarator" | "pointer_type_declarator" => {}
            // `Foo::count`, `operator==`, `~Foo`: members of another scope
            _ => {
                return match fn_pending {
                    Some(f) => Declared::Func(f),
                    None => Declared::None,
                };
            }
        }
        match inner_declarator(cur) {
            Some(next) => cur = next,
            None => return Declared::None,
        }
    }
    Declared::None
}

/// `Foo repo_ GUARDED_BY(mu_);` parses as an ERROR holding `repo_` followed by a declarator for
/// a function `GUARDED_BY(mu_)`: with an ALL_CAPS "function" name the variable is the identifier
/// in the ERROR just before it.
fn annotated_name<'t>(cx: &Cx, f: Node<'t>) -> Option<Node<'t>> {
    let name = f.child_by_field_name("declarator").filter(|n| matches!(n.kind(), "identifier" | "field_identifier"))?;
    if !is_macro_name(cx.text(name)) {
        return None;
    }
    let err = f.prev_sibling().filter(|p| p.kind() == "ERROR")?;
    err.named_child(0).filter(|c| matches!(c.kind(), "identifier" | "field_identifier") && !is_macro_name(cx.text(*c)))
}

/// `FOO_BAR`, `GUARDED_BY`: preprocessor-macro shaped (two or more chars, no lowercase).
fn is_macro_name(s: &str) -> bool {
    s.len() > 1 && s.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_') && s.bytes().any(|b| b.is_ascii_uppercase())
}

/// Reserved words a mis-parse of macro-heavy code can put in a declarator position. Words that
/// are reserved only in C++ (`public`, `new`, `class`, ...) are ordinary identifiers in C and are
/// only rejected in trees from the C++ grammar.
fn is_reserved(n: Node, s: &str) -> bool {
    const BOTH: &[&str] = &[
        "auto", "break", "case", "char", "const", "continue", "default", "do", "double", "else", "enum", "extern", "float", "for", "goto", "if", "inline",
        "int", "long", "register", "return", "short", "signed", "sizeof", "static", "struct", "switch", "typedef", "union", "unsigned", "void", "volatile",
        "while",
    ];
    const CPP: &[&str] = &[
        "bool",
        "catch",
        "class",
        "constexpr",
        "delete",
        "explicit",
        "false",
        "friend",
        "mutable",
        "namespace",
        "new",
        "nullptr",
        "operator",
        "private",
        "protected",
        "public",
        "template",
        "this",
        "throw",
        "true",
        "try",
        "typename",
        "using",
        "virtual",
    ];
    BOTH.contains(&s) || (CPP.contains(&s) && n.language().id_for_node_kind("class_specifier", true) != 0)
}

/// Record a binding named by the identifier node `nm`, at the line of the name.
/// A fact about the node `n` declares, found from anywhere by that node: a function's return
/// type, what an alias names.
fn owned_fact(cx: &mut Cx, scope: u32, n: Node, nm: Node, kind: BindKind, ty: &str) {
    if let Some(row) = cx.row_at(cx.start(n)) {
        let (name, line) = (cx.text(nm).to_owned(), cx.line(nm));
        cx.facts.bindings.push(BindingFact { scope, name, kind, decl_type: ty.to_owned(), init_ctor: String::new(), line, node_row: row });
    }
}

fn bind(cx: &mut Cx, scope: u32, nm: Node, kind: BindKind, ty: &str, init: &str) {
    let name = cx.text(nm);
    // `_GLIBCXX_NODISCARD _GLIBCXX20_CONSTEXPR` before a declaration reads as `Type name;`
    if name.is_empty() || is_reserved(nm, name) || (is_macro_name(name) && is_macro_name(ty)) {
        return;
    }
    let line = cx.line(nm);
    cx.add_binding(scope, name, kind, ty, init, line);
}

/// Bind the name a parameter / range-for declarator declares (no initialiser).
fn bind_declarator(cx: &mut Cx, d: Node, scope: u32, kind: BindKind, base: &str) {
    match classify(cx, d) {
        Declared::Var(nm, fn_ptr) => {
            bind(cx, scope, nm, kind, if fn_ptr { "" } else { base }, "");
        }
        Declared::Structured(s) => structured(cx, s, scope),
        Declared::Func(_) | Declared::None => {}
    }
}

fn structured(cx: &mut Cx, s: Node, scope: u32) {
    for id in kids(s) {
        if id.kind() == "identifier" {
            bind(cx, scope, id, BindKind::Local, "", "");
        }
    }
}

fn scope_kind(cx: &Cx, scope: u32) -> ScopeKind {
    cx.facts.scopes.get(scope as usize).map(|s| s.kind).unwrap_or(ScopeKind::File)
}

/// `Foo x;`, `Foo x(1), *y = new Foo;`, `auto x = make();`, `static Foo g;`, `auto [a, b] = f();`.
fn declaration(cx: &mut Cx, n: Node, scope: u32, depth: u32) {
    // class bodies / lambdas inside the type (`struct S { ... } s;`)
    if let Some(t) = n.child_by_field_name("type") {
        walk(cx, t, scope, depth + 1);
    }
    let (base, is_auto) = decl_base(cx, n);
    let in_class = scope_kind(cx, scope) == ScopeKind::Class;
    let in_function = scope_kind(cx, scope) == ScopeKind::Function;
    let kind = if in_class { BindKind::Field } else { BindKind::Local };
    let decl_value = n.child_by_field_name("value");
    let mut c = n.walk();
    let declarators: Vec<Node> = n.children_by_field_name("declarator", &mut c).collect();
    for d in declarators {
        let (target, value) =
            if d.kind() == "init_declarator" { (d.child_by_field_name("declarator"), d.child_by_field_name("value")) } else { (Some(d), decl_value) };
        let Some(target) = target else { continue };
        match classify(cx, target) {
            Declared::Var(nm, fn_ptr) => {
                let (ty, init) = if fn_ptr {
                    (String::new(), String::new())
                } else if is_auto {
                    value.map(|v| value_info(cx, v)).unwrap_or_default()
                } else {
                    (base.clone(), String::new())
                };
                bind(cx, scope, nm, kind, &ty, &init);
            }
            Declared::Structured(s) => structured(cx, s, scope),
            Declared::Func(f) => {
                // `Foo x(a, b);` in a body is the most vexing parse of a constructed local: a
                // block-scope function declaration with only unnamed parameters is vanishingly rare.
                if in_function && !is_auto && !base.is_empty() && is_vexing_local(f) {
                    if let Some(nm) = f.child_by_field_name("declarator").filter(|d| d.kind() == "identifier") {
                        bind(cx, scope, nm, kind, &base, "");
                    }
                }
            }
            Declared::None => {}
        }
        if let Some(v) = value {
            walk(cx, v, scope, depth + 1);
        }
    }
}

/// A function declarator whose parameter list is non-empty and has no named parameters
/// (`Foo x(a, b)` parsed as a declaration of a function taking types `a` and `b`).
fn is_vexing_local(f: Node) -> bool {
    let Some(params) = f.child_by_field_name("parameters") else { return false };
    let mut any = false;
    for p in kids(params) {
        if !p.is_named() || p.kind() == "comment" {
            continue;
        }
        if p.kind() != "parameter_declaration" || p.child_by_field_name("declarator").is_some() {
            return false;
        }
        any = true;
    }
    any
}

/// Class / struct members: `Foo* next;`, `std::vector<Foo> v, *w;`, `struct { int a; } in;`.
fn field_declaration(cx: &mut Cx, n: Node, scope: u32, depth: u32) {
    let mut c = n.walk();
    let declarators: Vec<Node> = n.children_by_field_name("declarator", &mut c).collect();
    if let Some(t) = n.child_by_field_name("type") {
        let anonymous_member =
            declarators.is_empty() && matches!(t.kind(), "struct_specifier" | "union_specifier" | "class_specifier") && t.child_by_field_name("name").is_none();
        match t.child_by_field_name("body") {
            // `union { int a; float b; };`: the members belong to the enclosing class
            Some(body) if anonymous_member => walk_children(cx, body, scope, depth + 1),
            _ => walk(cx, t, scope, depth + 1),
        }
    }
    let (base, _) = decl_base(cx, n);
    let kind = if scope_kind(cx, scope) == ScopeKind::Class { BindKind::Field } else { BindKind::Local };
    for d in declarators {
        if let Declared::Var(nm, fn_ptr) = classify(cx, d) {
            bind(cx, scope, nm, kind, if fn_ptr { "" } else { &base }, "");
        }
    }
    if let Some(v) = n.child_by_field_name("default_value") {
        walk(cx, v, scope, depth + 1);
    }
}

/// `typedef a::Foo X;`, `typedef struct node node_t;`, `typedef struct { ... } anon_t;`.
fn type_definition(cx: &mut Cx, n: Node, scope: u32, depth: u32) {
    if let Some(t) = n.child_by_field_name("type") {
        walk(cx, t, scope, depth + 1);
    }
    let (base, _) = decl_base(cx, n);
    if base.is_empty() {
        return;
    }
    let mut c = n.walk();
    let declarators: Vec<Node> = n.children_by_field_name("declarator", &mut c).collect();
    for d in declarators {
        if let Declared::Var(nm, false) = classify(cx, d) {
            bind(cx, scope, nm, BindKind::Local, &base, "");
            owned_fact(cx, scope, n, nm, BindKind::TypeAlias, &base);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// types and initialisers

/// Qualified head of a named type: `const a::Foo<int>&` -> `a::Foo`, `struct node` -> `node`;
/// empty for primitives, `auto`, `decltype(...)` and anything unnamed.
///
/// Wrapper chains (`typename typename ... T`) are unwrapped iteratively: this runs outside the
/// depth-bounded walk (from parameters, declarations and call arguments), so it must not recurse.
fn type_name(cx: &Cx, t: Node) -> String {
    let mut t = t;
    for _ in 0..MAX_CHAIN {
        match t.kind() {
            "type_identifier" | "identifier" | "namespace_identifier" => return cx.text(t).to_owned(),
            "qualified_identifier" | "template_type" | "nested_namespace_specifier" => return path_text(cx.text(t)),
            // `typename T::type`, a template argument / alias target `const Foo*`
            "dependent_type" | "type_descriptor" => {
                match t.child_by_field_name("type").or_else(|| kids(t).find(|c| c.is_named() && c.kind() != "type_qualifier")) {
                    Some(inner) => t = inner,
                    None => return String::new(),
                }
            }
            "struct_specifier" | "class_specifier" | "union_specifier" | "enum_specifier" => {
                return t.child_by_field_name("name").map(|nm| path_text(cx.text(nm))).unwrap_or_default();
            }
            // primitive_type, sized_type_specifier, placeholder_type_specifier, decltype, macro types
            _ => return String::new(),
        }
    }
    // an absurdly long wrapper chain: no usable type
    String::new()
}

/// `std :: vector < Foo > :: iterator` -> `std::vector::iterator`: drops template arguments,
/// `template` / `typename` keywords, whitespace and a leading global `::`.
fn path_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut angle = 0u32;
    for ch in s.chars() {
        match ch {
            '<' => angle += 1,
            '>' => angle = angle.saturating_sub(1),
            _ if angle > 0 => {}
            c if c.is_whitespace() => out.push(' '),
            c => out.push(c),
        }
    }
    let joined: String = out.split_whitespace().filter(|w| !matches!(*w, "template" | "typename")).collect();
    joined.trim_start_matches("::").to_owned()
}

/// Type information of an `auto` initialiser: `(decl_type, init_ctor)`.
fn value_info(cx: &Cx, v: Node) -> (String, String) {
    let mut v = v;
    for _ in 0..MAX_CHAIN {
        match v.kind() {
            "parenthesized_expression" => match v.named_child(0) {
                Some(i) => v = i,
                None => break,
            },
            "new_expression" | "compound_literal_expression" => {
                return (v.child_by_field_name("type").map(|t| type_name(cx, t)).unwrap_or_default(), String::new());
            }
            "call_expression" => return call_info(cx, v),
            // `Foo x{1}` in `auto` form is `auto x = Foo{1}`; plain values carry no type here
            _ => break,
        }
    }
    (String::new(), String::new())
}

fn call_info(cx: &Cx, call: Node) -> (String, String) {
    let Some(f) = call.child_by_field_name("function") else { return Default::default() };
    // `std::make_unique<Foo>(..)`, `static_cast<Foo*>(..)`: the template argument is the type
    if let Some(tf) = template_callee(f) {
        let name = tf.child_by_field_name("name").map(|n| cx.text(n)).unwrap_or("");
        let scope = if f.kind() == "qualified_identifier" { path_text(cx.text(f)) } else { String::new() };
        let std_ok = scope.is_empty() || scope == format!("std::{name}");
        if std_ok && matches!(name, "make_unique" | "make_shared" | "static_cast" | "dynamic_cast" | "reinterpret_cast" | "const_cast") {
            let arg = tf.child_by_field_name("arguments").and_then(|a| kids(a).find(|c| c.is_named()));
            return (arg.map(|a| type_name(cx, a)).unwrap_or_default(), String::new());
        }
    }
    match f.kind() {
        // `Foo(1)` (a constructor by naming convention) vs `make_foo()` / `CreateFoo()`
        "identifier" | "template_function" => {
            let name = path_text(cx.text(f));
            match callee_shape(&name) {
                Callee::Ctor => (name, String::new()),
                Callee::Call => (String::new(), format!("{name}()")),
                Callee::Macro => Default::default(),
            }
        }
        // `ns::Foo(1)` is a constructor, `Foo::create()` a static factory
        "qualified_identifier" => {
            let path = path_text(cx.text(f));
            let mut segs = path.rsplit("::");
            let (last, owner) = (segs.next().unwrap_or(""), segs.next().unwrap_or(""));
            if path.is_empty() {
                Default::default()
            } else if callee_shape(last) == Callee::Ctor && !owner.is_empty() && !starts_upper(owner) {
                (path, String::new())
            } else {
                (String::new(), format!("{path}()"))
            }
        }
        // `other.build()`, `other->build()`, `a.b().c()`
        "field_expression" => (String::new(), member_path(cx, f).map(|p| format!("{p}()")).unwrap_or_default()),
        _ => Default::default(),
    }
}

fn template_callee(f: Node) -> Option<Node> {
    match f.kind() {
        "template_function" => Some(f),
        "qualified_identifier" => f.child_by_field_name("name").filter(|n| n.kind() == "template_function"),
        _ => None,
    }
}

fn starts_upper(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Callee {
    /// `Foo(..)`: a class constructed by value
    Ctor,
    /// `make_foo(..)`, `CreateFoo(..)`: a function whose return type the resolver follows
    Call,
    /// `MAKE_FOO(..)`: a macro; nothing to say about the value
    Macro,
}

/// C++ has no syntactic difference between `Foo(1)` and `make_foo(1)`: capitalised names are
/// taken as types, except ALL_CAPS macros and CamelCase factory functions (`CreateFoo`,
/// `GetBar`, `MakeUnique`: a leading verb followed by another capitalised word).
fn callee_shape(name: &str) -> Callee {
    if !starts_upper(name) {
        return Callee::Call;
    }
    if is_macro_name(name) {
        return Callee::Macro;
    }
    const VERBS: &[&str] = &[
        "Get", "Create", "Make", "New", "Build", "Find", "Load", "Parse", "Read", "Open", "Alloc", "Allocate", "Init", "Compute", "Lookup", "Fetch",
        "Generate", "Acquire", "Clone", "Copy", "Wrap", "To", "From", "As", "Default",
    ];
    for v in VERBS {
        if let Some(rest) = name.strip_prefix(v) {
            if rest.is_empty() || rest.starts_with(|c: char| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') {
                return Callee::Call;
            }
        }
    }
    Callee::Ctor
}

/// Dotted call path of a member access chain: `this->a.b` -> `this.a.b`, `x.f().g` -> `x.f().g`.
/// `None` for receivers that are not names, members or calls of such.
fn member_path(cx: &Cx, n: Node) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut cur = n;
    for _ in 0..MAX_CHAIN {
        match cur.kind() {
            "field_expression" => {
                let field = cur.child_by_field_name("field").filter(|f| f.kind() == "field_identifier")?;
                parts.push(format!(".{}", cx.text(field)));
                cur = cur.child_by_field_name("argument")?;
            }
            "call_expression" => {
                parts.push("()".into());
                cur = cur.child_by_field_name("function")?;
            }
            "identifier" | "this" => {
                parts.push(cx.text(cur).to_owned());
                return Some(parts.iter().rev().map(String::as_str).collect());
            }
            "qualified_identifier" => {
                parts.push(path_text(cx.text(cur)));
                return Some(parts.iter().rev().map(String::as_str).collect());
            }
            "parenthesized_expression" => cur = cur.named_child(0)?,
            _ => return None,
        }
    }
    None
}
