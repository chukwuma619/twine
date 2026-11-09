use crate::db::Db;
use crate::fiber::{
    FiberRpc, HashAlgorithm, ParsedInvoice, SendFailureKind, classify_send_failure,
    payment_expiry_limited, payment_invoice_expired, payment_route_failed, route_not_ready,
};
use crate::nostr::publish_outbounds;
use crate::types::{Order, Trade};
use crate::{
    Actor, CANCELED, CANT_DO, CantDoPayload, ClientAction, Decision, EXPIRED, Envelope,
    FIBER_POLL_SECS, INVOICE_EXPIRY_SECS, InvoiceStatus, NEED_ROUTE, NEW_INVOICE,
    NewInvoicePayload, OUTCOME_BUYER_UNPAID, OUTCOME_FIAT_OWED, OUTCOME_UNRESOLVED, Outbound,
    PAY_INVOICE, PayInvoicePayload, PayoutBudget, Phase, REFUNDING, RELEASE_DUE, ReleaseDuePayload,
    SETTLED, SOLVER_WINDOW_SECS, TRADE_RECORD, TradeRecord, TradeRecordNotice, WAITING_FIAT,
    WaitingFiatPayload, apply_clock, apply_expired, apply_release_failed, apply_release_succeeded,
    apply_route_blocked, apply_route_failed_after_fiat, apply_route_ready, invoice_payable_through,
    payout_budget, payout_invoice_must_last_secs, trade_actor, unix_now, unix_now_ms,
};
use anyhow::{Result, anyhow, bail};
use nostr_sdk::prelude::*;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::warn;

#[derive(Clone, Copy, Debug)]
pub enum PollKind {
    Hold,
    Payout,
}

#[derive(Clone)]
struct PollHub {
    client: Client,
    keys: Keys,
    polling: Arc<Mutex<HashSet<String>>>,
}

pub struct Engine<F> {
    pub db: Arc<Db>,
    pub(crate) fiber: Arc<F>,
    pub(crate) solver: Option<String>,
    polls: Arc<Mutex<Option<PollHub>>>,
}

impl<F> Clone for Engine<F> {
    fn clone(&self) -> Self {
        Self {
            db: self.db.clone(),
            fiber: self.fiber.clone(),
            solver: self.solver.clone(),
            polls: self.polls.clone(),
        }
    }
}

