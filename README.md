# Twine

Twine is a non-custodial peer-to-peer desk for Nervos CKB. Traders pay each other in fiat outside the protocol. CKB stays in each trader's own Fiber wallet. The coordinator never holds fiat and never holds a key that can spend locked CKB.

This repository is the coordinator stack:

| Piece | What it is |
| --- | --- |
| `twine-daemon` | Rust trade engine. Talks to traders over Nostr and to one Fiber node over JSON-RPC. |
| `twine-push` | Content-free FCM doorbell so a phone can wake without trade data in the notification. |
| `docker/` | Fiber (`fnn` 0.9.1) image used by the operator hub. |
| `DEPLOY.md` | VPS install for Fiber, the daemon, and push. |

Related repos: the [Flutter client](https://github.com/chukwuma619/twine_app) and the early [twine-web](https://github.com/chukwuma619/twine-web) download site. 

## Why Twine

Buying CKB for local currency usually means depositing into a custodial P2P desk or passing KYC on a fiat ramp. Twine keeps CKB in Fiber channels the traders already control:

1. The buyer mints secret `S_b` on their own Fiber node and builds a payout invoice under `H(S_b)`.
2. The daemon creates a hold invoice under the same hash and stores no preimage.
3. The seller pays that hold from their own node (Fiber Studio or any Fiber wallet).
4. Fiat moves bank-to-bank / Zelle-to-Zelle outside Twine.
5. On release the hub pays the buyer's invoice, learns `S_b`, arms the watchtower, and settles the hold.
6. If nobody settles, Fiber's 36-hour timelock returns the CKB to the seller.

The operator can coordinate the book and a published solver, but cannot settle locked coins before paying the buyer.

## How a trade works

Someone posts CKB for sale. Someone else takes a slice and includes a Fiber invoice that should receive the CKB. The daemon creates a hold invoice under that invoice's payment hash and does not learn the preimage. The seller pays the hold from their own node. The daemon then asks Fiber whether that invoice has a route, using `send_payment` with `dry_run: true`. That check builds a path and does not send the payment or reveal the preimage. Fiat instructions appear only when a route comes back. If it does not, the trade stays `awaiting-route` until the buyer sends the same invoice again. The same dry run runs again when the buyer marks the payment sent. If it fails, the click is refused and the trade returns to `awaiting-route`. The buyer sends fiat directly, then marks it sent. The seller releases. The daemon pays the buyer's invoice, learns the preimage from that payment, and settles the hold. If the trade does not finish, Fiber's timelock returns the CKB to the seller.

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
- Fiber hold invoices last 36 hours. An unpaid hold expires after 1 hour. The buyer can mark fiat sent only while a 2-hour release grace, a 4-hour solver window, and Fiber's routing time are still left. That routing time is at least 30 minutes and is longer when the incoming TLC needs it. After fiat is marked sent, the seller has 2 hours to release. If that grace passes, the buyer is told to open a dispute and seal this trade's chat to the solver.
- If the buyer's invoice has no route when the hold locks, fiat stays closed (`awaiting-route`) until they paste that same invoice again. A later routing failure after fiat was sent still waits in `awaiting-invoice`. An invoice with an expiry has to stay payable through the hold. An expired one cannot be replaced, so the hold refunds. If the seller had released, or the solver recorded that the fiat arrived, that refund also publishes a public `fiat-owed` record.
- Disputes with sealed solver evidence. The daemon still cannot read the thread.
- Public order book (kind `31420`), Fiber node announcement with pubkey, listen addresses, optional solver, and solver window (kind `31421`), and public trade outcomes (kind `31422`).
- SQLite for orders, trades, and processed events. The preimage is removed from the daemon database after settle; the watchtower copy remains for channel safety.
- `twine-push`, which wakes a phone without putting trade content in the notification.
- Host docs in `DEPLOY.md` and a Fiber node image under `docker/`.

There is no fee, no rating aggregate, no bond, no UDT market, no `twine-cli`, and no coordinator Docker image yet.

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
| `FIBER_NETWORK` | no | `testnet` (default) or `mainnet`. Selects invoice currency `Fibt` or `Fibb`. |
| `TWINE_SOLVER` | no | Nostr pubkey allowed to send `resolve`, hex or `npub`. Stored as lowercase hex. Empty means disputes are refused. |
| `TWINE_DB` | no | SQLite path. Default `./twine.db`. |
| `RUST_LOG` | no | Tracing filter. Default `info`. |

`twine-push` is a separate crate. It is not started by `cargo run` in the repo root.

| Variable | Required | Meaning |
| --- | --- | --- |
| `TWINE_RELAYS` | no | Same relay list as the daemon. Default `wss://relay.backr.site`. |
| `TWINE_PUSH_LISTEN` | no | HTTP bind. Default `0.0.0.0:8787`. |
| `TWINE_PUSH_TOKENS` | no | JSON file of device tokens. Default `./twine-push-tokens.json`. |
| `FCM_SERVICE_ACCOUNT` | no | Firebase service-account JSON path, or the JSON itself. Without it, doorbells are logged only. |
| `GOOGLE_APPLICATION_CREDENTIALS` | no | Fallback path if `FCM_SERVICE_ACCOUNT` is unset. |
| `RUST_LOG` | no | Tracing filter. Default `info`. |

## Fiber channels

The daemon pays the buyer's invoice. It does not open, fund, or rebalance a channel. Before fiat, it checks the route with Fiber `send_payment` `dry_run: true` (v0.9.1: the call builds a path, stores no session, and sends no onion). A wallet that cannot receive gets its own inbound, then sends the same invoice again. Kind `31421` publishes this node's pubkey and listen addresses. Kind `31422` publishes a `fiat-owed` trade record when the CKB returns after the fiat had arrived, a `buyer-unpaid` record when the solver finds the buyer did not pay, and an `unresolved` record when the solver window ends with no decision.

## Develop

```bash
cargo test
cargo run --release
```

`cargo run` reads the variables above from the environment. Tests that talk to a live Fiber node stay ignored unless `TWINE_RPC` and `TWINE_RPC_TOKEN` are set:

```bash
cargo test -- --ignored
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
| `docker/` | Fiber node image. |
| `config/` | Fiber config templates used with the image. |
| `DEPLOY.md` | VPS install for the daemon, Fiber, and `twine-push`. |
| `PROPOSAL.md` | Grant proposal, protocol design, and milestone plan. |
