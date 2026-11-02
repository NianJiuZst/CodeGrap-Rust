//! Facts for TypeScript / TSX / JavaScript / JSX: ES imports and exports, CommonJS `require`,
//! function / class scopes, parameters, locals and class fields with their types.

use super::util::*;
use super::*;

/// Export-fact name marking a `declare module 'name' { ... }` block (`from_spec` is the module's
/// name, `from_name` `script` when the file has no top-level import or export, so that the block
/// declares the module rather than augmenting one). The leading NUL keeps it out of real names.
pub const AMBIENT_MODULE: &str = "\0ambient";

pub fn extract(cx: &mut Cx, root: Node) {
    walk(cx, root, 0);
    // Mutations are relevant only to initializer / factory evidence. Keep the
    // subset needed by resolution, avoiding a permanent fact for every write.
    let mutations: std::collections::HashSet<String> = cx.facts.bindings.iter().filter(|b| b.kind == BindKind::Mutation).map(|b| b.name.clone()).collect();
    let relevant: std::collections::HashSet<String> = cx
        .facts
        .bindings
        .iter()
        .filter(|b| b.init_ctor.ends_with("()") || b.kind == BindKind::CommonJsObject)
        .map(|b| b.name.clone())
        .chain(cx.facts.bindings.iter().filter(|b| b.kind == BindKind::MemberFn && !b.init_ctor.is_empty()).map(|b| b.init_ctor.clone()))
        .chain(cx.facts.imports.iter().map(|i| i.local.clone()))
        .collect();
    cx.facts.bindings.retain(|b| {
        if b.kind == BindKind::Mutation {
            relevant.contains(&b.name) || b.name == "module.exports" || b.name.split_once('.').is_some_and(|(root, _)| relevant.contains(root))
        } else if b.kind == BindKind::FactoryMixin {
            !b.init_ctor.split(',').any(|name| mutations.contains(name))
        } else {
            true
        }
    });
    let script = !kids(root).any(|c| matches!(c.kind(), "import_statement" | "export_statement" | "import_alias"));
    for decl in kids(root).filter(|c| c.kind() == "ambient_declaration") {
        let Some(module) = kids(decl).find(|c| c.kind() == "module") else { continue };
        let (Some(name), Some(_body)) = (module.child_by_field_name("name"), module.child_by_field_name("body")) else { continue };
        if name.kind() != "string" {
            continue;
        }
        let spec = unquote(cx.text(name)).to_owned();
        if spec.is_empty() || spec.contains('*') {
            continue; // a wildcard pattern names no specifier exactly
        }
        cx.facts.exports.push(ExportFact {
            name: AMBIENT_MODULE.into(),
            node_row: NONE,
            from_spec: spec,
            from_name: if script { "script" } else { "module" }.into(),
            star: false,
        });
    }
}

fn walk_children(cx: &mut Cx, n: Node, scope: u32) {
    for ch in kids(n) {
        walk(cx, ch, scope);
    }
}

fn walk(cx: &mut Cx, n: Node, scope: u32) {
    match n.kind() {
        "import_statement" => import_stmt(cx, n, scope),
        "export_statement" => {
            export_stmt(cx, n);
            walk_children(cx, n, scope);
        }
        "assignment_expression" => {
            if scope == 0 {
                commonjs_export(cx, n);
            }
            if let Some(left) = n.child_by_field_name("left") {
                record_mutations(cx, left, scope);
            }
            walk_children(cx, n, scope);
        }
        "augmented_assignment_expression" | "update_expression" => {
            if let Some(left) = n.child_by_field_name("left").or_else(|| n.child_by_field_name("argument")) {
                record_mutations(cx, left, scope);
            }
            walk_children(cx, n, scope);
        }
        "function_declaration" | "generator_function_declaration" => {
            if scope != 0 {
                if let Some(name) = n.child_by_field_name("name") {
                    let nm = cx.text(name).to_owned();
                    cx.add_binding(scope, &nm, BindKind::LocalFn, "", "", 0);
                    let row = cx.row_at(cx.start(n));
                    cx.facts.bindings.last_mut().unwrap().node_row = row.unwrap_or(NONE);
                }
            }
            function(cx, n, scope);
        }
        "function_expression" | "function" | "generator_function" | "arrow_function" | "method_definition" => function(cx, n, scope),
        "class_declaration" | "abstract_class_declaration" | "class" => class(cx, n, scope),
        "lexical_declaration" | "variable_declaration" => var_decl(cx, n, scope),
        "call_expression" => {
            super::args::js_call(cx, n);
            assigned_this_call(cx, n, scope);
            walk_children(cx, n, scope);
        }
        "for_in_statement" => {
            if let Some(l) = n.child_by_field_name("left") {
                bind_pattern(cx, l, scope, BindKind::Local, "", "");
            }
            walk_children(cx, n, scope);
        }
        "catch_clause" => {
            if let Some(p) = n.child_by_field_name("parameter") {
                bind_pattern(cx, p, scope, BindKind::Local, "", "");
            }
            walk_children(cx, n, scope);
        }
        _ => walk_children(cx, n, scope),
    }
}

