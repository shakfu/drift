//! The simulation world: markets + traders advanced by a deterministic tick.
//!
//! Tick phase order is **production -> price update -> trading**: each tick first
//! transforms stock via industries, then reprices every market from the new
//! stock, then lets traders act on those fresh prices. Trades change stock, which
//! the next tick's repricing reflects. The world owns its RNG so a run is fully
//! reproducible and a dumped [`Snapshot`] is resumable.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use drift_combat::{Combatant, Encounter, Outcome, Vec2};
use drift_core::{CommodityId, DetRng, Money, ShipId, SystemId, Tick};
use drift_data::ScenarioDef;
use drift_mods::Registry;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::command::{Command, CommandError, Owner, PlayerId};
use crate::contract::{Contract, ContractId, ContractKind, ContractState};
use crate::event::{EventCategory, SimEvent};
use crate::finance::{Future, Loan, LoanId, Policy};
use crate::market::{Market, MarketGood};
use crate::patrol::{Patrol, PatrolId, PatrolLocation};
use crate::pricing::PricingSet;
use drift_script::ScriptedPricing;
use crate::production::{apply_recipe, elastic_factor, response_signal, MAX_ELASTIC_FACTOR};
use crate::trader::{choose_trade, Trader, TraderId, TraderLocation};

/// Per-tick probability a docked patrol (pirate or navy) relocates to a
/// danger-weighted neighbor.
const ROAM_CHANCE: f64 = 0.12;

/// Most recent simulation events retained for the debug log (older ones drop off).
const EVENT_CAP: usize = 2000;

#[derive(Debug, Error)]
pub enum WorldError {
    #[error("scenario references unknown ship '{0}'")]
    UnknownShip(String),

    #[error("system '{system}' runs an industry using commodity '{commodity}', which it does not trade (add it to initial_stock)")]
    MissingIndustryCommodity { system: String, commodity: String },

    #[error("the galaxy has no systems")]
    NoSystems,
}

/// Resolved piracy settings for a run (the scenario's `pirate_ship` looked up to
/// a handle plus the numeric knobs).
#[derive(Debug, Clone)]
struct PiracyRuntime {
    pirate_ship: ShipId,
    base_ambush_chance: f64,
    max_pirates: u32,
    respawn_delay: u64,
    fleet_size: u32,
    bounty: Money,
    reinforce_interval: u64,
}

/// Resolved navy settings.
#[derive(Debug, Clone)]
struct NavyRuntime {
    navy_ship: ShipId,
    fleet_size: u32,
    reinforce_interval: u64,
    upkeep: Money,
    funding: Money,
}

/// Resolved escort settings.
#[derive(Debug, Clone)]
struct EscortRuntime {
    escort_ship: ShipId,
    count: u32,
    fee: Money,
}

/// Resolved delivery-contract settings (numeric knobs only; contract targets are
/// discovered from the live economy, so there is nothing to look up here). All
/// intervals are clamped to at least 1 to keep generation well-defined.
#[derive(Debug, Clone)]
struct ContractRuntime {
    max_open: u32,
    generation_interval: u64,
    deadline_ticks: u64,
    reward_factor: f64,
    min_shortfall: u32,
    max_quantity: u32,
    bounty_target: u32,
    bounty_reward: Money,
    courier_reward: Money,
}

/// Resolved lending terms (clamped where a zero would be degenerate).
#[derive(Debug, Clone)]
struct LoanRuntime {
    rate_bps: u32,
    accrual_interval: u64,
    term_ticks: u64,
    max_principal: Money,
    max_loans: u32,
}

/// Resolved insurance terms.
#[derive(Debug, Clone)]
struct InsuranceRuntime {
    premium: Money,
    payout: Money,
    term_ticks: u64,
}

/// Resolved futures-market terms.
#[derive(Debug, Clone)]
struct FutureRuntime {
    fee: Money,
    term_ticks: u64,
    max_quantity: u32,
    max_futures: u32,
}

/// Combat steps advanced per economy tick, so a battle plays out over several
/// ticks rather than resolving instantly.
const COMBAT_STEPS_PER_TICK: u64 = 25;
/// Hard cap on a single battle's length (matching the old instant-resolve cap): a
/// fight still undecided at the cap is completed with whoever is currently alive.
const MAX_COMBAT_STEPS: u64 = 500;

/// A battle spread across ticks: the [`Encounter`], its own RNG (so its evolution
/// is isolated from other per-tick randomness), how many steps it has run, and the
/// metadata to apply its outcome by stable id when it decides.
struct ActiveEncounter {
    encounter: Encounter,
    rng: DetRng,
    steps: u64,
    kind: EncounterKind,
}

/// A read-only view of a running battle for observers (snapshot/client): where it
/// is happening and the current state of its combatants. Owned and serializable,
/// so it rides the snapshot and the wire without exposing the world's live
/// encounter bookkeeping.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EncounterView {
    /// The system the battle is taking place at.
    pub system: SystemId,
    /// The combatants and their current positions/state.
    pub combatants: Vec<Combatant>,
}

/// What a running battle is, and how to map its combatants back to world agents.
enum EncounterKind {
    /// A laden trader ambushed near `dest`. Combatant order: the trader (index 0),
    /// its escorts, the navy defenders (`navy_offset..`), then the pirates
    /// (`pirate_offset..`). Navy and pirates are addressed by stable id.
    Ambush {
        trader: TraderId,
        dest: SystemId,
        navy: Vec<PatrolId>,
        pirates: Vec<PatrolId>,
        navy_offset: usize,
        pirate_offset: usize,
        respawn_delay: u64,
        bounty: Money,
    },
    /// The navy engaging pirates at `sys`. Combatant order: navy (`0..`) then
    /// pirates (`pirates_offset..`).
    Hunt {
        sys: SystemId,
        navy: Vec<PatrolId>,
        pirates: Vec<PatrolId>,
        pirates_offset: usize,
    },
}

/// Cumulative piracy tallies over a run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PiracyStats {
    /// Ambushes that were triggered.
    pub ambushes: u64,
    /// Traders destroyed by pirates.
    pub traders_lost: u64,
    /// Pirates destroyed by traders (in ambushes).
    pub pirates_destroyed: u64,
    /// Total bounty credits paid out to victorious traders.
    pub bounties_paid: Money,
    /// Pirates destroyed by the navy (on patrol, outside ambushes).
    pub pirates_suppressed: u64,
    /// Navy ships lost fighting pirates.
    pub navy_lost: u64,
    /// Total escort fees paid by traders for protection on their jumps.
    pub escort_fees_paid: Money,
    /// Total navy upkeep drawn from the treasury over the run.
    pub navy_upkeep: Money,
}

/// A serializable view of mutable world state (excludes the static registry).
/// Used for state dumps and determinism checks.
#[derive(Serialize)]
pub struct Snapshot<'a> {
    pub tick: Tick,
    pub rng: &'a DetRng,
    pub markets: &'a [Market],
    pub traders: &'a [Trader],
    /// Fractional production progress per system, per industry (elastic rates
    /// carry a remainder between ticks).
    pub progress: &'a [Vec<f64>],
    pub piracy: PiracyStats,
    pub pirates: &'a [Patrol],
    pub navy: &'a [Patrol],
    /// The live delivery-contract board.
    pub contracts: &'a [Contract],
    /// Outstanding loans.
    pub loans: &'a [Loan],
    /// Active insurance policies.
    pub policies: &'a [Policy],
    /// Open futures positions.
    pub futures: &'a [Future],
    /// Battles currently playing out (owned views, built on demand).
    pub encounters: Vec<EncounterView>,
    /// Next trader id to be assigned; part of state so a resumed run keeps ids
    /// unique.
    pub next_trader_id: u64,
    /// Next contract id to be assigned; part of state for the same reason.
    pub next_contract_id: u64,
    /// Next loan id to be assigned; part of state for the same reason.
    pub next_loan_id: u64,
}

/// The world. Shares the immutable [`Registry`] (static content) via `Arc` and
/// owns the mutable simulation state. Owning (rather than borrowing) the registry
/// lets the world live in a long-lived host such as a UI app or a server session.
pub struct World {
    registry: Arc<Registry>,
    tick: Tick,
    rng: DetRng,
    markets: Vec<Market>,
    traders: Vec<Trader>,
    /// `progress[system][industry]` accumulates fractional applications so that
    /// price-scaled (non-integer) throughput stays smooth and deterministic.
    progress: Vec<Vec<f64>>,
    /// Resolved piracy settings, or `None` when the scenario disables piracy.
    piracy: Option<PiracyRuntime>,
    piracy_stats: PiracyStats,
    /// The persistent, roaming pirate fleet.
    pirates: Vec<Patrol>,
    /// Resolved navy settings, or `None` when there is no navy.
    navy_runtime: Option<NavyRuntime>,
    /// The persistent, roaming navy fleet.
    navy: Vec<Patrol>,
    /// Battles currently playing out across ticks (see [`ActiveEncounter`]).
    encounters: Vec<ActiveEncounter>,
    /// Monotonic source of stable patrol ids; never decreases, ids never reused.
    next_patrol_id: u64,
    /// Resolved escort settings, or `None` when traders travel unescorted.
    escort: Option<EscortRuntime>,
    /// Public treasury that funds the navy: income accrues each tick and upkeep is
    /// drawn from it. A deficit stalls navy reinforcement.
    treasury: Money,
    /// Compiled pricing scripts, indexed by [`PricingStrategy::Scripted`]. Static
    /// content (re-supplied at construction), so it is not part of the snapshot.
    pricing_scripts: Vec<ScriptedPricing>,
    /// How strongly traders discount profit by destination danger (0 = neutral).
    risk_aversion: f64,
    /// Resolved contract settings, or `None` when the scenario has no board.
    contract_runtime: Option<ContractRuntime>,
    /// The live delivery-contract board (open and accepted contracts).
    contracts: Vec<Contract>,
    /// Monotonic source of stable contract ids; never decreases, ids never reused.
    next_contract_id: u64,
    /// Resolved lending terms, or `None` when the scenario offers no loans.
    loan_runtime: Option<LoanRuntime>,
    /// Outstanding loans against traders' capital.
    loans: Vec<Loan>,
    /// Monotonic source of stable loan ids; never decreases, ids never reused.
    next_loan_id: u64,
    /// Resolved insurance terms, or `None` when none is offered.
    insurance_runtime: Option<InsuranceRuntime>,
    /// Active insurance policies (one per insured trader).
    policies: Vec<Policy>,
    /// Resolved futures terms, or `None` when there is no futures market.
    future_runtime: Option<FutureRuntime>,
    /// Open, cash-settled futures positions.
    futures: Vec<Future>,
    /// Player commands queued for the next tick (drained in `command_phase`).
    commands: Vec<Command>,
    commands_applied: u64,
    commands_rejected: u64,
    /// Errors from the most recent `command_phase` (ephemeral UI feedback; not
    /// simulation state, so excluded from the snapshot).
    last_errors: Vec<CommandError>,
    /// Monotonic source of stable trader ids; never decreases, ids never reused.
    next_trader_id: u64,
    /// Bounded, deterministic log of notable happenings (debug/observability;
    /// ephemeral, not part of the snapshot).
    events: VecDeque<SimEvent>,
}

