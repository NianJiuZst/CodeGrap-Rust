//! Reference-name shapes (design doc 6.3): the walkers encode the syntactic form of a call in
//! the name (`this.svc.run`, `new Widget().render`, `super().run`, `T::new`, `inner().m`).

use cg_core::LangFamily;
use smallvec::SmallVec;
use std::borrow::Cow;

#[derive(Debug, PartialEq, Eq)]
pub enum Shape<'a> {
    Bare(&'a str),
    /// `root.path.method` / `root::path::method` with `root` an ordinary identifier.
    Member {
        root: &'a str,
        path: SmallVec<[&'a str; 3]>,
        method: &'a str,
    },
    /// `this|self|cls|Self` followed by fields and a final method.
    This {
        kw: &'a str,
        fields: SmallVec<[&'a str; 3]>,
        method: &'a str,
    },
    Super {
        method: &'a str,
    },
    New {
        class: &'a str,
        method: &'a str,
    },
    CallResult {
        inner: &'a str,
        method: &'a str,
    },
    ValueResult {
        inner: &'a str,
        method: &'a str,
    },
}

fn split_segments(name: &str) -> SmallVec<[&str; 6]> {
    let mut out: SmallVec<[&str; 6]> = SmallVec::new();
    let mut start = 0;
    let b = name.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'.' {
            out.push(&name[start..i]);
            start = i + 1;
        } else if b[i] == b':' && b.get(i + 1) == Some(&b':') {
            out.push(&name[start..i]);
            i += 1;
            start = i + 1;
        }
        i += 1;
    }
    out.push(&name[start..]);
    out
}

fn last_segment(s: &str) -> &str {
    split_segments(s).last().copied().unwrap_or(s)
}

pub fn parse_shape(name: &str) -> Shape<'_> {
    let name = name.trim();
    if let Some(rest) = name.strip_prefix("new ") {
        if let Some(i) = rest.find("().") {
            return Shape::New { class: rest[..i].trim(), method: last_segment(&rest[i + 3..]) };
        }
        return Shape::Bare(rest.trim());
    }
    if let Some((value, method)) = name.rsplit_once('.') {
        if let Some(inner) = value.strip_suffix("()") {
            return if inner == "super" { Shape::Super { method } } else { Shape::CallResult { inner, method } };
        }
        if value.contains(')') || value.ends_with('?') {
            return Shape::ValueResult { inner: value, method };
        }
    }
    if name == "super" {
        return Shape::Bare("super");
    }
    let segs = split_segments(name);
    let n = segs.len();
    if n == 1 {
        return Shape::Bare(segs[0].trim_end_matches("[]"));
    }
    let first = segs[0];
    let method = segs[n - 1].trim_end_matches("[]");
    if matches!(first, "this" | "self" | "cls" | "Self") {
        return Shape::This { kw: first, fields: segs[1..n - 1].iter().map(|s| s.trim_end_matches("[]")).collect(), method };
    }
    if first == "super" {
        return Shape::Super { method };
    }
    Shape::Member { root: first.trim_end_matches("[]"), path: segs[1..n - 1].iter().map(|s| s.trim_end_matches("[]")).collect(), method }
}