fn names_in_pattern(cx: &Cx, n: Node, out: &mut Vec<String>) {
    match n.kind() {
        "identifier" | "shorthand_property_identifier_pattern" => out.push(cx.text(n).to_owned()),
        "assignment_pattern" | "object_assignment_pattern" => {
            if let Some(l) = n.child_by_field_name("left") {
                names_in_pattern(cx, l, out);
            }
        }
        "pair_pattern" => {
            if let Some(v) = n.child_by_field_name("value") {
                names_in_pattern(cx, v, out);
            }
        }
        "object_pattern" | "array_pattern" | "rest_pattern" | "parenthesized_expression" => {
            for ch in kids(n) {
                names_in_pattern(cx, ch, out);
            }
        }
        _ => {}
    }
}

fn record_mutations(cx: &mut Cx, target: Node, scope: u32) {
    let mut names = Vec::new();
    names_in_pattern(cx, target, &mut names);
    if target.kind() == "member_expression" {
        if let (Some(object), Some(property)) = (target.child_by_field_name("object"), target.child_by_field_name("property")) {
            if object.kind() == "identifier" {
                names.push(format!("{}.{}", cx.text(object), cx.text(property)));
            }
        }
    }
    for name in names {
        cx.add_binding(scope, &name, BindKind::Mutation, "", "", cx.line(target));
    }
}

fn assigned_this_call(cx: &mut Cx, call: Node, scope: u32) {
    let Some(callee) = call.child_by_field_name("function").filter(|f| f.kind() == "member_expression") else { return };
    let (Some(receiver), Some(method)) = (callee.child_by_field_name("object"), callee.child_by_field_name("property")) else { return };
    if receiver.kind() != "this" {
        return;
    }
    // Only the nearest function supplies the receiver. In particular, a
    // nested ordinary callback must not inherit its outer method's `this`.
    let mut ancestor = call.parent();
    let function = loop {
        let Some(n) = ancestor else { return };
        if matches!(n.kind(), "function_declaration" | "function_expression" | "function" | "arrow_function" | "method_definition") {
            break n;
        }
        ancestor = n.parent();
    };
    if !matches!(function.kind(), "function_expression" | "function") {
        return;
    }
    let Some(assignment) = function.parent().filter(|p| p.kind() == "assignment_expression" && p.child_by_field_name("right") == Some(function)) else {
        return;
    };
    let Some(object) = assignment
        .child_by_field_name("left")
        .filter(|l| l.kind() == "member_expression")
        .and_then(|l| l.child_by_field_name("object"))
        .filter(|o| o.kind() == "identifier")
    else {
        return;
    };
    let Some(row) = cx.row_at(cx.start(function)) else { return };
    let (line, col) = cg_core::text::unpack_pos(cx.start(call));
    cx.facts.bindings.push(BindingFact {
        scope,
        name: cx.text(method).to_owned(),
        kind: BindKind::AssignedThisCall,
        decl_type: cx.text(object).to_owned(),
        init_ctor: col.to_string(),
        line,
        node_row: row,
    });
}

fn bind_pattern(cx: &mut Cx, pat: Node, scope: u32, kind: BindKind, decl_type: &str, init: &str) {
    let mut names = Vec::new();
    names_in_pattern(cx, pat, &mut names);
    let single = names.len() == 1 && pat.kind() == "identifier";
    let line = cx.line(pat);
    for nm in names {
        if single {
            cx.add_binding(scope, &nm, kind, decl_type, init, line);
        } else {
            cx.add_binding(scope, &nm, kind, "", "", line);
        }
    }
}

/// Type name from a type node (`Foo`, `Foo<T>`, `ns.Foo`, `Foo | null`); empty for builtins and
/// structural types.
fn ts_type_name(cx: &Cx, n: Node) -> String {
    match n.kind() {
        "type_annotation" => n.named_child(0).map(|c| ts_type_name(cx, c)).unwrap_or_default(),
        "type_identifier" | "identifier" => cx.text(n).to_owned(),
        // primitives type arguments for overload selection (`args`); `any` / `unknown` say nothing
        "predefined_type" if matches!(cx.text(n), "string" | "number" | "boolean" | "bigint" | "symbol") => cx.text(n).to_owned(),
        "nested_type_identifier" | "member_expression" => cx.text(n).to_owned(),
        "generic_type" => n.child_by_field_name("name").map(|c| ts_type_name(cx, c)).unwrap_or_default(),
        "parenthesized_type" | "readonly_type" | "required_parameter" => n.named_child(0).map(|c| ts_type_name(cx, c)).unwrap_or_default(),
        "union_type" | "intersection_type" => {
            for ch in kids(n) {
                if !ch.is_named() {
                    continue;
                }
                let t = ts_type_name(cx, ch);
                if !t.is_empty() && !matches!(t.as_str(), "null" | "undefined" | "string" | "number" | "boolean" | "bigint" | "symbol") {
                    return t;
                }
            }
            String::new()
        }
        _ => String::new(),
    }
}

