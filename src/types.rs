use crate::constant::{
    CANCEL, CANCELED, CANT_DO, DISPUTE, DISPUTED, EXPIRED, FIAT_SENT, FIAT_SENT_OK, NEW_INVOICE,
    NEW_ORDER, PAY_INVOICE, REFUNDING, RELEASE, RESOLVE, SETTLED, TAKE, WAITING_FIAT,
};
use crate::util::shannons_to_ckb_string;
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

/// Trade and order phase. An order with no trade is `Pending`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Pending,
    WaitingHold,
    WaitingFiat,
    FiatSent,
    Releasing,
    AwaitingInvoice,
    Disputed,
    Refunding,
    Settled,
    Canceled,
    Expired,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::WaitingHold => "waiting-hold",
            Self::WaitingFiat => "waiting-fiat",
            Self::FiatSent => "fiat-sent",
            Self::Releasing => "releasing",
            Self::AwaitingInvoice => "awaiting-invoice",
            Self::Disputed => "disputed",
            Self::Refunding => "refunding",
            Self::Settled => "settled",
            Self::Canceled => "canceled",
            Self::Expired => "expired",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "waiting-hold" => Some(Self::WaitingHold),
            "waiting-fiat" => Some(Self::WaitingFiat),
            "fiat-sent" => Some(Self::FiatSent),
            "releasing" => Some(Self::Releasing),
            "awaiting-invoice" => Some(Self::AwaitingInvoice),
            "disputed" => Some(Self::Disputed),
            "refunding" => Some(Self::Refunding),
            "settled" => Some(Self::Settled),
            "canceled" => Some(Self::Canceled),
            "expired" => Some(Self::Expired),
            _ => None,
        }
    }

    /// Hold is still locked, or the daemon is still waiting on that hold.
    pub fn watched(self) -> bool {
        matches!(
            self,
            Self::WaitingHold
                | Self::WaitingFiat
                | Self::FiatSent
                | Self::Releasing
                | Self::AwaitingInvoice
                | Self::Disputed
                | Self::Refunding
        )
    }

    pub fn watched_phases() -> &'static [Phase] {
        &[
            Self::WaitingHold,
            Self::WaitingFiat,
            Self::FiatSent,
            Self::Releasing,
            Self::AwaitingInvoice,
            Self::Disputed,
            Self::Refunding,
        ]
    }

    pub fn returns_slice(self) -> bool {
        matches!(self, Self::Canceled | Self::Expired)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderStatus {
    Open,
    Canceled,
}

impl OrderStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Canceled => "canceled",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "open" => Some(Self::Open),
            "canceled" => Some(Self::Canceled),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Who is allowed to move a trade. `Maker` is only the order owner, for canceling
/// an ad that has no trade yet. Once a trade exists, actions follow `Seller` and
/// `Buyer`, which swap with the order side.
pub enum Actor {
    Maker,
    Seller,
    Buyer,
    Solver,
    Other,
}

/// A sell offers CKB. A buy offers fiat and asks for CKB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Buy => "buy",
            Self::Sell => "sell",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "buy" => Some(Self::Buy),
            "sell" => Some(Self::Sell),
            _ => None,
        }
    }
}

