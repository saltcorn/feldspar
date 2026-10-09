//! Where an automated backup is sent: a directory on this server, a directory
//! on an SFTP server, or an S3-compatible bucket.
//!
//! A [`Destination`] is what the admin chose and what is stored. It is
//! [`check`](Destination::check)ed when the schedule is saved, by doing what a
//! run will do (write a probe file, list, delete it), so a wrong password or a
//! read-only bucket is reported in the dialog rather than by a backup that never
//! arrives. A run [`open`](Destination::open)s a [`Connection`] and puts, lists
//! and deletes through it, whichever kind it is, so the scheduling and the
//! pruning are written once.
//!
//! **Secrets** (the SFTP password, the S3 secret key) are stored as entered and
//! sent to the admin UI as [`SECRET_SENTINEL`]. A save that hands the sentinel
//! back means "unchanged" and keeps what is stored, as an LLM provider's key
//! does.

use std::path::{Component, Path, PathBuf};

use sc_error::{Error, Repr, Result};
use sc_types::SECRET_SENTINEL;
use serde_json::{Map, Value as Json, json};
use uuid::Uuid;

use super::s3::{DEFAULT_REGION, S3Client};
use super::sftp::{SftpConnection, SftpTarget};

/// The port SFTP listens on unless told otherwise.
pub const DEFAULT_SFTP_PORT: u16 = 22;

/// What a backup's file names start with, so an S3 listing can ask for just
/// those (the full pattern is checked by the pruning).
pub const LIST_PREFIX: &str = "feldspar-backup-";

/// Where an automated backup goes.
#[derive(Debug, Clone, PartialEq)]
pub enum Destination {
    /// An absolute directory on this server, without a trailing separator.
    Local {
        directory: String,
    },
    Sftp(SftpDestination),
    S3(S3Destination),
}

#[derive(Debug, Clone, PartialEq)]
pub struct SftpDestination {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    /// Absolute, or relative to the login's directory; empty for the login's
    /// directory itself. No trailing `/`.
    pub directory: String,
    /// The host key fingerprint recorded when the schedule was saved; a run
    /// refuses a server presenting any other. Set by the server, never by the
    /// request.
    pub host_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct S3Destination {
    /// As entered; empty means AWS itself, at the region's endpoint.
    pub endpoint: String,
    pub bucket: String,
    /// As entered; empty means [`DEFAULT_REGION`].
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
}

impl S3Destination {
    fn region(&self) -> &str {
        if self.region.is_empty() {
            DEFAULT_REGION
        } else {
            &self.region
        }
    }

    fn endpoint(&self) -> String {
        if self.endpoint.is_empty() {
            format!("https://s3.{}.amazonaws.com", self.region())
        } else {
            self.endpoint.clone()
        }
    }

    fn client(&self) -> Result<S3Client> {
        S3Client::new(
            &self.endpoint(),
            &self.bucket,
            self.region(),
            &self.access_key,
            &self.secret_key,
        )
    }
}

impl SftpDestination {
    fn target(&self) -> SftpTarget<'_> {
        SftpTarget {
            host: &self.host,
            port: self.port,
            username: &self.username,
            password: &self.password,
            host_key: self.host_key.as_deref(),
        }
    }

    /// `name` in the destination's directory, as SFTP paths it.
    fn path_of(&self, name: &str) -> String {
        if self.directory.is_empty() {
            name.to_owned()
        } else if self.directory == "/" {
            format!("/{name}")
        } else {
            format!("{}/{name}", self.directory)
        }
    }
}

impl Destination {
    /// Read a destination from a request body's `destination` object, checking
    /// everything that can be checked without reaching it. A `host_key` in the
    /// body is ignored: only [`check`](Destination::check) sets one.
    pub fn from_body(value: Option<&Json>) -> Result<Destination> {
        let obj = value.and_then(Json::as_object).ok_or_else(|| {
            Error::invalid("`destination` must say where the backups go, with its `kind`")
        })?;
        Destination::parse(obj, false)
    }

    /// Read a stored destination.
    pub fn from_stored(value: &Json) -> Option<Destination> {
        Destination::parse(value.as_object()?, true).ok()
    }

