use crate::db::Db;
use crate::fiber::{FiberRpc, ParsedInvoice};
use crate::nostr::publish_outbounds;
use crate::types::{Order, OrderStatus, Trade};
use crate::{
    CANCELED, CANT_DO, CancelPayload, CantDoPayload, ClientAction, DISPUTED, Decision,
    DisputePayload, EXPIRED, Envelope, FIAT_SENT_OK, FIBER_POLL_SECS, FiatSentPayload,
    InvoiceStatus, NEW_INVOICE, NewInvoicePayload, NewOrderPayload, Outbound, PAY_INVOICE,
    PayInvoicePayload, Phase, REFUNDING, ResolvePayload, SETTLED, TakeSellPayload, WAITING_FIAT,
    actor_of, apply_cancel, apply_clock, apply_dispute, apply_expired, apply_fiat_sent,
    apply_hold_received, apply_locked, apply_release, apply_release_failed,
    apply_release_succeeded, apply_resolve, apply_take, hex_bytes, parse_winner,
    validate_new_order, validate_take,
};
use anyhow::{Result, anyhow, bail};
use nostr_sdk::prelude::*;
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::warn;
use uuid::Uuid;

#[derive(Clone, Copy)]
pub enum PollKind {
    Hold,
    Payout,
}

pub struct Engine<F> {
    pub db: Arc<Db>,
    fiber: Arc<F>,
    solver: Option<String>,
}

impl<F> Clone for Engine<F> {
    fn clone(&self) -> Self {
        Self {
            db: self.db.clone(),
            fiber: self.fiber.clone(),
            solver: self.solver.clone(),
        }
    }
}

impl<F: FiberRpc> Engine<F> {
    pub fn new(db: Arc<Db>, fiber: Arc<F>, solver: Option<String>) -> Self {
        Self { db, fiber, solver }
    }

