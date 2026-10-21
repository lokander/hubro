//! SSH local port forwarding for Postgres connections, built on russh.
//!
//! A [`Tunnel`] owns one SSH session and a local TCP listener; every client
//! connection to the listener is forwarded through a `direct-tcpip` channel
//! to the configured target (the database as seen from the SSH server).
//! Dropping the tunnel shuts the forward down and disconnects the session —
//! the UI ties a tunnel's lifetime to its connection tab.

use std::borrow::Cow;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::client;
#[cfg(any(unix, windows))]
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::known_hosts::learn_known_hosts_path;
use russh::keys::{
    load_secret_key, Algorithm, HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKey,
    PublicKeyOrCertificate,
};
use russh::Disconnect;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, watch};

mod known_hosts;

/// How long reaching the SSH server may take — the TCP connect, the key
/// exchange and the host key check — before the attempt is abandoned. The
/// bound a Postgres open already has (sqlx's 30s acquire timeout). Measured
/// without it: a bastion that drops SYNs held the connect for 133s on Linux,
/// and one that accepts the TCP connection and never sends its banner held it
/// for good.
///
/// Authentication is deliberately outside it: an agent may be waiting on the
/// user (a confirmation prompt, a hardware key's touch), and a connect the
/// user can see is waiting can be cancelled from its row.
const SSH_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Once authenticated, a session that has heard nothing from the server for
/// this long sends it a keepalive, and [`KEEPALIVE_MAX`] unanswered ones end
/// the session. Without them a bastion that vanished silently (a dropped
/// network, a suspended laptop's NAT entry gone) leaves the session looking
/// alive for as long as the OS keeps retransmitting — and every forward
/// through it hanging.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
const KEEPALIVE_MAX: usize = 3;

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
    /// The ssh-agent: `SSH_AUTH_SOCK` on Unix, the OpenSSH service's
    /// `\\.\pipe\openssh-ssh-agent` named pipe on Windows.
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
    /// The server's host key is not recorded in any known_hosts file (first
    /// contact). Carries the fingerprint/type for display and the serialized
    /// key so the caller can persist it on user approval (trust-on-first-use).
    HostKeyUnknown(HostKeyInfo),
    /// The server presented a key that differs from the one recorded for this
    /// host — a possible man-in-the-middle. Never auto-trusted.
    HostKeyChanged(HostKeyInfo),
    /// The server presented a key a known_hosts file marks `@revoked`. Refused
    /// even when the same key is also listed as trusted, as OpenSSH does.
    HostKeyRevoked(HostKeyInfo),
}

/// A server host key that the user must make a trust decision about, plus the
/// data needed to display it and (for [`TunnelError::HostKeyUnknown`]) to
/// persist it via [`trust_host_key`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostKeyInfo {
    pub host: String,
    pub port: u16,
    /// SHA-256 fingerprint, e.g. `SHA256:abc…`.
    pub fingerprint: String,
    /// Key algorithm name, e.g. `ssh-ed25519`.
    pub key_type: String,
    /// The public key in OpenSSH one-line form, replayed to [`trust_host_key`].
    pub key_openssh: String,
}

impl fmt::Display for TunnelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TunnelError::Connect(m) => write!(f, "SSH tunnel: connection failed: {m}"),
            TunnelError::Auth(m) => write!(f, "SSH tunnel: authentication failed: {m}"),
            TunnelError::NeedsPassphrase(m) => write!(f, "SSH tunnel: {m}"),
            TunnelError::Forward(m) => write!(f, "SSH tunnel: port forwarding failed: {m}"),
            TunnelError::HostKeyUnknown(info) => write!(
                f,
                "SSH tunnel: unrecognized host key for {}:{} ({} {})",
                info.host, info.port, info.key_type, info.fingerprint
            ),
            TunnelError::HostKeyChanged(info) => write!(
                f,
                "SSH tunnel: HOST KEY CHANGED for {}:{} ({} {}) — possible \
                 man-in-the-middle; refusing to connect",
                info.host, info.port, info.key_type, info.fingerprint
            ),
            TunnelError::HostKeyRevoked(info) => write!(
                f,
                "SSH tunnel: the host key for {}:{} ({} {}) is marked @revoked in \
                 known_hosts; refusing to connect",
                info.host, info.port, info.key_type, info.fingerprint
            ),
        }
    }
}

impl std::error::Error for TunnelError {}

impl TunnelError {
    /// The key to offer a trust-on-first-use prompt for, or the error back to
    /// show as a refusal. Only a first-contact key may be trusted from the UI;
    /// a changed key is a possible man-in-the-middle and a revoked one is
    /// refused outright. Every variant is named, so a new one has to
    /// decide.
    pub fn into_trust_prompt(self) -> Result<HostKeyInfo, TunnelError> {
        match self {
            TunnelError::HostKeyUnknown(info) => Ok(info),
            err @ (TunnelError::Connect(_)
            | TunnelError::Auth(_)
            | TunnelError::NeedsPassphrase(_)
            | TunnelError::Forward(_)
            | TunnelError::HostKeyChanged(_)
            | TunnelError::HostKeyRevoked(_)) => Err(err),
        }
    }
}

/// Trust status of a server host key relative to the known_hosts files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostKeyStatus {
    /// A matching entry exists — accept silently.
    Trusted,
    /// No file records a key for this host — first contact (trust-on-first-use).
    Unknown,
    /// A file records this host with a *different* key — possible MITM.
    Changed,
    /// A file marks this very key `@revoked`.
    Revoked,
}

/// hubro's own known_hosts store (writable): `~/.config/hubro/known_hosts`.
/// New trust decisions are recorded here; the user's `~/.ssh/known_hosts` is
/// read but never modified.
pub fn app_known_hosts_path() -> Option<PathBuf> {
    crate::config::app_config_file("known_hosts")
}

/// The app's default set of files consulted to decide whether a server host
/// key is trusted. Reads honor the user's OpenSSH `~/.ssh/known_hosts` (so
/// hosts already trusted via `ssh` connect without a prompt) in addition to
/// hubro's own store. Passed into [`Tunnel::open`] by the UI; tests inject
/// their own paths instead.
pub fn default_known_hosts_read() -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Some(app) = app_known_hosts_path() {
        files.push(app);
    }
    if let Some(home) = dirs::home_dir() {
        files.push(home.join(".ssh").join("known_hosts"));
    }
    files
}

/// Classifies `key` for `host:port` against what `files` record for that host
/// ([`known_hosts::recorded_for`]), by [`host_key_verdict`].
fn verify_host_key(host: &str, port: u16, key: &PublicKey, files: &[PathBuf]) -> HostKeyStatus {
    let recorded = known_hosts::recorded_for(host, port, files);
    host_key_verdict(key, &recorded.keys, &recorded.revoked)
}

