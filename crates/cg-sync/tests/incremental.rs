//! "Incremental equals full" (design doc 9.6): after every edit, the view the sync produces must
//! be the same graph as a fresh index of the same tree.

use cg_resolve::{index_project, IndexOptions};
use cg_store::{dump, Store};
use cg_sync::{sync, SyncOptions};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const FILES: &[(&str, &str)] = &[
    ("ts/service.ts", "export class Service {\n  run() { return helper(); }\n  stop() { return 0; }\n}\nexport function helper() { return 1; }\n"),
    ("ts/util.ts", "export function util() { return helper2(); }\nfunction helper2() { return 2; }\n"),
    ("ts/barrel.ts", "export * from './service';\nexport { util } from './util';\n"),
    ("ts/base.ts", "export class Base {\n  base() { return 3; }\n}\n"),
    ("ts/widget.ts", "import { Service, util } from './barrel';\nimport { Base } from './base';\nexport class Widget extends Base {\n  svc = new Service();\n  render() {\n    this.svc.run();\n    util();\n    this.base();\n  }\n}\n"),
    ("ts/main.ts", "import { Widget } from './widget';\nimport { Missing } from './missing';\nimport './sidefx';\nconst w = new Widget();\nw.render();\nw.base();\nlater();\nnew Missing();\n"),
    ("ts/types.ts", "export class A1 {\n  m() { return 1; }\n}\nexport class B1 {\n  m() { return 2; }\n}\n"),
    ("ts/factory.ts", "import { A1, B1 } from './types';\nexport function make(): A1 {\n  return new A1();\n}\nexport const unused = B1;\n"),
    ("ts/mk.ts", "import { Widget } from './widget';\nexport function mkWidget(): Widget {\n  return new Widget();\n}\n"),
    ("ts/m2.ts", "import { mkWidget } from './mk';\nmkWidget().base();\n"),
    ("ts/barrel2.ts", "export * from './barrel';\n"),
    ("ts/viab2.ts", "import { util } from './barrel2';\nutil();\n"),
    ("ts/svc.ts", "import { A1 } from './types';\nexport class Svc {\n  repo: A1 = new A1();\n}\n"),
    ("ts/getsvc.ts", "import { Svc } from './svc';\nexport function getSvc(): Svc {\n  return new Svc();\n}\n"),
    ("ts/usesvc.ts", "import { getSvc } from './getsvc';\ngetSvc().repo.m();\n"),
    ("ts/pick/index.ts", "export class Pick1 {\n  go() { return 1; }\n}\n"),
    ("ts/picker.ts", "import { Pick1 } from './pick';\nconst p = new Pick1();\np.go();\n"),
    ("ts/use.ts", "import { make } from './factory';\nconst f = make();\nf.m();\n"),
    ("py/pkg/__init__.py", ""),
    ("py/pkg/core.py", "class Engine:\n    def start(self):\n        return self.helper()\n\n    def helper(self):\n        return 1\n"),
    ("py/pkg/util.py", "def util():\n    return 2\n"),
    ("py/pkg/app.py", "from .core import Engine\nfrom .util import util\nfrom .nothere import thing\n\n\ndef main():\n    e = Engine()\n    e.start()\n    util()\n    thing()\n    later_py()\n"),
    ("go/go.mod", "module example.com/m\n\ngo 1.21\n"),
    ("go/a/a.go", "package a\n\nfunc Do() int {\n\treturn helper()\n}\n\nfunc helper() int { return 1 }\n\ntype T struct{}\n\nfunc (t *T) Run() { Do() }\n"),
    ("go/b/b.go", "package b\n\nimport \"example.com/m/a\"\n\nfunc Use() {\n\ta.Do()\n\ta.Later()\n\tt := &a.T{}\n\tt.Run()\n}\n"),
    ("java/com/ex/Shape.java", "package com.ex;\n\npublic interface Shape {\n    double area();\n}\n"),
    ("java/com/ex/Circle.java", "package com.ex;\n\npublic class Circle implements Shape {\n    public double area() { return 3.14; }\n    public void grow() { area(); }\n}\n"),
    ("java/com/ex/Main.java", "package com.ex;\n\npublic class Main {\n    public static void main(String[] args) {\n        Circle c = new Circle();\n        c.area();\n        c.grow();\n        new Later().go();\n    }\n}\n"),
    ("rs/Cargo.toml", "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
    ("rs/src/lib.rs", "mod util;\n\npub fn run() {\n    util::go();\n    util::later();\n}\n\npub struct Thing;\n\nimpl Thing {\n    pub fn new() -> Thing { Thing }\n    pub fn act(&self) { run(); }\n}\n"),
    ("rs/src/util.rs", "pub fn go() {\n    inner();\n}\n\nfn inner() {}\n"),
    ("kt/A.kt", "package demo\n\nopen class A {\n    fun hello(): Int = 1\n}\n"),
    ("kt/B.kt", "package demo\n\nclass B : A() {\n    fun use() {\n        hello()\n    }\n}\n"),
    ("ui/App.tsx", "import { Child } from './Child';\nimport { Other } from './Other';\nexport function App() {\n  return <div><Child /><Other /></div>;\n}\n"),
    ("ui/Child.tsx", "export function Child() {\n  return <span />;\n}\n"),
    ("ui/Other.tsx", "export const Other = () => <b />;\n"),
    ("ui/Comp.jsx", "import React from 'react';\nexport class Comp extends React.Component {\n  handle() {\n    this.setState({ a: 1 });\n  }\n  render() {\n    return null;\n  }\n}\n"),
    ("bus/bus.ts", "import { EventEmitter } from 'events';\nexport const bus = new EventEmitter();\nexport function fire() {\n  bus.emit('tick');\n}\n"),
    ("bus/handler.ts", "import { bus } from './bus';\nexport function onTick() {}\nbus.on('tick', onTick);\n"),
    ("java/com/ex/Square.java", "package com.ex;\n\npublic class Square implements Shape {\n    public double area() { return 4; }\n}\n"),
    ("go/a/greeter.go", "package a\n\ntype Greeter interface {\n\tGreet() string\n\tName() string\n}\n\ntype En struct{}\n\nfunc (e *En) Greet() string { return \"hi\" }\nfunc (e *En) Name() string { return \"en\" }\n"),
    ("kt/C.kt", "package demo\n\nclass C {\n    fun f() {\n        val b = B()\n        b.hello()\n        b.use()\n    }\n}\n"),
];

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn write(root: &Path, rel: &str, content: &str, step: u64) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, content).unwrap();
    bump(&p, step);
}

