//! Facts for Ruby: `require` / `require_relative` / `load` side-effect imports, class /
//! module / method / block scopes, parameters, locals, `rescue Foo => e`, `for x in xs`,
//! instance / class variables and `attr_*` fields of the enclosing class, and constants.
//!
//! Ruby has no type annotations: evidence comes from initialisers only (`Foo.new` gives the
//! declared type `Foo`; other calls give a bounded call path such as `Foo.create()`). A local
//! is bound at its first assignment and again at every later assignment in the same scope that
//! carries evidence, so the resolver sees the type current at each use. A field (`@x`, `@@x`)
//! has one binding per class, typed by the first assignment with evidence.

use super::util::*;
use super::*;
use rustc_hash::{FxHashMap, FxHashSet};

/// Recursion bound: deeper subtrees are skipped rather than risking the stack.
const MAX_DEPTH: u32 = 400;
/// Longest constant / call-path text kept as initialiser evidence.
const MAX_PATH: usize = 4096;
/// Nesting bound for receiver chains in call paths and for unwrapping assigned values.
const MAX_PATH_DEPTH: u8 = 16;

pub fn extract(cx: &mut Cx, root: Node) {
    let mut rb = Rb { cx, soft: vec![false], vars: FxHashSet::default(), fields: FxHashMap::default(), attrs: Vec::new() };
    rb.walk(root, 0, 0);
    rb.flush_attrs();
}

struct Rb<'c, 'a> {
    cx: &'c mut Cx<'a>,
    /// Per scope: `true` for blocks and lambdas, whose variable lookup continues into the
    /// enclosing scope (closures). `def`, `class`, `module` and the file are hard boundaries.
    soft: Vec<bool>,
    /// Local variables (params, locals, constants) already bound, by (scope, name).
    vars: FxHashSet<(u32, String)>,
    /// The one Field binding of each (class scope, name): its index in the facts' bindings.
    fields: FxHashMap<(u32, String), usize>,
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

    fn walk_children(&mut self, n: Node, scope: u32, depth: u32) {
        for ch in kids(n) {
            self.walk(ch, scope, depth + 1);
        }
    }

    fn walk_field(&mut self, n: Node, field: &str, scope: u32, depth: u32) {
        if let Some(c) = n.child_by_field_name(field) {
            self.walk(c, scope, depth + 1);
        }
    }

    fn walk(&mut self, n: Node, scope: u32, depth: u32) {
        if depth > MAX_DEPTH {
            return;
        }
        match n.kind() {
            "class" | "module" => {
                // the superclass expression (`< Struct.new(:a)`) is evaluated outside the body
                self.walk_field(n, "superclass", scope, depth);
                let sc = self.push_scope(ScopeKind::Class, scope, n, false);
                self.walk_field(n, "body", sc, depth);
            }
            "singleton_class" => {
                self.walk_field(n, "value", scope, depth);
                let sc = self.push_scope(ScopeKind::Class, scope, n, false);
                self.walk_field(n, "body", sc, depth);
            }
            "method" | "singleton_method" => {
                let sc = self.push_scope(ScopeKind::Function, scope, n, false);
                if let Some(p) = n.child_by_field_name("parameters") {
                    self.params(p, sc, depth + 1);
                }
                self.walk_field(n, "body", sc, depth);
            }
            "block" | "do_block" => {
                let sc = self.push_scope(ScopeKind::Function, scope, n, true);
                if let Some(p) = n.child_by_field_name("parameters") {
                    self.params(p, sc, depth + 1);
                }
                self.walk_field(n, "body", sc, depth);
            }
            "lambda" => {
                // `->(x) { ... }`: one scope for the lambda; its body block adds no second one
                let sc = self.push_scope(ScopeKind::Function, scope, n, true);
                if let Some(p) = n.child_by_field_name("parameters") {
                    self.params(p, sc, depth + 1);
                }
                if let Some(body) = n.child_by_field_name("body") {
                    if matches!(body.kind(), "block" | "do_block") {
                        if let Some(p) = body.child_by_field_name("parameters") {
                            self.params(p, sc, depth + 2);
                        }
                        self.walk_field(body, "body", sc, depth + 1);
                    } else {
                        self.walk(body, sc, depth + 1);
                    }
                }
            }
            "call" => {
                self.call(n, scope);
                self.walk_children(n, scope, depth);
            }
            "assignment" => {
                if let (Some(left), Some(right)) = (n.child_by_field_name("left"), n.child_by_field_name("right")) {
                    let (decl, init) = self.value_info(right, 0);
                    self.assign(left, scope, &decl, &init, depth + 1);
                }
                self.walk_children(n, scope, depth);
            }
            "operator_assignment" => {
                if let Some(left) = n.child_by_field_name("left") {
                    let op = n.child_by_field_name("operator").map(|o| self.cx.text(o)).unwrap_or("");
                    let (decl, init) = match n.child_by_field_name("right") {
                        Some(r) if matches!(op, "||=" | "&&=") => self.value_info(r, 0),
                        _ => Default::default(),
                    };
                    self.assign(left, scope, &decl, &init, depth + 1);
                }
                self.walk_children(n, scope, depth);
            }
            "rescue" => {
                if let Some(var) = n.child_by_field_name("variable") {
                    // `rescue Foo => e` types `e`; with several classes (`rescue A, B => e`) the
                    // type is ambiguous and none is recorded
                    let ty = match n.child_by_field_name("exceptions") {
                        Some(e) if e.named_child_count() == 1 => e.named_child(0).and_then(|c| const_path(self.cx, c)).unwrap_or_default(),
                        _ => String::new(),
                    };
                    for target in kids(var).filter(|c| c.is_named()) {
                        self.assign(target, scope, &ty, "", depth + 2);
                    }
                }
                self.walk_children(n, scope, depth);
            }
            "for" => {
                if let Some(p) = n.child_by_field_name("pattern") {
                    self.assign(p, scope, "", "", depth + 1);
                }
                self.walk_children(n, scope, depth);
            }
            _ => self.walk_children(n, scope, depth),
        }
    }

