//! `drift` — headless driver for the simulation core.
//!
//! Three subcommands: `validate` (load + link mods, report errors), `run`
//! (advance the economy N ticks, optionally dump state), and `inspect` (run while
//! periodically printing average prices so convergence is observable).

mod play;

use std::io::{BufReader, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use drift_combat::{Combatant, Encounter, Outcome, Vec2};
use drift_core::{CommodityId, DetRng, SimContext, Step, Tick};
use drift_data::ScenarioDef;
use drift_economy::{Command as SimCommand, PlayerId, World};
use drift_mods::Registry;
use drift_sim::{load_registry, load_scenario, Session};

#[derive(Parser)]
#[command(name = "drift", about = "Headless economy simulation for the Drift space sim")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Load and link the mods, reporting any content errors.
    Validate {
        #[arg(long, default_value = "mods/")]
        mods: PathBuf,
    },
    /// Run the economy for a number of ticks.
    Run {
        #[arg(long, default_value = "mods/")]
        mods: PathBuf,
        #[arg(long, default_value = "scenarios/equilibrium.ron")]
        scenario: PathBuf,
        /// Override the scenario's tick count.
        #[arg(long)]
        ticks: Option<u64>,
        /// Override the scenario's seed.
        #[arg(long)]
        seed: Option<u64>,
        /// Write the final world state as JSON to this path.
        #[arg(long)]
        dump: Option<PathBuf>,
        /// Print the simulation event log (recent tail) after the run.
        #[arg(long)]
        log: bool,
        /// Stream events to stdout live, tick by tick, as the run proceeds
        /// (the full log, not just the recent tail).
        #[arg(long)]
        log_stream: bool,
    },
    /// Run the economy, printing average prices every N ticks.
    Inspect {
        #[arg(long, default_value = "mods/")]
        mods: PathBuf,
        #[arg(long, default_value = "scenarios/equilibrium.ron")]
        scenario: PathBuf,
        #[arg(long)]
        ticks: Option<u64>,
        #[arg(long)]
        seed: Option<u64>,
        /// Sampling interval, in ticks.
        #[arg(long, default_value_t = 100)]
        every: u64,
    },
    /// Stage a deterministic combat encounter between two squadrons.
    Battle {
        #[arg(long, default_value = "mods/")]
        mods: PathBuf,
        /// Ship id for faction 0.
        #[arg(long, default_value = "core:cobra_mk3")]
        ship: String,
        /// Ship id for faction 1 (defaults to the same as faction 0).
        #[arg(long)]
        vs: Option<String>,
        /// Number of ships per side.
        #[arg(long, default_value_t = 3)]
        per_side: u32,
        #[arg(long, default_value_t = 1)]
        seed: u64,
        #[arg(long, default_value_t = 2000)]
        max_ticks: u64,
    },
    /// Play the living galaxy as a trader (interactive).
    Play {
        #[arg(long, default_value = "mods/")]
        mods: PathBuf,
        #[arg(long, default_value = "scenarios/equilibrium.ron")]
        scenario: PathBuf,
        #[arg(long)]
        seed: Option<u64>,
        /// System to start docked at.
        #[arg(long, default_value = "core:lave")]
        start: String,
        /// Ship to fly.
        #[arg(long, default_value = "core:cobra_mk3")]
        ship: String,
        /// Starting capital in credits.
        #[arg(long, default_value_t = 1000)]
        capital: i64,
    },
}

/// Average price of each commodity across every market that trades it.
fn average_prices(world: &World) -> Vec<(CommodityId, f64)> {
    let reg = world.registry();
    let mut out = Vec::new();
    for (cid, _) in reg.commodities() {
        let mut sum = 0i64;
        let mut n = 0i64;
        for market in world.markets() {
            if let Some(p) = market.price(cid) {
                sum += p;
                n += 1;
            }
        }
        if n > 0 {
            out.push((cid, sum as f64 / n as f64));
        }
    }
    out
}

fn total_trader_capital(world: &World) -> i64 {
    world.traders().iter().map(|t| t.capital).sum()
}