impl World {
    /// Build a world from linked content and a scenario, seeded by `seed`.
    ///
    /// Takes shared ownership of the [`Registry`] via `Arc` (clone it cheaply to
    /// keep a handle for rendering/tooling). Each system's `pricing` name was
    /// validated at link time, so it is resolved here via the same strategy set.
    /// Traders are placed on random systems using the seeded RNG.
    pub fn new(
        registry: Arc<Registry>,
        scenario: &ScenarioDef,
        seed: u64,
        pricing: &PricingSet,
    ) -> Result<Self, WorldError> {
        if registry.system_count() == 0 {
            return Err(WorldError::NoSystems);
        }

        // The compiled pricing scripts are static content; keep a handle so
        // repricing can dispatch scripted strategies each tick.
        let pricing_scripts: Vec<ScriptedPricing> = pricing.scripts().to_vec();

        // --- markets ---
        let mut markets = Vec::with_capacity(registry.system_count());
        for sys in registry.systems() {
            let strategy = *pricing
                .resolve(&sys.pricing)
                .expect("pricing validated at link time");

            let mut goods = std::collections::BTreeMap::new();
            for &(commodity, qty) in &sys.initial_stock {
                let def = registry.commodity(commodity);
                let price =
                    strategy.price(&pricing_scripts, def.base_price, qty, qty, def.elasticity);
                goods.insert(
                    commodity,
                    MarketGood {
                        stock: qty,
                        equilibrium: qty,
                        price,
                    },
                );
            }

            // Every commodity an industry touches must be tradeable here, or the
            // production phase would silently drop output / never run.
            for &rid in &sys.industries {
                let recipe = registry.recipe(rid);
                for (c, _) in recipe.inputs.iter().chain(recipe.outputs.iter()) {
                    if !goods.contains_key(c) {
                        return Err(WorldError::MissingIndustryCommodity {
                            system: registry.system_name(sys.id).to_string(),
                            commodity: registry.commodity_name(*c).to_string(),
                        });
                    }
                }
            }

            markets.push(Market {
                system: sys.id,
                pricing: strategy,
                goods,
            });
        }

        // --- traders ---
        let mut rng = DetRng::from_seed(seed);
        let nsys = registry.system_count();
        let mut next_trader_id = 0u64;
        let mut traders = Vec::with_capacity(scenario.traders.count as usize);
        if scenario.traders.count > 0 {
            // Only resolve the ship when traders are actually spawned, so a
            // zero-trader probe (e.g. content validation) needs no ship.
            let ship = registry
                .ship_id(&scenario.traders.ship)
                .ok_or_else(|| WorldError::UnknownShip(scenario.traders.ship.clone()))?;
            for _ in 0..scenario.traders.count {
                let at = SystemId(rng.range_usize(0, nsys) as u32);
                let id = TraderId(next_trader_id);
                next_trader_id += 1;
                traders.push(Trader::new(id, ship, scenario.traders.starting_capital, at));
            }
        }

        let progress: Vec<Vec<f64>> = registry
            .systems()
            .map(|s| vec![0.0f64; s.industries.len()])
            .collect();

        // --- piracy ---
        // Stable ids for every patrol, so running battles can refer to their
        // participants across ticks.
        let mut next_patrol_id = 0u64;
        let mut pirates = Vec::new();
        let piracy = match &scenario.piracy {
            None => None,
            Some(cfg) => {
                let pirate_ship = registry
                    .ship_id(&cfg.pirate_ship)
                    .ok_or_else(|| WorldError::UnknownShip(cfg.pirate_ship.clone()))?;
                let runtime = PiracyRuntime {
                    pirate_ship,
                    base_ambush_chance: cfg.base_ambush_chance,
                    max_pirates: cfg.max_pirates.max(1),
                    respawn_delay: cfg.respawn_delay,
                    fleet_size: cfg.fleet_size,
                    bounty: cfg.bounty,
                    reinforce_interval: cfg.reinforce_interval.max(1),
                };
                pirates = spawn_fleet(
                    &registry,
                    &mut rng,
                    pirate_ship,
                    runtime.fleet_size,
                    &mut next_patrol_id,
                );
                Some(runtime)
            }
        };

        // --- navy ---
        let mut navy = Vec::new();
        let navy_runtime = match &scenario.navy {
            None => None,
            Some(cfg) => {
                let navy_ship = registry
                    .ship_id(&cfg.ship)
                    .ok_or_else(|| WorldError::UnknownShip(cfg.ship.clone()))?;
                let runtime = NavyRuntime {
                    navy_ship,
                    fleet_size: cfg.fleet_size,
                    reinforce_interval: cfg.reinforce_interval.max(1),
                    upkeep: cfg.upkeep,
                    funding: cfg.funding,
                };
                navy = spawn_fleet(
                    &registry,
                    &mut rng,
                    navy_ship,
                    runtime.fleet_size,
                    &mut next_patrol_id,
                );
                Some(runtime)
            }
        };

        // --- escorts ---
        let escort = match &scenario.escort {
            None => None,
            Some(cfg) => {
                let escort_ship = registry
                    .ship_id(&cfg.ship)
                    .ok_or_else(|| WorldError::UnknownShip(cfg.ship.clone()))?;
                Some(EscortRuntime {
                    escort_ship,
                    count: cfg.count,
                    fee: cfg.fee,
                })
            }
        };

        // --- contracts ---
        let contract_runtime = scenario.contract.as_ref().map(|cfg| ContractRuntime {
            max_open: cfg.max_open,
            generation_interval: cfg.generation_interval.max(1),
            deadline_ticks: cfg.deadline_ticks.max(1),
            reward_factor: cfg.reward_factor,
            min_shortfall: cfg.min_shortfall,
            max_quantity: cfg.max_quantity.max(1),
            bounty_target: cfg.bounty_target,
            bounty_reward: cfg.bounty_reward,
            courier_reward: cfg.courier_reward,
        });

        // --- loans ---
        let loan_runtime = scenario.loan.as_ref().map(|cfg| LoanRuntime {
            rate_bps: cfg.rate_bps,
            accrual_interval: cfg.accrual_interval.max(1),
            term_ticks: cfg.term_ticks,
            max_principal: cfg.max_principal,
            max_loans: cfg.max_loans,
        });

        // --- insurance & futures ---
        let insurance_runtime = scenario.insurance.as_ref().map(|cfg| InsuranceRuntime {
            premium: cfg.premium,
            payout: cfg.payout,
            term_ticks: cfg.term_ticks.max(1),
        });
        let future_runtime = scenario.future.as_ref().map(|cfg| FutureRuntime {
            fee: cfg.fee,
            term_ticks: cfg.term_ticks.max(1),
            max_quantity: cfg.max_quantity.max(1),
            max_futures: cfg.max_futures,
        });

        Ok(World {
            registry,
            tick: Tick::ZERO,
            rng,
            markets,
            traders,
            progress,
            piracy,
            piracy_stats: PiracyStats::default(),
            pirates,
            navy_runtime,
            navy,
            encounters: Vec::new(),
            next_patrol_id,
            escort,
            treasury: 0,
            pricing_scripts,
            risk_aversion: scenario.risk_aversion,
            contract_runtime,
            contracts: Vec::new(),
            next_contract_id: 0,
            loan_runtime,
            loans: Vec::new(),
            next_loan_id: 0,
            insurance_runtime,
            policies: Vec::new(),
            future_runtime,
            futures: Vec::new(),
            commands: Vec::new(),
            commands_applied: 0,
            commands_rejected: 0,
            last_errors: Vec::new(),
            next_trader_id,
            events: VecDeque::new(),
        })
    }

    /// Record a simulation event (trimming the oldest once the buffer is full).
    /// `system` is where it happened, if that is meaningful (`None` otherwise).
    fn log_event(&mut self, category: EventCategory, system: Option<SystemId>, message: String) {
        self.events.push_back(SimEvent {
            tick: self.tick,
            category,
            system,
            message,
        });
        if self.events.len() > EVENT_CAP {
            self.events.pop_front();
        }
    }