/// (declared type, initialiser) of a value expression: `new Foo()` -> ("", "Foo"),
/// `foo()` -> ("", "foo()"), `x as Foo` -> ("Foo", "").
fn value_info(cx: &Cx, v: Node) -> (String, String) {
    value_info_inner(cx, v, false)
}

fn value_info_inner(cx: &Cx, v: Node, in_satisfies: bool) -> (String, String) {
    match v.kind() {
        // Assignment expressions evaluate to their right-hand value, including
        // `const app = module.exports = express()`.
        "assignment_expression" => v.child_by_field_name("right").map(|c| value_info_inner(cx, c, in_satisfies)).unwrap_or_default(),
        "new_expression" => {
            let c = v.child_by_field_name("constructor").map(|c| cx.text(c).to_owned()).unwrap_or_default();
            (String::new(), c)
        }
        "await_expression" | "parenthesized_expression" | "non_null_expression" => {
            v.named_child(0).map(|c| value_info_inner(cx, c, in_satisfies)).unwrap_or_default()
        }
        // Preserve the operand's evidence, but mark it so an unknown receiver
        // cannot be guessed from an unrelated global method.
        "satisfies_expression" => {
            let (ty, init) = v.named_child(0).map(|c| value_info_inner(cx, c, true)).unwrap_or_default();
            if ty.is_empty() && !init.starts_with(JS_SATISFIES_VALUE) {
                (ty, format!("{JS_SATISFIES_VALUE}{init}"))
            } else {
                (ty, init)
            }
        }
        "identifier" if in_satisfies => (String::new(), format!("{JS_BINDING_ALIAS}{}", cx.text(v))),
        "string" | "template_string" if in_satisfies => ("string".into(), String::new()),
        "number" if in_satisfies => (if cx.text(v).ends_with('n') { "bigint" } else { "number" }.into(), String::new()),
        "true" | "false" if in_satisfies => ("boolean".into(), String::new()),
        "unary_expression" if in_satisfies => match v.child_by_field_name("argument") {
            Some(n) if n.kind() == "number" && matches!(cx.text(v).as_bytes().first(), Some(b'-' | b'+')) => {
                (if cx.text(n).ends_with('n') { "bigint" } else { "number" }.into(), String::new())
            }
            _ => Default::default(),
        },
        "as_expression" => {
            let t = v.named_child(1).map(|c| ts_type_name(cx, c)).unwrap_or_default();
            (t, String::new())
        }
        "call_expression" => match v.child_by_field_name("function") {
            Some(f) if matches!(f.kind(), "identifier" | "member_expression") => (String::new(), format!("{}()", cx.text(f))),
            _ => Default::default(),
        },
        _ => Default::default(),
    }
}

fn import_stmt(cx: &mut Cx, n: Node, scope: u32) {
    let Some(src) = n.child_by_field_name("source") else { return };
    let spec = unquote(cx.text(src)).to_owned();
    let line = cx.line(n);
    let stmt_type_only = kids(n).any(|c| c.kind() == "type");
    let mut any = false;
    for clause in kids(n) {
        match clause.kind() {
            "import_clause" => {
                for part in kids(clause) {
                    match part.kind() {
                        "identifier" => {
                            any = true;
                            let local = cx.text(part).to_owned();
                            cx.add_import(ImportFact {
                                local,
                                imported: "default".into(),
                                spec: spec.clone(),
                                kind: ImportKind::Default,
                                type_only: stmt_type_only,
                                scope,
                                line,
                            });
                        }
                        "named_imports" => {
                            for sp in kids(part) {
                                if sp.kind() != "import_specifier" {
                                    continue;
                                }
                                any = true;
                                let name = sp.child_by_field_name("name").map(|c| unquote(cx.text(c)).to_owned()).unwrap_or_default();
                                let alias = sp.child_by_field_name("alias").map(|c| cx.text(c).to_owned());
                                let t_only = stmt_type_only || kids(sp).any(|c| c.kind() == "type");
                                cx.add_import(ImportFact {
                                    local: alias.unwrap_or_else(|| name.clone()),
                                    imported: name,
                                    spec: spec.clone(),
                                    kind: ImportKind::Named,
                                    type_only: t_only,
                                    scope,
                                    line,
                                });
                            }
                        }
                        "namespace_import" => {
                            any = true;
                            let local = kids(part).find(|c| c.kind() == "identifier").map(|c| cx.text(c).to_owned()).unwrap_or_default();
                            cx.add_import(ImportFact {
                                local,
                                imported: "*".into(),
                                spec: spec.clone(),
                                kind: ImportKind::Namespace,
                                type_only: stmt_type_only,
                                scope,
                                line,
                            });
                        }
                        _ => {}
                    }
                }
            }
            "import_require_clause" => {
                any = true;
                let local = kids(clause).find(|c| c.kind() == "identifier").map(|c| cx.text(c).to_owned()).unwrap_or_default();
                cx.add_import(ImportFact { local, imported: "*".into(), spec: spec.clone(), kind: ImportKind::Require, type_only: false, scope, line });
            }
            _ => {}
        }
    }
    if !any {
        cx.add_import(ImportFact { local: String::new(), imported: "*".into(), spec, kind: ImportKind::SideEffect, type_only: false, scope, line });
    }
}

