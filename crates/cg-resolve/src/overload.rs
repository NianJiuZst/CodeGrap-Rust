//! Overload selection (upstream `javaOverloadsFitting` / `retargetSelfOverload`): when one
//! type or file declares several functions of a name, the call's arguments decide which one
//! it reaches. Arity comes first (required, optional, variadic parameters), then each argument
//! the call site makes readable is scored against the parameter's declared type: the same
//! type, a widening or boxing conversion, a project supertype. An overload the arguments
//! certainly cannot reach is ruled out; anything unread fits. When the arguments settle
//! nothing, the first declared overload stands, as before.

use cg_core::{Lang, LangFamily, NodeIdx};
use cg_extract::facts::args::ARGS_MARK;
use cg_store::{RefRec, View};
use regex::Regex;
use smallvec::SmallVec;
use std::sync::LazyLock;

/// Argument shapes of a call reference (see `cg_extract::facts::args`).
pub struct CallArgs<'v> {
    pub shapes: SmallVec<[&'v str; 6]>,
}

impl<'v> CallArgs<'v> {
    pub fn of(view: &'v View, r: &RefRec) -> Option<CallArgs<'v>> {
        if r.candidates.0 == cg_core::NONE {
            return None;
        }
        let list = view.list(r.candidates);
        let (first, rest) = list.split_first()?;
        (*first == ARGS_MARK).then(|| CallArgs { shapes: rest.iter().copied().collect() })
    }
}

/// What an argument is, as far as the call site and its bindings say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArgTy {
    Unknown,
    Null,
    /// A lambda / function value taking this many parameters (`None`: unknown).
    Lambda(Option<u8>),
    /// A static type as written (`int`, `String`, `Foo`) and the project type it names.
    Named(String, Option<NodeIdx>),
}

/// One declared parameter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Param {
    /// Declared type, simplified: no generics, annotations, modifiers or nullability; `[]`
    /// kept; empty when not written.
    pub ty: String,
    pub optional: bool,
    pub variadic: bool,
    /// Kotlin `T?`: accepts `null`.
    pub nullable: bool,
    /// A function type (`(a: A) => B`, `(A) -> B`): its parameter count when written.
    pub function: Option<Option<u8>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Params {
    pub items: SmallVec<[Param; 6]>,
}

impl Params {
    fn required(&self) -> usize {
        self.items.iter().filter(|p| !p.optional && !p.variadic).count()
    }
    fn variadic(&self) -> bool {
        self.items.last().is_some_and(|p| p.variadic)
    }
    /// Whether `n` arguments can be passed.
    pub fn takes(&self, n: usize) -> bool {
        n >= self.required() && (self.variadic() || n <= self.items.len())
    }
    /// The parameter the `i`th argument binds to.
    fn param_for(&self, i: usize) -> Option<&Param> {
        self.items.get(i).or_else(|| self.items.last().filter(|p| p.variadic))
    }
}

