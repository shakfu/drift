# Multiplayer readiness

How `drift` is designed to scale to multiplayer, and what has been provisioned for
it so far. For the current architecture see [architecture.md](./architecture.md);
for the broader plan see [roadmap.md](./roadmap.md).

The intent is **not** to build networking now. It is to make a small number of
design decisions that keep multiplayer a *layering* project rather than a rewrite.

## Where we already are

The single most valuable property for networked simulation is **determinism**, and
it already exists, tested and guarded:

- No wall-clock, no `HashMap` iteration, no `thread_rng` in simulation logic; a
  single seeded `DetRng`. Same seed produces a byte-identical run.
- A **headless core** (the simulation runs with no renderer) — exactly what a
  dedicated server needs.
- A **discrete tick** simulation — networking keys naturally off ticks (input
  scheduling, ordering, replay all reference ticks).
- A serializable **`snapshot()`** — the basis for state sync, save/resume, and
  late-join.

## The difficulty is bimodal

- **The abstract galaxy/economy layer** (everything today) is *easy* to make
  multiplayer. It is turn-like: trades and combat resolve on ticks, not
  milliseconds. A server-authoritative model at a low tick rate fits, and most of
  the machinery already exists.
- **A real-time cockpit/flight layer** (does not exist) is where hard netcode lives
  — client prediction, interpolation, lag compensation, rollback. It is
  independent and only needed for co-located real-time flight.

Multiplayer for the game *as shaped today* is therefore very achievable; the scary
parts of a "space MMO" all live in a layer that has not been built.

## Recommended model: server-authoritative, low tick rate

The server runs the one canonical `World`; clients send **commands** (intents) and
receive **state** (snapshots/deltas). This prevents cheating, tolerates client-side
non-determinism (clients are observers), and scales to many players. Determinism
remains valuable server-side (reproducibility, debugging, save/restore, optional
prediction) but correctness does not depend on clients simulating identically.

### Alternative: deterministic lockstep (send inputs only)

Every client runs the full simulation; the server only orders inputs.
Bandwidth-light and elegant for small-N co-op, but fragile: one non-determinism
bug or one cheater desyncs everyone, and it demands **bit-identical cross-platform
floating point**. The simulation is `f64`-heavy; `f64` is deterministic on one
platform but not guaranteed identical across compilers/architectures. Prefer
server-authoritative. Keep lockstep only as an option for a trusted co-op mode, and
if it is ever pursued, isolate determinism-critical math behind fixed-point first.

## The structural decision: single-player as an in-process client/server

Structure single-player as the N=1 case of the multiplayer loop:

```
              commands                       snapshot/deltas
  Client  ---------------->  Server(World)  ---------------->  Client
 (render, input)            tick pipeline                    (render)
```

If single-player runs client and server in one process over a loopback channel,
multiplayer becomes a **transport swap, not a rewrite**. The trap to avoid is the
opposite: letting UI mutate the world directly (e.g. `world.markets[x].buy(...)`),
which is unorderable and un-networkable.

## Provisions

### Made now (this repo)

- **Commands applied at a tick boundary** (scaffolded — see below). Every player
  action is a serializable `Command` drained and validated in a `command_phase`
  that runs first in `tick()`. Single-player enqueues locally; multiplayer enqueues
  from the network. This is the load-bearing provision.
- **Agent ownership.** `Trader` carries an `Owner` (`Npc` or `Player(PlayerId)`).
  NPC-owned traders run the greedy AI; player-owned traders act only on commands.
  This is the "who controls what / whose command is valid" model.
- **Player-as-agent.** The player's ship is a `Trader` in the same collection as
  NPCs, so the world already handles N players uniformly — there is no
  `world.the_player` singleton.
- **Stable agent handles.** Commands address a trader by a `TraderId` — a
  monotonic, never-reused id assigned by the world and observed in state — not by a
  vector index. A stale id (its trader removed) simply fails to resolve, so traders
  can be added and removed safely and a server can echo ids to clients without an
  ABA hazard. `Command::Despawn` exercises removal.
- **Determinism discipline** kept intact (no new wall-clock / unordered iteration;
  the single seeded RNG advances the whole world; the id counter is part of state).

### To make when the work reaches them

- **Session/driver type — DONE.** `drift-sim::Session` owns the `World` and
  centralizes loading, command application, ticking, per-tick event draining, and
  snapshots. The CLI and the graphical client both drive it; a server is that plus a
  socket.
- **`World<'r>` -> `Arc<Registry>` — DONE.** The world now owns `Arc<Registry>`
  (no lifetime), so it holds cleanly in a long-lived session/resource. (Done as the
  graphical-client prerequisite.)
- **Authoritative networked server — DONE.** `drift-server` is the `Session`
  plus a socket (see below).
- **Networked client — DONE.** `drift-client --connect <addr>` renders an owned
  `WorldView` mirror of the server's broadcasts and drives player commands from a
  Pilot panel (see below).

### Deferred (premature now)

Delta encoding and interest management (needed only at scale); client prediction /
rollback / lag compensation (needed only for real-time flight); accounts / auth /
persistence backend.

## The command pipeline (scaffold)

Implemented in `drift-economy`:

- **`command.rs`** — `PlayerId`, `Owner { Npc, Player(PlayerId) }`, `TraderId`, the
  `Command` enum (`Spawn`, `Despawn`, `Jump`, `Buy`, `Sell`), and `CommandError`.
  `Command` and its operands are serde-serializable, i.e. already wire-ready.
  Traders are addressed by stable `TraderId`, resolved to the current slot at apply
  time.
