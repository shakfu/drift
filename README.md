# drift

A Rust take on the Elite/Oolite space sim: **real-time piloted space combat and flight on top of a deep, deterministic trading economy and a mod system.** Drift began as a headless economy core and has grown into a playable 3-D flight client, a mod-scripting runtime, a player/mission/finance layer, and a server-authoritative multiplayer scaffold — all driven by one deterministic simulation.

The architecture keeps a hard line between two clocks. The **abstract galaxy simulation** is deterministic, headless, and testable — the source of truth for the economy, piracy, and every NPC agent. The **real-time flight layer** (the Bevy client) is where you personally fly and fight. The flight layer *reads* sim state and reports outcomes back only as validated commands, so the core stays byte-for-byte deterministic no matter what happens in the cockpit. This "two clocks, one authority" firewall is the load-bearing design decision.

## What runs today

### The living economy (headless core)

A galaxy of star systems, each a specialized marketplace, is loaded entirely from mods. NPC traders arbitrage price differences between systems; their buying and selling is exactly what pulls prices back toward equilibrium. Left to run with no player, the economy settles into a stable, plausible price structure.

- **Dynamic supply/demand pricing** — prices follow stock relative to an equilibrium anchor, not fixed tables. Prices are sticky (eased toward target) to keep discrete trading from inducing boom/bust cycles.

- **Price-elastic demand** — consumers (population and refiners alike) buy less of a good when it is locally dear. This negative feedback caps scarcity prices at an interior equilibrium instead of pinning a chronically short good at its clamp.

- **Production chains** — `ore -> alloys -> machinery -> luxuries`, plus raw farming/mining and population consumption. Specialization creates the trade.

- **NPC trader economy** — greedy buy-low/sell-high agents whose flows self-correct shortages and gluts; idle traders deadhead toward opportunity rather than stranding at starved systems.

- **Risk-aware routing** — traders value a run by its risk-adjusted expected value, discounting profit by the destination's danger (`risk_aversion`). Cautious traders shun valuable cargo runs into lawless space: they lose far fewer ships (and end up richer), at the cost of underserving the frontier.

- **Persistent pirate fleets & bounties** — pirates are first-class roaming agents that congregate in lawless (`danger > 0`) systems, carry persistent battle damage between fights, and are periodically reinforced toward a target fleet size. A laden trader arriving at a pirate-held system may be ambushed; a victorious trader collects a **bounty** per kill. Danger is emergent — clear a route of pirates and it stays safe until they return — and a fully-safe galaxy never spawns any. Combat and economy are one system: losses choke frontier supply and push manufactured-goods prices up.

- **Escorts & navy patrols** — convoys can hire escort fighters that join the trader's side in an ambush, and a persistent navy fleet patrols the frontier: it hunts pirates where it finds them and defends traders under attack. Escorts charge a per-jump fee and the navy draws upkeep from a public treasury, so law enforcement is a real economic cost — an underfunded navy runs a deficit and shrinks under attrition.

- **Multi-tick running battles** — encounters play out over several economy ticks rather than resolving instantly. Each `Encounter` is serializable and steppable, with its own RNG; engaged patrols and traders are frozen until the fight completes, when the casualty/bounty/insurance/contract hooks fire.

- **Determinism** — a seeded RNG drives the whole simulation; the same seed produces a byte-identical run, with a serializable `Snapshot` for state dumps and resume.

### Play & flight

- **3-D flight & combat (Bevy)** — fly your own ship in-system with an arcade chase-camera feel, dock at the station to trade (buy/sell over the live market), jump between systems, and **dogfight pirates in real time**. Combat has weapon variety (a fast Pulse, a heavy Cannon, a three-bolt Scatter, cycled with `Tab`), shield-then-hull damage, and explosions. Kills and losses **report back to the sim** — a destroyed pirate is removed from the galaxy and pays its bounty; your death destroys the trader (cargo lost) and pays out insurance. You are a real trader spawned through the same command pipeline as everything else.

- **Interactive trader (CLI)** — play the living galaxy as text: buy low, run cargo through pirate space, fight or flee, sell high, take out loans and contracts.

- **Missions & contracts** — a job board with delivery, courier, and bounty contracts on top of the economy plumbing.

- **Financial instruments** — loans (interest compounds; overdue loans are called), insurance (a premium buys a payout if your trader is destroyed), and cash-settled futures (long/short at spot, settled against the galaxy reference price at maturity).

- **Graphical observer & player client (egui)** — a live galaxy-map view of the running sim (systems coloured by danger, jump edges, animated ships, on-map combat flashes, a colour-coded event log) with a Pilot panel that drives launch/buy/sell/jump/retire through the command pipeline.

### Platform

- **Data-driven mods** — all content (commodities, recipes, systems, ships) is authored as RON and loaded through a mod-loader with dependency ordering, explicit override rules, and fail-fast link-time validation.

- **Mod scripting (Rhai)** — a sandboxed, fuel-limited scripting runtime (chosen over Lua/WASM for being pure-Rust and sandboxed by construction). A Rhai-authored pricing strategy plugs into the same name seam as the built-ins: a system can name a scripted strategy exactly like `supply_demand_v1`.

- **Server-authoritative multiplayer** — a networked server (`drift-server`: TCP + length-prefixed JSON over the command pipeline) and a networked client that mirrors an authoritative `WorldView` and renders server broadcasts. The player client drives a single command sink whether it is a local session or a server.

## Workspace

