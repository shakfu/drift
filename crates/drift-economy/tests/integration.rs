//! End-to-end economy behavior: trader self-correction, convergence, determinism.

use std::collections::HashSet;
use std::sync::Arc;
use std::path::PathBuf;

use drift_data::{
    CombatStats, CommodityAmount, CommodityDef, ContractConfig, EscortConfig, LoanConfig,
    NavyConfig, PiracyConfig, ProductionRecipe, ScenarioDef, ShipDef, SystemDef, TraderSpawn,
};
use drift_economy::{
    builtin_pricing, Command, CommandError, ContractId, ContractKind, ContractState, FutureSide,
    PiracyStats, PlayerId, TraderId, TraderLocation, World,
};
use drift_mods::{link, load_and_link, MergedContent, Registry};

fn pricing_names() -> HashSet<String> {
    builtin_pricing().names().map(String::from).collect()
}

/// Path to the bundled `mods/` directory, relative to this crate.
fn core_mods_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../mods")
}

/// The full vector of average commodity prices (in registry order), for
/// volatility measurement.
fn price_vector(world: &World) -> Vec<f64> {
    world
        .registry()
        .commodities()
        .map(|(cid, _)| {
            let mut sum = 0i64;
            let mut n = 0i64;
            for m in world.markets() {
                if let Some(p) = m.price(cid) {
                    sum += p;
                    n += 1;
                }
            }
            if n > 0 {
                sum as f64 / n as f64
            } else {
                0.0
            }
        })
        .collect()
}

fn l1(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| (x - y).abs()).sum()
}

// ---------------------------------------------------------------------------
// A: NPC traders narrow a producer/consumer price differential.
// ---------------------------------------------------------------------------

/// Build a minimal two-system galaxy: A produces food, B consumes it, joined by
/// a short jump. `_c` marks the commodity index for readers.
fn two_system_registry() -> Arc<Registry> {
    let merged = MergedContent {
        scripts: vec![],
        commodities: vec![CommodityDef {
            id: "t:food".into(),
            name: "Food".into(),
            base_price: 100,
            unit_mass: 1,
            elasticity: 1.0,
            category: "food".into(),
        }],
        recipes: vec![
            ProductionRecipe {
                id: "t:grow".into(),
                inputs: vec![],
                outputs: vec![CommodityAmount {
                    commodity: "t:food".into(),
                    qty: 10,
                }],
                rate: 1,
                elasticity: 0.0,
            },
            ProductionRecipe {
                id: "t:eat".into(),
                inputs: vec![CommodityAmount {
                    commodity: "t:food".into(),
                    qty: 10,
                }],
                outputs: vec![],
                rate: 1,
                elasticity: 0.0,
            },
        ],
        systems: vec![
            SystemDef {
                id: "t:a".into(),
                name: "A".into(),
                position: [0.0, 0.0],
                industries: vec!["t:grow".into()],
                connections: vec!["t:b".into()],
                initial_stock: vec![CommodityAmount {
                    commodity: "t:food".into(),
                    qty: 500,
                }],
                pricing: "supply_demand_v1".into(),
                danger: 0.0,
            },
            SystemDef {
                id: "t:b".into(),
                name: "B".into(),
                position: [1.0, 0.0],
                industries: vec!["t:eat".into()],
                connections: vec!["t:a".into()],
                initial_stock: vec![CommodityAmount {
                    commodity: "t:food".into(),
                    qty: 500,
                }],
                pricing: "supply_demand_v1".into(),
                danger: 0.0,
            },
        ],
        ships: vec![ShipDef {
            id: "t:freighter".into(),
            name: "Freighter".into(),
            cargo_capacity: 1000,
            jump_speed: 100.0, // ~1 tick between the systems
            hull: 100,
            max_speed: 100.0,
            combat: None,
            visual: None,
        }],
    };
    Arc::new(link(merged, &pricing_names()).expect("inline registry links"))
}

fn scenario(count: u32, ship: &str, capital: i64) -> ScenarioDef {
    ScenarioDef {
        name: "test".into(),
        seed: 1,
        ticks: 0,
        traders: TraderSpawn {
            count,
            ship: ship.into(),
            starting_capital: capital,
        },
        piracy: None,
        risk_aversion: 0.0,
        escort: None,
        navy: None,
        contract: None,
        loan: None,
        insurance: None,
        future: None,
    }
}

#[test]
fn npc_traders_narrow_the_price_differential() {
    let reg = two_system_registry();
    let pricing = builtin_pricing();
    const TICKS: u64 = 300;

    // Without traders: producer gluts (price falls), consumer starves (price
    // rises) — the differential grows unchecked.
    let mut idle = World::new(reg.clone(), &scenario(0, "t:freighter", 0), 1, &pricing).unwrap();
    idle.run(TICKS);
    let differential_without =
        (price_at(&idle, "t:b", "t:food") - price_at(&idle, "t:a", "t:food")).abs();

    // With traders: food is carried A -> B, holding both prices closer together.
    let mut active = World::new(reg.clone(), &scenario(8, "t:freighter", 1_000_000), 1, &pricing).unwrap();
    active.run(TICKS);
    let a_yes = price_at(&active, "t:a", "t:food");
    let b_yes = price_at(&active, "t:b", "t:food");
    let differential_with = (b_yes - a_yes).abs();

    assert!(
        differential_with < differential_without,
        "traders should shrink the differential: without={differential_without}, with={differential_with}"
    );
    assert!(
        differential_with >= 0.0 && differential_with < differential_without * 0.75,
        "expected a meaningful reduction: without={differential_without}, with={differential_with}"
    );
}

/// Price of a commodity at a specific system, both by id.
fn price_at(world: &World, system: &str, commodity: &str) -> f64 {
    let reg = world.registry();
    let cid = reg.commodity_id(commodity).unwrap();
    let sid = reg
        .system_id(system)
        .unwrap_or_else(|| panic!("system {system} not found"));
    world.markets()[sid.index()].price(cid).unwrap() as f64
}

// ---------------------------------------------------------------------------
// A2: price-elastic demand caps a scarce good below its clamp.
// ---------------------------------------------------------------------------

/// One isolated system that both produces commodity `t:x` at a fixed rate and
/// consumes it at a higher nominal rate; `consumer_elasticity` sets whether that
/// demand backs off as the good gets dear. No traders, no neighbours.
fn single_system_registry(consumer_elasticity: f64) -> Arc<Registry> {
    let merged = MergedContent {
        scripts: vec![],
        commodities: vec![CommodityDef {
            id: "t:x".into(),
            name: "X".into(),
            base_price: 100,
            unit_mass: 1,
            elasticity: 1.0,
            category: "misc".into(),
        }],
        recipes: vec![
            // Fixed supply: 5 per tick.
            ProductionRecipe {
                id: "t:make".into(),
                inputs: vec![],
                outputs: vec![CommodityAmount { commodity: "t:x".into(), qty: 5 }],
                rate: 1,
                elasticity: 0.0,
            },
            // Nominal demand 20 per tick (4x supply) — elasticity decides whether
            // it throttles.
            ProductionRecipe {
                id: "t:use".into(),
                inputs: vec![CommodityAmount { commodity: "t:x".into(), qty: 1 }],
                outputs: vec![],
                rate: 20,
                elasticity: consumer_elasticity,
            },
        ],
        systems: vec![SystemDef {
            id: "t:solo".into(),
            name: "Solo".into(),
            position: [0.0, 0.0],
            industries: vec!["t:make".into(), "t:use".into()],
            connections: vec![],
            initial_stock: vec![CommodityAmount { commodity: "t:x".into(), qty: 500 }],
            pricing: "supply_demand_v1".into(),
            danger: 0.0,
        }],
        ships: vec![ShipDef {
            id: "t:freighter".into(),
            name: "Freighter".into(),
            cargo_capacity: 10,
            jump_speed: 1.0,
            hull: 1,
            max_speed: 1.0,
            combat: None,
            visual: None,
        }],
    };
    Arc::new(link(merged, &pricing_names()).expect("single-system registry links"))
}

fn solo_price(elasticity: f64, ticks: u64) -> f64 {
    let reg = single_system_registry(elasticity);
    let pricing = builtin_pricing();
    let mut w = World::new(reg.clone(), &scenario(0, "t:freighter", 0), 1, &pricing).unwrap();
    w.run(ticks);
    let cid = reg.commodity_id("t:x").unwrap();
    w.markets()[0].price(cid).unwrap() as f64
}

#[test]
fn elastic_demand_caps_scarcity_price_below_clamp() {
    // Clamp ceiling for base 100 is MAX_FACTOR * base.
    let clamp = 4.0 * 100.0;

    // Inelastic demand: consumption ignores price, the good drains, price pins at
    // the clamp.
    let inelastic = solo_price(0.0, 800);
    assert!(
        inelastic >= clamp * 0.95,
        "inelastic demand should pin near the clamp, got {inelastic}"
    );

    // Elastic demand: consumption backs off as the good gets dear, so the price
    // settles at an interior equilibrium (analytically ~200 here), well below the
    // clamp.
    let elastic = solo_price(2.0, 800);
    assert!(
        elastic < clamp * 0.75,
        "elastic demand should hold price below the clamp, got {elastic}"
    );
    assert!(elastic < inelastic, "elastic {elastic} should be below inelastic {inelastic}");

    // ...and it is a stable equilibrium, not still moving.
    let earlier = solo_price(2.0, 600);
    assert!(
        (elastic - earlier).abs() < clamp * 0.05,
        "elastic equilibrium should be stable: t=600 {earlier}, t=800 {elastic}"
    );
}

// ---------------------------------------------------------------------------
// B: the full galaxy converges to stable prices.
// ---------------------------------------------------------------------------

#[test]
fn full_galaxy_prices_converge() {
    let reg = Arc::new(load_and_link(&core_mods_path(), &pricing_names()).expect("core mod links"));
    let scn = scenario(24, "core:cobra_mk3", 5000);
    let pricing = builtin_pricing();
    let mut world = World::new(reg.clone(), &scn, 42, &pricing).unwrap();

    // Sample the price vector every `STEP` ticks and accumulate the per-step L1
    // movement in an early vs. a late window. Convergence = late movement is much
    // smaller than early movement.
    const STEP: u64 = 50;
    const SAMPLES: u64 = 40; // 2000 ticks
    let mut prev = price_vector(&world);
    let mut early = 0.0;
    let mut late = 0.0;
    for i in 0..SAMPLES {
        world.run(STEP);
        let cur = price_vector(&world);
        let movement = l1(&prev, &cur);
        if i < SAMPLES / 4 {
            early += movement;
        } else if i >= SAMPLES * 3 / 4 {
            late += movement;
        }
        prev = cur;
    }

    assert!(
        late < early * 0.25,
        "prices did not converge: early movement {early:.1}, late movement {late:.1}"
    );

    // No price diverged: the clamp keeps every commodity strictly positive and
    // finite (never zero, never infinity).
    for p in price_vector(&world) {
        assert!(p.is_finite() && p > 0.0, "price {p} out of bounds");
    }
}

// B2: manufacturing capacity is a live, convergent investment dynamic.
// ---------------------------------------------------------------------------

