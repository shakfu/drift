//! Production phase: systems run their industries, transforming stock.
//!
//! A recipe applies some number of times per tick, each application gated on all
//! inputs being in stock. Recipes with no inputs are raw producers; recipes with
//! no outputs are consumers (population demand), which create the shortages that
//! pull trade. Producing/consuming a commodity requires the system's market to
//! already trade it — enforced when the world is built.
//!
//! Throughput is **price-elastic** (see [`elastic_factor`]): the nominal `rate`
//! is scaled by a factor of current price, so consumers buy less when a good is
//! dear and producers make more when their product is dear. That negative
//! feedback lets the economy settle to a genuine equilibrium instead of pinning a
//! chronically short good at its price clamp.
//!
//! Throughput is additionally scaled by an **endogenous capacity** — a slow
//! capital stock per manufacturing industry (see [`capacity_target`] and
//! [`eased_capacity`]). Where elasticity is a market's *immediate* response to the
//! current price, capacity is the *accumulated* one: a factory grows its capital
//! when its processing margin (output value minus input cost) is fat and lets it
//! decay when the margin is thin. Because capital is sticky (it eases toward its
//! target far slower than prices move), supply gains a lagged, path-dependent
//! response — investment and disinvestment — on top of the instantaneous
//! elasticity. Only transformers (recipes with both inputs and outputs) have
//! capacity; raw extractors and pure consumers are fixed (see [`has_capacity`]).

use drift_core::{CommodityId, Money};
use drift_mods::ResolvedRecipe;

use crate::market::Market;

/// Upper bound on the elastic scaling factor, so a wild price cannot make a
/// recipe run absurdly fast.
pub const MAX_ELASTIC_FACTOR: f64 = 4.0;

/// Lower/upper bounds on an industry's endogenous **capacity** multiplier.
///
/// Capacity scales throughput *on top of* the instantaneous [`elastic_factor`]:
/// where elasticity is a market's immediate response to the current price, a
/// factory's capacity is its accumulated capital stock, which grows or decays
/// slowly with sustained profitability. The bounds keep a boom from exploding to
/// infinity and a bust from killing an industry outright (a floored industry can
/// always recover if its margin returns).
pub const MIN_CAPACITY: f64 = 0.25;
pub const MAX_CAPACITY: f64 = 4.0;

/// Investment elasticity: how strongly a profitability ratio pulls the capacity
/// target. Deliberately mild (< 1), so capital chases returns without
/// overreacting to a transient margin.
pub const CAPACITY_INVEST_ELASTICITY: f64 = 0.5;

/// Fraction of the gap to the capacity target closed each tick. Far slower than
/// price smoothing (`PRICE_SMOOTHING == 0.2`): capital is sticky, so capacity
/// lags the price signal by many ticks. That lag is the source of the richer
/// investment dynamics (over- and under-shoot, hysteresis), and being slow keeps
/// the capital/price loop damped rather than driving a cobweb blow-up.
pub const CAPACITY_SMOOTHING: f64 = 0.02;

/// Whether an industry has an endogenous capacity at all. Capacity models
/// *processing* capital, so it applies only to transformers — recipes with both
/// inputs and outputs (the manufacturing chain). Raw extractors (no inputs) are
/// fixed natural endowments, and pure consumers (no outputs) are population
/// demand; neither invests, so both hold nominal capacity `1.0`.
pub fn has_capacity(recipe: &ResolvedRecipe) -> bool {
    !recipe.inputs.is_empty() && !recipe.outputs.is_empty()
}

/// The value one application of `recipe` adds at the prices `price_of` returns:
/// the outputs valued minus the inputs valued. This per-application margin is the
/// profit signal capacity investment chases.
pub fn recipe_margin(recipe: &ResolvedRecipe, price_of: impl Fn(CommodityId) -> Money) -> Money {
    let value = |goods: &[(CommodityId, u32)]| -> Money {
        goods
            .iter()
            .map(|(c, q)| price_of(*c) * *q as Money)
            .sum()
    };
    value(&recipe.outputs) - value(&recipe.inputs)
}

