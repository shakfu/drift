//! In-system real-time flight, wired to the simulation (M2). The player *is* a
//! trader in the sim: you fly its ship arcade-style around its current star system,
//! the system is populated with the other agents the sim reports docked there, and
//! pressing a number key issues a real `Command::Jump` through the pipeline — the
//! sim moves the trader, and when it arrives the scene rebuilds for the new system.
//!
//! The determinism firewall holds: the sim advances on its own fixed tick and this
//! layer only *reads* it, except to feed validated player commands (jump) back
//! through the pipeline. Your free-flight position is client-side and never enters
//! the sim; the sim only knows which system your trader is docked at.
//!
//! Controls follow Oolite: **←/→** roll · **↑/↓** pitch · **, / .** yaw · **W/S**
//! throttle (held, with a zero detent) · **X** stop · **J** torus drive (fast
//! cruise, burns fuel, mass-locked near hostiles) · **A** fire · **M** launch a
//! homing missile at the locked target · **E** ECM (detonate incoming missiles) ·
//! **T** lock / cycle target · **R** clear lock · **Tab** switch weapon · **1-9**
//! jump (costs fuel by distance; refuel by docking). Attacking the navy makes you
//! wanted, and the police turn on a fugitive. Destroyed pirates spill **cargo
//! canisters** — fly over them to scoop the loot into your hold and sell it at a
//! station. Weapons run hot: sustained fire raises the laser **temperature** and
//! an overheated laser cuts out until it cools (watch the `TEMP` gauge); the
//! **beam laser** (a `Tab` weapon) melts a target in a cone but heats fast. A
//! contact scanner sits bottom-left, ship gauges (speed/throttle/hull/shield plus
//! roll and pitch indicators) bottom-right, and a compass to the station
//! bottom-centre; a locked target shows a reticle, a lead pip (where to aim), and
//! a data panel top-right. The
//! Pulse is a **gimballed** weapon — it auto-tracks the locked target within a cone
//! — while the Cannon and Scatter are **fixed** and fire straight down the nose.
//! Any **navy** ships in the system spawn as allies and fight the pirates alongside
//! you (teal on the scanner). The station is an Oolite-style rotating Coriolis
//! (cuboctahedron) with a docking slot. Ship hulls are **data-driven**: each
//! agent renders as its own ship type from the `visual` block on its `ShipDef`
//! (silhouette family + dimensions + tint), so a mod adds a ship's appearance with
//! no client change.
//! Run from the repo root: `cargo run -p drift-flight --features gui`.

use std::f32::consts::{PI, TAU};
use std::path::PathBuf;

use bevy::core_pipeline::bloom::Bloom;
use bevy::prelude::*;
use bevy::render::mesh::{Indices, PrimitiveTopology};
use bevy::render::render_asset::RenderAssetUsages;
use std::collections::HashMap;

use drift_core::{CommodityId, ShipId, SystemId};
use drift_data::{HullShape, ShipVisual};
use drift_economy::{Command, PatrolId, PlayerId, TraderLocation};
use drift_flight::combat::Health;
use drift_flight::flight::{Controls, Ship, Throttle as FlightThrottle};
use drift_flight::nav::{self, LegalStatus, MAX_FUEL};
use drift_flight::weapons::WeaponHeat;
use drift_flight::targeting::{
    cycle, firing_solution, gimbal_aim, home_missile, nearest, radar_contact,
};
use drift_sim::Session;

/// Projectile speed (world units / s) and how close a bolt must pass to hit.
const PROJECTILE_SPEED: f32 = 220.0;
const HIT_RADIUS: f32 = 3.0;

/// Homing-missile tuning: flight speed, turn limit (rad/s), warhead damage,
/// detonation radius, and self-destruct lifetime.
const MISSILE_SPEED: f32 = 160.0;
const MISSILE_TURN: f32 = 2.2;
const MISSILE_DAMAGE: f32 = 45.0;
const MISSILE_HIT_RADIUS: f32 = 4.0;
const MISSILE_TTL: f32 = 9.0;
/// Player missile stores (refilled on docking), launch spacing, and ECM cooldown.
const PLAYER_MISSILES: u32 = 4;
const MISSILE_COOLDOWN: f32 = 0.6;
const ECM_COOLDOWN: f32 = 2.5;

/// Torus-drive / fuel-injector cruise speed while engaged (world units / s) — a
/// fast hop to the station, disabled by mass-lock (any hostile present).
const INJECTOR_SPEED: f32 = 620.0;

/// How close the player must fly to a cargo canister to scoop it, and how long a
/// canister drifts before it decays.
const SCOOP_RADIUS: f32 = 8.0;
const CARGO_TTL: f32 = 30.0;

/// How fast the held throttle setpoint slews (fraction of full range per second).
const THROTTLE_RATE: f32 = 1.2;

/// Contact scanner: on-screen radius (px) and the world distance the rim maps to.
const RADAR_RADIUS_PX: f32 = 82.0;
const RADAR_RANGE: f32 = 500.0;
/// Size of the pooled radar blip nodes, and how many the scanner can show at once.
const RADAR_BLIP_PX: f32 = 6.0;
const RADAR_BLIPS: usize = 24;

/// The player this client controls.
const PLAYER: PlayerId = PlayerId(0);

/// Where the station sits in each system's local space, and how close you must fly
/// to dock with it.
const STATION_POS: Vec3 = Vec3::new(0.0, 0.0, -70.0);
const DOCK_RANGE: f32 = 20.0;

/// Cockpit interaction state: whether the player is docked at the station, and the
/// market cursor / trade quantity used while docked.
#[derive(Resource)]
struct Cockpit {
    docked: bool,
    cursor: usize,
    qty: u32,
}

/// The in-process simulation.
#[derive(Resource)]
struct Sim(Session);

/// Which trader the player flies, and the system it is currently in.
#[derive(Resource)]
struct PlayerState {
    trader: drift_economy::TraderId,
    /// Current docked system, or `None` while jumping between systems.
    system: Option<SystemId>,
    /// The system last rendered, to detect arrival at a new one.
    rendered: Option<SystemId>,
}


#[derive(Component)]
struct PlayerShip;
#[derive(Component)]
struct Flight(Ship);
#[derive(Component)]
struct ChaseCamera;
#[derive(Component)]
struct Hud;
/// A sim agent rendered in the current system (rebuilt each frame).
#[derive(Component)]
struct AgentShip;
/// A body that slowly spins about `+Y` for a sense of life (e.g. a planet).
#[derive(Component)]
struct Spin(f32);
/// A Coriolis station: rolls about its docking axis (`+Z`, facing the player),
/// the way Oolite's stations rotate.
#[derive(Component)]
struct StationSpin(f32);
/// Hull/shield of a combatant (player or hostile).
#[derive(Component)]
struct Combat(Health);
/// A hostile pirate flying real-time combat against the player, tagged with the
/// sim patrol it stands in for (so a kill can be reported back).
#[derive(Component)]
struct Hostile(PatrolId);

/// A friendly navy ship that fights on the player's side, spawned from the navy
/// patrols the sim reports in the current system.
#[derive(Component)]
struct Ally(PatrolId);
/// A weapon's cooldown timer (seconds until it can fire again).
#[derive(Component)]
struct FireCooldown(f32);
/// A weapon bolt in flight.
#[derive(Component)]
struct Projectile {
    vel: Vec3,
    damage: f32,
    /// `0` = friendly (player or ally), `1` = hostile. Friendly bolts hit
    /// hostiles; hostile bolts hit the player and allies.
    faction: u8,
    ttl: f32,
}

/// A homing missile in flight, tracking a specific `target` entity.
#[derive(Component)]
struct Missile {
    /// The entity this missile is chasing (a hostile for a player missile, or the
    /// player for a hostile missile).
    target: Entity,
    /// Current velocity (constant speed, steered by the homing system).
    vel: Vec3,
    /// `0` = friendly (fired by the player), `1` = hostile. ECM clears the other
    /// side's missiles.
    faction: u8,
    /// Self-destruct timer.
    ttl: f32,
}

/// A hostile's missile launcher: seconds until it can fire its next missile at the
/// player. Staggered per ship so a wing doesn't volley in unison.
#[derive(Component)]
struct MissileBay(f32);

/// A drifting cargo canister (loot from a kill). Fly over it to scoop it into the
/// hold via [`Command::ScoopCargo`].
#[derive(Component)]
struct Cargo {
    commodity: CommodityId,
    qty: u32,
    /// Drift velocity (bleeds off), so wreckage scatters a little.
    vel: Vec3,
    /// Decay timer.
    ttl: f32,
}

/// Shared cargo-canister assets (a small tumbling crate).
#[derive(Resource)]
struct CargoAssets {
    mesh: Handle<Mesh>,
    material: Handle<StandardMaterial>,
}

/// The player's missile stores and launch cooldown.
#[derive(Resource)]
struct PlayerMissile {
    ammo: u32,
    cooldown: f32,
}

/// Fuel in the tank (light-year units), spent by hyperspace jumps and the
/// injectors, refilled on docking.
#[derive(Resource)]
struct Fuel(f32);

/// The player's rap sheet: the bounty on their head. Attacking clean ships (the
/// navy) raises it; it drives the [`LegalStatus`] the navy reacts to.
#[derive(Resource, Default)]
struct Rap {
    bounty: u32,
}

impl Rap {
    fn status(&self) -> LegalStatus {
        nav::legal_status(self.bounty)
    }
}

/// ECM (electronic counter-measures) cooldown: seconds until it can fire again.
#[derive(Resource, Default)]
struct Ecm {
    cooldown: f32,
    /// A brief flash timer when ECM discharges, for the HUD.
    flash: f32,
}

/// Shared missile assets: a bright bolt mesh and per-faction materials.
#[derive(Resource)]
struct MissileAssets {
    mesh: Handle<Mesh>,
    player: Handle<StandardMaterial>,
    enemy: Handle<StandardMaterial>,
}

/// Which system's hostiles and allies are currently spawned (each rebuilt on
/// jump, tracked separately so a spawn pass for one does not shadow the other).
#[derive(Resource)]
struct Arena {
    last: Option<SystemId>,
    allies_last: Option<SystemId>,
}

/// Shared assets for weapon bolts: a small and a big bolt mesh, one material per
/// player weapon, and the hostile bolt material.
#[derive(Resource)]
struct ProjectileAssets {
    small_mesh: Handle<Mesh>,
    big_mesh: Handle<Mesh>,
    player: [Handle<StandardMaterial>; 3],
    enemy: Handle<StandardMaterial>,
    /// Bolt material for friendly allies (navy teal), distinct from the player's.
    ally: Handle<StandardMaterial>,
}

/// A player weapon type.
struct Weapon {
    name: &'static str,
    damage: f32,
    cooldown: f32,
    speed: f32,
    /// Bolts fired per shot (a spread when > 1).
    bolts: u8,
    /// Angular spread between bolts (radians).
    spread: f32,
    /// Use the big bolt mesh.
    big: bool,
    /// Gimbal half-angle (radians): if set, the weapon auto-tracks a locked target
    /// within this cone of the nose (Elite-style gimballed mount). `None` is a
    /// fixed weapon that always fires straight ahead.
    gimbal: Option<f32>,
    /// Laser heat added per shot (bolt weapons) or per second (a `beam`), as a
    /// fraction of the overheat limit. See [`WeaponHeat`].
    heat: f32,
    /// A continuous hitscan **beam** rather than discrete bolts: `damage` is read
    /// as damage-per-second and it hits the nearest hostile in a narrow forward
    /// cone (see `beam_fire`).
    beam: bool,
}

/// The player's selectable weapons: a fast **gimballed** pulse (weak, forgiving,
/// runs cool), a heavy fixed cannon (high damage, runs hot), a fixed three-bolt
/// scatter, and a continuous **beam laser** (melts a target but overheats fast).
const WEAPONS: [Weapon; 4] = [
    Weapon { name: "PULSE",   damage: 8.0,  cooldown: 0.18, speed: 240.0, bolts: 1, spread: 0.0,  big: false, gimbal: Some(0.22), heat: 0.05, beam: false },
    Weapon { name: "CANNON",  damage: 26.0, cooldown: 0.70, speed: 180.0, bolts: 1, spread: 0.0,  big: true,  gimbal: None,       heat: 0.16, beam: false },
    Weapon { name: "SCATTER", damage: 6.0,  cooldown: 0.50, speed: 205.0, bolts: 3, spread: 0.13, big: false, gimbal: None,       heat: 0.12, beam: false },
    Weapon { name: "BEAM",    damage: 60.0, cooldown: 0.0,  speed: 0.0,   bolts: 0, spread: 0.0,  big: false, gimbal: None,       heat: 0.55, beam: true  },
];

/// Laser cooling rate (heat fraction dissipated per second).
const LASER_COOL_RATE: f32 = 0.32;
/// Beam laser reach and its forward cone (cosine of the half-angle).
const BEAM_RANGE: f32 = 130.0;
const BEAM_COS: f32 = 0.985; // ~10 degrees

/// The player's selected weapon (index into [`WEAPONS`]).
#[derive(Resource)]
struct PlayerWeapon(usize);

/// The player laser's temperature / overheat state (Oolite laser temperature).
#[derive(Resource, Default)]
struct LaserHeat(WeaponHeat);

/// The rendered beam of the beam laser (a thin bright bar), shown only while the
/// beam is firing.
#[derive(Component)]
struct BeamVisual;

/// Held throttle setpoint (Elite-style set-and-hold): `W`/`S` slew it, `X` cuts to
/// a dead stop, and the flight model thrusts to hold it. Wraps the tested
/// [`drift_flight::flight::Throttle`], which carries the zero-detent behaviour.
#[derive(Resource, Default)]
struct Throttle(FlightThrottle);

/// The player's current attitude input (`roll`/`pitch` in `[-1, 1]`), published by
/// `fly` for the HUD's roll and pitch indicators to read.
#[derive(Resource, Default)]
struct FlightInput {
    roll: f32,
    pitch: f32,
}

