use crate::db::Db;
use crate::fiber::FiberRpc;
use crate::nostr::publish_outbounds;
use crate::types::{Order, OrderStatus, Trade};
use crate::{
    CANCELED, CANT_DO, CancelPayload, CantDoPayload, ClientAction, Decision, EXPIRED, Envelope,
    FIAT_SENT_OK, FIBER_POLL_SECS, FiatSentPayload, InvoiceStatus, NewOrderPayload, Outbound,
    PAY_INVOICE, PayInvoicePayload, Phase, SETTLED, TakeSellPayload, WAITING_FIAT, actor_of,
    apply_cancel, apply_expired, apply_fiat_sent, apply_hold_received, apply_locked, apply_release,
    apply_release_failed, apply_release_succeeded, apply_take, hex_bytes, validate_new_order,
    validate_take,
};
use anyhow::{Result, anyhow, bail};
use nostr_sdk::prelude::*;
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;
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
}

impl<F> Clone for Engine<F> {
    fn clone(&self) -> Self {
        Self {
            db: self.db.clone(),
            fiber: self.fiber.clone(),
        }
    }
}

impl<F: FiberRpc> Engine<F> {
    pub fn new(db: Arc<Db>, fiber: Arc<F>) -> Self {
        Self { db, fiber }
    }

    pub fn resume_trade_ids(&self) -> Result<(Vec<String>, Vec<String>)> {
        let holds = self
            .db
            .trades_in_states(&[Phase::WaitingHold])?
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
        };
        self.db.insert_trade(&trade)?;
        self.db
            .set_available(&order.id, order.available_shannons - shannons)?;
        self.db.insert_preimage(&payment_hash, &preimage)?;

