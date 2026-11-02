//! Facts for Go: imports (with the default package name guessed from the path), the file's
//! package, function / method / closure scopes, parameters, receivers, short variable
//! declarations, `var` specs and struct fields (embedded fields included).

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

/// Default package name for an import path: last element, skipping `vN`, trimming `go-` / `-go`
/// / `.vN` decorations. Used when the import has no explicit name.
pub fn default_package_name(path: &str) -> String {
    let mut segs: Vec<&str> = path.split('/').collect();
    if segs.len() > 1 {
        let last = segs[segs.len() - 1];
        if last.len() > 1 && last.starts_with('v') && last[1..].bytes().all(|b| b.is_ascii_digit()) {
            segs.pop();
        }
    }
    let last = segs.last().copied().unwrap_or(path);
    let last = last.rsplit_once(".v").map(|(a, _)| a).unwrap_or(last);
    let last = last.strip_prefix("go-").unwrap_or(last);
    let last = last.strip_suffix("-go").unwrap_or(last);
    last.replace(['-', '.'], "_")
}

fn walk(cx: &mut Cx, n: Node, scope: u32) {
    match n.kind() {
        "package_clause" => {
            if let Some(id) = kids(n).find(|c| c.kind() == "package_identifier") {
                let nm = cx.text(id).to_owned();
                cx.add_binding(0, &nm, BindKind::Package, "", "", 1);
            }
        }
        "import_spec" => import_spec(cx, n, scope),
        "function_declaration" | "method_declaration" => function(cx, n, scope),
        "func_literal" => function(cx, n, scope),
        "type_spec" => type_spec(cx, n, scope),
        "type_alias" => {
            // `type A = B` names another type; a defined `type A B` is a type of its own
            if let Some(name) = n.child_by_field_name("name") {
                let name = cx.text(name).to_owned();
                cx.facts.go_types.push(GoTypeFact { name, alias: true, type_params: Vec::new(), func_type: false });
            }
        }
        "composite_literal" => {
            field_literal(cx, n);
            walk_children(cx, n, scope);
        }
        "assignment_statement" => {
            field_assignment(cx, n);
            walk_children(cx, n, scope);
        }
        "short_var_declaration" => {
            let (l, r) = (n.child_by_field_name("left"), n.child_by_field_name("right"));
            if let (Some(l), Some(r)) = (l, r) {
                bind_pairs(cx, l, Some(r), None, scope);
            }
            walk_children(cx, n, scope);
        }
        "var_spec" => {
            let ty = n.child_by_field_name("type").map(|t| go_type_name(cx, t));
            let names: Vec<Node> = kids(n).filter(|c| c.kind() == "identifier").collect();
            let value = n.child_by_field_name("value");
            for (i, nm) in names.iter().enumerate() {
                let (vt, init) = value.and_then(|v| v.named_child(i)).map(|e| value_info(cx, e)).unwrap_or_default();
                let text = cx.text(*nm).to_owned();
                let line = cx.line(*nm);
                let decl = ty.clone().filter(|t| !t.is_empty()).unwrap_or(vt);
                if value.and_then(|v| v.named_child(i)).is_some_and(|e| e.kind() == "func_literal") {
                    cx.add_binding(scope, &text, BindKind::LocalFn, "", "", line);
                } else {
                    cx.add_binding(scope, &text, BindKind::Local, &decl, &init, line);
                }
            }
            walk_children(cx, n, scope);
        }
        "range_clause" => {
            if let Some(l) = n.child_by_field_name("left") {
                bind_pairs(cx, l, None, None, scope);
            }
            walk_children(cx, n, scope);
        }
        _ => walk_children(cx, n, scope),
    }
}

fn go_type_name(cx: &Cx, n: Node) -> String {
    match n.kind() {
        "type_identifier" => cx.text(n).to_owned(),
        "qualified_type" => cx.text(n).to_owned(),
        "pointer_type" | "parenthesized_type" => n.named_child(0).map(|c| go_type_name(cx, c)).unwrap_or_default(),
        "generic_type" => n.child_by_field_name("type").map(|c| go_type_name(cx, c)).unwrap_or_default(),
        _ => String::new(),
    }
}