/// The currently locked hostile (its entity), or `None`. Cycled with `T`, cleared
/// when it dies or with `R`.
#[derive(Resource, Default)]
struct LockedTarget(Option<Entity>);

/// Read-model of the locked target for the HUD and gunnery aids, refreshed each
/// frame by [`targeting`] so the text HUD does not re-query the world.
#[derive(Resource, Default)]
struct LockInfo {
    locked: bool,
    range: f32,
    /// Closing speed along the line of sight: `+` closing, `-` opening.
    closing: f32,
    /// Target hull and shield as gauge fractions in `[0, 1]`.
    hull_frac: f32,
    shield_frac: f32,
    /// Whether a forward-in-time firing solution exists (a lead pip is shown).
    firing: bool,
    /// Whether the player's nose is aligned with the lead solution (on target).
    on_target: bool,
    /// The lead solution's world aim direction, for a gimballed weapon to track.
    solution_aim: Option<Vec3>,
    /// Whether the selected weapon is gimballed and the target is inside its arc.
    gimbal_locked: bool,
}

/// The floating gunnery pip: where to aim so the current weapon's bolts intercept
/// the locked target. The skill cue of Elite gunnery.
#[derive(Component)]
struct LeadPip;

/// The bracket drawn on the locked target itself.
#[derive(Component)]
struct TargetReticle;

/// Root UI node of the contact scanner.
#[derive(Component)]
struct RadarRoot;

/// One pooled blip on the scanner, repositioned/recoloured each frame.
#[derive(Component)]
struct RadarBlip;

// --- HUD gauge markers (the fill node of each bar, width driven each frame) ---
#[derive(Component)]
struct SpeedFill;
#[derive(Component)]
struct ThrottleFill;
#[derive(Component)]
struct HullFill;
#[derive(Component)]
struct ShieldFill;
#[derive(Component)]
struct FuelFill;
#[derive(Component)]
struct TempFill;
/// The locked-target panel root (shown only when a target is locked).
#[derive(Component)]
struct TargetPanel;
#[derive(Component)]
struct TargetText;
#[derive(Component)]
struct TargetHullFill;
#[derive(Component)]
struct TargetShieldFill;
/// The sliding knob of a centre-zero indicator (roll or pitch); its horizontal
/// position tracks the attitude input.
#[derive(Component)]
struct RollKnob;
#[derive(Component)]
struct PitchKnob;
/// The moving blip on the compass disc, pointing toward the station.
#[derive(Component)]
struct CompassDot;

/// A short-lived explosion flash that grows and vanishes.
#[derive(Component)]
struct Explosion {
    age: f32,
    max: f32,
}

/// A flying explosion fragment.
#[derive(Component)]
struct Fragment {
    vel: Vec3,
    ttl: f32,
}

/// Explosion visuals (created once).
#[derive(Resource)]
struct ExplosionAssets {
    flash_mesh: Handle<Mesh>,
    flash_mat: Handle<StandardMaterial>,
    frag_mesh: Handle<Mesh>,
    frag_mat: Handle<StandardMaterial>,
}
/// The full-screen flash shown while jumping between systems.
#[derive(Component)]
struct TransitVeil;

/// Handles to the per-system scenery re-flavoured on each jump.
#[derive(Resource)]
struct Scenery {
    star: Entity,
    planet: Entity,
    last: Option<SystemId>,
}

fn main() {
    let (session, player) = start_session();
    App::new()
        .add_plugins(DefaultPlugins)
        .insert_resource(Sim(session))
        .insert_resource(player)
        .insert_resource(Cockpit { docked: false, cursor: 0, qty: 5 })
        .insert_resource(Arena { last: None, allies_last: None })
        .insert_resource(PlayerWeapon(0))
        .insert_resource(LaserHeat::default())
        .insert_resource(Throttle::default())
        .insert_resource(FlightInput::default())
        .insert_resource(PlayerMissile { ammo: PLAYER_MISSILES, cooldown: 0.0 })
        .insert_resource(Ecm::default())
        .insert_resource(Fuel(MAX_FUEL))
        .insert_resource(Rap::default())
        .insert_resource(LockedTarget::default())
        .insert_resource(LockInfo::default())
        .insert_resource(ClearColor(Color::srgb(0.01, 0.01, 0.03)))
        .insert_resource(AmbientLight { brightness: 120.0, ..default() })
        // Advance the sim a few times a second, independent of the render rate.
        .insert_resource(Time::<Fixed>::from_hz(10.0))
        .add_systems(Startup, setup)
        .add_systems(FixedUpdate, advance_sim)
        .add_systems(
            Update,
            // Nested chains: Bevy's chained-tuple impls top out at 20 systems, so the
            // frame is grouped into three ordered sub-chains, themselves chained.
            (
                (
                    dock_input,
                    trade_input,
                    jump_input,
                    sync_system,
                    manage_hostiles,
                    manage_allies,
                    refresh_agents,
                )
                    .chain(),
                (
                    switch_weapon,
                    cool_laser,
                    player_fire,
                    beam_fire,
                    fire_missile,
                    hostile_ai,
                    ally_ai,
                    enemy_missiles,
                    ecm,
                    missile_homing,
                    move_projectiles,
                    projectile_hits,
                    regen_shields,
                    cull_dead,
                )
                    .chain(),
                (
                    throttle_input,
                    animate_flashes,
                    animate_fragments,
                    fly,
                    follow_camera,
                    apply_flavour,
                    spin,
                    spin_station,
                    transit_veil,
                    targeting,
                    move_cargo,
                    scoop_cargo,
                    update_hud,
                    update_radar,
                    update_gauges,
                    update_indicators,
                )
                    .chain(),
            )
                .chain(),
        )
        .run();
}

/// Load content, spawn a player-owned trader at Lave through the command pipeline,
/// and return the session plus the tracked player state.
fn start_session() -> (Session, PlayerState) {
    let args: Vec<String> = std::env::args().collect();
    let scenario = args.get(1).map_or("scenarios/equilibrium.ron", String::as_str);
    let mods = args.get(2).map_or("mods", String::as_str);
    let mut session = Session::load(&PathBuf::from(mods), &PathBuf::from(scenario), Some(42))
        .unwrap_or_else(|e| panic!("failed to load '{scenario}' / '{mods}': {e:?} (run from repo root)"));

    let (ship, start) = {
        let reg = session.world().registry();
        (
            reg.ship_id("core:cobra_mk3").expect("core:cobra_mk3 ship"),
            reg.system_id("core:lave").expect("core:lave system"),
        )
    };
    session.queue_command(Command::Spawn { player: PLAYER, ship, at: start, capital: 5000 });
    session.world_mut().tick();
    let trader = session
        .world()
        .traders()
        .iter()
        .find(|t| t.is_player())
        .expect("player trader present after spawn")
        .id;

    (session, PlayerState { trader, system: Some(start), rendered: None })
}

