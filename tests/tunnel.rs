//! SSH tunnel integration tests. They need Docker containers (Docker only,
//! per CLAUDE.md) and are skipped unless `HUBRO_SSH_TEST` is set.
//!
//! Exact setup (throwaway keys live in a scratch dir, nothing is checked in):
//!
//! ```sh
//! # Postgres server (shared with tests/db_postgres.rs):
//! docker run -d --name hubro-pg-test -e POSTGRES_PASSWORD=testpass \
//!   -e POSTGRES_USER=tester -e POSTGRES_DB=demo -p 5433:5432 postgres:17-alpine
//!
//! # Throwaway keys (the encrypted key's passphrase must be "letmein"):
//! ssh-keygen -t ed25519 -N ""        -f "$SCRATCH/ssh-test-key"
//! ssh-keygen -t ed25519 -N "letmein" -f "$SCRATCH/ssh-test-key-enc"
//!
//! # SSH server that can reach the Postgres container by name:
//! docker network create hubro-test-net
//! docker network connect hubro-test-net hubro-pg-test
//! docker run -d --name hubro-ssh-test --network hubro-test-net \
//!   -p 2222:22 alpine:3 sh -c "
//!     apk add --no-cache openssh >/dev/null &&
//!     ssh-keygen -A &&
//!     sed -i 's/^AllowTcpForwarding no/AllowTcpForwarding yes/' /etc/ssh/sshd_config &&
//!     adduser -D tunnel &&
//!     passwd -u tunnel &&
//!     mkdir -p /home/tunnel/.ssh &&
//!     echo '$(cat $SCRATCH/ssh-test-key.pub)' > /home/tunnel/.ssh/authorized_keys &&
//!     chown -R tunnel:tunnel /home/tunnel/.ssh &&
//!     chmod 700 /home/tunnel/.ssh && chmod 600 /home/tunnel/.ssh/authorized_keys &&
//!     exec /usr/sbin/sshd -D -e"
//!
//! # Optional SQL Server target for the mssql-through-tunnel test (FRE-58);
//! # skipped unless HUBRO_SSH_TEST_MSSQL_PASSWORD is also set:
//! docker run -d --name hubro-mssql-test --network hubro-test-net \
//!   -e ACCEPT_EULA=Y -e "MSSQL_SA_PASSWORD=Str0ng!Passw0rd" \
//!   mcr.microsoft.com/mssql/server:2022-latest
//!
//! # Run (key paths are required; host/port/user/db-target have defaults
//! # matching the commands above):
//! HUBRO_SSH_TEST=1 \
//! HUBRO_SSH_TEST_KEY="$SCRATCH/ssh-test-key" \
//! HUBRO_SSH_TEST_ENC_KEY="$SCRATCH/ssh-test-key-enc" \
//! HUBRO_SSH_TEST_MSSQL_PASSWORD='Str0ng!Passw0rd' \
//! cargo test --test tunnel
//! ```

use std::path::PathBuf;

use hubro::db::{
    mssql_url_target, mssql_url_via_local_port, mssql_url_with_password, DbPool, MssqlAuth, Value,
};
use hubro::tunnel::{Tunnel, TunnelAuth, TunnelConfig, TunnelError};

/// Everything the gated tests need, from the environment (with defaults
/// matching the docker commands in the file header).
struct SshTestEnv {
    host: String,
    port: u16,
    user: String,
    key: PathBuf,
    encrypted_key: PathBuf,
    /// The database as seen from inside the SSH container.
    db_host: String,
    db_port: u16,
}

fn ssh_env() -> Option<SshTestEnv> {
    if std::env::var("HUBRO_SSH_TEST").is_err() {
        eprintln!("skipping ssh tunnel test: HUBRO_SSH_TEST not set");
        return None;
    }
    let var =
        |name: &str, default: &str| std::env::var(name).unwrap_or_else(|_| default.to_string());
    Some(SshTestEnv {
        host: var("HUBRO_SSH_TEST_HOST", "127.0.0.1"),
        port: var("HUBRO_SSH_TEST_PORT", "2222").parse().unwrap(),
        user: var("HUBRO_SSH_TEST_USER", "tunnel"),
        key: PathBuf::from(
            std::env::var("HUBRO_SSH_TEST_KEY").expect("HUBRO_SSH_TEST_KEY must be set"),
        ),
        encrypted_key: PathBuf::from(
            std::env::var("HUBRO_SSH_TEST_ENC_KEY").expect("HUBRO_SSH_TEST_ENC_KEY must be set"),
        ),
        db_host: var("HUBRO_SSH_TEST_DB_HOST", "hubro-pg-test"),
        db_port: var("HUBRO_SSH_TEST_DB_PORT", "5432").parse().unwrap(),
    })
}

impl SshTestEnv {
    fn config(&self) -> TunnelConfig {
        TunnelConfig {
            host: self.host.clone(),
            port: self.port,
            user: self.user.clone(),
            auth: TunnelAuth::KeyFile {
                path: self.key.clone(),
            },
        }
    }
}

