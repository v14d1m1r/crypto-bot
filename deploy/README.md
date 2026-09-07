# Linode Testnet deployment

This runbook deploys the existing **Binance Spot Testnet** mode. It does not enable production trading or real-money credentials.

The first deployment keeps the Rust API private on `127.0.0.1:3001`. Run the dashboard on your local computer and reach the API through an SSH tunnel instead of exposing either service publicly.

## 1. Prepare Ubuntu

Use a supported Ubuntu LTS image, apply updates, confirm time synchronization, and install build/runtime dependencies:

```bash
sudo apt update
sudo apt upgrade
sudo apt install build-essential pkg-config libssl-dev sqlite3 curl git
timedatectl status
```

Create an unprivileged service account and private directories:

```bash
sudo useradd --system --home-dir /var/lib/crypto-bot --create-home --shell /usr/sbin/nologin crypto-bot
sudo install -d -o root -g root -m 0755 /opt/crypto-bot/bin
sudo install -d -o crypto-bot -g crypto-bot -m 0700 /var/lib/crypto-bot
sudo install -d -o root -g crypto-bot -m 0750 /etc/crypto-bot
sudo install -d -o crypto-bot -g crypto-bot -m 0700 /var/backups/crypto-bot
```

Configure a Linode Cloud Firewall with default-deny inbound traffic. Allow SSH only from your administrative IP where practical. The bot needs outbound TCP 443 for Binance and Telegram.

## 2. Build and install

Install Rust with `rustup` for your deployment account, copy or clone this repository, and build the release binary from the repository root:

```bash
cargo test
cargo test fault_harness -- --nocapture
cargo build --release --locked
```

Install immutable program files:

```bash
sudo install -o root -g root -m 0755 target/release/crypto-bot /opt/crypto-bot/bin/crypto-bot
sudo install -o root -g root -m 0755 deploy/backup-sqlite.sh /opt/crypto-bot/bin/backup-sqlite.sh
```

## 3. Configure secrets

Install the template, edit it on the server, and never copy the populated file back into the repository:

```bash
sudo install -o root -g crypto-bot -m 0640 deploy/crypto-bot.env.example /etc/crypto-bot/crypto-bot.env
sudoedit /etc/crypto-bot/crypto-bot.env
```

Replace every `replace-on-server` value. Use Testnet credentials only. Keep `BOT_API_ADDRESS=127.0.0.1:3001`.

Validate the environment and non-executing Testnet order check before installing the service:

```bash
sudo -u crypto-bot bash -c 'set -a; source /etc/crypto-bot/crypto-bot.env; set +a; /opt/crypto-bot/bin/crypto-bot testnet-check'
sudo -u crypto-bot bash -c 'set -a; source /etc/crypto-bot/crypto-bot.env; set +a; /opt/crypto-bot/bin/crypto-bot telegram-check'
```

## 4. Install systemd services

```bash
sudo install -o root -g root -m 0644 deploy/systemd/crypto-bot.service /etc/systemd/system/crypto-bot.service
sudo install -o root -g root -m 0644 deploy/systemd/crypto-bot-backup.service /etc/systemd/system/crypto-bot-backup.service
sudo install -o root -g root -m 0644 deploy/systemd/crypto-bot-backup.timer /etc/systemd/system/crypto-bot-backup.timer
sudo systemd-analyze verify /etc/systemd/system/crypto-bot.service /etc/systemd/system/crypto-bot-backup.service
sudo systemctl daemon-reload
sudo systemctl enable --now crypto-bot.service
sudo systemctl enable --now crypto-bot-backup.timer
```

Inspect startup and follow logs:

```bash
sudo systemctl status crypto-bot.service
sudo journalctl -u crypto-bot.service -f
```

The bot handles both `SIGINT` and systemd's `SIGTERM`, closes WebSockets, flushes queued Telegram alerts, and then exits.

## 5. Check health privately

The health endpoint returns HTTP `503` until a recent closed candle has been persisted, and whenever candle data becomes stale:

