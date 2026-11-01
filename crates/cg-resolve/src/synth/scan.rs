//! Helpers shared by the site scanners: line / UTF-16 column of an offset, the function a
//! line sits in, and call arguments read off comment-stripped source.

use crate::frameworks::href::{index_at_depth0, match_bracket};
use cg_core::NodeKind;
use cg_extract::HookCtx;

/// Line starts of a source text, for offset -> (line, column) lookups.
pub struct LineIndex<'a> {
    src: &'a str,
    starts: Vec<usize>,
}

impl<'a> LineIndex<'a> {
    pub fn new(src: &'a str) -> Self {
        let mut starts = vec![0];
        starts.extend(src.bytes().enumerate().filter(|(_, b)| *b == b'\n').map(|(i, _)| i + 1));
        LineIndex { src, starts }
    }

    /// 1-based line of a byte offset.
    pub fn line(&self, off: usize) -> u32 {
        self.starts.partition_point(|s| *s <= off) as u32
    }

    /// UTF-16 column of a byte offset on its line.
    pub fn col(&self, off: usize) -> u32 {
        let i = self.starts.partition_point(|s| *s <= off) - 1;
        let start = self.starts[i];
        let end = self.starts.get(i + 1).map_or(self.src.len(), |e| e - 1);
        cg_core::text::col16(&self.src[start..end.max(start)], off - start)
    }

    /// Text of lines `from..=to` (1-based, clamped).
    pub fn lines(&self, from: u32, to: u32) -> &'a str {
        let from = (from.max(1) as usize - 1).min(self.starts.len() - 1);
        let to = (to as usize).min(self.starts.len());
        let end = self.starts.get(to).copied().unwrap_or(self.src.len());
        &self.src[self.starts[from]..end.max(self.starts[from])]
    }
}

/// A bundle or minified file: lines far longer than written code. Its pervasive `x[k](…)`,
/// `.get(…)` and one-letter names are a false-positive minefield, and its one giant line
/// makes per-match position lookups quadratic, so the site scanners skip it.
pub fn is_minified(src: &str) -> bool {
    let lines = src.bytes().filter(|b| *b == b'\n').count() + 1;
    src.len() / lines > 200 || (src.len() > 20_000 && src.split('\n').any(|l| l.len() > 5_000))
}

/// Kinds a call site is attributed to (upstream `FN_KINDS`).
pub const FN_KINDS: &[NodeKind] = &[NodeKind::Method, NodeKind::Function, NodeKind::Component];

/// Row of the tightest node of `kinds` whose lines contain `line`.
pub fn enclosing_row(ctx: &HookCtx<'_>, line: u32, kinds: &[NodeKind]) -> Option<u32> {
    let mut best: Option<(u32, u32, u32)> = None; // (span, start, row)
    for (row, n) in ctx.rows.nodes().enumerate() {
        if !kinds.contains(&n.kind()) {
            continue;
        }
        let (s, e) = (n.start_line(), n.end_line().max(n.start_line()));
        if s <= line && line <= e {
            let span = e - s;
            if best.is_none_or(|(b, bs, _)| span < b || (span == b && s > bs)) {
                best = Some((span, s, row as u32));
            }
        }
    }
    best.map(|b| b.2)
}

/// Top-level arguments of the call whose `(` is at `open`, trimmed.
pub fn call_args(src: &str, open: usize) -> Option<Vec<&str>> {
    let close = match_bracket(src, open)?;
    let inner = &src[open + 1..close];
    let mut out = Vec::new();
    let mut start = 0;
    while let Some(c) = index_at_depth0(inner, b',', start) {
        out.push(inner[start..c].trim());
        start = c + 1;
    }
    let last = inner[start..].trim();
    if !last.is_empty() || !out.is_empty() {
        out.push(last);
    }
    Some(out)
}

/// Whether the byte before `at` may end an identifier or member chain (`x.` / `x$`), so a
/// match starting at `at` would begin mid-chain.
pub fn mid_chain(src: &str, at: usize) -> bool {
    at > 0 && (src.as_bytes()[at - 1].is_ascii_alphanumeric() || matches!(src.as_bytes()[at - 1], b'_' | b'$' | b'.'))
}

/// The initializer of the nearest `const|let|var name = …` above `line` and not above `floor`
/// (a reassignment in between makes it unknown): what a bare identifier argument holds.
pub fn local_initializer<'a>(lines: &LineIndex<'a>, name: &str, line: u32, floor: u32) -> Option<&'a str> {
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$') {
        return None;
    }
    let src = lines.src;
    let mut l = line.saturating_sub(1);
    while l >= floor.max(1) {
        let text = lines.lines(l, l);
        let line_off = text.as_ptr() as usize - src.as_ptr() as usize;
        for kw in ["const ", "let ", "var "] {
            let Some(i) = text.find(&format!("{kw}{name}")) else { continue };
            let after_name = i + kw.len() + name.len();
            if text[after_name..].starts_with(|c: char| c.is_alphanumeric() || c == '_' || c == '$') {
                continue;
            }
            let rest = text[after_name..].trim_start();
            let rest = if let Some(r) = rest.strip_prefix(':') { r.find('=').map_or("", |e| &r[e..]) } else { rest };
            if let Some(init) = rest.strip_prefix('=').filter(|r| !r.starts_with(['=', '>'])) {
                // the initializer may continue onto the lines before the call
                let init_off = line_off + (text.len() - init.len());
                let limit = lines.lines(line, line).as_ptr() as usize - src.as_ptr() as usize;
                let init_full = &src[init_off..limit.max(init_off)];
                let end = statement_end(init_full);
                return Some(init_full[..end].trim());
            }
        }
        let reassigned = text.match_indices(name).any(|(i, _)| {
            let before = text[..i].chars().next_back();
            let after = text[i + name.len()..].trim_start();
            !before.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$' || c == '.')
                && after.starts_with('=')
                && !after.starts_with("==")
                && !after.starts_with("=>")
        });
        if reassigned {
            return None;
        }
        if l == 0 {
            break;
        }
        l -= 1;
    }
    None
}

/// End of the expression statement starting the text: the first `;` or line break at depth 0.
fn statement_end(s: &str) -> usize {
    let b = s.as_bytes();
    let mut depth = 0i32;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' | b'\'' => i = crate::frameworks::href::string_end(s, i).unwrap_or(b.len()),
            b'`' => i = crate::frameworks::href::template_end(s, i).unwrap_or(b.len()),
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b';' if depth <= 0 => return i,
            b'\n' if depth <= 0 => {
                // a continuation line (`? a : b`, `+ x`) belongs to the expression
                let next = s[i + 1..].trim_start();
                if !next.starts_with(['?', ':', '.', '+', '&', '|']) {
                    return i;
                }
            }
            _ => {}
        }
        i += 1;
    }
    s.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_and_locals() {
        let src = "function f() {\n  const href = ok\n    ? '/a'\n    : '/b';\n  go(href);\n}\n";
        let li = LineIndex::new(src);
        assert_eq!(li.line(src.find("go(").unwrap()), 5);
        assert_eq!(li.col(src.find("go(").unwrap()), 2);
        assert_eq!(local_initializer(&li, "href", 5, 1), Some("ok\n    ? '/a'\n    : '/b'"));
        let src2 = "let x = '/a';\nx = other;\ngo(x);\n";
        assert_eq!(local_initializer(&LineIndex::new(src2), "x", 3, 1), None);
        assert_eq!(call_args("f(a, {b: 1, c}, `x${y}`)", 1).unwrap(), vec!["a", "{b: 1, c}", "`x${y}`"]);
    }
}
