//! Entra token-acquisition flows (FRE-43) driven end to end against a local
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
    let token = acquire_token(
        &EntraAuth::interactive_default(),
        OSSRDBMS_RESOURCE,
        Some("CACHED_REFRESH"),
        &endpoints,
        Duration::from_secs(5),
        browser_must_not_open,
    )
    .await
    .unwrap();

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
    .unwrap_err();

    assert!(
        matches!(&err, AzureError::OAuth { error, .. } if error == "invalid_grant"),
        "expected an OAuth error, got {err:?}"
    );
    assert!(err.to_string().contains("token expired"));
}
