//! The admin API's endpoints: users, roles, role grants, credits and API tokens.
//!
//! `sc-server`'s `handlers::users` holds their handlers.

use super::*;

/// Declare this domain's endpoints on `set`.
pub(super) fn register(set: &mut EndpointSet) {
    // A role is a row in `_fd_roles` (§7.1, §9), not a bare integer: it carries
    // a name and, in `attributes`, whatever role-specific settings arrive later.
    // `users.role` is a foreign key onto it, so creating a role is a
    // prerequisite for assigning a user to it — which is why creating and
    // deleting roles has to be reachable from the admin UI and not only from a
    // SQL prompt.

    set.register(
        Endpoint::new("listRoles", Method::Get, api().lit("roles"))
            .output(TypeSchema::array(role_schema()))
            .auth(AuthRequirement::admin())
            .mcp(
                "List the roles: each one's number (1 is admin, 100 is public — \
                 anyone, signed in or not; a lower number is more privileged), name \
                 and description. A user holds one role; a table, query, trigger or \
                 stream's `min_role` is the least-privileged role that may use it.",
            ),
    );

    set.register(
        Endpoint::new("createRole", Method::Post, api().lit("roles"))
            .input(TypeSchema::struct_of([
                StructField::new("role", TypeSchema::int()),
                StructField::new("name", TypeSchema::text()),
                StructField::new("description", TypeSchema::text()),
            ]))
            .output(role_schema())
            .auth(AuthRequirement::admin())
            .mcp(
                McpTag::new(
                    "Create a role: `role` is its number, 2 to 99 (1 and 100 are built \
                     in), lower being more privileged; `name` must be unused. A role \
                     decides what its users may reach, so say plainly what you are \
                     creating it for.",
                )
                .needs(Grant::AccessChanges),
            ),
    );

    // Addressed by the role *number*, not the row id: the number is what
    // `users.role` and every `min_role` holds, so it is the handle an admin
    // already has in front of them.
    set.register(
        Endpoint::new(
            "deleteRole",
            Method::Delete,
            api().lit("roles").param("role", ValueType::Int),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "deleted",
            TypeSchema::bool(),
        )]))
        .auth(AuthRequirement::admin())
        .mcp(
            McpTag::new(
                "Delete a role by its number. Refused for a built-in role and for one \
                 a user still holds — move those users first.",
            )
            .needs(Grant::AccessChanges)
            .and_needs(Grant::Drop),
        ),
    );

    // The users screen manages accounts the same way every other screen manages
    // its objects — a list, a form, and the operations that are not edits.
    //
    // Four of these are *not* row edits and that is why they are endpoints of
    // their own rather than fields of `updateUser`: disabling an account,
    // dropping its sessions, becoming it, and resetting its password each do
    // something a column write cannot (they touch sessions, or they hand back a
    // secret that exists only in that response). Spelling them as booleans in an
    // update body would hide that.

    set.register(
        Endpoint::new("listUsers", Method::Get, api().lit("users"))
            .output(TypeSchema::array(user_row_schema()))
            .auth(AuthRequirement::admin())
            .mcp(
                "List the user accounts: id, email, role and whether each is disabled, \
                 with any columns the admin added. Never the password hashes.",
            ),
    );

    set.register(
        Endpoint::new("createUser", Method::Post, api().lit("users"))
            .input(user_input_schema())
            // The row, plus the password if one was generated because the admin
            // left it blank — the only moment it is readable (§7.1).
            .output(TypeSchema::struct_of([
                StructField::new("user", user_row_schema()),
                StructField::new(
                    "generated_password",
                    TypeSchema::optional(TypeSchema::text()),
                ),
            ]))
            .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "updateUser",
            Method::Put,
            api().lit("users").param("id", ValueType::Uuid),
        )
        .input(user_input_schema())
        .output(user_row_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "deleteUser",
            Method::Delete,
            api().lit("users").param("id", ValueType::Uuid),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "deleted",
            TypeSchema::bool(),
        )]))
        .auth(AuthRequirement::admin())
        .mcp(
            McpTag::new(
                "Delete a user account by id (from `listUsers`). Prefer \
                 `setUserDisabled`, which keeps the row and everything that names it. \
                 Refused for your own account and for the last enabled admin.",
            )
            .needs(Grant::AccessChanges)
            .and_needs(Grant::Drop),
        ),
    );

    set.register(
        Endpoint::new(
            "setUserDisabled",
            Method::Post,
            api()
                .lit("users")
                .param("id", ValueType::Uuid)
                .lit("disabled"),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "disabled",
            TypeSchema::bool(),
        )]))
        .output(user_row_schema())
        .auth(AuthRequirement::admin())
        .mcp(
            McpTag::new(
                "Disable (`disabled: true`) or re-enable a user account by id. A \
                 disabled account cannot sign in and its sessions stop working, but \
                 its row stays. Refused for your own account and for the last \
                 enabled admin.",
            )
            .needs(Grant::AccessChanges),
        ),
    );

    set.register(
        Endpoint::new(
            "forceLogoutUser",
            Method::Post,
            api()
                .lit("users")
                .param("id", ValueType::Uuid)
                .lit("force-logout"),
        )
        // `ok`, not a count of sessions ended: the sessions are dropped by the
        // transport (which owns the store) after the handler has returned, so a
        // number here would be one the handler had to guess.
        .output(TypeSchema::struct_of([StructField::new(
            "ok",
            TypeSchema::bool(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // Swap the caller's own session for one belonging to this user, with no
    // password: an admin who can reset that password can already sign in as them
    // (§7.1), so this adds convenience rather than authority — and it costs the
    // admin their admin session, which is the honest price of the swap.
    set.register(
        Endpoint::new(
            "becomeUser",
            Method::Post,
            api()
                .lit("users")
                .param("id", ValueType::Uuid)
                .lit("become"),
        )
        .output(user_summary_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "setRandomPassword",
            Method::Post,
            api()
                .lit("users")
                .param("id", ValueType::Uuid)
                .lit("random-password"),
        )
        .output(TypeSchema::struct_of([
            StructField::new("email", TypeSchema::text()),
            StructField::new("password", TypeSchema::text()),
        ]))
        .auth(AuthRequirement::admin()),
    );

    // Role grants (TODO-payments P3.2): roles a user holds for a while beside
    // the base role `updateUser` sets — a subscription's, a trial's, or one an
    // admin gives by hand. The effective role is the most privileged of the two.
    set.register(
        Endpoint::new(
            "listUserRoleGrants",
            Method::Get,
            api()
                .lit("users")
                .param("id", ValueType::Uuid)
                .lit("role-grants"),
        )
        .output(crate::auth::user_role_grants_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "grantUserRole",
            Method::Post,
            api()
                .lit("users")
                .param("id", ValueType::Uuid)
                .lit("role-grants"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("role", TypeSchema::int()),
            // Days from now; left out, the grant lasts until it is revoked.
            StructField::new("expires_in_days", TypeSchema::optional(TypeSchema::int())),
            StructField::new("note", TypeSchema::optional(TypeSchema::text())),
        ]))
        .output(crate::auth::role_grant_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "revokeUserRoleGrant",
            Method::Post,
            api()
                .lit("users")
                .param("id", ValueType::Uuid)
                .lit("role-grants")
                .param("grant", ValueType::Uuid)
                .lit("revoke"),
        )
        .output(crate::auth::role_grant_schema())
        .auth(AuthRequirement::admin()),
    );

    // LLM credits (TODO-payments P6.2): the balance an agent run's model calls
    // are charged to. The page shows the balance, the buckets and the latest
    // entries, grants credits by hand, and sets the low-balance threshold.
    // Amounts are exact decimal strings.
    set.register(
        Endpoint::new(
            "getUserCredits",
            Method::Get,
            api()
                .lit("users")
                .param("id", ValueType::Uuid)
                .lit("credits"),
        )
        .output(crate::auth::user_credits_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "grantUserCredits",
            Method::Post,
            api()
                .lit("users")
                .param("id", ValueType::Uuid)
                .lit("credits"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("amount", TypeSchema::text()),
            // Days from now; left out, the credits never expire.
            StructField::new("expires_in_days", TypeSchema::optional(TypeSchema::int())),
            StructField::new("note", TypeSchema::optional(TypeSchema::text())),
        ]))
        .output(crate::auth::user_credits_schema())
        .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new(
            "setUserCreditThreshold",
            Method::Post,
            api()
                .lit("users")
                .param("id", ValueType::Uuid)
                .lit("credits")
                .lit("threshold"),
        )
        .input(TypeSchema::struct_of([
            // Left out or null: no low-balance notices.
            StructField::new("threshold", TypeSchema::optional(TypeSchema::text())),
        ]))
        .output(crate::auth::user_credits_schema())
        .auth(AuthRequirement::admin()),
    );

    //
    // The credential the administration MCP server authenticates with: a bearer
    // token that **names a user** and runs with exactly their authority, so
    // there is no second authorization model to keep in step with this one.
    //
    // Three endpoints, and the shape of them is decided by two rules:
    //
    // - **None of them is tagged for MCP.** A token that can mint tokens is a
    //   token that cannot be revoked: the agent holding one could replace it the
    //   moment an admin took it away, and the credential would outlive the
    //   decision to end it. Minting is a thing a person does at a screen.
    // - **The plaintext appears in exactly one response**, `createApiToken`'s.
    //   Nothing reads it back afterwards because nothing can — the table holds a
    //   hash — and `listApiTokens` has no field for it or for the hash.
    //
    // A mint is always for the **calling admin**. Not a limitation of the
    // storage — the row names any user — but of this API: minting a credential
    // that runs as somebody else is handing out their authority without their
    // knowledge, and the person who wants a token is standing at the screen.

    set.register(
        Endpoint::new("listApiTokens", Method::Get, api().lit("api-tokens"))
            .output(TypeSchema::array(api_token_schema()))
            .auth(AuthRequirement::admin()),
    );

    set.register(
        Endpoint::new("createApiToken", Method::Post, api().lit("api-tokens"))
            .input(TypeSchema::struct_of([
                StructField::new("label", TypeSchema::text()),
                // The six flags of §13.6, each optional so a client that sends
                // none gets the same safe defaults an unconfigured copilot has:
                // it can build, it cannot drop, it cannot widen access.
                StructField::new("grants", TypeSchema::optional(api_token_grants_schema())),
                // Days, not a timestamp: an admin decides how long a laptop
                // keeps a credential, not the instant it stops. Absent means a
                // token that does not lapse, which the screen has to say plainly.
                StructField::new("expires_in_days", TypeSchema::optional(TypeSchema::int())),
            ]))
            .output(TypeSchema::struct_of([
                StructField::new("token", api_token_schema()),
                // Shown once. The response is the only place this value ever
                // exists outside the client that receives it.
                StructField::new("secret", TypeSchema::text()),
            ]))
            .auth(AuthRequirement::admin()),
    );

    // Revocation rather than deletion, and a `POST` rather than a `DELETE`: the
    // row stays, marked, because a revocation is a thing that happened and the
    // list is where an admin sees that it did.
    set.register(
        Endpoint::new(
            "revokeApiToken",
            Method::Post,
            api()
                .lit("api-tokens")
                .param("id", ValueType::Uuid)
                .lit("revoke"),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "revoked",
            TypeSchema::bool(),
        )]))
        .auth(AuthRequirement::admin()),
    );
}

