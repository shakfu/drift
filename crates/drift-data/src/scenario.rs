//! Scenario schema — parameters for a simulation run.
//!
//! A scenario is *not* content (it does not define the galaxy); it configures a
//! run over already-loaded content: the RNG seed, how long to run, and which NPC
//! traders populate the economy. Kept separate so the same galaxy can be exercised
//! by many scenarios (calm vs. shock, few vs. many traders).

use drift_core::Money;
use serde::{Deserialize, Serialize};

/// How to populate the galaxy with NPC traders at the start of a run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraderSpawn {
    /// Number of NPC traders to create.
    pub count: u32,
    /// Ship id every spawned trader flies.
    pub ship: String,
    /// Starting capital, in credits, per trader.
    pub starting_capital: Money,
}

/// Piracy settings for a run. Absent means no piracy (traders are never ambushed).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PiracyConfig {
    /// Ship id pirates fly.
    pub pirate_ship: String,
    /// Per-tick, per-pirate probability that a pirate present at a laden trader's
    /// destination intercepts it. More pirates at a system => more likely ambush.
    pub base_ambush_chance: f64,
    /// Maximum number of pirates that join a single ambush.
    pub max_pirates: u32,
    /// Ticks a destroyed trader is out of action before respawning.
    pub respawn_delay: u64,
    /// Target size of the persistent, roaming pirate fleet. Pirates are spawned at
    /// (and roam toward) systems with `danger > 0`; a galaxy with no dangerous
    /// systems has no pirates.
    pub fleet_size: u32,
    /// Credits paid to a trader for each pirate it destroys in a won fight.
    pub bounty: Money,
    /// Ticks between reinforcement checks that top the fleet back up to
    /// `fleet_size`.
    pub reinforce_interval: u64,
}

/// Convoy escorts: armed ships that fight alongside every trader when it is
/// ambushed. Absent means traders travel unescorted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EscortConfig {
    /// Ship id the escorts fly.
    pub ship: String,
    /// Number of escorts accompanying each trader.
    pub count: u32,
    /// Credits a trader pays for its escort each time it jumps — protection is a
    /// running cost, not a free good. `0` (the default) keeps escorts free.
    #[serde(default)]
    pub fee: Money,
}

/// A persistent navy fleet that patrols lawless space, hunts pirates, and
/// defends traders under ambush. Absent means no navy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NavyConfig {
    /// Ship id navy patrols fly.
    pub ship: String,
    /// Target size of the persistent navy fleet.
    pub fleet_size: u32,
    /// Ticks between reinforcement checks that top the fleet back up.
    pub reinforce_interval: u64,
    /// Upkeep paid per navy ship per tick, drawn from the public treasury. `0`
    /// (the default) makes the navy free to run.
    #[serde(default)]
    pub upkeep: Money,
    /// Treasury income per tick that funds the navy (an abstraction of the tax
    /// base). When upkeep outruns funding the treasury goes into deficit and
    /// reinforcement stalls, so an underfunded navy shrinks under attrition.
    #[serde(default)]
    pub funding: Money,
}

/// Delivery-contract settings: a board that posts cargo-run missions generated
/// from real market shortages. Absent means no contract board.
///
/// Contracts ride on the spot economy: each tick's generation looks for the
/// system most starved of a good (stock far below its equilibrium anchor) and
/// posts a mission to import it, rewarding a premium over the local spot price. A
/// player accepts a contract, acquires the goods, and delivers them before the
/// deadline — all through the ordinary command pipeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContractConfig {
    /// Maximum simultaneously-open (unaccepted) contracts on the board.
    pub max_open: u32,
    /// Ticks between generation attempts (a new contract is posted at most once
    /// per interval, and only while the board is below `max_open`).
    pub generation_interval: u64,
    /// Ticks a newly posted contract lasts before its delivery deadline.
    pub deadline_ticks: u64,
    /// Reward multiplier over the destination's spot value of the cargo
    /// (`reward = destination_price * quantity * reward_factor`). Above `1.0`
    /// pays a premium for guaranteed delivery.
    pub reward_factor: f64,
    /// Minimum shortfall (`equilibrium - stock`) at a system for a good before it
    /// is worth posting a delivery contract to import it.
    pub min_shortfall: u32,
    /// Cap on a single delivery contract's quantity, so it stays fulfillable by
    /// one ship even when the shortfall is large.
    pub max_quantity: u32,
    /// Pirates a bounty contract asks the holder to destroy. `0` disables bounty
    /// contracts.
    #[serde(default)]
    pub bounty_target: u32,
    /// Reward for completing a bounty contract.
    #[serde(default)]
    pub bounty_reward: Money,
    /// Base reward for a courier contract, scaled up by the route's danger. `0`
    /// disables courier contracts.
    #[serde(default)]
    pub courier_reward: Money,
}

/// Lending terms for a run: the bank a docked trader can borrow from. Absent means
/// no lending (loan commands are rejected). Interest is charged on the outstanding
/// balance every `accrual_interval` ticks, and the balance is due `term_ticks`
/// after the loan is taken; past due, the lender seizes the balance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoanConfig {
    /// Interest per accrual period, in basis points of the outstanding balance
    /// (e.g. `100` = 1% compounded each period).
    pub rate_bps: u32,
    /// Ticks between interest accruals.
    pub accrual_interval: u64,
    /// Ticks from origination until the balance is due in full.
    pub term_ticks: u64,
    /// Largest principal a single loan may be taken for.
    pub max_principal: Money,
    /// Most open loans one trader may carry at once.
    pub max_loans: u32,
}

