use cg_core::EdgeKind;
use cg_resolve::{build_seg, IndexOptions};
use cg_store::{dump, View};
use std::sync::Arc;

fn project(files: &[(&str, &str)]) -> (tempfile::TempDir, View) {
    let dir = tempfile::tempdir().unwrap();
    for (path, contents) in files {
        let dest = dir.path().join(path);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(dest, contents).unwrap();
    }
    let seg = build_seg(dir.path(), &IndexOptions { threads: Some(2), ..Default::default() }).unwrap().seg;
    let view = View::new(Arc::new(seg)).with_root(dir.path().to_path_buf());
    (dir, view)
}

#[test]
fn go_promoted_dispatch_respects_shadowing_and_ambiguity() {
    let (_dir, view) = project(&[
        ("go.mod", "module example.com/test\n\ngo 1.21\n"),
        (
            "p/types.go",
            r#"package p

type Closer interface { Close() error }
type Base struct{}
type Middle struct { Base }
type Wrapped struct { Middle }
type Other struct{}
type Ambiguous struct { Base; Other }
type Override struct { Base }
type BadOverride struct { Base }
type Service interface { Open(); Close() error }
type ServiceWrapper struct { Base }
type FieldShadow struct { Base; Close string }
"#,
        ),
        (
            "p/methods.go",
            r#"package p
func (Base) Close() error { return nil }
func (Other) Close() error { return nil }
func (Override) Close() error { return nil }
func (BadOverride) Close(n int) error { return nil }
func (ServiceWrapper) Open() {}
"#,
        ),
    ]);
    let edges = dump::dump_edges(&view);
    let has = |s: &str| edges.iter().any(|e| e == s);
    assert!(has("p/types.go::Wrapped -implements-> p/types.go::Closer"), "{edges:#?}");
    assert!(has("p/types.go::Closer::Close -calls-> p/methods.go::Base::Close"), "{edges:#?}");
    assert!(has("p/types.go::Closer::Close -calls-> p/methods.go::Override::Close"), "{edges:#?}");
    assert!(!has("p/types.go::Ambiguous -implements-> p/types.go::Closer"), "{edges:#?}");
    assert!(!has("p/types.go::BadOverride -implements-> p/types.go::Closer"), "{edges:#?}");
    assert!(!has("p/types.go::FieldShadow -implements-> p/types.go::Closer"), "a field shadows a promoted method: {edges:#?}");
    assert!(!has("p/types.go::Closer::Close -calls-> p/methods.go::BadOverride::Close"), "{edges:#?}");
    // Base alone does not implement Service; this edge exists only because ServiceWrapper
    // combines its own Open with Base's promoted Close.
    assert!(has("p/types.go::ServiceWrapper -implements-> p/types.go::Service"), "{edges:#?}");
    assert!(has("p/types.go::Service::Close -calls-> p/methods.go::Base::Close"), "{edges:#?}");
    assert!(has("p/types.go::Service::Open -calls-> p/methods.go::ServiceWrapper::Open"), "{edges:#?}");
    // Keep the test's expectation explicitly about call edges, not containment.
    assert!(view.files().flat_map(|f| view.nodes_in_file(f)).any(|n| view.out_edges(cg_core::NodeIdx(n)).any(|e| e.kind == EdgeKind::Calls)));
}