/// Split a list body at top-level commas; `<`/`>` nest except in `=>` and `->`.
fn split_top(s: &str) -> SmallVec<[&str; 6]> {
    let mut out = SmallVec::new();
    let (mut depth, mut start) = (0i32, 0usize);
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' | b'\'' | b'`' => {
                let q = b[i];
                i += 1;
                while i < b.len() && b[i] != q {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'(' | b'[' | b'{' | b'<' => depth += 1,
            b'>' if i > 0 && matches!(b[i - 1], b'=' | b'-') => {}
            b')' | b']' | b'}' | b'>' => depth -= 1,
            b',' if depth == 0 => {
                out.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    let last = s[start..].trim();
    if !last.is_empty() || !out.is_empty() {
        out.push(last);
    }
    out
}

/// Byte range of the parameter list: the first `(` outside type arguments, to its `)`.
fn param_list(sig: &str) -> Option<&str> {
    let b = sig.as_bytes();
    let mut angle = 0i32;
    let mut open = None;
    for (i, c) in b.iter().enumerate() {
        match c {
            b'<' => angle += 1,
            b'>' if i > 0 && matches!(b[i - 1], b'=' | b'-') => {}
            b'>' => angle -= 1,
            b'(' if angle <= 0 => {
                open = Some(i);
                break;
            }
            _ => {}
        }
    }
    let open = open?;
    let mut depth = 0i32;
    for (i, c) in b.iter().enumerate().skip(open) {
        match c {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&sig[open + 1..i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// `Map<K, V>` -> `Map`, `java.util.List<X>[]` -> `List[]`, `Foo?` -> `Foo`.
pub fn simplify_type(t: &str) -> String {
    let mut out = String::with_capacity(t.len());
    let mut depth = 0i32;
    let b = t.as_bytes();
    for (i, c) in t.char_indices() {
        match c {
            '<' => depth += 1,
            '>' if i > 0 && matches!(b[i - 1], b'=' | b'-') => {}
            '>' => depth -= 1,
            _ if depth > 0 => {}
            ' ' | '\t' | '\n' | '?' | '!' => {}
            _ => out.push(c),
        }
    }
    // drop package qualifiers: `java.lang.String` -> `String`, `ns.Foo` -> `Foo`
    let arrays = out.len() - out.trim_end_matches("[]").len();
    let base = &out[..out.len() - arrays];
    let base = base.rsplit(['.', ':']).next().unwrap_or(base);
    format!("{base}{}", &out[out.len() - arrays..])
}

fn function_arity(t: &str, arrow: &str) -> Option<Option<u8>> {
    let t = t.trim();
    let at = t.find(arrow)?;
    let head = t[..at].trim();
    let head = head.strip_prefix("suspend").map(str::trim).unwrap_or(head);
    let inner = head.strip_prefix('(').and_then(|h| h.strip_suffix(')'));
    Some(inner.map(|i| split_top(i).into_iter().filter(|p| !p.is_empty()).count() as u8))
}

/// Parameters read off a stored signature: Java `void (String s, int... n)`, Kotlin
/// `(s: String, n: Int = 0)`, TypeScript `(s: string, n?: number): void`.
pub fn parse_params(lang: Lang, sig: &str) -> Option<Params> {
    let body = param_list(sig)?;
    let mut items = SmallVec::new();
    for raw in split_top(body) {
        if raw.is_empty() {
            continue;
        }
        let p = match lang {
            Lang::Kotlin => kotlin_param(raw),
            Lang::Java => java_param(raw),
            l if l.family() == LangFamily::Js => match js_param(raw) {
                Some(p) => p,
                None => continue, // `this: Foo`
            },
            _ => return None,
        };
        items.push(p);
    }
    Some(Params { items })
}

fn java_param(raw: &str) -> Param {
    let mut tokens: Vec<&str> = Vec::new();
    let mut rest = raw;
    // annotations, possibly with arguments
    loop {
        rest = rest.trim_start();
        if let Some(r) = rest.strip_prefix('@') {
            let end = r.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.')).unwrap_or(r.len());
            rest = &r[end..];
            if rest.trim_start().starts_with('(') {
                if let Some(close) = rest.find(')') {
                    rest = &rest[close + 1..];
                }
            }
            continue;
        }
        if let Some(r) = rest.strip_prefix("final ") {
            rest = r;
            continue;
        }
        break;
    }
    let variadic = rest.contains("...");
    let rest = rest.replace("...", " ");
    tokens.extend(rest.split_whitespace());
    // the last token is the name; the type is what precedes it
    let ty = if tokens.len() >= 2 { tokens[..tokens.len() - 1].join("") } else { tokens.first().map(|t| t.to_string()).unwrap_or_default() };
    Param { ty: simplify_type(&ty), optional: false, variadic, nullable: true, function: None }
}

fn kotlin_param(raw: &str) -> Param {
    let variadic = raw.split_whitespace().any(|t| t == "vararg");
    let (_, ty) = raw.split_once(':').unwrap_or(("", raw));
    let parts = split_eq(ty);
    let ty = parts.0.trim();
    let optional = parts.1;
    let nullable = ty.ends_with('?');
    let function = function_arity(ty, "->");
    let ty = if function.is_some() { "Function".to_owned() } else { simplify_type(ty) };
    Param { ty, optional, variadic, nullable, function }
}

/// Split `Type = default` at a top-level `=` that is not part of `=>` / `==`.
fn split_eq(s: &str) -> (&str, bool) {
    let b = s.as_bytes();
    let mut depth = 0i32;
    for i in 0..b.len() {
        match b[i] {
            b'(' | b'[' | b'{' | b'<' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b'>' if i > 0 && matches!(b[i - 1], b'=' | b'-') => {}
            b'>' => depth -= 1,
            b'=' if depth == 0 && b.get(i + 1) != Some(&b'>') && b.get(i + 1) != Some(&b'=') && (i == 0 || !matches!(b[i - 1], b'=' | b'!' | b'<' | b'>')) => {
                return (&s[..i], true);
            }
            _ => {}
        }
    }
    (s, false)
}

fn js_param(raw: &str) -> Option<Param> {
    let mut s = raw.trim();
    for m in ["public ", "private ", "protected ", "readonly ", "override "] {
        while let Some(r) = s.strip_prefix(m) {
            s = r.trim_start();
        }
    }
    let variadic = s.starts_with("...");
    let s = s.trim_start_matches("...");
    let (head, has_default) = split_eq(s);
    // the name (or pattern) ends at the first top-level `:` / `?:`
    let b = head.as_bytes();
    let mut depth = 0i32;
    let mut colon = None;
    for (i, c) in b.iter().enumerate() {
        match c {
            b'(' | b'[' | b'{' | b'<' => depth += 1,
            b')' | b']' | b'}' | b'>' => depth -= 1,
            b':' if depth == 0 => {
                colon = Some(i);
                break;
            }
            _ => {}
        }
    }
    let (name, ty) = match colon {
        Some(i) => (&head[..i], head[i + 1..].trim()),
        None => (head, ""),
    };
    let name = name.trim();
    if name == "this" {
        return None;
    }
    let optional = has_default || name.ends_with('?');
    let ty_simple = if ty.contains("=>") {
        "Function".to_owned()
    } else if ty.contains('|') {
        // a union takes several types: it rules nothing out by name
        String::new()
    } else {
        simplify_type(ty)
    };
    Some(Param { ty: ty_simple, optional, variadic, nullable: true, function: function_arity(ty, "=>") })
}

// ------------------------------------------------------------------- fitting

const JAVA_PRIMITIVES: &[&str] = &["byte", "short", "int", "long", "float", "double", "char", "boolean"];

fn java_box(p: &str) -> Option<&'static str> {
    Some(match p {
        "byte" => "Byte",
        "short" => "Short",
        "int" => "Integer",
        "long" => "Long",
        "float" => "Float",
        "double" => "Double",
        "char" => "Character",
        "boolean" => "Boolean",
        _ => return None,
    })
}

fn java_unbox(p: &str) -> Option<&'static str> {
    JAVA_PRIMITIVES.iter().copied().find(|prim| java_box(prim) == Some(p))
}

/// Java's widening primitive conversions (JLS 5.1.2).
fn java_widens(from: &str, to: &str) -> bool {
    let order: &[&str] = match from {
        "byte" => &["short", "int", "long", "float", "double"],
        "short" | "char" => &["int", "long", "float", "double"],
        "int" => &["long", "float", "double"],
        "long" => &["float", "double"],
        "float" => &["double"],
        _ => &[],
    };
    order.contains(&to)
}

/// JDK functional interfaces and the parameter count of their single method.
fn jdk_functional(t: &str) -> Option<u8> {
    Some(match t {
        "Runnable" | "Callable" | "Supplier" | "BooleanSupplier" | "IntSupplier" | "LongSupplier" | "DoubleSupplier" => 0,
        "Consumer" | "Function" | "Predicate" | "UnaryOperator" | "IntFunction" | "IntPredicate" | "IntConsumer" | "IntUnaryOperator" | "LongFunction"
        | "ToIntFunction" | "ToLongFunction" | "ToDoubleFunction" | "DoubleFunction" | "LongConsumer" | "DoubleConsumer" => 1,
        "BiConsumer" | "BiFunction" | "BiPredicate" | "BinaryOperator" | "Comparator" | "IntBinaryOperator" | "ToIntBiFunction" => 2,
        _ => return None,
    })
}

/// Types every value of a JDK leaf type (String, a boxed primitive) is assignable to.
fn java_value_supertype(arg: &str, param: &str) -> bool {
    match param {
        "Object" | "Serializable" | "Comparable" | "Constable" | "ConstantDesc" => true,
        "CharSequence" => arg == "String",
        "Number" => java_unbox(arg).is_some_and(|p| !matches!(p, "char" | "boolean")) || JAVA_PRIMITIVES.contains(&arg) && !matches!(arg, "char" | "boolean"),
        _ => false,
    }
}

/// A type name that stands for any type at all: a type parameter or a top type.
fn open_type(family: LangFamily, t: &str, type_params: &[&str]) -> bool {
    if t.is_empty() || type_params.contains(&t) {
        return true;
    }
    // `T`, `E`, `K2`: a single capital (and digits) is conventionally a type variable
    let mut c = t.chars();
    if c.next().is_some_and(|f| f.is_ascii_uppercase()) && c.all(|x| x.is_ascii_digit()) {
        return true;
    }
    match family {
        LangFamily::Jvm => matches!(t, "Object" | "Any"),
        LangFamily::Js => matches!(t, "any" | "unknown" | "object" | "Object" | "{}"),
        _ => true,
    }
}

const KOTLIN_NUMBERS: &[&str] = &["Byte", "Short", "Int", "Long", "Float", "Double", "UByte", "UShort", "UInt", "ULong"];
const JS_PRIMITIVES: &[&str] = &["string", "number", "boolean", "bigint", "symbol"];

/// How the project relates an argument to a parameter type written in one candidate's file:
/// `Some(true)` when it is that type or a subtype, `Some(false)` when the parameter is a
/// project type the argument cannot be, `None` when it cannot say.
pub type Relate<'a> = dyn Fn(&ArgTy, &str) -> Option<bool> + 'a;

/// Score of passing `arg` for `param`: `None` when the language refuses it.
pub fn fit_arg(family: LangFamily, arg: &ArgTy, param: &Param, type_params: &[&str], relate: &Relate<'_>) -> Option<u8> {
    let pt = param.ty.as_str();
    match arg {
        ArgTy::Unknown => Some(1),
        ArgTy::Null => match family {
            LangFamily::Jvm if JAVA_PRIMITIVES.contains(&pt) => None,
            LangFamily::Jvm if !param.nullable => None,
            _ => Some(2),
        },
        ArgTy::Lambda(k) => {
            if let Some(want) = param.function {
                return match (want, k) {
                    (Some(w), Some(k)) if w != *k => None,
                    _ => Some(3),
                };
            }
            if open_type(family, pt, type_params) {
                return Some(1);
            }
            match family {
                LangFamily::Jvm => {
                    if let Some(n) = jdk_functional(pt) {
                        return match k {
                            Some(k) if *k != n => None,
                            _ => Some(3),
                        };
                    }
                    if JAVA_PRIMITIVES.contains(&pt)
                        || java_unbox(pt).is_some()
                        || KOTLIN_NUMBERS.contains(&pt)
                        || matches!(pt, "String" | "Boolean" | "Char")
                        || pt.ends_with("[]")
                    {
                        return None;
                    }
                    // a project class or enum can't take a lambda; an interface may be functional
                    match relate(&ArgTy::Lambda(*k), pt) {
                        Some(false) => None,
                        _ => Some(1),
                    }
                }
                LangFamily::Js => {
                    if JS_PRIMITIVES.contains(&pt) || pt.ends_with("[]") {
                        None
                    } else if pt == "Function" {
                        Some(3)
                    } else {
                        Some(1)
                    }
                }
                _ => Some(1),
            }
        }
        ArgTy::Named(at, _) => {
            let at = at.as_str();
            if open_type(family, pt, type_params) {
                return Some(1);
            }
            if at == pt {
                return Some(3);
            }
            if param.function.is_some() || pt == "Function" {
                // a named value may hold a function; a primitive cannot
                return if JAVA_PRIMITIVES.contains(&at) || JS_PRIMITIVES.contains(&at) || matches!(at, "String" | "Int" | "Long" | "Boolean") {
                    None
                } else {
                    Some(1)
                };
            }
            match family {
                LangFamily::Jvm => {
                    let at_prim = JAVA_PRIMITIVES.contains(&at);
                    let pt_prim = JAVA_PRIMITIVES.contains(&pt);
                    if at_prim && pt_prim {
                        return java_widens(at, pt).then_some(2);
                    }
                    if at_prim {
                        // boxing, then a widening reference conversion
                        if java_box(at) == Some(pt) {
                            return Some(2);
                        }
                        return java_value_supertype(at, pt).then_some(1);
                    }
                    if pt_prim {
                        // unboxing, then widening
                        return java_unbox(at).filter(|p| *p == pt || java_widens(p, pt)).map(|_| 2);
                    }
                    let kotlin_value = KOTLIN_NUMBERS.contains(&at) || matches!(at, "Boolean" | "Char");
                    if kotlin_value || KOTLIN_NUMBERS.contains(&pt) {
                        // Kotlin: a numeric literal adapts to any numeric parameter
                        let both_numeric = KOTLIN_NUMBERS.contains(&at) && KOTLIN_NUMBERS.contains(&pt);
                        return if both_numeric {
                            Some(1)
                        } else if KOTLIN_NUMBERS.contains(&pt) || matches!(pt, "String" | "Boolean" | "Char") {
                            None
                        } else {
                            relate_or(relate, arg, pt, 1)
                        };
                    }
                    if at == "String" || java_unbox(at).is_some() {
                        if java_value_supertype(at, pt) {
                            return Some(2);
                        }
                        return if matches!(pt, "String" | "Boolean" | "Char") || java_unbox(pt).is_some() || pt.ends_with("[]") {
                            None
                        } else {
                            relate_or(relate, arg, pt, 1)
                        };
                    }
                    if at.ends_with("[]") != pt.ends_with("[]") {
                        return if pt.ends_with("[]") || matches!(pt, "String") || java_unbox(pt).is_some() { None } else { relate_or(relate, arg, pt, 1) };
                    }
                    if matches!(pt, "String") || java_unbox(pt).is_some() {
                        // a project type is never a String or a boxed primitive
                        return if arg_is_project(arg) { None } else { Some(1) };
                    }
                    relate_or(relate, arg, pt, 1)
                }
                LangFamily::Js => {
                    let at_prim = JS_PRIMITIVES.contains(&at);
                    let pt_prim = JS_PRIMITIVES.contains(&pt);
                    if at_prim || pt_prim {
                        // a primitive is only itself; a quoted literal type is a string
                        return if at == "string" && (pt.starts_with('\'') || pt.starts_with('"')) {
                            Some(2)
                        } else if at_prim && !pt_prim && !arg_is_project(arg) && relate(arg, pt).is_none() {
                            Some(1)
                        } else {
                            None
                        };
                    }
                    relate_or(relate, arg, pt, 1)
                }
                _ => Some(1),
            }
        }
    }
}

/// A type no project declaration can be: a primitive, `String`, a boxed number.
pub fn is_value_type(t: &str) -> bool {
    JAVA_PRIMITIVES.contains(&t)
        || java_unbox(t).is_some()
        || JS_PRIMITIVES.contains(&t)
        || KOTLIN_NUMBERS.contains(&t)
        || matches!(t, "String" | "Boolean" | "Char")
}

/// Whether `n` arguments can be passed to the declaration with this signature (`None`: the
/// signature does not read).
pub fn takes(lang: Lang, sig: Option<&str>, n: usize) -> Option<bool> {
    parse_params(lang, sig?).map(|p| p.takes(n))
}

/// C++ (upstream `cppOverloadFit`): how well a call fits a parameter list. The count first, then
/// each string literal against its parameter: a string type by name over a template parameter
/// that might be one, and a narrow literal never a wide parameter. Unread lists score nothing.
/// Whether a Swift call's argument labels (`_` for none) fit a declaration's parameter list:
/// each parameter's external label in order, one with a default value or a variadic one free
/// to be left out (upstream `swiftLabelsFit`).
pub fn swift_labels_fit(labels: &[&str], sig: &str) -> bool {
    let list = sig.trim().strip_prefix('(').and_then(|s| s.strip_suffix(')')).unwrap_or("");
    let mut i = 0;
    for p in split_top(list).into_iter().map(str::trim).filter(|p| !p.is_empty()) {
        let label = swift_param_label(p);
        if i < labels.len() && labels[i] == label {
            i += 1;
            continue;
        }
        if !(p.contains('=') || p.contains("...")) {
            return false;
        }
    }
    i == labels.len()
}

/// `@escaping inout label name: T` -> `label`; `_ name: T` -> `_`.
fn swift_param_label(p: &str) -> &str {
    let mut rest = p.trim_start();
    while let Some(attr) = rest.strip_prefix('@') {
        let end = attr.find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(attr.len());
        let mut after = &attr[end..];
        if let Some(args) = after.strip_prefix('(') {
            after = args.split_once(')').map_or("", |(_, r)| r);
        }
        rest = after.trim_start();
    }
    rest = rest.strip_prefix("inout ").unwrap_or(rest).trim_start();
    let end = rest.find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(rest.len());
    if end > 0 {
        &rest[..end]
    } else {
        "_"
    }
}

pub fn cpp_fit(sig: Option<&str>, args: &[&str]) -> i32 {
    static WIDE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bw(?:string|char_t|format|string_view)|wchar_t").unwrap());
    static STRINGISH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"string|char|Char|format").unwrap());
    static STRING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"string|char|Char|\bstr\b").unwrap());
    static TEMPLATE_PARAM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?:const\s+)?[A-Z]\w{0,2}\s*[&*]{0,2}\s*\w*$").unwrap());
    let Some(sig) = sig else { return 0 };
    if sig.ends_with(" = delete") {
        return i32::MIN;
    }
    let Some(list) = sig.trim_end_matches(';').trim().strip_prefix('(').and_then(|s| s.strip_suffix(')')) else { return 0 };
    let params: SmallVec<[&str; 6]> = split_top(list).into_iter().map(str::trim).filter(|p| !p.is_empty() && *p != "void").collect();
    let variadic = params.iter().any(|p| cpp_is_pack(p));
    let required = params.iter().filter(|p| !cpp_is_pack(p) && !p.contains('=')).count();
    let mut score = 0;
    if args.len() < required || (!variadic && args.len() > params.len()) {
        score -= 3;
    }
    for (arg, param) in args.iter().zip(&params) {
        if cpp_is_pack(param) {
            break;
        }
        if !matches!(*arg, "S" | "W") {
            continue;
        }
        score += if (*arg == "W") != WIDE.is_match(param) && STRINGISH.is_match(param) {
            -2
        } else if cpp_is_scalar(cpp_param_type(param)) {
            -3
        } else if STRING.is_match(param) {
            3
        } else if TEMPLATE_PARAM.is_match(param) {
            1
        } else {
            -2
        };
    }
    score
}

