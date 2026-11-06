# twine-push

Content-free doorbell for Twine. Watches kind 4242 and 4243 `#p` tags on the
official relay and sends FCM HTTP v1 with `{ "type": "trade_update" | "chat_wake" }`.

The subscription is live-only (`limit: 0`) so a restart does not replay stored
events. Device tokens are written to `TWINE_PUSH_TOKENS` after each
register/unregister.

```
TWINE_RELAYS=wss://relay.backr.site \
TWINE_PUSH_LISTEN=0.0.0.0:8787 \
TWINE_PUSH_TOKENS=/var/lib/twine-push/tokens.json \
FCM_SERVICE_ACCOUNT=/path/to/service-account.json \
cargo run --release
```

`FCM_SERVICE_ACCOUNT` is a Firebase service-account JSON file or the JSON
itself. `GOOGLE_APPLICATION_CREDENTIALS` is accepted as a fallback.

The app registers at `https://push.backr.site` unless built with
`--dart-define=TWINE_PUSH_URL=...`. A VPS install with systemd, Caddy, and
the service-account file is in [DEPLOY.md](../DEPLOY.md#push-doorbell).

- `POST /register` `{ "pubkey", "token" }`
- `POST /unregister` `{ "pubkey", "token" }`
- `GET /health`
