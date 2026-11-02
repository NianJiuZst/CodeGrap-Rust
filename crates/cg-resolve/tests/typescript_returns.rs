use cg_resolve::{build_seg, IndexOptions};
use cg_store::{dump, View};
use std::sync::Arc;

fn edges(files: &[(&str, &str)]) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    for (name, text) in files {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    let built = build_seg(dir.path(), &IndexOptions { threads: Some(2), ..Default::default() }).unwrap();
    dump::dump_edges(&View::new(Arc::new(built.seg)))
}

#[test]
fn typescript_factory_annotations_resolve_the_declared_receiver() {
    for ext in ["ts", "tsx"] {
        let graph = edges(&[
            ("types.ts", "export class A { run() {} }\nexport class B { run() {} }\n"),
            (
                "factory.ts",
                r#"import { A as Result } from './types';
import * as types from './types';
export function make(): Result { return new Result(); }
export const arrow = (): Result => new Result();
export const expression = function (): types.A { return new Result(); };
export class Factory {
  make(): types.A { return new Result(); }
  static build(): Result { return new Result(); }
}
"#,
            ),
            ("barrel.ts", "export { make as create, arrow, expression, Factory } from './factory';\n"),
            (
                &format!("app.{ext}"),
                r#"import { A } from './types';
import { create, arrow, expression, Factory } from './barrel';
export function direct() { const a = new A(); a.run(); }
export function named() { const a = create(); a.run(); }
export function chained() { create().run(); }
export function fromArrow() { const a = arrow(); a.run(); }
export function fromExpression() { const a = expression(); a.run(); }
export function fromMethod() { const factory = new Factory(); const a = factory.make(); a.run(); }
export function fromStatic() { const a = Factory.build(); a.run(); }
"#,
            ),
        ]);
        for caller in ["direct", "named", "chained", "fromArrow", "fromExpression", "fromMethod", "fromStatic"] {
            let prefix = format!("app.{ext}::{caller} -calls-> types.ts::");
            assert!(graph.contains(&format!("{prefix}A::run")), "{caller} missing A.run: {graph:#?}");
            assert!(!graph.contains(&format!("{prefix}B::run")), "{caller} must not reach B.run: {graph:#?}");
        }
    }
}

#[test]
fn nested_factories_keep_their_own_return_annotation() {
    let graph = edges(&[(
        "app.ts",
        r#"class A { run() {} }
class B { run() {} }
export function first() {
  function make(): A { return new A(); }
  const value = make(); value.run();
}
export function second() {
  function make(): B { return new B(); }
  const value = make(); value.run();
}
export function firstArrow() {
  const create = (): A => new A();
  const value = create(); value.run();
}
export function secondArrow() {
  const create = (): B => new B();
  const value = create(); value.run();
}
"#,
    )]);
    for (caller, expected, wrong) in [("first", "A", "B"), ("second", "B", "A"), ("firstArrow", "A", "B"), ("secondArrow", "B", "A")] {
        assert!(graph.contains(&format!("app.ts::{caller} -calls-> app.ts::{expected}::run")), "{graph:#?}");
        assert!(!graph.contains(&format!("app.ts::{caller} -calls-> app.ts::{wrong}::run")), "{graph:#?}");
    }
}

#[test]
fn unsupported_factory_returns_do_not_guess_a_unique_member() {
    // Only one `run` exists: ambiguity must not hide a global-name fallback.
    for (declaration, invocation) in [
        ("function make(): A | Other { throw 0; }", "make()"),
        ("function make(): A & Other { throw 0; }", "make()"),
        ("function make(): A[] { return []; }", "make()"),
        ("function make(): Promise<A> { throw 0; }", "make()"),
        ("async function make(): Promise<A> { throw 0; }", "make()"),
        ("function* make(): Generator<A> { throw 0; }", "make()"),
        ("function make(): External { throw 0; }", "make()"),
        ("function make(): unknown { throw 0; }", "make()"),
        ("function make(): any { throw 0; }", "make()"),
        ("function make(): void {}", "make()"),
        ("function make(): never { throw 0; }", "make()"),
        ("function make<A>(): A { throw 0; }", "make()"),
        ("class Factory<A> { make(): A { throw 0; } }\nconst factory = new Factory();", "factory.make()"),
    ] {
        let source = format!(
            "import {{ External }} from 'external';\nclass A {{ run() {{}} }}\nclass Other {{}}\n{declaration}\nexport function use() {{ const a = {invocation}; a.run(); }}\n"
        );
        let graph = edges(&[("app.ts", &source)]);
        assert!(!graph.contains(&"app.ts::use -calls-> app.ts::A::run".to_owned()), "{declaration}: {graph:#?}");
    }
}