/// `T&&... args`, not `tuple<T...> t`.
fn cpp_is_pack(p: &str) -> bool {
    cpp_without_templates(p).contains("...")
}

fn cpp_without_templates(p: &str) -> String {
    let mut depth = 0;
    p.chars()
        .filter(|c| {
            match c {
                '<' => depth += 1,
                '>' => depth = (depth - 1).max(0),
                _ => {}
            }
            depth == 0 && *c != '>'
        })
        .collect()
}

/// A parameter's type: no default, no name.
fn cpp_param_type(p: &str) -> &str {
    static NAMED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(.*[\w>*&\s])\b[A-Za-z_]\w*\s*(?:\[[^\]]*\])?$").unwrap());
    let mut depth = 0;
    let mut end = p.len();
    for (i, c) in p.char_indices() {
        match c {
            '<' | '(' | '[' => depth += 1,
            '>' | ')' | ']' => depth -= 1,
            '=' if depth == 0 => {
                end = i;
                break;
            }
            _ => {}
        }
    }
    let text = p[..end].trim();
    match NAMED.captures(text).map(|c| c.get(1).map_or("", |m| m.as_str()).trim()) {
        Some(head)
            if head.contains(|c: char| c.is_ascii_alphabetic())
                && !matches!(head, "const" | "volatile" | "unsigned" | "signed" | "struct" | "class" | "enum") =>
        {
            head
        }
        _ => text,
    }
}