fn export_stmt(cx: &mut Cx, n: Node) {
    let source = n.child_by_field_name("source").map(|s| unquote(cx.text(s)).to_owned());
    if let Some(spec) = &source {
        // `export ... from 'x'` depends on module `x`; recorded like a side-effect import so the
        // module reference resolves to the target file.
        let line = cx.line(n);
        cx.add_import(ImportFact {
            local: String::new(),
            imported: "*".into(),
            spec: spec.clone(),
            kind: ImportKind::SideEffect,
            type_only: false,
            scope: 0,
            line,
        });
    }
    let is_default = kids(n).any(|c| c.kind() == "default");
    let mut had_clause = false;
    for ch in kids(n) {
        match ch.kind() {
            "export_clause" => {
                had_clause = true;
                for sp in kids(ch) {
                    if sp.kind() != "export_specifier" {
                        continue;
                    }
                    let name = sp.child_by_field_name("name").map(|c| unquote(cx.text(c)).to_owned()).unwrap_or_default();
                    let alias = sp.child_by_field_name("alias").map(|c| unquote(cx.text(c)).to_owned());
                    cx.facts.exports.push(ExportFact {
                        name: alias.unwrap_or_else(|| name.clone()),
                        node_row: NONE,
                        from_spec: source.clone().unwrap_or_default(),
                        from_name: name,
                        star: false,
                    });
                }
            }
            "namespace_export" => {
                had_clause = true;
                let alias = kids(ch).find(|c| c.kind() == "identifier").map(|c| cx.text(c).to_owned()).unwrap_or_default();
                cx.facts.exports.push(ExportFact {
                    name: alias,
                    node_row: NONE,
                    from_spec: source.clone().unwrap_or_default(),
                    from_name: "*".into(),
                    star: false,
                });
            }
            "*" if source.is_some() && !had_clause => {
                cx.facts.exports.push(ExportFact {
                    name: "*".into(),
                    node_row: NONE,
                    from_spec: source.clone().unwrap_or_default(),
                    from_name: "*".into(),
                    star: true,
                });
            }
            _ => {}
        }
    }
    if is_default {
        // `export default Foo;` / `export default function foo() {}` / `export default class {}`
        if let Some(decl) = n.child_by_field_name("declaration") {
            let pos = cx.start(decl);
            let row = cx.row_at(pos).unwrap_or(NONE);
            cx.facts.exports.push(ExportFact { name: "default".into(), node_row: row, from_spec: String::new(), from_name: String::new(), star: false });
        } else if let Some(v) = n.child_by_field_name("value") {
            if v.kind() == "identifier" {
                let from_name = cx.text(v).to_owned();
                cx.facts.exports.push(ExportFact { name: "default".into(), node_row: NONE, from_spec: String::new(), from_name, star: false });
            }
        }
    }
}

fn commonjs_export(cx: &mut Cx, n: Node) {
    let Some(left) = n.child_by_field_name("left") else { return };
    let target = cx.text(left);
    let root = target.split('.').next().unwrap_or(target);
    if cx.lookup_binding(0, root).is_some() {
        return; // A user variable named module/exports shadows CommonJS globals.
    }
    let name = if target == "module.exports" {
        COMMONJS_MODULE
    } else if let Some(name) = target.strip_prefix("module.exports.").or_else(|| target.strip_prefix("exports.")) {
        if name.is_empty() || name.contains(['.', '[', ']']) {
            return;
        }
        name
    } else {
        return;
    };
    let Some(value) = n.child_by_field_name("right") else { return };
    commonjs_value(cx, name, value);
}