    // ------------------------------------------------------------ parameters

    /// `method_parameters`, `block_parameters`, `lambda_parameters`, `destructured_parameter`.
    fn params(&mut self, params: Node, sc: u32, depth: u32) {
        if depth > MAX_DEPTH {
            return;
        }
        let mut c = params.walk();
        let list: Vec<(Node, Option<&str>)> = params.children(&mut c).enumerate().map(|(i, ch)| (ch, params.field_name_for_child(i as u32))).collect();
        for (p, field) in list {
            match p.kind() {
                // `|a; b|` block-local variables are plain locals of the block
                "identifier" if field == Some("locals") => self.declare(p, sc, BindKind::Local),
                "identifier" => self.declare(p, sc, BindKind::Param),
                "optional_parameter" | "keyword_parameter" | "splat_parameter" | "hash_splat_parameter" | "block_parameter" => {
                    if let Some(name) = p.child_by_field_name("name") {
                        self.declare(name, sc, BindKind::Param);
                    }
                    // default values may hold blocks / lambdas
                    self.walk_field(p, "value", sc, depth);
                }
                "destructured_parameter" => self.params(p, sc, depth + 1),
                _ => {}
            }
        }
    }

    // ------------------------------------------------------------ bindings

    /// The scope variable `name` is already bound in: `scope` itself or, through blocks
    /// (closures), an enclosing scope up to the first hard boundary.
    fn bound_in(&self, mut scope: u32, name: &str) -> Option<u32> {
        let mut key = (scope, name.to_owned());
        loop {
            if self.vars.contains(&key) {
                return Some(scope);
            }
            if !self.soft.get(scope as usize).copied().unwrap_or(false) {
                return None;
            }
            scope = self.cx.facts.scopes[scope as usize].parent;
            if scope == NONE {
                return None;
            }
            key.0 = scope;
        }
    }

    /// Declare the parameter / block-local `id` as a variable of `scope` (untyped), unless that
    /// scope already has one of this name. Outer variables of the same name are shadowed.
    fn declare(&mut self, id: Node, scope: u32, kind: BindKind) {
        let name = self.cx.text(id);
        if name.is_empty() || !self.vars.insert((scope, name.to_owned())) {
            return;
        }
        let line = self.cx.line(id);
        self.cx.add_binding(scope, name, kind, "", "", line);
    }

