//! Facts for Ruby: `require` / `require_relative` / `load` side-effect imports, class /
//! module / method / block scopes, parameters, locals (first assignment per variable),
//! `rescue Foo => e`, `for x in xs`, instance / class variables and `attr_*` fields of the
//! enclosing class, and constants.
//!
//! Ruby has no type annotations: evidence comes from initialisers only (`Foo.new` gives the
//! declared type `Foo`; other calls give a bounded call path such as `Foo.create()`).

use super::util::*;
use super::*;
use rustc_hash::FxHashSet;

/// Longest receiver / call-path text kept as initialiser evidence.
const MAX_PATH: usize = 4096;
/// Nesting bound for receiver chains in call paths.
const MAX_PATH_DEPTH: u8 = 16;

pub fn extract(cx: &mut Cx, root: Node) {
    let mut rb = Rb { cx, soft: vec![false], vars: FxHashSet::default(), fields: FxHashSet::default(), attrs: Vec::new() };
    rb.walk(root, 0);
    rb.flush_attrs();
}

struct Rb<'c, 'a> {
    cx: &'c mut Cx<'a>,
    /// Per scope: `true` for blocks and lambdas, whose variable lookup continues into the
    /// enclosing scope (closures). `def`, `class`, `module` and the file are hard boundaries.
    soft: Vec<bool>,
    /// Local variables (params, locals, constants) already bound, by (scope, name).
    vars: FxHashSet<(u32, String)>,
    /// Fields (`@x`, `@@x`) already bound, by (class scope, name).
    fields: FxHashSet<(u32, String)>,
    /// `attr_*` fields, bound after the walk unless an assignment bound them first.
    attrs: Vec<(u32, String, u32)>,
}

impl Rb<'_, '_> {
    fn push_scope(&mut self, kind: ScopeKind, parent: u32, n: Node, soft: bool) -> u32 {
        let sc = self.cx.push_scope(kind, parent, n);
        self.soft.resize(sc as usize + 1, false);
        self.soft[sc as usize] = soft;
        sc
    }

    fn walk_children(&mut self, n: Node, scope: u32) {
        for ch in kids(n) {
            self.walk(ch, scope);
        }
    }

    fn walk_field(&mut self, n: Node, field: &str, scope: u32) {
        if let Some(c) = n.child_by_field_name(field) {
            self.walk(c, scope);
        }
    }

    fn walk(&mut self, n: Node, scope: u32) {
        match n.kind() {
            "class" | "module" => {
                // the superclass expression (`< Struct.new(:a)`) is evaluated outside the body
                self.walk_field(n, "superclass", scope);
                let sc = self.push_scope(ScopeKind::Class, scope, n, false);
                self.walk_field(n, "body", sc);
            }
            "singleton_class" => {
                self.walk_field(n, "value", scope);
                let sc = self.push_scope(ScopeKind::Class, scope, n, false);
                self.walk_field(n, "body", sc);
            }
            "method" | "singleton_method" => {
                let sc = self.push_scope(ScopeKind::Function, scope, n, false);
                if let Some(p) = n.child_by_field_name("parameters") {
                    self.params(p, sc);
                }
                self.walk_field(n, "body", sc);
            }
            "block" | "do_block" => {
                let sc = self.push_scope(ScopeKind::Function, scope, n, true);
                if let Some(p) = n.child_by_field_name("parameters") {
                    self.params(p, sc);
                }
                self.walk_field(n, "body", sc);
            }
            "lambda" => {
                // `->(x) { ... }`: one scope for the lambda; its body block adds no second one
                let sc = self.push_scope(ScopeKind::Function, scope, n, true);
                if let Some(p) = n.child_by_field_name("parameters") {
                    self.params(p, sc);
                }
                if let Some(body) = n.child_by_field_name("body") {
                    if matches!(body.kind(), "block" | "do_block") {
                        if let Some(p) = body.child_by_field_name("parameters") {
                            self.params(p, sc);
                        }
                        self.walk_field(body, "body", sc);
                    } else {
                        self.walk(body, sc);
                    }
                }
            }
            "call" => {
                self.call(n, scope);
                self.walk_children(n, scope);
            }
            "assignment" => {
                if let (Some(left), Some(right)) = (n.child_by_field_name("left"), n.child_by_field_name("right")) {
                    let (decl, init) = self.value_info(right);
                    self.bind_target(left, scope, &decl, &init);
                }
                self.walk_children(n, scope);
            }
            "operator_assignment" => {
                if let Some(left) = n.child_by_field_name("left") {
                    let op = n.child_by_field_name("operator").map(|o| self.cx.text(o)).unwrap_or("");
                    let (decl, init) = match n.child_by_field_name("right") {
                        Some(r) if matches!(op, "||=" | "&&=") => self.value_info(r),
                        _ => Default::default(),
                    };
                    self.bind_target(left, scope, &decl, &init);
                }
                self.walk_children(n, scope);
            }
            "rescue" => {
                if let Some(var) = n.child_by_field_name("variable") {
                    let ty = n.child_by_field_name("exceptions").and_then(|e| e.named_child(0)).and_then(|c| const_path(self.cx, c)).unwrap_or_default();
                    for target in kids(var).filter(|c| c.is_named()) {
                        self.bind_target(target, scope, &ty, "");
                    }
                }
                self.walk_children(n, scope);
            }
            "for" => {
                if let Some(p) = n.child_by_field_name("pattern") {
                    self.bind_target(p, scope, "", "");
                }
                self.walk_children(n, scope);
            }
            _ => self.walk_children(n, scope),
        }
    }