fn setup(
    mut commands: Commands,
    sim: Res<Sim>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    // Data-driven ship visuals: one mesh + tinted material per ship that declares
    // a `visual`, built from the registry, plus a generic fallback.
    let mut ship_mat = |c: [f32; 3]| {
        materials.add(StandardMaterial {
            base_color: Color::srgb(c[0], c[1], c[2]),
            // Metallic, fairly glossy hull that catches the star's light and keeps
            // a faint self-glow so ships read against deep space.
            emissive: LinearRgba::rgb(c[0] * 0.08, c[1] * 0.08, c[2] * 0.08),
            metallic: 0.7,
            perceptual_roughness: 0.35,
            reflectance: 0.5,
            ..default()
        })
    };
    let mut by_ship: HashMap<ShipId, (Handle<Mesh>, Handle<StandardMaterial>)> = HashMap::new();
    let player_ship = {
        let reg = sim.0.world().registry();
        for i in 0..reg.ship_count() {
            let id = ShipId(i as u32);
            if let Some(v) = &reg.ship(id).visual {
                let handle = (meshes.add(build_hull(v)), ship_mat(v.color));
                by_ship.insert(id, handle);
            }
        }
        sim.0
            .world()
            .traders()
            .iter()
            .find(|t| t.is_player())
            .map(|t| t.ship)
    };
    let ship_visuals = ShipVisuals {
        by_ship,
        fallback: (
            meshes.add(dart_hull(2.2, 1.4, 1.0, 0.4, 0.3, 0.2)),
            ship_mat([0.72, 0.72, 0.75]),
        ),
        engine_mesh: meshes.add(Sphere::new(0.28)),
        engine_mat: materials.add(StandardMaterial {
            base_color: Color::srgb(1.0, 0.6, 0.25),
            emissive: LinearRgba::rgb(3.5, 1.4, 0.35),
            unlit: true,
            ..default()
        }),
    };

    // Player ship: its own hull from the ship data (a Cobra Mk III), nose along -Z.
    let (player_mesh, player_mat) = player_ship
        .map(|s| ship_visuals.get(s))
        .unwrap_or_else(|| ship_visuals.fallback.clone());
    let engine = materials.add(StandardMaterial {
        base_color: Color::srgb(1.0, 0.6, 0.2),
        emissive: LinearRgba::rgb(4.0, 1.6, 0.3),
        unlit: true,
        ..default()
    });
    commands
        .spawn((
            PlayerShip,
            Flight(Ship::default()),
            Combat(player_health()),
            FireCooldown(0.0),
            Transform::default(),
            Visibility::default(),
        ))
        .with_children(|ship| {
            // Player hull (nose already along -Z, so no reorientation).
            ship.spawn((
                Mesh3d(player_mesh),
                MeshMaterial3d(player_mat),
                Transform::default(),
            ));
            // Engine glow at the tail (+Z is aft).
            ship.spawn((
                Mesh3d(meshes.add(Sphere::new(0.32))),
                MeshMaterial3d(engine),
                Transform::from_xyz(0.0, 0.0, 1.3),
            ));
        });

    // HDR camera with bloom, so emissive things (star, engines, beacons) glow.
    commands.spawn((
        Camera3d::default(),
        Camera { hdr: true, ..default() },
        Bloom::NATURAL,
        Transform::from_xyz(0.0, 4.0, 16.0).looking_at(Vec3::ZERO, Vec3::Y),
        ChaseCamera,
    ));

    // A star ahead (with its light) and a reference planet — colours/scale are
    // re-flavoured per system on arrival (see `apply_flavour`).
    let star_pos = Vec3::new(0.0, 0.0, -320.0);
    let star = commands
        .spawn((
            Mesh3d(meshes.add(Sphere::new(45.0))),
            MeshMaterial3d(materials.add(StandardMaterial {
                base_color: Color::srgb(1.0, 0.85, 0.4),
                emissive: LinearRgba::rgb(3.0, 2.4, 1.0),
                ..default()
            })),
            Transform::from_translation(star_pos),
        ))
        .id();
    commands.spawn((
        DirectionalLight { illuminance: 15_000.0, shadows_enabled: false, ..default() },
        Transform::from_translation(-star_pos).looking_at(Vec3::ZERO, Vec3::Y),
    ));
    let planet = commands
        .spawn((
            Mesh3d(meshes.add(Sphere::new(30.0))),
            MeshMaterial3d(materials.add(StandardMaterial {
                base_color: Color::srgb(0.3, 0.45, 0.7),
                ..default()
            })),
            Transform::from_xyz(160.0, -20.0, -180.0),
            Spin(0.05),
        ))
        .id();
    commands.insert_resource(Scenery { star, planet, last: None });

    // The station you dock at to trade — a slowly-rotating Coriolis cuboctahedron
    // with a dark docking slot on the face toward you (+Z). Fly up to it to dock.
    let station_scale = 7.0;
    commands
        .spawn((
            Mesh3d(meshes.add(coriolis_mesh())),
            MeshMaterial3d(materials.add(StandardMaterial {
                base_color: Color::srgb(0.62, 0.64, 0.7),
                emissive: LinearRgba::rgb(0.06, 0.12, 0.18),
                perceptual_roughness: 0.75,
                ..default()
            })),
            Transform::from_translation(STATION_POS).with_scale(Vec3::splat(station_scale)),
            StationSpin(0.3),
        ))
        .with_children(|station| {
            // Docking slot: a dark recessed rectangle on the +Z face (unit-space,
            // scaled with the parent). Emissive rim so it reads as the entrance.
            station.spawn((
                Mesh3d(meshes.add(Cuboid::new(0.5, 0.16, 0.08))),
                MeshMaterial3d(materials.add(StandardMaterial {
                    base_color: Color::srgb(0.02, 0.02, 0.03),
                    emissive: LinearRgba::rgb(0.3, 0.6, 0.9),
                    ..default()
                })),
                Transform::from_xyz(0.0, 0.0, 1.0),
            ));
        });

    // Full-screen flash for jumps (hidden until in transit).
    commands.spawn((
        Node { position_type: PositionType::Absolute, width: Val::Percent(100.0), height: Val::Percent(100.0), ..default() },
        BackgroundColor(Color::srgba(0.8, 0.9, 1.0, 0.55)),
        Visibility::Hidden,
        TransitVeil,
    ));

    // Starfield backdrop.
    let star_mesh = meshes.add(Sphere::new(1.4));
    let star_mat = materials.add(StandardMaterial {
        base_color: Color::WHITE,
        emissive: LinearRgba::rgb(1.5, 1.5, 1.7),
        unlit: true,
        ..default()
    });
    for (i, p) in starfield(650).into_iter().enumerate() {
        // Vary each star's size a little (deterministic per index) so the field
        // reads as depth rather than uniform dots.
        let scale = 0.5 + hash01(i as u32 ^ 0x1234_5678) * 1.3;
        commands.spawn((
            Mesh3d(star_mesh.clone()),
            MeshMaterial3d(star_mat.clone()),
            Transform::from_translation(p * 900.0).with_scale(Vec3::splat(scale)),
        ));
    }

    // Agent ships render from the data-driven visuals (mesh + tint per ship type).
    commands.insert_resource(ship_visuals);

    // Weapon bolts. One glowing material per player weapon (pulse=cyan,
    // cannon=amber, scatter=green), plus a small and a big bolt mesh.
    let mut bolt_mat = |r: f32, g: f32, b: f32| {
        materials.add(StandardMaterial {
            base_color: Color::srgb(r.min(1.0), g.min(1.0), b.min(1.0)),
            emissive: LinearRgba::rgb(r, g, b),
            unlit: true,
            ..default()
        })
    };
    commands.insert_resource(ProjectileAssets {
        small_mesh: meshes.add(Sphere::new(0.35)),
        big_mesh: meshes.add(Sphere::new(0.7)),
        player: [
            bolt_mat(0.5, 4.0, 4.0),
            bolt_mat(5.0, 2.2, 0.5),
            bolt_mat(0.8, 4.5, 1.2),
        ],
        enemy: bolt_mat(4.0, 0.8, 0.3),
        ally: bolt_mat(0.3, 3.2, 3.6),
    });

    // Missile assets: a small elongated body, bright per faction.
    commands.insert_resource(MissileAssets {
        mesh: meshes.add(Capsule3d::new(0.16, 0.9)),
        player: bolt_mat(3.0, 3.0, 1.0),
        enemy: bolt_mat(3.5, 0.9, 0.6),
    });

    // The beam-laser beam: a thin bright bar, scaled/aimed by `beam_fire`, hidden
    // until it fires. Unit length along +Z so a Z-scale sets its reach.
    commands.spawn((
        BeamVisual,
        Mesh3d(meshes.add(Cuboid::new(0.12, 0.12, 1.0))),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: Color::srgb(0.6, 0.95, 1.0),
            emissive: LinearRgba::rgb(1.5, 5.0, 6.0),
            unlit: true,
            ..default()
        })),
        Transform::default(),
        Visibility::Hidden,
    ));

    // Cargo canisters: a small tumbling crate with a faint glow.
    commands.insert_resource(CargoAssets {
        mesh: meshes.add(Cuboid::new(1.0, 1.0, 1.4)),
        material: materials.add(StandardMaterial {
            base_color: Color::srgb(0.65, 0.5, 0.3),
            emissive: LinearRgba::rgb(0.3, 0.2, 0.05),
            perceptual_roughness: 0.8,
            ..default()
        }),
    });

    // Explosion visuals: a growing central flash plus a burst of fragments.
    commands.insert_resource(ExplosionAssets {
        flash_mesh: meshes.add(Sphere::new(1.0)),
        flash_mat: materials.add(StandardMaterial {
            base_color: Color::srgb(1.0, 0.85, 0.5),
            emissive: LinearRgba::rgb(8.0, 4.5, 1.5),
            unlit: true,
            ..default()
        }),
        frag_mesh: meshes.add(Sphere::new(0.28)),
        frag_mat: materials.add(StandardMaterial {
            base_color: Color::srgb(1.0, 0.6, 0.25),
            emissive: LinearRgba::rgb(5.0, 1.8, 0.5),
            unlit: true,
            ..default()
        }),
    });

    // Gunnery aids: a lead pip (aim here) and a reticle on the locked target,
    // both hidden until there is a lock (see `targeting`).
    commands.spawn((
        LeadPip,
        Mesh3d(meshes.add(Torus { minor_radius: 0.14, major_radius: 1.1 })),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: Color::srgb(1.0, 0.95, 0.4),
            emissive: LinearRgba::rgb(5.5, 5.0, 0.7),
            unlit: true,
            ..default()
        })),
        Transform::default(),
        Visibility::Hidden,
    ));
    commands.spawn((
        TargetReticle,
        Mesh3d(meshes.add(Torus { minor_radius: 0.12, major_radius: 2.4 })),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: Color::srgb(1.0, 0.4, 0.4),
            emissive: LinearRgba::rgb(4.5, 0.8, 0.8),
            unlit: true,
            ..default()
        })),
        Transform::default(),
        Visibility::Hidden,
    ));

    // Contact scanner: a round panel bottom-left with a centre dot (you) and a
    // pool of blips that `update_radar` repositions and recolours each frame.
    let radar_d = RADAR_RADIUS_PX * 2.0;
    commands
        .spawn((
            RadarRoot,
            Node {
                position_type: PositionType::Absolute,
                bottom: Val::Px(16.0),
                left: Val::Px(16.0),
                width: Val::Px(radar_d),
                height: Val::Px(radar_d),
                border: UiRect::all(Val::Px(1.5)),
                ..default()
            },
            BorderColor(Color::srgba(0.4, 0.9, 1.0, 0.5)),
            BorderRadius::MAX,
            BackgroundColor(Color::srgba(0.02, 0.06, 0.12, 0.35)),
        ))
        .with_children(|root| {
            root.spawn((
                Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(RADAR_RADIUS_PX - 2.0),
                    top: Val::Px(RADAR_RADIUS_PX - 2.0),
                    width: Val::Px(4.0),
                    height: Val::Px(4.0),
                    ..default()
                },
                BackgroundColor(Color::srgb(0.7, 0.9, 1.0)),
                BorderRadius::MAX,
            ));
            for _ in 0..RADAR_BLIPS {
                root.spawn((
                    RadarBlip,
                    Node {
                        position_type: PositionType::Absolute,
                        width: Val::Px(RADAR_BLIP_PX),
                        height: Val::Px(RADAR_BLIP_PX),
                        ..default()
                    },
                    BackgroundColor(Color::srgb(1.0, 0.3, 0.3)),
                    BorderRadius::MAX,
                    Visibility::Hidden,
                ));
            }
        });

    commands.spawn((
        Text::new(""),
        TextFont { font_size: 15.0, ..default() },
        TextColor(Color::srgb(0.75, 0.9, 1.0)),
        Node { position_type: PositionType::Absolute, top: Val::Px(14.0), left: Val::Px(16.0), ..default() },
        Hud,
    ));

    // --- Ship instrument gauges (bottom-right cluster, Oolite-style) ---
    let gauges = commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                bottom: Val::Px(16.0),
                right: Val::Px(16.0),
                width: Val::Px(190.0),
                flex_direction: FlexDirection::Column,
                row_gap: Val::Px(4.0),
                padding: UiRect::all(Val::Px(8.0)),
                ..default()
            },
            BackgroundColor(Color::srgba(0.02, 0.06, 0.12, 0.35)),
            BorderRadius::all(Val::Px(4.0)),
        ))
        .id();
    spawn_bar(&mut commands, gauges, "SPD", Color::srgb(0.5, 0.9, 0.6), SpeedFill);
    spawn_bar(&mut commands, gauges, "THR", Color::srgb(0.4, 0.8, 1.0), ThrottleFill);
    spawn_bar(&mut commands, gauges, "HULL", Color::srgb(1.0, 0.7, 0.3), HullFill);
    spawn_bar(&mut commands, gauges, "SHLD", Color::srgb(0.4, 0.7, 1.0), ShieldFill);
    spawn_bar(&mut commands, gauges, "FUEL", Color::srgb(0.9, 0.75, 0.35), FuelFill);
    spawn_bar(&mut commands, gauges, "TEMP", Color::srgb(0.5, 0.85, 1.0), TempFill);
    spawn_indicator(&mut commands, gauges, "ROLL", RollKnob);
    spawn_indicator(&mut commands, gauges, "PTCH", PitchKnob);

    // --- Compass disc (bottom-centre): a blip pointing to the station ---
    let compass = commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                bottom: Val::Px(16.0),
                left: Val::Percent(50.0),
                width: Val::Px(64.0),
                height: Val::Px(64.0),
                margin: UiRect { left: Val::Px(-32.0), ..default() },
                border: UiRect::all(Val::Px(1.5)),
                ..default()
            },
            BorderColor(Color::srgba(0.4, 0.9, 1.0, 0.5)),
            BorderRadius::MAX,
            BackgroundColor(Color::srgba(0.02, 0.06, 0.12, 0.3)),
        ))
        .id();
    // Centre dot (you).
    commands.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: Val::Px(29.0),
            top: Val::Px(29.0),
            width: Val::Px(3.0),
            height: Val::Px(3.0),
            ..default()
        },
        BackgroundColor(Color::srgb(0.6, 0.8, 1.0)),
        BorderRadius::MAX,
        ChildOf(compass),
    ));
    // Station blip (repositioned each frame by `update_indicators`).
    commands.spawn((
        CompassDot,
        Node {
            position_type: PositionType::Absolute,
            left: Val::Px(27.0),
            top: Val::Px(3.0),
            width: Val::Px(6.0),
            height: Val::Px(6.0),
            ..default()
        },
        BackgroundColor(Color::srgb(0.4, 1.0, 0.6)),
        BorderRadius::MAX,
        ChildOf(compass),
    ));

    // --- Locked-target panel (top-right, shown only when a target is locked) ---
    let tpanel = commands
        .spawn((
            TargetPanel,
            Node {
                position_type: PositionType::Absolute,
                top: Val::Px(16.0),
                right: Val::Px(16.0),
                width: Val::Px(210.0),
                flex_direction: FlexDirection::Column,
                row_gap: Val::Px(4.0),
                padding: UiRect::all(Val::Px(8.0)),
                ..default()
            },
            BackgroundColor(Color::srgba(0.12, 0.03, 0.03, 0.4)),
            BorderRadius::all(Val::Px(4.0)),
            Visibility::Hidden,
        ))
        .id();
    commands.spawn((
        TargetText,
        Text::new(""),
        TextFont { font_size: 12.0, ..default() },
        TextColor(Color::srgb(1.0, 0.8, 0.8)),
        ChildOf(tpanel),
    ));
    spawn_bar(&mut commands, tpanel, "HULL", Color::srgb(1.0, 0.5, 0.4), TargetHullFill);
    spawn_bar(&mut commands, tpanel, "SHLD", Color::srgb(0.5, 0.7, 1.0), TargetShieldFill);

    // --- Fixed gunsight at screen centre (aim reference for fixed weapons) ---
    commands.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: Val::Percent(50.0),
            top: Val::Percent(50.0),
            width: Val::Px(10.0),
            height: Val::Px(10.0),
            margin: UiRect { left: Val::Px(-5.0), top: Val::Px(-5.0), ..default() },
            border: UiRect::all(Val::Px(1.5)),
            ..default()
        },
        BorderColor(Color::srgba(0.6, 0.9, 1.0, 0.7)),
        BorderRadius::MAX,
    ));
}

/// Advance the simulation one tick per fixed step (applying any queued commands).
fn advance_sim(mut sim: ResMut<Sim>) {
    sim.0.world_mut().tick();
}

/// Toggle docking with the station: `Space` docks when in range (and stops the
/// ship), or undocks when docked.
fn dock_input(
    keys: Res<ButtonInput<KeyCode>>,
    mut cockpit: ResMut<Cockpit>,
    mut missiles: ResMut<PlayerMissile>,
    mut fuel: ResMut<Fuel>,
    mut ship: Query<&mut Flight, With<PlayerShip>>,
) {
    if !keys.just_pressed(KeyCode::Space) {
        return;
    }
    if cockpit.docked {
        cockpit.docked = false;
        return;
    }
    if let Ok(mut f) = ship.single_mut() {
        if f.0.position.distance(STATION_POS) < DOCK_RANGE {
            cockpit.docked = true;
            f.0.velocity = Vec3::ZERO;
            // Rearm and refuel at the station.
            missiles.ammo = PLAYER_MISSILES;
            fuel.0 = MAX_FUEL;
        }
    }
}

/// While docked, browse the local market and buy/sell through the command
/// pipeline: Up/Down select a commodity, `[`/`]` adjust quantity, `B`/`S` trade.
fn trade_input(
    keys: Res<ButtonInput<KeyCode>>,
    mut cockpit: ResMut<Cockpit>,
    mut sim: ResMut<Sim>,
    player: Res<PlayerState>,
) {
    if !cockpit.docked {
        return;
    }
    let Some(sys) = player.system else {
        return;
    };
    let goods_len = sim.0.world().markets()[sys.index()].goods.len();
    if goods_len == 0 {
        return;
    }
    if keys.just_pressed(KeyCode::ArrowUp) {
        cockpit.cursor = cockpit.cursor.saturating_sub(1);
    }
    if keys.just_pressed(KeyCode::ArrowDown) {
        cockpit.cursor = (cockpit.cursor + 1).min(goods_len - 1);
    }
    if keys.just_pressed(KeyCode::BracketLeft) {
        cockpit.qty = cockpit.qty.saturating_sub(1).max(1);
    }
    if keys.just_pressed(KeyCode::BracketRight) {
        cockpit.qty = (cockpit.qty + 1).min(50);
    }

    let commodity = sim.0.world().markets()[sys.index()]
        .goods
        .keys()
        .nth(cockpit.cursor)
        .copied();
    let Some(commodity) = commodity else {
        return;
    };
    if keys.just_pressed(KeyCode::KeyB) {
        sim.0.queue_command(Command::Buy { player: PLAYER, trader: player.trader, commodity, qty: cockpit.qty });
    }
    if keys.just_pressed(KeyCode::KeyS) {
        sim.0.queue_command(Command::Sell { player: PLAYER, trader: player.trader, commodity, qty: cockpit.qty });
    }
}

/// Number keys 1-9 jump to the Nth connected system, through the command pipeline.
/// Disabled while docked (undock to fly and jump).
fn jump_input(
    keys: Res<ButtonInput<KeyCode>>,
    mut sim: ResMut<Sim>,
    player: Res<PlayerState>,
    cockpit: Res<Cockpit>,
    mut fuel: ResMut<Fuel>,
) {
    if cockpit.docked {
        return;
    }
    let Some(sys) = player.system else {
        return; // cannot jump mid-transit
    };
    const DIGITS: [KeyCode; 9] = [
        KeyCode::Digit1, KeyCode::Digit2, KeyCode::Digit3, KeyCode::Digit4, KeyCode::Digit5,
        KeyCode::Digit6, KeyCode::Digit7, KeyCode::Digit8, KeyCode::Digit9,
    ];
    let reg = sim.0.world().registry();
    let here = reg.system(sys).position;
    let connections: Vec<(SystemId, f32)> = reg
        .system(sys)
        .connections
        .clone()
        .into_iter()
        .map(|dest| {
            let p = reg.system(dest).position;
            let dist = ((p[0] - here[0]).powi(2) + (p[1] - here[1]).powi(2)).sqrt() as f32;
            (dest, dist)
        })
        .collect();
    for (i, &(dest, dist)) in connections.iter().enumerate().take(9) {
        if keys.just_pressed(DIGITS[i]) {
            // Hyperspace costs fuel by distance; refuse a jump the tank can't make.
            if nav::can_jump(fuel.0, dist) {
                fuel.0 = (fuel.0 - nav::jump_fuel_cost(dist)).max(0.0);
                sim.0.queue_command(Command::Jump { player: PLAYER, trader: player.trader, dest });
            }
        }
    }
}

