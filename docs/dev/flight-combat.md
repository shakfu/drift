# Bevy 3-D flight & combat — scoping

Forward-looking scope for Drift's **real-time, Elite-inspired 3-D flight and
combat** layer, the endgame the README defers "until the engine — renderer and
running-battle model — can support it." For the current state see
[architecture.md](./architecture.md); for the sequenced roadmap see
[roadmap.md](./roadmap.md); for the netcode model see [multiplayer.md](./multiplayer.md).

This is a scoping document, not a plan of record — it frames the decision, the
architecture, the hard problems, and a staged path. Nothing here is built yet.

## The one thing to get right first

**A 3-D flight layer is a new real-time subsystem, not a rendering pass over the
existing simulation.** The galaxy sim produces *no continuous ship kinematics*:
everything between systems is abstract (`InTransit { origin, dest, departure,
arrival }` interpolated for display), the economy advances on a discrete low-rate
tick, and combat is resolved by the headless `drift-combat` model. There is nothing
to "just render in 3-D and fly." A cockpit layer means adding a **real-time,
per-frame, in-system world** for the player's current location that runs *alongside*
the abstract galaxy sim.

The roadmap already names this fork — (A) strategic galaxy client vs (B) cockpit
flight — and flags the top risk: *quietly scope-creeping into a flight sim under the
banner of "adding graphics."* This document exists to make (B) explicit and bounded.

## What already exists that this builds on

The groundwork is further along than it looks:

- **`drift-combat` is a kinematic combat model.** It already integrates continuous
  position/velocity, steering to engagement range, hitscan weapons with
  distance-based accuracy, and shields/hull — just in 2-D and headless. It is the
  *seed* of 3-D combat, not a throwaway.
- **Multi-tick running battles exist.** Encounters now play out over several ticks
  (`ActiveEncounter` + `combat_phase`), each with its own RNG, participants keyed by
  stable `PatrolId`. This *is* the "running-battle model" the README was waiting
  for.
- **Battle state is already on the wire.** `EncounterView` (a battle's system and
  live combatant positions/state) rides the `Snapshot` and `WorldView`. A 3-D client
  can read it today and animate a fight — no sim change required.
- **The command pipeline + server authority.** Player actions are validated
  `Command`s applied at tick boundaries; the sim is deterministic and
  server-authoritative. This is both the asset (a clean seam to feed outcomes back
  through) and the constraint (see the determinism firewall below).
- **The renderer-never-feeds-back invariant.** The existing 2-D client already
  proves the discipline: rendering reads sim state and never mutates it. The 3-D
  layer must obey the same rule.

## Two combat models — decide which, or both, and in what order

These are fundamentally different products; conflating them is how the scope
explodes.

### Model 1 — *Watch* (deterministic replay in 3-D)
The headless model resolves the fight (as it does now, over ticks); the 3-D client
**animates** it from `EncounterView` positions promoted to 3-D. The player spectates
their ship fighting (think Elite's external/tactical view, or an auto-resolved
skirmish you watch). 

- **Reuses everything.** No new physics, no new combat, no netcode change,
  determinism untouched.
- **Effort:** a rendering + interpolation task. Weeks, not quarters.
- **Ceiling:** the player never *pilots*. It is a spectacle layer, not the Elite
  feel.

### Model 2 — *Pilot* (real-time player control)
The player directly flies in real-time 3-D — thrust, rotate, aim, fire — and the
outcome of a piloted fight feeds back into the abstract sim (ship lost, cargo
forfeited, bounty earned). This is the actual endgame, and it is a large, mostly-new
subsystem:

- A real-time in-system **flight layer** (arcade or Newtonian 6-DOF).
- Real-time **combat** (aiming, projectiles/hitscan, damage) at frame rate.
- **Reconciliation** with the deterministic sim (piloted outcomes become commands).
- For multiplayer, a **real-time netcode** model (prediction/rollback) distinct from
  the current low-tick command broadcast.

## Proposed architecture — two clocks, one authority

Run two loops with a strict authority boundary:

- **Galaxy sim (existing).** Fixed low tick, deterministic, **authoritative** for the
  economy, persistence, contracts/finance, and all agent behavior *between* systems.
  Unchanged by the flight layer.
- **Local flight layer (new, Bevy).** Real-time, per-frame, scoped to the player's
  *current system*: the player ship's kinematics, the other agents the sim says are
  in that system rendered as real-time entities, and real-time combat.
- **The seam.**
  - On arrival in a system, the flight layer **spawns** real-time entities for the
    traders/pirates/navy the sim reports there (from the read-model).
  - A fight the player is in plays out **in the flight layer**; its *outcome*
    (destroyed / survived / kills / cargo) is reported back to the sim as a
    `Command`/event, so the sim stays the source of truth for persistence.
  - On jump, the flight layer tears down; the sim owns the abstract transit.

### The determinism firewall (non-negotiable)

The deterministic sim is the backbone of replay, snapshots, and multiplayer. A
real-time piloted layer is inherently non-deterministic (human input, variable-step
float physics). Therefore:

