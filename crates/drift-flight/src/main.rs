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
//! Controls: **W/S** thrust · **arrows** pitch/yaw · **Q/E** roll · **1-9** jump.
//! Run from the repo root: `cargo run -p drift-flight --features gui`.

use std::f32::consts::{FRAC_PI_2, PI, TAU};
use std::path::PathBuf;

use bevy::core_pipeline::bloom::Bloom;
use bevy::prelude::*;
use drift_core::SystemId;
use drift_economy::{Command, PatrolId, PlayerId, TraderLocation};
use drift_flight::combat::Health;
use drift_flight::flight::{Controls, Ship};
use drift_sim::Session;

/// Projectile speed (world units / s) and how close a bolt must pass to hit.
const PROJECTILE_SPEED: f32 = 220.0;
const HIT_RADIUS: f32 = 3.0;

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

/// Reusable meshes/materials for the sim-driven agent ships.
#[derive(Resource)]
struct AgentAssets {
    mesh: Handle<Mesh>,
    trader: Handle<StandardMaterial>,
    pirate: Handle<StandardMaterial>,
    navy: Handle<StandardMaterial>,
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
/// A body that slowly spins for a sense of life (planet, station).
#[derive(Component)]
struct Spin(f32);
/// Hull/shield of a combatant (player or hostile).
#[derive(Component)]
struct Combat(Health);
/// A hostile pirate flying real-time combat against the player, tagged with the
/// sim patrol it stands in for (so a kill can be reported back).
#[derive(Component)]
struct Hostile(PatrolId);
/// A weapon's cooldown timer (seconds until it can fire again).
#[derive(Component)]
struct FireCooldown(f32);
/// A weapon bolt in flight.
#[derive(Component)]
struct Projectile {
    vel: Vec3,
    damage: f32,
    /// 0 = fired by the player, 1 = fired by a hostile.
    faction: u8,
    ttl: f32,
}

/// Which system's hostiles are currently spawned (rebuilt on jump).
#[derive(Resource)]
struct Arena {
    last: Option<SystemId>,
}

/// Shared assets for weapon bolts: a small and a big bolt mesh, one material per
/// player weapon, and the hostile bolt material.
#[derive(Resource)]
struct ProjectileAssets {
    small_mesh: Handle<Mesh>,
    big_mesh: Handle<Mesh>,
    player: [Handle<StandardMaterial>; 3],
    enemy: Handle<StandardMaterial>,
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
}

/// The player's selectable weapons: a fast weak pulse, a slow heavy cannon, and a
/// three-bolt scatter.
const WEAPONS: [Weapon; 3] = [
    Weapon { name: "PULSE",   damage: 8.0,  cooldown: 0.18, speed: 240.0, bolts: 1, spread: 0.0,  big: false },
    Weapon { name: "CANNON",  damage: 26.0, cooldown: 0.70, speed: 180.0, bolts: 1, spread: 0.0,  big: true },
    Weapon { name: "SCATTER", damage: 6.0,  cooldown: 0.50, speed: 205.0, bolts: 3, spread: 0.13, big: false },
];

/// The player's selected weapon (index into [`WEAPONS`]).
#[derive(Resource)]
struct PlayerWeapon(usize);

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
        .insert_resource(Arena { last: None })
        .insert_resource(PlayerWeapon(0))
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
                    refresh_agents,
                )
                    .chain(),
                (
                    switch_weapon,
                    player_fire,
                    hostile_ai,
                    move_projectiles,
                    projectile_hits,
                    regen_shields,
                    cull_dead,
                )
                    .chain(),
                (
                    animate_flashes,
                    animate_fragments,
                    fly,
                    follow_camera,
                    apply_flavour,
                    spin,
                    transit_veil,
                    update_hud,
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
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    // Player ship: a cone whose apex points along the nose (-Z).
    let hull = materials.add(StandardMaterial {
        base_color: Color::srgb(0.7, 0.8, 0.95),
        emissive: LinearRgba::rgb(0.05, 0.1, 0.25),
        ..default()
    });
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
            ship.spawn((
                Mesh3d(meshes.add(Cone { radius: 0.6, height: 2.0 })),
                MeshMaterial3d(hull),
                Transform::from_rotation(Quat::from_rotation_x(-FRAC_PI_2)),
            ));
            // Engine glow at the tail (+Z is aft).
            ship.spawn((
                Mesh3d(meshes.add(Sphere::new(0.32))),
                MeshMaterial3d(engine),
                Transform::from_xyz(0.0, 0.0, 1.1),
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

    // The station you dock at to trade — a slowly-rotating ring you fly up to.
    commands.spawn((
        Mesh3d(meshes.add(Torus { minor_radius: 1.6, major_radius: 6.0 })),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: Color::srgb(0.7, 0.7, 0.75),
            emissive: LinearRgba::rgb(0.25, 0.5, 0.7),
            ..default()
        })),
        Transform::from_translation(STATION_POS),
        Spin(0.4),
    ));

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
    for p in fibonacci_sphere(260) {
        commands.spawn((
            Mesh3d(star_mesh.clone()),
            MeshMaterial3d(star_mat.clone()),
            Transform::from_translation(p * 900.0),
        ));
    }

    // Agent ship assets (shared across the per-frame rebuild).
    let mut mat = |base: Color, e: LinearRgba| {
        materials.add(StandardMaterial { base_color: base, emissive: e, ..default() })
    };
    commands.insert_resource(AgentAssets {
        mesh: meshes.add(Cuboid::new(0.9, 0.6, 2.0)),
        trader: mat(Color::srgb(0.4, 0.7, 1.0), LinearRgba::rgb(0.1, 0.25, 0.6)),
        pirate: mat(Color::srgb(0.9, 0.3, 0.3), LinearRgba::rgb(0.6, 0.1, 0.1)),
        navy: mat(Color::srgb(0.4, 0.85, 0.85), LinearRgba::rgb(0.1, 0.4, 0.4)),
    });

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

    commands.spawn((
        Text::new(""),
        TextFont { font_size: 15.0, ..default() },
        TextColor(Color::srgb(0.75, 0.9, 1.0)),
        Node { position_type: PositionType::Absolute, top: Val::Px(14.0), left: Val::Px(16.0), ..default() },
        Hud,
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
) {
    if cockpit.docked {
        return;
    }
    let Some(sys) = player.system else {
        return; // cannot jump mid-transit
    };
    let connections: Vec<SystemId> = sim.0.world().registry().system(sys).connections.clone();
    const DIGITS: [KeyCode; 9] = [
        KeyCode::Digit1, KeyCode::Digit2, KeyCode::Digit3, KeyCode::Digit4, KeyCode::Digit5,
        KeyCode::Digit6, KeyCode::Digit7, KeyCode::Digit8, KeyCode::Digit9,
    ];
    for (i, &dest) in connections.iter().enumerate().take(9) {
        if keys.just_pressed(DIGITS[i]) {
            sim.0.queue_command(Command::Jump { player: PLAYER, trader: player.trader, dest });
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
    assets: Res<AgentAssets>,
    existing: Query<Entity, With<AgentShip>>,
) {
    for e in &existing {
        commands.entity(e).despawn();
    }
    let Some(sys) = player.system else {
        return;
    };
    let world = sim.0.world();

    let mut spawn = |id: u64, material: &Handle<StandardMaterial>| {
        commands.spawn((
            Mesh3d(assets.mesh.clone()),
            MeshMaterial3d(material.clone()),
            Transform::from_translation(agent_pos(id)),
            AgentShip,
        ));
    };
    // Traders and navy are ambient scenery; pirates are spawned as live hostiles by
    // `manage_hostiles`, not here.
    for t in world.traders() {
        if t.id != player.trader && t.location == TraderLocation::Docked(sys) {
            spawn(t.id.0, &assets.trader);
        }
    }
    for n in world.navy() {
        if n.docked_at() == Some(sys) {
            spawn(n.id.0, &assets.navy);
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
    assets: Res<AgentAssets>,
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
            commands.spawn((
                Hostile(p.id),
                Flight(Ship { position: agent_pos(p.id.0), ..default() }),
                Combat(Health::new(40.0, 20.0, 1.0)),
                FireCooldown(1.0 + (p.id.0 % 5) as f32 * 0.2),
                Transform::default(),
                Visibility::default(),
                Mesh3d(assets.mesh.clone()),
                MeshMaterial3d(assets.pirate.clone()),
            ));
        }
    }
}

/// Hostile AI: turn toward the player, close to engagement range, and fire.
fn hostile_ai(
    time: Res<Time>,
    mut commands: Commands,
    assets: Res<ProjectileAssets>,
    player: Query<&Transform, (With<PlayerShip>, Without<Hostile>)>,
    mut hostiles: Query<(&mut Flight, &mut Transform, &mut FireCooldown), With<Hostile>>,
) {
    let Ok(target) = player.single().map(|t| t.translation) else {
        return;
    };
    let dt = time.delta_secs();
    for (mut flight, mut tf, mut cd) in &mut hostiles {
        let to = target - flight.0.position;
        let dist = to.length();
        if dist > 0.5 {
            let desired = Quat::from_rotation_arc(Vec3::NEG_Z, to / dist);
            let rotation = flight.0.rotation.slerp(desired, (1.6 * dt).min(1.0));
            flight.0.rotation = rotation;
            // Close to a knife-fight range and hold there — fast enough to pursue
            // a fleeing player rather than being left behind.
            let speed = if dist > 35.0 { 72.0 } else { 0.0 };
            let velocity = flight.0.forward() * speed;
            flight.0.velocity = velocity;
            flight.0.position += velocity * dt;
        }
        tf.translation = flight.0.position;
        tf.rotation = flight.0.rotation;

        cd.0 -= dt;
        if cd.0 <= 0.0 && dist < 150.0 {
            cd.0 = 1.15;
            let dir = (target - flight.0.position).normalize_or_zero();
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
fn player_fire(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    cockpit: Res<Cockpit>,
    weapon: Res<PlayerWeapon>,
    mut commands: Commands,
    assets: Res<ProjectileAssets>,
    mut q: Query<(&Flight, &mut FireCooldown), With<PlayerShip>>,
) {
    let Ok((flight, mut cd)) = q.single_mut() else {
        return;
    };
    cd.0 -= time.delta_secs();
    if cockpit.docked || !keys.pressed(KeyCode::KeyF) || cd.0 > 0.0 {
        return;
    }
    let w = &WEAPONS[weapon.0];
    cd.0 = w.cooldown;
    let mesh = if w.big { &assets.big_mesh } else { &assets.small_mesh };
    let material = &assets.player[weapon.0];
    let up = flight.0.up();
    let base = flight.0.forward();
    for i in 0..w.bolts {
        let offset = (i as f32 - (w.bolts as f32 - 1.0) / 2.0) * w.spread;
        let dir = Quat::from_axis_angle(up, offset) * base;
        let vel = flight.0.velocity + dir * w.speed;
        spawn_projectile(&mut commands, mesh, material, flight.0.position + dir * 2.0, vel, w.damage, 0);
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
    projectiles: Query<(Entity, &Transform, &Projectile)>,
    mut targets: Query<(&Transform, &mut Combat, Has<Hostile>)>,
) {
    for (pe, pt, proj) in &projectiles {
        for (tt, mut hp, is_hostile) in &mut targets {
            let matches = (proj.faction == 0 && is_hostile) || (proj.faction == 1 && !is_hostile);
            if matches && pt.translation.distance(tt.translation) < HIT_RADIUS {
                hp.0.take_damage(proj.damage);
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
fn cull_dead(
    mut commands: Commands,
    mut sim: ResMut<Sim>,
    pstate: Res<PlayerState>,
    exp: Res<ExplosionAssets>,
    hostiles: Query<(Entity, &Combat, &Hostile, &Transform)>,
    mut player: Query<(&mut Combat, &Transform), (With<PlayerShip>, Without<Hostile>)>,
) {
    for (e, c, h, tf) in &hostiles {
        if !c.0.alive() {
            spawn_explosion(&mut commands, &exp, tf.translation);
            commands.entity(e).despawn();
            sim.0.queue_command(Command::DestroyedPirate {
                player: PLAYER,
                trader: pstate.trader,
                pirate: h.0,
            });
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
    Controls {
        thrust: axis(KeyCode::KeyS, KeyCode::KeyW),
        pitch: axis(KeyCode::ArrowDown, KeyCode::ArrowUp),
        yaw: axis(KeyCode::ArrowRight, KeyCode::ArrowLeft),
        roll: axis(KeyCode::KeyE, KeyCode::KeyQ),
    }
}

fn fly(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    cockpit: Res<Cockpit>,
    pstate: Res<PlayerState>,
    mut q: Query<(&mut Flight, &mut Transform), With<PlayerShip>>,
) {
    // Docked, or between systems (jumping / destroyed): the ship is parked.
    let frozen = cockpit.docked || pstate.system.is_none();
    let controls = if frozen { Controls::default() } else { read_controls(&keys) };
    let dt = time.delta_secs();
    for (mut flight, mut tf) in &mut q {
        flight.0.step(&controls, dt);
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

fn update_hud(
    sim: Res<Sim>,
    player: Res<PlayerState>,
    cockpit: Res<Cockpit>,
    weapon: Res<PlayerWeapon>,
    ship: Query<(&Flight, &Combat), With<PlayerShip>>,
    hostiles: Query<(), With<Hostile>>,
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
        let (speed, near, hull, shield) = ship.single().map_or((0.0, false, 0.0, 0.0), |(f, c)| {
            (
                f.0.speed(),
                f.0.position.distance(STATION_POS) < DOCK_RANGE,
                c.0.hull.max(0.0),
                c.0.shield.max(0.0),
            )
        });
        let enemies = hostiles.iter().count();
        let mut s = format!(
            "SYSTEM  {}   danger {:.2}\nCapital {} cr     SPEED {:>4.0}\nHULL {:>4.0}   SHIELD {:>4.0}   HOSTILES {}\nWEAPON  {}  [Tab to switch]\n\nJump to:\n",
            sd.name, sd.danger, capital, speed, hull, shield, enemies, WEAPONS[weapon.0].name
        );
        for (i, &dest) in sd.connections.iter().enumerate().take(9) {
            s += &format!("  [{}] {}\n", i + 1, reg.system(dest).name);
        }
        s += if near {
            "\n[Space] DOCK    W/S thrust  arrows pitch/yaw  Q/E roll  F fire"
        } else {
            "\nW/S thrust  arrows pitch/yaw  Q/E roll  F fire   (fly to the ring to dock)"
        };
        s
    };
    *text = Text::new(body);
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
