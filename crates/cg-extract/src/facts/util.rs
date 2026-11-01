//! Shared helpers for the per-language fact extractors.

use super::*;
use cg_core::text::{col16, pack_pos};

/// Extraction context: source text, line index and the facts being built.
pub struct Cx<'a> {
    pub src: &'a str,
    pub rows: FileRows<'a>,
    pub facts: Facts,
    pub parse_timeout: std::time::Duration,
    /// Rust: the type parameters in scope, innermost last: (name, bounds written as
    /// `impl A + B`, or `_` for none).
    pub generics: Vec<Vec<(String, String)>>,
    line_starts: Vec<u32>,
    row_by_start: Option<rustc_hash::FxHashMap<u64, u32>>,
    /// Last (row, byte, UTF-16 column) computed: positions are asked in source order, and on
    /// a minified one-line file counting each column from the line start is quadratic.
    last_pos: std::cell::Cell<(usize, usize, u32)>,
}

impl<'a> Cx<'a> {
    pub fn new(src: &'a str, rows: FileRows<'a>) -> Self {
        let line_starts = text::line_starts(src);
        let end_line = line_starts.len() as u32;
        let facts = Facts::new(pack_pos(end_line + 1, 0));
        Cx {
            src,
            rows,
            facts,
            parse_timeout: std::time::Duration::from_secs(5),
            generics: Vec::new(),
            line_starts,
            row_by_start: None,
            last_pos: std::cell::Cell::new((usize::MAX, 0, 0)),
        }
    }

    /// Kernel node row whose span starts exactly at `pos` (packed line / UTF-16 column), skipping
    /// the file node. The first such node in extraction order wins.
    pub fn row_at(&mut self, pos: u64) -> Option<u32> {
        if self.row_by_start.is_none() {
            let mut m = rustc_hash::FxHashMap::default();
            for (i, n) in self.rows.nodes().enumerate().skip(1) {
                m.entry(pack_pos(n.start_line(), n.start_col())).or_insert(i as u32);
            }
            self.row_by_start = Some(m);
        }
        self.row_by_start.as_ref().and_then(|m| m.get(&pos).copied())
    }

    pub fn finish(self) -> Facts {
        self.facts
    }

    #[inline]
    pub fn text(&self, n: Node) -> &'a str {
        &self.src[n.start_byte()..n.end_byte()]
    }

    #[inline]
    pub fn field_text(&self, n: Node, field: &str) -> Option<&'a str> {
        n.child_by_field_name(field).map(|c| self.text(c))
    }

    /// Packed (1-based line, UTF-16 column) of a byte offset.
    pub fn pos_at(&self, byte: usize, row: usize) -> u64 {
        let ls = self.line_starts[row.min(self.line_starts.len() - 1)] as usize;
        let (last_row, last_byte, last_col) = self.last_pos.get();
        if last_row == row && last_byte <= byte && byte - last_byte < byte - ls && self.src.is_char_boundary(last_byte) && self.src.is_char_boundary(byte) {
            let col = last_col + self.src[last_byte..byte].chars().map(|c| c.len_utf16() as u32).sum::<u32>();
            self.last_pos.set((row, byte, col));
            return pack_pos(row as u32 + 1, col);
        }
        let le = self.line_starts.get(row + 1).map(|v| *v as usize).unwrap_or(self.src.len());
        let line = &self.src[ls..le];
        let col = col16(line, byte - ls);
        self.last_pos.set((row, byte, col));
        pack_pos(row as u32 + 1, col)
    }

    pub fn start(&self, n: Node) -> u64 {
        self.pos_at(n.start_byte(), n.start_position().row)
    }

    pub fn end(&self, n: Node) -> u64 {
        self.pos_at(n.end_byte(), n.end_position().row)
    }

    pub fn line(&self, n: Node) -> u32 {
        n.start_position().row as u32 + 1
    }

    /// Open a scope for `n`; returns its index.
    pub fn push_scope(&mut self, kind: ScopeKind, parent: u32, n: Node) -> u32 {
        let (start, end) = (self.start(n), self.end(n));
        self.facts.scopes.push(ScopeFact { kind, parent, start, end });
        (self.facts.scopes.len() - 1) as u32
    }

    pub fn add_binding(&mut self, scope: u32, name: &str, kind: BindKind, decl_type: &str, init_ctor: &str, line: u32) {
        if name.is_empty() {
            return;
        }
        self.facts.bindings.push(BindingFact {
            scope,
            name: name.to_owned(),
            kind,
            decl_type: decl_type.to_owned(),
            init_ctor: init_ctor.to_owned(),
            line,
            node_row: NONE,
        });
    }

    /// Type information of the nearest visible binding called `name` (declared so far).
    pub fn lookup_binding(&self, mut scope: u32, name: &str) -> Option<(String, String)> {
        loop {
            if self.facts.scopes[scope as usize].kind != ScopeKind::Class {
                if let Some(b) = self.facts.bindings.iter().rev().find(|b| {
                    b.scope == scope
                        && b.name == name
                        && !matches!(b.kind, BindKind::MemberFn | BindKind::ReturnType | BindKind::Deref | BindKind::Fixture | BindKind::TypeAlias)
                }) {
                    return Some((b.decl_type.clone(), b.init_ctor.clone()));
                }
            }
            let parent = self.facts.scopes[scope as usize].parent;
            if parent == NONE {
                return None;
            }
            scope = parent;
        }
    }

    /// Nearest enclosing class scope of `scope` (or `scope` itself).
    pub fn class_scope_of(&self, mut scope: u32) -> Option<u32> {
        loop {
            if self.facts.scopes[scope as usize].kind == ScopeKind::Class {
                return Some(scope);
            }
            let parent = self.facts.scopes[scope as usize].parent;
            if parent == NONE {
                return None;
            }
            scope = parent;
        }
    }

    pub fn add_import(&mut self, f: ImportFact) {
        self.facts.imports.push(f);
    }
}

