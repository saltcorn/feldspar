//! An SFTP connection for an automated backup: log in with a password, then
//! put, list and delete files in one directory.
//!
//! **The host key is trusted on first use, at save.** Saving a schedule
//! connects (to check the login and that the directory can be written) and
//! records the server's host key fingerprint with the schedule; every run after
//! that refuses a server presenting a different key, which is what stops a
//! password from being handed to whoever answers on that address. The admin
//! sees the fingerprint in the list and can compare it with the server's. A
//! server whose key was deliberately changed is accepted again by saving the
//! schedule.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::client;
use russh::keys::{HashAlg, PublicKeyOrCertificate};
use russh_sftp::client::SftpSession;
use sc_error::{Error, Result};
use tokio::io::AsyncWriteExt;

/// How long connecting and logging in may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long one SFTP request may wait for its answer (a write of one packet,
/// a listing), in seconds.
const REQUEST_TIMEOUT_SECS: u64 = 120;

/// Where to log in, and which host key to expect.
pub struct SftpTarget<'a> {
    pub host: &'a str,
    pub port: u16,
    pub username: &'a str,
    pub password: &'a str,
    /// The fingerprint recorded at save (`SHA256:…`), or `None` to accept
    /// whatever the server presents and report it.
    pub host_key: Option<&'a str>,
}

/// A logged-in SFTP session.
pub struct SftpConnection {
    sftp: SftpSession,
    /// Kept so the SSH connection outlives the session riding on it.
    _ssh: client::Handle<HostKeyCheck>,
    /// The fingerprint the server presented.
    pub host_key: String,
}

/// The client half of the handshake: check the server's key against the one
/// expected, and remember what it was either way.
struct HostKeyCheck {
    expected: Option<String>,
    seen: Arc<Mutex<Option<String>>>,
}

impl client::Handler for HostKeyCheck {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> std::result::Result<bool, Self::Error> {
        let fingerprint = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.fingerprint(HashAlg::Sha256),
            PublicKeyOrCertificate::Certificate(cert) => {
                cert.public_key().fingerprint(HashAlg::Sha256)
            }
        }
        .to_string();
        let accepted = self.expected.as_ref().is_none_or(|e| *e == fingerprint);
        *self.seen.lock().unwrap_or_else(|e| e.into_inner()) = Some(fingerprint);
        Ok(accepted)
    }
}

impl SftpConnection {
    /// Connect, check the host key and log in.
    pub async fn connect(target: &SftpTarget<'_>) -> Result<SftpConnection> {
        let place = format!("{}:{}", target.host, target.port);
        let seen = Arc::new(Mutex::new(None));
        let handler = HostKeyCheck {
            expected: target.host_key.map(str::to_owned),
            seen: Arc::clone(&seen),
        };
        let config = Arc::new(client::Config {
            inactivity_timeout: Some(Duration::from_secs(REQUEST_TIMEOUT_SECS)),
            ..Default::default()
        });
        let connected = tokio::time::timeout(
            CONNECT_TIMEOUT,
            client::connect(config, (target.host, target.port), handler),
        )
        .await
        .map_err(|_| {
            Error::file(format!(
                "connecting to {place}: no answer within {} seconds",
                CONNECT_TIMEOUT.as_secs()
            ))
        })?;
        let seen_key = || seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let mut ssh = match connected {
            Ok(ssh) => ssh,
            Err(russh::Error::UnknownKey) => {
                return Err(Error::file(format!(
                    "the SSH host key of {place} has changed: it was {}, it is now {}. \
                     If the server's key was changed on purpose, open this automated backup \
                     and save it to accept the new key",
                    target.host_key.unwrap_or("unknown"),
                    seen_key().as_deref().unwrap_or("unknown"),
                )));
            }
            Err(e) => {
                return Err(Error::file(format!("connecting to {place}: {e}")));
            }
        };
        let host_key = seen_key().unwrap_or_default();

        let auth = ssh
            .authenticate_password(target.username, target.password)
            .await
            .map_err(|e| Error::file(format!("logging in to {place}: {e}")))?;
        if !auth.success() {
            return Err(Error::file(format!(
                "{place} refused the user name or password for `{}`",
                target.username
            )));
        }
        let channel = ssh
            .channel_open_session()
            .await
            .map_err(|e| Error::file(format!("opening an SFTP session on {place}: {e}")))?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|e| Error::file(format!("starting SFTP on {place}: {e}")))?;
        let sftp = SftpSession::new(channel.into_stream())
            .await
            .map_err(|e| Error::file(format!("starting SFTP on {place}: {e}")))?;
        sftp.set_timeout(REQUEST_TIMEOUT_SECS);
        Ok(SftpConnection {
            sftp,
            _ssh: ssh,
            host_key,
        })
    }

    /// Create `dir` and any parent it lacks, as `mkdir -p` would. An empty
    /// `dir` is the login's own directory, which is there already.
    pub async fn create_dir_all(&self, dir: &str) -> Result<()> {
        if dir.is_empty() {
            return Ok(());
        }
        let mut path = if dir.starts_with('/') {
            String::from("/")
        } else {
            String::new()
        };
        for part in dir.split('/').filter(|p| !p.is_empty()) {
            if !path.is_empty() && !path.ends_with('/') {
                path.push('/');
            }
            path.push_str(part);
            let exists = self
                .sftp
                .try_exists(path.as_str())
                .await
                .map_err(|e| Error::file(format!("checking `{path}`: {e}")))?;
            if !exists {
                self.sftp
                    .create_dir(path.as_str())
                    .await
                    .map_err(|e| Error::file(format!("creating `{path}`: {e}")))?;
            }
        }
        Ok(())
    }

    /// Write `bytes` to `path`, replacing any file there.
    pub async fn write(&self, path: &str, bytes: &[u8]) -> Result<()> {
        let mut file = self
            .sftp
            .create(path)
            .await
            .map_err(|e| Error::file(format!("creating `{path}`: {e}")))?;
        file.write_all(bytes)
            .await
            .map_err(|e| Error::file(format!("writing `{path}`: {e}")))?;
        file.shutdown()
            .await
            .map_err(|e| Error::file(format!("writing `{path}`: {e}")))?;
        Ok(())
    }

    pub async fn rename(&self, from: &str, to: &str) -> Result<()> {
        self.sftp
            .rename(from, to)
            .await
            .map_err(|e| Error::file(format!("renaming `{from}` to `{to}`: {e}")))
    }

    pub async fn remove(&self, path: &str) -> Result<()> {
        self.sftp
            .remove_file(path)
            .await
            .map_err(|e| Error::file(format!("deleting `{path}`: {e}")))
    }

    /// The names of what is in `dir` other than directories (the login's
    /// directory when `dir` is empty). Not "the regular files": a server that
    /// sends no permissions gives no file type, and a listing that came back
    /// empty for it would mean nothing was ever pruned.
    pub async fn list_files(&self, dir: &str) -> Result<Vec<String>> {
        let listed = if dir.is_empty() { "." } else { dir };
        let entries = self
            .sftp
            .read_dir(listed)
            .await
            .map_err(|e| Error::file(format!("reading `{listed}`: {e}")))?;
        Ok(entries
            .filter(|e| !e.file_type().is_dir())
            .map(|e| e.file_name())
            .collect())
    }

    pub async fn close(self) {
        let _ = self.sftp.close().await;
    }
}