fn commonjs_value(cx: &mut Cx, name: &str, value: Node) {
    if let Some(spec) = require_target(cx, value) {
        let line = cx.line(value);
        cx.add_import(ImportFact {
            local: String::new(),
            imported: "*".into(),
            spec: spec.clone(),
            kind: ImportKind::SideEffect,
            type_only: false,
            scope: 0,
            line,
        });
        if name == COMMONJS_MODULE {
            cx.facts.exports.push(ExportFact { name: "*".into(), node_row: NONE, from_spec: spec.clone(), from_name: "*".into(), star: true });
            cx.facts.exports.push(ExportFact { name: "default".into(), node_row: NONE, from_spec: spec.clone(), from_name: "default".into(), star: false });
        }
        cx.facts.exports.push(ExportFact { name: name.to_owned(), node_row: NONE, from_spec: spec, from_name: COMMONJS_MODULE.into(), star: false });
        return;
    }
    let (node_row, from_spec, from_name) = match value.kind() {
        "identifier" | "member_expression" => {
            let text = cx.text(value);
            let (root, member) = text.split_once('.').map_or((text, None), |(a, b)| (a, Some(b)));
            if let Some(imp) = cx.facts.imports.iter().find(|i| i.scope == 0 && i.local == root) {
                let imported = member.unwrap_or(if imp.imported == "*" { COMMONJS_MODULE } else { &imp.imported });
                (NONE, imp.spec.clone(), imported.to_owned())
            } else if member.is_none() {
                (NONE, String::new(), text.to_owned())
            } else {
                return;
            }
        }
        "function_expression" | "function" | "generator_function" | "arrow_function" | "class" => {
            (cx.row_at(cx.start(value)).unwrap_or(NONE), String::new(), String::new())
        }
        "object" if name == COMMONJS_MODULE => {
            cx.facts.exports.push(ExportFact { name: name.to_owned(), node_row: NONE, from_spec: String::new(), from_name: "*".into(), star: false });
            for prop in kids(value) {
                if prop.kind() == "pair" {
                    if let (Some(key), Some(val)) = (prop.child_by_field_name("key"), prop.child_by_field_name("value")) {
                        let key = unquote(cx.text(key)).to_owned();
                        commonjs_value(cx, &key, val);
                    }
                } else if prop.kind() == "shorthand_property_identifier" {
                    commonjs_value(cx, cx.text(prop), prop);
                }
            }
            return;
        }
        "shorthand_property_identifier" => (NONE, String::new(), cx.text(value).to_owned()),
        _ => return,
    };
    cx.facts.exports.push(ExportFact { name: name.to_owned(), node_row, from_spec, from_name, star: false });
}

/// Only nominal return annotations can currently identify a receiver. In
/// particular, do not reuse ts_type_name's union selection or generic erasure.
fn return_type_name(cx: &Cx, n: Node) -> String {
    match n.kind() {
        "type_annotation" | "parenthesized_type" => n.named_child(0).map(|c| return_type_name(cx, c)).unwrap_or_default(),
        "type_identifier" | "nested_type_identifier" => cx.text(n).to_owned(),
        _ => String::new(),
    }
}

fn return_annotation(cx: &mut Cx, n: Node, scope: u32) {
    let (Some(annotation), Some(row)) = (n.child_by_field_name("return_type"), cx.row_at(cx.start(n))) else { return };
    let mut ty = return_type_name(cx, annotation);
    // Calling an async/generator function produces a wrapper, regardless of a
    // malformed annotation. Generic parameters must not resolve to a project
    // class with the same name, including parameters on an enclosing class.
    if kids(n).any(|c| matches!(c.kind(), "async" | "*")) {
        ty.clear();
    }
    let head = ty.split('.').next().unwrap_or("");
    let mut ancestor = Some(n);
    while let Some(node) = ancestor {
        if let Some(params) = node.child_by_field_name("type_parameters") {
            if kids(params).filter_map(|p| p.child_by_field_name("name")).any(|name| cx.text(name) == head) {
                ty.clear();
                break;
            }
        }
        ancestor = node.parent();
    }
    cx.facts.bindings.push(BindingFact {
        scope,
        name: cx.rows.node(row as usize).name().to_owned(),
        kind: BindKind::ReturnType,
        // Keep an empty fact for unsupported annotations: the resolver must
        // distinguish explicit but unknown returns from absent evidence.
        decl_type: ty,
        init_ctor: String::new(),
        line: cx.line(n),
        node_row: row,
    });
}

/// `X.prototype` -> `X`.
fn prototype_of<'a>(cx: &Cx<'a>, n: Node) -> Option<&'a str> {
    if n.kind() != "member_expression" || n.child_by_field_name("property").map(|p| cx.text(p)) != Some("prototype") {
        return None;
    }
    let object = n.child_by_field_name("object").filter(|o| o.kind() == "identifier")?;
    Some(cx.text(object))
}

/// The ES5 constructor an object literal's members are given to: `X.prototype = { ... }` or
/// `Object.assign(X.prototype, { ... })`.
fn prototype_object_owner<'a>(cx: &Cx<'a>, object: Node) -> Option<&'a str> {
    let parent = object.parent()?;
    match parent.kind() {
        "assignment_expression" if parent.child_by_field_name("right") == Some(object) => prototype_of(cx, parent.child_by_field_name("left")?),
        "arguments" => {
            let call = parent.parent().filter(|c| c.kind() == "call_expression")?;
            if call.child_by_field_name("function").map(|f| cx.text(f)) != Some("Object.assign") {
                return None;
            }
            prototype_of(cx, parent.named_child(0)?)
        }
        _ => None,
    }
}

