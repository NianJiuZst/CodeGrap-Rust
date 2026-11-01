//! Rust conditional item wrappers (`cfg_rt! { pub struct X; impl X { .. } }`, Tokio's
//! `cfg_*!` family) hide whole items from tree-sitter: the body is an opaque token tree, so
//! the kernel extracts neither the declarations nor the calls inside them.
//!
//! [`rust_item_wrappers`] returns a *position-preserving* view of the source in which only
//! the wrapper's `name! {` and its closing `}` are blanked (newlines kept, every other byte
//! unchanged), so line/column positions, and therefore node ids, of everything else stay
//! identical. Arbitrary macros, `cfg_if!` and macro definitions are left opaque; nothing is
//! expanded or executed.

use std::time::{Duration, Instant};
use tree_sitter::{Node, ParseOptions, Parser, Tree};

/// Largest source this pre-pass touches.
pub const MAX_BYTES: usize = 1024 * 1024;

fn wrapper(name: &str) -> bool {
    name.starts_with("cfg_") && name != "cfg_if" && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

fn collect(n: Node, src: &str, ranges: &mut Vec<(usize, usize)>, depth: usize) {
    if depth > 128 || n.kind() == "macro_definition" {
        return;
    }
    let mut c = n.walk();
    let children: Vec<Node> = n.children(&mut c).collect();
    if matches!(n.kind(), "macro_invocation" | "token_tree") {
        let mut i = 0;
        while i < children.len() {
            if i + 2 < children.len() && children[i + 1].kind() == "!" && children[i + 2].kind() == "token_tree" {
                let name = &src[children[i].byte_range()];
                let body = children[i + 2];
                if wrapper(name) && src.as_bytes().get(body.start_byte()) == Some(&b'{') && body.end_byte() > body.start_byte() + 1 {
                    ranges.push((children[i].start_byte(), body.start_byte() + 1));
                    ranges.push((body.end_byte() - 1, body.end_byte()));
                    collect(body, src, ranges, depth + 1);
                }
                // An unknown macro's tokens are not ordinary Rust items.
                i += 3;
            } else {
                collect(children[i], src, ranges, depth + 1);
                i += 1;
            }
        }
    } else {
        for ch in children {
            collect(ch, src, ranges, depth + 1);
        }
    }
}

/// Byte ranges to blank for the wrappers found in `tree` (empty when there are none).
pub fn wrapper_ranges(tree: &Tree, src: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    if src.contains("cfg_") {
        collect(tree.root_node(), src, &mut ranges, 0);
    }
    ranges
}

/// `src` with the given ranges blanked (newlines kept).
pub fn blank_ranges(src: &str, ranges: &[(usize, usize)]) -> String {
    let mut masked = src.as_bytes().to_vec();
    for &(start, end) in ranges {
        for b in &mut masked[start..end] {
            if *b != b'\n' && *b != b'\r' {
                *b = b' ';
            }
        }
    }
    // Only ASCII bytes of a macro name, `!` and braces (plus whitespace between them) are
    // replaced, so the result is still valid UTF-8.
    String::from_utf8(masked).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// The masked view of a Rust source and whether the unmasked parse already had errors,
/// or `None` when the file has no conditional item wrappers (or is too large / the parse
/// ran out of time; the caller then extracts the original text).
pub fn rust_item_wrappers(src: &str, timeout: Duration) -> Option<(String, bool)> {
    if src.len() > MAX_BYTES || !src.contains("cfg_") || timeout.is_zero() {
        return None;
    }
    let mut parser = Parser::new();
    parser.set_language(&cg_kernel::hooks::language("rust")?).ok()?;
    let deadline = Instant::now() + timeout;
    let mut expired = false;
    let mut cancel = |_: &tree_sitter::ParseState| {
        expired = Instant::now() >= deadline;
        expired
    };
    let bytes = src.as_bytes();
    let tree =
        parser.parse_with_options(&mut |offset, _| bytes.get(offset..).unwrap_or_default(), None, Some(ParseOptions::new().progress_callback(&mut cancel)))?;
    if expired {
        return None;
    }
    let ranges = wrapper_ranges(&tree, src);
    if ranges.is_empty() {
        return None;
    }
    Some((blank_ranges(src, &ranges), tree.root_node().has_error()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blanks_only_wrapper_tokens() {
        let src = "cfg_rt! {\n    pub struct A;\n    impl A { fn f(&self) {} }\n}\ncfg_if::cfg_if! { if #[cfg(x)] { fn g() {} } }\nfoo! { fn h() {} }\n";
        let (masked, had_error) = rust_item_wrappers(src, Duration::from_secs(5)).unwrap();
        assert!(!had_error);
        assert_eq!(masked.len(), src.len());
        assert_eq!(masked.lines().count(), src.lines().count());
        assert!(masked.starts_with("         \n    pub struct A;"));
        assert!(masked.contains("cfg_if::cfg_if!"));
        assert!(masked.contains("foo! { fn h() {} }"));
        // positions of untouched text are unchanged
        assert_eq!(masked.find("impl A"), src.find("impl A"));
    }

    #[test]
    fn nested_wrappers_and_impl_bodies() {
        let src = "impl S {\n    cfg_net! {\n        cfg_unix! {\n            fn a(&self) {}\n        }\n    }\n}\n";
        let (masked, _) = rust_item_wrappers(src, Duration::from_secs(5)).unwrap();
        assert!(!masked.contains("cfg_"));
        assert_eq!(masked.find("fn a"), src.find("fn a"));
    }

    #[test]
    fn untouched_without_wrappers() {
        assert!(rust_item_wrappers("fn main() { cfg_x(); }", Duration::from_secs(5)).is_none());
        assert!(rust_item_wrappers("cfg_rt! { fn a() {} }", Duration::ZERO).is_none());
        assert!(rust_item_wrappers("macro_rules! cfg_rt { ($($i:item)*) => { $($i)* } }", Duration::from_secs(5)).is_none());
    }
}