#[test]
fn manufacturing_capacity_invests_and_settles() {
    use drift_economy::production::{has_capacity, MAX_CAPACITY, MIN_CAPACITY};

    let reg = Arc::new(load_and_link(&core_mods_path(), &pricing_names()).expect("core mod links"));
    let scn = scenario(24, "core:cobra_mk3", 5000);
    let pricing = builtin_pricing();
    let mut world = World::new(reg.clone(), &scn, 42, &pricing).unwrap();

    // A flat snapshot of the capacity field, for movement measurement.
    let snap = |w: &World| -> Vec<f64> { w.capacity().iter().flatten().copied().collect() };

    // Sample capacity movement in an early vs. a late window: capital should surge
    // as it first chases returns, then settle. (Mirrors the price-convergence test.)
    const STEP: u64 = 50;
    const SAMPLES: u64 = 80; // 4000 ticks
    let mut prev = snap(&world);
    let mut early = 0.0;
    let mut late = 0.0;
    for i in 0..SAMPLES {
        world.run(STEP);
        let cur = snap(&world);
        let movement = l1(&prev, &cur);
        if i < SAMPLES / 4 {
            early += movement;
        } else if i >= SAMPLES * 3 / 4 {
            late += movement;
        }
        prev = cur;
    }

    // 1. Every capacity stays within the capital bounds.
    for &c in &snap(&world) {
        assert!(
            (MIN_CAPACITY..=MAX_CAPACITY).contains(&c),
            "capacity {c} escaped [{MIN_CAPACITY}, {MAX_CAPACITY}]"
        );
    }

    // 2. Only manufacturing (transformer) industries invest; raw extractors and
    //    consumers hold nominal capacity exactly. And at least one manufacturing
    //    industry actually moved — the dynamic is live, not inert.
    let mut manufacturing_moved = false;
    for (si, sys) in reg.systems().enumerate() {
        for (j, &rid) in sys.industries.iter().enumerate() {
            let cap = world.capacity()[si][j];
            if has_capacity(reg.recipe(rid)) {
                if (cap - 1.0).abs() > 0.05 {
                    manufacturing_moved = true;
                }
            } else {
                assert!(
                    (cap - 1.0).abs() < 1e-9,
                    "a non-transformer industry drifted to capacity {cap}, expected 1.0"
                );
            }
        }
    }
    assert!(
        manufacturing_moved,
        "no manufacturing capacity moved off nominal — the investment dynamic is inert"
    );

    // 3. Capital converges: late-window movement is a fraction of the early surge.
    assert!(
        late < early * 0.5,
        "capacity did not settle: early movement {early:.3}, late movement {late:.3}"
    );
}

// ---------------------------------------------------------------------------
// C: runs are deterministic in the seed.
// ---------------------------------------------------------------------------

#[test]
fn same_seed_is_byte_identical() {
    let reg = Arc::new(load_and_link(&core_mods_path(), &pricing_names()).expect("core mod links"));
    let scn = scenario(24, "core:cobra_mk3", 5000);
    let pricing = builtin_pricing();

    let run = |seed: u64| {
        let mut w = World::new(reg.clone(), &scn, seed, &pricing).unwrap();
        w.run(500);
        serde_json::to_string(&w.snapshot()).unwrap()
    };

    assert_eq!(run(42), run(42), "same seed must produce identical state");
    assert_ne!(
        run(42),
        run(7),
        "different seeds should diverge (trader placement differs)"
    );
}

// ---------------------------------------------------------------------------
// D: piracy — combat integrated into the live galaxy.
// ---------------------------------------------------------------------------

fn scenario_piracy(
    count: u32,
    ship: &str,
    pirate_ship: &str,
    chance: f64,
    risk_aversion: f64,
) -> ScenarioDef {
    ScenarioDef {
        name: "piracy".into(),
        seed: 1,
        ticks: 0,
        traders: TraderSpawn {
            count,
            ship: ship.into(),
            starting_capital: 5000,
        },
        piracy: Some(PiracyConfig {
            pirate_ship: pirate_ship.into(),
            base_ambush_chance: chance,
            max_pirates: 2,
            respawn_delay: 40,
            fleet_size: 12,
            bounty: 300,
            reinforce_interval: 15,
        }),
        risk_aversion,
        escort: None,
        navy: None,
        contract: None,
        loan: None,
        insurance: None,
        future: None,
    }
}

#[test]
fn piracy_destroys_traders_deterministically() {
    let reg = Arc::new(load_and_link(&core_mods_path(), &pricing_names()).expect("core mod links"));
    let pricing = builtin_pricing();
    // Unarmed shuttles hauling through the dangerous frontier: easy prey.
    let scn = scenario_piracy(20, "core:shuttle", "core:pirate", 0.15, 0.0);

    let run = |seed: u64| {
        let mut w = World::new(reg.clone(), &scn, seed, &pricing).unwrap();
        w.run(1500);
        w
    };

    let w = run(42);
    let stats = w.piracy_stats();
    assert!(stats.ambushes > 0, "dangerous routes should trigger ambushes");
    assert!(stats.traders_lost > 0, "unarmed traders should be destroyed");
    // The economy must not collapse under piracy: prices stay finite and positive.
    for p in price_vector(&w) {
        assert!(p.is_finite() && p > 0.0, "price {p} out of bounds under piracy");
    }
    // Same seed reproduces the exact piracy outcome.
    assert_eq!(stats, run(42).piracy_stats(), "piracy must be deterministic");
}

#[test]
fn armed_traders_survive_piracy_better_than_unarmed() {
    let reg = Arc::new(load_and_link(&core_mods_path(), &pricing_names()).expect("core mod links"));
    let pricing = builtin_pricing();

    let losses = |ship: &str| {
        let scn = scenario_piracy(20, ship, "core:pirate", 0.15, 0.0);
        let mut w = World::new(reg.clone(), &scn, 42, &pricing).unwrap();
        w.run(1500);
        w.piracy_stats()
    };

    let armed = losses("core:cobra_mk3");
    let unarmed = losses("core:shuttle");
    assert!(
        armed.traders_lost < unarmed.traders_lost,
        "armed traders ({}) should lose fewer ships than unarmed ({})",
        armed.traders_lost,
        unarmed.traders_lost
    );
    assert!(
        armed.pirates_destroyed > 0,
        "armed traders should destroy pirates"
    );
}

#[test]
fn no_piracy_config_means_no_ambushes() {
    let reg = Arc::new(load_and_link(&core_mods_path(), &pricing_names()).expect("core mod links"));
    let pricing = builtin_pricing();
    let scn = scenario(24, "core:cobra_mk3", 5000); // piracy: None
    let mut w = World::new(reg.clone(), &scn, 42, &pricing).unwrap();
    w.run(1000);
    assert_eq!(
        w.piracy_stats(),
        PiracyStats::default(),
        "no piracy config => no piracy activity"
    );
}

#[test]
fn safe_routes_are_never_ambushed() {
    // The two-system galaxy has danger 0.0 everywhere, so even with a high ambush
    // chance and unarmed traders there can be no ambushes.
    let reg = two_system_registry();
    let pricing = builtin_pricing();
    let scn = scenario_piracy(8, "t:freighter", "t:freighter", 0.9, 0.0);
    let mut w = World::new(reg.clone(), &scn, 1, &pricing).unwrap();
    w.run(500);
    assert!(
        w.pirates().is_empty(),
        "a galaxy with no dangerous systems must never spawn pirates"
    );
    assert_eq!(
        w.piracy_stats().ambushes,
        0,
        "danger 0 must yield no ambushes despite a piracy config"
    );
}

#[test]
fn risk_aware_routing_reduces_losses() {
    let reg = Arc::new(load_and_link(&core_mods_path(), &pricing_names()).expect("core mod links"));
    let pricing = builtin_pricing();

    // Armed cobras so surviving traders keep trading (isolating the routing choice
    // from respawn churn). Fewer ships should be lost as risk aversion rises.
    let losses = |aversion: f64| {
        let scn = scenario_piracy(20, "core:cobra_mk3", "core:pirate", 0.15, aversion);
        let mut w = World::new(reg.clone(), &scn, 42, &pricing).unwrap();
        w.run(2000);
        w.piracy_stats().traders_lost
    };

    let neutral = losses(0.0);
    let cautious = losses(2.0);
    assert!(
        cautious < neutral,
        "risk-aware traders should lose fewer ships: neutral={neutral}, cautious={cautious}"
    );
}

#[test]
fn pirate_fleet_is_persistent_and_bounded() {
    let reg = Arc::new(load_and_link(&core_mods_path(), &pricing_names()).expect("core mod links"));
    let pricing = builtin_pricing();
    // fleet_size in scenario_piracy is 12.
    let scn = scenario_piracy(20, "core:cobra_mk3", "core:pirate", 0.15, 0.0);
    let mut w = World::new(reg.clone(), &scn, 42, &pricing).unwrap();

    assert!(!w.pirates().is_empty(), "the fleet spawns at lawless systems");
    w.run(2000);

    assert!(
        !w.pirates().is_empty(),
        "reinforcement keeps the fleet alive across the run"
    );
    assert!(
        w.pirates().len() <= 12,
        "the fleet never exceeds its target size ({} > 12)",
        w.pirates().len()
    );
    assert!(
        w.piracy_stats().pirates_destroyed > 0,
        "there should be real attrition (pirates killed and replaced)"
    );
    // Pirates only ever occupy dangerous systems.
    for p in w.pirates() {
        if let Some(sys) = p.docked_at() {
            assert!(
                reg.system(sys).danger > 0.0,
                "pirates must stay in lawless space"
            );
        }
    }
}

#[test]
fn bounties_reward_armed_traders() {
    let reg = Arc::new(load_and_link(&core_mods_path(), &pricing_names()).expect("core mod links"));
    let pricing = builtin_pricing();
    let scn = scenario_piracy(20, "core:cobra_mk3", "core:pirate", 0.15, 0.0);
    let mut w = World::new(reg.clone(), &scn, 42, &pricing).unwrap();
    w.run(2000);
    let stats = w.piracy_stats();
    assert!(stats.pirates_destroyed > 0, "armed traders should kill pirates");
    assert!(
        stats.bounties_paid > 0,
        "victorious traders should collect bounties"
    );
}

#[test]
fn pirate_fleet_can_be_depleted_without_reinforcement() {
    let reg = Arc::new(load_and_link(&core_mods_path(), &pricing_names()).expect("core mod links"));
    let pricing = builtin_pricing();
    // No effective reinforcement: an aggressive armed fleet should grind the
    // pirates down over time.
    let scn = ScenarioDef {
        name: "deplete".into(),
        seed: 7,
        ticks: 0,
        traders: TraderSpawn {
            count: 24,
            ship: "core:cobra_mk3".into(),
            starting_capital: 5000,
        },
        piracy: Some(PiracyConfig {
            pirate_ship: "core:pirate".into(),
            base_ambush_chance: 0.2,
            max_pirates: 2,
            respawn_delay: 40,
            fleet_size: 10,
            bounty: 300,
            reinforce_interval: 1_000_000, // never reinforces within the run
        }),
        risk_aversion: 0.0,
        escort: None,
        navy: None,
        contract: None,
        loan: None,
        insurance: None,
        future: None,
    };
    let initial = World::new(reg.clone(), &scn, 7, &pricing).unwrap().pirates().len();
    let mut w = World::new(reg.clone(), &scn, 7, &pricing).unwrap();
    w.run(4000);

    assert!(initial > 0, "fleet starts populated");
    assert!(
        w.pirates().len() < initial,
        "an un-reinforced fleet should be depleted by armed traders: {} -> {}",
        initial,
        w.pirates().len()
    );
}

// ---------------------------------------------------------------------------
// E: trader escorts and navy patrols.
// ---------------------------------------------------------------------------

/// A piracy scenario over the core mod, optionally with convoy escorts and/or a
/// navy patrol. `risk_aversion` is 0 so traders engage rather than avoid.
fn scenario_defended(
    trader_ship: &str,
    escort: Option<(&str, u32)>,
    navy: Option<(&str, u32)>,
) -> ScenarioDef {
    ScenarioDef {
        name: "defended".into(),
        seed: 1,
        ticks: 0,
        traders: TraderSpawn {
            count: 20,
            ship: trader_ship.into(),
            starting_capital: 5000,
        },
        piracy: Some(PiracyConfig {
            pirate_ship: "core:pirate".into(),
            base_ambush_chance: 0.15,
            max_pirates: 2,
            respawn_delay: 40,
            fleet_size: 12,
            bounty: 300,
            reinforce_interval: 15,
        }),
        risk_aversion: 0.0,
        escort: escort.map(|(ship, count)| EscortConfig {
            ship: ship.into(),
            count,
            fee: 0, // free by default; a dedicated test exercises fees
        }),
        navy: navy.map(|(ship, fleet_size)| NavyConfig {
            ship: ship.into(),
            fleet_size,
            reinforce_interval: 20,
            // Well-funded by default so existing navy tests are unaffected; a
            // dedicated test exercises an underfunded navy.
            upkeep: 1,
            funding: 1000,
        }),
        contract: None,
        loan: None,
        insurance: None,
        future: None,
    }
}

