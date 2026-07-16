//! Contracts — missions layered on the living galaxy.
//!
//! A [`Contract`] is a job posted at an `origin` station: reach its `destination`
//! by a `deadline` and satisfy its [`ContractKind`], for a `reward`. Three kinds
//! ride on systems the simulation already models:
//!
//! - [`Delivery`](ContractKind::Delivery) — carry `quantity` of a `commodity` to a
//!   system starved of it (generated from real market shortfalls).
//! - [`Courier`](ContractKind::Courier) — carry a parcel from origin to
//!   destination; pure risk-priced travel, no goods.
//! - [`Bounty`](ContractKind::Bounty) — destroy `target` pirates while holding the
//!   contract, then claim at the destination. Progress accrues from ambushes the
//!   holder's trader wins.
//!
//! The player interacts entirely through the command pipeline: an
//! [`AcceptContract`](crate::Command::AcceptContract) binds an open contract to one
//! of the player's traders, and a [`FulfillContract`](crate::Command::FulfillContract)
//! at the destination checks the kind's condition, pays the reward, and clears the
//! contract. Generation, bounty progress, and expiry happen in the world; nothing
//! here mutates the world.

use drift_core::{CommodityId, Money, Quantity, SystemId, Tick};
use serde::{Deserialize, Serialize};

use crate::command::PlayerId;
use crate::trader::TraderId;

/// A stable, never-reused handle for a contract.
///
/// Assigned once, monotonically; a stale id (its contract fulfilled or expired,
/// hence removed from the board) simply fails to resolve. This is the same
/// discipline as [`TraderId`], and it is what lets a command address a specific
/// contract safely across ticks and across the network.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct ContractId(pub u64);

/// What a contract asks for. The fulfilment condition (checked at the destination)
/// varies by kind; everything else (origin, destination, reward, deadline) is
/// shared on [`Contract`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContractKind {
    /// Deliver `quantity` of `commodity`, consumed from the hold on completion.
    Delivery {
        commodity: CommodityId,
        quantity: Quantity,
    },
    /// Carry a parcel to the destination — no goods, just the trip.
    Courier,
    /// Destroy `target` pirates while holding the contract; `progress` accrues as
    /// the holder's trader wins ambushes and is claimed at the destination.
    Bounty { target: u32, progress: u32 },
}

/// Where a contract is in its lifecycle. Fulfilled and expired contracts are
/// removed from the board (not retained), so only the two live states exist here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContractState {
    /// On the board, available for any player to accept.
    Open,
    /// Claimed by a player's trader, which must complete it before the deadline.
    Accepted { player: PlayerId, trader: TraderId },
}

/// A mission on the board: satisfy [`kind`](Contract::kind) at `destination` by
/// `deadline` for `reward`. Posted at `origin` (the issuing station).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Contract {
    /// Stable handle (see [`ContractId`]).
    pub id: ContractId,
    /// What the contract asks for.
    pub kind: ContractKind,
    /// Where the job must be completed / claimed.
    pub destination: SystemId,
    /// Where the contract is posted (the issuing station).
    pub origin: SystemId,
    /// Credits paid on completion.
    pub reward: Money,
    /// Absolute tick by which the job must be completed. The deadline tick itself
    /// is still fulfillable; the contract is swept from the board on the following
    /// tick, and an attempt after the deadline is rejected as expired.
    pub deadline: Tick,
    /// Lifecycle state.
    pub state: ContractState,
}

impl Contract {
    /// Whether the contract is open (available to accept).
    pub fn is_open(&self) -> bool {
        matches!(self.state, ContractState::Open)
    }

    /// The `(player, trader)` currently holding this contract, if accepted.
    pub fn holder(&self) -> Option<(PlayerId, TraderId)> {
        match self.state {
            ContractState::Accepted { player, trader } => Some((player, trader)),
            ContractState::Open => None,
        }
    }

    /// The trader currently holding this contract, if accepted.
    pub fn held_by(&self) -> Option<TraderId> {
        self.holder().map(|(_, t)| t)
    }

    /// For a delivery contract, the `(commodity, quantity)` it requires; `None`
    /// for the other kinds (which carry no goods).
    pub fn cargo(&self) -> Option<(CommodityId, Quantity)> {
        match self.kind {
            ContractKind::Delivery { commodity, quantity } => Some((commodity, quantity)),
            ContractKind::Courier | ContractKind::Bounty { .. } => None,
        }
    }

    /// Whether the kind's completion condition is met, given the holding trader's
    /// held quantity of the delivery good (0 for non-delivery kinds). Location and
    /// deadline are checked by the caller; this is only the kind-specific part.
    pub fn condition_met(&self, held_qty: Quantity) -> bool {
        match self.kind {
            ContractKind::Delivery { quantity, .. } => held_qty >= quantity,
            ContractKind::Courier => true,
            ContractKind::Bounty { target, progress } => progress >= target,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drift_core::{CommodityId, SystemId};

    fn contract(kind: ContractKind, state: ContractState) -> Contract {
        Contract {
            id: ContractId(1),
            kind,
            destination: SystemId(1),
            origin: SystemId(0),
            reward: 500,
            deadline: Tick(100),
            state,
        }
    }

    #[test]
    fn open_and_holder_reflect_state() {
        let delivery = ContractKind::Delivery { commodity: CommodityId(0), quantity: 10 };
        let open = contract(delivery, ContractState::Open);
        assert!(open.is_open());
        assert_eq!(open.holder(), None);
        assert_eq!(open.held_by(), None);

        let held = contract(
            delivery,
            ContractState::Accepted { player: PlayerId(3), trader: TraderId(7) },
        );
        assert!(!held.is_open());
        assert_eq!(held.holder(), Some((PlayerId(3), TraderId(7))));
        assert_eq!(held.held_by(), Some(TraderId(7)));
    }

    #[test]
    fn cargo_and_condition_are_kind_specific() {
        let delivery = contract(
            ContractKind::Delivery { commodity: CommodityId(2), quantity: 10 },
            ContractState::Open,
        );
        assert_eq!(delivery.cargo(), Some((CommodityId(2), 10)));
        assert!(!delivery.condition_met(9));
        assert!(delivery.condition_met(10));

        let courier = contract(ContractKind::Courier, ContractState::Open);
        assert_eq!(courier.cargo(), None);
        assert!(courier.condition_met(0), "a courier only needs to arrive");

        let bounty = contract(ContractKind::Bounty { target: 3, progress: 2 }, ContractState::Open);
        assert_eq!(bounty.cargo(), None);
        assert!(!bounty.condition_met(0), "2 of 3 kills is not enough");
        let done = contract(ContractKind::Bounty { target: 3, progress: 3 }, ContractState::Open);
        assert!(done.condition_met(0));
    }
}
