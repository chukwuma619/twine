# Twine

Twine is a non-custodial peer-to-peer desk for Nervos CKB. Traders pay each other in fiat outside the protocol. CKB stays in each trader's own Fiber wallet. The coordinator never holds fiat and never holds a key that can spend locked CKB.

This repository is the coordinator stack:

| Piece | What it is |
| --- | --- |
| `twine-daemon` | Rust trade engine. Talks to traders over Nostr and to one Fiber node over JSON-RPC. |
| `twine-push` | Content-free FCM doorbell so a phone can wake without trade data in the notification. |
| `docker/` | Fiber (`fnn` 0.10.0) image used by the operator hub. |
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

Either party can cancel while the hold is still unpaid. After it locks, cancel is refused and the coins wait for settle or the timelock. Either party can open a dispute when `TWINE_SOLVER` is set. The node announcement publishes that solver and a 4-hour decision time. The solver can award the buyer, refund the seller when the buyer did not pay (`buyer-unpaid`), or record `fiat-owed` when the fiat arrived and the CKB still cannot be delivered. A buyer award or `fiat-owed` requires this trade's chat, sealed to the solver. The app encrypts that transcript to the solver's pubkey. The daemon stores the ciphertext so it can resend it, and cannot read it. The long-term chat key stays on the phone. A `fiat-sent` click does not create `fiat-owed`. It is written when the seller releases into a path that still cannot deliver CKB after fiat arrived, or when the solver records it. A decision after Fiber can no longer start the payout still writes that record, and the CKB returns to the seller. If fiat was marked sent and neither the seller nor the solver decides before the 4-hour window ends, the daemon publishes `unresolved` and refuses a later release. That record is not `buyer-unpaid` and not `fiat-owed`, and a later decision does not replace it. The Fiber refund still happens. A dispute opened before fiat is marked sent still ends at the fiat deadline. A fiat-sent click during a dispute starts the same release clock and leaves the dispute open. An instance with no solver says so on the payment screen before the buyer pays.

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
- Trampoline payout invoices are parsed and paid with `trampoline_hops`, including the dry-run route check.

There is no fee, no rating aggregate, no bond, no UDT market, no `twine-cli`, and no coordinator Docker image yet.

## Planned

Items already live are not repeated here: hold escrow, the dry-run route gate, trampoline invoices, disputes, kinds `31420` / `31421` / `31422`, the push doorbell, and the Flutter client’s book, chat, receipt photo, dispute-open, and platform keystore. Ordered by what lets a stranger finish one fiat trade, then by what lets someone else run that desk. Checked against Fiber `v0.10.0`. A payout is one payment under the buyer’s hash. The coordinator never holds a preimage for locked CKB. Channel open, backup, rebalance, and auto-accept stay on the operator’s Fiber node. Later is what is worth doing once people are trading.

### 1. Unblock trades

- **One open trade per ad.** The live book still takes several slices of one post while CKB remains. Close that race before adding markets.
- **Hub capacity on kind `31421`.** From `list_channels`, payable CKB is `local_balance` minus `offered_tlc_balance`, and receivable CKB is `remote_balance` minus `received_tlc_balance`, on enabled channels and respecting one-way direction. Those TLC fields are the locked amount (`pending_tlcs` is the same money). This number is the hub’s own channels. A multi-hop path is a separate check. Add a short operator profile on the same event.
- **A route check before anyone locks.** Document direct-channel vs multi-hop, and have the client read that published capacity before a take. Where the trader’s node can see the network, `graph_nodes` / `graph_channels` answer whether this node can reach the hub. Missing inbound on the buyer is the usual reason a trade sits in `awaiting-route`. The dry-run after the hold locks stays the gate that actually opens fiat.
- **Per-rail payment catalog** on ads, first NGN bank transfer and USD Zelle, each with a maximum fiat window that still leaves the 2h release grace, the 4h solver window, and the Fiber routing buffer. The app already lets a seller type free-form instructions. The catalog puts the rail and the clock on the order.

### 2. Trust and client

The Flutter app already posts, takes, cancels, chats, attaches a receipt, opens a dispute, and stores the `nsec`. GitHub releases cover desktop and Android. Store listing is still open.