/// Discovers the server's host key via a throwaway first-contact connect and
/// trusts it into a fresh temp known_hosts, returning the read set to pass to
/// later `Tunnel::open` calls (plus the temp dir, kept alive by the caller).
/// This keeps the host-key-verifying tests off the machine's real known_hosts.
async fn trusted_known_hosts(env: &SshTestEnv) -> (Vec<PathBuf>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let kh = dir.path().join("known_hosts");
    let err = Tunnel::open(
        env.config(),
        None,
        env.db_host.clone(),
        env.db_port,
        std::slice::from_ref(&kh),
    )
    .await
    .expect_err("first contact with an empty known_hosts must be refused");
    let TunnelError::HostKeyUnknown(info) = err else {
        panic!("expected HostKeyUnknown on first contact, got {err:?}");
    };
    hubro::tunnel::trust_host_key(&info.host, info.port, &info.key_openssh, &kh)
        .expect("trusting the discovered key should persist it");
    (vec![kh], dir)
}

#[tokio::test]
async fn postgres_connects_end_to_end_through_the_tunnel() {
    let Some(env) = ssh_env() else { return };
    let (kh, _kh_dir) = trusted_known_hosts(&env).await;
    let tunnel = Tunnel::open(env.config(), None, env.db_host.clone(), env.db_port, &kh)
        .await
        .expect("tunnel should open");
    // The saved URL points at the logical host; the connect goes through the
    // forwarded local port, exactly like AppState::connect_postgres does it.
    let url = format!(
        "postgres://tester:testpass@127.0.0.1:{}/demo",
        tunnel.local_port()
    );
    let pool = DbPool::open_postgres(&url)
        .await
        .expect("postgres should connect through the tunnel");
    let result = pool.query("SELECT 1").await.expect("query through tunnel");
    assert_eq!(result.rows, vec![vec![Value::Integer(1)]]);
    pool.close().await;
}

/// The SQL Server mirror of the end-to-end test above (FRE-58), exercising the
/// exact glue `AppState::connect_sqlserver` uses: the logical URL is rewritten
/// through the forwarded local port, and the driver gets the original hostname
/// as its TLS server name while dialing 127.0.0.1. Needs the
/// `hubro-mssql-test` container (see the file header); skipped unless
/// `HUBRO_SSH_TEST_MSSQL_PASSWORD` is set.
#[tokio::test]
async fn sqlserver_connects_end_to_end_through_the_tunnel() {
    let Some(env) = ssh_env() else { return };
    let Ok(password) = std::env::var("HUBRO_SSH_TEST_MSSQL_PASSWORD") else {
        eprintln!("skipping mssql tunnel test: HUBRO_SSH_TEST_MSSQL_PASSWORD not set");
        return;
    };
    let mssql_host = std::env::var("HUBRO_SSH_TEST_MSSQL_HOST")
        .unwrap_or_else(|_| "hubro-mssql-test".to_string());
    let (kh, _kh_dir) = trusted_known_hosts(&env).await;
    let tunnel = Tunnel::open(env.config(), None, mssql_host.clone(), 1433, &kh)
        .await
        .expect("tunnel should open");
    // The saved URL points at the logical host. The stock container's cert is
    // self-signed, so TLS stays on with trustServerCertificate — what the
    // form's dev checkbox produces.
    let url = format!("mssql://sa@{mssql_host}:1433/master?encrypt=on&trustServerCertificate=true");
    let connect_url = mssql_url_via_local_port(&url, tunnel.local_port()).unwrap();
    let full = mssql_url_with_password(&connect_url, &password).unwrap();
    let (tls_host, _) = mssql_url_target(&url).unwrap();
    let pool = DbPool::open_mssql_with(&full, &MssqlAuth::Password, Some(&tls_host))
        .await
        .expect("sql server should connect through the tunnel");
    let result = pool.query("SELECT 1").await.expect("query through tunnel");
    assert_eq!(result.rows, vec![vec![Value::Integer(1)]]);
    pool.close().await;
}

#[tokio::test]
async fn unreachable_ssh_server_is_a_tunnel_connect_error_not_a_db_error() {
    let Some(env) = ssh_env() else { return };
    // Port 9 (discard) on localhost: nothing listens there.
    let config = TunnelConfig {
        port: 9,
        ..env.config()
    };
    let err = Tunnel::open(config, None, env.db_host.clone(), env.db_port, &[])
        .await
        .expect_err("connecting to a closed port must fail");
    assert!(
        matches!(err, TunnelError::Connect(_)),
        "expected TunnelError::Connect, got {err:?}"
    );
    // The rendered message is recognizably a tunnel failure, never a
    // database one.
    assert!(err.to_string().starts_with("SSH tunnel: connection failed"));
}

