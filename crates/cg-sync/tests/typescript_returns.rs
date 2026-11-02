use cg_resolve::{index_project, IndexOptions};
use cg_store::{dump, Store};
use cg_sync::{sync, SyncOptions};
use std::path::Path;
use std::time::{Duration, SystemTime};

fn write(root: &Path, name: &str, source: &str, step: u64) {
    let path = root.join(name);
    std::fs::write(&path, source).unwrap();
    let stamp = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + step);
    std::fs::OpenOptions::new().write(true).open(path).unwrap().set_modified(stamp).unwrap();
}

fn canonical(root: &Path) -> Vec<String> {
    let snap = Store::locate(root).unwrap().open_snapshot().unwrap();
    dump::canonical(&snap.view)
}

fn assert_target(root: &Path, target: Option<&str>) {
    let snap = Store::locate(root).unwrap().open_snapshot().unwrap();
    let edges = dump::dump_edges(&snap.view);
    for name in ["A", "B"] {
        let edge = format!("app.ts::use -calls-> types.ts::{name}::run");
        assert_eq!(edges.contains(&edge), target == Some(name), "{edge}: {edges:#?}");
    }
}

#[test]
fn annotation_only_edits_invalidate_consumers_without_full_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let opts = IndexOptions { threads: Some(2), ..Default::default() };
    // Keep enough unrelated files/references that a full rebuild cannot hide
    // missing consumer invalidation. The factory body stays byte-identical.
    for i in 0..40 {
        write(root, &format!("filler{i}.ts"), &format!("function helper{i}() {{}}\nexport function other{i}() {{ helper{i}(); }}\n"), 1);
    }
    write(root, "types.ts", "export class A { run() {} }\nexport class B { run() {} }\n", 1);
    write(root, "barrel.ts", "export { make as create } from './factory';\n", 1);
    write(root, "app.ts", "import { create } from './barrel';\nexport function use() { const a = create(); a.run(); }\n", 1);
    let factory = |annotation: &str| format!("import {{ A, B }} from './types';\nexport function make(){annotation} {{ throw 0; }}\n");
    write(root, "factory.ts", &factory(": A"), 1);
    index_project(root, &opts).unwrap();
    assert_target(root, Some("A"));
    for (i, (annotation, target)) in [
        (": B", Some("B")),
        (": A | B", None),
        (": A", Some("A")),
        (": Promise<A>", None),
        (": A", Some("A")),
        ("", None),
        (": B", Some("B")),
        (": A", Some("A")),
    ]
    .into_iter()
    .enumerate()
    {
        write(root, "factory.ts", &factory(annotation), 100 + i as u64);
        let report = sync(root, &SyncOptions { index: opts.clone(), ..Default::default() }).unwrap();
        assert!(report.full_rebuild.is_none(), "step {i}: {report:?}");
        assert_target(root, target);
        let fresh = tempfile::tempdir().unwrap();
        for entry in std::fs::read_dir(root).unwrap() {
            let entry = entry.unwrap();
            if entry.path().is_file() {
                std::fs::copy(entry.path(), fresh.path().join(entry.file_name())).unwrap();
            }
        }
        index_project(fresh.path(), &opts).unwrap();
        assert_eq!(canonical(root), canonical(fresh.path()), "step {i}: {annotation}");
    }
}

#[test]
fn factory_body_type_edits_preserve_the_imported_return_target() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let opts = IndexOptions { threads: Some(2), ..Default::default() };
    // A body declaration is outside the return annotation's scope. Adding,
    // renaming or removing it must not redirect an unchanged consumer, even
    // when the edit takes the delta path instead of rebuilding the project.
    for i in 0..48 {
        write(root, &format!("filler{i}.ts"), &format!("function helper{i}() {{}}\nexport function other{i}() {{ helper{i}(); }}\n"), 1);
    }
    write(root, "types.ts", "export class A { run() {} }\nexport class B { run() {} }\n", 1);
    write(root, "app.ts", "import { make } from './factory';\nexport function use() { const a = make(); a.run(); }\n", 1);
    let factory = |declaration: &str| format!("import {{ A, B }} from './types';\nexport function make(): A {{\n  {declaration}\n  throw 0;\n}}\n");
    let assert_imported_target = |root: &Path| {
        let snap = Store::locate(root).unwrap().open_snapshot().unwrap();
        let edges = dump::dump_edges(&snap.view);
        let targets: Vec<_> = edges.iter().filter(|edge| edge.starts_with("app.ts::use -calls-> ") && edge.ends_with("::run")).map(String::as_str).collect();
        assert_eq!(targets, ["app.ts::use -calls-> types.ts::A::run"], "{edges:#?}");
    };
    write(root, "factory.ts", &factory(""), 1);
    index_project(root, &opts).unwrap();
    assert_imported_target(root);
    for (i, declaration) in [
        "class A { run() {} }",
        "class Local { run() {} }",
        "",
        "interface A { run(): void; }",
        "interface Local { run(): void; }",
        "",
        "type A = B;",
        "type Local = B;",
        "",
    ]
    .into_iter()
    .enumerate()
    {
        write(root, "factory.ts", &factory(declaration), 100 + i as u64);
        let report = sync(root, &SyncOptions { index: opts.clone(), ..Default::default() }).unwrap();
        assert!(report.full_rebuild.is_none(), "step {i}: {declaration}: {report:?}");
        assert_imported_target(root);
        let fresh = tempfile::tempdir().unwrap();
        for entry in std::fs::read_dir(root).unwrap() {
            let entry = entry.unwrap();
            if entry.path().is_file() {
                std::fs::copy(entry.path(), fresh.path().join(entry.file_name())).unwrap();
            }
        }
        index_project(fresh.path(), &opts).unwrap();
        assert_imported_target(fresh.path());
        assert_eq!(canonical(root), canonical(fresh.path()), "step {i}: {declaration}");
    }
}