/// (declared type, initialiser) of a Go expression.
fn value_info(cx: &Cx, e: Node) -> (String, String) {
    match e.kind() {
        "composite_literal" => (String::new(), e.child_by_field_name("type").map(|t| go_type_name(cx, t)).unwrap_or_default()),
        "unary_expression" => e.child_by_field_name("operand").map(|o| value_info(cx, o)).unwrap_or_default(),
        "call_expression" => match e.child_by_field_name("function") {
            Some(f) if matches!(f.kind(), "identifier" | "selector_expression") => {
                // `new(T)` and `make(T, ...)` name their type argument
                let callee = cx.text(f);
                if matches!(callee, "new" | "make") {
                    let arg = e.child_by_field_name("arguments").and_then(|a| a.named_child(0));
                    return (String::new(), arg.map(|a| go_type_name(cx, a)).unwrap_or_default());
                }
                (String::new(), format!("{callee}()"))
            }
            _ => Default::default(),
        },
        "parenthesized_expression" => e.named_child(0).map(|c| value_info(cx, c)).unwrap_or_default(),
        _ => Default::default(),
    }
}

/// `a, b := x, y` / `for i, v := range xs`.
fn bind_pairs(cx: &mut Cx, left: Node, right: Option<Node>, _ty: Option<&str>, scope: u32) {
    let lefts: Vec<Node> = kids(left).filter(|c| c.is_named()).collect();
    let lefts = if lefts.is_empty() { vec![left] } else { lefts };
    for (i, l) in lefts.iter().enumerate() {
        if l.kind() != "identifier" {
            continue;
        }
        let nm = cx.text(*l).to_owned();
        if nm == "_" {
            continue;
        }
        let line = cx.line(*l);
        let rv = right.and_then(|r| {
            let items: Vec<Node> = kids(r).filter(|c| c.is_named()).collect();
            if items.is_empty() {
                Some(r)
            } else {
                items.get(i).copied().or_else(|| items.first().copied().filter(|_| items.len() == 1))
            }
        });
        match rv {
            Some(v) if v.kind() == "func_literal" => cx.add_binding(scope, &nm, BindKind::LocalFn, "", "", line),
            Some(v) => {
                let (vt, init) = value_info(cx, v);
                cx.add_binding(scope, &nm, BindKind::Local, &vt, &init, line);
            }
            None => cx.add_binding(scope, &nm, BindKind::Local, "", "", line),
        }
    }
}

fn import_spec(cx: &mut Cx, n: Node, scope: u32) {
    let Some(path) = n.child_by_field_name("path") else { return };
    let spec = unquote(cx.text(path)).to_owned();
    let line = cx.line(n);
    let name = n.child_by_field_name("name").map(|c| cx.text(c).to_owned());
    let (local, kind) = match name.as_deref() {
        Some("_") => (String::new(), ImportKind::SideEffect),
        Some(".") => ("*".to_owned(), ImportKind::Namespace),
        Some(other) => (other.to_owned(), ImportKind::Default),
        None => (default_package_name(&spec), ImportKind::Default),
    };
    cx.add_import(ImportFact { local, imported: "*".into(), spec, kind, type_only: false, scope, line });
}

fn params(cx: &mut Cx, list: Node, sc: u32, kind: BindKind) {
    for p in kids(list) {
        match p.kind() {
            "parameter_declaration" | "variadic_parameter_declaration" => {
                let ty = p.child_by_field_name("type").map(|t| go_type_name(cx, t)).unwrap_or_default();
                for nm in kids(p).filter(|c| c.kind() == "identifier") {
                    let t = cx.text(nm).to_owned();
                    let line = cx.line(nm);
                    cx.add_binding(sc, &t, kind, &ty, "", line);
                }
            }
            _ => {}
        }
    }
}

