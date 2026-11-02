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
    // The kernel labels `obj.method = function method() {}` as a Function. Keep
    // evidence of the member assignment so an unknown receiver cannot match an
    // unrelated, plain function with the same name. A member given to an ES5
    // constructor's prototype (`X.prototype.m = ...`, `this.m = ...` in `function X`)
    // records the constructor's name.
    if let Some(parent) = n.parent() {
        let (member, owner) = match parent.kind() {
            "object" if n.kind() == "method_definition" => (n.child_by_field_name("name"), prototype_object_owner(cx, parent)),
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
                (member, owner)
            }
            "pair" if parent.child_by_field_name("value") == Some(n) => {
                (parent.child_by_field_name("key"), parent.parent().and_then(|o| prototype_object_owner(cx, o)))
            }
            _ => (None, None),
        };
        if let Some(member) = member {
            if let Some(row) = cx.row_at(cx.start(n)) {
                cx.facts.bindings.push(BindingFact {
                    scope,
                    name: cx.text(member).trim_matches(['\'', '"']).to_owned(),
                    kind: BindKind::MemberFn,
                    decl_type: owner.unwrap_or_default().to_owned(),
                    init_ctor: String::new(),
                    line: cx.line(n),
                    node_row: row,
                });
            }
        }
    }
    let sc = cx.push_scope(ScopeKind::Function, scope, n);
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
