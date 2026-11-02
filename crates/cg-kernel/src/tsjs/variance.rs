//! A narrow recovery for variance annotations unsupported by the pinned TS grammar.
//! Only a failed type-parameter list on a type declaration is considered; the parser
//! retry must be error-free. The equal-length mask leaves all source locations unchanged.

use tree_sitter::{Node, Tree};

fn skip_trivia(bytes: &[u8], mut i: usize, end: usize) -> usize {
    while i < end {
        if bytes[i].is_ascii_whitespace() {
            i += 1;
        } else if bytes[i..end].starts_with(b"/*") {
            let Some(close) = bytes[i + 2..end].windows(2).position(|w| w == b"*/") else { return end };
            i += close + 4;
        } else if bytes[i..end].starts_with(b"//") {
            i += 2;
            while i < end && bytes[i] != b'\n' { i += 1; }
        } else {
            break;
        }
    }
    i
}

fn ident_end(bytes: &[u8], start: usize, end: usize) -> usize {
    if start >= end || !(bytes[start].is_ascii_alphabetic() || matches!(bytes[start], b'_' | b'$')) {
        return start;
    }
    let mut i = start + 1;
    while i < end && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'_' | b'$')) { i += 1; }
    i
}

fn parameter_prefix(bytes: &[u8], start: usize, end: usize, spans: &mut Vec<(usize, usize)>) {
    let first = skip_trivia(bytes, start, end);
    let first_end = ident_end(bytes, first, end);
    if first == first_end || !matches!(&bytes[first..first_end], b"in" | b"out") { return; }
    let next = skip_trivia(bytes, first_end, end);
    if next == first_end { return; }
    let next_end = ident_end(bytes, next, end);
    if next == next_end || &bytes[next..next_end] == b"extends" { return; }
    if &bytes[first..first_end] == b"in" && &bytes[next..next_end] == b"out" {
        let name = skip_trivia(bytes, next_end, end);
        let name_end = ident_end(bytes, name, end);
        if name == next_end || name == name_end || &bytes[name..name_end] == b"extends" { return; }
        spans.push((next, next_end));
    }
    spans.push((first, first_end));
}

fn find_spans(node: Node<'_>, source: &[u8], spans: &mut Vec<(usize, usize)>) {
    let mut pending = vec![node];
    while let Some(n) = pending.pop() {
        if !n.has_error() { continue; }
        if n.kind() == "type_parameters"
            && n.parent().is_some_and(|p| matches!(p.kind(), "interface_declaration" | "class_declaration" | "abstract_class_declaration" | "type_alias_declaration"))
        {
            let mut cursor = n.walk();
            for child in n.children(&mut cursor) {
                if matches!(child.kind(), "<" | ",") {
                    parameter_prefix(source, child.end_byte(), n.end_byte(), spans);
                }
            }
        }
        let mut cursor = n.walk();
        pending.extend(n.named_children(&mut cursor).filter(|c| c.has_error()));
    }
}

pub(super) fn mask(tree: &Tree, source: &str) -> Option<String> {
    let mut spans = Vec::new();
    find_spans(tree.root_node(), source.as_bytes(), &mut spans);
    if spans.is_empty() { return None; }
    let mut bytes = source.as_bytes().to_vec();
    for (start, end) in spans { bytes[start..end].fill(b' '); }
    String::from_utf8(bytes).ok()
}