    pub fn tick_count(&self) -> Tick {
        self.tick
    }
    pub fn markets(&self) -> &[Market] {
        &self.markets
    }
    pub fn traders(&self) -> &[Trader] {
        &self.traders
    }
    pub fn registry(&self) -> &Registry {
        &self.registry
    }
    /// A cloned `Arc` handle to the shared registry (for a host that wants to keep
    /// its own reference alongside the world).
    pub fn registry_arc(&self) -> Arc<Registry> {
        Arc::clone(&self.registry)
    }
    pub fn piracy_stats(&self) -> PiracyStats {
        self.piracy_stats
    }
    pub fn pirates(&self) -> &[Patrol] {
        &self.pirates
    }
    pub fn navy(&self) -> &[Patrol] {
        &self.navy
    }
    /// The live delivery-contract board (open and accepted contracts).
    pub fn contracts(&self) -> &[Contract] {
        &self.contracts
    }
    /// Outstanding loans against traders' capital.
    pub fn loans(&self) -> &[Loan] {
        &self.loans
    }
    /// Active insurance policies.
    pub fn policies(&self) -> &[Policy] {
        &self.policies
    }
    /// Open futures positions.
    pub fn futures(&self) -> &[Future] {
        &self.futures
    }
    /// Number of battles currently playing out across ticks.
    pub fn active_encounters(&self) -> usize {
        self.encounters.len()
    }
    /// Read-only views of the running battles, for observers and clients.
    pub fn encounter_views(&self) -> Vec<EncounterView> {
        self.encounters
            .iter()
            .map(|e| {
                let system = match &e.kind {
                    EncounterKind::Ambush { dest, .. } => *dest,
                    EncounterKind::Hunt { sys, .. } => *sys,
                };
                EncounterView { system, combatants: e.encounter.combatants().to_vec() }
            })
            .collect()
    }
    /// The public treasury balance that funds the navy (may be in deficit).
    pub fn treasury(&self) -> Money {
        self.treasury
    }
    pub fn commands_applied(&self) -> u64 {
        self.commands_applied
    }
    pub fn commands_rejected(&self) -> u64 {
        self.commands_rejected
    }
    /// Errors from commands rejected in the most recent tick's `command_phase`.
    /// Ephemeral UI feedback; cleared at the start of each `command_phase`.
    pub fn last_command_errors(&self) -> &[CommandError] {
        &self.last_errors
    }
    /// The recorded simulation events, oldest first (a bounded recent tail).
    pub fn events(
        &self,
    ) -> impl DoubleEndedIterator<Item = &SimEvent> + ExactSizeIterator {
        self.events.iter()
    }

    /// Enqueue a player command for the next tick. The single input entry point —
    /// local now, over the network in a multiplayer server. Commands are validated
    /// and applied in `command_phase`, not on receipt, so ordering is deterministic.
    pub fn queue_command(&mut self, command: Command) {
        self.commands.push(command);
    }