/// The capacity an industry is drawn toward, from its current and baseline
/// per-application margins (see [`recipe_margin`]).
///
/// The target is the profitability ratio `margin_now / margin_base` raised to the
/// investment elasticity, clamped to `[MIN_CAPACITY, MAX_CAPACITY]`. The baseline
/// margin (margins valued at base prices) is a per-recipe constant, so at base
/// prices the ratio is `1` and the target is nominal capacity — the fixed point.
/// A non-positive baseline margin (a break-even or loss-leading recipe) yields
/// `1.0`, i.e. no investment signal, and a currently-unprofitable industry
/// (non-positive `margin_now`) is pulled to the floor.
pub fn capacity_target(margin_now: Money, margin_base: Money) -> f64 {
    if margin_base <= 0 {
        return 1.0;
    }
    let ratio = margin_now.max(0) as f64 / margin_base as f64;
    ratio
        .powf(CAPACITY_INVEST_ELASTICITY)
        .clamp(MIN_CAPACITY, MAX_CAPACITY)
}

/// Ease `capacity` a [`CAPACITY_SMOOTHING`] fraction toward `target`, clamped to
/// the capacity bounds. The slow ease is what makes capital sticky.
pub fn eased_capacity(capacity: f64, target: f64) -> f64 {
    let next = capacity + CAPACITY_SMOOTHING * (target - capacity);
    next.clamp(MIN_CAPACITY, MAX_CAPACITY)
}

/// Apply a recipe up to `count` times this tick, stopping early if inputs run
/// out. Returns the number of applications that actually ran.
pub fn apply_recipe(market: &mut Market, recipe: &ResolvedRecipe, count: u32) -> u32 {
    let mut applied = 0;
    for _ in 0..count {
        let inputs_available = recipe.inputs.iter().all(|(c, q)| market.stock(*c) >= *q);
        if !inputs_available {
            break; // no partial applications; stop once starved
        }
        for (c, q) in &recipe.inputs {
            let ok = market.try_remove(*c, *q);
            debug_assert!(ok, "input availability was just checked");
        }
        for (c, q) in &recipe.outputs {
            market.add(*c, *q);
        }
        applied += 1;
    }
    applied
}

/// Inelastic single-tick application at nominal `rate` (used where elasticity is
/// not modelled, e.g. focused unit tests).
pub fn run_recipe(market: &mut Market, recipe: &ResolvedRecipe) {
    apply_recipe(market, recipe, recipe.rate);
}

/// Which commodity's price drives a recipe's elastic response, and whether the
/// response is supply-side (`true`: produce more when dear) or demand-side
/// (`false`: consume less when dear).
///
/// A recipe with any **input** responds to its first input, demand-side — this
/// covers both population consumers and refiners, and is what gives every
/// consumed good a price-restoring force (a refiner throttling its intake keeps
/// an intermediate good's price from integrating to a clamp). Only a pure
/// producer (no inputs) falls back to a supply-side response on its output —
/// and that is deliberately left inelastic in content, because keying supply on
/// a producer's own (glutted, cheap) local price would perversely throttle
/// exports.
pub fn response_signal(recipe: &ResolvedRecipe) -> Option<(CommodityId, bool)> {
    if let Some(&(c, _)) = recipe.inputs.first() {
        Some((c, false))
    } else if let Some(&(c, _)) = recipe.outputs.first() {
        Some((c, true))
    } else {
        None
    }
}