/// A pointer (other than `char*`), number, bool or character: no string literal converts to one
/// (upstream `cppTypeCategory`).
fn cpp_is_scalar(ty: &str) -> bool {
    const CHARS: &[&str] = &["char", "wchar_t", "char8_t", "char16_t", "char32_t"];
    const NUMBERS: &[&str] = &[
        "int",
        "unsigned",
        "signed",
        "short",
        "long",
        "float",
        "double",
        "size_t",
        "ssize_t",
        "ptrdiff_t",
        "int8_t",
        "int16_t",
        "int32_t",
        "int64_t",
        "uint8_t",
        "uint16_t",
        "uint32_t",
        "uint64_t",
        "intptr_t",
        "uintptr_t",
        "size_type",
        "difference_type",
    ];
    let flat = cpp_without_templates(ty);
    let stars = flat.matches('*').count();
    let spaced = flat.replace(['*', '&'], " ");
    let words: SmallVec<[&str; 4]> =
        spaced.split_whitespace().filter(|w| !matches!(*w, "const" | "volatile" | "struct" | "class" | "typename" | "enum")).collect();
    let last = words.last().map_or("", |w| w.rsplit("::").next().unwrap_or(w));
    if stars > 0 {
        return !(CHARS.contains(&last) && stars == 1);
    }
    last == "bool"
        || (CHARS.contains(&last) && words.iter().all(|w| CHARS.contains(w) || matches!(*w, "signed" | "unsigned")))
        || NUMBERS.contains(&last)
        || (!words.is_empty() && words.iter().all(|w| NUMBERS.contains(w)))
}

