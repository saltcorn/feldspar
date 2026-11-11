//! The admin API's endpoints: source control for the installation's structure
//! (APP_SYNC_PROPOSAL §7.2) — the link to a git repository, Status, Pull,
//! Commit, Discard, branches, Deploy and the ledger.
//!
//! `sc-server`'s `handlers::source_control` holds their handlers, over
//! `sc_sync::SourceControl`. None is offered to the MCP projection: a deploy
//! or a discard can drop data, and a commit publishes the structure, as
//! backup and clearAll are kept from agents.
//!
//! They are mounted under `api/source-control`, apart from an application's
//! offline-sync endpoints (`sync/pull`, `sync/push`), which are another thing.

use super::registry::{blocker_schema, planned_op_schema, receipt_fields};
use super::*;

fn sc() -> PathSpec {
    api().lit("source-control")
}

/// Declare this domain's endpoints on `set`.
#[allow(clippy::too_many_lines)]
pub(super) fn register(set: &mut EndpointSet) {
    // The link (null when the installation is not linked) and the credential
    // stored, linked or not: a key can be generated before linking, since the
    // remote must be given its public half first. A token is never answered.
    set.register(
        Endpoint::new(ids::GET_SYNC_LINK, Method::Get, sc().lit("link")).output(
            TypeSchema::struct_of([
                StructField::new("link", TypeSchema::optional(link_schema())),
                StructField::new("credential", credential_schema()),
            ]),
        ),
    );

    // Link the installation, or change the link: pull, and on a new URL or
    // subdirectory take the base from the branch tip (§4 "Linking"). Without
    // a `credential` the stored one is kept; `deploy_key` uses the key
    // generated here; `https_token` with no `token` keeps the stored token.
    set.register(
        Endpoint::new(ids::SET_SYNC_LINK, Method::Put, sc().lit("link"))
            .input(TypeSchema::struct_of(link_input_fields()))
            .output(TypeSchema::struct_of([
                StructField::new("tip", TypeSchema::optional(TypeSchema::text())),
                StructField::new("base_taken", TypeSchema::bool()),
            ])),
    );

    // Unlink: forget the link and the base. Live and the credential stay.
    set.register(
        Endpoint::new(ids::DELETE_SYNC_LINK, Method::Delete, sc().lit("link")).output(
            TypeSchema::struct_of([StructField::new("ok", TypeSchema::bool())]),
        ),
    );

    // Generate a deploy key and make it the credential.
    set.register(
        Endpoint::new(ids::GENERATE_SYNC_KEY, Method::Post, sc().lit("key"))
            .output(credential_schema()),
    );

    // Whether a link would reach its remote: its branches, or a 400 saying
    // why not. Nothing is stored.
    set.register(
        Endpoint::new(
            ids::TEST_SYNC_LINK,
            Method::Post,
            sc().lit("link").lit("test"),
        )
        .input(TypeSchema::struct_of(link_input_fields()))
        .output(TypeSchema::struct_of([StructField::new(
            "branches",
            TypeSchema::array(TypeSchema::text()),
        )])),
    );

    // Status (§4): every entity not in sync, against the tip as of the last
    // pull, with what stops Deploy (D5) and the file stores' warnings (§3.5).
    // Unlinked, `linked` is false and the rest empty.
    set.register(
        Endpoint::new(ids::GET_SYNC_STATUS, Method::Get, sc().lit("status")).output(
            TypeSchema::struct_of([
                StructField::new("linked", TypeSchema::bool()),
                StructField::new("link", TypeSchema::optional(link_schema())),
                StructField::new("tip", TypeSchema::optional(TypeSchema::text())),
                StructField::new("base_commit", TypeSchema::optional(TypeSchema::text())),
                StructField::new("entries", TypeSchema::array(status_entry_schema())),
                // Each local change and conflict, by key: what Deploy is
                // refused over until it is committed or discarded.
                StructField::new("deploy_blockers", TypeSchema::array(TypeSchema::text())),
                StructField::new("warnings", TypeSchema::array(TypeSchema::text())),
            ]),
        ),
    );

    // One Status entry's diff: field by field from the base to the tip
    // (`incoming`) and to live (`local`), or line by line for a text file.
    set.register(
        Endpoint::new(ids::GET_SYNC_ENTITY_DIFF, Method::Get, sc().lit("diff"))
            .query([QueryParam::new("key", ValueType::Text).required()])
            .output(TypeSchema::struct_of([
                StructField::new("key", TypeSchema::text()),
                StructField::new("kind", TypeSchema::text()),
                StructField::new("name", TypeSchema::text()),
                StructField::new("class", TypeSchema::text()),
                // `{ type: "json", value }` or `{ type: "file", hash, size }`.
                StructField::new("base", TypeSchema::optional(TypeSchema::json())),
                StructField::new("tip", TypeSchema::optional(TypeSchema::json())),
                StructField::new("live", TypeSchema::optional(TypeSchema::json())),
                StructField::new("incoming", side_diff_schema()),
                StructField::new("local", side_diff_schema()),
            ])),
    );

    // Pull: fetch and move the branch. Live does not change.
    set.register(
        Endpoint::new(ids::SYNC_PULL, Method::Post, sc().lit("pull")).output(pulled_schema()),
    );

    // Commit the chosen local changes (all of them without `keys`) and push.
    // A conflict is committed only when it is in `keep_mine`.
    set.register(
        Endpoint::new(ids::SYNC_COMMIT, Method::Post, sc().lit("commit"))
            .input(TypeSchema::struct_of([
                StructField::new("message", TypeSchema::text()),
                StructField::new(
                    "keys",
                    TypeSchema::optional(TypeSchema::array(TypeSchema::text())),
                ),
                StructField::new(
                    "keep_mine",
                    TypeSchema::optional(TypeSchema::array(TypeSchema::text())),
                ),
            ]))
            .output(TypeSchema::struct_of([
                StructField::new("id", TypeSchema::uuid()),
                StructField::new("commit", TypeSchema::text()),
                StructField::new("branch", TypeSchema::text()),
                StructField::new("entities", TypeSchema::array(TypeSchema::text())),
                StructField::new("warnings", TypeSchema::array(TypeSchema::text())),
            ])),
    );

    // The review of a discard: live with each of `keys` back at its base
    // version, classified as a deploy is (§5).
    set.register(
        Endpoint::new(
            ids::PLAN_SYNC_DISCARD,
            Method::Post,
            sc().lit("discard").lit("plan"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "keys",
            TypeSchema::array(TypeSchema::text()),
        )]))
        .output(TypeSchema::struct_of(plan_fields())),
    );

    // Apply the reviewed discard.
    set.register(
        Endpoint::new(ids::SYNC_DISCARD, Method::Post, sc().lit("discard"))
            .input(TypeSchema::struct_of([
                StructField::new("keys", TypeSchema::array(TypeSchema::text())),
                StructField::new("plan_digest", TypeSchema::text()),
                StructField::new("ticked", TypeSchema::array(TypeSchema::text())),
            ]))
            .output(TypeSchema::struct_of(receipt_fields())),
    );

    // The remote's branches, and the one linked.
    set.register(
        Endpoint::new(ids::LIST_SYNC_BRANCHES, Method::Get, sc().lit("branches")).output(
            TypeSchema::struct_of([
                StructField::new("current", TypeSchema::text()),
                StructField::new("branches", TypeSchema::array(TypeSchema::text())),
            ]),
        ),
    );

    // Switch to a branch, or create one from the current tip (`create`) and
    // switch to it. Live and the base stay as they are.
    set.register(
        Endpoint::new(ids::SWITCH_SYNC_BRANCH, Method::Post, sc().lit("branch"))
            .input(TypeSchema::struct_of([
                StructField::new("name", TypeSchema::text()),
                StructField::new("create", TypeSchema::optional(TypeSchema::bool())),
            ]))
            .output(pulled_schema()),
    );

    // Pull, then plan deploying the tip (§6 steps 1–4): the review. `commit`
    // is what to confirm, with the digest; `policy` says whether
    // data-destructive operations may be ticked here and whether a full
    // backup is offered.
    set.register(
        Endpoint::new(
            ids::PLAN_SYNC_DEPLOY,
            Method::Post,
            sc().lit("deploy").lit("plan"),
        )
        .output(TypeSchema::struct_of(plan_fields().into_iter().chain([
            StructField::new("commit", TypeSchema::optional(TypeSchema::text())),
            StructField::new("policy", policy_schema()),
        ]))),
    );

    // Apply the reviewed deploy (§6 steps 6–10). A tip that moved, or live
    // that changed, since the review is a stale plan and nothing is applied.
    set.register(
        Endpoint::new(ids::SYNC_DEPLOY, Method::Post, sc().lit("deploy"))
            .input(TypeSchema::struct_of([
                StructField::new("commit", TypeSchema::optional(TypeSchema::text())),
                StructField::new("plan_digest", TypeSchema::text()),
                StructField::new("ticked", TypeSchema::array(TypeSchema::text())),
                StructField::new("full_backup", TypeSchema::optional(TypeSchema::bool())),
            ]))
            .output(TypeSchema::struct_of(receipt_fields())),
    );

    // Plan deploying a structure zip (§7.4), uploaded to
    // `/source-control/upload` (a raw route: its body is the zip), which
    // answered `upload`. `linked` says which rule applies: in the branch tip's
    // place, with the base rules, or with no link additively — created and
    // updated, never deleted.
    set.register(
        Endpoint::new(
            ids::PLAN_SYNC_ZIP_DEPLOY,
            Method::Post,
            sc().lit("zip").lit("plan"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "upload",
            TypeSchema::text(),
        )]))
        .output(TypeSchema::struct_of(plan_fields().into_iter().chain([
            StructField::new("linked", TypeSchema::bool()),
            StructField::new("policy", policy_schema()),
        ]))),
    );

    // Apply the reviewed zip deploy. The upload is kept until it is
    // deployed, so a stale plan can be reviewed again.
    set.register(
        Endpoint::new(
            ids::SYNC_ZIP_DEPLOY,
            Method::Post,
            sc().lit("zip").lit("deploy"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("upload", TypeSchema::text()),
            StructField::new("plan_digest", TypeSchema::text()),
            StructField::new("ticked", TypeSchema::array(TypeSchema::text())),
            StructField::new("full_backup", TypeSchema::optional(TypeSchema::bool())),
        ]))
        .output(TypeSchema::struct_of(receipt_fields())),
    );

    // A file store's synced contents (§3.5): its `sync_contents` setting, and
    // what it resolves to from how the structure uses the store — the
    // directories committed, those left out inside them, and the warnings
    // Status shows. `contents` previews another setting before it is saved.
    // `applies` is false for a store whose files are never copied (a git
    // store, which is pinned at a commit instead).
    set.register(
        Endpoint::new(
            ids::GET_FILE_STORE_SYNC,
            Method::Get,
            api()
                .lit("file-stores")
                .param("id", ValueType::Uuid)
                .lit("sync"),
        )
        .query([QueryParam::new("contents", ValueType::Text)])
        .output(file_store_sync_schema()),
    );

    // Change a store's `sync_contents`: `auto`, `include` or `exclude`.
    set.register(
        Endpoint::new(
            ids::SET_FILE_STORE_SYNC,
            Method::Put,
            api()
                .lit("file-stores")
                .param("id", ValueType::Uuid)
                .lit("sync"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "sync_contents",
            TypeSchema::text(),
        )]))
        .output(file_store_sync_schema()),
    );

    // The ledger, newest first: deploys, discards, commits and registry saves.
    set.register(
        Endpoint::new(ids::LIST_DEPLOYMENTS, Method::Get, sc().lit("deployments"))
            .query([QueryParam::new("limit", ValueType::Int)])
            .output(TypeSchema::struct_of([StructField::new(
                "deployments",
                TypeSchema::array(deployment_schema()),
            )])),
    );
}