    pub fn snapshot(&self) -> Snapshot<'_> {
        Snapshot {
            tick: self.tick,
            rng: &self.rng,
            markets: &self.markets,
            traders: &self.traders,
            progress: &self.progress,
            piracy: self.piracy_stats,
            pirates: &self.pirates,
            navy: &self.navy,
            contracts: &self.contracts,
            loans: &self.loans,
            policies: &self.policies,
            futures: &self.futures,
            encounters: self.encounter_views(),
            next_trader_id: self.next_trader_id,
            next_contract_id: self.next_contract_id,
            next_loan_id: self.next_loan_id,
        }
    }

    /// Advance the world by exactly one tick.
    ///
    /// Phase order: commands -> production -> price -> pirate movement -> navy
    /// (patrol + open hunts) -> piracy (open ambushes) -> combat (advance running
    /// battles) -> trading -> contracts -> finance. The navy and piracy phases
    /// *open* battles that the combat phase then advances a few steps per tick,
    /// applying casualties on completion; a trader in an unresolved fight is frozen
    /// (it neither arrives nor trades) until the fight ends; contracts generate and
    /// expire against the tick's settled markets; and finance accrues loan interest
    /// and calls overdue loans last.
    pub fn tick(&mut self) {
        self.command_phase();
        self.production_phase();
        self.price_phase();
        self.pirate_phase();
        self.navy_phase();
        self.piracy_phase();
        self.combat_phase();
        self.trading_phase();
        self.contract_phase();
        self.finance_phase();
        self.tick = self.tick.next();
    }

    /// Run `n` ticks.
    pub fn run(&mut self, n: u64) {
        for _ in 0..n {
            self.tick();
        }
    }

    /// Drain and apply the queued player commands, in submission order. Rejected
    /// commands (invalid input) are counted, not fatal. A server would impose a
    /// canonical order across players before this runs; single-player order is the
    /// local submission order.
    fn command_phase(&mut self) {
        self.last_errors.clear();
        let commands = std::mem::take(&mut self.commands);
        for command in commands {
            match self.apply_command(command) {
                Ok(()) => self.commands_applied += 1,
                Err(e) => {
                    self.commands_rejected += 1;
                    self.last_errors.push(e);
                }
            }
        }
    }

    /// Validate and apply a single command against the world. Every precondition
    /// (ownership, reachability, funds, stock, hold capacity) is checked, because
    /// commands are untrusted input. Traders are addressed by stable [`TraderId`],
    /// resolved to the current slot at apply time.
    fn apply_command(&mut self, command: Command) -> Result<(), CommandError> {
        let reg = self.registry.clone();
        match command {
            Command::Spawn {
                player,
                ship,
                at,
                capital,
            } => {
                if ship.index() >= reg.ship_count() {
                    return Err(CommandError::UnknownShip);
                }
                if at.index() >= reg.system_count() {
                    return Err(CommandError::InvalidSystem);
                }
                let id = self.fresh_trader_id();
                self.traders.push(Trader::owned(id, ship, capital, at, player));
                Ok(())
            }

            Command::Despawn { player, trader } => {
                let idx = self.owned_trader_index(trader, player)?;
                self.traders.remove(idx); // order-preserving; ids stay valid
                Ok(())
            }

            Command::Jump {
                player,
                trader,
                dest,
            } => {
                let idx = self.owned_trader_index(trader, player)?;
                let sys = self.docked_system(idx)?;
                if dest.index() >= reg.system_count() {
                    return Err(CommandError::InvalidSystem);
                }
                if !reg.system(sys).connections.contains(&dest) {
                    return Err(CommandError::Unreachable);
                }
                let ship = self.traders[idx].ship;
                let travel = self.travel_ticks(sys, dest, ship);
                self.traders[idx].location = TraderLocation::InTransit {
                    origin: sys,
                    dest,
                    departure: self.tick,
                    arrival: Tick(self.tick.0 + travel),
                };
                self.charge_escort(idx);
                Ok(())
            }

            Command::Buy {
                player,
                trader,
                commodity,
                qty,
            } => {
                if qty == 0 {
                    return Err(CommandError::ZeroQuantity);
                }
                let idx = self.owned_trader_index(trader, player)?;
                let sys = self.docked_system(idx)?;
                let market = &self.markets[sys.index()];
                let price = market.price(commodity).ok_or(CommandError::UnknownGood)?;
                if market.stock(commodity) < qty {
                    return Err(CommandError::InsufficientStock);
                }
                let cost = price * qty as i64;
                if self.traders[idx].capital < cost {
                    return Err(CommandError::InsufficientFunds);
                }
                // Check hold capacity (in mass units).
                let unit_mass = reg.commodity(commodity).unit_mass;
                let capacity = reg.ship(self.traders[idx].ship).cargo_capacity;
                let used: u32 = self.traders[idx]
                    .cargo
                    .iter()
                    .map(|(c, q)| q * reg.commodity(*c).unit_mass)
                    .sum();
                if used + qty * unit_mass > capacity {
                    return Err(CommandError::OverCapacity);
                }

                self.markets[sys.index()].try_remove(commodity, qty);
                self.traders[idx].capital -= cost;
                *self.traders[idx].cargo.entry(commodity).or_insert(0) += qty;
                Ok(())
            }

            Command::Sell {
                player,
                trader,
                commodity,
                qty,
            } => {
                if qty == 0 {
                    return Err(CommandError::ZeroQuantity);
                }
                let idx = self.owned_trader_index(trader, player)?;
                let sys = self.docked_system(idx)?;
                let held = self.traders[idx].cargo.get(&commodity).copied().unwrap_or(0);
                if held < qty {
                    return Err(CommandError::InsufficientCargo);
                }
                let price = self.markets[sys.index()]
                    .price(commodity)
                    .ok_or(CommandError::UnknownGood)?;

                self.markets[sys.index()].add(commodity, qty);
                self.traders[idx].capital += price * qty as i64;
                let remaining = held - qty;
                if remaining == 0 {
                    self.traders[idx].cargo.remove(&commodity);
                } else {
                    self.traders[idx].cargo.insert(commodity, remaining);
                }
                Ok(())
            }

            Command::AcceptContract {
                player,
                trader,
                contract,
            } => {
                // The trader must exist and belong to this player.
                self.owned_trader_index(trader, player)?;
                let ci = self.contract_index(contract)?;
                if !self.contracts[ci].is_open() {
                    return Err(CommandError::ContractUnavailable);
                }
                self.contracts[ci].state = ContractState::Accepted { player, trader };
                let dest = self.contracts[ci].destination;
                self.log_event(
                    EventCategory::System,
                    Some(dest),
                    format!(
                        "Contract #{} accepted by trader #{}",
                        contract.0, trader.0
                    ),
                );
                Ok(())
            }

            Command::FulfillContract {
                player,
                trader,
                contract,
            } => {
                let idx = self.owned_trader_index(trader, player)?;
                let ci = self.contract_index(contract)?;
                // Only the accepting trader may deliver it.
                if self.contracts[ci].holder() != Some((player, trader)) {
                    return Err(CommandError::ContractNotHeld);
                }
                // The deadline tick itself is still deliverable; past it is not.
                if self.tick > self.contracts[ci].deadline {
                    return Err(CommandError::ContractExpired);
                }
                let sys = self.docked_system(idx)?;
                if sys != self.contracts[ci].destination {
                    return Err(CommandError::WrongDestination);
                }

                // The kind's completion condition. For a delivery, the required
                // cargo must be aboard; for a bounty, enough kills must have
                // accrued; a courier only needs to have arrived.
                let held = self.contracts[ci].cargo().map_or(0, |(commodity, _)| {
                    self.traders[idx].cargo.get(&commodity).copied().unwrap_or(0)
                });
                if !self.contracts[ci].condition_met(held) {
                    return match self.contracts[ci].kind {
                        ContractKind::Delivery { .. } => Err(CommandError::InsufficientCargo),
                        ContractKind::Bounty { .. } => Err(CommandError::BountyIncomplete),
                        ContractKind::Courier => unreachable!("courier condition is always met"),
                    };
                }

                // Delivery consumes the cargo (delivered to the issuer, not sold
                // into the spot market). Courier and bounty carry no goods.
                if let Some((commodity, quantity)) = self.contracts[ci].cargo() {
                    let remaining = held - quantity;
                    if remaining == 0 {
                        self.traders[idx].cargo.remove(&commodity);
                    } else {
                        self.traders[idx].cargo.insert(commodity, remaining);
                    }
                }
                let reward = self.contracts[ci].reward;
                self.traders[idx].capital += reward;
                self.contracts.remove(ci);
                self.log_event(
                    EventCategory::System,
                    Some(sys),
                    format!(
                        "Contract #{} fulfilled by trader #{} (+{}cr)",
                        contract.0, trader.0, reward
                    ),
                );
                Ok(())
            }

            Command::TakeLoan {
                player,
                trader,
                principal,
            } => {
                let rt = self
                    .loan_runtime
                    .clone()
                    .ok_or(CommandError::LendingUnavailable)?;
                let idx = self.owned_trader_index(trader, player)?;
                self.docked_system(idx)?; // borrow at a station only
                if principal <= 0 {
                    return Err(CommandError::ZeroQuantity);
                }
                if principal > rt.max_principal {
                    return Err(CommandError::LoanTooLarge);
                }
                let held = self.loans.iter().filter(|l| l.borrower == trader).count() as u32;
                if held >= rt.max_loans {
                    return Err(CommandError::TooManyLoans);
                }

                let id = self.fresh_loan_id();
                self.loans.push(Loan {
                    id,
                    player,
                    borrower: trader,
                    principal,
                    outstanding: principal,
                    due: Tick(self.tick.0 + rt.term_ticks),
                });
                self.traders[idx].capital += principal;
                self.log_event(
                    EventCategory::System,
                    None,
                    format!(
                        "Loan #{} of {principal}cr taken by trader #{}",
                        id.0, trader.0
                    ),
                );
                Ok(())
            }

            Command::RepayLoan {
                player,
                trader,
                loan,
                amount,
            } => {
                let idx = self.owned_trader_index(trader, player)?;
                if amount <= 0 {
                    return Err(CommandError::ZeroQuantity);
                }
                let li = self.loan_index(loan)?;
                if !self.loans[li].held_by(player, trader) {
                    return Err(CommandError::LoanNotHeld);
                }
                // Pay no more than is owed, and no more than is on hand.
                let pay = amount.min(self.loans[li].outstanding);
                if self.traders[idx].capital < pay {
                    return Err(CommandError::InsufficientFunds);
                }
                self.traders[idx].capital -= pay;
                self.loans[li].outstanding -= pay;
                let remaining = self.loans[li].outstanding;
                if remaining == 0 {
                    self.loans.remove(li);
                }
                self.log_event(
                    EventCategory::System,
                    None,
                    format!(
                        "Loan #{} repaid {pay}cr by trader #{} ({remaining}cr remaining)",
                        loan.0, trader.0
                    ),
                );
                Ok(())
            }

            Command::BuyInsurance { player, trader } => {
                let rt = self
                    .insurance_runtime
                    .clone()
                    .ok_or(CommandError::InsuranceUnavailable)?;
                let idx = self.owned_trader_index(trader, player)?;
                self.docked_system(idx)?;
                if self.policies.iter().any(|p| p.insured == trader) {
                    return Err(CommandError::AlreadyInsured);
                }
                if self.traders[idx].capital < rt.premium {
                    return Err(CommandError::InsufficientFunds);
                }
                self.traders[idx].capital -= rt.premium;
                self.policies.push(Policy {
                    player,
                    insured: trader,
                    payout: rt.payout,
                    expiry: Tick(self.tick.0 + rt.term_ticks),
                });
                self.log_event(
                    EventCategory::System,
                    None,
                    format!(
                        "Trader #{} insured for {}cr (premium {}cr)",
                        trader.0, rt.payout, rt.premium
                    ),
                );
                Ok(())
            }

            Command::OpenFuture {
                player,
                trader,
                commodity,
                qty,
                side,
            } => {
                let rt = self
                    .future_runtime
                    .clone()
                    .ok_or(CommandError::FuturesUnavailable)?;
                let idx = self.owned_trader_index(trader, player)?;
                self.docked_system(idx)?;
                if qty == 0 {
                    return Err(CommandError::ZeroQuantity);
                }
                if qty > rt.max_quantity {
                    return Err(CommandError::FutureTooLarge);
                }
                let held = self.futures.iter().filter(|f| f.holder == trader).count() as u32;
                if held >= rt.max_futures {
                    return Err(CommandError::TooManyFutures);
                }
                // Lock the strike at the current galaxy reference (spot) price.
                let strike = self
                    .reference_price(commodity)
                    .ok_or(CommandError::UnknownGood)?;
                if self.traders[idx].capital < rt.fee {
                    return Err(CommandError::InsufficientFunds);
                }
                self.traders[idx].capital -= rt.fee;
                self.futures.push(Future {
                    player,
                    holder: trader,
                    commodity,
                    quantity: qty,
                    side,
                    strike,
                    maturity: Tick(self.tick.0 + rt.term_ticks),
                });
                self.log_event(
                    EventCategory::System,
                    None,
                    format!(
                        "Trader #{} opened {:?} {qty} {} future @ {strike}cr (fee {}cr)",
                        trader.0,
                        side,
                        reg.commodity_name(commodity),
                        rt.fee
                    ),
                );
                Ok(())
            }

            Command::DestroyedPirate { player, trader, pirate } => {
                let idx = self.owned_trader_index(trader, player)?;
                let pi = self
                    .pirates
                    .iter()
                    .position(|p| p.id == pirate)
                    .ok_or(CommandError::UnknownPatrol)?;
                let at = self.pirates[pi].docked_at();
                self.pirates.remove(pi);
                self.piracy_stats.pirates_destroyed += 1;

                let bounty = self.piracy.as_ref().map_or(0, |p| p.bounty);
                self.traders[idx].capital += bounty;
                self.piracy_stats.bounties_paid += bounty;

                // Credit a bounty contract this trader holds (one kill), noting a
                // quota just met.
                let mut completed: Vec<u64> = Vec::new();
                for c in &mut self.contracts {
                    let held = matches!(
                        c.state,
                        ContractState::Accepted { trader: ct, .. } if ct == trader
                    );
                    if held {
                        if let ContractKind::Bounty { target, progress } = &mut c.kind {
                            let before = *progress;
                            *progress = (before + 1).min(*target);
                            if before < *target && *progress >= *target {
                                completed.push(c.id.0);
                            }
                        }
                    }
                }
                self.log_event(
                    EventCategory::Combat,
                    at,
                    format!("Trader #{} destroyed a pirate (+{bounty}cr)", trader.0),
                );
                for cid in completed {
                    self.log_event(
                        EventCategory::System,
                        at,
                        format!("Bounty #{cid} quota met by trader #{}", trader.0),
                    );
                }
                Ok(())
            }

            Command::TraderDestroyed { player, trader } => {
                let idx = self.owned_trader_index(trader, player)?;
                let respawn_delay = self.piracy.as_ref().map_or(50, |p| p.respawn_delay);
                let at = match self.traders[idx].location {
                    TraderLocation::Docked(s) => Some(s),
                    TraderLocation::InTransit { dest, .. } => Some(dest),
                    TraderLocation::Destroyed { .. } => None,
                };
                self.piracy_stats.traders_lost += 1;
                self.traders[idx].cargo.clear();
                self.traders[idx].location = TraderLocation::Destroyed {
                    respawn: Tick(self.tick.0 + respawn_delay),
                };
                self.log_event(
                    EventCategory::Piracy,
                    at,
                    format!("Trader #{} destroyed in combat, cargo lost", trader.0),
                );
                if let Some(pi) = self.policies.iter().position(|p| p.insured == trader) {
                    let payout = self.policies[pi].payout;
                    self.policies.remove(pi);
                    self.traders[idx].capital += payout;
                    self.log_event(
                        EventCategory::System,
                        at,
                        format!("Insurance paid trader #{} {payout}cr for the loss", trader.0),
                    );
                }
                Ok(())
            }
        }
    }

    /// The galaxy reference price of a commodity: the mean spot price across every
    /// market that trades it, or `None` if none does.
    fn reference_price(&self, commodity: CommodityId) -> Option<Money> {
        let mut sum = 0i64;
        let mut n = 0i64;
        for m in &self.markets {
            if let Some(p) = m.price(commodity) {
                sum += p;
                n += 1;
            }
        }
        (n > 0).then(|| sum / n)
    }

    /// Allocate the next stable trader id.
    fn fresh_trader_id(&mut self) -> TraderId {
        let id = TraderId(self.next_trader_id);
        self.next_trader_id += 1;
        id
    }

    /// Resolve a `TraderId` to its current slot, checking it exists and is owned by
    /// `player`. A stale id (its trader removed) resolves to `UnknownTrader`.
    fn owned_trader_index(
        &self,
        id: TraderId,
        player: PlayerId,
    ) -> Result<usize, CommandError> {
        let idx = self
            .traders
            .iter()
            .position(|t| t.id == id)
            .ok_or(CommandError::UnknownTrader)?;
        if self.traders[idx].owner != Owner::Player(player) {
            return Err(CommandError::NotOwner);
        }
        Ok(idx)
    }

    /// Charge the trader at index `ti` its escort fee (protection is a running
    /// cost paid on every jump), if escorts are configured. No-op otherwise.
    fn charge_escort(&mut self, ti: usize) {
        let fee = self.escort.as_ref().map_or(0, |e| e.fee);
        if fee != 0 {
            self.traders[ti].capital -= fee;
            self.piracy_stats.escort_fees_paid += fee;
        }
    }

    /// The system a trader is docked at, or `NotDocked`.
    fn docked_system(&self, idx: usize) -> Result<SystemId, CommandError> {
        match self.traders[idx].location {
            TraderLocation::Docked(sys) => Ok(sys),
            _ => Err(CommandError::NotDocked),
        }
    }

    /// Allocate the next stable contract id.
    fn fresh_contract_id(&mut self) -> ContractId {
        let id = ContractId(self.next_contract_id);
        self.next_contract_id += 1;
        id
    }

    /// Resolve a `ContractId` to its current slot on the board. A stale id (its
    /// contract fulfilled or expired, hence removed) resolves to `UnknownContract`.
    fn contract_index(&self, id: ContractId) -> Result<usize, CommandError> {
        self.contracts
            .iter()
            .position(|c| c.id == id)
            .ok_or(CommandError::UnknownContract)
    }

    /// Allocate the next stable loan id.
    fn fresh_loan_id(&mut self) -> LoanId {
        let id = LoanId(self.next_loan_id);
        self.next_loan_id += 1;
        id
    }

    /// Resolve a `LoanId` to its current slot. A stale id (its loan repaid or
    /// called, hence removed) resolves to `UnknownLoan`.
    fn loan_index(&self, id: LoanId) -> Result<usize, CommandError> {
        self.loans
            .iter()
            .position(|l| l.id == id)
            .ok_or(CommandError::UnknownLoan)
    }

    /// Advance the financial layer: accrue and call loans, lapse expired insurance
    /// policies, and settle matured futures. Each section is a no-op without its
    /// runtime or with an empty book.
    fn finance_phase(&mut self) {
        let now = self.tick;

        // --- Loans: compound interest, then call overdue loans (strictly after
        //     the due tick, as with contracts), seizing the balance. ---
        if let Some(rt) = self.loan_runtime.clone() {
            if !self.loans.is_empty() {
                if now.get() > 0 && now.get().is_multiple_of(rt.accrual_interval) {
                    for loan in &mut self.loans {
                        let interest = loan.outstanding.saturating_mul(rt.rate_bps as i64) / 10_000;
                        loan.outstanding = loan.outstanding.saturating_add(interest);
                    }
                }
                let mut called: Vec<(u64, TraderId, Money)> = Vec::new();
                self.loans.retain(|loan| {
                    if now > loan.due {
                        called.push((loan.id.0, loan.borrower, loan.outstanding));
                        false
                    } else {
                        true
                    }
                });
                for (id, borrower, balance) in called {
                    if let Some(t) = self.traders.iter_mut().find(|t| t.id == borrower) {
                        t.capital -= balance;
                    }
                    self.log_event(
                        EventCategory::System,
                        None,
                        format!("Loan #{id} called: {balance}cr seized from trader #{}", borrower.0),
                    );
                }
            }
        }

        // --- Insurance: coverage lapses strictly after its expiry tick. ---
        if !self.policies.is_empty() {
            self.policies.retain(|p| now <= p.expiry);
        }

        // --- Futures: settle every matured position against the reference price,
        //     crediting (or debiting) the holder's capital. ---
        if !self.futures.is_empty() {
            let mut matured: Vec<Future> = Vec::new();
            self.futures.retain(|f| {
                if now >= f.maturity {
                    matured.push(*f);
                    false
                } else {
                    true
                }
            });
            for f in matured {
                let settle = self.reference_price(f.commodity).unwrap_or(f.strike);
                let payoff = f.payoff(settle);
                if let Some(t) = self.traders.iter_mut().find(|t| t.id == f.holder) {
                    t.capital += payoff;
                }
                self.log_event(
                    EventCategory::System,
                    None,
                    format!(
                        "Future settled for trader #{}: {} @ {settle} vs strike {} -> {payoff:+}cr",
                        f.holder.0,
                        self.registry.commodity_name(f.commodity),
                        f.strike
                    ),
                );
            }
        }
    }

    /// Post new contracts and expire overdue ones. No-op without a contract
    /// config. Runs last in the tick, against the settled markets.
    fn contract_phase(&mut self) {
        let Some(rt) = self.contract_runtime.clone() else {
            return;
        };
        let now = self.tick;
        let reg = self.registry.clone();

        // 1. Expire any contract whose deadline has arrived (open or accepted).
        //    Collect first, then log, since `retain` holds a borrow of the board.
        // Strictly after the deadline: the deadline tick itself stays fulfillable
        // (command phase precedes this one), and a delivery attempt on the tick
        // after is rejected `ContractExpired` before the contract is swept here.
        let mut expired: Vec<(u64, SystemId)> = Vec::new();
        self.contracts.retain(|c| {
            if now > c.deadline {
                expired.push((c.id.0, c.destination));
                false
            } else {
                true
            }
        });
        for (id, dest) in expired {
            self.log_event(
                EventCategory::System,
                Some(dest),
                format!(
                    "Contract #{id} expired, undelivered to {}",
                    reg.system_name(dest)
                ),
            );
        }

        // 2. Post at most one new contract per interval, while below the open cap.
        if !now.get().is_multiple_of(rt.generation_interval) {
            return;
        }
        let open = self.contracts.iter().filter(|c| c.is_open()).count() as u32;
        if open >= rt.max_open {
            return;
        }
        // Rotate the preferred kind by the generation counter so the board carries
        // variety; fall through to another kind when the preferred one has no
        // candidate this tick (e.g. no shortfall, or no pirates to hunt).
        let gen_count = now.get() / rt.generation_interval;
        let order: [u8; 3] = match gen_count % 3 {
            0 => [0, 1, 2],
            1 => [1, 2, 0],
            _ => [2, 0, 1],
        };
        let mut contract = None;
        for k in order {
            contract = match k {
                0 => self.generate_delivery(&rt, now),
                1 => self.generate_bounty(&rt, now),
                _ => self.generate_courier(&rt, now),
            };
            if contract.is_some() {
                break;
            }
        }

        if let Some(contract) = contract {
            let dest = contract.destination;
            let cid = contract.id.0;
            let msg = match contract.kind {
                ContractKind::Delivery { commodity, quantity } => format!(
                    "Contract #{cid} posted: deliver {quantity} {} to {} for {}cr",
                    reg.commodity_name(commodity),
                    reg.system_name(dest),
                    contract.reward,
                ),
                ContractKind::Courier => format!(
                    "Contract #{cid} posted: courier run {} -> {} for {}cr",
                    reg.system_name(contract.origin),
                    reg.system_name(dest),
                    contract.reward,
                ),
                ContractKind::Bounty { target, .. } => format!(
                    "Contract #{cid} posted: bounty on {target} pirate(s) near {} for {}cr",
                    reg.system_name(dest),
                    contract.reward,
                ),
            };
            self.contracts.push(contract);
            self.log_event(EventCategory::System, Some(dest), msg);
        }
    }

    /// A delivery contract from the largest current market shortfall, or `None` if
    /// nothing qualifies. Deterministic: the biggest shortfall wins, ties broken by
    /// `(system, commodity)` id. Skips a good that already has an open delivery
    /// contract for the same destination, so the board never stacks duplicates.
    fn generate_delivery(&mut self, rt: &ContractRuntime, now: Tick) -> Option<Contract> {
        let mut best: Option<(u32, SystemId, CommodityId, Money)> = None;
        for m in &self.markets {
            for (&commodity, good) in &m.goods {
                let shortfall = good.equilibrium.saturating_sub(good.stock);
                if shortfall < rt.min_shortfall {
                    continue;
                }
                let dup = self.contracts.iter().any(|c| {
                    c.is_open() && c.destination == m.system && c.cargo().map(|(cc, _)| cc) == Some(commodity)
                });
                if dup {
                    continue;
                }
                let better = match best {
                    None => true,
                    Some((bs, bsys, bc, _)) => {
                        shortfall > bs
                            || (shortfall == bs && (m.system.0, commodity.0) < (bsys.0, bc.0))
                    }
                };
                if better {
                    best = Some((shortfall, m.system, commodity, good.price));
                }
            }
        }

        let (shortfall, destination, commodity, price) = best?;
        let quantity = shortfall.min(rt.max_quantity);
        let reward = (price.max(1) as f64 * quantity as f64 * rt.reward_factor).round() as Money;
        let origin = self.surplus_source(commodity).unwrap_or(destination);
        let id = self.fresh_contract_id();
        Some(Contract {
            id,
            kind: ContractKind::Delivery { commodity, quantity },
            destination,
            origin,
            reward,
            deadline: Tick(now.0 + rt.deadline_ticks),
            state: ContractState::Open,
        })
    }

    /// A bounty on pirates around the most dangerous system that currently has
    /// pirates present (ties broken by system id). `None` if bounties are disabled,
    /// no such system exists, or one is already open there. The reward is claimed at
    /// that system once the holder's trader has destroyed `bounty_target` pirates.
    fn generate_bounty(&mut self, rt: &ContractRuntime, now: Tick) -> Option<Contract> {
        if rt.bounty_target == 0 {
            return None;
        }
        let mut best: Option<(f64, SystemId)> = None;
        for p in &self.pirates {
            let Some(sys) = p.docked_at() else { continue };
            let dup = self.contracts.iter().any(|c| {
                c.is_open() && c.destination == sys && matches!(c.kind, ContractKind::Bounty { .. })
            });
            if dup {
                continue;
            }
            let danger = self.registry.system(sys).danger;
            let better = match best {
                None => true,
                Some((bd, bsys)) => danger > bd || (danger == bd && sys.0 < bsys.0),
            };
            if better {
                best = Some((danger, sys));
            }
        }

        let (_, sys) = best?;
        let id = self.fresh_contract_id();
        Some(Contract {
            id,
            kind: ContractKind::Bounty { target: rt.bounty_target, progress: 0 },
            destination: sys,
            origin: sys,
            reward: rt.bounty_reward,
            deadline: Tick(now.0 + rt.deadline_ticks),
            state: ContractState::Open,
        })
    }

    /// A courier parcel between two connected systems, chosen deterministically by
    /// the generation counter so the pairing rotates over systems. `None` if
    /// couriers are disabled or every route already has an open courier. The reward
    /// scales with the destination's danger — a riskier run pays more.
    fn generate_courier(&mut self, rt: &ContractRuntime, now: Tick) -> Option<Contract> {
        if rt.courier_reward == 0 {
            return None;
        }
        let nsys = self.registry.system_count() as u64;
        if nsys == 0 {
            return None;
        }
        let gen_count = now.get() / rt.generation_interval;
        for offset in 0..nsys {
            let origin = SystemId(((gen_count + offset) % nsys) as u32);
            let Some(&dest) = self.registry.system(origin).connections.first() else {
                continue;
            };
            let dup = self.contracts.iter().any(|c| {
                c.is_open()
                    && matches!(c.kind, ContractKind::Courier)
                    && c.origin == origin
                    && c.destination == dest
            });
            if dup {
                continue;
            }
            let danger = self.registry.system(dest).danger;
            let reward = (rt.courier_reward as f64 * (1.0 + danger)).round() as Money;
            let id = self.fresh_contract_id();
            return Some(Contract {
                id,
                kind: ContractKind::Courier,
                destination: dest,
                origin,
                reward,
                deadline: Tick(now.0 + rt.deadline_ticks),
                state: ContractState::Open,
            });
        }
        None
    }

    /// The system holding the largest surplus (stock above equilibrium) of a
    /// commodity — a natural source for it — or `None` if nowhere has a surplus.
    fn surplus_source(&self, commodity: CommodityId) -> Option<SystemId> {
        let mut best: Option<(u32, SystemId)> = None;
        for m in &self.markets {
            if let Some(good) = m.goods.get(&commodity) {
                let surplus = good.stock.saturating_sub(good.equilibrium);
                if surplus == 0 {
                    continue;
                }
                let better = match best {
                    None => true,
                    Some((bs, bsys)) => surplus > bs || (surplus == bs && m.system.0 < bsys.0),
                };
                if better {
                    best = Some((surplus, m.system));
                }
            }
        }
        best.map(|(_, s)| s)
    }

    fn production_phase(&mut self) {
        let reg = self.registry.clone();
        for i in 0..self.markets.len() {
            let sys = reg.system(self.markets[i].system);
            for (j, &rid) in sys.industries.iter().enumerate() {
                let recipe = reg.recipe(rid);

                // Scale the nominal rate by the price-elastic response.
                let factor = match response_signal(recipe) {
                    Some((c, supply_side)) => {
                        let base = reg.commodity(c).base_price;
                        let price = self.markets[i].price(c).unwrap_or(base);
                        elastic_factor(recipe.elasticity, supply_side, base, price)
                    }
                    None => 1.0,
                };

                // Accumulate fractional throughput; apply the whole part; keep the
                // remainder. Cap the accumulator so a starved recipe cannot store
                // an unbounded burst.
                let cap = recipe.rate as f64 * MAX_ELASTIC_FACTOR + 1.0;
                let acc = (self.progress[i][j] + recipe.rate as f64 * factor).min(cap);
                let want = acc.floor();
                let applied = apply_recipe(&mut self.markets[i], recipe, want as u32);
                self.progress[i][j] = acc - applied as f64;
            }
        }
    }

    fn price_phase(&mut self) {
        let reg = self.registry.clone();
        let scripts = &self.pricing_scripts;
        for market in &mut self.markets {
            let strategy = market.pricing;
            for (&commodity, good) in market.goods.iter_mut() {
                let def = reg.commodity(commodity);
                let target = strategy.price(
                    scripts,
                    def.base_price,
                    good.stock,
                    good.equilibrium,
                    def.elasticity,
                );
                // Sticky prices: ease toward the target instead of snapping, to
                // damp trade-induced oscillation.
                good.price = crate::pricing::smoothed(good.price, target);
            }
        }
    }

    /// Move the persistent pirate fleet (arrivals, shield regen, danger-weighted
    /// roaming, periodic reinforcement). No-op without a piracy config.
    fn pirate_phase(&mut self) {
        let Some(rt) = self.piracy.clone() else {
            return;
        };
        let engaged = self.engaged_patrols();
        advance_fleet(
            &mut self.pirates,
            &self.registry,
            &mut self.rng,
            self.tick,
            rt.pirate_ship,
            rt.fleet_size,
            rt.reinforce_interval,
            &mut self.next_patrol_id,
            &engaged,
            true, // pirates are not funding-constrained
        );
    }

    /// Move the navy fleet the same way, then hunt: wherever navy and pirates are
    /// docked together, open a running battle that thins the pirate presence over
    /// the next few ticks. No-op without a navy config.
    fn navy_phase(&mut self) {
        let Some(rt) = self.navy_runtime.clone() else {
            return;
        };
        // Fund the navy: income in, upkeep out per active ship. A treasury deficit
        // stalls reinforcement, so an underfunded navy shrinks under attrition.
        let cost = rt.upkeep * self.navy.len() as i64;
        self.treasury += rt.funding - cost;
        self.piracy_stats.navy_upkeep += cost;
        let can_reinforce = self.treasury >= 0;

        let engaged = self.engaged_patrols();
        advance_fleet(
            &mut self.navy,
            &self.registry,
            &mut self.rng,
            self.tick,
            rt.navy_ship,
            rt.fleet_size,
            rt.reinforce_interval,
            &mut self.next_patrol_id,
            &engaged,
            can_reinforce,
        );
        self.navy_hunt_pirates();
    }

    /// For every system where the navy is present, engage any pirates docked
    /// there in a combined encounter, applying persistent damage to both sides.
    /// Open a running battle at each system where navy and pirates share a dock
    /// (and are not already fighting). The fight advances in `combat_phase` and its
    /// casualties are applied on completion.
    fn navy_hunt_pirates(&mut self) {
        let engaged = self.engaged_patrols();
        // Distinct systems the navy currently occupies (deterministic order).
        let mut systems: Vec<SystemId> =
            self.navy.iter().filter_map(Patrol::docked_at).collect();
        systems.sort_by_key(|s| s.0);
        systems.dedup();

        let reg = self.registry.clone();
        for sys in systems {
            let navy_ids = self.available_patrols(&self.navy, sys, &engaged, usize::MAX);
            let pirate_ids = self.available_patrols(&self.pirates, sys, &engaged, usize::MAX);
            if navy_ids.is_empty() || pirate_ids.is_empty() {
                continue;
            }

            let mut combatants = Vec::with_capacity(navy_ids.len() + pirate_ids.len());
            for (k, nid) in navy_ids.iter().enumerate() {
                let p = self.navy.iter().find(|p| p.id == *nid).expect("navy id valid");
                combatants.push(patrol_combatant(&reg, p, 0, side_pos(0.0, k)));
            }
            let pirates_offset = combatants.len();
            for (k, pid) in pirate_ids.iter().enumerate() {
                let p = self.pirates.iter().find(|p| p.id == *pid).expect("pirate id valid");
                combatants.push(patrol_combatant(&reg, p, 1, side_pos(30.0, k)));
            }

            let seed = self.rng.next_u64();
            self.encounters.push(ActiveEncounter {
                encounter: Encounter::new(combatants),
                rng: DetRng::from_seed(seed),
                steps: 0,
                kind: EncounterKind::Hunt { sys, navy: navy_ids, pirates: pirate_ids, pirates_offset },
            });
        }
    }

    /// Ids of live patrols docked at `sys` that are not already in a battle, up to
    /// `limit` of them (in deterministic fleet order).
    fn available_patrols(
        &self,
        fleet: &[Patrol],
        sys: SystemId,
        engaged: &HashSet<PatrolId>,
        limit: usize,
    ) -> Vec<PatrolId> {
        fleet
            .iter()
            .filter(|p| p.is_alive() && p.docked_at() == Some(sys) && !engaged.contains(&p.id))
            .map(|p| p.id)
            .take(limit)
            .collect()
    }

    /// Ids of patrols currently in a running battle (frozen from roaming and from
    /// being pulled into a second fight).
    fn engaged_patrols(&self) -> HashSet<PatrolId> {
        let mut set = HashSet::new();
        for e in &self.encounters {
            let (navy, pirates) = match &e.kind {
                EncounterKind::Ambush { navy, pirates, .. } => (navy, pirates),
                EncounterKind::Hunt { navy, pirates, .. } => (navy, pirates),
            };
            set.extend(navy.iter().copied());
            set.extend(pirates.iter().copied());
        }
        set
    }

    /// Whether a trader is currently in a running battle.
    fn trader_engaged(&self, id: TraderId) -> bool {
        self.encounters.iter().any(|e| {
            matches!(&e.kind, EncounterKind::Ambush { trader, .. } if *trader == id)
        })
    }

    /// Advance every running battle a few steps, and apply the outcome of any that
    /// decide (or hit the length cap) this tick.
    fn combat_phase(&mut self) {
        if self.encounters.is_empty() {
            return;
        }
        let mut completed: Vec<ActiveEncounter> = Vec::new();
        let mut i = 0;
        while i < self.encounters.len() {
            let ae = &mut self.encounters[i];
            let budget = COMBAT_STEPS_PER_TICK.min(MAX_COMBAT_STEPS.saturating_sub(ae.steps));
            let outcome = ae.encounter.advance(&mut ae.rng, budget);
            ae.steps += budget;
            if outcome != Outcome::Ongoing || ae.steps >= MAX_COMBAT_STEPS {
                completed.push(self.encounters.remove(i));
            } else {
                i += 1;
            }
        }
        for ae in completed {
            if matches!(ae.kind, EncounterKind::Ambush { .. }) {
                self.complete_ambush(ae);
            } else {
                self.complete_hunt(ae);
            }
        }
    }

    /// Apply a finished navy-vs-pirate hunt: write persistent damage back to the
    /// surviving patrols by id, tally casualties, and cull the dead.
    fn complete_hunt(&mut self, ae: ActiveEncounter) {
        let ActiveEncounter { encounter, kind, .. } = ae;
        let EncounterKind::Hunt { sys, navy, pirates, pirates_offset } = kind else {
            return;
        };
        let reg = self.registry.clone();
        let combs = encounter.combatants();

        let mut navy_down = 0u32;
        for (k, nid) in navy.iter().enumerate() {
            let c = &combs[k];
            if let Some(p) = self.navy.iter_mut().find(|p| p.id == *nid) {
                p.hull = c.hull;
                p.shield = c.shield;
            }
            if !c.alive {
                self.piracy_stats.navy_lost += 1;
                navy_down += 1;
            }
        }
        let mut killed = 0u32;
        for (k, pid) in pirates.iter().enumerate() {
            let c = &combs[pirates_offset + k];
            if let Some(p) = self.pirates.iter_mut().find(|p| p.id == *pid) {
                p.hull = c.hull;
                p.shield = c.shield;
            }
            if !c.alive {
                self.piracy_stats.pirates_suppressed += 1;
                killed += 1;
            }
        }
        if killed > 0 || navy_down > 0 {
            let system = &reg.system(sys).name;
            let mut msg = format!("Navy engaged pirates at {system}: {killed} destroyed");
            if navy_down > 0 {
                msg += &format!(", {navy_down} frigate(s) lost");
            }
            self.log_event(EventCategory::Navy, Some(sys), msg);
        }
        self.navy.retain(Patrol::is_alive);
        self.pirates.retain(Patrol::is_alive);
    }

    /// Ambushes of laden, in-transit traders by pirates present at their
    /// destination. No-op without a piracy config. Dead pirates are culled at the
    /// end.
    fn piracy_phase(&mut self) {
        let Some(base) = self.piracy.as_ref().map(|p| p.base_ambush_chance) else {
            return;
        };

        for t in 0..self.traders.len() {
            let TraderLocation::InTransit { dest, .. } = self.traders[t].location else {
                continue;
            };
            // Pirates prey on cargo; an empty (deadheading) trader is ignored.
            if self.traders[t].cargo.is_empty() {
                continue;
            }
            // A trader already in a running battle is not ambushed again.
            if self.trader_engaged(self.traders[t].id) {
                continue;
            }
            // Ambush likelihood rises with how many live pirates lurk at the
            // destination; no pirates there means no ambush.
            let present = self
                .pirates
                .iter()
                .filter(|p| p.is_alive() && p.docked_at() == Some(dest))
                .count();
            if present == 0 {
                continue;
            }
            let chance = (base * present as f64).clamp(0.0, 1.0);
            if self.rng.unit_f64() < chance {
                self.create_ambush(t, dest);
            }
        }
    }

    /// Open a running ambush. The trader's side (faction 0) is the trader itself,
    /// plus its convoy escorts, plus any free navy present at `dest`; the pirates
    /// (faction 1) are up to `max_pirates` live raiders docked there and not already
    /// fighting. Persistent combatants (navy, pirates) carry their current damage;
    /// escorts are fresh. The battle then advances in `combat_phase`, and
    /// `complete_ambush` settles it — the trader survives iff *its own* ship
    /// survives (escorts/navy winning does not save a dead trader).
    fn create_ambush(&mut self, t: usize, dest: SystemId) {
        let reg = self.registry.clone();
        let (max_pirates, respawn_delay, bounty) = {
            let p = self.piracy.as_ref().expect("piracy_phase gates on Some");
            (p.max_pirates, p.respawn_delay, p.bounty)
        };
        let escort = self.escort.clone();
        let engaged_set = self.engaged_patrols();

        // Up to `max_pirates` raiders at `dest` not already in another fight.
        let pirate_ids =
            self.available_patrols(&self.pirates, dest, &engaged_set, max_pirates as usize);
        if pirate_ids.is_empty() {
            return;
        }
        // Navy present at the destination (and free) joins the defense.
        let navy_ids = self.available_patrols(&self.navy, dest, &engaged_set, usize::MAX);

        // --- Faction 0: trader, then escorts, then navy defenders ---
        let mut combatants = Vec::new();
        let mut f0 = 0usize; // running index within faction 0 (for spacing)
        let trader_ship = self.traders[t].ship;
        let tdef = reg.ship(trader_ship);
        combatants.push(Combatant::new(
            trader_ship,
            0,
            tdef.combat.unwrap_or_default(),
            tdef.hull,
            tdef.max_speed,
            side_pos(0.0, f0),
        ));

        if let Some(e) = &escort {
            let edef = reg.ship(e.escort_ship);
            let estats = edef.combat.unwrap_or_default();
            for _ in 0..e.count {
                f0 += 1;
                combatants.push(Combatant::new(
                    e.escort_ship,
                    0,
                    estats,
                    edef.hull,
                    edef.max_speed,
                    side_pos(0.0, f0),
                ));
            }
        }

        let navy_offset = combatants.len();
        for nid in &navy_ids {
            f0 += 1;
            let p = self.navy.iter().find(|p| p.id == *nid).expect("navy id valid");
            combatants.push(patrol_combatant(&reg, p, 0, side_pos(0.0, f0)));
        }

        // --- Faction 1: pirates ---
        let pirate_offset = combatants.len();
        for (k, pid) in pirate_ids.iter().enumerate() {
            let p = self.pirates.iter().find(|p| p.id == *pid).expect("pirate id valid");
            combatants.push(patrol_combatant(&reg, p, 1, side_pos(30.0, k)));
        }

        self.piracy_stats.ambushes += 1;
        let seed = self.rng.next_u64();
        let trader = self.traders[t].id;
        self.encounters.push(ActiveEncounter {
            encounter: Encounter::new(combatants),
            rng: DetRng::from_seed(seed),
            steps: 0,
            kind: EncounterKind::Ambush {
                trader,
                dest,
                navy: navy_ids,
                pirates: pirate_ids,
                navy_offset,
                pirate_offset,
                respawn_delay,
                bounty,
            },
        });
    }

    /// Apply a finished ambush: write persistent damage back to the surviving
    /// patrols by id, then settle the trader — a survivor collects a bounty per
    /// pirate killed (and any bounty-contract progress); a lost one forfeits its
    /// cargo, is destroyed, and any insurance policy pays out.
    fn complete_ambush(&mut self, ae: ActiveEncounter) {
        let ActiveEncounter { encounter, kind, .. } = ae;
        let EncounterKind::Ambush {
            trader,
            dest,
            navy,
            pirates,
            navy_offset,
            pirate_offset,
            respawn_delay,
            bounty,
        } = kind
        else {
            return;
        };
        let reg = self.registry.clone();
        let combs = encounter.combatants();

        // Write navy defenders back by id.
        for (k, nid) in navy.iter().enumerate() {
            let c = &combs[navy_offset + k];
            if let Some(p) = self.navy.iter_mut().find(|p| p.id == *nid) {
                p.hull = c.hull;
                p.shield = c.shield;
            }
            if !c.alive {
                self.piracy_stats.navy_lost += 1;
            }
        }
        // Write pirates back; tally kills.
        let mut kills = 0u64;
        for (k, pid) in pirates.iter().enumerate() {
            let c = &combs[pirate_offset + k];
            if let Some(p) = self.pirates.iter_mut().find(|p| p.id == *pid) {
                p.hull = c.hull;
                p.shield = c.shield;
            }
            if !c.alive {
                kills += 1;
            }
        }
        self.piracy_stats.pirates_destroyed += kills;

        let trader_alive = combs[0].alive;
        self.pirates.retain(Patrol::is_alive);
        self.navy.retain(Patrol::is_alive);

        // The trader may have been despawned mid-fight; if gone, we are done.
        let Some(t) = self.traders.iter().position(|x| x.id == trader) else {
            return;
        };
        let system = &reg.system(dest).name;
        let tid = trader.0;
        if trader_alive {
            let reward = bounty * kills as i64;
            self.traders[t].capital += reward;
            self.piracy_stats.bounties_paid += reward;

            // Credit any bounty contract this trader holds, noting the ones that
            // just reached their quota so the player learns they can claim.
            let mut completed: Vec<u64> = Vec::new();
            if kills > 0 {
                for c in &mut self.contracts {
                    let held = matches!(
                        c.state,
                        ContractState::Accepted { trader: ct, .. } if ct == trader
                    );
                    if !held {
                        continue;
                    }
                    if let ContractKind::Bounty { target, progress } = &mut c.kind {
                        let before = *progress;
                        *progress = (before + kills as u32).min(*target);
                        if before < *target && *progress >= *target {
                            completed.push(c.id.0);
                        }
                    }
                }
            }

            let msg = format!(
                "Ambush near {system}: trader #{tid} beat {} pirate(s), killed {kills} (+{reward}cr)",
                pirates.len()
            );
            self.log_event(EventCategory::Combat, Some(dest), msg);
            for cid in completed {
                self.log_event(
                    EventCategory::System,
                    Some(dest),
                    format!("Bounty #{cid} quota met by trader #{tid}; claim at its station"),
                );
            }
        } else {
            self.piracy_stats.traders_lost += 1;
            self.traders[t].cargo.clear(); // shipment lost to the void
            self.traders[t].location = TraderLocation::Destroyed {
                respawn: Tick(self.tick.0 + respawn_delay),
            };
            let msg = format!("Ambush near {system}: trader #{tid} destroyed, cargo lost");
            self.log_event(EventCategory::Piracy, Some(dest), msg);

            // An active insurance policy pays out (once) on the loss.
            if let Some(pi) = self.policies.iter().position(|p| p.insured == trader) {
                let payout = self.policies[pi].payout;
                self.policies.remove(pi);
                self.traders[t].capital += payout;
                self.log_event(
                    EventCategory::System,
                    Some(dest),
                    format!("Insurance paid trader #{tid} {payout}cr for the loss"),
                );
            }
        }
    }

    fn trading_phase(&mut self) {
        let reg = self.registry.clone();
        let now = self.tick;
        let nsys = reg.system_count();

        for t in 0..self.traders.len() {
            // Respawn destroyed traders when their downtime elapses; otherwise
            // they take no action.
            if let TraderLocation::Destroyed { respawn } = self.traders[t].location {
                if now >= respawn {
                    let at = SystemId(self.rng.range_usize(0, nsys) as u32);
                    self.traders[t].location = TraderLocation::Docked(at);
                    let tid = self.traders[t].id.0;
                    let msg = format!("Trader #{tid} respawned at {}", reg.system(at).name);
                    self.log_event(EventCategory::System, Some(at), msg);
                }
                continue;
            }

            // A trader in a running battle is frozen: it neither arrives nor acts
            // until the fight completes (which may destroy it).
            if self.trader_engaged(self.traders[t].id) {
                continue;
            }

            // Resolve arrivals.
            if let TraderLocation::InTransit { dest, arrival, .. } = self.traders[t].location {
                if now >= arrival {
                    self.traders[t].location = TraderLocation::Docked(dest);
                } else {
                    continue; // still travelling
                }
            }

            let TraderLocation::Docked(sys) = self.traders[t].location else {
                continue;
            };

            // Player-owned traders have their arrivals/respawns resolved above but
            // take no autonomous action — they move and trade only via commands.
            if self.traders[t].is_player() {
                continue;
            }
            let sys_idx = sys.index();

            // If carrying cargo, sell it all here, then wait for next tick to buy.
            if !self.traders[t].cargo.is_empty() {
                self.sell_all(t, sys_idx);
                continue;
            }

            // Otherwise, look for a profitable outbound trade.
            let capital = self.traders[t].capital;
            let ship = self.traders[t].ship;
            let capacity = reg.ship(ship).cargo_capacity;

            let risk_aversion = self.risk_aversion;
            let plan = {
                let neighbor_ids = &reg.system(sys).connections;
                let neighbors: Vec<&Market> =
                    neighbor_ids.iter().map(|id| &self.markets[id.index()]).collect();
                let here = &self.markets[sys_idx];
                choose_trade(
                    here,
                    &neighbors,
                    capital,
                    capacity,
                    |c| reg.commodity(c).unit_mass,
                    |s| reg.system(s).danger,
                    risk_aversion,
                )
            };

            if let Some(plan) = plan {
                // Buy: draw stock from here (raising its price next repricing).
                let ok = self.markets[sys_idx].try_remove(plan.commodity, plan.qty);
                debug_assert!(ok, "choose_trade bounded qty by available stock");
                let cost: Money = plan.qty as i64 * plan.unit_cost;
                self.traders[t].capital -= cost;
                *self.traders[t].cargo.entry(plan.commodity).or_insert(0) += plan.qty;

                // Depart toward the destination.
                let travel = self.travel_ticks(sys, plan.dest, ship);
                self.traders[t].location = TraderLocation::InTransit {
                    origin: sys,
                    dest: plan.dest,
                    departure: now,
                    arrival: Tick(now.0 + travel),
                };
                self.charge_escort(t);
            } else if let Some(dest) = self.reposition_target(sys, capital, capacity) {
                // No trade available here (e.g. docked at a starved consumer with
                // nothing to buy). Deadhead empty toward opportunity instead of
                // stranding — otherwise the fleet piles up at consumers, transport
                // collapses, and producers glut while consumers starve.
                let travel = self.travel_ticks(sys, dest, ship);
                self.traders[t].location = TraderLocation::InTransit {
                    origin: sys,
                    dest,
                    departure: now,
                    arrival: Tick(now.0 + travel),
                };
                self.charge_escort(t);
            }
        }
    }

    /// Choose a neighbor to deadhead to when no trade is available here. Ranks
    /// neighbors by the best trade obtainable *from* that neighbor (one-hop
    /// lookahead), so traders drift toward producers/opportunity. Falls back to a
    /// seeded-random neighbor to escape dead pockets where no neighbor offers a
    /// trade. Returns `None` only if the system has no connections.
    fn reposition_target(
        &mut self,
        sys: SystemId,
        capital: Money,
        capacity: u32,
    ) -> Option<SystemId> {
        let reg = self.registry.clone();
        let risk_aversion = self.risk_aversion;
        let connections = &reg.system(sys).connections;
        if connections.is_empty() {
            return None;
        }

        // Deadheading itself is empty (unladen traders are never ambushed), so the
        // immediate jump to `n` carries no risk. We rank neighbors by the
        // risk-adjusted value of the onward trade available *from* each `n`.
        let mut best: Option<(SystemId, Money)> = None;
        for &n in connections {
            let onward = &reg.system(n).connections;
            let onward_markets: Vec<&Market> =
                onward.iter().map(|id| &self.markets[id.index()]).collect();
            let here = &self.markets[n.index()];
            if let Some(plan) = choose_trade(
                here,
                &onward_markets,
                capital,
                capacity,
                |c| reg.commodity(c).unit_mass,
                |s| reg.system(s).danger,
                risk_aversion,
            ) {
                let better = match best {
                    None => true,
                    Some((bn, bp)) => plan.score > bp || (plan.score == bp && n < bn),
                };
                if better {
                    best = Some((n, plan.score));
                }
            }
        }

        Some(match best {
            Some((n, _)) => n,
            None => connections[self.rng.range_usize(0, connections.len())],
        })
    }

    /// Sell every unit of a trader's cargo into the market it is docked at,
    /// crediting the trader. Goods the market does not trade are retained (should
    /// not happen: a trader only buys goods to sell at a neighbor that trades them).
    fn sell_all(&mut self, trader: usize, sys_idx: usize) {
        let cargo = std::mem::take(&mut self.traders[trader].cargo);
        let mut leftover = std::collections::BTreeMap::new();
        for (commodity, qty) in cargo {
            if let Some(price) = self.markets[sys_idx].price(commodity) {
                self.markets[sys_idx].add(commodity, qty);
                self.traders[trader].capital += qty as i64 * price;
            } else {
                leftover.insert(commodity, qty);
            }
        }
        self.traders[trader].cargo = leftover;
    }

    /// Whole-tick travel time between two systems for a given ship.
    fn travel_ticks(&self, from: SystemId, to: SystemId, ship: ShipId) -> u64 {
        travel_between(&self.registry, from, to, ship)
    }
}

