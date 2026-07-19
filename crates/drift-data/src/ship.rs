//! Ship schema.
//!
//! The trading sim consumes `cargo_capacity` and `jump_speed`; the combat model
//! consumes `hull`, `max_speed`, and the optional [`CombatStats`]. A ship with no
//! `combat` block is an unarmed civilian in an encounter.

use serde::{Deserialize, Serialize};

/// Combat loadout for a ship. Optional on [`ShipDef`]; absent means unarmed.
/// `Default` is all-zero, i.e. an inert, unarmed ship.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CombatStats {
    /// Shield hit points; absorbs damage before the hull and regenerates.
    pub shield: u32,
    /// Shield points regenerated per tick.
    pub shield_regen: f64,
    /// Damage dealt per successful shot.
    pub weapon_damage: u32,
    /// Maximum engagement distance for the weapon.
    pub weapon_range: f64,
    /// Ticks between shots.
    pub weapon_cooldown: u32,
    /// Point-blank hit probability in `[0, 1]`; falls off linearly to zero at
    /// `weapon_range`.
    pub accuracy: f64,
    /// Steering acceleration (velocity change per tick) when manoeuvring.
    pub acceleration: f64,
}

/// The silhouette family a ship's hull is built from. A renderer maps this plus
/// the [`ShipVisual`] dimensions to a mesh; it carries no engine types, so the
/// data stays engine-agnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HullShape {
    /// A sleek, pointed dart — fighters and light craft.
    Dart,
    /// A bulky hull with a hexagonal cross-section — traders and freighters.
    Freighter,
}

/// How a ship looks: the data-driven hull description a 3-D client renders. Kept
/// as plain dimensions and an RGB tint (not engine types) so a mod can introduce
/// a new ship's appearance without any client code change. Optional on
/// [`ShipDef`]; a ship without one falls back to a generic hull.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShipVisual {
    /// Hull silhouette family.
    pub hull: HullShape,
    /// Overall nose-to-tail length in world units.
    pub length: f32,
    /// Half-width (wingspan / beam) in world units.
    pub width: f32,
    /// Dorsal height in world units.
    pub height: f32,
    /// Hull tint as linear RGB in `[0, 1]`.
    pub color: [f32; 3],
}

/// A ship variant. NPC traders and (later) the player fly these.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShipDef {
    /// Namespaced unique id (e.g. `"core:cobra_mk3"`).
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Cargo hold capacity in mass units.
    pub cargo_capacity: u32,
    /// Jump speed: distance units traversable per tick when travelling between
    /// systems. Higher = fewer ticks in transit.
    pub jump_speed: f64,
    /// Structural hull points.
    pub hull: u32,
    /// Maximum in-system flight speed.
    pub max_speed: f64,
    /// Combat loadout. `None` = unarmed civilian.
    #[serde(default)]
    pub combat: Option<CombatStats>,
    /// How the ship looks to a 3-D client. `None` = fall back to a generic hull.
    #[serde(default)]
    pub visual: Option<ShipVisual>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ship_ron_roundtrips() {
        let def = ShipDef {
            id: "core:cobra_mk3".into(),
            name: "Cobra Mk III".into(),
            cargo_capacity: 35,
            jump_speed: 7.0,
            hull: 100,
            max_speed: 350.0,
            combat: Some(CombatStats {
                shield: 50,
                shield_regen: 1.0,
                weapon_damage: 8,
                weapon_range: 40.0,
                weapon_cooldown: 2,
                accuracy: 0.9,
                acceleration: 30.0,
            }),
            visual: None,
        };
        let text = ron::to_string(&def).unwrap();
        let back: ShipDef = ron::from_str(&text).unwrap();
        assert_eq!(def, back);
    }

    #[test]
    fn combat_defaults_to_none_when_omitted() {
        let text = r#"(id: "x", name: "X", cargo_capacity: 1, jump_speed: 1.0, hull: 1, max_speed: 1.0)"#;
        let s: ShipDef = ron::from_str(text).unwrap();
        assert_eq!(s.combat, None);
        assert_eq!(s.visual, None, "visual is optional and defaults to none");
    }

    #[test]
    fn visual_parses_from_ron() {
        let text = r#"(id: "x", name: "X", cargo_capacity: 1, jump_speed: 1.0, hull: 1, max_speed: 1.0,
            visual: Some((hull: dart, length: 4.0, width: 1.8, height: 0.5, color: (0.7, 0.8, 0.9))))"#;
        let s: ShipDef = ron::from_str(text).unwrap();
        let v = s.visual.expect("visual present");
        assert_eq!(v.hull, HullShape::Dart);
        assert_eq!(v.length, 4.0);
        assert_eq!(v.color, [0.7, 0.8, 0.9]);
        // Round-trips.
        let back: ShipDef = ron::from_str(&ron::to_string(&s).unwrap()).unwrap();
        assert_eq!(s, back);
    }
}