#[test]
fn escorts_reduce_trader_losses() {
    let reg = Arc::new(load_and_link(&core_mods_path(), &pricing_names()).expect("core mod links"));
    let pricing = builtin_pricing();
    // Unarmed shuttles: without help every ambush is fatal.
    let losses = |escort| {
        let scn = scenario_defended("core:shuttle", escort, None);
        let mut w = World::new(reg.clone(), &scn, 42, &pricing).unwrap();
        w.run(1500);
        w.piracy_stats().traders_lost
    };
    let unescorted = losses(None);
    let escorted = losses(Some(("core:escort", 2)));
    assert!(unescorted > 0, "unescorted shuttles should be dying");
    assert!(
        escorted < unescorted,
        "escorts should keep traders alive: unescorted={unescorted}, escorted={escorted}"
    );
}

#[test]
fn navy_patrols_suppress_pirates() {
    let reg = Arc::new(load_and_link(&core_mods_path(), &pricing_names()).expect("core mod links"));
    let pricing = builtin_pricing();
    let scn = scenario_defended("core:cobra_mk3", None, Some(("core:navy", 6)));
    let mut w = World::new(reg.clone(), &scn, 42, &pricing).unwrap();

    assert!(!w.navy().is_empty(), "the navy spawns to patrol lawless space");
    w.run(2000);

    assert!(
        w.piracy_stats().pirates_suppressed > 0,
        "the navy should destroy pirates on patrol"
    );
    assert!(w.navy().len() <= 6, "the navy never exceeds its fleet size");
    // The navy only patrols dangerous systems.
    for n in w.navy() {
        if let Some(sys) = n.docked_at() {
            assert!(reg.system(sys).danger > 0.0, "navy patrols lawless space only");
        }
    }
}

#[test]
fn navy_reduces_trader_losses() {
    let reg = Arc::new(load_and_link(&core_mods_path(), &pricing_names()).expect("core mod links"));
    let pricing = builtin_pricing();
    // A navy changes the whole RNG trajectory, so a single-seed comparison is
    // noisy; sum losses across several seeds to capture the causal effect.
    let total_losses = |navy| -> u64 {
        (0..8)
            .map(|seed| {
                let scn = scenario_defended("core:shuttle", None, navy);
                let mut w = World::new(reg.clone(), &scn, seed, &pricing).unwrap();
                w.run(1500);
                w.piracy_stats().traders_lost
            })
            .sum()
    };
    let undefended = total_losses(None);
    let defended = total_losses(Some(("core:navy", 12)));
    assert!(
        defended < undefended,
        "a strong navy (suppressing pirates and defending convoys) should cut total \
         losses across seeds: undefended={undefended}, defended={defended}"
    );
}

#[test]
fn safe_galaxy_has_no_navy() {
    // Two-system galaxy, danger 0 everywhere: even with a navy config there is
    // nothing to patrol, so no navy spawns.
    let reg = two_system_registry();
    let pricing = builtin_pricing();
    let mut scn = scenario(8, "t:freighter", 5000);
    scn.navy = Some(NavyConfig {
        ship: "t:freighter".into(),
        fleet_size: 5,
        reinforce_interval: 10,
        upkeep: 0,
        funding: 0,
    });
    let mut w = World::new(reg.clone(), &scn, 1, &pricing).unwrap();
    w.run(300);
    assert!(w.navy().is_empty(), "no lawless systems => no navy");
}

#[test]
fn defended_runs_are_deterministic() {
    let reg = Arc::new(load_and_link(&core_mods_path(), &pricing_names()).expect("core mod links"));
    let pricing = builtin_pricing();
    let run = |seed: u64| {
        let scn = scenario_defended("core:cobra_mk3", Some(("core:escort", 1)), Some(("core:navy", 6)));
        let mut w = World::new(reg.clone(), &scn, seed, &pricing).unwrap();
        w.run(800);
        serde_json::to_string(&w.snapshot()).unwrap()
    };
    assert_eq!(run(42), run(42), "escorts + navy must stay deterministic");
}

// ---------------------------------------------------------------------------
// F: the command pipeline (multiplayer-ready player actions).
// ---------------------------------------------------------------------------

/// The id of the (first) trader owned by `player`, read from world state as a
/// real client would after spawning.
fn player_trader_id(w: &World, player: PlayerId) -> TraderId {
    w.traders()
        .iter()
        .find(|t| t.owner == drift_economy::Owner::Player(player))
        .expect("player has a trader")
        .id
}

#[test]
fn player_can_spawn_and_trade_via_commands() {
    let reg = Arc::new(load_and_link(&core_mods_path(), &pricing_names()).expect("core mod links"));
    let pricing = builtin_pricing();
    // No NPC traders (count 0), no piracy: isolate the player.
    let scn = scenario(0, "core:cobra_mk3", 5000);
    let mut w = World::new(reg.clone(), &scn, 1, &pricing).unwrap();

    let player = PlayerId(0);
    let ship = reg.ship_id("core:cobra_mk3").unwrap();
    let lave = reg.system_id("core:lave").unwrap();
    let food = reg.commodity_id("core:food").unwrap();

    // Spawn a player-owned trader, then read its server-assigned id from state.
    w.queue_command(Command::Spawn { player, ship, at: lave, capital: 10_000 });
    w.tick();
    assert_eq!(w.traders().len(), 1);
    assert!(w.traders()[0].is_player());
    assert_eq!(w.traders()[0].location, TraderLocation::Docked(lave));
    assert_eq!(w.commands_applied(), 1);
    let tid = player_trader_id(&w, player);

    // Buy 10 food. command_phase runs before this tick's repricing, so the price
    // paid is the one currently on the market.
    let price = w.markets()[lave.index()].price(food).unwrap();
    let cap_before = w.traders()[0].capital;
    w.queue_command(Command::Buy { player, trader: tid, commodity: food, qty: 10 });
    w.tick();
    assert_eq!(w.traders()[0].cargo.get(&food).copied(), Some(10));
    assert_eq!(w.traders()[0].capital, cap_before - price * 10);

    // Sell 5 back.
    let sell_price = w.markets()[lave.index()].price(food).unwrap();
    let cap_before_sell = w.traders()[0].capital;
    w.queue_command(Command::Sell { player, trader: tid, commodity: food, qty: 5 });
    w.tick();
    assert_eq!(w.traders()[0].cargo.get(&food).copied(), Some(5));
    assert_eq!(w.traders()[0].capital, cap_before_sell + sell_price * 5);

    // Jump to a connected system.
    let leesti = reg.system_id("core:leesti").unwrap();
    w.queue_command(Command::Jump { player, trader: tid, dest: leesti });
    w.tick();
    assert!(matches!(
        w.traders()[0].location,
        TraderLocation::InTransit { dest, .. } if dest == leesti
    ));
    assert_eq!(w.commands_rejected(), 0, "no command should have been rejected");
}

#[test]
fn scooped_cargo_is_added_free_and_capped_by_the_hold() {
    let reg = Arc::new(load_and_link(&core_mods_path(), &pricing_names()).expect("core mod links"));
    let pricing = builtin_pricing();
    let scn = scenario(0, "core:cobra_mk3", 5000);
    let mut w = World::new(reg.clone(), &scn, 1, &pricing).unwrap();

    let player = PlayerId(0);
    let ship = reg.ship_id("core:cobra_mk3").unwrap();
    let lave = reg.system_id("core:lave").unwrap();
    let food = reg.commodity_id("core:food").unwrap();

    w.queue_command(Command::Spawn { player, ship, at: lave, capital: 5000 });
    w.tick();
    let tid = player_trader_id(&w, player);
    let cap_before = w.traders()[0].capital;

    // Scoop a small canister: added to the hold, free of charge.
    w.queue_command(Command::ScoopCargo { player, trader: tid, commodity: food, qty: 3 });
    w.tick();
    assert_eq!(w.traders()[0].cargo.get(&food).copied(), Some(3), "scoop fills the hold");
    assert_eq!(w.traders()[0].capital, cap_before, "scooping is free");

    // A huge scoop is capped at what the hold can carry (Cobra capacity 35, food
    // mass 1), not the full requested amount.
    w.queue_command(Command::ScoopCargo { player, trader: tid, commodity: food, qty: 10_000 });
    w.tick();
    let capacity = reg.ship(ship).cargo_capacity;
    assert_eq!(
        w.traders()[0].cargo.get(&food).copied(),
        Some(capacity),
        "scoop is capped by the hold, not the request"
    );

    // A full hold rejects a further scoop.
    let rejected_before = w.commands_rejected();
    w.queue_command(Command::ScoopCargo { player, trader: tid, commodity: food, qty: 1 });
    w.tick();
    assert_eq!(w.commands_rejected(), rejected_before + 1, "a full hold rejects the scoop");
}

#[test]
fn invalid_commands_are_rejected_without_side_effects() {
    let reg = Arc::new(load_and_link(&core_mods_path(), &pricing_names()).expect("core mod links"));
    let pricing = builtin_pricing();
    let scn = scenario(0, "core:cobra_mk3", 5000);
    let mut w = World::new(reg.clone(), &scn, 1, &pricing).unwrap();

    let player = PlayerId(0);
    let ship = reg.ship_id("core:cobra_mk3").unwrap();
    let lave = reg.system_id("core:lave").unwrap();
    let food = reg.commodity_id("core:food").unwrap();
    let ore = reg.commodity_id("core:ore").unwrap();
    let tionisla = reg.system_id("core:tionisla").unwrap(); // not adjacent to Lave

    w.queue_command(Command::Spawn { player, ship, at: lave, capital: 500 });
    w.tick();
    let tid = player_trader_id(&w, player);
    let cap = w.traders()[0].capital;
    let loc = w.traders()[0].location.clone();
    let applied_before = w.commands_applied();

    // Five bad commands, each failing a different check.
    w.queue_command(Command::Buy { player, trader: tid, commodity: food, qty: 1_000_000 }); // funds/stock
    w.queue_command(Command::Buy { player, trader: tid, commodity: ore, qty: 1 }); // Lave doesn't trade ore
    w.queue_command(Command::Jump { player, trader: tid, dest: tionisla }); // unreachable
    w.queue_command(Command::Buy { player: PlayerId(1), trader: tid, commodity: food, qty: 1 }); // not owner
    w.queue_command(Command::Jump { player, trader: TraderId(9999), dest: lave }); // unknown trader
    w.tick();

    assert_eq!(w.commands_rejected(), 5, "all five should be rejected");
    assert_eq!(w.commands_applied(), applied_before, "none should apply");
    // The trader is untouched.
    assert_eq!(w.traders()[0].capital, cap);
    assert!(w.traders()[0].cargo.is_empty());
    assert_eq!(w.traders()[0].location, loc);
}

#[test]
fn trader_ids_are_stable_across_removal() {
    let reg = Arc::new(load_and_link(&core_mods_path(), &pricing_names()).expect("core mod links"));
    let pricing = builtin_pricing();
    let scn = scenario(0, "core:cobra_mk3", 5000);
    let mut w = World::new(reg.clone(), &scn, 1, &pricing).unwrap();

    let player = PlayerId(0);
    let ship = reg.ship_id("core:cobra_mk3").unwrap();
    let lave = reg.system_id("core:lave").unwrap();
    let food = reg.commodity_id("core:food").unwrap();

    // Spawn two player traders; capture their ids from state.
    w.queue_command(Command::Spawn { player, ship, at: lave, capital: 5_000 });
    w.queue_command(Command::Spawn { player, ship, at: lave, capital: 8_000 });
    w.tick();
    assert_eq!(w.traders().len(), 2);
    let id_a = w.traders()[0].id;
    let id_b = w.traders()[1].id;
    assert_ne!(id_a, id_b);

    // Retire A. B shifts from index 1 to index 0, but its id is unchanged.
    w.queue_command(Command::Despawn { player, trader: id_a });
    w.tick();
    assert_eq!(w.traders().len(), 1);
    assert_eq!(w.traders()[0].id, id_b, "B kept its id despite moving slots");

    // A's id is now stale -> its command is rejected (no accidental hit on B).
    let rejected_before = w.commands_rejected();
    w.queue_command(Command::Buy { player, trader: id_a, commodity: food, qty: 1 });
    w.tick();
    assert_eq!(w.commands_rejected(), rejected_before + 1);

    // B still resolves correctly by id even though its slot moved.
    let price = w.markets()[lave.index()].price(food).unwrap();
    let cap_b = w.traders().iter().find(|t| t.id == id_b).unwrap().capital;
    w.queue_command(Command::Buy { player, trader: id_b, commodity: food, qty: 3 });
    w.tick();
    let b = w.traders().iter().find(|t| t.id == id_b).unwrap();
    assert_eq!(b.cargo.get(&food).copied(), Some(3));
    assert_eq!(b.capital, cap_b - price * 3);
}