/// The multiplier applied to a recipe's nominal rate given the signal
/// commodity's `base_price` and current `price`.
///
/// - supply-side: `(price / base)^elasticity` — dearer product, more output.
/// - demand-side: `(base / price)^elasticity` — dearer input, less consumption.
///
/// `elasticity == 0` (or a degenerate price) yields `1.0`, i.e. inelastic. The
/// result is clamped to `[0, MAX_ELASTIC_FACTOR]`.
pub fn elastic_factor(elasticity: f64, supply_side: bool, base_price: Money, price: Money) -> f64 {
    if elasticity == 0.0 {
        return 1.0;
    }
    let base = base_price.max(1) as f64;
    let p = price.max(1) as f64;
    let ratio = if supply_side { p / base } else { base / p };
    ratio.powf(elasticity).clamp(0.0, MAX_ELASTIC_FACTOR)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use drift_core::{CommodityId, RecipeId, SystemId};
    use drift_mods::ResolvedRecipe;

    use super::*;
    use crate::market::{Market, MarketGood};
    use crate::pricing::PricingStrategy;

    fn good(stock: u32) -> MarketGood {
        MarketGood {
            stock,
            equilibrium: 100,
            price: 100,
        }
    }

    fn market(goods: &[(u32, u32)]) -> Market {
        // goods: (commodity_index, stock)
        let mut m = BTreeMap::new();
        for (c, stock) in goods {
            m.insert(CommodityId(*c), good(*stock));
        }
        Market {
            system: SystemId(0),
            pricing: PricingStrategy::SupplyDemandV1,
            goods: m,
        }
    }

    #[test]
    fn raw_producer_adds_output() {
        let mut m = market(&[(0, 0)]); // ore, empty
        let recipe = ResolvedRecipe {
            id: RecipeId(0),
            inputs: vec![],
            outputs: vec![(CommodityId(0), 5)],
            rate: 3,
            elasticity: 0.0,
        };
        run_recipe(&mut m, &recipe);
        assert_eq!(m.stock(CommodityId(0)), 15); // 5 * rate 3
    }

    #[test]
    fn refiner_consumes_inputs_and_produces_outputs() {
        // ore=0, alloys=1
        let mut m = market(&[(0, 10), (1, 0)]);
        let recipe = ResolvedRecipe {
            id: RecipeId(0),
            inputs: vec![(CommodityId(0), 2)],
            outputs: vec![(CommodityId(1), 1)],
            rate: 3,
            elasticity: 0.0,
        };
        run_recipe(&mut m, &recipe);
        assert_eq!(m.stock(CommodityId(0)), 10 - 6, "consumed 2*3 ore");
        assert_eq!(m.stock(CommodityId(1)), 3, "produced 1*3 alloys");
    }

    #[test]
    fn halts_when_inputs_insufficient() {
        // Only enough ore for one application of a rate-5 recipe.
        let mut m = market(&[(0, 2), (1, 0)]);
        let recipe = ResolvedRecipe {
            id: RecipeId(0),
            inputs: vec![(CommodityId(0), 2)],
            outputs: vec![(CommodityId(1), 1)],
            rate: 5,
            elasticity: 0.0,
        };
        run_recipe(&mut m, &recipe);
        assert_eq!(m.stock(CommodityId(0)), 0, "all available ore consumed");
        assert_eq!(m.stock(CommodityId(1)), 1, "only one application ran");
    }

    #[test]
    fn consumer_recipe_drains_stock() {
        let mut m = market(&[(0, 10)]);
        let recipe = ResolvedRecipe {
            id: RecipeId(0),
            inputs: vec![(CommodityId(0), 2)],
            outputs: vec![],
            rate: 3,
            elasticity: 0.0,
        };
        run_recipe(&mut m, &recipe);
        assert_eq!(m.stock(CommodityId(0)), 4, "consumed 2*3, produced nothing");
    }

    #[test]
    fn demand_backs_off_when_input_is_dear() {
        // Pure consumer, demand-side elasticity: dearer input -> factor < 1.
        assert!(elastic_factor(1.0, false, 100, 200) < 1.0);
        // Cheaper input -> consume more.
        assert!(elastic_factor(1.0, false, 100, 50) > 1.0);
        // At base price -> inelastic point.
        assert!((elastic_factor(1.0, false, 100, 100) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn supply_ramps_when_output_is_dear() {
        // Producer, supply-side: dearer output -> factor > 1.
        assert!(elastic_factor(1.0, true, 100, 200) > 1.0);
        assert!(elastic_factor(1.0, true, 100, 50) < 1.0);
    }

    #[test]
    fn zero_elasticity_is_inelastic() {
        assert_eq!(elastic_factor(0.0, true, 100, 999), 1.0);
        assert_eq!(elastic_factor(0.0, false, 100, 1), 1.0);
    }

    #[test]
    fn factor_is_clamped() {
        assert!(elastic_factor(5.0, false, 100, 1) <= MAX_ELASTIC_FACTOR);
        assert!(elastic_factor(5.0, true, 100, 100_000) <= MAX_ELASTIC_FACTOR);
    }

    #[test]
    fn response_signal_prefers_input_demand_side() {
        // Pure producer (no inputs) -> supply-side on its output.
        let producer = ResolvedRecipe {
            id: RecipeId(0),
            inputs: vec![],
            outputs: vec![(CommodityId(7), 1)],
            rate: 1,
            elasticity: 0.5,
        };
        assert_eq!(response_signal(&producer), Some((CommodityId(7), true)));

        // A refiner (has both) responds demand-side to its input, so the
        // intermediate good it consumes gets a restoring force.
        let refiner = ResolvedRecipe {
            id: RecipeId(0),
            inputs: vec![(CommodityId(3), 2)],
            outputs: vec![(CommodityId(4), 1)],
            rate: 1,
            elasticity: 0.5,
        };
        assert_eq!(response_signal(&refiner), Some((CommodityId(3), false)));

        // A pure consumer -> demand-side on its input.
        let consumer = ResolvedRecipe {
            id: RecipeId(0),
            inputs: vec![(CommodityId(3), 1)],
            outputs: vec![],
            rate: 1,
            elasticity: 0.5,
        };
        assert_eq!(response_signal(&consumer), Some((CommodityId(3), false)));
    }

    fn refiner() -> ResolvedRecipe {
        // 2 ore (base 30) -> 1 alloy (base 80): base margin = 80 - 60 = 20.
        ResolvedRecipe {
            id: RecipeId(0),
            inputs: vec![(CommodityId(0), 2)],
            outputs: vec![(CommodityId(1), 1)],
            rate: 12,
            elasticity: 0.8,
        }
    }

    #[test]
    fn only_transformers_have_capacity() {
        let producer = ResolvedRecipe {
            id: RecipeId(0),
            inputs: vec![],
            outputs: vec![(CommodityId(0), 5)],
            rate: 1,
            elasticity: 0.0,
        };
        let consumer = ResolvedRecipe {
            id: RecipeId(0),
            inputs: vec![(CommodityId(0), 2)],
            outputs: vec![],
            rate: 1,
            elasticity: 1.0,
        };
        assert!(has_capacity(&refiner()), "a refiner invests");
        assert!(!has_capacity(&producer), "a raw extractor is a fixed endowment");
        assert!(!has_capacity(&consumer), "a consumer is population demand");
    }

    #[test]
    fn recipe_margin_is_value_added() {
        let r = refiner();
        // At base prices: 1*80 - 2*30 = 20.
        let base = |c: CommodityId| if c == CommodityId(0) { 30 } else { 80 };
        assert_eq!(recipe_margin(&r, base), 20);
        // Dearer output widens the margin.
        let dear = |c: CommodityId| if c == CommodityId(0) { 30 } else { 120 };
        assert_eq!(recipe_margin(&r, dear), 120 - 60);
    }

    #[test]
    fn capacity_target_has_base_as_its_fixed_point() {
        // Equal margins -> nominal capacity.
        assert!((capacity_target(20, 20) - 1.0).abs() < 1e-9);
        // A fatter margin pulls capacity up (but sub-linearly: elasticity 0.5).
        let up = capacity_target(80, 20); // ratio 4 -> 4^0.5 = 2
        assert!((up - 2.0).abs() < 1e-9, "got {up}");
        // A thin margin pulls it down.
        assert!(capacity_target(5, 20) < 1.0);
        // A loss pulls to the floor.
        assert_eq!(capacity_target(-10, 20), MIN_CAPACITY);
        // A non-positive baseline disables the signal (consumers, loss-leaders).
        assert_eq!(capacity_target(50, 0), 1.0);
        assert_eq!(capacity_target(50, -5), 1.0);
    }

    #[test]
    fn capacity_target_is_clamped() {
        assert!(capacity_target(Money::MAX / 2, 1) <= MAX_CAPACITY);
        assert!(capacity_target(0, 20) >= MIN_CAPACITY);
    }

    #[test]
    fn eased_capacity_creeps_toward_target_and_clamps() {
        // One step closes only CAPACITY_SMOOTHING of the gap: capital is sticky.
        let next = eased_capacity(1.0, 2.0);
        assert!((next - (1.0 + CAPACITY_SMOOTHING)).abs() < 1e-9, "got {next}");
        assert!(next < 1.1, "a single tick barely moves capacity");
        // Repeated easing converges to the target.
        let mut c = 1.0;
        for _ in 0..2000 {
            c = eased_capacity(c, 3.0);
        }
        assert!((c - 3.0).abs() < 1e-3, "converged to target, got {c}");
        // Bounds hold even for an out-of-range target.
        assert_eq!(eased_capacity(MAX_CAPACITY, 100.0), MAX_CAPACITY);
        assert_eq!(eased_capacity(MIN_CAPACITY, 0.0), MIN_CAPACITY);
    }

    #[test]
    fn apply_recipe_respects_count_and_starvation() {
        let mut m = market(&[(0, 4), (1, 0)]);
        let recipe = ResolvedRecipe {
            id: RecipeId(0),
            inputs: vec![(CommodityId(0), 2)],
            outputs: vec![(CommodityId(1), 1)],
            rate: 10,
            elasticity: 0.0,
        };
        // Ask for 5 applications but only 2 worth of input exist -> 2 run.
        let applied = apply_recipe(&mut m, &recipe, 5);
        assert_eq!(applied, 2);
        assert_eq!(m.stock(CommodityId(1)), 2);
    }
}