/// The function declaration a node sits in, when its name reads as a constructor's (`Foo`).
fn enclosing_constructor<'a>(cx: &Cx<'a>, n: Node) -> Option<&'a str> {
    let mut cur = n.parent();
    while let Some(p) = cur {
        if matches!(p.kind(), "function_declaration" | "function_expression" | "function" | "arrow_function" | "method_definition" | "class_body") {
            let name = (p.kind() == "function_declaration").then(|| p.child_by_field_name("name")).flatten().map(|nm| cx.text(nm))?;
            return name.starts_with(|c: char| c.is_ascii_uppercase()).then_some(name);
        }
        cur = p.parent();
    }
    None
}

fn function(cx: &mut Cx, n: Node, scope: u32) {
    if scope == 0 && n.kind() == "function_declaration" {
        factory_mixin(cx, n, scope);
    }
    // The kernel labels `obj.method = function method() {}` as a Function. Keep
    // evidence of the member assignment so an unknown receiver cannot match an
    // unrelated, plain function with the same name. A member given to an ES5
    // constructor's prototype (`X.prototype.m = ...`, `this.m = ...` in `function X`)
    // records the constructor's name.
    if let Some(parent) = n.parent() {
        let (member, owner, object_name) = match parent.kind() {
            "object" if n.kind() == "method_definition" => (n.child_by_field_name("name"), prototype_object_owner(cx, parent), None),
            "assignment_expression" if parent.child_by_field_name("right") == Some(n) => {
                let left = parent.child_by_field_name("left").filter(|left| matches!(left.kind(), "member_expression" | "subscript_expression"));
                let member = left.and_then(|left| {
                    left.child_by_field_name("property").or_else(|| left.child_by_field_name("index").filter(|index| index.kind() == "string"))
                });
                let object = left.and_then(|left| left.child_by_field_name("object"));
                let owner = match object {
                    Some(o) if o.kind() == "this" => enclosing_constructor(cx, parent),
                    Some(o) => prototype_of(cx, o),
                    None => None,
                };
                (member, owner, object.filter(|o| o.kind() == "identifier").map(|o| cx.text(o)))
            }
            "pair" if parent.child_by_field_name("value") == Some(n) => {
                (parent.child_by_field_name("key"), parent.parent().and_then(|o| prototype_object_owner(cx, o)), None)
            }
            _ => (None, None, None),
        };
        if let Some(member) = member {
            if let Some(row) = cx.row_at(cx.start(n)) {
                cx.facts.bindings.push(BindingFact {
                    scope,
                    name: cx.text(member).trim_matches(['\'', '"']).to_owned(),
                    kind: BindKind::MemberFn,
                    decl_type: owner.unwrap_or_default().to_owned(),
                    init_ctor: object_name.unwrap_or_default().to_owned(),
                    line: cx.line(n),
                    node_row: row,
                });
            }
        }
    }
    let sc = cx.push_scope(ScopeKind::Function, scope, n);
    return_annotation(cx, n, sc);
    let line = cx.line(n);
    if let Some(p) = n.child_by_field_name("parameter") {
        bind_pattern(cx, p, sc, BindKind::Param, "", ""); // single-identifier arrow parameter
    }
    if let Some(params) = n.child_by_field_name("parameters") {
        for p in kids(params) {
            match p.kind() {
                "required_parameter" | "optional_parameter" => {
                    let ty = p.child_by_field_name("type").map(|t| ts_type_name(cx, t)).unwrap_or_default();
                    if let Some(pat) = p.child_by_field_name("pattern") {
                        bind_pattern(cx, pat, sc, BindKind::Param, &ty, "");
                    }
                }
                "identifier" | "assignment_pattern" | "rest_pattern" | "object_pattern" | "array_pattern" => {
                    bind_pattern(cx, p, sc, BindKind::Param, "", "");
                }
                _ => {}
            }
        }
    }
    let _ = line;
    for ch in kids(n) {
        if matches!(ch.kind(), "formal_parameters") {
            // default values may contain closures
            for p in kids(ch) {
                walk(cx, p, sc);
            }
        } else {
            walk(cx, ch, sc);
        }
    }
}

