/// 1 CKB = 100_000_000 shannons.
pub const SHANNONS_PER_CKB: u128 = 100_000_000;

/// Hold invoice `final_expiry_delta`. Fiber accepts 160 minutes through 14 days.
/// 48h clears Fiber's 24h default final TLC plus the release and solver windows.
pub const HOLD_HOURS: u64 = 48;

pub const fn hold_secs(hours: u64) -> u64 {
    hours * 3_600
}

pub const HOLD_SECS: u64 = hold_secs(HOLD_HOURS);

/// How long an unpaid hold invoice stays open for the seller to lock.
pub const INVOICE_EXPIRY_SECS: u64 = 60 * 60;

/// After fiat is marked sent, the seller has this long to check the bank and release.
pub const RELEASE_GRACE_SECS: i64 = 2 * 3_600;

/// Published solver decision time after the release grace.
pub const SOLVER_WINDOW_SECS: i64 = 4 * 3_600;

/// Shortest routing buffer kept after the solver window. Fiber's TLC budget can require more.
pub const ROUTE_BUFFER_SECS: i64 = 30 * 60;

/// Release `MILLI_SECONDS_PER_EPOCH`. Debug builds use 2 seconds.
pub const FIBER_MILLIS_PER_EPOCH: u64 = 4 * 60 * 60 * 1000;

/// Two thirds of one commitment-delay epoch. Release `MIN_TLC_EXPIRY_DELTA`.
pub const FIBER_TLC_EXPIRY_DELAY_MS: u64 = FIBER_MILLIS_PER_EPOCH * 2 / 3;

/// Release `CHECK_CHANNELS_INTERVAL`. Added to the delay before Fiber drops a TLC.
pub const FIBER_CHECK_CHANNELS_MS: u64 = 60 * 1000;

/// Incoming TLC stays claimable until `expiry - this`.
pub const CLAIM_MARGIN_MS: u64 = FIBER_TLC_EXPIRY_DELAY_MS + FIBER_CHECK_CHANNELS_MS;

/// `DEFAULT_TLC_EXPIRY_DELTA`. Fiber adds at least this to every hop.
pub const FIBER_RANDOM_EXPIRY_RESERVE_MS: u64 = 4 * 60 * 60 * 1000;

/// `DEFAULT_FINAL_TLC_EXPIRY_DELTA` when the invoice omits the attribute.
pub const DEFAULT_FINAL_TLC_EXPIRY_DELTA_MS: u64 = 24 * 60 * 60 * 1000;

/// Lock a trampoline payment adds on top of the payee's final lock: Fiber v0.9.1
/// `trampoline_forward_expiry_delta` gives each trampoline segment
/// 3 x `DEFAULT_TLC_EXPIRY_DELTA` + `MIN_TLC_EXPIRY_DELTA`, plus the hop into the
/// trampoline node. Measured on testnet 2026-11-12: a 24h invoice behind one trampoline
/// needed 42.8h from the trampoline's peer.
pub const TRAMPOLINE_SLACK_MS: u64 = 3 * FIBER_RANDOM_EXPIRY_RESERVE_MS
    + FIBER_TLC_EXPIRY_DELAY_MS
    + FIBER_RANDOM_EXPIRY_RESERVE_MS
    + 20 * 60 * 1000;

/// `MAX_PAYMENT_TLC_EXPIRY_LIMIT`.
pub const MAX_TLC_EXPIRY_LIMIT_MS: u64 = 14 * 24 * 60 * 60 * 1000;

/// Shortest `tlc_expiry_limit` Fiber accepts.
pub const MIN_TLC_EXPIRY_LIMIT_MS: u64 = FIBER_TLC_EXPIRY_DELAY_MS;

/// Per-thousand routing-fee cap on `send_payment`. Fiber's default is 5.
pub const MAX_FEE_RATE: u64 = 5;

pub const TESTNET_INVOICE_CURRENCY: &str = "Fibt";

pub const MAINNET_INVOICE_CURRENCY: &str = "Fibb";

pub const DEV_INVOICE_CURRENCY: &str = "Fibd";

pub const FIBER_POLL_SECS: u64 = 2;

/// SQLite path when `TWINE_DB` is unset.
pub const DEFAULT_DB_PATH: &str = "./twine.db";

/// NIP-44 encrypted action or reply. The `p` tag is the recipient. Not a NIP.
pub const KIND_ACTION: u16 = 4242;

/// Public addressable order. The `d` tag is the order id. Not a NIP.
pub const KIND_ORDER: u16 = 31420;

/// Public addressable Fiber node. The `d` tag is [FIBER_NODE_TAG]. Not a NIP.
pub const KIND_FIBER_NODE: u16 = 31421;

/// Public addressable trade outcome. The `d` tag is the trade id. Not a NIP.
pub const KIND_TRADE_RECORD: u16 = 31422;

pub const FIBER_NODE_TAG: &str = "fiber-node";

pub const NEW_ORDER: &str = "new-order";
pub const TAKE: &str = "take";
pub const PAYOUT_INVOICE: &str = "payout-invoice";
pub const FIAT_SENT: &str = "fiat-sent";
pub const RELEASE: &str = "release";
pub const CANCEL: &str = "cancel";
pub const DISPUTE: &str = "dispute";
pub const RESOLVE: &str = "resolve";
pub const MY_TRADES: &str = "my-trades";

pub const PAY_INVOICE: &str = "pay-invoice";
pub const NEED_INVOICE: &str = "need-invoice";
pub const NEED_ROUTE: &str = "need-route";
pub const WAITING_FIAT: &str = "waiting-fiat";
pub const TRADE_RECORD: &str = "trade-record";

pub const OUTCOME_FIAT_OWED: &str = "fiat-owed";
pub const OUTCOME_BUYER_UNPAID: &str = "buyer-unpaid";
pub const OUTCOME_UNRESOLVED: &str = "unresolved";
pub const FIAT_SENT_OK: &str = "fiat-sent-ok";
pub const RELEASE_DUE: &str = "release-due";
pub const NEW_INVOICE: &str = "new-invoice";
pub const DISPUTED: &str = "disputed";
pub const REFUNDING: &str = "refunding";
pub const RELEASING: &str = "releasing";
pub const SETTLED: &str = "settled";
pub const CANCELED: &str = "canceled";
pub const EXPIRED: &str = "expired";
pub const CANT_DO: &str = "cant-do";
pub const TRADES: &str = "trades";
