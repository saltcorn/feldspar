//! Enforcing the **path-cumulative** access rule that [`FileMeta`] documents.
//!
//! [`FileMeta::min_role`] has been modelled since the MVP with the rule stated in
//! its own doc comment — *"to reach a file a user must clear the `min_role` of
//! the file **and** of every directory on its path"* — and nothing anywhere
//! enforced it. A stored rule that nothing checks is worse than no rule: an admin
//! sets a folder to admin-only, the UI shows it as restricted, and every file in
//! it stays readable. This module is that check.
//!
//! ## What "path-cumulative" means
//!
//! Roles follow the `sc-auth` convention: `1` is admin, `100` is public, and a
//! **lower number is more restrictive**. To reach `a/b/c.txt` a user must clear
//! the rule on the store, on `a`, on `a/b`, and on `c.txt` itself. The effective
//! floor is therefore the *most restrictive* (numerically smallest) rule on the
//! whole path, and a directory can only ever tighten access, never loosen it.
//!
//! That direction is the point. If a nested rule could widen access, then
//! restricting a folder would not actually restrict it — anything inside could
//! opt back out — and an admin could never reason about a tree from its root.
//!
//! ## Where the store's own floor fits
//!
//! A [`FileStoreDef::min_role`](crate::FileStoreDef) is the floor for the whole
//! store, applied before any per-file rule, and it composes the same way: it is
//! simply the outermost entry on the path.
//!
//! ## The owner (Saltcorn 1's rule)
//!
//! [`FileMeta::owner`] is an authority too (TODO F2, §2.1). A file is
//! **readable** if the caller's role clears the effective floor *or* the caller
//! is the file's owner, and it is **writable** (and deletable) by its owner or by
//! admin authority. That is what makes "min_role 1, and the uploader may read
//! it" — a private upload — expressible with no rule per user.
//!
//! Only the entry's *own* owner counts. Owning a folder does not grant the
//! files in it, because a folder's owner is whoever made it and the files are
//! whoever put them there; "the owner of the parent may read the child" would
//! hand every upload in a shared drop folder to the person who created it.
//!
//! An entry with **no owner** keeps the path rule for writes as well as reads —
//! it is a file nobody in particular put there (a scaffold, a code body under
//! admin authority, the disk underneath), and a caller who clears its floor may
//! change it as they always could. An *owned* entry is the owner's: clearing the
//! floor is not enough to overwrite or delete somebody else's upload.

use crate::store::{FileMeta, FileStore};
use sc_error::{Error, Result};

/// The least-restrictive role: everyone, including anonymous callers. Matches the
/// `sc-auth` scale where `100` is public.
pub const ROLE_PUBLIC: u8 = 100;

/// The most-restrictive role: admin authority, which every rule is cleared by.
pub const ROLE_ADMIN: u8 = 1;

/// Who is asking: the role every floor is checked against, and the user id the
/// owner rule compares with.
///
/// A struct rather than a bare role because the owner rule needs the id, and a
/// role-only check is how a file came to be unreadable by the person who
/// uploaded it. `user_id` is the account's id as [`FileMeta::owner`] records it
/// (a UUID's string form); `None` is a caller with no account — anonymous, or a
/// system run checked at a role — who owns nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileCaller {
    /// The caller's role (`1` admin … `100` public).
    pub role: u8,
    /// The caller's user id, if they are signed in.
    pub user_id: Option<String>,
}

impl FileCaller {
    /// A caller at `role`, signed in as `user_id` when there is one.
    pub fn new(role: u8, user_id: Option<String>) -> FileCaller {
        FileCaller { role, user_id }
    }

    /// A caller with no account, checked at `role` alone — anonymous at
    /// [`ROLE_PUBLIC`], or a system run at whatever role it was given.
    pub fn role(role: u8) -> FileCaller {
        FileCaller {
            role,
            user_id: None,
        }
    }

