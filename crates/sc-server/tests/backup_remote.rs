//! Automated backups sent off the server, end to end through the admin API and
//! the backup task: to an S3-compatible bucket and to an SFTP server, each
//! played by a server in this process.
//!
//! The fake S3 checks every request's Signature Version 4 signature against
//! the secret it knows, recomputed from what actually arrived on the wire — so a
//! request signed for a path, query or host other than the one sent is refused,
//! as a real service would refuse it. The SFTP server is russh's own server
//! half over a temporary directory, with OpenSSH's refusal to rename over an
//! existing file.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode as HttpStatus, Uri};
use axum::response::{IntoResponse, Response};
use chrono::{NaiveDateTime, Utc};
use russh::keys::{HashAlg, PrivateKey};
use russh::server::{Auth, ChannelOpenHandle, Msg, Server as _, Session};
use russh::{Channel, ChannelId};
use russh_sftp::protocol::{
    Attrs, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{furnish, has_entry, ok, setup, temp_dir, with_include};

// --- a fake S3 ---------------------------------------------------------------

const BUCKET: &str = "site";
const ACCESS_KEY: &str = "AKFELDSPARTEST";
/// With the characters a careless signer would mangle.
const SECRET_KEY: &str = "s3cr3t/key+with=odd chars";
const REGION: &str = "eu-central-1";
/// Keys per listing page, small so the pruning has to follow continuation
/// tokens.
const PAGE: usize = 2;

#[derive(Default)]
struct FakeS3 {
    objects: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl FakeS3 {
    fn keys(&self) -> Vec<String> {
        self.objects.lock().unwrap().keys().cloned().collect()
    }

    fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.objects.lock().unwrap().get(key).cloned()
    }

    fn put(&self, key: &str, body: &[u8]) {
        self.objects
            .lock()
            .unwrap()
            .insert(key.to_owned(), body.to_vec());
    }
}

async fn start_s3() -> (String, Arc<FakeS3>) {
    let state = Arc::new(FakeS3::default());
    let app = axum::Router::new()
        .fallback(s3_request)
        .layer(axum::extract::DefaultBodyLimit::disable())
        .with_state(Arc::clone(&state));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), state)
}

