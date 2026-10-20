//! SSH local port forwarding for Postgres connections, built on russh.
//!
//! A [`Tunnel`] owns one SSH session and a local TCP listener; every client
//! connection to the listener is forwarded through a `direct-tcpip` channel
//! to the configured target (the database as seen from the SSH server).
//! Dropping the tunnel shuts the forward down and disconnects the session —
//! the UI ties a tunnel's lifetime to its connection tab.

use std::borrow::Cow;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use russh::client;
#[cfg(any(unix, windows))]
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::known_hosts::{known_host_keys_path, learn_known_hosts_path};
use russh::keys::{
    load_secret_key, Algorithm, HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKey,
    PublicKeyOrCertificate,
};
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
        }
    }
}

impl std::error::Error for TunnelError {}

/// Trust status of a server host key relative to the known_hosts files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostKeyStatus {
    /// A matching entry exists — accept silently.
    Trusted,
    /// No readable file records this host — first contact (trust-on-first-use).
    /// A file russh cannot parse is skipped whole, so its records are lost too
    /// (FRE-289).
    Unknown,
    /// A file records this host with a *different* key — possible MITM.
    Changed,
}

/// hubro's own known_hosts store (writable): `~/.config/hubro/known_hosts`.
/// New trust decisions are recorded here; the user's `~/.ssh/known_hosts` is
/// read but never modified.
pub fn app_known_hosts_path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("hubro").join("known_hosts"))
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

/// Classifies `key` for `host:port` against every key `files` record for that
/// host, by [`host_key_verdict`]. Unreadable or unparsable files contribute no
/// opinion: russh fails a whole file on one undecodable line for the host,
/// which FRE-289 is to fix.
fn verify_host_key(host: &str, port: u16, key: &PublicKey, files: &[PathBuf]) -> HostKeyStatus {
    let recorded: Vec<PublicKey> = files
        .iter()
        .filter_map(|path| known_host_keys_path(host, port, path).ok())
        .flatten()
        .map(|(_, recorded)| recorded)
        .collect();
    host_key_verdict(key, &recorded)
}

