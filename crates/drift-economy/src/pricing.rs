//! Pricing strategies — the first behavior routed through the plugin seam.
//!
//! Content names a strategy per system (`pricing: "supply_demand_v1"`). A
//! [`PricingSet`] maps those names to resolved [`PricingStrategy`] handles and
//! holds any mod-authored scripts behind them. A strategy is either the built-in
//! formula or a [`ScriptedPricing`] authored in Rhai (see `drift-script`) — markets
//! store a small, serializable handle (`PricingStrategy`), and the compiled scripts
//! live in the set, indexed by the handle. Adding a scripted strategy needs no
//! schema change and no change to any market or caller.

use drift_core::{Money, NamedRegistry, Quantity};
use drift_script::ScriptedPricing;
use serde::{Deserialize, Serialize};

/// Lower/upper bounds on price as a multiple of base, so scarcity/glut cannot
/// drive prices to absurd extremes (or to zero, which would break trading).
pub const MIN_FACTOR: f64 = 0.25;
pub const MAX_FACTOR: f64 = 4.0;

/// Fraction of the gap between the current price and the freshly-computed target
/// that a market closes each tick. Sticky prices (< 1.0) damp the boom/bust limit
/// cycles that discrete, lumpy trading otherwise induces, so the economy settles
/// into a stable regime rather than oscillating at the clamp bounds.
pub const PRICE_SMOOTHING: f64 = 0.2;

/// Move `current` a `PRICE_SMOOTHING` fraction toward `target`, floored at 1.
pub fn smoothed(current: Money, target: Money) -> Money {
    let next = current as f64 + PRICE_SMOOTHING * (target - current) as f64;
    (next.round() as i64).max(1)
}

/// A resolved pricing strategy: a small, `Copy`, serializable handle stored in
/// every market. Either the built-in formula or a reference (by index) to a
/// mod-authored script held in the owning [`PricingSet`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PricingStrategy {
    /// The built-in supply/demand formula.
    SupplyDemandV1,
    /// A Rhai script at this index in the [`PricingSet`]'s script table.
    Scripted(u32),
}

impl PricingStrategy {
    /// Compute the unit price given the market's current `stock`, its `equilibrium`
    /// anchor, and the commodity's `base_price` and `elasticity`. `scripts` is the
    /// owning set's script table, consulted only for a [`Scripted`](Self::Scripted)
    /// strategy; a stale/out-of-range index falls back to the built-in formula so a
    /// mismatch can never panic the tick.
    pub fn price(
        self,
        scripts: &[ScriptedPricing],
        base_price: Money,
        stock: Quantity,
        equilibrium: Quantity,
        elasticity: f64,
    ) -> Money {
        match self {
            PricingStrategy::SupplyDemandV1 => {
                supply_demand_v1(base_price, stock, equilibrium, elasticity)
            }
            PricingStrategy::Scripted(i) => match scripts.get(i as usize) {
                Some(s) => s.price(base_price, stock, equilibrium, elasticity),
                None => supply_demand_v1(base_price, stock, equilibrium, elasticity),
            },
        }
    }
}

/// `price = base * clamp((equilibrium / stock)^elasticity)`.
///
/// Scarcity (stock below equilibrium) pushes price up; glut pushes it down; at
/// exactly equilibrium the price is `base`. Clamped to `[MIN_FACTOR, MAX_FACTOR]`
/// and floored at 1 credit.
pub fn supply_demand_v1(
    base_price: Money,
    stock: Quantity,
    equilibrium: Quantity,
    elasticity: f64,
) -> Money {
    let eq = equilibrium.max(1) as f64;
    let st = stock.max(1) as f64;
    let factor = (eq / st).powf(elasticity).clamp(MIN_FACTOR, MAX_FACTOR);
    let price = (base_price as f64 * factor).round() as i64;
    price.max(1)
}

/// The full set of pricing strategies available to a run: the name-keyed registry
/// of resolved handles, plus the compiled scripts those handles may point at.
///
/// This is what a host builds (from built-ins plus a mod's scripts), hands to the
/// loader for content validation (via [`names`](Self::names)), and passes to
/// `World::new` (which resolves each system's `pricing` name and keeps the script
/// table for repricing).
#[derive(Debug, Default)]
pub struct PricingSet {
    registry: NamedRegistry<PricingStrategy>,
    scripts: Vec<ScriptedPricing>,
}

impl PricingSet {
    /// Resolve a strategy name to its handle, or an error listing what is
    /// registered.
    pub fn resolve(&self, name: &str) -> Result<&PricingStrategy, drift_core::UnknownStrategy> {
        self.registry.resolve(name)
    }

    /// Whether a strategy name is registered.
    pub fn contains(&self, name: &str) -> bool {
        self.registry.contains(name)
    }

    /// Every registered strategy name (built-in and scripted) — the valid `pricing`
    /// values for content, handed to the loader for validation.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.registry.names()
    }

    /// The compiled script table, indexed by [`PricingStrategy::Scripted`].
    pub fn scripts(&self) -> &[ScriptedPricing] {
        &self.scripts
    }

    /// Register a mod-authored pricing script under `name`. The script is appended
    /// to the table and the name resolves to it, so content can select it exactly
    /// like a built-in.
    pub fn register_script(&mut self, name: impl Into<String>, script: ScriptedPricing) {
        let index = self.scripts.len() as u32;
        self.scripts.push(script);
        self.registry
            .register(name, PricingStrategy::Scripted(index));
    }
}

/// Build the set of built-in pricing strategies (no scripts). Hosts add mod
/// scripts with [`PricingSet::register_script`] before constructing the world.
pub fn builtin_pricing() -> PricingSet {
    let mut set = PricingSet::default();
    set.registry
        .register("supply_demand_v1", PricingStrategy::SupplyDemandV1);
    set
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn at_equilibrium_price_is_base() {
        assert_eq!(supply_demand_v1(100, 500, 500, 0.8), 100);
    }

    #[test]
    fn monotonic_decreasing_in_stock() {
        let mut prev = i64::MAX;
        for stock in [1u32, 50, 100, 250, 500, 1000, 5000] {
            let p = supply_demand_v1(100, stock, 500, 0.8);
            assert!(p <= prev, "price must not rise as stock rises (stock={stock}, p={p}, prev={prev})");
            prev = p;
        }
    }

    #[test]
    fn scarcity_raises_and_glut_lowers() {
        let base = supply_demand_v1(100, 500, 500, 0.8);
        let scarce = supply_demand_v1(100, 50, 500, 0.8);
        let glut = supply_demand_v1(100, 5000, 500, 0.8);
        assert!(scarce > base);
        assert!(glut < base);
    }

    #[test]
    fn clamps_at_bounds() {
        // Extreme scarcity clamps to MAX_FACTOR; extreme glut to MIN_FACTOR.
        let scarce = supply_demand_v1(100, 1, 100_000, 1.0);
        let glut = supply_demand_v1(100, 100_000, 1, 1.0);
        assert_eq!(scarce, (100.0 * MAX_FACTOR) as i64);
        assert_eq!(glut, (100.0 * MIN_FACTOR) as i64);
    }

    #[test]
    fn never_below_one_credit() {
        assert!(supply_demand_v1(1, 100_000, 1, 2.0) >= 1);
    }

    #[test]
    fn registry_resolves_builtin() {
        let reg = builtin_pricing();
        assert!(reg.contains("supply_demand_v1"));
        assert_eq!(reg.resolve("supply_demand_v1"), Ok(&PricingStrategy::SupplyDemandV1));
    }
}