impl<F: FiberRpc + 'static> Engine<F> {
    pub fn solver_pubkey(&self) -> Option<&str> {
        self.solver.as_deref()
    }

    pub fn new(db: Arc<Db>, fiber: Arc<F>, solver: Option<String>) -> Self {
        Self {
            db,
            fiber,
            solver,
            polls: Arc::new(Mutex::new(None)),
        }
    }

    /// Solver notices still waiting on a relay.
    pub fn pending_evidence_outbounds(&self) -> Result<Vec<Outbound>> {
        let Some(solver) = self.solver.as_deref() else {
            return Ok(Vec::new());
        };
        crate::app::dispute::pending_evidence_outbounds(&self.db, solver)
    }

    /// The solver notice in `outbound` reached a relay. Later passes leave it alone.
    pub fn note_evidence_delivered(&self, outbound: &[Outbound]) -> Result<()> {
        let Some(solver) = self.solver.as_deref() else {
            return Ok(());
        };
        for item in outbound {
            let Outbound::Reply { to, envelope } = item else {
                continue;
            };
            if to != solver {
                continue;
            }
            let shared = envelope
                .payload
                .as_ref()
                .is_some_and(|payload| payload.get("disclosures").is_some());
            if !shared {
                continue;
            }
            let Some(trade_id) = envelope.trade_id.as_deref() else {
                continue;
            };
            self.db.clear_evidence_pending(trade_id)?;
        }
        Ok(())
    }

    /// Nostr client used to publish what the hold and payout polls learn.
    /// Without this, `watch` does nothing. Tests leave it unset.
    pub fn bind_polls(&self, client: Client, keys: Keys, polling: Arc<Mutex<HashSet<String>>>) {
        *self.polls.lock().expect("polls") = Some(PollHub {
            client,
            keys,
            polling,
        });
    }

    /// Start the Fiber watch for a trade that is still open.
    /// A releasing trade also starts the payout watch.
    pub fn watch(&self, trade_id: &str) -> Result<()> {
        let hub = self.polls.lock().expect("polls").clone();
        let Some(hub) = hub else {
            return Ok(());
        };
        let Some(trade) = self.db.get_trade(trade_id)? else {
            return Ok(());
        };
        let phase = trade.phase()?;
        if phase.watched() || phase == Phase::WaitingInvoice {
            self.spawn_poll(
                PollKind::Hold,
                hub.client.clone(),
                hub.keys.clone(),
                trade_id.to_string(),
                hub.polling.clone(),
            );
        }
        if phase == Phase::Releasing || phase == Phase::AwaitingInvoice {
            self.spawn_poll(
                PollKind::Payout,
                hub.client,
                hub.keys,
                trade_id.to_string(),
                hub.polling,
            );
        }
        Ok(())
    }

    /// Re-register a preimage only after a payout has revealed it.
    /// An open hold has no preimage until the buyer is paid. That is the point.
    pub async fn arm_watchtower(&self) -> Result<()> {
        for trade in self.db.trades_in_states(Phase::watched_phases())? {
            if trade.phase()? == Phase::Refunding {
                if self.preimage_pending(&trade)? {
                    self.drop_preimage(&trade).await?;
                }
                continue;
            }
            let Some(hash) = trade.hold_payment_hash.as_deref() else {
                bail!("trade {} is open without a hold hash", trade.id);
            };
            let Some(preimage) = self.db.get_preimage(hash)? else {
                continue;
            };
            self.fiber
                .create_preimage(hash, &preimage)
                .await
                .map_err(|error| {
                    anyhow!(
                        "watchtower create_preimage failed for trade {}: {error}",
                        trade.id
                    )
                })?;
        }
        Ok(())
    }

    pub fn resume_trade_ids(&self) -> Result<(Vec<String>, Vec<String>)> {
        let holds = {
            let mut ids: Vec<String> = self
                .db
                .trades_in_states(Phase::watched_phases())?
                .into_iter()
                .map(|trade| trade.id)
                .collect();
            ids.extend(
                self.db
                    .trades_in_states(&[Phase::WaitingInvoice])?
                    .into_iter()
                    .map(|trade| trade.id),
            );
            ids
        };
        let payouts = self
            .db
            .trades_in_states(&[Phase::Releasing, Phase::AwaitingInvoice])?
            .into_iter()
            .map(|trade| trade.id)
            .collect();
        Ok((holds, payouts))
    }

    pub async fn handle(&self, sender: &str, envelope: Envelope) -> Result<Vec<Outbound>> {
        match envelope.client_action() {
            Some(ClientAction::NewOrder) => {
                crate::app::new_order::on_new_order(self, sender, envelope).await
            }
            Some(ClientAction::Take) => crate::app::take::on_take(self, sender, envelope).await,
            Some(ClientAction::PayoutInvoice) => {
                crate::app::payout_invoice::on_payout_invoice(self, sender, envelope).await
            }
            Some(ClientAction::FiatSent) => {
                crate::app::fiat_sent::on_fiat_sent(self, sender, envelope).await
            }
            Some(ClientAction::Release) => {
                crate::app::release::on_release(self, sender, envelope).await
            }
            Some(ClientAction::Cancel) => {
                crate::app::cancel::on_cancel(self, sender, envelope).await
            }
            Some(ClientAction::Dispute) => {
                crate::app::dispute::on_dispute(self, sender, envelope).await
            }
            Some(ClientAction::Resolve) => {
                crate::app::resolve::on_resolve(self, sender, envelope).await
            }
            Some(ClientAction::MyTrades) => crate::app::my_trades::on_my_trades(self, sender),
            None => Ok(vec![cant_do(
                sender,
                envelope.trade_id.clone(),
                "unknown action",
            )]),
        }
    }

    pub async fn on_hold_status(&self, trade_id: &str) -> Result<Vec<Outbound>> {
        let Some(mut trade) = self.db.get_trade(trade_id)? else {
            bail!("trade {trade_id} not found");
        };
        let phase = trade.phase()?;
        if phase == Phase::WaitingInvoice {
            return self.expire_waiting_invoice(&trade).await;
        }
        if !phase.watched() {
            return Ok(Vec::new());
        }
        let payment_hash = trade
            .hold_payment_hash
            .clone()
            .ok_or_else(|| anyhow!("trade missing hold hash"))?;
        let invoice = self.fiber.get_invoice(&payment_hash).await?;
        let status = InvoiceStatus::parse(&invoice.status)
            .ok_or_else(|| anyhow!("unknown invoice status {}", invoice.status))?;
        let order = self.require_order(&trade.order_id)?;
        if status == InvoiceStatus::Expired {
            return self.expire_trade(&order, &trade).await;
        }
        if status == InvoiceStatus::Cancelled && phase == Phase::WaitingHold {
            return self.complete_cancel(&order, &trade).await;
        }
        // A seller payment can land as Paid when Fiber settles the hold before
        // this poll observes Received. Either status means the coins arrived.
        // Both still need the inbound TLC expiry so pay_by / release clocks exist.
        if phase == Phase::WaitingHold
            && matches!(status, InvoiceStatus::Received | InvoiceStatus::Paid)
        {
            let Some(hold) = self.fiber.inbound_hold(&payment_hash).await? else {
                return Ok(Vec::new());
            };
            return self.after_hold_locked(&mut trade, &order, hold).await;
        }
        if matches!(status, InvoiceStatus::Received | InvoiceStatus::Paid)
            && trade.hold_expires_at_ms.is_none()
        {
            self.remember_hold_expiry(&mut trade, &payment_hash).await?;
        }
        if phase == Phase::Refunding && self.preimage_pending(&trade)? {
            self.drop_preimage(&trade).await?;
            return Ok(party_replies(
                &trade,
                Envelope::new(REFUNDING).with_trade(&trade.id),
            ));
        }
        let mut refund = self.refund_if_due(&trade).await?;
        if !refund.is_empty() {
            let mut published = self.publish_unresolved_if_due(&trade)?;
            published.append(&mut refund);
            return Ok(published);
        }
        let unresolved = self.publish_unresolved_if_due(&trade)?;
        if !unresolved.is_empty() {
            return Ok(unresolved);
        }
        self.release_due_notice(&trade)
    }

    pub(crate) async fn expire_trade(&self, order: &Order, trade: &Trade) -> Result<Vec<Outbound>> {
        match apply_expired(trade.phase()?) {
            Decision::Ok(_) => {
                // Stop claiming before the slice goes back on the book. Refunding is
                // watched, so a crash here retries the drop instead of bricking boot.
                if trade.phase()? != Phase::Refunding {
                    self.db
                        .set_trade_state(&trade.id, Phase::Refunding.as_str())?;
                }
                if self.preimage_pending(trade)? {
                    self.drop_preimage(trade).await?;
                }
                self.finish_terminal(order, trade, Phase::Expired)?;
                let order = self.require_order(&order.id)?;
                let mut outbound =
                    party_replies(trade, Envelope::new(EXPIRED).with_trade(&trade.id));
                if trade.fiat_acknowledged {
                    let mut published = self.publish_outcome(trade, OUTCOME_FIAT_OWED)?;
                    published.append(&mut outbound);
                    outbound = published;
                } else {
                    let mut published = self.publish_unresolved_if_due(trade)?;
                    published.append(&mut outbound);
                    outbound = published;
                }
                outbound.insert(0, Outbound::PublicOrder(order.public()));
                Ok(outbound)
            }
            Decision::Reject(_) => Ok(Vec::new()),
            Decision::NoOp => Ok(Vec::new()),
        }
    }

    async fn remember_hold_expiry(&self, trade: &mut Trade, payment_hash: &str) -> Result<()> {
        let Some(hold) = self.fiber.inbound_hold(payment_hash).await? else {
            return Ok(());
        };
        self.db
            .set_hold_timing(&trade.id, hold.expires_at_ms, hold.claim_margin_ms)?;
        trade.hold_expires_at_ms = Some(hold.expires_at_ms);
        trade.claim_margin_ms = Some(hold.claim_margin_ms);
        if trade.hold_received_at.is_none() {
            let now = unix_now();
            self.db.set_hold_received_at(&trade.id, now)?;
            trade.hold_received_at = Some(now);
        }
        Ok(())
    }

    async fn refund_if_due(&self, trade: &Trade) -> Result<Vec<Outbound>> {
        let phase = trade.phase()?;
        let (deadline, reason) = match phase {
            Phase::AwaitingRoute | Phase::WaitingFiat => (trade.pay_by(), Some(FIAT_WINDOW_CLOSED)),
            // A dispute opened before fiat is marked sent keeps the fiat deadline.
            Phase::Disputed if trade.fiat_sent_at.is_none() => {
                (trade.pay_by(), Some(FIAT_WINDOW_CLOSED))
            }
            Phase::FiatSent | Phase::AwaitingInvoice | Phase::Disputed | Phase::Releasing => {
                (trade.route_by(), None)
            }
            _ => (None, None),
        };
        match apply_clock(phase, unix_now(), deadline) {
            Decision::Ok(Phase::Refunding) => self.refund_for(trade, reason).await,
            Decision::NoOp => Ok(Vec::new()),
            other => bail!("invalid clock: {other:?}"),
        }
    }

    /// One notice after the seller's 2-hour grace, while the hold can still be paid.
    fn release_due_notice(&self, trade: &Trade) -> Result<Vec<Outbound>> {
        if !matches!(trade.phase()?, Phase::FiatSent | Phase::AwaitingInvoice)
            || trade.fiat_acknowledged
            || trade.release_prompted
        {
            return Ok(Vec::new());
        }
        let Some(release_by) = trade.release_by() else {
            return Ok(Vec::new());
        };
        if unix_now() < release_by {
            return Ok(Vec::new());
        }
        self.db.set_release_prompted(&trade.id)?;
        Ok(party_replies(
            trade,
            Envelope::new(RELEASE_DUE)
                .with_trade(&trade.id)
                .with_payload(ReleaseDuePayload {
                    release_by,
                    solver_window_secs: SOLVER_WINDOW_SECS,
                })?,
        ))
    }

    /// Solver found the buyer did not pay. Counts on the public trade record.
    pub(crate) async fn record_buyer_unpaid(&self, trade: &Trade) -> Result<Vec<Outbound>> {
        if trade.outcome.is_some() {
            bail!("trade {} already has a public record", trade.id);
        }
        self.db.set_fiat_acknowledged(&trade.id, false)?;
        let mut outbound = if trade.phase()? == Phase::Disputed {
            self.refund_for(trade, Some(BUYER_UNPAID_REASON)).await?
        } else {
            party_replies(
                trade,
                Envelope::new(REFUNDING)
                    .with_trade(&trade.id)
                    .with_payload(NewInvoicePayload {
                        reason: BUYER_UNPAID_REASON.to_string(),
                    })?,
            )
        };
        let mut published = self.publish_outcome(trade, OUTCOME_BUYER_UNPAID)?;
        published.append(&mut outbound);
        Ok(published)
    }

    /// Fiat arrived and the payout can no longer start. The CKB returns to the seller.
    pub(crate) async fn record_fiat_owed(
        &self,
        trade: &Trade,
        sender: &str,
    ) -> Result<Vec<Outbound>> {
        if trade.outcome.is_some() {
            return Ok(vec![cant_do(
                sender,
                Some(trade.id.clone()),
                RECORD_ALREADY,
            )]);
        }
        if !trade.chat_key_shared {
            return Ok(vec![cant_do(
                sender,
                Some(trade.id.clone()),
                CHAT_KEY_REQUIRED,
            )]);
        }
        self.db.set_fiat_acknowledged(&trade.id, true)?;
        if trade.phase()? == Phase::Disputed {
            return self.begin_refund(trade).await;
        }
        let mut outbound = party_replies(
            trade,
            Envelope::new(REFUNDING)
                .with_trade(&trade.id)
                .with_payload(NewInvoicePayload {
                    reason: FIAT_OWED_REASON.to_string(),
                })?,
        );
        let mut published = self.publish_outcome(trade, OUTCOME_FIAT_OWED)?;
        published.append(&mut outbound);
        Ok(published)
    }

    pub(crate) async fn begin_refund(&self, trade: &Trade) -> Result<Vec<Outbound>> {
        self.refund_for(trade, None).await
    }

    async fn refund_for(&self, trade: &Trade, reason: Option<&str>) -> Result<Vec<Outbound>> {
        self.db
            .set_trade_state(&trade.id, Phase::Refunding.as_str())?;
        self.drop_preimage(trade).await?;
        let acknowledged = self
            .db
            .get_trade(&trade.id)?
            .is_some_and(|current| current.fiat_acknowledged);
        let text = refund_reason(reason, acknowledged);
        let mut envelope = Envelope::new(REFUNDING).with_trade(&trade.id);
        if let Some(text) = &text {
            envelope = envelope.with_payload(NewInvoicePayload {
                reason: text.clone(),
            })?;
        }
        let mut outbound = party_replies(trade, envelope);
        if acknowledged {
            let mut published = self.publish_outcome(trade, OUTCOME_FIAT_OWED)?;
            published.append(&mut outbound);
            outbound = published;
        }
        Ok(outbound)
    }

    /// Public record plus the same outcome for the two apps. The first write sticks.
    fn publish_outcome(&self, trade: &Trade, outcome: &str) -> Result<Vec<Outbound>> {
        if !self.db.set_outcome_once(&trade.id, outcome)? {
            return Ok(Vec::new());
        }
        let order = self.require_order(&trade.order_id)?;
        let record = match outcome {
            OUTCOME_FIAT_OWED => TradeRecord::fiat_owed(trade, &order.fiat_currency),
            OUTCOME_BUYER_UNPAID => TradeRecord::buyer_unpaid(trade, &order.fiat_currency),
            OUTCOME_UNRESOLVED => TradeRecord::unresolved(trade, &order.fiat_currency),
            _ => bail!("unknown trade outcome {outcome}"),
        };
        let mut outbound = party_replies(
            trade,
            Envelope::new(TRADE_RECORD)
                .with_trade(&trade.id)
                .with_payload(TradeRecordNotice {
                    outcome: outcome.to_string(),
                })?,
        );
        outbound.insert(0, Outbound::TradeRecord(record));
        Ok(outbound)
    }

    /// Fiat was marked sent, and neither the seller nor the solver decided before the window ended.
    fn publish_unresolved_if_due(&self, trade: &Trade) -> Result<Vec<Outbound>> {
        if !unresolved_due(trade, unix_now()) {
            return Ok(Vec::new());
        }
        self.publish_outcome(trade, OUTCOME_UNRESOLVED)
    }

    /// After fiat-sent is recorded, check the route separately. A missing path moves the
    /// trade to awaiting-invoice without erasing the buyer's payment report.
    pub(crate) async fn route_after_fiat_sent(
        &self,
        trade: &Trade,
    ) -> Result<Option<Vec<Outbound>>> {
        match self.probe_payout(trade).await? {
            PayoutProbe::Ready { .. } => Ok(None),
            PayoutProbe::NoRoute(reason) | PayoutProbe::SessionExists(reason) => {
                Ok(Some(self.mark_awaiting_invoice_after_fiat(trade, &reason)?))
            }
            PayoutProbe::InvoiceExpired => Ok(Some(self.refund_expired_invoice(trade).await?)),
            PayoutProbe::WindowClosed => Ok(Some(self.refund_past_deadline(trade).await?)),
            PayoutProbe::Later => Ok(Some(self.mark_awaiting_invoice_after_fiat(
                trade,
                "could not check the payout route. Send the same invoice again",
            )?)),
        }
    }

    async fn after_hold_locked(
        &self,
        trade: &mut Trade,
        order: &Order,
        hold: crate::fiber::InboundHold,
    ) -> Result<Vec<Outbound>> {
        let received_at = unix_now();
        trade.hold_received_at = Some(received_at);
        trade.hold_expires_at_ms = Some(hold.expires_at_ms);
        trade.claim_margin_ms = Some(hold.claim_margin_ms);
        self.db.set_hold_received(
            &trade.id,
            Phase::WaitingHold.as_str(),
            received_at,
            Some(hold.expires_at_ms),
            Some(hold.claim_margin_ms),
        )?;
        self.finish_route_probe(trade, order).await
    }

    async fn finish_route_probe(&self, trade: &Trade, order: &Order) -> Result<Vec<Outbound>> {
        match self.probe_payout(trade).await? {
            PayoutProbe::Ready { .. } => self.mark_waiting_fiat(trade, order),
            PayoutProbe::NoRoute(reason) | PayoutProbe::SessionExists(reason) => {
                self.mark_awaiting_route(trade, &reason)
            }
            PayoutProbe::InvoiceExpired => self.refund_expired_invoice(trade).await,
            PayoutProbe::WindowClosed => self.refund_past_deadline(trade).await,
            PayoutProbe::Later => Ok(Vec::new()),
        }
    }

    fn mark_waiting_fiat(&self, trade: &Trade, order: &Order) -> Result<Vec<Outbound>> {
        match apply_route_ready(trade.phase()?) {
            Decision::Ok(phase) => {
                self.db.set_trade_state(&trade.id, phase.as_str())?;
                let Some(fresh) = self.db.get_trade(&trade.id)? else {
                    bail!("trade {} disappeared", trade.id);
                };
                Ok(party_replies(
                    &fresh,
                    Envelope::new(WAITING_FIAT)
                        .with_trade(&fresh.id)
                        .with_payload(WaitingFiatPayload::from_trade(
                            &fresh,
                            &order.fiat_currency,
                        ))?,
                ))
            }
            other => bail!("invalid route ready: {other:?}"),
        }
    }

    fn mark_awaiting_route(&self, trade: &Trade, reason: &str) -> Result<Vec<Outbound>> {
        match apply_route_blocked(trade.phase()?) {
            Decision::Ok(phase) => {
                self.db.set_trade_state(&trade.id, phase.as_str())?;
                warn!(trade_id = %trade.id, reason, "payout invoice has no route yet");
                Ok(party_replies(
                    trade,
                    Envelope::new(NEED_ROUTE)
                        .with_trade(&trade.id)
                        .with_payload(NewInvoicePayload {
                            reason: reason.to_string(),
                        })?,
                ))
            }
            other => bail!("invalid route blocked: {other:?}"),
        }
    }

    fn mark_awaiting_invoice_after_fiat(
        &self,
        trade: &Trade,
        reason: &str,
    ) -> Result<Vec<Outbound>> {
        match apply_route_failed_after_fiat(trade.phase()?) {
            Decision::Ok(phase) => {
                self.db.set_trade_state(&trade.id, phase.as_str())?;
                warn!(trade_id = %trade.id, reason, "payout route missing after fiat was marked sent");
                Ok(party_replies(
                    trade,
                    Envelope::new(NEW_INVOICE)
                        .with_trade(&trade.id)
                        .with_payload(NewInvoicePayload {
                            reason: reason.to_string(),
                        })?,
                ))
            }
            other => bail!("invalid post-fiat route failure: {other:?}"),
        }
    }

    /// Buyer sends the same payment hash again after the wallet can receive.
    pub(crate) async fn retry_payout_route(
        &self,
        trade: &Trade,
        invoice: &str,
    ) -> Result<Vec<Outbound>> {
        let parsed = match self.require_same_hold_hash(trade, invoice).await? {
            Ok(parsed) => parsed,
            Err(reason) => {
                return Ok(vec![cant_do(
                    &trade.buyer_nostr,
                    Some(trade.id.clone()),
                    &reason,
                )]);
            }
        };
        if let Some(outbound) = self.refund_due_outbound(trade).await? {
            return Ok(outbound);
        }
        self.db.set_payout_with_final_delta(
            &trade.id,
            invoice,
            None,
            parsed.final_tlc_expiry_delta_ms,
        )?;
        let Some(trade) = self.db.get_trade(&trade.id)? else {
            bail!("trade {} disappeared", trade.id);
        };
        let order = self.require_order(&trade.order_id)?;
        match self.probe_payout(&trade).await? {
            PayoutProbe::Ready { .. } => self.mark_waiting_fiat(&trade, &order),
            PayoutProbe::NoRoute(reason) | PayoutProbe::SessionExists(reason) => {
                Ok(vec![cant_do(&trade.buyer_nostr, Some(trade.id), &reason)])
            }
            PayoutProbe::InvoiceExpired => self.refund_expired_invoice(&trade).await,
            PayoutProbe::WindowClosed => self.refund_past_deadline(&trade).await,
            PayoutProbe::Later => Ok(vec![cant_do(
                &trade.buyer_nostr,
                Some(trade.id),
                "could not check the payout route. Send the same invoice again",
            )]),
        }
    }

    /// Fiber v0.9.1 dry run. A returned preimage is ignored and does not open fiat.
    /// Ready carries the TLC limit so release can send without a second budget pass.
    async fn probe_payout(&self, trade: &Trade) -> Result<PayoutProbe> {
        let Some(invoice) = trade
            .payout_invoice
            .clone()
            .filter(|invoice| !invoice.trim().is_empty())
        else {
            warn!(trade_id = %trade.id, "payout invoice missing; holding fiat");
            return Ok(PayoutProbe::Later);
        };
        let parsed = match self.fiber.parse_invoice(&invoice).await {
            Ok(parsed) => parsed,
            Err(error) => {
                warn!(trade_id = %trade.id, %error, "payout invoice could not be parsed");
                return Ok(PayoutProbe::Later);
            }
        };
        if parsed
            .expires_at_ms
            .is_some_and(|expires| expires < unix_now_ms())
        {
            return Ok(PayoutProbe::InvoiceExpired);
        }
        if trade.final_tlc_expiry_delta_ms != Some(parsed.final_tlc_expiry_delta_ms) {
            self.db
                .set_final_tlc_expiry_delta_ms(&trade.id, parsed.final_tlc_expiry_delta_ms)?;
        }
        let budget = self
            .outgoing_budget(trade, parsed.final_tlc_expiry_delta_ms)
            .await?;
        let tlc_expiry_limit_ms = match budget {
            PayoutBudget::Refund => return Ok(PayoutProbe::WindowClosed),
            PayoutBudget::ShorterInvoice => {
                return Ok(PayoutProbe::NoRoute(
                    "payout invoice lock is longer than the hold has left".into(),
                ));
            }
            PayoutBudget::Send {
                tlc_expiry_limit_ms,
            } => tlc_expiry_limit_ms,
        };
        match self
            .fiber
            .probe_payment(&invoice, tlc_expiry_limit_ms)
            .await
        {
            Ok(payment) if payment.preimage.is_some() => {
                warn!(
                    trade_id = %trade.id,
                    "dry run returned a preimage; fiat stays closed"
                );
                Ok(PayoutProbe::Later)
            }
            Ok(payment) if payment.status == "Failed" => {
                Ok(probe_from_failure(payment.failed_error.as_deref()))
            }
            Ok(_) => {
                // Fiat windows need the invoice lock, not Fiber's shortest default.
                if trade.fiat_sent_at.is_none()
                    && trade.hold_expires_at_ms.is_some_and(|expires| {
                        crate::fiat_deadline_secs(
                            expires,
                            trade.claim_margin_ms(),
                            parsed.final_tlc_expiry_delta_ms,
                        ) <= unix_now()
                    })
                {
                    return Ok(PayoutProbe::NoRoute(
                        "payout invoice lock is longer than the hold has left".into(),
                    ));
                }
                Ok(PayoutProbe::Ready {
                    tlc_expiry_limit_ms,
                })
            }
            Err(error) => Ok(probe_from_error(&error)),
        }
    }

    async fn refund_expired_invoice(&self, trade: &Trade) -> Result<Vec<Outbound>> {
        warn!(
            trade_id = %trade.id,
            "payout invoice expired; refunding the seller"
        );
        self.refund_for(trade, Some(PAYOUT_INVOICE_EXPIRED)).await
    }

    pub(crate) async fn expire_waiting_invoice(&self, trade: &Trade) -> Result<Vec<Outbound>> {
        let Some(created) = trade.hold_created_at else {
            return Ok(Vec::new());
        };
        if unix_now() < created + INVOICE_EXPIRY_SECS as i64 {
            return Ok(Vec::new());
        }
        let order = self.require_order(&trade.order_id)?;
        self.complete_cancel(&order, trade).await
    }

    /// Create the seller's hold under the buyer's payment hash. No preimage is stored.
    pub(crate) async fn bind_buyer_hold(
        &self,
        trade: &Trade,
        invoice: &str,
    ) -> Result<Result<Vec<Outbound>, String>> {
        if trade.phase()? != Phase::WaitingInvoice {
            return Ok(Err("this trade is not waiting for an invoice".into()));
        }
        let parsed = match self.fiber.parse_invoice(invoice).await {
            Ok(parsed) => parsed,
            Err(error) => return Ok(Err(format!("payout invoice rejected: {error}"))),
        };
        if let Err(reason) = payout_invoice_ok(&parsed, self.fiber.currency(), trade.shannons) {
            return Ok(Err(reason.into()));
        }
        if self.db.hold_hash_used(&parsed.payment_hash, &trade.id)? {
            return Ok(Err("this payout hash was already used".into()));
        }
        let order = self.require_order(&trade.order_id)?;
        if crate::fiat_deadline_secs(
            order.hold_secs as i64 * 1_000,
            crate::CLAIM_MARGIN_MS,
            parsed.final_tlc_expiry_delta_ms,
        ) <= 0
        {
            return Ok(Err(
                "payout invoice lock is longer than the hold has left".into()
            ));
        }
        let must_last = payout_invoice_must_last_secs(
            order.hold_secs,
            crate::CLAIM_MARGIN_MS,
            parsed.final_tlc_expiry_delta_ms,
        );
        let until_ms = unix_now_ms().saturating_add(must_last as i64 * 1_000);
        if !invoice_payable_through(parsed.expires_at_ms, until_ms) {
            let hours = must_last.div_ceil(3_600);
            return Ok(Err(format!(
                "payout invoice expires before the hold can be paid. It has to stay payable for {hours} hours, or have no expiry"
            )));
        }
        let created = match self
            .fiber
            .new_hold_invoice(
                trade.shannons,
                &parsed.payment_hash,
                parsed.hash_algorithm,
                &format!("twine {}", trade.id),
                order.hold_secs * 1_000,
                INVOICE_EXPIRY_SECS,
            )
            .await
        {
            Ok(created) => created,
            Err(error) => return Ok(Err(format!("hold invoice failed: {error}"))),
        };
        let created_at = unix_now();
        self.db.set_hold(
            &trade.id,
            &parsed.payment_hash,
            &created.invoice,
            created_at,
        )?;
        self.db.set_payout_with_final_delta(
            &trade.id,
            invoice,
            None,
            parsed.final_tlc_expiry_delta_ms,
        )?;
        self.db
            .set_trade_state(&trade.id, Phase::WaitingHold.as_str())?;
        self.watch(&trade.id)?;
        let order = self.require_order(&trade.order_id)?;
        Ok(Ok(pay_invoice_replies(
            trade,
            &order,
            created.invoice,
            created_at,
        )?))
    }

    /// A replacement invoice has to settle the hold that already exists.
    pub(crate) async fn require_same_hold_hash(
        &self,
        trade: &Trade,
        invoice: &str,
    ) -> Result<Result<ParsedInvoice, String>> {
        let Some(hold_hash) = trade.hold_payment_hash.as_deref() else {
            return Ok(Err("hold is not open yet".into()));
        };
        let parsed = match self.fiber.parse_invoice(invoice).await {
            Ok(parsed) => parsed,
            Err(error) => return Ok(Err(format!("payout invoice rejected: {error}"))),
        };
        if !same_hash(&parsed.payment_hash, hold_hash) {
            return Ok(Err(
                "this invoice uses a new payment hash. Retry with the same hash as the hold".into(),
            ));
        }
        let hold_algorithm = match self.hold_algorithm(trade).await? {
            Ok(algorithm) => algorithm,
            Err(reason) => return Ok(Err(reason)),
        };
        if parsed.hash_algorithm != hold_algorithm {
            return Ok(Err(
                "this invoice uses a different hash algorithm than the hold".into(),
            ));
        }
        if let Err(reason) = payout_invoice_ok(&parsed, self.fiber.currency(), trade.shannons) {
            return Ok(Err(reason.into()));
        }
        let until_ms = trade
            .hold_expires_at_ms
            .map(|expires| {
                crate::payout_deadline_secs(
                    expires,
                    trade.claim_margin_ms(),
                    parsed.final_tlc_expiry_delta_ms,
                )
                .saturating_mul(1000)
            })
            .unwrap_or_else(unix_now_ms);
        if !invoice_payable_through(parsed.expires_at_ms, until_ms) {
            return Ok(Err("this invoice expires before the payout window".into()));
        }
        Ok(Ok(parsed))
    }

    async fn hold_algorithm(&self, trade: &Trade) -> Result<Result<HashAlgorithm, String>> {
        let Some(invoice) = trade.hold_invoice.as_deref() else {
            return Ok(Err("hold is not open yet".into()));
        };
        match self.fiber.parse_invoice(invoice).await {
            Ok(parsed) => Ok(Ok(parsed.hash_algorithm)),
            Err(error) => Ok(Err(format!("hold invoice rejected: {error}"))),
        }
    }

    pub(crate) async fn drop_preimage(&self, trade: &Trade) -> Result<()> {
        let Some(hash) = trade.hold_payment_hash.as_deref() else {
            return Ok(());
        };
        if self.db.get_preimage(hash)?.is_none() {
            return Ok(());
        }
        self.fiber.remove_preimage(hash).await?;
        self.db.delete_preimage(hash)?;
        Ok(())
    }

    pub async fn on_payout_status(&self, trade_id: &str) -> Result<Vec<Outbound>> {
        let Some(mut trade) = self.db.get_trade(trade_id)? else {
            bail!("trade {trade_id} not found");
        };
        if trade.phase()? == Phase::AwaitingInvoice {
            return self.recover_awaiting_payout(&trade).await;
        }
        if trade.phase()? != Phase::Releasing {
            return Ok(Vec::new());
        }
        if let Some(hash) = trade.hold_payment_hash.clone()
            && trade.hold_expires_at_ms.is_none()
        {
            self.remember_hold_expiry(&mut trade, &hash).await?;
        }
        if trade.payout_payment_hash.is_none()
            && let Some(outbound) = self.refund_due_outbound(&trade).await?
        {
            return Ok(outbound);
        }
        let starting_fresh = trade.payout_payment_hash.is_none();
        let invoice = trade
            .payout_invoice
            .clone()
            .ok_or_else(|| anyhow!("missing payout invoice"))?;
        if trade.payout_payment_hash.is_none() {
            let hold_hash = trade
                .hold_payment_hash
                .clone()
                .ok_or_else(|| anyhow!("missing hold hash"))?;
            let parsed = match self.fiber.parse_invoice(&invoice).await {
                Ok(parsed) => parsed,
                Err(error) => {
                    return self.fail_payout(&trade, &format!("payout invoice rejected: {error}"));
                }
            };
            if !same_hash(&parsed.payment_hash, &hold_hash) {
                return self.fail_payout(&trade, "payout invoice hash does not match the hold");
            }
            let hold_algorithm = match self.hold_algorithm(&trade).await? {
                Ok(algorithm) => algorithm,
                Err(reason) => return self.fail_payout(&trade, &reason),
            };
            if parsed.hash_algorithm != hold_algorithm {
                return self.fail_payout(
                    &trade,
                    "payout invoice hash algorithm does not match the hold",
                );
            }
            if let Err(reason) = payout_invoice_ok(&parsed, self.fiber.currency(), trade.shannons) {
                return self.fail_payout(&trade, reason);
            }
            if parsed
                .expires_at_ms
                .is_some_and(|expires| expires < unix_now_ms())
            {
                return self.refund_expired_invoice(&trade).await;
            }
            // Record the hash before send_payment. A crash then resumes with get_payment
            // instead of paying the buyer twice.
            self.db.set_payout(&trade.id, &invoice, Some(&hold_hash))?;
            trade.payout_payment_hash = Some(hold_hash);
        }
        let payment_hash = trade
            .payout_payment_hash
            .clone()
            .ok_or_else(|| anyhow!("missing payout hash"))?;
        let hold_hash = trade
            .hold_payment_hash
            .clone()
            .ok_or_else(|| anyhow!("missing hold hash"))?;
        if !same_hash(&payment_hash, &hold_hash) {
            return self.fail_payout(&trade, "payout invoice hash does not match the hold");
        }
        match self.fiber.get_payment(&payment_hash).await {
            Err(error) if payment_missing(&error) && payout_window_closed(&trade) => {
                self.refund_past_deadline(&trade).await
            }
            Err(error) if payment_missing(&error) => {
                self.send_recorded_payout(&trade, &invoice).await
            }
            Err(error) => Err(error),
            Ok(payment) if payment.status == "Failed" && payout_window_closed(&trade) => {
                self.refund_past_deadline(&trade).await
            }
            Ok(payment)
                if payment.status == "Failed"
                    && payment_invoice_expired(payment.failed_error.as_deref()) =>
            {
                self.refund_expired_invoice(&trade).await
            }
            // Hop expiry past tlc_expiry_limit. Retry while the hold can still fit a route.
            Ok(payment)
                if payment.status == "Failed"
                    && payment_expiry_limited(payment.failed_error.as_deref()) =>
            {
                self.send_recorded_payout(&trade, &invoice).await
            }
            // Fiber records "no path" as Failed. A resubmitted invoice sends it again.
            Ok(payment)
                if starting_fresh
                    && payment.status == "Failed"
                    && payment_route_failed(payment.failed_error.as_deref()) =>
            {
                self.send_recorded_payout(&trade, &invoice).await
            }
            Ok(payment)
                if payment.status == "Failed"
                    && payment_route_failed(payment.failed_error.as_deref()) =>
            {
                self.fail_payout(&trade, PAYOUT_NO_ROUTE)
            }
            Ok(payment) => {
                self.finish_recorded_payout(&trade, &payment.status, payment.preimage)
                    .await
            }
        }
    }

    /// A payout can already be Success after the trade asked for another invoice.
    async fn recover_awaiting_payout(&self, trade: &Trade) -> Result<Vec<Outbound>> {
        let Some(hash) = trade.hold_payment_hash.clone() else {
            return Ok(Vec::new());
        };
        let Some(invoice) = trade
            .payout_invoice
            .clone()
            .filter(|value| !value.trim().is_empty())
        else {
            return Ok(Vec::new());
        };
        let payment = match self.fiber.get_payment(&hash).await {
            Ok(payment) => payment,
            Err(error) if payment_missing(&error) => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        if payment.status != "Success" {
            return Ok(Vec::new());
        }
        self.db
            .set_trade_state(&trade.id, Phase::Releasing.as_str())?;
        self.db.set_payout(&trade.id, &invoice, Some(&hash))?;
        self.finish_recorded_payout(trade, &payment.status, payment.preimage)
            .await
    }

    async fn refund_past_deadline(&self, trade: &Trade) -> Result<Vec<Outbound>> {
        warn!(trade_id = %trade.id, "payout window closed; refunding the seller");
        self.begin_refund(trade).await
    }

    async fn send_recorded_payout(&self, trade: &Trade, invoice: &str) -> Result<Vec<Outbound>> {
        // Reuse probe_payout's budget; the earlier dry run did not reserve capacity.
        let tlc_expiry_limit_ms = match self.probe_payout(trade).await? {
            PayoutProbe::Ready {
                tlc_expiry_limit_ms,
            } => tlc_expiry_limit_ms,
            PayoutProbe::NoRoute(reason) => return self.fail_payout(trade, &reason),
            // Session may already be live (Fiber CCH). Wait on get_payment.
            PayoutProbe::SessionExists(_) => {
                return self.await_existing_payout(trade).await;
            }
            PayoutProbe::InvoiceExpired => return self.refund_expired_invoice(trade).await,
            PayoutProbe::WindowClosed => return self.refund_past_deadline(trade).await,
            PayoutProbe::Later => return Ok(Vec::new()),
        };
        // A transport error may mean the payment landed. Leave the hash in place
        // and let the next poll read it. Do not fail the payout and do not send again.
        match self.fiber.send_payment(invoice, tlc_expiry_limit_ms).await {
            Ok(payment) if payment.status == "Failed" => {
                self.handle_send_failure(trade, payment.failed_error.as_deref())
                    .await
            }
            Ok(payment) => {
                self.finish_recorded_payout(trade, &payment.status, payment.preimage)
                    .await
            }
            Err(error) => self.handle_send_error(trade, &error).await,
        }
    }

    /// Session already exists: prefer the live Fiber payment over failing the trade.
    async fn await_existing_payout(&self, trade: &Trade) -> Result<Vec<Outbound>> {
        let Some(hash) = trade
            .payout_payment_hash
            .as_deref()
            .or(trade.hold_payment_hash.as_deref())
        else {
            return Ok(Vec::new());
        };
        match self.fiber.get_payment(hash).await {
            Ok(payment) => {
                self.finish_recorded_payout(trade, &payment.status, payment.preimage)
                    .await
            }
            Err(error) if payment_missing(&error) => {
                warn!(
                    trade_id = %trade.id,
                    "session exists but get_payment is missing; waiting for the next poll"
                );
                Ok(Vec::new())
            }
            Err(error) => Err(error),
        }
    }

    async fn handle_send_failure(
        &self,
        trade: &Trade,
        failed_error: Option<&str>,
    ) -> Result<Vec<Outbound>> {
        match classify_send_failure(failed_error) {
            SendFailureKind::InvoiceExpired => self.refund_expired_invoice(trade).await,
            SendFailureKind::SessionExists => self.await_existing_payout(trade).await,
            SendFailureKind::AttemptsInflight | SendFailureKind::ExpiryLimited => Ok(Vec::new()),
            SendFailureKind::NoRoute => {
                warn!(
                    trade_id = %trade.id,
                    error = failed_error.unwrap_or("no path"),
                    "payout has no route"
                );
                self.fail_payout(trade, PAYOUT_NO_ROUTE)
            }
            SendFailureKind::Other => {
                self.fail_payout(trade, failed_error.unwrap_or("buyer payout failed"))
            }
        }
    }

    async fn handle_send_error(
        &self,
        trade: &Trade,
        error: &anyhow::Error,
    ) -> Result<Vec<Outbound>> {
        let text = error.to_string();
        match classify_send_failure(Some(&text)) {
            SendFailureKind::InvoiceExpired => self.refund_expired_invoice(trade).await,
            SendFailureKind::SessionExists => self.await_existing_payout(trade).await,
            SendFailureKind::AttemptsInflight | SendFailureKind::ExpiryLimited => Ok(Vec::new()),
            SendFailureKind::NoRoute => {
                warn!(trade_id = %trade.id, %error, "payout has no route");
                self.fail_payout(trade, PAYOUT_NO_ROUTE)
            }
            SendFailureKind::Other if route_not_ready(error) => {
                warn!(trade_id = %trade.id, %error, "payout has no route");
                self.fail_payout(trade, PAYOUT_NO_ROUTE)
            }
            SendFailureKind::Other => Err(anyhow!("{error}")),
        }
    }

    /// `tlc_expiry_limit` from the inbound TLC. Missing expiry refunds instead of
    /// inventing Fiber's max budget, so the 2h/4h windows stay enforced.
    async fn outgoing_budget(&self, trade: &Trade, final_delta_ms: u64) -> Result<PayoutBudget> {
        if let Some(expires_at_ms) = trade.hold_expires_at_ms {
            let claim_margin_ms = if let Some(margin) = trade.claim_margin_ms {
                margin
            } else if let Some(hash) = trade.hold_payment_hash.as_deref() {
                self.fiber
                    .inbound_hold(hash)
                    .await?
                    .map(|hold| hold.claim_margin_ms)
                    .unwrap_or_else(|| trade.claim_margin_ms())
            } else {
                trade.claim_margin_ms()
            };
            return Ok(payout_budget(
                expires_at_ms,
                unix_now_ms(),
                claim_margin_ms,
                final_delta_ms,
            ));
        }
        let Some(hash) = trade.hold_payment_hash.as_deref() else {
            return Ok(PayoutBudget::Refund);
        };
        if let Some(hold) = self.fiber.inbound_hold(hash).await? {
            return Ok(payout_budget(
                hold.expires_at_ms,
                unix_now_ms(),
                hold.claim_margin_ms,
                final_delta_ms,
            ));
        }
        Ok(PayoutBudget::Refund)
    }

    async fn finish_recorded_payout(
        &self,
        trade: &Trade,
        status: &str,
        preimage: Option<String>,
    ) -> Result<Vec<Outbound>> {
        match status {
            "Success" => {
                if preimage.is_none() && !self.preimage_pending(trade)? {
                    // Fiber can report success before payment_preimage is filled in.
                    // Leave the recorded hash in place and read it again. Failing
                    // the payout here would clear that hash and could pay the buyer twice.
                    warn!(
                        trade_id = %trade.id,
                        "payout succeeded without payment_preimage; waiting for Fiber to return it"
                    );
                    return Ok(Vec::new());
                }
                self.settle_hold(trade, preimage.as_deref()).await
            }
            "Failed" => self.fail_payout(trade, "buyer payout failed"),
            _ => Ok(Vec::new()),
        }
    }

    async fn settle_hold(&self, trade: &Trade, revealed: Option<&str>) -> Result<Vec<Outbound>> {
        let Some(current) = self.db.get_trade(&trade.id)? else {
            return Ok(Vec::new());
        };
        if current.phase()? != Phase::Releasing {
            return Ok(Vec::new());
        }
        let payment_hash = trade
            .hold_payment_hash
            .as_deref()
            .ok_or_else(|| anyhow!("missing hold hash"))?;
        let preimage = match self.db.get_preimage(payment_hash)? {
            Some(stored) => stored,
            None => {
                let revealed =
                    revealed.ok_or_else(|| anyhow!("payout succeeded without a preimage"))?;
                self.db.insert_preimage(payment_hash, revealed)?;
                revealed.to_string()
            }
        };
        if let Err(error) = self.fiber.create_preimage(payment_hash, &preimage).await {
            let text = error.to_string().to_ascii_lowercase();
            if !text.contains("exist") && !text.contains("already") {
                return Err(error);
            }
        }
        if let Err(error) = self.fiber.settle_invoice(payment_hash, &preimage).await {
            let info = self.fiber.get_invoice(payment_hash).await?;
            if InvoiceStatus::parse(&info.status) != Some(InvoiceStatus::Paid) {
                return Err(error);
            }
        }
        // Leave the watchtower copy. settle_invoice pays the live channel; a
        // force-close of a commitment that still holds the HTLC needs this preimage.
        match apply_release_succeeded(Phase::Releasing) {
            Decision::Ok(phase) => {
                self.db.set_trade_state(&trade.id, phase.as_str())?;
                if let Err(error) = self.db.delete_preimage(payment_hash) {
                    warn!(%error, trade_id = %trade.id, "delete preimage after settle");
                }
                Ok(party_replies(
                    trade,
                    Envelope::new(SETTLED).with_trade(&trade.id),
                ))
            }
            other => bail!("invalid settle: {other:?}"),
        }
    }

    fn fail_payout(&self, trade: &Trade, reason: &str) -> Result<Vec<Outbound>> {
        match apply_release_failed(Phase::Releasing) {
            Decision::Ok(phase) => {
                if let Some(invoice) = &trade.payout_invoice {
                    self.db.set_payout(&trade.id, invoice, None)?;
                }
                self.db.set_trade_state(&trade.id, phase.as_str())?;
                let envelope = Envelope::new(NEW_INVOICE)
                    .with_trade(&trade.id)
                    .with_payload(NewInvoicePayload {
                        reason: reason.to_string(),
                    })?;
                Ok(party_replies(trade, envelope))
            }
            other => bail!("invalid payout fail: {other:?}"),
        }
    }

    pub(crate) async fn complete_cancel(
        &self,
        order: &Order,
        trade: &Trade,
    ) -> Result<Vec<Outbound>> {
        self.finish_terminal(order, trade, Phase::Canceled)?;
        if let Err(error) = self.drop_preimage(trade).await {
            warn!(%error, trade_id = %trade.id, "remove preimage");
        }
        let order = self.require_order(&order.id)?;
        let mut outbound = party_replies(trade, Envelope::new(CANCELED).with_trade(&trade.id));
        outbound.insert(0, Outbound::PublicOrder(order.public()));
        Ok(outbound)
    }

    fn preimage_pending(&self, trade: &Trade) -> Result<bool> {
        let Some(hash) = trade.hold_payment_hash.as_deref() else {
            return Ok(false);
        };
        Ok(self.db.get_preimage(hash)?.is_some())
    }

    pub(crate) fn finish_terminal(&self, order: &Order, trade: &Trade, phase: Phase) -> Result<()> {
        let restore = phase.returns_slice().then_some(trade.shannons);
        self.db
            .finish_trade(&order.id, &trade.id, phase.as_str(), restore)
    }

    pub(crate) async fn refund_due_outbound(&self, trade: &Trade) -> Result<Option<Vec<Outbound>>> {
        let outbound = self.refund_if_due(trade).await?;
        if outbound.is_empty() {
            Ok(None)
        } else {
            Ok(Some(outbound))
        }
    }

    pub(crate) fn trade_actor(&self, trade: &Trade, sender: &str) -> Actor {
        trade_actor(
            &trade.seller_nostr,
            &trade.buyer_nostr,
            self.solver.as_deref(),
            sender,
        )
    }

    pub(crate) fn require_order(&self, id: &str) -> Result<Order> {
        self.db
            .get_order(id)?
            .ok_or_else(|| anyhow!("order {id} not found"))
    }

    fn spawn_poll(
        &self,
        kind: PollKind,
        client: Client,
        keys: Keys,
        trade_id: String,
        polling: Arc<Mutex<HashSet<String>>>,
    ) {
        let key = match kind {
            PollKind::Hold => format!("hold:{trade_id}"),
            PollKind::Payout => format!("pay:{trade_id}"),
        };
        if !polling.lock().expect("poll").insert(key.clone()) {
            return;
        }
        let engine = self.clone();
        tokio::spawn(async move {
            loop {
                let result = match kind {
                    PollKind::Hold => engine.on_hold_status(&trade_id).await,
                    PollKind::Payout => engine.on_payout_status(&trade_id).await,
                };
                match result {
                    Ok(outbound) => {
                        if !outbound.is_empty() {
                            let _ = publish_outbounds(&client, &keys, &outbound).await;
                        }
                    }
                    Err(error) => match kind {
                        PollKind::Hold | PollKind::Payout => {
                            warn!(%error, trade_id, ?kind, "fiber poll");
                        }
                    },
                }
                let stay = match engine.db.get_trade(&trade_id) {
                    Ok(Some(trade)) => match (kind, trade.phase().ok()) {
                        (PollKind::Hold, Some(phase))
                            if phase.watched() || phase == Phase::WaitingInvoice =>
                        {
                            true
                        }
                        (PollKind::Payout, Some(Phase::Releasing | Phase::AwaitingInvoice)) => true,
                        _ => false,
                    },
                    _ => false,
                };
                if !stay {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(FIBER_POLL_SECS)).await;
            }
            polling.lock().expect("poll").remove(&key);
        });
    }
}

