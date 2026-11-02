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
fn typescript_satisfies_preserves_initializer_evidence() {
    let src = r#"const direct = new Service() satisfies Runner;
const factory = makeService() satisfies Runner;
const grouped = (new Service() satisfies Runner) satisfies object;
const asserted = (new Service() as Runner) satisfies object;
const cast = new Service() as Runner;
const unknown = externalValue satisfies Runner;
const annotated: Runner = new Service() satisfies object;
class Holder { service = new Service() satisfies Runner; }
"#;
    for lang in [Lang::TypeScript, Lang::Tsx] {
        let b = run("satisfies.ts", src, lang);
        assert_eq!(b.flags & (fileflags::PARSE_ERROR | fileflags::PANICKED), 0, "{:?}", b.errors);
        for (name, decl_type, init_ctor) in [
            ("direct", "", "\0js:satisfies:Service"),
            ("factory", "", "\0js:satisfies:makeService()"),
            ("grouped", "", "\0js:satisfies:Service"),
            ("asserted", "Runner", ""),
            ("cast", "Runner", ""),
            ("unknown", "", "\0js:satisfies:\0js:binding:externalValue"),
            ("annotated", "Runner", "\0js:satisfies:Service"),
            ("service", "", "\0js:satisfies:Service"),
        ] {
            let binding = b.facts.bindings.iter().find(|binding| binding.name == name).unwrap();
            assert_eq!((binding.decl_type.as_str(), binding.init_ctor.as_str()), (decl_type, init_ctor), "{lang:?} {name}");
        }
    }
}
