# Twine

Twine is a non-custodial peer-to-peer market for Nervos CKB. Traders pay each other in fiat outside the protocol. CKB stays in each trader's own Fiber wallet.

This repository is the coordinator daemon (`twine-daemon`) and a content-free push doorbell (`twine-push`). The daemon talks to the [Flutter client](https://github.com/chukwuma619/twine_app) over Nostr and to one Fiber node over JSON-RPC. It does not hold a trader's Fiber key, does not hold fiat, and does not read the trade chat.

A host runs the daemon, one Fiber node the hold invoices are locked to, and `twine-push` so a phone can wake. Host setup is [DEPLOY.md](DEPLOY.md). The Fiber image is [docker/README.md](docker/README.md). The doorbell is [twine-push/README.md](twine-push/README.md).

## How a trade works

Someone posts CKB for sale. Someone else takes a slice and includes a Fiber invoice that should receive the CKB. The daemon creates a hold invoice under that invoice's payment hash and does not learn the preimage. The seller pays the hold from their own node. The daemon then asks Fiber whether that invoice has a route, using `send_payment` with `dry_run: true`. That check builds a path and does not send the payment or reveal the preimage. Fiat instructions appear only when a route comes back. If it does not, the trade stays `awaiting-route` until the buyer sends the same invoice again. The same dry run runs again when the buyer marks the payment sent. If it fails, the click is refused and the trade returns to `awaiting-route`. The buyer sends fiat directly, then marks it sent. The seller releases. The daemon pays the buyer's invoice, learns the preimage from that payment, and settles the hold. If the trade does not finish, Fiber's timelock returns the CKB to the seller.

Cancel is only possible before the hold is locked. Either party can open a dispute when `TWINE_SOLVER` is set. The node announcement publishes that solver and a 4-hour decision time. The solver can award the buyer, refund the seller when the buyer did not pay (`buyer-unpaid`), or record `fiat-owed` when the fiat arrived and the CKB still cannot be delivered. A buyer award or `fiat-owed` requires the chat key. The daemon stores that the key was shared and does not keep the key. A `fiat-sent` click does not create `fiat-owed`. It is written when the seller releases, or when the solver records it. A decision after Fiber can no longer start the payout still writes that record, and the CKB returns to the seller. If fiat was marked sent and neither the seller nor the solver decides before the 4-hour window ends, the daemon publishes `unresolved` and refuses a later release. That record is not `buyer-unpaid` and not `fiat-owed`, and a later decision does not replace it. The Fiber refund still happens. A dispute opened before fiat is marked sent still ends at the fiat deadline. A fiat-sent click during a dispute starts the same release clock and leaves the dispute open. An instance with no solver says so on the payment screen before the buyer pays.

## What it does today

- Sell posts with a price, a fiat currency, and min and max. Partial takes are allowed, and several trades can be open on one post.
- Fiber hold invoices last 36 hours. An unpaid hold expires after 1 hour. The buyer can mark fiat sent only while a 2-hour release grace, a 4-hour solver window, and Fiber's routing time are still left. That routing time is at least 30 minutes and is longer when the incoming TLC needs it. After fiat is marked sent, the seller has 2 hours to release. If that grace passes, the buyer is told to open a dispute and share the chat key.
- If the buyer's invoice has no route when the hold locks, fiat stays closed (`awaiting-route`) until they paste that same invoice again. A later routing failure after fiat was sent still waits in `awaiting-invoice`. An invoice with an expiry has to stay payable through the hold. An expired one cannot be replaced, so the hold refunds. If the seller had released, or the solver recorded that the fiat arrived, that refund also publishes a public `fiat-owed` record.
- Disputes. A party can share the chat key with the solver. The daemon still cannot read the thread.
- A public order book and the operator's Fiber pubkey and listen addresses on Nostr.
- SQLite for orders, trades, and processed events. The preimage is removed from the database after settle.
- `twine-push`, which wakes a phone without putting trade content in the notification.

There is no fee, no rating, no bond, and no UDT market.

## Planned

Not in this tree yet. The list is the work described in [PROPOSAL.md](PROPOSAL.md).

- **Payment windows.** Each fiat transfer gets a fiat deadline. It ends early enough to leave the 2-hour release grace, the 4-hour solver window, and Fiber's routing time.
- **Trade record.** A public record per Nostr key: settled count, completion rate, median time to pay, and median time to release. Reputation mode keeps that history on the trader's key. Privacy mode uses a fresh key for one trade.
- **Hub capacity.** The node announcement publishes how much CKB the hub can receive and pay, plus an operator profile.
- **A quieter book.** Unrefreshed ads expire. Maintenance mode refuses new takes and lets open holds finish. One trade is in flight per ad.
- **Fees.** An operator fee (0% on the public hub) and a 0.1% developer fee. Both are shown before the seller locks, and both come out of the released amount. A developer-fee payment that does not route leaves the trade settled.
- **twine-cli.** Post, take, chat, release, and retry from a terminal. The solver awards a dispute from the CLI.
- **UDT markets.** Whitelisted CKB user-defined tokens, one asset per channel.
- **Packaging.** A coordinator image, and twine-web for guides, a live order book, and a public node directory.

## Configuration

Export these before `cargo run`. On a VPS, systemd loads the same variables. See [DEPLOY.md](DEPLOY.md).

| Variable | Required | Meaning |
| --- | --- | --- |
| `TWINE_NOSTR_SECRET` | yes | Daemon Nostr key. Hex or `nsec`. Startup logs the matching `npub`. |
| `TWINE_RELAYS` | yes | Comma-separated websocket URLs. At least one. |
| `TWINE_RPC` | yes | Fiber JSON-RPC URL, for example `http://127.0.0.1:8229`. |
| `TWINE_RPC_TOKEN` | yes | Biscuit for that RPC, sent as `Authorization: Bearer`. |
| `FIBER_NETWORK` | no | `testnet` (default) or `mainnet`. Selects invoice currency `Fibt` or `Fibb`. |
| `TWINE_SOLVER` | no | Hex Nostr pubkey allowed to send `resolve`. Empty means disputes are refused. |
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

## Layout

| Path | What it is |
| --- | --- |
| `src/` | Daemon: trade engine, Nostr, Fiber RPC, SQLite. |
| `twine-push/` | Content-free FCM doorbell. |
| `docker/` | Fiber node image. |
| `DEPLOY.md` | VPS install for the daemon, Fiber, and `twine-push`. |