/// `{ kind: "none" | "deploy_key" | "https_token", public_key?, username? }`.
fn credential_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("kind", TypeSchema::text()),
        StructField::new("public_key", TypeSchema::optional(TypeSchema::text())),
        StructField::new("username", TypeSchema::optional(TypeSchema::text())),
    ])
}

/// `{ destructive: "confirm" | "never", full_backup: "off" | "optional" | "required" }`.
fn policy_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("destructive", TypeSchema::text()),
        StructField::new("full_backup", TypeSchema::text()),
    ])
}

fn link_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("url", TypeSchema::text()),
        StructField::new("branch", TypeSchema::text()),
        StructField::new("subdirectory", TypeSchema::optional(TypeSchema::text())),
        StructField::new("credential", credential_schema()),
        StructField::new("policy", policy_schema()),
    ])
}

fn link_input_fields() -> Vec<StructField> {
    vec![
        StructField::new("url", TypeSchema::text()),
        StructField::new("branch", TypeSchema::text()),
        StructField::new("subdirectory", TypeSchema::optional(TypeSchema::text())),
        StructField::new(
            "credential",
            TypeSchema::optional(TypeSchema::struct_of([
                StructField::new("kind", TypeSchema::text()),
                StructField::new("username", TypeSchema::optional(TypeSchema::text())),
                StructField::new("token", TypeSchema::optional(TypeSchema::text())),
            ])),
        ),
        StructField::new(
            "policy",
            TypeSchema::optional(TypeSchema::struct_of([
                StructField::new("destructive", TypeSchema::optional(TypeSchema::text())),
                StructField::new("full_backup", TypeSchema::optional(TypeSchema::text())),
            ])),
        ),
    ]
}

