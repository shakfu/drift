//! The command pipeline — the multiplayer-ready entry point for player actions.
//!
//! Every action a player takes is a serializable [`Command`] queued via
//! [`World::queue_command`](crate::World::queue_command) and applied, validated, at
//! a tick boundary (the `command_phase`, which runs first each tick). Modelling
//! actions this way is the load-bearing provision for multiplayer: single-player
//! enqueues locally, a server would enqueue from the network, and in both cases the
//! world applies commands deterministically at the tick. Commands never mutate the
//! world directly, so they can be ordered, replayed, and rejected.
//!
//! See `docs/dev/multiplayer.md` for the surrounding design.

use drift_core::{CommodityId, Money, Quantity, ShipId, SystemId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::contract::ContractId;
use crate::finance::{FutureSide, LoanId};
use crate::patrol::PatrolId;
use crate::trader::TraderId;

/// Identifies a player. Player 0 is a perfectly good single-player convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PlayerId(pub u32);

/// Who controls an agent. NPC agents run the built-in AI; player-owned agents act
/// only on commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Owner {
    #[default]
    Npc,
    Player(PlayerId),
}

/// A player-issued action. Operands are resolved handles (the client/server maps
/// names to ids before issuing), and the whole type is serde-serializable, i.e.
/// already wire-ready. Traders are addressed by a stable [`TraderId`]: a client
/// spawns a ship, reads its server-assigned id from the next world state, and uses
/// that id thereafter — robust across other traders being added or removed.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Command {
    /// Bring a new player-owned trader into the galaxy. Its id is assigned by the
    /// world and observed in the resulting state.
    Spawn {
        player: PlayerId,
        ship: ShipId,
        at: SystemId,
        capital: Money,
    },
    /// Retire a player trader, removing it from the galaxy.
    Despawn { player: PlayerId, trader: TraderId },
    /// Order a docked player trader to jump to a connected system.
    Jump {
        player: PlayerId,
        trader: TraderId,
        dest: SystemId,
    },
    /// Buy `qty` of a commodity at the trader's current market.
    Buy {
        player: PlayerId,
        trader: TraderId,
        commodity: CommodityId,
        qty: Quantity,
    },
    /// Sell `qty` of a commodity from the hold at the trader's current market.
    Sell {
        player: PlayerId,
        trader: TraderId,
        commodity: CommodityId,
        qty: Quantity,
    },
    /// Claim an open delivery contract for one of the player's traders.
    AcceptContract {
        player: PlayerId,
        trader: TraderId,
        contract: ContractId,
    },
    /// Deliver an accepted contract's cargo at its destination, collecting the
    /// reward. Requires the holding trader to be docked at the destination with
    /// the full quantity in its hold, on or before the deadline.
    FulfillContract {
        player: PlayerId,
        trader: TraderId,
        contract: ContractId,
    },
    /// Borrow `principal` credits against a docked trader, creating a loan that
    /// accrues interest and is due after the run's loan term.
    TakeLoan {
        player: PlayerId,
        trader: TraderId,
        principal: Money,
    },
    /// Repay up to `amount` of a loan from the borrowing trader's capital. Paying
    /// the balance down to zero closes the loan.
    RepayLoan {
        player: PlayerId,
        trader: TraderId,
        loan: LoanId,
        amount: Money,
    },
    /// Insure a docked trader against destruction, paying the premium up front.
    BuyInsurance { player: PlayerId, trader: TraderId },
    /// Open a cash-settled futures position on a commodity at the current spot
    /// price, for a fee. It settles automatically at maturity.
    OpenFuture {
        player: PlayerId,
        trader: TraderId,
        commodity: CommodityId,
        qty: Quantity,
        side: FutureSide,
    },
    /// Report that the player's trader destroyed a pirate in a real-time (flight
    /// layer) fight. The sim removes the pirate, pays the bounty, and credits any
    /// bounty contract — the authoritative bookkeeping for a fight the flight layer
    /// resolved. A stale pirate id is simply rejected.
    DestroyedPirate {
        player: PlayerId,
        trader: TraderId,
        pirate: PatrolId,
    },
    /// Report that the player's trader was destroyed in a real-time fight. The sim
    /// destroys it (cargo lost, respawn scheduled) and pays out any insurance.
    TraderDestroyed { player: PlayerId, trader: TraderId },
}

impl Command {
    /// The player that issued the command (for authorization).
    pub fn player(&self) -> PlayerId {
        match *self {
            Command::Spawn { player, .. }
            | Command::Despawn { player, .. }
            | Command::Jump { player, .. }
            | Command::Buy { player, .. }
            | Command::Sell { player, .. }
            | Command::AcceptContract { player, .. }
            | Command::FulfillContract { player, .. }
            | Command::TakeLoan { player, .. }
            | Command::RepayLoan { player, .. }
            | Command::BuyInsurance { player, .. }
            | Command::OpenFuture { player, .. }
            | Command::DestroyedPirate { player, .. }
            | Command::TraderDestroyed { player, .. } => player,
        }
    }
}

/// Why a command was rejected. Rejection is normal (input is untrusted), never
/// fatal: the command is dropped and counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum CommandError {
    #[error("unknown ship id")]
    UnknownShip,
    #[error("unknown or invalid system id")]
    InvalidSystem,
    #[error("no such trader")]
    UnknownTrader,
    #[error("trader is not owned by this player")]
    NotOwner,
    #[error("trader is not docked")]
    NotDocked,
    #[error("destination is not reachable in one jump")]
    Unreachable,
    #[error("this market does not trade that commodity")]
    UnknownGood,
    #[error("not enough capital")]
    InsufficientFunds,
    #[error("not enough stock on the market")]
    InsufficientStock,
    #[error("not enough of that good in the hold")]
    InsufficientCargo,
    #[error("would exceed cargo capacity")]
    OverCapacity,
    #[error("quantity must be positive")]
    ZeroQuantity,
    #[error("no such contract")]
    UnknownContract,
    #[error("contract is not open for acceptance")]
    ContractUnavailable,
    #[error("contract is not held by this trader")]
    ContractNotHeld,
    #[error("contract deadline has passed")]
    ContractExpired,
    #[error("trader is not at the contract's destination")]
    WrongDestination,
    #[error("bounty contract's pirate quota is not yet met")]
    BountyIncomplete,
    #[error("lending is not available in this scenario")]
    LendingUnavailable,
    #[error("loan principal exceeds the maximum")]
    LoanTooLarge,
    #[error("trader already carries the maximum number of loans")]
    TooManyLoans,
    #[error("no such loan")]
    UnknownLoan,
    #[error("loan is not held by this trader")]
    LoanNotHeld,
    #[error("insurance is not available in this scenario")]
    InsuranceUnavailable,
    #[error("trader is already insured")]
    AlreadyInsured,
    #[error("a futures market is not available in this scenario")]
    FuturesUnavailable,
    #[error("futures position exceeds the maximum quantity")]
    FutureTooLarge,
    #[error("trader already carries the maximum number of futures")]
    TooManyFutures,
    #[error("no such pirate")]
    UnknownPatrol,
}