    /// Put every still-claimable hold back on the watchtower, and drop any we have
    /// already decided not to claim. A force-close can spend the HTLC only while
    /// `create_preimage` is registered.
    pub async fn arm_watchtower(&self) -> Result<()> {
        for trade in self.db.trades_in_states(Phase::watched_phases())? {
            if trade.phase()? == Phase::Refunding {
                self.drop_preimage(&trade).await?;
                continue;
            }
            let Some(hash) = trade.hold_payment_hash.as_deref() else {
                bail!("trade {} is open without a hold hash", trade.id);
            };
            let Some(preimage) = self.db.get_preimage(hash)? else {
                bail!(
                    "trade {} is open but the hold preimage is missing",
                    trade.id
                );
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
        let holds = self
            .db
            .trades_in_states(Phase::watched_phases())?
            .into_iter()
            .map(|trade| trade.id)
            .collect();
        let payouts = self
            .db
            .trades_in_states(&[Phase::Releasing])?
            .into_iter()
            .map(|trade| trade.id)
            .collect();
        Ok((holds, payouts))
    }

    pub async fn handle(&self, sender: &str, envelope: Envelope) -> Result<Vec<Outbound>> {
        match envelope.client_action() {
            Some(ClientAction::NewOrder) => self.on_new_order(sender, envelope).await,
            Some(ClientAction::TakeSell) => self.on_take_sell(sender, envelope).await,
            Some(ClientAction::Locked) => self.on_locked(sender, envelope).await,
            Some(ClientAction::FiatSent) => self.on_fiat_sent(sender, envelope).await,
            Some(ClientAction::Release) => self.on_release(sender, envelope).await,
            Some(ClientAction::Cancel) => self.on_cancel(sender, envelope).await,
            Some(ClientAction::Dispute) => self.on_dispute(sender, envelope).await,
            Some(ClientAction::Resolve) => self.on_resolve(sender, envelope).await,
            None => Ok(vec![cant_do(
                sender,
                envelope.trade_id.clone(),
                "unknown action",
            )]),
        }
    }

    async fn on_new_order(&self, sender: &str, envelope: Envelope) -> Result<Vec<Outbound>> {
        let payload: NewOrderPayload = match envelope.decode_payload() {
            Ok(payload) => payload,
            Err(_) => return Ok(vec![cant_do(sender, None, "invalid new-order payload")]),
        };
        let available = match validate_new_order(
            &payload.available_ckb,
            &payload.price_per_ckb,
            &payload.min,
            &payload.max,
            &payload.fiat_currency_code,
            &payload.payment_method,
            &payload.fiber_pubkey,
        ) {
            Ok(value) => value,
            Err(error) => return Ok(vec![cant_do(sender, None, &error.to_string())]),
        };
        let order = Order {
            id: Uuid::new_v4().to_string(),
            maker_nostr: sender.to_string(),
            maker_fiber: payload.fiber_pubkey,
            available_shannons: available,
            fiat_currency_code: payload.fiat_currency_code,
            price_per_ckb: payload.price_per_ckb,
            min: payload.min,
            max: payload.max,
            payment_method: payload.payment_method,
            status: OrderStatus::Open.as_str().into(),
        };
        self.db.insert_order(&order)?;
        Ok(vec![Outbound::PublicOrder(order.public())])
    }

    async fn on_take_sell(&self, sender: &str, envelope: Envelope) -> Result<Vec<Outbound>> {
        let payload: TakeSellPayload = match envelope.decode_payload() {
            Ok(payload) => payload,
            Err(_) => return Ok(vec![cant_do(sender, None, "invalid take-sell payload")]),
        };
        let Some(order) = self.db.get_order(&payload.order_id)? else {
            return Ok(vec![cant_do(sender, None, "order not found")]);
        };
        if !order.is_open() {
            return Ok(vec![cant_do(sender, None, "order is not open")]);
        }
        if sender == order.maker_nostr {
            return Ok(vec![cant_do(
                sender,
                None,
                "maker cannot take their own order",
            )]);
        }
        let has_open = self.db.open_trade_for_order(&order.id)?.is_some();
        match apply_take(Phase::Pending, has_open) {
            Decision::Ok(Phase::WaitingHold) => {}
            Decision::Reject(reason) => return Ok(vec![cant_do(sender, None, reason)]),
            _ => return Ok(vec![cant_do(sender, None, "cannot take this order")]),
        }
        let shannons = match validate_take(
            &payload.fiat_amount,
            &order.min,
            &order.max,
            &order.price_per_ckb,
            order.available_shannons,
        ) {
            Ok(value) => value,
            Err(error) => return Ok(vec![cant_do(sender, None, &error.to_string())]),
        };
        if payload.taker_fiber_pubkey.trim().is_empty() {
            return Ok(vec![cant_do(
                sender,
                None,
                "taker fiber pubkey is required",
            )]);
        }

        let mut secret = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut secret);
        let hash = Sha256::digest(secret);
        let preimage = hex_bytes(&secret);
        let payment_hash = hex_bytes(hash.as_slice());
        let trade_id = Uuid::new_v4().to_string();

        let trade = Trade {
            id: trade_id.clone(),
            order_id: order.id.clone(),
            taker_nostr: sender.to_string(),
            taker_fiber: payload.taker_fiber_pubkey,
            fiat_amount: payload.fiat_amount,
            shannons,
            state: Phase::WaitingHold.as_str().into(),
            hold_payment_hash: Some(payment_hash.clone()),
            hold_invoice: None,
            payout_invoice: None,
            payout_payment_hash: None,
            hold_received_at: None,
        };
        if let Err(error) = self.db.commit_take(&trade, &preimage) {
            return Ok(vec![cant_do(sender, None, &error.to_string())]);
        }

        let created = match self
            .fiber
            .new_hold_invoice(shannons, &payment_hash, &format!("twine {trade_id}"))
            .await
        {
            Ok(created) => created,
            Err(error) => {
                self.rollback_take(&trade, &payment_hash)?;
                return Ok(vec![cant_do(
                    sender,
                    Some(trade_id),
                    &format!("hold invoice failed: {error}"),
                )]);
            }
        };
        if let Err(error) = self.fiber.create_preimage(&payment_hash, &preimage).await {
            let _ = self.fiber.cancel_invoice(&payment_hash).await;
            let _ = self.fiber.remove_preimage(&payment_hash).await;
            self.rollback_take(&trade, &payment_hash)?;
            return Ok(vec![cant_do(
                sender,
                Some(trade_id),
                &format!("watchtower preimage failed: {error}"),
            )]);
        }
        self.db
            .set_hold(&trade_id, &payment_hash, &created.invoice)?;

        let order = self
            .db
            .get_order(&order.id)?
            .ok_or_else(|| anyhow!("order missing after take"))?;
        let pay = Envelope::new(PAY_INVOICE)
            .with_trade(&trade_id)
            .with_payload(PayInvoicePayload {
                invoice: created.invoice,
                amount_shannons: shannons.to_string(),
                order_id: order.id.clone(),
            })?;
        Ok(vec![
            Outbound::PublicOrder(order.public()),
            Outbound::Reply {
                to: order.maker_nostr.clone(),
                envelope: pay.clone(),
            },
            Outbound::Reply {
                to: sender.to_string(),
                envelope: pay,
            },
        ])
    }

    fn rollback_take(&self, trade: &Trade, payment_hash: &str) -> Result<()> {
        self.db
            .rollback_take(&trade.order_id, &trade.id, payment_hash, trade.shannons)
    }

    async fn on_locked(&self, sender: &str, envelope: Envelope) -> Result<Vec<Outbound>> {
        let Some(trade_id) = envelope.trade_id.clone() else {
            return Ok(vec![cant_do(sender, None, "trade_id is required")]);
        };
        let Some(trade) = self.db.get_trade(&trade_id)? else {
            return Ok(vec![cant_do(sender, Some(trade_id), "trade not found")]);
        };
        let order = self.require_order(&trade.order_id)?;
        let actor = self.actor(&order.maker_nostr, Some(&trade.taker_nostr), sender);
        match apply_locked(trade.phase()?, actor) {
            Decision::NoOp => Ok(Vec::new()),
            Decision::Reject(reason) => Ok(vec![cant_do(sender, Some(trade_id), reason)]),
            Decision::Ok(_) => Ok(Vec::new()),
        }
    }

    pub async fn on_hold_status(&self, trade_id: &str) -> Result<Vec<Outbound>> {
        let Some(mut trade) = self.db.get_trade(trade_id)? else {
            bail!("trade {trade_id} not found");
        };
        let phase = trade.phase()?;
        if !phase.watched() {
            return Ok(Vec::new());
        }
        let payment_hash = trade
            .hold_payment_hash
            .as_deref()
            .ok_or_else(|| anyhow!("trade missing hold hash"))?;
        let invoice = self.fiber.get_invoice(payment_hash).await?;
        let status = InvoiceStatus::parse(&invoice.status)
            .ok_or_else(|| anyhow!("unknown invoice status {}", invoice.status))?;
        let order = self.require_order(&trade.order_id)?;
        if status == InvoiceStatus::Expired {
            return self.expire_trade(&order, &trade).await;
        }
        if status == InvoiceStatus::Cancelled && phase == Phase::WaitingHold {
            return self.complete_cancel(&order, &trade).await;
        }
        if phase == Phase::WaitingHold && status == InvoiceStatus::Received {
            return match apply_hold_received(Phase::WaitingHold) {
                Decision::Ok(next) => {
                    self.db
                        .set_hold_received(&trade.id, next.as_str(), unix_now())?;
                    Ok(party_replies(
                        &order,
                        &trade,
                        Envelope::new(WAITING_FIAT).with_trade(&trade.id),
                    ))
                }
                other => bail!("invalid hold received: {other:?}"),
            };
        }
        if status == InvoiceStatus::Received && trade.hold_received_at.is_none() {
            let now = unix_now();
            self.db.set_hold_received_at(&trade.id, now)?;
            trade.hold_received_at = Some(now);
        }
        if phase == Phase::Refunding && self.preimage_pending(&trade)? {
            self.drop_preimage(&trade).await?;
            return Ok(party_replies(
                &order,
                &trade,
                Envelope::new(REFUNDING).with_trade(&trade.id),
            ));
        }
        self.refund_if_due(&order, &trade).await
    }

    async fn expire_trade(&self, order: &Order, trade: &Trade) -> Result<Vec<Outbound>> {
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
                    party_replies(&order, trade, Envelope::new(EXPIRED).with_trade(&trade.id));
                outbound.insert(0, Outbound::PublicOrder(order.public()));
                Ok(outbound)
            }
            Decision::Reject(_) => Ok(Vec::new()),
            Decision::NoOp => Ok(Vec::new()),
        }
    }

    async fn refund_if_due(&self, order: &Order, trade: &Trade) -> Result<Vec<Outbound>> {
        match apply_clock(trade.phase()?, unix_now(), trade.hold_received_at) {
            Decision::Ok(Phase::Refunding) => self.begin_refund(order, trade).await,
            Decision::NoOp => Ok(Vec::new()),
            other => bail!("invalid clock: {other:?}"),
        }
    }

    async fn begin_refund(&self, order: &Order, trade: &Trade) -> Result<Vec<Outbound>> {
        self.db
            .set_trade_state(&trade.id, Phase::Refunding.as_str())?;
        self.drop_preimage(trade).await?;
        Ok(party_replies(
            order,
            trade,
            Envelope::new(REFUNDING).with_trade(&trade.id),
        ))
    }

    async fn drop_preimage(&self, trade: &Trade) -> Result<()> {
        let Some(hash) = trade.hold_payment_hash.as_deref() else {
            return Ok(());
        };
        self.fiber.remove_preimage(hash).await?;
        self.db.delete_preimage(hash)?;
        Ok(())
    }

    async fn on_fiat_sent(&self, sender: &str, envelope: Envelope) -> Result<Vec<Outbound>> {
        let Some(trade_id) = envelope.trade_id.clone() else {
            return Ok(vec![cant_do(sender, None, "trade_id is required")]);
        };
        let payload: FiatSentPayload = match envelope.decode_payload() {
            Ok(payload) => payload,
            Err(_) => {
                return Ok(vec![cant_do(
                    sender,
                    Some(trade_id),
                    "invalid fiat-sent payload",
                )]);
            }
        };
        if payload.invoice.trim().is_empty() {
            return Ok(vec![cant_do(
                sender,
                Some(trade_id),
                "payout invoice is required",
            )]);
        }
        let Some(trade) = self.db.get_trade(&trade_id)? else {
            return Ok(vec![cant_do(sender, Some(trade_id), "trade not found")]);
        };
        let order = self.require_order(&trade.order_id)?;
        if let Some(outbound) = self.refund_due_outbound(&order, &trade).await? {
            return Ok(outbound);
        }
        let actor = self.actor(&order.maker_nostr, Some(&trade.taker_nostr), sender);
        match apply_fiat_sent(trade.phase()?, actor) {
            Decision::Ok(phase) => {
                self.db.set_payout(&trade.id, &payload.invoice, None)?;
                self.db.set_trade_state(&trade.id, phase.as_str())?;
                if phase == Phase::Releasing {
                    return Ok(Vec::new());
                }
                Ok(party_replies(
                    &order,
                    &trade,
                    Envelope::new(FIAT_SENT_OK).with_trade(&trade.id),
                ))
            }
            Decision::Reject(reason) => Ok(vec![cant_do(sender, Some(trade_id), reason)]),
            Decision::NoOp => Ok(Vec::new()),
        }
    }

    async fn on_release(&self, sender: &str, envelope: Envelope) -> Result<Vec<Outbound>> {
        let Some(trade_id) = envelope.trade_id.clone() else {
            return Ok(vec![cant_do(sender, None, "trade_id is required")]);
        };
        let Some(trade) = self.db.get_trade(&trade_id)? else {
            return Ok(vec![cant_do(sender, Some(trade_id), "trade not found")]);
        };
        let order = self.require_order(&trade.order_id)?;
        if let Some(outbound) = self.refund_due_outbound(&order, &trade).await? {
            return Ok(outbound);
        }
        let actor = self.actor(&order.maker_nostr, Some(&trade.taker_nostr), sender);
        match apply_release(trade.phase()?, actor) {
            Decision::Ok(phase) => {
                self.db.set_trade_state(&trade.id, phase.as_str())?;
                Ok(Vec::new())
            }
            Decision::Reject(reason) => Ok(vec![cant_do(sender, Some(trade_id), reason)]),
            Decision::NoOp => Ok(Vec::new()),
        }
    }

    pub async fn on_payout_status(&self, trade_id: &str) -> Result<Vec<Outbound>> {
        let Some(mut trade) = self.db.get_trade(trade_id)? else {
            bail!("trade {trade_id} not found");
        };
        if trade.phase()? != Phase::Releasing {
            return Ok(Vec::new());
        }
        let order = self.require_order(&trade.order_id)?;
        if trade.payout_payment_hash.is_none() {
            if let Some(outbound) = self.refund_due_outbound(&order, &trade).await? {
                return Ok(outbound);
            }
        }
        let invoice = trade
            .payout_invoice
            .clone()
            .ok_or_else(|| anyhow!("missing payout invoice"))?;
        if trade.payout_payment_hash.is_none() {
            let parsed = match self.fiber.parse_invoice(&invoice).await {
                Ok(parsed) => parsed,
                Err(error) => {
                    return self.fail_payout(
                        &order,
                        &trade,
                        &format!("payout invoice rejected: {error}"),
                    );
                }
            };
            if let Err(reason) = payout_invoice_ok(&parsed, self.fiber.currency(), trade.shannons) {
                return self.fail_payout(&order, &trade, reason);
            }
            let payment = match self.fiber.send_payment(&invoice).await {
                Ok(payment) => payment,
                Err(error) => return self.fail_payout(&order, &trade, &sender_reason(&error)),
            };
            self.db
                .set_payout(&trade.id, &invoice, Some(&payment.payment_hash))?;
            trade.payout_payment_hash = Some(payment.payment_hash.clone());
            if payment.status == "Failed" {
                return self.fail_payout(&order, &trade, "taker payout failed");
            }
            if payment.status == "Success" {
                return self.settle_hold(&order, &trade).await;
            }
            return Ok(Vec::new());
        }
        let payment_hash = trade.payout_payment_hash.as_deref().unwrap();
        let payment = self.fiber.get_payment(payment_hash).await?;
        match payment.status.as_str() {
            "Success" => self.settle_hold(&order, &trade).await,
            "Failed" => self.fail_payout(&order, &trade, "taker payout failed"),
            _ => Ok(Vec::new()),
        }
    }

    async fn settle_hold(&self, order: &Order, trade: &Trade) -> Result<Vec<Outbound>> {
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
        let preimage = self
            .db
            .get_preimage(payment_hash)?
            .ok_or_else(|| anyhow!("missing preimage"))?;
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
                    order,
                    trade,
                    Envelope::new(SETTLED).with_trade(&trade.id),
                ))
            }
            other => bail!("invalid settle: {other:?}"),
        }
    }

    fn fail_payout(&self, order: &Order, trade: &Trade, reason: &str) -> Result<Vec<Outbound>> {
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
                Ok(party_replies(order, trade, envelope))
            }
            other => bail!("invalid payout fail: {other:?}"),
        }
    }

    async fn on_dispute(&self, sender: &str, envelope: Envelope) -> Result<Vec<Outbound>> {
        let Some(trade_id) = envelope.trade_id.clone() else {
            return Ok(vec![cant_do(sender, None, "trade_id is required")]);
        };
        if self.solver.is_none() {
            return Ok(vec![cant_do(
                sender,
                Some(trade_id),
                "solver is not configured",
            )]);
        }
        let Some(trade) = self.db.get_trade(&trade_id)? else {
            return Ok(vec![cant_do(sender, Some(trade_id), "trade not found")]);
        };
        let order = self.require_order(&trade.order_id)?;
        if let Some(outbound) = self.refund_due_outbound(&order, &trade).await? {
            return Ok(outbound);
        }
        let actor = self.actor(&order.maker_nostr, Some(&trade.taker_nostr), sender);
        match apply_dispute(trade.phase()?, actor) {
            Decision::Ok(phase) => {
                let payload: DisputePayload = envelope.decode_payload().unwrap_or_default();
                if let Some(invoice) = payload
                    .invoice
                    .as_deref()
                    .map(str::trim)
                    .filter(|invoice| !invoice.is_empty())
                {
                    self.db.set_payout(&trade.id, invoice, None)?;
                }
                self.db.set_trade_state(&trade.id, phase.as_str())?;
                Ok(self.with_solver(
                    &order,
                    &trade,
                    Envelope::new(DISPUTED).with_trade(&trade.id),
                ))
            }
            Decision::Reject(reason) => Ok(vec![cant_do(sender, Some(trade_id), reason)]),
            Decision::NoOp => Ok(Vec::new()),
        }
    }

    async fn on_resolve(&self, sender: &str, envelope: Envelope) -> Result<Vec<Outbound>> {
        let Some(trade_id) = envelope.trade_id.clone() else {
            return Ok(vec![cant_do(sender, None, "trade_id is required")]);
        };
        let payload: ResolvePayload = match envelope.decode_payload() {
            Ok(payload) => payload,
            Err(_) => {
                return Ok(vec![cant_do(
                    sender,
                    Some(trade_id),
                    "invalid resolve payload",
                )]);
            }
        };
        let Some(winner) = parse_winner(&payload.winner) else {
            return Ok(vec![cant_do(
                sender,
                Some(trade_id),
                "winner must be buyer or seller",
            )]);
        };
        let Some(trade) = self.db.get_trade(&trade_id)? else {
            return Ok(vec![cant_do(sender, Some(trade_id), "trade not found")]);
        };
        let order = self.require_order(&trade.order_id)?;
        if let Some(outbound) = self.refund_due_outbound(&order, &trade).await? {
            return Ok(outbound);
        }
        let supplied = payload
            .invoice
            .as_deref()
            .map(str::trim)
            .filter(|invoice| !invoice.is_empty())
            .map(ToOwned::to_owned);
        let has_invoice = supplied.is_some()
            || trade
                .payout_invoice
                .as_deref()
                .is_some_and(|invoice| !invoice.trim().is_empty());
        let actor = self.actor(&order.maker_nostr, Some(&trade.taker_nostr), sender);
        match apply_resolve(trade.phase()?, actor, winner, has_invoice) {
            Decision::Ok(Phase::Refunding) => self.begin_refund(&order, &trade).await,
            Decision::Ok(Phase::Releasing) => {
                if let Some(invoice) = &supplied {
                    self.db.set_payout(&trade.id, invoice, None)?;
                }
                self.db
                    .set_trade_state(&trade.id, Phase::Releasing.as_str())?;
                Ok(Vec::new())
            }
            Decision::Ok(_) => Ok(Vec::new()),
            Decision::Reject(reason) => Ok(vec![cant_do(sender, Some(trade_id), reason)]),
            Decision::NoOp => Ok(Vec::new()),
        }
    }

    async fn on_cancel(&self, sender: &str, envelope: Envelope) -> Result<Vec<Outbound>> {
        if let Some(trade_id) = envelope.trade_id.clone() {
            return self.cancel_trade(sender, &trade_id).await;
        }
        let payload: CancelPayload = envelope
            .decode_payload()
            .unwrap_or(CancelPayload { order_id: None });
        let Some(order_id) = payload.order_id else {
            return Ok(vec![cant_do(
                sender,
                None,
                "order_id or trade_id is required",
            )]);
        };
        let Some(order) = self.db.get_order(&order_id)? else {
            return Ok(vec![cant_do(sender, None, "order not found")]);
        };
        if self.db.open_trade_for_order(&order.id)?.is_some() {
            return Ok(vec![cant_do(sender, None, "order has an open trade")]);
        }
        let actor = self.actor(&order.maker_nostr, None, sender);
        match apply_cancel(Phase::Pending, actor, None) {
            Decision::Ok(Phase::Canceled) => {
                if let Err(error) = self.db.cancel_order(&order.id) {
                    return Ok(vec![cant_do(sender, None, &error.to_string())]);
                }
                let order = self.require_order(&order.id)?;
                Ok(vec![
                    Outbound::PublicOrder(order.public()),
                    Outbound::Reply {
                        to: sender.to_string(),
                        envelope: Envelope::new(CANCELED),
                    },
                ])
            }
            Decision::Reject(reason) => Ok(vec![cant_do(sender, None, reason)]),
            _ => Ok(vec![cant_do(sender, None, "cannot cancel order")]),
        }
    }

    async fn cancel_trade(&self, sender: &str, trade_id: &str) -> Result<Vec<Outbound>> {
        let Some(trade) = self.db.get_trade(trade_id)? else {
            return Ok(vec![cant_do(
                sender,
                Some(trade_id.to_string()),
                "trade not found",
            )]);
        };
        let order = self.require_order(&trade.order_id)?;
        let actor = self.actor(&order.maker_nostr, Some(&trade.taker_nostr), sender);
        let invoice_status = if let Some(hash) = trade.hold_payment_hash.as_deref() {
            let invoice = self.fiber.get_invoice(hash).await?;
            InvoiceStatus::parse(&invoice.status)
        } else {
            Some(InvoiceStatus::Open)
        };
        match apply_cancel(trade.phase()?, actor, invoice_status) {
            Decision::Ok(Phase::Canceled) => {
                if invoice_status == Some(InvoiceStatus::Open) {
                    if let Some(hash) = trade.hold_payment_hash.as_deref() {
                        self.fiber.cancel_invoice(hash).await?;
                    }
                }
                self.complete_cancel(&order, &trade).await
            }
            Decision::Ok(Phase::Expired) => self.expire_trade(&order, &trade).await,
            Decision::Reject(reason) => {
                Ok(vec![cant_do(sender, Some(trade_id.to_string()), reason)])
            }
            _ => Ok(vec![cant_do(
                sender,
                Some(trade_id.to_string()),
                "cannot cancel",
            )]),
        }
    }

    async fn complete_cancel(&self, order: &Order, trade: &Trade) -> Result<Vec<Outbound>> {
        self.finish_terminal(order, trade, Phase::Canceled)?;
        if let Err(error) = self.drop_preimage(trade).await {
            warn!(%error, trade_id = %trade.id, "remove preimage");
        }
        let order = self.require_order(&order.id)?;
        let mut outbound =
            party_replies(&order, trade, Envelope::new(CANCELED).with_trade(&trade.id));
        outbound.insert(0, Outbound::PublicOrder(order.public()));
        Ok(outbound)
    }

    fn preimage_pending(&self, trade: &Trade) -> Result<bool> {
        let Some(hash) = trade.hold_payment_hash.as_deref() else {
            return Ok(false);
        };
        Ok(self.db.get_preimage(hash)?.is_some())
    }

    fn finish_terminal(&self, order: &Order, trade: &Trade, phase: Phase) -> Result<()> {
        let restore = phase.returns_slice().then_some(trade.shannons);
        self.db
            .finish_trade(&order.id, &trade.id, phase.as_str(), restore)
    }

    async fn refund_due_outbound(
        &self,
        order: &Order,
        trade: &Trade,
    ) -> Result<Option<Vec<Outbound>>> {
        let outbound = self.refund_if_due(order, trade).await?;
        if outbound.is_empty() {
            Ok(None)
        } else {
            Ok(Some(outbound))
        }
    }

    fn actor(&self, maker: &str, taker: Option<&str>, sender: &str) -> crate::Actor {
        actor_of(maker, taker, self.solver.as_deref(), sender)
    }

    fn with_solver(&self, order: &Order, trade: &Trade, envelope: Envelope) -> Vec<Outbound> {
        let mut outbound = party_replies(order, trade, envelope.clone());
        if let Some(solver) = &self.solver {
            outbound.push(Outbound::Reply {
                to: solver.clone(),
                envelope,
            });
        }
        outbound
    }

    fn require_order(&self, id: &str) -> Result<Order> {
        self.db
            .get_order(id)?
            .ok_or_else(|| anyhow!("order {id} not found"))
    }
}