/// Euclidean jump distance divided by the ship's jump speed, rounded up, at
/// least 1.
fn travel_between(reg: &Registry, from: SystemId, to: SystemId, ship: ShipId) -> u64 {
    let a = reg.system(from).position;
    let b = reg.system(to).position;
    let dist = ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt();
    let speed = reg.ship(ship).jump_speed.max(1e-6);
    ((dist / speed).ceil() as u64).max(1)
}

/// Spawn `size` fresh patrols at danger-weighted lawless systems (fewer if the
/// galaxy has few/no dangerous systems).
fn spawn_fleet(
    reg: &Registry,
    rng: &mut DetRng,
    ship: ShipId,
    size: u32,
    next_id: &mut u64,
) -> Vec<Patrol> {
    let def = reg.ship(ship);
    let stats = def.combat.unwrap_or_default();
    let mut fleet = Vec::new();
    for _ in 0..size {
        if let Some(sys) = pick_danger_system(reg, rng) {
            let id = PatrolId(*next_id);
            *next_id += 1;
            fleet.push(Patrol::new(id, ship, &stats, def.hull, sys));
        }
    }
    fleet
}

/// Advance a patrol fleet by one tick: resolve arrivals, regenerate shields, let
/// docked patrols roam toward lawless space, and periodically reinforce up to
/// `fleet_size`. Shared by pirates and navy. Patrols in `engaged` are frozen in a
/// running battle this tick — they neither move nor regenerate here (the encounter
/// owns their live state until it completes).
#[allow(clippy::too_many_arguments)]
fn advance_fleet(
    fleet: &mut Vec<Patrol>,
    reg: &Registry,
    rng: &mut DetRng,
    now: Tick,
    ship: ShipId,
    fleet_size: u32,
    reinforce_interval: u64,
    next_id: &mut u64,
    engaged: &HashSet<PatrolId>,
    can_reinforce: bool,
) {
    let def = reg.ship(ship);
    let stats = def.combat.unwrap_or_default();

    for patrol in fleet.iter_mut() {
        if engaged.contains(&patrol.id) {
            continue; // fighting: state lives in the encounter until it completes
        }
        if let PatrolLocation::InTransit { dest, arrival, .. } = patrol.location {
            if now >= arrival {
                patrol.location = PatrolLocation::Docked(dest);
            }
        }
        patrol.regen_shield(&stats);
        if let PatrolLocation::Docked(sys) = patrol.location {
            if rng.unit_f64() < ROAM_CHANCE {
                if let Some(dest) = pick_roam_neighbor(reg, sys, rng) {
                    let travel = travel_between(reg, sys, dest, ship);
                    patrol.location = PatrolLocation::InTransit {
                        origin: sys,
                        dest,
                        departure: now,
                        arrival: Tick(now.0 + travel),
                    };
                }
            }
        }
    }

    if can_reinforce && now.0.is_multiple_of(reinforce_interval) {
        while fleet.len() < fleet_size as usize {
            let Some(sys) = pick_danger_system(reg, rng) else {
                break;
            };
            let id = PatrolId(*next_id);
            *next_id += 1;
            fleet.push(Patrol::new(id, ship, &stats, def.hull, sys));
        }
    }
}

