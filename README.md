# Twine

Twine is a non-custodial peer-to-peer desk for Nervos CKB. Traders pay each other in fiat outside the protocol. CKB stays in each trader's own Fiber wallet. The coordinator never holds fiat and never holds a key that can spend locked CKB.

This repository is the coordinator stack:

| Piece | What it is |
| --- | --- |
| `twine-daemon` | Rust trade engine. Talks to traders over Nostr and to one Fiber node over JSON-RPC. |
| `twine-push` | Content-free FCM doorbell so a phone can wake without trade data in the notification. |
| `docker/` | Fiber (`fnn` 0.10.0-rc3) image used by the operator hub. |
| `DEPLOY.md` | VPS install for Fiber, the daemon, and push. |

Related repos: the [Flutter client](https://github.com/chukwuma619/twine_app) and the early [twine-web](https://github.com/chukwuma619/twine-web) download site. 

## Why Twine

Buying CKB for local currency usually means depositing into a custodial P2P desk or passing KYC on a fiat ramp. Twine keeps CKB in Fiber channels the traders already control:

1. The buyer mints secret `S_b` on their own Fiber node and builds a payout invoice under `H(S_b)`.
2. The daemon creates a hold invoice under the same hash and stores no preimage.
3. The seller pays that hold from their own node (Fiber Studio or any Fiber wallet).
4. Fiat moves bank-to-bank / Zelle-to-Zelle outside Twine.
5. On release the hub pays the buyer's invoice, learns `S_b`, arms the watchtower, and settles the hold.
6. If nobody settles, Fiber's 48-hour timelock returns the CKB to the seller.

The operator can coordinate the book and a published solver, but cannot settle locked coins before paying the buyer.

## How a trade works

Someone posts CKB for sale. Someone else takes a slice and includes a Fiber invoice that should receive the CKB. The daemon creates a hold invoice under that invoice's payment hash and does not learn the preimage. The seller pays the hold from their own node. The daemon then asks Fiber whether that invoice has a route, using `send_payment` with `dry_run: true`. That check builds a path and does not send the payment or reveal the preimage. Fiat instructions appear only when a route comes back. If it does not, the trade stays `awaiting-route` until the buyer sends the same invoice again. The buyer sends fiat directly, then marks it sent. That report is recorded even if the route disappeared after the bank transfer; routing recovery then moves to `awaiting-invoice` separately. The seller releases. The daemon pays the buyer's invoice, learns the preimage from that payment, and settles the hold. If the trade does not finish, Fiber's timelock returns the CKB to the seller. Published fiat and payout deadlines follow the inbound channel claim delay and the buyer invoice's required lock time.

Cancel is only possible before the hold is locked. Either party can open a dispute when `TWINE_SOLVER` is set. The node announcement publishes that solver and a 4-hour decision time. The solver can award the buyer, refund the seller when the buyer did not pay (`buyer-unpaid`), or record `fiat-owed` when the fiat arrived and the CKB still cannot be delivered. A buyer award or `fiat-owed` requires this trade's chat, sealed to the solver. The app encrypts that transcript to the solver's pubkey. The daemon stores the ciphertext so it can resend it, and cannot read it. The long-term chat key stays on the phone. A `fiat-sent` click does not create `fiat-owed`. It is written when the seller releases into a path that still cannot deliver CKB after fiat arrived, or when the solver records it. A decision after Fiber can no longer start the payout still writes that record, and the CKB returns to the seller. If fiat was marked sent and neither the seller nor the solver decides before the 4-hour window ends, the daemon publishes `unresolved` and refuses a later release. That record is not `buyer-unpaid` and not `fiat-owed`, and a later decision does not replace it. The Fiber refund still happens. A dispute opened before fiat is marked sent still ends at the fiat deadline. A fiat-sent click during a dispute starts the same release clock and leaves the dispute open. An instance with no solver says so on the payment screen before the buyer pays.

### Trade phases

```
waiting-hold → awaiting-route ⇄ waiting-fiat → fiat-sent → releasing → settled
                     ↑                │              │            │
                     └────────────────┘              │            └→ awaiting-invoice → releasing
                                                     ↓
                                                 disputed → releasing | refunding → expired
```

Also: `canceled` (hold still Open), `refunding` / `expired` on timelock.

## What it does today

- Sell posts with a price, a local currency, and min and max. Partial takes are allowed, and several trades can be open on one post while CKB remains.
- Fiber hold invoices last 48 hours. An unpaid hold expires after 1 hour. The buyer can mark fiat sent only while a 2-hour release grace, a 4-hour solver window, and Fiber's routing time are still left. That routing time follows the inbound channel claim margin and the buyer invoice's lock, and is at least 30 minutes. After fiat is marked sent, the seller has 2 hours to release. If that grace passes, the buyer is told to open a dispute and seal this trade's chat to the solver.
- If the buyer's invoice has no route when the hold locks, fiat stays closed (`awaiting-route`) until they paste that same invoice again. A dispute cannot open the release clock until fiat instructions were offered. A later routing failure after fiat was marked sent keeps the fiat report and waits in `awaiting-invoice`. An invoice with an expiry has to stay payable through the hold. An expired one cannot be replaced, so the hold refunds. If the seller had released, or the solver recorded that the fiat arrived, that refund also publishes a public `fiat-owed` record.
- Disputes with sealed solver evidence. The daemon still cannot read the thread.
- Public order book (kind `31420`), Fiber node announcement with pubkey, listen addresses, optional solver, and solver window (kind `31421`), and public trade outcomes (kind `31422`).
- SQLite for orders, trades, and processed events. The preimage is removed from the daemon database after settle; the watchtower copy remains for channel safety.
- `twine-push`, which wakes a phone without putting trade content in the notification.
- Host docs in `DEPLOY.md` and a Fiber node image under `docker/`.

There is no fee, no rating aggregate, no bond, no UDT market, no `twine-cli`, and no coordinator Docker image yet.

## Planned

Items already live (hold escrow, dry-run route gate, disputes, kind `31420`/`31421`/`31422`, push doorbell) are not repeated here. Ordered by trader and operator value. Features here must fit Twine’s model: buyer-minted trade preimage, Fiber holds, Nostr book—not a 1:1 copy of other desks.

### 1. Unblock trades

- Extend kind `31421` with live **receivable / payable** CKB from Fiber `list_channels` (`local_balance` / `remote_balance`, minus locked TLCs) and an operator profile.
- Document multi-hop vs direct-channel expectations for traders.
- Per-rail payment catalog on ads (first: NGN bank transfer, USD Zelle) with **per-method maximum fiat windows**, still ending early enough for the 2h release grace, 4h solver window, and Fiber routing buffer.
- Enforce **one open trade per ad** (live PoC still allows concurrent partial takes while CKB remains).
- **Cooperative cancel**: both parties agree to abort while the hold is still unpaid (`Open`); after lock, Twine still refuses `cancel_invoice` on the trade hold and waits for settle or timelock.

### 2. Trust and client

- Public per-pubkey aggregates on top of kind `31422`: settled count, completion rate, median time to pay, median time to release.
- Publish successful settles into the public record (today settles stay on private trade replies).
- Optional **peer ratings** after settle (counterparty score on Nostr), separate from the objective outcome aggregates.
- **Reputation mode** (stable identity key + per-trade keys) vs **privacy mode** (fresh key per trade, blank history)—clients sign actions with the trade key; reputation attaches only when the identity is revealed.
- **Fixed-price and market-price ads**, with an optional **fiat premium/discount** vs a published CKB/fiat rate feed on Nostr.
- Rebuilt **twine-app** (filters, dual clocks, trade records, hub capacity, QR, chat, disputes, native keystores) and store / desktop packaging.
- Optional **solver mode** in twine-app (or a small solver-only build):
  - Separate **solver `nsec`** from any trading identity (never the same key); Keystore + screenshot blocking on key screens; QR to show the solver `npub` to the hub operator.
  - Pick **one hub** + relays; live **pending disputes**, take/claim a case, and **my disputes** (in progress / closed).
  - Chat with buyer and seller; view sealed trade evidence when a party shares it; images/PDFs via the planned receipt media path.
  - Resolve with Twine outcomes only (`award-buyer`, `buyer-unpaid`, `fiat-owed`)—never `cancel_invoice` on a locked trade hold.
  - Content-free push wakes for new disputes (reuse `twine-push`).
  - Optional **read vs read-write** solver permission (watch queue vs award).

### 3. Run a real hub

- Coordinator **Docker image** and third-party operator docs (Fiber image already exists under `docker/`).
- Public hub with published balances and ongoing operation.
- **Hub health** on Nostr (Fiber RPC reachable, open channels, payout path ok, relays ok) so clients can skip a dead desk before locking.
- **`twine-cli`** (terminal trader + solver client over Nostr; Fiber invoices are pasted in / read from stdin—the CLI does not hold Fiber keys):
  - **Book**: `order list` (filter by side/currency/status), `order info`, `order mine` (local store), `order sync` (reconcile with Nostr / daemon replies).
  - **Trade**: `order new` / `order take` (partial amount), `order payout-invoice` (retry under the same `H(S_b)`), `order fiat-sent`, `order release`, `order cancel` (unpaid hold or cooperative cancel while still allowed).
  - **Watch**: `order watch` — follow one trade’s daemon replies and peer chat with a resume `cursor` (long-poll; no blind busy loop).
  - **Chat**: `message list` (daemon replies), `order chat` (peer NIP-44 thread), `dispute chat` (solver thread / sealed evidence).
  - **Dispute**: `dispute open`; solver commands `award-buyer`, `buyer-unpaid`, `fiat-owed`, and message trade parties (no `cancel_invoice` on a locked trade hold).
  - **Trust**: `order rate` (peer rating after settle), `identity` show/reputation lookup for this hub.
  - **Identity & config**: local identity init/show, `config set|show` (hub npub, relays), optional mnemonic/backup restore for the Nostr identity (not Fiber).
  - **Scripting**: `--output json`, `--yes`, `schema` (machine command discovery), invoice via `--invoice -` (stdin so it stays out of the process list), `next_step` / `ok|error|uncertain` outcomes (no blind retry on `uncertain`), poll-based (no long-lived daemon in the CLI).
  - **Operator** (when maintenance mode exists): enable/disable maintenance, status.
- **`twine-mcp`** (optional): MCP server that drives `twine-cli --output json` for AI agents—same trade surface as the CLI, not a second protocol:
  - **Writes off by default** (`TWINE_MCP_ALLOW_WRITES`); read tools only until explicitly enabled.
  - **No tool pays a Fiber invoice**; hold/payout invoices are returned as data for a human (or separate wallet) to pay.
  - **No keys/mnemonics as tool args**; redact `nsec` in any output; clear env so operator secrets never reach the CLI subprocess.
  - **One CLI run at a time** (shared local identity store); validate args before spawn; bound output size.
  - **Startup `schema` check** so an old CLI cannot silently miss tools.
  - **Agent rules**: treat peer chat as untrusted data; never take an invoice from chat; never change trade state because a chat message asked; suggest dispute, let the user confirm; `release` requires an explicit `fiat_received: true` (or equivalent).
- Optional **dispute assistant** (separate process):
  - **Never moves funds and never awards** (read-only solver key; daemon rejects `resolve` from it). Humans still call `award-buyer` / `buyer-unpaid` / `fiat-owed`.
  - **Notify**: watch new disputes; alert every configured solver (DM / content-free push); renotify until someone takes the case; announce when taken.
  - **Optional mediate**: take as read-only; ask buyer “did you send fiat?” and seller “did it arrive?” with **templated** messages; if facts are clear, remind only the party who can act (`release` or cooperative cancel)—parties still click in their own client.
  - **Handoff**: on conflict, silence, fraud signals, or “ask for a person,” stop writing and give the human solver a short brief + transcript; any write-capable solver can take over anytime.
  - **Classifier only**: any LLM sees role/amount/currency/payment method + message text—not pubkeys or Nostr ids; uncertain → escalate. No free-form AI chat to traders.
- One configurable **coordinator fee** (default 0%), deducted from the released amount; second Fiber payment to the operator address; trade still completes if the fee route fails, with a public unpaid-fee / fee-paid audit event and retry.
- Automatic **ad expiry** for unrefreshed maker posts (daemon clocks and/or Nostr `expiration` tags).
- Operator **maintenance mode**: block new takes while in-flight holds settle or expire.

### 4. Discovery and scale

- **twine-web**: guides, live Nostr order book, public node directory with capacity and health.
- **Basic MPP** (`allow_mpp` / `max_parts`) so large payouts can split across channels when one path cannot carry the full amount.
- Whitelisted **UDT markets** (one asset per channel), using Fiber invoice / payment `udt_type_script` the same way CKB holds work today.
- **CCH / Lightning bridge** desk mode: accept or settle against BTC Lightning invoices via Fiber CCH (`send_btc` / `receive_btc`) for CKB↔BTC rails alongside fiat.
- **fiber-js / WASM** browser wallets so twine-web traders can pay holds and mint payout invoices without a separate desktop node (when upstream productization is ready).
- When Fiber ships **Loop In/Out** and provider-mode liquidity management, wire trader-facing inbound/outbound guides and optional hub liquidity products.

### 5. Operator and routing polish

These build on RPCs already in the [Fiber](https://github.com/nervosnetwork/fiber) tree; Twine today only uses invoice / payment / watchtower preimage / `node_info` / `list_channels` (for inbound TLC timing).

- **Hop hints** and optional trampoline params so buyers on private channels can still be routed without publishing every channel.
- Reactive hold / payment updates via Fiber **pubsub** (`subscribe_store_changes`) instead of only polling `get_invoice` / `list_channels`.
- Operator tooling around Fiber **`list_payments`**, **`admin.backup`**, and channel fee updates (`update_channel`) for fee retries and hub ops—without the daemon opening or rebalancing trader channels.
- **External funding** path for the hub (`open_channel_with_external_funding` / `submit_signed_funding_tx`) so operators can fund with hardware wallets or multisig.
- Optional **circular rebalance** helpers for the *operator* node (`allow_self_payment` + `build_router`), documented separately from the trade engine (daemon still does not rebalance for traders).

### 6. Nostr polish

- Maker **relay lists** (NIP-65) and healthier multi-relay subscribe / publish failover.
- Receipt media via **Blossom** (not NIP-96; that NIP is unrecommended upstream) instead of stuffing large ciphertext into chat events.
- Web Push (and non-FCM doorbells) alongside `twine-push`, still content-free.
- Stronger action privacy (**NIP-59 gift wraps** around kind `4242`) so metadata is harder to correlate on public relays.
- Cross-instance **reputation federation**: today’s `31422` events are per daemon; a later schema could let clients merge records across hubs they trust.
- Optional trader profiles (NIP-05 / kind `0`) linked from reputation mode without requiring KYC.
- Optional **PoW** on first-contact actions to slow spam on the public book.

### 7. Later

Useful once trade volume is real:

- Formal third-party security audit (before serious mainnet money).
- Mainnet fee defaults, rate limits, and anti-spam on the public book (NIP-13 and/or small Fiber micropayments to post).
- Optional **anti-abuse bond**: a *second* Fiber hold with a **hub-owned preimage** (not the trade hold). Released on honest complete; settled to the hub only on solver directive or waiting-state timeout. Trade escrow stays buyer-hash / no early cancel.
- Maker-defined **shorter fiat timers** inside the hub’s hold envelope.
- **Buy-side maker ads** (makers post “want to buy CKB”; sellers take), still using buyer-minted payout hashes for the trade hold.
- **Multi-solver** claim queues (take a pending dispute; one solver per case) and solver marketplaces.
- Concurrent trades as a first-class product again (after one-trade-per-ad lands).
- When Fiber ships **PTLC / programmable hold** application protocols, migrate the escrow model off hash-revealed hold invoices where it improves privacy or multi-path safety.
- Fiber **keysend** for push payments (fees, tips, or refunds without an invoice).
- Fiber **`custom_records`** to attach trade ids to payments for accounting.
- Fiber **graph RPCs** for a “can I reach this hub?” path check in clients.
- Biscuit-scoped Fiber RPC for multi-operator hubs.
- Hub **auto-accept channel** config as an operator product (wallets still open/fund their own channels).
- Fiber **AMP** and any **agent fair-exchange / x402** work when upstream lands ([Fiber #1255](https://github.com/nervosnetwork/fiber/issues/1255))—separate from Twine’s fiat desk; trade holds stay buyer-minted.
- Nostr **NIP-46** remote signer so the trading `nsec` can stay off the phone.
- Nostr **NIP-40** event expiry tags for ads (alongside daemon-side ad expiry).
- Nostr **NIP-51** block / trust lists for hubs and counterparties.
- Nostr **NIP-42** AUTH for relay access control.
- Nostr **NIP-17** private DMs as an alternate chat path.
- Ephemeral Nostr events for trader online presence.
- Prefer Twine’s own **dispute assistant** over Nostr **NIP-90** data vending (NIP-90 is unrecommended); if revisited, narrow judge only—not fund movers.

Worth considering (research more before committing):

- Optional **NIP-69** (`38383`) bridge/mirror so Twine ads can appear in the shared Nostr P2P order pool without replacing kind `31420`.
- **NIP-07** (web) and **NIP-55** (Android) external signers alongside NIP-46, so clients need not embed an `nsec`.
- **NIP-49** (`ncryptsec`) encrypted backup for the Nostr identity instead of raw mnemonic restore (NIP-06 is unrecommended).
- First-class **inbound liquidity / channel-open** trader guides and in-app checks (beyond multi-hop docs)—the usual blocker behind `awaiting-route`.
- **Client↔daemon protocol version** (and CLI `schema` age checks) so app/CLI cannot silently talk past an older hub.
- Hub **operator metrics** (RPC/logs/Prometheus-style)—Nostr hub health for traders is not enough to run a public desk.
- **Structured receipt schema** on top of Blossom (amount, rail, reference, timestamps) so solvers can reason over metadata, not only image blobs.
- When Fiber ships **hosted / LSP channels**, wire mobile/light clients that should not run a full node (distinct from fiber-js/WASM browser wallets).

## Nostr surface

| Kind | Purpose |
| --- | --- |
| `4242` | NIP-44 encrypted actions and replies between traders and the daemon (`new-order`, `take`, `fiat-sent`, `release`, `dispute`, `resolve`, …). |
| `31420` | Public addressable sell order. `d` tag is the order id. |
| `31421` | Public Fiber node announcement. `d` tag is `fiber-node`. |
| `31422` | Public trade outcome (`fiat-owed`, `buyer-unpaid`, `unresolved`). `d` tag is the trade id. |

Chat between traders is also NIP-44 encrypted end-to-end. The daemon is not a party to that conversation unless a dispute seals a transcript to the solver.

## Architecture

```
twine-app ──NIP-44──► Nostr relays ◄──NIP-44── twine-daemon ──JSON-RPC──► fnn (Fiber)
     ▲                    │                         │
     │                    │                         └── SQLite
     └──── FCM wake ── twine-push (watches #p tags)
```

Traders pay holds and receive payouts on their own Fiber nodes (often via [Fiber Studio](https://getfiberstudio.com)). The hub node is where hold invoices are locked; wallets open channels to it. The daemon never opens or rebalances those channels.


## Configuration

Export these before `cargo run`. On a VPS, systemd loads the same variables. See [DEPLOY.md](DEPLOY.md).

| Variable | Required | Meaning |
| --- | --- | --- |
| `TWINE_NOSTR_SECRET` | yes | Daemon Nostr key. Hex or `nsec`. Startup logs the matching `npub`. |
| `TWINE_RELAYS` | yes | Comma-separated websocket URLs. At least one. |
| `TWINE_RPC` | yes | Fiber JSON-RPC URL, for example `http://127.0.0.1:8229`. |
| `TWINE_RPC_TOKEN` | yes | Biscuit for that RPC, sent as `Authorization: Bearer`. |
| `FIBER_NETWORK` | no | `testnet` (default), `mainnet`, or `dev` (local e2e). Selects invoice currency `Fibt`, `Fibb`, or `Fibd`. |
| `TWINE_SOLVER` | no | Nostr pubkey allowed to send `resolve`, hex or `npub`. Stored as lowercase hex. Empty means disputes are refused. |
| `TWINE_DB` | no | SQLite path. Default `./twine.db`. |
| `RUST_LOG` | no | Tracing filter. Default `info`. |

`twine-push` is a separate crate. It is not started by `cargo run` in the repo root.

| Variable | Required | Meaning |
| --- | --- | --- |
| `TWINE_RELAYS` | no | Same relay list as the daemon. Default `wss://relay.backr.site`. |
| `TWINE_PUSH_LISTEN` | no | HTTP bind. Default `0.0.0.0:8787`. |
| `TWINE_PUSH_PUBLIC_URL` | no | Public base URL for NIP-98 auth (for example `https://push.backr.site`). Required behind a reverse proxy. |
| `TWINE_PUSH_TOKENS` | no | JSON file of device tokens. Default `./twine-push-tokens.json`. |
| `FCM_SERVICE_ACCOUNT` | no | Firebase service-account JSON path, or the JSON itself. Without it, doorbells are logged only. |
| `GOOGLE_APPLICATION_CREDENTIALS` | no | Fallback path if `FCM_SERVICE_ACCOUNT` is unset. |
| `RUST_LOG` | no | Tracing filter. Default `info`. |

## Fiber channels

The daemon pays the buyer's invoice. It does not open, fund, or rebalance a channel. Before fiat, it checks the route with Fiber `send_payment` `dry_run: true` (the call builds a path, stores no session, and sends no onion). A wallet that cannot receive gets its own inbound, then sends the same invoice again. Kind `31421` publishes this node's pubkey and listen addresses. Kind `31422` publishes a `fiat-owed` trade record when the CKB returns after the fiat had arrived, a `buyer-unpaid` record when the solver finds the buyer did not pay, and an `unresolved` record when the solver window ends with no decision.

## Develop

```bash
cargo test
cargo run --release
```

`cargo run` reads the variables above from the environment. Default `cargo test` uses an in-process FakeFiber. Live Fiber coverage is a separate multi-node e2e harness (`make e2e`): local CKB + three `fnn` processes (hub / seller / buyer). See [tests/e2e/README.md](tests/e2e/README.md).

```bash
make e2e        # start nodes, then cargo test -- --ignored
make e2e-down   # stop nodes
```

Push doorbell:

```bash
cd twine-push
cargo run --release
```

## Layout

| Path | What it is |
| --- | --- |
| `src/` | Daemon: trade engine (`engine.rs`), action handlers (`app/`), Nostr, Fiber RPC, SQLite, state machine (`state.rs`). |
| `twine-push/` | Content-free FCM doorbell. |
| `tests/e2e/` | Multi-`fnn` e2e harness wrapping a Fiber v0.10.0-rc3 checkout. |
| `docker/` | Fiber node image. |
| `config/` | Fiber config templates used with the image. |
| `DEPLOY.md` | VPS install for the daemon, Fiber, and `twine-push`. |