/// Trusted if any recorded key is the offered one, wherever it sits; Changed if
/// the host is recorded at all but only under other keys; Unknown only when
/// nothing is recorded. This is OpenSSH's core rule (`hostfile.c`), so a key of
/// a different *type* counts as Changed; its `@revoked` markers and wildcard
/// patterns are not replicated (FRE-289). The type case matters because russh
/// does not check the key a server sends
/// against the negotiated algorithm, so a server can agree to the recorded type
/// and present another. Reading that as Unknown turned a man-in-the-middle into
/// a trust prompt for a host the user already trusts (FRE-288, reproduced
/// against a server that negotiated ssh-ed25519 and sent an ECDSA key).
fn host_key_verdict(offered: &PublicKey, recorded: &[PublicKey]) -> HostKeyStatus {
    // Key material only: the comment is not part of the key's identity.
    if recorded.iter().any(|r| r.key_data() == offered.key_data()) {
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
    for path in files {
        if let Ok(keys) = known_host_keys_path(host, port, path) {
            for (_, recorded) in keys {
                for algorithm in presenting_algorithms(recorded.algorithm()) {
                    if !algorithms.contains(&algorithm) {
                        algorithms.push(algorithm);
                    }
                }
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
fn client_config(host: &str, port: u16, known_hosts_read: &[PathBuf]) -> client::Config {
    let mut ssh_config = client::Config::default();
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
/// OpenSSH server fail (FRE-288, measured against 10.3), so RSA gets all three,
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
struct HostKeyVerifier {
    host: String,
    port: u16,
    files: Vec<PathBuf>,
    verdict: Arc<Mutex<Option<HostKeyStatus>>>,
    offered_key: Arc<Mutex<Option<PublicKey>>>,
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
    /// [`TunnelError::HostKeyUnknown`] and a changed one
    /// [`TunnelError::HostKeyChanged`], both before authentication.
    pub async fn open(
        config: TunnelConfig,
        passphrase: Option<String>,
        target_host: String,
        target_port: u16,
        known_hosts_read: &[PathBuf],
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
        let verifier = HostKeyVerifier {
            host: config.host.clone(),
            port: config.port,
            files: known_hosts_read.to_vec(),
            verdict: Arc::clone(&verdict),
            offered_key: Arc::clone(&offered_key),
        };
        let mut handle = match client::connect(ssh_config, address, verifier).await {
            Ok(handle) => handle,
            // If the handshake failed because we rejected the host key, turn
            // the generic connect error into a precise host-key error the UI
            // can act on (trust prompt for Unknown, hard refusal for Changed).
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

/// Accepts local connections and forwards each through its own
/// `direct-tcpip` channel until the shutdown signal fires, then disconnects
/// the SSH session. Dropping the `JoinSet` aborts in-flight copies.
async fn accept_loop(
    handle: client::Handle<HostKeyVerifier>,
    listener: TcpListener,
    target_host: String,
    target_port: u16,
    mut shutdown: oneshot::Receiver<()>,
) {
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

    // Two distinct ed25519 host keys (OpenSSH one-line form).
    const KEY_A: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ";
    const KEY_B: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIA6rWI3G1sz07DnfFlrouTcysQlj2P+jpNSOEWD9OJ3X";
    // Keys of two other types, for a host recorded under ed25519.
    const KEY_ECDSA: &str = "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBCt0yDppBirhQ+3NGimeGgyET84m3sS5pae0/OTdHoborlN0wMG6LJ+jPgmO4EGBUVMGR3RZckSgUOGQOeitvgE=";
    const KEY_RSA: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQCqBoV58WdupNiprCHYGyuv2uVZPMZdP5V7D5aQIJ+jwsF7IPkov1+cfQSz0QUMl3c+DhvCJglpU/Z58Oe5I48Xp1/IJLv8zCG3eySQalqFZumylVLEDlBOpnM4E04Oq8kJXkesca3eTQW2mfSRL0tlNQd8hAfSwjELjld0yXnPH2r0v4bILgxVfmpxQuicF6wVwvDQl7VqS8AOcKwhQqZWAE18ANVCCt+VMX3oVwtnkh027DurjDI5yVCZxt7NE35gqbk6Odr7yDu9zTSWf200rTcyDAQdOp36/Nny/1n35K9e11wiqOzxMoDbn2+HX+UIWqVevSaT111509AgXAtH";

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
            // FRE-288: a different type is not a different host.
            (KEY_ECDSA, vec![KEY_A], HostKeyStatus::Changed),
            (KEY_RSA, vec![KEY_A], HostKeyStatus::Changed),
            // A host may hold several keys; any match is enough.
            (KEY_ECDSA, vec![KEY_A, KEY_ECDSA], HostKeyStatus::Trusted),
        ];
        for (offered, recorded, expected) in cases {
            let recorded: Vec<PublicKey> = recorded.into_iter().map(pubkey).collect();
            assert_eq!(
                host_key_verdict(&pubkey(offered), &recorded),
                expected,
                "{offered} against {recorded:?}"
            );
        }
        // The comment is not part of the key.
        let commented = pubkey(&format!("{KEY_A} someone@laptop"));
        assert_eq!(host_key_verdict(&commented, &[a]), HostKeyStatus::Trusted);
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
    fn every_readable_file_counts_and_an_unparsable_one_erases_only_itself() {
        let dir = tempfile::tempdir().unwrap();
        // hubro's own store comes first in `default_known_hosts_read`, so its
        // record must count; a broken second file must not erase it. The
        // double space makes russh read the key type as the key and fail the
        // whole file.
        let first = write_known_hosts(dir.path(), "app", &[&format!("example.com {KEY_A}")]);
        let broken = write_known_hosts(dir.path(), "user", &[&format!("example.com  {KEY_B}")]);
        assert!(known_host_keys_path("example.com", 22, &broken).is_err());
        let files = [first, broken];
        assert_eq!(
            verify_host_key("example.com", 22, &pubkey(KEY_ECDSA), &files),
            HostKeyStatus::Changed
        );
        assert_eq!(
            verify_host_key("example.com", 22, &pubkey(KEY_A), &files),
            HostKeyStatus::Trusted
        );
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
}