/// The six flags a token carries, which are the `admin_copilot` agent's six
/// flags (§13.6) — one vocabulary for "what may this agent do to my
/// installation?", whether the agent is the built-in copilot or an external one
/// reached over MCP.
///
/// Every field is optional on the way *in* and present on the way *out*: a
/// client may leave a flag to its default, and a stored credential records what
/// was actually agreed to rather than what a later default would say.
///
/// `allow_all` beside them holds every flag, including those a later version
/// adds (`crate::mcp::ALLOW_ALL`).
fn api_token_grants_schema() -> TypeSchema {
    TypeSchema::struct_of(
        crate::mcp::FLAG_KEYS
            .into_iter()
            .chain([crate::mcp::ALLOW_ALL])
            .map(|key| StructField::new(key, TypeSchema::optional(TypeSchema::bool()))),
    )
}

/// One stored API token, as a list sees it.
///
/// There is no field for the token and none for its hash, which is the same
/// omission `sc_auth::ApiToken` makes and for the same reason: "shown once" is a
/// property of the shape rather than of everyone's care.
fn api_token_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("id", TypeSchema::uuid()),
        StructField::new("user_id", TypeSchema::uuid()),
        StructField::new("label", TypeSchema::text()),
        // Every flag present and explicit — the record of what was agreed to.
        StructField::new("grants", TypeSchema::json()),
        StructField::new("created_at", TypeSchema::timestamp()),
        StructField::new("expires_at", TypeSchema::optional(TypeSchema::timestamp())),
        StructField::new(
            "last_used_at",
            TypeSchema::optional(TypeSchema::timestamp()),
        ),
        StructField::new("revoked_at", TypeSchema::optional(TypeSchema::timestamp())),
        // Whether it would authenticate right now as far as the row can tell —
        // neither revoked nor lapsed. The screen draws a badge from this rather
        // than recomputing the clock arithmetic in TypeScript.
        StructField::new("live", TypeSchema::bool()),
    ])
}

