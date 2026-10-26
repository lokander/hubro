//! An Entra connection's later logins, against a real Postgres.
//!
//! On Azure Postgres the Entra access token *is* the password, and it
//! expires after about an hour, while the pool goes on opening connections:
//! every script run's (a run's connection is closed when it ends) and sqlx's
//! replacements for connections it retires. Each logs in with the pool's
//! options, so the token in them has to be renewed — silently, through the
//! provider the connect hands the pool (`hubro::db::entra::EntraTokens`) —
//! by a script run before it takes its connection, and by a task that does it
//! on a timer for everything else.
//!
//! No tenant is needed to test the hops that matter: a role whose password
//! is changed stands in for a token that stopped working, and the provider is
//! driven by an injected clock and renewal. Needs `HUBRO_PG_TEST_URL` — see
//! tests/db_postgres.rs.

mod common;

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use hubro::db::entra::{Clock, EntraTokens, Renew, RenewError, Renewed, Secret, Token};
use hubro::db::{run_script, split_statements, DbPool};

async fn run(pool: &DbPool, sql: &str) -> Result<(), String> {
    let statements = split_statements(sql, pool.dialect());
    run_script(pool, pool.backend_capabilities(), &statements, |_| {})
        .await
        .map(|_| ())
        .map_err(|e| e.error.to_string())
}

/// A role that logs in with password `first`, and a provider for it whose
/// first renewal hands out `second` (the role's next password) and whose
/// later ones are refused. The clock is at `base + clock` seconds; `calls`
/// counts renewals and `reads` counts clock reads — every ask of the
/// provider reads it, so `reads` shows whether anything is still asking.
struct Rig {
    admin: DbPool,
    role: String,
    url: String,
    clock: Arc<AtomicU64>,
    calls: Arc<AtomicUsize>,
    reads: Arc<AtomicUsize>,
    tokens: Arc<EntraTokens>,
}

const BASE: Duration = Duration::from_secs(1_000_000);

