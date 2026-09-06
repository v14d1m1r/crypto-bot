#[test]
fn systemd_service_is_hardened_and_restartable() {
    let unit = include_str!("../deploy/systemd/crypto-bot.service");
    for directive in [
        "User=crypto-bot",
        "EnvironmentFile=/etc/crypto-bot/crypto-bot.env",
        "Restart=on-failure",
        "KillSignal=SIGTERM",
        "NoNewPrivileges=true",
        "ProtectSystem=strict",
        "ReadWritePaths=/var/lib/crypto-bot",
    ] {
        assert!(unit.contains(directive), "missing {directive}");
    }
}

#[test]
fn deployment_environment_stays_on_testnet_and_localhost() {
    let environment = include_str!("../deploy/crypto-bot.env.example");
    assert!(environment.lines().any(|line| line == "BOT_MODE=testnet"));
    assert!(
        environment
            .lines()
            .any(|line| line == "BOT_API_ADDRESS=127.0.0.1:3001")
    );
    assert!(environment.contains("BINANCE_TESTNET_API_KEY=replace-on-server"));
    assert!(
        !environment
            .lines()
            .any(|line| line.starts_with("BINANCE_API_KEY="))
    );
}

#[test]
fn backup_script_uses_online_backup_and_path_guards() {
    let script = include_str!("../deploy/backup-sqlite.sh");
    assert!(script.contains("set -euo pipefail"));
    assert!(script.contains(".backup '$temporary'"));
    assert!(script.contains("PRAGMA quick_check"));
    assert!(script.contains("Refusing unexpected database path"));
    assert!(script.contains("Refusing unexpected backup directory"));
}
