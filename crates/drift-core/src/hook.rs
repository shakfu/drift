//! The plugin seam.
//!
//! Moddable behavior (how prices are computed, how NPC traders choose routes) is
//! referenced from content by *name* — e.g. `pricing = "supply_demand_v1"`. A
//! [`NamedRegistry`] maps those names to concrete handlers. Today every handler
//! is a built-in Rust strategy registered at startup; later a `"wasm:..."` or
//! `"lua:..."` name can resolve to a sandboxed module without changing any
//! content, any caller, or the data schema. That deferral is the whole point of
//! routing behavior through names instead of hard-coding it.
//!
//! The registry is generic over the handler type `H` so each behavior family
//! (pricing, trade policy, ...) gets its own strongly-typed registry in the
//! crate that defines those handlers, while the lookup/validation machinery
//! lives here once.

use std::collections::HashMap;

use thiserror::Error;

/// A name-keyed registry of behavior handlers.
#[derive(Debug)]
pub struct NamedRegistry<H> {
    handlers: HashMap<String, H>,
}

// An empty registry needs no `H: Default`, so implement `Default` by hand rather
// than deriving it (the derive would over-constrain the handler type).
impl<H> Default for NamedRegistry<H> {
    fn default() -> Self {
        Self::new()
    }
}

/// Raised when content references a strategy name that was never registered.
/// Fail-fast at link time beats a silent wrong-behavior fallback at runtime.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("unknown strategy '{name}' (registered: [{available}])")]
pub struct UnknownStrategy {
    pub name: String,
    pub available: String,
}

impl<H> NamedRegistry<H> {
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
        }
    }

    /// Register `handler` under `name`, replacing any previous handler with that
    /// name. Built-ins are registered once at startup, so replacement here is a
    /// programmer action, not mod content.
    pub fn register(&mut self, name: impl Into<String>, handler: H) {
        self.handlers.insert(name.into(), handler);
    }

    /// Resolve a name to its handler, or a descriptive error listing what *is*
    /// registered (so a typo in content produces an actionable message).
    pub fn resolve(&self, name: &str) -> Result<&H, UnknownStrategy> {
        self.handlers.get(name).ok_or_else(|| UnknownStrategy {
            name: name.to_owned(),
            available: self.names().collect::<Vec<_>>().join(", "),
        })
    }

    pub fn contains(&self, name: &str) -> bool {
        self.handlers.contains_key(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.handlers.keys().map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_registered_handler() {
        let mut reg: NamedRegistry<u32> = NamedRegistry::new();
        reg.register("supply_demand_v1", 1);
        assert_eq!(reg.resolve("supply_demand_v1"), Ok(&1));
        assert!(reg.contains("supply_demand_v1"));
    }

    #[test]
    fn unknown_name_is_an_error_not_a_fallback() {
        let mut reg: NamedRegistry<u32> = NamedRegistry::new();
        reg.register("supply_demand_v1", 1);
        let err = reg.resolve("typo_v9").unwrap_err();
        assert_eq!(err.name, "typo_v9");
        assert!(err.available.contains("supply_demand_v1"));
    }
}