/// Read the player trader's location from the sim into [`PlayerState`]; on arrival
/// at a new system, reset the ship to the origin so it "warps in".
fn sync_system(
    sim: Res<Sim>,
    mut player: ResMut<PlayerState>,
    mut ship: Query<&mut Flight, With<PlayerShip>>,
) {
    let current = sim
        .0
        .world()
        .traders()
        .iter()
        .find(|t| t.id == player.trader)
        .and_then(|t| match t.location {
            TraderLocation::Docked(s) => Some(s),
            _ => None,
        });
    player.system = current;

    if current.is_some() && current != player.rendered {
        player.rendered = current;
        if let Ok(mut f) = ship.single_mut() {
            f.0 = Ship::default();
        }
    }
}

/// Rebuild the current system's agent ships each frame from the sim: every trader,
/// pirate, and navy ship docked here (position is a stable function of its id, so
/// they hold still rather than flicker).
fn refresh_agents(
    mut commands: Commands,
    sim: Res<Sim>,
    player: Res<PlayerState>,
    ships: Res<ShipVisuals>,
    existing: Query<Entity, With<AgentShip>>,
) {
    for e in &existing {
        commands.entity(e).despawn();
    }
    let Some(sys) = player.system else {
        return;
    };
    let world = sim.0.world();

    // Only traders are ambient scenery here. Pirates spawn as live hostiles
    // (`manage_hostiles`) and navy as live allies (`manage_allies`), not here.
    for t in world.traders() {
        if t.id != player.trader && t.location == TraderLocation::Docked(sys) {
            let (mesh, material) = ships.get(t.ship);
            commands.spawn((
                Mesh3d(mesh),
                MeshMaterial3d(material),
                Transform::from_translation(agent_pos(t.id.0)),
                AgentShip,
            ));
        }
    }
}

/// A deterministic visual identity for a system: `(star colour, star scale,
/// planet colour, planet position)`, derived from the system id so each place
/// looks distinct but is identical every run.
fn system_flavour(sys: SystemId) -> (Color, f32, Color, Vec3) {
    let h = sys.0.wrapping_mul(2_654_435_761);
    let hue = (h % 360) as f32;
    let star = Color::hsl(hue, 0.75, 0.62);
    let star_scale = 0.8 + (h / 360 % 5) as f32 * 0.16;
    let planet = Color::hsl((hue + 150.0) % 360.0, 0.5, 0.5);
    let side = if h.is_multiple_of(2) { 1.0 } else { -1.0 };
    let planet_pos = Vec3::new(
        side * 150.0,
        -20.0 + (h / 7 % 4) as f32 * 14.0,
        -170.0 - (h / 11 % 3) as f32 * 40.0,
    );
    (star, star_scale, planet, planet_pos)
}

/// Re-colour and reposition the star and planet when the player arrives in a new
/// system, so each system reads as its own place.
fn apply_flavour(
    player: Res<PlayerState>,
    mut scenery: ResMut<Scenery>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut bodies: Query<(&mut Transform, &MeshMaterial3d<StandardMaterial>)>,
) {
    let Some(sys) = player.system else {
        return;
    };
    if scenery.last == Some(sys) {
        return;
    }
    scenery.last = Some(sys);
    let (star_col, star_scale, planet_col, planet_pos) = system_flavour(sys);

    if let Ok((mut tf, mat)) = bodies.get_mut(scenery.star) {
        tf.scale = Vec3::splat(star_scale);
        if let Some(m) = materials.get_mut(&mat.0) {
            let l = star_col.to_linear();
            m.base_color = star_col;
            m.emissive = LinearRgba::rgb(l.red * 3.0, l.green * 3.0, l.blue * 3.0);
        }
    }
    if let Ok((mut tf, mat)) = bodies.get_mut(scenery.planet) {
        tf.translation = planet_pos;
        if let Some(m) = materials.get_mut(&mat.0) {
            m.base_color = planet_col;
        }
    }
}

/// Slowly rotate bodies tagged [`Spin`] for a sense of life.
fn spin(time: Res<Time>, mut q: Query<(&mut Transform, &Spin)>) {
    let dt = time.delta_secs();
    for (mut tf, s) in &mut q {
        tf.rotate_y(s.0 * dt);
    }
}

/// Roll a Coriolis station about its docking axis (local `+Z`), so its slot
/// sweeps round as it faces you — the signature Oolite station motion.
fn spin_station(time: Res<Time>, mut q: Query<(&mut Transform, &StationSpin)>) {
    let dt = time.delta_secs();
    for (mut tf, s) in &mut q {
        tf.rotate_local_z(s.0 * dt);
    }
}

/// Show the full-screen flash while the trader is jumping between systems.
fn transit_veil(player: Res<PlayerState>, mut veil: Query<&mut Visibility, With<TransitVeil>>) {
    if let Ok(mut v) = veil.single_mut() {
        *v = if player.system.is_none() { Visibility::Visible } else { Visibility::Hidden };
    }
}

/// Fresh player hull/shield. A moderate shield regen so damage accumulates in a
/// sustained fight rather than shrugging off every hit.
fn player_health() -> Health {
    Health::new(120.0, 60.0, 2.5)
}

/// Spawn a weapon bolt with the given mesh and glowing material.
fn spawn_projectile(
    commands: &mut Commands,
    mesh: &Handle<Mesh>,
    material: &Handle<StandardMaterial>,
    pos: Vec3,
    vel: Vec3,
    damage: f32,
    faction: u8,
) {
    commands.spawn((
        Mesh3d(mesh.clone()),
        MeshMaterial3d(material.clone()),
        Transform::from_translation(pos),
        Projectile { vel, damage, faction, ttl: 3.0 },
    ));
}

/// (Re)spawn the current system's pirates as live hostiles on each jump; destroyed
/// hostiles stay dead until you leave and return.
fn manage_hostiles(
    mut commands: Commands,
    sim: Res<Sim>,
    player: Res<PlayerState>,
    mut arena: ResMut<Arena>,
    ships: Res<ShipVisuals>,
    hostiles: Query<Entity, With<Hostile>>,
) {
    let Some(sys) = player.system else {
        // In transit or destroyed: clear the local battle; it rebuilds on arrival.
        for e in &hostiles {
            commands.entity(e).despawn();
        }
        arena.last = None;
        return;
    };
    if arena.last == Some(sys) {
        return;
    }
    arena.last = Some(sys);
    for e in &hostiles {
        commands.entity(e).despawn();
    }
    for p in sim.0.world().pirates() {
        if p.docked_at() == Some(sys) {
            let e = commands
                .spawn((
                    Hostile(p.id),
                    Flight(Ship { position: agent_pos(p.id.0), ..default() }),
                    Combat(Health::new(40.0, 20.0, 1.0)),
                    FireCooldown(1.0 + (p.id.0 % 5) as f32 * 0.2),
                    // Staggered first missile so a wing doesn't volley together.
                    MissileBay(6.0 + (p.id.0 % 7) as f32 * 1.5),
                    Transform::default(),
                    Visibility::default(),
                ))
                .id();
            ships.attach(&mut commands, e, p.ship);
        }
    }
}

/// (Re)spawn the current system's navy patrols as friendly allies on each jump,
/// mirroring [`manage_hostiles`]. They fight on the player's side (see `ally_ai`).
/// Allies are tougher than pirates so a navy presence is worth having at your
/// back. A downed ally stays down until you leave and return.
fn manage_allies(
    mut commands: Commands,
    sim: Res<Sim>,
    player: Res<PlayerState>,
    mut arena: ResMut<Arena>,
    ships: Res<ShipVisuals>,
    allies: Query<Entity, With<Ally>>,
) {
    let Some(sys) = player.system else {
        for e in &allies {
            commands.entity(e).despawn();
        }
        arena.allies_last = None;
        return;
    };
    if arena.allies_last == Some(sys) {
        return;
    }
    arena.allies_last = Some(sys);
    for e in &allies {
        commands.entity(e).despawn();
    }
    for p in sim.0.world().navy() {
        if p.docked_at() == Some(sys) {
            let e = commands
                .spawn((
                    Ally(p.id),
                    Flight(Ship { position: agent_pos(p.id.0), ..default() }),
                    Combat(Health::new(55.0, 30.0, 1.5)),
                    FireCooldown(0.8 + (p.id.0 % 5) as f32 * 0.2),
                    Transform::from_translation(agent_pos(p.id.0)),
                    Visibility::default(),
                ))
                .id();
            ships.attach(&mut commands, e, p.ship);
        }
    }
}

/// Hostile AI: pick the nearest enemy (the player or an ally), turn to its lead
/// point, close to engagement range, orbit, and fire leading shots.
#[allow(clippy::type_complexity)]
fn hostile_ai(
    time: Res<Time>,
    mut commands: Commands,
    assets: Res<ProjectileAssets>,
    player: Query<(&Transform, &Flight), (With<PlayerShip>, Without<Hostile>, Without<Ally>)>,
    allies: Query<(&Transform, &Flight), (With<Ally>, Without<Hostile>, Without<PlayerShip>)>,
    mut hostiles: Query<(&mut Flight, &mut Transform, &mut FireCooldown, &Hostile)>,
) {
    // Enemies of a hostile: the player plus every ally. Each is a possible target.
    let mut enemies: Vec<(Vec3, Vec3)> = Vec::new();
    if let Ok((ptf, pflight)) = player.single() {
        enemies.push((ptf.translation, pflight.0.velocity));
    }
    for (tf, f) in &allies {
        enemies.push((tf.translation, f.0.velocity));
    }
    if enemies.is_empty() {
        return;
    }
    let positions: Vec<Vec3> = enemies.iter().map(|(p, _)| *p).collect();
    let dt = time.delta_secs();
    for (mut flight, mut tf, mut cd, hostile) in &mut hostiles {
        // Engage the nearest enemy.
        let (target, target_vel) = nearest(flight.0.position, &positions)
            .map(|i| enemies[i])
            .expect("enemies is non-empty");
        let to = target - flight.0.position;
        let dist = to.length();
        if dist > 0.5 {
            // Face the player's lead point so the nose tracks where it will shoot.
            let aim = firing_solution(flight.0.position, PROJECTILE_SPEED, target, target_vel)
                .map(|s| s.aim)
                .unwrap_or(to / dist);
            let desired = Quat::from_rotation_arc(Vec3::NEG_Z, aim);
            flight.0.rotation = flight.0.rotation.slerp(desired, (1.6 * dt).min(1.0));

            // Close to a knife-fight band, then orbit rather than sitting still —
            // a strafing target the player has to keep leading. Circling sense
            // alternates by id so a wing does not stack on one arc.
            let velocity = if dist > 35.0 {
                (to / dist) * 72.0
            } else {
                let sense = if hostile.0 .0 % 2 == 0 { 1.0 } else { -1.0 };
                (to / dist).cross(Vec3::Y).normalize_or_zero() * 40.0 * sense
            };
            flight.0.velocity = velocity;
            flight.0.position += velocity * dt;
        }
        tf.translation = flight.0.position;
        tf.rotation = flight.0.rotation;

        cd.0 -= dt;
        if cd.0 <= 0.0 && dist < 150.0 {
            cd.0 = 1.15;
            // Lead the player, so bolts converge on where they are going.
            let dir = firing_solution(flight.0.position, PROJECTILE_SPEED, target, target_vel)
                .map(|s| s.aim)
                .unwrap_or_else(|| (target - flight.0.position).normalize_or_zero());
            spawn_projectile(
                &mut commands,
                &assets.small_mesh,
                &assets.enemy,
                flight.0.position + dir * 2.0,
                dir * PROJECTILE_SPEED,
                8.0,
                1,
            );
        }
    }
}

