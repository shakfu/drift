//! The engine-agnostic scene model: a pure mapping from simulation state to 3-D
//! geometry a renderer draws.
//!
//! This carries no dependency on any graphics engine — it produces plain
//! coordinates and descriptors, so it is unit-testable without a display and could
//! back a Bevy, macroquad, or headless renderer alike. The Bevy app (behind the
//! `gui` feature) is a thin adapter that walks a [`Scene`] and spawns meshes.
//!
//! Two coordinate transforms live here:
//!
//! - **Galaxy → world.** The galaxy's 2-D coordinates map onto the world's ground
//!   plane (`y` up), so systems and jump edges lay out like a map you look down on.
//! - **Combat-local → world.** A running battle's combatants have positions in the
//!   encounter's own 2-D space (a few tens of units across); those are scaled down
//!   and offset around the battle's system node, and lifted slightly off the plane,
//!   so a fight reads as a cluster of ships hovering over its star system.

use drift_economy::EncounterView;
use drift_mods::Registry;

/// A 3-D point, `[x, y, z]`, `y` up. Plain array so this module needs no engine.
pub type V3 = [f32; 3];

/// World units per unit of galaxy coordinate.
const GALAXY_SCALE: f32 = 6.0;
/// World units per unit of combat-local coordinate (combat space is ~tens of units
/// wide, so this shrinks a skirmish to sit neatly over its system node).
const COMBAT_SCALE: f32 = 0.03;
/// Height a battle is lifted above the galaxy plane, so ships read as flying over
/// the star rather than sitting on the map.
const BATTLE_LIFT: f32 = 0.4;

/// Map a galaxy 2-D coordinate onto the world ground plane (galaxy `y` becomes
/// world `-z`, so "up" on the map is "away" in 3-D).
pub fn galaxy_to_world(p: [f64; 2]) -> V3 {
    [p[0] as f32 * GALAXY_SCALE, 0.0, -(p[1] as f32) * GALAXY_SCALE]
}

/// Place a combatant at combat-local `(x, y)` around `system_pos`, scaled down and
/// lifted off the plane.
pub fn combatant_to_world(system_pos: V3, local_x: f64, local_y: f64) -> V3 {
    [
        system_pos[0] + local_x as f32 * COMBAT_SCALE,
        system_pos[1] + BATTLE_LIFT,
        system_pos[2] + local_y as f32 * COMBAT_SCALE,
    ]
}

/// A star system node.
pub struct SceneSystem {
    pub pos: V3,
    /// Lawlessness in `[0, 1]`, for colouring.
    pub danger: f32,
    pub name: String,
}

/// A jump connection between two systems (drawn once per pair).
pub struct SceneEdge {
    pub a: V3,
    pub b: V3,
}

/// One ship in a running battle.
pub struct SceneCombatant {
    pub pos: V3,
    /// Combat faction (0 = trader/defenders, 1 = pirates), for colouring.
    pub faction: u8,
    pub alive: bool,
}

/// A beacon for a running battle — a bright marker lifted above the system so a
/// fight reads from the galaxy overview even when it is brief.
pub struct SceneBattle {
    pub center: V3,
    /// Live combatants (for sizing/labelling the beacon).
    pub alive: usize,
    pub total: usize,
}

/// Height the battle beacon floats above its system node.
const BEACON_LIFT: f32 = 1.5;

/// One beacon per running battle.
pub fn build_battles(reg: &Registry, encounters: &[EncounterView]) -> Vec<SceneBattle> {
    encounters
        .iter()
        .map(|enc| {
            let base = galaxy_to_world(reg.system(enc.system).position);
            SceneBattle {
                center: [base[0], base[1] + BEACON_LIFT, base[2]],
                alive: enc.combatants.iter().filter(|c| c.alive).count(),
                total: enc.combatants.len(),
            }
        })
        .collect()
}

/// Everything a renderer needs to draw one frame of the spectator view.
pub struct Scene {
    pub systems: Vec<SceneSystem>,
    pub edges: Vec<SceneEdge>,
    /// All combatants across every running battle, positioned in world space.
    pub combatants: Vec<SceneCombatant>,
}

/// Build the static galaxy (systems + jump edges) from the registry.
pub fn build_static(reg: &Registry) -> (Vec<SceneSystem>, Vec<SceneEdge>) {
    let systems = reg
        .systems()
        .map(|s| SceneSystem {
            pos: galaxy_to_world(s.position),
            danger: s.danger as f32,
            name: s.name.clone(),
        })
        .collect();

    let mut edges = Vec::new();
    for s in reg.systems() {
        let a = galaxy_to_world(s.position);
        for &c in &s.connections {
            // Draw each undirected pair once.
            if c.0 > s.id.0 {
                edges.push(SceneEdge { a, b: galaxy_to_world(reg.system(c).position) });
            }
        }
    }
    (systems, edges)
}

/// Position every combatant of every running battle in world space.
pub fn build_combatants(reg: &Registry, encounters: &[EncounterView]) -> Vec<SceneCombatant> {
    let mut out = Vec::new();
    for enc in encounters {
        let base = galaxy_to_world(reg.system(enc.system).position);
        for cb in &enc.combatants {
            out.push(SceneCombatant {
                pos: combatant_to_world(base, cb.pos.x, cb.pos.y),
                faction: cb.faction,
                alive: cb.alive,
            });
        }
    }
    out
}

/// Build the full scene for a frame: static galaxy plus the live battles.
pub fn build_scene(reg: &Registry, encounters: &[EncounterView]) -> Scene {
    let (systems, edges) = build_static(reg);
    Scene {
        systems,
        edges,
        combatants: build_combatants(reg, encounters),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn galaxy_maps_onto_the_ground_plane() {
        // The origin maps to the origin; y is up (always 0 for a system) and galaxy
        // y becomes world -z.
        assert_eq!(galaxy_to_world([0.0, 0.0]), [0.0, 0.0, 0.0]);
        let p = galaxy_to_world([1.0, 2.0]);
        assert_eq!(p[0], GALAXY_SCALE);
        assert_eq!(p[1], 0.0, "systems sit on the ground plane");
        assert_eq!(p[2], -2.0 * GALAXY_SCALE, "galaxy +y is world -z");
    }

    #[test]
    fn combatants_cluster_around_and_above_their_system() {
        let sys = [10.0f32, 0.0, -4.0];
        // A combatant at combat-local (30, -30): a small offset around the node,
        // lifted off the plane.
        let p = combatant_to_world(sys, 30.0, -30.0);
        assert!((p[0] - (10.0 + 30.0 * COMBAT_SCALE)).abs() < 1e-6);
        assert_eq!(p[1], sys[1] + BATTLE_LIFT, "battles hover above the plane");
        assert!((p[2] - (-4.0 + -30.0 * COMBAT_SCALE)).abs() < 1e-6);
        // The offset is small: a wide fight still sits near its node.
        assert!((p[0] - sys[0]).abs() < 1.5, "combat stays close to its system");
    }
}