- **`World::queue_command(cmd)`** — the single input entry point (local now,
  network later).
- **`World::command_phase()`** — runs first each tick; drains the queue and applies
  each command through `apply_command`, which **validates** ownership, reachability,
  funds, stock, and hold capacity. Invalid commands are rejected (counted, not
  fatal) — essential because multiplayer input is untrusted. Applying at the tick
  boundary (not on receipt) is what makes ordering deterministic.
- Player-owned traders are skipped by the NPC trading AI; their arrivals and
  respawns are still processed, but their buy/sell/jump decisions come only from
  commands.

Observability: `World::commands_applied()` / `commands_rejected()`.

Because no scenario spawns player traders and no commands are queued in the
existing runs, `command_phase` is a no-op there and the simulation is byte-identical
to before — so determinism and all existing tests are unaffected.

## The server (`drift-server`)

The authoritative server realizes the recommended model directly: it is a
`drift-sim::Session` plus a socket. Nothing about the simulation changed to make
this work — the command pipeline, `Session`, and serde-serializable `Command` /
`Snapshot` / `SimEvent` were the whole provision.

- **Transport.** TCP with length-prefixed JSON framing (a 4-byte big-endian
  length, then that many bytes of a JSON message). JSON reuses the serde derives
  already on `Command` and `SimEvent` and keeps the protocol language-agnostic;
  the length prefix makes messages self-delimiting on a stream. The wire contract
  lives in its own crate, `drift-proto` (so a client depends on the contract, not
  on the server binary): `ClientMessage { Command }`, `ServerMessage { State {
  tick, events, snapshot } }`, the shared `read_msg`/`write_msg` framing, and
  `WorldView` (below).
- **No async runtime.** The simulation is turn-like at a low tick rate, so plain
  `std` threads suffice and stay trivially testable: one accept thread, one reader
  thread per client, and the **sim thread** that owns the `Session`. All network
  traffic funnels through a single channel of input events, so exactly one thread
  mutates the world — determinism is preserved. Wall-clock is used only to
  *schedule* ticks (via `recv_timeout`), never inside simulation logic.
- **Authoritative loop.** The sim thread selects between "a client input arrived"
  and "the next tick is due". Commands are queued as they arrive and applied at the
  next tick boundary in arrival order (the existing `command_phase`), so ordering
  is deterministic and validation rejects untrusted input without crashing.
- **State broadcast.** After each tick the server broadcasts a `State` with that
  tick's events; the full snapshot rides along only every `snapshot_every` ticks
  (and once immediately on connect) to bound bandwidth. The snapshot is carried as
  a `serde_json::Value` so the server can serialize the borrowed `Snapshot`
  without an owned mirror type. Full-snapshot-per-interval is the simple correct
  baseline; delta encoding and interest management are the scaling path (deferred).
- **Single-player is unchanged.** It remains an in-process `Session`; the server is
  an *alternative host* for the same façade, not a replacement.

## The networked client (`drift-client --connect`)

The graphical client renders from a **read-model** that either an in-process
`Session` or a networked server fills, so the renderer is written once and the
network is just an alternate state source.

- **Owned mirror.** The server broadcasts a borrowed `Snapshot` serialized to
  JSON; that type is serialize-only, so the client deserializes into `WorldView`
  (in `drift-proto`) — an owned struct declaring just the fields a client needs
  (serde ignores the rest). This is the "non-snapshotable state" failure mode
  avoided in practice: everything the client needs round-trips through JSON.
- **Non-blocking UI.** A background reader thread decodes broadcasts and keeps the
  latest `WorldView` plus a bounded event log behind a mutex; the UI thread reads
  a cheap clone each frame and never blocks on the socket.
- **Interpolation without a local clock.** The server ticks at its own low rate;
  the client interpolates agent motion between received ticks using a running
  estimate of the inter-tick wall-clock interval (the `InTransit { origin,
  departure, arrival }` fields make this a pure lerp along the jump edge).
- **Static content is local.** Only mutable state is sent; the client loads the
  same mods locally for system positions, names, danger, and jump edges. Identical
  content means identical interning, so market/system indices align. A
  **content-version handshake** enforces that invariant: on connect the client
  sends `Registry::content_hash()` (a fixed FNV-1a fingerprint of the fully-linked
  data in interned order) as a `ClientMessage::Hello`, and the server refuses a
  mismatch with a `ServerMessage::Reject` before the client ever enters the sim.
  Mismatched mods therefore fail loudly at connect instead of silently rendering a
  desynced world.
- **Player controls.** A "Pilot" panel drives the command path from the UI:
  launch a ship, then buy/sell against the docked market, jump to a connected
  system, or retire. It issues through one command sink — queued on the local
  `Session` in-process, or sent to the server when networked — so the *same* panel
  is single-player and multiplayer. The player finds its own trader by owner in
  the received state (no id bookkeeping), and the server validates every command
  authoritatively, so the UI can issue optimistically and a rejection is a no-op.

## Failure modes to guard against

- **Direct-mutation UI** bypassing commands (the biggest one).
- **A `the_player` singleton** baked into `World`.
- **Non-snapshotable state** creeping into `World` (breaks save/sync/late-join).
- **Choosing lockstep, then discovering cross-platform `f64` drift.**
- **Wall-clock or unordered iteration** sneaking into simulation logic later.
