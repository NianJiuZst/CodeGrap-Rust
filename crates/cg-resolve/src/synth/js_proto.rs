//! ES5 inheritance between constructor functions, written as a call rather than `extends`:
//! `util.inherits(Child, Parent)`, `Child.prototype = Object.create(Parent.prototype)`,
//! `Object.setPrototypeOf(Child.prototype, Parent.prototype)` and `Child.prototype.__proto__ =
//! Parent.prototype`. Each becomes an `extends` reference from the child constructor's node, so
//! the hierarchy (and member lookup through it) treats the constructors as it treats classes.

use cg_core::{EdgeKind, Lang, LangFamily, NodeKind, RefKind};
use cg_extract::{ExtraRef, FileHook, HookCtx, HookOut};
use regex::Regex;
use std::sync::OnceLock;

fn inherit_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        let id = r"[A-Za-z_$][\w$]*";
        let path = r"[A-Za-z_$][\w$]*(?:\.[A-Za-z_$][\w$]*)*";
        Regex::new(&format!(
            r"(?:\binherits\s*\(\s*({id})\s*,\s*({path})\s*\))|(?:\b({id})\.prototype\s*=\s*Object\.create\(\s*({path})\.prototype\b)|(?:Object\.setPrototypeOf\(\s*({id})\.prototype\s*,\s*({path})\.prototype\s*\))|(?:\b({id})\.prototype\.__proto__\s*=\s*({path})\.prototype\b)"
        ))
        .expect("static regex")
    })
}

pub struct InheritScanner;

impl FileHook for InheritScanner {
    fn name(&self) -> &'static str {
        "es5-inherits"
    }
    fn applies(&self, lang: Lang, _rel: &str) -> bool {
        lang.family() == LangFamily::Js
    }
    fn triggers(&self) -> &'static [&'static str] {
        &["inherits", "Object.create", "setPrototypeOf", "__proto__"]
    }
    fn run(&self, ctx: &HookCtx<'_>, out: &mut HookOut) {
        if super::scan::is_minified(ctx.src) {
            return;
        }
        let safe = cg_extract::post::strip_js_comments(ctx.src);
        let lines = super::scan::LineIndex::new(&safe);
        for c in inherit_re().captures_iter(&safe) {
            let pair = (1..=7).step_by(2).find_map(|i| Some((c.get(i)?, c.get(i + 1)?)));
            let Some((child, parent)) = pair else { continue };
            if child.as_str() == parent.as_str() {
                continue;
            }
            // the child constructor declared in this file
            let row = ctx.rows.nodes().enumerate().find(|(_, n)| n.kind() == NodeKind::Function && n.name() == child.as_str()).map(|(i, _)| i as u32);
            let Some(row) = row else { continue };
            out.refs.push(ExtraRef {
                from_row: row,
                kind: RefKind::Edge(EdgeKind::Extends),
                name: parent.as_str().to_owned(),
                line: lines.line(parent.start()),
                col: lines.col(parent.start()),
            });
        }
    }
}