#[tokio::test]
async fn encrypted_key_without_passphrase_asks_for_one() {
    let Some(env) = ssh_env() else { return };
    let config = TunnelConfig {
        auth: TunnelAuth::KeyFile {
            path: env.encrypted_key.clone(),
        },
        ..env.config()
    };

    // No passphrase: the distinguishable needs-passphrase error (callers
    // use it to raise the prompt instead of showing an error).
    let err = Tunnel::open(config.clone(), None, env.db_host.clone(), env.db_port, &[])
        .await
        .expect_err("an encrypted key must not load without a passphrase");
    assert!(
        matches!(err, TunnelError::NeedsPassphrase(_)),
        "expected TunnelError::NeedsPassphrase, got {err:?}"
    );

    // Wrong passphrase: same category, so the prompt comes back.
    let err = Tunnel::open(
        config,
        Some("wrong".to_string()),
        env.db_host.clone(),
        env.db_port,
        &[],
    )
    .await
    .expect_err("a wrong passphrase must not decrypt the key");
    assert!(
        matches!(err, TunnelError::NeedsPassphrase(_)),
        "expected TunnelError::NeedsPassphrase, got {err:?}"
    );
}

#[tokio::test]
async fn dropping_the_tunnel_closes_the_local_listener() {
    let Some(env) = ssh_env() else { return };
    let (kh, _kh_dir) = trusted_known_hosts(&env).await;
    let tunnel = Tunnel::open(env.config(), None, env.db_host.clone(), env.db_port, &kh)
        .await
        .expect("tunnel should open");
    let port = tunnel.local_port();

    // Live tunnel: the local port accepts connections.
    tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("the forwarded port should accept while the tunnel lives");

    drop(tunnel);

    // Shutdown is signalled, not synchronous — poll until the listener is
    // gone (bounded so a regression fails rather than hangs).
    for _ in 0..50 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err()
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("the local listener still accepts connections after the tunnel was dropped");
}

// The desktop app drives the tunnel from a multi-thread tokio runtime
// (dioxus-desktop), unlike the current-thread runtime of plain
// `#[tokio::test]` — cover that environment too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tunnel_works_on_a_multi_thread_runtime() {
    let Some(env) = ssh_env() else { return };
    let (kh, _kh_dir) = trusted_known_hosts(&env).await;
    let tunnel = Tunnel::open(env.config(), None, env.db_host.clone(), env.db_port, &kh)
        .await
        .expect("tunnel should open on a multi-thread runtime");
    let url = format!(
        "postgres://tester:testpass@127.0.0.1:{}/demo",
        tunnel.local_port()
    );
    let pool = DbPool::open_postgres(&url)
        .await
        .expect("pg through tunnel");
    pool.query("SELECT 1").await.expect("query");
    pool.close().await;
}

#[tokio::test]
async fn unknown_host_key_is_refused_then_trusted_and_connects() {
    let Some(env) = ssh_env() else { return };
    let dir = tempfile::tempdir().unwrap();
    let kh = dir.path().join("known_hosts");

    // First contact against an empty known_hosts: refused, with the offered
    // key's fingerprint surfaced so the UI can prompt.
    let err = Tunnel::open(
        env.config(),
        None,
        env.db_host.clone(),
        env.db_port,
        std::slice::from_ref(&kh),
    )
    .await
    .expect_err("an unrecognized host key must be refused");
    let TunnelError::HostKeyUnknown(info) = err else {
        panic!("expected HostKeyUnknown, got {err:?}");
    };
    assert_eq!(info.host, env.host);
    assert_eq!(info.port, env.port);
    assert!(
        info.fingerprint.starts_with("SHA256:"),
        "fingerprint should be a SHA-256 form: {}",
        info.fingerprint
    );

    // Trusting persists the key; the same connect then succeeds.
    hubro::tunnel::trust_host_key(&info.host, info.port, &info.key_openssh, &kh)
        .expect("trusting the key should persist it");
    let tunnel = Tunnel::open(
        env.config(),
        None,
        env.db_host.clone(),
        env.db_port,
        std::slice::from_ref(&kh),
    )
    .await
    .expect("a trusted host key should connect");
    drop(tunnel);
}

#[tokio::test]
async fn a_changed_host_key_is_refused_as_a_possible_mitm() {
    let Some(env) = ssh_env() else { return };
    // Discover the server's real key so we can confirm it offered ed25519,
    // then pin a *different* ed25519 key — the condition that makes
    // known_hosts report a change rather than an unknown host.
    let (kh, _kh_dir) = trusted_known_hosts(&env).await;

    // A second, different ed25519 key. If the server offered ed25519, pinning
    // this makes verification see a changed key.
    const OTHER_ED25519: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ";
    let real_line = std::fs::read_to_string(&kh[0]).unwrap();
    if !real_line.contains("ssh-ed25519") {
        eprintln!("server did not offer an ed25519 key; skipping changed-key assertion");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let pinned = dir.path().join("known_hosts");
    std::fs::write(
        &pinned,
        format!("[{}]:{} {OTHER_ED25519}\n", env.host, env.port),
    )
    .unwrap();

    let err = Tunnel::open(
        env.config(),
        None,
        env.db_host.clone(),
        env.db_port,
        std::slice::from_ref(&pinned),
    )
    .await
    .expect_err("a changed host key must be refused");
    assert!(
        matches!(err, TunnelError::HostKeyChanged(_)),
        "expected HostKeyChanged, got {err:?}"
    );
    assert!(err.to_string().contains("HOST KEY CHANGED"));
}
