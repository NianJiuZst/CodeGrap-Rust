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