fn status_entry_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("key", TypeSchema::text()),
        StructField::new("kind", TypeSchema::text()),
        StructField::new("name", TypeSchema::text()),
        // `incoming`, `incoming_delete`, `incoming_new`, `local_change`,
        // `local_delete`, `local_new`, `pin_moved`, `conflict`.
        StructField::new("class", TypeSchema::text()),
        // The application a view, page, library item or translation is in.
        StructField::new("application", TypeSchema::optional(TypeSchema::text())),
        // The file it is in: live's, else the tip's, else the base's.
        StructField::new("path", TypeSchema::text()),
        // Whether its file is one the registry editor opens.
        StructField::new("in_registry", TypeSchema::bool()),
    ])
}

fn side_diff_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new(
            "changes",
            TypeSchema::array(TypeSchema::struct_of([
                StructField::new("path", TypeSchema::text()),
                StructField::new("before", TypeSchema::optional(TypeSchema::json())),
                StructField::new("after", TypeSchema::optional(TypeSchema::json())),
            ])),
        ),
        StructField::new(
            "text",
            TypeSchema::optional(TypeSchema::struct_of([StructField::new(
                "unified",
                TypeSchema::text(),
            )])),
        ),
        StructField::new("binary", TypeSchema::bool()),
    ])
}

fn pulled_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("before", TypeSchema::optional(TypeSchema::text())),
        StructField::new("tip", TypeSchema::optional(TypeSchema::text())),
        // Whether the remote was force-pushed, so the branch was reset.
        StructField::new("forced", TypeSchema::bool()),
    ])
}

