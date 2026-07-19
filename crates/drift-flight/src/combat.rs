//! Real-time combat health — the engine-agnostic damage model for M3.
//!
//! A [`Health`] carries hull and a regenerating shield. Damage is absorbed by the
//! shield first and only spills into the hull once the shield is down; a ship dies
//! when its hull reaches zero. This mirrors the headless `drift-combat` rule, but
//! in render-space `f32` and with no graphics or AI attached, so it is unit-tested
//! here and simply wrapped as a component by the Bevy app.
//!
//! Real-time combat is a *client-side* layer for the player's own fights: it is
//! authoritative for what happens in the cockpit, and reports outcomes back to the
//! abstract simulation as commands. It is not itself part of the deterministic sim.

/// Hull and shield for a combatant in a real-time fight.
#[derive(Debug, Clone, Copy)]
pub struct Health {
    pub hull: f32,
    pub max_hull: f32,
    pub shield: f32,
    pub max_shield: f32,
    pub shield_regen: f32,
}

impl Health {
    /// A ship with `hull` structure and a `shield` that regenerates `regen` points
    /// per second up to its starting value. The starting `hull`/`shield` are taken
    /// as the maxima, so [`hull_frac`](Self::hull_frac) and
    /// [`shield_frac`](Self::shield_frac) read as gauge fills.
    pub fn new(hull: f32, shield: f32, regen: f32) -> Self {
        Self { hull, max_hull: hull, shield, max_shield: shield, shield_regen: regen }
    }

    /// Hull as a fraction of maximum, for a gauge (0 if it has no hull).
    pub fn hull_frac(&self) -> f32 {
        if self.max_hull > 0.0 {
            (self.hull / self.max_hull).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }

    /// Apply `amount` damage: the shield absorbs first, the remainder cuts hull.
    pub fn take_damage(&mut self, amount: f32) {
        let absorbed = self.shield.min(amount);
        self.shield -= absorbed;
        self.hull -= amount - absorbed;
    }

    /// Whether the ship is still flying.
    pub fn alive(&self) -> bool {
        self.hull > 0.0
    }

    /// Regenerate the shield toward its maximum.
    pub fn regen(&mut self, dt: f32) {
        self.shield = (self.shield + self.shield_regen * dt).min(self.max_shield);
    }

    /// Shield as a fraction of maximum, for a bar (0 if it has no shield).
    pub fn shield_frac(&self) -> f32 {
        if self.max_shield > 0.0 {
            (self.shield / self.max_shield).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shield_absorbs_before_hull() {
        let mut h = Health::new(100.0, 20.0, 0.0);
        h.take_damage(15.0);
        assert_eq!(h.shield, 5.0);
        assert_eq!(h.hull, 100.0, "hull untouched while shield holds");
        h.take_damage(15.0); // 5 to shield, 10 to hull
        assert_eq!(h.shield, 0.0);
        assert_eq!(h.hull, 90.0);
        assert!(h.alive());
    }

    #[test]
    fn hull_depletion_is_death() {
        let mut h = Health::new(10.0, 0.0, 0.0);
        h.take_damage(100.0);
        assert!(!h.alive());
    }

    #[test]
    fn shield_regen_caps_at_max() {
        let mut h = Health::new(100.0, 30.0, 5.0);
        h.shield = 0.0;
        h.regen(1.0);
        assert_eq!(h.shield, 5.0);
        for _ in 0..100 {
            h.regen(1.0);
        }
        assert_eq!(h.shield, 30.0, "regen cannot exceed max");
        assert_eq!(h.shield_frac(), 1.0);
    }
}
