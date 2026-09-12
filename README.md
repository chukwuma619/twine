# Twine

Non-custodial peer-to-peer exchange for Nervos CKB and Fiber.

Twine lets two people trade CKB (or a Fiber UDT) for local fiat without KYC and without depositing into a platform wallet. The seller’s funds are locked with a Fiber **hold invoice** until fiat is confirmed, then released to the buyer.

Two strands, bound until the trade is done.

## How it works

1. A maker posts a buy or sell order.
2. A taker accepts it.
3. The seller pays a Fiber hold invoice. Sats stay locked in their channel, not in Twine.
4. The buyer sends fiat off-chain (bank, cash, mobile money).
5. The seller releases. Twine settles the hold invoice and pays the buyer’s Fiber invoice.
6. If something goes wrong, either party opens a dispute and a solver decides.

Users do **not** connect a Fiber node to the app. They use a Fiber wallet. Only the Twine operator runs a Fiber node.

## Status

Scaffold only. This repo is the starting point for the daemon and later clients.

## Repo layout (planned)

| Path | Role |
| --- | --- |
| `twine` (this repo) | Coordinator daemon |
| `twine-app` | Trader client |
| `twine-cli` | Command-line client |

## License

MIT
