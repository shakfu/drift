//! `drift-economy` — the simulation heart: markets, pricing, production, traders.
//!
//! Consumes a linked [`drift_mods::Registry`] plus a scenario and produces a
//! deterministic, tickable [`World`]. The economy's differentiating behavior
//! lives here: dynamic supply/demand pricing, production chains, and NPC traders
//! whose arbitrage self-corrects prices toward equilibrium.

pub mod command;
pub mod contract;
pub mod event;
pub mod finance;
pub mod market;
pub mod patrol;
pub mod pricing;
pub mod production;
pub mod trader;
pub mod world;

pub use command::{Command, CommandError, Owner, PlayerId};
pub use contract::{Contract, ContractId, ContractKind, ContractState};
pub use event::{EventCategory, SimEvent};
pub use finance::{Future, FutureSide, Loan, LoanId, Policy};
pub use market::{Market, MarketGood};
pub use patrol::{Patrol, PatrolId, PatrolLocation};
pub use pricing::{builtin_pricing, pricing_for, PricingScriptError, PricingSet, PricingStrategy};
pub use trader::{choose_trade, Trader, TraderId, TraderLocation, TradePlan};
pub use world::{EncounterView, PiracyStats, Snapshot, World, WorldError};