/// Bring the call names of the C-family, C#, PHP, Ruby and Swift walkers into the shapes
/// `parse_shape` understands. Other families pass through unchanged.
///
/// * whitespace around member operators (`a\n  .b`) is dropped;
/// * null-conditional / forced access (`a?.b`, `a!.b`) and `->` become `.`;
/// * PHP: `parent::m` -> `super.m`, `self::m` / `static::m` -> `this.m`, `$f` -> `f`,
///   `\Ns\Klass::m` -> `Ns\Klass::m` (a namespaced root is resolved as written);
/// * C#: `base.M` -> `super.M`, verbatim identifiers `@x` -> `x`;
/// * Ruby: `@x.m` -> `self.@x.m`, `self.class.m` -> `self.m`, `::Foo.m` -> `Foo.m`;
/// * C/C++: a trailing arity marker (`Foo::Foo/0`) is dropped.
pub fn normalize_call_name(family: LangFamily, name: &str) -> Cow<'_, str> {
    if !matches!(family, LangFamily::C | LangFamily::CSharp | LangFamily::Php | LangFamily::Ruby | LangFamily::Swift) {
        return Cow::Borrowed(name);
    }
    let mut s: Cow<'_, str> = Cow::Borrowed(name.trim());
    if s.contains(char::is_whitespace) && !s.starts_with("new ") {
        s = Cow::Owned(squeeze_member_whitespace(&s));
    }
    match family {
        LangFamily::C => {
            if let Some(i) = s.rfind('/') {
                if i + 1 < s.len() && s[i + 1..].bytes().all(|b| b.is_ascii_digit()) {
                    s = Cow::Owned(s[..i].to_owned());
                }
            }
            if s.contains("->") && !s.contains("->*") {
                s = Cow::Owned(s.replace("->", "."));
            }
        }
        LangFamily::CSharp => {
            if s.contains("?.") || s.contains("!.") {
                s = Cow::Owned(s.replace("?.", ".").replace("!.", "."));
            }
            if let Some(rest) = s.strip_prefix("base.") {
                s = Cow::Owned(format!("super.{rest}"));
            }
            if s.contains('@') {
                s = Cow::Owned(s.replace('@', ""));
            }
        }
        LangFamily::Php => {
            if s.contains("->") {
                s = Cow::Owned(s.replace("?->", ".").replace("->", "."));
            }
            for (pre, to) in [("parent::", "super."), ("self::", "this."), ("static::", "this.")] {
                if let Some(rest) = s.strip_prefix(pre) {
                    s = Cow::Owned(format!("{to}{rest}"));
                    break;
                }
            }
            if s.contains('\\') {
                // absolute and `namespace\`-relative names keep their namespace
                let t = s.strip_prefix('\\').unwrap_or(&s);
                let t = t.strip_prefix("namespace\\").unwrap_or(t);
                if t.len() != s.len() {
                    s = Cow::Owned(t.to_owned());
                }
            }
            if let Some(rest) = s.strip_prefix('$') {
                s = Cow::Owned(rest.to_owned());
            }
        }
        LangFamily::Ruby => {
            if let Some(rest) = s.strip_prefix("::") {
                s = Cow::Owned(rest.to_owned());
            }
            if let Some(rest) = s.strip_prefix("self.class.") {
                s = Cow::Owned(format!("self.{rest}"));
            }
            if s.starts_with('@') && s.contains('.') {
                s = Cow::Owned(format!("self.{s}"));
            }
            if s.contains("&.") {
                s = Cow::Owned(s.replace("&.", "."));
            }
        }
        LangFamily::Swift if s.contains("?.") || s.contains("!.") => {
            s = Cow::Owned(s.replace("?.", ".").replace("!.", "."));
        }
        _ => {}
    }
    s
}

