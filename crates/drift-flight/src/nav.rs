//! Navigation & legal state — the engine-agnostic rules behind fuel-limited
//! hyperspace, the fuel injectors / torus drive, and the player's legal standing.
//!
//! Like [`crate::targeting`], the load-bearing logic lives here so it is unit
//! tested and merely *driven* by the Bevy app: how much fuel a jump costs, and
//! how a rap sheet maps to a legal status the police react to.

/// Maximum fuel a ship can carry, in abstract "light-year" units (Oolite's tank is
/// 7.0 LY). Hyperspace jumps spend it by distance; the injectors burn it for speed.
pub const MAX_FUEL: f32 = 7.0;

/// Fuel spent per unit of inter-system distance on a hyperspace jump. Tuned so a
/// typical adjacent hop costs a couple of units and the tank is good for a few
/// jumps before a refuel is due.
pub const FUEL_PER_DISTANCE: f32 = 0.35;

/// Fuel burned per second while the injectors / torus drive are engaged.
pub const INJECTOR_BURN: f32 = 0.6;

/// The fuel a hyperspace jump of the given inter-system `distance` costs. Floored
/// at a small minimum so even a neighbour is never free.
pub fn jump_fuel_cost(distance: f32) -> f32 {
    (distance * FUEL_PER_DISTANCE).max(0.1)
}

/// Whether a jump of `distance` is affordable with `fuel` in the tank.
pub fn can_jump(fuel: f32, distance: f32) -> bool {
    fuel + 1e-4 >= jump_fuel_cost(distance)
}

/// The player's legal standing, derived from their bounty. The police (navy) leave
/// a clean pilot alone, and hunt an offender or fugitive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegalStatus {
    Clean,
    Offender,
    Fugitive,
}

/// Bounty at or above which a pilot is a [`Fugitive`](LegalStatus::Fugitive) —
/// hunted on sight. Below it (but non-zero) they are an
/// [`Offender`](LegalStatus::Offender); zero is [`Clean`](LegalStatus::Clean).
pub const FUGITIVE_BOUNTY: u32 = 50;

/// Map a bounty to a [`LegalStatus`].
pub fn legal_status(bounty: u32) -> LegalStatus {
    if bounty == 0 {
        LegalStatus::Clean
    } else if bounty < FUGITIVE_BOUNTY {
        LegalStatus::Offender
    } else {
        LegalStatus::Fugitive
    }
}

impl LegalStatus {
    /// Whether the police (navy) treat this pilot as prey.
    pub fn is_wanted(self) -> bool {
        !matches!(self, LegalStatus::Clean)
    }

    /// Short HUD label.
    pub fn label(self) -> &'static str {
        match self {
            LegalStatus::Clean => "CLEAN",
            LegalStatus::Offender => "OFFENDER",
            LegalStatus::Fugitive => "FUGITIVE",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jump_cost_scales_with_distance_and_has_a_floor() {
        assert!((jump_fuel_cost(10.0) - 3.5).abs() < 1e-5);
        assert!(jump_fuel_cost(0.0) >= 0.1, "even a zero-distance hop is not free");
        assert!(jump_fuel_cost(20.0) > jump_fuel_cost(10.0));
    }

    #[test]
    fn affordability_respects_the_tank() {
        // A full tank affords a mid-range jump but not an enormous one.
        assert!(can_jump(MAX_FUEL, 10.0));
        assert!(!can_jump(MAX_FUEL, 25.0), "beyond range on a full tank");
        // An empty tank affords nothing.
        assert!(!can_jump(0.0, 5.0));
        // Exactly enough is enough.
        assert!(can_jump(jump_fuel_cost(8.0), 8.0));
    }

    #[test]
    fn legal_status_thresholds() {
        assert_eq!(legal_status(0), LegalStatus::Clean);
        assert_eq!(legal_status(1), LegalStatus::Offender);
        assert_eq!(legal_status(FUGITIVE_BOUNTY - 1), LegalStatus::Offender);
        assert_eq!(legal_status(FUGITIVE_BOUNTY), LegalStatus::Fugitive);
        assert_eq!(legal_status(999), LegalStatus::Fugitive);
    }

    #[test]
    fn only_clean_is_unwanted() {
        assert!(!LegalStatus::Clean.is_wanted());
        assert!(LegalStatus::Offender.is_wanted());
        assert!(LegalStatus::Fugitive.is_wanted());
    }
}
