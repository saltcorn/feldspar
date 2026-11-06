//! User, Role, sessions, authentication (layer 5).
//!
//! The MVP fills this in incrementally (TODO Phase 5). So far: the `users` table
//! schema and its one-time [`bootstrap`] into the [`Catalog`](sc_catalog::Catalog),
//! the [`User`] value read back from a row, argon2id password hashing and
//! verification ([`hash_password`], [`verify_password`]), the create-first-user
//! flow ([`create_first_user`], [`any_user_exists`]), credential
//! [`authenticate`]ion, the admin-driven [`create_user`] path, and the
//! [`SessionStore`] behind login/logout — rows in
//! [`_fd_sessions`](SESSIONS_TABLE) behind a per-node cache, so a second
//! application server is a deployment decision rather than a rewrite — and the
//! **API tokens** of [`_fd_api_tokens`](API_TOKENS_TABLE), the bearer credential
//! the administration MCP server authenticates with (§13.6). A token names a
//! user and resolves to one, which is the whole of that server's authentication
//! ([`authenticate_api_token`], [`mint_api_token`]).
//!
//! **Password tokens** ([`_fd_password_tokens`](PASSWORD_TOKENS_TABLE)) are the
//! single-use credential behind an invitation and a forgotten password: an
//! account created with no password ([`invite_user`]) or one whose owner cannot
//! remember it gets a link, and [`redeem_password_token`] spends it.
//!
//! **Two-factor authentication** ([`second_factor`](crate::verify_second_factor)):
//! an authenticator app's codes and recovery codes, asked for after a password
//! when an account turned it on or its role requires it.
//!
//! **Signing in is typestate** ([`signin`](crate::PasswordVerified), security
//! review 2026-11 §2): a session starts from a [`FullyAuthenticated`] alone —
//! a [`PasswordVerified`] completed with a [`SecondFactorProof`] or a
//! [`NoSecondFactorNeeded`], or a named [`SecondFactorWaiver`]. Every
//! credential change raises the account's [`AuthEpoch`], which ends the
//! sessions and sign-ins from before it ([`CredentialsChanged`]).
//!
//! Roles live in [`_fd_roles`](ROLES_TABLE) and `users.role` is a foreign key
//! onto it (§7.1, §9): a role is a row carrying a name and role-specific
//! settings, not a bare integer with a convention attached.
//!
//! **Role grants** ([`role_grants`](ROLE_GRANTS_TABLE), TODO-payments P3) are
//! roles held for a while beside that base role. Every way of loading a user
//! — a session, an API token, a sign-in, [`load_user`] — serves them at their
//! [`effective_role`]: the most privileged of the base role and the grants in
//! force.
//!
//! **LLM credits** ([`credit_buckets`](CREDIT_BUCKETS_TABLE),
//! [`credit_entries`](CREDIT_ENTRIES_TABLE) and
//! [`credit_accounts`](CREDIT_ACCOUNTS_TABLE), TODO-payments P6) are the balance
//! an agent run's model calls are charged to: [`grant_credits`],
//! [`credit_balance`], and the debit the agent loop alone calls
//! ([`debit_credits`]).
//!
//! Neither is created by [`bootstrap`]: they are niche, so a new installation
//! lists `users` alone. A module that needs them declares them (`core_tables`
//! — the payments module does), and the first write made without them creates
//! them; until then every read answers as if they were empty.
//!
//! `unreachable_pub` (security review 2026-11, F-15): an item is `pub` only
//! when another crate calls it. A primitive that skips the guarded entry
//! points stays `pub(crate)`, so a handler elsewhere cannot reach past the
//! guard by importing it.
//!
//! Indexing, string slicing and unchecked arithmetic are warned about here
//! (security review 2026-11, §6; TODO-type-hardening H1.7) when built with
//! `--cfg feldspar_advisory_lints` (the CI `clippy-advisory` job): this crate
//! handles request input, where each is a panic a request can cause. Advisory
//! until the existing hits are fixed, so they do not fail `-D warnings`.
#![warn(unreachable_pub)]
#![cfg_attr(
    feldspar_advisory_lints,
    warn(
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects
    )
)]

/// This crate's own tables' access to the primary database
/// (TODO-code-quality Q3.5): the one `Unchecked::system` its internal stores
/// make, listed in docs/SECURITY_REVIEW_2026-10.md's system-authority
/// inventory. Crate-private, so nothing outside the crate borrows it.
pub(crate) const INTERNAL_TABLES: sc_catalog::Unchecked<'static> = sc_catalog::Unchecked::system(
    "sc-auth: users, sessions, tokens, roles, grants and the credits ledger",
);

