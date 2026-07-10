//! SSH local port forwarding for Postgres connections, built on russh.
//!
//! A [`Tunnel`] owns one SSH session and a local TCP listener; every client
//! connection to the listener is forwarded through a `direct-tcpip` channel
//! to the configured target (the database as seen from the SSH server).
//! Dropping the tunnel shuts the forward down and disconnects the session —
//! the UI ties a tunnel's lifetime to its connection tab.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use russh::client;
use russh::keys::agent::client::AgentClient;
use russh::keys::{load_secret_key, HashAlg, PrivateKey, PrivateKeyWithHashAlg};
use russh::Disconnect;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

/// SSH tunnel settings persisted with a saved connection. No secrets here:
/// key passphrases live in the OS keyring / session memory, like database
/// passwords.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelConfig {
    pub host: String,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    pub user: String,
    pub auth: TunnelAuth,
}

fn default_ssh_port() -> u16 {
    22
}

/// How to authenticate to the SSH server. Internally tagged on `method` so
/// the TOML form is a plain table (`method = "keyfile"`, `path = "…"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "lowercase")]
pub enum TunnelAuth {
    /// The ssh-agent at `SSH_AUTH_SOCK`.
    Agent,
    /// A private key file; an encrypted key prompts for its passphrase.
    KeyFile { path: PathBuf },
}

/// Error opening or running a tunnel. Every message is prefixed
/// "SSH tunnel:" so the UI never confuses a tunnel failure with a database
/// failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelError {
    /// TCP or SSH-protocol failure reaching the SSH server.
    Connect(String),
    /// The server or agent rejected authentication.
    Auth(String),
    /// The key file is encrypted and the passphrase is missing or wrong.
    /// Distinct from [`TunnelError::Auth`] so callers can prompt for the
    /// passphrase instead of surfacing an error.
    NeedsPassphrase(String),
    /// Local listener or port-forward setup failed.
    Forward(String),
}

impl fmt::Display for TunnelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TunnelError::Connect(m) => write!(f, "SSH tunnel: connection failed: {m}"),
            TunnelError::Auth(m) => write!(f, "SSH tunnel: authentication failed: {m}"),
            TunnelError::NeedsPassphrase(m) => write!(f, "SSH tunnel: {m}"),
            TunnelError::Forward(m) => write!(f, "SSH tunnel: port forwarding failed: {m}"),
        }
    }
}

impl std::error::Error for TunnelError {}

/// russh client handler.
///
/// SECURITY WARNING: `check_server_key` accepts ANY server host key, so the
/// tunnel is not protected against man-in-the-middle attacks on first (or
/// any) connect. Host-key verification against known_hosts is a follow-up
/// issue — do not ship a release build without it.
struct AcceptAnyHostKey;

impl client::Handler for AcceptAnyHostKey {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        // See the SECURITY WARNING on `AcceptAnyHostKey`.
        Ok(true)
    }
}

/// A live SSH tunnel: connections to `127.0.0.1:local_port()` are forwarded
/// to the target host/port through the SSH session. Dropping it closes the
/// listener, aborts in-flight forwards, and disconnects the session.
pub struct Tunnel {
    local_port: u16,
    shutdown: Option<oneshot::Sender<()>>,
    /// Kept so the accept loop is tied to the tunnel's lifetime; the loop
    /// exits via the shutdown signal, not via abort, so the SSH disconnect
    /// still runs.
    _task: tokio::task::JoinHandle<()>,
}

impl fmt::Debug for Tunnel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tunnel")
            .field("local_port", &self.local_port)
            .finish_non_exhaustive()
    }
}

