//! Financial instruments layered on the spot economy.
//!
//! The first instrument is the [`Loan`]: a trader borrows capital now against
//! future trading income, the balance accrues interest each accrual period, and it
//! is repaid (in part or full) through the command pipeline. Left unpaid past its
//! term, the lender calls the loan and seizes the outstanding balance from the
//! borrower's capital — leverage cuts both ways.
//!
//! Loans are the foundation of the "sophisticated trading" layer: they let a
//! player lever up a promising route, at the risk of a debt spiral if it turns.
//! Insurance and futures are intended to join this module later behind the same
//! account/settlement shape.

use drift_core::{CommodityId, Money, Quantity, Tick};
use serde::{Deserialize, Serialize};

use crate::command::PlayerId;
use crate::trader::TraderId;

/// A stable, never-reused handle for a loan (same discipline as `TraderId` /
/// `ContractId`): assigned once, monotonically; a stale id fails to resolve.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct LoanId(pub u64);

/// An outstanding loan against a trader's capital.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Loan {
    /// Stable handle (see [`LoanId`]).
    pub id: LoanId,
    /// The player who owns the borrowing trader (for authorization).
    pub player: PlayerId,
    /// The trader whose capital the loan was credited to and is repaid from.
    pub borrower: TraderId,
    /// Amount originally borrowed.
    pub principal: Money,
    /// Current balance owed; grows with interest, shrinks with repayment.
    pub outstanding: Money,
    /// Absolute tick the balance is due in full. The due tick itself is still
    /// repayable; past it the lender calls the loan and seizes the balance.
    pub due: Tick,
}

impl Loan {
    /// Whether this loan is held by `(player, trader)`.
    pub fn held_by(&self, player: PlayerId, trader: TraderId) -> bool {
        self.player == player && self.borrower == trader
    }
}

/// A ship-loss insurance policy: while in force, a trader destroyed by pirates is
/// compensated `payout` (once). Policies are keyed by the insured trader (one at a
/// time) rather than an id, since nothing addresses a specific policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    /// The player who owns the insured trader.
    pub player: PlayerId,
    /// The insured trader.
    pub insured: TraderId,
    /// Compensation paid on destruction while covered.
    pub payout: Money,
    /// Absolute tick coverage lapses (no payout after it).
    pub expiry: Tick,
}

/// Which way a futures position is taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FutureSide {
    /// Agreed to buy at the strike: profits when the settlement price is higher.
    Long,
    /// Agreed to sell at the strike: profits when the settlement price is lower.
    Short,
}

/// An open, cash-settled commodity-futures position. At `maturity` the difference
/// between the galaxy reference price and the `strike` (locked at open) is credited
/// to or debited from the holder's capital, scaled by `quantity` and `side`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Future {
    /// The player who owns the holding trader.
    pub player: PlayerId,
    /// The trader carrying the position.
    pub holder: TraderId,
    /// The commodity the position is on.
    pub commodity: CommodityId,
    /// Units the position covers.
    pub quantity: Quantity,
    /// Long or short.
    pub side: FutureSide,
    /// Reference price locked at open.
    pub strike: Money,
    /// Absolute tick the position settles.
    pub maturity: Tick,
}

impl Future {
    /// Cash payoff at a `settle` reference price: positive is a gain to the holder.
    pub fn payoff(&self, settle: Money) -> Money {
        let move_up = (settle - self.strike) * self.quantity as Money;
        match self.side {
            FutureSide::Long => move_up,
            FutureSide::Short => -move_up,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn held_by_matches_both_player_and_trader() {
        let loan = Loan {
            id: LoanId(1),
            player: PlayerId(2),
            borrower: TraderId(3),
            principal: 1000,
            outstanding: 1000,
            due: Tick(500),
        };
        assert!(loan.held_by(PlayerId(2), TraderId(3)));
        assert!(!loan.held_by(PlayerId(9), TraderId(3)), "wrong player");
        assert!(!loan.held_by(PlayerId(2), TraderId(9)), "wrong trader");
    }

    #[test]
    fn future_payoff_is_signed_by_side_and_move() {
        let base = Future {
            player: PlayerId(0),
            holder: TraderId(0),
            commodity: CommodityId(0),
            quantity: 10,
            side: FutureSide::Long,
            strike: 100,
            maturity: Tick(200),
        };
        // Long gains when the price rises, loses when it falls.
        assert_eq!(base.payoff(120), 200, "long: +20 over 10 units");
        assert_eq!(base.payoff(80), -200, "long: -20 over 10 units");
        // Short is the mirror image.
        let short = Future { side: FutureSide::Short, ..base };
        assert_eq!(short.payoff(120), -200);
        assert_eq!(short.payoff(80), 200);
    }
}
