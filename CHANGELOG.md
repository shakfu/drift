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
  test drives a live market's prices from a Rhai script.
- Scripts loaded from mod manifests (no more programmatic registration): a
  `[[scripts]]` table (`name`, `path`, `kind`) declares a named strategy backed by
  a `.rhai` file. The loader reads the source from disk, enforces unique script
  names across mods, rejects a name that shadows a built-in, and folds the declared
  names into the pricing set it validates each system's `pricing` against — so a
  system selects a disk-authored strategy by name and a typo still fails fast at
  link time. `Registry::scripts()` carries the source through linking;
  `drift_economy::pricing_for(&registry)` compiles them into the run's strategy set
  at `Session` build, aborting with a clear error if a script does not compile.
  `ScriptKind` (currently just `pricing`) is the extension point for future
  trader-AI and event-rule hooks. Scripts are folded into `Registry::content_hash`,
  so a behaviour-mod difference is caught by the client/server handshake too.

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
- Endogenous manufacturing capacity: each transformer industry (a recipe with both
  inputs and outputs) carries a slow capital stock that scales its throughput on
  top of the instantaneous price elasticity. Capacity eases toward a target set by
  the industry's processing margin (output value minus input cost) relative to that
  margin at base prices — investing when the margin is fat, disinvesting when thin,
  with base prices as the fixed point. Capital is deliberately sticky (eased at
  `0.02`/tick against prices' `0.2`), so supply gains a lagged, path-dependent
  response that still converges: in the bundled galaxy manufacturing capital
  settles around `2x` and bids intermediate goods up into a new interior
  equilibrium. Raw extractors and pure consumers are fixed endowments and hold
  nominal capacity. Capacity is part of the `Snapshot` (so a resumed run restores
  it) and is covered by unit tests plus an end-to-end convergence test.
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
  burst of fragments when any ship dies.
- Elite-style targeting and flight feel. A new tested, engine-agnostic
  `drift_flight::targeting` module carries the defining combat math — a projectile
  **lead/intercept** solver (`firing_solution`), a **radar contact** projection
  (`radar_contact` + a unit-disc mapping), and target-selection helpers — with 19
  unit tests (the lead solver is checked for self-consistency: the projectile and
  target genuinely coincide at the solved time). The Bevy app drives it: `T` locks
  and cycles the nearest hostile, drawing a reticle on it and a floating **lead
  pip** at the weapon's intercept point (you fly the nose onto the pip, not onto
  the enemy — fixed weapons fire straight ahead); a bottom-left **contact scanner**
  plots hostiles and the station by bearing/range with behind-contacts dimmed;
  **throttle is set-and-hold** (`W`/`S` slew it, `X` cuts to a stop) instead of
  momentary thrust, with a **zero detent** so the throttle parks cleanly on a dead
  stop rather than sliding through into reverse (crossing into reverse takes a
  deliberate release-and-press); the detent lives in the tested `flight::Throttle`
  and is unit-tested (held reverse from a positive setting never overshoots past
  zero). The HUD gains a target panel (range, closing speed, hull,
  shield, and an on-target cue); and hostiles now **lead their own shots** with the
  same solver and orbit at knife range rather than firing at where you were.
- Gimballed vs. fixed weapons. A weapon may carry a gimbal half-angle: a
  **gimballed** weapon (the Pulse) auto-tracks the locked target within its cone —
  `targeting::gimbal_aim` resolves whether the target is in-arc and returns the
  tracking direction — while **fixed** weapons (Cannon, Scatter) always fire
  straight down the nose, trading forgiveness for punch. The HUD shows the mount
  type and a "GIMBAL LOCKED" / "out of arc" cue. Unit-tested (cone edge in/out,
  target astern) and wired through the same lead solution.
- Navy fights alongside you. Navy patrols the sim reports in the current system
  spawn as live **allies** that hunt the nearest hostile, lead their shots (the
  same `firing_solution` the player and pirates use), and orbit at range — the
  mirror of the hostile AI, firing friendly bolts. Hostiles now engage the nearest
  of the player *or* an ally, so a defended system plays out as a real skirmish
  rather than everyone dogpiling the player. Allies are shown on the HUD (`NAVY n`)
  and the scanner (teal); a downed ally is a local casualty only — the report never
  reaches the sim, which manages its own navy attrition, so the determinism
  firewall holds (the flight layer stays authoritative solely for the player's
  outcomes).
- Oolite-aligned controls, HUD, and 3-D objects (studied from the OoliteProject
  source). **Controls** match Oolite: roll on the left/right arrows, pitch on
  up/down (flight-sim sense), yaw on `,`/`.`, throttle on `W`/`S`, fire on `A`,
  target on `T`. **3-D objects** are the iconic silhouettes, built procedurally
  (no external assets): the station is a rotating **Coriolis cuboctahedron** (six
  square + eight triangular faces) with a docking slot on the face toward you,
  rolling about its docking axis. A `faceted_mesh` helper builds flat-shaded
  low-poly meshes with correct outward normals and winding, plus `dart_hull` and
  `freighter_hull` primitives for fighter and freighter silhouettes.
