//! `drift-combat` — the current 2-D space combat model (M2).
//!
//! This is a deterministic stand-in for `drift`'s intended endgame: advanced,
//! Elite-inspired real-time space combat. That fuller system (piloted, multi-tick
//! running battles) is a core goal of the game, deferred until the engine —
//! renderer and running-battle model — can support it. What lives here is the
//! headless, testable placeholder that resolves encounters today so the economy
//! has real combat pressure to react to.
//!
//! Builds on the economy's ship defs: an [`Encounter`] holds [`Combatant`]s drawn
//! from [`drift_data::ShipDef`]s across factions and resolves a battle one
//! deterministic tick at a time. Ships target the nearest enemy, steer to
//! engagement range, and fire hitscan weapons whose accuracy falls off with
//! distance and is rolled against the shared seeded RNG. Shields absorb damage and
//! regenerate; hull depletion destroys a ship.
//!
//! The model is 2-D for now (matching the galaxy's coordinates) and self-contained:
//! [`Encounter`] implements [`drift_core::Step`], so it advances over the same
//! per-tick seam as the rest of the simulation — and so the full combat system can
//! later slot into that same seam.

pub mod combatant;
pub mod encounter;
pub mod math;

pub use combatant::Combatant;
pub use encounter::{Encounter, Outcome, DT};
pub use math::Vec2;
