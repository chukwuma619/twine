use crate::constant::{PAYMENT_WINDOW_SECS, SAFETY_SECS};
use crate::types::{Actor, Decision, InvoiceStatus, Phase};

/// The poster owns the order. Only they can cancel it before someone takes it.
pub fn order_actor(maker: &str, solver: Option<&str>, sender: &str) -> Actor {
    if sender == maker {
        Actor::Maker
    } else if solver == Some(sender) {
        Actor::Solver
    } else {
        Actor::Other
    }
}

/// Seller locks and releases. Buyer marks fiat sent and names the payout invoice.
pub fn trade_actor(seller: &str, buyer: &str, solver: Option<&str>, sender: &str) -> Actor {
    if sender == seller {
        Actor::Seller
    } else if sender == buyer {
        Actor::Buyer
    } else if solver == Some(sender) {
        Actor::Solver
    } else {
        Actor::Other
    }
}

/// Last moment a payout or a buyer-wins resolution may start.
/// After that the Fiber hold is too close to expiry, so the seller is refunded.
pub fn action_deadline(received_at: i64, hold_secs: u64) -> i64 {
    received_at + hold_secs as i64 - SAFETY_SECS as i64
}

pub fn apply_clock(phase: Phase, now: i64, received_at: Option<i64>, hold_secs: u64) -> Decision {
    let Some(received_at) = received_at else {
        return Decision::NoOp;
    };
    let payment_end = received_at + PAYMENT_WINDOW_SECS as i64;
    let action_end = action_deadline(received_at, hold_secs);
    match phase {
        Phase::WaitingFiat if now >= payment_end => Decision::Ok(Phase::Refunding),
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

pub fn apply_hold_received(phase: Phase) -> Decision {
    match phase {
        Phase::WaitingHold => Decision::Ok(Phase::WaitingFiat),
        _ => Decision::Reject("hold is not waiting"),
    }
}

pub fn apply_fiat_sent(phase: Phase, actor: Actor) -> Decision {
    match (phase, actor) {
        (Phase::WaitingFiat, Actor::Buyer) => Decision::Ok(Phase::FiatSent),
        (Phase::AwaitingInvoice, Actor::Buyer) => Decision::Ok(Phase::Releasing),
        (Phase::Disputed, Actor::Buyer) => Decision::Ok(Phase::Disputed),
        (Phase::WaitingFiat | Phase::AwaitingInvoice | Phase::Disputed, _) => {
            Decision::Reject("only the buyer can submit a payout invoice")
        }
        _ => Decision::Reject(
            "fiat-sent is only valid while waiting for fiat, a new invoice, or during a dispute",
        ),
    }
}

pub fn apply_release(phase: Phase, actor: Actor, has_invoice: bool) -> Decision {
    match (phase, actor) {
        (Phase::FiatSent | Phase::Disputed, Actor::Seller) if has_invoice => {
            Decision::Ok(Phase::Releasing)
        }
        (Phase::FiatSent | Phase::Disputed, Actor::Seller) => {
            Decision::Reject("payout invoice is required")
        }
        (Phase::FiatSent | Phase::Disputed, _) => Decision::Reject("only the seller can release"),
        _ => Decision::Reject("release is only valid after fiat is sent or during a dispute"),
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
            Actor::Seller | Actor::Buyer,
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
        Phase::Pending => match actor {
            Actor::Maker => Decision::Ok(Phase::Canceled),
            _ => Decision::Reject("only the poster can cancel an open order"),
        },
        Phase::WaitingHold => {
            if !matches!(actor, Actor::Seller | Actor::Buyer) {
                return Decision::Reject("only a party to the trade can cancel");
            }
            match invoice {
                Some(InvoiceStatus::Received) | Some(InvoiceStatus::Paid) => {
                    Decision::Reject("hold is locked; wait for the timelock")
                }
                Some(InvoiceStatus::Expired) => Decision::Ok(Phase::Expired),
                Some(InvoiceStatus::Open) | Some(InvoiceStatus::Cancelled) => {
                    Decision::Ok(Phase::Canceled)
                }
                None => Decision::Reject("unknown hold invoice status"),
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
    use crate::constant::PAYMENT_WINDOW_SECS;
    use crate::types::{Actor, Decision, InvoiceStatus, Phase};

    fn seller() -> Actor {
        Actor::Seller
    }

    fn buyer() -> Actor {
        Actor::Buyer
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
    fn fiat_sent_is_buyer_only() {
        assert_eq!(
            apply_fiat_sent(Phase::WaitingFiat, buyer()),
            Decision::Ok(Phase::FiatSent)
        );
        assert!(matches!(
            apply_fiat_sent(Phase::WaitingFiat, seller()),
            Decision::Reject(_)
        ));
        assert!(matches!(
            apply_fiat_sent(Phase::WaitingHold, buyer()),
            Decision::Reject(_)
        ));
    }

    #[test]
    fn release_then_failed_payout_waits_for_a_new_invoice() {
        assert_eq!(
            apply_release(Phase::FiatSent, seller(), true),
            Decision::Ok(Phase::Releasing)
        );
        assert_eq!(
            apply_release_failed(Phase::Releasing),
            Decision::Ok(Phase::AwaitingInvoice)
        );
        assert_eq!(
            apply_fiat_sent(Phase::AwaitingInvoice, buyer()),
            Decision::Ok(Phase::Releasing)
        );
        assert_eq!(
            apply_release_succeeded(Phase::Releasing),
            Decision::Ok(Phase::Settled)
        );
        assert!(matches!(
            apply_release(Phase::FiatSent, buyer(), true),
            Decision::Reject(_)
        ));
    }

    #[test]
    fn seller_can_release_during_a_dispute_when_the_buyer_named_an_invoice() {
        assert_eq!(
            apply_release(Phase::Disputed, seller(), true),
            Decision::Ok(Phase::Releasing)
        );
        assert_eq!(
            apply_release(Phase::Disputed, seller(), false),
            Decision::Reject("payout invoice is required")
        );
        assert!(matches!(
            apply_release(Phase::Disputed, buyer(), true),
            Decision::Reject(_)
        ));
        assert_eq!(
            apply_fiat_sent(Phase::Disputed, buyer()),
            Decision::Ok(Phase::Disputed)
        );
    }

    #[test]
    fn cancel_pending_is_the_poster() {
        assert_eq!(
            apply_cancel(Phase::Pending, Actor::Maker, None),
            Decision::Ok(Phase::Canceled)
        );
        assert!(matches!(
            apply_cancel(Phase::Pending, seller(), None),
            Decision::Reject(_)
        ));
        assert!(matches!(
            apply_cancel(Phase::Pending, buyer(), None),
            Decision::Reject(_)
        ));
        assert!(matches!(
            apply_cancel(Phase::Pending, Actor::Other, None),
            Decision::Reject(_)
        ));
    }

    #[test]
    fn cancel_waiting_hold_requires_an_open_invoice() {
        assert_eq!(
            apply_cancel(Phase::WaitingHold, buyer(), Some(InvoiceStatus::Open)),
            Decision::Ok(Phase::Canceled)
        );
        assert!(matches!(
            apply_cancel(Phase::WaitingHold, buyer(), None),
            Decision::Reject(_)
        ));
        assert_eq!(
            apply_cancel(Phase::WaitingHold, seller(), Some(InvoiceStatus::Received)),
            Decision::Reject("hold is locked; wait for the timelock")
        );
        assert_eq!(
            apply_cancel(Phase::WaitingFiat, seller(), Some(InvoiceStatus::Received)),
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
    fn order_actor_is_the_poster() {
        assert_eq!(
            order_actor("poster", Some("solver"), "poster"),
            Actor::Maker
        );
        assert_eq!(
            order_actor("poster", Some("solver"), "solver"),
            Actor::Solver
        );
        assert_eq!(order_actor("poster", Some("solver"), "other"), Actor::Other);
    }

    #[test]
    fn trade_actor_names_seller_and_buyer() {
        assert_eq!(
            trade_actor("seller", "buyer", Some("s"), "seller"),
            Actor::Seller
        );
        assert_eq!(
            trade_actor("seller", "buyer", Some("s"), "buyer"),
            Actor::Buyer
        );
        assert_eq!(
            trade_actor("seller", "buyer", Some("s"), "s"),
            Actor::Solver
        );
    }

    #[test]
    fn payment_window_and_hold_refund_without_settling() {
        let received = 1_000;
        let hold = 36 * 3_600;
        assert_eq!(
            apply_clock(
                Phase::WaitingFiat,
                received + PAYMENT_WINDOW_SECS as i64 - 1,
                Some(received),
                hold
            ),
            Decision::NoOp
        );
        assert_eq!(
            apply_clock(
                Phase::WaitingFiat,
                received + PAYMENT_WINDOW_SECS as i64,
                Some(received),
                hold
            ),
            Decision::Ok(Phase::Refunding)
        );
        let deadline = action_deadline(received, hold);
        assert_eq!(
            apply_clock(Phase::Disputed, deadline - 1, Some(received), hold),
            Decision::NoOp
        );
        assert_eq!(
            apply_clock(Phase::Disputed, deadline, Some(received), hold),
            Decision::Ok(Phase::Refunding)
        );
        assert_eq!(
            apply_clock(Phase::AwaitingInvoice, deadline, Some(received), hold),
            Decision::Ok(Phase::Refunding)
        );
        let short = 16 * 3_600;
        let short_deadline = action_deadline(received, short);
        assert!(short_deadline < deadline);
        assert_eq!(
            apply_clock(Phase::Disputed, short_deadline, Some(received), short),
            Decision::Ok(Phase::Refunding)
        );
    }

    #[test]
    fn dispute_and_resolve() {
        assert_eq!(
            apply_dispute(Phase::WaitingFiat, buyer()),
            Decision::Ok(Phase::Disputed)
        );
        assert_eq!(
            apply_dispute(Phase::FiatSent, seller()),
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
            apply_resolve(Phase::Disputed, seller(), Winner::Seller, true),
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
