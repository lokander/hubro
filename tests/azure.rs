//! Entra token-acquisition flows driven end to end against a local
//! fake HTTP server and a fake "browser" — no real Azure. Covers the
//! interactive authorization-code exchange, silent refresh-token redemption,
//! managed-identity (IMDS), and OAuth error surfacing.

use std::collections::HashMap;
use std::time::Duration;

use hubro::azure::{
    acquire_token, AzureError, Endpoints, EntraAuth, OSSRDBMS_RESOURCE, SQLDB_RESOURCE,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

/// Spawns a throwaway HTTP server that answers every request with the same
/// status line and JSON body. Returns its base URL (`http://127.0.0.1:PORT`).
async fn fake_server(status_line: &'static str, body: &'static str) -> String {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let response = format!(
                    "{status_line}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            });
        }
    });
    format!("http://127.0.0.1:{port}")
}

/// Stands in for the browser: parses the authorize URL, extracts the loopback
/// `redirect_uri` and `state`, and fires the redirect back with a fake code —
/// exactly what Entra would do after the user signs in.
fn fake_browser(auth_url: &str) -> Result<(), AzureError> {
    let parsed = url::Url::parse(auth_url).unwrap();
    let pairs: HashMap<String, String> = parsed.query_pairs().into_owned().collect();
    let redirect = url::Url::parse(&pairs["redirect_uri"]).unwrap();
    let port = redirect.port().unwrap();
    let state = pairs["state"].clone();
    tokio::spawn(async move {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let request = format!(
            "GET /?code=FAKE_CODE&state={state} HTTP/1.1\r\nHost: localhost\r\n\
             Connection: close\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf).await;
    });
    Ok(())
}

/// A browser opener that must never run — asserts a flow stayed silent.
fn browser_must_not_open(_: &str) -> Result<(), AzureError> {
    panic!("the browser must not open on this path");
}

#[tokio::test]
async fn interactive_flow_exchanges_the_code_for_a_token() {
    let login = fake_server(
        "HTTP/1.1 200 OK",
        r#"{"access_token":"ACCESS_TOKEN","expires_in":3600,"refresh_token":"REFRESH_TOKEN"}"#,
    )
    .await;
    let endpoints = Endpoints {
        login,
        imds: "http://127.0.0.1:1".to_string(),
    };

    let token = acquire_token(
        &EntraAuth::interactive_default(),
        OSSRDBMS_RESOURCE,
        None,
        &endpoints,
        Duration::from_secs(5),
        fake_browser,
    )
    .await
    .token
    .unwrap();

    assert_eq!(token.secret, "ACCESS_TOKEN");
    assert_eq!(token.refresh_token.as_deref(), Some("REFRESH_TOKEN"));
}

#[tokio::test]
async fn a_cached_refresh_token_is_redeemed_without_opening_a_browser() {
    let login = fake_server(
        "HTTP/1.1 200 OK",
        r#"{"access_token":"RENEWED","expires_in":3600,"refresh_token":"ROTATED"}"#,
    )
    .await;
    let endpoints = Endpoints {
        login,
        imds: "http://127.0.0.1:1".to_string(),
    };

    // browser_must_not_open panics if the interactive flow is reached.
    let acquired = acquire_token(
        &EntraAuth::interactive_default(),
        OSSRDBMS_RESOURCE,
        Some("CACHED_REFRESH"),
        &endpoints,
        Duration::from_secs(5),
        browser_must_not_open,
    )
    .await;
    assert!(!acquired.refresh_rejected);
    let token = acquired.token.unwrap();

    assert_eq!(token.secret, "RENEWED");
    assert_eq!(token.refresh_token.as_deref(), Some("ROTATED"));
}

#[tokio::test]
async fn managed_identity_reads_the_token_from_imds() {
    // If the App Service identity endpoint is present in the environment it
    // would take precedence; these tests assume it is not (true in dev/CI).
    if std::env::var("IDENTITY_ENDPOINT").is_ok() {
        eprintln!("skipping: IDENTITY_ENDPOINT is set in this environment");
        return;
    }
    let imds = fake_server(
        "HTTP/1.1 200 OK",
        r#"{"access_token":"MI_TOKEN","expires_on":"4102444800","token_type":"Bearer"}"#,
    )
    .await;
    let endpoints = Endpoints {
        login: "http://127.0.0.1:1".to_string(),
        imds,
    };

    let token = acquire_token(
        &EntraAuth::ManagedIdentity { client_id: None },
        SQLDB_RESOURCE,
        None,
        &endpoints,
        Duration::from_secs(5),
        browser_must_not_open,
    )
    .await
    .token
    .unwrap();

    assert_eq!(token.secret, "MI_TOKEN");
    assert!(token.refresh_token.is_none());
}

#[tokio::test]
async fn a_token_endpoint_error_surfaces_as_an_oauth_error() {
    let login = fake_server(
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"invalid_grant","error_description":"AADSTS70008: token expired"}"#,
    )
    .await;
    let endpoints = Endpoints {
        login,
        imds: "http://127.0.0.1:1".to_string(),
    };

    // No cached refresh → interactive; the fake browser returns a code, but the
    // token exchange fails with a 400 the provider describes.
    let err = acquire_token(
        &EntraAuth::interactive_default(),
        OSSRDBMS_RESOURCE,
        None,
        &endpoints,
        Duration::from_secs(5),
        fake_browser,
    )
    .await
    .token
    .unwrap_err();

    assert!(
        matches!(&err, AzureError::OAuth { error, .. } if error == "invalid_grant"),
        "expected an OAuth error, got {err:?}"
    );
    assert!(err.to_string().contains("token expired"));
}