/// Ally AI: navy ships hunt the nearest hostile, lead their shots, and orbit at
/// range — the mirror of [`hostile_ai`], firing friendly (faction 0) bolts. With
/// no hostiles present they simply hold station.
#[allow(clippy::type_complexity)]
#[allow(clippy::type_complexity)]
fn ally_ai(
    time: Res<Time>,
    mut commands: Commands,
    assets: Res<ProjectileAssets>,
    rap: Res<Rap>,
    player: Query<(&Transform, &Flight), (With<PlayerShip>, Without<Ally>, Without<Hostile>)>,
    hostiles: Query<(&Transform, &Flight), (With<Hostile>, Without<Ally>, Without<PlayerShip>)>,
    mut allies: Query<
        (&mut Flight, &mut Transform, &mut FireCooldown, &Ally),
        (Without<Hostile>, Without<PlayerShip>),
    >,
) {
    // The navy turn on a wanted pilot: a fugitive/offender is the target, and their
    // shots become hostile (faction 1) so they land on the player.
    let wanted = rap.status().is_wanted();
    let (targets, bolt_faction): (Vec<(Vec3, Vec3)>, u8) = if wanted {
        (
            player.iter().map(|(tf, f)| (tf.translation, f.0.velocity)).collect(),
            1,
        )
    } else {
        (
            hostiles.iter().map(|(tf, f)| (tf.translation, f.0.velocity)).collect(),
            2,
        )
    };
    let dt = time.delta_secs();
    for (mut flight, mut tf, mut cd, ally) in &mut allies {
        cd.0 -= dt;
        if targets.is_empty() {
            // No enemies: hold position (transform already tracks the flight state).
            tf.translation = flight.0.position;
            tf.rotation = flight.0.rotation;
            continue;
        }
        let positions: Vec<Vec3> = targets.iter().map(|(p, _)| *p).collect();
        let (target, target_vel) = nearest(flight.0.position, &positions)
            .map(|i| targets[i])
            .expect("targets is non-empty");
        let to = target - flight.0.position;
        let dist = to.length();
        if dist > 0.5 {
            let aim = firing_solution(flight.0.position, PROJECTILE_SPEED, target, target_vel)
                .map(|s| s.aim)
                .unwrap_or(to / dist);
            let desired = Quat::from_rotation_arc(Vec3::NEG_Z, aim);
            flight.0.rotation = flight.0.rotation.slerp(desired, (1.8 * dt).min(1.0));
            let velocity = if dist > 40.0 {
                (to / dist) * 78.0
            } else {
                let sense = if ally.0 .0 % 2 == 0 { 1.0 } else { -1.0 };
                (to / dist).cross(Vec3::Y).normalize_or_zero() * 42.0 * sense
            };
            flight.0.velocity = velocity;
            flight.0.position += velocity * dt;
        }
        tf.translation = flight.0.position;
        tf.rotation = flight.0.rotation;

        if cd.0 <= 0.0 && dist < 160.0 {
            cd.0 = 1.0;
            let dir = firing_solution(flight.0.position, PROJECTILE_SPEED, target, target_vel)
                .map(|s| s.aim)
                .unwrap_or_else(|| (target - flight.0.position).normalize_or_zero());
            spawn_projectile(
                &mut commands,
                &assets.small_mesh,
                &assets.ally,
                flight.0.position + dir * 2.0,
                dir * PROJECTILE_SPEED,
                9.0,
                bolt_faction,
            );
        }
    }
}

/// Cycle the selected player weapon with `Tab` (unless docked).
fn switch_weapon(
    keys: Res<ButtonInput<KeyCode>>,
    cockpit: Res<Cockpit>,
    mut weapon: ResMut<PlayerWeapon>,
) {
    if !cockpit.docked && keys.just_pressed(KeyCode::Tab) {
        weapon.0 = (weapon.0 + 1) % WEAPONS.len();
    }
}

/// Fire the player's selected weapon while `F` is held (unless docked). Multi-bolt
/// weapons fan their bolts across a small spread about the ship's up axis.
#[allow(clippy::too_many_arguments)]
fn player_fire(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    cockpit: Res<Cockpit>,
    weapon: Res<PlayerWeapon>,
    lock: Res<LockInfo>,
    mut heat: ResMut<LaserHeat>,
    mut commands: Commands,
    assets: Res<ProjectileAssets>,
    mut q: Query<(&Flight, &mut FireCooldown), With<PlayerShip>>,
) {
    let Ok((flight, mut cd)) = q.single_mut() else {
        return;
    };
    cd.0 -= time.delta_secs();
    let w = &WEAPONS[weapon.0];
    // Beam weapons are handled by `beam_fire`; a bolt weapon fires only if it is
    // not overheated (Oolite laser temperature).
    if w.beam || cockpit.docked || !keys.pressed(KeyCode::KeyA) || cd.0 > 0.0 || !heat.0.can_fire() {
        return;
    }
    cd.0 = w.cooldown;
    heat.0.add(w.heat);
    let mesh = if w.big { &assets.big_mesh } else { &assets.small_mesh };
    let material = &assets.player[weapon.0];
    let up = flight.0.up();
    let nose = flight.0.forward();
    // A gimballed weapon tracks the locked target's lead within its cone; a fixed
    // weapon (or one whose target is outside the arc) fires straight down the nose.
    let base = match (w.gimbal, lock.solution_aim) {
        (Some(cone), Some(aim)) => gimbal_aim(nose, aim, cone).unwrap_or(nose),
        _ => nose,
    };
    for i in 0..w.bolts {
        let offset = (i as f32 - (w.bolts as f32 - 1.0) / 2.0) * w.spread;
        let dir = Quat::from_axis_angle(up, offset) * base;
        let vel = flight.0.velocity + dir * w.speed;
        spawn_projectile(&mut commands, mesh, material, flight.0.position + dir * 2.0, vel, w.damage, 0);
    }
}

/// Dissipate the player laser's heat each frame (and clear an overheat cut-out
/// once it has cooled enough to come back online).
fn cool_laser(time: Res<Time>, mut heat: ResMut<LaserHeat>) {
    heat.0.cool(LASER_COOL_RATE, time.delta_secs());
}

/// The beam laser: a continuous hitscan while `A` is held. It melts the nearest
/// hostile within a narrow forward cone (damage-per-second), builds heat fast, and
/// renders a bright bar out to the hit point (or its full reach when it hits air).
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn beam_fire(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    cockpit: Res<Cockpit>,
    weapon: Res<PlayerWeapon>,
    mut heat: ResMut<LaserHeat>,
    player: Query<&Flight, With<PlayerShip>>,
    mut hostiles: Query<(Entity, &Transform, &mut Combat), With<Hostile>>,
    mut beam: Query<(&mut Transform, &mut Visibility), (With<BeamVisual>, Without<Hostile>)>,
) {
    let w = &WEAPONS[weapon.0];
    let dt = time.delta_secs();
    let firing = w.beam && !cockpit.docked && keys.pressed(KeyCode::KeyA) && heat.0.can_fire();
    if !firing {
        for (_, mut v) in &mut beam {
            *v = Visibility::Hidden;
        }
        return;
    }
    let Ok(flight) = player.single() else {
        for (_, mut v) in &mut beam {
            *v = Visibility::Hidden;
        }
        return;
    };
    heat.0.add(w.heat * dt);
    let origin = flight.0.position;
    let dir = flight.0.forward();

    // Nearest hostile inside the forward cone and within reach.
    let mut best: Option<(Entity, f32)> = None;
    for (e, tf, _) in hostiles.iter() {
        let to = tf.translation - origin;
        let dist = to.length();
        if dist < 1e-3 || dist > BEAM_RANGE || to.normalize().dot(dir) < BEAM_COS {
            continue;
        }
        match best {
            Some((_, d)) if dist >= d => {}
            _ => best = Some((e, dist)),
        }
    }
    let reach = match best {
        Some((e, d)) => {
            if let Ok((_, _, mut hp)) = hostiles.get_mut(e) {
                hp.0.take_damage(w.damage * dt);
            }
            d
        }
        None => BEAM_RANGE,
    };

    // Render: a unit-Z bar centred at half-reach, scaled to `reach`, aimed along
    // the nose.
    for (mut tf, mut v) in &mut beam {
        tf.translation = origin + dir * (reach * 0.5);
        tf.rotation = Quat::from_rotation_arc(Vec3::Z, dir);
        tf.scale = Vec3::new(1.0, 1.0, reach);
        *v = Visibility::Visible;
    }
}

/// Spawn a homing missile at `pos` flying along `dir` toward `target`.
fn spawn_missile(
    commands: &mut Commands,
    assets: &MissileAssets,
    pos: Vec3,
    dir: Vec3,
    target: Entity,
    faction: u8,
) {
    let material = if faction == 0 { assets.player.clone() } else { assets.enemy.clone() };
    let dir = dir.normalize_or_zero();
    commands.spawn((
        Mesh3d(assets.mesh.clone()),
        MeshMaterial3d(material),
        // The capsule's long axis is +Y; point it along the flight direction.
        Transform::from_translation(pos)
            .with_rotation(Quat::from_rotation_arc(Vec3::Y, dir)),
        Missile { target, vel: dir * MISSILE_SPEED, faction, ttl: MISSILE_TTL },
    ));
}

/// Fire a homing missile at the locked target with `M` (limited stores, refilled
/// on docking). Requires a lock — the missile needs something to chase.
#[allow(clippy::too_many_arguments)]
fn fire_missile(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    cockpit: Res<Cockpit>,
    locked: Res<LockedTarget>,
    assets: Res<MissileAssets>,
    mut store: ResMut<PlayerMissile>,
    mut commands: Commands,
    player: Query<&Flight, With<PlayerShip>>,
) {
    store.cooldown = (store.cooldown - time.delta_secs()).max(0.0);
    if cockpit.docked || !keys.just_pressed(KeyCode::KeyM) {
        return;
    }
    let Some(target) = locked.0 else { return };
    if store.ammo == 0 || store.cooldown > 0.0 {
        return;
    }
    let Ok(flight) = player.single() else { return };
    store.ammo -= 1;
    store.cooldown = MISSILE_COOLDOWN;
    let dir = flight.0.forward();
    spawn_missile(&mut commands, &assets, flight.0.position + dir * 2.5, dir, target, 0);
}

/// Hostiles occasionally loose a homing missile at the player, so ECM earns its
/// keep. Each launcher fires on its own stagger; out of range it just retries.
#[allow(clippy::type_complexity)]
fn enemy_missiles(
    time: Res<Time>,
    assets: Res<MissileAssets>,
    mut commands: Commands,
    player: Query<(Entity, &Transform), (With<PlayerShip>, Without<Hostile>)>,
    mut bays: Query<(&Transform, &mut MissileBay), With<Hostile>>,
) {
    let Ok((pe, ptf)) = player.single() else { return };
    let dt = time.delta_secs();
    for (tf, mut bay) in &mut bays {
        bay.0 -= dt;
        if bay.0 > 0.0 {
            continue;
        }
        let to = ptf.translation - tf.translation;
        if to.length() < 220.0 {
            bay.0 = 11.0; // reload
            let dir = to.normalize_or_zero();
            spawn_missile(&mut commands, &assets, tf.translation + dir * 2.0, dir, pe, 1);
        } else {
            bay.0 = 2.0; // out of range: check again soon
        }
    }
}

/// Steer every missile toward its target and detonate on proximity (a big warhead
/// hit). A missile whose target is gone coasts until its timer runs out.
#[allow(clippy::type_complexity)]
fn missile_homing(
    time: Res<Time>,
    cockpit: Res<Cockpit>,
    pstate: Res<PlayerState>,
    mut commands: Commands,
    exp: Res<ExplosionAssets>,
    mut missiles: Query<(Entity, &mut Transform, &mut Missile)>,
    mut targets: Query<(&Transform, &Flight, &mut Combat), Without<Missile>>,
) {
    // Docked or between systems: clear missiles in flight (the local fight ends).
    if cockpit.docked || pstate.system.is_none() {
        for (me, _, _) in &missiles {
            commands.entity(me).despawn();
        }
        return;
    }
    let dt = time.delta_secs();
    for (me, mut mtf, mut missile) in &mut missiles {
        missile.ttl -= dt;
        if missile.ttl <= 0.0 {
            commands.entity(me).despawn();
            continue;
        }
        if let Ok((ttf, tflight, mut thp)) = targets.get_mut(missile.target) {
            let vel = home_missile(
                mtf.translation,
                missile.vel,
                ttf.translation,
                tflight.0.velocity,
                MISSILE_SPEED,
                MISSILE_TURN,
                dt,
            );
            missile.vel = vel;
            mtf.translation += vel * dt;
            if vel != Vec3::ZERO {
                mtf.rotation = Quat::from_rotation_arc(Vec3::Y, vel.normalize());
            }
            if mtf.translation.distance(ttf.translation) < MISSILE_HIT_RADIUS {
                thp.0.take_damage(MISSILE_DAMAGE);
                spawn_explosion(&mut commands, &exp, mtf.translation);
                commands.entity(me).despawn();
            }
        } else {
            let vel = missile.vel;
            mtf.translation += vel * dt;
        }
    }
}

/// Fire ECM with `E` (on a cooldown): detonate every incoming (hostile) missile at
/// once — the Oolite counter to a missile lock.
#[allow(clippy::too_many_arguments)]
fn ecm(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    cockpit: Res<Cockpit>,
    exp: Res<ExplosionAssets>,
    mut state: ResMut<Ecm>,
    mut commands: Commands,
    missiles: Query<(Entity, &Transform, &Missile)>,
) {
    let dt = time.delta_secs();
    state.cooldown = (state.cooldown - dt).max(0.0);
    state.flash = (state.flash - dt).max(0.0);
    if cockpit.docked || !keys.just_pressed(KeyCode::KeyE) || state.cooldown > 0.0 {
        return;
    }
    state.cooldown = ECM_COOLDOWN;
    state.flash = 0.4;
    for (e, tf, m) in &missiles {
        if m.faction == 1 {
            spawn_explosion(&mut commands, &exp, tf.translation);
            commands.entity(e).despawn();
        }
    }
}

/// Drift and tumble cargo canisters, expire them, and clear them on jump/dock.
fn move_cargo(
    time: Res<Time>,
    cockpit: Res<Cockpit>,
    pstate: Res<PlayerState>,
    mut commands: Commands,
    mut q: Query<(Entity, &mut Transform, &mut Cargo)>,
) {
    if cockpit.docked || pstate.system.is_none() {
        for (e, _, _) in &q {
            commands.entity(e).despawn();
        }
        return;
    }
    let dt = time.delta_secs();
    for (e, mut tf, mut cargo) in &mut q {
        cargo.ttl -= dt;
        if cargo.ttl <= 0.0 {
            commands.entity(e).despawn();
            continue;
        }
        let v = cargo.vel;
        tf.translation += v * dt;
        cargo.vel *= (1.0 - 0.5 * dt).max(0.0);
        tf.rotate_local_y(1.4 * dt);
        tf.rotate_local_x(0.8 * dt);
    }
}