/// Record the narrow `merge-descriptors` factory pattern used by Express: a local
/// function/object is mixed with an imported module and then returned. We do not
/// infer anything from an arbitrary function called `mixin` or from nested calls.
fn factory_mixin(cx: &mut Cx, n: Node, scope: u32) {
    let (Some(name), Some(body)) = (n.child_by_field_name("name"), n.child_by_field_name("body")) else { return };
    if n.child_by_field_name("parameters").is_some_and(|p| kids(p).any(|c| c.is_named())) {
        return;
    }
    let statements: Vec<_> = kids(body).filter(|s| s.is_named()).collect();
    // An early exit or loop makes a direct mixin conditional on control flow.
    if statements.iter().any(|s| {
        matches!(
            s.kind(),
            "if_statement"
                | "switch_statement"
                | "try_statement"
                | "for_statement"
                | "for_in_statement"
                | "while_statement"
                | "do_statement"
                | "throw_statement"
        )
    }) {
        return;
    }
    let returns: Vec<_> = statements.iter().filter(|s| s.kind() == "return_statement").collect();
    if returns.len() != 1 {
        return;
    }
    let Some(value) = kids(*returns[0]).find(|c| c.kind() == "identifier") else { return };
    let returned = cx.text(value);
    let mut locals = Vec::new();
    let mut declared_object = None;
    for s in &statements {
        if matches!(s.kind(), "lexical_declaration" | "variable_declaration") {
            for d in kids(*s).filter(|d| d.kind() == "variable_declarator") {
                if let Some(id) = d.child_by_field_name("name").filter(|x| x.kind() == "identifier") {
                    let local = cx.text(id);
                    locals.push(local);
                    if local == returned
                        && d.child_by_field_name("value").is_some_and(|v| matches!(v.kind(), "function_expression" | "function" | "arrow_function" | "object"))
                    {
                        declared_object = Some(d.start_byte());
                    }
                }
            }
        }
    }
    let Some(declared_at) = declared_object else { return };
    for s in &statements {
        if s.kind() != "expression_statement" || s.start_byte() <= declared_at || s.start_byte() >= returns[0].start_byte() {
            continue;
        }
        let Some(call) = kids(*s).find(|c| c.kind() == "call_expression") else { continue };
        let (Some(func), Some(args)) = (call.child_by_field_name("function"), call.child_by_field_name("arguments")) else { continue };
        if func.kind() != "identifier" {
            continue;
        }
        let callee = cx.text(func);
        if locals.contains(&callee) || !cx.facts.imports.iter().any(|i| i.scope == 0 && i.local == callee && i.spec == "merge-descriptors") {
            continue;
        }
        let args: Vec<_> = kids(args).filter(|c| c.is_named()).collect();
        if args.len() < 2 || args[0].kind() != "identifier" || cx.text(args[0]) != returned || args[1].kind() != "identifier" {
            continue;
        }
        let source = cx.text(args[1]);
        if locals.contains(&source) {
            continue;
        }
        let reassigned = statements.iter().filter(|s| s.kind() == "expression_statement").any(|s| {
            kids(*s).any(|c| {
                c.kind() == "assignment_expression"
                    && c.child_by_field_name("left").is_some_and(|left| left.kind() == "identifier" && [returned, callee, source].contains(&cx.text(left)))
            })
        });
        if reassigned {
            continue;
        }
        let Some(spec) = cx.facts.imports.iter().find(|i| i.scope == 0 && i.local == source).map(|i| i.spec.clone()) else { continue };
        let Some(row) = cx.row_at(cx.start(n)) else { continue };
        cx.facts.bindings.push(BindingFact {
            scope,
            name: cx.text(name).to_owned(),
            kind: BindKind::FactoryMixin,
            decl_type: spec,
            init_ctor: format!("{callee},{source},{returned}"),
            line: cx.line(n),
            node_row: row,
        });
    }
}

fn class(cx: &mut Cx, n: Node, scope: u32) {
    let sc = cx.push_scope(ScopeKind::Class, scope, n);
    let Some(body) = n.child_by_field_name("body") else {
        walk_children(cx, n, scope);
        return;
    };
    for m in kids(body) {
        match m.kind() {
            "public_field_definition" | "field_definition" | "property_signature" => {
                let name = m.child_by_field_name("name").or_else(|| m.child_by_field_name("property"));
                let ty = m.child_by_field_name("type").map(|t| ts_type_name(cx, t)).unwrap_or_default();
                let (vt, init) = m.child_by_field_name("value").map(|v| value_info(cx, v)).unwrap_or_default();
                if let Some(name) = name {
                    let nm = cx.text(name).trim_start_matches('#').to_owned();
                    let line = cx.line(m);
                    cx.add_binding(sc, &nm, BindKind::Field, if ty.is_empty() { &vt } else { &ty }, &init, line);
                }
                if let Some(v) = m.child_by_field_name("value") {
                    walk(cx, v, sc);
                }
            }
            "method_definition" => {
                let is_ctor = m.child_by_field_name("name").map(|c| cx.text(c) == "constructor").unwrap_or(false);
                if is_ctor {
                    if let Some(params) = m.child_by_field_name("parameters") {
                        for p in kids(params) {
                            let has_mod = kids(p).any(|c| matches!(c.kind(), "accessibility_modifier" | "readonly"));
                            if has_mod && matches!(p.kind(), "required_parameter" | "optional_parameter") {
                                let ty = p.child_by_field_name("type").map(|t| ts_type_name(cx, t)).unwrap_or_default();
                                if let Some(pat) = p.child_by_field_name("pattern") {
                                    let nm = cx.text(pat).to_owned();
                                    let line = cx.line(p);
                                    cx.add_binding(sc, &nm, BindKind::Field, &ty, "", line);
                                }
                            }
                        }
                    }
                }
                function(cx, m, sc);
            }
            _ => walk(cx, m, sc),
        }
    }
}