- **Pick the key model before publishing more history.** Reputation mode keeps a stable identity key and signs each trade with a per-trade key. The record attaches when that identity is revealed. Privacy mode uses a fresh key per trade and shows a blank history. Kind `31422` has to name the key that owns the record.
- **Publish successful settles** on kind `31422`. That kind is currently `fiat-owed`, `buyer-unpaid`, and `unresolved`.
- **Aggregates in the client**, computed from those events: settled count, completion rate, median time to pay, median time to release. The events stay the source.
- **Solver mode** in twine-app, or a small solver-only build. The daemon already accepts `resolve`. The app cannot send it.
  - The solver `nsec` is a different key from any trading identity. Keystore, and screenshot blocking on key screens. A QR shows the solver `npub` to the hub operator.
  - One hub and its relays. Live pending disputes, take one case, and my disputes (in progress / closed). A content-free `twine-push` wake goes to every configured solver until someone takes the case, then a wake that it was taken.
  - Chat with the buyer and the seller. Read sealed trade evidence when a party shares it. Receipt files follow the Blossom path below.
  - Resolve with `award-buyer`, `buyer-unpaid`, or `fiat-owed`. A locked trade hold stays off `cancel_invoice`.
  - Optional read vs read-write: watch the queue, or award.
- **Fill the app gaps:** filters, both clocks, the counterparty’s public record, hub capacity, and a QR for a Fiber invoice. App Store and Play Store listing. Chat, dispute-open, and the keystore stay as they are.
- **Fixed-price and market-price ads**, with an optional fiat premium or discount against a CKB/fiat rate the hub publishes on Nostr. Trust shown on a counterparty is the settle record above, not a star rating.

### 3. Run a real hub

- **Protocol version** on kind `31421` and on kind `4242`, with the same check in CLI `schema`. The shipping app and a newer hub have to be able to refuse each other on purpose.
- **Coordinator Docker image** and docs for a third-party operator. The Fiber image under `docker/` is already `fnn` 0.10.0. Those docs point at Fiber’s `admin.backup` for the node store. The daemon does not keep a second copy of channel state.
- **Hub health** on that same `31421` event: Fiber RPC reachable, channels open, a recent payout-path dry-run succeeded, relays reachable. Refresh it on a clock so a client can skip a dead desk before a hold locks. Capacity is the fields from section 1. Keeping this event fresh is the public hub.
- **Ad expiry** on the daemon clock, for maker posts that are not refreshed. Also set a NIP-40 `expiration` tag for relays that honor it. The daemon clock is the authority. NIP-40 is still a draft, and a relay may keep the event.
- **Maintenance mode.** Block new takes while in-flight holds settle or expire.
- **Operator metrics** from the daemon: RPC errors, stuck phases, relay lag. The Nostr health event is what a trader sees. The operator needs the logs.
- **One coordinator fee**, default 0%, taken from the released amount as a second Fiber payment to an operator invoice. The trade still completes when that route fails. Publish an unpaid-fee or fee-paid event, and retry the fee.
- **`twine-cli`**, a terminal trader and solver over Nostr. Fiber invoices are pasted or read from stdin. The CLI holds no Fiber key.
  - **Book:** `order list` (side, currency, status), `order info`, `order mine`, `order sync`.
  - **Trade:** `order new`, `order take` (partial amount), `order payout-invoice` (same `H(S_b)`), `order fiat-sent`, `order release`, `order cancel` while the hold is still `Open`.
  - **Watch:** `order watch` with a resume cursor. Long-poll.
  - **Chat:** `message list` for daemon replies, `order chat` for the NIP-44 thread, `dispute chat` for the solver thread and sealed evidence.
  - **Dispute:** `dispute open`. Solver commands are `award-buyer`, `buyer-unpaid`, `fiat-owed`, plus messages to the parties. A locked trade hold stays off `cancel_invoice`.
  - **Trust:** `identity` show, and reputation lookup for this hub from kind `31422`.
  - **Identity and config:** init/show, `config set|show` (hub npub, relays). Backup of the Nostr identity is a local NIP-49 `ncryptsec` file. It is never published, and it is never a Fiber key. NIP-49 is still a draft, so version the file. NIP-06 stays unused.
  - **Scripting:** `--output json`, `--yes`, `schema`, `--invoice -` so the invoice stays off the process list, and `next_step` with `ok`, `error`, or `uncertain`. An `uncertain` result is not retried blindly. Each command exits.
  - **Operator:** maintenance on/off and status, once maintenance mode exists.
