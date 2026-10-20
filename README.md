# Twine

Twine is a peer-to-peer CKB market. Fiat moves outside the protocol. CKB stays in each trader's Fiber wallet.

This repository is the coordinator daemon (`twine-daemon`). It speaks Nostr to the [Flutter client](https://github.com/chukwuma619/twine_app) and JSON-RPC to one Fiber node (`fnn`). It never holds a user's Fiber key. Each trader pays and receives on their own node. The operator runs this daemon and one Twine Fiber node that the hold invoices are locked to.

There is no HTTP API for trades. Twine charges no fee, keeps no ratings, takes no bond, and does not use UDTs. The two traders chat on the client. The daemon does not read that thread.

## How a trade works

A post offers CKB. The poster is the seller. The taker is the buyer. There is no buy post, and there is no automatic matching: someone picks a post. Anyone except the poster can take it. A post has at most one open trade. The next take waits until that trade is settled, canceled, or expired.

1. Someone posts CKB for sale: price, currency, size, and payment methods.
2. Someone else takes it: a fiat amount inside the post's min and max, their Fiber pubkey, one payment method, and a Fiber invoice that should receive the CKB.
3. The daemon creates a hold invoice under that payout invoice's payment hash. It does not learn the preimage. Both people get `pay-invoice`.
4. The seller pays the hold from their own Fiber node. The daemon waits until Fiber reports it `Received`, then sends `waiting-fiat`. That message names the payment method. Account numbers are not stored here. Traders share them in client chat.
5. The buyer sends fiat outside Twine and marks it sent. The payout invoice is already the one from take, so this message can leave `invoice` empty. A replacement must use the same payment hash.
6. The seller releases. The daemon records the hold hash, pays the buyer's invoice, and only then reads the preimage Fiber learned from that payment. It registers that preimage with the watchtower and settles the hold. Both people get `settled`.
7. If the buyer does not pay in time, or a later phase hits the safety deadline below, Fiber refunds the seller. The slice goes back on the post when the hold expires.

Cancel is only possible before the hold is locked. The poster can cancel an untaken post. Either party can cancel while the hold invoice is still `Open`. Once Fiber reports `Received` or `Paid`, cancel is refused and the coins wait for release or for the timelock.

Until the payout succeeds, the daemon cannot settle. If the payout fails, the buyer receives `new-invoice` and can submit another invoice with the same payment hash. A payment hash already used by another trade is rejected.

The operator does not custody trader keys. The solver (`TWINE_SOLVER`) is a Nostr pubkey that can resolve a dispute. Disputes are refused while this is unset.

### States

| State | Meaning |
| --- | --- |
| `waiting-invoice` | The buyer has not sent the payout invoice, so no hold exists yet. A new sell post does not stay here: `take` includes the invoice. |
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

The daemon watches every state from `waiting-hold` through `refunding`, and also `waiting-invoice` so a trade still waiting on an invoice does not reserve the slice forever. On startup it re-registers a preimage only after a payout has revealed one, drops that preimage for any trade already `refunding`, and resumes polling.

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
| `31421` | `d` is `fiber-node` | Public JSON `{"pubkey":"<fnn pubkey>","addresses":["/ip4/.../tcp/8228"]}`. The app shows this so a wallet can connect and open a channel to the operator's node. |
| `31422` | `d` is `payment-methods` | Public JSON `{"methods":[...]}`. The app reads this for the currencies and methods on a new post. |
| `4243` | `p` is the other trader, `t` is the trade id | NIP-44 chat between the two traders. The client uses this. The daemon does not subscribe to it. |

On startup the daemon connects to `TWINE_RELAYS`, subscribes to kind `4242` events tagged to its own pubkey, logs its `npub`, announces the Fiber node and the payment catalog, and republishes every stored order. One relay accepting an event is enough. A relay that already stored it treats a retry as a duplicate. An event id that was already handled is ignored.

`pay-invoice` and `waiting-fiat` include `seller_nostr` and `buyer_nostr` so each client can find the other.

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

`new-order` creates a sell post. `side` must be `sell`. Amounts are positive decimals. `min` must be less than or equal to `max`. `fiber_pubkey` is the poster's Fiber node.

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

`take` buys from that post. `invoice` is required. It is the buyer's payout invoice, and its payment hash is the one the hold is locked to. `fiber_addr` is optional and must be a **public** Fiber listen multiaddr. Loopback and LAN addresses are refused. A local node should connect outbound to the hub instead.

```json
{
  "order_id": "<order id>",
  "fiat_amount": "1000",
  "fiber_pubkey": "<taker's fnn pubkey>",
  "payment_method_id": "gtbank",
  "invoice": "<buyer payout invoice>",
  "fiber_addr": "/ip4/1.2.3.4/tcp/8228"
}
```

The rest:

| Action | Who | Payload |
| --- | --- | --- |
| `fiat-sent` | Buyer | `{ "invoice", "fiber_addr"? }`. Leave `invoice` empty when `take` already stored it. A replacement must use the same payment hash. |
| `release` | Seller | None. Only after a payout invoice is stored. |
| `cancel` | Poster, or either party | A post uses `{ "order_id" }` and no `trade_id`. A trade sets `trade_id` on the envelope. |
| `dispute` | Either party | Optional `{ "invoice", "conversation_key" }`. |
| `resolve` | Solver | `{ "winner": "buyer" \| "seller", "invoice": null }`. `invoice` is the buyer's payout invoice when the solver awards the buyer. |

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

A public order is published alongside the replies that change the book (`new-order`, `take`, cancel, expire). `status` is `open` or `canceled`. A filled post stays `open` with `available_ckb` at `0`.

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

## Configuration

Export the variables below before `cargo run`. The [run](#run-locally) section writes them to `.env` and exports that file. On a VPS, systemd loads the same variables. That install is [DEPLOY.md](DEPLOY.md).

| Variable | Required | Meaning |
| --- | --- | --- |
| `TWINE_NOSTR_SECRET` | yes | Daemon Nostr key. Hex or `nsec`. Startup logs the matching `npub`. People paste that `npub` and `TWINE_RELAYS` into the app. |
| `TWINE_RELAYS` | yes | Comma-separated websocket URLs. At least one. |
| `TWINE_RPC` | yes | Fiber JSON-RPC URL. Locally this is `http://127.0.0.1:8227`. |
| `TWINE_RPC_TOKEN` | yes | Biscuit for that RPC, sent as `Authorization: Bearer`. |
| `FIBER_NETWORK` | no | `testnet` (default) or `mainnet`. Selects invoice currency `Fibt` or `Fibb`. |
| `TWINE_SOLVER` | no | Hex Nostr pubkey allowed to send `resolve`. Empty means disputes are refused. |
| `TWINE_DB` | no | SQLite path. Default `./twine.db`. |
| `RUST_LOG` | no | Tracing filter. Default `info`. |

`FIBER_SECRET_KEY_PASSWORD` is the Fiber node's password. It encrypts `fiber-node/ckb/key`. The password typed into `ckb-cli` only unlocks that wallet inside `ckb-cli`.

## Fiber channels

Traders open a Fiber channel to this node's pubkey (kind `31421`). Twine does not mint channel balance. Payments need directional liquidity ([Fiber overview](https://www.fiber.world/docs/concept/liquidity/overview)): outbound to send, inbound to receive. Capacity is not the same as usable send or receive.

| Leg | Who | Liquidity |
| --- | --- | --- |
| Lock | Seller → Twine Fiber | Seller outbound (open and fund). Twine inbound room. |
| Payout | Twine Fiber → buyer | Twine outbound toward the buyer. Buyer inbound: funds on the operator's side that can flow to them. A buyer-funded channel alone cannot receive. |

Keep spare CKB on the Twine Fiber node. On release, if the buyer cannot receive, the daemon opens a private one-way channel (`open_channel` with `one_way: true`) funded with the trade plus at least 100 CKB so a default Fiber node auto-accepts. The buyer node must stay online, connected to this hub, and auto-accept. A laptop node does not need a public IP: connect outbound to `announced_addrs`. Loopback and LAN listen addresses are refused. A seller lock on one channel does not create outbound on the buyer's. Multi-hop through another hub only works if this node has a funded path there.

## Run locally

Run this from the repository root. You need a Rust toolchain that supports edition 2024 (`rustup`), Docker, [ckb-cli 2.0.0](https://github.com/nervosnetwork/ckb-cli/releases/tag/v2.0.0), and `openssl`. Start Fiber before the daemon. Traders open a channel to this node. The daemon announces its pubkey as kind `31421`.

Use the published image `nervos/fiber:0.9.1`. Image build, `fnn-cli`, and store migration are in [docker/README.md](docker/README.md).

### CKB key

`fiber-node/ckb/key` is the node's CKB wallet. Fiber's [Docker guide](https://www.fiber.world/docs/quick-start/run-a-node/docker) uses the same file. `account new` asks for a wallet password and prints a `lock_arg`. The `--url` flag makes the printed address a testnet `ckt1` address.

```bash
ckb-cli --url https://testnet.ckbapp.dev account new
mkdir -p "$HOME/twine-ckb" fiber-node/ckb
ckb-cli account export \
  --lock-arg YOUR_LOCK_ARG \
  --extended-privkey-path "$HOME/twine-ckb/exported-key"
head -n 1 "$HOME/twine-ckb/exported-key" | sed 's/^0x//' > "$HOME/twine-ckb/ckb-key"
rm "$HOME/twine-ckb/exported-key"
cp "$HOME/twine-ckb/ckb-key" fiber-node/ckb/key
chmod 600 "$HOME/twine-ckb/ckb-key" fiber-node/ckb/key
ckb-cli --url https://testnet.ckbapp.dev util key-info --privkey-path "$HOME/twine-ckb/ckb-key"
```

`account export` asks for the wallet password again. The exported line starts with `0x`, and Fiber rejects a plaintext key in that form. Send testnet CKB to the `ckt1` address from `key-info`. The node spends it on chain fees. A `ckb1` address is the same key on mainnet.

An account already in `ckb-cli` skips `account new` and exports with its own `lock_arg`. A key from another wallet is one line of 64 hex characters with no `0x` prefix: write that line to `$HOME/twine-ckb/ckb-key` and copy it to `fiber-node/ckb/key`.

On first start, Fiber encrypts `fiber-node/ckb/key` with the password below. `$HOME/twine-ckb/ckb-key` stays the plaintext copy. Replace `YOUR_FIBER_PASSWORD` before running the block. One line, no spaces. Every later start of this store needs the same password.

```bash
umask 077
cat > "$HOME/twine-ckb/fiber.env" << 'EOF'
FIBER_SECRET_KEY_PASSWORD=YOUR_FIBER_PASSWORD
EOF
chmod 600 "$HOME/twine-ckb/fiber.env"
```

### Fiber node

The commands below start testnet, which uses `https://testnet.ckbapp.dev/`. For mainnet, add `-e FIBER_CONFIG_TEMPLATE=/usr/local/share/fiber/config/mainnet/config.yml` to the first `docker run`, then set `ckb.rpc_url` in `fiber-node/config.yml` to a CKB RPC this machine can reach, and set `FIBER_NETWORK=mainnet` in `.env`. A store cannot change chains. To switch, stop the container, move `fiber-node/` aside, and start again.

Run one of the two `docker run` commands. On Linux, `--network host` puts the node's `127.0.0.1:8227` on the machine, and that address is `TWINE_RPC`.

```bash
docker run -d --name fiber-node --restart unless-stopped \
  --network host \
  --env-file "$HOME/twine-ckb/fiber.env" \
  -e RUST_LOG=info \
  -v "$(pwd)/fiber-node:/fiber" \
  nervos/fiber:0.9.1
```

Docker Desktop on macOS keeps the container's loopback inside the container, so skip the command above. This one publishes the Fiber port. RPC stays inside the container until the biscuit public key is set, and `fnn-cli` reads it from there:

```bash
docker run -d --name fiber-node --restart unless-stopped \
  --env-file "$HOME/twine-ckb/fiber.env" \
  -e RUST_LOG=info \
  -v "$(pwd)/fiber-node:/fiber" \
  -p 8228:8228 \
  nervos/fiber:0.9.1
```

Decrypting the key can take about half a minute. Continue once `pubkey` is printed. Run the command again if it is missing.

```bash
docker exec fiber-node fnn-cli info
```

Copy `pubkey`. The token below binds itself to it.

When wallets on another machine open a channel here, or when a buyer runs Fiber on a laptop, uncomment `announced_addrs` in `fiber-node/config.yml` and set a reachable address. Local nodes connect outbound to this address; the hub cannot dial their home network. The restart below picks it up:

```yaml
  announced_addrs:
    - "/ip4/YOUR-PUBLIC-IP/tcp/8228"
```

### RPC token

`biscuit-cli` 0.6.0-beta.2 matches the token format Fiber 0.9.1 checks. The Fiber node checks the biscuit against `rpc.biscuit_public_key`. The token needs `read` and `write` on `invoices`, `payments`, `channels`, and `peers`, `read("node")`, `write("watchtower")`, and `node("<fnn pubkey>")`. `create_preimage` refuses a token that has no `node` fact. Fiber's [biscuit auth](https://github.com/nervosnetwork/fiber/blob/v0.9.1/docs/biscuit-auth.md) describes the format.

```bash
cargo install biscuit-cli --version 0.6.0-beta.2 --locked
umask 077
biscuit keypair | tee "$HOME/twine-ckb/biscuit-keypair.txt"
chmod 600 "$HOME/twine-ckb/biscuit-keypair.txt"
```

The `Private key:` line signs tokens. The `Public key:` line starts with `ed25519/`. Set `FNN_PUBKEY` to the Fiber `pubkey` from `fnn-cli info`.

```bash
FNN_PUBKEY=YOUR_FNN_PUBKEY
cat > "$HOME/twine-ckb/permissions.bc" << EOF
read("node");
read("invoices");
write("invoices");
read("payments");
write("payments");
read("channels");
write("channels");
read("peers");
write("peers");
write("watchtower");
node("$FNN_PUBKEY");
EOF
PRIV=$(awk -F': ' '/^Private key:/{print $2}' "$HOME/twine-ckb/biscuit-keypair.txt")
biscuit generate --private-key "$PRIV" "$HOME/twine-ckb/permissions.bc" \
  | tr -d '\n' > "$HOME/twine-ckb/rpc-token"
echo >> "$HOME/twine-ckb/rpc-token"
chmod 600 "$HOME/twine-ckb/rpc-token"
```

Write the public key into `fiber-node/config.yml`. On Linux leave `RPC_BIND` as written. On macOS set it to `0.0.0.0:8227`. Fiber accepts that bind once the public key is in the file, and Docker can then publish RPC on the machine's loopback. If the write is denied, put `sudo env RPC_BIND="$RPC_BIND" PUB="$PUB"` in front of `python3` and drop the two assignments already on that line.

```bash
RPC_BIND=127.0.0.1:8227
# macOS Docker Desktop: RPC_BIND=0.0.0.0:8227
PUB=$(awk -F': ' '/^Public key:/{print $2}' "$HOME/twine-ckb/biscuit-keypair.txt")
RPC_BIND="$RPC_BIND" PUB="$PUB" python3 - << 'PY'
import os
from pathlib import Path
path = Path("fiber-node/config.yml")
text = path.read_text()
old = 'listening_addr: "127.0.0.1:8227"'
new = f'listening_addr: "{os.environ["RPC_BIND"]}"\n  biscuit_public_key: "{os.environ["PUB"]}"'
if "biscuit_public_key:" not in text:
    if old not in text:
        raise SystemExit("rpc listening_addr not found")
    path.write_text(text.replace(old, new, 1))
PY
grep -n biscuit_public_key fiber-node/config.yml
```

On Linux, restart the container you already started:

```bash
docker restart fiber-node
```

On macOS, remove that container and start it again with RPC published on `127.0.0.1`:

```bash
docker rm -f fiber-node
docker run -d --name fiber-node --restart unless-stopped \
  --env-file "$HOME/twine-ckb/fiber.env" \
  -e RUST_LOG=info \
  -v "$(pwd)/fiber-node:/fiber" \
  -p 127.0.0.1:8227:8227 \
  -p 8228:8228 \
  nervos/fiber:0.9.1
```

Give the key decrypt half a minute, then call RPC with the token. A JSON `node_info` result means the token matches the public key. After this, `fnn-cli info` needs `--auth-token`.

```bash
curl -sS http://127.0.0.1:8227 \
  -H "Authorization: Bearer $(tr -d '\n' < "$HOME/twine-ckb/rpc-token")" \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"node_info","params":[]}'
```

### Daemon

From the repo root. This writes a new Nostr secret. To keep an existing identity, put that `nsec` or 64-character hex secret in `TWINE_NOSTR_SECRET` after the file is written. Add `TWINE_SOLVER=<hex pubkey>` when a solver exists.

```bash
umask 077
NOSTR_SECRET=$(openssl rand -hex 32)
RPC_TOKEN=$(tr -d '\n' < "$HOME/twine-ckb/rpc-token")
cat > .env << EOF
FIBER_NETWORK=testnet
RUST_LOG=info
TWINE_NOSTR_SECRET=${NOSTR_SECRET}
TWINE_RELAYS=wss://relay.damus.io,wss://nos.lol
TWINE_RPC=http://127.0.0.1:8227
TWINE_RPC_TOKEN=${RPC_TOKEN}
EOF
chmod 600 .env
unset NOSTR_SECRET RPC_TOKEN
set -a && source .env && set +a
cargo run --release
```

Startup logs the daemon `npub` and the Fiber pubkey. The daemon exits when no relay connects, the RPC token is empty, or `FIBER_NETWORK` is not `testnet` or `mainnet`. The first successful RPC call pins that Fiber pubkey in SQLite. Restarting against the same node is fine. A different node, while a hold is still open, is refused.

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
| `DEPLOY.md` | VPS install for this daemon and a Fiber node. |