> The flight layer is an **edge subsystem**. Its *outcomes* feed the sim through the
> command pipeline as discrete, validated events. The sim **never** reads flight-layer
> state. Bevy/physics types **never** appear in `drift-economy`/`drift-combat`.

This is the same rule the current client already follows — extended to a subsystem
that happens to be authoritative for one thing only: the resolution of the *player's
local, real-time* fights. Everything abstract (NPC-vs-NPC in other systems) still
resolves in the deterministic headless model.

### Consequence: combat has two implementations

Player-local fights resolve in the real-time 3-D layer; remote/abstract fights
resolve in headless `drift-combat`. That is a deliberate split, but it means two
combat codebases that must stay **balance-consistent** (same ship stats, comparable
outcomes). Mitigation: share the *data* (ship `CombatStats`) and keep the real-time
layer's tuning traceable to the headless model, so a Cobra beats a pirate in both.

## Bevy specifics

Bevy is the right engine **if 3-D is a firm commitment** — an ECS with 3-D
rendering, glTF asset pipeline, input, and audio in one. The roadmap's own guidance:
*pick Bevy if 3-D is a firm goal; pick macroquad+egui if the near-term goal is "see
the sim live" cheaply.*

- **Integration pattern.** The sim is a library (`World::tick`). Run it inside Bevy
  as a resource ticked on a **`FixedUpdate`** schedule at the sim rate, while the
  flight layer runs in **`Update`** (per frame) and interpolates. This mirrors the
  existing fixed-timestep accumulator in `drift-client`, now with Bevy's scheduler.
- **Crate structure.** A **new leaf crate** (`drift-flight`, say) depends on the sim
  crates and `bevy`; the sim crates stay Bevy-free, exactly as `drift-client`
  depends inward and never the reverse. The existing read-models (`WorldView`,
  `Snapshot`, `EncounterView`, `ViewData`) likely suffice as the sim→render
  boundary; add a view type only if a real need appears.
- **Ship visuals as data.** Already a TODO — a `sprite`/`model`/`color` on `ShipDef`
  or a client-side asset manifest keyed by ship id. Placeholder primitives first;
  glTF later. Do not block the flight loop on art.
- **Real costs.** Bevy's version churn (breaking releases), compile times, and binary
  size are genuine. Pin a version, budget CI time, and expect periodic migration.

## Hard problems / failure modes

1. **Two sources of truth for combat** (above) — bound by making the flight layer
   authoritative only for the player's local fight, reporting outcomes back.
2. **Determinism erosion** — bound by the firewall: outcomes in, never state out.
3. **Real-time multiplayer** is the scope multiplier. Two players dogfighting in one
   system in real time need prediction/rollback or lockstep — a whole networking
   layer unlike today's command broadcast. **Do not** attempt it before single-player
   3-D flight ships.
4. **Flight-sim scope creep** — 6-DOF Newtonian flight, docking computers, targeting,
   weapon variety, AI dogfighting are each projects. Start arcade (thrust + rotate,
   one weapon, `drift-combat`'s existing targeting for AI).
5. **No assets** — primitives first; data-driven visuals; glTF as a later content
   pass.

## Staged milestones

1. **M1 — 3-D combat spectator.** A Bevy client renders the galaxy/system and
   **animates multi-tick battles from `EncounterView`** in 3-D. No piloting. Reuses
   the headless combat model and data already on the wire; proves the Bevy
   integration and the sim-as-`FixedUpdate`-plugin pattern; touches neither
   determinism nor netcode. *(Model 1 — the cheap, high-leverage entry point.)*
2. **M2 — single-player in-system flight, no combat.** The player flies their ship in
   real time within a system (arcade flight), docks/undocks, and sees other agents as
   real-time entities; jump/dock feed the sim as commands.
3. **M3 — single-player real-time combat.** The player fights in real time; the
   flight layer is authoritative for the player's fights and reports outcomes to the
   sim; `drift-combat`'s AI/weapon logic ports to 3-D for NPC behavior.
4. **M4 — real-time multiplayer flight** *(much later, separate)*. Prediction/rollback
   netcode over the server-authoritative model.

## Recommendation

- **Bevy, yes — if 3-D is a firm commitment.** For a cheap "see it live" client,
  macroquad+egui would do; for the piloted endgame, Bevy is correct.
- **Start at M1.** It is the lowest-risk, highest-leverage step: it reuses the
  running-battle model and the `EncounterView` data already on the wire, delivers
  visible 3-D combat, and de-risks the Bevy/sim integration before any real-time or
  netcode complexity.
- **Hold the determinism firewall absolutely.** The flight layer is an edge
  subsystem; outcomes feed the sim as commands; the sim never depends on it; Bevy
  types never leak inward.
- **Defer real-time multiplayer flight.** It is the scope multiplier and is not
  required for a compelling single-player 3-D experience.

The through-line: the headless simulation, the multi-tick running-battle model, and
`EncounterView` on the wire were the prerequisites the README named. They exist now.
M1 is the first step that turns them into something you can watch in 3-D, and the
staircase from there to piloted combat is explicit and bounded.
