use crate::constant::{
    CANCEL, DISPUTE, FIAT_SENT, INVOICE_EXPIRY_SECS, MY_TRADES, NEW_ORDER, PAYOUT_INVOICE, RELEASE,
    RELEASE_GRACE_SECS, RESOLVE, TAKE,
};
use crate::util::{fiat_deadline_secs, payout_deadline_secs, shannons_to_ckb_string};
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Pending,
    WaitingInvoice,
    WaitingHold,
    AwaitingRoute,
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
            Self::WaitingInvoice => "waiting-invoice",
            Self::WaitingHold => "waiting-hold",
            Self::AwaitingRoute => "awaiting-route",
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
            "waiting-invoice" => Some(Self::WaitingInvoice),
            "waiting-hold" => Some(Self::WaitingHold),
            "awaiting-route" => Some(Self::AwaitingRoute),
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
                | Self::AwaitingRoute
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
            Self::AwaitingRoute,
            Self::WaitingFiat,
            Self::FiatSent,
            Self::Releasing,
            Self::AwaitingInvoice,
            Self::Disputed,
            Self::Refunding,
        ]
    }

    /// A slice is reserved. Includes `waiting-invoice`, where no hold exists yet.
    /// Another take can use the CKB still available. The post cannot be canceled.
    pub fn open_phases() -> &'static [Phase] {
        &[
            Self::WaitingInvoice,
            Self::WaitingHold,
            Self::AwaitingRoute,
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

/// The maker posted the order. On a trade, seller and buyer are the CKB sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    PayoutInvoice,
    FiatSent,
    Release,
    Cancel,
    Dispute,
    Resolve,
    MyTrades,
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
            PAYOUT_INVOICE => Some(ClientAction::PayoutInvoice),
            FIAT_SENT => Some(ClientAction::FiatSent),
            RELEASE => Some(ClientAction::Release),
            CANCEL => Some(ClientAction::Cancel),
            DISPUTE => Some(ClientAction::Dispute),
            RESOLVE => Some(ClientAction::Resolve),
            MY_TRADES => Some(ClientAction::MyTrades),
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
    /// `sell`. A post offers CKB.
    pub side: String,
    pub available_ckb: String,
    pub fiat_currency: String,
    pub price_per_ckb: String,
    pub min: String,
    pub max: String,
}

