#!/usr/bin/env bash
set -euo pipefail

database_path="${CRYPTO_BOT_DB_PATH:-/var/lib/crypto-bot/crypto_bot_testnet.db}"
backup_directory="${CRYPTO_BOT_BACKUP_DIR:-/var/backups/crypto-bot}"

case "$database_path" in
  /var/lib/crypto-bot/*.db) ;;
  *) echo "Refusing unexpected database path: $database_path" >&2; exit 1 ;;
esac
case "$backup_directory" in
  /var/backups/crypto-bot|/var/backups/crypto-bot/*) ;;
  *) echo "Refusing unexpected backup directory: $backup_directory" >&2; exit 1 ;;
esac

test -f "$database_path"
command -v sqlite3 >/dev/null
umask 077
mkdir -p "$backup_directory"

timestamp="$(date -u +%Y%m%dT%H%M%SZ)"
temporary="$backup_directory/.crypto_bot_testnet-$timestamp.db.tmp"
destination="$backup_directory/crypto_bot_testnet-$timestamp.db"

sqlite3 "$database_path" ".timeout 5000" ".backup '$temporary'"
sqlite3 "$temporary" "PRAGMA quick_check" | grep -qx "ok"
mv -- "$temporary" "$destination"

find "$backup_directory" -maxdepth 1 -type f -name 'crypto_bot_testnet-*.db' -mtime +14 -delete
echo "Created $destination"
