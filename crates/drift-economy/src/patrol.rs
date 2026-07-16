//! Persistent roaming combat agents — pirates and the navy alike.
//!
//! A [`Patrol`] is a first-class galaxy agent: it has a location, roams between
//! systems, and carries **persistent** hull and shield between fights. A patrol
//! that survives a skirmish stays wounded (shields regenerate, hull does not), so
//! repeated encounters wear a fleet down and reinforcement tops it back up. When
//! hull is depleted the agent is culled.
//!
//! Pirates and navy ships share this representation; they differ only in behavior
//! (see the pirate/navy phases in [`crate::world`]).

use drift_core::{ShipId, SystemId, Tick};
use drift_data::CombatStats;
use serde::{Deserialize, Serialize};

/// A stable, never-reused handle for a patrol (pirate or navy ship), so a running
/// battle can refer to its participants across ticks even as fleets are culled and
/// reinforced. Same discipline as `TraderId`/`ContractId`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct PatrolId(pub u64);

/// Where a patrol is right now. (No "destroyed" state — dead agents are removed.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PatrolLocation {
    Docked(SystemId),
    /// Departed `origin` at `departure`, arriving at `dest` at `arrival`. The
    /// origin/departure let a client interpolate position along the jump edge.
    InTransit {
        origin: SystemId,
        dest: SystemId,
        departure: Tick,
        arrival: Tick,
    },
}

/// A persistent roaming ship (pirate or navy).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Patrol {
    /// Stable handle (see [`PatrolId`]).
    pub id: PatrolId,
    pub ship: ShipId,
    pub location: PatrolLocation,
    /// Persistent hull; `<= 0` marks the agent for removal.
    pub hull: i32,
    /// Persistent shield (regenerates each tick up to its maximum).
    pub shield: f64,
}

impl Patrol {
    /// Spawn a fresh, undamaged agent docked at `at`.
    pub fn new(id: PatrolId, ship: ShipId, stats: &CombatStats, hull: u32, at: SystemId) -> Self {
        Self {
            id,
            ship,
            location: PatrolLocation::Docked(at),
            hull: hull as i32,
            shield: stats.shield as f64,
        }
    }

    pub fn is_alive(&self) -> bool {
        self.hull > 0
    }

    /// The system the agent is currently docked at, if any.
    pub fn docked_at(&self) -> Option<SystemId> {
        match self.location {
            PatrolLocation::Docked(s) => Some(s),
            PatrolLocation::InTransit { .. } => None,
        }
    }

    /// Regenerate shields toward the maximum from `stats`.
    pub fn regen_shield(&mut self, stats: &CombatStats) {
        let max = stats.shield as f64;
        self.shield = (self.shield + stats.shield_regen).min(max);
    }
}