        let created = match self
            .fiber
            .new_hold_invoice(shannons, &payment_hash, &format!("twine {trade_id}"))
            .await
        {
            Ok(created) => created,
            Err(error) => {
                self.rollback_take(&order, &trade, &payment_hash)?;
                return Ok(vec![cant_do(
                    sender,
                    Some(trade_id),
                    &format!("hold invoice failed: {error}"),
                )]);
            }
        };
        if let Err(error) = self.fiber.create_preimage(&payment_hash, &preimage).await {
            let _ = self.fiber.cancel_invoice(&payment_hash).await;
            self.rollback_take(&order, &trade, &payment_hash)?;
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

    fn rollback_take(&self, order: &Order, trade: &Trade, payment_hash: &str) -> Result<()> {
        self.db.delete_preimage(payment_hash)?;
        self.db.delete_trade(&trade.id)?;
        self.db.set_available(&order.id, order.available_shannons)?;
        Ok(())
    }

    async fn on_locked(&self, sender: &str, envelope: Envelope) -> Result<Vec<Outbound>> {
        let Some(trade_id) = envelope.trade_id.clone() else {
            return Ok(vec![cant_do(sender, None, "trade_id is required")]);
        };
        let Some(trade) = self.db.get_trade(&trade_id)? else {
            return Ok(vec![cant_do(sender, Some(trade_id), "trade not found")]);
        };
        let order = self.require_order(&trade.order_id)?;
        let actor = actor_of(&order.maker_nostr, Some(&trade.taker_nostr), sender);
        match apply_locked(trade.phase()?, actor) {
            Decision::NoOp => Ok(Vec::new()),
            Decision::Reject(reason) => Ok(vec![cant_do(sender, Some(trade_id), reason)]),
            Decision::Ok(_) => Ok(Vec::new()),
        }
    }

    pub async fn on_hold_status(&self, trade_id: &str) -> Result<Vec<Outbound>> {
        let Some(trade) = self.db.get_trade(trade_id)? else {
            bail!("trade {trade_id} not found");
        };
        if trade.phase()? != Phase::WaitingHold {
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
        match status {
            InvoiceStatus::Received => match apply_hold_received(Phase::WaitingHold) {
                Decision::Ok(phase) => {
                    self.db.set_trade_state(&trade.id, phase.as_str())?;
                    Ok(party_replies(
                        &order,
                        &trade,
                        Envelope::new(WAITING_FIAT).with_trade(&trade.id),
                    ))
                }
                other => bail!("invalid hold received: {other:?}"),
            },
            InvoiceStatus::Expired => self.expire_trade(&order, &trade).await,
            InvoiceStatus::Open | InvoiceStatus::Cancelled | InvoiceStatus::Paid => Ok(Vec::new()),
        }
    }

    async fn expire_trade(&self, order: &Order, trade: &Trade) -> Result<Vec<Outbound>> {
        match apply_expired(trade.phase()?) {
            Decision::Ok(phase) => {
                self.finish_terminal(order, trade, phase)?;
                Ok(party_replies(
                    order,
                    trade,
                    Envelope::new(EXPIRED).with_trade(&trade.id),
                ))
            }
            Decision::Reject(_) => Ok(Vec::new()),
            Decision::NoOp => Ok(Vec::new()),
        }
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
        let actor = actor_of(&order.maker_nostr, Some(&trade.taker_nostr), sender);
        match apply_fiat_sent(trade.phase()?, actor) {
            Decision::Ok(phase) => {
                self.db.set_payout(&trade.id, &payload.invoice, None)?;
                self.db.set_trade_state(&trade.id, phase.as_str())?;
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
        let actor = actor_of(&order.maker_nostr, Some(&trade.taker_nostr), sender);
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
        let invoice = trade
            .payout_invoice
            .clone()
            .ok_or_else(|| anyhow!("missing payout invoice"))?;
        if trade.payout_payment_hash.is_none() {
            self.fiber.parse_invoice(&invoice).await?;
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
        let payment_hash = trade
            .hold_payment_hash
            .as_deref()
            .ok_or_else(|| anyhow!("missing hold hash"))?;
        let preimage = self
            .db
            .get_preimage(payment_hash)?
            .ok_or_else(|| anyhow!("missing preimage"))?;
        self.fiber.settle_invoice(payment_hash, &preimage).await?;
        let _ = self.fiber.remove_preimage(payment_hash).await;
        self.db.delete_preimage(payment_hash)?;
        match apply_release_succeeded(Phase::Releasing) {
            Decision::Ok(phase) => {
                self.db.set_trade_state(&trade.id, phase.as_str())?;
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
                Ok(vec![cant_do(
                    &order.maker_nostr,
                    Some(trade.id.clone()),
                    reason,
                )])
            }
            other => bail!("invalid payout fail: {other:?}"),
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
        let actor = actor_of(&order.maker_nostr, None, sender);
        match apply_cancel(Phase::Pending, actor, None) {
            Decision::Ok(Phase::Canceled) => {
                self.db
                    .set_order_status(&order.id, OrderStatus::Canceled.as_str())?;
                self.db.set_available(&order.id, 0)?;
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
        let actor = actor_of(&order.maker_nostr, Some(&trade.taker_nostr), sender);
        let invoice_status = if let Some(hash) = trade.hold_payment_hash.as_deref() {
            let invoice = self.fiber.get_invoice(hash).await?;
            InvoiceStatus::parse(&invoice.status)
        } else {
            Some(InvoiceStatus::Open)
        };
        match apply_cancel(trade.phase()?, actor, invoice_status) {
            Decision::Ok(Phase::Canceled) => {
                if let Some(hash) = trade.hold_payment_hash.as_deref() {
                    if invoice_status == Some(InvoiceStatus::Open) {
                        self.fiber.cancel_invoice(hash).await?;
                    }
                    self.db.delete_preimage(hash)?;
                }
                self.finish_terminal(&order, &trade, Phase::Canceled)?;
                let order = self.require_order(&order.id)?;
                let mut outbound = party_replies(
                    &order,
                    &trade,
                    Envelope::new(CANCELED).with_trade(&trade.id),
                );
                outbound.insert(0, Outbound::PublicOrder(order.public()));
                Ok(outbound)
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

    fn finish_terminal(&self, order: &Order, trade: &Trade, phase: Phase) -> Result<()> {
        self.db.set_trade_state(&trade.id, phase.as_str())?;
        if phase.returns_slice() {
            let current = self.require_order(&order.id)?;
            self.db
                .set_available(&order.id, current.available_shannons + trade.shannons)?;
        }
        Ok(())
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
                    Ok(outbound) if !outbound.is_empty() => {
                        let _ = publish_outbounds(&client, &keys, &outbound).await;
                        break;
                    }
                    Ok(_) => {}
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
                            (PollKind::Hold, Some(Phase::WaitingHold)) => true,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::support::{
        TestContext, cant_do_reason, fiat_sent, open_order, outbound_replies, replies_contain,
        take, wait_invoice_status,
    };
    use crate::{CANCEL, LOCKED, RELEASE, TAKE_SELL};

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