/// Scoop cargo canisters the player flies over into the hold, through the command
/// pipeline. The sim caps the amount by hold space; a full hold leaves the
/// canister drifting.
fn scoop_cargo(
    mut sim: ResMut<Sim>,
    pstate: Res<PlayerState>,
    cockpit: Res<Cockpit>,
    mut commands: Commands,
    player: Query<&Transform, With<PlayerShip>>,
    canisters: Query<(Entity, &Transform, &Cargo)>,
) {
    if cockpit.docked || pstate.system.is_none() {
        return;
    }
    let Ok(ptf) = player.single() else { return };
    // Is there room in the hold? (A read of sim state, before any mutation.)
    let has_room = {
        let world = sim.0.world();
        let Some(t) = world.traders().iter().find(|t| t.is_player()) else {
            return;
        };
        let reg = world.registry();
        let used: u32 = t.cargo.iter().map(|(c, q)| q * reg.commodity(*c).unit_mass).sum();
        used < reg.ship(t.ship).cargo_capacity
    };
    if !has_room {
        return;
    }
    for (e, tf, cargo) in &canisters {
        if ptf.translation.distance(tf.translation) < SCOOP_RADIUS {
            sim.0.queue_command(Command::ScoopCargo {
                player: PLAYER,
                trader: pstate.trader,
                commodity: cargo.commodity,
                qty: cargo.qty,
            });
            commands.entity(e).despawn();
        }
    }
}

/// Move bolts and expire them.
fn move_projectiles(
    time: Res<Time>,
    mut commands: Commands,
    mut q: Query<(Entity, &mut Transform, &mut Projectile)>,
) {
    let dt = time.delta_secs();
    for (e, mut tf, mut p) in &mut q {
        tf.translation += p.vel * dt;
        p.ttl -= dt;
        if p.ttl <= 0.0 {
            commands.entity(e).despawn();
        }
    }
}

/// Resolve bolt hits: a player bolt damages hostiles, a hostile bolt damages the
/// player. A single mutable query over all combatants (distinguished by `Hostile`)
/// avoids aliasing.
fn projectile_hits(
    mut commands: Commands,
    mut rap: ResMut<Rap>,
    projectiles: Query<(Entity, &Transform, &Projectile)>,
    mut targets: Query<(&Transform, &mut Combat, Has<Hostile>, Has<Ally>)>,
) {
    /// Bounty added each time a player shot strikes the navy — attacking the
    /// police is a crime that quickly makes a pilot wanted.
    const CRIME_PER_HIT: u32 = 6;

    for (pe, pt, proj) in &projectiles {
        for (tt, mut hp, is_hostile, is_ally) in &mut targets {
            // faction 0 = player: hits hostiles, or the navy (a crime).
            // faction 1 = hostile-aligned: hits everything not hostile.
            // faction 2 = navy: hits hostiles only.
            let matches = match proj.faction {
                0 => is_hostile || is_ally,
                1 => !is_hostile,
                2 => is_hostile,
                _ => false,
            };
            if matches && pt.translation.distance(tt.translation) < HIT_RADIUS {
                hp.0.take_damage(proj.damage);
                if proj.faction == 0 && is_ally {
                    rap.bounty += CRIME_PER_HIT;
                }
                commands.entity(pe).despawn();
                break;
            }
        }
    }
}

/// Regenerate shields each frame.
fn regen_shields(time: Res<Time>, mut q: Query<&mut Combat>) {
    let dt = time.delta_secs();
    for mut c in &mut q {
        c.0.regen(dt);
    }
}

/// Remove destroyed hostiles (with an explosion) and report each kill to the sim
/// (removes the pirate, pays the bounty). If the player is destroyed, blow it up,
/// report it (the sim destroys the trader and pays insurance), and heal for the
/// sim-driven respawn.
#[allow(clippy::type_complexity)] // an irreducible Bevy query filter (disjoint from hostiles)
#[allow(clippy::too_many_arguments)]
fn cull_dead(
    mut commands: Commands,
    mut sim: ResMut<Sim>,
    pstate: Res<PlayerState>,
    exp: Res<ExplosionAssets>,
    cargo_assets: Res<CargoAssets>,
    hostiles: Query<(Entity, &Combat, &Hostile, &Transform)>,
    allies: Query<(Entity, &Combat, &Transform), (With<Ally>, Without<Hostile>, Without<PlayerShip>)>,
    mut player: Query<(&mut Combat, &Transform), (With<PlayerShip>, Without<Hostile>)>,
) {
    let commodities = sim.0.world().registry().commodity_count().max(1) as u64;
    for (e, c, h, tf) in &hostiles {
        if !c.0.alive() {
            spawn_explosion(&mut commands, &exp, tf.translation);
            commands.entity(e).despawn();
            sim.0.queue_command(Command::DestroyedPirate {
                player: PLAYER,
                trader: pstate.trader,
                pirate: h.0,
            });
            // Spill loot: a canister or two, deterministic from the pirate id so a
            // fixed seed replays identically.
            let id = h.0 .0;
            let drops = 1 + (id % 2);
            for k in 0..drops {
                let seed = id.wrapping_add(k * 7);
                let commodity = CommodityId((seed % commodities) as u32);
                let qty = 1 + (seed % 3) as u32;
                let angle = (seed % 8) as f32 / 8.0 * TAU;
                let offset = Vec3::new(angle.cos() * 2.0, ((seed % 3) as f32 - 1.0) * 1.5, angle.sin() * 2.0);
                commands.spawn((
                    Cargo { commodity, qty, vel: offset.normalize_or_zero() * 6.0, ttl: CARGO_TTL },
                    Mesh3d(cargo_assets.mesh.clone()),
                    MeshMaterial3d(cargo_assets.material.clone()),
                    Transform::from_translation(tf.translation + offset),
                ));
            }
        }
    }
    // A downed ally is a local casualty only: it despawns and flashes, but the
    // report never reaches the sim — the determinism firewall keeps the flight
    // layer authoritative solely for the *player's* outcomes, and the sim manages
    // its own navy attrition. The ally reappears if the player leaves and returns.
    for (e, c, tf) in &allies {
        if !c.0.alive() {
            spawn_explosion(&mut commands, &exp, tf.translation);
            commands.entity(e).despawn();
        }
    }
    if let Ok((mut c, tf)) = player.single_mut() {
        if !c.0.alive() {
            spawn_explosion(&mut commands, &exp, tf.translation);
            sim.0.queue_command(Command::TraderDestroyed { player: PLAYER, trader: pstate.trader });
            // Heal for the respawn; position resets when the sim respawns the trader.
            c.0 = player_health();
        }
    }
}

/// Spawn an explosion at `pos`: a growing central flash and a burst of fragments
/// flying out in a fixed spherical spread (deterministic, no RNG).
fn spawn_explosion(commands: &mut Commands, assets: &ExplosionAssets, pos: Vec3) {
    commands.spawn((
        Mesh3d(assets.flash_mesh.clone()),
        MeshMaterial3d(assets.flash_mat.clone()),
        Transform::from_translation(pos).with_scale(Vec3::splat(0.4)),
        Explosion { age: 0.0, max: 0.45 },
    ));
    for dir in fibonacci_sphere(12) {
        commands.spawn((
            Mesh3d(assets.frag_mesh.clone()),
            MeshMaterial3d(assets.frag_mat.clone()),
            Transform::from_translation(pos),
            Fragment { vel: dir * 42.0, ttl: 0.7 },
        ));
    }
}

/// Grow each explosion flash and despawn it at the end of its life.
fn animate_flashes(
    time: Res<Time>,
    mut commands: Commands,
    mut q: Query<(Entity, &mut Transform, &mut Explosion)>,
) {
    let dt = time.delta_secs();
    for (e, mut tf, mut ex) in &mut q {
        ex.age += dt;
        let t = (ex.age / ex.max).min(1.0);
        tf.scale = Vec3::splat(0.4 + t * 6.0);
        if ex.age >= ex.max {
            commands.entity(e).despawn();
        }
    }
}

/// Fly explosion fragments outward and despawn them at the end of their life.
fn animate_fragments(
    time: Res<Time>,
    mut commands: Commands,
    mut q: Query<(Entity, &mut Transform, &mut Fragment)>,
) {
    let dt = time.delta_secs();
    for (e, mut tf, mut fr) in &mut q {
        tf.translation += fr.vel * dt;
        fr.ttl -= dt;
        if fr.ttl <= 0.0 {
            commands.entity(e).despawn();
        }
    }
}

/// A stable scatter position for an agent, by id, in the local system space.
fn agent_pos(id: u64) -> Vec3 {
    let angle = (id % 24) as f32 / 24.0 * TAU;
    let radius = 28.0 + (id % 4) as f32 * 16.0;
    let height = ((id % 7) as f32 - 3.0) * 6.0;
    Vec3::new(angle.cos() * radius, height, -30.0 - angle.sin() * radius)
}

fn read_controls(keys: &ButtonInput<KeyCode>) -> Controls {
    let axis = |neg: KeyCode, pos: KeyCode| (keys.pressed(pos) as i32 - keys.pressed(neg) as i32) as f32;
    // Oolite's scheme: roll on the left/right arrows, pitch on up/down (flight-sim
    // sense, so up-arrow noses down), yaw on the comma/period keys. Thrust comes
    // from the held throttle (see `fly`), not the keys.
    Controls {
        thrust: 0.0,
        pitch: axis(KeyCode::ArrowUp, KeyCode::ArrowDown),
        yaw: axis(KeyCode::Period, KeyCode::Comma),
        roll: axis(KeyCode::ArrowRight, KeyCode::ArrowLeft),
    }
}

/// Slew the held throttle: `W` raises it, `S` lowers it, `X` cuts it to a dead
/// stop. Held between frames so the ship keeps its speed hands-off.
///
/// Zero is a detent: slewing into `0` parks there and holds while the key is still
/// down (so you can settle on a dead stop without overshooting into reverse), and
/// crossing into the opposite sign needs a fresh press — releasing the throttle
/// keys re-arms the crossing.
fn throttle_input(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    cockpit: Res<Cockpit>,
    pstate: Res<PlayerState>,
    mut throttle: ResMut<Throttle>,
) {
    if cockpit.docked || pstate.system.is_none() {
        throttle.0.stop();
        return;
    }
    if keys.just_pressed(KeyCode::KeyX) {
        throttle.0.stop();
    }
    let up = keys.pressed(KeyCode::KeyW) as i32 - keys.pressed(KeyCode::KeyS) as i32;
    throttle.0.step(up, THROTTLE_RATE, time.delta_secs());
}

#[allow(clippy::too_many_arguments)]
fn fly(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    cockpit: Res<Cockpit>,
    pstate: Res<PlayerState>,
    throttle: Res<Throttle>,
    mut input: ResMut<FlightInput>,
    mut fuel: ResMut<Fuel>,
    hostiles: Query<(), With<Hostile>>,
    mut q: Query<(&mut Flight, &mut Transform), With<PlayerShip>>,
) {
    // Docked, or between systems (jumping / destroyed): the ship is parked.
    let frozen = cockpit.docked || pstate.system.is_none();
    let mut controls = if frozen { Controls::default() } else { read_controls(&keys) };
    // Drive thrust from the held throttle so the ship holds a set speed.
    controls.thrust = if frozen { 0.0 } else { throttle.0.level };
    // Publish the attitude input for the HUD roll/pitch indicators.
    input.roll = controls.roll;
    input.pitch = controls.pitch;
    let dt = time.delta_secs();

    // Torus drive / fuel injectors: a fast cruise while `J` is held, burning fuel.
    // Mass-locked (disabled) whenever a hostile is present — no fleeing a fight.
    let injectors =
        !frozen && keys.pressed(KeyCode::KeyJ) && fuel.0 > 0.0 && hostiles.is_empty();

    for (mut flight, mut tf) in &mut q {
        flight.0.step(&controls, dt);
        if injectors {
            let fwd = flight.0.forward();
            flight.0.position += fwd * INJECTOR_SPEED * dt;
            fuel.0 = (fuel.0 - nav::INJECTOR_BURN * dt).max(0.0);
        }
        tf.translation = flight.0.position;
        tf.rotation = flight.0.rotation;
    }
}

fn follow_camera(
    player: Query<&Transform, (With<PlayerShip>, Without<ChaseCamera>)>,
    mut camera: Query<&mut Transform, With<ChaseCamera>>,
) {
    let (Ok(p), Ok(mut c)) = (player.single(), camera.single_mut()) else {
        return;
    };
    let target = p.translation + p.rotation * Vec3::new(0.0, 3.5, 15.0);
    c.translation = c.translation.lerp(target, 0.2);
    let look = p.translation + p.rotation * Vec3::new(0.0, 0.0, -10.0);
    c.look_at(look, p.rotation * Vec3::Y);
}