/// Advance the world one tick at a time, streaming each tick's newly-emitted
/// events to stdout as they happen (the full log, unbounded by the ring buffer).
fn stream_run(session: &mut Session, ticks: u64) -> Result<()> {
    let mut out = std::io::stdout().lock();
    for _ in 0..ticks {
        let events = session.step();
        if !events.is_empty() {
            for e in events {
                writeln!(out, "[{:>6}] {:?}: {}", e.tick.get(), e.category, e.message)?;
            }
            out.flush()?;
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    match Cli::parse().command {
        Command::Validate { mods } => {
            let reg = load_registry(&mods)?;
            // Build a zero-trader session too, so economy-level validation (e.g.
            // industries referencing untraded commodities) runs. No ship needed.
            let probe = ScenarioDef {
                name: "validate-probe".into(),
                seed: 0,
                ticks: 0,
                traders: drift_data::TraderSpawn {
                    count: 0,
                    ship: String::new(),
                    starting_capital: 0,
                },
                piracy: None,
                risk_aversion: 0.0,
                escort: None,
                navy: None,
                contract: None,
                loan: None,
                insurance: None,
                future: None,
            };
            Session::new(reg.clone(), &probe, 0).context("economy validation")?;
            println!(
                "ok: {} commodities, {} recipes, {} systems linked",
                reg.commodity_count(),
                reg.recipe_count(),
                reg.system_count()
            );
        }

        Command::Run {
            mods,
            scenario,
            ticks,
            seed,
            dump,
            log,
            log_stream,
        } => {
            let reg = load_registry(&mods)?;
            let scn = load_scenario(&scenario)?;
            let seed = seed.unwrap_or(scn.seed);
            let ticks = ticks.unwrap_or(scn.ticks);
            let mut session = Session::new(reg.clone(), &scn, seed).context("building world")?;

            if log_stream {
                stream_run(&mut session, ticks)?;
            } else {
                session.run(ticks);
            }
            let world = session.world();

            println!("ran {ticks} ticks (seed {seed})");
            for (cid, avg) in average_prices(world) {
                println!("  {:<16} avg price {:>8.1}", reg.commodity_name(cid), avg);
            }
            println!("  total trader capital {}", total_trader_capital(world));
            let p = world.piracy_stats();
            if p.ambushes > 0 || !world.pirates().is_empty() {
                println!(
                    "  piracy: {} ambushes, {} traders lost, {} pirates destroyed, {} bounties paid",
                    p.ambushes, p.traders_lost, p.pirates_destroyed, p.bounties_paid
                );
                println!(
                    "  fleets: {} pirates active, {} navy active ({} pirates suppressed, {} navy lost)",
                    world.pirates().len(),
                    world.navy().len(),
                    p.pirates_suppressed,
                    p.navy_lost
                );
            }

            if let Some(path) = dump {
                let json = serde_json::to_string_pretty(&session.snapshot())?;
                std::fs::write(&path, json)
                    .with_context(|| format!("writing dump to {}", path.display()))?;
                println!("dumped state to {}", path.display());
            }

            if log {
                println!("--- event log (recent tail) ---");
                for e in session.world().events() {
                    println!("[{:>6}] {:?}: {}", e.tick.get(), e.category, e.message);
                }
            }
        }

        Command::Inspect {
            mods,
            scenario,
            ticks,
            seed,
            every,
        } => {
            let reg = load_registry(&mods)?;
            let scn = load_scenario(&scenario)?;
            let seed = seed.unwrap_or(scn.seed);
            let ticks = ticks.unwrap_or(scn.ticks);
            let mut session = Session::new(reg.clone(), &scn, seed).context("building world")?;

            // Header.
            print!("{:>8}", "tick");
            for (cid, _) in reg.commodities() {
                print!("{:>12}", reg.commodity_name(cid));
            }
            println!("{:>14}{:>8}", "capital", "lost");

            let sample = |w: &World| {
                print!("{:>8}", w.tick_count().get());
                let prices = average_prices(w);
                for (_, avg) in &prices {
                    print!("{:>12.1}", avg);
                }
                println!(
                    "{:>14}{:>8}",
                    total_trader_capital(w),
                    w.piracy_stats().traders_lost
                );
            };

            sample(session.world());
            let mut remaining = ticks;
            while remaining > 0 {
                let step = remaining.min(every);
                session.run(step);
                remaining -= step;
                sample(session.world());
            }
        }

        Command::Battle {
            mods,
            ship,
            vs,
            per_side,
            seed,
            max_ticks,
        } => {
            let reg = load_registry(&mods)?;
            let vs = vs.unwrap_or_else(|| ship.clone());

            let mut combatants = spawn_squadron(&reg, &ship, 0, per_side, -50.0)?;
            combatants.extend(spawn_squadron(&reg, &vs, 1, per_side, 50.0)?);
            let mut enc = Encounter::new(combatants);

            println!(
                "battle: {per_side}x {ship} (faction 0) vs {per_side}x {vs} (faction 1), seed {seed}"
            );

            // Drive the encounter over the Step seam, counting ticks for the report.
            let mut rng = DetRng::from_seed(seed);
            let mut ticks = 0u64;
            while enc.outcome() == Outcome::Ongoing && ticks < max_ticks {
                let mut ctx = SimContext::new(Tick(ticks), &mut rng);
                enc.step(&mut ctx);
                ticks += 1;
            }

            match enc.outcome() {
                Outcome::Victory(f) => println!("faction {f} wins after {ticks} ticks"),
                Outcome::Draw => println!("mutual destruction after {ticks} ticks"),
                Outcome::Ongoing => println!("stalemate: undecided after {ticks} ticks"),
            }
            println!(
                "  survivors: faction 0 = {}/{}, faction 1 = {}/{}",
                enc.survivors(0),
                per_side,
                enc.survivors(1),
                per_side
            );
        }

        Command::Play {
            mods,
            scenario,
            seed,
            start,
            ship,
            capital,
        } => {
            let mut session = Session::load(&mods, &scenario, seed).context("building world")?;
            let reg = session.registry_arc();

            let start_sys = reg
                .system_id(&start)
                .with_context(|| format!("unknown start system '{start}'"))?;
            let ship_id = reg
                .ship_id(&ship)
                .with_context(|| format!("unknown ship '{ship}'"))?;

            // Spawn the player ship, then read back its stable id.
            let player = PlayerId(0);
            session.queue_command(SimCommand::Spawn {
                player,
                ship: ship_id,
                at: start_sys,
                capital,
            });
            session.step();
            let trader = session
                .world()
                .traders()
                .iter()
                .find(|t| t.is_player())
                .expect("player ship spawned")
                .id;

            let stdin = std::io::stdin();
            let mut stdout = std::io::stdout();
            writeln!(stdout, "Welcome to Drift, commander. You fly a {ship}.")?;
            play::run_repl(
                &reg,
                session.world_mut(),
                player,
                trader,
                BufReader::new(stdin.lock()),
                &mut stdout,
            )?;
        }
    }

    Ok(())
}

/// Spawn `count` ships of `ship_id` for `faction`, lined up at `x` and spread out
/// along y. Errors if the ship id is unknown.
fn spawn_squadron(
    reg: &Registry,
    ship_id: &str,
    faction: u8,
    count: u32,
    x: f64,
) -> Result<Vec<Combatant>> {
    let sid = reg
        .ship_id(ship_id)
        .with_context(|| format!("unknown ship '{ship_id}'"))?;
    let def = reg.ship(sid);
    let stats = def.combat.unwrap_or_default();
    if stats.weapon_damage == 0 {
        eprintln!("warning: ship '{ship_id}' is unarmed and cannot win");
    }
    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count {
        let y = (i as f64 - (count as f64 - 1.0) / 2.0) * 20.0;
        out.push(Combatant::new(
            sid,
            faction,
            stats,
            def.hull,
            def.max_speed,
            Vec2::new(x, y),
        ));
    }
    Ok(out)
}
