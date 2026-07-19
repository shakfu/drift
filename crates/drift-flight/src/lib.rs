//! `drift-flight` — the 3-D flight/combat client.
//!
//! Milestone 1 is a **combat spectator**: a Bevy app renders the galaxy in 3-D and
//! animates the simulation's multi-tick running battles, driven entirely by data
//! already on the wire (`EncounterView`). It does not pilot anything and it never
//! feeds state back into the simulation — the determinism firewall from
//! `docs/dev/flight-combat.md` holds: the sim advances on its own fixed tick and
//! the renderer is a pure read.
//!
//! The engine-agnostic models are the tested core and carry no graphics
//! dependency: [`scene`] (sim → 3-D geometry), [`flight`] (arcade kinematics),
//! [`combat`] (shield/hull health), and [`targeting`] (the lead/intercept solver
//! and radar-contact projection that give combat its Elite-style gunnery and
//! scanner). The Bevy app that draws them lives in the crate's binary behind the
//! `gui` feature, so the default workspace build stays free of the heavy graphics
//! stack — the verifiable logic is unit-tested here and merely driven by the app.

pub mod combat;
pub mod flight;
pub mod nav;
pub mod scene;
pub mod targeting;
pub mod weapons;