    /// Bind the identifier / constant `id` assigned in `scope`. The first assignment of a name
    /// creates its variable. A later assignment in the same scope adds a binding at its line
    /// when it carries type evidence (the resolver takes the latest binding before a use) and
    /// nothing otherwise. Assigning an outer variable inside a block adds nothing either: it
    /// is the same variable.
    fn assign_var(&mut self, id: Node, scope: u32, decl: &str, init: &str) {
        let name = self.cx.text(id);
        if name.is_empty() {
            return;
        }
        match self.bound_in(scope, name) {
            None => {
                self.vars.insert((scope, name.to_owned()));
            }
            Some(s) if s == scope && !(decl.is_empty() && init.is_empty()) => {}
            Some(_) => return,
        }
        let line = self.cx.line(id);
        self.cx.add_binding(scope, name, BindKind::Local, decl, init, line);
    }

    /// Bind `@x` / `@@x` as a field of the enclosing class: one binding per class and name, at
    /// the first assignment. A later assignment only supplies the type evidence that binding
    /// still lacks (`@v = nil` in `initialize`, `@v = Foo.new` elsewhere).
    fn bind_field(&mut self, var: Node, scope: u32, decl: &str, init: &str) {
        let Some(cs) = self.cx.class_scope_of(scope) else { return };
        let name = self.cx.text(var);
        if name.len() < 2 {
            return;
        }
        if let Some(&i) = self.fields.get(&(cs, name.to_owned())) {
            let b = &mut self.cx.facts.bindings[i];
            if b.decl_type.is_empty() && b.init_ctor.is_empty() {
                b.decl_type = decl.to_owned();
                b.init_ctor = init.to_owned();
            }
            return;
        }
        let line = self.cx.line(var);
        self.cx.add_binding(cs, name, BindKind::Field, decl, init, line);
        self.fields.insert((cs, name.to_owned()), self.cx.facts.bindings.len() - 1);
    }

    /// Bind an assignment target (`_lhs` or `left_assignment_list`).
    fn assign(&mut self, t: Node, scope: u32, decl: &str, init: &str, depth: u32) {
        if depth > MAX_DEPTH {
            return;
        }
        match t.kind() {
            "identifier" | "constant" => self.assign_var(t, scope, decl, init),
            "instance_variable" | "class_variable" => self.bind_field(t, scope, decl, init),
            // multiple assignment: no per-target type evidence
            "left_assignment_list" | "destructured_left_assignment" | "rest_assignment" => {
                for ch in kids(t).filter(|c| c.is_named()) {
                    self.assign(ch, scope, "", "", depth + 1);
                }
            }
            _ => {}
        }
    }

    /// (declared type, initialiser call path) of an assigned value.
    fn value_info(&self, v: Node, depth: u8) -> (String, String) {
        if depth >= MAX_PATH_DEPTH {
            return Default::default();
        }
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
            "parenthesized_statements" if v.named_child_count() == 1 => v.named_child(0).map(|c| self.value_info(c, depth + 1)).unwrap_or_default(),
            "assignment" => v.child_by_field_name("right").map(|c| self.value_info(c, depth + 1)).unwrap_or_default(),
            "rescue_modifier" => v.child_by_field_name("body").map(|c| self.value_info(c, depth + 1)).unwrap_or_default(),
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
            if self.fields.contains_key(&(cs, name.clone())) {
                continue;
            }
            self.cx.add_binding(cs, &name, BindKind::Field, "", "", line);
            self.fields.insert((cs, name), self.cx.facts.bindings.len() - 1);
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
/// a constant path, or one longer than `MAX_PATH`.
fn const_path(cx: &Cx, n: Node) -> Option<String> {
    if n.end_byte() - n.start_byte() > MAX_PATH {
        return None;
    }
    // iterate down the `scope` chain: `A::B::C` nests as ((A::B)::C)
    let mut parts = Vec::new();
    let mut cur = n;
    loop {
        match cur.kind() {
            "constant" => {
                parts.push(cx.text(cur));
                break;
            }
            "scope_resolution" => {
                let name = cur.child_by_field_name("name").filter(|c| c.kind() == "constant")?;
                parts.push(cx.text(name));
                match cur.child_by_field_name("scope") {
                    Some(s) => cur = s,
                    None => break,
                }
            }
            _ => return None,
        }
    }
    parts.reverse();
    Some(parts.join("::"))
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
