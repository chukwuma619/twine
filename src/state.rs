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

pub fn apply_clock(phase: Phase, now: i64, deadline: Option<i64>) -> Decision {
    let Some(deadline) = deadline else {
        return Decision::NoOp;
    };
    match phase {
        Phase::AwaitingRoute
        | Phase::WaitingFiat
        | Phase::FiatSent
        | Phase::AwaitingInvoice
        | Phase::Disputed
            if now >= deadline =>
        {
            Decision::Ok(Phase::Refunding)
        }
        _ => Decision::NoOp,
    }
}

pub fn apply_take(phase: Phase) -> Decision {
    match phase {
        Phase::Pending => Decision::Ok(Phase::WaitingHold),
        _ => Decision::Reject("order is not open for taking"),
    }
}

/// The locked hold can ask for fiat once the payout invoice has a route.
pub fn apply_route_ready(phase: Phase) -> Decision {
    match phase {
        Phase::WaitingHold | Phase::AwaitingRoute => Decision::Ok(Phase::WaitingFiat),
        _ => Decision::Reject("hold is not waiting on a route"),
    }
}

/// Coins are locked and the payout invoice cannot be paid yet.
/// Waiting-fiat can return here when the fiat-sent dry run finds no route.
pub fn apply_route_blocked(phase: Phase) -> Decision {
    match phase {
        Phase::WaitingHold | Phase::AwaitingRoute | Phase::WaitingFiat => {
            Decision::Ok(Phase::AwaitingRoute)
        }
        _ => Decision::Reject("hold is not waiting on a route"),
    }
}

