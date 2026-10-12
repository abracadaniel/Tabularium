//! SSH tunnelling (local port forwarding over a `direct-tcpip` channel) via russh.
//!
//! [`open_tunnel`] connects and authenticates to the SSH server, binds `127.0.0.1:0` and
//! forwards every accepted local connection to `remote_host:remote_port` as seen from the
//! SSH server. The tunnel shuts down when the [`SshTunnel`] is dropped.

#[allow(unused_imports)]
use tabularium_core::{Error, Result, SshConfig};

/// A running local port forward. Connect to `127.0.0.1:local_port` to reach the remote
/// endpoint. Dropping the tunnel stops the listener, all forwarded connections and the
/// SSH session.
pub struct SshTunnel {
    pub local_port: u16,
    /// `host:port` the tunnel forwards to (as resolved by the SSH server).
    pub remote: String,
    #[cfg(feature = "ssh")]
    task: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for SshTunnel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshTunnel").field("local_port", &self.local_port).field("remote", &self.remote).finish()
    }
}

impl Drop for SshTunnel {
    fn drop(&mut self) {
        #[cfg(feature = "ssh")]
        {
            log::debug!("closing SSH tunnel 127.0.0.1:{} -> {}", self.local_port, self.remote);
            self.task.abort();
        }
    }
}

#[cfg(not(feature = "ssh"))]
pub async fn open_tunnel(_ssh: &SshConfig, _remote_host: &str, _remote_port: u16) -> Result<SshTunnel> {
    Err(Error::Unsupported("SSH tunnels are not compiled into this build".into()))
}

/// Open an SSH tunnel to `remote_host:remote_port` through the configured SSH server.
#[cfg(feature = "ssh")]
pub async fn open_tunnel(ssh: &SshConfig, remote_host: &str, remote_port: u16) -> Result<SshTunnel> {
    imp::open_tunnel(ssh, remote_host, remote_port).await
}

#[cfg(feature = "ssh")]
mod imp {
    use std::sync::Arc;
    use std::time::Duration;

