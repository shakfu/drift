//! An encounter: a set of [`Combatant`]s across factions, advanced one tick at a
//! time until one side remains.
//!
//! Each tick every live ship targets its nearest enemy, steers to close to
//! engagement range, and fires when in range and off cooldown. Accuracy falls off
//! with distance and is resolved against the shared seeded RNG, so a battle is
//! fully reproducible. Implementing [`Step`] makes the encounter the concrete
//! consumer of core's per-tick seam.

use drift_core::{DetRng, SimContext, Step, Tick};
use serde::{Deserialize, Serialize};

use crate::combatant::Combatant;
use crate::math::Vec2;

/// One tick of continuous time for the integrator.
pub const DT: f64 = 1.0;

/// The result of (a point in) an encounter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Two or more factions still have live ships.
    Ongoing,
    /// No ships survive.
    Draw,
    /// Exactly one faction has live ships.
    Victory(u8),
}

/// A battle in progress.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Encounter {
    pub combatants: Vec<Combatant>,
}

impl Encounter {
    pub fn new(combatants: Vec<Combatant>) -> Self {
        Self { combatants }
    }

    pub fn combatants(&self) -> &[Combatant] {
        &self.combatants
    }

    pub fn alive_count(&self) -> usize {
        self.combatants.iter().filter(|c| c.alive).count()
    }

    /// Number of live ships in a faction.
    pub fn survivors(&self, faction: u8) -> usize {
        self.combatants
            .iter()
            .filter(|c| c.alive && c.faction == faction)
            .count()
    }

    /// Nearest living enemy of combatant `i`, by distance then lowest index (for
    /// deterministic tie-breaking).
    fn nearest_enemy(&self, i: usize) -> Option<usize> {
        let me = &self.combatants[i];
        let mut best: Option<(usize, f64)> = None;
        for (j, other) in self.combatants.iter().enumerate() {
            if j == i || !other.alive || other.faction == me.faction {
                continue;
            }
            let d = me.pos.distance(other.pos);
            let better = match best {
                None => true,
                Some((_, bd)) => d < bd,
            };
            if better {
                best = Some((j, d));
            }
        }
        best.map(|(j, _)| j)
    }

    /// Current outcome.
    pub fn outcome(&self) -> Outcome {
        let mut factions: Vec<u8> = Vec::new();
        for c in &self.combatants {
            if c.alive && !factions.contains(&c.faction) {
                factions.push(c.faction);
            }
        }
        match factions.len() {
            0 => Outcome::Draw,
            1 => Outcome::Victory(factions[0]),
            _ => Outcome::Ongoing,
        }
    }

    /// Run the encounter to a decision or until `max_ticks` elapse, drawing
    /// randomness from `rng`. Returns the final outcome.
    pub fn resolve(&mut self, rng: &mut DetRng, max_ticks: u64) -> Outcome {
        self.advance(rng, max_ticks)
    }

    /// Advance the encounter by up to `steps` ticks, stopping early if it decides,
    /// and return the current outcome. This is the per-tick seam a running-battle
    /// host uses to spread a fight across several simulation ticks (`resolve` is
    /// the special case of advancing to completion in one call). The combat tick
    /// counter is local to the encounter and only feeds the RNG-bearing context.
    pub fn advance(&mut self, rng: &mut DetRng, steps: u64) -> Outcome {
        for t in 0..steps {
            if self.outcome() != Outcome::Ongoing {
                break;
            }
            let mut ctx = SimContext::new(Tick(t), rng);
            self.step(&mut ctx);
        }
        self.outcome()
    }
}

impl Step for Encounter {
    fn step(&mut self, ctx: &mut SimContext) {
        let n = self.combatants.len();
        for i in 0..n {
            if !self.combatants[i].alive {
                continue;
            }
            if self.combatants[i].cooldown > 0 {
                self.combatants[i].cooldown -= 1;
            }

            let Some(j) = self.nearest_enemy(i) else {
                // No enemy left: coast on current velocity.
                let v = self.combatants[i].vel;
                self.combatants[i].pos = self.combatants[i].pos + v * DT;
                self.combatants[i].regen_shield(DT);
                continue;
            };

            let apos = self.combatants[i].pos;
            let tpos = self.combatants[j].pos;
            let dist = apos.distance(tpos);
            let stats = self.combatants[i].stats;
            let max_speed = self.combatants[i].max_speed;

            // Steering: close to ~60% of weapon range, then hold station so ships
            // sit at engagement distance rather than flying through each other.
            let standoff = stats.weapon_range * 0.6;
            let desired_vel = if dist > standoff {
                (tpos - apos).normalized() * max_speed
            } else {
                Vec2::default()
            };
            let dv = (desired_vel - self.combatants[i].vel).clamp_length(stats.acceleration * DT);
            let new_vel = (self.combatants[i].vel + dv).clamp_length(max_speed);
            self.combatants[i].vel = new_vel;
            self.combatants[i].pos = apos + new_vel * DT;

            // Firing: in range, armed, off cooldown. Accuracy falls off linearly
            // to zero at max range; the outcome is a seeded RNG roll.
            if self.combatants[i].is_armed()
                && self.combatants[i].cooldown == 0
                && dist <= stats.weapon_range
            {
                let chance = (stats.accuracy * (1.0 - dist / stats.weapon_range)).clamp(0.0, 1.0);
                if ctx.rng.unit_f64() < chance {
                    self.combatants[j].take_damage(stats.weapon_damage);
                }
                self.combatants[i].cooldown = stats.weapon_cooldown;
            }

            self.combatants[i].regen_shield(DT);
        }
    }
}

