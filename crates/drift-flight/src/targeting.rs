//! Targeting math — the engine-agnostic core of Elite-style gunnery and the
//! contact scanner.
//!
//! This is where the *defining* combat mechanics live, kept free of any graphics
//! engine so they are unit-tested here and merely *driven* by the Bevy app:
//!
//! - [`firing_solution`] — the **lead / intercept** calculation. Fixed weapons
//!   fire straight ahead, so hitting a moving target means aiming at where it
//!   *will be*. This is the skill mechanic of Elite combat (the gunnery "pip"),
//!   and it powers both the player's lead indicator and enemies that lead their
//!   shots instead of firing at where you already were.
//! - [`radar_contact`] — projects a world contact into the ship's local frame as
//!   a bearing / range / elevation, the classic Elite radar disc (a contact's
//!   position *relative to your facing*, with a stalk for above/below plane).
//! - [`nearest`] / [`cycle`] — target selection over a set of contacts.
//!
//! Conventions match [`crate::flight`] and Bevy: `-Z` is forward, `+X` right,
//! `+Y` up. Types are `glam`'s (the same types Bevy uses), so the app consumes
//! these results directly.

use glam::{Quat, Vec2, Vec3};

/// A weapon lead solution: how to aim, where the shot connects, and when.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FiringSolution {
    /// Unit aim direction from the shooter toward the intercept point. A fixed
    /// weapon hits the target when its nose is aligned with this.
    pub aim: Vec3,
    /// World-space point where the projectile and the target coincide — the spot
    /// to paint the lead pip.
    pub point: Vec3,
    /// Seconds from firing to intercept.
    pub time: f32,
}

/// Compute the lead solution to hit a target with a projectile of muzzle speed
/// `projectile_speed`.
///
/// `target_vel` is the target's velocity **relative to the shooter** (in this
/// game bolts inherit the shooter's velocity, so pass `target_vel - shooter_vel`;
/// for a stationary shooter it is just the target's velocity). Returns `None` when
/// no forward-in-time intercept exists — e.g. the target outruns the projectile
/// while receding — in which case a shooter should fall back to aiming straight at
/// the target's present position.
///
/// Solves `|p + v t| = s t` for the earliest `t > 0`, where `p` is the relative
/// position, `v = target_vel`, and `s = projectile_speed`. Squaring gives the
/// quadratic `(v·v - s²) t² + 2(p·v) t + (p·p) = 0`.
pub fn firing_solution(
    shooter_pos: Vec3,
    projectile_speed: f32,
    target_pos: Vec3,
    target_vel: Vec3,
) -> Option<FiringSolution> {
    let p = target_pos - shooter_pos;
    let s = projectile_speed;

    // Degenerate: already coincident. No meaningful direction; let the caller
    // decide (it will typically just hold fire).
    let dist = p.length();
    if dist < 1e-6 {
        return None;
    }
    if s <= 0.0 {
        return None;
    }

    let a = target_vel.dot(target_vel) - s * s;
    let b = 2.0 * p.dot(target_vel);
    let c = p.dot(p);

    // Earliest positive root of a t² + b t + c = 0.
    let t = if a.abs() < 1e-6 {
        // Linear case: target closing speed ~ projectile speed.
        if b.abs() < 1e-9 {
            return None;
        }
        let t = -c / b;
        if t > 0.0 {
            Some(t)
        } else {
            None
        }
    } else {
        let disc = b * b - 4.0 * a * c;
        if disc < 0.0 {
            None
        } else {
            let sq = disc.sqrt();
            let t0 = (-b - sq) / (2.0 * a);
            let t1 = (-b + sq) / (2.0 * a);
            // Smallest strictly-positive root.
            match (t0 > 1e-6, t1 > 1e-6) {
                (true, true) => Some(t0.min(t1)),
                (true, false) => Some(t0),
                (false, true) => Some(t1),
                (false, false) => None,
            }
        }
    }?;

    let point = target_pos + target_vel * t;
    let aim = (point - shooter_pos).normalize_or_zero();
    if aim == Vec3::ZERO {
        return None;
    }
    Some(FiringSolution { aim, point, time: t })
}

