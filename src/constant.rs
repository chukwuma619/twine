use crate::types::ConfiguredPaymentMethod;

/// 1 CKB = 100_000_000 shannons.
pub const SHANNONS_PER_CKB: u128 = 100_000_000;

/// Fiber accepts 16 to 48 hours. A received hold cannot be canceled.
pub const HOLD_HOURS: u64 = 36;

pub const fn hold_secs(hours: u64) -> u64 {
    hours * 3_600
}

pub const HOLD_SECS: u64 = hold_secs(HOLD_HOURS);

/// How long the buyer has to pay after the seller locks.
pub const PAYMENT_WINDOW_SECS: u64 = 15 * 60;

/// A post may name at most this many catalog ids.
pub const MAX_PAYMENT_METHODS: usize = 5;

/// A post can use a method only when its currency matches the post.
pub const SUPPORTED_PAYMENT_METHODS: &[ConfiguredPaymentMethod] = &[
    ConfiguredPaymentMethod {
        id: "gtbank",
        kind: "bank",
        label: "GTBank",
        currency: "NGN",
    },
    ConfiguredPaymentMethod {
        id: "zelle",
        kind: "wallet",
        label: "Zelle",
        currency: "USD",
    },
];

/// How long an unpaid hold invoice stays open for the seller to lock.
pub const INVOICE_EXPIRY_SECS: u64 = 60 * 60;

/// Do not start a payout this close to the Fiber timelock.
pub const SAFETY_SECS: u64 = 30 * 60;

/// Per-thousand routing-fee cap on `send_payment`. Fiber's default is 5.
pub const MAX_FEE_RATE: u64 = 5;

pub const TESTNET_INVOICE_CURRENCY: &str = "Fibt";

pub const MAINNET_INVOICE_CURRENCY: &str = "Fibb";

/// Hold invoices. Must match the preimage the daemon stores.
pub const HASH_ALGORITHM: &str = "sha256";

pub const FIBER_POLL_SECS: u64 = 2;

/// SQLite path when `TWINE_DB` is unset.
pub const DEFAULT_DB_PATH: &str = "./twine.db";

/// NIP-44 encrypted action or reply. The `p` tag is the recipient. Not a NIP.
pub const KIND_ACTION: u16 = 4242;

/// Public addressable order. The `d` tag is the order id. Not a NIP.
pub const KIND_ORDER: u16 = 31420;

/// Public addressable Fiber node. The `d` tag is [FIBER_NODE_TAG]. Not a NIP.
pub const KIND_FIBER_NODE: u16 = 31421;

pub const FIBER_NODE_TAG: &str = "fiber-node";

/// Public addressable payment catalog. The `d` tag is [PAYMENT_CATALOG_TAG]. Not a NIP.
pub const KIND_CATALOG: u16 = 31422;

pub const PAYMENT_CATALOG_TAG: &str = "payment-methods";

pub const NEW_ORDER: &str = "new-order";
pub const TAKE: &str = "take";
pub const FIAT_SENT: &str = "fiat-sent";
pub const RELEASE: &str = "release";
pub const CANCEL: &str = "cancel";
pub const DISPUTE: &str = "dispute";
pub const RESOLVE: &str = "resolve";

pub const PAY_INVOICE: &str = "pay-invoice";
pub const WAITING_FIAT: &str = "waiting-fiat";
pub const FIAT_SENT_OK: &str = "fiat-sent-ok";
pub const NEW_INVOICE: &str = "new-invoice";
pub const DISPUTED: &str = "disputed";
pub const REFUNDING: &str = "refunding";
pub const SETTLED: &str = "settled";
pub const CANCELED: &str = "canceled";
pub const EXPIRED: &str = "expired";
pub const CANT_DO: &str = "cant-do";
