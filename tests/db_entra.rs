//! An Entra connection's later logins, against a real Postgres.
//!
//! On Azure Postgres the Entra access token *is* the password, and it
//! expires after about an hour, while the pool goes on opening connections:
//! every script run's (a run's connection is closed when it ends) and sqlx's
//! replacements for connections it retires. Each logs in with the pool's
//! options, so the token in them has to be renewed — silently, through the
//! provider the connect hands the pool (`hubro::db::entra::EntraTokens`).
//!
//! No tenant is needed to test the hop that matters: a role whose password
//! is changed stands in for a token that stopped working, and the provider is
//! driven by an injected clock and renewal. Needs `HUBRO_PG_TEST_URL` — see
//! tests/db_postgres.rs.

mod common;

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use hubro::db::entra::{Clock, EntraTokens, Renew, RenewError, Renewed, Token};
use hubro::db::{run_script, split_statements, DbPool};

async fn run(pool: &DbPool, sql: &str) -> Result<(), String> {
    let statements = split_statements(sql, pool.dialect());
    run_script(pool, pool.backend_capabilities(), &statements, |_| {})
        .await
        .map(|_| ())
        .map_err(|e| e.error.to_string())
}

#[tokio::test]
async fn postgres_entra_logins_after_the_first_token_expires_use_a_renewed_one() {
    let Some(admin_url) = common::pg_test_url().await else {
        return;
    };
    let admin = DbPool::open_postgres(&admin_url).await.unwrap();
    let role = format!("hubro_entra_{}", std::process::id());
    let _ = admin.execute(&format!("DROP ROLE IF EXISTS {role}")).await;
    admin
        .execute(&format!("CREATE ROLE {role} LOGIN PASSWORD 'first'"))
        .await
        .unwrap();
    let mut url = url::Url::parse(&admin_url).unwrap();
    url.set_username(&role).unwrap();
    url.set_password(Some("first")).unwrap();

    let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let clock = Arc::new(AtomicU64::new(0));
    let now: Clock = {
        let clock = Arc::clone(&clock);
        Arc::new(move || base + Duration::from_secs(clock.load(Ordering::SeqCst)))
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let renew: Renew = {
        let calls = Arc::clone(&calls);
        Arc::new(move |refresh: Option<String>| {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                assert_eq!(refresh.as_deref(), Some("kept"));
                if n == 0 {
                    Ok(Renewed {
                        token: Token {
                            secret: "second".into(),
                            expires_at: base + Duration::from_secs(7200),
                        },
                        refresh: None,
                    })
                } else {
                    Err(RenewError::RefreshRejected("invalid_grant".into()))
                }
            })
        })
    };
    let tokens = Arc::new(EntraTokens::new(
        Token {
            secret: "first".into(),
            expires_at: base + Duration::from_secs(3600),
        },
        Some("kept".into()),
        false,
        renew,
        now,
    ));
    let pool = DbPool::open_postgres_entra(url.as_str(), tokens)
        .await
        .unwrap();

    // The connect's own connection, logged in with the first token, is used
    // and closed by this run; every later run logs in anew.
    run(&pool, "SELECT 1").await.unwrap();
    // The first token stops working, as an expired one does, and the
    // connections already logged in with it are ended, so the next run's
    // login is a new one whatever the pool had idle.
    admin
        .execute(&format!("ALTER ROLE {role} PASSWORD 'second'"))
        .await
        .unwrap();
    admin
        .query(&format!(
            "SELECT count(pg_terminate_backend(pid)) FROM pg_stat_activity \
             WHERE usename = '{role}'"
        ))
        .await
        .unwrap();
    let stale = run(&pool, "SELECT 1").await;
    assert!(
        stale.is_err(),
        "a login with the old token succeeded, so the server does not check \
         passwords and nothing below is tested"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0, "the token was not due yet");

    // Within the renewal margin: the run renews before it logs in, and the
    // pool's later logins — the grid's too — use the renewed token.
    clock.store(3500, Ordering::SeqCst);
    run(&pool, "SELECT 1")
        .await
        .unwrap_or_else(|e| panic!("a login after renewal failed: {e}"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    pool.query("SELECT 1").await.unwrap();

    // The renewed token expires, and its refresh token is refused: the run
    // fails saying to reconnect, rather than with the server's password error.
    clock.store(7200, Ordering::SeqCst);
    let refused = run(&pool, "SELECT 1").await.unwrap_err();
    assert!(refused.contains("reconnect to sign in again"), "{refused}");
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    pool.close().await;
    // The role's sessions end with the pool; wait for the server to see that.
    for _ in 0..50 {
        if admin.execute(&format!("DROP ROLE {role}")).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    admin.close().await;
}