    fn parse(obj: &Map<String, Json>, keep_host_key: bool) -> Result<Destination> {
        let text = |key: &str| {
            obj.get(key)
                .and_then(Json::as_str)
                .map(str::trim)
                .unwrap_or_default()
                .to_owned()
        };
        let required = |key: &str, what: &str| {
            let value = text(key);
            if value.is_empty() {
                Err(Error::invalid(format!("`{key}`: enter the {what}")))
            } else {
                Ok(value)
            }
        };
        match obj.get("kind").and_then(Json::as_str).unwrap_or_default() {
            "local" => Ok(Destination::Local {
                directory: normalise_directory(&text("directory"))?,
            }),
            "sftp" => {
                let host = required("host", "SFTP server's host name")?;
                if host.contains(['/', ' ', '@']) {
                    return Err(Error::invalid(format!(
                        "`{host}` is not a host name; enter it without a user name, scheme or path"
                    )));
                }
                let port = match obj.get("port") {
                    None | Some(Json::Null) => DEFAULT_SFTP_PORT,
                    Some(value) => value
                        .as_u64()
                        .and_then(|p| u16::try_from(p).ok())
                        .filter(|p| *p > 0)
                        .ok_or_else(|| {
                            Error::invalid("`port` must be a whole number from 1 to 65535")
                        })?,
                };
                Ok(Destination::Sftp(SftpDestination {
                    host,
                    port,
                    username: required("username", "user name")?,
                    // Not trimmed: a password is whatever was typed.
                    password: match obj.get("password").and_then(Json::as_str) {
                        Some(p) if !p.is_empty() => p.to_owned(),
                        _ => return Err(Error::invalid("`password`: enter the password")),
                    },
                    directory: normalise_remote_directory(&text("directory"))?,
                    host_key: if keep_host_key {
                        obj.get("host_key")
                            .and_then(Json::as_str)
                            .map(str::to_owned)
                    } else {
                        None
                    },
                }))
            }
            "s3" => {
                let endpoint = text("endpoint").trim_end_matches('/').to_owned();
                if !endpoint.is_empty() {
                    let url = reqwest::Url::parse(&endpoint).map_err(|_| {
                        Error::invalid(format!(
                            "`{endpoint}` is not an endpoint URL; enter it as https://host[:port]"
                        ))
                    })?;
                    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                        return Err(Error::invalid(format!(
                            "`{endpoint}` is not an endpoint URL; enter it as https://host[:port]"
                        )));
                    }
                }
                let bucket = required("bucket", "bucket name")?;
                if bucket.contains(['/', ' ']) {
                    return Err(Error::invalid(format!(
                        "`{bucket}` is not a bucket name; enter the name alone, without `/`"
                    )));
                }
                Ok(Destination::S3(S3Destination {
                    endpoint,
                    bucket,
                    region: text("region"),
                    access_key: required("access_key", "access key")?,
                    secret_key: match obj.get("secret_key").and_then(Json::as_str) {
                        Some(k) if !k.trim().is_empty() => k.trim().to_owned(),
                        _ => return Err(Error::invalid("`secret_key`: enter the secret key")),
                    },
                }))
            }
            other => Err(Error::invalid(format!(
                "`{other}` is not a kind of backup destination; choose `local`, `sftp` or `s3`"
            ))),
        }
    }

    /// The stored shape, secrets included.
    pub fn to_json(&self) -> Json {
        match self {
            Destination::Local { directory } => json!({ "kind": "local", "directory": directory }),
            Destination::Sftp(d) => json!({
                "kind": "sftp",
                "host": d.host,
                "port": d.port,
                "username": d.username,
                "password": d.password,
                "directory": d.directory,
                "host_key": d.host_key,
            }),
            Destination::S3(d) => json!({
                "kind": "s3",
                "endpoint": d.endpoint,
                "bucket": d.bucket,
                "region": d.region,
                "access_key": d.access_key,
                "secret_key": d.secret_key,
            }),
        }
    }

    /// What the admin UI is sent: the stored shape with the secrets replaced
    /// by [`SECRET_SENTINEL`].
    pub fn redacted_json(&self) -> Json {
        let mut out = self.to_json();
        if let Some(obj) = out.as_object_mut() {
            for key in ["password", "secret_key"] {
                if obj.contains_key(key) {
                    obj.insert(key.to_owned(), json!(SECRET_SENTINEL));
                }
            }
        }
        out
    }

    /// Put back the secrets a save left as [`SECRET_SENTINEL`] — "unchanged" —
    /// from `previous`, the destination being edited. A sentinel with nothing
    /// of that kind stored is refused: saving the mask as the password would
    /// make a schedule that looks configured and can never log in.
    pub fn keep_secrets(mut self, previous: Option<&Destination>) -> Result<Destination> {
        match (&mut self, previous) {
            (Destination::Sftp(d), Some(Destination::Sftp(p))) if d.password == SECRET_SENTINEL => {
                d.password = p.password.clone();
            }
            (Destination::S3(d), Some(Destination::S3(p))) if d.secret_key == SECRET_SENTINEL => {
                d.secret_key = p.secret_key.clone();
            }
            (Destination::Sftp(d), _) if d.password == SECRET_SENTINEL => {
                return Err(Error::invalid("`password`: enter the password"));
            }
            (Destination::S3(d), _) if d.secret_key == SECRET_SENTINEL => {
                return Err(Error::invalid("`secret_key`: enter the secret key"));
            }
            _ => {}
        }
        Ok(self)
    }

    /// Where the backups go, in one line, for the list, the status and error
    /// messages: a path, an `sftp://` address or the bucket's URL.
    pub fn location(&self) -> String {
        match self {
            Destination::Local { directory } => directory.clone(),
            Destination::Sftp(d) => {
                let port = if d.port == DEFAULT_SFTP_PORT {
                    String::new()
                } else {
                    format!(":{}", d.port)
                };
                let dir = if d.directory.is_empty() || d.directory.starts_with('/') {
                    d.directory.clone()
                } else {
                    format!("/~/{}", d.directory)
                };
                format!("sftp://{}@{}{port}{dir}", d.username, d.host)
            }
            Destination::S3(d) => format!("{}/{}", d.endpoint(), d.bucket),
        }
    }

    /// What two schedules must not share: the place the files land, whoever
    /// logs in to put them there. Each schedule's pruning would otherwise
    /// delete the other's backups.
    pub fn place(&self) -> String {
        match self {
            Destination::Local { directory } => directory.clone(),
            Destination::Sftp(d) => format!(
                "sftp://{}:{}/{}",
                d.host.to_ascii_lowercase(),
                d.port,
                d.directory
            ),
            Destination::S3(d) => format!("{}/{}", d.endpoint().to_ascii_lowercase(), d.bucket),
        }
    }

    /// The sentence a clash in [`place`](Destination::place) is refused with.
    pub fn place_taken(&self) -> String {
        match self {
            Destination::S3(d) => format!(
                "another automated backup already writes to the bucket `{}`; each schedule needs a bucket of its own",
                d.bucket
            ),
            _ => format!(
                "another automated backup already writes to `{}`; each schedule needs a directory of its own",
                self.location()
            ),
        }
    }

    /// Make sure a run will work, by doing what it does: create the directory
    /// if it is missing, write a probe file, list, and delete the probe. Done
    /// when the schedule is saved, so a mistake is reported in the dialog.
    ///
    /// For SFTP this is also where the server's host key is learnt and
    /// recorded (see [`super::sftp`]).
    pub async fn check(&mut self) -> Result<()> {
        let probe = format!(".feldspar-backup-probe-{}", Uuid::new_v4());
        let location = self.location();
        // Refused as invalid input, with the transport's own sentence (not
        // its "file error:" prefix) after where it was going.
        let failed = |e: Error| {
            let message = match e.repr() {
                Repr::File(m) | Repr::Invalid(m) => m.clone(),
                _ => e.to_string(),
            };
            Error::invalid(format!("{location}: {message}"))
        };
        match self {
            Destination::Local { directory } => check_local(directory).await,
            Destination::Sftp(d) => {
                d.host_key = None;
                let conn = SftpConnection::connect(&d.target()).await.map_err(failed)?;
                d.host_key = Some(conn.host_key.clone());
                let result = async {
                    conn.create_dir_all(&d.directory).await?;
                    conn.write(&d.path_of(&probe), b"").await?;
                    conn.list_files(&d.directory).await?;
                    conn.remove(&d.path_of(&probe)).await
                }
                .await;
                conn.close().await;
                result.map_err(failed)
            }
            Destination::S3(d) => {
                let client = d.client().map_err(failed)?;
                client.put(&probe, Vec::new()).await.map_err(failed)?;
                let listed = client.list(LIST_PREFIX).await;
                let deleted = client.delete(&probe).await;
                listed.map_err(failed)?;
                deleted.map_err(failed)
            }
        }
    }

    /// Connect, for a run.
    pub async fn open(&self) -> Result<Connection> {
        match self {
            Destination::Local { directory } => {
                let dir = PathBuf::from(directory);
                tokio::fs::create_dir_all(&dir)
                    .await
                    .map_err(|e| Error::file(format!("creating `{}`: {e}", dir.display())))?;
                Ok(Connection::Local(dir))
            }
            Destination::Sftp(d) => {
                let conn = SftpConnection::connect(&d.target()).await?;
                conn.create_dir_all(&d.directory).await?;
                Ok(Connection::Sftp(Box::new(conn), d.clone()))
            }
            Destination::S3(d) => Ok(Connection::S3(d.client()?)),
        }
    }
}