#[test]
fn player_traders_are_not_ai_driven() {
    let reg = Arc::new(load_and_link(&core_mods_path(), &pricing_names()).expect("core mod links"));
    let pricing = builtin_pricing();
    let scn = scenario(0, "core:cobra_mk3", 5000);
    let mut w = World::new(reg.clone(), &scn, 1, &pricing).unwrap();

    let player = PlayerId(0);
    let ship = reg.ship_id("core:cobra_mk3").unwrap();
    let lave = reg.system_id("core:lave").unwrap();
    w.queue_command(Command::Spawn { player, ship, at: lave, capital: 7_000 });
    w.tick();

    // With no further commands, the player's trader must sit still: the NPC AI
    // never touches it.
    w.run(300);
    assert_eq!(w.traders()[0].location, TraderLocation::Docked(lave));
    assert!(w.traders()[0].cargo.is_empty());
    assert_eq!(w.traders()[0].capital, 7_000);
}

#[test]
fn commanded_runs_are_deterministic() {
    let reg = Arc::new(load_and_link(&core_mods_path(), &pricing_names()).expect("core mod links"));
    let pricing = builtin_pricing();
    let scn = scenario(6, "core:cobra_mk3", 5000); // some NPCs too

    let scripted = |seed: u64| {
        let mut w = World::new(reg.clone(), &scn, seed, &pricing).unwrap();
        let player = PlayerId(0);
        let ship = reg.ship_id("core:cobra_mk3").unwrap();
        let lave = reg.system_id("core:lave").unwrap();
        let food = reg.commodity_id("core:food").unwrap();
        let leesti = reg.system_id("core:leesti").unwrap();
        w.queue_command(Command::Spawn { player, ship, at: lave, capital: 10_000 });
        w.run(5);
        let tid = player_trader_id(&w, player);
        w.queue_command(Command::Buy { player, trader: tid, commodity: food, qty: 5 });
        w.run(5);
        w.queue_command(Command::Jump { player, trader: tid, dest: leesti });
        w.run(20);
        serde_json::to_string(&w.snapshot()).unwrap()
    };
    assert_eq!(scripted(1), scripted(1), "same seed + same commands => identical state");
}

#[test]
fn simulation_events_are_recorded_and_deterministic() {
    let reg = Arc::new(load_and_link(&core_mods_path(), &pricing_names()).expect("core mod links"));
    let pricing = builtin_pricing();
    // Piracy drives ambush/respawn events.
    let scn = scenario_piracy(20, "core:cobra_mk3", "core:pirate", 0.15, 0.0);
    let run = |seed: u64| {
        let mut w = World::new(reg.clone(), &scn, seed, &pricing).unwrap();
        w.run(1500);
        w.events()
            .map(|e| (e.tick, e.category, e.message.clone()))
            .collect::<Vec<_>>()
    };
    let a = run(42);
    assert!(!a.is_empty(), "a piracy run should record events");
    assert_eq!(a, run(42), "events are deterministic for a fixed seed");
}

#[test]
fn combat_events_carry_their_location() {
    // Every fight-related event (Combat/Piracy/Navy) records where it happened, so
    // a viewer can place it on the map; and that location is a lawless system,
    // since ambushes and patrols only occur where danger > 0.
    use drift_economy::EventCategory;
    let reg = Arc::new(load_and_link(&core_mods_path(), &pricing_names()).expect("core mod links"));
    let pricing = builtin_pricing();
    let scn = scenario_defended("core:cobra_mk3", None, Some(("core:navy", 6)));
    let mut w = World::new(reg.clone(), &scn, 42, &pricing).unwrap();
    w.run(2000);

    let combat: Vec<_> = w
        .events()
        .filter(|e| matches!(
            e.category,
            EventCategory::Combat | EventCategory::Piracy | EventCategory::Navy
        ))
        .collect();
    assert!(!combat.is_empty(), "a defended piracy run should record fights");
    for e in combat {
        let sys = e.system.expect("a fight event must know its system");
        assert!(
            reg.system(sys).danger > 0.0,
            "fights occur in lawless space: {} at danger {}",
            e.message,
            reg.system(sys).danger
        );
    }
}

#[test]
fn per_tick_event_streaming_reconstructs_the_full_log() {
    let reg = Arc::new(load_and_link(&core_mods_path(), &pricing_names()).expect("core mod links"));
    let pricing = builtin_pricing();
    let scn = scenario_piracy(20, "core:cobra_mk3", "core:pirate", 0.15, 0.0);
    let mut w = World::new(reg.clone(), &scn, 7, &pricing).unwrap();

    // Short enough that the (2000-cap) ring buffer never drops anything, so the
    // final buffer holds every event that was emitted.
    const TICKS: u64 = 150;
    let mut streamed: Vec<(u64, String)> = Vec::new();
    for _ in 0..TICKS {
        let now = w.tick_count();
        w.tick();
        let mut this: Vec<(u64, String)> = w
            .events()
            .rev()
            .take_while(|e| e.tick == now)
            .map(|e| (e.tick.get(), e.message.clone()))
            .collect();
        this.reverse();
        streamed.extend(this);
    }

    let full: Vec<(u64, String)> =
        w.events().map(|e| (e.tick.get(), e.message.clone())).collect();
    assert!(!streamed.is_empty(), "the run should emit events");
    assert_eq!(
        streamed, full,
        "streaming each tick's tail events reconstructs the full log exactly, in order"
    );
}

// ---------------------------------------------------------------------------
// G: delivery contracts — missions layered on the spot economy.
// ---------------------------------------------------------------------------

/// A contract board over the two-system galaxy. `deadline_ticks` is left long so
/// contracts do not expire mid-test; the expiry test overrides it.
fn scenario_contracts(deadline_ticks: u64) -> ScenarioDef {
    ScenarioDef {
        name: "contracts".into(),
        seed: 1,
        ticks: 0,
        traders: TraderSpawn {
            count: 0,
            ship: "t:freighter".into(),
            starting_capital: 0,
        },
        piracy: None,
        risk_aversion: 0.0,
        escort: None,
        navy: None,
        contract: Some(ContractConfig {
            max_open: 4,
            generation_interval: 5,
            deadline_ticks,
            reward_factor: 1.5,
            min_shortfall: 10,
            max_quantity: 20,
            // Delivery-only board (the two-system galaxy has no danger/pirates).
            bounty_target: 0,
            bounty_reward: 0,
            courier_reward: 0,
        }),
        loan: None,
        insurance: None,
        future: None,
    }
}

#[test]
fn contracts_are_generated_from_shortages_deterministically() {
    // With no traders, system B (which only consumes food) drains below its
    // equilibrium and posts an import contract; A (which only produces) never does.
    let reg = two_system_registry();
    let pricing = builtin_pricing();
    let board = |seed| {
        let mut w = World::new(reg.clone(), &scenario_contracts(1000), seed, &pricing).unwrap();
        w.run(200);
        w.contracts().to_vec()
    };

    let a = board(1);
    assert!(!a.is_empty(), "sustained consumption at B should post an import contract");
    assert_eq!(a, board(1), "the board is deterministic for a fixed seed");

    let bsys = reg.system_id("t:b").unwrap();
    let food = reg.commodity_id("t:food").unwrap();
    for c in &a {
        assert_eq!(c.destination, bsys, "contracts target the starved consumer, B");
        let (commodity, quantity) = c.cargo().expect("delivery-only board");
        assert_eq!(commodity, food);
        assert!(c.reward > 0, "a contract must pay a positive reward");
        assert!(quantity > 0 && quantity <= 20, "quantity is capped by max_quantity");
    }
}

#[test]
fn player_accepts_and_fulfills_a_contract() {
    let reg = two_system_registry();
    let pricing = builtin_pricing();
    let mut w = World::new(reg.clone(), &scenario_contracts(1000), 1, &pricing).unwrap();

    let player = PlayerId(0);
    let ship = reg.ship_id("t:freighter").unwrap();
    let asys = reg.system_id("t:a").unwrap();
    let bsys = reg.system_id("t:b").unwrap();
    let food = reg.commodity_id("t:food").unwrap();

    // Let a shortage build at B so a contract is posted.
    w.run(20);
    let contract = w.contracts().iter().find(|c| c.is_open()).expect("an open contract").clone();
    let cid = contract.id;
    let need = contract.cargo().map(|(_, q)| q).expect("a delivery contract");
    let reward = contract.reward;
    assert_eq!(contract.destination, bsys);

    // Spawn a player trader at A (the surplus source), with capital to buy cargo.
    w.queue_command(Command::Spawn { player, ship, at: asys, capital: 1_000_000 });
    w.tick();
    let tid = player_trader_id(&w, player);

    // Accept the contract.
    w.queue_command(Command::AcceptContract { player, trader: tid, contract: cid });
    w.tick();
    assert!(
        matches!(
            w.contracts().iter().find(|c| c.id == cid).unwrap().state,
            ContractState::Accepted { .. }
        ),
        "the accepted contract is now held"
    );

    // Buy the required cargo at A, then jump to B and wait for arrival.
    w.queue_command(Command::Buy { player, trader: tid, commodity: food, qty: need });
    w.tick();
    assert_eq!(
        w.traders().iter().find(|t| t.id == tid).unwrap().cargo.get(&food).copied(),
        Some(need)
    );
    w.queue_command(Command::Jump { player, trader: tid, dest: bsys });
    w.run(5);
    assert!(
        matches!(
            w.traders().iter().find(|t| t.id == tid).unwrap().location,
            TraderLocation::Docked(s) if s == bsys
        ),
        "the trader should have arrived at B"
    );

    // Deliver: reward paid, cargo consumed, contract removed from the board.
    let cap_before = w.traders().iter().find(|t| t.id == tid).unwrap().capital;
    w.queue_command(Command::FulfillContract { player, trader: tid, contract: cid });
    w.tick();

    let t = w.traders().iter().find(|t| t.id == tid).unwrap();
    assert_eq!(t.capital, cap_before + reward, "the reward is paid on delivery");
    assert_eq!(t.cargo.get(&food).copied().unwrap_or(0), 0, "the cargo is consumed");
    assert!(
        w.contracts().iter().all(|c| c.id != cid),
        "a fulfilled contract leaves the board"
    );
}

#[test]
fn contract_acceptance_is_validated() {
    let reg = two_system_registry();
    let pricing = builtin_pricing();
    let mut w = World::new(reg.clone(), &scenario_contracts(1000), 1, &pricing).unwrap();
    let player = PlayerId(0);
    let ship = reg.ship_id("t:freighter").unwrap();
    let asys = reg.system_id("t:a").unwrap();

    w.queue_command(Command::Spawn { player, ship, at: asys, capital: 1_000_000 });
    w.run(20);
    let tid = player_trader_id(&w, player);
    let cid = w.contracts().iter().find(|c| c.is_open()).expect("open contract").id;

    // Unknown contract id.
    w.queue_command(Command::AcceptContract { player, trader: tid, contract: ContractId(99_999) });
    w.tick();
    assert_eq!(w.last_command_errors().first(), Some(&CommandError::UnknownContract));

    // Accepting the real one succeeds.
    w.queue_command(Command::AcceptContract { player, trader: tid, contract: cid });
    w.tick();
    assert!(w.last_command_errors().is_empty(), "a valid acceptance is not rejected");

    // Accepting an already-accepted contract fails.
    w.queue_command(Command::AcceptContract { player, trader: tid, contract: cid });
    w.tick();
    assert_eq!(w.last_command_errors().first(), Some(&CommandError::ContractUnavailable));
}