/// Target lock and gunnery aids. `T` cycles the locked hostile (nearest first),
/// `R` clears it. Each frame this validates the lock, paints the reticle on the
/// locked ship and the lead pip at the selected weapon's intercept point, and
/// fills [`LockInfo`] for the HUD. The lead pip is the whole game of it: fixed
/// weapons fire straight ahead, so you fly the nose onto the pip, not onto the
/// enemy.
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn targeting(
    keys: Res<ButtonInput<KeyCode>>,
    weapon: Res<PlayerWeapon>,
    cockpit: Res<Cockpit>,
    pstate: Res<PlayerState>,
    mut locked: ResMut<LockedTarget>,
    mut info: ResMut<LockInfo>,
    player_q: Query<
        (&Transform, &Flight),
        (With<PlayerShip>, Without<Hostile>, Without<LeadPip>, Without<TargetReticle>),
    >,
    hostiles: Query<
        (Entity, &Transform, &Flight, &Combat),
        (With<Hostile>, Without<PlayerShip>, Without<LeadPip>, Without<TargetReticle>),
    >,
    mut pip: Query<
        (&mut Transform, &mut Visibility),
        (With<LeadPip>, Without<PlayerShip>, Without<Hostile>, Without<TargetReticle>),
    >,
    mut reticle: Query<
        (&mut Transform, &mut Visibility),
        (With<TargetReticle>, Without<PlayerShip>, Without<Hostile>, Without<LeadPip>),
    >,
) {
    *info = LockInfo::default();
    let mut hide_all = || {
        for (_, mut v) in &mut pip {
            *v = Visibility::Hidden;
        }
        for (_, mut v) in &mut reticle {
            *v = Visibility::Hidden;
        }
    };

    let Ok((ptf, pflight)) = player_q.single() else {
        hide_all();
        return;
    };
    if cockpit.docked || pstate.system.is_none() {
        locked.0 = None;
        hide_all();
        return;
    }

    // Contacts, nearest first, so cycling walks outward from the closest threat.
    let mut contacts: Vec<(Entity, Vec3, Vec3, f32, f32)> = hostiles
        .iter()
        .map(|(e, tf, f, c)| (e, tf.translation, f.0.velocity, c.0.hull_frac(), c.0.shield_frac()))
        .collect();
    let ppos = ptf.translation;
    contacts.sort_by(|a, b| {
        ppos.distance_squared(a.1).total_cmp(&ppos.distance_squared(b.1))
    });
    let entities: Vec<Entity> = contacts.iter().map(|c| c.0).collect();
    let positions: Vec<Vec3> = contacts.iter().map(|c| c.1).collect();

    // Lock input.
    if keys.just_pressed(KeyCode::KeyR) {
        locked.0 = None;
    }
    if keys.just_pressed(KeyCode::KeyT) {
        let cur = locked.0.and_then(|e| entities.iter().position(|&x| x == e));
        locked.0 = match cur {
            Some(_) => cycle(cur, entities.len()).map(|i| entities[i]),
            None => nearest(ppos, &positions).map(|i| entities[i]),
        };
    }
    // Drop a stale lock (target destroyed or out of this system).
    if let Some(e) = locked.0 {
        if !entities.contains(&e) {
            locked.0 = None;
        }
    }

    let Some(target) = locked.0.and_then(|e| contacts.iter().find(|c| c.0 == e)) else {
        hide_all();
        return;
    };
    let (_, tpos, tvel, thull_frac, tshield_frac) = *target;

    // Reticle on the target itself.
    for (mut tf, mut v) in &mut reticle {
        tf.translation = tpos;
        *v = Visibility::Visible;
    }

    // Lead pip: where to aim so this weapon's bolts (which inherit the player's
    // velocity) intercept the target. Aim direction is compared to the nose for an
    // on-target cue.
    let speed = WEAPONS[weapon.0].speed;
    let solution = firing_solution(ppos, speed, tpos, tvel - pflight.0.velocity);
    for (mut tf, mut v) in &mut pip {
        match &solution {
            Some(sol) => {
                tf.translation = sol.point;
                *v = Visibility::Visible;
            }
            None => *v = Visibility::Hidden,
        }
    }

    let los = tpos - ppos;
    let range = los.length();
    let closing = if range > 1e-3 {
        (pflight.0.velocity - tvel).dot(los / range)
    } else {
        0.0
    };
    info.locked = true;
    info.range = range;
    info.hull_frac = thull_frac;
    info.shield_frac = tshield_frac;
    info.closing = closing;
    info.firing = solution.is_some();
    info.on_target = solution
        .map(|s| pflight.0.forward().dot(s.aim) > 0.9995)
        .unwrap_or(false);
    info.solution_aim = solution.map(|s| s.aim);
    info.gimbal_locked = match (WEAPONS[weapon.0].gimbal, solution) {
        (Some(cone), Some(s)) => gimbal_aim(pflight.0.forward(), s.aim, cone).is_some(),
        _ => false,
    };
}

#[allow(clippy::too_many_arguments)]
fn update_hud(
    sim: Res<Sim>,
    player: Res<PlayerState>,
    cockpit: Res<Cockpit>,
    weapon: Res<PlayerWeapon>,
    missiles: Res<PlayerMissile>,
    ecm: Res<Ecm>,
    rap: Res<Rap>,
    ship: Query<(&Flight, &Combat), With<PlayerShip>>,
    hostiles: Query<(), With<Hostile>>,
    allies: Query<(), With<Ally>>,
    mut hud: Query<&mut Text, With<Hud>>,
) {
    let Ok(mut text) = hud.single_mut() else {
        return;
    };
    let world = sim.0.world();
    let Some(sys) = player.system else {
        *text = Text::new("IN TRANSIT...");
        return;
    };
    let reg = world.registry();
    let sd = reg.system(sys);
    let trader = world.traders().iter().find(|t| t.id == player.trader);
    let capital = trader.map(|t| t.capital).unwrap_or(0);
    let (cargo_used, cargo_cap) = trader
        .map(|t| {
            let used: u32 = t.cargo.iter().map(|(c, q)| q * reg.commodity(*c).unit_mass).sum();
            (used, reg.ship(t.ship).cargo_capacity)
        })
        .unwrap_or((0, 0));

    let body = if cockpit.docked {
        let market = &world.markets()[sys.index()];
        let mut s = format!("DOCKED \u{2014} {}\nCapital {} cr     trade qty {}\n\n", sd.name, capital, cockpit.qty);
        for (i, (c, good)) in market.goods.iter().enumerate() {
            let held = trader.and_then(|t| t.cargo.get(c)).copied().unwrap_or(0);
            let cursor = if i == cockpit.cursor { '>' } else { ' ' };
            s += &format!(
                "{} {:<10} price {:>5}   stock {:>5}   held {:>3}\n",
                cursor,
                reg.commodity_name(*c),
                good.price,
                good.stock,
                held
            );
        }
        s += "\nUp/Down select   [ / ] qty   B buy   S sell   Space undock";
        s
    } else {
        let near = ship
            .single()
            .map(|(f, _)| f.0.position.distance(STATION_POS) < DOCK_RANGE)
            .unwrap_or(false);
        let enemies = hostiles.iter().count();
        let friendlies = allies.iter().count();
        let wkind = if WEAPONS[weapon.0].beam {
            "beam"
        } else if WEAPONS[weapon.0].gimbal.is_some() {
            "gimbal"
        } else {
            "fixed"
        };
        // Speed/throttle/hull/shield and the locked-target readout are on the
        // graphical gauges now (see `update_gauges`); this panel is nav + status.
        let ecm_state = if ecm.cooldown > 0.0 { "charging" } else { "ready" };
        let status = rap.status().label();
        let bounty = if rap.bounty > 0 { format!("   bounty {}", rap.bounty) } else { String::new() };
        let mut s = format!(
            "SYSTEM  {}   danger {:.2}\nCapital {} cr     CARGO {}/{}     HOSTILES {}   NAVY {}\nSTATUS  {}{}\nWEAPON  {} ({})  [Tab switch]\nMISSILES {}  [M] fire     ECM {}  [E]\n",
            sd.name, sd.danger, capital, cargo_used, cargo_cap, enemies, friendlies, status, bounty,
            WEAPONS[weapon.0].name, wkind, missiles.ammo, ecm_state
        );
        s += "\nJump to:\n";
        for (i, &dest) in sd.connections.iter().enumerate().take(9) {
            s += &format!("  [{}] {}\n", i + 1, reg.system(dest).name);
        }
        s += if near {
            "\n[Space] DOCK   W/S throttle  J torus  arrows roll/pitch  , . yaw  T lock  A fire  M msl  E ecm"
        } else {
            "\nW/S throttle  J torus  arrows roll/pitch  , . yaw  T lock  A fire  M msl  E ecm   (1-9 jump)"
        };
        s
    };
    *text = Text::new(body);
}

/// Reposition the scanner blips each frame: project every contact into the
/// player's local frame ([`radar_contact`]) and place it on the disc. `+Y` on the
/// disc is dead ahead (top of the scanner); UI `top` grows downward, so the disc
/// `y` is inverted. Contacts behind the ship are dimmed.
#[allow(clippy::type_complexity)]
fn update_radar(
    pstate: Res<PlayerState>,
    cockpit: Res<Cockpit>,
    player_q: Query<&Transform, With<PlayerShip>>,
    hostiles: Query<&Transform, (With<Hostile>, Without<PlayerShip>)>,
    allies: Query<&Transform, (With<Ally>, Without<PlayerShip>, Without<Hostile>)>,
    mut blips: Query<(&mut Node, &mut Visibility, &mut BackgroundColor), With<RadarBlip>>,
) {
    let Ok(ptf) = player_q.single() else {
        for (_, mut v, _) in &mut blips {
            *v = Visibility::Hidden;
        }
        return;
    };

    // Contacts: hostiles (red), allies (teal), and the station (green), while flying.
    let mut contacts: Vec<(Vec3, Color)> = Vec::new();
    if !(cockpit.docked || pstate.system.is_none()) {
        for tf in &hostiles {
            contacts.push((tf.translation, Color::srgb(1.0, 0.3, 0.3)));
        }
        for tf in &allies {
            contacts.push((tf.translation, Color::srgb(0.3, 0.9, 1.0)));
        }
        contacts.push((STATION_POS, Color::srgb(0.4, 1.0, 0.6)));
    }

    let mut it = contacts.into_iter();
    for (mut node, mut vis, mut bg) in &mut blips {
        match it.next() {
            Some((pos, color)) => {
                let rc = radar_contact(ptf.translation, ptf.rotation, pos);
                let d = rc.disc(RADAR_RANGE);
                node.left = Val::Px(RADAR_RADIUS_PX + d.x * RADAR_RADIUS_PX - RADAR_BLIP_PX * 0.5);
                node.top = Val::Px(RADAR_RADIUS_PX - d.y * RADAR_RADIUS_PX - RADAR_BLIP_PX * 0.5);
                *bg = BackgroundColor(if rc.ahead { color } else { color.with_alpha(0.45) });
                *vis = Visibility::Visible;
            }
            None => *vis = Visibility::Hidden,
        }
    }
}

/// Drive the instrument gauges from live state: fill widths for speed, throttle,
/// hull and shield (with warning colours), plus the locked-target panel and its
/// own hull/shield bars. A `ParamSet` holds the several `&mut Node` fill queries so
/// they never alias.
#[allow(clippy::type_complexity)]
#[allow(clippy::too_many_arguments)]
fn update_gauges(
    throttle: Res<Throttle>,
    lock: Res<LockInfo>,
    weapon: Res<PlayerWeapon>,
    fuel: Res<Fuel>,
    heat: Res<LaserHeat>,
    player: Query<(&Flight, &Combat), With<PlayerShip>>,
    mut fills: ParamSet<(
        Query<&mut Node, With<SpeedFill>>,
        Query<(&mut Node, &mut BackgroundColor), With<ThrottleFill>>,
        Query<(&mut Node, &mut BackgroundColor), With<HullFill>>,
        Query<&mut Node, With<ShieldFill>>,
        Query<&mut Node, With<TargetHullFill>>,
        Query<&mut Node, With<TargetShieldFill>>,
        Query<&mut Node, With<FuelFill>>,
        Query<(&mut Node, &mut BackgroundColor), With<TempFill>>,
    )>,
    mut panel: Query<&mut Visibility, With<TargetPanel>>,
    mut ttext: Query<&mut Text, With<TargetText>>,
) {
    fn set_w(node: &mut Node, frac: f32) {
        node.width = Val::Percent(frac.clamp(0.0, 1.0) * 100.0);
    }

    let (speed_frac, hull_frac, shield_frac) = player
        .single()
        .map(|(f, c)| (f.0.speed() / 140.0, c.0.hull_frac(), c.0.shield_frac()))
        .unwrap_or((0.0, 0.0, 0.0));

    if let Ok(mut n) = fills.p0().single_mut() {
        set_w(&mut n, speed_frac);
    }
    if let Ok((mut n, mut bg)) = fills.p1().single_mut() {
        set_w(&mut n, throttle.0.level.abs());
        // Reverse throttle reads amber, forward cyan.
        *bg = BackgroundColor(if throttle.0.level >= 0.0 {
            Color::srgb(0.4, 0.8, 1.0)
        } else {
            Color::srgb(1.0, 0.6, 0.2)
        });
    }
    if let Ok((mut n, mut bg)) = fills.p2().single_mut() {
        set_w(&mut n, hull_frac);
        *bg = BackgroundColor(if hull_frac < 0.3 {
            Color::srgb(1.0, 0.3, 0.2)
        } else {
            Color::srgb(1.0, 0.7, 0.3)
        });
    }
    if let Ok(mut n) = fills.p3().single_mut() {
        set_w(&mut n, shield_frac);
    }
    if let Ok(mut n) = fills.p6().single_mut() {
        set_w(&mut n, fuel.0 / MAX_FUEL);
    }
    if let Ok((mut n, mut bg)) = fills.p7().single_mut() {
        set_w(&mut n, heat.0.frac());
        // Cool = blue, hot = amber, cut out = red.
        *bg = BackgroundColor(if heat.0.is_cut_out() {
            Color::srgb(1.0, 0.25, 0.2)
        } else if heat.0.frac() > 0.7 {
            Color::srgb(1.0, 0.6, 0.2)
        } else {
            Color::srgb(0.5, 0.85, 1.0)
        });
    }

    // Locked-target panel: toggle, fill its bars, and write its readout + cue.
    if let Ok(mut v) = panel.single_mut() {
        *v = if lock.locked { Visibility::Visible } else { Visibility::Hidden };
    }
    if lock.locked {
        if let Ok(mut n) = fills.p4().single_mut() {
            set_w(&mut n, lock.hull_frac);
        }
        if let Ok(mut n) = fills.p5().single_mut() {
            set_w(&mut n, lock.shield_frac);
        }
        if let Ok(mut t) = ttext.single_mut() {
            let closing = if lock.closing >= 0.0 { "closing" } else { "opening" };
            let cue = if !lock.firing {
                "NO SOLUTION"
            } else if WEAPONS[weapon.0].gimbal.is_some() {
                if lock.gimbal_locked { "GIMBAL LOCKED \u{2014} FIRE" } else { "out of gimbal arc" }
            } else if lock.on_target {
                "ON TARGET \u{2014} FIRE"
            } else {
                "fly nose onto the pip"
            };
            *t = Text::new(format!(
                "TARGET\nrange {:>5.0}   {} {:>3.0}\n{}",
                lock.range,
                closing,
                lock.closing.abs(),
                cue
            ));
        }
    }
}

