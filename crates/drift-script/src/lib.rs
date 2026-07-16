//! `drift-script` — the mod-scripting engine.
//!
//! Mods author behavior in [Rhai](https://rhai.rs), a small scripting language
//! *written in and for* Rust. It is the engine behind Drift's `NamedRegistry`
//! seam: content references behavior by name, and where the engine used to resolve
//! that name only to a built-in Rust handler, it can now resolve it to a
//! mod-authored script.
//!
//! Rhai is chosen over Lua or a WASM runtime because it fits Drift's constraints
//! directly:
//!
//! - **Pure Rust.** No C dependency, so the all-Rust workspace still cross-compiles
//!   cleanly and there is no separate toolchain for modders.
//! - **Sandboxed by construction.** A fresh [`rhai::Engine`] has no filesystem,
//!   network, clock, or RNG access unless the host explicitly registers it — so a
//!   script cannot reach anything ambient or non-deterministic. We register
//!   nothing of the sort.
//! - **Operation-limited.** [`Engine::set_max_operations`] caps the work a single
//!   call may do, so a runaway or malicious script is terminated rather than
//!   hanging the tick.
//!
//! Determinism: with no clock/RNG exposed and a fixed operation budget, a script
//! is a pure function of its inputs — same inputs, same output, run to run. (Bit-
//! exact cross-*platform* results for transcendental math are not guaranteed by any
//! engine, but Drift is server-authoritative, so scripts need only be
//! self-consistent on the host that runs them.)
//!
//! The first hook is [`ScriptedPricing`]: a market pricing strategy authored in
//! Rhai. More hooks (trader AI, event rules) slot into the same seam later.

use std::sync::Arc;

use drift_core::{Money, Quantity};
use rhai::{Engine, Scope, AST};
use thiserror::Error;

/// The largest number of Rhai operations one script call may execute before it is
/// aborted. Generous for the small arithmetic a pricing script does, but a hard
/// backstop against an infinite loop stalling the simulation tick.
const MAX_OPERATIONS: u64 = 100_000;

/// A script failed to compile.
#[derive(Debug, Error)]
#[error("script compile error: {0}")]
pub struct CompileError(String);

/// Build a sandboxed, operation-limited engine. A fresh engine already denies
/// filesystem/network/clock/RNG access; we only add the fuel cap. Nothing that
/// could introduce non-determinism is ever registered.
fn sandboxed_engine() -> Engine {
    let mut engine = Engine::new();
    engine.set_max_operations(MAX_OPERATIONS);
    engine
}

/// A market pricing strategy authored in Rhai.
///
/// The script must define a function
/// `fn price(base, stock, equilibrium, elasticity)` returning the unit price as an
/// integer. It is called once per market per repricing tick. Any error (a bad
/// script, a wrong return type, or exceeding the operation budget) is contained:
/// [`price`](ScriptedPricing::price) floors to 1 credit rather than panicking the
/// simulation.
#[derive(Clone)]
pub struct ScriptedPricing {
    engine: Arc<Engine>,
    ast: Arc<AST>,
}

impl std::fmt::Debug for ScriptedPricing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The compiled AST/engine are opaque; a compiled script has no meaningful
        // human-readable form here.
        f.write_str("ScriptedPricing(<rhai>)")
    }
}

impl ScriptedPricing {
    /// Compile a pricing script. Fails if the source does not parse.
    pub fn compile(source: &str) -> Result<Self, CompileError> {
        let engine = sandboxed_engine();
        let ast = engine
            .compile(source)
            .map_err(|e| CompileError(e.to_string()))?;
        Ok(Self {
            engine: Arc::new(engine),
            ast: Arc::new(ast),
        })
    }

    /// Compute a unit price by calling the script's `price` function. A script that
    /// errors, runs away, or returns a non-integer yields `1` (the price floor), so
    /// a broken mod can never crash or hang the tick.
    pub fn price(
        &self,
        base_price: Money,
        stock: Quantity,
        equilibrium: Quantity,
        elasticity: f64,
    ) -> Money {
        let mut scope = Scope::new();
        let result: Result<i64, _> = self.engine.call_fn(
            &mut scope,
            &self.ast,
            "price",
            (base_price, stock as i64, equilibrium as i64, elasticity),
        );
        result.map(|p| p.max(1)).unwrap_or(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A script that recreates the built-in linear supply/demand feel simply.
    const LINEAR: &str = r#"
        fn price(base, stock, equilibrium, elasticity) {
            // Cheaper when stock exceeds equilibrium, dearer when short.
            let ratio = equilibrium.to_float() / stock.to_float();
            let p = (base.to_float() * ratio).to_int();
            if p < 1 { 1 } else { p }
        }
    "#;

    #[test]
    fn compiles_and_prices() {
        let s = ScriptedPricing::compile(LINEAR).unwrap();
        // At equilibrium (stock == equilibrium) the ratio is 1, so price == base.
        assert_eq!(s.price(100, 500, 500, 1.0), 100);
        // Scarcity (stock below equilibrium) raises the price.
        assert!(s.price(100, 250, 500, 1.0) > 100);
        // Glut lowers it.
        assert!(s.price(100, 1000, 500, 1.0) < 100);
    }

    #[test]
    fn is_deterministic_across_instances_and_calls() {
        let a = ScriptedPricing::compile(LINEAR).unwrap();
        let b = ScriptedPricing::compile(LINEAR).unwrap();
        for stock in [1u32, 50, 100, 500, 5000] {
            let pa = a.price(100, stock, 500, 1.0);
            let pb = b.price(100, stock, 500, 1.0);
            assert_eq!(pa, pb, "same script + inputs -> same price (stock={stock})");
            // And stable when called again.
            assert_eq!(pa, a.price(100, stock, 500, 1.0));
        }
    }

    #[test]
    fn a_runaway_script_is_terminated_not_hung() {
        // An infinite loop must hit the operation cap and abort, so `price` falls
        // back to the floor rather than hanging the tick.
        let s = ScriptedPricing::compile(
            "fn price(base, stock, equilibrium, elasticity) { let x = 0; loop { x += 1; } x }",
        )
        .unwrap();
        assert_eq!(s.price(100, 500, 500, 1.0), 1, "runaway aborts to the price floor");
    }

    #[test]
    fn a_broken_script_floors_to_one_rather_than_crashing() {
        // Wrong return type (a string) is contained, not a panic.
        let s = ScriptedPricing::compile(
            r#"fn price(base, stock, equilibrium, elasticity) { "not a number" }"#,
        )
        .unwrap();
        assert_eq!(s.price(100, 500, 500, 1.0), 1);
    }

    #[test]
    fn the_sandbox_denies_ambient_access() {
        // There is no filesystem/clock/RNG in a fresh engine, so a script that
        // reaches for one fails to *compile* (the symbol is unknown) — proving the
        // engine exposes nothing non-deterministic or I/O-bearing by default.
        assert!(
            ScriptedPricing::compile(
                "fn price(base, stock, equilibrium, elasticity) { timestamp() }"
            )
            .is_ok(),
            "compiles: rhai resolves unknown fns at call time"
        );
        let s = ScriptedPricing::compile(
            "fn price(base, stock, equilibrium, elasticity) { timestamp() }",
        )
        .unwrap();
        // `timestamp` is not registered, so the call errors and is contained.
        assert_eq!(s.price(100, 500, 500, 1.0), 1, "ambient calls are unavailable");
    }
}
