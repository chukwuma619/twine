# Twine

Twine is a non-custodial peer-to-peer market for Nervos CKB. Traders pay each other in fiat outside the protocol. CKB stays in each trader's own Fiber wallet.

This repository is the coordinator daemon (`twine-daemon`) and a content-free push doorbell (`twine-push`). The daemon talks to the [Flutter client](https://github.com/chukwuma619/twine_app) over Nostr and to one Fiber node over JSON-RPC. It does not hold a trader's Fiber key, does not hold fiat, and does not read the trade chat.

A host runs the daemon, one Fiber node the hold invoices are locked to, and `twine-push` so a phone can wake. Host setup is [DEPLOY.md](DEPLOY.md). The Fiber image is [docker/README.md](docker/README.md). The doorbell is [twine-push/README.md](twine-push/README.md).

## How a trade works

Someone posts CKB for sale. Someone else takes a slice and includes a Fiber invoice that should receive the CKB. The daemon creates a hold invoice under that invoice's payment hash and does not learn the preimage. The seller pays the hold from their own node. The buyer sends fiat directly, then marks it sent. The seller releases. The daemon pays the buyer's invoice, learns the preimage from that payment, and settles the hold. If the trade does not finish, Fiber's timelock returns the CKB to the seller.

Cancel is only possible before the hold is locked. Either party can open a dispute when `TWINE_SOLVER` is set. The solver can award the buyer or let the hold refund the seller.

## What it does today

- Sell posts with a price, a fiat currency, and min and max. Partial takes are allowed, and several trades can be open on one post.
- Fiber hold invoices last 36 hours. An unpaid hold expires after 1 hour. A payout must start at least 30 minutes before the hold ends.
- If the buyer's invoice does not route, they paste that same invoice again after the wallet can receive. An invoice with an expiry has to stay payable through the hold. An expired one cannot be replaced, so the hold refunds.
- Disputes. A party can share the chat key with the solver. The daemon still cannot read the thread.
- A public order book and the operator's Fiber pubkey and listen addresses on Nostr.
- SQLite for orders, trades, and processed events. The preimage is removed from the database after settle.
- `twine-push`, which wakes a phone without putting trade content in the notification.

There is no fee, no rating, no bond, and no UDT market.

## Planned

Not in this tree yet. The list is the work described in [PROPOSAL.md](PROPOSAL.md).

- **Payment windows.** NGN bank transfer and USD via Zelle each get a fiat deadline, ending at least 30 minutes before the 36-hour hold.
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

The daemon pays the buyer's invoice. It does not open, fund, or rebalance a channel. A wallet that cannot receive gets its own inbound, then sends the same invoice again. Kind `31421` publishes this node's pubkey and listen addresses.

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