fn arg_is_project(arg: &ArgTy) -> bool {
    matches!(arg, ArgTy::Named(_, Some(_)))
}

fn relate_or(relate: &Relate<'_>, arg: &ArgTy, pt: &str, unknown: u8) -> Option<u8> {
    match relate(arg, pt) {
        Some(true) => Some(2),
        Some(false) => None,
        None => Some(unknown),
    }
}

/// Fit of a whole overload: `None` when the arguments can't be passed to it, else a score
/// (variable arity ranks below fixed arity, as in Java's third phase).
pub fn fit(family: LangFamily, params: &Params, args: &[ArgTy], type_params: &[&str], relate: &Relate<'_>) -> Option<u32> {
    if !params.takes(args.len()) {
        return None;
    }
    let mut score = 10u32;
    let mut spread = false;
    for (i, a) in args.iter().enumerate() {
        let p = params.param_for(i)?;
        if p.variadic && i + 1 >= params.items.len() {
            spread = true;
            // `f(T... xs)` called with an array passes it whole
            let mut elem = p.clone();
            if args.len() == params.items.len() && matches!(a, ArgTy::Named(t, _) if t.ends_with("[]")) {
                elem.ty = format!("{}[]", p.ty);
            }
            score += fit_arg(family, a, &elem, type_params, relate)? as u32;
        } else {
            score += fit_arg(family, a, p, type_params, relate)? as u32;
        }
    }
    if spread || (params.variadic() && args.len() < params.items.len()) {
        score = score.saturating_sub(2);
    }
    Some(score)
}