/// The opener a silent connect passes: no browser, so the flow ends here.
fn refuse_browser(_: &str) -> Result<(), AzureError> {
    Err(AzureError::Browser(
        "interactive sign-in required".to_string(),
    ))
}

#[tokio::test]
async fn a_refused_refresh_token_is_reported_so_the_caller_forgets_it() {
    let login = fake_server(
        "HTTP/1.1 400 Bad Request",
        r#"{"error":"invalid_grant","error_description":"AADSTS700082: expired"}"#,
    )
    .await;
    let endpoints = Endpoints {
        login,
        imds: "http://127.0.0.1:1".to_string(),
    };

    let acquired = acquire_token(
        &EntraAuth::interactive_default(),
        OSSRDBMS_RESOURCE,
        Some("STALE_REFRESH"),
        &endpoints,
        Duration::from_secs(5),
        refuse_browser,
    )
    .await;

    assert!(
        acquired.refresh_rejected,
        "a refresh token Entra refused for good was not reported, so it stays \
         kept and costs a token round trip before every later sign-in"
    );
    // Still falls through to the interactive flow, which this opener refuses.
    assert!(matches!(acquired.token, Err(AzureError::Browser(_))));
}

#[tokio::test]
async fn a_refresh_that_fails_for_any_other_reason_keeps_the_token() {
    // An outage, and a token endpoint nobody is listening on: neither says the
    // token is bad, and forgetting it would sign the user out over a blip.
    let outage = fake_server(
        "HTTP/1.1 503 Service Unavailable",
        r#"{"error":"temporarily_unavailable","error_description":"try later"}"#,
    )
    .await;
    for login in [outage, "http://127.0.0.1:1".to_string()] {
        let endpoints = Endpoints {
            login: login.clone(),
            imds: "http://127.0.0.1:1".to_string(),
        };
        let acquired = acquire_token(
            &EntraAuth::interactive_default(),
            OSSRDBMS_RESOURCE,
            Some("GOOD_REFRESH"),
            &endpoints,
            Duration::from_secs(5),
            refuse_browser,
        )
        .await;
        assert!(!acquired.refresh_rejected, "{login}");
        assert!(acquired.token.is_err());
    }
}

/// What a real browser sends the loopback listener besides the redirect.
#[derive(Clone, Copy, Debug)]
enum Noise {
    /// A speculative preconnect: opened and held, nothing sent.
    IdlePreconnect,
    /// A connection closed before sending a request.
    EmptyConnection,
    /// An unrelated request, such as the favicon fetch.
    Favicon,
}

/// A browser that opens `noise` connections to the loopback listener before
/// the redirect, on a connection of their own, as Chrome does.
fn noisy_browser(noise: Noise) -> impl FnOnce(&str) -> Result<(), AzureError> {
    move |auth_url: &str| {
        let parsed = url::Url::parse(auth_url).unwrap();
        let pairs: HashMap<String, String> = parsed.query_pairs().into_owned().collect();
        let port = url::Url::parse(&pairs["redirect_uri"])
            .unwrap()
            .port()
            .unwrap();
        let state = pairs["state"].clone();
        tokio::spawn(async move {
            let mut first = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            match noise {
                Noise::IdlePreconnect => {}
                Noise::EmptyConnection => first.shutdown().await.unwrap(),
                Noise::Favicon => first
                    .write_all(b"GET /favicon.ico HTTP/1.1\r\nHost: localhost\r\n\r\n")
                    .await
                    .unwrap(),
            }
            // Give the listener the chance to take the noise first.
            tokio::time::sleep(Duration::from_millis(200)).await;
            let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            let request = format!(
                "GET /?code=FAKE_CODE&state={state} HTTP/1.1\r\nHost: localhost\r\n\
                 Connection: close\r\n\r\n"
            );
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;
            drop(first);
        });
        Ok(())
    }
}

