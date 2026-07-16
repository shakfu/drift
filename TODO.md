# TODO

Outstanding work for Drift. For rationale and sequencing see
[docs/dev/roadmap.md](docs/dev/roadmap.md); for the multiplayer plan see
[docs/dev/multiplayer.md](docs/dev/multiplayer.md). For what is already built, see
[CHANGELOG.md](CHANGELOG.md).

## Near-term (highest leverage)

- [ ] Verify `drift-client` renders on a real display — it is currently
      compile-checked and unit-tested only (no GUI in the dev sandbox).
- [x] Graphical client: per-node **market panels** (click *any* system to show its
      prices, stock vs. equilibrium, and production chains). Click a node to open a
      floating panel; click empty space to close. Reads the same `view.markets`
      served locally or over the wire, so it works in both modes.
- [x] Graphical client: on-map **combat flashes** / ambush markers, so fights are
      visible where they happen (not only in the log). `SimEvent` now carries an
      optional `system`; the client pulses a fading, expanding ring at that node
      for each recent Combat/Piracy/Navy event (decaying in ticks, so it honours
      pause/speed), gated by the same per-category filters as the log.

## Graphical client polish

- [ ] Pan/zoom the galaxy map.
- [ ] Ship visuals as data (a `sprite`/`color` field on `ShipDef`, or a client-side
      asset manifest keyed by ship id).
- [ ] A graphical **player** client (input + HUD) over the command pipeline.

## 3-D flight/combat (Bevy) — see [docs/dev/flight-combat.md](docs/dev/flight-combat.md)

- [~] M1 — 3-D combat spectator (`drift-flight`). Renders the galaxy in 3-D and
      animates running battles from `EncounterView`; a pure read of the sim (no
      piloting, determinism firewall intact). The engine-agnostic `scene` model (sim
      -> 3-D geometry) is the tested core; the Bevy app is behind a `gui` feature so
      the default workspace build stays fast and Bevy-free.
      - [ ] Animate agents (traders/pirates/navy) along jump edges, not just battles.
      - [ ] Orbit/pan/zoom camera; HUD; ship visuals as data.
- [~] M2 — single-player in-system real-time flight, wired to the sim. Arcade
      flight (tested `flight` model) with a chase camera; the player is a real
      trader spawned through the command pipeline; the current system is populated
      with the agents the sim reports docked there; jumping (`1-9`) and cockpit
      trading/docking (fly to the station, `Space` to dock, market panel issuing
      `Buy`/`Sell`) all go through the pipeline. Determinism firewall intact
      (free-flight position never enters the sim).
      - [x] Polish: HDR camera + bloom, engine glow, per-system star/planet
            flavour, spinning bodies, a jump flash.
      - [ ] Cinematic jump transition; data-driven ship visuals.
- [~] M3 — single-player real-time combat. Pirates in the current system spawn as
      live hostiles that steer toward the player and fire; the player fires bolts
      (`F`); a tested `combat::Health` model (shield-then-hull) resolves hits;
      destroyed hostiles are removed and the player respawns on death. Client-side
      and authoritative for the player's fight.
      - [x] Report outcomes to the sim: `DestroyedPirate` (removes the pirate, pays
            the bounty, credits bounty contracts) and `TraderDestroyed` (destroys
            the trader, pays insurance) commands, applied at the tick boundary —
            the flight layer stays authoritative for the player's fight but the sim
            records the consequences. Headless tests cover both.
      - [x] Explosions: a growing emissive flash plus a deterministic burst of
            fragments when any ship dies (player or hostile).
      - [x] Weapon variety: three selectable player weapons (fast Pulse, heavy
            Cannon, three-bolt Scatter), cycled with `Tab`, each with its own
            damage/fire-rate/bolt visuals; shown on the HUD.
      - [ ] Escorts/navy fight alongside; ship-visual variety.
      - [ ] Further combat-feel tuning from playtesting.
- [ ] M4 — real-time multiplayer flight (prediction/rollback; much later, separate).

## Gameplay depth

- [x] Missions and contracts (cargo runs, bounty contracts, courier jobs) on top of
      the existing bounty/economy plumbing. One board with a `ContractKind`
      (Delivery / Courier / Bounty), `AcceptContract`/`FulfillContract` commands,
      per-kind generation + completion in a `contract_phase`, bounty progress
      credited from ambush wins, stable `ContractId` handles, `Snapshot`/`WorldView`
      wiring, a `drift-client` contracts panel, `drift play` CLI support, and full
      test coverage. Enabled in the `equilibrium` scenario.
