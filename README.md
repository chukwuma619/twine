# Twine

Twine is a peer-to-peer CKB market settled on [Fiber](https://github.com/nervosnetwork/fiber). Fiat moves outside the protocol. The seller's coins sit in a Fiber hold invoice until the seller releases them, or until that hold expires and Fiber refunds the seller.

This repository is the coordinator daemon (`twine-daemon`). It speaks Nostr to clients and JSON-RPC to one Fiber node (`fnn`). It never holds a user's Fiber key. Each trader runs their own node and pays or receives on it. The operator runs this daemon and one Twine Fiber node that the hold invoices are locked to.

There is no HTTP API for trades. Twine charges no fee, keeps no ratings, takes no bond, and does not use UDTs. The two traders chat on the client. The daemon does not read that thread.

## Roles

| Role | Who | What they do |
| --- | --- | --- |
| Poster | Whoever creates the post | Sets the side, price, currency, size, and payment methods. Only they can cancel the post before it is taken. |
| Seller | The CKB seller | Locks the hold invoice and later releases. On a sell post this is the poster. On a buy post this is the taker. |
| Buyer | The CKB buyer | Pays fiat, then names the Fiber invoice that should receive the coins. On a sell post this is the taker. On a buy post this is the poster. |
| Solver | `TWINE_SOLVER` | A Nostr pubkey that can resolve a dispute. Disputes are refused while this is unset. |
| Operator | Whoever runs this process | Runs the daemon and the Twine Fiber node. Does not custody trader keys. |

Anyone except the poster can take a post. A post has at most one open trade. The next take waits until that trade is settled, canceled, or expired.

## How a trade works

1. **Post.** The poster sends `new-order`. The daemon publishes a public order. `side` is `sell` (offering CKB) or `buy` (bidding for CKB). `status` stays `open` until the poster cancels it. A filled post stays `open` with `available_ckb` at `0`.

2. **Take.** Someone else sends `take` with a fiat amount inside the post's min and max, their Fiber pubkey, and one payment-method id from the post. On a sell post the taker is the buyer, so `take` also carries the buyer's Fiber invoice. The daemon debits that slice, creates the seller's hold under that invoice's payment hash, and does not learn the preimage. Both parties receive `pay-invoice`. On a buy post the maker is the buyer, so `take` has no invoice. Both parties receive `need-invoice`, and the buyer sends `payout-invoice` before the hold exists. A payment hash already used by another trade is rejected. The same hash is required if the buyer later replaces the invoice.

3. **Lock.** The seller pays the hold invoice from their own Fiber node. The daemon polls until Fiber reports the invoice `Received`, then sends `waiting-fiat` to both parties. That message names the chosen payment method. Account numbers are not stored here. Traders share them in the client chat.

4. **Fiat.** The buyer pays outside Twine and sends `fiat-sent`. The payout invoice is already the one from take or `payout-invoice`, so this message can leave `invoice` empty. A replacement invoice must use the same payment hash. The daemon replies `fiat-sent-ok`.

5. **Release.** The seller sends `release`. The daemon records the hold hash, pays the buyer's invoice (`send_payment`), and only then reads the preimage Fiber learned from that payment. It registers that preimage with the watchtower and settles the hold. Both parties receive `settled`. Until the payout succeeds, the daemon cannot settle. If the payout fails, the buyer receives `new-invoice` and can submit another invoice with the same payment hash.

6. **Refund.** If the buyer does not send fiat in time, or a later phase runs into the safety deadline below, the daemon sends `refunding`. A preimage is removed only when one was stored. Fiber's timelock then returns the hold to the seller. The slice goes back on the post when Fiber reports the hold invoice `Expired`, and both parties receive `expired`. A buy post that never receives an invoice is canceled after the invoice expiry and the slice returns.

Cancel is only possible before the hold is locked. The poster can cancel an untaken post. Either party can cancel while the hold invoice is still `Open`. Once Fiber reports `Received` or `Paid`, cancel is refused and the coins wait for release or for the timelock.

### States

| State | Meaning |
| --- | --- |
| `waiting-invoice` | A buy post was taken. The buyer has not sent the payout invoice, so no hold exists yet. |
| `waiting-hold` | Hold invoice exists. The seller has not locked it yet. The daemon does not know the preimage. |
| `waiting-fiat` | Hold is locked. The buyer has the payment window to pay and name a payout invoice. |
| `fiat-sent` | The buyer named a payout invoice. Waiting for the seller to release. |
| `releasing` | The daemon is paying the buyer, then settling the hold. |
| `awaiting-invoice` | The payout failed. The buyer must send another invoice with the same payment hash. |
| `disputed` | A party opened a dispute. The seller can still release. The solver can resolve. |
| `refunding` | Fiber will refund the seller at the timelock. A stored preimage is dropped. The slice is still reserved. |
| `settled` | The buyer was paid and the hold was settled. |
| `canceled` | The trade or the post was canceled before the hold locked. A canceled trade returns its slice. |
| `expired` | The hold invoice expired. The slice is back on the post. |

The daemon watches every state from `waiting-hold` through `refunding`, and also `waiting-invoice` so an unanswered buy post does not reserve the slice forever. On startup it re-registers a preimage only after a payout has revealed one, drops that preimage for any trade already `refunding`, and resumes polling.

### Disputes

Either party can send `dispute` during `waiting-fiat`, `fiat-sent`, or `awaiting-invoice`, and only when `TWINE_SOLVER` is set. The buyer may include a payout invoice. Either party may include a `conversation_key` (64 hex characters, 32 bytes). A bad key is ignored and does not block the dispute.

Both parties receive `disputed` with no payload. The solver receives `disputed` with the conversation key when it was valid, so the solver can read the client thread. The daemon still does not.

The seller can `release` during a dispute when a payout invoice is already stored. Otherwise only the solver can `resolve`:

| `winner` | Result |
| --- | --- |
| `seller` | `refunding`. A stored preimage is removed. |
| `buyer` | `releasing`, which requires a payout invoice from the dispute, the resolve message, or an earlier `fiat-sent`. |

A resolve that arrives after the safety deadline refunds the seller instead.

### Time

All of these are fixed in `src/constant.rs`.

| Constant | Value | Effect |
| --- | --- | --- |
| Hold | 36 hours | How long the seller's coins stay in the Fiber hold. Fiber accepts 16 to 48 hours. A received hold cannot be canceled, so this is also how long the coins can sit if the trade never finishes. |
| Payment window | 15 minutes | After the hold is `Received`, the buyer must pay and send `fiat-sent`. After that the trade goes to `refunding`. |
| Safety margin | 30 minutes | A payout or a buyer-wins resolution must start at least this long before the hold ends. `fiat-sent`, `awaiting-invoice`, and `disputed` refund the seller after that. |
| Unpaid hold | 1 hour | An unpaid hold invoice expires. The slice returns to the post. |
| Poll | 2 seconds | How often the daemon asks Fiber about an open hold or payout. |

`1` CKB is `100_000_000` shannons. Fiat and CKB amounts in messages are decimal strings. The daemon rounds a conversion that lands between shannons half-up to the nearest shannon.

### Payment methods

The catalog lives in `SUPPORTED_PAYMENT_METHODS`. This build ships with two:

| id | kind | label | currency |
| --- | --- | --- | --- |
| `gtbank` | `bank` | GTBank | `NGN` |
| `zelle` | `wallet` | Zelle | `USD` |

A post names up to five ids. Each id must be in the catalog, and its currency must be the post's fiat currency. The public order shows `id`, `kind`, `label`, and `currency`. The daemon does not store account details.

Add or remove methods by editing that list and restarting. Startup writes the list into SQLite and deletes catalog rows that are no longer in it. A method that an open post still references cannot be removed.

## Nostr

Clients encrypt actions to the daemon. The daemon encrypts replies back. Orders, the Fiber node pubkey, and the payment catalog are public. The event kinds below are application constants, not NIPs.

| Kind | Tag | Content |
| --- | --- | --- |
| `4242` | `p` is the recipient | NIP-44 (v2) ciphertext. An action to the daemon, or a reply from it. |
| `31420` | `d` is the order id | Public JSON order. Addressable. |
| `31421` | `d` is `fiber-node` | Public JSON `{"pubkey":"<fnn pubkey>"}`. The app shows this so a wallet can open a channel to the operator's node. |
| `31422` | `d` is `payment-methods` | Public JSON `{"methods":[...]}`. The app reads this for the currencies and methods on a new post. |
| `4243` | `p` is the other trader, `t` is the trade id | NIP-44 chat between the two traders. The client uses this. The daemon does not subscribe to it. |

On startup the daemon connects to `TWINE_RELAYS`, subscribes to kind `4242` events tagged to its own pubkey, logs its `npub`, announces the Fiber node and the payment catalog, and republishes every stored order. One relay accepting an event is enough. A relay that already stored it treats a retry as a duplicate. An event id that was already handled is ignored.

`pay-invoice` and `waiting-fiat` include `seller_nostr` and `buyer_nostr` so each client can find the other.

### Envelope

Actions and replies are JSON, then NIP-44 encrypted:

```json
{
  "action": "take",
  "trade_id": "optional-trade-id",
  "payload": {}
}
```

`trade_id` and `payload` are omitted when empty. An unknown `action` gets `cant-do`.

### Client actions

`new-order`

```json
{
  "side": "sell",
  "fiber_pubkey": "<poster's fnn pubkey>",
  "available_ckb": "10",
  "fiat_currency": "NGN",
  "price_per_ckb": "1500",
  "min": "1000",
  "max": "9000",
  "payment_methods": [{ "method_id": "gtbank" }]
}
```

`side` is `sell` or `buy`. `available_ckb`, `price_per_ckb`, `min`, and `max` are positive decimals. `min` must be less than or equal to `max`. `fiber_pubkey` is the poster's Fiber node.

`take`

```json
{
  "order_id": "<order id>",
  "fiat_amount": "1000",
  "fiber_pubkey": "<taker's fnn pubkey>",
  "payment_method_id": "gtbank"
}
```

`fiat-sent` requires `trade_id` and `{ "invoice": "<fiber invoice>" }`. Only the buyer can send it.

`release` requires `trade_id` and no payload. Only the seller can send it, and only after a payout invoice is stored.

`cancel` cancels a post or a trade. A post uses `{ "order_id": "<order id>" }` and no `trade_id`. A trade sets `trade_id` on the envelope.

`dispute` requires `trade_id`. The payload is optional:

```json
{
  "invoice": "<buyer payout invoice, optional>",
  "conversation_key": "<64 hex chars, optional>"
}
```

`resolve` is the solver only. `winner` is `buyer` or `seller`. `invoice` is optional and is the buyer's payout invoice when the solver awards the buyer.

```json
{ "winner": "seller", "invoice": null }
```

### Daemon replies

| Action | Who receives it | Payload |
| --- | --- | --- |
| `pay-invoice` | Seller and buyer | `invoice`, `amount_shannons`, `order_id`, `seller_nostr`, `buyer_nostr` |
| `waiting-fiat` | Seller and buyer | `fiat_amount`, `fiat_currency`, `reference` (the trade id), `kind`, `label`, `currency`, `seller_nostr`, `buyer_nostr` |
| `fiat-sent-ok` | Seller and buyer | none |
| `new-invoice` | Seller and buyer | `reason` |
| `disputed` | Seller and buyer | none |
| `disputed` | Solver | `conversation_key` when the client sent a valid one |
| `refunding` | Seller and buyer | none |
| `settled` | Seller and buyer | none |
| `canceled` | The poster, when a post is canceled | `order_id` |
| `canceled` | Seller and buyer, when a trade is canceled | none |
| `expired` | Seller and buyer | none |
| `cant-do` | The sender | `reason` |

A public order is published alongside the replies that change the book (`new-order`, `take`, cancel, expire):

```json
{
  "order_id": "<uuid>",
  "side": "sell",
  "maker_nostr_pubkey": "<hex pubkey>",
  "maker_fiber_pubkey": "<fnn pubkey>",
  "available_ckb": "10",
  "fiat_currency": "NGN",
  "price_per_ckb": "1500",
  "min": "1000",
  "max": "9000",
  "payment_methods": [
    { "id": "gtbank", "kind": "bank", "label": "GTBank", "currency": "NGN" }
  ],
  "hold_hours": 36,
  "status": "open"
}
```

`status` is `open` or `canceled`.

## Configuration

The process reads its environment directly. It does not load a `.env` file. Copy `.env.example` and export it yourself.

| Variable | Required | Meaning |
| --- | --- | --- |
| `TWINE_NOSTR_SECRET` | yes | Daemon Nostr key. Hex or `nsec`. Startup logs the matching `npub`. People paste that `npub` and `TWINE_RELAYS` into the app. |
| `TWINE_RELAYS` | yes | Comma-separated websocket URLs. At least one. |
| `TWINE_RPC` | yes | Fiber JSON-RPC URL, usually `http://127.0.0.1:8227`. |
| `TWINE_RPC_TOKEN` | yes | Biscuit for that RPC, sent as `Authorization: Bearer`. |
| `FIBER_NETWORK` | no | `testnet` (default) or `mainnet`. Selects invoice currency `Fibt` or `Fibb`. |
| `TWINE_SOLVER` | no | Hex Nostr pubkey allowed to send `resolve`. Empty means disputes are refused. |
| `TWINE_DB` | no | SQLite path. Default `./twine.db`. |
| `RUST_LOG` | no | Tracing filter. Default `info`. |

The Fiber node checks the biscuit against `rpc.biscuit_public_key`. The token needs:

```
read("node");
read("invoices"); write("invoices");
read("payments"); write("payments");
node("<this fnn pubkey>");
write("watchtower");
```

`create_preimage` refuses a token that has no `node` fact. See Fiber's [biscuit auth](https://github.com/nervosnetwork/fiber/blob/main/docs/biscuit-auth.md) for how to mint the key pair.

`FIBER_SECRET_KEY_PASSWORD` and `CKB_RPC_URL` belong to the Fiber node, not to this daemon. They are in `.env.example` because the node image reads them. Testnet uses Fiber's bundled `https://testnet.ckbapp.dev/`. Mainnet must set `CKB_RPC_URL` to a CKB RPC the node can reach. A store cannot change chains. To switch, stop the node and move `./fiber-node` aside.

## Run

Install a Rust toolchain that supports edition 2024, then:

```bash
cp .env.example .env
# fill in the daemon key, relays, and RPC token
set -a && source .env && set +a
cargo run --release
```

The daemon refuses to start when no relay connects, when the RPC token is empty, or when `FIBER_NETWORK` is neither `testnet` nor `mainnet`.

The first successful RPC call stores that Fiber node's pubkey. Restarting against the same node is fine. Restarting against a different node while a hold is still open fails on purpose, and so does booting with open holds and no stored pubkey.

## Fiber node

Traders do not use this node as their wallet. They open a channel to it so a hold invoice can be paid. The daemon announces the pubkey as kind `31421`.

`docker/` builds an `fnn` image from [nervosnetwork/fiber](https://github.com/nervosnetwork/fiber). It is the Fiber node, not this daemon. Setup, ports, `fnn-cli`, and store migration are in [docker/README.md](docker/README.md). Bundled configs are `config/testnet/config.yml` and `config/mainnet/config.yml`. RPC listens on `127.0.0.1:8227` in those files.

## Persistence

SQLite holds orders, the payment catalog, trades, a payout preimage after the buyer has been paid, processed Nostr event ids, and the pinned Fiber pubkey. On Unix the file is created mode `0600`. The preimage is deleted from the database after settle. The watchtower copy stays, because a force-close of an older commitment can still need it. Keep the database with the node it was pinned to. `.env`, `*.db`, and `fiber-node/` are gitignored.

## Tests

```bash
cargo test
```

Tests that talk to a live node are ignored unless you pass `--ignored` and export `TWINE_RPC` and `TWINE_RPC_TOKEN`:

```bash
cargo test -- --ignored
```

## Layout

| Path | What it is |
| --- | --- |
| `src/main.rs` | Process entry. Connects Nostr and Fiber, then dispatches events. |
| `src/engine.rs` | Trade engine, hold and payout polls, watchtower arming. |
| `src/state.rs` | Who may move a trade, and which state comes next. |
| `src/app/` | One file per client action. |
| `src/nostr.rs` | Connect, encrypt, publish. |
| `src/fiber/` | JSON-RPC for invoices, payments, the node pubkey, and the watchtower. |
| `src/db.rs` | SQLite. |
| `src/constant.rs` | Time limits, catalog, and event kinds. |
| `docker/` | Fiber node image. |
