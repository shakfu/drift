//! `drift-mods` — the mod-loader.
//!
//! Turns a directory of mods into fully-linked, handle-based game data. The two
//! halves are [`loader::load`] (discover + order + read + merge, producing
//! string-id [`MergedContent`]) and [`registry::link`] (resolve every reference,
//! producing an immutable [`Registry`]). [`load_and_link`] runs both.
//!
//! The loader is deliberately ignorant of the economy: it validates pricing
//! strategy names against a set the caller supplies, so behavior lives in
//! `drift-economy` while content validation lives here.

pub mod error;
pub mod loader;
pub mod manifest;
pub mod registry;

use std::collections::HashSet;
use std::path::Path;

pub use error::LoadError;
pub use loader::{load, LoadedScript, MergedContent};
pub use manifest::{Manifest, ScriptEntry, ScriptKind};
pub use registry::{link, Registry, ResolvedRecipe, ResolvedSystem};

/// Convenience: load every mod under `root` and link it, validating `pricing`
/// names against `known_pricing`.
pub fn load_and_link(
    root: &Path,
    known_pricing: &HashSet<String>,
) -> Result<Registry, LoadError> {
    let merged = load(root)?;
    link(merged, known_pricing)
}