fn bump(p: &Path, step: u64) {
    let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + step * 7) + Duration::from_nanos(step * 1013);
    let f = fs::OpenOptions::new().write(true).open(p).unwrap();
    f.set_modified(t).unwrap();
}

fn copy_tree(src: &Path, dst: &Path) {
    for e in fs::read_dir(src).unwrap().flatten() {
        let name = e.file_name();
        if name == ".cgr" {
            continue;
        }
        let (s, d) = (e.path(), dst.join(&name));
        if s.is_dir() {
            fs::create_dir_all(&d).unwrap();
            copy_tree(&s, &d);
        } else {
            fs::copy(&s, &d).unwrap();
        }
    }
}

fn opts() -> IndexOptions {
    IndexOptions { threads: Some(2), ..Default::default() }
}

fn canonical_of(root: &Path) -> Vec<String> {
    let store = Store::locate(root).expect("index");
    let snap = store.open_snapshot().expect("snapshot");
    dump::canonical(&snap.view)
}

fn full_canonical(src: &Path) -> Vec<String> {
    let oracle = tempfile::tempdir().unwrap();
    copy_tree(src, oracle.path());
    index_project(oracle.path(), &opts()).unwrap();
    canonical_of(oracle.path())
}

fn first_diff(a: &[String], b: &[String]) -> String {
    let mut out = String::new();
    let (sa, sb): (std::collections::BTreeSet<&String>, std::collections::BTreeSet<&String>) = (a.iter().collect(), b.iter().collect());
    let only_a: Vec<_> = sa.difference(&sb).take(8).collect();
    let only_b: Vec<_> = sb.difference(&sa).take(8).collect();
    out.push_str(&format!("{} vs {} lines\n-- only incremental:\n", a.len(), b.len()));
    for l in only_a {
        out.push_str(&format!("  {l}\n"));
    }
    out.push_str("-- only full:\n");
    for l in only_b {
        out.push_str(&format!("  {l}\n"));
    }
    out
}

fn new_file(lang: &str, n: usize, callee: &str) -> (String, String) {
    match lang {
        "ts" => (format!("ts/gen{n}.ts"), format!("import {{ helper }} from './service';\nexport function gen{n}() {{ return helper() + {callee}(); }}\n")),
        "py" => (format!("py/pkg/gen{n}.py"), format!("from .util import util\n\n\ndef gen{n}():\n    return util()\n")),
        "go" => (format!("go/a/gen{n}.go"), format!("package a\n\nfunc Gen{n}() int {{ return Do() }}\n")),
        "java" => {
            (format!("java/com/ex/Gen{n}.java"), format!("package com.ex;\n\npublic class Gen{n} {{\n    public void run() {{ new Circle().area(); }}\n}}\n"))
        }
        "rs" => (format!("rs/src/gen{n}.rs"), "pub fn extra() { crate::run(); }\n".to_string()),
        _ => (format!("kt/G{n}.kt"), format!("package demo\n\nclass G{n} {{\n    fun go() {{ A().hello() }}\n}}\n")),
    }
}