/// Drive the roll and pitch indicator knobs from the live attitude input, and the
/// compass blip from the station's direction in the ship's local frame (a blip
/// pushed to the rim and dimmed when the station is behind you).
#[allow(clippy::type_complexity)]
fn update_indicators(
    input: Res<FlightInput>,
    player: Query<&Transform, With<PlayerShip>>,
    mut set: ParamSet<(
        Query<&mut Node, With<RollKnob>>,
        Query<&mut Node, With<PitchKnob>>,
        Query<(&mut Node, &mut BackgroundColor), With<CompassDot>>,
    )>,
) {
    if let Ok(mut n) = set.p0().single_mut() {
        n.left = Val::Percent(50.0 + input.roll.clamp(-1.0, 1.0) * 45.0);
    }
    if let Ok(mut n) = set.p1().single_mut() {
        n.left = Val::Percent(50.0 + input.pitch.clamp(-1.0, 1.0) * 45.0);
    }

    if let Ok(ptf) = player.single() {
        // Station direction in the ship's local frame (-Z forward, +X right, +Y up).
        let d = (ptf.rotation.inverse() * (STATION_POS - ptf.translation)).normalize_or_zero();
        let ahead = d.z < 0.0;
        let planar = Vec2::new(d.x, d.y);
        let plen = planar.length();
        // Ahead: offset by how far off-axis it is; behind: pin to the rim.
        let off = if plen < 1e-4 {
            Vec2::ZERO
        } else {
            (planar / plen) * if ahead { plen } else { 1.0 }
        };
        const CR: f32 = 30.0; // compass inner radius (px)
        const DOT: f32 = 6.0;
        if let Ok((mut n, mut bg)) = set.p2().single_mut() {
            n.left = Val::Px(CR + off.x * CR - DOT / 2.0);
            n.top = Val::Px(CR - off.y * CR - DOT / 2.0); // UI top grows downward
            *bg = BackgroundColor(if ahead {
                Color::srgb(0.4, 1.0, 0.6)
            } else {
                Color::srgba(0.4, 1.0, 0.6, 0.4)
            });
        }
    }
}

/// Build a flat-shaded mesh from convex faces given as vertex-index loops. Each
/// face gets a single outward normal (from its centroid), and its winding is fixed
/// to face outward — so faceted, low-poly shapes (the Oolite look) render cleanly
/// without authoring per-vertex normals or worrying about winding order.
fn faceted_mesh(verts: &[Vec3], faces: &[&[usize]]) -> Mesh {
    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut normals: Vec<[f32; 3]> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    for face in faces {
        let centroid = face.iter().map(|&i| verts[i]).sum::<Vec3>() / face.len() as f32;
        let outward = centroid.normalize_or_zero();
        let base = positions.len() as u32;
        for &i in *face {
            positions.push(verts[i].to_array());
            normals.push(outward.to_array());
        }
        // Fan-triangulate, flipping any triangle that would wind inward.
        for k in 1..face.len() as u32 - 1 {
            let p0 = verts[face[0]];
            let p1 = verts[face[k as usize]];
            let p2 = verts[face[(k + 1) as usize]];
            if (p1 - p0).cross(p2 - p0).dot(outward) >= 0.0 {
                indices.extend([base, base + k, base + k + 1]);
            } else {
                indices.extend([base, base + k + 1, base + k]);
            }
        }
    }
    Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default())
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
        .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
        .with_inserted_indices(Indices::U32(indices))
}

/// The Coriolis station: a unit **cuboctahedron** (six square + eight triangular
/// faces), Oolite's iconic "hexagonal" starport. Scaled up and given a docking
/// slot separately; slowly rotates about the docking axis.
fn coriolis_mesh() -> Mesh {
    let v = [
        Vec3::new(1., 1., 0.),
        Vec3::new(1., -1., 0.),
        Vec3::new(-1., 1., 0.),
        Vec3::new(-1., -1., 0.),
        Vec3::new(1., 0., 1.),
        Vec3::new(1., 0., -1.),
        Vec3::new(-1., 0., 1.),
        Vec3::new(-1., 0., -1.),
        Vec3::new(0., 1., 1.),
        Vec3::new(0., 1., -1.),
        Vec3::new(0., -1., 1.),
        Vec3::new(0., -1., -1.),
    ];
    let faces: &[&[usize]] = &[
        // Six square faces (aligned with the cube faces).
        &[0, 4, 1, 5],
        &[2, 6, 3, 7],
        &[0, 8, 2, 9],
        &[1, 10, 3, 11],
        &[4, 8, 6, 10],
        &[5, 9, 7, 11],
        // Eight triangular faces (aligned with the cube corners).
        &[0, 8, 4],
        &[0, 9, 5],
        &[1, 4, 10],
        &[1, 11, 5],
        &[2, 6, 8],
        &[2, 9, 7],
        &[3, 10, 6],
        &[3, 7, 11],
    ];
    faceted_mesh(&v, faces)
}

// --- The Oolite ship roster (procedural faceted hulls) -----------------------
//
// Recognisable low-poly stand-ins for Oolite's main ships, built from the
// `faceted_mesh` primitives rather than the game's `.dat` models (which are not
// redistributable and this build loads no external assets). Proportions follow
// each ship's silhouette so roles read at a glance: sleek darts for fighters,
// wide flat deltas for light craft, bulky hex hulls for traders. They are
// coloured by faction at spawn.

/// A six-vertex faceted **dart** hull: pointed nose at `-Z`, tail at `+Z`, with
/// the given half-width and dorsal/ventral heights. The convex form keeps
/// `faceted_mesh`'s centroid normals correct; proportions distinguish the ships.
fn dart_hull(nose: f32, tail: f32, half_w: f32, top: f32, bot: f32, mid: f32) -> Mesh {
    let v = [
        Vec3::new(0., 0., -nose),
        Vec3::new(0., 0., tail),
        Vec3::new(half_w, 0., mid),
        Vec3::new(-half_w, 0., mid),
        Vec3::new(0., top, mid),
        Vec3::new(0., -bot, mid),
    ];
    let faces: &[&[usize]] = &[
        &[0, 2, 4], &[0, 4, 3], &[0, 3, 5], &[0, 5, 2],
        &[1, 4, 2], &[1, 3, 4], &[1, 5, 3], &[1, 2, 5],
    ];
    faceted_mesh(&v, faces)
}

/// A bulky freighter hull: a hexagonal (elliptical) cross-section tapering to a
/// nose and tail — the trader silhouette (Python, Boa, Anaconda-alikes).
fn freighter_hull(nose: f32, tail: f32, radius: f32, height: f32) -> Mesh {
    const SIDES: usize = 6;
    let mut v = vec![Vec3::new(0., 0., -nose), Vec3::new(0., 0., tail)];
    for i in 0..SIDES {
        let a = i as f32 / SIDES as f32 * TAU;
        v.push(Vec3::new(a.cos() * radius, a.sin() * height, 0.2));
    }
    let mut faces: Vec<[usize; 3]> = Vec::new();
    for i in 0..SIDES {
        let a = 2 + i;
        let b = 2 + (i + 1) % SIDES;
        faces.push([0, a, b]); // nose fan
        faces.push([1, b, a]); // tail fan
    }
    let refs: Vec<&[usize]> = faces.iter().map(|f| f.as_slice()).collect();
    faceted_mesh(&v, &refs)
}

/// Build a hull mesh from a data-driven [`ShipVisual`]: the silhouette family
/// selects the primitive, and the dimensions set its proportions. Adding a ship to
/// a mod (with a `visual` block) gives it a hull with no client code change.
fn build_hull(v: &ShipVisual) -> Mesh {
    match v.hull {
        // Split the length into a longer nose and shorter tail for a forward rake.
        HullShape::Dart => dart_hull(
            v.length * 0.6,
            v.length * 0.4,
            v.width,
            v.height,
            v.height * 0.72,
            v.length * 0.1,
        ),
        HullShape::Freighter => freighter_hull(v.length * 0.5, v.length * 0.5, v.width, v.height),
    }
}

/// The data-driven ship visuals: a mesh + tinted material per [`ShipId`], built
/// once from the registry's `ShipDef.visual` blocks, plus a generic fallback for
/// any ship that declares none. Every agent renders as its *own* ship type, so the
/// variety is authored in content, not hard-coded here.
#[derive(Resource)]
struct ShipVisuals {
    by_ship: HashMap<ShipId, (Handle<Mesh>, Handle<StandardMaterial>)>,
    fallback: (Handle<Mesh>, Handle<StandardMaterial>),
    /// Shared engine-glow mesh + material, added at every ship's tail for life.
    engine_mesh: Handle<Mesh>,
    engine_mat: Handle<StandardMaterial>,
}

impl ShipVisuals {
    fn get(&self, ship: ShipId) -> (Handle<Mesh>, Handle<StandardMaterial>) {
        self.by_ship.get(&ship).cloned().unwrap_or_else(|| self.fallback.clone())
    }

    /// Attach a ship's hull mesh and a tail engine-glow as children of `parent`
    /// (which must carry the gameplay components, `Transform`, and `Visibility`).
    fn attach(&self, commands: &mut Commands, parent: Entity, ship: ShipId) {
        let (mesh, material) = self.get(ship);
        commands.spawn((Mesh3d(mesh), MeshMaterial3d(material), Transform::default(), ChildOf(parent)));
        commands.spawn((
            Mesh3d(self.engine_mesh.clone()),
            MeshMaterial3d(self.engine_mat.clone()),
            Transform::from_xyz(0.0, 0.0, 1.4),
            ChildOf(parent),
        ));
    }
}

/// Spawn one labelled HUD bar — a `label` plus a track holding a coloured fill —
/// as a child of `panel`. The fill carries `marker`; [`update_gauges`] drives its
/// width each frame. Kept generic so every gauge is one call.
fn spawn_bar<M: Component>(commands: &mut Commands, panel: Entity, label: &str, color: Color, marker: M) {
    let row = commands
        .spawn((
            Node {
                width: Val::Percent(100.0),
                height: Val::Px(11.0),
                align_items: AlignItems::Center,
                column_gap: Val::Px(6.0),
                ..default()
            },
            ChildOf(panel),
        ))
        .id();
    commands.spawn((
        Text::new(label),
        TextFont { font_size: 10.0, ..default() },
        TextColor(Color::srgb(0.6, 0.75, 0.85)),
        Node { width: Val::Px(34.0), ..default() },
        ChildOf(row),
    ));
    let track = commands
        .spawn((
            Node { flex_grow: 1.0, height: Val::Px(7.0), ..default() },
            BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.08)),
            ChildOf(row),
        ))
        .id();
    commands.spawn((
        Node { width: Val::Percent(50.0), height: Val::Percent(100.0), ..default() },
        BackgroundColor(color),
        marker,
        ChildOf(track),
    ));
}

/// Spawn a centre-zero indicator bar (a track with a centre tick and a sliding
/// knob carrying `marker`) as a child of `panel` — used for the roll and pitch
/// indicators, whose knob [`update_indicators`] slides with the attitude input.
fn spawn_indicator<M: Component>(commands: &mut Commands, panel: Entity, label: &str, marker: M) {
    let row = commands
        .spawn((
            Node {
                width: Val::Percent(100.0),
                height: Val::Px(11.0),
                align_items: AlignItems::Center,
                column_gap: Val::Px(6.0),
                ..default()
            },
            ChildOf(panel),
        ))
        .id();
    commands.spawn((
        Text::new(label),
        TextFont { font_size: 10.0, ..default() },
        TextColor(Color::srgb(0.6, 0.75, 0.85)),
        Node { width: Val::Px(34.0), ..default() },
        ChildOf(row),
    ));
    let track = commands
        .spawn((
            Node { flex_grow: 1.0, height: Val::Px(7.0), ..default() },
            BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.08)),
            ChildOf(row),
        ))
        .id();
    // Centre reference tick.
    commands.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: Val::Percent(50.0),
            width: Val::Px(1.0),
            height: Val::Percent(100.0),
            ..default()
        },
        BackgroundColor(Color::srgba(0.6, 0.8, 1.0, 0.4)),
        ChildOf(track),
    ));
    // Sliding knob (centred at rest; `margin.left = -2` centres its 4px width).
    commands.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: Val::Percent(50.0),
            width: Val::Px(4.0),
            height: Val::Percent(100.0),
            margin: UiRect { left: Val::Px(-2.0), ..default() },
            ..default()
        },
        BackgroundColor(Color::srgb(0.7, 0.95, 1.0)),
        marker,
        ChildOf(track),
    ));
}

/// Deterministic near-uniform points on the unit sphere (golden-angle spiral) for
/// the starfield — no RNG, so the sky is identical every run.
fn fibonacci_sphere(n: usize) -> Vec<Vec3> {
    let golden = PI * (3.0 - 5.0_f32.sqrt());
    (0..n)
        .map(|i| {
            let y = 1.0 - (i as f32 / (n as f32 - 1.0)) * 2.0;
            let r = (1.0 - y * y).max(0.0).sqrt();
            let t = golden * i as f32;
            Vec3::new(t.cos() * r, y, t.sin() * r)
        })
        .collect()
}

/// A cheap integer hash to a float in `[0, 1)`. Not an RNG — it holds no state, so
/// the same input always yields the same output — which lets the starfield look
/// randomly scattered while staying byte-identical every run.
fn hash01(mut x: u32) -> f32 {
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846c_a68b);
    x ^= x >> 16;
    x as f32 / u32::MAX as f32
}

/// A naturally-scattered starfield: `n` points placed by **uniform random**
/// sampling of the sphere (from a hash of the index), so the sky looks randomly
/// strewn rather than the ordered spiral a Fibonacci lattice produces — while
/// remaining deterministic (no RNG). `z = 1 - 2u`, `phi = 2*pi*v` is the standard
/// area-uniform sphere sampling.
fn starfield(n: usize) -> Vec<Vec3> {
    (0..n as u32)
        .map(|i| {
            let u = hash01(i.wrapping_mul(2).wrapping_add(0x9e37_79b9));
            let v = hash01(i.wrapping_mul(2).wrapping_add(0x85eb_ca6b));
            let z = 1.0 - 2.0 * u;
            let r = (1.0 - z * z).max(0.0).sqrt();
            let phi = TAU * v;
            Vec3::new(r * phi.cos(), z, r * phi.sin())
        })
        .collect()
}
