# Twine multi-fnn e2e

Local CKB + three `fnn` nodes. Rust `Engine` tests drive the trade flow. FakeFiber stays the default `cargo test` suite.

## Topology

| Role | Fiber node | Default RPC | Job |
| --- | --- | --- | --- |
| hub | node 1 | `http://127.0.0.1:21714` | Twine hold / payout RPC (`TWINE_RPC`) |
| seller | node 2 | `http://127.0.0.1:21715` | Pays the hold (`TWINE_E2E_SELLER_RPC`) |
| buyer | node 3 | `http://127.0.0.1:21716` | Mints the payout invoice (`TWINE_E2E_BUYER_RPC`) |

`setup-channels.sh` opens seller→hub and hub→buyer.

## Prerequisites

- Rust toolchain (Twine + building Fiber `fnn`)
- [`ckb`](https://github.com/nervosnetwork/ckb) and `ckb-cli` on `PATH`
- curl, python3
- Fiber checkout at **v0.10.0-rc3** (override with `FIBER_REPO`):

```bash
git clone https://github.com/nervosnetwork/fiber.git ../fiber
git -C ../fiber checkout v0.10.0-rc3
```

## Run

```bash
make e2e-up    # start CKB + 3 fnn, open channels, write tests/e2e/.env
make e2e       # e2e-up + cargo test -- --ignored
make e2e-down
```

`FIBER_NETWORK=dev` selects invoice currency `Fibd`. Local Fiber RPC has no biscuit auth; `.env` still sets a placeholder token for `HttpFiber`.

First `e2e-up` builds Fiber and initializes the CKB dev chain (several minutes). Do not commit `tests/e2e/.env` or logs. Local/manual only — not in CI yet.