/// Reduce a written type to the name resolution can look up: strip generics, pointers,
/// references, arrays, optionals, qualifiers' leading `&`/`*`/`mut`, keep a dotted/`::` path.
pub fn simple_type_name(t: &str) -> String {
    let mut s = t.trim();
    loop {
        let before = s;
        s = s.trim_start_matches(['&', '*', '?']).trim();
        for kw in ["mut ", "dyn ", "impl ", "const ", "readonly ", "final ", "val ", "var "] {
            if let Some(r) = s.strip_prefix(kw) {
                s = r.trim_start();
            }
        }
        if s == before {
            break;
        }
    }
    // `'a Foo` lifetimes
    if s.starts_with('\'') {
        s = s.split_whitespace().nth(1).unwrap_or("");
    }
    let end = s.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$' || c == '.' || c == ':')).unwrap_or(s.len());
    let name = s[..end].trim_end_matches(':').trim_end_matches('.');
    // `[]Foo` (Go slices) and `Foo[]` land here as empty or `Foo`
    name.to_owned()
}

/// `'./x'` / `"./x"` / `` `./x` `` -> `./x`; anything else is returned trimmed.
pub fn unquote(s: &str) -> &str {
    let t = s.trim();
    for q in ['"', '\'', '`'] {
        if t.len() >= 2 && t.starts_with(q) && t.ends_with(q) {
            return &t[1..t.len() - 1];
        }
    }
    t
}

/// Iterate the children of `n` with a fresh cursor.
pub fn kids<'t>(n: Node<'t>) -> impl Iterator<Item = Node<'t>> {
    let mut c = n.walk();
    let v: Vec<Node<'t>> = n.children(&mut c).collect();
    v.into_iter()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_names() {
        assert_eq!(simple_type_name("Foo<T>"), "Foo");
        assert_eq!(simple_type_name("&mut Foo"), "Foo");
        assert_eq!(simple_type_name("*Server"), "Server");
        assert_eq!(simple_type_name("pkg.Type"), "pkg.Type");
        assert_eq!(simple_type_name("std::vec::Vec<u8>"), "std::vec::Vec");
        assert_eq!(simple_type_name("Foo?"), "Foo");
        assert_eq!(simple_type_name("[]Foo"), "");
    }
}