/// `a\n   .b` -> `a.b`: drop whitespace that touches a member operator.
fn squeeze_member_whitespace(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_whitespace() {
            let mut j = i;
            while j < b.len() && b[j].is_ascii_whitespace() {
                j += 1;
            }
            let next = &s[j..];
            let prev = out.as_bytes().last().copied();
            let touches = next.starts_with('.')
                || next.starts_with("?.")
                || next.starts_with("!.")
                || next.starts_with("&.")
                || next.starts_with("->")
                || next.starts_with("::")
                || matches!(prev, Some(b'.') | Some(b'>') | Some(b':'));
            if !touches {
                out.push(' ');
            }
            i = j;
            continue;
        }
        let ch = s[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// First and remaining segments of a dotted / `::` name (type references like `pkg.Type`).
pub fn split_head(name: &str) -> Option<(&str, &str)> {
    if let Some(i) = name.find("::") {
        return Some((&name[..i], &name[i + 2..]));
    }
    name.find('.').map(|i| (&name[..i], &name[i + 1..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes() {
        assert_eq!(parse_shape("foo"), Shape::Bare("foo"));
        assert_eq!(parse_shape("helper.go"), Shape::Member { root: "helper", path: SmallVec::new(), method: "go" });
        assert_eq!(parse_shape("s.repo.Find"), Shape::Member { root: "s", path: SmallVec::from_slice(&["repo"]), method: "Find" });
        assert_eq!(parse_shape("this.svc.run"), Shape::This { kw: "this", fields: SmallVec::from_slice(&["svc"]), method: "run" });
        assert_eq!(parse_shape("self.repo[].find"), Shape::This { kw: "self", fields: SmallVec::from_slice(&["repo"]), method: "find" });
        assert_eq!(parse_shape("new Widget().render"), Shape::New { class: "Widget", method: "render" });
        assert_eq!(parse_shape("super().run"), Shape::Super { method: "run" });
        assert_eq!(parse_shape("inner().m"), Shape::CallResult { inner: "inner", method: "m" });
        assert_eq!(parse_shape("T::new"), Shape::Member { root: "T", path: SmallVec::new(), method: "new" });
        assert_eq!(parse_shape("Self::other"), Shape::This { kw: "Self", fields: SmallVec::new(), method: "other" });
        assert_eq!(parse_shape("a::b::c"), Shape::Member { root: "a", path: SmallVec::from_slice(&["b"]), method: "c" });
        assert_eq!(split_head("pkg.Type"), Some(("pkg", "Type")));
        assert_eq!(split_head("a::B"), Some(("a", "B")));
        assert_eq!(split_head("Type"), None);
    }

    #[test]
    fn normalized_names() {
        let n = |f, s| normalize_call_name(f, s).into_owned();
        assert_eq!(n(LangFamily::Php, "this->repo.find"), "this.repo.find");
        assert_eq!(n(LangFamily::Php, "parent::__construct"), "super.__construct");
        assert_eq!(n(LangFamily::Php, "static::create"), "this.create");
        assert_eq!(n(LangFamily::Php, "\\FastRoute\\simpleDispatcher"), "FastRoute\\simpleDispatcher");
        assert_eq!(n(LangFamily::Php, "\\App\\Models\\User::find"), "App\\Models\\User::find");
        assert_eq!(n(LangFamily::Php, "namespace\\Sub\\helper"), "Sub\\helper");
        assert_eq!(n(LangFamily::C, "::util::log"), "::util::log");
        assert_eq!(n(LangFamily::Php, "$callback"), "callback");
        assert_eq!(n(LangFamily::CSharp, "svc\n        ?.Run"), "svc.Run");
        assert_eq!(n(LangFamily::CSharp, "base.Dispose"), "super.Dispose");
        assert_eq!(n(LangFamily::CSharp, "@interface.IsOpenGeneric"), "interface.IsOpenGeneric");
        assert_eq!(n(LangFamily::Ruby, "@repo.find"), "self.@repo.find");
        assert_eq!(n(LangFamily::Ruby, "self.class.settings"), "self.settings");
        assert_eq!(n(LangFamily::Ruby, "::File.join"), "File.join");
        assert_eq!(n(LangFamily::C, "fmt::basic_format_arg::basic_format_arg/0"), "fmt::basic_format_arg::basic_format_arg");
        assert_eq!(n(LangFamily::C, "node->next.visit"), "node.next.visit");
        assert_eq!(n(LangFamily::Swift, "session?.request"), "session.request");
        assert_eq!(n(LangFamily::Js, "a?.b"), "a?.b");
        assert_eq!(n(LangFamily::Ruby, "new Foo().bar"), "new Foo().bar");
    }
}