pub(crate) fn party_replies(trade: &Trade, envelope: Envelope) -> Vec<Outbound> {
    vec![
        Outbound::Reply {
            to: trade.seller_nostr.clone(),
            envelope: envelope.clone(),
        },
        Outbound::Reply {
            to: trade.buyer_nostr.clone(),
            envelope,
        },
    ]
}

pub(crate) fn cant_do(to: &str, trade_id: Option<String>, reason: &str) -> Outbound {
    let mut envelope = Envelope::new(CANT_DO);
    envelope.trade_id = trade_id;
    envelope.payload = serde_json::to_value(CantDoPayload {
        reason: reason.to_string(),
    })
    .ok();
    Outbound::Reply {
        to: to.to_string(),
        envelope,
    }
}

fn pay_invoice_replies(
    trade: &Trade,
    order: &Order,
    invoice: String,
    created_at: i64,
) -> Result<Vec<Outbound>> {
    let pay = Envelope::new(PAY_INVOICE)
        .with_trade(&trade.id)
        .with_payload(PayInvoicePayload {
            invoice,
            amount_shannons: trade.shannons.to_string(),
            order_id: order.id.clone(),
            seller_nostr: trade.seller_nostr.clone(),
            buyer_nostr: trade.buyer_nostr.clone(),
            lock_by: Some(created_at + INVOICE_EXPIRY_SECS as i64),
            hold_ends_at: Some(created_at + order.hold_secs as i64),
        })?;
    Ok(vec![
        Outbound::PublicOrder(order.public()),
        Outbound::Reply {
            to: trade.seller_nostr.clone(),
            envelope: pay.clone(),
        },
        Outbound::Reply {
            to: trade.buyer_nostr.clone(),
            envelope: pay,
        },
    ])
}