impl Tunnel {
    /// Connects and authenticates to the SSH server, binds an ephemeral
    /// local port, and starts forwarding it to `target_host:target_port`
    /// (an address as seen from the SSH server, e.g. the database host).
    ///
    /// `passphrase` decrypts a [`TunnelAuth::KeyFile`] key when needed; a
    /// missing or wrong passphrase yields [`TunnelError::NeedsPassphrase`]
    /// before any network traffic, so callers can prompt cheaply.
    pub async fn open(
        config: TunnelConfig,
        passphrase: Option<String>,
        target_host: String,
        target_port: u16,
    ) -> Result<Tunnel, TunnelError> {
        // Load (and decrypt) the key first: fail fast on passphrase
        // problems without a wasted SSH connection.
        let key = match &config.auth {
            TunnelAuth::KeyFile { path } => Some(load_key(path, passphrase.as_deref())?),
            TunnelAuth::Agent => None,
        };

        let ssh_config = Arc::new(client::Config::default());
        let address = (config.host.as_str(), config.port);
        let mut handle = client::connect(ssh_config, address, AcceptAnyHostKey)
            .await
            .map_err(|e| TunnelError::Connect(format!("{}:{}: {e}", config.host, config.port)))?;

        authenticate(&mut handle, &config, key).await?;

        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| TunnelError::Forward(format!("binding a local port: {e}")))?;
        let local_port = listener
            .local_addr()
            .map_err(|e| TunnelError::Forward(format!("reading the local port: {e}")))?
            .port();

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(accept_loop(
            handle,
            listener,
            target_host,
            target_port,
            shutdown_rx,
        ));

        Ok(Tunnel {
            local_port,
            shutdown: Some(shutdown_tx),
            _task: task,
        })
    }

    /// The forwarded local port: connect Postgres to
    /// `127.0.0.1:<local_port>`.
    pub fn local_port(&self) -> u16 {
        self.local_port
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            // The accept loop selects on this signal, drops the listener and
            // all per-connection copy tasks, and disconnects the session. If
            // the task is already gone the send just fails — nothing to do.
            let _ = shutdown.send(());
        }
    }
}

/// Loads a private key file, mapping russh's errors onto the tunnel error
/// vocabulary. `KeyIsEncrypted` (no passphrase) and a failed decrypt (wrong
/// passphrase) both surface as [`TunnelError::NeedsPassphrase`].
fn load_key(path: &Path, passphrase: Option<&str>) -> Result<PrivateKey, TunnelError> {
    use russh::keys::ssh_key;
    use russh::keys::Error as KeyError;
    match load_secret_key(path, passphrase) {
        Ok(key) => Ok(key),
        Err(KeyError::KeyIsEncrypted) => Err(TunnelError::NeedsPassphrase(format!(
            "the key {} is encrypted — a passphrase is required",
            path.display()
        ))),
        Err(KeyError::SshKey(ssh_key::Error::Crypto)) if passphrase.is_some() => {
            Err(TunnelError::NeedsPassphrase(format!(
                "the key {} could not be decrypted — wrong passphrase?",
                path.display()
            )))
        }
        Err(err) => Err(TunnelError::Auth(format!(
            "loading the key {}: {err}",
            path.display()
        ))),
    }
}

/// Public-key authentication: with the loaded key file, or by trying every
/// identity the ssh-agent offers.
async fn authenticate(
    handle: &mut client::Handle<AcceptAnyHostKey>,
    config: &TunnelConfig,
    key: Option<PrivateKey>,
) -> Result<(), TunnelError> {
    match key {
        Some(key) => {
            let hash_alg = best_rsa_hash(handle, key.algorithm().is_rsa()).await;
            let result = handle
                .authenticate_publickey(
                    config.user.clone(),
                    PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg),
                )
                .await
                .map_err(|e| TunnelError::Auth(e.to_string()))?;
            if result.success() {
                Ok(())
            } else {
                Err(TunnelError::Auth(format!(
                    "the server rejected the key for user {}",
                    config.user
                )))
            }
        }
        None => authenticate_with_agent(handle, config).await,
    }
}

#[cfg(unix)]
async fn authenticate_with_agent(
    handle: &mut client::Handle<AcceptAnyHostKey>,
    config: &TunnelConfig,
) -> Result<(), TunnelError> {
    let mut agent = AgentClient::connect_env()
        .await
        .map_err(|e| TunnelError::Auth(format!("ssh-agent unavailable: {e}")))?;
    let identities = agent
        .request_identities()
        .await
        .map_err(|e| TunnelError::Auth(format!("ssh-agent: {e}")))?;
    if identities.is_empty() {
        return Err(TunnelError::Auth(
            "the ssh-agent holds no identities (ssh-add a key first)".to_string(),
        ));
    }
    let count = identities.len();
    for identity in identities {
        let public = identity.public_key().into_owned();
        let hash_alg = best_rsa_hash(handle, public.algorithm().is_rsa()).await;
        let result = handle
            .authenticate_publickey_with(config.user.clone(), public, hash_alg, &mut agent)
            .await;
        if matches!(result, Ok(r) if r.success()) {
            return Ok(());
        }
    }
    Err(TunnelError::Auth(format!(
        "the server accepted none of the ssh-agent's {count} identities for user {}",
        config.user
    )))
}

