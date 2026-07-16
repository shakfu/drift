//! `drift-flight` — the 3-D flight/combat client.
//!
//! Milestone 1 is a **combat spectator**: a Bevy app renders the galaxy in 3-D and
//! animates the simulation's multi-tick running battles, driven entirely by data
//! already on the wire (`EncounterView`). It does not pilot anything and it never
//! feeds state back into the simulation — the determinism firewall from
//! `docs/dev/flight-combat.md` holds: the sim advances on its own fixed tick and
//! the renderer is a pure read.
//!
//! The engine-agnostic [`scene`] model (the sim → 3-D geometry mapping) is the
//! tested core and carries no graphics dependency. The Bevy app that draws a
//! [`scene::Scene`] lives in the crate's binary behind the `gui` feature, so the
//! default workspace build stays free of the heavy graphics stack.

pub mod combat;
pub mod flight;
pub mod scene;