fn mutate(root: &Path, rng: &mut Rng, step: u64, log: &mut Vec<String>) {
    let mut files: Vec<String> = Vec::new();
    fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
        for e in fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if e.file_name() == ".cgr" {
                continue;
            }
            if p.is_dir() {
                walk(&p, base, out);
            } else {
                out.push(p.strip_prefix(base).unwrap().to_string_lossy().replace('\\', "/"));
            }
        }
    }
    walk(root, root, &mut files);
    files.sort();
    let src_files: Vec<&String> = files.iter().filter(|f| !f.ends_with(".toml") && !f.ends_with("go.mod")).collect();
    let pick = |rng: &mut Rng| -> String { src_files[rng.below(src_files.len())].clone() };
    let kind = rng.below(10);
    match kind {
        0 | 1 => {
            // append a function that calls something
            let f = pick(rng);
            let cur = fs::read_to_string(root.join(&f)).unwrap();
            let add = match f.rsplit('.').next().unwrap() {
                "ts" => format!("\nexport function extra{step}() {{ return helper() + util(); }}\n"),
                "py" => format!("\n\ndef extra{step}():\n    return util()\n"),
                "go" => format!("\nfunc Extra{step}() {{ Do() }}\n"),
                "java" => return mutate(root, rng, step, log),
                "rs" => format!("\npub fn extra{step}() {{ run(); }}\n"),
                "kt" => format!("\nfun extra{step}() = A().hello()\n"),
                _ => return mutate(root, rng, step, log),
            };
            log.push(format!("append {f}"));
            write(root, &f, &(cur + &add), step);
        }
        2 => {
            let f = pick(rng);
            let cur = fs::read_to_string(root.join(&f)).unwrap();
            let c = match f.rsplit('.').next().unwrap() {
                "py" => "# note\n",
                _ => "// note\n",
            };
            log.push(format!("comment {f}"));
            write(root, &f, &format!("{c}{cur}"), step);
        }
        3 => {
            // rename a known definition in its own file only (callers dangle)
            let table: &[(&str, &str)] = &[
                ("ts/service.ts", "helper"),
                ("ts/util.ts", "util"),
                ("py/pkg/util.py", "util"),
                ("go/a/a.go", "Do"),
                ("rs/src/util.rs", "go"),
                ("ts/base.ts", "base"),
                ("kt/A.kt", "hello"),
            ];
            let (f, name) = table[rng.below(table.len())];
            if let Ok(cur) = fs::read_to_string(root.join(f)) {
                if cur.contains(name) {
                    log.push(format!("rename {name} in {f}"));
                    write(root, f, &cur.replace(name, &format!("{name}_v{step}")), step);
                }
            }
        }
        4 => {
            // remove a file
            if src_files.len() > 8 {
                let f = pick(rng);
                log.push(format!("remove {f}"));
                fs::remove_file(root.join(&f)).unwrap();
            }
        }
        5 | 6 => {
            let lang = ["ts", "py", "go", "java", "rs", "kt"][rng.below(6)];
            let (rel, content) = new_file(lang, step as usize, "util");
            log.push(format!("add {rel}"));
            write(root, &rel, &content, step);
        }
        7 => {
            let f = pick(rng);
            log.push(format!("touch {f}"));
            bump(&root.join(&f), step);
        }
        8 => {
            // change inheritance / interface
            let choices: &[(&str, &str, &str)] = &[
                ("ts/widget.ts", "extends Base", ""),
                ("ts/widget.ts", "export class Widget {", "export class Widget extends Base {"),
                ("kt/B.kt", ": A()", ""),
                ("java/com/ex/Circle.java", " implements Shape", ""),
            ];
            let (f, from, to) = choices[rng.below(choices.len())];
            if let Ok(cur) = fs::read_to_string(root.join(f)) {
                if cur.contains(from) {
                    log.push(format!("hierarchy {f}: {from:?} -> {to:?}"));
                    write(root, f, &cur.replacen(from, to, 1), step);
                }
            }
        }
        _ => {
            // restore an original file's content (undo)
            let (f, c) = FILES[rng.below(FILES.len())];
            if !f.ends_with(".toml") && f != "go/go.mod" {
                log.push(format!("restore {f}"));
                write(root, f, c, step);
            }
        }
    }
}

fn run_sequence(seed: u64, steps: usize) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for (i, (f, c)) in FILES.iter().enumerate() {
        write(root, f, c, i as u64);
    }
    index_project(root, &opts()).unwrap();
    assert_eq!(canonical_of(root), full_canonical(root), "fresh index must equal itself");
    let mut rng = Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1);
    let mut log: Vec<String> = Vec::new();
    let mut deltas = 0;
    for step in 1..=steps as u64 {
        let before = log.len();
        mutate(root, &mut rng, 100 + step, &mut log);
        let rep = sync(root, &SyncOptions { index: opts(), ..Default::default() }).unwrap_or_else(|e| panic!("sync failed at step {step}: {e}\n{log:#?}"));
        if rep.full_rebuild.is_none() && !rep.up_to_date {
            deltas += 1;
        }
        let inc = canonical_of(root);
        let full = full_canonical(root);
        assert!(inc == full, "seed {seed} step {step} ({:?}) diverged:\n{}\nlog: {log:#?}", &log[before..], first_diff(&inc, &full));
    }
    assert!(deltas > steps / 3, "most steps should have used a delta layer ({deltas}/{steps})");
}

