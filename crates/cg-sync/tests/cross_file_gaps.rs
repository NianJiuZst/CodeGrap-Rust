//! A change on either side of a cross-file link must agree with a fresh index.
use cg_resolve::{index_project, IndexOptions};
use cg_store::{dump, Store};
use cg_sync::{sync, SyncOptions};
use std::path::Path;
use std::time::{Duration, SystemTime};

const FILES: &[(&str, &str)] = &[
    ("package.json", r#"{"dependencies":{"@nestjs/core":"10","@nestjs/common":"10","react-router-dom":"6"}}"#),
    ("src/users.controller.ts", "import { Controller, Get } from '@nestjs/common';\n@Controller('users')\nexport class UsersController {\n  @Get(':id')\n  one() {}\n}\n"),
    ("src/users.module.ts", "import { Module } from '@nestjs/common';\nimport { UsersController } from './users.controller';\n@Module({ controllers: [UsersController] })\nexport class UsersModule {}\n"),
    ("src/app.module.ts", "import { Module } from '@nestjs/common';\nimport { RouterModule } from '@nestjs/core';\nimport { UsersModule } from './users.module';\n@Module({ imports: [UsersModule, RouterModule.register([{ path: 'admin', module: UsersModule }])] })\nexport class AppModule {}\n"),
    ("src/main.ts", "import { NestFactory } from '@nestjs/core';\nimport { AppModule } from './app.module';\nasync function bootstrap() {\n  const app = await NestFactory.create(AppModule);\n  app.setGlobalPrefix('api');\n}\n"),
    ("client/api.ts", "export function load() {\n  return fetch('/api/admin/users/1');\n}\n"),
    ("web/src/pages.tsx", "export function Home() { return null }\nexport function Other() { return null }\n"),
    ("web/src/routes.tsx", "import { Home, Other } from './pages';\nexport const routes = [{ path: '/home', element: <Home /> }];\nexport const otherRoutes = [{ path: '/other', element: <Other /> }];\nexport default [{ path: '/special', element: <Other /> }];\n"),
    ("web/src/barrel.ts", "export { default as defaultRoutes } from './routes';\n"),
    ("web/src/app.tsx", "import { createBrowserRouter } from 'react-router-dom';\nimport { routes, otherRoutes } from './routes';\nimport { defaultRoutes } from './barrel';\nexport const router = createBrowserRouter(routes);\n"),
    ("web/src/nav.tsx", "import { Link } from 'react-router-dom';\nexport function Nav() {\n  return <Link to=\"/home\">home</Link>;\n}\n"),
    ("go/go.mod", "module example.com/test\n\ngo 1.21\n"),
    ("go/x/types.go", "package x\ntype Service interface { Open(); Close() error }\ntype Base struct{}\ntype Wrapper struct { Base }\n"),
    ("go/x/methods.go", "package x\nfunc (Base) Close() error { return nil }\nfunc (Wrapper) Open() {}\n"),
];

fn opts() -> IndexOptions {
    IndexOptions { threads: Some(2), ..Default::default() }
}
fn write(root: &Path, rel: &str, text: &str, step: u64) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, text).unwrap();
    let stamp = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + step);
    std::fs::OpenOptions::new().write(true).open(path).unwrap().set_modified(stamp).unwrap();
}
fn graph(root: &Path) -> Vec<String> {
    let snap = Store::locate(root).unwrap().open_snapshot().unwrap();
    dump::canonical(&snap.view)
}
fn edges(root: &Path) -> Vec<String> {
    let snap = Store::locate(root).unwrap().open_snapshot().unwrap();
    dump::dump_edges(&snap.view)
}
fn fresh(root: &Path) -> Vec<String> {
    let temp = tempfile::tempdir().unwrap();
    for (rel, _) in FILES {
        let text = std::fs::read_to_string(root.join(rel)).unwrap();
        write(temp.path(), rel, &text, 1);
    }
    index_project(temp.path(), &opts()).unwrap();
    graph(temp.path())
}

