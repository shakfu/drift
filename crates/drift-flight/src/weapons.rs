//! Weapon subsystems — the engine-agnostic **laser temperature** model (Oolite's
//! laser overheating). Firing builds heat; heat dissipates over time; and a laser
//! pushed past its limit **cuts out** until it has cooled back down, so sustained
//! fire is self-limiting. Kept here so the rule is unit tested and merely driven
//! by the Bevy app (a temperature gauge and a fire gate).

/// Heat below which an overheated (cut-out) laser comes back online — hysteresis,
/// so it does not chatter on/off right at the limit.
const RESET_FRACTION: f32 = 0.35;

/// The laser temperature of a weapon mount, as a `0..=1` fraction of its limit.
#[derive(Debug, Clone, Copy, Default)]
pub struct WeaponHeat {
    heat: f32,
    /// Latched when the laser overheated; cleared once it cools to `RESET_FRACTION`.
    cut_out: bool,
}

impl WeaponHeat {
    pub fn new() -> Self {
        Self::default()
    }

    /// Heat as a `0..=1` fraction, for a gauge.
    pub fn frac(&self) -> f32 {
        self.heat.clamp(0.0, 1.0)
    }

    /// Whether the laser is currently cut out (too hot to fire).
    pub fn is_cut_out(&self) -> bool {
        self.cut_out
    }

    /// Whether the weapon may fire this instant.
    pub fn can_fire(&self) -> bool {
        !self.cut_out
    }

    /// Add heat from firing (a fraction of the limit). Overheats — latching the
    /// cut-out — once heat reaches the limit.
    pub fn add(&mut self, amount: f32) {
        self.heat = (self.heat + amount.max(0.0)).min(1.5);
        if self.heat >= 1.0 {
            self.cut_out = true;
        }
    }

    /// Dissipate heat over `dt` seconds at `rate` (fraction per second), clearing
    /// the cut-out once the laser has cooled enough to come back online.
    pub fn cool(&mut self, rate: f32, dt: f32) {
        self.heat = (self.heat - rate * dt).max(0.0);
        if self.cut_out && self.heat <= RESET_FRACTION {
            self.cut_out = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sustained_fire_overheats_and_cuts_out() {
        let mut h = WeaponHeat::new();
        // Fire until it trips.
        for _ in 0..20 {
            if h.can_fire() {
                h.add(0.1);
            }
        }
        assert!(h.is_cut_out(), "sustained fire must overheat");
        assert!(!h.can_fire(), "an overheated laser cannot fire");
        assert!(h.frac() >= 1.0 - 1e-6);
    }

    #[test]
    fn cooling_below_reset_brings_it_back_online() {
        let mut h = WeaponHeat::new();
        h.add(1.0);
        assert!(h.is_cut_out());
        // Still cut out just above the reset point.
        h.cool(1.0, 0.6); // heat 1.0 -> 0.4
        assert!(h.is_cut_out(), "not cooled enough yet");
        h.cool(1.0, 0.1); // heat 0.4 -> 0.3 (<= reset)
        assert!(!h.is_cut_out(), "cooled below reset: back online");
        assert!(h.can_fire());
    }

    #[test]
    fn measured_fire_never_overheats() {
        // Firing within the cooling budget keeps the laser online indefinitely.
        let mut h = WeaponHeat::new();
        for _ in 0..200 {
            h.cool(1.0, 0.2); // dissipate 0.2
            if h.can_fire() {
                h.add(0.15); // add less than we dissipate
            }
        }
        assert!(!h.is_cut_out(), "cool-limited fire never overheats");
        assert!(h.frac() < 1.0);
    }

    #[test]
    fn heat_never_goes_negative_or_unbounded() {
        let mut h = WeaponHeat::new();
        h.cool(1.0, 10.0);
        assert_eq!(h.frac(), 0.0);
        for _ in 0..100 {
            h.add(1.0);
        }
        assert!(h.frac() <= 1.0, "gauge fraction is clamped");
    }
}
