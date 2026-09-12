# Twine

Non-custodial peer-to-peer exchange for Nervos CKB and Fiber.

Two strands, bound until the trade is done.

## The problem

Buying or selling CKB for local money still means a custodial exchange or a KYC on-ramp. Those are the only real doors: Binance-style P2P that locks your coins on their books, or card/bank ramps like Alchemy Pay that take your identity. Neither is usable if you cannot pass KYC, your bank will not touch crypto, or you do not want a company holding the trade.

On-chain DEXes do not help. UTXOSwap and the rest swap CKB for other tokens. They cannot take naira, bolívares, or a bank transfer.

Bitcoin already has a way out of this. Mostro (and lnp2pBot before it) lets two people trade sats for fiat over Lightning: no account, no KYC, seller’s coins locked in a hold invoice instead of deposited. Nervos now has the same primitive on Fiber. Nobody has used it for this. There is no KYC-free, non-custodial CKB ↔ local-fiat desk.

That is the gap Twine fills. Two people trade CKB (or later a Fiber UDT) for local fiat. The seller’s funds stay locked in their Fiber channel until the fiat arrives. Twine coordinates the trade. It never holds the money.

## How it works

1. A maker posts a buy or sell order.
2. A taker accepts it.
3. The seller pays a Fiber hold invoice. CKB stays locked in their channel, not in Twine.
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