- [x] Financial instruments (futures, loans, insurance) — the "sophisticated
      trading" differentiator. All three live in `drift-economy::finance`, settle in
      a `finance_phase`, ride on `Snapshot`/`WorldView`, and have a `drift-client`
      Finance panel plus `drift play` commands. Enabled in the `equilibrium`
      scenario.
      - [x] Loans: `TakeLoan`/`RepayLoan`; interest compounds each period, overdue
            loans are called (balance seized).
      - [x] Insurance: `BuyInsurance`; a premium buys a payout if pirates destroy
            the covered trader (rides on the piracy system).
      - [x] Futures: `OpenFuture` (long/short) at the spot price, cash-settled
            against the galaxy reference price at maturity.
- [x] Escort fees / navy funding as real economic costs. Escorts charge a per-jump
      fee to the trader they protect; the navy costs upkeep per ship drawn from a
      public treasury funded by per-tick income, so an underfunded navy runs a
      deficit and shrinks under attrition (reinforcement stalls). Tracked in
      `PiracyStats` (`escort_fees_paid`, `navy_upkeep`) and `World::treasury()`.
- [x] Multi-tick running battles: encounters now play out over several economy
      ticks. `drift-combat`'s `Encounter` is serializable and steppable
      (`advance`); the world holds `ActiveEncounter`s (each with its own RNG),
      opened by the piracy/navy phases and advanced a few steps per tick in a
      `combat_phase`. Participants are addressed by stable `PatrolId`; engaged
      patrols/traders are frozen (no roam/arrival/re-ambush) until the fight
      completes, when the existing casualty/bounty/insurance/contract hooks fire.

## Multiplayer

- [x] Server-authoritative networking transport (`drift-server`: TCP +
      length-prefixed JSON over the command pipeline; std threads, no async).
- [x] Networked **client** (`drift-client --connect`): an owned `WorldView` mirror
      that applies server broadcasts and renders (shared wire contract in
      `drift-proto`).
- [x] Graphical **player** client: a Pilot panel drives launch / buy / sell / jump
      / retire from the UI, through one command sink (local `Session` or server),
      round-trip tested in both modes.
- [ ] Content-version handshake: the client loads mods locally and assumes they
      match the server; send a content hash on connect to detect a mismatch.
- [ ] Snapshot delta encoding + interest management (needed only at scale).
- [ ] Client prediction / rollback (needed only for a real-time flight layer).
- [ ] Optional hardening: generational `TraderId` (the current monotonic,
      never-reused id is already ABA-safe; revisit only if it becomes a bottleneck).

## Modding / scripting

- [~] Mod-scripting runtime plugging into the `NamedRegistry` seam. Engine chosen:
      **Rhai** (pure Rust, sandboxed by construction, operation-limited) over
      Lua/WASM — see `drift-script` for the rationale; `wasmi` is the
      determinism-hardened swap-in behind the same seam if lockstep-across-clients
      is ever needed.
      - [x] `drift-script` crate: `ScriptedPricing` (a Rhai-authored market pricing
            strategy), sandboxed + fuel-limited, with tests proving determinism,
            runaway-termination, error containment, and no ambient access.
      - [x] Wire into the pricing seam: `PricingStrategy::Scripted(u32)` +
            `PricingSet` (name registry bundled with the compiled script table);
            `register_script` adds a named scripted strategy, the world keeps the
            table and dispatches it each repricing tick. A system can now name a
            scripted strategy exactly like a built-in; an end-to-end test drives a
            market's prices from a Rhai script.
      - [ ] Load `.rhai` scripts declared in a mod manifest (schema + loader), so a
            mod adds behavior from disk (currently registered programmatically).
      - [ ] More hooks (trader AI, event rules) on the same seam.

## Content and balance

- [ ] Larger galaxy: more commodities, ships, and systems.
- [ ] Deepen the economy further (the core differentiator).

## Observability and tooling

- [ ] Richer event set (large NPC trades, reinforcements) and optional event-to-file.
- [x] CI running `make test` and `make lint` (GitHub Actions, `.github/workflows/ci.yml`;
      installs the eframe/egui system libs so the whole workspace builds).
