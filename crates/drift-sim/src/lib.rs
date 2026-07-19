//! `drift-sim` — the session/driver layer.
//!
//! A [`Session`] owns a running [`World`] and is the single façade a host (the CLI,
//! a future server, in-process single-player) drives: it centralizes building a
//! world from content + a scenario, applying commands, advancing ticks, draining
//! per-tick events, and taking snapshots — so hosts don't repeat that wiring or
//! reach into `World` internals. Single-player is the N=1 case of the same façade
//! a networked server would use.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use drift_data::ScenarioDef;
use drift_economy::{
    builtin_pricing, pricing_for, Command, PricingScriptError, SimEvent, Snapshot, World,
    WorldError,
};
use drift_mods::{load_and_link, LoadError, Registry};
use thiserror::Error;

/// Errors from loading content/scenario or building a session.
#[derive(Debug, Error)]
pub enum SessionError {
    #[error("i/o error reading {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse scenario {path}: {source}")]
    Scenario {
        path: String,
        #[source]
        source: ron::error::SpannedError,
    },
    #[error(transparent)]
    Load(#[from] LoadError),
    #[error(transparent)]
    Script(#[from] PricingScriptError),
    #[error(transparent)]
    World(#[from] WorldError),
}

/// The set of pricing strategy names the engine can execute (for content
/// validation). Kept in one place so hosts don't rebuild it.
fn pricing_names() -> HashSet<String> {
    builtin_pricing().names().map(String::from).collect()
}

/// Load and link a registry from a mods directory.
pub fn load_registry(mods: &Path) -> Result<Arc<Registry>, SessionError> {
    Ok(Arc::new(load_and_link(mods, &pricing_names())?))
}

/// Read and parse a scenario file.
pub fn load_scenario(path: &Path) -> Result<ScenarioDef, SessionError> {
    let text = std::fs::read_to_string(path).map_err(|source| SessionError::Io {
        path: path.display().to_string(),
        source,
    })?;
    ron::from_str(&text).map_err(|source| SessionError::Scenario {
        path: path.display().to_string(),
        source,
    })
}

/// A running simulation and the façade for driving it.
pub struct Session {
    world: World,
}

impl Session {
    /// Build a session from an already-loaded registry, a scenario, and a seed.
    /// The pricing registry is resolved internally (hosts never pass it).
    pub fn new(
        registry: Arc<Registry>,
        scenario: &ScenarioDef,
        seed: u64,
    ) -> Result<Self, SessionError> {
        // Compile the registry's mod-declared pricing scripts into the strategy
        // set (built-ins plus scripts), so a system that names a scripted strategy
        // resolves it at world-build time.
        let pricing = pricing_for(&registry)?;
        let world = World::new(registry, scenario, seed, &pricing)?;
        Ok(Self { world })
    }

    /// Convenience: load a registry and scenario from disk and build a session.
    /// `seed` overrides the scenario's seed when `Some`.
    pub fn load(
        mods: &Path,
        scenario: &Path,
        seed: Option<u64>,
    ) -> Result<Self, SessionError> {
        let registry = load_registry(mods)?;
        let scn = load_scenario(scenario)?;
        let seed = seed.unwrap_or(scn.seed);
        Self::new(registry, &scn, seed)
    }

    /// Advance exactly one tick and return the events emitted during it (in
    /// order). This is the per-tick event primitive hosts stream or broadcast.
    pub fn step(&mut self) -> Vec<SimEvent> {
        let now = self.world.tick_count();
        self.world.tick();
        // This tick's events are the trailing entries (logged with `tick == now`).
        let mut fresh: Vec<SimEvent> = self
            .world
            .events()
            .rev()
            .take_while(|e| e.tick == now)
            .cloned()
            .collect();
        fresh.reverse();
        fresh
    }

    /// Advance `n` ticks. Events remain queryable via [`world`](Self::world).
    pub fn run(&mut self, n: u64) {
        self.world.run(n);
    }

    /// Queue a player command for the next tick.
    pub fn queue_command(&mut self, command: Command) {
        self.world.queue_command(command);
    }

    /// A serializable snapshot of the mutable world state.
    pub fn snapshot(&self) -> Snapshot<'_> {
        self.world.snapshot()
    }

    pub fn world(&self) -> &World {
        &self.world
    }
    pub fn world_mut(&mut self) -> &mut World {
        &mut self.world
    }
    pub fn registry(&self) -> &Registry {
        self.world.registry()
    }
    /// A cloned `Arc` handle to the shared registry (independent of `self`, so a
    /// host can read the registry while mutating the world).
    pub fn registry_arc(&self) -> Arc<Registry> {
        self.world.registry_arc()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use drift_data::TraderSpawn;
    use tempfile::TempDir;

    use super::*;

    fn mods_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../mods")
    }

    fn scenario_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scenarios/frontier.ron")
    }

    #[test]
    fn load_builds_a_runnable_session() {
        let mut s = Session::load(&mods_path(), &scenario_path(), Some(1)).unwrap();
        assert_eq!(s.world().tick_count().get(), 0);
        s.run(100);
        assert_eq!(s.world().tick_count().get(), 100);
        assert!(s.registry().system_count() > 0);
    }

    #[test]
    fn step_returns_that_ticks_events_and_matches_the_log() {
        let mut s = Session::load(&mods_path(), &scenario_path(), Some(7)).unwrap();
        // Reconstruct the full log from per-tick step() results.
        let mut streamed: Vec<(u64, String)> = Vec::new();
        for _ in 0..150 {
            for e in s.step() {
                streamed.push((e.tick.get(), e.message));
            }
        }
        let full: Vec<(u64, String)> = s
            .world()
            .events()
            .map(|e| (e.tick.get(), e.message.clone()))
            .collect();
        assert!(!streamed.is_empty(), "the frontier run should emit events");
        assert_eq!(streamed, full, "step() reconstructs the full event log in order");
    }

    /// A quiet sandbox scenario: no NPC traders, no piracy, so nothing perturbs
    /// the market prices set at world build.
    fn quiet_sandbox() -> ScenarioDef {
        ScenarioDef {
            name: "script-sandbox".into(),
            seed: 1,
            ticks: 0,
            traders: TraderSpawn {
                count: 0,
                ship: "test:hauler".into(),
                starting_capital: 1000,
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

    /// Write a mod whose single system is priced by a Rhai script returning `3 *
    /// base`. The built-in strategy would price the good at `base` when stock is at
    /// equilibrium, so `3 * base` unambiguously proves the *script* ran.
    fn write_scripted_mod(root: &Path) {
        let dir = root.join("core");
        fs::create_dir_all(dir.join("scripts")).unwrap();
        fs::create_dir_all(dir.join("commodities")).unwrap();
        fs::create_dir_all(dir.join("systems")).unwrap();
        fs::create_dir_all(dir.join("ships")).unwrap();
        fs::write(
            dir.join("manifest.toml"),
            r#"id = "test"
name = "Test"
version = "0.1.0"

[[scripts]]
name = "test:triple"
path = "scripts/triple.rhai"
kind = "pricing"
"#,
        )
        .unwrap();
        fs::write(
            dir.join("scripts/triple.rhai"),
            "fn price(base, stock, equilibrium, elasticity) { base * 3 }",
        )
        .unwrap();
        fs::write(
            dir.join("commodities/goods.ron"),
            r#"[ (id: "test:food", name: "Food", base_price: 100, unit_mass: 1, elasticity: 0.8, category: "food") ]"#,
        )
        .unwrap();
        fs::write(
            dir.join("systems/lave.ron"),
            r#"[
                (id: "test:lave", name: "Lave", position: (0.0, 0.0),
                 industries: [], connections: [],
                 initial_stock: [(commodity: "test:food", qty: 100)],
                 pricing: "test:triple"),
            ]"#,
        )
        .unwrap();
        fs::write(
            dir.join("ships/hauler.ron"),
            r#"[ (id: "test:hauler", name: "Hauler", cargo_capacity: 20, jump_speed: 5.0, hull: 80, max_speed: 200.0) ]"#,
        )
        .unwrap();
    }

    #[test]
    fn a_mod_declared_pricing_script_drives_a_market() {
        // End-to-end: a `.rhai` file on disk, declared in a manifest, selected by a
        // system's `pricing`, compiled into the strategy set, and actually pricing
        // the market — all through `Session`, with no programmatic registration.
        let tmp = TempDir::new().unwrap();
        write_scripted_mod(tmp.path());

        let reg = load_registry(tmp.path()).expect("scripted mod should load and link");
        assert_eq!(reg.scripts().len(), 1, "the declared script was loaded");

        let session = Session::new(reg, &quiet_sandbox(), 1).expect("world should build");
        let food = session.registry().commodity_id("test:food").unwrap();
        let sys = session.registry().system_id("test:lave").unwrap();
        let market = session
            .world()
            .markets()
            .iter()
            .find(|m| m.system == sys)
            .unwrap();
        assert_eq!(
            market.price(food),
            Some(300),
            "the market is priced by the script (3 * base), not the built-in"
        );
    }

    #[test]
    fn a_broken_pricing_script_fails_the_session_build() {
        // A script that does not compile must abort `Session::new` with a clear
        // error, not surface as a silent price-floor at the first repricing tick.
        let tmp = TempDir::new().unwrap();
        write_scripted_mod(tmp.path());
        // Clobber the script with a syntax error.
        fs::write(
            tmp.path().join("core/scripts/triple.rhai"),
            "fn price(base, stock, equilibrium, elasticity) { base * }",
        )
        .unwrap();

        let reg = load_registry(tmp.path()).expect("content still links (source is not compiled here)");
        // `Session` is not `Debug`, so match rather than `unwrap_err`.
        let err = match Session::new(reg, &quiet_sandbox(), 1) {
            Ok(_) => panic!("a broken pricing script must fail the session build"),
            Err(e) => e,
        };
        assert!(
            matches!(err, SessionError::Script(_)),
            "a compile failure should be a Script error, got {err:?}"
        );
    }

    #[test]
    fn sessions_are_deterministic() {
        let dump = |seed| {
            let reg = load_registry(&mods_path()).unwrap();
            let scn = load_scenario(&scenario_path()).unwrap();
            let mut s = Session::new(reg, &scn, seed).unwrap();
            s.run(500);
            serde_json::to_string(&s.snapshot()).unwrap()
        };
        assert_eq!(dump(42), dump(42));
    }
}
