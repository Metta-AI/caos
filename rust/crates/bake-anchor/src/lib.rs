//! bake-anchor: a dependency-only crate (see Cargo.toml). It holds the std
//! tools' crates.io dependencies so the std/cargo bake vendors and
//! precompiles them for the source-built tools that live outside the workspace.
//! There is no runtime code; `use … as _` marks each crate used so the
//! `unused_crate_dependencies` lint stays quiet without importing any names.

use minreq as _;
use regex as _;
use serde_json as _;
use tiny_http as _;