    // ------------------------------------------------------------ parameters

    /// `method_parameters`, `block_parameters`, `lambda_parameters`, `destructured_parameter`.
    fn params(&mut self, params: Node, sc: u32) {
        let mut c = params.walk();
        let list: Vec<(Node, Option<&str>)> = params.children(&mut c).enumerate().map(|(i, ch)| (ch, params.field_name_for_child(i as u32))).collect();
        for (p, field) in list {
            match p.kind() {
                // `|a; b|` block-local variables are plain locals of the block
                "identifier" if field == Some("locals") => self.bind_var(p, sc, BindKind::Local, "", ""),
                "identifier" => self.bind_var(p, sc, BindKind::Param, "", ""),
                "optional_parameter" | "keyword_parameter" | "splat_parameter" | "hash_splat_parameter" | "block_parameter" => {
                    if let Some(name) = p.child_by_field_name("name") {
                        self.bind_var(name, sc, BindKind::Param, "", "");
                    }
                    // default values may hold blocks / lambdas
                    self.walk_field(p, "value", sc);
                }
                "destructured_parameter" => self.params(p, sc),
                _ => {}
            }
        }
    }

    // ------------------------------------------------------------ bindings

    /// Whether variable `name` is already bound in `scope` or, through blocks, in the scopes
    /// it closes over.
    fn var_bound(&self, mut scope: u32, name: &str) -> bool {
        let mut key = (scope, name.to_owned());
        loop {
            if self.vars.contains(&key) {
                return true;
            }
            if !self.soft.get(scope as usize).copied().unwrap_or(false) {
                return false;
            }
            scope = self.cx.facts.scopes[scope as usize].parent;
            if scope == NONE {
                return false;
            }
            key.0 = scope;
        }
    }

    /// Bind the identifier / constant `id` in `scope` unless the variable already exists
    /// (only the first assignment of a name creates a binding).
    fn bind_var(&mut self, id: Node, scope: u32, kind: BindKind, decl: &str, init: &str) {
        let name = self.cx.text(id);
        if name.is_empty() || self.var_bound(scope, name) {
            return;
        }
        let line = self.cx.line(id);
        self.cx.add_binding(scope, name, kind, decl, init, line);
        self.vars.insert((scope, name.to_owned()));
    }

    /// Bind `@x` / `@@x` as a field of the enclosing class (first assignment per class wins).
    fn bind_field(&mut self, var: Node, scope: u32, decl: &str, init: &str) {
        let Some(cs) = self.cx.class_scope_of(scope) else { return };
        let name = self.cx.text(var);
        if name.len() < 2 || self.fields.contains(&(cs, name.to_owned())) {
            return;
        }
        let line = self.cx.line(var);
        self.cx.add_binding(cs, name, BindKind::Field, decl, init, line);
        self.fields.insert((cs, name.to_owned()));
    }

    /// Bind an assignment target (`_lhs` or `left_assignment_list`).
    fn bind_target(&mut self, t: Node, scope: u32, decl: &str, init: &str) {
        match t.kind() {
            "identifier" | "constant" => self.bind_var(t, scope, BindKind::Local, decl, init),
            "instance_variable" | "class_variable" => self.bind_field(t, scope, decl, init),
            // multiple assignment: no per-target type evidence
            "left_assignment_list" | "destructured_left_assignment" | "rest_assignment" => {
                for ch in kids(t).filter(|c| c.is_named()) {
                    self.bind_target(ch, scope, "", "");
                }
            }
            _ => {}
        }
    }

