/// 1 CKB = 100_000_000 shannons.
pub const SHANNONS_PER_CKB: u128 = 100_000_000;

/// Hold-invoice final TLC expiry. Fiber minimum is 16 hours.
pub const FINAL_EXPIRY_DELTA_MS: u64 = 57_600_000;

/// Seconds the seller has to pay the hold invoice while it is still `Open`.
pub const INVOICE_EXPIRY_SECS: u64 = 3_600;

/// Routing-fee cap on `send_payment`, per thousand. Fiber's default is 5.
pub const MAX_FEE_RATE: u64 = 5;

/// Invoice currency for Fiber testnet (`fibt` invoices).
pub const TESTNET_INVOICE_CURRENCY: &str = "Fibt";

/// Invoice currency for Fiber mainnet (`fibb` invoices).
pub const MAINNET_INVOICE_CURRENCY: &str = "Fibb";

/// Hash used for hold invoices. Must match the preimage the daemon stores.
pub const HASH_ALGORITHM: &str = "sha256";

/// How often the daemon asks Fiber for hold and payout status.
pub const FIBER_POLL_SECS: u64 = 2;

/// SQLite path when `TWINE_DB` is unset.
pub const DEFAULT_DB_PATH: &str = "./twine.db";

/// NIP-44 encrypted action or reply. The `p` tag is the recipient. Not a NIP.
pub const KIND_ACTION: u16 = 4242;

/// Public addressable sell order. The `d` tag is the order id. Not a NIP.
pub const KIND_ORDER: u16 = 31420;

pub const NEW_ORDER: &str = "new-order";
pub const TAKE_SELL: &str = "take-sell";
pub const LOCKED: &str = "locked";
pub const FIAT_SENT: &str = "fiat-sent";
pub const RELEASE: &str = "release";
pub const CANCEL: &str = "cancel";

pub const PAY_INVOICE: &str = "pay-invoice";
pub const WAITING_FIAT: &str = "waiting-fiat";
pub const FIAT_SENT_OK: &str = "fiat-sent-ok";
pub const SETTLED: &str = "settled";
pub const CANCELED: &str = "canceled";
pub const EXPIRED: &str = "expired";
pub const CANT_DO: &str = "cant-do";