#[test]
fn contract_fulfilment_is_validated() {
    let reg = two_system_registry();
    let pricing = builtin_pricing();
    let mut w = World::new(reg.clone(), &scenario_contracts(1000), 1, &pricing).unwrap();
    let player = PlayerId(0);
    let ship = reg.ship_id("t:freighter").unwrap();
    let asys = reg.system_id("t:a").unwrap();
    let bsys = reg.system_id("t:b").unwrap();

    // Two player traders at A.
    w.queue_command(Command::Spawn { player, ship, at: asys, capital: 1_000_000 });
    w.queue_command(Command::Spawn { player, ship, at: asys, capital: 1_000_000 });
    w.run(20);
    let ids: Vec<TraderId> = w.traders().iter().map(|t| t.id).collect();
    let (t1, t2) = (ids[0], ids[1]);
    let cid = w.contracts().iter().find(|c| c.is_open()).expect("open contract").id;

    // t1 accepts the contract.
    w.queue_command(Command::AcceptContract { player, trader: t1, contract: cid });
    w.tick();

    // A non-holder trader cannot deliver it.
    w.queue_command(Command::FulfillContract { player, trader: t2, contract: cid });
    w.tick();
    assert_eq!(w.last_command_errors().first(), Some(&CommandError::ContractNotHeld));

    // The holder, but at the wrong system (still docked at A).
    w.queue_command(Command::FulfillContract { player, trader: t1, contract: cid });
    w.tick();
    assert_eq!(w.last_command_errors().first(), Some(&CommandError::WrongDestination));

    // At the destination but without the cargo.
    w.queue_command(Command::Jump { player, trader: t1, dest: bsys });
    w.run(5);
    assert!(matches!(
        w.traders().iter().find(|t| t.id == t1).unwrap().location,
        TraderLocation::Docked(s) if s == bsys
    ));
    w.queue_command(Command::FulfillContract { player, trader: t1, contract: cid });
    w.tick();
    assert_eq!(w.last_command_errors().first(), Some(&CommandError::InsufficientCargo));
}

#[test]
fn contracts_expire_and_reject_late_delivery() {
    let reg = two_system_registry();
    let pricing = builtin_pricing();
    // Short deadline so the contract lapses within the test.
    let mut w = World::new(reg.clone(), &scenario_contracts(5), 1, &pricing).unwrap();
    let player = PlayerId(0);
    let ship = reg.ship_id("t:freighter").unwrap();
    let asys = reg.system_id("t:a").unwrap();

    // Tick 0 posts a contract (B is already short by end of the first tick).
    w.tick();
    let contract = w.contracts().iter().find(|c| c.is_open()).expect("a posted contract").clone();
    let cid = contract.id;
    let deadline = contract.deadline.get();

    // Spawn and accept, but never deliver.
    w.queue_command(Command::Spawn { player, ship, at: asys, capital: 1_000_000 });
    w.tick();
    let tid = player_trader_id(&w, player);
    w.queue_command(Command::AcceptContract { player, trader: tid, contract: cid });
    w.tick();

    // Advance to exactly one tick past the deadline. The deadline tick is still
    // fulfillable; the tick after rejects late delivery, then sweeps the board.
    while w.tick_count().get() <= deadline {
        w.tick();
    }
    assert!(
        w.contracts().iter().any(|c| c.id == cid),
        "the lapsed contract is still on the board for its rejection tick"
    );
    w.queue_command(Command::FulfillContract { player, trader: tid, contract: cid });
    w.tick();
    assert_eq!(w.last_command_errors().first(), Some(&CommandError::ContractExpired));
    assert!(
        w.contracts().iter().all(|c| c.id != cid),
        "the expired contract is swept from the board"
    );
}

// --- Courier contracts ---

/// A courier-only board over the safe two-system galaxy: delivery is disabled by
/// an unreachable shortfall threshold, bounties by a zero target.
fn scenario_courier() -> ScenarioDef {
    ScenarioDef {
        name: "courier".into(),
        seed: 1,
        ticks: 0,
        traders: TraderSpawn { count: 0, ship: "t:freighter".into(), starting_capital: 0 },
        piracy: None,
        risk_aversion: 0.0,
        escort: None,
        navy: None,
        contract: Some(ContractConfig {
            max_open: 2,
            generation_interval: 5,
            deadline_ticks: 1000,
            reward_factor: 1.5,
            min_shortfall: 1_000_000, // no delivery contracts
            max_quantity: 20,
            bounty_target: 0, // no bounties
            bounty_reward: 0,
            courier_reward: 500,
        }),
        loan: None,
        insurance: None,
        future: None,
    }
}

#[test]
fn courier_contracts_are_generated_and_fulfilled_on_arrival() {
    let reg = two_system_registry();
    let pricing = builtin_pricing();
    let mut w = World::new(reg.clone(), &scenario_courier(), 1, &pricing).unwrap();

    // Tick 0 posts a courier run between two connected systems.
    w.tick();
    let contract = w
        .contracts()
        .iter()
        .find(|c| c.is_open())
        .expect("a courier contract")
        .clone();
    assert!(matches!(contract.kind, ContractKind::Courier), "kind is courier");
    assert_eq!(contract.cargo(), None, "a courier carries no goods");
    assert_ne!(contract.origin, contract.destination);
    let cid = contract.id;

    // Spawn at the courier's origin and take the job.
    let player = PlayerId(0);
    let ship = reg.ship_id("t:freighter").unwrap();
    w.queue_command(Command::Spawn { player, ship, at: contract.origin, capital: 1000 });
    w.tick();
    let tid = player_trader_id(&w, player);
    w.queue_command(Command::AcceptContract { player, trader: tid, contract: cid });
    w.tick();

    // At the origin, not the destination: cannot complete yet.
    w.queue_command(Command::FulfillContract { player, trader: tid, contract: cid });
    w.tick();
    assert_eq!(w.last_command_errors().first(), Some(&CommandError::WrongDestination));

    // Travel to the destination and complete — no cargo required.
    w.queue_command(Command::Jump { player, trader: tid, dest: contract.destination });
    w.run(5);
    assert!(matches!(
        w.traders().iter().find(|t| t.id == tid).unwrap().location,
        TraderLocation::Docked(s) if s == contract.destination
    ));
    let cap_before = w.traders().iter().find(|t| t.id == tid).unwrap().capital;
    w.queue_command(Command::FulfillContract { player, trader: tid, contract: cid });
    w.tick();
    assert!(w.last_command_errors().is_empty(), "a courier completes on arrival");
    let t = w.traders().iter().find(|t| t.id == tid).unwrap();
    assert_eq!(t.capital, cap_before + contract.reward, "the courier reward is paid");
    assert!(w.contracts().iter().all(|c| c.id != cid), "the courier leaves the board");
}

// --- Bounty contracts ---

/// A two-system galaxy with a lawless destination (Bport, danger 1) where pirates
/// spawn, reachable from a safe source (Aport). The player flies an overwhelming
/// warship and the pirates are unarmed paper targets, so an ambush is a guaranteed,
/// deterministic win — enough to drive a bounty to completion.
fn dangerous_reg() -> Arc<Registry> {
    let merged = MergedContent {
        scripts: vec![],
        commodities: vec![CommodityDef {
            id: "t:food".into(),
            name: "Food".into(),
            base_price: 100,
            unit_mass: 1,
            elasticity: 1.0,
            category: "food".into(),
        }],
        recipes: vec![
            ProductionRecipe {
                id: "t:grow".into(),
                inputs: vec![],
                outputs: vec![CommodityAmount { commodity: "t:food".into(), qty: 10 }],
                rate: 1,
                elasticity: 0.0,
            },
            ProductionRecipe {
                id: "t:eat".into(),
                inputs: vec![CommodityAmount { commodity: "t:food".into(), qty: 1 }],
                outputs: vec![],
                rate: 1,
                elasticity: 0.0,
            },
        ],
        systems: vec![
            SystemDef {
                id: "t:a".into(),
                name: "Aport".into(),
                position: [0.0, 0.0],
                industries: vec!["t:grow".into()],
                connections: vec!["t:b".into()],
                initial_stock: vec![CommodityAmount { commodity: "t:food".into(), qty: 500 }],
                pricing: "supply_demand_v1".into(),
                danger: 0.0,
            },
            SystemDef {
                id: "t:b".into(),
                name: "Bport".into(),
                position: [1.0, 0.0],
                industries: vec!["t:eat".into()],
                connections: vec!["t:a".into()],
                initial_stock: vec![CommodityAmount { commodity: "t:food".into(), qty: 500 }],
                pricing: "supply_demand_v1".into(),
                danger: 1.0,
            },
        ],
        ships: vec![
            ShipDef {
                id: "t:warship".into(),
                name: "Warship".into(),
                cargo_capacity: 1000,
                jump_speed: 100.0,
                hull: 100_000,
                max_speed: 1000.0,
                combat: Some(CombatStats {
                    shield: 10_000,
                    shield_regen: 1000.0,
                    weapon_damage: 100_000,
                    weapon_range: 10_000.0,
                    weapon_cooldown: 1,
                    accuracy: 1.0,
                    acceleration: 1000.0,
                }),
                visual: None,
            },
            ShipDef {
                id: "t:pirate".into(),
                name: "Paper Pirate".into(),
                cargo_capacity: 1,
                jump_speed: 100.0,
                hull: 1,
                max_speed: 1.0,
                combat: None, // unarmed: deals no damage,
                visual: None,
            },
        ],
    };
    Arc::new(link(merged, &pricing_names()).expect("dangerous registry links"))
}

fn scenario_bounty() -> ScenarioDef {
    ScenarioDef {
        name: "bounty".into(),
        seed: 1,
        ticks: 0,
        traders: TraderSpawn { count: 0, ship: "t:warship".into(), starting_capital: 0 },
        piracy: Some(PiracyConfig {
            pirate_ship: "t:pirate".into(),
            base_ambush_chance: 1.0, // laden trips into Bport are always ambushed
            max_pirates: 1,
            respawn_delay: 1000,
            fleet_size: 5,
            bounty: 100,
            reinforce_interval: 1000,
        }),
        risk_aversion: 0.0,
        escort: None,
        navy: None,
        contract: Some(ContractConfig {
            max_open: 3,
            generation_interval: 10,
            deadline_ticks: 1000,
            reward_factor: 1.0,
            min_shortfall: 1_000_000, // no delivery contracts
            max_quantity: 20,
            bounty_target: 1,
            bounty_reward: 5000,
            courier_reward: 0, // no couriers
        }),
        loan: None,
        insurance: None,
        future: None,
    }
}

#[test]
fn bounty_is_posted_at_the_lawless_system() {
    let reg = dangerous_reg();
    let pricing = builtin_pricing();
    let mut w = World::new(reg.clone(), &scenario_bounty(), 1, &pricing).unwrap();
    w.tick();
    let bounty = w
        .contracts()
        .iter()
        .find(|c| matches!(c.kind, ContractKind::Bounty { .. }))
        .expect("a bounty contract");
    assert!(
        reg.system(bounty.destination).danger > 0.0,
        "bounties are posted where pirates operate"
    );
    assert!(matches!(bounty.kind, ContractKind::Bounty { target: 1, progress: 0 }));
}

#[test]
fn bounty_without_kills_is_rejected() {
    let reg = dangerous_reg();
    let pricing = builtin_pricing();
    let mut w = World::new(reg.clone(), &scenario_bounty(), 1, &pricing).unwrap();
    w.tick();
    let cid = w
        .contracts()
        .iter()
        .find(|c| matches!(c.kind, ContractKind::Bounty { .. }))
        .unwrap()
        .id;
    let bsys = reg.system_id("t:b").unwrap();

    // Dock at the claim station (safe while docked) and accept, but with no kills.
    let player = PlayerId(0);
    let ship = reg.ship_id("t:warship").unwrap();
    w.queue_command(Command::Spawn { player, ship, at: bsys, capital: 1000 });
    w.tick();
    let tid = player_trader_id(&w, player);
    w.queue_command(Command::AcceptContract { player, trader: tid, contract: cid });
    w.tick();
    w.queue_command(Command::FulfillContract { player, trader: tid, contract: cid });
    w.tick();
    assert_eq!(w.last_command_errors().first(), Some(&CommandError::BountyIncomplete));
}