/// Resolve a **gimballed** weapon's aim: a gimbal mount tracks a locked target
/// automatically, but only within a cone of `half_angle` radians about the ship's
/// nose. Returns the tracking direction (`target_aim`, typically a
/// [`firing_solution`]'s `aim`) when the target lies inside the cone, or `None`
/// when it is outside the gimbal arc — in which case the weapon cannot converge
/// and the caller falls back to firing straight along the nose.
///
/// `nose` and `target_aim` are treated as directions and normalised defensively.
pub fn gimbal_aim(nose: Vec3, target_aim: Vec3, half_angle: f32) -> Option<Vec3> {
    let n = nose.normalize_or_zero();
    let a = target_aim.normalize_or_zero();
    if n == Vec3::ZERO || a == Vec3::ZERO {
        return None;
    }
    // Within the cone iff the angle between the vectors is <= half_angle, i.e.
    // their dot (both unit) is >= cos(half_angle).
    if n.dot(a) >= half_angle.cos() {
        Some(a)
    } else {
        None
    }
}

/// Steer a homing **missile** one step toward its target and return its new
/// velocity. The missile flies at constant `speed` and turns its heading toward
/// the target's intercept point (via [`firing_solution`], so it cuts the corner on
/// a crossing target) at up to `turn_rate` radians per second — a hard turn limit
/// is what makes a missile dodgeable and ECM worthwhile.
///
/// If the missile is not yet moving it launches straight at the aim point; if the
/// target is unreachable by a lead solution it steers at the target's present
/// position.
pub fn home_missile(
    pos: Vec3,
    vel: Vec3,
    target_pos: Vec3,
    target_vel: Vec3,
    speed: f32,
    turn_rate: f32,
    dt: f32,
) -> Vec3 {
    let aim = firing_solution(pos, speed, target_pos, target_vel)
        .map(|s| s.aim)
        .unwrap_or_else(|| (target_pos - pos).normalize_or_zero());
    if aim == Vec3::ZERO {
        return vel;
    }
    let cur = {
        let n = vel.normalize_or_zero();
        if n == Vec3::ZERO { aim } else { n }
    };
    let max = (turn_rate * dt).max(0.0);
    let ang = cur.dot(aim).clamp(-1.0, 1.0).acos();
    let dir = if ang <= max || ang < 1e-6 {
        aim
    } else {
        let axis = cur.cross(aim).normalize_or_zero();
        if axis == Vec3::ZERO {
            aim // antiparallel: snap to the aim (rare, and next step is fine)
        } else {
            (Quat::from_axis_angle(axis, max) * cur).normalize_or_zero()
        }
    };
    dir * speed
}

/// A world contact projected into a ship's local frame, for the radar/scanner.
///
/// The scanner is read *relative to your facing*: `bearing` is the horizontal
/// angle from your nose, `elevation` the signed height above/below your plane, and
/// `planar_range`/`range` the flat and true distances. [`disc`](Self::disc) maps
/// it onto a unit radar disc for drawing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RadarContact {
    /// Horizontal bearing in radians: `0` dead ahead, `+` to the right, wrapping
    /// to `±π` directly behind.
    pub bearing: f32,
    /// Distance to the contact within the ship's local horizontal plane.
    pub planar_range: f32,
    /// Signed local height: `+` above the plane, `-` below.
    pub elevation: f32,
    /// True straight-line distance to the contact.
    pub range: f32,
    /// Whether the contact is in the forward hemisphere.
    pub ahead: bool,
}

impl RadarContact {
    /// Position on a unit radar disc (`+Y` is dead ahead / top, `+X` is right),
    /// with the planar range scaled so `max_range` reaches the rim and anything
    /// beyond is clamped to it. Elevation is intentionally *not* folded in — draw
    /// it separately (a stalk or colour) as Elite does.
    pub fn disc(&self, max_range: f32) -> Vec2 {
        let r = if max_range > 0.0 {
            (self.planar_range / max_range).clamp(0.0, 1.0)
        } else {
            0.0
        };
        Vec2::new(self.bearing.sin() * r, self.bearing.cos() * r)
    }
}

