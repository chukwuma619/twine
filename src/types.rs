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
pub enum Side {
    Sell,
    Buy,
}

impl Side {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sell => "sell",
            Self::Buy => "buy",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "sell" => Some(Self::Sell),
            "buy" => Some(Self::Buy),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Who is allowed to move an order or a trade.
/// The maker posted it. On a trade, seller and buyer are the CKB sides.
pub enum Actor {
    Maker,
    Seller,
    Buyer,
    Solver,
    Other,
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
    /// `sell` or `buy`. A sell post offers CKB. A buy post bids for CKB.
    pub side: String,
    pub fiber_pubkey: String,
    pub available_ckb: String,
    pub fiat_currency: String,
    pub price_per_ckb: String,
    pub min: String,
    pub max: String,
    pub payment_method: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Someone joining a post. `fiber_pubkey` is the taker's Fiber key.
/// On a sell post the taker buys CKB. On a buy post the taker sells CKB.
pub struct TakePayload {
    pub order_id: String,
    pub fiat_amount: String,
    pub fiber_pubkey: String,
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
    pub seller_nostr: String,
    pub seller_fiber: String,
    pub buyer_nostr: String,
    pub buyer_fiber: String,
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
            })
            .unwrap();
        let json = serde_json::to_string(&env).unwrap();
        let parsed: Envelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.client_action(), Some(ClientAction::NewOrder));
        let payload: NewOrderPayload = parsed.decode_payload().unwrap();
        assert_eq!(payload.fiat_currency, "NGN");
    }

    #[test]
    fn a_post_names_its_side_and_maker() {
        let sell = sample_order("sell", "seller", "fiber-seller");
        assert_eq!(sell.public().side, "sell");
        assert_eq!(sell.public().maker_nostr_pubkey, "seller");
        assert_eq!(sell.public().maker_fiber_pubkey, "fiber-seller");
        assert_eq!(sell.public().hold_hours, 16);
        let buy = sample_order("buy", "buyer", "fiber-buyer");
        assert_eq!(buy.public().side, "buy");
        assert_eq!(buy.public().maker_nostr_pubkey, "buyer");
        assert_eq!(Side::parse("sell"), Some(Side::Sell));
        assert_eq!(Side::parse("buy"), Some(Side::Buy));
        assert_eq!(Side::parse("ask"), None);
    }

    fn sample_order(side: &str, maker: &str, fiber: &str) -> Order {
        Order {
            id: "order".into(),
            side: side.into(),
            maker_nostr: maker.into(),
            maker_fiber: fiber.into(),
            available_shannons: 1,
            fiat_currency: "NGN".into(),
            price_per_ckb: "1".into(),
            min: "1".into(),
            max: "2".into(),
            payment_method: "bank".into(),
            status: "open".into(),
            hold_secs: 16 * 3_600,
        }
    }
}
