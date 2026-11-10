//! The sync link and the base, kept in `_fd_sync` (APP_SYNC_PROPOSAL §2, §4,
//! §7.1).
//!
//! `_fd_sync` holds at most one row:
//!
//! - **the link**: the remote's URL, the branch, and the subdirectory of the
//!   repository the structure lives in (for a repository that holds other
//!   things too). No URL means the installation is not linked;
//! - **the credential** the remote is reached with: none (a public or a
//!   `file://` remote), a deploy key generated here
//!   ([`sc_files::generate_deploy_key_in`]), or an HTTPS token, sealed with the
//!   instance key like every other secret. A key can be generated before
//!   there is a link to use it, since the remote has to be given the public
//!   half first;
//! - **the policy** (§5);
//! - **the base** (§4): each entity as it was last synced here, by a commit, a
//!   deploy or a discard. A tree, not a commit, so it survives force-pushes and
//!   branch switches and can hold deferred operations; compressed, with each
//!   store file as its blob hash ([`StructureTree::with_blob_hashes`]), whose
//!   bytes the sync mirror keeps reachable (`base::BASE_REF`). The commit the
//!   base was last taken from is kept beside it, for the history screen.
//!
//! The table is additive and created by the bootstrap ([`bootstrap`]), so no
//! migration is needed.

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chrono::Utc;
use serde::Serialize;
use serde_json::{Map, Value as Json, json};

use sc_catalog::{Catalog, DataField};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_files::HttpsToken;
use sc_query::{Assignment, Expr, Insert, Select, Source as From, Statement, Update, Value};
use sc_types::{BasicType, TypeRef};

use crate::deploy::Policy;
use crate::ledger::bootstrap_ledger;
use crate::tree::{StructureTree, TreeEntry};

/// The link's table.
pub const SYNC_TABLE: &str = "_fd_sync";

/// Its one row's id.
const ROW_ID: &str = "link";

/// How the remote is reached.
#[derive(Clone, Default, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Credential {
    /// Anonymously: a public repository, or one on this machine.
    #[default]
    None,
    /// Over SSH, with a key generated here. The private half never leaves the
    /// data directory; the public half is what the remote is given.
    DeployKey {
        /// The private key's file.
        #[serde(skip)]
        key_path: PathBuf,
        /// The public key, in `authorized_keys` form.
        public_key: String,
    },
    /// Over HTTPS, with a token.
    HttpsToken {
        /// The username sent with it.
        username: String,
        /// The token, sealed with the instance key. Never serialised.
        #[serde(skip)]
        sealed: String,
    },
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Credential::None => f.write_str("None"),
            Credential::DeployKey { key_path, .. } => f
                .debug_struct("DeployKey")
                .field("key_path", key_path)
                .finish_non_exhaustive(),
            Credential::HttpsToken { username, .. } => f
                .debug_struct("HttpsToken")
                .field("username", username)
                .finish_non_exhaustive(),
        }
    }
}

impl Credential {
    /// An HTTPS token, sealed with `catalog`'s instance key.
    pub fn https_token(catalog: &Catalog, username: Option<&str>, token: &str) -> Result<Self> {
        let token = token.trim();
        if token.is_empty() {
            return Err(Error::invalid("an HTTPS token cannot be empty"));
        }
        let username = HttpsToken::new(username, "").username().to_owned();
        Ok(Credential::HttpsToken {
            username,
            sealed: catalog.secret_key()?.seal(token)?,
        })
    }

    fn kind(&self) -> &'static str {
        match self {
            Credential::None => "none",
            Credential::DeployKey { .. } => "deploy_key",
            Credential::HttpsToken { .. } => "https_token",
        }
    }
}

/// Where the structure lives: the link's settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SyncLink {
    /// The remote's URL.
    pub url: String,
    /// The branch Status and Deploy compare against.
    pub branch: String,
    /// The directory of the repository the structure is in; `None` for its
    /// top.
    pub subdirectory: Option<String>,
    /// How the remote is reached.
    pub credential: Credential,
    /// The instance's policy.
    pub policy: Policy,
}

impl SyncLink {
    /// A link to `url`'s `branch`, anonymously, under the default policy.
    pub fn new(url: impl Into<String>, branch: impl Into<String>) -> SyncLink {
        SyncLink {
            url: url.into(),
            branch: branch.into(),
            subdirectory: None,
            credential: Credential::None,
            policy: Policy::default(),
        }
    }

    /// The link with the structure in `dir` of the repository.
    pub fn in_subdirectory(mut self, dir: impl Into<String>) -> SyncLink {
        self.subdirectory = Some(dir.into());
        self
    }