type Edit = Box<dyn Fn(&Path, u64)>;

#[test]
fn watcher_paths_match_full_after_edits_renames_and_directory_events() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for (i, (f, c)) in FILES.iter().enumerate() {
        write(root, f, c, i as u64);
    }
    index_project(root, &opts()).unwrap();
    let edits: Vec<(&str, Edit, Vec<&str>)> = vec![
        ("edit", replace_in("ts/service.ts", "return 1", "return 22"), vec!["ts/service.ts"]),
        ("add", put("ts/fresh.ts", "import { helper } from './service';\nexport const fresh = helper();\n"), vec!["ts/fresh.ts"]),
        ("rename old", delete("ts/fresh.ts"), vec!["ts/fresh.ts", "ts/renamed.ts"]),
        ("rename new", put("ts/renamed.ts", "export function fresh() {}\n"), vec!["ts/renamed.ts"]),
        ("register", put("bus/reg.ts", "import { bus } from './bus';\nimport { onTick } from './handler';\nbus.on('tock', onTick);\n"), vec!["bus/reg.ts"]),
        ("emit", replace_in("bus/bus.ts", "'tick'", "'tock'"), vec!["bus/bus.ts"]),
        ("directory", put("folder/child/new.ts", "export function nested() {}\n"), vec!["folder"]),
        ("config", put("package.json", "{\"name\":\"fixture\",\"dependencies\":{\"express\":\"4\"}}\n"), vec!["package.json"]),
    ];
    for (step, (name, edit, paths)) in edits.into_iter().enumerate() {
        edit(root, 900 + step as u64);
        let paths = paths.into_iter().map(|p| root.join(p)).collect();
        sync(root, &SyncOptions { index: opts(), paths: Some(paths), ..Default::default() }).unwrap();
        assert_eq!(canonical_of(root), full_canonical(root), "event {name}");
    }
    fs::remove_dir_all(root.join("folder")).unwrap();
    sync(root, &SyncOptions { index: opts(), paths: Some(vec![root.join("folder")]), ..Default::default() }).unwrap();
    assert_eq!(canonical_of(root), full_canonical(root), "removed directory");
    // An absent event is recovered by the periodic / explicit full scan.
    write(root, "ts/unreported.ts", "export function recovered() {}\n", 999);
    sync(root, &SyncOptions { index: opts(), paths: Some(vec![]), ..Default::default() }).unwrap();
    assert!(!canonical_of(root).iter().any(|s| s.contains("recovered")));
    sync(root, &SyncOptions { index: opts(), ..Default::default() }).unwrap();
    assert_eq!(canonical_of(root), full_canonical(root));
}

fn replace_in(rel: &'static str, from: &'static str, to: &'static str) -> Edit {
    Box::new(move |root, step| {
        let cur = fs::read_to_string(root.join(rel)).unwrap();
        assert!(cur.contains(from), "{rel} lacks {from:?}");
        write(root, rel, &cur.replace(from, to), step);
    })
}

fn put(rel: &'static str, content: &'static str) -> Edit {
    Box::new(move |root, step| write(root, rel, content, step))
}

fn delete(rel: &'static str) -> Edit {
    Box::new(move |root, _| fs::remove_file(root.join(rel)).unwrap())
}

fn original(rel: &'static str) -> Edit {
    let c = FILES.iter().find(|f| f.0 == rel).unwrap().1;
    put(rel, c)
}