    use russh::client::{self, AuthResult, Handle};
    use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKey};
    use tabularium_core::{Error, Result, SshAuth, SshConfig};
    use tokio::net::TcpListener;
    use tokio::task::JoinSet;

    use super::SshTunnel;

    pub(super) struct ClientHandler {
        host: String,
    }

    impl client::Handler for ClientHandler {
        type Error = russh::Error;

        async fn check_server_key(&mut self, server_public_key: &PublicKey) -> Result<bool, Self::Error> {
            // TODO: verify against ~/.ssh/known_hosts (russh::keys::check_known_hosts) and
            // let the UI confirm unknown/changed host keys. For now every key is accepted.
            log::info!(
                "SSH host key for {}: {} {}",
                self.host,
                server_public_key.algorithm(),
                server_public_key.fingerprint(HashAlg::Sha256)
            );
            Ok(true)
        }
    }

    fn ssh_err(context: &str, e: impl std::fmt::Display) -> Error {
        Error::Connection(format!("SSH {context}: {e}"))
    }

    pub(super) async fn open_tunnel(ssh: &SshConfig, remote_host: &str, remote_port: u16) -> Result<SshTunnel> {
        if ssh.host.trim().is_empty() {
            return Err(Error::Config("SSH host is empty".into()));
        }
        let port = if ssh.port == 0 { 22 } else { ssh.port };
        let config = Arc::new(client::Config {
            keepalive_interval: Some(Duration::from_secs(30)),
            keepalive_max: 4,
            inactivity_timeout: None,
            ..Default::default()
        });
        let handler = ClientHandler { host: format!("{}:{port}", ssh.host) };
        let mut handle = tokio::time::timeout(
            Duration::from_secs(30),
            client::connect(config, (ssh.host.as_str(), port), handler),
        )
        .await
        .map_err(|_| Error::Connection(format!("SSH connect to {}:{port} timed out", ssh.host)))?
        .map_err(|e| ssh_err(&format!("connect to {}:{port}", ssh.host), e))?;

        authenticate(&mut handle, ssh).await?;

        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let local_port = listener.local_addr()?.port();
        let remote = format!("{remote_host}:{remote_port}");
        log::info!("SSH tunnel 127.0.0.1:{local_port} -> {}@{}:{port} -> {remote}", ssh.user, ssh.host);

        let handle = Arc::new(handle);
        let remote_host = remote_host.to_string();
        let task = tokio::spawn(async move {
            // Connection tasks live in the JoinSet, so aborting this task (on drop of the
            // tunnel) aborts every forwarded connection and releases the SSH session.
            let mut conns = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (mut socket, peer) = match accepted {
                            Ok(a) => a,
                            Err(e) => {
                                log::warn!("SSH tunnel accept failed: {e}");
                                continue;
                            }
                        };
                        let _ = socket.set_nodelay(true);
                        let handle = handle.clone();
                        let host = remote_host.clone();
                        conns.spawn(async move {
                            let channel = match handle
                                .channel_open_direct_tcpip(
                                    host.clone(),
                                    u32::from(remote_port),
                                    peer.ip().to_string(),
                                    u32::from(peer.port()),
                                )
                                .await
                            {
                                Ok(c) => c,
                                Err(e) => {
                                    log::warn!("SSH direct-tcpip to {host}:{remote_port} failed: {e}");
                                    return;
                                }
                            };
                            let mut stream = channel.into_stream();
                            match tokio::io::copy_bidirectional(&mut socket, &mut stream).await {
                                Ok((up, down)) => log::debug!("SSH tunnel connection closed ({up} B up, {down} B down)"),
                                Err(e) => log::debug!("SSH tunnel connection ended: {e}"),
                            }
                        });
                    }
                    Some(_) = conns.join_next(), if !conns.is_empty() => {}
                }
            }
        });
        Ok(SshTunnel { local_port, remote, task })
    }

    async fn authenticate(handle: &mut Handle<ClientHandler>, ssh: &SshConfig) -> Result<()> {
        let user = ssh.user.clone();
        let result = match &ssh.auth {
            SshAuth::Password(pw) => handle
                .authenticate_password(user.clone(), pw.clone())
                .await
                .map_err(|e| ssh_err("password authentication", e))?,
            SshAuth::Key { path, passphrase } => {
                let path = expand_home(path);
                let key = russh::keys::load_secret_key(&path, passphrase.as_deref().filter(|p| !p.is_empty()))
                    .map_err(|e| ssh_err(&format!("loading key {}", path.display()), e))?;
                let hash = handle
                    .best_supported_rsa_hash()
                    .await
                    .map_err(|e| ssh_err("negotiation", e))?
                    .flatten();
                handle
                    .authenticate_publickey(user.clone(), PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                    .await
                    .map_err(|e| ssh_err("public key authentication", e))?
            }
            SshAuth::Agent => agent_auth(handle, &user).await?,
        };
        match result {
            AuthResult::Success => Ok(()),
            AuthResult::Failure { remaining_methods, .. } => Err(Error::Connection(format!(
                "SSH authentication failed for user {user} (server accepts: {remaining_methods:?})"
            ))),
        }
    }

    fn expand_home(path: &std::path::Path) -> std::path::PathBuf {
        if let Ok(rest) = path.strip_prefix("~") {
            let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
            if let Some(home) = home {
                return std::path::PathBuf::from(home).join(rest);
            }
        }
        path.to_path_buf()
    }

    #[cfg(unix)]
    async fn agent_auth(handle: &mut Handle<ClientHandler>, user: &str) -> Result<AuthResult> {
        let agent = russh::keys::agent::client::AgentClient::connect_env()
            .await
            .map_err(|e| ssh_err("agent (SSH_AUTH_SOCK)", e))?;
        agent_auth_with(handle, user, agent).await
    }

    #[cfg(windows)]
    async fn agent_auth(handle: &mut Handle<ClientHandler>, user: &str) -> Result<AuthResult> {
        // The Windows OpenSSH agent service listens on this named pipe.
        let agent = russh::keys::agent::client::AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent")
            .await
            .map_err(|e| ssh_err("agent (named pipe openssh-ssh-agent)", e))?;
        agent_auth_with(handle, user, agent).await
    }

    #[cfg(not(any(unix, windows)))]
    async fn agent_auth(_handle: &mut Handle<ClientHandler>, _user: &str) -> Result<AuthResult> {
        Err(Error::Unsupported("ssh-agent authentication is not supported on this platform".into()))
    }

    #[cfg(any(unix, windows))]
    async fn agent_auth_with<S>(
        handle: &mut Handle<ClientHandler>,
        user: &str,
        mut agent: russh::keys::agent::client::AgentClient<S>,
    ) -> Result<AuthResult>
    where
        S: russh::keys::agent::client::AgentStream + Send + Unpin + 'static,
    {
        let identities = agent.request_identities().await.map_err(|e| ssh_err("agent", e))?;
        if identities.is_empty() {
            return Err(Error::Connection("SSH agent has no identities".into()));
        }
        let hash = handle.best_supported_rsa_hash().await.map_err(|e| ssh_err("negotiation", e))?.flatten();
        let mut last = None;
        for key in identities {
            let alg = if key.algorithm().is_rsa() { hash } else { None };
            match handle.authenticate_publickey_with(user.to_string(), key, alg, &mut agent).await {
                Ok(AuthResult::Success) => return Ok(AuthResult::Success),
                Ok(other) => last = Some(other),
                Err(e) => log::debug!("SSH agent key rejected: {e}"),
            }
        }
        last.ok_or_else(|| Error::Connection("SSH agent authentication failed".into()))
    }
}

#[cfg(all(test, feature = "ssh"))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unreachable_ssh_server_is_connection_error() {
        // Port 1 on localhost is closed: connect must fail quickly with Error::Connection.
        let cfg = SshConfig {
            enabled: true,
            host: "127.0.0.1".into(),
            port: 1,
            user: "nobody".into(),
            auth: tabularium_core::SshAuth::Password("x".into()),
        };
        let err = open_tunnel(&cfg, "localhost", 5432).await.unwrap_err();
        assert!(matches!(err, Error::Connection(_)), "{err:?}");
    }
}