fn s3_error(status: HttpStatus, code: &str) -> Response {
    (
        status,
        format!(
            "<?xml version=\"1.0\"?><Error><Code>{code}</Code><Message>{code}</Message></Error>"
        ),
    )
        .into_response()
}

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            out.push(u8::from_str_radix(&raw[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

fn query_pairs(uri: &Uri) -> Vec<(String, String)> {
    uri.query()
        .unwrap_or_default()
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

/// Recompute the request's signature from what arrived, as S3 does.
fn verify(method: &Method, uri: &Uri, headers: &HeaderMap, body: &[u8]) -> Result<(), String> {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .ok_or_else(|| format!("missing {name}"))
    };
    let auth = header("authorization")?;
    let fields: HashMap<&str, &str> = auth
        .strip_prefix("AWS4-HMAC-SHA256 ")
        .ok_or("AuthorizationHeaderMalformed")?
        .split(',')
        .filter_map(|f| f.trim().split_once('='))
        .collect();
    let credential = fields
        .get("Credential")
        .ok_or("AuthorizationHeaderMalformed")?;
    if credential.split('/').next() != Some(ACCESS_KEY) {
        return Err("InvalidAccessKeyId".into());
    }
    if !credential.contains(&format!("/{REGION}/s3/")) {
        return Err("AuthorizationHeaderMalformed".into());
    }
    let signed: Vec<(String, String)> = fields
        .get("SignedHeaders")
        .ok_or("AuthorizationHeaderMalformed")?
        .split(';')
        .map(|name| Ok((name.to_owned(), header(name)?)))
        .collect::<Result<_, String>>()?;
    let payload_hash = header("x-amz-content-sha256")?;
    let actual: String = Sha256::digest(body)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if actual != payload_hash {
        return Err("XAmzContentSHA256Mismatch".into());
    }
    let at = NaiveDateTime::parse_from_str(&header("x-amz-date")?, "%Y%m%dT%H%M%SZ")
        .map_err(|e| e.to_string())?
        .and_utc();
    let expected = sc_server::s3_authorization(&sc_server::S3Signing {
        method: method.as_str(),
        path: uri.path(),
        query: &query_pairs(uri),
        headers: &signed,
        payload_hash: &payload_hash,
        access_key: ACCESS_KEY,
        secret_key: SECRET_KEY,
        region: REGION,
        at,
    });
    if expected != auth {
        return Err("SignatureDoesNotMatch".into());
    }
    Ok(())
}

async fn s3_request(
    State(s3): State<Arc<FakeS3>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(code) = verify(&method, &uri, &headers, &body) {
        return s3_error(HttpStatus::FORBIDDEN, &code);
    }
    let Some(rest) = uri.path().strip_prefix(&format!("/{BUCKET}")) else {
        return s3_error(HttpStatus::NOT_FOUND, "NoSuchBucket");
    };
    let key = percent_decode(rest.trim_start_matches('/'));
    match (method, key.is_empty()) {
        (Method::PUT, false) => {
            s3.put(&key, &body);
            HttpStatus::OK.into_response()
        }
        (Method::DELETE, false) => {
            s3.objects.lock().unwrap().remove(&key);
            HttpStatus::NO_CONTENT.into_response()
        }
        (Method::GET, true) => {
            let query: HashMap<String, String> = query_pairs(&uri).into_iter().collect();
            assert_eq!(query.get("list-type").map(String::as_str), Some("2"));
            let prefix = query.get("prefix").cloned().unwrap_or_default();
            let start: usize = query
                .get("continuation-token")
                .map(|t| {
                    t.strip_prefix("page/")
                        .and_then(|t| t.strip_suffix('='))
                        .unwrap()
                        .parse()
                        .unwrap()
                })
                .unwrap_or(0);
            let keys: Vec<String> = s3
                .keys()
                .into_iter()
                .filter(|k| k.starts_with(&prefix))
                .collect();
            let end = (start + PAGE).min(keys.len());
            let contents: String = keys[start..end]
                .iter()
                .map(|k| format!("<Contents><Key>{k}</Key><Size>1</Size></Contents>"))
                .collect();
            let truncated = end < keys.len();
            let next = if truncated {
                format!("<NextContinuationToken>page/{end}=</NextContinuationToken>")
            } else {
                String::new()
            };
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
                 <ListBucketResult><Name>{BUCKET}</Name><IsTruncated>{truncated}</IsTruncated>\
                 {next}{contents}</ListBucketResult>"
            )
            .into_response()
        }
        _ => s3_error(HttpStatus::METHOD_NOT_ALLOWED, "MethodNotAllowed"),
    }
}

/// A backup name `days` ago.
fn backup_name(days: i64) -> String {
    format!(
        "feldspar-backup-{}.zip",
        (Utc::now() - chrono::Duration::days(days)).format("%Y-%m-%d-%H%M%S")
    )
}

/// The old backups the pruning should delete: three, so the listing takes
/// more than one page.
fn old_backups() -> Vec<String> {
    (1..=3)
        .map(|d| format!("feldspar-backup-2000-01-0{d}-020000.zip"))
        .collect()
}

#[tokio::test]
async fn an_automated_backup_goes_to_an_s3_bucket() -> sc_error::Result<()> {
    let mut server = setup().await?;
    furnish(&mut server).await?;
    let (endpoint, s3) = start_s3().await;
    let destination = |secret: &str| {
        json!({
            "kind": "s3", "endpoint": endpoint, "bucket": BUCKET, "region": REGION,
            "access_key": ACCESS_KEY, "secret_key": secret,
        })
    };
    let body = |destination: Value| {
        with_include(json!({
            "destination": destination, "frequency": "daily", "retention_days": 7,
        }))
    };

    // A wrong secret is refused at save, with the service's own reason.
    let (status, refused) = server
        .client
        .send(
            "POST",
            "/api/backup/schedules",
            Some(body(destination("wrong"))),
        )
        .await;
    assert!(status.is_client_error(), "{status} {refused}");
    assert!(
        refused.to_string().contains("SignatureDoesNotMatch"),
        "{refused}"
    );
    // So is the mask, with nothing stored to stand for.
    let (status, refused) = server
        .client
        .send(
            "POST",
            "/api/backup/schedules",
            Some(body(destination(sc_types::SECRET_SENTINEL))),
        )
        .await;
    assert!(status.is_client_error(), "{status} {refused}");

    let created = ok(
        &mut server,
        "POST",
        "/api/backup/schedules",
        Some(body(destination(SECRET_KEY))),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_owned();
    assert_eq!(
        created["destination"]["secret_key"],
        json!(sc_types::SECRET_SENTINEL)
    );
    assert_eq!(created["location"], json!(format!("{endpoint}/{BUCKET}")));
    // The save's probe was written and taken away again.
    assert!(s3.keys().is_empty(), "{:?}", s3.keys());

    let old = old_backups();
    let recent = backup_name(2);
    for name in old.iter().chain([&recent, &"notes.txt".to_owned()]) {
        s3.put(name, b"x");
    }

    let scheduler = sc_server::BackupScheduler::new(server.catalog.clone());
    assert_eq!(scheduler.tick(Utc::now()).await.len(), 1);

    let list = ok(&mut server, "GET", "/api/backup/schedules", None).await;
    let entry = &list[0];
    assert_eq!(entry["last_error"], Value::Null, "{entry}");
    let written = entry["last_file"].as_str().unwrap();
    let name = written
        .strip_prefix(&format!("{endpoint}/{BUCKET}/"))
        .unwrap_or_else(|| panic!("{written}"));
    let archive = s3.get(name).expect("the backup is in the bucket");
    assert!(has_entry(&archive, "tables/books/table.json"));
    assert!(!has_entry(&archive, "tables/books/rows.json"));
    assert!(!has_entry(&archive, "users.json"));
    let keys = s3.keys();
    for gone in &old {
        assert!(!keys.contains(gone), "{gone} should have been pruned");
    }
    assert!(keys.contains(&recent) && keys.contains(&"notes.txt".to_owned()));
    // The secret never leaves the server.
    assert!(!list.to_string().contains(SECRET_KEY));

    // Edited with the mask handed back: the stored secret is kept, which the
    // save's probe (signed with it) proves.
    let updated = ok(
        &mut server,
        "PUT",
        &format!("/api/backup/schedules/{id}"),
        Some(json!({
            "destination": entry["destination"],
            "frequency": "weekly",
            "retention_days": 30,
            "include": entry["include"],
        })),
    )
    .await;
    assert_eq!(updated["frequency"], json!("weekly"));
    let stored = sc_config::stored_config(&server.catalog, sc_config::BACKUP_SCHEDULES)
        .await?
        .unwrap();
    assert_eq!(stored[0]["destination"]["secret_key"], json!(SECRET_KEY));
    Ok(())
}

// --- an SFTP server -----------------------------------------------------------

const SFTP_USER: &str = "feldspar";
const SFTP_PASSWORD: &str = "hunter2";

/// A fresh Ed25519 host key, drawn from two v4 UUIDs' randomness.
fn host_key() -> PrivateKey {
    let mut seed = [0u8; 32];
    seed[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    seed[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    PrivateKey::from(russh::keys::ssh_key::private::Ed25519Keypair::from_seed(
        &seed,
    ))
}

fn fingerprint(key: &PrivateKey) -> String {
    key.public_key().fingerprint(HashAlg::Sha256).to_string()
}

/// Serve SFTP over `root` on `port` (0 for any), returning the port and the
/// task to abort to stop it.
async fn start_sftp(
    root: PathBuf,
    key: PrivateKey,
    port: u16,
) -> (u16, tokio::task::JoinHandle<()>) {
    let config = Arc::new(russh::server::Config {
        keys: vec![key],
        auth_rejection_time: Duration::from_millis(10),
        auth_rejection_time_initial: Some(Duration::ZERO),
        ..Default::default()
    });
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        let mut server = SshServer { root };
        let _ = server.run_on_socket(config, &listener).await;
    });
    (port, task)
}

#[derive(Clone)]
struct SshServer {
    root: PathBuf,
}

impl russh::server::Server for SshServer {
    type Handler = SshSession;

    fn new_client(&mut self, _: Option<SocketAddr>) -> SshSession {
        SshSession {
            root: self.root.clone(),
            channels: HashMap::new(),
        }
    }
}

struct SshSession {
    root: PathBuf,
    channels: HashMap<ChannelId, Channel<Msg>>,
}

impl russh::server::Handler for SshSession {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(if user == SFTP_USER && password == SFTP_PASSWORD {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.close(channel)?;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        id: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        match (name, self.channels.remove(&id)) {
            ("sftp", Some(channel)) => {
                session.channel_success(id)?;
                let fs = SftpFs {
                    root: self.root.clone(),
                    files: HashMap::new(),
                    dirs: HashMap::new(),
                    next: 0,
                };
                russh_sftp::server::run(channel.into_stream(), fs).await;
            }
            _ => session.channel_failure(id)?,
        }
        Ok(())
    }
}

/// SFTP over a directory: the login's directory is `root`, and an absolute
/// path is taken from `root` too.
struct SftpFs {
    root: PathBuf,
    files: HashMap<String, std::fs::File>,
    /// An open listing, until it has been read.
    dirs: HashMap<String, Option<Vec<File>>>,
    next: u64,
}

impl SftpFs {
    fn real(&self, path: &str) -> Result<PathBuf, StatusCode> {
        let rel = path.trim_start_matches('/');
        if rel.split('/').any(|p| p == "..") {
            return Err(StatusCode::PermissionDenied);
        }
        Ok(if rel.is_empty() || rel == "." {
            self.root.clone()
        } else {
            self.root.join(rel)
        })
    }

    fn handle(&mut self) -> String {
        self.next += 1;
        self.next.to_string()
    }
}

fn done(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: "Ok".into(),
        language_tag: "en-US".into(),
    }
}

fn io(e: std::io::Error) -> StatusCode {
    match e.kind() {
        std::io::ErrorKind::NotFound => StatusCode::NoSuchFile,
        std::io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
        _ => StatusCode::Failure,
    }
}

impl russh_sftp::server::Handler for SftpFs {
    type Error = StatusCode;

    fn unimplemented(&self) -> StatusCode {
        StatusCode::OpUnsupported
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: OpenFlags,
        _: FileAttributes,
    ) -> Result<Handle, StatusCode> {
        let file = std::fs::OpenOptions::new()
            .read(pflags.contains(OpenFlags::READ))
            .write(pflags.contains(OpenFlags::WRITE))
            .create(pflags.contains(OpenFlags::CREATE))
            .truncate(pflags.contains(OpenFlags::TRUNCATE))
            .open(self.real(&filename)?)
            .map_err(io)?;
        let handle = self.handle();
        self.files.insert(handle.clone(), file);
        Ok(Handle { id, handle })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, StatusCode> {
        use std::io::{Seek, SeekFrom, Write};
        let file = self.files.get_mut(&handle).ok_or(StatusCode::Failure)?;
        file.seek(SeekFrom::Start(offset)).map_err(io)?;
        file.write_all(&data).map_err(io)?;
        Ok(done(id))
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, StatusCode> {
        self.files.remove(&handle);
        self.dirs.remove(&handle);
        Ok(done(id))
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, StatusCode> {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(self.real(&path)?).map_err(io)? {
            let entry = entry.map_err(io)?;
            let meta = entry.metadata().map_err(io)?;
            files.push(File::new(
                entry.file_name().to_string_lossy(),
                FileAttributes::from(&meta),
            ));
        }
        let handle = self.handle();
        self.dirs.insert(handle.clone(), Some(files));
        Ok(Handle { id, handle })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, StatusCode> {
        match self.dirs.get_mut(&handle).and_then(Option::take) {
            Some(files) => Ok(Name { id, files }),
            None => Err(StatusCode::Eof),
        }
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, StatusCode> {
        std::fs::remove_file(self.real(&filename)?).map_err(io)?;
        Ok(done(id))
    }

    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        _: FileAttributes,
    ) -> Result<Status, StatusCode> {
        std::fs::create_dir(self.real(&path)?).map_err(io)?;
        Ok(done(id))
    }

    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, StatusCode> {
        let to = self.real(&newpath)?;
        // OpenSSH's SFTP server will not rename over an existing file.
        if to.exists() {
            return Err(StatusCode::Failure);
        }
        std::fs::rename(self.real(&oldpath)?, to).map_err(io)?;
        Ok(done(id))
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, StatusCode> {
        let meta = std::fs::metadata(self.real(&path)?).map_err(io)?;
        Ok(Attrs {
            id,
            attrs: FileAttributes::from(&meta),
        })
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, StatusCode> {
        let meta = std::fs::symlink_metadata(self.real(&path)?).map_err(io)?;
        Ok(Attrs {
            id,
            attrs: FileAttributes::from(&meta),
        })
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, StatusCode> {
        let file = self.files.get(&handle).ok_or(StatusCode::Failure)?;
        let meta = file.metadata().map_err(io)?;
        Ok(Attrs {
            id,
            attrs: FileAttributes::from(&meta),
        })
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, StatusCode> {
        Ok(Name {
            id,
            files: vec![File::dummy(format!("/{}", path.trim_start_matches('/')))],
        })
    }
}

#[tokio::test]
async fn an_automated_backup_goes_to_an_sftp_server() -> sc_error::Result<()> {
    let mut server = setup().await?;
    furnish(&mut server).await?;
    let root = temp_dir();
    let key = host_key();
    let (port, sftp) = start_sftp(root.clone(), key.clone(), 0).await;
    let destination = |password: &str| {
        json!({
            "kind": "sftp", "host": "127.0.0.1", "port": port, "username": SFTP_USER,
            "password": password, "directory": "backups/nightly/",
        })
    };
    let body = |destination: Value| {
        with_include(json!({
            "destination": destination, "frequency": "daily", "retention_days": 7,
        }))
    };

    // A wrong password is refused at save.
    let (status, refused) = server
        .client
        .send(
            "POST",
            "/api/backup/schedules",
            Some(body(destination("wrong"))),
        )
        .await;
    assert!(status.is_client_error(), "{status} {refused}");
    assert!(
        refused
            .to_string()
            .contains("refused the user name or password"),
        "{refused}"
    );

    let created = ok(
        &mut server,
        "POST",
        "/api/backup/schedules",
        Some(body(destination(SFTP_PASSWORD))),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_owned();
    assert_eq!(
        created["destination"]["password"],
        json!(sc_types::SECRET_SENTINEL)
    );
    // The host key, learnt on save.
    assert_eq!(created["destination"]["host_key"], json!(fingerprint(&key)));
    let location = format!("sftp://{SFTP_USER}@127.0.0.1:{port}/~/backups/nightly");
    assert_eq!(created["location"], json!(location));
    // The directory was made on save, and the probe taken away again.
    let dir = root.join("backups/nightly");
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);

    let old = old_backups();
    let recent = backup_name(2);
    for name in old.iter().chain([&recent, &"notes.txt".to_owned()]) {
        std::fs::write(dir.join(name), b"x").unwrap();
    }

    let now = Utc::now();
    let scheduler = sc_server::BackupScheduler::new(server.catalog.clone());
    assert_eq!(scheduler.tick(now).await.len(), 1);

    let list = ok(&mut server, "GET", "/api/backup/schedules", None).await;
    let entry = &list[0];
    assert_eq!(entry["last_error"], Value::Null, "{entry}");
    let written = entry["last_file"].as_str().unwrap();
    let name = written
        .strip_prefix(&format!("{location}/"))
        .unwrap_or_else(|| panic!("{written}"));
    let archive = std::fs::read(dir.join(name)).unwrap();
    assert!(has_entry(&archive, "tables/books/table.json"));
    assert!(!has_entry(&archive, "users.json"));
    for gone in &old {
        assert!(!dir.join(gone).exists(), "{gone} should have been pruned");
    }
    assert!(dir.join(&recent).exists() && dir.join("notes.txt").exists());
    assert!(std::fs::read_dir(&dir).unwrap().all(|e| {
        !e.unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".partial")
    }));
    assert!(!list.to_string().contains(SFTP_PASSWORD));

    // Edited with the mask handed back: the save logs in with the stored
    // password.
    ok(
        &mut server,
        "PUT",
        &format!("/api/backup/schedules/{id}"),
        Some(json!({
            "destination": entry["destination"],
            "frequency": "daily",
            "retention_days": 30,
            "include": entry["include"],
        })),
    )
    .await;
    let stored = sc_config::stored_config(&server.catalog, sc_config::BACKUP_SCHEDULES)
        .await?
        .unwrap();
    assert_eq!(stored[0]["destination"]["password"], json!(SFTP_PASSWORD));

    // Another server answering on that address, with another key: the next
    // run refuses it rather than handing over the password.
    sftp.abort();
    let _ = sftp.await;
    let impostor = host_key();
    let (_, _impostor) = start_sftp(root.clone(), impostor.clone(), port).await;
    assert_eq!(
        scheduler
            .tick(now + chrono::Duration::days(1) + chrono::Duration::minutes(1))
            .await
            .len(),
        1
    );
    let list = ok(&mut server, "GET", "/api/backup/schedules", None).await;
    let error = list[0]["last_error"].as_str().unwrap_or_default();
    assert!(error.contains("host key"), "{error}");
    assert!(error.contains(&fingerprint(&impostor)), "{error}");
    Ok(())
}
