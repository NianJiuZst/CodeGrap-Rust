//! A module-level JS/TS value that hands a function on (upstream `const-alias-synthesizer.ts`):
//!
//! ```text
//! export const extname = (isWin ? win32.extname : posix.extname);
//! export const lexer = _Lexer.lex;
//! export const parse = marked;
//! ```
//!
//! Calling `extname` runs one of two functions, yet no call to either is written anywhere. The
//! value links to every function its initializer (kept in the declaration's signature) can hand
//! on: each arm of a conditional, `??` and `||`. Every name must reach a real binding - the
//! file's own declaration or one of its imports, then members of that - so `const keys =
//! Object.keys` links nothing, and a name that ends on data links nothing.

use super::{order_of, Out};
use crate::engine::{Bound, Engine};
use crate::resolve::{Accept, FileCx, NameRes, WorkerCx};
use cg_core::{EdgeKind, LangFamily, NodeIdx, NodeKind, NONE};
use cg_extract::fileflags;

/// How far a conditional may nest in its own arms.
const MAX_OPERANDS: usize = 8;

const NOT_A_BINDING: &[&str] = &["true", "false", "null", "undefined", "this", "super", "NaN", "Infinity"];

fn is_ident(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_' || c == '$') && s.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$')
}

/// Byte offsets of `pat` at bracket depth 0, outside strings.
fn top_level(code: &str, pat: &str) -> Vec<usize> {
    let b = code.as_bytes();
    let mut out = Vec::new();
    let (mut depth, mut i) = (0i32, 0usize);
    while i < b.len() {
        match b[i] {
            b'"' | b'\'' | b'`' => {
                let q = b[i];
                i += 1;
                while i < b.len() && b[i] != q {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
            }
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            _ if depth == 0 && code[i..].starts_with(pat) => {
                out.push(i);
                i += pat.len();
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    out
}

fn strip_parens(mut s: &str) -> &str {
    loop {
        let t = s.trim();
        match t.strip_prefix('(').and_then(|r| r.strip_suffix(')')) {
            // `(a) || (b)` is not one parenthesized expression
            Some(inner) if top_level(inner, ")").is_empty() => s = inner,
            _ => return t,
        }
    }
}

/// The names an initializer can hand on: the arms of a conditional, the operands of `??` /
/// `||`, each with parentheses, casts and `!` taken off. `None` when it is anything else.
fn operands(init: &str, out: &mut Vec<String>, depth: usize) -> Option<()> {
    if depth > MAX_OPERANDS || out.len() > MAX_OPERANDS {
        return None;
    }
    let code = strip_parens(init);
    if code.contains("=>") || code.starts_with("function") || code.starts_with("new ") {
        return None;
    }
    // `cond ? a : b`: the first top-level `?` that is no `?.` / `??`
    let question = top_level(code, "?").into_iter().find(|&i| !code[i..].starts_with("?.") && !code[i..].starts_with("??") && !code[..i].ends_with('?'));
    if let Some(q) = question {
        let rest = &code[q + 1..];
        let colon = *top_level(rest, ":").first()?;
        operands(&rest[..colon], out, depth + 1)?;
        return operands(&rest[colon + 1..], out, depth + 1);
    }
    for sep in ["??", "||"] {
        let at = top_level(code, sep);
        if !at.is_empty() {
            let mut start = 0;
            for i in at.into_iter().chain(std::iter::once(code.len())) {
                operands(&code[start..i], out, depth + 1)?;
                start = i + sep.len();
            }
            return Some(());
        }
    }
    let mut name = code;
    for cast in [" as ", " satisfies "] {
        if let Some((value, ty)) = name.split_once(cast) {
            if ty.contains("Mock") {
                return None; // a test's mock setup runs nothing it names
            }
            name = value.trim();
        }
    }
    let name = strip_parens(name.trim_end_matches('!'));
    let flat: String = name.split_whitespace().collect::<String>().replace("?.", ".");
    if !flat.split('.').all(is_ident) || NOT_A_BINDING.contains(&flat.as_str()) {
        return None;
    }
    out.push(flat);
    Some(())
}

/// The function a name path reaches from the top level of `cx`'s file.
fn reach<'e, 'v>(e: &'e Engine<'v>, cx: &FileCx<'e, 'v>, wcx: &mut WorkerCx<'e, 'v>, path: &[&str], line: u32) -> Option<NodeIdx> {
    let view = e.view;
    let callable = |n: NodeIdx| matches!(view.node(n).kind(), NodeKind::Function | NodeKind::Method | NodeKind::Component);
    let first = match cx.resolve_name(wcx, path[0], Accept::Any, 0, line, None, false) {
        NameRes::Found(n, ..) => Bound::Node(n),
        NameRes::Module(m) => Bound::Module(m),
        // a value declared at the top of this file
        NameRes::Local => Bound::Node(e.lookup_top_level(cx.file, path[0])?),
        _ => return None,
    };
    match (first, path.len()) {
        (Bound::Node(n), 1) => {
            if callable(n) {
                Some(n)
            } else {
                cx.forward_alias_node(wcx, n, None).filter(|m| callable(*m))
            }
        }
        (Bound::Node(n), 2) => match view.node(n).kind() {
            NodeKind::Class => e.find_member(n, path[1]).filter(|m| callable(*m)),
            NodeKind::Constant | NodeKind::Variable => cx.forward_alias_node(wcx, n, Some(path[1])).filter(|m| callable(*m)),
            _ => None,
        },
        (Bound::Module(m), 2) => match e.lookup_export(m, path[1], 0)? {
            Bound::Node(n) if callable(n) => Some(n),
            _ => None,
        },
        (Bound::Module(m), 3) => match e.lookup_export(m, path[1], 0)? {
            Bound::Node(c) if view.node(c).kind() == NodeKind::Class => e.find_member(c, path[2]).filter(|n| callable(*n)),
            _ => None,
        },
        _ => None,
    }
}

pub(crate) fn const_aliases<'e, 'v>(e: &'e Engine<'v>, out: &mut Out, wcx: &mut WorkerCx<'e, 'v>) {
    let view = e.view;
    for f in view.files() {
        let fr = view.file(f);
        if fr.lang().family() != LangFamily::Js || fr.flags() & fileflags::GENERATED != 0 {
            continue;
        }
        let values: Vec<NodeIdx> = view
            .nodes_in_file(f)
            .map(NodeIdx)
            .filter(|n| {
                let node = view.node(*n);
                matches!(node.kind(), NodeKind::Constant | NodeKind::Variable) && node.parent().is_some_and(|p| view.node(p).kind() == NodeKind::File)
            })
            .collect();
        if values.is_empty() {
            continue;
        }
        let cx = wcx.file_cx(e, f);
        for n in values {
            let node = view.node(n);
            let Some(init) = node.signature().and_then(|s| s.trim().strip_prefix('=')) else { continue };
            // a cut signature leaves its last operand unread
            let (init, cut) = match init.trim_end().strip_suffix("...") {
                Some(head) => (head, true),
                None => (init, false),
            };
            let mut names = Vec::new();
            if operands(init, &mut names, 0).is_none() {
                continue;
            }
            if cut {
                names.pop();
            }
            let mut linked: Vec<NodeIdx> = Vec::new();
            for name in &names {
                let path: Vec<&str> = name.split('.').collect();
                if path.len() > 3 {
                    continue;
                }
                let Some(target) = reach(e, &cx, wcx, &path, node.start_line()) else { continue };
                if target == n || linked.contains(&target) {
                    continue;
                }
                linked.push(target);
                let meta = serde_json::json!({
                    "synthesizedBy": "const-alias",
                    "via": name,
                    "registeredAt": format!("{}:{}", fr.path(), node.start_line()),
                });
                out.push_meta(EdgeKind::Calls, n, target, node.start_line(), NONE, meta, order_of("const-alias"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::operands;

    fn names(init: &str) -> Option<Vec<String>> {
        let mut out = Vec::new();
        operands(init, &mut out, 0).map(|_| out)
    }

    #[test]
    fn initializer_operands() {
        assert_eq!(names(" (isWin ? win32.extname : posix.extname)"), Some(vec!["win32.extname".into(), "posix.extname".into()]));
        assert_eq!(names("_Lexer.lex"), Some(vec!["_Lexer.lex".into()]));
        assert_eq!(names("a ?? b || c"), Some(vec!["a".into(), "b".into(), "c".into()]));
        assert_eq!(names("opts?.handler ?? noop"), Some(vec!["opts.handler".into(), "noop".into()]));
        assert_eq!(names("useAuth as jest.Mock"), None);
        assert_eq!(names("createIcon('x')"), None);
        assert_eq!(names("x => x ? a : b"), None);
        assert_eq!(names("{ a: 1 }"), None);
        assert_eq!(names("true"), None);
    }
}