#[test]
fn bounty_contract_accrues_kills_and_pays_out() {
    let reg = dangerous_reg();
    let pricing = builtin_pricing();
    let mut w = World::new(reg.clone(), &scenario_bounty(), 1, &pricing).unwrap();

    // A bounty is posted at the lawless system.
    w.tick();
    let bounty = w
        .contracts()
        .iter()
        .find(|c| matches!(c.kind, ContractKind::Bounty { .. }))
        .expect("a bounty contract")
        .clone();
    let cid = bounty.id;
    let bsys = reg.system_id("t:b").unwrap();
    let asys = reg.system_id("t:a").unwrap();
    let food = reg.commodity_id("t:food").unwrap();
    assert_eq!(bounty.destination, bsys);

    // Warship spawns at the safe source, takes the bounty, and loads cargo (so it
    // reads as prey and draws an ambush on the run into lawless space).
    let player = PlayerId(0);
    let ship = reg.ship_id("t:warship").unwrap();
    w.queue_command(Command::Spawn { player, ship, at: asys, capital: 1_000_000 });
    w.tick();
    let tid = player_trader_id(&w, player);
    w.queue_command(Command::AcceptContract { player, trader: tid, contract: cid });
    w.tick();
    w.queue_command(Command::Buy { player, trader: tid, commodity: food, qty: 5 });
    w.tick();

    // Run laden into Bport: the guaranteed ambush is won and a kill accrues.
    w.queue_command(Command::Jump { player, trader: tid, dest: bsys });
    for _ in 0..50 {
        let docked = matches!(
            w.traders().iter().find(|t| t.id == tid).map(|t| t.location.clone()),
            Some(TraderLocation::Docked(s)) if s == bsys
        );
        if docked {
            break;
        }
        w.tick();
    }
    let held = w.contracts().iter().find(|c| c.id == cid).expect("bounty still held");
    assert!(
        matches!(held.kind, ContractKind::Bounty { progress, .. } if progress >= 1),
        "an ambush win should credit a kill, got {:?}",
        held.kind
    );

    // Claim the completed bounty at its station.
    let cap_before = w.traders().iter().find(|t| t.id == tid).unwrap().capital;
    w.queue_command(Command::FulfillContract { player, trader: tid, contract: cid });
    w.tick();
    assert!(w.last_command_errors().is_empty(), "the met bounty pays out: {:?}", w.last_command_errors());
    let t = w.traders().iter().find(|t| t.id == tid).unwrap();
    assert_eq!(t.capital, cap_before + bounty.reward);
    assert!(w.contracts().iter().all(|c| c.id != cid), "the claimed bounty leaves the board");
}

// ---------------------------------------------------------------------------
// H: loans — the first financial instrument.
// ---------------------------------------------------------------------------

/// A lending-enabled run over the safe two-system galaxy.
fn scenario_loans(rate_bps: u32, accrual_interval: u64, term_ticks: u64) -> ScenarioDef {
    ScenarioDef {
        name: "loans".into(),
        seed: 1,
        ticks: 0,
        traders: TraderSpawn { count: 0, ship: "t:freighter".into(), starting_capital: 0 },
        piracy: None,
        risk_aversion: 0.0,
        escort: None,
        navy: None,
        contract: None,
        loan: Some(LoanConfig {
            rate_bps,
            accrual_interval,
            term_ticks,
            max_principal: 10_000,
            max_loans: 2,
        }),
        insurance: None,
        future: None,
    }
}

/// Spawn a player-owned freighter docked at Aport and return its id.
fn spawn_at_a(reg: &Registry, w: &mut World, capital: i64) -> (PlayerId, TraderId) {
    let player = PlayerId(0);
    let ship = reg.ship_id("t:freighter").unwrap();
    let asys = reg.system_id("t:a").unwrap();
    w.queue_command(Command::Spawn { player, ship, at: asys, capital });
    w.tick();
    (player, player_trader_id(w, player))
}

#[test]
fn a_loan_credits_capital_and_is_repaid() {
    let reg = two_system_registry();
    let pricing = builtin_pricing();
    // Interest-free for a clean principal check.
    let mut w = World::new(reg.clone(), &scenario_loans(0, 100, 1000), 1, &pricing).unwrap();
    let (player, tid) = spawn_at_a(&reg, &mut w, 1000);

    w.queue_command(Command::TakeLoan { player, trader: tid, principal: 5000 });
    w.tick();
    assert_eq!(w.loans().len(), 1, "the loan is on the book");
    let loan_id = w.loans()[0].id;
    assert_eq!(w.loans()[0].outstanding, 5000);
    assert_eq!(
        w.traders().iter().find(|t| t.id == tid).unwrap().capital,
        6000,
        "the principal is credited to capital"
    );

    // Repay in full: the loan closes and capital returns to the pre-loan level.
    w.queue_command(Command::RepayLoan { player, trader: tid, loan: loan_id, amount: 5000 });
    w.tick();
    assert!(w.loans().is_empty(), "a fully repaid loan closes");
    assert_eq!(w.traders().iter().find(|t| t.id == tid).unwrap().capital, 1000);
}

#[test]
fn loan_interest_compounds_each_period() {
    let reg = two_system_registry();
    let pricing = builtin_pricing();
    // 10% every 10 ticks.
    let mut w = World::new(reg.clone(), &scenario_loans(1000, 10, 1000), 1, &pricing).unwrap();
    let (player, tid) = spawn_at_a(&reg, &mut w, 0);

    w.queue_command(Command::TakeLoan { player, trader: tid, principal: 1000 });
    w.tick(); // loan created at tick 1
    assert_eq!(w.loans()[0].outstanding, 1000);

    // Two accrual boundaries fall within the next stretch (ticks 10 and 20).
    w.run(25);
    assert_eq!(
        w.loans()[0].outstanding,
        1210,
        "1000 -> 1100 -> 1210 over two 10% periods"
    );
}

#[test]
fn overdue_loan_is_called_and_seized() {
    let reg = two_system_registry();
    let pricing = builtin_pricing();
    // Short term, interest-free, so the seizure equals the principal.
    let mut w = World::new(reg.clone(), &scenario_loans(0, 1000, 5), 1, &pricing).unwrap();
    let (player, tid) = spawn_at_a(&reg, &mut w, 1000);

    w.queue_command(Command::TakeLoan { player, trader: tid, principal: 800 });
    w.tick();
    assert_eq!(w.traders().iter().find(|t| t.id == tid).unwrap().capital, 1800);
    let due = w.loans()[0].due.get();

    // Advance to just past the due tick; the loan is called and the balance seized.
    while w.tick_count().get() <= due {
        w.tick();
    }
    w.tick(); // now > due: the call fires this tick
    assert!(w.loans().is_empty(), "the overdue loan is called");
    assert_eq!(
        w.traders().iter().find(|t| t.id == tid).unwrap().capital,
        1000,
        "the outstanding balance is seized from capital"
    );
}

#[test]
fn loan_commands_are_validated() {
    let reg = two_system_registry();
    let pricing = builtin_pricing();
    let mut w = World::new(reg.clone(), &scenario_loans(0, 100, 1000), 1, &pricing).unwrap();
    let (player, tid) = spawn_at_a(&reg, &mut w, 1000);
    let bsys = reg.system_id("t:b").unwrap();

    // Principal above the cap.
    w.queue_command(Command::TakeLoan { player, trader: tid, principal: 20_000 });
    w.tick();
    assert_eq!(w.last_command_errors().first(), Some(&CommandError::LoanTooLarge));

    // Repaying a loan that does not exist.
    w.queue_command(Command::RepayLoan {
        player,
        trader: tid,
        loan: drift_economy::LoanId(999),
        amount: 1,
    });
    w.tick();
    assert_eq!(w.last_command_errors().first(), Some(&CommandError::UnknownLoan));

    // Two loans is the cap; a third is rejected.
    w.queue_command(Command::TakeLoan { player, trader: tid, principal: 100 });
    w.queue_command(Command::TakeLoan { player, trader: tid, principal: 100 });
    w.tick();
    assert_eq!(w.loans().len(), 2);
    w.queue_command(Command::TakeLoan { player, trader: tid, principal: 100 });
    w.tick();
    assert_eq!(w.last_command_errors().first(), Some(&CommandError::TooManyLoans));

    // Borrowing is a docked-only action: in transit it is rejected.
    w.queue_command(Command::Jump { player, trader: tid, dest: bsys });
    w.queue_command(Command::TakeLoan { player, trader: tid, principal: 100 });
    w.tick();
    assert!(
        w.last_command_errors().contains(&CommandError::NotDocked),
        "cannot borrow while in transit"
    );
}

#[test]
fn lending_unavailable_without_a_loan_config() {
    let reg = two_system_registry();
    let pricing = builtin_pricing();
    let mut w = World::new(reg.clone(), &scenario(0, "t:freighter", 0), 1, &pricing).unwrap();
    let (player, tid) = spawn_at_a(&reg, &mut w, 100);

    w.queue_command(Command::TakeLoan { player, trader: tid, principal: 50 });
    w.tick();
    assert_eq!(w.last_command_errors().first(), Some(&CommandError::LendingUnavailable));
}

// ---------------------------------------------------------------------------
// I: insurance and futures.
// ---------------------------------------------------------------------------

/// A galaxy where a lightly-built player hauler is prey: Aport (safe) feeds Bport
/// (lawless, danger 1) where armed raiders spawn. An unarmed hauler running laden
/// into Bport is destroyed deterministically.
fn deadly_reg() -> Arc<Registry> {
    let merged = MergedContent {
        scripts: vec![],
        commodities: vec![CommodityDef {
            id: "t:food".into(),
            name: "Food".into(),
            base_price: 100,
            unit_mass: 1,
            elasticity: 1.0,
            category: "food".into(),
        }],
        recipes: vec![
            ProductionRecipe {
                id: "t:grow".into(),
                inputs: vec![],
                outputs: vec![CommodityAmount { commodity: "t:food".into(), qty: 10 }],
                rate: 1,
                elasticity: 0.0,
            },
            ProductionRecipe {
                id: "t:eat".into(),
                inputs: vec![CommodityAmount { commodity: "t:food".into(), qty: 1 }],
                outputs: vec![],
                rate: 1,
                elasticity: 0.0,
            },
        ],
        systems: vec![
            SystemDef {
                id: "t:a".into(),
                name: "Aport".into(),
                position: [0.0, 0.0],
                industries: vec!["t:grow".into()],
                connections: vec!["t:b".into()],
                initial_stock: vec![CommodityAmount { commodity: "t:food".into(), qty: 500 }],
                pricing: "supply_demand_v1".into(),
                danger: 0.0,
            },
            SystemDef {
                id: "t:b".into(),
                name: "Bport".into(),
                position: [1.0, 0.0],
                industries: vec!["t:eat".into()],
                connections: vec!["t:a".into()],
                initial_stock: vec![CommodityAmount { commodity: "t:food".into(), qty: 500 }],
                pricing: "supply_demand_v1".into(),
                danger: 1.0,
            },
        ],
        ships: vec![
            ShipDef {
                id: "t:hauler".into(),
                name: "Hauler".into(),
                cargo_capacity: 100,
                jump_speed: 100.0,
                hull: 10,
                max_speed: 10.0,
                combat: None, // unarmed prey,
                visual: None,
            },
            ShipDef {
                id: "t:raider".into(),
                name: "Raider".into(),
                cargo_capacity: 1,
                jump_speed: 100.0,
                hull: 100,
                max_speed: 100.0,
                combat: Some(CombatStats {
                    shield: 100,
                    shield_regen: 10.0,
                    weapon_damage: 1000,
                    weapon_range: 10_000.0,
                    weapon_cooldown: 1,
                    accuracy: 1.0,
                    acceleration: 100.0,
                }),
                visual: None,
            },
        ],
    };
    Arc::new(link(merged, &pricing_names()).expect("deadly registry links"))
}