mod check_user;
mod create;
mod credits;
mod epoch;
mod first_user;
mod grants;
mod guest;
mod login;
mod lookup;
mod manage;
mod password;
mod password_tokens;
mod roles;
mod second_factor;
mod secret_types;
mod session;
mod signin;
mod tokens;
mod user;
mod users;

pub use check_user::{ATTR_CHECK_USER, set_check_user};
pub use create::{CreatedUser, NewUser, create_user, create_user_with, sign_up_user};
pub use credits::{
    COL_ACCOUNT_DEBT, COL_ACCOUNT_THRESHOLD, COL_ACCOUNT_UPDATED_AT, COL_CREDIT_AMOUNT,
    COL_CREDIT_CREATED_AT, COL_CREDIT_EXPIRES_AT, COL_CREDIT_ID, COL_CREDIT_KEY, COL_CREDIT_KIND,
    COL_CREDIT_NOTE, COL_CREDIT_REMAINING, COL_CREDIT_SOURCE, COL_CREDIT_USER, COL_ENTRY_AGENT,
    COL_ENTRY_BALANCE, COL_ENTRY_BUCKET, COL_ENTRY_COST, COL_ENTRY_INPUT_TOKENS, COL_ENTRY_MODEL,
    COL_ENTRY_OUTPUT_TOKENS, COL_ENTRY_RUN, CREDIT_ACCOUNTS_TABLE, CREDIT_BUCKETS_TABLE,
    CREDIT_DECIMALS, CREDIT_ENTRIES_TABLE, CREDIT_SOURCE_ADMIN, CREDIT_SOURCE_AGENT,
    CREDITS_OWNERSHIP, CreditBalance, CreditBucket, CreditDebit, CreditDebited, CreditEntry,
    CreditGrant, CreditRevoked, CreditRollover, ENTRY_DEBIT, ENTRY_GRANT, ENTRY_LOW_BALANCE,
    ENTRY_REVOKE, ENTRY_ROLLOVER, NewCreditGrant, bootstrap_credits, credit_balance, credits,
    debit_credits, grant_credits, list_credit_buckets, list_credit_entries, revoke_credits,
    set_low_balance_threshold,
};
pub use epoch::{AUTH_EPOCHS_TABLE, AuthEpoch, CredentialsChanged, current_epoch};
pub use first_user::{any_user_exists, create_first_user, first_user_token_matches};
pub use grants::{
    COL_GRANT_CREATED_AT, COL_GRANT_EXPIRES_AT, COL_GRANT_ID, COL_GRANT_NOTE, COL_GRANT_REVOKED_AT,
    COL_GRANT_ROLE, COL_GRANT_SOURCE, COL_GRANT_SOURCE_REF, COL_GRANT_STARTS_AT, COL_GRANT_USER,
    EffectiveRole, GRANT_SOURCE_ADMIN, NewRoleGrant, ROLE_GRANTS_OWNERSHIP, ROLE_GRANTS_TABLE,
    RoleGrant, bootstrap_role_grants, effective_role, find_role_grant, grant_role, grant_role_once,
    list_role_grants, load_role_grant, revoke_role_grant, revoke_role_grant_by_id,
};
pub use guest::{
    DEFAULT_GUEST_SESSION_DAYS, GUEST_SESSIONS_TABLE, GuestSession, StartedGuestSession,
    end_guest_session, guest_session, start_guest_session,
};
pub use login::{authenticate, authenticate_admin};
pub use lookup::{first_user_with_role, load_base_user, load_user, load_user_by_email};
pub use manage::{
    UserChanged, UserUpdate, claim_unproven_account, delete_user, set_user_disabled,
    set_user_password, update_user,
};
pub use password::{
    HashScheme, RANDOM_PASSWORD_LENGTH, hash_password, is_valid_hash, random_password,
    verify_password,
};
pub use password_tokens::{
    COL_CREATED_AT as COL_PASSWORD_TOKEN_CREATED_AT,
    COL_EXPIRES_AT as COL_PASSWORD_TOKEN_EXPIRES_AT, COL_PURPOSE as COL_PASSWORD_TOKEN_PURPOSE,
    COL_TOKEN_HASH as COL_PASSWORD_TOKEN_HASH, COL_USER as COL_PASSWORD_TOKEN_USER,
    INVITE_TTL_DAYS, InviteOutcome, PASSWORD_TOKEN_BYTES, PASSWORD_TOKENS_TABLE,
    PasswordTokenPurpose, RESET_TTL_MINUTES, SECOND_FACTOR_TTL_MINUTES, VERIFY_TTL_HOURS,
    delete_password_tokens_for_user, invite_user, issue_password_token,
    password_token_issued_within, redeem_email_verification, redeem_password_token, user_email,
    user_has_password,
};
pub use roles::{
    COL_ATTRIBUTES as COL_ROLE_ATTRIBUTES, COL_DESCRIPTION as COL_ROLE_DESCRIPTION,
    COL_NAME as COL_ROLE_NAME, ROLES_TABLE, RoleDef, bootstrap_roles, delete_role, list_roles,
    load_role, save_role,
};
pub use second_factor::{
    ATTR_REQUIRE_SECOND_FACTOR, Attempt, AttemptPermit, CODE_DIGITS, MAX_GUESSES, PendingChallenge,
    RECOVERY_CODE_COUNT, RECOVERY_CODES_TABLE, SECOND_FACTOR_TABLE, STEP_SECONDS,
    SecondFactorEnabled, SecondFactorNeeded, SecondFactorRemoved, SecondFactorStatus,
    SpentChallenge, begin_second_factor_setup, decode_secret, disable_second_factor,
    finish_second_factor_setup, issue_second_factor_challenge, otpauth_url, qr_svg,
    regenerate_recovery_codes, requires_second_factor, reserve_attempt,
    role_requires_second_factor, second_factor_challenge, second_factor_enabled,
    second_factor_needed, second_factor_status, second_factor_users, spend_second_factor_challenge,
    totp_at, verify_second_factor,
};
pub use secret_types::{ChallengeToken, RecoveryCode, TotpSecret};
pub use session::{
    APP_TOKEN_PREFIX, CACHE_CAPACITY, CACHE_TTL_SECONDS, COL_AUTH_EPOCH as COL_SESSION_AUTH_EPOCH,
    COL_EXPIRES_AT as COL_SESSION_EXPIRES_AT, COL_TOKEN_HASH as COL_SESSION_TOKEN_HASH,
    COL_USER as COL_SESSION_USER, DEFAULT_TTL_HOURS, LiveSession, SESSIONS_TABLE, SessionEnded,
    SessionListener, SessionScope, SessionStore, changeset_session_key, changeset_token_key,
    create_session, delete_sessions_for_user, delete_sessions_lacking_second_factor,
};
pub use signin::{
    FirstFactor, FullyAuthenticated, NoSecondFactorNeeded, PasswordVerified, SecondFactor,
    SecondFactorProof, SecondFactorWaiver,
};
pub use tokens::{
    API_TOKENS_TABLE, ApiCaller, ApiToken, COL_CREATED_AT as COL_TOKEN_CREATED_AT,
    COL_EXPIRES_AT as COL_TOKEN_EXPIRES_AT, COL_GRANTS as COL_TOKEN_GRANTS, COL_ID as COL_TOKEN_ID,
    COL_LABEL as COL_TOKEN_LABEL, COL_LAST_USED_AT as COL_TOKEN_LAST_USED_AT,
    COL_REVOKED_AT as COL_TOKEN_REVOKED_AT, COL_TOKEN_HASH as COL_API_TOKEN_HASH,
    COL_USER as COL_TOKEN_USER, GRANT_ROUTES_ONLY, LAST_USED_THROTTLE_SECONDS, MintedApiToken,
    NewApiToken, TOKEN_BYTES, TOKEN_PREFIX, authenticate_api_token, authenticate_route_token,
    bearer_secret_hash, list_api_tokens, load_api_token, mint_api_token, new_bearer_secret,
    revoke_api_token,
};
pub use user::{AuthenticatedUser, User, role_of};

/// Held while the role grants or the credits ledger are created on first use,
/// so two first writes at once do not both create them.
static CREATING_TABLES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub use users::{
    COL_ATTRIBUTES, COL_DISABLED, COL_EMAIL, COL_EMAIL_VERIFIED, COL_ID, COL_LANGUAGE,
    COL_PASSWORD_HASH, COL_ROLE, SYSTEM_USER_COLUMNS, USERS_TABLE, bootstrap, is_api_hidden_column,
    is_system_user_column,
};
