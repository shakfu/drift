//! `drift-data` — the moddable content schema.
//!
//! Pure `serde` definition types with no behavior. This is the contract that mods
//! author against: everything here is loaded from RON, keyed by namespaced string
//! ids, and resolved to runtime handles by `drift-mods`. Keeping the schema in its
//! own crate (separate from runtime simulation state in `drift-economy`) is what
//! keeps the plugin boundary clean — a mod depends only on this vocabulary.
//!
//! All defs use `deny_unknown_fields`: an unrecognized key in content is a hard
//! error, so typos fail at load instead of silently doing nothing.

pub mod commodity;
pub mod production;
pub mod scenario;
pub mod ship;
pub mod system;

pub use commodity::{CommodityAmount, CommodityDef};
pub use production::ProductionRecipe;
pub use scenario::{
    ContractConfig, EscortConfig, FutureConfig, InsuranceConfig, LoanConfig, NavyConfig,
    PiracyConfig, ScenarioDef, TraderSpawn,
};
pub use ship::{CombatStats, ShipDef};
pub use system::SystemDef;
