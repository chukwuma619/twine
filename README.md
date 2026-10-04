# Twine

Peer-to-peer CKB market settled on Fiber. Fiat moves outside the protocol. The seller's coins sit in a Fiber hold invoice until the seller releases them, or until that hold expires and Fiber refunds the seller. After the seller locks, the buyer has 15 minutes to pay. A dispute stays open until the hold must be refunded.

Twine coordinates the trade over Nostr. It never holds a user's Fiber key. Each trader runs their own `fnn`. The operator runs this daemon and one Twine Fiber node.

A post is either a sell or a buy. The poster sets the price, the fiat currency, and how much CKB the post still covers. The daemon keeps its own catalog of payment methods and ships with two. The coordinator adds or removes methods in `SUPPORTED_PAYMENT_METHODS`. A post can use only those. A sell post and a buy post both name catalog ids. The public order shows the id, kind, label, and currency. The two methods it ships with are GTBank for NGN and Zelle for USD. A post can use a method only when that method's currency is the post's fiat currency, so a USD post shows the buyer only USD methods. The daemon does not store account details. Those are shared in the client chat. `waiting-fiat` names the chosen method after the seller locks the hold. Anyone except the poster can take it. On a sell post the taker buys CKB. On a buy post the taker sells CKB. Either way the CKB seller locks the hold, and the CKB buyer submits the payout invoice. The daemon watches the hold from the moment it is created. Either party can open a dispute, and the CKB seller can still release until `TWINE_SOLVER` decides. There is no HTTP API for trades, no fees charged by Twine, no ratings, no bonds, no UDT, and no chat. A public order's `side` is `sell` or `buy`.

Clients send NIP-44 encrypted events to the daemon (`kind` 4242). Posted orders are public addressable events (`kind` 31420). Those kinds are application constants, not NIPs.