/// Project `contact_pos` into the observer's local frame for the scanner.
pub fn radar_contact(observer_pos: Vec3, observer_rot: Quat, contact_pos: Vec3) -> RadarContact {
    // World offset rotated into the ship's local axes (inverse of its rotation).
    let local = observer_rot.inverse() * (contact_pos - observer_pos);
    // Local frame: -Z forward, +X right, +Y up.
    let forward = -local.z;
    let right = local.x;
    let up = local.y;
    let planar_range = (right * right + forward * forward).sqrt();
    RadarContact {
        bearing: right.atan2(forward),
        planar_range,
        elevation: up,
        range: local.length(),
        ahead: forward >= 0.0,
    }
}

/// Index of the contact nearest to `from`, or `None` if there are none.
pub fn nearest(from: Vec3, contacts: &[Vec3]) -> Option<usize> {
    contacts
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            from.distance_squared(**a)
                .total_cmp(&from.distance_squared(**b))
        })
        .map(|(i, _)| i)
}

/// The next target index when cycling with a lock key. Advances (wrapping) from
/// `current`, starts at `0` when nothing is locked, and yields `None` for an empty
/// contact set.
pub fn cycle(current: Option<usize>, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(match current {
        Some(i) => (i + 1) % len,
        None => 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f32, b: f32, eps: f32) -> bool {
        (a - b).abs() < eps
    }

    #[test]
    fn stationary_target_is_aimed_at_directly() {
        // Target dead ahead, not moving: aim straight at it, hit at dist/speed.
        let sol = firing_solution(Vec3::ZERO, 100.0, Vec3::new(0.0, 0.0, -200.0), Vec3::ZERO)
            .expect("a stationary target in range is always solvable");
        assert!((sol.aim - Vec3::NEG_Z).length() < 1e-4, "aim points at the target");
        assert!((sol.point - Vec3::new(0.0, 0.0, -200.0)).length() < 1e-3);
        assert!(approx(sol.time, 2.0, 1e-3), "time = 200 / 100");
    }

    #[test]
    fn lead_solution_actually_intercepts() {
        // A crossing target: the solution must be self-consistent — at time t the
        // projectile (fired along `aim` at `speed`) and the target are at the same
        // point. This is the property the whole mechanic rests on.
        let shooter = Vec3::new(0.0, 0.0, 0.0);
        let speed = 120.0;
        let target = Vec3::new(50.0, 0.0, -150.0);
        let vel = Vec3::new(40.0, 10.0, 0.0); // crossing + climbing
        let sol = firing_solution(shooter, speed, target, vel).expect("solvable");

        // Where the projectile is at t, flying from the shooter along `aim`.
        let proj_at_t = shooter + sol.aim * speed * sol.time;
        // Where the target is at t.
        let tgt_at_t = target + vel * sol.time;
        assert!(
            (proj_at_t - tgt_at_t).length() < 1e-2,
            "projectile and target coincide at t: {proj_at_t:?} vs {tgt_at_t:?}"
        );
        // And the lead point is genuinely ahead of the target's current position.
        assert!(sol.point.x > target.x, "lead leads the crossing motion");
    }

    #[test]
    fn a_target_outrunning_the_bolt_has_no_solution() {
        // Directly receding faster than the projectile: never catchable.
        let sol = firing_solution(
            Vec3::ZERO,
            50.0,
            Vec3::new(0.0, 0.0, -100.0),
            Vec3::new(0.0, 0.0, -80.0), // fleeing at 80 > 50
        );
        assert!(sol.is_none(), "a faster receding target is unhittable");
    }

    #[test]
    fn head_on_target_is_solvable_and_leads_short() {
        // Approaching head-on: still solvable, intercept sooner than the static
        // case because the target closes the gap.
        let sol = firing_solution(
            Vec3::ZERO,
            100.0,
            Vec3::new(0.0, 0.0, -200.0),
            Vec3::new(0.0, 0.0, 50.0), // approaching
        )
        .expect("head-on is solvable");
        assert!(sol.time < 2.0, "closing target is hit before the static 2.0s");
    }

    /// Fly a missile step-by-step and return the closest it ever got to a target
    /// moving at constant velocity — the real test of homing.
    fn closest_approach(
        mut pos: Vec3,
        mut vel: Vec3,
        mut target: Vec3,
        tvel: Vec3,
        speed: f32,
        turn_rate: f32,
    ) -> f32 {
        let dt = 1.0 / 30.0;
        let mut best = f32::MAX;
        for _ in 0..400 {
            vel = home_missile(pos, vel, target, tvel, speed, turn_rate, dt);
            pos += vel * dt;
            target += tvel * dt;
            best = best.min(pos.distance(target));
        }
        best
    }

    #[test]
    fn missile_homes_onto_a_stationary_target() {
        // Launched 90 degrees off-axis, a nimble missile still comes around and hits.
        let hit = closest_approach(
            Vec3::ZERO,
            Vec3::new(60.0, 0.0, 0.0), // flying +X, target is dead ahead -Z
            Vec3::new(0.0, 0.0, -120.0),
            Vec3::ZERO,
            60.0,
            3.0,
        );
        assert!(hit < 3.0, "missile should reach the target, closest was {hit}");
    }

    #[test]
    fn missile_leads_a_crossing_target() {
        let hit = closest_approach(
            Vec3::ZERO,
            Vec3::new(0.0, 0.0, -60.0),
            Vec3::new(20.0, 0.0, -150.0),
            Vec3::new(30.0, 0.0, 0.0), // crossing
            70.0,
            3.5,
        );
        assert!(hit < 4.0, "missile should intercept the crossing target, closest was {hit}");
    }

    #[test]
    fn a_sluggish_missile_can_be_outturned() {
        // A very low turn rate against a fast crossing target: it misses by a lot,
        // which is exactly why ECM and jinking matter.
        let hit = closest_approach(
            Vec3::ZERO,
            Vec3::new(0.0, 0.0, -80.0),
            Vec3::new(5.0, 0.0, -40.0),
            Vec3::new(60.0, 0.0, 0.0),
            80.0,
            0.15,
        );
        assert!(hit > 5.0, "a sluggish missile should be dodgeable, closest was {hit}");
    }

    #[test]
    fn missile_keeps_constant_speed() {
        let v = home_missile(
            Vec3::ZERO,
            Vec3::new(0.0, 0.0, -50.0),
            Vec3::new(30.0, 0.0, -100.0),
            Vec3::ZERO,
            75.0,
            3.0,
            1.0 / 30.0,
        );
        assert!((v.length() - 75.0).abs() < 1e-3, "speed held at 75, got {}", v.length());
    }

    #[test]
    fn gimbal_tracks_within_the_cone_and_drops_outside() {
        let nose = Vec3::NEG_Z;
        let cone = 0.2; // ~11.5 degrees

        // A target aim a few degrees off the nose: the gimbal tracks it exactly.
        let near = Quat::from_rotation_y(0.1) * Vec3::NEG_Z;
        let tracked = gimbal_aim(nose, near, cone).expect("inside the cone");
        assert!((tracked - near.normalize()).length() < 1e-4, "tracks the target aim");

        // Just inside vs. just outside the cone edge.
        let inside = Quat::from_rotation_y(0.19) * Vec3::NEG_Z;
        let outside = Quat::from_rotation_y(0.21) * Vec3::NEG_Z;
        assert!(gimbal_aim(nose, inside, cone).is_some(), "just inside tracks");
        assert!(gimbal_aim(nose, outside, cone).is_none(), "just outside drops lock");

        // A target dead astern is never in a forward gimbal arc.
        assert!(gimbal_aim(nose, Vec3::Z, cone).is_none());
    }

    #[test]
    fn radar_places_contacts_by_bearing() {
        let obs = Vec3::ZERO;
        let rot = Quat::IDENTITY; // facing -Z

        // Dead ahead.
        let ahead = radar_contact(obs, rot, Vec3::new(0.0, 0.0, -100.0));
        assert!(approx(ahead.bearing, 0.0, 1e-4), "ahead bearing ~ 0");
        assert!(ahead.ahead);
        assert!(approx(ahead.elevation, 0.0, 1e-4));
        assert!(approx(ahead.range, 100.0, 1e-3));

        // To the right (+X).
        let right = radar_contact(obs, rot, Vec3::new(100.0, 0.0, 0.0));
        assert!(approx(right.bearing, std::f32::consts::FRAC_PI_2, 1e-4), "right ~ +90");

        // Behind (+Z).
        let behind = radar_contact(obs, rot, Vec3::new(0.0, 0.0, 100.0));
        assert!(approx(behind.bearing.abs(), std::f32::consts::PI, 1e-4), "behind ~ ±180");
        assert!(!behind.ahead);

        // Above (+Y).
        let above = radar_contact(obs, rot, Vec3::new(0.0, 80.0, -1.0));
        assert!(above.elevation > 0.0, "above the plane");
    }

    #[test]
    fn radar_is_relative_to_facing() {
        // Yaw the observer 90 deg to the left (nose swings toward +X world); a
        // contact on +X world should now read as dead ahead.
        let rot = Quat::from_rotation_y(-std::f32::consts::FRAC_PI_2);
        // Sanity: this rotation turns the nose (-Z) toward +X.
        let nose = rot * Vec3::NEG_Z;
        assert!(nose.x > 0.9, "nose now points +X: {nose:?}");

        let c = radar_contact(Vec3::ZERO, rot, Vec3::new(100.0, 0.0, 0.0));
        assert!(approx(c.bearing, 0.0, 1e-3), "contact is dead ahead after the turn");
        assert!(c.ahead);
    }

    #[test]
    fn disc_maps_ahead_to_top_and_right_to_right() {
        let ahead = radar_contact(Vec3::ZERO, Quat::IDENTITY, Vec3::new(0.0, 0.0, -50.0));
        let d = ahead.disc(100.0);
        assert!(d.y > 0.0 && approx(d.x, 0.0, 1e-4), "ahead maps to the top: {d:?}");

        let right = radar_contact(Vec3::ZERO, Quat::IDENTITY, Vec3::new(50.0, 0.0, 0.0));
        let d = right.disc(100.0);
        assert!(d.x > 0.0 && approx(d.y, 0.0, 1e-4), "right maps to the right: {d:?}");
    }

    #[test]
    fn disc_clamps_beyond_max_range() {
        let far = radar_contact(Vec3::ZERO, Quat::IDENTITY, Vec3::new(0.0, 0.0, -10_000.0));
        let d = far.disc(100.0);
        assert!(d.length() <= 1.0 + 1e-6, "a distant contact sits on the rim");
    }

    #[test]
    fn nearest_picks_the_closest() {
        let from = Vec3::ZERO;
        let contacts = [
            Vec3::new(0.0, 0.0, -300.0),
            Vec3::new(10.0, 0.0, 0.0), // closest
            Vec3::new(0.0, 200.0, 0.0),
        ];
        assert_eq!(nearest(from, &contacts), Some(1));
        assert_eq!(nearest(from, &[]), None);
    }

    #[test]
    fn cycle_wraps_and_bootstraps() {
        assert_eq!(cycle(None, 3), Some(0), "locks the first when nothing is held");
        assert_eq!(cycle(Some(0), 3), Some(1));
        assert_eq!(cycle(Some(2), 3), Some(0), "wraps around");
        assert_eq!(cycle(Some(0), 0), None, "no contacts, no lock");
    }
}