- Data-driven ship visuals. `ShipDef` gains an optional `visual` block — a
  `HullShape` (`dart`/`freighter`), nose-to-tail `length`, `width`, `height`, and
  an RGB `color` — so a ship's appearance is authored in content, not the client.
  The flight client builds one mesh + tinted material per ship type from the
  registry's `visual` blocks (with a generic fallback) and renders every agent as
  its *own* ship: the bundled `core:*` ships are given hulls, so the player flies a
  Cobra Mk III, navy frigates and pirate raiders and Pythons all look distinct, and
  a mod can add a new ship's look with zero client code. The visual is engine-
  agnostic data (no renderer types) and is deliberately excluded from
  `content_hash` — it is cosmetic and must not gate the multiplayer handshake. **HUD** gains an Oolite-style instrument cluster: speed,
  throttle, hull, and shield **gauges** (bottom-right, with warning colours and a
  reverse-throttle tint), centre-zero **roll and pitch indicators**, a **compass**
  (bottom-centre) whose blip points to the station and dims when it is behind you,
  a locked-target **data panel** with the target's own hull/shield bars
  (top-right), and a fixed centre **gunsight**; the text panel is now just
  nav/jump/market. `combat::Health` gained `max_hull`/`hull_frac` so hull
  reads as a gauge fraction (unit-tested).
- Missiles and ECM (Oolite's signature combat layer). Lock a target (`T`) and fire
  a **homing missile** (`M`) from limited stores (refilled on docking); the missile
  chases its target, turning toward the intercept point at a hard rad/s limit — the
  guidance is a new tested `targeting::home_missile` (unit tests prove it hits a
  stationary and a crossing target, holds constant speed, and that a sluggish
  missile is dodgeable). Hostiles loose their own homing missiles at you on a
  stagger, and **ECM** (`E`, on a cooldown) detonates every incoming missile at
  once — the counter that makes a lock survivable. Warheads hit hard (≈45 damage);
  the HUD shows missile stores and ECM readiness; missiles clear on jump/dock.
- Fuel-limited hyperspace, torus drive, and legal status (more Oolite systems),
  with the rules in a new tested `drift_flight::nav` module. **Hyperspace costs
  fuel** by inter-system distance (`nav::jump_fuel_cost`): a jump you cannot afford
  is refused, and docking refuels the 7.0-LY tank. The **torus drive / fuel
  injectors** (`J`) is a fast cruise to the station that burns fuel and is
  mass-locked (disabled) whenever a hostile is present. **Legal status**: attacking
  the navy raises a bounty (`nav::legal_status` → Clean / Offender / Fugitive), and
  a wanted pilot is hunted by the police — the navy switch from escorting you to
  firing on you. A `FUEL` gauge joins the HUD cluster and the readout shows legal
  status and bounty. The nav math (jump cost, affordability, status thresholds) is
  unit-tested.
- Cargo canisters & scooping (the Oolite loot loop). A destroyed pirate spills
  drifting **cargo canisters** (deterministic from its id, so a fixed seed replays
  identically); flying within scoop range vacuums them into the hold through a new
  authoritative `Command::ScoopCargo`, which adds as much as the hold can carry
  free of charge and rejects a full hold (unit-tested — free, mass-capped, and
  rejected when full). Canisters drift, tumble, and decay, and clear on jump/dock;
  the HUD shows a `CARGO used/cap` readout so a filling hold is visible. This
  closes the kill → scoop → sell loop: shoot a pirate, scoop its cargo, sell it at
  the next station — all riding the existing command pipeline, so the determinism
  firewall holds (the flight layer only *reports* the scoop; the sim owns the
  hold).
- Weapon subsystems and a beam laser. A tested `drift_flight::weapons` module adds
  Oolite-style **laser temperature**: firing builds heat, heat dissipates, and a
  laser pushed to its limit **cuts out** until it cools back (hysteresis so it
  doesn't chatter), making sustained fire self-limiting — unit-tested (overheats
  under sustained fire, comes back online once cool, and measured fire never
  trips). Each weapon has a heat cost, a `TEMP` gauge joins the HUD (amber hot, red
  cut-out), and a fourth weapon lands: a continuous **beam laser** that melts the
  nearest hostile in a forward cone (damage-per-second, rendered as a bright bar)
  and heats up fast.
- Improved ship rendering: hulls are now **metallic** (glossier, catching the
  star's light with a faint self-glow), and every combat ship carries a tail
  **engine glow** (attached as a child of the hull), so pirates and navy read as
  ships rather than flat shapes.
- Natural starfield: the sky was a Fibonacci lattice, whose even spacing read as an
  ordered spiral/grid. It is now scattered by uniform random sphere-sampling from a
  hash of the star index (deterministic — no RNG, identical every run) with
  per-star size variation, so it looks randomly strewn.
- The targeting/weapons math is verified headlessly; the Bevy wiring is compile-
  and clippy-checked (visual tuning wants a real display).
- The determinism firewall holds: the
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
- Content-version handshake: `Registry::content_hash()` fingerprints the
  fully-linked content (a dependency-free FNV-1a over commodities, recipes,
  systems, and ships in interned order, so it is stable across builds and changes
  with any content difference). On connect a client sends the hash as a
  `ClientMessage::Hello`; the server compares it to its own and, on a mismatch,
  replies `ServerMessage::Reject` and drops the connection before the client
  enters the sim. Mismatched mods now fail loudly at connect instead of silently
  desyncing id interning against the authoritative world.
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