/// Ship-loss insurance terms. Absent means no insurance is offered. A docked
/// trader pays `premium` for coverage lasting `term_ticks`; if it is destroyed by
/// pirates while covered, the policy pays `payout` (once) into its capital.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InsuranceConfig {
    /// Up-front cost of a policy.
    pub premium: Money,
    /// Compensation paid if the insured trader is destroyed while covered.
    pub payout: Money,
    /// Ticks a policy remains in force.
    pub term_ticks: u64,
}

/// Commodity-futures terms. Absent means no futures market. A docked trader opens
/// a cash-settled position (long or short) of up to `max_quantity` units at the
/// current spot price; at maturity (`term_ticks` later) it settles against the
/// galaxy reference price, crediting or debiting the difference. `fee` is the
/// commission to open one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FutureConfig {
    /// Commission charged to open a position.
    pub fee: Money,
    /// Ticks from open until the position settles.
    pub term_ticks: u64,
    /// Largest quantity a single position may cover.
    pub max_quantity: u32,
    /// Most open positions one trader may carry at once.
    pub max_futures: u32,
}

/// A complete run configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioDef {
    /// Human-readable name.
    pub name: String,
    /// RNG seed. Fixed here so a scenario is reproducible by default; the CLI may
    /// override it.
    pub seed: u64,
    /// Default number of ticks to run (CLI may override).
    pub ticks: u64,
    /// NPC trader population.
    pub traders: TraderSpawn,
    /// Optional piracy settings. `None` (the default) disables piracy entirely.
    #[serde(default)]
    pub piracy: Option<PiracyConfig>,
    /// How strongly traders avoid dangerous routes. A trade's profit is discounted
    /// by `clamp(danger * risk_aversion, 0, 1)` as the perceived chance of losing
    /// the cargo. `0` (the default) is risk-neutral: danger is ignored in routing.
    #[serde(default)]
    pub risk_aversion: f64,
    /// Optional convoy escorts. `None` (the default) = traders travel unescorted.
    #[serde(default)]
    pub escort: Option<EscortConfig>,
    /// Optional navy patrol fleet. `None` (the default) = no navy.
    #[serde(default)]
    pub navy: Option<NavyConfig>,
    /// Optional delivery-contract board. `None` (the default) = no contracts.
    #[serde(default)]
    pub contract: Option<ContractConfig>,
    /// Optional lending terms. `None` (the default) = no loans available.
    #[serde(default)]
    pub loan: Option<LoanConfig>,
    /// Optional ship-loss insurance. `None` (the default) = none offered.
    #[serde(default)]
    pub insurance: Option<InsuranceConfig>,
    /// Optional commodity-futures market. `None` (the default) = none offered.
    #[serde(default)]
    pub future: Option<FutureConfig>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenario_ron_roundtrips() {
        let def = ScenarioDef {
            name: "equilibrium".into(),
            seed: 42,
            ticks: 2000,
            traders: TraderSpawn {
                count: 24,
                ship: "core:cobra_mk3".into(),
                starting_capital: 1000,
            },
            piracy: Some(PiracyConfig {
                pirate_ship: "core:pirate".into(),
                base_ambush_chance: 0.05,
                max_pirates: 2,
                respawn_delay: 50,
                fleet_size: 12,
                bounty: 400,
                reinforce_interval: 20,
            }),
            risk_aversion: 1.5,
            escort: Some(EscortConfig {
                ship: "core:escort".into(),
                count: 1,
                fee: 50,
            }),
            navy: Some(NavyConfig {
                ship: "core:navy".into(),
                fleet_size: 6,
                reinforce_interval: 30,
                upkeep: 5,
                funding: 40,
            }),
            contract: Some(ContractConfig {
                max_open: 5,
                generation_interval: 25,
                deadline_ticks: 200,
                reward_factor: 1.2,
                min_shortfall: 50,
                max_quantity: 40,
                bounty_target: 3,
                bounty_reward: 4000,
                courier_reward: 800,
            }),
            loan: Some(LoanConfig {
                rate_bps: 100,
                accrual_interval: 50,
                term_ticks: 500,
                max_principal: 20000,
                max_loans: 3,
            }),
            insurance: Some(InsuranceConfig {
                premium: 500,
                payout: 4000,
                term_ticks: 300,
            }),
            future: Some(FutureConfig {
                fee: 100,
                term_ticks: 200,
                max_quantity: 50,
                max_futures: 3,
            }),
        };
        let text = ron::to_string(&def).unwrap();
        let back: ScenarioDef = ron::from_str(&text).unwrap();
        assert_eq!(def, back);
    }

    #[test]
    fn optional_fields_default_when_omitted() {
        let text = r#"(name: "s", seed: 1, ticks: 10, traders: (count: 0, ship: "", starting_capital: 0))"#;
        let s: ScenarioDef = ron::from_str(text).unwrap();
        assert_eq!(s.piracy, None);
        assert_eq!(s.risk_aversion, 0.0);
        assert_eq!(s.escort, None);
        assert_eq!(s.navy, None);
        assert_eq!(s.contract, None);
        assert_eq!(s.loan, None);
        assert_eq!(s.insurance, None);
        assert_eq!(s.future, None);
    }
}
