# Roadmap

Forward-looking plan for `drift`. For the current state, see
[architecture.md](./architecture.md).

## Sequenced roadmap

Ordered by dependency and value from the current headless simulation:

1. **Graphical observer client** — DONE (`drift-client`, egui/eframe). A live
   galaxy-map view (systems coloured by danger, jump edges, pause/speed controls,
   piracy HUD) over a fixed-timestep sim loop, with agents **animated along jump
   edges** (interpolated from `InTransit { origin, departure }`), per-node market
   panels, on-map combat flashes, and a contracts panel. Only real-display
   verification remains (no GUI in the dev sandbox). Detailed strategy below.
2. **Player agent** — DONE (interactive CLI). `drift play` lets a human fly a
   trader through the living galaxy over the command pipeline (buy/sell/jump/wait),
   with pirate ambushes and bounties narrated in transit. See `drift-cli/src/play.rs`.
   A graphical player client is a later layer on the same commands.
3. **Missions and contracts** — DONE. One board with three `ContractKind`s —
   delivery (cargo runs from market shortfalls), courier (parcel runs), and bounty
   (pirate kills, credited from ambush wins) — taken and completed through the
   command pipeline (`AcceptContract`/`FulfillContract`), on the `Snapshot`/
   `WorldView` wire, with a `drift-client` panel and `drift play` support.
4. **Financial instruments** — DONE (core three). `drift-economy::finance` has
   loans (`TakeLoan`/`RepayLoan`, interest + called defaults), insurance
   (`BuyInsurance`, payout on a covered loss), and futures (`OpenFuture`,
   cash-settled against the reference price at maturity) — all settled in a
   `finance_phase`, on the `Snapshot`/`WorldView` wire, with a client Finance panel
   and `drift play` commands. Still open under this theme: escort fees and navy
   funding as real economic costs (protection is currently free).
5. **Mod scripting** — IN PROGRESS. Engine chosen and de-risked: **Rhai** (pure
   Rust, sandboxed by construction, operation-limited), in the new `drift-script`
   crate, with a `ScriptedPricing` hook proven deterministic/sandboxed/fuel-limited
   by tests. `wasmi` is the hardening swap-in behind the same seam if
   lockstep-across-clients determinism is ever required. Next: wire scripted
   strategies through the `NamedRegistry` seam and load `.rhai` from mod manifests.
6. **Multi-tick running battles** — DONE. Encounters now play out over several
   economy ticks: `drift-combat`'s `Encounter` is serializable and steppable, and
   the world advances `ActiveEncounter`s (each with its own RNG, participants keyed
   by stable `PatrolId`) a few steps per tick in a `combat_phase`, freezing engaged
   agents and applying outcomes on completion. This is the headless "running-battle
   model" the eventual real-time/3-D combat layer will build on; the client can now
   read a fight's live positions to animate it.

The graphical client is first because it is read-only over an interface that
already exists, and it unblocks the player agent.

## Multiplayer readiness

Multiplayer is a cross-cutting concern rather than a single roadmap step, and the
architecture is deliberately kept ready for it. The recommended path is a
server-authoritative model at a low tick rate, with single-player structured as an
in-process client/server so that going multiplayer is a transport swap rather than
a rewrite. The load-bearing provision — a validated, tick-boundary **command
pipeline** with agent ownership — is already scaffolded. See
[multiplayer.md](./multiplayer.md) for the full design, the provisions made so far,
and what is deliberately deferred.

Concretely, the player agent (roadmap item 2) should be built *on* the command
pipeline: the player's ship is an owned `Trader`, and player actions are `Command`s
applied in `command_phase`. This keeps the single-player and multiplayer code paths
identical.

---

## Graphical client: implementation strategy

### Governing principle: client as an observer over the headless core

The core was built for this. The client is a **new leaf crate** (`drift-client`)
that depends on the simulation crates and never the reverse; the simulation stays
pure. Each frame the client reads `World` state and draws it. This preserves
determinism, testability, and moddability.

### The decision to make first: what kind of client?

This fork determines everything, and it is easy to get wrong by assuming that
adding a game engine means flying a spaceship.

- **(A) Strategic / galaxy-map client (recommended first).** Render what the
  simulation actually computes: the system graph, animated trade / pirate / navy
  flows along edges, live market panels, and combat as outcome events. Think of
  Elite's galaxy map plus market screens, or a 4X map, not the cockpit. Low
  model-mismatch, high value, achievable in weeks.