#[test]
fn react_router_uses_imported_route_tables_and_ignores_unregistered_arrays() {
    let (_dir, view) = project(&[
        ("package.json", r#"{"dependencies":{"react":"19","react-router-dom":"6"}}"#),
        ("src/pages.tsx", "export function Home() { return null; }\nexport function Team() { return null; }\nexport function Ghost() { return null; }\n"),
        ("src/routes.tsx", r#"import { Home, Team, Ghost } from './pages';
export const routes = [
  { path: '/', element: <Home /> },
  { path: '/teams', children: [
    { path: ':id', element: <Team /> },
  ] },
];
export const menu = [{ path: '/ghost', element: <Ghost /> }];
"#),
        ("src/barrel.ts", "export { routes as appRoutes } from './routes';\n"),
        ("src/app.tsx", "import { createBrowserRouter } from 'react-router-dom';\nimport { appRoutes as table } from './barrel';\nexport const router = createBrowserRouter(table);\n"),
        ("src/nav.tsx", r#"import { Link } from 'react-router-dom';
export function Nav() {
  return <><Link to="/teams/7">team</Link><Link to="/ghost">ghost</Link></>;
}
"#),
    ]);
    let edges = dump::dump_edges(&view);
    let has = |s: &str| edges.iter().any(|e| e == s);
    assert!(has("src/routes.tsx::src/routes.tsx::route:/teams/:id -references-> src/pages.tsx::Team"), "{edges:#?}");
    assert!(has("src/nav.tsx::Nav -navigates-> src/routes.tsx::src/routes.tsx::route:/teams/:id"), "{edges:#?}");
    assert!(!edges.iter().any(|e| e.starts_with("src/nav.tsx::Nav -navigates->") && e.contains("/ghost")), "unregistered array must not be active: {edges:#?}");
}

#[test]
fn nest_module_mount_and_global_prefix_reach_http_routes() {
    let (_dir, view) = project(&[
        ("package.json", r#"{"dependencies":{"@nestjs/core":"10","@nestjs/common":"10"}}"#),
        ("src/users.controller.ts", r#"import { Controller, Get, Post } from '@nestjs/common';
@Controller('users')
export class UsersController {
  @Get(':id')
  one() {}
  @Post('health')
  health() {}
}
"#),
        ("src/users.module.ts", "import { Module } from '@nestjs/common';\nimport { UsersController } from './users.controller';\n@Module({ controllers: [UsersController] })\nexport class UsersModule {}\n"),
        ("src/admin.module.ts", "import { Module } from '@nestjs/common';\nimport { UsersModule } from './users.module';\n@Module({ imports: [UsersModule] })\nexport class AdminModule {}\n"),
        ("src/app.module.ts", r#"import { Module } from '@nestjs/common';
import { RouterModule as Routes } from '@nestjs/core';
import { AdminModule } from './admin.module';
import { UsersModule as Users } from './users.module';
@Module({ imports: [AdminModule, Routes.register([
  { path: 'admin', module: AdminModule, children: [
    { path: 'v2', module: Users },
  ] },
])] })
export class AppModule {}
"#),
        ("src/main.ts", r#"import { NestFactory as Factory } from '@nestjs/core';
import { AppModule } from './app.module';
async function bootstrap() {
  const app = await Factory.create(AppModule);
  app.setGlobalPrefix('api', { exclude: [{ path: 'admin/v2/users/health', method: RequestMethod.POST }] });
}
"#),
        ("client/api.ts", r#"export function one() {
  return fetch('/api/admin/v2/users/7');
}
export function old() {
  return fetch('/users/7');
}
export function health() {
  return fetch('/admin/v2/users/health', { method: 'POST' });
}
"#),
    ]);
    let edges = dump::dump_edges(&view);
    let has = |s: &str| edges.iter().any(|e| e == s);
    assert!(has("client/api.ts::one -calls-> src/users.controller.ts::src/users.controller.ts::GET:/users/:id"), "{edges:#?}");
    assert!(!edges.iter().any(|e| e.starts_with("client/api.ts::old -calls-> src/users.controller.ts")), "old unmounted path: {edges:#?}");
    assert!(has("client/api.ts::health -calls-> src/users.controller.ts::src/users.controller.ts::POST:/users/health"), "excluded global prefix: {edges:#?}");
}

#[test]
fn react_router_composes_imported_children_across_multiple_files() {
    let (_dir, view) = project(&[
        ("package.json", r#"{"dependencies":{"react-router-dom":"6"}}"#),
        ("src/pages.tsx", "export function Profile() { return null }\nexport function Settings() { return null }\n"),
        ("src/child.tsx", "import { Profile, Settings } from './pages';\nexport const childRoutes = [\n  { path: 'profile', element: <div>你好<Profile /></div> },\n  { path: 'settings', element: <Settings /> },\n];\n"),
        ("src/parent.ts", "import { childRoutes } from './child';\nimport { createMemoryRouter } from 'react-router-dom';\nexport const adminRoutes = [\n  { path: 'admin', children: childRoutes },\n];\nexport const preview = createMemoryRouter([{ path: '/preview', Component: Preview }]);\nfunction Preview() {}\n"),
        ("src/app.ts", "import { createBrowserRouter } from 'react-router-dom';\nimport { adminRoutes } from './parent';\nexport const router = createBrowserRouter([{ path: '/dashboard', children: adminRoutes }]);\n"),
        ("src/nav.tsx", "import { Link } from 'react-router-dom';\nexport function Nav() {\n  return <><Link to=\"/dashboard/admin/profile\">yes</Link><Link to=\"/profile\">no</Link></>;\n}\n"),
    ]);
    let edges = dump::dump_edges(&view);
    let target = "src/nav.tsx::Nav -navigates-> src/child.tsx::src/child.tsx::route:/profile";
    assert!(edges.iter().any(|e| e == target), "nested imported route: {edges:#?}");
    let metas: Vec<_> = view
        .files()
        .flat_map(|f| view.nodes_in_file(f))
        .flat_map(|n| view.out_edges(cg_core::NodeIdx(n)))
        .filter_map(|e| view.edge_meta(e.eid))
        .filter(|m| m.contains("react-router-link") && m.contains("profile"))
        .collect();
    assert_eq!(metas.len(), 1, "only the mounted path should match: {metas:#?}");
    assert!(metas[0].contains("/dashboard/admin/profile"), "{metas:#?}");
}

#[test]
fn nest_separates_two_bootstraps_and_partial_exclusions() {
    let (_dir, view) = project(&[
        ("package.json", r#"{"dependencies":{"@nestjs/core":"10","@nestjs/common":"10"}}"#),
        ("src/users.ts", "import { Controller, Get, All, Module } from '@nestjs/common';\n@Controller('users')\nexport class UsersController {\n  @Get(':id')\n  one() {}\n  @All('hooks')\n  hooks() {}\n}\n@Module({ controllers: [UsersController] })\nexport class UsersModule {}\n"),
        ("src/a.ts", "import { Module } from '@nestjs/common';\nimport { RouterModule } from '@nestjs/core';\nimport { UsersModule } from './users';\n@Module({ imports: [UsersModule, RouterModule.register([{ path: 'a', module: UsersModule }])] })\nexport class AppA {}\n"),
        ("src/b.ts", "import { Module } from '@nestjs/common';\nimport { RouterModule } from '@nestjs/core';\nimport { UsersModule } from './users';\n@Module({ imports: [UsersModule, RouterModule.register([{ path: 'b', module: UsersModule }])] })\nexport class AppB {}\n"),
        ("src/main-a.ts", "import { NestFactory } from '@nestjs/core';\nimport { AppA } from './a';\nasync function start() {\n const app = await NestFactory.create(AppA);\n app.setGlobalPrefix('v1', { exclude: ['a/users/health', { path: 'a/users/hooks', method: RequestMethod.POST }] });\n}\n"),
        ("src/main-b.ts", "import { NestFactory } from '@nestjs/core';\nimport { AppB } from './b';\nasync function start() {\n const app = await NestFactory.create(AppB);\n app.setGlobalPrefix('v2');\n}\n"),
        ("client.ts", "export function a() { return fetch('/v1/a/users/7'); }\nexport function b() { return fetch('/v2/b/users/7'); }\nexport function health() { return fetch('/a/users/health'); }\nexport function wrong() { return fetch('/v1/b/users/7'); }\nexport function unprefixed() { return fetch('/a/users/7'); }\nexport function prefixedHealth() { return fetch('/v1/a/users/health'); }\nexport function getHooks() { return fetch('/v1/a/users/hooks'); }\nexport function postHooks() { return fetch('/a/users/hooks', { method: 'POST' }); }\nexport function badPost() { return fetch('/v1/a/users/hooks', { method: 'POST' }); }\n"),
    ]);
    let edges = dump::dump_edges(&view);
    for name in ["a", "b", "health"] {
        assert!(edges.iter().any(|e| e.starts_with(&format!("client.ts::{name} -calls-> src/users.ts"))), "{name}: {edges:#?}");
    }
    for name in ["wrong", "unprefixed", "prefixedHealth"] {
        assert!(!edges.iter().any(|e| e.starts_with(&format!("client.ts::{name} -calls-> src/users.ts"))), "{name}: {edges:#?}");
    }
    for name in ["getHooks", "postHooks"] {
        assert!(edges.iter().any(|e| e.starts_with(&format!("client.ts::{name} -calls->")) && e.contains("ALL:/users/hooks")), "{name}: {edges:#?}");
    }
    assert!(!edges.iter().any(|e| e.starts_with("client.ts::badPost -calls->") && e.contains("src/users.ts")), "{edges:#?}");
}

#[test]
fn react_router_accepts_default_exported_route_arrays() {
    let (_dir, view) = project(&[
        ("package.json", r#"{"dependencies":{"react-router-dom":"6"}}"#),
        ("src/pages.tsx", "export function Home() { return null; }\n"),
        ("src/routes.tsx", "import { Home } from './pages';\nexport default [\n  { path: '/default', element: <Home /> },\n];\n"),
        ("src/barrel.ts", "export { default as routeTable } from './routes';\n"),
        (
            "src/app.ts",
            "import { useRoutes } from 'react-router-dom';\nimport { routeTable } from './barrel';\nexport function App() { return useRoutes(routeTable); }\n",
        ),
        ("src/nav.tsx", "import { Link } from 'react-router-dom';\nexport function Nav() { return <Link to=\"/default\">go</Link> }\n"),
    ]);
    let edges = dump::dump_edges(&view);
    assert!(edges.iter().any(|e| e == "src/nav.tsx::Nav -navigates-> src/routes.tsx::src/routes.tsx::route:/default"), "{edges:#?}");
}