/// A destination, reached: what a run writes, lists and deletes through.
pub enum Connection {
    Local(PathBuf),
    Sftp(Box<SftpConnection>, SftpDestination),
    S3(S3Client),
}

impl Connection {
    /// Store `bytes` as `name`, whole or not at all, and say where it went.
    ///
    /// On a disk and over SFTP that means writing a dot-prefixed `.partial`
    /// file and renaming it into place, so neither a reader nor the pruning
    /// ever sees half a backup. An S3 put is atomic already.
    pub async fn put(&self, name: &str, bytes: Vec<u8>) -> Result<String> {
        let partial = format!(".{name}.partial");
        match self {
            Connection::Local(dir) => {
                let target = dir.join(name);
                let partial = dir.join(partial);
                tokio::fs::write(&partial, &bytes)
                    .await
                    .map_err(|e| Error::file(format!("writing `{}`: {e}", partial.display())))?;
                if let Err(e) = tokio::fs::rename(&partial, &target).await {
                    let _ = tokio::fs::remove_file(&partial).await;
                    return Err(Error::file(format!("writing `{}`: {e}", target.display())));
                }
                Ok(target.to_string_lossy().into_owned())
            }
            Connection::Sftp(conn, d) => {
                let target = d.path_of(name);
                let partial = d.path_of(&partial);
                if let Err(e) = conn.write(&partial, &bytes).await {
                    let _ = conn.remove(&partial).await;
                    return Err(e);
                }
                // SFTP's rename will not replace a file (OpenSSH refuses), and
                // a second run in the same second would want to.
                let _ = conn.remove(&target).await;
                if let Err(e) = conn.rename(&partial, &target).await {
                    let _ = conn.remove(&partial).await;
                    return Err(e);
                }
                Ok(format!(
                    "{}/{name}",
                    Destination::Sftp(d.clone()).location()
                ))
            }
            Connection::S3(client) => {
                client.put(name, bytes).await?;
                Ok(client.url_of(name))
            }
        }
    }