    /// Nobody: the public role and no account.
    pub fn anonymous() -> FileCaller {
        FileCaller::role(ROLE_PUBLIC)
    }

    /// Whether this caller has admin authority, which clears every rule.
    pub fn is_admin(&self) -> bool {
        self.role <= ROLE_ADMIN
    }

    /// Whether this caller is the recorded owner of the entry `meta` describes.
    ///
    /// An entry with no owner is owned by nobody, and a caller with no account
    /// owns nothing — two `None`s are not a match.
    pub fn owns(&self, meta: &FileMeta) -> bool {
        matches!((&self.user_id, &meta.owner), (Some(me), Some(owner)) if me == owner)
    }

    /// Whether the role alone clears `floor`.
    fn clears(&self, floor: Option<u8>) -> bool {
        floor.is_none_or(|required| self.role <= required)
    }
}

/// Combine two optional rules, keeping the more restrictive (smaller) one.
///
/// `None` means "this level says nothing", which must leave the inherited floor
/// untouched rather than reset it to public — otherwise a directory with no rule
/// of its own would silently undo its parent's.
fn tighten(current: Option<u8>, rule: Option<u8>) -> Option<u8> {
    match (current, rule) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// Every prefix of `path` that carries a rule, from the store root inwards,
/// including `path` itself.
///
/// `a/b/c.txt` yields `a`, `a/b`, `a/b/c.txt` — each of which may carry its own
/// [`FileMeta`]. Empty segments and `.` are skipped so the caller's normalisation
/// quirks (leading, trailing or doubled slashes) cannot change the answer.
fn ancestors(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if !current.is_empty() {
            current.push('/');
        }
        current.push_str(segment);
        out.push(current.clone());
    }
    out
}

/// The effective minimum role required to reach `path` in `store`, given the
/// store-wide floor `store_min_role`.
///
/// `None` means nothing on the path restricts it. Metadata is read for each
/// ancestor in turn; a path component whose metadata cannot be read at all (it
/// does not exist yet, which is normal when writing a new file) contributes
/// nothing rather than failing the check — the existence of the target is the
/// caller's concern, not the access rule's.
pub async fn effective_min_role(
    store: &dyn FileStore,
    store_min_role: Option<u8>,
    path: &str,
) -> Result<Option<u8>> {
    let mut floor = store_min_role;
    for ancestor in ancestors(path) {
        // A missing component simply has no rule of its own. Reading metadata for
        // something that is not there is not an access failure, and treating it
        // as one would make writing a new file impossible.
        let meta = store.get_meta(&ancestor).await.unwrap_or_default();
        floor = tighten(floor, meta.min_role);
    }
    Ok(floor)
}

/// Check that `caller` may **read** `path`, returning an error if not.
///
/// Remember the scale is inverted: a user clears a rule when their role number is
/// **less than or equal to** the required one (admin `1` clears everything;
/// public `100` clears only unrestricted paths). A caller who does not clear the
/// floor may still read an entry they own (§2.1).
///
/// The error deliberately does not say which component of the path denied
/// access, nor whether the target exists. Naming the restricted folder would tell
/// a caller who cannot read a tree something about its shape, and distinguishing
/// "forbidden" from "not found" turns the endpoint into a probe for the existence
/// of files the caller may not see.
pub async fn check_access(
    store: &dyn FileStore,
    store_min_role: Option<u8>,
    path: &str,
    caller: &FileCaller,
) -> Result<()> {
    if caller.is_admin() {
        return Ok(());
    }
    // A repository's own `.git` is the server's, not the store's content: its
    // `config` carries remote URLs with their tokens and its objects carry every
    // file the history ever held, so no rule set on the files reaches it.
    if crate::in_git_dir(path) {
        return Err(denied(store, path));
    }
    let floor = effective_min_role(store, store_min_role, path).await?;
    if caller.clears(floor) {
        return Ok(());
    }
    if caller.user_id.is_some() && caller.owns(&store.get_meta(path).await.unwrap_or_default()) {
        return Ok(());
    }
    Err(denied(store, path))
}

