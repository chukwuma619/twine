# Deploy

A Twine host runs two processes on one machine: a Fiber node (`nervos/fiber:0.9.1`) and `twine-daemon`. Traders reach the daemon through Nostr. They open a Fiber channel to port **8230**. Fiber's JSON-RPC stays on `127.0.0.1:8229`. Ports `8227` and `8228` stay free for Fiber Studio.

systemd passes the daemon its environment from `/opt/twine/.env`. Variable meanings are in the [README](README.md#configuration).

Paths below use `/opt/twine`. Keep `twine.db` next to the `fiber-node/` store it was pinned to.

## Machine

A current Ubuntu VPS with systemd is enough. Add 4 GB of swap before the Rust compiles so they are not killed.

| Resource | Minimum |
| --- | --- |
| CPU | 2 vCPU |
| Memory | 4 GB RAM, plus 4 GB swap while compiling |
| Disk | 40 GB SSD |

| Port | Bind | Why |
| --- | --- | --- |
| 22/tcp | public | SSH |
| 8230/tcp | public | Fiber peer-to-peer. Wallets open channels here. |
| 8229/tcp | `127.0.0.1` | Fiber JSON-RPC. The daemon is the only client. |

The daemon has no listening port. It opens outbound websockets to `TWINE_RELAYS`, and the Fiber node opens outbound HTTPS to its CKB RPC.

## Prepare the VPS

SSH in as a user that can `sudo`.

```bash
sudo apt update
sudo apt install -y build-essential pkg-config libssl-dev curl ca-certificates git openssl docker.io
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"

sudo fallocate -l 4G /swapfile
sudo chmod 600 /swapfile
sudo mkswap /swapfile
sudo swapon /swapfile
echo '/swapfile none swap sw 0 0' | sudo tee -a /etc/fstab

sudo ufw allow OpenSSH
sudo ufw allow 8230/tcp
sudo ufw enable
```

Rust edition 2024 needs a current stable toolchain. `rustup` installs that.

```bash
sudo mkdir -p /opt/twine
sudo chown "$USER:$USER" /opt/twine
git clone https://github.com/chukwuma619/twine.git /opt/twine
cd /opt/twine
mkdir -p fiber-node/ckb secrets
```

`docker/` in this repo builds Fiber from source. This install uses the published image.

## CKB key

Create the node's wallet on your own computer, then copy the key file to the VPS. Fiber spends from this key when it funds channels. Install [ckb-cli 2.0.0](https://github.com/nervosnetwork/ckb-cli/releases/tag/v2.0.0). Fiber's [Docker guide](https://www.fiber.world/docs/quick-start/run-a-node/docker) uses the same key file.

On first start, Fiber encrypts `fiber-node/ckb/key` with the password in `secrets/fiber.env`. The password you type into `ckb-cli` only unlocks that wallet on your computer. Keep the plaintext key there. Copy it with `scp` so it does not land in SSH history.

### On your computer

`account new` asks for a wallet password and prints a `lock_arg`. The `--url` flag makes the printed address a testnet `ckt1` address.

```bash
ckb-cli --url https://testnet.ckbapp.dev account new
```

Export that account. `account export` asks for the wallet password again. The first line is the private key, and Fiber rejects a plaintext key that starts with `0x`.

```bash
mkdir -p "$HOME/twine-ckb"
ckb-cli account export \
  --lock-arg YOUR_LOCK_ARG \
  --extended-privkey-path "$HOME/twine-ckb/exported-key"
head -n 1 "$HOME/twine-ckb/exported-key" | sed 's/^0x//' > "$HOME/twine-ckb/ckb-key"
rm "$HOME/twine-ckb/exported-key"
chmod 600 "$HOME/twine-ckb/ckb-key"
ckb-cli --url https://testnet.ckbapp.dev util key-info --privkey-path "$HOME/twine-ckb/ckb-key"
```

Send testnet CKB to the `ckt1` address from `key-info`. The node spends it on chain fees. A `ckb1` address is the same key on mainnet.

An account already stored in `ckb-cli` skips `account new` and exports with its own `lock_arg`. A key from another wallet is one line of 64 hex characters with no `0x` prefix: write that line to `$HOME/twine-ckb/ckb-key`.

Copy the key with the same user, host, and `-i` identity as your `ssh` command.

```bash
scp "$HOME/twine-ckb/ckb-key" USER@VPS:/opt/twine/fiber-node/ckb/key
```

## Fiber node

On the VPS, from `/opt/twine`. Replace `YOUR_FIBER_PASSWORD` in the block before running it. One line, no spaces. Every later start of this store needs the same password.

```bash
chmod 600 /opt/twine/fiber-node/ckb/key
umask 077
cat > /opt/twine/secrets/fiber.env << 'EOF'
FIBER_SECRET_KEY_PASSWORD=YOUR_FIBER_PASSWORD
EOF
chmod 600 /opt/twine/secrets/fiber.env
```

The image binds RPC to `127.0.0.1:8227` and the peer port to `8228`. Move the peer bind to `8230` before the first start, and the RPC section below moves RPC to `127.0.0.1:8229`. That leaves `8227` and `8228` free for Fiber Studio. `--network host` puts those addresses on the VPS, which is where `TWINE_RPC` points. Host networking also makes the firewall rule above apply to `8230`. `8229` is loopback-only, so it is not opened in the firewall.

The command below starts testnet. For mainnet, add `-e FIBER_CONFIG_TEMPLATE=/usr/local/share/fiber/config/mainnet/config.yml` before the first start, then set `ckb.rpc_url` in `fiber-node/config.yml` to a CKB RPC this machine can reach, and set `FIBER_NETWORK=mainnet` in the daemon env below. A store cannot change chains. To switch, stop the container, move `fiber-node/` aside, and start again.

```bash
sudo docker run -d --name fiber-node --restart unless-stopped \
  --network host \
  --env-file /opt/twine/secrets/fiber.env \
  -e RUST_LOG=info \
  -v /opt/twine/fiber-node:/fiber \
  nervos/fiber:0.9.1
```

The first start writes `fiber-node/config.yml`. On testnet that file points at `https://testnet.ckbapp.dev/`. Decrypting the key can take about half a minute. Continue once the container is `Up` and the config file exists. If the container is not `Up`, `sudo docker logs --tail 50 fiber-node` prints the reason.

```bash
sudo docker ps --filter name=fiber-node
ls fiber-node/config.yml
sudo docker exec fiber-node fnn-cli info
```

Copy `pubkey` from `fnn-cli info`. Run it again if the line is missing. The RPC token below binds itself to this pubkey.

Set the address wallets use to open a channel. Local buyer and seller nodes connect **outbound** to this address; the VPS cannot reach `127.0.0.1` on a laptop. Replace `YOUR_PUBLIC_IP` with this machine's public IPv4 (`curl -4 ifconfig.me`).

```bash
PUBLIC_IP=YOUR_PUBLIC_IP
sudo sed -i 's|/ip4/0.0.0.0/tcp/8228|/ip4/0.0.0.0/tcp/8230|' fiber-node/config.yml
sudo sed -i "s|# - \"/ip4/YOUR-FIBER-NODE-PUBLIC-IP/tcp/8228\"|- \"/ip4/${PUBLIC_IP}/tcp/8230\"|" fiber-node/config.yml
sudo grep -n -A4 announced_addrs fiber-node/config.yml
```

Continue when the address line is uncommented and shows your public IP. Leave the container running. The RPC section restarts it after the biscuit public key is in the same file.

## RPC token

Still on the VPS. `biscuit-cli` 0.6.0-beta.2 matches the token format Fiber 0.9.1 checks. `-j1` keeps the compile inside the memory above.

```bash
. "$HOME/.cargo/env"
cargo install biscuit-cli --version 0.6.0-beta.2 --locked -j1
cd /opt/twine
umask 077
biscuit keypair | tee secrets/biscuit-keypair.txt
chmod 600 secrets/biscuit-keypair.txt
```

The `Private key:` line signs tokens. The `Public key:` line starts with `ed25519/`. `node("")` is the local watchtower id. Fiber parses that fact as base58, so a hex pubkey makes `create_preimage` return Unauthorized.

```bash
cat > secrets/permissions.bc << EOF
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
node("");
EOF
PRIV=$(awk -F': ' '/^Private key:/{print $2}' secrets/biscuit-keypair.txt)
biscuit generate --private-key "$PRIV" secrets/permissions.bc \
  | tr -d '\n' > secrets/rpc-token
echo >> secrets/rpc-token
chmod 600 secrets/rpc-token
```

Write the biscuit public key under `rpc.listening_addr` and restart Fiber:

```bash
PUB=$(awk -F': ' '/^Public key:/{print $2}' secrets/biscuit-keypair.txt)
sudo sed -i 's/listening_addr: "127.0.0.1:8227"/listening_addr: "127.0.0.1:8229"/' fiber-node/config.yml
sudo sed -i "/listening_addr: \"127.0.0.1:8229\"/a\\  biscuit_public_key: \"${PUB}\"" fiber-node/config.yml
sudo grep -n biscuit_public_key fiber-node/config.yml
sudo docker restart fiber-node
```

`grep` prints `biscuit_public_key`. After the restart (give the key decrypt half a minute), RPC accepts the token:

```bash
curl -sS http://127.0.0.1:8229 \
  -H "Authorization: Bearer $(tr -d '\n' < /opt/twine/secrets/rpc-token)" \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"node_info","params":[]}'
```

A JSON `node_info` result means the token matches the public key. From here, `fnn-cli info` needs that token:

```bash
sudo docker exec fiber-node fnn-cli --auth-token "$(tr -d '\n' < /opt/twine/secrets/rpc-token)" info
```

## Daemon

Still on the VPS, in `/opt/twine`. This writes a new Nostr secret. To keep an existing daemon identity, put that `nsec` or 64-character hex secret in `TWINE_NOSTR_SECRET` after the file is written. `FIBER_NETWORK` matches the node started above. Add `TWINE_SOLVER=<hex pubkey>` when a solver exists. Until then the daemon refuses disputes.

```bash
cd /opt/twine
umask 077
NOSTR_SECRET=$(openssl rand -hex 32)
RPC_TOKEN=$(tr -d '\n' < secrets/rpc-token)
cat > .env << EOF
FIBER_NETWORK=testnet
RUST_LOG=info
TWINE_NOSTR_SECRET=${NOSTR_SECRET}
TWINE_RELAYS=wss://relay.damus.io,wss://nos.lol
TWINE_RPC=http://127.0.0.1:8229
TWINE_RPC_TOKEN=${RPC_TOKEN}
TWINE_DB=/opt/twine/twine.db
EOF
chmod 600 .env
unset NOSTR_SECRET RPC_TOKEN
```

Build and install the service. `-j1` fits the compile in 4 GB of RAM plus the swap file.

```bash
. "$HOME/.cargo/env"
cd /opt/twine
cargo build --release -j1

sudo tee /etc/systemd/system/twine.service > /dev/null << 'EOF'
[Unit]
Description=Twine daemon
After=docker.service
Wants=docker.service

[Service]
Type=simple
WorkingDirectory=/opt/twine
EnvironmentFile=/opt/twine/.env
ExecStart=/opt/twine/target/release/twine-daemon
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF
sudo systemctl daemon-reload
sudo systemctl enable --now twine
sudo journalctl -u twine -f
```

Ctrl-C stops the log and leaves the service running. Startup logs the daemon `npub` and the Fiber pubkey. People paste that `npub` and the `TWINE_RELAYS` list into the app. The daemon also publishes the Fiber pubkey so a wallet can open a channel.

Fund the Fiber node so sellers can lock (inbound room) and buyers can be paid (outbound liquidity toward buyers). On release the daemon opens a Twine-funded one-way channel to a buyer who cannot receive yet, at least 100 CKB so a default Fiber node auto-accepts. The RPC token must be able to `list_channels`, `list_peers`, `connect_peer`, and `open_channel`. Local wallets must already be connected to this hub. A channel the buyer funds alone cannot receive the payout. See [Fiber channels](README.md#fiber-channels).

The daemon exits when no relay connects, the RPC token is empty, or `FIBER_NETWORK` is not `testnet` or `mainnet`. systemd starts it again after 5 seconds. If it starts while Fiber is still decrypting its key, those attempts fail and the retry picks up once RPC answers.

The first successful RPC call pins that Fiber pubkey in SQLite. Restarting against the same node is fine. A different node, while a hold is still open, is refused.

## Keep these together

Copy this set off the machine before you rely on the host.

| Path | What it is |
| --- | --- |
| `/opt/twine/twine.db` | Orders, trades, catalog, pinned Fiber pubkey. Mode `0600`. |
| `/opt/twine/fiber-node/` | Fiber store, config, and the encrypted CKB key. |
| `/opt/twine/.env` | Daemon environment: Nostr secret and RPC token. |
| `/opt/twine/secrets/fiber.env` | Password Fiber uses to decrypt its CKB key. |
| `/opt/twine/secrets/rpc-token` | Biscuit the daemon sends to Fiber. |
| `/opt/twine/secrets/biscuit-keypair.txt` | Biscuit private key. Needed to mint another token. |
| `$HOME/twine-ckb/ckb-key` | Plaintext CKB key, on the computer that created it. |

A restored database only works with the node it was pinned to.

Restart Fiber first, then `sudo systemctl restart twine`.

To upgrade the daemon, pull the repo, run `cargo build --release -j1` again, and restart the unit. Stay on `nervos/fiber:0.9.1` until a Fiber upgrade is intentional. Store migration for a Fiber version change is in [docker/README.md](docker/README.md).
