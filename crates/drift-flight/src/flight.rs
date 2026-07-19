//! Arcade in-system flight — the engine-agnostic kinematic model for M2.
//!
//! A [`Ship`] carries a position, velocity, and orientation, and advances under
//! player [`Controls`] with an arcade feel: thrust accelerates along the ship's
//! nose, a light drag bleeds off speed so it is not purely Newtonian, and turning
//! rotates the ship in its own local axes. This is deliberately *not* the abstract
//! galaxy simulation — it is a real-time, client-side layer for the player's own
//! ship, tested here without any graphics engine.
//!
//! Conventions match Bevy's: `-Z` is forward, `+Y` is up. Types are `glam`'s, which
//! are the same types Bevy uses, so the app moves a ship by writing this model's
//! `position`/`rotation` straight onto a `Transform`.

use glam::{EulerRot, Quat, Vec3};

/// Thrust acceleration (world units / s^2) at full throttle.
const ACCEL: f32 = 45.0;
/// Speed cap (world units / s).
const MAX_SPEED: f32 = 140.0;
/// Turn rate (radians / s) at full deflection, per axis — brisk enough to keep a
/// bogey in the reticle in a dogfight.
const TURN_RATE: f32 = 2.2;
/// Fraction of speed bled off per second with no thrust — the arcade "space
/// friction" that makes the ship handle rather than drift forever.
const DRAG: f32 = 0.5;

/// A ship's real-time kinematic state.
#[derive(Debug, Clone, Copy)]
pub struct Ship {
    pub position: Vec3,
    pub velocity: Vec3,
    pub rotation: Quat,
}

impl Default for Ship {
    fn default() -> Self {
        Self {
            position: Vec3::ZERO,
            velocity: Vec3::ZERO,
            rotation: Quat::IDENTITY,
        }
    }
}

/// Player flight input, each component in `[-1, 1]`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Controls {
    /// Forward (`+1`) / reverse (`-1`) throttle.
    pub thrust: f32,
    /// Nose up (`+1`) / down (`-1`).
    pub pitch: f32,
    /// Nose left (`+1`) / right (`-1`).
    pub yaw: f32,
    /// Roll left (`+1`) / right (`-1`).
    pub roll: f32,
}

impl Ship {
    /// The ship's forward direction (its nose), a unit vector.
    pub fn forward(&self) -> Vec3 {
        self.rotation * Vec3::NEG_Z
    }

    /// The ship's up direction, a unit vector.
    pub fn up(&self) -> Vec3 {
        self.rotation * Vec3::Y
    }

    /// Current speed (world units / s).
    pub fn speed(&self) -> f32 {
        self.velocity.length()
    }

    /// Advance the ship by `dt` seconds under `c`. Rotation is applied in the
    /// ship's local axes; thrust accelerates along the (post-rotation) nose; drag
    /// and a speed cap keep handling arcade-tight.
    pub fn step(&mut self, c: &Controls, dt: f32) {
        // Turn first, in local space, so thrust applies along the new heading.
        let turn = Quat::from_euler(
            EulerRot::YXZ,
            c.yaw * TURN_RATE * dt,
            c.pitch * TURN_RATE * dt,
            c.roll * TURN_RATE * dt,
        );
        self.rotation = (self.rotation * turn).normalize();

        // Thrust along the nose.
        self.velocity += self.forward() * (c.thrust * ACCEL) * dt;
        // Light drag, then clamp speed.
        self.velocity *= (1.0 - DRAG * dt).clamp(0.0, 1.0);
        let speed = self.velocity.length();
        if speed > MAX_SPEED {
            self.velocity = self.velocity / speed * MAX_SPEED;
        }

        self.position += self.velocity * dt;
    }
}

/// A held throttle setpoint in `[-1, 1]` with a **zero detent**.
///
/// Set-and-hold throttle (Elite-style) rather than momentary thrust: the client
/// slews it up/down and the flight model thrusts to hold it. The detent makes zero
/// sticky — slewing into `0` parks there and holds while the key is still down, so
/// the pilot settles on a dead stop without sliding through into reverse; crossing
/// into the opposite sign takes a deliberate release-and-press. Kept here (engine-
/// agnostic) so the detent behaviour is unit-tested; the app wraps it as a Bevy
/// resource.
#[derive(Debug, Clone, Copy, Default)]
pub struct Throttle {
    /// Current setpoint, `-1` (full reverse) to `+1` (full ahead).
    pub level: f32,
    /// Latched once the throttle has parked at the zero detent. Cleared when the
    /// throttle keys are released (`step` with `input == 0`), which re-arms
    /// crossing zero on the next press.
    pub at_zero_detent: bool,
}