| Crate           | Responsibility                                                        |
|-----------------|-----------------------------------------------------------------------|
| `drift-core`    | Primitives: typed ids + interning, deterministic RNG, tick, money, the `Step`/`SimContext` seam, and the `NamedRegistry` plugin seam. |
| `drift-data`    | The moddable content schema (pure serde defs).                        |
| `drift-mods`    | The mod-loader: discover, order, merge, and link content into an immutable `Registry`. |
| `drift-economy` | The simulation: markets, pricing, production, NPC traders, piracy, contracts, and finance — the `World`. |
| `drift-combat`  | 2-D combat model: factions, targeting AI, hitscan weapons, shields, and serializable multi-tick encounter resolution. |
| `drift-script`  | The mod-scripting runtime (Rhai): sandboxed, fuel-limited strategies plugging into the name seam. |
| `drift-sim`     | Session/driver façade owning a `World`: loading, command application, ticking, event draining, snapshots. |
| `drift-proto`   | The client/server wire contract (`WorldView`, snapshots, events, commands). |
| `drift-server`  | The authoritative networked host (TCP, std threads, no async).        |
| `drift-cli`     | Driver: `validate`, `run`, `inspect`, `battle`, and `play` (interactive). |
| `drift-client`  | Graphical observer + player client (egui/eframe): galaxy-map view and a Pilot panel over the command pipeline. |
| `drift-flight`  | The 3-D flight/combat client (Bevy): in-system real-time flight, trading, and combat, behind a `gui` feature. |

## The plugin seam

Behavior that mods may vary is referenced from content by *name* (`pricing: "supply_demand_v1"`) and resolved through a registry of strategies. The loader validates those names against what the engine can execute, so content fails fast on a typo. This seam is live for scripting: `drift-script` registers a Rhai-authored `Scripted` pricing strategy under a name, and a market can select it exactly like a built-in. Because the data model is fully data-addressable and versioned, further hooks (trader AI, event rules) and a determinism-hardened `wasmi` swap-in plug into the same seam with no schema or caller changes.

## The determinism firewall

The real-time flight layer never mutates the simulation directly. It reads sim state to render, and reports what happened in the cockpit — a kill, a death, a trade, a jump — as validated `Command`s applied at a tick boundary. The abstract galaxy simulation therefore stays deterministic and byte-identical for a fixed seed, and the flight layer is a pure edge subsystem. Sim types never leak into the renderer, and engine types never leak into the sim crates. The tested, engine-agnostic `flight` (kinematics) and `combat` (shield/hull) models carry the verifiable logic; the Bevy app is a thin driver over them.

## Usage

```sh
make test                 # full workspace test suite (must stay green)
make validate             # load + link the bundled mods, report errors
make run                  # run the equilibrium scenario (override: make run TICKS=5000 SEED=7)

# Fly and fight in 3-D (Bevy; behind the `gui` feature). Fly to the station ring and
# Space to dock/trade, 1-9 to jump, F to fire, Tab to switch weapon. Use the lawless
# frontier scenario for immediate pirate contact:
cargo run -p drift-flight --features gui -- scenarios/frontier.ron

# Play the living galaxy as a trader (interactive text): buy low, run cargo through
# pirate space, fight or flee, sell high, take loans/contracts. Try the lawless
# `frontier` scenario for real danger:
cargo run -p drift-cli -- play --mods mods/ --scenario scenarios/frontier.ron

# Watch the living galaxy in a window (egui/eframe): systems coloured by danger,
# jump edges, ships animated along their routes, pause/speed controls, piracy HUD,
# a live colour-coded event log, and a Pilot panel to fly it yourself:
cargo run -p drift-client -- --scenario scenarios/frontier.ron

# Server-authoritative multiplayer: run the host, then connect observers/players.
# The client must load the same mods as the server.
cargo run -p drift-server -- --scenario scenarios/frontier.ron --addr 127.0.0.1:4000
cargo run -p drift-client -- --connect 127.0.0.1:4000

# Watch prices converge:
cargo run -p drift-cli -- inspect --mods mods/ --scenario scenarios/equilibrium.ron --ticks 2000 --every 200

# Deterministic state dump (identical for a fixed seed):
cargo run -p drift-cli -- run --mods mods/ --scenario scenarios/equilibrium.ron --seed 42 --dump state.json

# Print the simulation event log (ambushes, bounties, navy battles, respawns).
# --log prints the recent tail at the end; --log-stream streams the full log live,
# tick by tick, to stdout (pipeable: ... --log-stream | grep Piracy):
cargo run -p drift-cli -- run --mods mods/ --scenario scenarios/frontier.ron --ticks 400 --log
cargo run -p drift-cli -- run --mods mods/ --scenario scenarios/frontier.ron --log-stream

# Stage a standalone combat encounter (squadron vs squadron):
cargo run -p drift-cli -- battle --mods mods/ --ship core:python --vs core:cobra_mk3 --per-side 3 --seed 1
```

## Content and formats

Mods live under `mods/<id>/` with a `manifest.toml` and RON content in `commodities/`, `production/`, `systems/`, `ships/`. Human-authored content uses RON; machine state dumps use JSON (which supports the RNG's 128-bit counter). Two scenarios ship: `equilibrium` (law-enforced, with navy and escorts) and `frontier` (lawless hard mode: heavy piracy, no navy or escorts).

## Roadmap (not yet built)

The next horizon is **M4: real-time multiplayer flight** — client prediction/rollback so several piloted ships share one authoritative server, which the current in-process flight layer does not yet do. Other planned work: loading `.rhai` scripts declared in mod manifests (currently registered programmatically) and more scripting hooks; allied escorts/navy fighting alongside you in the flight layer and data-driven ship visuals; a content-version handshake between client and server; and a larger galaxy with a deeper economy. See `TODO.md` and `docs/dev/` for the full picture.