/// Targeted edits, each checked against a fresh index: the cases where a missed affected
/// reference would show.
#[test]
fn targeted_edits_match_full_rebuild() {
    let scenarios: Vec<(&str, Edit)> = vec![
        ("rename ts def", replace_in("ts/service.ts", "helper", "helper_x")),
        ("restore ts def", original("ts/service.ts")),
        ("remove barrel", delete("ts/barrel.ts")),
        ("re-add barrel", original("ts/barrel.ts")),
        ("barrel drops util", put("ts/barrel.ts", "export * from './service';\n")),
        ("barrel back", original("ts/barrel.ts")),
        ("rename method base", replace_in("ts/base.ts", "base()", "root()")),
        ("rename method back", original("ts/base.ts")),
        ("remove base class file", delete("ts/base.ts")),
        ("add base class back", original("ts/base.ts")),
        ("widget stops extending", replace_in("ts/widget.ts", " extends Base", "")),
        ("widget extends again", original("ts/widget.ts")),
        ("duplicate helper elsewhere", put("ts/dup.ts", "export function helper() { return 9; }\n")),
        ("remove duplicate", delete("ts/dup.ts")),
        ("py remove util", delete("py/pkg/util.py")),
        ("py re-add util", original("py/pkg/util.py")),
        ("py import from new module", replace_in("py/pkg/app.py", "from .util import util", "from .util2 import util")),
        ("py add util2", put("py/pkg/util2.py", "def util():\n    return 7\n")),
        ("py engine drops helper", replace_in("py/pkg/core.py", "def helper(self):", "def other(self):")),
        ("py engine restored", original("py/pkg/core.py")),
        ("go add Do2", put("go/a/b2.go", "package a\n\nfunc Do2() int { return 2 }\n")),
        ("go use Do2", replace_in("go/b/b.go", "a.Do()", "a.Do2()")),
        ("go remove Do2", delete("go/a/b2.go")),
        ("go restore b", original("go/b/b.go")),
        ("java drop implements", replace_in("java/com/ex/Circle.java", " implements Shape", "")),
        ("java implements again", original("java/com/ex/Circle.java")),
        ("java remove Circle", delete("java/com/ex/Circle.java")),
        ("java add Circle", original("java/com/ex/Circle.java")),
        ("rs rename util fn", replace_in("rs/src/util.rs", "pub fn go()", "pub fn go2()")),
        ("rs restore", original("rs/src/util.rs")),
        ("rs remove util module", delete("rs/src/util.rs")),
        ("rs add util module", original("rs/src/util.rs")),
        ("kt B no longer extends A", replace_in("kt/B.kt", " : A()", "")),
        ("kt B extends A", original("kt/B.kt")),
        ("kt rename hello", replace_in("kt/A.kt", "hello", "hi")),
        ("kt restore", original("kt/A.kt")),
        ("ts widget calls missing", replace_in("ts/widget.ts", "util();", "util(); later();")),
        ("ts define later", put("ts/later.ts", "export function later() {}\n")),
        (
            "ts widget imports later",
            replace_in("ts/widget.ts", "import { Base } from './base';", "import { Base } from './base';\nimport { later } from './later';"),
        ),
        ("ts remove later", delete("ts/later.ts")),
        ("ts add missing module", put("ts/missing.ts", "export class Missing {}\n")),
        ("ts remove missing module", delete("ts/missing.ts")),
        ("ts widget stops extending (main calls w.base)", replace_in("ts/widget.ts", " extends Base", "")),
        ("ts widget extends again", original("ts/widget.ts")),
        ("ts make returns B1", replace_in("ts/factory.ts", "make(): A1", "make(): B1")),
        ("ts make returns A1 again", original("ts/factory.ts")),
        ("ts add sidefx module", put("ts/sidefx.ts", "console.log(1);\n")),
        ("ts remove sidefx module", delete("ts/sidefx.ts")),
        ("ts pick.ts shadows pick/index.ts", put("ts/pick.ts", "export class Other {}\n")),
        ("ts pick.ts removed", delete("ts/pick.ts")),
        ("ts barrel drops util (widget consumer)", put("ts/barrel.ts", "export * from './service';\n")),
        ("ts barrel back again", original("ts/barrel.ts")),
        ("ts widget stops extending 2", replace_in("ts/widget.ts", " extends Base", "")),
        ("ts widget extends again 2", original("ts/widget.ts")),
        ("go comment in a.go (callers keep targets)", replace_in("go/a/a.go", "package a\n", "package a\n\n// note\n")),
        ("go a.go restored", original("go/a/a.go")),
        ("java comment in Circle", replace_in("java/com/ex/Circle.java", "package com.ex;\n", "package com.ex;\n// note\n")),
        ("java Circle restored", original("java/com/ex/Circle.java")),
        ("rs comment in util.rs", replace_in("rs/src/util.rs", "pub fn go()", "// note\npub fn go()")),
        ("rs util.rs restored", original("rs/src/util.rs")),
        ("kt comment in A.kt", replace_in("kt/A.kt", "open class A", "// note\nopen class A")),
        ("kt A.kt restored", original("kt/A.kt")),
        ("py comment in core.py", replace_in("py/pkg/core.py", "class Engine", "# note\nclass Engine")),
        ("py core.py restored", original("py/pkg/core.py")),
        ("ts comment in types.ts", replace_in("ts/types.ts", "export class A1", "// note\nexport class A1")),
        ("ts types.ts restored", original("ts/types.ts")),
        ("ts barrel drops util (transitive consumer viab2)", put("ts/barrel.ts", "export * from './service';\n")),
        ("ts barrel restored 2", original("ts/barrel.ts")),
        ("ts Svc.repo changes type (usesvc.ts only talks to getSvc)", replace_in("ts/svc.ts", "repo: A1 = new A1()", "repo: B1 = new B1()")),
        ("ts Svc.repo type restored", original("ts/svc.ts")),
        ("ts later defined in main's package", put("ts/anywhere.ts", "export function later() {}\n")),
        ("ts remove anywhere", delete("ts/anywhere.ts")),
        ("py add nothere", put("py/pkg/nothere.py", "def thing():\n    return 0\n")),
        ("py define later_py", put("py/pkg/later.py", "def later_py():\n    return 0\n")),
        ("py remove both", delete("py/pkg/nothere.py")),
        ("py remove later", delete("py/pkg/later.py")),
        ("go define Later", put("go/a/later.go", "package a\n\nfunc Later() {}\n")),
        ("go remove Later", delete("go/a/later.go")),
        ("go package representative changes", put("go/a/00.go", "package a\n\nfunc Zero() {}\n")),
        ("go package representative restored", delete("go/a/00.go")),
        ("java define Later", put("java/com/ex/Later.java", "package com.ex;\n\npublic class Later {\n    public void go() {}\n}\n")),
        ("java remove Later", delete("java/com/ex/Later.java")),
        ("rs define later", replace_in("rs/src/util.rs", "fn inner() {}", "fn inner() {}\npub fn later() {}")),
        ("rs undo later", original("rs/src/util.rs")),
        ("kt B stops extending (C calls b.hello)", replace_in("kt/B.kt", " : A()", "")),
        ("kt B extends A again", original("kt/B.kt")),
        ("jsx: rename Child", replace_in("ui/Child.tsx", "Child", "Kid")),
        ("jsx: restore Child", original("ui/Child.tsx")),
        ("jsx: remove Other", delete("ui/Other.tsx")),
        ("jsx: Other back", original("ui/Other.tsx")),
        ("jsx: App renders a third component", put("ui/Third.tsx", "export function Third() { return <i />; }\n")),
        ("jsx: App uses Third", replace_in("ui/App.tsx", "<Other />", "<Other /><Third />")),
        ("jsx: App uses Third restored", original("ui/App.tsx")),
        ("jsx: remove Third", delete("ui/Third.tsx")),
        ("react: comp loses render", replace_in("ui/Comp.jsx", "render()", "paint()")),
        ("react: comp restored", original("ui/Comp.jsx")),
        ("react: comp stops being a Component", replace_in("ui/Comp.jsx", " extends React.Component", "")),
        ("react: comp is a Component again", original("ui/Comp.jsx")),
        ("emitter: handler registered for another event", replace_in("bus/handler.ts", "'tick'", "'tock'")),
        ("emitter: handler restored", original("bus/handler.ts")),
        ("emitter: emitter renamed event", replace_in("bus/bus.ts", "'tick'", "'tock'")),
        ("emitter: emitter restored", original("bus/bus.ts")),
        ("emitter: remove handler file", delete("bus/handler.ts")),
        ("emitter: handler file back", original("bus/handler.ts")),
        ("emitter: second handler", put("bus/h2.ts", "import { bus } from './bus';\nfunction second() {}\nbus.on('tick', second);\n")),
        ("emitter: second handler removed", delete("bus/h2.ts")),
        (
            "emitter: registration without a new definition",
            put("bus/reg.ts", "import { bus } from './bus';\nimport { onTick } from './handler';\nbus.on('tock', onTick);\n"),
        ),
        ("emitter: activate new registration", replace_in("bus/bus.ts", "'tick'", "'tock'")),
        ("emitter: remove registration", delete("bus/reg.ts")),
        ("emitter: restore event", original("bus/bus.ts")),
        ("java: remove Square", delete("java/com/ex/Square.java")),
        ("java: Square back", original("java/com/ex/Square.java")),
        ("java: Square stops implementing", replace_in("java/com/ex/Square.java", " implements Shape", "")),
        ("java: Square implements again", original("java/com/ex/Square.java")),
        ("java: Shape loses area", replace_in("java/com/ex/Shape.java", "double area();", "double volume();")),
        ("java: Shape restored", original("java/com/ex/Shape.java")),
        ("go: En loses Name", replace_in("go/a/greeter.go", "func (e *En) Name() string { return \"en\" }\n", "")),
        ("go: En regains Name", original("go/a/greeter.go")),
        (
            "go: second implementer",
            put(
                "go/a/fr.go",
                "package a\n\ntype Fr struct{}\n\nfunc (f *Fr) Greet() string { return \"salut\" }\nfunc (f *Fr) Name() string { return \"fr\" }\n",
            ),
        ),
        ("go: second implementer removed", delete("go/a/fr.go")),
        ("go: interface gains a method", replace_in("go/a/greeter.go", "Name() string\n}", "Name() string\n\tBye() string\n}")),
        ("go: interface restored", original("go/a/greeter.go")),
        ("kt A.hello gets a return type", replace_in("kt/A.kt", "fun hello(): Int = 1", "fun hello(): String = \"x\"")),
    ];
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for (i, (f, c)) in FILES.iter().enumerate() {
        write(root, f, c, i as u64);
    }
    index_project(root, &opts()).unwrap();
    for (n, (name, edit)) in scenarios.iter().enumerate() {
        edit(root, 500 + n as u64);
        let rep = sync(root, &SyncOptions { index: opts(), ..Default::default() }).unwrap_or_else(|e| panic!("sync failed at {name}: {e}"));
        let (inc, full) = (canonical_of(root), full_canonical(root));
        assert!(inc == full, "scenario {n} ({name}) diverged (full rebuild: {:?}):\n{}", rep.full_rebuild, first_diff(&inc, &full));
    }
}

