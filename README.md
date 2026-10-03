# Twine

Peer-to-peer CKB market settled on Fiber. Fiat moves outside the protocol. The seller's coins sit in a Fiber hold invoice until the seller releases them, or until the 16-hour timelock refunds the seller.

Twine coordinates the trade over Nostr. It never holds a user's Fiber key. Each trader runs their own `fnn`. The operator runs this daemon and one Twine Fiber node.

Milestone 1 is one sell on Fiber testnet. Either party can open a dispute, and `TWINE_SOLVER` resolves it. There is no HTTP API for trades, no buy ads, no fees charged by Twine, no ratings, no bonds, no UDT, and no chat.

Clients send NIP-44 encrypted events to the daemon (`kind` 4242). Posted orders are public addressable events (`kind` 31420). Those kinds are application constants, not NIPs.

