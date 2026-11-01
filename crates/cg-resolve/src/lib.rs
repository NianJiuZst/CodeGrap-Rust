//! cg-resolve: from extraction tables to a connected code graph.

pub mod project;

pub use project::{normalize, parent_dir, ProjectInfo};
pub mod modules;
pub use modules::{ImportTarget, Modules, PkgId};
pub mod engine;
pub use engine::{Bound, Engine, ExportEntry};
pub mod builtins;
pub mod overload;
pub mod shape;
pub use shape::{parse_shape, Shape};
pub mod resolve;
pub use resolve::{by, Accept, FileCx, NameRes, Outcome, TypeRes, WorkerCx};
pub mod driver;
pub use driver::{resolve_all, resolve_refs, round_of, ResolveOutput, Round_};
pub mod frameworks;
pub mod index;
pub mod synth;
pub use index::{build_seg, hooks_for, index_project, index_with_guard, Built, IndexOptions, IndexReport};