fn scenario_insurance() -> ScenarioDef {
    ScenarioDef {
        name: "insurance".into(),
        seed: 1,
        ticks: 0,
        traders: TraderSpawn { count: 0, ship: "t:hauler".into(), starting_capital: 0 },
        piracy: Some(PiracyConfig {
            pirate_ship: "t:raider".into(),
            base_ambush_chance: 1.0,
            max_pirates: 1,
            respawn_delay: 1000,
            fleet_size: 5,
            bounty: 100,
            reinforce_interval: 1000,
        }),
        risk_aversion: 0.0,
        escort: None,
        navy: None,
        contract: None,
        loan: None,
        insurance: Some(drift_data::InsuranceConfig { premium: 500, payout: 4000, term_ticks: 1000 }),
        future: None,
    }
}

#[test]
fn insurance_pays_out_on_a_covered_loss() {
    let reg = deadly_reg();
    let pricing = builtin_pricing();
    let mut w = World::new(reg.clone(), &scenario_insurance(), 1, &pricing).unwrap();
    let player = PlayerId(0);
    let ship = reg.ship_id("t:hauler").unwrap();
    let asys = reg.system_id("t:a").unwrap();
    let bsys = reg.system_id("t:b").unwrap();
    let food = reg.commodity_id("t:food").unwrap();

    w.queue_command(Command::Spawn { player, ship, at: asys, capital: 10_000 });
    w.tick();
    let tid = player_trader_id(&w, player);

    // Insure the hauler; the premium is charged up front.
    w.queue_command(Command::BuyInsurance { player, trader: tid });
    w.tick();
    assert_eq!(w.policies().len(), 1);
    assert_eq!(
        w.traders().iter().find(|t| t.id == tid).unwrap().capital,
        10_000 - 500
    );

    // Load cargo, then run laden into the ambush and be destroyed.
    w.queue_command(Command::Buy { player, trader: tid, commodity: food, qty: 5 });
    w.tick();
    let cap_before_loss = w.traders().iter().find(|t| t.id == tid).unwrap().capital;
    w.queue_command(Command::Jump { player, trader: tid, dest: bsys });
    w.run(3);

    let t = w.traders().iter().find(|t| t.id == tid).unwrap();
    assert!(matches!(t.location, TraderLocation::Destroyed { .. }), "the hauler is destroyed");
    assert_eq!(t.capital, cap_before_loss + 4000, "insurance pays the payout");
    assert!(w.policies().is_empty(), "the policy is consumed by the payout");
}

#[test]
fn insurance_commands_are_validated() {
    let reg = deadly_reg();
    let pricing = builtin_pricing();

    // No insurance configured -> unavailable.
    let mut w0 = World::new(reg.clone(), &scenario(0, "t:hauler", 0), 1, &pricing).unwrap();
    let ship = reg.ship_id("t:hauler").unwrap();
    let asys = reg.system_id("t:a").unwrap();
    let player = PlayerId(0);
    w0.queue_command(Command::Spawn { player, ship, at: asys, capital: 1000 });
    w0.tick();
    let tid0 = player_trader_id(&w0, player);
    w0.queue_command(Command::BuyInsurance { player, trader: tid0 });
    w0.tick();
    assert_eq!(w0.last_command_errors().first(), Some(&CommandError::InsuranceUnavailable));

    // With insurance: double-buy is rejected, and too-poor cannot afford the premium.
    let mut w = World::new(reg.clone(), &scenario_insurance(), 1, &pricing).unwrap();
    w.queue_command(Command::Spawn { player, ship, at: asys, capital: 600 });
    w.tick();
    let tid = player_trader_id(&w, player);
    w.queue_command(Command::BuyInsurance { player, trader: tid });
    w.tick();
    assert!(w.last_command_errors().is_empty(), "first policy is bought");
    w.queue_command(Command::BuyInsurance { player, trader: tid });
    w.tick();
    assert_eq!(w.last_command_errors().first(), Some(&CommandError::AlreadyInsured));
}

/// A single-system, industry-free galaxy: with no production, consumption, or NPC
/// traders, the reference price never moves — so a future settles flat.
fn static_reg() -> Arc<Registry> {
    let merged = MergedContent {
        scripts: vec![],
        commodities: vec![CommodityDef {
            id: "t:widget".into(),
            name: "Widget".into(),
            base_price: 100,
            unit_mass: 1,
            elasticity: 1.0,
            category: "misc".into(),
        }],
        recipes: vec![],
        systems: vec![SystemDef {
            id: "t:hub".into(),
            name: "Hub".into(),
            position: [0.0, 0.0],
            industries: vec![],
            connections: vec![],
            initial_stock: vec![CommodityAmount { commodity: "t:widget".into(), qty: 500 }],
            pricing: "supply_demand_v1".into(),
            danger: 0.0,
        }],
        ships: vec![ShipDef {
            id: "t:freighter".into(),
            name: "Freighter".into(),
            cargo_capacity: 100,
            jump_speed: 100.0,
            hull: 100,
            max_speed: 100.0,
            combat: None,
            visual: None,
        }],
    };
    Arc::new(link(merged, &pricing_names()).expect("static registry links"))
}

fn scenario_futures() -> ScenarioDef {
    ScenarioDef {
        name: "futures".into(),
        seed: 1,
        ticks: 0,
        traders: TraderSpawn { count: 0, ship: "t:freighter".into(), starting_capital: 0 },
        piracy: None,
        risk_aversion: 0.0,
        escort: None,
        navy: None,
        contract: None,
        loan: None,
        insurance: None,
        future: Some(drift_data::FutureConfig {
            fee: 100,
            term_ticks: 20,
            max_quantity: 50,
            max_futures: 2,
        }),
    }
}

#[test]
fn a_future_settles_flat_in_a_static_market() {
    let reg = static_reg();
    let pricing = builtin_pricing();
    let mut w = World::new(reg.clone(), &scenario_futures(), 1, &pricing).unwrap();
    let player = PlayerId(0);
    let ship = reg.ship_id("t:freighter").unwrap();
    let hub = reg.system_id("t:hub").unwrap();
    let widget = reg.commodity_id("t:widget").unwrap();

    w.queue_command(Command::Spawn { player, ship, at: hub, capital: 10_000 });
    w.tick();
    let tid = player_trader_id(&w, player);

    w.queue_command(Command::OpenFuture {
        player,
        trader: tid,
        commodity: widget,
        qty: 10,
        side: FutureSide::Long,
    });
    w.tick();
    assert_eq!(w.futures().len(), 1);
    let cap_after_fee = w.traders().iter().find(|t| t.id == tid).unwrap().capital;
    assert_eq!(cap_after_fee, 10_000 - 100, "the open fee is charged");
    let maturity = w.futures()[0].maturity.get();

    // Run to maturity; a static market settles the position to zero P&L.
    while w.tick_count().get() < maturity {
        w.tick();
    }
    w.tick();
    assert!(w.futures().is_empty(), "the matured position closes");
    assert_eq!(
        w.traders().iter().find(|t| t.id == tid).unwrap().capital,
        cap_after_fee,
        "a flat market settles to zero P&L"
    );
}

#[test]
fn future_commands_are_validated() {
    let reg = static_reg();
    let pricing = builtin_pricing();
    let widget = reg.commodity_id("t:widget").unwrap();
    let hub = reg.system_id("t:hub").unwrap();
    let ship = reg.ship_id("t:freighter").unwrap();
    let player = PlayerId(0);

    // No futures market -> unavailable.
    let mut w0 = World::new(reg.clone(), &scenario(0, "t:freighter", 0), 1, &pricing).unwrap();
    w0.queue_command(Command::Spawn { player, ship, at: hub, capital: 10_000 });
    w0.tick();
    let tid0 = player_trader_id(&w0, player);
    w0.queue_command(Command::OpenFuture {
        player,
        trader: tid0,
        commodity: widget,
        qty: 1,
        side: FutureSide::Long,
    });
    w0.tick();
    assert_eq!(w0.last_command_errors().first(), Some(&CommandError::FuturesUnavailable));

    // With a market: over-size is rejected, and the per-trader cap is enforced.
    let mut w = World::new(reg.clone(), &scenario_futures(), 1, &pricing).unwrap();
    w.queue_command(Command::Spawn { player, ship, at: hub, capital: 10_000 });
    w.tick();
    let tid = player_trader_id(&w, player);

    w.queue_command(Command::OpenFuture {
        player,
        trader: tid,
        commodity: widget,
        qty: 999,
        side: FutureSide::Long,
    });
    w.tick();
    assert_eq!(w.last_command_errors().first(), Some(&CommandError::FutureTooLarge));

    // Cap is 2 positions.
    for _ in 0..2 {
        w.queue_command(Command::OpenFuture {
            player,
            trader: tid,
            commodity: widget,
            qty: 5,
            side: FutureSide::Short,
        });
        w.tick();
    }
    assert_eq!(w.futures().len(), 2);
    w.queue_command(Command::OpenFuture {
        player,
        trader: tid,
        commodity: widget,
        qty: 5,
        side: FutureSide::Short,
    });
    w.tick();
    assert_eq!(w.last_command_errors().first(), Some(&CommandError::TooManyFutures));
}

// ---------------------------------------------------------------------------
// J: multi-tick running battles.
// ---------------------------------------------------------------------------

/// A galaxy built for a *slow* fight: both the player's brawler and the pirates
/// deal little damage per shot against thick hull/shield, so an ambush takes many
/// combat steps — i.e. it spans several economy ticks rather than resolving at
/// once. Aport (safe) feeds Bport (lawless).
fn grind_reg() -> Arc<Registry> {
    let brawler = |id: &str, name: &str| ShipDef {
        id: id.into(),
        name: name.into(),
        cargo_capacity: 100,
        jump_speed: 100.0,
        hull: 300,
        max_speed: 50.0,
        combat: Some(CombatStats {
            shield: 100,
            shield_regen: 0.0,
            weapon_damage: 2, // tiny bites -> long fight
            weapon_range: 60.0,
            weapon_cooldown: 2,
            accuracy: 0.8,
            acceleration: 40.0,
        }),
        visual: None,
    };
    let merged = MergedContent {
        scripts: vec![],
        commodities: vec![CommodityDef {
            id: "t:food".into(),
            name: "Food".into(),
            base_price: 100,
            unit_mass: 1,
            elasticity: 1.0,
            category: "food".into(),
        }],
        recipes: vec![
            ProductionRecipe {
                id: "t:grow".into(),
                inputs: vec![],
                outputs: vec![CommodityAmount { commodity: "t:food".into(), qty: 10 }],
                rate: 1,
                elasticity: 0.0,
            },
            ProductionRecipe {
                id: "t:eat".into(),
                inputs: vec![CommodityAmount { commodity: "t:food".into(), qty: 1 }],
                outputs: vec![],
                rate: 1,
                elasticity: 0.0,
            },
        ],
        systems: vec![
            SystemDef {
                id: "t:a".into(),
                name: "Aport".into(),
                position: [0.0, 0.0],
                industries: vec!["t:grow".into()],
                connections: vec!["t:b".into()],
                initial_stock: vec![CommodityAmount { commodity: "t:food".into(), qty: 500 }],
                pricing: "supply_demand_v1".into(),
                danger: 0.0,
            },
            SystemDef {
                id: "t:b".into(),
                name: "Bport".into(),
                position: [1.0, 0.0],
                industries: vec!["t:eat".into()],
                connections: vec!["t:a".into()],
                initial_stock: vec![CommodityAmount { commodity: "t:food".into(), qty: 500 }],
                pricing: "supply_demand_v1".into(),
                danger: 1.0,
            },
        ],
        ships: vec![brawler("t:brawler", "Brawler"), brawler("t:raider", "Raider")],
    };
    Arc::new(link(merged, &pricing_names()).expect("grind registry links"))
}