#[cfg(test)]
mod tests {
    use drift_core::ShipId;
    use drift_data::CombatStats;

    use super::*;

    fn fighter() -> CombatStats {
        CombatStats {
            shield: 30,
            shield_regen: 0.5,
            weapon_damage: 10,
            weapon_range: 40.0,
            weapon_cooldown: 2,
            accuracy: 1.0,
            acceleration: 20.0,
        }
    }

    fn at(faction: u8, stats: CombatStats, hull: u32, pos: Vec2) -> Combatant {
        Combatant::new(ShipId(0), faction, stats, hull, 300.0, pos)
    }

    #[test]
    fn outcome_detects_victory_and_draw() {
        let mut e = Encounter::new(vec![
            at(0, fighter(), 10, Vec2::new(0.0, 0.0)),
            at(1, fighter(), 10, Vec2::new(5.0, 0.0)),
        ]);
        assert_eq!(e.outcome(), Outcome::Ongoing);
        e.combatants[1].alive = false;
        assert_eq!(e.outcome(), Outcome::Victory(0));
        e.combatants[0].alive = false;
        assert_eq!(e.outcome(), Outcome::Draw);
    }

    #[test]
    fn nearest_enemy_is_chosen() {
        let e = Encounter::new(vec![
            at(0, fighter(), 10, Vec2::new(0.0, 0.0)),
            at(1, fighter(), 10, Vec2::new(100.0, 0.0)),
            at(1, fighter(), 10, Vec2::new(10.0, 0.0)),
        ]);
        assert_eq!(e.nearest_enemy(0), Some(2), "the closer enemy (index 2)");
        // A ship ignores same-faction ships.
        assert_eq!(e.nearest_enemy(1), Some(0));
    }

    #[test]
    fn a_duel_ends_deterministically() {
        let build = || {
            Encounter::new(vec![
                at(0, fighter(), 40, Vec2::new(-20.0, 0.0)),
                at(1, fighter(), 40, Vec2::new(20.0, 0.0)),
            ])
        };
        let mut a = build();
        let mut b = build();
        let out_a = a.resolve(&mut DetRng::from_seed(5), 500);
        let out_b = b.resolve(&mut DetRng::from_seed(5), 500);
        assert_ne!(out_a, Outcome::Ongoing, "the duel should conclude");
        assert_eq!(out_a, out_b, "same seed -> same outcome");
    }

    #[test]
    fn stronger_ship_reliably_wins() {
        // Faction 0 has far more hull, shield, and firepower: it should win across
        // many seeds.
        let strong = CombatStats {
            shield: 200,
            weapon_damage: 30,
            ..fighter()
        };
        let mut wins = 0;
        for seed in 0..20 {
            let mut e = Encounter::new(vec![
                at(0, strong, 300, Vec2::new(-20.0, 0.0)),
                at(1, fighter(), 40, Vec2::new(20.0, 0.0)),
            ]);
            if e.resolve(&mut DetRng::from_seed(seed), 1000) == Outcome::Victory(0) {
                wins += 1;
            }
        }
        assert_eq!(wins, 20, "the much stronger ship should win every time");
    }

    #[test]
    fn unarmed_ship_cannot_win() {
        // An unarmed civilian (faction 1) versus an armed fighter (faction 0).
        let unarmed = CombatStats {
            weapon_damage: 0,
            weapon_range: 0.0,
            ..fighter()
        };
        let mut e = Encounter::new(vec![
            at(0, fighter(), 40, Vec2::new(-15.0, 0.0)),
            at(1, unarmed, 40, Vec2::new(15.0, 0.0)),
        ]);
        assert_eq!(e.resolve(&mut DetRng::from_seed(1), 1000), Outcome::Victory(0));
    }
}