#[test]
fn return_names_obey_the_factory_scope_and_imports() {
    let graph = edges(&[
        ("unrelated.ts", "export class Missing { unique() {} }\n"),
        (
            "app.ts",
            r#"class A { run() {} }
export function outer() {
  class A { run() {} }
  function make(): A { return new A(); }
  const a = make(); a.run();
}
function missing(): Missing { throw 0; }
export function useMissing() { const a = missing(); a.unique(); }
"#,
        ),
    ]);
    assert!(graph.contains(&"app.ts::outer -calls-> app.ts::outer::A::run".to_owned()), "{graph:#?}");
    assert!(!graph.contains(&"app.ts::outer -calls-> app.ts::A::run".to_owned()), "{graph:#?}");
    assert!(!graph.contains(&"app.ts::useMissing -calls-> unrelated.ts::Missing::unique".to_owned()), "{graph:#?}");
}

#[test]
fn imported_return_annotations_use_the_type_namespace() {
    let graph = edges(&[
        (
            "types.ts",
            "export interface Result { run(): void; }\nexport class Wrong { run() {} }\nexport function Result(): Wrong { return new Wrong(); }\n",
        ),
        ("barrel.ts", "export { Result as Renamed } from './types';\n"),
        (
            "factory.ts",
            "import { Renamed as ReturnType } from './barrel';\nimport * as types from './barrel';\nexport function named(): ReturnType { throw 0; }\nexport function qualified(): types.Renamed { throw 0; }\n",
        ),
        (
            "app.ts",
            "import { named, qualified } from './factory';\nimport { Renamed } from './barrel';\nexport function fromNamed() { const value = named(); value.run(); }\nexport function fromQualified() { const value = qualified(); value.run(); }\nexport function fromValue() { const value = Renamed(); value.run(); }\n",
        ),
    ]);
    for (caller, target) in [("fromNamed", "Result"), ("fromQualified", "Result"), ("fromValue", "Wrong")] {
        let prefix = format!("app.ts::{caller} -calls-> ");
        let targets: Vec<_> = graph.iter().filter(|edge| edge.starts_with(&prefix) && edge.ends_with("::run")).collect();
        let expected = format!("{prefix}types.ts::{target}::run");
        assert_eq!(targets, vec![&expected], "{caller}: {graph:#?}");
    }
}

#[test]
fn function_body_types_do_not_shadow_return_annotations() {
    for declaration in ["class Result { run() {} }", "interface Result { run(): void }", "type Result = { run(): void };"] {
        // Each body declares the same name as its return annotation, but body
        // declarations are not visible in the function's signature.
        let factories = format!(
            r#"function make(): Result {{ {declaration} throw 0; }}
const arrow = (): Result => {{ {declaration} throw 0; }};
const expression = function (): Result {{ {declaration} throw 0; }};
class Factory {{ make(): Result {{ {declaration} throw 0; }} }}
function fromNamed() {{ const value = make(); value.run(); }}
function fromArrow() {{ const value = arrow(); value.run(); }}
function fromExpression() {{ const value = expression(); value.run(); }}
function fromMethod() {{ const factory = new Factory(); const value = factory.make(); value.run(); }}
"#
        );
        let nested_factories = factories.replace("Factory", "NestedFactory");
        let source = format!(
            r#"import {{ A as Result }} from './types';
{factories}
export function outer() {{
  class Result {{ run() {{}} }}
  {nested_factories}
}}
"#
        );
        let graph = edges(&[
            // Two project methods ensure a missing return type cannot pass
            // through a unique-member fallback.
            ("types.ts", "export class A { run() {} }\nexport class B { run() {} }\n"),
            ("app.ts", &source),
        ]);
        for (scope, target) in [("", "types.ts::A::run"), ("outer::", "app.ts::outer::Result::run")] {
            for caller in ["fromNamed", "fromArrow", "fromExpression", "fromMethod"] {
                let prefix = format!("app.ts::{scope}{caller} -calls-> ");
                let calls: Vec<_> = graph.iter().filter(|edge| edge.starts_with(&prefix) && edge.ends_with("::run")).collect();
                let expected = format!("{prefix}{target}");
                assert_eq!(calls, vec![&expected], "{declaration}, {scope}{caller}: {graph:#?}");
            }
        }
    }
}