fn plan_fields() -> Vec<StructField> {
    vec![
        StructField::new("operations", TypeSchema::array(planned_op_schema())),
        StructField::new("blockers", TypeSchema::array(blocker_schema())),
        StructField::new("needs_secrets", TypeSchema::array(TypeSchema::text())),
        StructField::new("needs_credentials", TypeSchema::array(TypeSchema::text())),
        StructField::new("digest", TypeSchema::text()),
    ]
}

fn file_store_sync_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("applies", TypeSchema::bool()),
        // `auto`, `include` or `exclude`: stored, or previewed.
        StructField::new("sync_contents", TypeSchema::text()),
        // Store-relative; `""` is the whole store.
        StructField::new("synced", TypeSchema::array(TypeSchema::text())),
        StructField::new("excluded", TypeSchema::array(TypeSchema::text())),
        StructField::new("warnings", TypeSchema::array(TypeSchema::text())),
    ])
}

/// One ledger row (`sc_sync::Deployment`).
fn deployment_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("id", TypeSchema::uuid()),
        // `deploy`, `discard`, `commit` or `edit` (a registry save).
        StructField::new("kind", TypeSchema::text()),
        StructField::new("result", TypeSchema::text()),
        StructField::new("commit", TypeSchema::optional(TypeSchema::text())),
        StructField::new("branch", TypeSchema::optional(TypeSchema::text())),
        StructField::new("actor", TypeSchema::optional(TypeSchema::text())),
        // `ui` or `cli`.
        StructField::new("source", TypeSchema::text()),
        // The plan's counts, what was applied, the notes, the drift; for a
        // commit its message and entities; for a registry save its `path`.
        StructField::new("summary", TypeSchema::json()),
        StructField::new("deferred", TypeSchema::array(TypeSchema::text())),
        StructField::new("backups", TypeSchema::array(TypeSchema::text())),
        // A registry save's file before and after: what "restore this
        // version" saves back.
        StructField::new("before", TypeSchema::optional(TypeSchema::json())),
        StructField::new("after", TypeSchema::optional(TypeSchema::json())),
        StructField::new("started_at", TypeSchema::timestamp()),
        StructField::new("finished_at", TypeSchema::timestamp()),
    ])
}