    /// The names of the files there that could be backups (the pruning picks
    /// out the ones that are).
    pub async fn list(&self) -> Result<Vec<String>> {
        match self {
            Connection::Local(dir) => {
                let read =
                    |e: std::io::Error| Error::file(format!("reading `{}`: {e}", dir.display()));
                let mut entries = tokio::fs::read_dir(dir).await.map_err(read)?;
                let mut names = Vec::new();
                while let Some(entry) = entries.next_entry().await.map_err(read)? {
                    if entry.file_type().await.is_ok_and(|t| t.is_file())
                        && let Some(name) = entry.file_name().to_str()
                    {
                        names.push(name.to_owned());
                    }
                }
                Ok(names)
            }
            Connection::Sftp(conn, d) => conn.list_files(&d.directory).await,
            Connection::S3(client) => client.list(LIST_PREFIX).await,
        }
    }

    pub async fn delete(&self, name: &str) -> Result<()> {
        match self {
            Connection::Local(dir) => {
                let path = dir.join(name);
                tokio::fs::remove_file(&path)
                    .await
                    .map_err(|e| Error::file(format!("deleting `{}`: {e}", path.display())))
            }
            Connection::Sftp(conn, d) => conn.remove(&d.path_of(name)).await,
            Connection::S3(client) => client.delete(name).await,
        }
    }