impl<F: FiberRpc + 'static> Engine<F> {
    pub fn spawn_poll(
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
                        PollKind::Hold => warn!(%error, trade_id, "hold poll"),
                        PollKind::Payout => {
                            warn!(%error, trade_id, "payout poll");
                            break;
                        }
                    },
                }
                match engine.db.get_trade(&trade_id) {
                    Ok(Some(trade)) => {
                        let stay = match (kind, trade.phase().ok()) {
                            (PollKind::Hold, Some(phase)) if phase.watched() => true,
                            (PollKind::Payout, Some(Phase::Releasing)) => true,
                            _ => false,
                        };
                        if !stay {
                            break;
                        }
                    }
                    _ => break,
                }
                tokio::time::sleep(Duration::from_secs(FIBER_POLL_SECS)).await;
            }
            polling.lock().expect("poll").remove(&key);
        });
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn party_replies(order: &Order, trade: &Trade, envelope: Envelope) -> Vec<Outbound> {
    vec![
        Outbound::Reply {
            to: order.maker_nostr.clone(),
            envelope: envelope.clone(),
        },
        Outbound::Reply {
            to: trade.taker_nostr.clone(),
            envelope,
        },
    ]
}

fn cant_do(to: &str, trade_id: Option<String>, reason: &str) -> Outbound {
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

fn sender_reason(error: &anyhow::Error) -> String {
    format!("taker payout failed: {error}")
}

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
    use crate::fiber::{InvoiceCreated, InvoiceInfo, ParsedInvoice, PaymentInfo};
    use crate::util::support::{
        TestContext, cant_do_reason, fiat_sent, open_order, outbound_replies, replies_contain,
        take, wait_invoice_status,
    };
    use crate::{
        CANCEL, DISPUTE, DISPUTED, EXPIRED, FIAT_WINDOW_SECS, LOCKED, NEW_INVOICE, OrderStatus,
        REFUNDING, RELEASE, RESOLVE, ResolvePayload, SETTLED, TAKE_SELL, WAITING_FIAT,
    };
    use async_trait::async_trait;
    use std::sync::Mutex as StdMutex;

    struct FakeFiber {
        status: StdMutex<String>,
        removed: StdMutex<bool>,
        preimages: StdMutex<HashSet<String>>,
        payment_status: StdMutex<String>,
        parsed_amount: StdMutex<u128>,
    }

    #[async_trait]
    impl FiberRpc for FakeFiber {
        async fn new_hold_invoice(&self, _: u128, _: &str, _: &str) -> Result<InvoiceCreated> {
            bail!("unused")
        }
        async fn new_payout_invoice(&self, _: u128, _: &str) -> Result<InvoiceCreated> {
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
        async fn send_payment(&self, _: &str) -> Result<PaymentInfo> {
            bail!("unused")
        }
        async fn get_payment(&self, payment_hash: &str) -> Result<PaymentInfo> {
            Ok(PaymentInfo {
                payment_hash: payment_hash.to_string(),
                status: self.payment_status.lock().expect("payment").clone(),
            })
        }
        async fn parse_invoice(&self, _: &str) -> Result<ParsedInvoice> {
            Ok(ParsedInvoice {
                payment_hash: "parsed".into(),
                currency: "Fibt".into(),
                amount: Some(*self.parsed_amount.lock().expect("amount")),
            })
        }
        async fn node_pubkey(&self) -> Result<String> {
            bail!("unused")
        }
        fn currency(&self) -> &str {
            "Fibt"
        }
    }

    fn fake_engine(status: &str) -> (Engine<FakeFiber>, Arc<FakeFiber>) {
        let fiber = Arc::new(FakeFiber {
            status: StdMutex::new(status.to_string()),
            removed: StdMutex::new(false),
            preimages: StdMutex::new(HashSet::from(["hash".to_string()])),
            payment_status: StdMutex::new("Inflight".to_string()),
            parsed_amount: StdMutex::new(100_000_000),
        });
        let db = Arc::new(Db::open_in_memory().unwrap());
        db.insert_order(&Order {
            id: "order".into(),
            maker_nostr: "seller".into(),
            maker_fiber: "fiber-seller".into(),
            available_shannons: 1_000_000_000,
            fiat_currency_code: "NGN".into(),
            price_per_ckb: "1000".into(),
            min: "1000".into(),
            max: "10000".into(),
            payment_method: "bank".into(),
            status: OrderStatus::Open.as_str().into(),
        })
        .unwrap();
        db.insert_trade(&Trade {
            id: "trade".into(),
            order_id: "order".into(),
            taker_nostr: "buyer".into(),
            taker_fiber: "fiber-buyer".into(),
            fiat_amount: "1000".into(),
            shannons: 100_000_000,
            state: Phase::WaitingHold.as_str().into(),
            hold_payment_hash: Some("hash".into()),
            hold_invoice: Some("inv".into()),
            payout_invoice: None,
            payout_payment_hash: None,
            hold_received_at: None,
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
        let trade = engine.db.get_trade("trade").unwrap().unwrap();
        assert!(trade.hold_received_at.is_some());
        engine
            .db
            .set_hold_received_at("trade", unix_now() - (FIAT_WINDOW_SECS as i64) - 1)
            .unwrap();
        let refunding = engine.on_hold_status("trade").await.unwrap();
        assert!(replies_contain(&refunding, REFUNDING));
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
    async fn solver_seller_win_refunds_and_buyer_win_needs_invoice() {
        let (engine, fiber) = fake_engine("Received");
        engine.on_hold_status("trade").await.unwrap();
        let disputed = engine
            .handle("buyer", Envelope::new(DISPUTE).with_trade("trade"))
            .await
            .unwrap();
        assert!(replies_contain(&disputed, DISPUTED));
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
        assert!(
            engine
                .db
                .get_trade("trade")
                .unwrap()
                .unwrap()
                .payout_invoice
                .is_none()
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
        assert!(*fiber.removed.lock().unwrap());
        assert_eq!(
            engine.db.get_trade("trade").unwrap().unwrap().state,
            Phase::Refunding.as_str()
        );
        assert!(!fiber.preimages.lock().unwrap().contains("hash"));
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
            .set_payout("trade", "buyer-invoice", Some("payout"))
            .unwrap();
        *fiber.payment_status.lock().unwrap() = "Success".into();
        let settled = engine.on_payout_status("trade").await.unwrap();
        assert!(replies_contain(&settled, SETTLED));
        assert!(fiber.preimages.lock().unwrap().contains("hash"));
        assert!(!*fiber.removed.lock().unwrap());
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
    async fn an_open_hold_without_a_preimage_refuses_to_boot() {
        let (engine, _) = fake_engine("Received");
        engine.db.delete_preimage("hash").unwrap();
        engine
            .db
            .set_trade_state("trade", Phase::WaitingFiat.as_str())
            .unwrap();
        assert!(engine.arm_watchtower().await.is_err());
    }

    #[tokio::test]
    #[ignore = "requires a running Fiber node (TWINE_RPC)"]
    async fn take_creates_hold_and_registers_preimage() {
        let ctx = TestContext::new().await;
        let twine_pk = ctx.fiber.node_pubkey().await.unwrap();
        let order_id = open_order(&ctx.engine, "maker", &twine_pk).await;
        let (trade_id, invoice, amount) = take(&ctx.engine, "taker", &order_id, &twine_pk).await;
        assert!(!invoice.is_empty());
        assert_eq!(amount, "100000000");
        let trade = ctx.engine.db.get_trade(&trade_id).unwrap().unwrap();
        let hash = trade.hold_payment_hash.unwrap();
        assert!(ctx.engine.db.get_preimage(&hash).unwrap().is_some());
        let info = ctx.fiber.get_invoice(&hash).await.unwrap();
        assert_eq!(info.status, "Open");
    }

    #[tokio::test]
    #[ignore = "requires a running Fiber node (TWINE_RPC)"]
    async fn pay_invoice_never_includes_preimage() {
        let ctx = TestContext::new().await;
        let twine_pk = ctx.fiber.node_pubkey().await.unwrap();
        let order_id = open_order(&ctx.engine, "maker", &twine_pk).await;
        let outbound = ctx
            .engine
            .handle(
                "taker",
                Envelope::new(TAKE_SELL)
                    .with_payload(TakeSellPayload {
                        order_id,
                        fiat_amount: "1000".into(),
                        taker_fiber_pubkey: twine_pk,
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
        let twine_pk = ctx.fiber.node_pubkey().await.unwrap();
        let order_id = open_order(&ctx.engine, "maker", &twine_pk).await;
        let (trade_id, _, _) = take(&ctx.engine, "taker", &order_id, &twine_pk).await;
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
            .handle("maker", Envelope::new(CANCEL).with_trade(&trade_id))
            .await
            .unwrap();
        assert!(replies_contain(&canceled, CANCELED));
        let info = ctx.fiber.get_invoice(&hash).await.unwrap();
        assert_eq!(info.status, "Cancelled");
    }

    #[tokio::test]
    #[ignore = "requires a running Fiber node (TWINE_RPC)"]
    async fn lock_is_noop_while_hold_is_open() {
        let ctx = TestContext::new().await;
        let twine_pk = ctx.fiber.node_pubkey().await.unwrap();
        let order_id = open_order(&ctx.engine, "maker", &twine_pk).await;
        let (trade_id, _, _) = take(&ctx.engine, "taker", &order_id, &twine_pk).await;
        let locked = ctx
            .engine
            .handle("maker", Envelope::new(LOCKED).with_trade(&trade_id))
            .await
            .unwrap();
        assert!(locked.is_empty());
        let again = ctx
            .engine
            .handle("maker", Envelope::new(LOCKED).with_trade(&trade_id))
            .await
            .unwrap();
        assert!(again.is_empty());
    }

    #[tokio::test]
    #[ignore = "requires a running Fiber node (TWINE_RPC)"]
    async fn lock_poll_and_release_pay_taker_first() {
        let ctx = TestContext::new().await;
        let twine_pk = ctx.fiber.node_pubkey().await.unwrap();
        let order_id = open_order(&ctx.engine, "maker", &twine_pk).await;
        let (trade_id, _, _) = take(&ctx.engine, "taker", &order_id, &twine_pk).await;
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
            .new_payout_invoice(100_000_000, "twine taker")
            .await
            .unwrap();
        fiat_sent(&ctx.engine, "taker", &trade_id, &payout.invoice).await;

        ctx.engine
            .handle("maker", Envelope::new(RELEASE).with_trade(&trade_id))
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
        let twine_pk = ctx.fiber.node_pubkey().await.unwrap();
        let order_id = open_order(&ctx.engine, "maker", &twine_pk).await;
        let (trade_id, _, _) = take(&ctx.engine, "taker", &order_id, &twine_pk).await;
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
            .handle("maker", Envelope::new(CANCEL).with_trade(&trade_id))
            .await
            .unwrap();
        assert!(cant_do_reason(&rejected).contains("timelock"));
        assert!(replies_contain(&rejected, CANT_DO));
    }
}
