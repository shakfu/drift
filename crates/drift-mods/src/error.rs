//! Loader error taxonomy.
//!
//! Every failure is specific and actionable: which file, which id, which missing
//! reference. The linking philosophy is fail-fast — a dangling reference or an
//! unknown strategy name aborts the load rather than degrading silently at
//! runtime.

use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum LoadError {
    #[error("i/o error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse manifest {path}: {source}")]
    ManifestParse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("failed to parse content {path}: {source}")]
    ContentParse {
        path: PathBuf,
        #[source]
        source: ron::error::SpannedError,
    },

    #[error("mod '{mod_id}' depends on '{dependency}', which was not found")]
    MissingDependency { mod_id: String, dependency: String },

    #[error("dependency cycle among mods: [{0}]")]
    DependencyCycle(String),

    #[error(
        "duplicate {kind} id '{id}': defined by both '{first}' and '{second}' \
         (the overriding mod must list it in `overrides`)"
    )]
    DuplicateId {
        kind: &'static str,
        id: String,
        first: String,
        second: String,
    },

    #[error("{kind} '{referrer}' references unknown {target_kind} '{target}'")]
    DanglingRef {
        kind: &'static str,
        referrer: String,
        target_kind: &'static str,
        target: String,
    },

    #[error("system '{system}' uses unknown pricing strategy '{strategy}'")]
    UnknownPricing { system: String, strategy: String },

    #[error(
        "script '{name}' in mod '{mod_id}' shadows a built-in strategy of the same \
         name; rename the script"
    )]
    ScriptShadowsBuiltin { mod_id: String, name: String },
}