```bash
curl --fail-with-body http://127.0.0.1:3001/api/health
```

A risk circuit breaker does not make the process unhealthy; it sets `ready_for_entries` to `false` in the JSON response.

From your local computer, open an SSH tunnel:

```bash
ssh -N -L 3001:127.0.0.1:3001 your-user@your-linode-ip
```

Keep that session open, run the dashboard locally, and open `http://localhost:3000`:

```powershell
cd dashboard
npm.cmd run dev
```

Do not open ports 3000 or 3001 in the Linode firewall for this setup.

## 6. Verify backups and recovery

Trigger and inspect the first online SQLite backup:

```bash
sudo systemctl start crypto-bot-backup.service
sudo systemctl status crypto-bot-backup.service
sudo -u crypto-bot ls -la /var/backups/crypto-bot
```

The daily timer retains 14 days of database backups and validates each backup with `PRAGMA quick_check`. Also enable Linode instance backups as a second layer.

Practice a restore while the bot is stopped:

```bash
sudo systemctl stop crypto-bot.service
sudo cp /var/backups/crypto-bot/crypto_bot_testnet-YYYYMMDDTHHMMSSZ.db /var/lib/crypto-bot/crypto_bot_testnet.db
sudo chown crypto-bot:crypto-bot /var/lib/crypto-bot/crypto_bot_testnet.db
sudo chmod 0600 /var/lib/crypto-bot/crypto_bot_testnet.db
sudo systemctl start crypto-bot.service
```

Use the exact backup filename; do not paste the placeholder literally.

## Updating

### Enable a 100-USDT budget on an existing deployment

Commit and push the budget changes from your local checkout, then run on Linode:

```bash
cd /home/src/crypto-bot
git pull --ff-only
cargo test --locked && cargo build --release --locked
```

After the build succeeds, make an online database backup and stop the service:

```bash
sudo systemctl start crypto-bot-backup.service
sudo systemctl stop crypto-bot.service
sudoedit /etc/crypto-bot/crypto-bot.env
```

Add or update these settings (do not replace the file containing your credentials):

```dotenv
BOT_TESTNET_BUDGET=100
BOT_POSITION_FRACTION=0.25
BOT_MAX_ORDER_QUOTE=25
```

Install and start the updated binary:

```bash
sudo install -o root -g root -m 0755 target/release/crypto-bot /opt/crypto-bot/bin/crypto-bot
sudo systemctl restart crypto-bot.service
sudo journalctl -u crypto-bot.service --since "1 minute ago" --no-pager -l
curl --fail-with-body http://127.0.0.1:3001/api/status
```

The response includes `budget.initial_equity: 100`. The current equity starts near 100 USDT and then changes with fills and prices. The initial tracked position counts toward that allocation; unrelated exchange funds do not. Existing positions larger than the requested budget prevent activation. Restart the local dashboard with `npm.cmd run dev` from `dashboard/` if it is not already picking up the updated files, and keep the SSH tunnel open.

No database deletion is needed. Keep `BOT_TESTNET_BUDGET=100` on later restarts: the allocation is persistent, so changing/removing the setting will fail startup rather than refill losses. Existing circuit-breaker halts are preserved. Backups include the budget tables. The dashboard excludes pre-budget equity snapshots from its chart and keeps historical trades visible. See the [budget accounting notes](../README.md#testnet-portfolio-budget) for fee handling.

### Regular code updates

Build and test the new revision before replacing the binary:

```bash
cargo test
cargo build --release --locked
sudo systemctl stop crypto-bot.service
sudo install -o root -g root -m 0755 target/release/crypto-bot /opt/crypto-bot/bin/crypto-bot
sudo systemctl start crypto-bot.service
sudo journalctl -u crypto-bot.service -n 100 --no-pager
```

Keep this deployment on Testnet during the soak period. A future production mode needs a separate review, database, credentials, limits, and emergency controls.