impl Throttle {
    /// Advance the throttle one frame. `input` is the raw axis (`+1` raise, `-1`
    /// lower, `0` released), `rate` the slew per second, `dt` the timestep.
    pub fn step(&mut self, input: i32, rate: f32, dt: f32) {
        if input == 0 {
            // Released: re-arm so the next press may cross zero.
            self.at_zero_detent = false;
            return;
        }
        let next = self.level + input as f32 * rate * dt;
        if (self.level > 0.0 && next <= 0.0) || (self.level < 0.0 && next >= 0.0) {
            // Slewed into the zero detent: stop exactly at zero and latch.
            self.level = 0.0;
            self.at_zero_detent = true;
        } else if self.at_zero_detent && self.level == 0.0 {
            // Parked at the detent with the key still held: hold zero until release.
            self.level = 0.0;
        } else {
            self.level = next.clamp(-1.0, 1.0);
        }
    }

    /// Cut to a parked dead stop (the client's `X` key).
    pub fn stop(&mut self) {
        self.level = 0.0;
        self.at_zero_detent = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: Vec3, b: Vec3) -> bool {
        (a - b).length() < 1e-3
    }

    #[test]
    fn throttle_parks_at_zero_without_overshooting_into_reverse() {
        // Held reverse from a positive setting must settle on a dead stop, not slide
        // through into negative — the whole point of the detent.
        let mut t = Throttle { level: 0.6, at_zero_detent: false };
        for _ in 0..1000 {
            t.step(-1, 1.2, 0.1);
            assert!(t.level >= 0.0, "held reverse never overshoots past zero: {}", t.level);
        }
        assert_eq!(t.level, 0.0, "settled exactly on the stop");
        assert!(t.at_zero_detent, "and latched the detent");
    }

    #[test]
    fn crossing_zero_needs_a_fresh_press() {
        // Parked at the detent, still holding down: stays at zero.
        let mut t = Throttle { level: 0.0, at_zero_detent: true };
        t.step(-1, 1.2, 0.1);
        assert_eq!(t.level, 0.0, "held key stays parked at the detent");
        // Release re-arms, then a fresh press crosses into reverse.
        t.step(0, 1.2, 0.1);
        assert!(!t.at_zero_detent, "release clears the latch");
        t.step(-1, 1.2, 0.1);
        assert!(t.level < 0.0, "a fresh press crosses into reverse");
    }

    #[test]
    fn detent_is_symmetric_from_reverse() {
        // Raising out of reverse also stops at zero rather than shooting to forward.
        let mut t = Throttle { level: -0.5, at_zero_detent: false };
        for _ in 0..1000 {
            t.step(1, 1.2, 0.1);
            assert!(t.level <= 0.0, "held forward from reverse never overshoots: {}", t.level);
        }
        assert_eq!(t.level, 0.0);
        assert!(t.at_zero_detent);
    }

    #[test]
    fn throttle_is_clamped_and_stop_parks() {
        let mut t = Throttle::default();
        for _ in 0..1000 {
            t.step(1, 1.2, 0.1);
        }
        assert!(t.level <= 1.0 + 1e-6 && t.level >= 0.99, "reaches and holds full ahead");
        t.stop();
        assert_eq!(t.level, 0.0);
        assert!(t.at_zero_detent, "stop parks at the detent");
    }

    #[test]
    fn thrust_accelerates_along_the_nose() {
        let mut s = Ship::default(); // facing -Z
        s.step(&Controls { thrust: 1.0, ..Default::default() }, 0.1);
        // Moved forward (toward -Z), with no sideways drift.
        assert!(s.position.z < 0.0, "thrust moves the ship forward: {:?}", s.position);
        assert!(s.position.x.abs() < 1e-4 && s.position.y.abs() < 1e-4);
        assert!(s.speed() > 0.0);
    }

    #[test]
    fn drag_bleeds_speed_with_no_thrust() {
        let mut s = Ship { velocity: Vec3::new(0.0, 0.0, -50.0), ..Default::default() };
        let before = s.speed();
        for _ in 0..10 {
            s.step(&Controls::default(), 0.1);
        }
        assert!(s.speed() < before, "coasting bleeds speed: {} -> {}", before, s.speed());
    }

    #[test]
    fn speed_is_capped() {
        let mut s = Ship::default();
        // Full throttle for a long time can never exceed the cap.
        for _ in 0..1000 {
            s.step(&Controls { thrust: 1.0, ..Default::default() }, 0.1);
        }
        assert!(s.speed() <= MAX_SPEED + 1e-2, "speed cap holds: {}", s.speed());
    }

    #[test]
    fn yaw_rotates_the_heading() {
        let mut s = Ship::default();
        let f0 = s.forward();
        // Yaw for a bit; the nose should swing away from straight ahead.
        for _ in 0..20 {
            s.step(&Controls { yaw: 1.0, ..Default::default() }, 0.05);
        }
        let f1 = s.forward();
        assert!(!approx(f0, f1), "yaw changes the heading");
        assert!((f1.length() - 1.0).abs() < 1e-3, "heading stays a unit vector");
    }
}