#[test]
fn single_edit_matches_full_rebuild() {
    run_sequence(1, 1);
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Random edit sequences (design doc 9.6). Local default: 6 sequences of 25 steps; the PR job
/// runs 6 x 100 and the nightly job 40 x 250 through `CGR_SYNC_SEQUENCES` / `CGR_SYNC_STEPS`.
#[test]
fn random_edit_sequences_match_full_rebuild() {
    let sequences = env_usize("CGR_SYNC_SEQUENCES", 6) as u64;
    let steps = env_usize("CGR_SYNC_STEPS", 25);
    for seed in 1..=sequences {
        run_sequence(seed, steps);
    }
}

#[test]
fn up_to_date_tree_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for (i, (f, c)) in FILES.iter().enumerate() {
        write(root, f, c, i as u64);
    }
    index_project(root, &opts()).unwrap();
    let rep = sync(root, &SyncOptions { index: opts(), ..Default::default() }).unwrap();
    assert!(rep.up_to_date);
    let m1 = Store::at(root).read_manifest().unwrap().unwrap();
    write(root, "ts/util.ts", "export function util() { return 5; }\n", 999);
    let rep = sync(root, &SyncOptions { index: opts(), ..Default::default() }).unwrap();
    assert!(!rep.up_to_date && rep.full_rebuild.is_none(), "{rep:?}");
    let m2 = Store::at(root).read_manifest().unwrap().unwrap();
    assert!(m2.generation > m1.generation && m2.delta.is_some());
    let rep = sync(root, &SyncOptions { index: opts(), ..Default::default() }).unwrap();
    assert!(rep.up_to_date, "second sync of the same state is a no-op: {rep:?}");
    assert_eq!(Store::at(root).read_manifest().unwrap().unwrap().generation, m2.generation);
}