fn default_order_side() -> String {
    Side::Sell.as_str().to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvoiceStatus {
    Open,
    Cancelled,
    Expired,
    Received,
    Paid,
}

impl InvoiceStatus {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "Open" => Some(Self::Open),
            "Cancelled" | "Canceled" => Some(Self::Cancelled),
            "Expired" => Some(Self::Expired),
            "Received" => Some(Self::Received),
            "Paid" => Some(Self::Paid),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Ok(Phase),
    NoOp,
    Reject(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientAction {
    NewOrder,
    Take,
    FiatSent,
    Release,
    Cancel,
    Dispute,
    Resolve,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyAction {
    PayInvoice,
    WaitingFiat,
    FiatSentOk,
    NewInvoice,
    Disputed,
    Refunding,
    Settled,
    Canceled,
    Expired,
    CantDo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trade_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
}

impl Envelope {
    pub fn new(action: impl Into<String>) -> Self {
        Self {
            action: action.into(),
            trade_id: None,
            payload: None,
        }
    }

    pub fn with_trade(mut self, trade_id: impl Into<String>) -> Self {
        self.trade_id = Some(trade_id.into());
        self
    }

    pub fn with_payload<T: Serialize>(mut self, payload: T) -> Result<Self, serde_json::Error> {
        self.payload = Some(serde_json::to_value(payload)?);
        Ok(self)
    }

    pub fn client_action(&self) -> Option<ClientAction> {
        match self.action.as_str() {
            NEW_ORDER => Some(ClientAction::NewOrder),
            TAKE => Some(ClientAction::Take),
            FIAT_SENT => Some(ClientAction::FiatSent),
            RELEASE => Some(ClientAction::Release),
            CANCEL => Some(ClientAction::Cancel),
            DISPUTE => Some(ClientAction::Dispute),
            RESOLVE => Some(ClientAction::Resolve),
            _ => None,
        }
    }

    pub fn reply_action(&self) -> Option<ReplyAction> {
        match self.action.as_str() {
            PAY_INVOICE => Some(ReplyAction::PayInvoice),
            WAITING_FIAT => Some(ReplyAction::WaitingFiat),
            FIAT_SENT_OK => Some(ReplyAction::FiatSentOk),
            NEW_INVOICE => Some(ReplyAction::NewInvoice),
            DISPUTED => Some(ReplyAction::Disputed),
            REFUNDING => Some(ReplyAction::Refunding),
            SETTLED => Some(ReplyAction::Settled),
            CANCELED => Some(ReplyAction::Canceled),
            EXPIRED => Some(ReplyAction::Expired),
            CANT_DO => Some(ReplyAction::CantDo),
            _ => None,
        }
    }

    pub fn decode_payload<T: for<'de> Deserialize<'de>>(&self) -> Result<T, serde_json::Error> {
        match &self.payload {
            Some(value) => serde_json::from_value(value.clone()),
            None => serde_json::from_value(serde_json::Value::Null),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewOrderPayload {
    /// `buy` or `sell`. Omitted means `sell`.
    #[serde(default = "default_order_side")]
    pub side: String,
    pub fiber_pubkey: String,
    pub available_ckb: String,
    pub fiat_currency: String,
    pub price_per_ckb: String,
    pub min: String,
    pub max: String,
    pub payment_method: String,
    pub hold_hours: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Works for a buy or a sell. The order's `side` decides who locks.
pub struct TakePayload {
    pub order_id: String,
    pub fiat_amount: String,
    pub taker_fiber_pubkey: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FiatSentPayload {
    pub invoice: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisputePayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invoice: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvePayload {
    pub winner: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invoice: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewInvoicePayload {
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PayInvoicePayload {
    pub invoice: String,
    pub amount_shannons: String,
    pub order_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CantDoPayload {
    pub reason: String,
}

#[derive(Debug, Clone)]
pub enum Outbound {
    Reply { to: String, envelope: Envelope },
    PublicOrder(PublicOrder),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicOrder {
    pub order_id: String,
    pub side: String,
    pub maker_nostr_pubkey: String,
    pub maker_fiber_pubkey: String,
    pub available_ckb: String,
    pub fiat_currency: String,
    pub price_per_ckb: String,
    pub min: String,
    pub max: String,
    pub payment_method: String,
    pub hold_hours: u64,
}

pub struct Order {
    pub id: String,
    pub side: String,
    pub maker_nostr: String,
    pub maker_fiber: String,
    pub available_shannons: u128,
    pub fiat_currency: String,
    pub price_per_ckb: String,
    pub min: String,
    pub max: String,
    pub payment_method: String,
    pub status: String,
    pub hold_secs: u64,
}

impl Order {
    pub fn order_side(&self) -> Result<Side> {
        Side::parse(&self.side).ok_or_else(|| anyhow!("unknown order side {}", self.side))
    }

    /// Nostr key that pays the hold invoice and may release it.
    pub fn seller_nostr<'a>(&'a self, trade: &'a Trade) -> Result<&'a str> {
        match self.order_side()? {
            Side::Sell => Ok(self.maker_nostr.as_str()),
            Side::Buy => Ok(trade.taker_nostr.as_str()),
        }
    }

    /// Nostr key that pays fiat and submits the payout invoice.
    pub fn buyer_nostr<'a>(&'a self, trade: &'a Trade) -> Result<&'a str> {
        match self.order_side()? {
            Side::Sell => Ok(trade.taker_nostr.as_str()),
            Side::Buy => Ok(self.maker_nostr.as_str()),
        }
    }

    pub fn public(&self) -> PublicOrder {
        PublicOrder {
            order_id: self.id.clone(),
            side: self.side.clone(),
            maker_nostr_pubkey: self.maker_nostr.clone(),
            maker_fiber_pubkey: self.maker_fiber.clone(),
            available_ckb: shannons_to_ckb_string(self.available_shannons),
            fiat_currency: self.fiat_currency.clone(),
            price_per_ckb: self.price_per_ckb.clone(),
            min: self.min.clone(),
            max: self.max.clone(),
            payment_method: self.payment_method.clone(),
            hold_hours: self.hold_secs / 3_600,
        }
    }

    pub fn is_open(&self) -> bool {
        OrderStatus::parse(&self.status) == Some(OrderStatus::Open)
    }
}

pub struct Trade {
    pub id: String,
    pub order_id: String,
    pub taker_nostr: String,
    pub taker_fiber: String,
    pub fiat_amount: String,
    pub shannons: u128,
    pub state: String,
    pub hold_payment_hash: Option<String>,
    pub hold_invoice: Option<String>,
    pub payout_invoice: Option<String>,
    pub payout_payment_hash: Option<String>,
    /// Unix seconds when the hold invoice became `Received`.
    pub hold_received_at: Option<i64>,
}

impl Trade {
    pub fn phase(&self) -> Result<Phase> {
        Phase::parse(&self.state).ok_or_else(|| anyhow!("unknown trade state {}", self.state))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_roundtrip() {
        let env = Envelope::new(NEW_ORDER)
            .with_payload(NewOrderPayload {
                side: "sell".into(),
                fiber_pubkey: "pk".into(),
                available_ckb: "10".into(),
                fiat_currency: "NGN".into(),
                price_per_ckb: "1500".into(),
                min: "1000".into(),
                max: "5000".into(),
                payment_method: "bank".into(),
                hold_hours: 36,
            })
            .unwrap();
        let json = serde_json::to_string(&env).unwrap();
        let parsed: Envelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.client_action(), Some(ClientAction::NewOrder));
        let payload: NewOrderPayload = parsed.decode_payload().unwrap();
        assert_eq!(payload.fiat_currency, "NGN");
    }

    #[test]
    fn a_missing_side_is_a_sell_and_a_buy_locks_from_the_taker() {
        let parsed: Envelope = serde_json::from_str(
            r#"{"action":"new-order","payload":{"fiber_pubkey":"pk","available_ckb":"10","fiat_currency":"NGN","price_per_ckb":"1500","min":"1000","max":"5000","payment_method":"bank","hold_hours":16}}"#,
        )
        .unwrap();
        let payload: NewOrderPayload = parsed.decode_payload().unwrap();
        assert_eq!(payload.side, "sell");
        assert_eq!(payload.hold_hours, 16);

        let order = Order {
            id: "order".into(),
            side: "buy".into(),
            maker_nostr: "maker".into(),
            maker_fiber: "fiber-maker".into(),
            available_shannons: 1,
            fiat_currency: "NGN".into(),
            price_per_ckb: "1".into(),
            min: "1".into(),
            max: "2".into(),
            payment_method: "bank".into(),
            status: "open".into(),
            hold_secs: 16 * 3_600,
        };
        let trade = Trade {
            id: "trade".into(),
            order_id: "order".into(),
            taker_nostr: "taker".into(),
            taker_fiber: "fiber-taker".into(),
            fiat_amount: "1".into(),
            shannons: 1,
            state: "waiting-hold".into(),
            hold_payment_hash: None,
            hold_invoice: None,
            payout_invoice: None,
            payout_payment_hash: None,
            hold_received_at: None,
        };
        assert_eq!(order.seller_nostr(&trade).unwrap(), "taker");
        assert_eq!(order.buyer_nostr(&trade).unwrap(), "maker");
        assert_eq!(order.public().side, "buy");
        assert_eq!(order.public().hold_hours, 16);
    }
}
