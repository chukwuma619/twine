use crate::constant::{DISPUTE_WINDOW_SECS, FIAT_WINDOW_SECS, FINAL_EXPIRY_DELTA_MS, SAFETY_SECS};
use crate::types::{Actor, Decision, InvoiceStatus, Phase};

pub fn actor_of(maker: &str, taker: Option<&str>, solver: Option<&str>, sender: &str) -> Actor {
    if sender == maker {
        Actor::Maker
    } else if taker == Some(sender) {
        Actor::Taker
    } else if solver == Some(sender) {
        Actor::Solver
    } else {
        Actor::Other
    }
}

/// Last moment a payout or a buyer-wins resolution may start.
pub fn action_deadline(received_at: i64) -> i64 {
    let fiber = received_at + (FINAL_EXPIRY_DELTA_MS / 1000) as i64 - SAFETY_SECS as i64;
    let dispute = received_at + FIAT_WINDOW_SECS as i64 + DISPUTE_WINDOW_SECS as i64;
    fiber.min(dispute)
}

pub fn apply_clock(phase: Phase, now: i64, received_at: Option<i64>) -> Decision {
    let Some(received_at) = received_at else {
        return Decision::NoOp;
    };
    let fiat_end = received_at + FIAT_WINDOW_SECS as i64;
    let action_end = action_deadline(received_at);
    match phase {
        Phase::WaitingFiat if now >= fiat_end => Decision::Ok(Phase::Refunding),
        Phase::FiatSent | Phase::AwaitingInvoice | Phase::Disputed if now >= action_end => {
            Decision::Ok(Phase::Refunding)
        }
        _ => Decision::NoOp,
    }
}

pub fn apply_take(phase: Phase, has_open_trade: bool) -> Decision {
    if has_open_trade {
        return Decision::Reject("order already has an open trade");
    }
    match phase {
        Phase::Pending => Decision::Ok(Phase::WaitingHold),
        _ => Decision::Reject("order is not open for taking"),
    }
}

pub fn apply_locked(phase: Phase, actor: Actor) -> Decision {
    match (phase, actor) {
        (Phase::WaitingHold, Actor::Maker) => Decision::NoOp,
        (Phase::WaitingHold, _) => Decision::Reject("only the seller can lock"),
        _ => Decision::Reject("lock is only valid while waiting for the hold"),
    }
}

pub fn apply_hold_received(phase: Phase) -> Decision {
    match phase {
        Phase::WaitingHold => Decision::Ok(Phase::WaitingFiat),
        _ => Decision::Reject("hold is not waiting"),
    }
}

pub fn apply_fiat_sent(phase: Phase, actor: Actor) -> Decision {
    match (phase, actor) {
        (Phase::WaitingFiat, Actor::Taker) => Decision::Ok(Phase::FiatSent),
        (Phase::AwaitingInvoice, Actor::Taker) => Decision::Ok(Phase::Releasing),
        (Phase::WaitingFiat | Phase::AwaitingInvoice, _) => {
            Decision::Reject("only the buyer can submit a payout invoice")
        }
        _ => Decision::Reject("fiat-sent is only valid while waiting for fiat or a new invoice"),
    }
}

pub fn apply_release(phase: Phase, actor: Actor) -> Decision {
    match (phase, actor) {
        (Phase::FiatSent, Actor::Maker) => Decision::Ok(Phase::Releasing),
        (Phase::FiatSent, _) => Decision::Reject("only the seller can release"),
        _ => Decision::Reject("release is only valid after fiat-sent"),
    }
}

pub fn apply_release_succeeded(phase: Phase) -> Decision {
    match phase {
        Phase::Releasing => Decision::Ok(Phase::Settled),
        _ => Decision::Reject("no payout in progress"),
    }
}

pub fn apply_release_failed(phase: Phase) -> Decision {
    match phase {
        Phase::Releasing => Decision::Ok(Phase::AwaitingInvoice),
        _ => Decision::Reject("no payout in progress"),
    }
}

