# Twine

Peer-to-peer CKB market settled on Fiber. Fiat moves outside the protocol. The seller's coins sit in a Fiber hold invoice until the seller releases them, or until the 16-hour timelock refunds the seller.

Twine coordinates the trade over Nostr. It never holds a user's Fiber key. Each trader runs their own `fnn`. The operator runs this daemon and one Twine Fiber node.

Milestone 1 is one sell on Fiber testnet. There is no HTTP API for trades, no buy ads, no fees charged by Twine, no disputes, no ratings, no bonds, no UDT, and no chat.

Clients send NIP-44 encrypted events to the daemon (`kind` 4242). Posted orders are public addressable events (`kind` 31420). Those kinds are application constants, not NIPs.

## Run

```bash
cp .env.example .env
# set FIBER_SECRET_KEY_PASSWORD, TWINE_NOSTR_SECRET, TWINE_RELAYS
mkdir -p fiber-node/ckb
# write the Twine node's CKB private key to fiber-node/ckb/key (mode 600)
docker compose up --build
```

`FIBER_NETWORK` selects the bundled config inside `nervos/fiber:v0.9.1`, the same switch as that image:

| `FIBER_NETWORK` | Template copied on first start |
| --- | --- |
| `testnet` (default) | `/usr/local/share/fiber/config/testnet/config.yml` |
| `mainnet` | `/usr/local/share/fiber/config/mainnet/config.yml` (`FIBER_CONFIG_TEMPLATE`) |

The copy happens only when `fiber-node/config.yml` is missing. A store belongs to one chain. To switch, stop the stack, move `fiber-node/` aside, and start again. Mainnet also needs `CKB_RPC_URL`: the bundled file points at `http://127.0.0.1:8114/`, which this container cannot reach. Testnet uses the bundled `https://testnet.ckbapp.dev/`.

The image binds Fiber RPC to `127.0.0.1:8227`. Compose rewrites that default to `0.0.0.0:8227` so the daemon can reach `fnn` at `http://fiber:8227`. Port `8227` is not published. P2P stays on `8228`. After the first start, set `announced_addrs` in `fiber-node/config.yml` if this node should be reachable from the internet.

```bash
docker compose exec fiber fnn-cli info
```

Relays must accept custom kinds and store NIP-33 addressable events. `wss://relay.damus.io` and `wss://nos.lol` do that today. Kind `31420` keeps the latest `d` tag (order id). Kind `4242` must be delivered to the daemon pubkey.

## Testnet sell

Amounts are shannons. 1 CKB = 100_000_000 shannons. Testnet invoices use currency `Fibt`. Mainnet invoices use `Fibb`. The daemon follows `FIBER_NETWORK`. Routing fees are extra shannons on top of the invoice, paid by whoever sends the payment (seller on lock, Twine on release). Cap is Fiber's default, 5 per thousand.

Open channels in the payment direction only:

1. Fund three Fiber testnet nodes: seller, taker, Twine.
2. Open a seller-to-Twine channel (seller outbound).
3. Open a Twine-to-taker channel (Twine outbound). Size it for the trade plus the routing fee.
4. A reverse channel is not required for this sell.

A posted sell, take, lock, fiat-sent, and release should leave `get_invoice` on the Twine node as `Paid`. Cancel works only while the hold invoice is `Open`. After `Received`, the seller is refunded when the 16-hour timelock expires.

## License

MIT