/// Build a combatant from a patrol, carrying its persistent hull/shield.
fn patrol_combatant(reg: &Registry, p: &Patrol, faction: u8, pos: Vec2) -> Combatant {
    let def = reg.ship(p.ship);
    let mut c = Combatant::new(
        p.ship,
        faction,
        def.combat.unwrap_or_default(),
        def.hull,
        def.max_speed,
        pos,
    );
    c.hull = p.hull;
    c.shield = p.shield;
    c
}

/// A spawn position for combatant `k` on the side at x-offset `x`, spread along y
/// so both sides start within weapon range.
fn side_pos(x: f64, k: usize) -> Vec2 {
    Vec2::new(x, k as f64 * 8.0)
}

/// Deterministically pick a system id from `(id, weight)` pairs in proportion to
/// weight. Returns `None` if the total weight is non-positive.
fn weighted_pick(items: &[(SystemId, f64)], rng: &mut DetRng) -> Option<SystemId> {
    let total: f64 = items.iter().map(|(_, w)| w.max(0.0)).sum();
    if total <= 0.0 {
        return None;
    }
    let mut r = rng.unit_f64() * total;
    for (id, w) in items {
        r -= w.max(0.0);
        if r < 0.0 {
            return Some(*id);
        }
    }
    items.last().map(|(id, _)| *id) // floating-point guard
}

/// A lawless (`danger > 0`) system, weighted by danger. `None` if the galaxy has
/// no dangerous systems — so those galaxies never get pirates.
fn pick_danger_system(reg: &Registry, rng: &mut DetRng) -> Option<SystemId> {
    let candidates: Vec<(SystemId, f64)> = reg
        .systems()
        .filter(|s| s.danger > 0.0)
        .map(|s| (s.id, s.danger))
        .collect();
    weighted_pick(&candidates, rng)
}

/// A danger-weighted neighbor to roam to. Restricted to `danger > 0` neighbors so
/// pirates stay confined to lawless space (safe systems remain genuinely safe).
/// `None` means "stay put".
fn pick_roam_neighbor(reg: &Registry, sys: SystemId, rng: &mut DetRng) -> Option<SystemId> {
    let candidates: Vec<(SystemId, f64)> = reg
        .system(sys)
        .connections
        .iter()
        .map(|&n| (n, reg.system(n).danger))
        .filter(|(_, d)| *d > 0.0)
        .collect();
    weighted_pick(&candidates, rng)
}
