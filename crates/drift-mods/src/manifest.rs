//! Mod manifest — `manifest.toml` at the root of each mod directory.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::LoadError;

/// Parsed `manifest.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Unique mod id (namespace prefix for this mod's content ids, by convention).
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Version string (unvalidated in M1; semver enforcement is future work).
    pub version: String,
    /// Ids of mods that must load before this one.
    #[serde(default)]
    pub dependencies: Vec<String>,
    /// Content ids this mod intentionally replaces. A cross-mod id collision is
    /// an error unless the overriding mod lists the id here — making every
    /// override a deliberate, auditable act.
    #[serde(default)]
    pub overrides: Vec<String>,
    /// Behavior scripts this mod contributes, each declared as a `[[scripts]]`
    /// table. A script registers a named strategy at load time — content then
    /// selects it by name exactly like a built-in (e.g. a system's
    /// `pricing: "<name>"`). The loader reads the `.rhai` file from disk and
    /// fails the load if it is missing.
    #[serde(default)]
    pub scripts: Vec<ScriptEntry>,
}

/// Which engine seam a mod script plugs into. The name a script registers is
/// resolved through this seam, so the kind determines the calling convention the
/// script must implement. Only pricing exists today; trader-AI and event-rule
/// kinds are the planned extensions, and an unknown kind fails the load.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptKind {
    /// A market pricing strategy. The script must define
    /// `fn price(base, stock, equilibrium, elasticity)` returning the unit price.
    #[default]
    Pricing,
}

/// One `[[scripts]]` entry in a manifest: a named script backed by a `.rhai` file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptEntry {
    /// The strategy name content references (e.g. `pricing: "volatile_v1"`). Must
    /// be unique across all loaded mods and must not shadow a built-in strategy.
    pub name: String,
    /// Path to the `.rhai` source, relative to this mod's directory.
    pub path: String,
    /// Which seam this script plugs into. Defaults to [`ScriptKind::Pricing`].
    #[serde(default)]
    pub kind: ScriptKind,
}

impl Manifest {
    pub fn from_path(path: &Path) -> Result<Self, LoadError> {
        let text = std::fs::read_to_string(path).map_err(|source| LoadError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        toml::from_str(&text).map_err(|source| LoadError::ManifestParse {
            path: path.to_path_buf(),
            source,
        })
    }

    pub fn overrides(&self, id: &str) -> bool {
        self.overrides.iter().any(|o| o == id)
    }
}

/// A discovered mod directory: its manifest plus where it lives on disk.
#[derive(Debug, Clone)]
pub struct ModDir {
    pub manifest: Manifest,
    pub dir: PathBuf,
}