fn scenario_grind() -> ScenarioDef {
    ScenarioDef {
        name: "grind".into(),
        seed: 1,
        ticks: 0,
        traders: TraderSpawn { count: 0, ship: "t:brawler".into(), starting_capital: 0 },
        piracy: Some(PiracyConfig {
            pirate_ship: "t:raider".into(),
            base_ambush_chance: 1.0,
            max_pirates: 1,
            respawn_delay: 1000,
            fleet_size: 4,
            bounty: 100,
            reinforce_interval: 1000,
        }),
        risk_aversion: 0.0,
        escort: None,
        navy: None,
        contract: None,
        loan: None,
        insurance: None,
        future: None,
    }
}

#[test]
fn a_battle_spans_several_ticks_and_freezes_the_trader() {
    let reg = grind_reg();
    let pricing = builtin_pricing();
    let mut w = World::new(reg.clone(), &scenario_grind(), 1, &pricing).unwrap();
    let player = PlayerId(0);
    let ship = reg.ship_id("t:brawler").unwrap();
    let asys = reg.system_id("t:a").unwrap();
    let bsys = reg.system_id("t:b").unwrap();
    let food = reg.commodity_id("t:food").unwrap();

    w.queue_command(Command::Spawn { player, ship, at: asys, capital: 1_000_000 });
    w.tick();
    let tid = player_trader_id(&w, player);
    w.queue_command(Command::Buy { player, trader: tid, commodity: food, qty: 5 });
    w.tick();

    // Fly laden into Bport: the ambush opens on the jump tick.
    w.queue_command(Command::Jump { player, trader: tid, dest: bsys });
    w.tick();
    assert_eq!(w.active_encounters(), 1, "the ambush opens as a running battle");

    // Advance several ticks: the battle is still live and the trader is frozen
    // in transit (not arrived, not respawned) the whole time.
    let mut ticks_in_battle = 0;
    for _ in 0..200 {
        if w.active_encounters() == 0 {
            break;
        }
        let loc = &w.traders().iter().find(|t| t.id == tid).unwrap().location;
        assert!(
            matches!(loc, TraderLocation::InTransit { .. }),
            "an engaged trader stays frozen in transit, not {loc:?}"
        );
        ticks_in_battle += 1;
        w.tick();
    }
    assert!(
        ticks_in_battle >= 2,
        "the fight should occupy several ticks, took {ticks_in_battle}"
    );
    assert_eq!(w.active_encounters(), 0, "the battle eventually completes");

    // After completion the trader has resolved one way or the other (arrived at B,
    // or destroyed) — it is no longer stuck mid-jump forever.
    let loc = w.traders().iter().find(|t| t.id == tid).unwrap().location.clone();
    assert!(
        matches!(loc, TraderLocation::Docked(_) | TraderLocation::Destroyed { .. }),
        "a settled trader is docked or destroyed, not {loc:?}"
    );
}

#[test]
fn a_lopsided_battle_still_resolves_and_pays_out() {
    // Sanity that outcomes still apply through the multi-tick path: a strong navy
    // grinds pirates down over a piracy run, and prices stay sane.
    let reg = Arc::new(load_and_link(&core_mods_path(), &pricing_names()).expect("core mod links"));
    let pricing = builtin_pricing();
    let scn = scenario_defended("core:cobra_mk3", None, Some(("core:navy", 6)));
    let mut w = World::new(reg.clone(), &scn, 42, &pricing).unwrap();
    w.run(2000);
    assert!(
        w.piracy_stats().pirates_suppressed > 0,
        "the navy still suppresses pirates through running battles"
    );
    for p in price_vector(&w) {
        assert!(p.is_finite() && p > 0.0, "prices stay bounded under multi-tick combat");
    }
}

// ---------------------------------------------------------------------------
// K: protection as a real economic cost (escort fees, navy funding).
// ---------------------------------------------------------------------------

#[test]
fn escort_fees_are_charged_on_jumps() {
    let reg = two_system_registry();
    let pricing = builtin_pricing();

    // Escorts with a fee: every jump costs the trader, so fees accrue.
    let mut scn = scenario(6, "t:freighter", 100_000);
    scn.escort = Some(EscortConfig { ship: "t:freighter".into(), count: 1, fee: 20 });
    let mut w = World::new(reg.clone(), &scn, 1, &pricing).unwrap();
    w.run(200);
    assert!(
        w.piracy_stats().escort_fees_paid > 0,
        "escorted traders pay for protection when they jump"
    );

    // Free escorts: nothing accrues (the old behavior).
    let mut scn0 = scenario(6, "t:freighter", 100_000);
    scn0.escort = Some(EscortConfig { ship: "t:freighter".into(), count: 1, fee: 0 });
    let mut w0 = World::new(reg.clone(), &scn0, 1, &pricing).unwrap();
    w0.run(200);
    assert_eq!(w0.piracy_stats().escort_fees_paid, 0);
}

#[test]
fn an_underfunded_navy_shrinks() {
    let reg = Arc::new(load_and_link(&core_mods_path(), &pricing_names()).expect("core mod links"));
    let pricing = builtin_pricing();

    // Piracy inflicts navy losses; with funding the fleet is topped back up, without
    // it the losses are permanent and the fleet dwindles.
    let run = |upkeep: i64, funding: i64| {
        let mut scn = scenario_defended("core:cobra_mk3", None, Some(("core:navy", 8)));
        scn.navy = Some(NavyConfig {
            ship: "core:navy".into(),
            fleet_size: 8,
            reinforce_interval: 20,
            upkeep,
            funding,
        });
        let mut w = World::new(reg.clone(), &scn, 42, &pricing).unwrap();
        w.run(2000);
        (w.navy().len(), w.treasury())
    };

    let (funded, _) = run(1, 10_000);
    let (starved, deficit) = run(100, 0);
    assert!(funded > 0, "a funded navy is sustained near its target size");
    assert!(
        starved < funded,
        "an underfunded navy shrinks under attrition: starved={starved}, funded={funded}"
    );
    assert!(deficit < 0, "an unfunded navy runs the treasury into deficit, got {deficit}");
}

// ---------------------------------------------------------------------------
// L: mod scripting — a Rhai-authored pricing strategy through the name-seam.
// ---------------------------------------------------------------------------

#[test]
fn a_scripted_pricing_strategy_drives_the_market() {
    use drift_script::ScriptedPricing;

    // A mod-authored pricing strategy: price is always 2 x base, ignoring stock —
    // trivially distinct from the built-in supply/demand curve, so we can tell the
    // script really ran.
    let mut pricing = builtin_pricing();
    pricing.register_script(
        "mod:double",
        ScriptedPricing::compile("fn price(base, stock, equilibrium, elasticity) { base * 2 }")
            .unwrap(),
    );
    // The scripted name is now a valid `pricing` value for content validation.
    let known: HashSet<String> = pricing.names().map(String::from).collect();

    // A one-system galaxy whose market selects the scripted strategy by name.
    let merged = MergedContent {
        scripts: vec![],
        commodities: vec![CommodityDef {
            id: "t:widget".into(),
            name: "Widget".into(),
            base_price: 100,
            unit_mass: 1,
            elasticity: 1.0,
            category: "misc".into(),
        }],
        recipes: vec![],
        systems: vec![SystemDef {
            id: "t:hub".into(),
            name: "Hub".into(),
            position: [0.0, 0.0],
            industries: vec![],
            connections: vec![],
            initial_stock: vec![CommodityAmount { commodity: "t:widget".into(), qty: 500 }],
            pricing: "mod:double".into(),
            danger: 0.0,
        }],
        ships: vec![ShipDef {
            id: "t:freighter".into(),
            name: "Freighter".into(),
            cargo_capacity: 10,
            jump_speed: 1.0,
            hull: 1,
            max_speed: 1.0,
            combat: None,
            visual: None,
        }],
    };
    let reg = Arc::new(link(merged, &known).expect("links with the scripted strategy registered"));

    let mut w = World::new(reg.clone(), &scenario(0, "t:freighter", 0), 1, &pricing).unwrap();
    let widget = reg.commodity_id("t:widget").unwrap();

    // Initial price is the script's output: 2 * base = 200 (not the built-in curve,
    // which at equilibrium would be exactly base = 100).
    assert_eq!(w.markets()[0].price(widget).unwrap(), 200, "the script sets price = 2 * base");

    // The script ignores stock, so the price holds through repricing.
    w.run(50);
    assert_eq!(w.markets()[0].price(widget).unwrap(), 200);
}

// ---------------------------------------------------------------------------
// M: real-time combat outcomes reported back to the sim.
// ---------------------------------------------------------------------------

#[test]
fn reporting_a_pirate_kill_removes_it_and_pays_bounty() {
    let reg = Arc::new(load_and_link(&core_mods_path(), &pricing_names()).expect("core mod links"));
    let pricing = builtin_pricing();
    // Piracy present (so pirates exist) but ambush chance 0 and no NPC traders, so
    // nothing changes the fleet except our report.
    let scn = scenario_piracy(0, "core:cobra_mk3", "core:pirate", 0.0, 0.0);
    let mut w = World::new(reg.clone(), &scn, 42, &pricing).unwrap();

    let player = PlayerId(0);
    let ship = reg.ship_id("core:cobra_mk3").unwrap();
    let lave = reg.system_id("core:lave").unwrap();
    w.queue_command(Command::Spawn { player, ship, at: lave, capital: 1000 });
    w.tick();
    let tid = player_trader_id(&w, player);

    assert!(!w.pirates().is_empty(), "the piracy scenario spawns pirates");
    let pirate = w.pirates()[0].id;
    let before = w.pirates().len();
    let cap_before = w.traders().iter().find(|t| t.id == tid).unwrap().capital;

    w.queue_command(Command::DestroyedPirate { player, trader: tid, pirate });
    w.tick();

    assert_eq!(w.pirates().len(), before - 1, "the reported pirate is removed");
    assert!(w.pirates().iter().all(|p| p.id != pirate));
    assert_eq!(w.piracy_stats().pirates_destroyed, 1);
    assert_eq!(
        w.traders().iter().find(|t| t.id == tid).unwrap().capital,
        cap_before + 300, // scenario_piracy bounty
        "the bounty is paid to the player"
    );

    // A stale pirate id is rejected, not fatal.
    w.queue_command(Command::DestroyedPirate { player, trader: tid, pirate });
    w.tick();
    assert_eq!(w.last_command_errors().first(), Some(&CommandError::UnknownPatrol));
}

#[test]
fn reporting_player_death_destroys_the_trader_and_pays_insurance() {
    let reg = Arc::new(load_and_link(&core_mods_path(), &pricing_names()).expect("core mod links"));
    let pricing = builtin_pricing();
    let mut scn = scenario_piracy(0, "core:cobra_mk3", "core:pirate", 0.0, 0.0);
    scn.insurance = Some(drift_data::InsuranceConfig { premium: 500, payout: 4000, term_ticks: 1000 });
    let mut w = World::new(reg.clone(), &scn, 42, &pricing).unwrap();

    let player = PlayerId(0);
    let ship = reg.ship_id("core:cobra_mk3").unwrap();
    let lave = reg.system_id("core:lave").unwrap();
    let food = reg.commodity_id("core:food").unwrap();
    w.queue_command(Command::Spawn { player, ship, at: lave, capital: 10_000 });
    w.tick();
    let tid = player_trader_id(&w, player);
    w.queue_command(Command::BuyInsurance { player, trader: tid });
    w.queue_command(Command::Buy { player, trader: tid, commodity: food, qty: 5 });
    w.tick();
    assert_eq!(w.policies().len(), 1);
    let cap_before = w.traders().iter().find(|t| t.id == tid).unwrap().capital;

    w.queue_command(Command::TraderDestroyed { player, trader: tid });
    w.tick();

    let t = w.traders().iter().find(|t| t.id == tid).unwrap();
    assert!(matches!(t.location, TraderLocation::Destroyed { .. }), "the trader is destroyed");
    assert!(t.cargo.is_empty(), "cargo is lost on destruction");
    assert_eq!(w.piracy_stats().traders_lost, 1);
    assert_eq!(t.capital, cap_before + 4000, "insurance pays out on the loss");
    assert!(w.policies().is_empty(), "the policy is consumed");
}
