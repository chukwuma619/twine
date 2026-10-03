use crate::types::{Actor, Decision, InvoiceStatus, Phase};

pub fn actor_of(maker: &str, taker: Option<&str>, sender: &str) -> Actor {
    if sender == maker {
        Actor::Maker
    } else if taker == Some(sender) {
        Actor::Taker
    } else {
        Actor::Other
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
        (Phase::WaitingFiat, _) => Decision::Reject("only the taker can submit a payout invoice"),
        _ => Decision::Reject("fiat-sent is only valid while waiting for fiat"),
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
        Phase::Releasing => Decision::Ok(Phase::FiatSent),
        _ => Decision::Reject("no payout in progress"),
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
        Phase::WaitingFiat | Phase::FiatSent | Phase::Releasing => {
            Decision::Reject("hold is locked; wait for the timelock")
        }
        _ => Decision::Reject("nothing to cancel"),
    }
}

pub fn apply_expired(phase: Phase) -> Decision {
    match phase {
        Phase::WaitingHold => Decision::Ok(Phase::Expired),
        _ => Decision::Reject("expiry only applies while waiting for the hold"),
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
    fn release_then_failed_payout_returns_to_fiat_sent() {
        assert_eq!(
            apply_release(Phase::FiatSent, maker()),
            Decision::Ok(Phase::Releasing)
        );
        assert_eq!(
            apply_release_failed(Phase::Releasing),
            Decision::Ok(Phase::FiatSent)
        );
        assert_eq!(
            apply_release(Phase::FiatSent, maker()),
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
    fn actor_of_matches_maker_and_taker() {
        assert_eq!(actor_of("m", Some("t"), "m"), Actor::Maker);
        assert_eq!(actor_of("m", Some("t"), "t"), Actor::Taker);
        assert_eq!(actor_of("m", Some("t"), "x"), Actor::Other);
    }
}