/// Revoked if any `@revoked` entry is the offered key, whatever else is
/// recorded; then Trusted if any recorded key is the offered one, wherever it
/// sits; Changed if the host is recorded at all but only under other keys;
/// Unknown only when nothing is recorded. This is OpenSSH's rule (`hostfile.c`),
/// so a key of a different *type* counts as Changed. The type case matters
/// because russh does not check the key a server sends against the negotiated
/// algorithm, so a server can agree to the recorded type and present another.
/// Reading that as Unknown turned a man-in-the-middle into a trust prompt for a
/// host the user already trusts (reproduced against a server that negotiated
/// ssh-ed25519 and sent an ECDSA key).
fn host_key_verdict(
    offered: &PublicKey,
    recorded: &[PublicKey],
    revoked: &[PublicKey],
) -> HostKeyStatus {
    // Key material only: the comment is not part of the key's identity.
    let is_offered = |r: &PublicKey| r.key_data() == offered.key_data();
    if revoked.iter().any(is_offered) {
        HostKeyStatus::Revoked
    } else if recorded.iter().any(is_offered) {
        HostKeyStatus::Trusted
    } else if recorded.is_empty() {
        HostKeyStatus::Unknown
    } else {
        HostKeyStatus::Changed
    }
}

/// The host-key algorithms [`Tunnel::open`] negotiates for `host:port`: those
/// that present a key recorded for it in `files`, so an honest server holding
/// several keys shows the recorded one rather than another, which would read as
/// [`HostKeyStatus::Changed`]. Empty when nothing is recorded, which leaves
/// russh's defaults. This steers an honest server and guards nothing: a server
/// can agree to these and send another type anyway, which is why
/// [`host_key_verdict`] decides on the key actually sent. It is stricter than
/// OpenSSH, which only orders known types first, so a server that rotates to a
/// brand-new key type fails negotiation with a generic connect error until the
/// stale entry is removed.
fn recorded_key_algorithms(host: &str, port: u16, files: &[PathBuf]) -> Vec<Algorithm> {
    let mut algorithms = Vec::new();
    for recorded in known_hosts::recorded_for(host, port, files).keys {
        for algorithm in presenting_algorithms(recorded.algorithm()) {
            if !algorithms.contains(&algorithm) {
                algorithms.push(algorithm);
            }
        }
    }
    algorithms
}

/// The russh client config for `host:port`: defaults, except that host-key
/// negotiation asks for the recorded key's type ([`recorded_key_algorithms`]),
/// so an honest server holding several keys presents the one we trust. The
/// check that a different type is Changed rather than Unknown is
/// [`host_key_verdict`]'s, since negotiation is not enforced on the key a
/// server sends. Nothing recorded → russh's defaults.
///
/// It also turns on keepalives ([`KEEPALIVE_INTERVAL`]), which russh leaves
/// off.
fn client_config(host: &str, port: u16, known_hosts_read: &[PathBuf]) -> client::Config {
    let mut ssh_config = client::Config {
        keepalive_interval: Some(KEEPALIVE_INTERVAL),
        keepalive_max: KEEPALIVE_MAX,
        ..client::Config::default()
    };
    let pinned = recorded_key_algorithms(host, port, known_hosts_read);
    if !pinned.is_empty() {
        ssh_config.preferred.key = Cow::Owned(pinned);
    }
    ssh_config
}

/// The algorithms a server can present a key of this format under. known_hosts
/// records an RSA key as `ssh-rsa`, its format, but servers sign with it as
/// rsa-sha2-512 or rsa-sha2-256, and OpenSSH 8.8+ disables SHA-1 `ssh-rsa` by
/// default. Pinning `ssh-rsa` alone made every reconnect to an RSA-only
/// OpenSSH server fail (measured against OpenSSH 10.3), so RSA gets all three,
/// in russh's default order.
fn presenting_algorithms(format: Algorithm) -> Vec<Algorithm> {
    match format {
        Algorithm::Rsa { .. } => vec![
            Algorithm::Rsa {
                hash: Some(HashAlg::Sha512),
            },
            Algorithm::Rsa {
                hash: Some(HashAlg::Sha256),
            },
            Algorithm::Rsa { hash: None },
        ],
        other => vec![other],
    }
}

/// Builds the display/persistence info for the key the server offered.
fn host_key_info(host: &str, port: u16, key: &PublicKey) -> HostKeyInfo {
    HostKeyInfo {
        host: host.to_string(),
        port,
        fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
        key_type: key.algorithm().as_str().to_string(),
        key_openssh: key.to_openssh().unwrap_or_default(),
    }
}

/// Records a server key into the known_hosts file at `write_path` so a later
/// connect to `host:port` treats it as trusted. `key_openssh` is the serialized
/// key carried by [`TunnelError::HostKeyUnknown`] — this is the "yes, trust it"
/// half of the trust-on-first-use prompt. The UI passes [`app_known_hosts_path`].
pub fn trust_host_key(
    host: &str,
    port: u16,
    key_openssh: &str,
    write_path: &Path,
) -> Result<(), TunnelError> {
    let key = PublicKey::from_openssh(key_openssh)
        .map_err(|e| TunnelError::Connect(format!("parsing the host key: {e}")))?;
    learn_known_hosts_path(host, port, &key, write_path)
        .map_err(|e| TunnelError::Connect(format!("recording the host key: {e}")))
}

/// russh client handler that verifies the server's host key against
/// known_hosts. `check_server_key` records its verdict (and the offered key) so
/// [`Tunnel::open`] can turn a rejection into a precise [`TunnelError`] after
/// `client::connect` fails.
///
/// The session owns its handler and drops it when it ends, however it ends,
/// which is what makes `alive` the session's end signal (see
/// [`Tunnel::session_ended`]). russh's own `disconnected` callback is not
/// that: it is skipped when shutting down the socket fails, which is the
/// likely case for a connection the server reset.
struct HostKeyVerifier {
    host: String,
    port: u16,
    files: Vec<PathBuf>,
    verdict: Arc<Mutex<Option<HostKeyStatus>>>,
    offered_key: Arc<Mutex<Option<PublicKey>>>,
    /// Never sent on; held only to be dropped with the session.
    _alive: watch::Sender<()>,
}

impl client::Handler for HostKeyVerifier {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        offered: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        // A host certificate is refused, not checked by the key inside it:
        // trusting one means trusting the CA that signed it, and known_hosts
        // here records keys, never authorities. The connect then fails with no
        // verdict, as a generic error. russh only negotiates certificates when
        // `preferred.host_key_certificates` names algorithms, and `open` leaves
        // it empty, so this is the fail-closed answer rather than a live path.
        let PublicKeyOrCertificate::PublicKey { key, .. } = offered else {
            return Ok(false);
        };
        let status = verify_host_key(&self.host, self.port, key, &self.files);
        if let Ok(mut offered) = self.offered_key.lock() {
            *offered = Some(key.clone());
        }
        if let Ok(mut verdict) = self.verdict.lock() {
            *verdict = Some(status);
        }
        Ok(matches!(status, HostKeyStatus::Trusted))
    }
}