pub fn apply_fiat_sent(phase: Phase, actor: Actor) -> Decision {
    match (phase, actor) {
        (Phase::AwaitingRoute, Actor::Buyer) => Decision::Reject(
            "the wallet cannot receive yet. Send the same invoice again before paying",
        ),
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
            Phase::AwaitingRoute | Phase::WaitingFiat | Phase::FiatSent | Phase::AwaitingInvoice,
            Actor::Seller | Actor::Buyer,
        ) => Decision::Ok(Phase::Disputed),
        (
            Phase::AwaitingRoute | Phase::WaitingFiat | Phase::FiatSent | Phase::AwaitingInvoice,
            _,
        ) => Decision::Reject("only a party to the trade can dispute"),
        _ => Decision::Reject("dispute is only valid while the hold is locked"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Winner {
    Buyer,
    Seller,
    /// Fiat arrived and the CKB cannot be delivered. The protocol action is still a seller refund.
    FiatOwed,
}

pub fn parse_winner(value: &str) -> Option<Winner> {
    match value.trim() {
        "buyer" => Some(Winner::Buyer),
        "seller" => Some(Winner::Seller),
        "fiat-owed" => Some(Winner::FiatOwed),
        _ => None,
    }
}

pub fn apply_resolve(
    phase: Phase,
    actor: Actor,
    winner: Winner,
    has_invoice: bool,
    chat_shared: bool,
) -> Decision {
    if phase != Phase::Disputed {
        return Decision::Reject("resolve is only valid during a dispute");
    }
    if actor != Actor::Solver {
        return Decision::Reject("only the solver can resolve a dispute");
    }
    match winner {
        Winner::Seller => Decision::Ok(Phase::Refunding),
        Winner::FiatOwed if chat_shared => Decision::Ok(Phase::Refunding),
        Winner::FiatOwed => Decision::Reject(
            "share the chat key with the solver. The receipt cannot be read without it",
        ),
        Winner::Buyer if !chat_shared => Decision::Reject(
            "share the chat key with the solver. The receipt cannot be read without it, so the solver cannot pay the buyer",
        ),
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
        Phase::WaitingInvoice => {
            if matches!(actor, Actor::Seller | Actor::Buyer) {
                Decision::Ok(Phase::Canceled)
            } else {
                Decision::Reject("only a party to the trade can cancel")
            }
        }
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
        Phase::AwaitingRoute
        | Phase::WaitingFiat
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

    fn seller() -> Actor {
        Actor::Seller
    }

    fn buyer() -> Actor {
        Actor::Buyer
    }

    #[test]
    fn take_only_from_pending() {
        assert_eq!(apply_take(Phase::Pending), Decision::Ok(Phase::WaitingHold));
        assert!(matches!(
            apply_take(Phase::WaitingHold),
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
    fn hold_deadline_refunds_without_settling() {
        let expires_at_ms = 50_000_000_000;
        let deadline = crate::payout_deadline_secs(expires_at_ms);
        assert_eq!(
            apply_clock(Phase::WaitingFiat, deadline - 1, Some(deadline)),
            Decision::NoOp
        );
        assert_eq!(
            apply_clock(Phase::WaitingFiat, deadline, Some(deadline)),
            Decision::Ok(Phase::Refunding)
        );
        assert_eq!(
            apply_clock(Phase::Disputed, deadline, Some(deadline)),
            Decision::Ok(Phase::Refunding)
        );
        assert_eq!(
            apply_clock(Phase::AwaitingInvoice, deadline, Some(deadline)),
            Decision::Ok(Phase::Refunding)
        );
        assert_eq!(
            apply_clock(Phase::WaitingFiat, deadline, None),
            Decision::NoOp
        );
        assert!(deadline < expires_at_ms / 1000);
    }

    #[test]
    fn outgoing_budget_ends_before_fiber_drops_the_incoming_tlc() {
        let now_ms = 1_000_000_000_000;
        let expires_at_ms = now_ms + 36 * 3_600 * 1000;
        let crate::PayoutBudget::Send {
            tlc_expiry_limit_ms,
        } = crate::payout_budget(expires_at_ms, now_ms, 24 * 3_600 * 1000)
        else {
            panic!("a 36 hour hold fits a 24 hour final hop");
        };
        assert!(
            now_ms + tlc_expiry_limit_ms as i64 + crate::CLAIM_MARGIN_MS as i64 <= expires_at_ms
        );
        let late = expires_at_ms - crate::CLAIM_MARGIN_MS as i64 - 60_000;
        assert_eq!(
            crate::payout_budget(expires_at_ms, late, crate::MIN_TLC_EXPIRY_LIMIT_MS),
            crate::PayoutBudget::Refund
        );
        let mid = now_ms + 20 * 3_600 * 1000;
        assert_eq!(
            crate::payout_budget(expires_at_ms, mid, 24 * 3_600 * 1000),
            crate::PayoutBudget::ShorterInvoice
        );
        assert_eq!(
            crate::payout_budget(
                expires_at_ms,
                expires_at_ms + 1,
                crate::MIN_TLC_EXPIRY_LIMIT_MS
            ),
            crate::PayoutBudget::Refund
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
            apply_resolve(Phase::Disputed, Actor::Solver, Winner::Seller, true, false),
            Decision::Ok(Phase::Refunding)
        );
        assert_eq!(
            apply_resolve(
                Phase::Disputed,
                Actor::Solver,
                Winner::FiatOwed,
                false,
                true
            ),
            Decision::Ok(Phase::Refunding)
        );
        assert!(matches!(
            apply_resolve(
                Phase::Disputed,
                Actor::Solver,
                Winner::FiatOwed,
                true,
                false
            ),
            Decision::Reject(_)
        ));
        assert!(matches!(
            apply_resolve(Phase::Disputed, Actor::Solver, Winner::Buyer, true, false),
            Decision::Reject(_)
        ));
        assert_eq!(
            apply_dispute(Phase::AwaitingRoute, buyer()),
            Decision::Ok(Phase::Disputed)
        );
        assert!(matches!(
            apply_fiat_sent(Phase::AwaitingRoute, buyer()),
            Decision::Reject(_)
        ));
        assert_eq!(
            apply_route_blocked(Phase::WaitingFiat),
            Decision::Ok(Phase::AwaitingRoute)
        );
        assert_eq!(
            apply_route_blocked(Phase::WaitingHold),
            Decision::Ok(Phase::AwaitingRoute)
        );
        assert_eq!(
            apply_resolve(Phase::Disputed, Actor::Solver, Winner::Buyer, true, true),
            Decision::Ok(Phase::Releasing)
        );
        assert_eq!(
            apply_resolve(Phase::Disputed, Actor::Solver, Winner::Buyer, false, true),
            Decision::Reject("payout invoice is required")
        );
        assert!(matches!(
            apply_resolve(Phase::Disputed, seller(), Winner::Seller, true, true),
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