- **(B) Cockpit / in-system flight client (the classic Elite feel).** The
  simulation produces no continuous ship kinematics, so there is nothing to fly.
  This requires a separate real-time in-system layer for the player's local system
  (ships with continuous position and velocity, real-time combat piloted directly)
  running alongside the abstract galaxy simulation. That is a large, mostly new
  subsystem, not a rendering task.

**Recommendation:** build (A) now; treat (B) as a later, separate milestone that
adds a real-time local layer. Combat already has 2-D kinematics
(`Encounter`/`Combatant`) that could seed a local battle view, but everything
between systems is abstract. (A) is now built; the Bevy 3-D flight/combat scoping
for (B) — architecture, the determinism firewall, and staged milestones M1–M4 — is
in [flight-combat.md](./flight-combat.md).

### Engine choice

- **Recommended for (A): `macroquad` + `egui`** (via `egui-macroquad`). A 2-D
  canvas for the galaxy graph and flows, egui for market and HUD panels. Minimal
  machinery, fast iteration, low risk. The client is mostly "draw nodes and edges
  plus data panels," which is immediate-mode UI's sweet spot.
- **`Bevy`** if the intent is to head toward (B) or 3-D: one ECS engine for 2-D now
  and 3-D later, richer but heavier, and its `Resource`/`'static` model forces the
  lifetime refactor below sooner. Pick Bevy if 3-D is a firm goal; pick
  macroquad + egui if the near-term goal is "see the simulation live" cheaply.

Start with macroquad + egui; adopt Bevy only when a spatial or 3-D cockpit is
actually on the table.

### Integration architecture

- **Simulation / render decoupling.** The simulation is discrete and
  deterministic; rendering is continuous at ~60 fps. Use a **fixed-timestep
  accumulator**: advance `World::tick()` at a fixed simulation rate (adjustable and
  pausable) and **interpolate** the render between the previous and current state.
  Determinism holds because the simulation only advances on fixed steps; speed and
  pause are purely client concerns and never feed back into simulation state.
- **Interpolation support — DONE.** The trader and patrol `InTransit` variants now
  carry `{ origin, dest, departure, arrival }`, so the client lerps position as
  `lerp(origin.pos, dest.pos, (now - departure) / (arrival - departure))` (with a
  sub-tick fraction from the fixed-timestep accumulator).
- **The `World<'r>` lifetime — DONE.** `World` now owns `Arc<Registry>` (no
  lifetime), so it lives cleanly in the client app. `World::new` takes an
  `Arc<Registry>`; `World::registry_arc()` hands back a shared handle.
- **Visuals as data.** Ships have no art yet. Add an optional `sprite` / `model` /
  `color` to `ShipDef` (or a separate client-side asset manifest keyed by ship id).
  Data-driven visuals are consistent with the mod philosophy and are skippable with
  sensible defaults.

### Visualizing combat (the instant-resolution wrinkle)

Encounters resolve in one tick, discarding per-step positions. Options, cheapest
first:

- **Event flash / notification** — show an ambush marker and its outcome. Fits the
  strategic client; do this first.
- **Deterministic replay** — the `Encounter` already steps deterministically; have
  `resolve()` optionally record combatant trajectories, then the client replays
  them over a few seconds. Gives watchable skirmishes without changing outcomes.
- **Real-time local combat** — only relevant for the cockpit client (B).

### Phased plan for the client

1. **Static galaxy view** — draw systems (position, danger) and jump edges from the
   `Registry`. Pause / step controls driving `World::tick()`.
2. **Live agents** — animate traders / pirates / navy along edges (after the
   `origin` field is added); color by faction; show a `piracy_stats` HUD.
3. **Market panels** — egui overlays per selected system: prices, stock versus
   equilibrium, production chains.
4. **Combat events** — ambush and patrol flashes with outcomes; optionally the
   recorded-replay view.
5. **(Later) Player agent and input** — the client stops being read-only: player
   commands feed the simulation. This is roadmap item 2.

### Risks and failure modes

- **Scope-creeping into a flight sim.** The biggest risk is quietly attempting (B)
  under the banner of "adding graphics." Decide (A) versus (B) explicitly and up
  front.
- **Leaking rendering into the simulation.** Keep `drift-economy` and
  `drift-combat` renderer-free; all client concerns live in `drift-client`. If a
  Bevy or macroquad type appears in a simulation crate, stop.
- **Breaking determinism via the client.** Speed, pause, and interpolation must
  never feed back into simulation state; the simulation advances only on fixed
  ticks.
- **Over-investing in art before the loop is fun.** The strategic view is legible
  with primitive shapes; add ship art last.