fn same_hash(left: &str, right: &str) -> bool {
    left.trim().eq_ignore_ascii_case(right.trim())
}

fn payment_missing(error: &anyhow::Error) -> bool {
    let text = error.to_string().to_ascii_lowercase();
    text.contains("not found") || text.contains("no such")
}

fn payout_window_closed(trade: &Trade) -> bool {
    trade
        .route_by()
        .is_some_and(|deadline| unix_now() >= deadline)
}

const PAYOUT_NO_ROUTE: &str = "no route to the payout invoice. The wallet has to be able to receive, then send this invoice again. Do not send fiat until the trade asks for it";

const PAYOUT_REUSED_INVOICE: &str = "this payout invoice was already used on this hub. Create a fresh invoice with a new payment hash, then send it again";

const FIAT_OWED_REASON: &str = "The buyer had already paid. The CKB is returning to the seller, who owes that fiat back. This desk cannot move the fiat.";

const BUYER_UNPAID_REASON: &str =
    "The solver found the buyer did not pay. The CKB is returning to the seller.";

const CHAT_KEY_REQUIRED: &str = "send this trade's chat to the solver. The receipt cannot be read without it, so the solver cannot pay the buyer";

const FIAT_WINDOW_CLOSED: &str = "the fiat window has closed. It has to leave 2 hours for the seller to release and 4 hours for the solver before Fiber can no longer start the payout";

pub(crate) const WINDOW_ENDED: &str = "the solver window has ended. The CKB returns to the seller";

pub(crate) const RECORD_ALREADY: &str = "this trade already has a public record";

/// Release stops once the 6-hour window has ended, or once a public record is stored.
pub(crate) fn release_refusal(trade: &Trade, now: i64) -> Option<&'static str> {
    if trade
        .solver_deadline()
        .is_some_and(|deadline| now >= deadline)
    {
        return Some(WINDOW_ENDED);
    }
    if trade.outcome.is_some() {
        return Some(RECORD_ALREADY);
    }
    None
}

enum PayoutProbe {
    Ready { tlc_expiry_limit_ms: u64 },
    NoRoute(String),
    SessionExists(String),
    InvoiceExpired,
    WindowClosed,
    Later,
}

fn probe_from_failure(failed_error: Option<&str>) -> PayoutProbe {
    match classify_send_failure(failed_error) {
        SendFailureKind::InvoiceExpired => PayoutProbe::InvoiceExpired,
        SendFailureKind::SessionExists => PayoutProbe::SessionExists(PAYOUT_REUSED_INVOICE.into()),
        SendFailureKind::NoRoute | SendFailureKind::ExpiryLimited => {
            PayoutProbe::NoRoute(PAYOUT_NO_ROUTE.into())
        }
        SendFailureKind::AttemptsInflight | SendFailureKind::Other => {
            if let Some(error) = failed_error {
                warn!(%error, "dry run failed");
            }
            PayoutProbe::Later
        }
    }
}

fn probe_from_error(error: &anyhow::Error) -> PayoutProbe {
    let text = error.to_string();
    match classify_send_failure(Some(&text)) {
        SendFailureKind::InvoiceExpired => PayoutProbe::InvoiceExpired,
        SendFailureKind::SessionExists => PayoutProbe::SessionExists(PAYOUT_REUSED_INVOICE.into()),
        SendFailureKind::NoRoute | SendFailureKind::ExpiryLimited => {
            PayoutProbe::NoRoute(PAYOUT_NO_ROUTE.into())
        }
        SendFailureKind::AttemptsInflight => PayoutProbe::Later,
        SendFailureKind::Other if route_not_ready(error) => {
            PayoutProbe::NoRoute(PAYOUT_NO_ROUTE.into())
        }
        SendFailureKind::Other => {
            warn!(%error, "dry run could not be checked");
            PayoutProbe::Later
        }
    }
}

/// The solver window ended after fiat was marked sent, and nobody decided.
fn unresolved_due(trade: &Trade, now: i64) -> bool {
    if trade.fiat_sent_at.is_none() || trade.fiat_acknowledged || trade.outcome.is_some() {
        return false;
    }
    let Some(deadline) = trade.solver_deadline() else {
        return false;
    };
    if now < deadline {
        return false;
    }
    matches!(
        trade.phase().ok(),
        Some(
            Phase::FiatSent
                | Phase::AwaitingInvoice
                | Phase::Disputed
                | Phase::Refunding
                | Phase::Expired
        )
    )
}

fn refund_reason(reason: Option<&str>, fiat_acknowledged: bool) -> Option<String> {
    match (reason, fiat_acknowledged) {
        (Some(existing), true) => Some(format!("{existing} {FIAT_OWED_REASON}")),
        (None, true) => Some(FIAT_OWED_REASON.to_string()),
        (Some(existing), false) => Some(existing.to_string()),
        (None, false) => None,
    }
}

const PAYOUT_INVOICE_EXPIRED: &str = "the payout invoice expired. The wallet cannot replace that payment hash, so the hold is returning the CKB to the seller";