/// One role (technical design §7.1, §9).
///
/// **Why roles are rows and not a constant list.** Roles are the fixed scale
/// `1..=100`, and for a while an integer was all a role was: `1` meant admin
/// because a constant said so, `100` meant public, and the ninety-eight numbers
/// between meant whatever an installation's users made them mean. That stops
/// working the moment a role has to *carry* something — a name to show in a
/// pick-list, settings that apply to everyone holding it — because a row can
/// carry those and an integer cannot. So `_fd_roles` holds them, `users.role`
/// references it, and this endpoint reports what is there rather than what a
/// constant asserts.
///
/// `builtin` marks the two the system itself depends on (admin and public):
/// they are not deletable, and the UI has to know that before offering the
/// button rather than after refusing the request.
fn role_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("role", TypeSchema::int()),
        StructField::new("name", TypeSchema::text()),
        StructField::new("description", TypeSchema::text()),
        StructField::new("builtin", TypeSchema::bool()),
    ])
}

fn user_input_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("email", TypeSchema::text()),
        StructField::new("password", TypeSchema::optional(TypeSchema::text())),
        StructField::new("role", TypeSchema::int()),
        // A BCP-47 tag, or null/absent for "whatever the request negotiates".
        // On an **update** the two are not the same: an absent `language` leaves
        // the stored one alone, and an explicit `null` clears it, which is what
        // the form's "Site default" option sends (§16.1).
        StructField::new("language", TypeSchema::optional(TypeSchema::text())),
        StructField::new("extra", TypeSchema::optional(TypeSchema::json())),
    ])
}