    /// Check what an admin gave, and tidy it: a URL and a branch name, and a
    /// subdirectory that stays inside the repository.
    pub fn checked(mut self) -> Result<SyncLink> {
        self.url = self.url.trim().to_owned();
        if self.url.is_empty() {
            return Err(Error::invalid(
                "a sync link needs the URL of a git repository",
            ));
        }
        if self.url.starts_with('-') {
            return Err(Error::invalid(format!(
                "`{}` is not a repository URL",
                self.url
            )));
        }
        self.branch = self.branch.trim().to_owned();
        check_branch(&self.branch)?;
        self.subdirectory = match self
            .subdirectory
            .as_deref()
            .map(|d| d.trim().trim_matches('/'))
        {
            None | Some("") => None,
            Some(dir) => {
                let bad = dir.contains('\\')
                    || dir.contains('\0')
                    || dir
                        .split('/')
                        .any(|s| s.is_empty() || s == "." || s == ".." || s == ".git");
                if bad {
                    return Err(Error::invalid(format!(
                        "`{dir}` is not a directory inside the repository"
                    )));
                }
                Some(dir.to_owned())
            }
        };
        Ok(self)
    }
}

/// Refuse a branch name git would not take, or would read as something else.
pub fn check_branch(branch: &str) -> Result<()> {
    let bad = branch.is_empty()
        || branch.starts_with('-')
        || branch.starts_with('/')
        || branch.ends_with('/')
        || branch.ends_with('.')
        || branch.ends_with(".lock")
        || branch.contains("..")
        || branch.contains("//")
        || branch.contains("@{")
        || branch == "@"
        || branch
            .chars()
            .any(|c| c.is_control() || " ~^:?*[\\".contains(c));
    if bad {
        return Err(Error::invalid(format!(
            "`{branch}` is not a branch name git accepts"
        )));
    }
    Ok(())
}

/// The base and the commit it was last taken from.
#[derive(Debug, Clone, PartialEq)]
pub struct Base {
    /// Each entity as last synced here.
    pub tree: StructureTree,
    /// The commit it came from, if one.
    pub commit: Option<String>,
}

fn text() -> TypeRef {
    TypeRef::Basic(BasicType::Text)
}

fn sync_fields() -> Vec<DataField> {
    vec![
        DataField::plain("id", text()).required().primary_key(),
        DataField::plain("url", text()),
        DataField::plain("branch", text()),
        DataField::plain("subdirectory", text()),
        DataField::plain("credential_kind", text()).required(),
        DataField::plain("credential_ref", text()),
        DataField::plain("credential_public", text()),
        DataField::plain("credential_secret", text()),
        DataField::plain("policy", TypeRef::Basic(BasicType::Json)).required(),
        DataField::plain("base_tree", TypeRef::Basic(BasicType::Bytes)),
        DataField::plain("base_commit", text()),
        DataField::plain("updated_at", TypeRef::Basic(BasicType::Timestamp)).required(),
    ]
}

/// Ensure `_fd_sync` and the ledger's tables exist: what the bootstrap runs.
/// Idempotent, and additive on an existing database.
pub async fn bootstrap(catalog: &Catalog) -> Result<()> {
    if catalog.get(SYNC_TABLE)?.is_none() {
        catalog.bootstrap_table(SYNC_TABLE, &sync_fields()).await?;
    }
    bootstrap_ledger(catalog).await
}

/// The link, or `None` when the installation is not linked.
pub async fn load_link(catalog: &Catalog) -> Result<Option<SyncLink>> {
    let Some(row) = load_row(catalog).await? else {
        return Ok(None);
    };
    let Some(url) = text_of(&row, "url") else {
        return Ok(None);
    };
    Ok(Some(SyncLink {
        url,
        branch: text_of(&row, "branch").unwrap_or_else(|| "main".to_owned()),
        subdirectory: text_of(&row, "subdirectory"),
        credential: credential_of(&row)?,
        policy: match row.get("policy") {
            Some(Value::Json(v)) => serde_json::from_value(v.clone()).unwrap_or_default(),
            _ => Policy::default(),
        },
    }))
}

/// The credential stored, linked or not.
pub async fn load_credential(catalog: &Catalog) -> Result<Credential> {
    match load_row(catalog).await? {
        Some(row) => credential_of(&row),
        None => Ok(Credential::None),
    }
}

/// Store the link's settings, keeping the base.
pub async fn save_link(catalog: &Catalog, link: &SyncLink) -> Result<()> {
    let mut values = vec![
        ("url", Value::Text(link.url.clone())),
        ("branch", Value::Text(link.branch.clone())),
        (
            "subdirectory",
            link.subdirectory.clone().map_or(Value::Null, Value::Text),
        ),
        ("policy", Value::Json(json!(link.policy))),
    ];
    values.extend(credential_values(&link.credential));
    upsert(catalog, values).await
}

/// Store a credential without touching the link: a key generated before
/// linking.
pub async fn save_credential(catalog: &Catalog, credential: &Credential) -> Result<()> {
    upsert(catalog, credential_values(credential)).await
}

