use cg_resolve::{index_project, IndexOptions};
use cg_store::{dump, Store};
use cg_sync::{sync, SyncOptions};
use std::path::Path;
use std::time::{Duration, SystemTime};

fn write(root: &Path, name: &str, source: &str, step: u64) {
    let path = root.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, source).unwrap();
    let stamp = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + step);
    std::fs::OpenOptions::new().write(true).open(path).unwrap().set_modified(stamp).unwrap();
}

fn graph(root: &Path) -> Vec<String> {
    let snap = Store::locate(root).unwrap().open_snapshot().unwrap();
    dump::canonical(&snap.view)
}

fn assert_targets(root: &Path, target: &str) {
    let snap = Store::locate(root).unwrap().open_snapshot().unwrap();
    let edges = dump::dump_edges(&snap.view);
    for caller in ["src/owner.rs::Owner::run", "src/caller.rs::call"] {
        let calls: Vec<_> = edges.iter().filter(|e| e.starts_with(&format!("{caller} -calls-> "))).map(String::as_str).collect();
        assert_eq!(calls, [format!("{caller} -calls-> src/worker.rs::{target}::execute")], "{edges:#?}");
    }
    for ty in ["Worker", "Other"] {
        let edge = format!("src/owner.rs::Owner::clone_worker -calls-> src/worker.rs::{ty}::clone");
        assert_eq!(edges.contains(&edge), ty == target, "{edges:#?}");
    }
}

#[test]
fn rust_field_type_swaps_retarget_unchanged_callers_without_full_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let opts = IndexOptions { threads: Some(2), ..Default::default() };
    let mut files = vec![
        ("Cargo.toml".to_owned(), "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n".to_owned()),
        ("src/lib.rs".to_owned(), "pub mod worker;\npub mod owner;\npub mod caller;\n".to_owned()),
        (
            "src/worker.rs".to_owned(),
            "pub struct Worker;\nimpl Worker { pub fn execute(&self) -> u8 { 7 } pub fn clone(&self) -> u8 { 99 } }\npub struct Other;\nimpl Other { pub fn execute(&self) -> u8 { 8 } pub fn clone(&self) -> u8 { 98 } }\n".to_owned(),
        ),
        ("src/caller.rs".to_owned(), "use crate::owner::Owner;\npub fn call(owner: &Owner) -> u8 { owner.worker.execute() }\n".to_owned()),
    ];
    // Exercise a delta: a full rebuild must not conceal missing consumer invalidation.
    for i in 0..48 {
        files.push((format!("src/filler{i}.rs"), format!("fn helper{i}() {{}}\npub fn filler{i}() {{ helper{i}(); }}\n")));
    }
    let owner = |ty: &str| {
        let decoy = if ty == "Other" { "Worker" } else { "Other" };
        format!(
            "use crate::worker::{{Worker, Other}};\npub struct Owner {{ pub worker: {ty} }}\nimpl Owner {{\n    pub fn run(&self) -> u8 {{ self.worker.execute() }}\n    pub fn clone_worker(&self) {{ self.worker.clone(); }}\n}}\nstruct Decoy {{ worker: {decoy} }}\n"
        )
    };
    for (name, source) in &files {
        write(root, name, source, 1);
    }
    write(root, "src/owner.rs", &owner("Worker"), 1);
    index_project(root, &opts).unwrap();
    assert_targets(root, "Worker");
    // Swapping same-named fields between owners must not cancel the fact diff.
    for (i, ty) in ["Other", "Worker", "Other", "Worker"].into_iter().enumerate() {
        let source = owner(ty);
        write(root, "src/owner.rs", &source, 100 + i as u64);
        let report = sync(root, &SyncOptions { index: opts.clone(), ..Default::default() }).unwrap();
        assert!(report.full_rebuild.is_none(), "step {i}: {report:?}");
        assert_targets(root, ty);
        let fresh = tempfile::tempdir().unwrap();
        for (name, contents) in &files {
            write(fresh.path(), name, contents, 1);
        }
        write(fresh.path(), "src/owner.rs", &source, 1);
        index_project(fresh.path(), &opts).unwrap();
        assert_targets(fresh.path(), ty);
        assert_eq!(graph(root), graph(fresh.path()), "step {i}: {ty}");
    }
}
