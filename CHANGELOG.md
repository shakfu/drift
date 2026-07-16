# Changelog

All notable changes to Drift are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
the project aims to follow [Semantic Versioning](https://semver.org/). Drift is
pre-1.0 and unreleased; everything below is development toward the first release.

## [Unreleased]

### Added

Core simulation

- Deterministic, headless economy core: a single seeded RNG drives a discrete tick
  loop, so the same seed produces a byte-identical run. Serializable `Snapshot` for
  state dumps and resume.
- Cargo workspace: `drift-core` (typed ids + interning, `DetRng`, tick, money, the
  `Step`/`SimContext` seam, the `NamedRegistry` plugin seam), `drift-data` (moddable
  serde schema), `drift-mods` (mod-loader), `drift-economy` (the `World`),
  `drift-combat`, `drift-sim` (the session/driver layer), `drift-proto` (the
  client/server wire contract), `drift-server` (the authoritative networked host),
  `drift-cli`, `drift-client`.
- `drift-sim::Session`: a session/driver façade that owns a `World` and centralizes
  loading, command application, ticking, per-tick event draining, and snapshots.
  Both the CLI and the graphical client drive it; a server would use the same façade.

Modding / scripting

- `drift-script`: the mod-scripting engine, built on **Rhai** (chosen over Lua and
  WASM for being pure Rust, sandboxed by construction, and operation-limited — full
  rationale in the crate docs). Its first hook is `ScriptedPricing`, a market
  pricing strategy authored in Rhai, with tests proving determinism, that a runaway
  script is terminated (not hung), that a broken script is contained to the price
  floor rather than crashing, and that the sandbox exposes nothing ambient.
- Scripted pricing wired through the `NamedRegistry` seam: `PricingStrategy` gains a
  `Scripted` handle, and a `PricingSet` bundles the name registry with the compiled
  script table. `register_script` adds a named Rhai strategy that content selects by
  name exactly like a built-in; the world keeps the table and dispatches scripted
  strategies each repricing tick, floored safely on any script error. An end-to-end
  test drives a live market's prices from a Rhai script. Loading `.rhai` from mod
  manifests (rather than registering programmatically) is the remaining step.

Data-driven mods

- RON content (commodities, recipes, systems, ships) loaded through a mod-loader
  with dependency ordering, explicit `overrides` rules, and fail-fast link-time
  validation of every cross-reference into an immutable `Registry`.
- Name-keyed plugin seam: content references behavior by name (e.g. a pricing
  strategy), validated against what the engine can execute — ready for a future
  WASM/Lua runtime with no schema change.

Economy

- Dynamic supply/demand pricing with sticky (eased) prices to damp trade-induced
  boom/bust cycles.
- Price-elastic demand: consumers and refiners buy less of a good when it is
  locally dear, capping scarcity prices at an interior equilibrium.
- Production chains (`ore -> alloys -> machinery -> luxuries`) plus raw production
  and population consumption.
- NPC trader economy: greedy buy-low/sell-high agents that self-correct shortages
  and gluts; idle traders deadhead toward opportunity.
- Risk-aware routing: traders discount a run's profit by destination danger
  (`risk_aversion`), losing fewer ships at the cost of underserving the frontier.

Missions

- Contracts (`drift-economy::contract`): a board of missions with three kinds
  behind a `ContractKind`, all taken and completed through the command pipeline
  (`AcceptContract` / `FulfillContract`) and carrying stable, never-reused
  `ContractId` handles on the `Snapshot`/`WorldView` wire:
  - **Delivery** — cargo runs generated from live market shortfalls (a system
    starved of a good posts an import contract paying a premium over its spot
    price); completed by carrying the goods to the destination.
  - **Courier** — carry a parcel between two connected systems for a
    danger-scaled fee; completed on arrival, no goods.
  - **Bounty** — destroy a quota of pirates near a lawless system; progress is
    credited as the holder's trader wins ambushes, and the reward is claimed at the
    station.
  Generation rotates through the kinds and deadline expiry runs in a
  `contract_phase`. The `equilibrium` scenario enables all three by default.
- Contracts panel in `drift-client`: a floating window (toggled from the HUD)
  listing the board — what to deliver, where, the reward, and ticks remaining —
  with Accept and Fulfil buttons for the player's trader, issued through the same
  command sink as the pilot panel. Works in local and networked modes.
- `drift play` contract support: `contracts` lists the board, `accept <id>` takes
  an open contract, `fulfil <id>` delivers a held one at its destination for the
  reward, and the dashboard shows contracts the player is holding.

Financial instruments

The "sophisticated trading" layer (`drift-economy::finance`): three instruments,
all taken at a docked station through the command pipeline, settled in a
`finance_phase`, carried on the resumable `Snapshot` and the `WorldView` wire, with
a `drift-client` Finance panel and `drift play` commands (`borrow`/`repay`,
`insure`, `future`, `finance`). The `equilibrium` scenario enables all three.

- **Loans** — borrow `principal` against a trader (`TakeLoan`); the balance
  compounds interest each accrual period and is repaid in part or full
  (`RepayLoan`). Unpaid past its term, the lender calls the loan and seizes the
  balance from capital — leverage cuts both ways. Stable `LoanId` handles.
- **Insurance** — a premium (`BuyInsurance`) buys a one-time payout if pirates
  destroy the covered trader; policies lapse at term. Rides on the piracy system.
- **Futures** — open a cash-settled long or short position on a commodity
  (`OpenFuture`) at the current spot price for a fee; at maturity it settles
  against the galaxy reference price, crediting or debiting the difference. Rewards
  reading where the economy is heading.

Combat and factions

- 2-D combat model (`drift-combat`): faction targeting AI, steering to engagement
  range, hitscan weapons with distance-based accuracy, regenerating shields and
  hull, and deterministic encounter resolution.
- Multi-tick running battles: encounters play out over several economy ticks
  instead of resolving instantly. `Encounter` is serializable and steppable
  (`advance`); the world holds `ActiveEncounter`s — each with its own seeded RNG so
  a fight's evolution is isolated from other per-tick randomness — opened by the
  piracy and navy phases and advanced a few steps per tick in a `combat_phase`.
  Participants are addressed by stable `PatrolId`s; engaged patrols and traders are
  frozen (they do not roam, arrive, or get pulled into a second fight) until the
  battle completes, at which point casualties, bounties, insurance payouts, and
  bounty-contract progress are applied. Determinism and every existing combat
  outcome property are preserved.
- Persistent roaming pirate fleets with bounties. Danger is emergent — clearing a
  route of pirates keeps it safe until they return, and a danger-free galaxy spawns
  none.
- Trader escorts (convoy protection) and a persistent navy fleet that hunts pirates
  on patrol and defends traders under ambush.
- Protection as a real economic cost: escorts charge a per-jump fee to the trader
  they protect, and the navy costs upkeep per ship drawn from a public treasury
  funded by per-tick income — an underfunded navy runs a deficit, cannot reinforce,
  and shrinks under attrition. Tracked in `PiracyStats` and `World::treasury()`.
- Running battles on the wire: `EncounterView` (a battle's system and live
  combatant state) rides the `Snapshot` and `WorldView`, and the graphical client
  draws a pulsing marker with the combatant count at each system where a multi-tick
  fight is under way.

3-D flight/combat

- `drift-flight`: the 3-D flight/combat client. M1 (combat spectator) renders the
  galaxy in 3-D and animates the running battles from `EncounterView`. M2 adds
  **in-system real-time flight wired to the simulation**: arcade flight (the tested,
  engine-agnostic `flight` model) with a chase camera, the player spawned as a real
  trader through the command pipeline, the current star system populated with the
  agents the sim reports docked there, jumping between systems (`Command::Jump`),
  and **cockpit trading/docking** — fly to the station, dock, and a market panel
  buys/sells through `Command::Buy`/`Sell`. M3 adds **real-time combat**: the
  current system's pirates spawn as live hostiles that steer toward the player and
  fire, the player shoots back (`F`), and a tested `combat::Health` model
  (shield-then-hull) resolves hits. Combat is client-side and authoritative for the
  player's own fight, and its **outcomes are reported back to the sim** through two
  new commands — `DestroyedPirate` (removes the pirate, pays the bounty, credits
  bounty contracts) and `TraderDestroyed` (destroys the trader, pays insurance) —
  so a kill or a loss in the cockpit updates the simulated galaxy. Combat has
  **weapon variety** — three selectable player weapons (a fast Pulse, a heavy
  Cannon, and a three-bolt Scatter), cycled with `Tab`, each with its own damage,
  fire rate, and bolt visuals — and **explosions**: a growing emissive flash and a
  burst of fragments when any ship dies. The determinism firewall holds: the
  free-flight position never enters the sim; only validated commands flow back. The
  Bevy app lives behind a `gui` feature so the default workspace build/test/lint
  stays fast and graphics-free; the tested `flight`/`scene` models carry the
  verifiable logic. See `docs/dev/flight-combat.md` for the architecture and M1–M4
  staging.

Player and clients

- Command pipeline: player actions are validated `Command`s applied at a tick
  boundary; agent ownership (`Owner`); stable, never-reused `TraderId` handles. The
  multiplayer-ready input path (single-player is the N=1 case).
- Interactive player CLI (`drift play`): fly a trader through the living galaxy
  (buy/sell/jump/wait/status/map), with pirate ambushes and bounties narrated in
  transit.
- Graphical observer client (`drift-client`, egui/eframe): a live galaxy-map view
  (systems coloured by danger, jump edges, agents animated along their routes) over
  a fixed-timestep sim loop, with pause/speed controls, a piracy HUD, and a
  colour-coded, per-category-filterable event log.
- Per-node market panels: click any system to open a floating panel with its
  danger, market goods (price, stock, equilibrium, and a surplus/shortage tag), and
  production chains. Reads the same `WorldView.markets` served locally or over the
  wire, so it works in both single-player and networked modes.
- On-map combat flashes: each recent fight pulses a fading, expanding ring at the
  system where it happened (decaying in ticks, so it honours pause and speed),
  gated by the same per-category filters as the log. `SimEvent` now carries an
  optional `system` so a viewer can place events on the map.
- CLI subcommands: `validate`, `run` (with `--dump`, `--log`, `--log-stream`),
  `inspect`, `battle`, `play`.
- Authoritative networked server (`drift-server`): a `Session` plus a TCP socket.
  Clients connect, send serialized `Command`s, and receive state; the server ticks
  the one canonical world at a fixed low rate and broadcasts each tick's events,
  with a full snapshot on connect and every `snapshot_every` ticks. Length-prefixed
  JSON framing (reusing the existing serde `Command`/`SimEvent`), `std`-threads only
  (no async runtime), one thread mutating the world so determinism holds.
- Shared wire-contract crate (`drift-proto`): the `ClientMessage`/`ServerMessage`
  types, the length-prefixed JSON framing, and `WorldView` — the owned mirror a
  client deserializes a broadcast snapshot into (the server sends a borrowed,
  serialize-only `Snapshot`).
- Networked client mode (`drift-client --connect <addr>`): the graphical client
  now renders from a read-model fed by either an in-process `Session` or a remote
  server. A background thread receives broadcasts into an owned `WorldView` and a
  bounded event log; the client interpolates agent motion between the server's
  ticks.
- Player controls (Pilot panel): launch a ship, buy/sell against the docked
  market, jump to a connected system, or retire — issued through one command sink
  that queues on the in-process `Session` (single-player) or sends to the server
  (networked). `WorldView` now carries per-system markets so the client can price
  buys and sells. The player finds its own trader by owner in the received state.

Observability

- Continuous integration (GitHub Actions): every push to `main` and every pull
  request runs `make lint` (clippy, warnings denied) and `make test` (the full
  workspace suite) on Ubuntu, installing the eframe/egui system libraries so the
  graphical client builds too.
- Simulation event log: a deterministic `SimEvent` stream (ambush win/loss, navy
  suppression battles, respawns, contract post/accept/fulfil/expire) read via
  `World::events()`, shown in the client log panel, printed as a tail by
  `run --log`, and streamed live tick-by-tick to stdout by `run --log-stream`. Each
  event carries an optional `system` locating where it happened.

Content and docs

- Core mod: an 8-system production-chain galaxy with lawless frontier systems.
- Scenarios: `equilibrium` (law-enforced, with navy and escorts) and `frontier`
  (lawless hard mode: heavy piracy, no navy or escorts).
- Developer docs under `docs/dev/`: `architecture.md`, `roadmap.md`,
  `multiplayer.md`.

### Changed

- Renamed the project from `cobra` to `drift` (crates `drift-*`, binary `drift`).
  The "Cobra Mk III" ship keeps its name — that is Elite content, not the project
  name.
- `World` now owns `Arc<Registry>` (no `'r` lifetime), so it can be held in a
  long-lived client app or server session.
- The `InTransit` variants of `TraderLocation`/`PatrolLocation` now carry `origin`
  and `departure`, so a client can interpolate an agent's position along its jump
  edge.
- Trade route selection ranks candidates by risk-adjusted expected value rather
  than raw profit.

### Fixed

- Mod dependency toposort produced a reversed load order.
- Discrete, lumpy trading induced a price limit cycle pinned at the clamp; resolved
  with sticky prices.
- Traders could strand at starved systems (they could only depart by buying, and a
  starved system has nothing to buy); they now deadhead toward opportunity.
- Supply elasticity keyed on a producer's own (glutted, cheap) local price
  perversely throttled exports; demand-side elasticity on the consumed good is used
  instead, which also gives intermediate goods a price-restoring force.