#[tokio::test]
async fn the_redirect_is_found_among_the_browser_s_other_connections() {
    let login = fake_server(
        "HTTP/1.1 200 OK",
        r#"{"access_token":"ACCESS_TOKEN","expires_in":3600,"refresh_token":"REFRESH_TOKEN"}"#,
    )
    .await;
    let endpoints = Endpoints {
        login,
        imds: "http://127.0.0.1:1".to_string(),
    };
    for noise in [
        Noise::IdlePreconnect,
        Noise::EmptyConnection,
        Noise::Favicon,
    ] {
        let acquired = acquire_token(
            &EntraAuth::interactive_default(),
            OSSRDBMS_RESOURCE,
            None,
            &endpoints,
            Duration::from_secs(3),
            noisy_browser(noise),
        )
        .await;
        assert_eq!(
            acquired.token.map(|token| token.secret),
            Ok("ACCESS_TOKEN".to_string()),
            "{noise:?} reached the listener before the redirect, and the \
             sign-in the user completed was reported as failed"
        );
    }
}

#[tokio::test]
async fn a_silent_connection_cannot_hold_the_sign_in_past_its_deadline() {
    // A preconnect the browser never uses, and no redirect: the wait ends at
    // its deadline rather than reading the silent connection for ever.
    let endpoints = Endpoints {
        login: "http://127.0.0.1:1".to_string(),
        imds: "http://127.0.0.1:1".to_string(),
    };
    let started = std::time::Instant::now();
    let acquired = tokio::time::timeout(
        Duration::from_secs(10),
        acquire_token(
            &EntraAuth::interactive_default(),
            OSSRDBMS_RESOURCE,
            None,
            &endpoints,
            Duration::from_secs(1),
            |auth_url: &str| {
                let parsed = url::Url::parse(auth_url).unwrap();
                let pairs: HashMap<String, String> = parsed.query_pairs().into_owned().collect();
                let port = url::Url::parse(&pairs["redirect_uri"])
                    .unwrap()
                    .port()
                    .unwrap();
                tokio::spawn(async move {
                    let _held = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
                    tokio::time::sleep(Duration::from_secs(30)).await;
                });
                Ok(())
            },
        ),
    )
    .await
    .expect("a silent connection held the sign-in past its deadline");
    assert!(matches!(acquired.token, Err(AzureError::Timeout)));
    assert!(started.elapsed() < Duration::from_secs(5));
}

/// An opener that records the loopback port and sends nothing.
fn port_recorder(
    port: std::sync::Arc<std::sync::Mutex<Option<u16>>>,
) -> impl FnOnce(&str) -> Result<(), AzureError> {
    move |auth_url: &str| {
        let parsed = url::Url::parse(auth_url).unwrap();
        let pairs: HashMap<String, String> = parsed.query_pairs().into_owned().collect();
        *port.lock().unwrap() = url::Url::parse(&pairs["redirect_uri"]).unwrap().port();
        Ok(())
    }
}

/// Whether anything still listens on the loopback `port`, once the runtime
/// has had a moment to drop what was aborted.
async fn still_listening(port: u16) -> bool {
    tokio::time::sleep(Duration::from_millis(200)).await;
    TcpStream::connect(("127.0.0.1", port)).await.is_ok()
}

#[tokio::test]
async fn the_loopback_listener_closes_with_the_sign_in() {
    // Cancelling a connect drops the sign-in mid-wait. The listener must go
    // with it: left bound, it accepts from any local process for the rest of
    // the run, and every cancelled attempt leaks another.
    let endpoints = Endpoints {
        login: "http://127.0.0.1:1".to_string(),
        imds: "http://127.0.0.1:1".to_string(),
    };
    for (case, wait) in [
        ("cancelled mid-wait", Duration::from_secs(30)),
        ("timed out", Duration::from_millis(300)),
    ] {
        let port = std::sync::Arc::new(std::sync::Mutex::new(None));
        let _ = tokio::time::timeout(
            Duration::from_secs(1),
            acquire_token(
                &EntraAuth::interactive_default(),
                OSSRDBMS_RESOURCE,
                None,
                &endpoints,
                wait,
                port_recorder(port.clone()),
            ),
        )
        .await;
        let port = port.lock().unwrap().expect("the browser was opened");
        assert!(
            !still_listening(port).await,
            "{case}: the sign-in's loopback listener outlived it"
        );
    }
}