fn payout_invoice_ok(
    parsed: &ParsedInvoice,
    currency: &str,
    shannons: u128,
) -> Result<(), &'static str> {
    if parsed.currency != currency {
        return Err("payout invoice currency does not match this node");
    }
    match parsed.amount {
        Some(amount) if amount == shannons => Ok(()),
        Some(_) => Err("payout invoice amount does not match the trade"),
        None => Err("payout invoice amount is required"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::dispute::sample_solver_evidence;
    use crate::fiber::{
        FiberNodeInfo, FiberRpc, InboundHold, InvoiceCreated, InvoiceInfo, InvoiceRpc, NodeRpc,
        ParsedInvoice, PaymentInfo, PaymentRpc, WatchtowerRpc,
    };
    use crate::util::support::{
        TestContext, cant_do_reason, fiat_sent, open_order, outbound_replies, replies_contain,
        take, wait_invoice_status,
    };
    use crate::{
        CANCEL, CANCELED, DISPUTE, DISPUTED, DisputePayload, EXPIRED, FIAT_SENT, FIAT_SENT_OK,
        FiatSentPayload, NEW_INVOICE, NEW_ORDER, NewInvoicePayload, NewOrderPayload, OrderStatus,
        PAY_INVOICE, REFUNDING, RELEASE, RESOLVE, ResolvePayload, SETTLED, TAKE, TakePayload,
        WAITING_FIAT, WaitingFiatPayload,
    };
    use async_trait::async_trait;
    use std::sync::Mutex as StdMutex;

    struct FakeFiber {
        status: StdMutex<String>,
        removed: StdMutex<bool>,
        preimages: StdMutex<HashSet<String>>,
        payment_status: StdMutex<String>,
        payment_preimage: StdMutex<Option<String>>,
        payment_failed_error: StdMutex<Option<String>>,
        parsed_amount: StdMutex<u128>,
        invoice_algorithm: StdMutex<HashAlgorithm>,
        hold_algorithm: StdMutex<HashAlgorithm>,
        get_payment_missing: StdMutex<bool>,
        get_payment_missing_times: StdMutex<u32>,
        send_count: StdMutex<u32>,
        send_status: StdMutex<String>,
        send_failed_error: StdMutex<Option<String>>,
        send_error: StdMutex<Option<String>>,
        inbound_missing: StdMutex<bool>,
        inbound_expiry: StdMutex<Option<i64>>,
        inbound_claim_margin_ms: StdMutex<Option<u64>>,
        last_tlc_expiry_limit: StdMutex<Option<u64>>,
        final_delta_ms: StdMutex<u64>,
        expires_at_ms: StdMutex<Option<i64>>,
        probe_error: StdMutex<Option<String>>,
        probe_preimage: StdMutex<bool>,
    }

    impl FakeFiber {
        fn new(status: &str, parsed_amount: u128) -> Self {
            Self {
                status: StdMutex::new(status.to_string()),
                removed: StdMutex::new(false),
                preimages: StdMutex::new(HashSet::new()),
                payment_status: StdMutex::new("Inflight".to_string()),
                payment_preimage: StdMutex::new(None),
                payment_failed_error: StdMutex::new(None),
                parsed_amount: StdMutex::new(parsed_amount),
                invoice_algorithm: StdMutex::new(HashAlgorithm::CkbHash),
                hold_algorithm: StdMutex::new(HashAlgorithm::CkbHash),
                get_payment_missing: StdMutex::new(false),
                get_payment_missing_times: StdMutex::new(0),
                send_count: StdMutex::new(0),
                send_status: StdMutex::new("Success".into()),
                send_failed_error: StdMutex::new(None),
                send_error: StdMutex::new(None),
                inbound_missing: StdMutex::new(false),
                inbound_expiry: StdMutex::new(None),
                inbound_claim_margin_ms: StdMutex::new(None),
                last_tlc_expiry_limit: StdMutex::new(None),
                // Short lock so a 36h hold still fits the published 2h + 4h windows in tests.
                final_delta_ms: StdMutex::new(crate::MIN_TLC_EXPIRY_LIMIT_MS),
                expires_at_ms: StdMutex::new(None),
                probe_error: StdMutex::new(None),
                probe_preimage: StdMutex::new(false),
            }
        }
    }

    #[async_trait]
    impl InvoiceRpc for FakeFiber {
        async fn new_hold_invoice(
            &self,
            _: u128,
            payment_hash: &str,
            hash_algorithm: HashAlgorithm,
            _: &str,
            _: u64,
            _: u64,
        ) -> Result<InvoiceCreated> {
            *self.hold_algorithm.lock().expect("hold algorithm") = hash_algorithm;
            Ok(InvoiceCreated {
                invoice: format!("hold-{payment_hash}"),
                payment_hash: payment_hash.to_string(),
            })
        }
        async fn new_payout_invoice(&self, _: u128, _: &str, _: u64) -> Result<InvoiceCreated> {
            bail!("unused")
        }
        async fn get_invoice(&self, payment_hash: &str) -> Result<InvoiceInfo> {
            Ok(InvoiceInfo {
                invoice: String::new(),
                payment_hash: payment_hash.to_string(),
                status: self.status.lock().expect("status").clone(),
            })
        }
        async fn cancel_invoice(&self, _: &str) -> Result<InvoiceInfo> {
            bail!("unused")
        }
        async fn settle_invoice(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        async fn parse_invoice(&self, invoice: &str) -> Result<ParsedInvoice> {
            if invoice.starts_with("unsupported-") {
                bail!("unsupported hash algorithm blake2b");
            }
            let hash_algorithm = if invoice.starts_with("hold-") {
                *self.hold_algorithm.lock().expect("hold algorithm")
            } else {
                *self.invoice_algorithm.lock().expect("algorithm")
            };
            Ok(ParsedInvoice {
                payment_hash: "hash".into(),
                currency: "Fibt".into(),
                amount: Some(*self.parsed_amount.lock().expect("amount")),
                hash_algorithm,
                final_tlc_expiry_delta_ms: *self.final_delta_ms.lock().expect("final delta"),
                expires_at_ms: *self.expires_at_ms.lock().expect("expires"),
            })
        }
    }

    #[async_trait]
    impl WatchtowerRpc for FakeFiber {
        async fn create_preimage(&self, payment_hash: &str, _: &str) -> Result<()> {
            self.preimages
                .lock()
                .expect("preimages")
                .insert(payment_hash.to_string());
            Ok(())
        }
        async fn remove_preimage(&self, payment_hash: &str) -> Result<()> {
            self.preimages
                .lock()
                .expect("preimages")
                .remove(payment_hash);
            *self.removed.lock().expect("removed") = true;
            Ok(())
        }
    }

    #[async_trait]
    impl PaymentRpc for FakeFiber {
        async fn probe_payment(&self, _: &str, _: u64) -> Result<PaymentInfo> {
            if let Some(error) = self.probe_error.lock().expect("probe").clone() {
                bail!("{error}");
            }
            Ok(PaymentInfo {
                payment_hash: "hash".into(),
                status: "Created".into(),
                preimage: self
                    .probe_preimage
                    .lock()
                    .expect("probe preimage")
                    .then(|| "preimage".to_string()),
                failed_error: None,
            })
        }
        async fn send_payment(&self, _: &str, tlc_expiry_limit_ms: u64) -> Result<PaymentInfo> {
            *self.send_count.lock().expect("send") += 1;
            *self.last_tlc_expiry_limit.lock().expect("limit") = Some(tlc_expiry_limit_ms);
            if let Some(error) = self.send_error.lock().expect("send error").clone() {
                bail!("{error}");
            }
            let status = self.send_status.lock().expect("send status").clone();
            Ok(PaymentInfo {
                payment_hash: "hash".into(),
                preimage: (status == "Success").then(|| "preimage".to_string()),
                failed_error: self.send_failed_error.lock().expect("send failed").clone(),
                status,
            })
        }
        async fn get_payment(&self, payment_hash: &str) -> Result<PaymentInfo> {
            let mut times = self
                .get_payment_missing_times
                .lock()
                .expect("missing times");
            if *times > 0 {
                *times -= 1;
                bail!("payment not found");
            }
            if *self.get_payment_missing.lock().expect("missing") {
                bail!("payment not found");
            }
            Ok(PaymentInfo {
                payment_hash: payment_hash.to_string(),
                status: self.payment_status.lock().expect("payment").clone(),
                preimage: self.payment_preimage.lock().expect("preimage").clone(),
                failed_error: self.payment_failed_error.lock().expect("failed").clone(),
            })
        }
    }

    #[async_trait]
    impl NodeRpc for FakeFiber {
        async fn node_info(&self) -> Result<FiberNodeInfo> {
            Ok(FiberNodeInfo {
                pubkey: "fiber-hub".into(),
                addresses: Vec::new(),
            })
        }

        async fn inbound_hold(&self, _: &str) -> Result<Option<InboundHold>> {
            if *self.inbound_missing.lock().expect("missing tlc") {
                return Ok(None);
            }
            Ok(Some(InboundHold {
                expires_at_ms: self
                    .inbound_expiry
                    .lock()
                    .expect("expiry")
                    .unwrap_or_else(|| unix_now_ms() + crate::HOLD_SECS as i64 * 1000),
                claim_margin_ms: self
                    .inbound_claim_margin_ms
                    .lock()
                    .expect("claim margin")
                    .unwrap_or(crate::CLAIM_MARGIN_MS),
            }))
        }
    }

    impl FiberRpc for FakeFiber {
        fn currency(&self) -> &str {
            "Fibt"
        }
    }

    fn fake_engine(status: &str) -> (Engine<FakeFiber>, Arc<FakeFiber>) {
        let fiber = Arc::new(FakeFiber::new(status, 100_000_000));
        fiber
            .preimages
            .lock()
            .expect("preimages")
            .insert("hash".into());
        let db = Arc::new(Db::open_in_memory().unwrap());
        db.insert_order(&Order {
            id: "order".into(),
            side: "sell".into(),
            maker_nostr: "seller".into(),
            maker_fiber: "fiber-seller".into(),
            available_shannons: 1_000_000_000,
            fiat_currency: "NGN".into(),
            price_per_ckb: "1000".into(),
            min: "1000".into(),
            max: "10000".into(),
            status: OrderStatus::Open.as_str().into(),
            hold_secs: crate::HOLD_SECS,
            reserved_shannons: 0,
        })
        .unwrap();
        db.insert_trade(&Trade {
            id: "trade".into(),
            order_id: "order".into(),
            seller_nostr: "seller".into(),
            seller_fiber: "fiber-seller".into(),
            buyer_nostr: "buyer".into(),
            buyer_fiber: "fiber-buyer".into(),
            fiat_amount: "1000".into(),
            shannons: 100_000_000,
            state: Phase::WaitingHold.as_str().into(),
            hold_payment_hash: Some("hash".into()),
            hold_invoice: Some("inv".into()),
            payout_invoice: Some("buyer-invoice".into()),
            payout_payment_hash: None,
            hold_received_at: None,
            hold_created_at: Some(unix_now()),
            buyer_fiber_addr: None,
            hold_expires_at_ms: None,
            claim_margin_ms: None,
            final_tlc_expiry_delta_ms: Some(crate::MIN_TLC_EXPIRY_LIMIT_MS),
            fiat_acknowledged: false,
            fiat_sent_at: None,
            chat_key_shared: false,
            release_prompted: false,
            outcome: None,
        })
        .unwrap();
        db.insert_preimage("hash", "preimage").unwrap();
        let engine = Engine::new(db, fiber.clone(), Some("solver".into()));
        (engine, fiber)
    }

    #[tokio::test]
    async fn fiat_window_refunds_without_revealing_preimage() {
        let (engine, fiber) = fake_engine("Received");
        let waiting = engine.on_hold_status("trade").await.unwrap();
        assert!(replies_contain(&waiting, WAITING_FIAT));
        let details: WaitingFiatPayload = waiting
            .iter()
            .find_map(|item| match item {
                Outbound::Reply { envelope, .. } if envelope.action == WAITING_FIAT => {
                    Some(envelope.decode_payload().unwrap())
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(details.fiat_currency, "NGN");
        assert_eq!(details.reference, "trade");
        assert_eq!(details.seller_nostr, "seller");
        assert_eq!(details.buyer_nostr, "buyer");
        assert!(details.pay_by.is_some());
        assert!(details.release_by.is_none());
        let trade = engine.db.get_trade("trade").unwrap().unwrap();
        assert!(trade.hold_received_at.is_some());
        assert_eq!(details.pay_by, trade.pay_by());
        assert!(details.release_by.is_none());
        engine
            .db
            .set_hold_expires_at_ms("trade", unix_now_ms())
            .unwrap();
        let refunding = engine.on_hold_status("trade").await.unwrap();
        assert!(replies_contain(&refunding, REFUNDING));
        assert!(
            !refunding
                .iter()
                .any(|item| matches!(item, Outbound::TradeRecord(_)))
        );
        assert!(*fiber.removed.lock().unwrap());
        assert!(engine.db.get_preimage("hash").unwrap().is_none());
        assert_eq!(
            engine.db.get_trade("trade").unwrap().unwrap().state,
            Phase::Refunding.as_str()
        );

        *fiber.status.lock().unwrap() = "Expired".into();
        let expired = engine.on_hold_status("trade").await.unwrap();
        assert!(replies_contain(&expired, EXPIRED));
        let order = engine.db.get_order("order").unwrap().unwrap();
        assert_eq!(order.available_shannons, 1_100_000_000);
    }

    #[tokio::test]
    async fn paid_hold_opens_the_fiat_step() {
        let (engine, _) = fake_engine("Paid");
        let waiting = engine.on_hold_status("trade").await.unwrap();
        assert!(replies_contain(&waiting, WAITING_FIAT));
        let trade = engine.db.get_trade("trade").unwrap().unwrap();
        assert_eq!(trade.state, Phase::WaitingFiat.as_str());
        assert!(trade.hold_received_at.is_some());
        assert!(trade.hold_expires_at_ms.is_some());
        assert!(trade.pay_by().is_some());
    }

    #[tokio::test]
    async fn a_paid_hold_without_tlc_does_not_start_the_fiat_window() {
        let (engine, fiber) = fake_engine("Paid");
        *fiber.inbound_missing.lock().unwrap() = true;
        let waiting = engine.on_hold_status("trade").await.unwrap();
        assert!(waiting.is_empty());
        let trade = engine.db.get_trade("trade").unwrap().unwrap();
        assert_eq!(trade.state, Phase::WaitingHold.as_str());
        assert!(trade.hold_expires_at_ms.is_none());
        assert!(trade.pay_by().is_none());
    }

    #[tokio::test]
    async fn a_reused_invoice_session_asks_for_a_fresh_invoice() {
        let (engine, fiber) = fake_engine("Received");
        *fiber.probe_error.lock().unwrap() = Some(
            "Payment session already exists: 0xhash with payment session status: Inflight".into(),
        );
        let blocked = engine.on_hold_status("trade").await.unwrap();
        assert!(replies_contain(&blocked, crate::NEED_ROUTE));
        assert!(!replies_contain(&blocked, WAITING_FIAT));
        let reason = blocked.iter().find_map(|item| match item {
            Outbound::Reply { envelope, .. } if envelope.action == crate::NEED_ROUTE => {
                envelope.decode_payload::<NewInvoicePayload>().ok()
            }
            _ => None,
        });
        assert!(
            reason
                .as_ref()
                .is_some_and(|payload| payload.reason.contains("fresh invoice"))
        );
        assert_eq!(
            engine.db.get_trade("trade").unwrap().unwrap().state,
            Phase::AwaitingRoute.as_str()
        );
    }

    #[tokio::test]
    async fn release_with_existing_session_waits_instead_of_asking_for_a_fresh_invoice() {
        let (engine, fiber) = fake_engine("Received");
        engine
            .db
            .set_trade_state("trade", Phase::Releasing.as_str())
            .unwrap();
        *fiber.get_payment_missing.lock().unwrap() = true;
        *fiber.probe_error.lock().unwrap() = Some(
            "Payment session already exists: 0xhash with payment session status: Inflight".into(),
        );
        let waiting = engine.on_payout_status("trade").await.unwrap();
        assert!(waiting.is_empty());
        assert_eq!(
            engine.db.get_trade("trade").unwrap().unwrap().state,
            Phase::Releasing.as_str()
        );
        assert_eq!(*fiber.send_count.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn release_with_existing_session_settles_when_get_payment_is_live() {
        let (engine, fiber) = fake_engine("Received");
        engine
            .db
            .set_trade_state("trade", Phase::Releasing.as_str())
            .unwrap();
        // Miss once to pass the gate, then return Success for the live session.
        *fiber.get_payment_missing_times.lock().unwrap() = 1;
        *fiber.payment_status.lock().unwrap() = "Success".into();
        *fiber.payment_preimage.lock().unwrap() = Some("preimage".into());
        *fiber.probe_error.lock().unwrap() = Some(
            "Payment session already exists: 0xhash with payment session status: Success".into(),
        );
        let settled = engine.on_payout_status("trade").await.unwrap();
        assert!(replies_contain(&settled, SETTLED));
        assert_eq!(*fiber.send_count.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn inflight_attempts_on_failed_session_wait_for_next_poll() {
        let (engine, fiber) = fake_engine("Received");
        *fiber.probe_error.lock().unwrap() = Some(
            "Payment session 0xhash has attempts that are in flight state, can not retry".into(),
        );
        let waiting = engine.on_hold_status("trade").await.unwrap();
        assert!(waiting.is_empty());
        assert_eq!(
            engine.db.get_trade("trade").unwrap().unwrap().state,
            Phase::WaitingHold.as_str()
        );

        *fiber.probe_error.lock().unwrap() = None;
        engine
            .db
            .set_trade_state("trade", Phase::Releasing.as_str())
            .unwrap();
        *fiber.get_payment_missing.lock().unwrap() = true;
        *fiber.send_status.lock().unwrap() = "Failed".into();
        *fiber.send_failed_error.lock().unwrap() = Some(
            "Payment session 0xhash has attempts that are in flight state, can not retry".into(),
        );
        let releasing = engine.on_payout_status("trade").await.unwrap();
        assert!(releasing.is_empty());
        assert_eq!(
            engine.db.get_trade("trade").unwrap().unwrap().state,
            Phase::Releasing.as_str()
        );
    }

    #[tokio::test]
    async fn a_missing_route_holds_fiat_until_the_same_invoice_routes() {
        let (engine, fiber) = fake_engine("Received");
        *fiber.probe_error.lock().unwrap() =
            Some("send_payment failed: Failed to build route, no path found".into());
        let blocked = engine.on_hold_status("trade").await.unwrap();
        assert!(replies_contain(&blocked, crate::NEED_ROUTE));
        assert!(!replies_contain(&blocked, WAITING_FIAT));
        assert_eq!(*fiber.send_count.lock().unwrap(), 0);
        assert_eq!(
            engine.db.get_trade("trade").unwrap().unwrap().state,
            Phase::AwaitingRoute.as_str()
        );
        let paid = engine
            .handle(
                "buyer",
                Envelope::new(FIAT_SENT)
                    .with_trade("trade")
                    .with_payload(FiatSentPayload {
                        invoice: "buyer-invoice".into(),
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(cant_do_reason(&paid).contains("cannot receive"));
        *fiber.probe_error.lock().unwrap() = None;
        let opened = engine
            .handle(
                "buyer",
                Envelope::new(crate::PAYOUT_INVOICE)
                    .with_trade("trade")
                    .with_payload(crate::PayoutInvoicePayload {
                        invoice: "buyer-invoice".into(),
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(replies_contain(&opened, WAITING_FIAT));
        assert_eq!(
            engine.db.get_trade("trade").unwrap().unwrap().state,
            Phase::WaitingFiat.as_str()
        );
        assert_eq!(*fiber.send_count.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn a_dry_run_preimage_does_not_open_fiat() {
        let (engine, fiber) = fake_engine("Received");
        *fiber.probe_preimage.lock().unwrap() = true;
        let waiting = engine.on_hold_status("trade").await.unwrap();
        assert!(waiting.is_empty());
        assert_eq!(
            engine.db.get_trade("trade").unwrap().unwrap().state,
            Phase::WaitingHold.as_str()
        );
        assert!(engine.db.get_preimage("hash").unwrap().is_some());
    }

    #[tokio::test]
    async fn solver_fiat_owed_refunds_and_records_the_fiat() {
        let (engine, fiber) = fake_engine("Received");
        engine.on_hold_status("trade").await.unwrap();
        engine
            .handle(
                "buyer",
                Envelope::new(FIAT_SENT)
                    .with_trade("trade")
                    .with_payload(FiatSentPayload {
                        invoice: String::new(),
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            !engine
                .db
                .get_trade("trade")
                .unwrap()
                .unwrap()
                .fiat_acknowledged
        );
        engine
            .handle(
                "buyer",
                Envelope::new(DISPUTE)
                    .with_trade("trade")
                    .with_payload(DisputePayload {
                        invoice: None,
                        solver_evidence: vec![sample_solver_evidence()],
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        let resolved = engine
            .handle(
                "solver",
                Envelope::new(RESOLVE)
                    .with_trade("trade")
                    .with_payload(ResolvePayload {
                        winner: "fiat-owed".into(),
                        invoice: None,
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(replies_contain(&resolved, REFUNDING));
        let record = resolved.iter().find_map(|item| match item {
            Outbound::TradeRecord(record) => Some(record),
            _ => None,
        });
        let record = record.expect("fiat-owed record");
        assert_eq!(record.outcome, "fiat-owed");
        assert_eq!(record.seller_nostr, "seller");
        assert_eq!(record.fiat_currency, "NGN");
        let reason = resolved.iter().find_map(|item| match item {
            Outbound::Reply { envelope, .. } if envelope.action == REFUNDING => {
                let payload: NewInvoicePayload = envelope.decode_payload().unwrap();
                Some(payload.reason)
            }
            _ => None,
        });
        assert!(reason.unwrap().contains("owes that fiat"));
        assert!(
            engine
                .db
                .get_trade("trade")
                .unwrap()
                .unwrap()
                .fiat_acknowledged
        );
        assert!(*fiber.removed.lock().unwrap());
        let corrected = engine
            .handle(
                "solver",
                Envelope::new(RESOLVE)
                    .with_trade("trade")
                    .with_payload(ResolvePayload {
                        winner: "seller".into(),
                        invoice: None,
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(cant_do_reason(&corrected).contains("public record"));
        assert!(
            engine
                .db
                .get_trade("trade")
                .unwrap()
                .unwrap()
                .fiat_acknowledged
        );
        assert_eq!(
            engine
                .db
                .get_trade("trade")
                .unwrap()
                .unwrap()
                .outcome
                .as_deref(),
            Some(crate::OUTCOME_FIAT_OWED)
        );
    }

    #[tokio::test]
    async fn a_dispute_before_fiat_still_ends_at_the_fiat_deadline() {
        let (engine, fiber) = fake_engine("Received");
        engine.on_hold_status("trade").await.unwrap();
        engine
            .handle(
                "buyer",
                Envelope::new(DISPUTE)
                    .with_trade("trade")
                    .with_payload(DisputePayload {
                        invoice: None,
                        solver_evidence: Vec::new(),
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        let trade = engine.db.get_trade("trade").unwrap().unwrap();
        let route_by = trade.route_by().expect("route deadline");
        let pay_by = trade.pay_by().expect("fiat deadline");
        assert!(pay_by < route_by);
        let expires_at_ms = unix_now_ms()
            + 3_600_000
            + crate::CLAIM_MARGIN_MS as i64
            + crate::MIN_TLC_EXPIRY_LIMIT_MS as i64
            + crate::FIBER_RANDOM_EXPIRY_RESERVE_MS as i64;
        engine
            .db
            .set_hold_expires_at_ms("trade", expires_at_ms)
            .unwrap();
        let fresh = engine.db.get_trade("trade").unwrap().unwrap();
        assert!(fresh.pay_by().expect("pay") < unix_now());
        assert!(fresh.route_by().expect("route") > unix_now());
        let refunding = engine.on_hold_status("trade").await.unwrap();
        assert!(replies_contain(&refunding, REFUNDING));
        assert_eq!(
            engine.db.get_trade("trade").unwrap().unwrap().state,
            Phase::Refunding.as_str()
        );
        assert!(*fiber.removed.lock().unwrap());
    }

    #[tokio::test]
    async fn fiat_sent_during_a_dispute_starts_the_release_clock() {
        let (engine, _) = fake_engine("Received");
        engine.on_hold_status("trade").await.unwrap();
        engine
            .handle(
                "buyer",
                Envelope::new(DISPUTE)
                    .with_trade("trade")
                    .with_payload(DisputePayload {
                        invoice: None,
                        solver_evidence: vec![sample_solver_evidence()],
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        let sent = engine
            .handle(
                "buyer",
                Envelope::new(FIAT_SENT)
                    .with_trade("trade")
                    .with_payload(FiatSentPayload {
                        invoice: String::new(),
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(replies_contain(&sent, FIAT_SENT_OK));
        let payload = sent.iter().find_map(|item| match item {
            Outbound::Reply { envelope, .. } if envelope.action == FIAT_SENT_OK => {
                let body: crate::FiatSentOkPayload = envelope.decode_payload().unwrap();
                Some(body)
            }
            _ => None,
        });
        let payload = payload.expect("fiat-sent payload");
        let trade = engine.db.get_trade("trade").unwrap().unwrap();
        assert_eq!(trade.state, Phase::Disputed.as_str());
        let sent_at = trade.fiat_sent_at.expect("fiat sent at");
        assert_eq!(payload.release_by, sent_at + crate::RELEASE_GRACE_SECS);
        assert_eq!(payload.solver_window_secs, crate::SOLVER_WINDOW_SECS);
        assert!(!trade.fiat_acknowledged);
    }

    #[tokio::test]
    async fn release_then_expiry_records_fiat_owed() {
        let (engine, fiber) = fake_engine("Received");
        engine.on_hold_status("trade").await.unwrap();
        engine
            .handle(
                "buyer",
                Envelope::new(FIAT_SENT)
                    .with_trade("trade")
                    .with_payload(FiatSentPayload {
                        invoice: String::new(),
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        engine
            .handle("seller", Envelope::new(RELEASE).with_trade("trade"))
            .await
            .unwrap();
        assert!(
            engine
                .db
                .get_trade("trade")
                .unwrap()
                .unwrap()
                .fiat_acknowledged
        );
        engine
            .db
            .set_hold_expires_at_ms("trade", unix_now_ms())
            .unwrap();
        *fiber.get_payment_missing.lock().unwrap() = true;
        let refunding = engine.on_payout_status("trade").await.unwrap();
        assert!(replies_contain(&refunding, REFUNDING));
        assert!(
            refunding
                .iter()
                .any(|item| matches!(item, Outbound::TradeRecord(_)))
        );
        assert_eq!(*fiber.send_count.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn solver_seller_win_refunds_and_buyer_win_needs_invoice() {
        let (engine, fiber) = fake_engine("Received");
        engine.on_hold_status("trade").await.unwrap();
        let disputed = engine
            .handle(
                "buyer",
                Envelope::new(DISPUTE)
                    .with_trade("trade")
                    .with_payload(DisputePayload {
                        invoice: None,
                        solver_evidence: vec![sample_solver_evidence()],
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(replies_contain(&disputed, DISPUTED));
        let evidence = sample_solver_evidence();
        let solver_payload = disputed.iter().find_map(|item| match item {
            Outbound::Reply { to, envelope } if to == "solver" && envelope.action == DISPUTED => {
                envelope.payload.clone()
            }
            _ => None,
        });
        let disclosure = solver_payload
            .as_ref()
            .and_then(|payload| payload.get("disclosures"))
            .and_then(|value| value.as_array())
            .and_then(|rows| rows.first());
        assert_eq!(
            disclosure
                .and_then(|row| row.get("from"))
                .and_then(|value| value.as_str()),
            Some("buyer")
        );
        assert_eq!(
            disclosure
                .and_then(|row| row.get("evidence"))
                .and_then(|value| value.as_array())
                .and_then(|blobs| blobs.first())
                .and_then(|blob| blob.as_str()),
            Some(evidence.as_str())
        );
        assert!(solver_payload.unwrap().get("conversation_key").is_none());
        assert!(engine.db.evidence_is_pending("trade").unwrap());
        let pending = engine.pending_evidence_outbounds().unwrap();
        assert_eq!(pending.len(), 1);
        engine.note_evidence_delivered(&disputed).unwrap();
        assert!(!engine.db.evidence_is_pending("trade").unwrap());
        assert!(engine.pending_evidence_outbounds().unwrap().is_empty());
        let buyer_payload = disputed.iter().find_map(|item| match item {
            Outbound::Reply { to, envelope } if to == "buyer" && envelope.action == DISPUTED => {
                envelope.payload.clone()
            }
            _ => None,
        });
        assert_eq!(
            buyer_payload
                .as_ref()
                .and_then(|payload| payload.get("solver"))
                .and_then(|value| value.as_str()),
            Some("solver")
        );
        assert!(buyer_payload.unwrap().get("disclosures").is_none());
        engine.db.set_payout("trade", "", None).unwrap();
        let missing = engine
            .handle(
                "solver",
                Envelope::new(RESOLVE)
                    .with_trade("trade")
                    .with_payload(ResolvePayload {
                        winner: "buyer".into(),
                        invoice: None,
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        engine
            .db
            .set_payout("trade", "buyer-invoice", None)
            .unwrap();
        assert!(cant_do_reason(&missing).contains("payout invoice"));
        let sneaky = engine
            .handle(
                "buyer",
                Envelope::new(RESOLVE)
                    .with_trade("trade")
                    .with_payload(ResolvePayload {
                        winner: "buyer".into(),
                        invoice: Some("evil".into()),
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(cant_do_reason(&sneaky).contains("solver"));
        assert_eq!(
            engine
                .db
                .get_trade("trade")
                .unwrap()
                .unwrap()
                .payout_invoice
                .as_deref(),
            Some("buyer-invoice")
        );
        let seller = engine
            .handle(
                "solver",
                Envelope::new(RESOLVE)
                    .with_trade("trade")
                    .with_payload(ResolvePayload {
                        winner: "seller".into(),
                        invoice: None,
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(replies_contain(&seller, REFUNDING));
        let record = seller.iter().find_map(|item| match item {
            Outbound::TradeRecord(record) => Some(record),
            _ => None,
        });
        assert_eq!(record.expect("buyer-unpaid record").outcome, "buyer-unpaid");
        assert!(*fiber.removed.lock().unwrap());
        assert_eq!(
            engine.db.get_trade("trade").unwrap().unwrap().state,
            Phase::Refunding.as_str()
        );
        assert!(!fiber.preimages.lock().unwrap().contains("hash"));
    }

    #[tokio::test]
    async fn fiat_sent_after_the_deadline_refunds_without_a_fiat_record() {
        let (engine, _) = fake_engine("Received");
        engine.on_hold_status("trade").await.unwrap();
        engine
            .db
            .set_hold_expires_at_ms("trade", unix_now_ms())
            .unwrap();
        let refunding = engine
            .handle(
                "buyer",
                Envelope::new(FIAT_SENT)
                    .with_trade("trade")
                    .with_payload(FiatSentPayload {
                        invoice: String::new(),
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(replies_contain(&refunding, REFUNDING));
        let reason = refunding.iter().find_map(|item| match item {
            Outbound::Reply { envelope, .. } if envelope.action == REFUNDING => {
                let payload: NewInvoicePayload = envelope.decode_payload().unwrap();
                Some(payload.reason)
            }
            _ => None,
        });
        assert!(reason.unwrap().contains("2 hours"));
        assert!(
            !refunding
                .iter()
                .any(|item| matches!(item, Outbound::TradeRecord(_)))
        );
    }

    #[tokio::test]
    async fn release_grace_asks_the_buyer_to_dispute_once() {
        let (engine, _) = fake_engine("Received");
        engine.on_hold_status("trade").await.unwrap();
        let sent = engine
            .handle(
                "buyer",
                Envelope::new(FIAT_SENT)
                    .with_trade("trade")
                    .with_payload(FiatSentPayload {
                        invoice: String::new(),
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        let payload: crate::FiatSentOkPayload = sent
            .iter()
            .find_map(|item| match item {
                Outbound::Reply { envelope, .. } if envelope.action == FIAT_SENT_OK => {
                    Some(envelope.decode_payload().unwrap())
                }
                _ => None,
            })
            .unwrap();
        let trade = engine.db.get_trade("trade").unwrap().unwrap();
        let sent_at = trade.fiat_sent_at.expect("fiat sent");
        assert_eq!(payload.release_by, sent_at + crate::RELEASE_GRACE_SECS);
        assert_eq!(payload.solver_window_secs, crate::SOLVER_WINDOW_SECS);
        assert!(engine.on_hold_status("trade").await.unwrap().is_empty());
        engine
            .db
            .set_fiat_sent_at("trade", unix_now() - crate::RELEASE_GRACE_SECS - 1)
            .unwrap();
        let due = engine.on_hold_status("trade").await.unwrap();
        assert!(replies_contain(&due, crate::RELEASE_DUE));
        assert_eq!(
            engine.db.get_trade("trade").unwrap().unwrap().state,
            Phase::FiatSent.as_str()
        );
        assert!(engine.on_hold_status("trade").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn waiting_fiat_is_not_probed_until_the_buyer_marks_sent() {
        let (engine, fiber) = fake_engine("Received");
        engine.on_hold_status("trade").await.unwrap();
        *fiber.probe_error.lock().unwrap() =
            Some("send_payment failed: Failed to build route, no path found".into());
        assert!(engine.on_hold_status("trade").await.unwrap().is_empty());
        assert_eq!(
            engine.db.get_trade("trade").unwrap().unwrap().state,
            Phase::WaitingFiat.as_str()
        );
        let sent = engine
            .handle(
                "buyer",
                Envelope::new(FIAT_SENT)
                    .with_trade("trade")
                    .with_payload(FiatSentPayload {
                        invoice: String::new(),
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(replies_contain(&sent, FIAT_SENT_OK));
        assert!(replies_contain(&sent, NEW_INVOICE));
        assert!(cant_do_reason(&sent).is_empty());
        assert_eq!(*fiber.send_count.lock().unwrap(), 0);
        let trade = engine.db.get_trade("trade").unwrap().unwrap();
        assert_eq!(trade.state, Phase::AwaitingInvoice.as_str());
        assert!(trade.fiat_sent_at.is_some());
        assert!(!trade.fiat_acknowledged);

        // Restoring the route before seller release returns to fiat-sent instead of paying out.
        *fiber.probe_error.lock().unwrap() = None;
        let retried = engine
            .handle(
                "buyer",
                Envelope::new(FIAT_SENT)
                    .with_trade("trade")
                    .with_payload(FiatSentPayload {
                        invoice: "buyer-invoice".into(),
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(replies_contain(&retried, FIAT_SENT_OK));
        assert!(!replies_contain(&retried, NEW_INVOICE));
        let trade = engine.db.get_trade("trade").unwrap().unwrap();
        assert_eq!(trade.state, Phase::FiatSent.as_str());
        assert!(!trade.fiat_acknowledged);
        assert_eq!(*fiber.send_count.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn seller_can_release_after_post_fiat_route_loss() {
        let (engine, fiber) = fake_engine("Received");
        engine.on_hold_status("trade").await.unwrap();
        *fiber.probe_error.lock().unwrap() =
            Some("send_payment failed: Failed to build route, no path found".into());
        engine
            .handle(
                "buyer",
                Envelope::new(FIAT_SENT)
                    .with_trade("trade")
                    .with_payload(FiatSentPayload {
                        invoice: String::new(),
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            engine.db.get_trade("trade").unwrap().unwrap().state,
            Phase::AwaitingInvoice.as_str()
        );
        // Seller can still release while awaiting-invoice; once the route is back, payout settles.
        *fiber.probe_error.lock().unwrap() = None;
        *fiber.get_payment_missing.lock().unwrap() = true;
        let released = engine
            .handle("seller", Envelope::new(RELEASE).with_trade("trade"))
            .await
            .unwrap();
        assert!(cant_do_reason(&released).is_empty());
        let trade = engine.db.get_trade("trade").unwrap().unwrap();
        assert_eq!(trade.state, Phase::Releasing.as_str());
        assert!(trade.fiat_acknowledged);
        let settled = engine.on_payout_status("trade").await.unwrap();
        assert!(replies_contain(&settled, SETTLED));
    }

    #[tokio::test]
    async fn a_missed_solver_window_publishes_unresolved_and_still_refunds() {
        let (engine, fiber) = fake_engine("Received");
        engine.on_hold_status("trade").await.unwrap();
        engine
            .handle(
                "buyer",
                Envelope::new(FIAT_SENT)
                    .with_trade("trade")
                    .with_payload(FiatSentPayload {
                        invoice: String::new(),
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        engine
            .db
            .set_fiat_sent_at(
                "trade",
                unix_now() - crate::RELEASE_GRACE_SECS - crate::SOLVER_WINDOW_SECS - 1,
            )
            .unwrap();
        engine.db.set_release_prompted("trade").unwrap();
        let noted = engine.on_hold_status("trade").await.unwrap();
        let record = noted.iter().find_map(|item| match item {
            Outbound::TradeRecord(record) => Some(record.outcome.as_str()),
            _ => None,
        });
        assert_eq!(record, Some(crate::OUTCOME_UNRESOLVED));
        assert!(replies_contain(&noted, crate::TRADE_RECORD));
        assert!(!replies_contain(&noted, REFUNDING));
        assert_eq!(
            engine.db.get_trade("trade").unwrap().unwrap().state,
            Phase::FiatSent.as_str()
        );
        assert_eq!(
            engine
                .db
                .get_trade("trade")
                .unwrap()
                .unwrap()
                .outcome
                .as_deref(),
            Some(crate::OUTCOME_UNRESOLVED)
        );
        let late_release = engine
            .handle("seller", Envelope::new(RELEASE).with_trade("trade"))
            .await
            .unwrap();
        assert!(cant_do_reason(&late_release).contains("solver window"));
        assert_eq!(
            engine.db.get_trade("trade").unwrap().unwrap().state,
            Phase::FiatSent.as_str()
        );
        assert!(
            !engine
                .db
                .get_trade("trade")
                .unwrap()
                .unwrap()
                .fiat_acknowledged
        );
        let late_decision = engine
            .handle(
                "solver",
                Envelope::new(RESOLVE)
                    .with_trade("trade")
                    .with_payload(ResolvePayload {
                        winner: "buyer".into(),
                        invoice: None,
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(cant_do_reason(&late_decision).contains("public record"));
        assert!(engine.on_hold_status("trade").await.unwrap().is_empty());

        engine
            .db
            .set_hold_expires_at_ms("trade", unix_now_ms())
            .unwrap();
        let refunding = engine.on_hold_status("trade").await.unwrap();
        assert!(replies_contain(&refunding, REFUNDING));
        assert!(
            !refunding
                .iter()
                .any(|item| matches!(item, Outbound::TradeRecord(_)))
        );
        assert_eq!(*fiber.send_count.lock().unwrap(), 0);
        assert!(*fiber.removed.lock().unwrap());
    }

    #[tokio::test]
    async fn buyer_award_without_the_chat_key_is_refused() {
        let (engine, fiber) = fake_engine("Received");
        engine.on_hold_status("trade").await.unwrap();
        engine
            .handle(
                "buyer",
                Envelope::new(FIAT_SENT)
                    .with_trade("trade")
                    .with_payload(FiatSentPayload {
                        invoice: String::new(),
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        engine
            .handle(
                "buyer",
                Envelope::new(DISPUTE)
                    .with_trade("trade")
                    .with_payload(DisputePayload {
                        invoice: None,
                        solver_evidence: Vec::new(),
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        let refused = engine
            .handle(
                "solver",
                Envelope::new(RESOLVE)
                    .with_trade("trade")
                    .with_payload(ResolvePayload {
                        winner: "buyer".into(),
                        invoice: None,
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(cant_do_reason(&refused).contains("trade's chat"));
        assert!(!*fiber.removed.lock().unwrap());
        let mut legacy = Envelope::new(DISPUTE).with_trade("trade");
        legacy.payload = Some(serde_json::json!({
            "conversation_key": "ab".repeat(32),
        }));
        let legacy = engine.handle("buyer", legacy).await.unwrap();
        assert!(cant_do_reason(&legacy).contains("trade's chat"));
        let garbage = engine
            .handle(
                "buyer",
                Envelope::new(DISPUTE)
                    .with_trade("trade")
                    .with_payload(DisputePayload {
                        invoice: None,
                        solver_evidence: vec!["not-a-nip44-message".into()],
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(cant_do_reason(&garbage).contains("NIP-44"));
        assert!(
            !engine
                .db
                .get_trade("trade")
                .unwrap()
                .unwrap()
                .chat_key_shared
        );
        let sealed = engine
            .handle(
                "buyer",
                Envelope::new(DISPUTE)
                    .with_trade("trade")
                    .with_payload(DisputePayload {
                        invoice: None,
                        solver_evidence: vec![sample_solver_evidence()],
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(sealed.iter().any(|item| match item {
            Outbound::Reply { to, envelope } => {
                to == "solver"
                    && envelope
                        .payload
                        .as_ref()
                        .is_some_and(|payload| payload.get("disclosures").is_some())
            }
            _ => false,
        }));
        assert!(
            engine
                .db
                .get_trade("trade")
                .unwrap()
                .unwrap()
                .chat_key_shared
        );
    }

    #[tokio::test]
    async fn a_late_solver_decision_records_fiat_owed_after_the_refund() {
        let (engine, _) = fake_engine("Received");
        engine.on_hold_status("trade").await.unwrap();
        engine
            .handle(
                "buyer",
                Envelope::new(FIAT_SENT)
                    .with_trade("trade")
                    .with_payload(FiatSentPayload {
                        invoice: String::new(),
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        engine
            .handle(
                "buyer",
                Envelope::new(DISPUTE)
                    .with_trade("trade")
                    .with_payload(DisputePayload {
                        invoice: None,
                        solver_evidence: vec![sample_solver_evidence()],
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        engine
            .db
            .set_hold_expires_at_ms("trade", unix_now_ms())
            .unwrap();
        let refunding = engine.on_hold_status("trade").await.unwrap();
        assert!(replies_contain(&refunding, REFUNDING));
        assert!(
            !refunding
                .iter()
                .any(|item| matches!(item, Outbound::TradeRecord(_)))
        );
        let noted = engine
            .handle(
                "solver",
                Envelope::new(RESOLVE)
                    .with_trade("trade")
                    .with_payload(ResolvePayload {
                        winner: "fiat-owed".into(),
                        invoice: None,
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        let record = noted.iter().find_map(|item| match item {
            Outbound::TradeRecord(record) => Some(record.outcome.as_str()),
            _ => None,
        });
        assert_eq!(record, Some("fiat-owed"));
    }

    #[tokio::test]
    async fn restart_rearms_a_claimable_hold_and_drops_a_refund() {
        let (engine, fiber) = fake_engine("Received");
        engine
            .db
            .set_trade_state("trade", Phase::WaitingFiat.as_str())
            .unwrap();
        fiber.preimages.lock().unwrap().clear();
        engine.arm_watchtower().await.unwrap();
        assert!(fiber.preimages.lock().unwrap().contains("hash"));

        engine
            .db
            .set_trade_state("trade", Phase::Refunding.as_str())
            .unwrap();
        engine.arm_watchtower().await.unwrap();
        assert!(!fiber.preimages.lock().unwrap().contains("hash"));
        assert!(engine.db.get_preimage("hash").unwrap().is_none());
    }

    #[tokio::test]
    async fn settle_leaves_the_watchtower_preimage() {
        let (engine, fiber) = fake_engine("Received");
        engine
            .db
            .set_trade_state("trade", Phase::Releasing.as_str())
            .unwrap();
        engine
            .db
            .set_payout("trade", "buyer-invoice", Some("hash"))
            .unwrap();
        *fiber.payment_status.lock().unwrap() = "Success".into();
        let settled = engine.on_payout_status("trade").await.unwrap();
        assert!(replies_contain(&settled, SETTLED));
        assert!(fiber.preimages.lock().unwrap().contains("hash"));
        assert!(!*fiber.removed.lock().unwrap());
        assert!(engine.db.get_preimage("hash").unwrap().is_none());
    }

    #[tokio::test]
    async fn a_revealed_preimage_settles_after_fiber_returns_it() {
        let (engine, fiber) = fake_engine("Received");
        engine.db.delete_preimage("hash").unwrap();
        fiber.preimages.lock().unwrap().clear();
        engine
            .db
            .set_trade_state("trade", Phase::Releasing.as_str())
            .unwrap();
        engine
            .db
            .set_payout("trade", "buyer-invoice", Some("hash"))
            .unwrap();
        *fiber.payment_status.lock().unwrap() = "Success".into();
        let waiting = engine.on_payout_status("trade").await.unwrap();
        assert!(waiting.is_empty());
        assert_eq!(
            engine.db.get_trade("trade").unwrap().unwrap().state,
            Phase::Releasing.as_str()
        );

        *fiber.payment_preimage.lock().unwrap() = Some("revealed".into());
        let settled = engine.on_payout_status("trade").await.unwrap();
        assert!(replies_contain(&settled, SETTLED));
        assert!(fiber.preimages.lock().unwrap().contains("hash"));
        assert!(engine.db.get_preimage("hash").unwrap().is_none());
    }

    #[tokio::test]
    async fn payout_rejects_an_invoice_for_the_wrong_amount() {
        let (engine, fiber) = fake_engine("Received");
        engine
            .db
            .set_trade_state("trade", Phase::Releasing.as_str())
            .unwrap();
        engine
            .db
            .set_payout("trade", "buyer-invoice", None)
            .unwrap();
        *fiber.parsed_amount.lock().unwrap() = 1;
        let outbound = engine.on_payout_status("trade").await.unwrap();
        assert!(replies_contain(&outbound, NEW_INVOICE));
        let trade = engine.db.get_trade("trade").unwrap().unwrap();
        assert_eq!(trade.state, Phase::AwaitingInvoice.as_str());
        assert!(trade.payout_payment_hash.is_none());
    }

    #[test]
    fn payout_invoice_must_match_the_trade() {
        let invoice = ParsedInvoice {
            payment_hash: "h".into(),
            currency: "Fibt".into(),
            amount: Some(10),
            hash_algorithm: HashAlgorithm::CkbHash,
            final_tlc_expiry_delta_ms: crate::MIN_TLC_EXPIRY_LIMIT_MS,
            expires_at_ms: None,
        };
        assert!(payout_invoice_ok(&invoice, "Fibt", 10).is_ok());
        assert!(payout_invoice_ok(&invoice, "Fibb", 10).is_err());
        let wrong = ParsedInvoice {
            amount: Some(11),
            ..invoice
        };
        assert!(payout_invoice_ok(&wrong, "Fibt", 10).is_err());
        let open = ParsedInvoice {
            amount: None,
            ..wrong
        };
        assert!(payout_invoice_ok(&open, "Fibt", 10).is_err());
    }

    #[tokio::test]
    async fn the_hold_uses_the_buyer_invoice_algorithm() {
        let (engine, fiber) = fake_engine("Open");
        engine
            .db
            .set_trade_state("trade", Phase::WaitingInvoice.as_str())
            .unwrap();
        *fiber.invoice_algorithm.lock().unwrap() = HashAlgorithm::Sha256;
        let trade = engine.db.get_trade("trade").unwrap().unwrap();
        engine
            .bind_buyer_hold(&trade, "buyer-invoice")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*fiber.hold_algorithm.lock().unwrap(), HashAlgorithm::Sha256);
        assert_eq!(
            engine
                .db
                .get_trade("trade")
                .unwrap()
                .unwrap()
                .hold_payment_hash
                .as_deref(),
            Some("hash")
        );
    }

    #[tokio::test]
    async fn an_unsupported_buyer_algorithm_is_rejected_before_the_hold() {
        let (engine, fiber) = fake_engine("Open");
        engine
            .db
            .set_trade_state("trade", Phase::WaitingInvoice.as_str())
            .unwrap();
        let trade = engine.db.get_trade("trade").unwrap().unwrap();
        let before = trade.hold_payment_hash.clone();
        let rejected = engine
            .bind_buyer_hold(&trade, "unsupported-invoice")
            .await
            .unwrap()
            .unwrap_err();
        assert!(rejected.contains("unsupported hash algorithm"));
        assert_eq!(
            *fiber.hold_algorithm.lock().unwrap(),
            HashAlgorithm::CkbHash
        );
        let trade = engine.db.get_trade("trade").unwrap().unwrap();
        assert_eq!(trade.hold_payment_hash, before);
        assert_eq!(trade.state, Phase::WaitingInvoice.as_str());
    }

    #[tokio::test]
    async fn a_short_lived_invoice_is_refused_before_the_hold() {
        let (engine, fiber) = fake_engine("Open");
        engine
            .db
            .set_trade_state("trade", Phase::WaitingInvoice.as_str())
            .unwrap();
        *fiber.expires_at_ms.lock().unwrap() = Some(unix_now_ms());
        let trade = engine.db.get_trade("trade").unwrap().unwrap();
        let rejected = engine
            .bind_buyer_hold(&trade, "buyer-invoice")
            .await
            .unwrap()
            .unwrap_err();
        assert!(rejected.contains("expires before the hold"));
        assert!(rejected.contains("no expiry"));
        let trade = engine.db.get_trade("trade").unwrap().unwrap();
        assert_eq!(trade.hold_payment_hash.as_deref(), Some("hash"));
        assert_eq!(trade.hold_invoice.as_deref(), Some("inv"));
        assert_eq!(trade.state, Phase::WaitingInvoice.as_str());
    }

    #[tokio::test]
    async fn an_invoice_lock_longer_than_the_hold_is_refused_before_the_hold() {
        let (engine, fiber) = fake_engine("Open");
        engine
            .db
            .set_trade_state("trade", Phase::WaitingInvoice.as_str())
            .unwrap();
        *fiber.final_delta_ms.lock().unwrap() = crate::DEFAULT_FINAL_TLC_EXPIRY_DELTA_MS;
        let trade = engine.db.get_trade("trade").unwrap().unwrap();
        let rejected = engine
            .bind_buyer_hold(&trade, "buyer-invoice")
            .await
            .unwrap()
            .unwrap_err();
        assert!(rejected.contains("longer than the hold"));
        assert_eq!(
            engine.db.get_trade("trade").unwrap().unwrap().state,
            Phase::WaitingInvoice.as_str()
        );
    }

    #[tokio::test]
    async fn a_replacement_invoice_must_use_the_hold_algorithm() {
        let (engine, fiber) = fake_engine("Received");
        engine
            .db
            .set_hold("trade", "hash", "hold-hash", unix_now())
            .unwrap();
        *fiber.hold_algorithm.lock().unwrap() = HashAlgorithm::CkbHash;
        *fiber.invoice_algorithm.lock().unwrap() = HashAlgorithm::Sha256;
        let trade = engine.db.get_trade("trade").unwrap().unwrap();
        let rejected = engine
            .require_same_hold_hash(&trade, "buyer-invoice")
            .await
            .unwrap()
            .unwrap_err();
        assert!(rejected.contains("hash algorithm"));
    }

    #[tokio::test]
    async fn payout_refuses_an_invoice_whose_algorithm_differs_from_the_hold() {
        let (engine, fiber) = fake_engine("Received");
        engine
            .db
            .set_hold("trade", "hash", "hold-hash", unix_now())
            .unwrap();
        *fiber.hold_algorithm.lock().unwrap() = HashAlgorithm::CkbHash;
        *fiber.invoice_algorithm.lock().unwrap() = HashAlgorithm::Sha256;
        releasing_payout(&engine, &fiber).await;
        let outbound = engine.on_payout_status("trade").await.unwrap();
        assert!(replies_contain(&outbound, NEW_INVOICE));
        assert_eq!(*fiber.send_count.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn an_open_hold_without_a_preimage_still_boots() {
        let (engine, fiber) = fake_engine("Received");
        engine.db.delete_preimage("hash").unwrap();
        fiber.preimages.lock().unwrap().clear();
        engine
            .db
            .set_trade_state("trade", Phase::WaitingFiat.as_str())
            .unwrap();
        engine.arm_watchtower().await.unwrap();
        assert!(fiber.preimages.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_buyer_buys_the_sellers_offer() {
        let fiber = Arc::new(FakeFiber::new("Open", 100_000_000));
        let engine = Engine::new(Arc::new(Db::open_in_memory().unwrap()), fiber.clone(), None);
        let posted = engine
            .handle(
                "seller",
                Envelope::new(NEW_ORDER)
                    .with_payload(NewOrderPayload {
                        side: "sell".into(),
                        available_ckb: "10".into(),
                        fiat_currency: "NGN".into(),
                        price_per_ckb: "1000".into(),
                        min: "1000".into(),
                        max: "10000".into(),
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        let Outbound::PublicOrder(public) = &posted[0] else {
            panic!("expected a public order");
        };
        assert_eq!(public.hold_hours, 36);
        assert_eq!(public.side, "sell");
        assert_eq!(public.maker_nostr_pubkey, "seller");
        let published = serde_json::to_string(public).unwrap();
        assert!(!published.contains("maker_fiber_pubkey"));
        assert_eq!(public.fiat_currency, "NGN");
        let order_id = public.order_id.clone();

        let own = engine
            .handle(
                "seller",
                Envelope::new(TAKE)
                    .with_payload(TakePayload {
                        order_id: order_id.clone(),
                        fiat_amount: "1000".into(),
                        invoice: None,
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(cant_do_reason(&own).contains("your own order"));

        let taken = engine
            .handle(
                "buyer",
                Envelope::new(TAKE)
                    .with_payload(TakePayload {
                        order_id,
                        fiat_amount: "1000".into(),
                        invoice: Some("buyer-invoice".into()),
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(reply_to(&taken, "seller", PAY_INVOICE));
        assert!(reply_to(&taken, "buyer", PAY_INVOICE));
        assert!(engine.db.get_preimage("hash").unwrap().is_none());
        assert!(fiber.preimages.lock().unwrap().is_empty());
        let trade_id = taken
            .iter()
            .find_map(|item| match item {
                Outbound::Reply { envelope, .. } => envelope.trade_id.clone(),
                _ => None,
            })
            .unwrap();

        *fiber.status.lock().unwrap() = "Received".into();
        let waiting = engine.on_hold_status(&trade_id).await.unwrap();
        assert!(replies_contain(&waiting, WAITING_FIAT));
        let details: WaitingFiatPayload = waiting
            .iter()
            .find_map(|item| match item {
                Outbound::Reply { envelope, .. } if envelope.action == WAITING_FIAT => {
                    Some(envelope.decode_payload().unwrap())
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(details.reference, trade_id);
        assert_eq!(details.fiat_currency, "NGN");
        assert_eq!(details.seller_nostr, "seller");
        assert_eq!(details.buyer_nostr, "buyer");
        assert!(details.pay_by.is_some());
        assert!(details.release_by.is_none());

        let seller_fiat = engine
            .handle(
                "seller",
                Envelope::new(FIAT_SENT)
                    .with_trade(&trade_id)
                    .with_payload(FiatSentPayload {
                        invoice: "seller-invoice".into(),
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(cant_do_reason(&seller_fiat).contains("buyer"));

        let fiat = engine
            .handle(
                "buyer",
                Envelope::new(FIAT_SENT)
                    .with_trade(&trade_id)
                    .with_payload(FiatSentPayload {
                        invoice: "buyer-invoice".into(),
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(replies_contain(&fiat, FIAT_SENT_OK));

        let buyer_release = engine
            .handle("buyer", Envelope::new(RELEASE).with_trade(&trade_id))
            .await
            .unwrap();
        assert!(cant_do_reason(&buyer_release).contains("seller"));
        engine
            .handle("seller", Envelope::new(RELEASE).with_trade(&trade_id))
            .await
            .unwrap();
        assert_eq!(
            engine.db.get_trade(&trade_id).unwrap().unwrap().state,
            Phase::Releasing.as_str()
        );
    }

    async fn releasing_payout(engine: &Engine<FakeFiber>, fiber: &FakeFiber) {
        engine
            .db
            .set_trade_state("trade", Phase::Releasing.as_str())
            .unwrap();
        engine
            .db
            .set_payout("trade", "buyer-invoice", None)
            .unwrap();
        *fiber.get_payment_missing.lock().unwrap() = true;
    }

    #[tokio::test]
    async fn payout_pays_the_invoice() {
        let (engine, fiber) = fake_engine("Received");
        releasing_payout(&engine, &fiber).await;
        let settled = engine.on_payout_status("trade").await.unwrap();
        assert!(replies_contain(&settled, SETTLED));
        assert_eq!(*fiber.send_count.lock().unwrap(), 1);
        let trade = engine.db.get_trade("trade").unwrap().unwrap();
        let expires_at_ms = trade.hold_expires_at_ms.expect("tlc expiry");
        let limit = fiber
            .last_tlc_expiry_limit
            .lock()
            .unwrap()
            .expect("tlc expiry limit");
        assert!(
            unix_now_ms() + limit as i64 + crate::CLAIM_MARGIN_MS as i64 <= expires_at_ms + 2_000
        );
    }

    #[tokio::test]
    async fn a_late_observation_keeps_the_tlc_expiry() {
        let (engine, fiber) = fake_engine("Received");
        let expires_at_ms = unix_now_ms() + 5 * 3_600 * 1000;
        *fiber.inbound_expiry.lock().unwrap() = Some(expires_at_ms);
        let waiting = engine.on_hold_status("trade").await.unwrap();
        // Five hours cannot fit Fiber's route reserve, so fiat stays closed.
        assert!(replies_contain(&waiting, REFUNDING));
        let trade = engine.db.get_trade("trade").unwrap().unwrap();
        assert_eq!(trade.hold_expires_at_ms, Some(expires_at_ms));
        assert_eq!(
            trade.pay_by(),
            Some(crate::fiat_deadline_secs(
                expires_at_ms,
                trade.claim_margin_ms(),
                trade.final_tlc_expiry_delta_ms(),
            ))
        );
    }

    #[tokio::test]
    async fn a_missing_tlc_does_not_start_the_fiat_window() {
        let (engine, fiber) = fake_engine("Received");
        *fiber.inbound_missing.lock().unwrap() = true;
        let waiting = engine.on_hold_status("trade").await.unwrap();
        assert!(waiting.is_empty());
        let trade = engine.db.get_trade("trade").unwrap().unwrap();
        assert_eq!(trade.state, Phase::WaitingHold.as_str());
        assert!(trade.hold_expires_at_ms.is_none());
    }

    #[tokio::test]
    async fn a_long_payout_invoice_is_rejected_before_send() {
        let (engine, fiber) = fake_engine("Received");
        *fiber.final_delta_ms.lock().unwrap() = 30 * 3_600 * 1000;
        releasing_payout(&engine, &fiber).await;
        let outbound = engine.on_payout_status("trade").await.unwrap();
        assert!(replies_contain(&outbound, NEW_INVOICE));
        assert_eq!(*fiber.send_count.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn an_outgoing_lock_that_does_not_fit_is_not_treated_as_a_missing_route() {
        let (engine, fiber) = fake_engine("Received");
        recorded_payout(
            &engine,
            &fiber,
            "Failed",
            Some("Failed to build route, payment hop expiry 10 exceeds tlc_expiry_limit 9"),
        );
        *fiber.send_status.lock().unwrap() = "Failed".into();
        *fiber.send_failed_error.lock().unwrap() =
            Some("Failed to build route, payment hop expiry 10 exceeds tlc_expiry_limit 9".into());
        let outbound = engine.on_payout_status("trade").await.unwrap();
        assert!(outbound.is_empty());
        assert_eq!(*fiber.send_count.lock().unwrap(), 1);
        assert_eq!(
            engine.db.get_trade("trade").unwrap().unwrap().state,
            Phase::Releasing.as_str()
        );
    }

    #[tokio::test]
    async fn payout_asks_for_a_new_invoice_when_send_has_no_route() {
        let (engine, fiber) = fake_engine("Received");
        releasing_payout(&engine, &fiber).await;
        *fiber.send_status.lock().unwrap() = "Failed".into();
        *fiber.send_failed_error.lock().unwrap() =
            Some("Build payment route error: PathFind error: no path found".into());
        let outbound = engine.on_payout_status("trade").await.unwrap();
        assert!(replies_contain(&outbound, NEW_INVOICE));
        let reason = outbound.iter().find_map(|item| match item {
            Outbound::Reply { envelope, .. } if envelope.action == NEW_INVOICE => {
                let payload: NewInvoicePayload = envelope.decode_payload().unwrap();
                Some(payload.reason)
            }
            _ => None,
        });
        assert!(reason.unwrap().contains("no route"));
        assert_eq!(*fiber.send_count.lock().unwrap(), 1);
        assert_eq!(
            engine.db.get_trade("trade").unwrap().unwrap().state,
            Phase::AwaitingInvoice.as_str()
        );
    }

    #[tokio::test]
    async fn payout_refunds_when_the_incoming_tlc_can_no_longer_be_claimed() {
        let (engine, fiber) = fake_engine("Received");
        engine
            .db
            .set_hold_expires_at_ms("trade", unix_now_ms())
            .unwrap();
        releasing_payout(&engine, &fiber).await;
        let outbound = engine.on_payout_status("trade").await.unwrap();
        assert!(replies_contain(&outbound, REFUNDING));
        assert_eq!(*fiber.send_count.lock().unwrap(), 0);
        assert_eq!(
            engine.db.get_trade("trade").unwrap().unwrap().state,
            Phase::Refunding.as_str()
        );
        assert!(engine.db.get_preimage("hash").unwrap().is_none());
    }

    #[tokio::test]
    async fn a_payment_already_sent_still_settles_inside_the_safety_window() {
        let (engine, fiber) = fake_engine("Received");
        engine
            .db
            .set_hold_expires_at_ms("trade", unix_now_ms())
            .unwrap();
        engine
            .db
            .set_trade_state("trade", Phase::Releasing.as_str())
            .unwrap();
        engine
            .db
            .set_payout("trade", "buyer-invoice", Some("hash"))
            .unwrap();
        *fiber.payment_status.lock().unwrap() = "Success".into();
        *fiber.payment_preimage.lock().unwrap() = Some("preimage".into());
        let settled = engine.on_payout_status("trade").await.unwrap();
        assert!(replies_contain(&settled, SETTLED));
        assert_eq!(*fiber.send_count.lock().unwrap(), 0);
    }

    fn recorded_payout(
        engine: &Engine<FakeFiber>,
        fiber: &FakeFiber,
        status: &str,
        error: Option<&str>,
    ) {
        engine
            .db
            .set_trade_state("trade", Phase::Releasing.as_str())
            .unwrap();
        engine
            .db
            .set_payout("trade", "buyer-invoice", Some("hash"))
            .unwrap();
        *fiber.payment_status.lock().unwrap() = status.into();
        *fiber.payment_failed_error.lock().unwrap() = error.map(str::to_string);
    }

    #[tokio::test]
    async fn a_missing_path_asks_for_a_new_invoice() {
        let (engine, fiber) = fake_engine("Received");
        recorded_payout(
            &engine,
            &fiber,
            "Failed",
            Some("Build payment route error: Failed to build route, PathFind error: no path found"),
        );
        let outbound = engine.on_payout_status("trade").await.unwrap();
        assert!(replies_contain(&outbound, NEW_INVOICE));
        assert_eq!(*fiber.send_count.lock().unwrap(), 0);
        assert_eq!(
            engine.db.get_trade("trade").unwrap().unwrap().state,
            Phase::AwaitingInvoice.as_str()
        );
    }

    #[tokio::test]
    async fn a_resubmitted_invoice_is_sent_again_after_a_missing_path() {
        let (engine, fiber) = fake_engine("Received");
        engine
            .db
            .set_trade_state("trade", Phase::Releasing.as_str())
            .unwrap();
        engine
            .db
            .set_payout("trade", "buyer-invoice", None)
            .unwrap();
        *fiber.payment_status.lock().unwrap() = "Failed".into();
        *fiber.payment_failed_error.lock().unwrap() = Some("PathFind error: no path found".into());
        let settled = engine.on_payout_status("trade").await.unwrap();
        assert!(replies_contain(&settled, SETTLED));
        assert_eq!(*fiber.send_count.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn an_expired_invoice_refunds_instead_of_asking_for_a_replacement() {
        let (engine, fiber) = fake_engine("Received");
        recorded_payout(&engine, &fiber, "Failed", Some("invoice expired"));
        let outbound = engine.on_payout_status("trade").await.unwrap();
        assert!(replies_contain(&outbound, REFUNDING));
        assert!(outbound_replies(&outbound).iter().any(|envelope| {
            envelope.action == REFUNDING
                && envelope.payload.as_ref().is_some_and(|payload| {
                    payload["reason"]
                        .as_str()
                        .is_some_and(|reason| reason.contains("expired"))
                })
        }));
        assert_eq!(*fiber.send_count.lock().unwrap(), 0);
        assert_eq!(
            engine.db.get_trade("trade").unwrap().unwrap().state,
            Phase::Refunding.as_str()
        );
        assert!(engine.db.get_preimage("hash").unwrap().is_none());
    }

    #[tokio::test]
    async fn an_invoice_that_expires_before_send_refunds() {
        let (engine, fiber) = fake_engine("Received");
        releasing_payout(&engine, &fiber).await;
        *fiber.expires_at_ms.lock().unwrap() = Some(unix_now_ms() - 1);
        let outbound = engine.on_payout_status("trade").await.unwrap();
        assert!(replies_contain(&outbound, REFUNDING));
        assert_eq!(*fiber.send_count.lock().unwrap(), 0);
        assert_eq!(
            engine.db.get_trade("trade").unwrap().unwrap().state,
            Phase::Refunding.as_str()
        );
    }

    #[tokio::test]
    async fn a_failed_payout_that_is_not_a_route_still_asks_for_a_new_invoice() {
        let (engine, fiber) = fake_engine("Received");
        recorded_payout(&engine, &fiber, "Failed", Some("payment hash mismatch"));
        let outbound = engine.on_payout_status("trade").await.unwrap();
        assert!(replies_contain(&outbound, NEW_INVOICE));
        assert_eq!(*fiber.send_count.lock().unwrap(), 0);
        assert_eq!(
            engine.db.get_trade("trade").unwrap().unwrap().state,
            Phase::AwaitingInvoice.as_str()
        );
    }

    #[tokio::test]
    async fn a_payout_that_succeeded_after_a_route_failure_settles() {
        let (engine, fiber) = fake_engine("Received");
        engine
            .db
            .set_trade_state("trade", Phase::AwaitingInvoice.as_str())
            .unwrap();
        engine
            .db
            .set_payout("trade", "buyer-invoice", None)
            .unwrap();
        *fiber.payment_status.lock().unwrap() = "Success".into();
        *fiber.payment_preimage.lock().unwrap() = Some("preimage".into());
        let settled = engine.on_payout_status("trade").await.unwrap();
        assert!(replies_contain(&settled, SETTLED));
        assert_eq!(
            engine.db.get_trade("trade").unwrap().unwrap().state,
            Phase::Settled.as_str()
        );
        assert_eq!(*fiber.send_count.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn a_buy_post_cannot_be_created_or_taken() {
        let fiber = Arc::new(FakeFiber::new("Open", 0));
        let engine = Engine::new(Arc::new(Db::open_in_memory().unwrap()), fiber, None);
        let rejected = engine
            .handle(
                "buyer",
                Envelope::new(NEW_ORDER)
                    .with_payload(NewOrderPayload {
                        side: "buy".into(),
                        available_ckb: "10".into(),
                        fiat_currency: "USD".into(),
                        price_per_ckb: "1000".into(),
                        min: "1000".into(),
                        max: "10000".into(),
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(cant_do_reason(&rejected).contains("sell"));
        assert!(engine.db.orders().unwrap().is_empty());

        engine
            .db
            .insert_order(&Order {
                id: "buy-1".into(),
                side: "buy".into(),
                maker_nostr: "buyer".into(),
                maker_fiber: "fiber-buyer".into(),
                available_shannons: 1_000_000_000,
                fiat_currency: "USD".into(),
                price_per_ckb: "1000".into(),
                min: "1000".into(),
                max: "10000".into(),
                status: OrderStatus::Open.as_str().into(),
                hold_secs: 36 * 3_600,
                reserved_shannons: 0,
            })
            .unwrap();
        let taken = engine
            .handle(
                "seller",
                Envelope::new(TAKE)
                    .with_payload(TakePayload {
                        order_id: "buy-1".into(),
                        fiat_amount: "1000".into(),
                        invoice: None,
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(cant_do_reason(&taken).contains("sell"));
        assert!(engine.db.open_trade_for_order("buy-1").unwrap().is_none());
    }

    #[tokio::test]
    async fn a_post_must_be_sell() {
        let fiber = Arc::new(FakeFiber::new("Open", 0));
        let engine = Engine::new(Arc::new(Db::open_in_memory().unwrap()), fiber, None);
        let rejected = engine
            .handle(
                "maker",
                Envelope::new(NEW_ORDER)
                    .with_payload(NewOrderPayload {
                        side: "swap".into(),
                        available_ckb: "10".into(),
                        fiat_currency: "NGN".into(),
                        price_per_ckb: "1000".into(),
                        min: "1000".into(),
                        max: "10000".into(),
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(cant_do_reason(&rejected).contains("sell"));
    }

    #[tokio::test]
    async fn only_the_poster_can_cancel_an_open_post() {
        let fiber = Arc::new(FakeFiber::new("Open", 0));
        let engine = Engine::new(Arc::new(Db::open_in_memory().unwrap()), fiber, None);
        let posted = engine
            .handle(
                "buyer",
                Envelope::new(NEW_ORDER)
                    .with_payload(NewOrderPayload {
                        side: "sell".into(),
                        available_ckb: "10".into(),
                        fiat_currency: "USD".into(),
                        price_per_ckb: "1000".into(),
                        min: "1000".into(),
                        max: "10000".into(),
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        let Outbound::PublicOrder(public) = &posted[0] else {
            panic!("expected a public order");
        };
        assert_eq!(public.status, "open");
        let order_id = public.order_id.clone();
        let stranger = engine
            .handle(
                "seller",
                Envelope::new(CANCEL)
                    .with_payload(crate::CancelPayload {
                        order_id: Some(order_id.clone()),
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(cant_do_reason(&stranger).contains("poster"));
        let canceled = engine
            .handle(
                "buyer",
                Envelope::new(CANCEL)
                    .with_payload(crate::CancelPayload {
                        order_id: Some(order_id.clone()),
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(replies_contain(&canceled, CANCELED));
        let published = canceled.iter().find_map(|item| match item {
            Outbound::PublicOrder(order) => Some(order),
            Outbound::Reply { .. } | Outbound::TradeRecord(_) => None,
        });
        assert_eq!(published.unwrap().status, "canceled");
        let reply = canceled.iter().find_map(|item| match item {
            Outbound::Reply { envelope, .. } if envelope.action == CANCELED => Some(envelope),
            _ => None,
        });
        let payload: crate::CancelPayload = reply.unwrap().decode_payload().unwrap();
        assert_eq!(payload.order_id.as_deref(), Some(order_id.as_str()));
    }

    fn reply_to(out: &[Outbound], to: &str, action: &str) -> bool {
        out.iter().any(|item| match item {
            Outbound::Reply { to: dest, envelope } => dest == to && envelope.action == action,
            Outbound::PublicOrder(_) | Outbound::TradeRecord(_) => false,
        })
    }

    #[tokio::test]
    #[ignore = "requires a running Fiber node (TWINE_RPC)"]
    async fn take_creates_hold_and_registers_preimage() {
        let ctx = TestContext::new().await;
        let order_id = open_order(&ctx.engine, "seller").await;
        let (trade_id, invoice, amount) = take(&ctx.engine, "buyer", &order_id).await;
        assert!(!invoice.is_empty());
        assert_eq!(amount, "100000000");
        let trade = ctx.engine.db.get_trade(&trade_id).unwrap().unwrap();
        let hash = trade.hold_payment_hash.unwrap();
        assert!(ctx.engine.db.get_preimage(&hash).unwrap().is_none());
        let info = ctx.fiber.get_invoice(&hash).await.unwrap();
        assert_eq!(info.status, "Open");
    }

    #[tokio::test]
    #[ignore = "requires a running Fiber node (TWINE_RPC)"]
    async fn pay_invoice_never_includes_preimage() {
        let ctx = TestContext::new().await;
        let order_id = open_order(&ctx.engine, "seller").await;
        let payout = ctx
            .fiber
            .new_payout_invoice(100_000_000, "twine payout", crate::MIN_TLC_EXPIRY_LIMIT_MS)
            .await
            .unwrap();
        let outbound = ctx
            .engine
            .handle(
                "buyer",
                Envelope::new(TAKE)
                    .with_payload(TakePayload {
                        order_id,
                        fiat_amount: "1000".into(),
                        invoice: Some(payout.invoice),
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(replies_contain(&outbound, PAY_INVOICE));
        let json = serde_json::to_string(&outbound_replies(&outbound)).unwrap();
        assert!(!json.contains("preimage"));
        assert!(!json.contains("payment_preimage"));
    }

    #[tokio::test]
    #[ignore = "requires a running Fiber node (TWINE_RPC)"]
    async fn cancel_open_hold() {
        let ctx = TestContext::new().await;
        let order_id = open_order(&ctx.engine, "seller").await;
        let (trade_id, _, _) = take(&ctx.engine, "buyer", &order_id).await;
        let hash = ctx
            .engine
            .db
            .get_trade(&trade_id)
            .unwrap()
            .unwrap()
            .hold_payment_hash
            .unwrap();
        let canceled = ctx
            .engine
            .handle("seller", Envelope::new(CANCEL).with_trade(&trade_id))
            .await
            .unwrap();
        assert!(replies_contain(&canceled, CANCELED));
        let info = ctx.fiber.get_invoice(&hash).await.unwrap();
        assert_eq!(info.status, "Cancelled");
    }

    #[tokio::test]
    async fn a_take_starts_the_hold_watch_and_locked_is_unknown() {
        let (engine, _) = fake_engine("Open");
        let polling = Arc::new(StdMutex::new(HashSet::new()));
        engine.bind_polls(Client::default(), Keys::generate(), polling.clone());
        engine.watch("trade").unwrap();
        assert!(polling.lock().unwrap().contains("hold:trade"));
        assert!(!polling.lock().unwrap().contains("pay:trade"));

        let locked = engine
            .handle("seller", Envelope::new("locked").with_trade("trade"))
            .await
            .unwrap();
        assert!(cant_do_reason(&locked).contains("unknown"));
    }

    #[tokio::test]
    async fn seller_can_release_during_a_dispute() {
        let (engine, _) = fake_engine("Received");
        engine.on_hold_status("trade").await.unwrap();
        let missing = engine
            .handle("seller", Envelope::new(RELEASE).with_trade("trade"))
            .await
            .unwrap();
        assert!(cant_do_reason(&missing).contains("fiat"));
        engine
            .handle(
                "buyer",
                Envelope::new(DISPUTE)
                    .with_trade("trade")
                    .with_payload(DisputePayload {
                        invoice: Some("buyer-invoice".into()),
                        solver_evidence: Vec::new(),
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        let blocked = engine
            .handle("buyer", Envelope::new(RELEASE).with_trade("trade"))
            .await
            .unwrap();
        assert!(cant_do_reason(&blocked).contains("seller"));
        engine
            .handle("seller", Envelope::new(RELEASE).with_trade("trade"))
            .await
            .unwrap();
        assert_eq!(
            engine.db.get_trade("trade").unwrap().unwrap().state,
            Phase::Releasing.as_str()
        );
    }

    #[tokio::test]
    #[ignore = "requires a running Fiber node (TWINE_RPC)"]
    async fn lock_poll_and_release_pay_the_buyer_first() {
        let ctx = TestContext::new().await;
        let order_id = open_order(&ctx.engine, "seller").await;
        let (trade_id, _, _) = take(&ctx.engine, "buyer", &order_id).await;
        let hash = ctx
            .engine
            .db
            .get_trade(&trade_id)
            .unwrap()
            .unwrap()
            .hold_payment_hash
            .clone()
            .unwrap();

        wait_invoice_status(&ctx.fiber, &hash, "Received", Duration::from_secs(120)).await;
        let waiting = ctx.engine.on_hold_status(&trade_id).await.unwrap();
        assert!(replies_contain(&waiting, WAITING_FIAT));

        let payout = ctx
            .fiber
            .new_payout_invoice(100_000_000, "twine buyer", crate::MIN_TLC_EXPIRY_LIMIT_MS)
            .await
            .unwrap();
        fiat_sent(&ctx.engine, "buyer", &trade_id, &payout.invoice).await;

        ctx.engine
            .handle("seller", Envelope::new(RELEASE).with_trade(&trade_id))
            .await
            .unwrap();
        let settled = ctx.engine.on_payout_status(&trade_id).await.unwrap();
        assert!(replies_contain(&settled, SETTLED));
        assert_eq!(
            ctx.engine.db.get_trade(&trade_id).unwrap().unwrap().state,
            Phase::Settled.as_str()
        );
        assert!(ctx.engine.db.get_preimage(&hash).unwrap().is_none());
    }

    #[tokio::test]
    #[ignore = "requires a running Fiber node (TWINE_RPC)"]
    async fn reject_cancel_after_hold_received() {
        let ctx = TestContext::new().await;
        let order_id = open_order(&ctx.engine, "seller").await;
        let (trade_id, _, _) = take(&ctx.engine, "buyer", &order_id).await;
        let hash = ctx
            .engine
            .db
            .get_trade(&trade_id)
            .unwrap()
            .unwrap()
            .hold_payment_hash
            .unwrap();
        wait_invoice_status(&ctx.fiber, &hash, "Received", Duration::from_secs(120)).await;
        ctx.engine.on_hold_status(&trade_id).await.unwrap();
        let rejected = ctx
            .engine
            .handle("seller", Envelope::new(CANCEL).with_trade(&trade_id))
            .await
            .unwrap();
        assert!(cant_do_reason(&rejected).contains("timelock"));
        assert!(replies_contain(&rejected, CANT_DO));
    }

    #[tokio::test]
    async fn my_trades_returns_only_the_caller() {
        let (engine, _) = fake_engine("Received");
        let mine = engine
            .handle("buyer", Envelope::new(crate::MY_TRADES))
            .await
            .unwrap();
        let payload: crate::TradesPayload = mine
            .iter()
            .find_map(|item| match item {
                Outbound::Reply { to, envelope }
                    if to == "buyer" && envelope.action == crate::TRADES =>
                {
                    Some(envelope.decode_payload().unwrap())
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(payload.trades.len(), 1);
        assert_eq!(payload.trades[0].trade_id, "trade");
        assert_eq!(payload.trades[0].buyer_nostr, "buyer");
        assert!(payload.trades[0].lock_by.is_some());
        let stranger = engine
            .handle("stranger", Envelope::new(crate::MY_TRADES))
            .await
            .unwrap();
        let empty: crate::TradesPayload = stranger
            .iter()
            .find_map(|item| match item {
                Outbound::Reply { to, envelope }
                    if to == "stranger" && envelope.action == crate::TRADES =>
                {
                    Some(envelope.decode_payload().unwrap())
                }
                _ => None,
            })
            .unwrap();
        assert!(empty.trades.is_empty());
        let reserved = engine.db.get_order("order").unwrap().unwrap();
        assert_eq!(reserved.reserved_shannons, 100_000_000);
        assert_eq!(reserved.public().reserved_ckb.as_deref(), Some("1"));
    }
}