/// Check that `caller` may **write** `path` — create, replace, delete, rename
/// or change the metadata of it — returning an error if not.
///
/// Admin authority may write anything. An entry with an owner may be written by
/// that owner and nobody else. An entry with no owner, or a path with nothing
/// there yet, follows the path rule a read does: a caller who clears the floor
/// may create or change it.
///
/// A **directory** is checked all the way down: deleting or moving a folder is
/// deleting or moving every file in it, so one that holds somebody else's file
/// is refused exactly as that file would be.
pub async fn check_write(
    store: &dyn FileStore,
    store_min_role: Option<u8>,
    path: &str,
    caller: &FileCaller,
) -> Result<()> {
    if caller.is_admin() {
        return Ok(());
    }
    let meta = store.get_meta(path).await.unwrap_or_default();
    let permitted = match &meta.owner {
        Some(_) => caller.owns(&meta),
        None => caller.clears(effective_min_role(store, store_min_role, path).await?),
    };
    if !permitted {
        return Err(denied(store, path));
    }
    if matches!(store.stat(path).await, Ok(Some(stat)) if stat.is_dir)
        && holds_foreign_file(store, path, caller).await?
    {
        return Err(denied(store, path));
    }
    Ok(())
}

/// Whether anything beneath the directory `dir` is owned by somebody other than
/// `caller` — what makes deleting or moving the directory a write to their file.
async fn holds_foreign_file(store: &dyn FileStore, dir: &str, caller: &FileCaller) -> Result<bool> {
    let mut stack = vec![dir.to_owned()];
    while let Some(current) = stack.pop() {
        for entry in store.list(&current).await? {
            let meta = store.get_meta(&entry.path).await.unwrap_or_default();
            if meta.owner.is_some() && !caller.owns(&meta) {
                return Ok(true);
            }
            if entry.is_dir {
                stack.push(entry.path);
            }
        }
    }
    Ok(false)
}

/// Record `owner` as the creator of `path`, if nothing has been recorded yet.
///
/// Called after every write that can create an entry on somebody's behalf. Two
/// things it deliberately does not do:
///
/// - **It does not overwrite an existing owner.** The owner is the creator, so
///   saving somebody else's file does not take it over.
/// - **It does not fail the write.** The bytes are already there, and a store
///   whose backend cannot keep metadata (no xattr support on the filesystem) must
///   still be usable — an error after a successful upload would be a lie about
///   what happened.
pub async fn record_owner(store: &dyn FileStore, path: &str, owner: &str) {
    let Ok(meta) = store.get_meta(path).await else {
        return;
    };
    if meta.owner.is_some() {
        return;
    }
    let _ = store
        .set_meta(
            path,
            &FileMeta {
                owner: Some(owner.to_owned()),
                ..meta
            },
        )
        .await;
}

/// The one refusal every check answers with.
fn denied(store: &dyn FileStore, path: &str) -> Error {
    Error::auth(format!(
        "not permitted to access {path:?} in file store {}",
        store.name()
    ))
}

/// The entries of the directory `dir` that `caller` may see, with their
/// metadata — the whole of what a listing needs, in one call.
///
/// A caller who may read `dir` sees every child they clear or own. A caller who
/// may **not** read it still sees the children they own (§2.1: a user's own
/// uploads in a private drop folder), and nothing else. A caller who owns
/// nothing there gets the same refusal [`check_access`] gives, so an
/// unreadable folder still looks like a missing one.
pub async fn list_visible(
    store: &dyn FileStore,
    store_min_role: Option<u8>,
    dir: &str,
    caller: &FileCaller,
) -> Result<Vec<VisibleEntry>> {
    let readable = check_access(store, store_min_role, dir, caller).await;
    if readable.is_err() && caller.user_id.is_none() {
        return Err(denied(store, dir));
    }
    let dir_floor = effective_min_role(store, store_min_role, dir).await?;
    let entries = match store.list(dir).await {
        Ok(entries) => entries,
        Err(_) if readable.is_err() => return Err(denied(store, dir)),
        Err(e) => return Err(e),
    };
    let visible = visible_entries(store, dir_floor, entries, caller).await?;
    if readable.is_err() && visible.is_empty() {
        return Err(denied(store, dir));
    }
    Ok(visible)
}