    /// Log out, where there is anything to log out of.
    pub async fn close(self) {
        if let Connection::Sftp(conn, _) = self {
            conn.close().await;
        }
    }
}

/// A local directory as stored: trimmed, absolute, with no `..` and no
/// trailing separator, so `/srv/backups/` and `/srv/backups` are recognised as
/// the same directory.
pub fn normalise_directory(raw: &str) -> Result<String> {
    if raw.is_empty() {
        return Err(Error::invalid(
            "`directory` must name a directory on the server",
        ));
    }
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err(Error::invalid(format!(
            "`{raw}` is not an absolute path; the directory must start from the root of the server's file system"
        )));
    }
    if path.components().any(|c| c == Component::ParentDir) {
        return Err(Error::invalid(format!(
            "`{raw}` contains `..`; give the directory's path without it"
        )));
    }
    let normal: PathBuf = path.components().collect();
    Ok(normal.to_string_lossy().into_owned())
}

/// A directory on an SFTP server: absolute, or relative to where the login
/// starts (empty for that directory itself), with no `..`, no empty or `.`
/// segments and no trailing `/`. SFTP paths are `/`-separated whatever this
/// server runs on, so this is not a [`Path`].
pub fn normalise_remote_directory(raw: &str) -> Result<String> {
    let parts: Vec<&str> = raw
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    if parts.contains(&"..") {
        return Err(Error::invalid(format!(
            "`{raw}` contains `..`; give the directory's path without it"
        )));
    }
    let joined = parts.join("/");
    Ok(if raw.starts_with('/') {
        format!("/{joined}")
    } else {
        joined
    })
}