#[test]
fn typescript_satisfies_receiver_edits_match_full_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "service.ts", "export interface Runner { run(): void }\nexport class First { run() {} }\nexport class Second { run() {} }\n", 0);
    // Enough independent files and references to exercise a delta rather than
    // the small-project or affected-reference full-rebuild fallback.
    for i in 0..40 {
        write(root, &format!("filler{i}.ts"), &format!("export function filler{i}() {{ helper{i}(); }}\nfunction helper{i}() {{}}\n"), 0);
    }
    for (step, (expression, target)) in [
        ("new First() satisfies Runner", "First"),
        ("new Second() satisfies Runner", "Second"),
        ("(new First() as Runner) satisfies object", "Runner"),
        ("new First() satisfies Runner", "First"),
    ]
    .iter()
    .enumerate()
    {
        write(
            root,
            "app.ts",
            &format!(
                "import {{ Runner, First, Second }} from './service';\nexport function main() {{\n    const service = {expression};\n    service.run();\n}}\n"
            ),
            step as u64 + 1,
        );
        if step == 0 {
            index_project(root, &opts()).unwrap();
        } else {
            let report = sync(root, &SyncOptions { index: opts(), ..Default::default() }).unwrap();
            assert!(report.full_rebuild.is_none(), "must exercise incremental resolution: {report:?}");
        }
        let snap = Store::at(root).open_snapshot().unwrap();
        let calls: Vec<_> = dump::dump_edges(&snap.view).into_iter().filter(|edge| edge.starts_with("app.ts::main -calls-> ")).collect();
        assert_eq!(calls, vec![format!("app.ts::main -calls-> service.ts::{target}::run")]);
        assert_eq!(canonical_of(root), full_canonical(root), "{expression}");
    }
}