pub fn apply_dispute(phase: Phase, actor: Actor) -> Decision {
    match (phase, actor) {
        (
            Phase::WaitingFiat | Phase::FiatSent | Phase::AwaitingInvoice,
            Actor::Maker | Actor::Taker,
        ) => Decision::Ok(Phase::Disputed),
        (Phase::WaitingFiat | Phase::FiatSent | Phase::AwaitingInvoice, _) => {
            Decision::Reject("only a party to the trade can dispute")
        }
        _ => Decision::Reject("dispute is only valid while the hold is locked"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Winner {
    Buyer,
    Seller,
}

pub fn parse_winner(value: &str) -> Option<Winner> {
    match value.trim() {
        "buyer" => Some(Winner::Buyer),
        "seller" => Some(Winner::Seller),
        _ => None,
    }
}

pub fn apply_resolve(phase: Phase, actor: Actor, winner: Winner, has_invoice: bool) -> Decision {
    if phase != Phase::Disputed {
        return Decision::Reject("resolve is only valid during a dispute");
    }
    if actor != Actor::Solver {
        return Decision::Reject("only the solver can resolve a dispute");
    }
    match winner {
        Winner::Seller => Decision::Ok(Phase::Refunding),
        Winner::Buyer if has_invoice => Decision::Ok(Phase::Releasing),
        Winner::Buyer => Decision::Reject("payout invoice is required"),
    }
}

pub fn apply_cancel(phase: Phase, actor: Actor, invoice: Option<InvoiceStatus>) -> Decision {
    match phase {
        Phase::Pending => {
            if actor == Actor::Maker {
                Decision::Ok(Phase::Canceled)
            } else {
                Decision::Reject("only the maker can cancel a pending order")
            }
        }
        Phase::WaitingHold => {
            if !matches!(actor, Actor::Maker | Actor::Taker) {
                return Decision::Reject("only a party to the trade can cancel");
            }
            match invoice {
                Some(InvoiceStatus::Received) | Some(InvoiceStatus::Paid) => {
                    Decision::Reject("hold is locked; wait for the timelock")
                }
                Some(InvoiceStatus::Expired) => Decision::Ok(Phase::Expired),
                Some(InvoiceStatus::Open) | Some(InvoiceStatus::Cancelled) | None => {
                    Decision::Ok(Phase::Canceled)
                }
            }
        }
        Phase::WaitingFiat
        | Phase::FiatSent
        | Phase::Releasing
        | Phase::AwaitingInvoice
        | Phase::Disputed
        | Phase::Refunding => Decision::Reject("hold is locked; wait for the timelock"),
        _ => Decision::Reject("nothing to cancel"),
    }
}

pub fn apply_expired(phase: Phase) -> Decision {
    if phase.watched() {
        Decision::Ok(Phase::Expired)
    } else {
        Decision::Reject("expiry only applies while the hold can still refund")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Actor, Decision, InvoiceStatus, Phase};

    fn maker() -> Actor {
        Actor::Maker
    }

    fn taker() -> Actor {
        Actor::Taker
    }

    #[test]
    fn take_only_from_pending_without_open_trade() {
        assert_eq!(
            apply_take(Phase::Pending, false),
            Decision::Ok(Phase::WaitingHold)
        );
        assert_eq!(
            apply_take(Phase::Pending, true),
            Decision::Reject("order already has an open trade")
        );
        assert!(matches!(
            apply_take(Phase::WaitingHold, false),
            Decision::Reject(_)
        ));
    }

    #[test]
    fn locked_is_seller_only_and_duplicate_is_noop() {
        assert_eq!(apply_locked(Phase::WaitingHold, maker()), Decision::NoOp);
        assert_eq!(apply_locked(Phase::WaitingHold, maker()), Decision::NoOp);
        assert!(matches!(
            apply_locked(Phase::WaitingHold, taker()),
            Decision::Reject(_)
        ));
        assert!(matches!(
            apply_locked(Phase::WaitingFiat, maker()),
            Decision::Reject(_)
        ));
    }

    #[test]
    fn fiat_sent_is_taker_only_in_waiting_fiat() {
        assert_eq!(
            apply_fiat_sent(Phase::WaitingFiat, taker()),
            Decision::Ok(Phase::FiatSent)
        );
        assert!(matches!(
            apply_fiat_sent(Phase::WaitingFiat, maker()),
            Decision::Reject(_)
        ));
        assert!(matches!(
            apply_fiat_sent(Phase::WaitingHold, taker()),
            Decision::Reject(_)
        ));
    }

    #[test]
    fn release_then_failed_payout_waits_for_a_new_invoice() {
        assert_eq!(
            apply_release(Phase::FiatSent, maker()),
            Decision::Ok(Phase::Releasing)
        );
        assert_eq!(
            apply_release_failed(Phase::Releasing),
            Decision::Ok(Phase::AwaitingInvoice)
        );
        assert_eq!(
            apply_fiat_sent(Phase::AwaitingInvoice, taker()),
            Decision::Ok(Phase::Releasing)
        );
        assert_eq!(
            apply_release_succeeded(Phase::Releasing),
            Decision::Ok(Phase::Settled)
        );
        assert!(matches!(
            apply_release(Phase::FiatSent, taker()),
            Decision::Reject(_)
        ));
    }

    #[test]
    fn cancel_pending_is_maker_only() {
        assert_eq!(
            apply_cancel(Phase::Pending, maker(), None),
            Decision::Ok(Phase::Canceled)
        );
        assert!(matches!(
            apply_cancel(Phase::Pending, taker(), None),
            Decision::Reject(_)
        ));
    }

    #[test]
    fn cancel_waiting_hold_requires_open_or_unknown_invoice() {
        assert_eq!(
            apply_cancel(Phase::WaitingHold, taker(), Some(InvoiceStatus::Open)),
            Decision::Ok(Phase::Canceled)
        );
        assert_eq!(
            apply_cancel(Phase::WaitingHold, maker(), Some(InvoiceStatus::Received)),
            Decision::Reject("hold is locked; wait for the timelock")
        );
        assert_eq!(
            apply_cancel(Phase::WaitingFiat, maker(), Some(InvoiceStatus::Received)),
            Decision::Reject("hold is locked; wait for the timelock")
        );
    }

    #[test]
    fn expired_hold_returns_slice_phase() {
        assert_eq!(
            apply_expired(Phase::WaitingHold),
            Decision::Ok(Phase::Expired)
        );
        assert!(Phase::Expired.returns_slice());
        assert!(!Phase::Settled.returns_slice());
    }

    #[test]
    fn actor_of_matches_maker_taker_and_solver() {
        assert_eq!(actor_of("m", Some("t"), Some("s"), "m"), Actor::Maker);
        assert_eq!(actor_of("m", Some("t"), Some("s"), "t"), Actor::Taker);
        assert_eq!(actor_of("m", Some("t"), Some("s"), "s"), Actor::Solver);
        assert_eq!(actor_of("m", Some("t"), Some("s"), "x"), Actor::Other);
    }

    #[test]
    fn fiat_window_and_dispute_window_refund_without_settling() {
        let received = 1_000;
        assert_eq!(
            apply_clock(Phase::WaitingFiat, received + 7_199, Some(received)),
            Decision::NoOp
        );
        assert_eq!(
            apply_clock(Phase::WaitingFiat, received + 7_200, Some(received)),
            Decision::Ok(Phase::Refunding)
        );
        let deadline = action_deadline(received);
        assert_eq!(
            apply_clock(Phase::Disputed, deadline - 1, Some(received)),
            Decision::NoOp
        );
        assert_eq!(
            apply_clock(Phase::Disputed, deadline, Some(received)),
            Decision::Ok(Phase::Refunding)
        );
        assert_eq!(
            apply_clock(Phase::AwaitingInvoice, deadline, Some(received)),
            Decision::Ok(Phase::Refunding)
        );
    }

    #[test]
    fn dispute_and_resolve() {
        assert_eq!(
            apply_dispute(Phase::WaitingFiat, taker()),
            Decision::Ok(Phase::Disputed)
        );
        assert_eq!(
            apply_dispute(Phase::FiatSent, maker()),
            Decision::Ok(Phase::Disputed)
        );
        assert!(matches!(
            apply_dispute(Phase::WaitingFiat, Actor::Solver),
            Decision::Reject(_)
        ));
        assert_eq!(
            apply_resolve(Phase::Disputed, Actor::Solver, Winner::Seller, true),
            Decision::Ok(Phase::Refunding)
        );
        assert_eq!(
            apply_resolve(Phase::Disputed, Actor::Solver, Winner::Buyer, true),
            Decision::Ok(Phase::Releasing)
        );
        assert_eq!(
            apply_resolve(Phase::Disputed, Actor::Solver, Winner::Buyer, false),
            Decision::Reject("payout invoice is required")
        );
        assert!(matches!(
            apply_resolve(Phase::Disputed, maker(), Winner::Seller, true),
            Decision::Reject(_)
        ));
    }

    #[test]
    fn watched_hold_can_expire() {
        assert_eq!(
            apply_expired(Phase::Refunding),
            Decision::Ok(Phase::Expired)
        );
        assert_eq!(apply_expired(Phase::Disputed), Decision::Ok(Phase::Expired));
        assert!(matches!(apply_expired(Phase::Settled), Decision::Reject(_)));
    }
}