/// Make sure a local directory is one this server can use, creating it if it
/// does not exist yet.
async fn check_local(directory: &str) -> Result<()> {
    let path = Path::new(directory);
    match tokio::fs::metadata(path).await {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            return Err(Error::invalid(format!(
                "`{directory}` exists and is not a directory"
            )));
        }
        Err(_) => tokio::fs::create_dir_all(path).await.map_err(|e| {
            Error::invalid(format!("could not create the directory `{directory}`: {e}"))
        })?,
    }
    // Writable, by doing it: permission bits do not tell the whole story (a
    // read-only mount, an ACL), and the run would find out the same way.
    let probe = path.join(format!(".feldspar-backup-probe-{}", Uuid::new_v4()));
    tokio::fs::write(&probe, b"")
        .await
        .map_err(|e| Error::invalid(format!("this server cannot write to `{directory}`: {e}")))?;
    let _ = tokio::fs::remove_file(&probe).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(v: Json) -> Result<Destination> {
        Destination::from_body(Some(&v))
    }

    fn sftp() -> Destination {
        body(json!({
            "kind": "sftp", "host": "backup.example.com", "username": "feldspar",
            "password": "hunter2", "directory": "backups/site/"
        }))
        .unwrap()
    }

    fn s3() -> Destination {
        body(json!({
            "kind": "s3", "endpoint": "https://minio.example.com:9000/", "bucket": "site",
            "access_key": "AK", "secret_key": "SK"
        }))
        .unwrap()
    }

    #[test]
    fn a_local_directory_must_be_absolute_and_is_normalised() {
        assert_eq!(
            normalise_directory("/srv/backups/").unwrap(),
            "/srv/backups"
        );
        assert_eq!(
            normalise_directory("/srv//backups").unwrap(),
            "/srv/backups"
        );
        assert!(normalise_directory("backups").is_err());
        assert!(normalise_directory("").is_err());
        assert!(normalise_directory("/srv/../etc").is_err());
    }

    #[test]
    fn a_remote_directory_may_be_relative() {
        assert_eq!(normalise_remote_directory("").unwrap(), "");
        assert_eq!(normalise_remote_directory("a//b/./").unwrap(), "a/b");
        assert_eq!(normalise_remote_directory("/srv/b/").unwrap(), "/srv/b");
        assert_eq!(normalise_remote_directory("/").unwrap(), "/");
        assert!(normalise_remote_directory("a/../b").is_err());
    }

    #[test]
    fn each_kind_reads_its_fields_and_checks_them() {
        let Destination::Sftp(d) = sftp() else {
            panic!()
        };
        assert_eq!(d.port, DEFAULT_SFTP_PORT);
        assert_eq!(d.directory, "backups/site");
        assert_eq!(d.path_of("f.zip"), "backups/site/f.zip");
        let Destination::S3(d) = s3() else { panic!() };
        assert_eq!(d.endpoint, "https://minio.example.com:9000");
        assert_eq!(d.region(), DEFAULT_REGION);

        for bad in [
            json!({ "kind": "ftp" }),
            json!({ "kind": "local", "directory": "relative" }),
            json!({ "kind": "sftp", "host": "h", "username": "u" }),
            json!({ "kind": "sftp", "host": "h", "username": "u", "password": "p", "port": 70000 }),
            json!({ "kind": "sftp", "host": "u@h", "username": "u", "password": "p" }),
            json!({ "kind": "s3", "bucket": "b", "access_key": "a" }),
            json!({ "kind": "s3", "endpoint": "minio:9000", "bucket": "b", "access_key": "a", "secret_key": "s" }),
            json!({ "kind": "s3", "bucket": "b/c", "access_key": "a", "secret_key": "s" }),
        ] {
            assert!(body(bad.clone()).is_err(), "{bad} should be refused");
        }
        assert!(Destination::from_body(None).is_err());
    }

    #[test]
    fn aws_is_the_endpoint_when_none_is_given() {
        let d = body(json!({
            "kind": "s3", "bucket": "b", "region": "eu-west-2", "access_key": "a", "secret_key": "s"
        }))
        .unwrap();
        assert_eq!(d.location(), "https://s3.eu-west-2.amazonaws.com/b");
    }

    #[test]
    fn a_body_cannot_set_the_host_key_but_the_store_keeps_it() {
        let mut v = sftp().to_json();
        v["host_key"] = json!("SHA256:abc");
        let Destination::Sftp(from_body) = Destination::from_body(Some(&v)).unwrap() else {
            panic!()
        };
        assert_eq!(from_body.host_key, None);
        let Destination::Sftp(stored) = Destination::from_stored(&v).unwrap() else {
            panic!()
        };
        assert_eq!(stored.host_key.as_deref(), Some("SHA256:abc"));
    }

    #[test]
    fn secrets_are_masked_and_kept_when_the_mask_comes_back() {
        for (dest, key, secret) in [(sftp(), "password", "hunter2"), (s3(), "secret_key", "SK")] {
            let shown = dest.redacted_json();
            assert_eq!(shown[key], json!(SECRET_SENTINEL));
            assert!(!shown.to_string().contains(secret));

            // Handed back unchanged: the stored secret survives the edit.
            let edited = Destination::from_body(Some(&shown)).unwrap();
            let kept = edited.clone().keep_secrets(Some(&dest)).unwrap();
            assert_eq!(kept.to_json()[key], json!(secret));
            // With nothing of that kind to keep, the mask is refused.
            assert!(edited.clone().keep_secrets(None).is_err());
            assert!(
                edited
                    .keep_secrets(Some(&Destination::Local {
                        directory: "/b".into()
                    }))
                    .is_err()
            );
        }
        // A new secret replaces the stored one.
        let mut v = sftp().to_json();
        v["password"] = json!("new");
        let d = Destination::from_body(Some(&v))
            .unwrap()
            .keep_secrets(Some(&sftp()))
            .unwrap();
        assert_eq!(d.to_json()["password"], json!("new"));
    }

    #[test]
    fn locations_and_places() {
        assert_eq!(
            sftp().location(),
            "sftp://feldspar@backup.example.com/~/backups/site"
        );
        assert_eq!(s3().location(), "https://minio.example.com:9000/site");
        // Another user writing to the same directory is the same place.
        let mut other = sftp().to_json();
        other["username"] = json!("someone-else");
        other["host"] = json!("BACKUP.example.com");
        assert_eq!(
            Destination::from_body(Some(&other)).unwrap().place(),
            sftp().place()
        );
    }

    #[test]
    fn a_stored_destination_round_trips() {
        let mut d = sftp();
        if let Destination::Sftp(s) = &mut d {
            s.host_key = Some("SHA256:xyz".into());
        }
        for d in [
            d,
            s3(),
            Destination::Local {
                directory: "/srv/b".into(),
            },
        ] {
            assert_eq!(Destination::from_stored(&d.to_json()), Some(d));
        }
    }
}