- **`twine-mcp`**, optional, after the CLI. It runs `twine-cli --output json`. Same commands, one protocol.
  - Writes stay off unless `TWINE_MCP_ALLOW_WRITES` is set. Read tools until then.
  - No tool pays a Fiber invoice. Hold and payout invoices come back as data for a person, or a separate wallet, to pay.
  - No key or mnemonic as a tool argument. Redact `nsec` in output. Clear the environment so operator secrets never reach the CLI subprocess.
  - One CLI run at a time on the shared identity store. Validate arguments before spawn. Bound the output size.
  - A startup `schema` check, so an old CLI cannot quietly drop commands.
  - Agent rules: peer chat is untrusted data. An invoice comes from the daemon reply, never from chat. A chat message never changes trade state. Suggest a dispute and let the user confirm. `release` requires an explicit `fiat_received: true`.

### 4. Discovery and scale

- **twine-web:** guides, a live kind `31420` book, and a directory of hubs from kind `31421` capacity and health. The web repo is still the download site.
- **Whitelisted UDT markets**, one asset per channel, using invoice and payment `udt_type_script` the same way CKB holds work. The RPC is in Fiber 0.10.0. Build it against testnet. Fiber’s public-node docs still say mainnet public nodes hold no USDI, so a mainnet UDT market waits until those channels exist.

### 5. Operator and routing polish

The daemon pays the buyer and watches the hold. Trampoline invoices are already paid with `trampoline_hops`. `graph_nodes` and `graph_channels` are the pre-take check in section 1. Opening, funding, backing up, rebalancing, and auto-accepting channels stay `fnn` commands.

- **`hop_hints`** on the payout, so a buyer behind a private last hop can still be reached. Fiber treats a hint as a hint. The route may still go elsewhere.
- **`subscribe_store_changes`** on the Fiber websocket, so hold and payment updates arrive as events. Polling `get_invoice` and `list_channels` remains the fallback.

### 6. Nostr polish

- **Relay failover**, and maker relay lists (NIP-65), so one dead relay does not hide the book.
- **Receipt files on Blossom**, plus a short record the solver can read: amount, rail, reference, timestamps. The file bytes stay encrypted to the counterparty and, once a dispute is sealed, to the solver. NIP-96 stays unused.
- **NIP-59 gift wrap** around kind `4242`, so a public relay does not show who sent which action. A gift wrap is signed by a one-time key, and a relay may require NIP-42 auth before it stores one. Support that auth when a chosen relay asks for it. The public book stays on public relays.
- **Web Push**, still content-free, beside `twine-push`, for a browser that has no FCM token.
- **External signers.** NIP-46 so the trading `nsec` can stay off the phone. NIP-07 on the web and NIP-55 on Android, so those clients need not embed an `nsec`. The platform keystore remains the default.
- **Optional profile** (NIP-05 and kind `0`) linked from reputation mode. No KYC field.
- **Optional NIP-13 PoW** on the first public action, a new ad or a first take. This is the anti-spam step that does not charge a Fiber payment.
- **NIP-51 lists**, so a client can mute a hub or a counterparty.

### 7. Later

Worth doing once people are actually trading. The hold stays one buyer-minted hash, and one hub keeps one published solver.

- A third-party security audit before serious mainnet money.
- Maker-defined shorter fiat timers inside the hub’s hold envelope. The per-method windows in section 1 come first.
- **Buy-side maker ads** (“want to buy CKB”). The trade hold still uses a buyer-minted payout hash.
- **Dispute assistant**, a separate process. It never moves funds and never awards. It uses a read-only solver key, and the daemon rejects `resolve` from that key. Humans still call `award-buyer`, `buyer-unpaid`, and `fiat-owed`.
  - Solver-mode push already covers “someone take this case.” The rest is mediation: templated questions (“did you send fiat?”, “did it arrive?”). When the facts are clear, remind only the party who can `release`, or who can cancel while the hold is still `Open`. They still act in their own client.
  - On conflict, silence, a fraud signal, or “ask for a person,” stop writing. Hand the human solver a short brief and the transcript. Any write-capable solver can take over.
  - Any model sees role, amount, currency, payment method, and message text. It does not see pubkeys or Nostr ids. Uncertain means escalate. There is no free-form AI chat with traders.
- **Reputation federation.** A `31422` event belongs to one daemon today. A later schema can let a client merge records across hubs it already trusts.
- Mainnet fee defaults and rate limits on the public book. Spam control before that is the NIP-13 PoW in section 6.

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
| `tests/e2e/` | Multi-`fnn` e2e harness wrapping a Fiber v0.10.0 checkout. |
| `docker/` | Fiber node image. |
| `config/` | Fiber config templates used with the image. |
| `DEPLOY.md` | VPS install for the daemon, Fiber, and `twine-push`. |