#[test]
fn compaction_preserves_live_graph_and_future_sync_without_source_access() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for (i, (f, c)) in FILES.iter().enumerate() {
        write(root, f, c, i as u64);
    }
    write(root, "zz.ts", "export function lastFile() {}\n", 100);
    write(root, "ts/tail-consumer.ts", "import { lastFile } from '../zz';\nexport function tailConsumer() { lastFile(); }\n", 101);
    index_project(root, &opts()).unwrap();
    write(root, "ts/util.ts", "export function util() { return 5; }\nexport function added() { return util(); }\n", 999);
    fs::remove_file(root.join("zz.ts")).unwrap();
    let so = SyncOptions { index: opts(), ..Default::default() };
    sync(root, &so).unwrap();
    let store = Store::at(root);
    let snap = store.open_snapshot().unwrap();
    assert!(snap.view.delta.is_some(), "compaction must exercise a delta, including a removed last-file dependency");
    let before = dump::canonical(&snap.view);
    // This view deliberately has no project/source root. Compaction still has everything
    // it needs in the mapped snapshot, including all facts for later syncs.
    let mut offline = cg_store::View::new(snap.view.base.clone());
    if let Some(delta) = &snap.view.delta {
        offline = offline.with_delta(delta.seg.clone()).unwrap();
    }
    let seg = cg_store::compact::compact(&offline);
    assert_eq!(seg.encoded_len(), seg.to_bytes().len() as u64);
    let packed = cg_store::View::new(std::sync::Arc::new(seg));
    assert_eq!(before, dump::canonical(&packed));
    let again = cg_store::compact::compact(&packed);
    assert_eq!(packed.base.to_bytes(), again.to_bytes(), "compaction is deterministic and idempotent");
    let guard = store.write_lock(Duration::from_secs(1)).unwrap();
    store.publish_base(&again, &guard).unwrap();
    drop(guard);
    assert_eq!(before, dump::canonical(&snap.view), "readers keep the previous complete generation during publication");
    write(root, "ts/new.ts", "import { util } from './util';\nexport function main() { return util(); }\n", 1001);
    write(root, "zz.ts", "export function lastFile() {}\n", 1002);
    sync(root, &so).unwrap();
    assert_eq!(canonical_of(root), full_canonical(root));
    let added = store.open_snapshot().unwrap();
    let packed = cg_store::View::new(std::sync::Arc::new(cg_store::compact::compact(&added.view)));
    assert_eq!(dump::canonical(&added.view), dump::canonical(&packed), "new-file ids also survive compaction");
}

#[test]
fn idle_compaction_waits_for_quiet_and_never_needs_the_source_tree() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for (i, (f, c)) in FILES.iter().enumerate() {
        write(root, f, c, i as u64);
    }
    index_project(root, &opts()).unwrap();
    write(root, "ts/util.ts", &format!("// comment\n{}", FILES.iter().find(|f| f.0 == "ts/util.ts").unwrap().1), 1000);
    sync(root, &SyncOptions { index: opts(), ..Default::default() }).unwrap();
    let store = Store::at(root);
    assert!(store.open_snapshot().unwrap().view.delta.is_some());
    let before = canonical_of(root);
    let mut manifest = store.read_manifest().unwrap().unwrap();
    manifest.syncs_since_merge = 200;
    fs::write(root.join(".cgr/manifest"), serde_json::to_vec(&manifest).unwrap()).unwrap();
    assert!(!cg_sync::compact_if_due(root).unwrap().compacted, "a save must not trigger an immediate merge");
    manifest.updated_ms = manifest.updated_ms.saturating_sub(60_000);
    fs::write(root.join(".cgr/manifest"), serde_json::to_vec(&manifest).unwrap()).unwrap();
    // Delete every source directory. Re-extraction / re-resolution would change the graph.
    for entry in fs::read_dir(root).unwrap().flatten() {
        if entry.file_name() == ".cgr" {
            continue;
        }
        if entry.path().is_dir() {
            fs::remove_dir_all(entry.path()).unwrap();
        } else {
            fs::remove_file(entry.path()).unwrap();
        }
    }
    let guard = store.write_lock(Duration::from_secs(1)).unwrap();
    assert!(!cg_sync::compact_if_due(root).unwrap().compacted, "maintenance must yield to an active writer");
    drop(guard);
    assert!(cg_sync::compact_if_due(root).unwrap().compacted);
    assert!(store.open_snapshot().unwrap().view.delta.is_none());
    assert_eq!(before, canonical_of(root));
}

#[allow(dead_code)]
fn _p(_: PathBuf) {}

#[test]
#[ignore]
fn print_fixture_refs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for (i, (f, c)) in FILES.iter().enumerate() {
        write(root, f, c, i as u64);
    }
    index_project(root, &opts()).unwrap();
    for l in canonical_of(root) {
        if (l.starts_with("R ") && (l.contains("ts/m2.ts") || l.contains("ts/usesvc.ts") || l.contains("ts/viab2.ts") || l.contains("ts/use.ts")))
            || l.starts_with("F ") && l.contains("m2")
        {
            println!("{l}");
        }
    }
}

/// The synthesis fixtures must really produce synthesised edges, otherwise the scenarios above
/// would pass vacuously.
#[test]
fn fixture_produces_synthesised_edges() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for (i, (f, c)) in FILES.iter().enumerate() {
        write(root, f, c, i as u64);
    }
    index_project(root, &opts()).unwrap();
    let dump = canonical_of(root);
    let synth: Vec<&String> = dump.iter().filter(|l| l.starts_with("E ") && l.contains("class=2")).collect();
    for l in &synth {
        println!("{l}");
    }
    let has = |needle: &str| synth.iter().any(|l| l.contains(needle));
    assert!(has("ui/App.tsx"), "jsx render edge missing");
    assert!(has("bus/"), "event emitter edge missing");
    assert!(has("Comp"), "react setState/render edge missing");
    assert!(has("Square") || has("Circle"), "java implementation edge missing");
    assert!(has("greeter.go"), "go implementation edge missing");
}