/// Filter a directory listing to the entries `caller` may see: those whose rule
/// they clear, and those they own.
///
/// A listing has to be filtered rather than merely refused: the directory itself
/// may be readable while individual children are not, and showing names the
/// caller cannot open leaks exactly what the rule was set to hide.
///
/// The parent's floor is passed in as `dir_floor` (already including the store's)
/// so this only consults each child's own metadata — the ancestors are common to
/// every entry and re-reading them per child would be one `get_meta` per level
/// per file.
pub async fn filter_visible(
    store: &dyn FileStore,
    dir_floor: Option<u8>,
    entries: Vec<crate::Entry>,
    caller: &FileCaller,
) -> Result<Vec<crate::Entry>> {
    Ok(visible_entries(store, dir_floor, entries, caller)
        .await?
        .into_iter()
        .map(|visible| visible.entry)
        .collect())
}

/// One entry of a filtered listing, with what the filtering already had to know
/// about it.
#[derive(Debug, Clone)]
pub struct VisibleEntry {
    /// The entry itself.
    pub entry: crate::Entry,
    /// Its own metadata — the rule set *here*, and who created it.
    pub meta: FileMeta,
    /// The rule that actually reaches it: this entry's own tightened by the
    /// directory's (which already includes the store's).
    pub effective_min_role: Option<u8>,
}

/// [`filter_visible`], keeping the metadata rather than discarding it.
///
/// The filter reads every child's [`FileMeta`] to decide whether the caller may
/// see it, and a caller that then wants to *show* the rule — a file manager with
/// an access column — would otherwise read all of it a second time. Same walk,
/// same rule, one `get_meta` per entry.
pub async fn visible_entries(
    store: &dyn FileStore,
    dir_floor: Option<u8>,
    entries: Vec<crate::Entry>,
    caller: &FileCaller,
) -> Result<Vec<VisibleEntry>> {
    let mut visible = Vec::with_capacity(entries.len());
    for entry in entries {
        // Not listed below admin, for the reason `check_access` refuses it.
        if !caller.is_admin() && crate::in_git_dir(&entry.path) {
            continue;
        }
        let meta: FileMeta = store.get_meta(&entry.path).await.unwrap_or_default();
        let effective_min_role = tighten(dir_floor, meta.min_role);
        if caller.is_admin() || caller.clears(effective_min_role) || caller.owns(&meta) {
            visible.push(VisibleEntry {
                entry,
                meta,
                effective_min_role,
            });
        }
    }
    Ok(visible)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ancestors_walks_from_the_root_inwards() {
        assert_eq!(ancestors("a/b/c.txt"), ["a", "a/b", "a/b/c.txt"]);
        assert_eq!(ancestors("only.txt"), ["only.txt"]);
        assert!(ancestors("").is_empty());
        // Normalisation quirks must not change which rules apply.
        assert_eq!(ancestors("/a//b/./c"), ["a", "a/b", "a/b/c"]);
    }

    #[test]
    fn tightening_keeps_the_most_restrictive_rule() {
        // Lower is more restrictive, so a nested rule can only narrow access.
        assert_eq!(tighten(Some(40), Some(20)), Some(20));
        assert_eq!(tighten(Some(20), Some(40)), Some(20));
        // A level with no rule leaves the inherited floor alone — it must not
        // reset it to public, or a folder's restriction would be undone by any
        // unmarked child.
        assert_eq!(tighten(Some(20), None), Some(20));
        assert_eq!(tighten(None, Some(20)), Some(20));
        assert_eq!(tighten(None, None), None);
    }
}