#[test]
fn cross_file_edits_remain_equal_to_full_rebuild() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    for (i, (rel, contents)) in FILES.iter().enumerate() {
        write(root, rel, contents, i as u64);
    }
    index_project(root, &opts()).unwrap();
    let contains = |needle: &str| edges(root).iter().any(|e| e.contains(needle));
    assert!(contains("client/api.ts::load -calls-> src/users.controller.ts"));
    assert!(contains("web/src/nav.tsx::Nav -navigates-> web/src/routes.tsx::web/src/routes.tsx::route:/home"));
    assert!(contains("go/x/types.go::Service::Close -calls-> go/x/methods.go::Base::Close"));
    let changes = [
        ("src/main.ts", "setGlobalPrefix('api')", "setGlobalPrefix('v2')"),
        ("client/api.ts", "/api/admin/users/1", "/v2/admin/users/1"),
        ("src/app.module.ts", "path: 'admin'", "path: 'private'"),
        ("web/src/app.tsx", "createBrowserRouter(routes)", "createBrowserRouter(otherRoutes)"),
        ("web/src/nav.tsx", "/home", "/other"),
        ("web/src/routes.tsx", "path: '/other'", "path: 'changed'"),
        ("go/x/methods.go", "Close() error", "Close(n int) error"),
        ("go/x/methods.go", "Close(n int) error", "Close() error"),
        ("src/users.module.ts", "controllers: [UsersController]", "controllers: []"),
        ("web/src/app.tsx", "createBrowserRouter(otherRoutes)", "createBrowserRouter([{ path: '/admin', children: otherRoutes }])"),
        ("web/src/nav.tsx", "/other", "/admin/changed"),
        ("web/src/app.tsx", "path: '/admin'", "path: '/private'"),
        ("go/x/types.go", "struct { Base }", "struct { Base; Close string }"),
        ("go/x/types.go", "struct { Base; Close string }", "struct { Base }"),
        ("web/src/app.tsx", "createBrowserRouter([{ path: '/private', children: otherRoutes }])", "createBrowserRouter(defaultRoutes)"),
        ("web/src/nav.tsx", "/admin/changed", "/special"),
        ("web/src/routes.tsx", "path: '/special'", "path: '/new'"),
        ("src/users.module.ts", "controllers: []", "controllers: [UsersController]"),
        ("client/api.ts", "/v2/admin/users/1", "/v2/private/users/health"),
        ("src/main.ts", "setGlobalPrefix('v2')", "setGlobalPrefix('v2', { exclude: ['private/users/health'] })"),
        ("client/api.ts", "/v2/private/users/health", "/private/users/health"),
    ];
    for (i, (file, from, to)) in changes.iter().enumerate() {
        let current = std::fs::read_to_string(root.join(file)).unwrap();
        assert!(current.contains(from));
        write(root, file, &current.replacen(from, to, 1), 100 + i as u64);
        let report = sync(root, &SyncOptions { index: opts(), ..Default::default() }).unwrap();
        assert!(report.full_rebuild.is_none(), "step {i} used a full rebuild: {:?}", report.full_rebuild);
        let (inc, full) = (graph(root), fresh(root));
        assert_eq!(inc, full, "incremental graph differs at step {i}: {file}");
        match i {
            0 | 2 | 8 => assert!(!contains("client/api.ts::load -calls-> src/users.controller.ts"), "stale HTTP edge at step {i}"),
            1 => assert!(contains("client/api.ts::load -calls-> src/users.controller.ts")),
            3 | 5 => assert!(!contains("web/src/nav.tsx::Nav -navigates->"), "stale navigation at step {i}"),
            4 => assert!(contains("web/src/nav.tsx::Nav -navigates-> web/src/routes.tsx::web/src/routes.tsx::route:/other")),
            6 => assert!(!contains("go/x/types.go::Service::Close -calls-> go/x/methods.go::Base::Close")),
            7 => assert!(contains("go/x/types.go::Service::Close -calls-> go/x/methods.go::Base::Close")),
            10 => assert!(contains("web/src/nav.tsx::Nav -navigates-> web/src/routes.tsx::web/src/routes.tsx::route:/changed")),
            11 => assert!(!contains("web/src/nav.tsx::Nav -navigates->")),
            12 => assert!(!contains("go/x/types.go::Service::Close -calls-> go/x/methods.go::Base::Close")),
            13 => assert!(contains("go/x/types.go::Service::Close -calls-> go/x/methods.go::Base::Close")),
            15 => assert!(contains("web/src/nav.tsx::Nav -navigates-> web/src/routes.tsx::web/src/routes.tsx::route:/special")),
            16 => assert!(!contains("web/src/nav.tsx::Nav -navigates->")),
            18 | 20 => assert!(contains("client/api.ts::load -calls-> src/users.controller.ts")),
            19 => assert!(!contains("client/api.ts::load -calls-> src/users.controller.ts")),
            _ => {}
        }
    }
}