async fn rig(admin_url: &str, suffix: &str) -> Rig {
    let admin = DbPool::open_postgres(admin_url).await.unwrap();
    let role = format!("hubro_entra_{}_{suffix}", std::process::id());
    let _ = admin.execute(&format!("DROP ROLE IF EXISTS {role}")).await;
    admin
        .execute(&format!("CREATE ROLE {role} LOGIN PASSWORD 'first'"))
        .await
        .unwrap();
    let mut url = url::Url::parse(admin_url).unwrap();
    url.set_username(&role).unwrap();
    url.set_password(Some("first")).unwrap();

    let base = SystemTime::UNIX_EPOCH + BASE;
    let clock = Arc::new(AtomicU64::new(0));
    let reads = Arc::new(AtomicUsize::new(0));
    let now: Clock = {
        let (clock, reads) = (Arc::clone(&clock), Arc::clone(&reads));
        Arc::new(move || {
            reads.fetch_add(1, Ordering::SeqCst);
            base + Duration::from_secs(clock.load(Ordering::SeqCst))
        })
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let renew: Renew = {
        let calls = Arc::clone(&calls);
        Arc::new(move |refresh: Option<Secret>| {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                assert_eq!(refresh.as_ref().map(Secret::expose), Some("kept"));
                if n == 0 {
                    Ok(Renewed {
                        token: Token {
                            secret: Secret::new("second"),
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
            secret: Secret::new("first"),
            expires_at: base + Duration::from_secs(3600),
        },
        Some(Secret::new("kept")),
        false,
        renew,
        now,
    ));
    Rig {
        admin,
        role,
        url: url.to_string(),
        clock,
        calls,
        reads,
        tokens,
    }
}

impl Rig {
    /// The first token stops working, as an expired one does, and the
    /// connections already logged in with it are ended, so the pool's next
    /// login is a new one whatever it had idle.
    async fn expire_first_token(&self) {
        let role = &self.role;
        self.admin
            .execute(&format!("ALTER ROLE {role} PASSWORD 'second'"))
            .await
            .unwrap();
        self.admin
            .query_unbounded(&format!(
                "SELECT count(pg_terminate_backend(pid)) FROM pg_stat_activity \
                 WHERE usename = '{role}'"
            ))
            .await
            .unwrap();
    }

    async fn drop_role(self) {
        // The role's sessions end with its pools; wait for the server to see that.
        for _ in 0..50 {
            let dropped = self
                .admin
                .execute(&format!("DROP ROLE {}", self.role))
                .await;
            if dropped.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        self.admin.close().await;
    }
}

#[tokio::test]
async fn postgres_entra_logins_after_the_first_token_expires_use_a_renewed_one() {
    let Some(admin_url) = common::pg_test_url().await else {
        return;
    };
    let rig = rig(&admin_url, "run").await;
    let pool = DbPool::open_postgres_entra(&rig.url, Arc::clone(&rig.tokens))
        .await
        .unwrap();

    // The connect's own connection, logged in with the first token, is used
    // and closed by this run; every later run logs in anew.
    run(&pool, "SELECT 1").await.unwrap();
    rig.expire_first_token().await;
    let stale = run(&pool, "SELECT 1").await;
    assert!(
        stale.is_err(),
        "a login with the old token succeeded, so the server does not check \
         passwords and nothing below is tested"
    );
    assert_eq!(
        rig.calls.load(Ordering::SeqCst),
        0,
        "the token was not due yet"
    );

    // Within the renewal margin: the run renews before it logs in, and the
    // pool's later logins — the grid's too — use the renewed token.
    rig.clock.store(3500, Ordering::SeqCst);
    run(&pool, "SELECT 1")
        .await
        .unwrap_or_else(|e| panic!("a login after renewal failed: {e}"));
    assert_eq!(rig.calls.load(Ordering::SeqCst), 1);
    pool.query_unbounded("SELECT 1").await.unwrap();

    // The renewed token expires, and its refresh token is refused: the run
    // fails saying to reconnect, rather than with the server's password error.
    rig.clock.store(7200, Ordering::SeqCst);
    let refused = run(&pool, "SELECT 1").await.unwrap_err();
    assert!(refused.contains("reconnect to sign in again"), "{refused}");
    assert_eq!(rig.calls.load(Ordering::SeqCst), 2);

    pool.close().await;
    rig.drop_role().await;
}

/// How long a wait for the renewal task may take before it fails: generous,
/// so a loaded runner that starves the task for a while is not a failure.
const DEADLINE: Duration = Duration::from_secs(10);

/// Polls `ready` every few milliseconds until it holds or [`DEADLINE`]
/// passes; whether it held.
async fn eventually(mut ready: impl FnMut() -> bool) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < DEADLINE {
        if ready() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    ready()
}

/// Whether the task is asking: the provider's clock is read again.
async fn asking(reads: &AtomicUsize) -> bool {
    let from = reads.load(Ordering::SeqCst);
    eventually(|| reads.load(Ordering::SeqCst) > from).await
}

/// Whether the task has stopped asking: a quiet spell of `quiet` with no
/// read of the clock, found within [`DEADLINE`]. One tick already under way
/// when its pool closed is allowed to finish first.
async fn stopped(reads: &AtomicUsize, quiet: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < DEADLINE {
        let from = reads.load(Ordering::SeqCst);
        tokio::time::sleep(quiet).await;
        if reads.load(Ordering::SeqCst) == from {
            return true;
        }
    }
    false
}

/// The grid's logins after an expiry, with **no script run** to renew first:
/// only the pool's renewal task can have put the new token in its options.
/// Then the task's end: it stops asking once the pool is closed, and once
/// the last handle on a pool is dropped without a close — each shown alive
/// first, so "it stopped" is not read off a task that never ran.
///
/// Waits poll for the expected state with a generous deadline rather than
/// sleeping fixed amounts, so a loaded runner slows the test without failing
/// it. A quiet spell is ten ticks long: a live task asks every tick.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_entra_renewal_task_keeps_the_grid_logging_in_and_ends_with_its_pool() {
    let Some(admin_url) = common::pg_test_url().await else {
        return;
    };
    let tick = Duration::from_millis(50);
    let quiet = tick * 10;
    let rig = rig(&admin_url, "task").await;
    let pool = DbPool::open_postgres_entra_renewing_every(&rig.url, Arc::clone(&rig.tokens), tick)
        .await
        .unwrap();
    rig.expire_first_token().await;
    assert!(
        pool.query_unbounded("SELECT 1").await.is_err(),
        "a login with the old token succeeded, so nothing below is tested"
    );

    rig.clock.store(3500, Ordering::SeqCst);
    assert!(
        eventually(|| rig.calls.load(Ordering::SeqCst) > 0).await,
        "the task did not renew the token"
    );
    assert_eq!(rig.calls.load(Ordering::SeqCst), 1);
    pool.query_unbounded("SELECT 1")
        .await
        .unwrap_or_else(|e| panic!("the grid's login after the task renewed failed: {e}"));

    // Closed: alive until then, quiet after.
    assert!(
        asking(&rig.reads).await,
        "the task stopped while its pool is open"
    );
    pool.close().await;
    assert!(
        stopped(&rig.reads, quiet).await,
        "the task kept asking after its pool closed"
    );

    // Dropped without a close: alive while a handle remains, quiet once the
    // last one goes. Opened with the password the role has now.
    let mut renewed_url = url::Url::parse(&rig.url).unwrap();
    renewed_url.set_password(Some("second")).unwrap();
    let dropped = DbPool::open_postgres_entra_renewing_every(
        renewed_url.as_str(),
        Arc::clone(&rig.tokens),
        tick,
    )
    .await
    .unwrap();
    let copy = dropped.clone();
    drop(dropped);
    assert!(
        asking(&rig.reads).await,
        "the task ended while a handle on its pool remained"
    );
    drop(copy);
    assert!(
        stopped(&rig.reads, quiet).await,
        "the task outlived the last handle on its pool"
    );

    rig.drop_role().await;
}