fn require_target(cx: &Cx, v: Node) -> Option<String> {
    let v = if v.kind() == "await_expression" { v.named_child(0)? } else { v };
    if v.kind() != "call_expression" {
        return None;
    }
    let f = v.child_by_field_name("function")?;
    if f.kind() != "identifier" || cx.text(f) != "require" {
        return None;
    }
    let args = v.child_by_field_name("arguments")?;
    let first = args.named_child(0)?;
    if first.kind() != "string" && first.kind() != "template_string" {
        return None;
    }
    Some(unquote(cx.text(first)).to_owned())
}

fn var_decl(cx: &mut Cx, n: Node, scope: u32) {
    for d in kids(n) {
        if d.kind() != "variable_declarator" {
            continue;
        }
        let Some(name) = d.child_by_field_name("name") else { continue };
        let ty = d.child_by_field_name("type").map(|t| ts_type_name(cx, t)).unwrap_or_default();
        let value = d.child_by_field_name("value");
        if let Some(v) = value {
            // `var app = exports = module.exports = {}` ties subsequent
            // `app.method = ...` assignments to the module's exported object.
            if scope == 0 && name.kind() == "identifier" && cx.lookup_binding(0, "module").is_none() {
                let mut cur = v;
                let mut module_export = false;
                while cur.kind() == "assignment_expression" {
                    module_export |= cur.child_by_field_name("left").is_some_and(|left| cx.text(left) == "module.exports");
                    let Some(right) = cur.child_by_field_name("right") else { break };
                    cur = right;
                }
                if module_export && cur.kind() == "object" {
                    let local = cx.text(name).to_owned();
                    cx.add_binding(scope, &local, BindKind::CommonJsObject, "", "", cx.line(d));
                }
            }
            if let Some(spec) = require_target(cx, v) {
                let line = cx.line(d);
                match name.kind() {
                    "identifier" => {
                        let local = cx.text(name).to_owned();
                        cx.add_import(ImportFact { local, imported: "*".into(), spec, kind: ImportKind::Require, type_only: false, scope, line });
                    }
                    "object_pattern" => {
                        for pr in kids(name) {
                            let (imported, local) = match pr.kind() {
                                "shorthand_property_identifier_pattern" => (cx.text(pr).to_owned(), cx.text(pr).to_owned()),
                                "pair_pattern" => {
                                    let k = pr.child_by_field_name("key").map(|c| cx.text(c).to_owned()).unwrap_or_default();
                                    let v = pr.child_by_field_name("value").map(|c| cx.text(c).to_owned()).unwrap_or_default();
                                    (k, v)
                                }
                                _ => continue,
                            };
                            cx.add_import(ImportFact { local, imported, spec: spec.clone(), kind: ImportKind::Require, type_only: false, scope, line });
                        }
                    }
                    _ => {}
                }
                continue;
            }
            let is_fn = matches!(v.kind(), "arrow_function" | "function_expression" | "function" | "generator_function");
            let constant = kids(n).next().is_some_and(|k| k.kind() == "const");
            if is_fn && name.kind() == "identifier" {
                let nm = cx.text(name).to_owned();
                let line = cx.line(d);
                cx.add_binding(scope, &nm, BindKind::LocalFn, "", "", line);
                let row = cx.row_at(cx.start(v));
                cx.facts.bindings.last_mut().unwrap().node_row = row.unwrap_or(NONE);
            } else if constant && name.kind() == "identifier" && v.kind() == "identifier" && ty.is_empty() && cx.text(v) != cx.text(name) {
                // `const local = other`: the name is the other name
                let (nm, other) = (cx.text(name).to_owned(), cx.text(v).to_owned());
                let line = cx.line(d);
                cx.add_binding(scope, &nm, BindKind::Alias, "", &other, line);
            } else {
                let (vt, init) = value_info(cx, v);
                let dt = if ty.is_empty() { vt } else { ty };
                bind_pattern(cx, name, scope, BindKind::Local, &dt, &init);
            }
            walk(cx, v, scope);
        } else {
            bind_pattern(cx, name, scope, BindKind::Local, &ty, "");
        }
    }
}
