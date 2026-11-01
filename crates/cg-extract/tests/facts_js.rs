use cg_core::Lang;
use cg_extract::*;

fn run(rel: &str, src: &str, lang: Lang) -> FileBatch {
    let cfg = ExtractConfig::new(".");
    extract_bytes(rel, src.as_bytes(), 0, lang, &cfg)
}

#[test]
fn typescript_imports_exports_bindings() {
    let src = r#"import { Foo, bar as baz, type T } from './foo';
import * as ns from "./ns";
import Def from './def';
import './side';
export { x, y as z } from './x';
export * from './all';
export default Def;
const cjs = require('./cjs');
const { a, b: c } = require('./destructured');

export class Widget extends Base {
  private svc: Service;
  items = new Map();
  constructor(private repo: Repo) { super(); this.svc = new Service(); }
  render(arg: Param): void {
    const helper = new Helper();
    const typed: Other | null = null;
    function localFn() {}
    for (const item of this.items) { item.go(); }
  }
}
"#;
    let b = run("t1.ts", src, Lang::TypeScript);
    assert_eq!(b.flags & fileflags::PARSE_ERROR, 0);
    let f = &b.facts;
    let imp = |local: &str| f.imports.iter().find(|i| i.local == local).unwrap_or_else(|| panic!("no import {local}: {:?}", f.imports));
    assert_eq!((imp("Foo").imported.as_str(), imp("Foo").kind), ("Foo", ImportKind::Named));
    assert_eq!((imp("baz").imported.as_str(), imp("baz").spec.as_str()), ("bar", "./foo"));
    assert!(imp("T").type_only);
    assert_eq!((imp("ns").kind, imp("ns").imported.as_str()), (ImportKind::Namespace, "*"));
    assert_eq!((imp("Def").kind, imp("Def").imported.as_str()), (ImportKind::Default, "default"));
    assert!(f.imports.iter().any(|i| i.kind == ImportKind::SideEffect && i.spec == "./side"));
    assert_eq!(imp("cjs").kind, ImportKind::Require);
    assert_eq!((imp("c").imported.as_str(), imp("c").spec.as_str()), ("b", "./destructured"));
    assert!(f.exports.iter().any(|e| e.name == "z" && e.from_name == "y" && e.from_spec == "./x"));
    assert!(f.exports.iter().any(|e| e.star && e.from_spec == "./all"));
    assert!(f.exports.iter().any(|e| e.name == "default" && e.from_name == "Def"));
    let bind = |name: &str| f.bindings.iter().find(|x| x.name == name).unwrap_or_else(|| panic!("no binding {name}: {:?}", f.bindings));
    assert_eq!(bind("svc").kind, BindKind::Field);
    assert_eq!(bind("svc").decl_type, "Service");
    assert_eq!(bind("items").init_ctor, "Map");
    assert_eq!(bind("repo").kind, BindKind::Field);
    assert_eq!(bind("repo").decl_type, "Repo");
    assert_eq!(bind("arg").kind, BindKind::Param);
    assert_eq!(bind("arg").decl_type, "Param");
    assert_eq!(bind("helper").init_ctor, "Helper");
    assert_eq!(bind("typed").decl_type, "Other");
    assert_eq!(bind("localFn").kind, BindKind::LocalFn);
    assert_eq!(bind("item").kind, BindKind::Local);
    // scopes nest: file -> class -> method -> ...
    assert_eq!(f.scopes[0].kind, ScopeKind::File);
    assert!(f.scopes.iter().any(|s| s.kind == ScopeKind::Class));
    assert!(f.scopes.iter().filter(|s| s.kind == ScopeKind::Function).count() >= 3);
}

#[test]
fn typescript_return_facts_preserve_unknowns_and_declaration_identity() {
    let source = r#"class A {}
function named(): A { return new A(); }
const arrow = (): (A) => new A();
const expression = function (): ns.A { throw 0; };
class Factory {
  make(): A { return new A(); }
}
function union(): A | Other { throw 0; }
function container(): Promise<A> { throw 0; }
function generic<A>(): A { throw 0; }
class Generic<A> { genericMethod(): A { throw 0; } }
async function asyncFactory(): A { throw 0; }
function* generator(): A { throw 0; }
function unannotated() { return new A(); }
"#;
    for (path, lang) in [("returns.ts", Lang::TypeScript), ("returns.tsx", Lang::Tsx)] {
        let batch = run(path, source, lang);
        assert_eq!(batch.flags & fileflags::PARSE_ERROR, 0);
        let rows = batch.rows();
        let facts: Vec<_> = batch.facts.bindings.iter().filter(|b| b.kind == BindKind::ReturnType).collect();
        assert_eq!(facts.len(), 10, "{facts:#?}");
        for (name, expected) in [
            ("named", "A"),
            ("arrow", "A"),
            ("expression", "ns.A"),
            ("make", "A"),
            ("union", ""),
            ("container", ""),
            ("generic", ""),
            ("genericMethod", ""),
            ("asyncFactory", ""),
            ("generator", ""),
        ] {
            let fact = facts.iter().find(|b| b.name == name).unwrap_or_else(|| panic!("missing {name}: {facts:#?}"));
            assert_eq!(fact.decl_type, expected, "{name}");
            assert!(fact.init_ctor.is_empty());
            let node = rows.node(fact.node_row as usize);
            assert_eq!(node.name(), name);
            assert_eq!(node.start_line(), fact.line);
        }
    }
    let js = run("plain.js", "function make() { return new A(); }", Lang::JavaScript);
    assert!(!js.facts.bindings.iter().any(|b| b.kind == BindKind::ReturnType));
}