fn function(cx: &mut Cx, n: Node, scope: u32) {
    let sc = cx.push_scope(ScopeKind::Function, scope, n);
    if let Some(r) = n.child_by_field_name("receiver") {
        params(cx, r, sc, BindKind::Param);
    }
    if let Some(p) = n.child_by_field_name("parameters") {
        params(cx, p, sc, BindKind::Param);
    }
    if let Some(body) = n.child_by_field_name("body") {
        walk(cx, body, sc);
    }
}

fn type_spec(cx: &mut Cx, n: Node, scope: u32) {
    let ty = n.child_by_field_name("type");
    let func_type = ty.is_some_and(|t| t.kind() == "function_type");
    if let Some(name) = n.child_by_field_name("name") {
        let params: Vec<String> = n
            .child_by_field_name("type_parameters")
            .map(|tp| {
                kids(tp)
                    .filter(|d| d.kind() == "type_parameter_declaration")
                    .flat_map(|d| kids(d).filter(|c| c.kind() == "identifier").map(|c| cx.text(c).to_owned()).collect::<Vec<_>>())
                    .collect()
            })
            .unwrap_or_default();
        if !params.is_empty() || func_type {
            let name = cx.text(name).to_owned();
            cx.facts.go_types.push(GoTypeFact { name, alias: false, type_params: params, func_type });
        }
    }
    let Some(ty) = ty else { return };
    let row = cx.row_at(cx.start(n));
    if ty.kind() == "interface_type" {
        // embedded interfaces (`io.Reader`, `error`); a union or `~T` constrains, it embeds nothing
        for elem in kids(ty).filter(|c| c.kind() == "type_elem") {
            let types: Vec<Node> = kids(elem).filter(|c| c.is_named()).collect();
            if let ([t], Some(row)) = (types.as_slice(), row) {
                embed(cx, row, *t);
            }
        }
        return;
    }
    if ty.kind() != "struct_type" {
        return;
    }
    let sc = cx.push_scope(ScopeKind::Class, scope, n);
    let Some(list) = kids(ty).find(|c| c.kind() == "field_declaration_list") else { return };
    for f in kids(list) {
        if f.kind() != "field_declaration" {
            continue;
        }
        let ftype = f.child_by_field_name("type");
        let tname = ftype.map(|t| go_type_name(cx, t)).unwrap_or_default();
        let names: Vec<Node> = kids(f).filter(|c| c.kind() == "field_identifier").collect();
        let line = cx.line(f);
        if names.is_empty() {
            // embedded field: promoted fields / methods; named after its type
            let simple = tname.rsplit('.').next().unwrap_or("").to_owned();
            cx.add_binding(sc, &simple, BindKind::Field, &tname, "embedded", line);
            if let (Some(t), Some(row)) = (ftype, row) {
                embed(cx, row, t);
            }
        } else {
            let func = ftype.is_some_and(|t| t.kind() == "function_type");
            for nm in names {
                let t = cx.text(nm).to_owned();
                cx.add_binding(sc, &t, BindKind::Field, &tname, "", line);
                if let (true, Some(row)) = (func, row) {
                    cx.facts.go_fields.push(GoFieldFact::FuncField { row, field: t });
                }
            }
        }
    }
}

/// Record an embedded type: `io.Reader`, `*os.File`, `Base`, `error`.
fn embed(cx: &mut Cx, row: u32, t: Node) {
    let t = match t.kind() {
        "pointer_type" | "parenthesized_type" => match t.named_child(0) {
            Some(inner) => inner,
            None => return,
        },
        "generic_type" => match t.child_by_field_name("type") {
            Some(inner) => inner,
            None => return,
        },
        _ => t,
    };
    let (qualifier, name) = match t.kind() {
        "type_identifier" => (String::new(), cx.text(t).to_owned()),
        "qualified_type" => match (t.child_by_field_name("package"), t.child_by_field_name("name")) {
            (Some(p), Some(nm)) => (cx.text(p).to_owned(), cx.text(nm).to_owned()),
            _ => return,
        },
        _ => return,
    };
    cx.facts.go_embeds.push(GoEmbedFact { row, qualifier, name });
}

