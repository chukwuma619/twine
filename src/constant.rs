/// 1 CKB = 100_000_000 shannons.
pub const SHANNONS_PER_CKB: u128 = 100_000_000;

/// Shortest hold Fiber will create.
pub const MIN_HOLD_HOURS: u64 = 16;

/// Longest hold an order may set.
/// A received hold cannot be canceled, so this is the longest a seller's coins
/// can sit if the buyer never pays. 48 hours covers the 2-hour fiat window,
/// the 18-hour dispute window, and a failed payout retry.
pub const MAX_HOLD_HOURS: u64 = 48;

/// Hold stored on orders created before the field existed.
/// Also the TLC expiry on a payout invoice this node creates.
pub const DEFAULT_HOLD_HOURS: u64 = 36;

pub const fn hold_secs(hours: u64) -> u64 {
    hours * 3_600
}

pub const FINAL_EXPIRY_DELTA_MS: u64 = hold_secs(DEFAULT_HOLD_HOURS) * 1_000;

/// Seconds the seller has to pay the hold invoice while it is still `Open`.
pub const INVOICE_EXPIRY_SECS: u64 = 3_600;

/// Seconds the buyer has to mark fiat sent after the hold is received.
pub const FIAT_WINDOW_SECS: u64 = 2 * 3_600;

/// Seconds the solver has to resolve a dispute after the fiat window closes.
pub const DISPUTE_WINDOW_SECS: u64 = 18 * 3_600;

/// Stop starting a payout this long before the Fiber timelock.
pub const SAFETY_SECS: u64 = 30 * 60;

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

/// Public addressable order. The `d` tag is the order id. Not a NIP.
pub const KIND_ORDER: u16 = 31420;

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