/// Forget the link and the base. The credential stays, so linking again
/// needs no new key.
pub async fn unlink(catalog: &Catalog) -> Result<()> {
    upsert(
        catalog,
        vec![
            ("url", Value::Null),
            ("branch", Value::Null),
            ("subdirectory", Value::Null),
            ("base_tree", Value::Null),
            ("base_commit", Value::Null),
        ],
    )
    .await
}

/// The base, or `None` before the installation was first linked.
pub async fn load_base(catalog: &Catalog) -> Result<Option<Base>> {
    let Some(row) = load_row(catalog).await? else {
        return Ok(None);
    };
    let Some(Value::Bytes(bytes)) = row.get("base_tree") else {
        return Ok(None);
    };
    Ok(Some(Base {
        tree: decode_base(bytes)?,
        commit: text_of(&row, "base_commit"),
    }))
}

/// Store the base. Its store files must be blob hashes already, with their
/// bytes kept in the mirror (`base::keep_base_files`).
pub async fn store_base(catalog: &Catalog, base: &Base) -> Result<()> {
    upsert(
        catalog,
        vec![
            ("base_tree", Value::Bytes(encode_base(&base.tree)?)),
            (
                "base_commit",
                base.commit.clone().map_or(Value::Null, Value::Text),
            ),
        ],
    )
    .await
}

/// The base as stored: every path's entry as JSON, gzipped.
pub fn encode_base(tree: &StructureTree) -> Result<Vec<u8>> {
    let mut entries = Map::new();
    for (path, entry) in tree.iter() {
        let value = match entry {
            TreeEntry::Json(value) => json!({ "json": value }),
            TreeEntry::File(bytes) => json!({ "file": STANDARD.encode(bytes) }),
            TreeEntry::Blob { hash, size } => json!({ "blob": hash, "size": size }),
        };
        entries.insert(path.to_owned(), value);
    }
    let bytes = serde_json::to_vec(&json!({ "v": 1, "entries": entries }))
        .map_err(|e| Error::msg(format!("encoding the base: {e}")))?;
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&bytes)
        .and_then(|()| gz.finish())
        .map_err(|e| Error::msg(format!("compressing the base: {e}")))
}

/// Read back what [`encode_base`] wrote.
pub fn decode_base(bytes: &[u8]) -> Result<StructureTree> {
    let damaged = |why: String| {
        Error::msg(format!(
            "the stored base in `{SYNC_TABLE}` cannot be read ({why}); link the installation \
             again to take a new one"
        ))
    };
    let mut raw = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .read_to_end(&mut raw)
        .map_err(|e| damaged(e.to_string()))?;
    let value: Json = serde_json::from_slice(&raw).map_err(|e| damaged(e.to_string()))?;
    let entries = value
        .get("entries")
        .and_then(Json::as_object)
        .ok_or_else(|| damaged("no entries".into()))?;
    let mut tree = StructureTree::new();
    for (path, entry) in entries {
        if let Some(value) = entry.get("json") {
            tree.insert_json(path.clone(), value.clone());
        } else if let Some(file) = entry.get("file").and_then(Json::as_str) {
            let bytes = STANDARD
                .decode(file)
                .map_err(|e| damaged(format!("{path}: {e}")))?;
            tree.insert_file(path.clone(), bytes);
        } else if let Some(hash) = entry.get("blob").and_then(Json::as_str) {
            tree.insert_blob(path.clone(), hash, entry.get("size").and_then(Json::as_u64));
        } else {
            return Err(damaged(format!("`{path}` holds nothing")));
        }
    }
    Ok(tree)
}

// --- the row ----------------------------------------------------------------------

fn credential_values(credential: &Credential) -> Vec<(&'static str, Value)> {
    let (reference, public, secret) = match credential {
        Credential::None => (None, None, None),
        Credential::DeployKey {
            key_path,
            public_key,
        } => (
            Some(key_path.to_string_lossy().into_owned()),
            Some(public_key.clone()),
            None,
        ),
        Credential::HttpsToken { username, sealed } => {
            (Some(username.clone()), None, Some(sealed.clone()))
        }
    };
    let opt = |v: Option<String>| v.map_or(Value::Null, Value::Text);
    vec![
        ("credential_kind", Value::Text(credential.kind().to_owned())),
        ("credential_ref", opt(reference)),
        ("credential_public", opt(public)),
        ("credential_secret", opt(secret)),
    ]
}