/// A live SSH tunnel: connections to `127.0.0.1:local_port()` are forwarded
/// to the target host/port through the SSH session. Dropping it closes the
/// listener, aborts in-flight forwards, and disconnects the session.
pub struct Tunnel {
    local_port: u16,
    /// `host:port` of the SSH server, for [`Self::ended_message`].
    server: String,
    /// Closes when the SSH session ends (see [`HostKeyVerifier`]).
    alive: watch::Receiver<()>,
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
    ///
    /// The server's host key is verified against `known_hosts_read` (the UI
    /// passes [`default_known_hosts_read`]): an unrecognized key yields
    /// [`TunnelError::HostKeyUnknown`], a changed one
    /// [`TunnelError::HostKeyChanged`] and a revoked one
    /// [`TunnelError::HostKeyRevoked`], all before authentication.
    ///
    /// Reaching the server is abandoned after [`SSH_CONNECT_TIMEOUT`].
    pub async fn open(
        config: TunnelConfig,
        passphrase: Option<String>,
        target_host: String,
        target_port: u16,
        known_hosts_read: &[PathBuf],
    ) -> Result<Tunnel, TunnelError> {
        Self::open_within(
            config,
            passphrase,
            target_host,
            target_port,
            known_hosts_read,
            SSH_CONNECT_TIMEOUT,
        )
        .await
    }

    /// [`Self::open`] with the connect bound as a parameter, so a test can
    /// wait out a short one.
    async fn open_within(
        config: TunnelConfig,
        passphrase: Option<String>,
        target_host: String,
        target_port: u16,
        known_hosts_read: &[PathBuf],
        connect_limit: Duration,
    ) -> Result<Tunnel, TunnelError> {
        // Load (and decrypt) the key first: fail fast on passphrase
        // problems without a wasted SSH connection.
        let key = match &config.auth {
            TunnelAuth::KeyFile { path } => Some(load_key(path, passphrase.as_deref())?),
            TunnelAuth::Agent => None,
        };

        let ssh_config = Arc::new(client_config(&config.host, config.port, known_hosts_read));
        let address = (config.host.as_str(), config.port);
        let verdict = Arc::new(Mutex::new(None));
        let offered_key = Arc::new(Mutex::new(None));
        let (alive_tx, alive) = watch::channel(());
        let verifier = HostKeyVerifier {
            host: config.host.clone(),
            port: config.port,
            files: known_hosts_read.to_vec(),
            verdict: Arc::clone(&verdict),
            offered_key: Arc::clone(&offered_key),
            _alive: alive_tx,
        };
        let connect = client::connect(ssh_config, address, verifier);
        let connected = match tokio::time::timeout(connect_limit, connect).await {
            Ok(connected) => connected,
            Err(_) => {
                return Err(TunnelError::Connect(format!(
                    "{}:{}: no answer within {connect_limit:?}",
                    config.host, config.port
                )))
            }
        };
        let mut handle = match connected {
            Ok(handle) => handle,
            // If the handshake failed because we rejected the host key, turn
            // the generic connect error into a precise host-key error the UI
            // can act on (trust prompt for Unknown, hard refusal for Changed
            // and Revoked).
            Err(e) => {
                let status = verdict.lock().ok().and_then(|mut v| v.take());
                let offered = offered_key.lock().ok().and_then(|mut k| k.take());
                return Err(match (status, offered) {
                    (Some(HostKeyStatus::Unknown), Some(key)) => {
                        TunnelError::HostKeyUnknown(host_key_info(&config.host, config.port, &key))
                    }
                    (Some(HostKeyStatus::Changed), Some(key)) => {
                        TunnelError::HostKeyChanged(host_key_info(&config.host, config.port, &key))
                    }
                    (Some(HostKeyStatus::Revoked), Some(key)) => {
                        TunnelError::HostKeyRevoked(host_key_info(&config.host, config.port, &key))
                    }
                    _ => TunnelError::Connect(format!("{}:{}: {e}", config.host, config.port)),
                });
            }
        };

        authenticate(&mut handle, &config, key).await?;

        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| TunnelError::Forward(format!("binding a local port: {e}")))?;
        let local_port = listener
            .local_addr()
            .map_err(|e| TunnelError::Forward(format!("reading the local port: {e}")))?
            .port();

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(forward(
            handle,
            listener,
            target_host,
            target_port,
            shutdown_rx,
        ));

        Ok(Tunnel {
            local_port,
            server: format!("{}:{}", config.host, config.port),
            alive,
            shutdown: Some(shutdown_tx),
            _task: task,
        })
    }

    /// Resolves once the SSH session has ended: the server closed it, the
    /// network dropped it (noticed by keepalives, [`KEEPALIVE_INTERVAL`]), or
    /// the tunnel was dropped. From then on nothing can be forwarded, so a
    /// database connection over the tunnel is gone even though its pool does
    /// not know it — measured, with the bastion killed, every query on a
    /// Postgres tab failed at once with "unexpected response from
    /// SSLRequest" or "Connection reset by peer", and the tab still looked
    /// connected. This is how the UI finds out.
    ///
    /// The forward itself keeps accepting, and each connection it accepts
    /// fails its channel open at once and is closed. That is what keeps those
    /// failures immediate: closing the port instead was tried, and a refused
    /// connect is one sqlx retries until its 30s acquire timeout, so every
    /// query took 30s to fail.
    pub fn session_ended(&self) -> impl Future<Output = ()> + Send + 'static {
        session_end(self.alive.clone())
    }

    /// What to tell the user once [`Self::session_ended`] has resolved.
    pub fn ended_message(&self) -> String {
        format!("SSH tunnel: the session to {} has ended", self.server)
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
    handle: &mut client::Handle<HostKeyVerifier>,
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
    handle: &mut client::Handle<HostKeyVerifier>,
    config: &TunnelConfig,
) -> Result<(), TunnelError> {
    let agent = AgentClient::connect_env()
        .await
        .map_err(|e| TunnelError::Auth(format!("ssh-agent unavailable: {e}")))?;
    try_agent_identities(handle, config, agent).await
}

/// The named pipe the Windows OpenSSH agent serves; it exists only while
/// the "OpenSSH Authentication Agent" service is running.
#[cfg(windows)]
const WINDOWS_AGENT_PIPE: &str = r"\\.\pipe\openssh-ssh-agent";

#[cfg(windows)]
async fn authenticate_with_agent(
    handle: &mut client::Handle<HostKeyVerifier>,
    config: &TunnelConfig,
) -> Result<(), TunnelError> {
    let agent = connect_windows_agent().await?;
    try_agent_identities(handle, config, agent).await
}

