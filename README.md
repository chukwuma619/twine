# Twine

Peer-to-peer CKB market settled on Fiber. Fiat moves outside the protocol. The seller's coins sit in a Fiber hold invoice until the seller releases them, or until that hold expires and Fiber refunds the seller. Each order chooses the hold, from 16 hours to 48 hours.

Twine coordinates the trade over Nostr. It never holds a user's Fiber key. Each trader runs their own `fnn`. The operator runs this daemon and one Twine Fiber node.

Milestone 1 is a buy or a sell on Fiber testnet. A client sends `take` for either side. On a sell the maker locks the hold. On a buy the taker locks it, and the maker submits the payout invoice. The daemon watches the hold from the moment it is created. Either party can open a dispute, and the seller can still release until `TWINE_SOLVER` decides. There is no HTTP API for trades, no fees charged by Twine, no ratings, no bonds, no UDT, and no chat.

Clients send NIP-44 encrypted events to the daemon (`kind` 4242). Posted orders are public addressable events (`kind` 31420). Those kinds are application constants, not NIPs.