    /// (declared type, initialiser call path) of an assigned value.
    fn value_info(&self, v: Node) -> (String, String) {
        match v.kind() {
            "call" => {
                let Some(method) = v.child_by_field_name("method") else { return Default::default() };
                if let Some(recv) = v.child_by_field_name("receiver") {
                    if self.cx.text(method) == "new" {
                        if let Some(ty) = const_path(self.cx, recv) {
                            return (ty, String::new());
                        }
                    }
                } else if v.child_by_field_name("arguments").is_none() && v.child_by_field_name("block").is_none() {
                    // a bare name may be a local variable rather than a call
                    return Default::default();
                }
                (String::new(), call_path(self.cx, v, 0).unwrap_or_default())
            }
            // `x = (Foo.new)`, `x = y = Foo.new`, `x = Foo.new rescue nil`
            "parenthesized_statements" if v.named_child_count() == 1 => v.named_child(0).map(|c| self.value_info(c)).unwrap_or_default(),
            "assignment" => v.child_by_field_name("right").map(|c| self.value_info(c)).unwrap_or_default(),
            "rescue_modifier" => v.child_by_field_name("body").map(|c| self.value_info(c)).unwrap_or_default(),
            _ => Default::default(),
        }
    }

    // ------------------------------------------------------------ calls

    /// `require` / `require_relative` / `load` imports and `attr_*` fields.
    fn call(&mut self, n: Node, scope: u32) {
        if n.child_by_field_name("receiver").is_some() {
            return;
        }
        let Some(method) = n.child_by_field_name("method") else { return };
        let Some(args) = n.child_by_field_name("arguments") else { return };
        match self.cx.text(method) {
            m @ ("require" | "require_relative" | "load") => {
                let Some(path) = args.named_child(0).and_then(|a| plain_string(self.cx, a)) else { return };
                if path.is_empty() {
                    return;
                }
                let spec = if m == "require_relative" && !(path.starts_with("./") || path.starts_with("../")) { format!("./{path}") } else { path.to_owned() };
                let line = self.cx.line(n);
                self.cx.add_import(ImportFact {
                    local: String::new(),
                    imported: String::new(),
                    spec,
                    kind: ImportKind::SideEffect,
                    type_only: false,
                    scope,
                    line,
                });
            }
            "attr_accessor" | "attr_reader" | "attr_writer" | "attr" => {
                let Some(cs) = self.cx.class_scope_of(scope) else { return };
                for a in kids(args).filter(|c| c.is_named()) {
                    let name = match a.kind() {
                        "simple_symbol" => self.cx.text(a).strip_prefix(':'),
                        "string" => plain_string(self.cx, a),
                        _ => None,
                    };
                    if let Some(name) = name.filter(|s| !s.is_empty() && !s.starts_with('@')) {
                        let line = self.cx.line(a);
                        self.attrs.push((cs, format!("@{name}"), line));
                    }
                }
            }
            _ => {}
        }
    }

    /// `attr_*` fields not bound by an assignment anywhere in their class.
    fn flush_attrs(&mut self) {
        for (cs, name, line) in std::mem::take(&mut self.attrs) {
            if self.fields.insert((cs, name.clone())) {
                self.cx.add_binding(cs, &name, BindKind::Field, "", "", line);
            }
        }
    }
}

/// Text of a string literal without interpolation (`'a'`, `"a"`, `%q(a)`); `None` otherwise.
fn plain_string<'a>(cx: &Cx<'a>, s: Node) -> Option<&'a str> {
    if s.kind() != "string" {
        return None;
    }
    let mut content = None;
    for ch in kids(s).filter(|c| c.is_named()) {
        if ch.kind() != "string_content" || content.is_some() {
            return None;
        }
        content = Some(cx.text(ch));
    }
    Some(content.unwrap_or(""))
}

/// `Foo` / `Foo::Bar` / `::Foo` -> `Foo` / `Foo::Bar` / `Foo`; `None` for anything that is not
/// a constant path.
fn const_path(cx: &Cx, n: Node) -> Option<String> {
    match n.kind() {
        "constant" => Some(cx.text(n).to_owned()),
        "scope_resolution" => {
            let name = n.child_by_field_name("name").filter(|c| c.kind() == "constant")?;
            match n.child_by_field_name("scope") {
                Some(s) => Some(format!("{}::{}", const_path(cx, s)?, cx.text(name))),
                None => Some(cx.text(name).to_owned()),
            }
        }
        _ => None,
    }
}

/// Bounded call path of a value: `other.build()`, `Foo.create()`, `Foo.new().build()`, `make()`.
fn call_path(cx: &Cx, n: Node, depth: u8) -> Option<String> {
    if depth >= MAX_PATH_DEPTH || n.end_byte() - n.start_byte() > MAX_PATH {
        return None;
    }
    match n.kind() {
        "identifier" | "instance_variable" | "class_variable" | "global_variable" | "self" => Some(cx.text(n).to_owned()),
        "constant" | "scope_resolution" => const_path(cx, n),
        "call" => {
            let method = n.child_by_field_name("method")?;
            if !matches!(method.kind(), "identifier" | "constant") {
                return None;
            }
            let path = match n.child_by_field_name("receiver") {
                Some(r) => format!("{}.{}()", call_path(cx, r, depth + 1)?, cx.text(method)),
                None => format!("{}()", cx.text(method)),
            };
            Some(path)
        }
        _ => None,
    }
}