#[cfg(not(unix))]
async fn authenticate_with_agent(
    _handle: &mut client::Handle<AcceptAnyHostKey>,
    _config: &TunnelConfig,
) -> Result<(), TunnelError> {
    Err(TunnelError::Auth(
        "ssh-agent authentication is not supported on this platform".to_string(),
    ))
}

/// The hash algorithm for RSA signatures, negotiated with the server; `None`
/// for non-RSA keys (russh ignores it) and for servers that never sent
/// extension info.
async fn best_rsa_hash(handle: &client::Handle<AcceptAnyHostKey>, is_rsa: bool) -> Option<HashAlg> {
    if !is_rsa {
        return None;
    }
    match handle.best_supported_rsa_hash().await {
        // The server announced its preference (possibly "SHA-1 only").
        Ok(Some(alg)) => alg,
        // No EXT_INFO: most modern servers still accept rsa-sha2-256.
        Ok(None) => Some(HashAlg::Sha256),
        Err(_) => None,
    }
}

/// Accepts local connections and forwards each through its own
/// `direct-tcpip` channel until the shutdown signal fires, then disconnects
/// the SSH session. Dropping the `JoinSet` aborts in-flight copies.
async fn accept_loop(
    handle: client::Handle<AcceptAnyHostKey>,
    listener: TcpListener,
    target_host: String,
    target_port: u16,
    mut shutdown: oneshot::Receiver<()>,
) {
    let mut copies = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => {
                let Ok((mut stream, peer)) = accepted else { break };
                let channel = handle
                    .channel_open_direct_tcpip(
                        target_host.clone(),
                        u32::from(target_port),
                        "127.0.0.1".to_string(),
                        u32::from(peer.port()),
                    )
                    .await;
                match channel {
                    Ok(channel) => {
                        copies.spawn(async move {
                            let mut channel_stream = channel.into_stream();
                            let _ = tokio::io::copy_bidirectional(&mut stream, &mut channel_stream)
                                .await;
                        });
                    }
                    // The client sees its socket close; the pool reports a
                    // connection error. Nothing useful to do here.
                    Err(_) => drop(stream),
                }
            }
        }
    }
    let _ = handle
        .disconnect(Disconnect::ByApplication, "tunnel closed", "")
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_display_with_the_ssh_tunnel_prefix() {
        let cases = [
            TunnelError::Connect("refused".into()),
            TunnelError::Auth("rejected".into()),
            TunnelError::NeedsPassphrase("key k is encrypted".into()),
            TunnelError::Forward("bind failed".into()),
        ];
        for err in cases {
            assert!(
                err.to_string().starts_with("SSH tunnel: "),
                "missing prefix: {err}"
            );
        }
        // The categories stay distinguishable in the rendered message.
        assert!(TunnelError::Connect("x".into())
            .to_string()
            .contains("connection failed"));
        assert!(TunnelError::Auth("x".into())
            .to_string()
            .contains("authentication failed"));
        assert!(TunnelError::Forward("x".into())
            .to_string()
            .contains("port forwarding failed"));
    }

    #[test]
    fn tunnel_config_defaults_the_port() {
        let parsed: TunnelConfig =
            toml::from_str("host = \"bastion\"\nuser = \"deploy\"\n\n[auth]\nmethod = \"agent\"\n")
                .unwrap();
        assert_eq!(parsed.port, 22);
        assert_eq!(parsed.auth, TunnelAuth::Agent);
    }

    #[test]
    fn tunnel_auth_tags_are_toml_friendly() {
        let keyfile = TunnelConfig {
            host: "bastion.example.com".into(),
            port: 2222,
            user: "deploy".into(),
            auth: TunnelAuth::KeyFile {
                path: PathBuf::from("/home/u/.ssh/id_ed25519"),
            },
        };
        let text = toml::to_string(&keyfile).unwrap();
        assert!(text.contains("method = \"keyfile\""));
        let back: TunnelConfig = toml::from_str(&text).unwrap();
        assert_eq!(back, keyfile);
    }
}