fn credential_of(row: &Row) -> Result<Credential> {
    let kind = text_of(row, "credential_kind").unwrap_or_else(|| "none".to_owned());
    Ok(match kind.as_str() {
        "none" => Credential::None,
        "deploy_key" => Credential::DeployKey {
            key_path: PathBuf::from(text_of(row, "credential_ref").unwrap_or_default()),
            public_key: text_of(row, "credential_public").unwrap_or_default(),
        },
        "https_token" => Credential::HttpsToken {
            username: text_of(row, "credential_ref")
                .unwrap_or_else(|| HttpsToken::DEFAULT_USERNAME.to_owned()),
            sealed: text_of(row, "credential_secret").unwrap_or_default(),
        },
        other => {
            return Err(Error::msg(format!(
                "`{SYNC_TABLE}.credential_kind` holds `{other}`, which this version does not read"
            )));
        }
    })
}

/// The token a credential holds, opened with the instance key.
pub(crate) fn open_token(catalog: &Catalog, credential: &Credential) -> Result<Option<HttpsToken>> {
    match credential {
        Credential::HttpsToken { username, sealed } => {
            let token = catalog.secret_key()?.open(sealed)?;
            Ok(Some(HttpsToken::new(Some(username), token)))
        }
        _ => Ok(None),
    }
}

fn text_of(row: &Row, col: &str) -> Option<String> {
    match row.get(col) {
        Some(Value::Text(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

async fn load_row(catalog: &Catalog) -> Result<Option<Row>> {
    if catalog.get(SYNC_TABLE)?.is_none() {
        return Ok(None);
    }
    let select =
        Select::from(From::table(SYNC_TABLE)).filter(Expr::col("id").eq(Expr::lit(ROW_ID)));
    let rows: Vec<Row> = catalog
        .primary(SYNC_STATE)
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await?;
    Ok(rows.into_iter().next())
}

async fn upsert(catalog: &Catalog, values: Vec<(&'static str, Value)>) -> Result<()> {
    bootstrap(catalog).await?;
    let mut values: BTreeMap<&'static str, Value> = values.into_iter().collect();
    values.insert("updated_at", Value::Timestamp(Utc::now()));
    let statement = if load_row(catalog).await?.is_some() {
        let assignments = values
            .into_iter()
            .map(|(col, value)| Assignment::new(col.to_owned(), Expr::Lit(value)))
            .collect();
        Statement::from(
            Update::new(SYNC_TABLE, assignments).filter(Expr::col("id").eq(Expr::lit(ROW_ID))),
        )
    } else {
        values.insert("id", Value::Text(ROW_ID.to_owned()));
        values
            .entry("credential_kind")
            .or_insert_with(|| Value::Text("none".to_owned()));
        values
            .entry("policy")
            .or_insert_with(|| Value::Json(json!(Policy::default())));
        let (columns, values): (Vec<String>, Vec<Expr>) = values
            .into_iter()
            .map(|(col, value)| (col.to_owned(), Expr::Lit(value)))
            .unzip();
        Statement::from(Insert::row(SYNC_TABLE, columns, values))
    };
    catalog
        .primary(SYNC_STATE)
        .query(&statement)
        .await?
        .try_collect()
        .await?;
    Ok(())
}

/// The link's access to the primary database (docs/SECURITY_REVIEW_2026-10.md's
/// system-authority inventory).
const SYNC_STATE: sc_catalog::Unchecked<'static> =
    sc_catalog::Unchecked::system("sc-sync: the sync link and the base");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_base_round_trips_through_its_stored_form() {
        let mut tree = StructureTree::new();
        tree.insert_json(
            "tables/tickets.json",
            json!({ "name": "tickets", "fields": [] }),
        );
        tree.insert_blob("file-stores/assets/files/logo.png", "ab12", Some(3));
        tree.insert_file("README.md", b"\x00\xffbytes".to_vec());
        let stored = encode_base(&tree).unwrap();
        assert_eq!(&stored[..2], &[0x1f, 0x8b], "gzipped");
        assert_eq!(decode_base(&stored).unwrap(), tree);
        let err = decode_base(b"not gzip").unwrap_err().to_string();
        assert!(err.contains("link the installation again"), "{err}");
    }

    #[test]
    fn a_link_is_tidied_and_a_path_out_of_the_repository_refused() {
        let link = SyncLink::new(" file:///r.git ", " main ")
            .in_subdirectory("/apps/desk/")
            .checked()
            .unwrap();
        assert_eq!(link.url, "file:///r.git");
        assert_eq!(link.branch, "main");
        assert_eq!(link.subdirectory.as_deref(), Some("apps/desk"));
        for dir in ["../up", "a/./b", ".git/hooks"] {
            assert!(
                SyncLink::new("u", "main")
                    .in_subdirectory(dir)
                    .checked()
                    .is_err(),
                "{dir}"
            );
        }
        for branch in ["", "-f", "a..b", "a b", "x.lock", "@{1}"] {
            assert!(check_branch(branch).is_err(), "{branch:?}");
        }
        assert!(check_branch("feature/sync-2").is_ok());
    }
}