/// Someone joining a sell post. The taker buys CKB.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TakePayload {
    pub order_id: String,
    pub fiat_amount: String,
    /// Buyer's payout invoice. Required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invoice: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PayoutInvoicePayload {
    pub invoice: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NeedInvoicePayload {
    pub amount_shannons: String,
    pub fiat_amount: String,
    pub fiat_currency: String,
    pub seller_nostr: String,
    pub buyer_nostr: String,
    /// Unix seconds by which the buyer must send the payout invoice.
    pub submit_by: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FiatSentPayload {
    /// Empty keeps the invoice already stored on the trade.
    #[serde(default)]
    pub invoice: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisputePayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invoice: Option<String>,
    /// NIP-44 v2 ciphertexts from this party to the solver.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub solver_evidence: Vec<String>,
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
    pub seller_nostr: String,
    pub buyer_nostr: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock_by: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_ends_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CantDoPayload {
    pub reason: String,
}

#[derive(Debug, Clone)]
pub enum Outbound {
    Reply { to: String, envelope: Envelope },
    PublicOrder(PublicOrder),
    TradeRecord(TradeRecord),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicOrder {
    pub order_id: String,
    pub side: String,
    pub maker_nostr_pubkey: String,
    pub available_ckb: String,
    pub fiat_currency: String,
    pub price_per_ckb: String,
    pub min: String,
    pub max: String,
    pub hold_hours: u64,
    /// `open` or `canceled`. A filled post stays `open` with `available_ckb` at 0.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserved_ckb: Option<String>,
}

/// One published outcome for a trade.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TradeRecord {
    pub trade_id: String,
    pub seller_nostr: String,
    pub buyer_nostr: String,
    pub fiat_amount: String,
    pub fiat_currency: String,
    /// `fiat-owed`, `buyer-unpaid`, or `unresolved`. The CKB refund is a separate protocol action.
    pub outcome: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TradeRecordNotice {
    pub outcome: String,
}

impl TradeRecord {
    pub fn fiat_owed(trade: &Trade, fiat_currency: &str) -> Self {
        Self::outcome(trade, fiat_currency, crate::OUTCOME_FIAT_OWED)
    }

    pub fn buyer_unpaid(trade: &Trade, fiat_currency: &str) -> Self {
        Self::outcome(trade, fiat_currency, crate::OUTCOME_BUYER_UNPAID)
    }

    pub fn unresolved(trade: &Trade, fiat_currency: &str) -> Self {
        Self::outcome(trade, fiat_currency, crate::OUTCOME_UNRESOLVED)
    }

    fn outcome(trade: &Trade, fiat_currency: &str, outcome: &str) -> Self {
        Self {
            trade_id: trade.id.clone(),
            seller_nostr: trade.seller_nostr.clone(),
            buyer_nostr: trade.buyer_nostr.clone(),
            fiat_amount: trade.fiat_amount.clone(),
            fiat_currency: fiat_currency.to_string(),
            outcome: outcome.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FiatSentOkPayload {
    pub release_by: i64,
    pub solver_window_secs: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseDuePayload {
    pub release_by: i64,
    pub solver_window_secs: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitingFiatPayload {
    pub fiat_amount: String,
    pub fiat_currency: String,
    pub reference: String,
    pub seller_nostr: String,
    pub buyer_nostr: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pay_by: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_by: Option<i64>,
}

impl WaitingFiatPayload {
    pub fn from_trade(trade: &Trade, fiat_currency: &str) -> Self {
        Self {
            fiat_amount: trade.fiat_amount.clone(),
            fiat_currency: fiat_currency.to_string(),
            reference: trade.id.clone(),
            seller_nostr: trade.seller_nostr.clone(),
            buyer_nostr: trade.buyer_nostr.clone(),
            pay_by: trade.pay_by(),
            release_by: trade.release_by(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisputedPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub solver: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TradeSnapshot {
    pub trade_id: String,
    pub order_id: String,
    pub state: String,
    pub seller_nostr: String,
    pub buyer_nostr: String,
    pub fiat_amount: String,
    pub fiat_currency: String,
    pub amount_shannons: String,
    pub reference: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_invoice: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payout_invoice: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock_by: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_ends_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pay_by: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_by: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
}

impl TradeSnapshot {
    pub fn from_trade(trade: &Trade, fiat_currency: &str, hold_secs: u64) -> Self {
        Self {
            trade_id: trade.id.clone(),
            order_id: trade.order_id.clone(),
            state: trade.state.clone(),
            seller_nostr: trade.seller_nostr.clone(),
            buyer_nostr: trade.buyer_nostr.clone(),
            fiat_amount: trade.fiat_amount.clone(),
            fiat_currency: fiat_currency.to_string(),
            amount_shannons: trade.shannons.to_string(),
            reference: trade.id.clone(),
            hold_invoice: trade.hold_invoice.clone(),
            payout_invoice: trade.payout_invoice.clone(),
            lock_by: trade.lock_by(),
            hold_ends_at: trade.hold_ends_at(hold_secs),
            pay_by: trade.pay_by(),
            release_by: trade.release_by(),
            outcome: trade.outcome.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TradesPayload {
    pub trades: Vec<TradeSnapshot>,
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
    pub status: String,
    pub hold_secs: u64,
    pub reserved_shannons: u128,
}

impl Order {
    pub fn public(&self) -> PublicOrder {
        PublicOrder {
            order_id: self.id.clone(),
            side: self.side.clone(),
            maker_nostr_pubkey: self.maker_nostr.clone(),
            available_ckb: shannons_to_ckb_string(self.available_shannons),
            fiat_currency: self.fiat_currency.clone(),
            price_per_ckb: self.price_per_ckb.clone(),
            min: self.min.clone(),
            max: self.max.clone(),
            hold_hours: self.hold_secs / 3_600,
            status: self.status.clone(),
            reserved_ckb: Some(shannons_to_ckb_string(self.reserved_shannons)),
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
    pub hold_received_at: Option<i64>,
    pub hold_created_at: Option<i64>,
    pub buyer_fiber_addr: Option<String>,
    /// Inbound TLC expiry in milliseconds.
    pub hold_expires_at_ms: Option<i64>,
    /// Seller released, or the solver decided the fiat arrived.
    /// A `fiat-sent` click does not set this.
    pub fiat_acknowledged: bool,
    /// Unix seconds when the buyer marked fiat sent. Starts the release grace.
    pub fiat_sent_at: Option<i64>,
    /// A party sealed this trade's chat to the solver. The ciphertext lives in `solver_evidence`.
    pub chat_key_shared: bool,
    /// The release-grace notice has already been sent.
    pub release_prompted: bool,
    /// `fiat-owed`, `buyer-unpaid`, or `unresolved`, once that record is published.
    pub outcome: Option<String>,
}

impl Trade {
    pub fn phase(&self) -> Result<Phase> {
        Phase::parse(&self.state).ok_or_else(|| anyhow!("unknown trade state {}", self.state))
    }

    pub fn lock_by(&self) -> Option<i64> {
        self.hold_created_at
            .map(|at| at + INVOICE_EXPIRY_SECS as i64)
    }

    /// TLC expiry once locked. Before that, the invoice's requested hold.
    pub fn hold_ends_at(&self, hold_secs: u64) -> Option<i64> {
        if let Some(expires_at_ms) = self.hold_expires_at_ms {
            return Some(expires_at_ms / 1000);
        }
        self.hold_created_at.map(|at| at + hold_secs as i64)
    }

    /// Last second the buyer may mark fiat sent.
    pub fn pay_by(&self) -> Option<i64> {
        self.hold_expires_at_ms.map(fiat_deadline_secs)
    }

    /// Last second a payout can start.
    pub fn route_by(&self) -> Option<i64> {
        self.hold_expires_at_ms.map(payout_deadline_secs)
    }

    /// Seller's deadline to release, measured from fiat-sent.
    pub fn release_by(&self) -> Option<i64> {
        self.fiat_sent_at
            .map(|at| at.saturating_add(RELEASE_GRACE_SECS))
    }

    /// End of the published solver window, measured from fiat-sent.
    pub fn solver_deadline(&self) -> Option<i64> {
        self.fiat_sent_at.map(|at| {
            at.saturating_add(RELEASE_GRACE_SECS)
                .saturating_add(crate::SOLVER_WINDOW_SECS)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_phases_add_waiting_invoice_to_the_watched_holds() {
        let open = Phase::open_phases();
        assert_eq!(open[0], Phase::WaitingInvoice);
        assert_eq!(&open[1..], Phase::watched_phases());
    }

    #[test]
    fn envelope_roundtrip() {
        let env = Envelope::new(NEW_ORDER)
            .with_payload(NewOrderPayload {
                side: "sell".into(),
                available_ckb: "10".into(),
                fiat_currency: "NGN".into(),
                price_per_ckb: "1500".into(),
                min: "1000".into(),
                max: "5000".into(),
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
        let published = serde_json::to_string(&sell.public()).unwrap();
        assert!(!published.contains("maker_fiber_pubkey"));
        assert_eq!(sell.public().hold_hours, 16);
        assert_eq!(sell.public().status, "open");
        let mut closed = sample_order("sell", "seller", "fiber-seller");
        closed.status = "canceled".into();
        assert_eq!(closed.public().status, "canceled");
        assert!(published.contains("NGN"));
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
            status: "open".into(),
            hold_secs: 16 * 3_600,
            reserved_shannons: 0,
        }
    }
}