/// Index of the overload the arguments pick among the candidates (in preference order: the
/// first of several that fit alike wins), or `None` when none fits.
pub fn choose(
    family: LangFamily,
    sigs: &[Option<Params>],
    args: &[ArgTy],
    type_params: &[SmallVec<[&str; 2]>],
    relate: &dyn Fn(usize, &ArgTy, &str) -> Option<bool>,
) -> Option<usize> {
    let mut best: Option<(usize, u32)> = None;
    for (i, sig) in sigs.iter().enumerate() {
        let Some(sig) = sig else { continue };
        let tp: &[&str] = type_params.get(i).map_or(&[], |v| v.as_slice());
        if let Some(score) = fit(family, sig, args, tp, &|a: &ArgTy, t: &str| relate(i, a, t)) {
            if best.is_none_or(|(_, b)| score > b) {
                best = Some((i, score));
            }
        }
    }
    best.map(|b| b.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none(_: &ArgTy, _: &str) -> Option<bool> {
        None
    }

    fn none_i(_: usize, _: &ArgTy, _: &str) -> Option<bool> {
        None
    }

    fn named(t: &str) -> ArgTy {
        ArgTy::Named(t.into(), None)
    }

    #[test]
    fn parses_signatures() {
        let p = parse_params(Lang::Java, "void (int n, @Deprecated final long m)").unwrap();
        assert_eq!(p.items.iter().map(|p| p.ty.as_str()).collect::<Vec<_>>(), vec!["int", "long"]);
        let p = parse_params(Lang::Java, "<T> void (Map<String, List<T>> m, T... xs)").unwrap();
        assert_eq!(p.items.iter().map(|p| (p.ty.as_str(), p.variadic)).collect::<Vec<_>>(), vec![("Map", false), ("T", true)]);
        let p = parse_params(Lang::Kotlin, "(n: Int, m: Long = 0L, f: (Int) -> Unit)").unwrap();
        assert_eq!(p.items.iter().map(|p| (p.ty.as_str(), p.optional)).collect::<Vec<_>>(), vec![("Int", false), ("Long", true), ("Function", false)]);
        assert_eq!(p.items[2].function, Some(Some(1)));
        let p = parse_params(Lang::Kotlin, "(vararg xs: Int)").unwrap();
        assert!(p.items[0].variadic && p.takes(0) && p.takes(3));
        let p = parse_params(Lang::TypeScript, "(this: Foo, n?: number, cb: (a: string) => void = noop, ...rest: string[]): void").unwrap();
        assert_eq!(
            p.items.iter().map(|p| (p.ty.as_str(), p.optional, p.variadic)).collect::<Vec<_>>(),
            vec![("number", true, false), ("Function", true, false), ("string[]", false, true)]
        );
        assert_eq!(parse_params(Lang::TypeScript, "(a: string, b = 3)").unwrap().items.len(), 2);
        assert_eq!(simplify_type("java.util.List<String>[]"), "List[]");
    }

    #[test]
    fn java_conversions() {
        let one = |sig: &str, args: &[ArgTy]| fit(LangFamily::Jvm, &parse_params(Lang::Java, sig).unwrap(), args, &[], &none);
        assert!(one("void (String s)", &[named("int")]).is_none());
        assert!(one("void (long n)", &[named("int")]).is_some());
        assert!(one("void (Integer n)", &[named("int")]).is_some());
        assert!(one("void (int n)", &[named("long")]).is_none());
        assert!(one("void (Object o)", &[named("int")]).is_some());
        assert!(one("void (int n)", &[ArgTy::Null]).is_none());
        assert!(one("void (Runnable r)", &[ArgTy::Lambda(Some(0))]).is_some());
        assert!(one("void (Function f)", &[ArgTy::Lambda(Some(0))]).is_none());
        assert!(one("void (String s)", &[ArgTy::Lambda(Some(1))]).is_none());
        assert!(one("void (String... s)", &[named("String"), named("String")]).is_some());
        assert!(one("void (String a)", &[named("String"), named("String")]).is_none());
        // the exact type outranks a widening
        let sigs = vec![parse_params(Lang::Java, "void (long n)"), parse_params(Lang::Java, "void (int n)")];
        assert_eq!(choose(LangFamily::Jvm, &sigs, &[named("int")], &[], &none_i), Some(1));
        // fixed arity outranks variable arity
        let sigs = vec![parse_params(Lang::Java, "void (int... n)"), parse_params(Lang::Java, "void (int n)")];
        assert_eq!(choose(LangFamily::Jvm, &sigs, &[named("int")], &[], &none_i), Some(1));
        // nothing fits: nothing chosen
        assert_eq!(choose(LangFamily::Jvm, &sigs, &[named("String"), named("String")], &[], &none_i), None);
        // alike: the first declared stands
        let sigs = vec![parse_params(Lang::Java, "void (Object o)"), parse_params(Lang::Java, "void (Object o)")];
        assert_eq!(choose(LangFamily::Jvm, &sigs, &[ArgTy::Unknown], &[], &none_i), Some(0));
    }

    #[test]
    fn kotlin_and_ts() {
        let k = |sig: &str, args: &[ArgTy]| fit(LangFamily::Jvm, &parse_params(Lang::Kotlin, sig).unwrap(), args, &[], &none);
        assert!(k("(s: String)", &[named("Int")]).is_none());
        assert!(k("(n: Long)", &[named("Int")]).is_some(), "an integer literal adapts");
        assert!(k("(n: Int, m: Long = 0L)", &[named("Int")]).is_some());
        assert!(k("(s: String)", &[ArgTy::Null]).is_none());
        assert!(k("(s: String?)", &[ArgTy::Null]).is_some());
        assert!(k("(f: (Int) -> Unit)", &[ArgTy::Lambda(Some(2))]).is_none());
        let t = |sig: &str, args: &[ArgTy]| fit(LangFamily::Js, &parse_params(Lang::TypeScript, sig).unwrap(), args, &[], &none);
        assert!(t("(s: string): void", &[named("number")]).is_none());
        assert!(t("(n: number, m?: number): void", &[named("number")]).is_some());
        assert!(t("(a: any, b?: any): void", &[named("string"), named("number")]).is_some());
        assert!(t("(cb: () => void)", &[named("string")]).is_none());
        assert!(t("(mode: 'a' | 'b')", &[named("string")]).is_some());
    }
}

#[cfg(test)]
mod cpp_tests {
    use super::cpp_fit;

    fn fit(sig: &str, args: &[&str]) -> i32 {
        cpp_fit(Some(sig), args)
    }

    #[test]
    fn a_format_string_goes_to_the_string_parameter_not_the_style() {
        let plain = "(format_string<T...> fmt, T&&... args)";
        let styled = "(text_style ts, format_string<T...> fmt, T&&... args)";
        assert!(fit(plain, &["S", "?"]) > fit(styled, &["S", "?"]));
        // nothing readable: they tie, and the caller's order decides
        assert_eq!(fit(plain, &["?", "?"]), fit(styled, &["?", "?"]));
    }

    #[test]
    fn swift_labels_pick_the_declaration() {
        assert!(swift_labels_fit(&["_", "task", "didCompleteWithError"], "(_ session: URLSession, task: URLSessionTask, didCompleteWithError error: Error?)"));
        assert!(!swift_labels_fit(&["_", "dataTask", "didReceive"], "(_ session: URLSession, task: URLSessionTask, didCompleteWithError error: Error?)"));
        assert!(swift_labels_fit(&["_"], "(_ url: URL, method: HTTPMethod = .get, @escaping handler: (() -> Void)? = nil)"));
        assert!(!swift_labels_fit(&[], "(_ url: URL)"));
    }

    #[test]
    fn count_width_pointers_and_deleted() {
        assert!(fit("(int a)", &["?", "?"]) < fit("(int a, int b = 0)", &["?", "?"]));
        assert!(fit("(wstring_view s)", &["S"]) < fit("(string_view s)", &["S"]));
        assert!(fit("(wstring_view s)", &["W"]) > fit("(string_view s)", &["W"]));
        assert!(fit("(std::string* out)", &["S"]) < fit("(const char* s)", &["S"]));
        assert!(fit("(std::size_t n)", &["S"]) < fit("(const S& s)", &["S"]));
        assert_eq!(fit("(void)", &[]), 0);
        assert!(fit("(const char* s) = delete", &["S"]) < fit("(int n, int m)", &["S"]));
        assert_eq!(cpp_fit(None, &["S"]), 0);
    }
}