#[cfg(windows)]
async fn connect_windows_agent(
) -> Result<AgentClient<tokio::net::windows::named_pipe::NamedPipeClient>, TunnelError> {
    AgentClient::connect_named_pipe(WINDOWS_AGENT_PIPE)
        .await
        .map_err(|e| {
            TunnelError::Auth(format!(
                "ssh-agent unavailable: {e} (is the \"OpenSSH Authentication Agent\" Windows service running?)"
            ))
        })
}

#[cfg(any(unix, windows))]
async fn try_agent_identities<S: AgentStream + Send + Unpin>(
    handle: &mut client::Handle<HostKeyVerifier>,
    config: &TunnelConfig,
    mut agent: AgentClient<S>,
) -> Result<(), TunnelError> {
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

#[cfg(not(any(unix, windows)))]
async fn authenticate_with_agent(
    _handle: &mut client::Handle<HostKeyVerifier>,
    _config: &TunnelConfig,
) -> Result<(), TunnelError> {
    Err(TunnelError::Auth(
        "ssh-agent authentication is not supported on this platform".to_string(),
    ))
}

/// The hash algorithm for RSA signatures, negotiated with the server; `None`
/// for non-RSA keys (russh ignores it) and for servers that never sent
/// extension info.
async fn best_rsa_hash(handle: &client::Handle<HostKeyVerifier>, is_rsa: bool) -> Option<HashAlg> {
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

/// Resolves when the sender half of `alive` is dropped — the session's end.
async fn session_end(mut alive: watch::Receiver<()>) {
    // Nothing is ever sent, so `changed` returns only once the sender is gone.
    while alive.changed().await.is_ok() {}
}

/// Runs the forward over this session until the tunnel is dropped, then
/// disconnects the session.
async fn forward(
    handle: client::Handle<HostKeyVerifier>,
    listener: TcpListener,
    target_host: String,
    target_port: u16,
    shutdown: oneshot::Receiver<()>,
) {
    let handle = Arc::new(handle);
    let open = {
        let handle = Arc::clone(&handle);
        move |peer_port: u16| {
            let handle = Arc::clone(&handle);
            let target_host = target_host.clone();
            async move {
                handle
                    .channel_open_direct_tcpip(
                        target_host,
                        u32::from(target_port),
                        "127.0.0.1".to_string(),
                        u32::from(peer_port),
                    )
                    .await
                    .map(|channel| channel.into_stream())
            }
        }
    };
    accept_loop(listener, open, shutdown).await;
    let _ = handle
        .disconnect(Disconnect::ByApplication, "tunnel closed", "")
        .await;
}

/// Accepts local connections and forwards each through the stream `open`
/// makes for it — a `direct-tcpip` channel — until the shutdown signal fires.
/// Returning drops the listener, so the local port stops accepting, and the
/// `JoinSet`, which aborts in-flight forwards.
///
/// Opening a channel is a round trip to the SSH server, and for OpenSSH it
/// lasts until the server's own connect to the target completes. So it runs
/// inside the forward's task, never in this loop: awaited here, it held up
/// every later connection and the shutdown signal with it — measured, a
/// target that drops SYNs kept a second client waiting past 150s and a
/// dropped tunnel's port open past 200s.
async fn accept_loop<O, F, S, E>(
    listener: TcpListener,
    open: O,
    mut shutdown: oneshot::Receiver<()>,
) where
    O: Fn(u16) -> F,
    F: Future<Output = Result<S, E>> + Send + 'static,
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    E: Send + 'static,
{
    let mut copies = tokio::task::JoinSet::new();
    loop {
        // Reap finished forwards so their results don't accumulate for the
        // tunnel's lifetime.
        while copies.try_join_next().is_some() {}
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => {
                let (mut stream, peer) = match accepted {
                    Ok(accepted) => accepted,
                    // Transient accept errors (ECONNABORTED, fd pressure)
                    // must not kill the tunnel; only shutdown ends the loop.
                    Err(_) => continue,
                };
                let channel = open(peer.port());
                copies.spawn(async move {
                    // A failed open drops the client's socket, and its pool
                    // reports a connection error. Nothing useful to add here.
                    if let Ok(mut channel) = channel.await {
                        let _ = tokio::io::copy_bidirectional(&mut stream, &mut channel).await;
                    }
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Two distinct ed25519 host keys (OpenSSH one-line form).
    const KEY_A: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ";
    const KEY_B: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIA6rWI3G1sz07DnfFlrouTcysQlj2P+jpNSOEWD9OJ3X";
    // Keys of two other types, for a host recorded under ed25519.
    const KEY_ECDSA: &str = "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBCt0yDppBirhQ+3NGimeGgyET84m3sS5pae0/OTdHoborlN0wMG6LJ+jPgmO4EGBUVMGR3RZckSgUOGQOeitvgE=";
    const KEY_RSA: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQCqBoV58WdupNiprCHYGyuv2uVZPMZdP5V7D5aQIJ+jwsF7IPkov1+cfQSz0QUMl3c+DhvCJglpU/Z58Oe5I48Xp1/IJLv8zCG3eySQalqFZumylVLEDlBOpnM4E04Oq8kJXkesca3eTQW2mfSRL0tlNQd8hAfSwjELjld0yXnPH2r0v4bILgxVfmpxQuicF6wVwvDQl7VqS8AOcKwhQqZWAE18ANVCCt+VMX3oVwtnkh027DurjDI5yVCZxt7NE35gqbk6Odr7yDu9zTSWf200rTcyDAQdOp36/Nny/1n35K9e11wiqOzxMoDbn2+HX+UIWqVevSaT111509AgXAtH";
    // ssh-key's own 1024-bit DSA test key (OpenSSH 10 can no longer make one).
    const KEY_DSA: &str = "ssh-dss AAAAB3NzaC1kc3MAAACBANw9iSUO2UYhFMssjUgW46URqv8bBrDgHeF8HLBOWBvKuXF2Rx2J/XyhgX48SOLMuv0hcPaejlyLarabnF9F2V4dkpPpZSJ+7luHmxEjNxwhsdtg8UteXAWkeCzrQ6MvRJZHcDBjYh56KGvslbFnJsGLXlI4PQCyl6awNImwYGilAAAAFQCJGBU3hZf+QtP9Jh/nbfNlhFu7hwAAAIBHObOQioQVRm3HsVb7mOy3FVKhcLoLO3qoG9gTkd4KeuehtFAC3+rckiX7xSCnE/5BBKdL7VP9WRXac2Nlr9Pwl3e7zPut96wrCHt/TZX6vkfXKkbpUIj5zSqfvyNrWKaYJkfzwAQwrXNS1Hol676Ud/DDEn2oatdEhkS3beWHXAAAAIBgQqaz/YYTRMshzMzYcZ4lqgvgmA55y6v0h39e8HH2A5dwNS6sPUw2jyna+le0dceNRJifFld1J+WYM0vmquSr11DDavgEidOSaXwfMvPPPJqLmbzdtT16N+Gij9U9STQTHPQcQ3xnNNHgQAStzZJbhLOVbDDDo5BO7LMUALDfSA==";

    fn pubkey(s: &str) -> PublicKey {
        PublicKey::from_openssh(s).unwrap()
    }

    fn write_known_hosts(dir: &Path, name: &str, lines: &[&str]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, lines.join("\n")).unwrap();
        path
    }

    fn info(host: &str) -> HostKeyInfo {
        host_key_info(host, 22, &pubkey(KEY_A))
    }

    #[test]
    fn errors_display_with_the_ssh_tunnel_prefix() {
        let cases = [
            TunnelError::Connect("refused".into()),
            TunnelError::Auth("rejected".into()),
            TunnelError::NeedsPassphrase("key k is encrypted".into()),
            TunnelError::Forward("bind failed".into()),
            TunnelError::HostKeyUnknown(info("host")),
            TunnelError::HostKeyChanged(info("host")),
            TunnelError::HostKeyRevoked(info("host")),
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
        assert!(TunnelError::HostKeyRevoked(info("host"))
            .to_string()
            .contains("@revoked"));
        // The MITM case shouts, and both host-key errors show the fingerprint.
        assert!(TunnelError::HostKeyChanged(info("host"))
            .to_string()
            .contains("HOST KEY CHANGED"));
        assert!(TunnelError::HostKeyUnknown(info("host"))
            .to_string()
            .contains("SHA256:"));
    }

    #[test]
    fn host_key_info_carries_fingerprint_type_and_serialized_key() {
        let got = host_key_info("db.internal", 2222, &pubkey(KEY_A));
        assert_eq!(got.host, "db.internal");
        assert_eq!(got.port, 2222);
        assert_eq!(got.key_type, "ssh-ed25519");
        assert!(got.fingerprint.starts_with("SHA256:"));
        // The serialized key round-trips back to the same public key.
        assert_eq!(pubkey(&got.key_openssh), pubkey(KEY_A));
    }

    #[test]
    fn unknown_when_no_file_mentions_the_host() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("known_hosts"); // never created
        let empty = write_known_hosts(dir.path(), "empty", &[]);
        assert_eq!(
            verify_host_key("example.com", 22, &pubkey(KEY_A), &[missing, empty]),
            HostKeyStatus::Unknown
        );
    }

    #[test]
    fn trusted_when_a_file_records_the_same_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_known_hosts(
            dir.path(),
            "known_hosts",
            &[&format!("example.com {KEY_A}")],
        );
        assert_eq!(
            verify_host_key("example.com", 22, &pubkey(KEY_A), &[path]),
            HostKeyStatus::Trusted
        );
    }

    #[test]
    fn changed_when_a_file_records_a_different_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_known_hosts(
            dir.path(),
            "known_hosts",
            &[&format!("example.com {KEY_A}")],
        );
        // Server now offers KEY_B for a host pinned to KEY_A.
        assert_eq!(
            verify_host_key("example.com", 22, &pubkey(KEY_B), &[path]),
            HostKeyStatus::Changed
        );
    }

    #[test]
    fn the_verdict_reads_a_key_of_any_other_type_as_changed() {
        let a = pubkey(KEY_A);
        let cases = [
            (KEY_A, vec![], HostKeyStatus::Unknown),
            (KEY_A, vec![KEY_A], HostKeyStatus::Trusted),
            (KEY_B, vec![KEY_A], HostKeyStatus::Changed),
            // A different type is not a different host: a server can
            // negotiate the recorded type and present another.
            (KEY_ECDSA, vec![KEY_A], HostKeyStatus::Changed),
            (KEY_RSA, vec![KEY_A], HostKeyStatus::Changed),
            // A host may hold several keys; any match is enough.
            (KEY_ECDSA, vec![KEY_A, KEY_ECDSA], HostKeyStatus::Trusted),
        ];
        for (offered, recorded, expected) in cases {
            let recorded: Vec<PublicKey> = recorded.into_iter().map(pubkey).collect();
            assert_eq!(
                host_key_verdict(&pubkey(offered), &recorded, &[]),
                expected,
                "{offered} against {recorded:?}"
            );
        }
        // The comment is not part of the key.
        let commented = pubkey(&format!("{KEY_A} someone@laptop"));
        assert_eq!(
            host_key_verdict(&commented, &[a], &[]),
            HostKeyStatus::Trusted
        );
    }

    #[test]
    fn changed_when_a_file_records_the_host_under_another_key_type() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_known_hosts(
            dir.path(),
            "known_hosts",
            &[&format!("[db.internal]:2222 {KEY_A}")],
        );
        assert_eq!(
            verify_host_key("db.internal", 2222, &pubkey(KEY_ECDSA), &[path]),
            HostKeyStatus::Changed
        );
    }

    #[test]
    fn a_stale_line_before_the_matching_one_does_not_hide_it() {
        // russh's own check stopped at the first conflicting line, so a host
        // with an old and a new key in one file read as Changed. Any recorded
        // match now trusts, as in OpenSSH.
        let dir = tempfile::tempdir().unwrap();
        let path = write_known_hosts(
            dir.path(),
            "known_hosts",
            &[
                &format!("example.com {KEY_B}"),
                &format!("example.com {KEY_A}"),
            ],
        );
        assert_eq!(
            verify_host_key("example.com", 22, &pubkey(KEY_A), &[path]),
            HostKeyStatus::Trusted
        );
    }

    #[tokio::test]
    async fn the_handler_refuses_a_substituted_key_type_as_changed() {
        use russh::client::Handler as _;

        // The wire from the key russh hands over to the verdict `open` maps to
        // HostKeyChanged: a server recorded under ed25519 that presents ECDSA.
        let dir = tempfile::tempdir().unwrap();
        let path = write_known_hosts(
            dir.path(),
            "known_hosts",
            &[&format!("[db.internal]:2222 {KEY_A}")],
        );
        let mut verifier = HostKeyVerifier {
            host: "db.internal".into(),
            port: 2222,
            files: vec![path],
            verdict: Arc::default(),
            offered_key: Arc::default(),
            _alive: watch::channel(()).0,
        };
        let offered = PublicKeyOrCertificate::from(pubkey(KEY_ECDSA));
        assert!(!verifier.check_server_key(&offered).await.unwrap());
        assert_eq!(
            *verifier.verdict.lock().unwrap(),
            Some(HostKeyStatus::Changed)
        );
        assert_eq!(
            verifier.offered_key.lock().unwrap().as_ref(),
            Some(&pubkey(KEY_ECDSA))
        );

        // A host with no record is refused too, as Unknown: the trust prompt,
        // never a silent accept.
        verifier.host = "new.internal".into();
        assert!(!verifier.check_server_key(&offered).await.unwrap());
        assert_eq!(
            *verifier.verdict.lock().unwrap(),
            Some(HostKeyStatus::Unknown)
        );
    }

    #[test]
    fn a_recorded_rsa_key_pins_every_algorithm_it_can_be_presented_under() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_known_hosts(
            dir.path(),
            "known_hosts",
            &[&format!("example.com {KEY_RSA}")],
        );
        // What `open` hands russh, not just what the helper computes.
        let config = client_config("example.com", 22, &[path]);
        let names: Vec<&str> = config.preferred.key.iter().map(|a| a.as_str()).collect();
        // OpenSSH 8.8+ offers only the first two by default; `ssh-rsa` alone
        // never matched.
        assert_eq!(names, ["rsa-sha2-512", "rsa-sha2-256", "ssh-rsa"]);
        // Nothing recorded leaves russh's defaults alone.
        let defaults = client::Config::default().preferred.key;
        assert_eq!(
            client_config("absent.example", 22, &[]).preferred.key,
            defaults
        );
    }

    #[test]
    fn every_readable_file_counts_and_a_bad_line_erases_only_itself() {
        let dir = tempfile::tempdir().unwrap();
        // hubro's own store comes first in `default_known_hosts_read`, so its
        // record must count. In the second file an undecodable line for the
        // host must cost only itself (russh's reader failed the whole file on
        // it, dropping every other key recorded there), and an unreadable path
        // contributes nothing.
        let first = write_known_hosts(dir.path(), "app", &[&format!("example.com {KEY_A}")]);
        let second = write_known_hosts(
            dir.path(),
            "user",
            &[
                "example.com ssh-ed25519 AAAA!!notbase64",
                &format!("example.com {KEY_B}"),
            ],
        );
        let unreadable = dir.path().to_path_buf(); // a directory
        let files = [first, second, unreadable];
        for (offered, expected) in [
            (KEY_ECDSA, HostKeyStatus::Changed),
            (KEY_A, HostKeyStatus::Trusted),
            (KEY_B, HostKeyStatus::Trusted),
        ] {
            assert_eq!(
                verify_host_key("example.com", 22, &pubkey(offered), &files),
                expected,
                "{offered}"
            );
        }
    }

    #[test]
    fn a_matching_file_wins_over_a_mismatch_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        // The app store still pins the old key; the user's file has the new one.
        let stale = write_known_hosts(dir.path(), "app", &[&format!("example.com {KEY_B}")]);
        let good = write_known_hosts(dir.path(), "user", &[&format!("example.com {KEY_A}")]);
        assert_eq!(
            verify_host_key("example.com", 22, &pubkey(KEY_A), &[stale, good]),
            HostKeyStatus::Trusted
        );
    }

    #[test]
    fn recorded_algorithms_collects_pinned_types_across_files() {
        let dir = tempfile::tempdir().unwrap();
        let a = write_known_hosts(dir.path(), "app", &[&format!("example.com {KEY_A}")]);
        let b = write_known_hosts(dir.path(), "user", &[&format!("example.com {KEY_B}")]);
        // Two ed25519 entries collapse to a single distinct algorithm.
        let algs = recorded_key_algorithms("example.com", 22, &[a, b]);
        assert_eq!(algs, vec![pubkey(KEY_A).algorithm()]);
        // A host with no recorded key pins nothing (falls back to defaults).
        assert!(recorded_key_algorithms("absent.example", 22, &[]).is_empty());
    }

    #[test]
    fn learning_a_key_makes_a_later_check_trust_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_hosts");
        // First contact: unknown. After learning: trusted — including on a
        // non-standard port, which is recorded as `[host]:port`.
        assert_eq!(
            verify_host_key(
                "db.internal",
                2222,
                &pubkey(KEY_A),
                std::slice::from_ref(&path)
            ),
            HostKeyStatus::Unknown
        );
        learn_known_hosts_path("db.internal", 2222, &pubkey(KEY_A), &path).unwrap();
        assert_eq!(
            verify_host_key("db.internal", 2222, &pubkey(KEY_A), &[path]),
            HostKeyStatus::Trusted
        );
    }

    #[tokio::test]
    async fn a_host_certificate_is_refused_whatever_known_hosts_says_of_its_key() {
        use russh::client::Handler as _;
        use russh::keys::ssh_key::certificate::{Builder, CertType};
        use russh::keys::ssh_key::private::Ed25519Keypair;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_hosts");
        learn_known_hosts_path("db.internal", 22, &pubkey(KEY_A), &path).unwrap();
        let verifier = |host: &str| HostKeyVerifier {
            host: host.into(),
            port: 22,
            files: vec![path.clone()],
            verdict: Arc::default(),
            offered_key: Arc::default(),
            _alive: watch::channel(()).0,
        };

        // Control: the recorded key, offered bare, is trusted.
        let bare = PublicKeyOrCertificate::from(pubkey(KEY_A));
        assert!(verifier("db.internal")
            .check_server_key(&bare)
            .await
            .unwrap());

        // A CA-signed host certificate is refused whatever known_hosts says
        // about the key inside it, and leaves no verdict behind: an Unknown one
        // would become a trust prompt for a key nobody vouched for.
        let ca = PrivateKey::from(Ed25519Keypair::from_seed(&[7; 32]));
        for (host, key, inner, status) in [
            ("db.internal", KEY_A, "trusted", HostKeyStatus::Trusted),
            ("db.internal", KEY_B, "changed", HostKeyStatus::Changed),
            ("new.internal", KEY_A, "unknown", HostKeyStatus::Unknown),
        ] {
            // The row really is what it says, offered bare.
            let files = std::slice::from_ref(&path);
            assert_eq!(verify_host_key(host, 22, &pubkey(key), files), status);
            let mut builder =
                Builder::new([0; 16], pubkey(key).key_data().clone(), 0, u64::MAX).unwrap();
            builder.cert_type(CertType::Host).unwrap();
            builder.valid_principal(host).unwrap();
            let certificate = PublicKeyOrCertificate::from(builder.sign(&ca).unwrap());
            let mut refusing = verifier(host);
            assert!(
                !refusing.check_server_key(&certificate).await.unwrap(),
                "accepted a certificate wrapping a {inner} key"
            );
            assert_eq!(*refusing.verdict.lock().unwrap(), None, "{inner} key");
            assert!(
                refusing.offered_key.lock().unwrap().is_none(),
                "{inner} key"
            );
        }
    }

    #[test]
    fn verdicts_match_what_openssh_decides() {
        // Each row's expected verdict is what the real OpenSSH 10.5 client
        // decided for the same layout with StrictHostKeyChecking=yes (measured
        // against a test sshd on [localhost]:2222, mirrored here onto port 22).
        // KEY_A is the key the server presents; KEY_B stands for a recorded
        // key that is not it.
        const H: &str = "db.example.com";
        const GARBLED: &str = "db.example.com ssh-ed25519 AAAA!!notbase64";
        // `ssh-keygen -H` over a `db.example.com` line.
        const HASHED: &str = "|1|tbLEo3oZNezhlFk5483qNgLrsh8=|l9n0k6ONOAQcZH3VHlomE0dSN2E=";
        // `ssh-keygen -H` over a `[db.example.com]:2222` line.
        const HASHED_2222: &str = "|1|WwCpWp49FwaNIJF7d1cKy5eQxGk=|2rA7XH6xTom2guhR5+kXEWIE87U=";
        let a_b64 = KEY_A.split_once(' ').unwrap().1;
        let rsa_b64 = KEY_RSA.split_once(' ').unwrap().1;
        let rows: Vec<(&str, Vec<u8>, u16, &str, HostKeyStatus)> = vec![
            (
                "garbled line beside a record of another key",
                format!("{H} {KEY_B}\n{GARBLED}\n").into(),
                22,
                KEY_A,
                HostKeyStatus::Changed,
            ),
            (
                "garbled line beside the key's own record",
                format!("{H} {KEY_A}\n{GARBLED}\n").into(),
                22,
                KEY_A,
                HostKeyStatus::Trusted,
            ),
            (
                "only a garbled line for the host",
                format!("{GARBLED}\n").into(),
                22,
                KEY_A,
                HostKeyStatus::Unknown,
            ),
            (
                "@revoked alone",
                format!("@revoked {H} {KEY_A}\n").into(),
                22,
                KEY_A,
                HostKeyStatus::Revoked,
            ),
            (
                "@revoked beside a plain listing",
                format!("{H} {KEY_A}\n@revoked * {KEY_A}\n").into(),
                22,
                KEY_A,
                HostKeyStatus::Revoked,
            ),
            (
                "non-UTF-8 byte elsewhere",
                [
                    format!("{H} {KEY_B}\nother.host {KEY_B} caf").as_bytes(),
                    b"\xe9\n",
                ]
                .concat(),
                22,
                KEY_A,
                HostKeyStatus::Changed,
            ),
            (
                "wildcard recording another key",
                format!("*.example.com {KEY_B}\n").into(),
                22,
                KEY_A,
                HostKeyStatus::Changed,
            ),
            (
                "wildcard recording the key",
                format!("*.example.com {KEY_A}\n").into(),
                22,
                KEY_A,
                HostKeyStatus::Trusted,
            ),
            (
                "wildcard negating this host",
                format!("*.example.com,!{H} {KEY_A}\n").into(),
                22,
                KEY_A,
                HostKeyStatus::Unknown,
            ),
            (
                "uppercase pattern",
                format!("DB.Example.COM {KEY_A}\n").into(),
                22,
                KEY_A,
                HostKeyStatus::Trusted,
            ),
            (
                "tab-separated fields",
                format!("{H}\t{}\n", KEY_B.replace(' ', "\t")).into(),
                22,
                KEY_A,
                HostKeyStatus::Changed,
            ),
            (
                "@cert-authority only",
                format!("@cert-authority {H} {KEY_A}\n").into(),
                22,
                KEY_A,
                HostKeyStatus::Unknown,
            ),
            (
                "hashed entry recording the key",
                format!("{HASHED} {KEY_A}\n").into(),
                22,
                KEY_A,
                HostKeyStatus::Trusted,
            ),
            (
                "hashed entry recording another key",
                format!("{HASHED} {KEY_A}\n").into(),
                22,
                KEY_B,
                HostKeyStatus::Changed,
            ),
            (
                "type field disagreeing with its key",
                format!("{H} ssh-rsa {a_b64}\n").into(),
                22,
                KEY_A,
                HostKeyStatus::Unknown,
            ),
            (
                "type mismatch beside a record of another key",
                format!("{H} {KEY_B}\n{H} ssh-rsa {a_b64}\n").into(),
                22,
                KEY_A,
                HostKeyStatus::Changed,
            ),
            (
                "hashed entry off port 22",
                format!("{HASHED_2222} {KEY_A}\n").into(),
                2222,
                KEY_A,
                HostKeyStatus::Trusted,
            ),
            (
                "hashed entry off port 22, another key",
                format!("{HASHED_2222} {KEY_A}\n").into(),
                2222,
                KEY_B,
                HostKeyStatus::Changed,
            ),
            (
                "plain entry for port 22 looked up off it",
                format!("{H} {KEY_A}\n").into(),
                2222,
                KEY_A,
                HostKeyStatus::Unknown,
            ),
            (
                "RSA line typed rsa-sha2-256",
                format!("{H} rsa-sha2-256 {rsa_b64}\n").into(),
                22,
                KEY_RSA,
                HostKeyStatus::Trusted,
            ),
            (
                "RSA line typed rsa-sha2-512, another key",
                format!("{H} rsa-sha2-512 {rsa_b64}\n").into(),
                22,
                KEY_A,
                HostKeyStatus::Changed,
            ),
            (
                "? glob",
                format!("db.example.co? {KEY_A}\n").into(),
                22,
                KEY_A,
                HostKeyStatus::Trusted,
            ),
            (
                "comma list",
                format!("other.host,{H} {KEY_A}\n").into(),
                22,
                KEY_A,
                HostKeyStatus::Trusted,
            ),
            (
                "unknown marker",
                format!("@foo {H} {KEY_A}\n").into(),
                22,
                KEY_A,
                HostKeyStatus::Unknown,
            ),
            // hubro's choice, not a measurement: OpenSSH 10 has no DSA and reads Unknown.
            (
                "DSA-only record, another key",
                format!("{H} {KEY_DSA}\n").into(),
                22,
                KEY_A,
                HostKeyStatus::Changed,
            ),
        ];
        let dir = tempfile::tempdir().unwrap();
        for (i, (label, contents, port, offered, expected)) in rows.into_iter().enumerate() {
            let path = dir.path().join(format!("known_hosts_{i}"));
            std::fs::write(&path, contents).unwrap();
            assert_eq!(
                verify_host_key(H, port, &pubkey(offered), &[path]),
                expected,
                "{label}"
            );
        }
        // The host as the user typed it is folded too, hashed entries included.
        let path = dir.path().join("known_hosts_upper");
        std::fs::write(&path, format!("{HASHED} {KEY_A}\n")).unwrap();
        assert_eq!(
            verify_host_key("DB.Example.COM", 22, &pubkey(KEY_A), &[path]),
            HostKeyStatus::Trusted
        );
    }

    #[test]
    fn only_a_first_contact_key_can_be_offered_for_trust() {
        let cases = [
            (TunnelError::Connect("x".into()), false),
            (TunnelError::Auth("x".into()), false),
            (TunnelError::NeedsPassphrase("x".into()), false),
            (TunnelError::Forward("x".into()), false),
            (TunnelError::HostKeyUnknown(info("host")), true),
            (TunnelError::HostKeyChanged(info("host")), false),
            (TunnelError::HostKeyRevoked(info("host")), false),
        ];
        for (err, prompts) in cases {
            let shown = err.to_string();
            match err.into_trust_prompt() {
                Ok(got) => {
                    assert!(prompts, "offered trust for: {shown}");
                    assert_eq!(got, info("host"));
                }
                Err(back) => {
                    assert!(!prompts, "refused a first contact: {shown}");
                    assert_eq!(back.to_string(), shown, "the refusal changed");
                }
            }
        }
    }

    #[tokio::test]
    async fn the_handler_refuses_a_revoked_key_as_revoked() {
        use russh::client::Handler as _;

        let dir = tempfile::tempdir().unwrap();
        let path = write_known_hosts(
            dir.path(),
            "known_hosts",
            &[
                &format!("[db.internal]:2222 {KEY_A}"),
                &format!("@revoked * {KEY_A}"),
            ],
        );
        let mut verifier = HostKeyVerifier {
            host: "db.internal".into(),
            port: 2222,
            files: vec![path],
            verdict: Arc::default(),
            offered_key: Arc::default(),
            _alive: watch::channel(()).0,
        };
        let offered = PublicKeyOrCertificate::from(pubkey(KEY_A));
        assert!(!verifier.check_server_key(&offered).await.unwrap());
        assert_eq!(
            *verifier.verdict.lock().unwrap(),
            Some(HostKeyStatus::Revoked)
        );
    }

    #[test]
    fn revoked_keys_are_not_pinned_for_negotiation() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_known_hosts(
            dir.path(),
            "known_hosts",
            &[&format!("@revoked example.com {KEY_RSA}")],
        );
        assert!(recorded_key_algorithms("example.com", 22, &[path]).is_empty());
    }

    #[test]
    fn tunnel_config_defaults_the_port() {
        let parsed: TunnelConfig =
            toml::from_str("host = \"bastion\"\nuser = \"deploy\"\n\n[auth]\nmethod = \"agent\"\n")
                .unwrap();
        assert_eq!(parsed.port, 22);
        assert_eq!(parsed.auth, TunnelAuth::Agent);
    }

    /// The Windows agent transport, end to end over a real named pipe: when
    /// the OpenSSH agent service isn't running (its pipe name is free), host
    /// a mock agent on that exact name and drive the production connect path
    /// against it; when the real agent is running, just list its identities.
    /// Either way `connect_windows_agent` + `request_identities` must
    /// round-trip the ssh-agent protocol over the pipe.
    #[cfg(windows)]
    #[tokio::test]
    async fn windows_agent_pipe_round_trips() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::windows::named_pipe::ServerOptions;

        // When creation fails, the real agent service owns the name — the
        // test then runs against the real agent instead of the mock.
        let mock_server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(WINDOWS_AGENT_PIPE)
            .ok();

        let server_task = mock_server.map(|mut server| {
            tokio::spawn(async move {
                server.connect().await.unwrap();
                // SSH_AGENTC_REQUEST_IDENTITIES: uint32 len=1, byte 11.
                let mut request = [0u8; 5];
                server.read_exact(&mut request).await.unwrap();
                assert_eq!(request, [0, 0, 0, 1, 11]);
                // SSH_AGENT_IDENTITIES_ANSWER: len=5, byte 12, nkeys=0.
                server
                    .write_all(&[0, 0, 0, 5, 12, 0, 0, 0, 0])
                    .await
                    .unwrap();
            })
        });

        let mut agent = connect_windows_agent().await.expect("pipe connect");
        let identities = agent.request_identities().await.expect("identities");
        if let Some(task) = server_task {
            task.await.unwrap();
            assert!(identities.is_empty());
        }
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

    #[tokio::test]
    async fn an_ssh_server_that_never_answers_is_given_up_on_at_the_bound() {
        // Accepted by the kernel, never by anyone who speaks SSH: measured,
        // russh waits for the banner for good. Agent auth, so no key file is
        // read before the connect.
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let config = TunnelConfig {
            host: "127.0.0.1".into(),
            port: listener.local_addr().unwrap().port(),
            user: "nobody".into(),
            auth: TunnelAuth::Agent,
        };
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            Tunnel::open_within(
                config,
                None,
                "db".into(),
                5432,
                &[],
                Duration::from_millis(300),
            ),
        )
        .await
        .expect("the connect must give up at its bound");
        let Err(TunnelError::Connect(message)) = result else {
            panic!("expected a connect error, got {result:?}");
        };
        assert!(message.ends_with("no answer within 300ms"), "{message}");
    }

    /// Runs [`accept_loop`] on a fresh listener whose channel opens are made
    /// by `open`, returning the port, the shutdown sender and the loop's task.
    async fn run_accept_loop<O, F>(
        open: O,
    ) -> (u16, oneshot::Sender<()>, tokio::task::JoinHandle<()>)
    where
        O: Fn(u16) -> F + Send + 'static,
        F: Future<Output = Result<tokio::io::DuplexStream, ()>> + Send + 'static,
    {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (shutdown_tx, shutdown) = oneshot::channel();
        let task = tokio::spawn(accept_loop(listener, open, shutdown));
        (port, shutdown_tx, task)
    }

    /// Whether a connect to the port is refused, i.e. the listener is gone.
    async fn refused(port: u16) -> bool {
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err()
    }

    #[tokio::test]
    async fn a_channel_that_never_opens_holds_up_neither_the_next_nor_shutdown() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        // The first open never completes — a target that drops SYNs keeps
        // OpenSSH's channel open pending for minutes. Every later one opens
        // at once onto a stream that greets its client.
        let opens = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let open = {
            let opens = Arc::clone(&opens);
            move |_peer: u16| {
                let first = opens.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
                async move {
                    if first {
                        std::future::pending::<()>().await;
                    }
                    let (mut far, near) = tokio::io::duplex(64);
                    far.write_all(b"hi").await.unwrap();
                    // Keep the far end open for as long as the forward runs.
                    tokio::spawn(async move {
                        let mut sink = [0u8; 1];
                        let _ = far.read(&mut sink).await;
                    });
                    Ok(near)
                }
            }
        };
        let (port, shutdown, task) = run_accept_loop(open).await;
        let _stuck = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let mut next = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let mut greeting = [0u8; 2];
        tokio::time::timeout(Duration::from_secs(5), next.read_exact(&mut greeting))
            .await
            .expect("the second client must not wait behind the first one's channel")
            .unwrap();
        assert_eq!(&greeting, b"hi");

        shutdown.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("shutdown must not wait behind a pending channel")
            .unwrap();
        assert!(refused(port).await, "the port must close with the tunnel");
    }

    #[test]
    fn sessions_send_keepalives() {
        let config = client_config("h", 22, &[]);
        assert_eq!(config.keepalive_interval, Some(KEEPALIVE_INTERVAL));
        assert_ne!(
            config.keepalive_max, 0,
            "0 would never end a silent session"
        );
    }
}