/// A value a struct field can be given: a name (`h`, `d.refresh`, `pkg.Handle`, `o.opts.F`) or
/// a conversion of one (`HandlerFunc(h.serve)`, kept as written for the linker to check).
fn field_value(cx: &Cx, v: Node) -> Option<String> {
    let v = if v.kind() == "literal_element" { v.named_child(0)? } else { v };
    match v.kind() {
        "identifier" => Some(cx.text(v).to_owned()),
        "selector_expression" => {
            let text = cx.text(v);
            let plain = text.split('.').all(|s| {
                let s = s.trim();
                s.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_') && s.chars().all(|c| c.is_alphanumeric() || c == '_')
            });
            (plain && text.split('.').count() <= 3).then(|| text.split_whitespace().collect())
        }
        "parenthesized_expression" => field_value(cx, v.named_child(0)?),
        "call_expression" => {
            let callee = v.child_by_field_name("function")?;
            let args: Vec<Node> = kids(v.child_by_field_name("arguments")?).filter(|a| a.is_named()).collect();
            if !matches!(callee.kind(), "identifier" | "selector_expression") || args.len() != 1 {
                return None;
            }
            let inner = field_value(cx, args[0])?;
            Some(format!("{}({inner})", cx.text(callee)))
        }
        _ => None,
    }
}

/// `T{F: v}` / `&T{a, v}`: the values a composite literal puts in the fields of `T`.
fn field_literal(cx: &mut Cx, n: Node) {
    let Some(ty) = n.child_by_field_name("type") else { return };
    let ty_text = match ty.kind() {
        "type_identifier" | "qualified_type" => cx.text(ty).to_owned(),
        "generic_type" => match ty.child_by_field_name("type") {
            Some(t) => cx.text(t).to_owned(),
            None => return,
        },
        _ => return,
    };
    let Some(body) = n.child_by_field_name("body") else { return };
    for (i, el) in kids(body).filter(|c| c.is_named() && c.kind() != "comment").enumerate() {
        let (field, value) = match el.kind() {
            "keyed_element" => {
                let key = el.child_by_field_name("key").and_then(|k| k.named_child(0));
                let Some(key) = key.filter(|k| matches!(k.kind(), "identifier" | "field_identifier")) else { continue };
                let Some(value) = el.child_by_field_name("value") else { continue };
                (cx.text(key).to_owned(), value)
            }
            "literal_element" => (format!("#{i}"), el),
            _ => continue,
        };
        let pos = cx.start(value);
        if let Some(value) = field_value(cx, value) {
            cx.facts.go_fields.push(GoFieldFact::Literal { ty: ty_text.clone(), field, value, line: (pos >> 32) as u32, col: pos as u32 });
        }
    }
}

/// `x.F = v` / `x.y.F = v`: a value put in field `F` of what `x` (or `x.y`) holds.
fn field_assignment(cx: &mut Cx, n: Node) {
    if n.child_by_field_name("operator").map(|o| cx.text(o)) != Some("=") {
        return;
    }
    let (Some(left), Some(right)) = (n.child_by_field_name("left"), n.child_by_field_name("right")) else { return };
    let lefts: Vec<Node> = kids(left).filter(|c| c.is_named()).collect();
    let rights: Vec<Node> = kids(right).filter(|c| c.is_named()).collect();
    if lefts.len() != rights.len() {
        return;
    }
    for (l, r) in lefts.into_iter().zip(rights) {
        if l.kind() != "selector_expression" {
            continue;
        }
        let (Some(operand), Some(field)) = (l.child_by_field_name("operand"), l.child_by_field_name("field")) else { continue };
        let receiver = match operand.kind() {
            "identifier" => cx.text(operand).to_owned(),
            "selector_expression" => match (operand.child_by_field_name("operand"), operand.child_by_field_name("field")) {
                (Some(o), Some(f)) if o.kind() == "identifier" => format!("{}.{}", cx.text(o), cx.text(f)),
                _ => continue,
            },
            _ => continue,
        };
        if let Some(value) = field_value(cx, r) {
            let pos = cx.start(r);
            cx.facts.go_fields.push(GoFieldFact::Assign { receiver, field: cx.text(field).to_owned(), value, line: (pos >> 32) as u32, col: pos as u32 });
        }
    }
}
