//! Two-factor authentication: an authenticator app's codes (TOTP, RFC 6238) and
//! the single-use recovery codes that stand in for a lost phone (design §7.2).
//!
//! # What an account holds
//!
//! A row in [`_fd_second_factor`](SECOND_FACTOR_TABLE) per account that has set
//! one up (or is setting one up): the shared secret, **sealed** with the
//! instance key ([`sc_catalog::SecretKey`]) because unlike a password it has to
//! be read back to check a code; when it was turned on (`NULL` while the setup
//! waits for its first code); and the last time-step a code was accepted for, so
//! a code seen over somebody's shoulder cannot be typed again in the same
//! 30 seconds.
//!
//! Recovery codes are rows of [`_fd_recovery_codes`](RECOVERY_CODES_TABLE),
//! stored as an HMAC of the account and the code under a key derived from the
//! instance key ([`crate::secret_types`]). Spending one deletes its row in the
//! statement that finds it, so two requests racing with the same code cannot
//! both sign in. They are shown once, when they are made.
//!
//! # Checking a code
//!
//! Every check of a code takes an [`AttemptPermit`] by value
//! ([`verify_second_factor`], [`finish_second_factor_setup`]): the guess was
//! counted against the account before the code is looked at
//! ([`reserve_attempt`]), and the check settles it — a right code gives it
//! back. A right code is a [`SecondFactorProof`], which is what completes a
//! sign-in ([`crate::PasswordVerified::complete`]) and what replacing the
//! recovery codes takes.
//!
//! Neither table is a foreign key onto `users`, for the reason
//! [`crate::tokens`] gives; deleting an account deletes its rows
//! ([`crate::delete_user`]).
//!
//! # The codes
//!
//! Six digits from HMAC-SHA-1 over a 30-second time-step, with a 20-byte
//! secret, which is what every authenticator app assumes when the `otpauth://`
//! URL says nothing else. A code is accepted for the step before and after the
//! current one too, so a phone whose clock is a few seconds out still works.
//!
//! # Whom it applies to
//!
//! An account that turned it on is asked for a code at every sign-in. A role
//! can **require** it ([`ATTR_REQUIRE_SECOND_FACTOR`] in `_fd_roles.attributes`):
//! an account with that role that has not set one up is taken through setting
//! one up before it gets a session. The server decides this where every
//! session starts (`apply_session`), with [`second_factor_needed`].

use chrono::Utc;
use hmac::{Hmac, Mac};
use sc_catalog::{Catalog, DataField, Table};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{
    Assignment, BinOp, Delete, Expr, Insert, Projection, Select, Source, Statement, UnOp, Update,
    Value,
};
use sc_types::{BasicType, Role, TypeRef};
use uuid::Uuid;

use crate::epoch::{CredentialsChanged, bump_auth_epoch};
use crate::password_tokens::{
    PasswordTokenPurpose, issue_challenge_token, password_token_holder, spend_challenge_token,
};
use crate::secret_types::{ChallengeToken, RecoveryCode, RecoveryCodeHash, TotpSecret};
use crate::signin::{NoSecondFactorNeeded, PasswordVerified, SecondFactorProof};

/// Name of the table holding each account's authenticator secret.
pub const SECOND_FACTOR_TABLE: &str = "_fd_second_factor";
/// Name of the table holding each account's unspent recovery codes.
pub const RECOVERY_CODES_TABLE: &str = "_fd_recovery_codes";

/// The account (`users.id`) — the primary key of [`SECOND_FACTOR_TABLE`], and
/// the owner of a recovery code.
pub(crate) const COL_USER: &str = "user_id";
/// The base32 secret, sealed with the instance key.
pub(crate) const COL_SECRET: &str = "secret";
/// When the second factor was turned on; `NULL` while its setup waits for the
/// first code.
pub(crate) const COL_ENABLED_AT: &str = "enabled_at";
/// The last time-step a code was accepted for.
pub(crate) const COL_LAST_STEP: &str = "last_step";
/// When the row was written.
pub(crate) const COL_CREATED_AT: &str = "created_at";
/// Guesses at the code counted in the current window ([`reserve_attempt`]).
pub(crate) const COL_GUESSES: &str = "guesses";
/// When the current guessing window began.
pub(crate) const COL_GUESSES_SINCE: &str = "guesses_since";

/// How many codes an account may have checked in [`GUESS_WINDOW`] before they
/// are refused until it passes — wrong ones, since a right one is refunded. A
/// code is six digits and lasts 30 seconds, so this makes guessing one hopeless.
pub const MAX_GUESSES: i64 = 5;
/// The window [`MAX_GUESSES`] counts over.
pub(crate) const GUESS_WINDOW: chrono::TimeDelta = chrono::TimeDelta::minutes(15);
/// The keyed hash of the account and a recovery code
/// ([`RecoveryCodeHash`]) — the primary key of [`RECOVERY_CODES_TABLE`].
pub(crate) const COL_CODE_HASH: &str = "code_hash";

/// The role attribute (`_fd_roles.attributes`) that makes a second factor
/// mandatory for every account holding the role.
pub const ATTR_REQUIRE_SECOND_FACTOR: &str = "require_second_factor";

/// How long a code lasts, in seconds.
pub const STEP_SECONDS: i64 = 30;
/// How many digits a code has.
pub const CODE_DIGITS: u32 = 6;
/// How many recovery codes an account is given at a time.
pub const RECOVERY_CODE_COUNT: usize = 10;

fn second_factor_fields() -> Vec<DataField> {
    let text = || TypeRef::Basic(BasicType::Text);
    let uuid = || TypeRef::Basic(BasicType::Uuid);
    let timestamp = || TypeRef::Basic(BasicType::Timestamp);
    vec![
        DataField::plain(COL_USER, uuid()).required().primary_key(),
        DataField::plain(COL_SECRET, text()).required(),
        DataField::plain(COL_ENABLED_AT, timestamp()),
        DataField::plain(COL_LAST_STEP, TypeRef::Basic(BasicType::Int)),
        DataField::plain(COL_CREATED_AT, timestamp()).required(),
        // Nullable, so the bootstrap adds them to an existing table.
        DataField::plain(COL_GUESSES, TypeRef::Basic(BasicType::Int)),
        DataField::plain(COL_GUESSES_SINCE, timestamp()),
    ]
}

fn recovery_code_fields() -> Vec<DataField> {
    vec![
        DataField::plain(COL_CODE_HASH, TypeRef::Basic(BasicType::Text))
            .required()
            .primary_key(),
        DataField::plain(COL_USER, TypeRef::Basic(BasicType::Uuid)).required(),
        DataField::plain(COL_CREATED_AT, TypeRef::Basic(BasicType::Timestamp)).required(),
    ]
}

/// Ensure both tables exist. **Must run after the users table**, which their
/// rows name; [`bootstrap`](crate::bootstrap) does them in that order.
pub(crate) async fn bootstrap_second_factor(catalog: &Catalog) -> Result<Table> {
    catalog
        .bootstrap_table(RECOVERY_CODES_TABLE, &recovery_code_fields())
        .await?;
    catalog
        .bootstrap_table(SECOND_FACTOR_TABLE, &second_factor_fields())
        .await
}

// --- the codes ---------------------------------------------------------------

/// The code for `secret` at time-step `step` (RFC 4226's HOTP, which TOTP is
/// over the time-step).
pub(crate) fn hotp(secret: &[u8], step: u64) -> u32 {
    // `new_from_slice` accepts a key of any length for HMAC.
    let Ok(mut mac) = Hmac::<sha1::Sha1>::new_from_slice(secret) else {
        return u32::MAX;
    };
    mac.update(&step.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = usize::from(digest[digest.len() - 1] & 0x0f);
    let binary = (u32::from(digest[offset] & 0x7f) << 24)
        | (u32::from(digest[offset + 1]) << 16)
        | (u32::from(digest[offset + 2]) << 8)
        | u32::from(digest[offset + 3]);
    binary % 10u32.pow(CODE_DIGITS)
}

/// The time-step `unix_seconds` falls in.
pub(crate) fn time_step(unix_seconds: i64) -> u64 {
    u64::try_from(unix_seconds.div_euclid(STEP_SECONDS)).unwrap_or(0)
}

/// The code for `secret` at `unix_seconds`, as the app shows it: six digits,
/// leading zeros kept.
pub fn totp_at(secret: &TotpSecret, unix_seconds: i64) -> String {
    format!(
        "{:0width$}",
        hotp(secret.bytes(), time_step(unix_seconds)),
        width = CODE_DIGITS as usize
    )
}

/// The time-step `code` is right for at `unix_seconds`, allowing one step of
/// clock drift either way — or `None`.
pub(crate) fn matching_step(secret: &TotpSecret, code: &str, unix_seconds: i64) -> Option<u64> {
    let code = digits_of(code)?;
    let now = time_step(unix_seconds);
    [now, now.saturating_sub(1), now + 1]
        .into_iter()
        .find(|step| constant_time_eq(hotp(secret.bytes(), *step), code))
}

/// A six-digit code, with any spaces or dashes a person typed taken out.
fn digits_of(code: &str) -> Option<u32> {
    let digits: String = code.chars().filter(|c| !matches!(c, ' ' | '-')).collect();
    (digits.len() == CODE_DIGITS as usize && digits.chars().all(|c| c.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
}

fn constant_time_eq(a: u32, b: u32) -> bool {
    (a ^ b) == 0
}

/// A base32 secret, read leniently ([`TotpSecret::from_base32`]).
pub fn decode_secret(secret: &str) -> Result<TotpSecret> {
    TotpSecret::from_base32(secret)
}

/// The `otpauth://` URL an authenticator app is given (as a QR code, or as a
/// link on the phone itself): the account it is for, the issuer it is listed
/// under, and the secret.
pub fn otpauth_url(issuer: &str, account: &str, secret: &str) -> String {
    let label = format!("{issuer}:{account}");
    format!(
        "otpauth://totp/{}?secret={secret}&issuer={}&algorithm=SHA1&digits={CODE_DIGITS}&period={STEP_SECONDS}",
        url_encode(&label),
        url_encode(issuer),
    )
}

fn url_encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// `text` as a QR code, in SVG: what a setup screen shows the app's camera.
/// Black on white whatever the page's theme, because that is what a camera
/// reads best.
pub fn qr_svg(text: &str) -> Result<String> {
    let code = qrcode::QrCode::new(text.as_bytes())
        .map_err(|e| Error::invalid(format!("cannot make a QR code: {e}")))?;
    Ok(code
        .render::<qrcode::render::svg::Color<'_>>()
        .min_dimensions(200, 200)
        .dark_color(qrcode::render::svg::Color("#000000"))
        .light_color(qrcode::render::svg::Color("#ffffff"))
        .build())
}

// --- an account's second factor ----------------------------------------------

/// Where an account's second factor stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SecondFactorStatus {
    /// Whether a code is asked for at sign-in.
    pub enabled: bool,
    /// How many recovery codes are left unspent.
    pub recovery_codes_left: usize,
}

struct Stored {
    secret: TotpSecret,
    enabled: bool,
}

async fn stored(catalog: &Catalog, user_id: Uuid) -> Result<Option<Stored>> {
    let select = Select::from(Source::table(SECOND_FACTOR_TABLE))
        .columns(vec![
            Projection::expr(Expr::col(COL_SECRET)),
            Projection::expr(Expr::col(COL_ENABLED_AT)),
        ])
        .filter(Expr::col(COL_USER).eq(Expr::lit(user_id)))
        .limit(1);
    let found = rows(catalog, Statement::from(select)).await?;
    let Some(row) = found.first() else {
        return Ok(None);
    };
    let Some(Value::Text(sealed)) = row.get(COL_SECRET) else {
        return Err(Error::invalid(format!(
            "{SECOND_FACTOR_TABLE}.{COL_SECRET} of user {user_id} is not text"
        )));
    };
    let secret = decode_secret(&catalog.secret_key()?.open(sealed)?)?;
    let enabled = !matches!(row.get(COL_ENABLED_AT), None | Some(Value::Null));
    Ok(Some(Stored { secret, enabled }))
}

/// Whether `user_id` is asked for a code at sign-in.
pub async fn second_factor_enabled(catalog: &Catalog, user_id: Uuid) -> Result<bool> {
    let select = Select::from(Source::table(SECOND_FACTOR_TABLE))
        .columns(vec![Projection::expr(Expr::col(COL_USER))])
        .filter(
            Expr::col(COL_USER)
                .eq(Expr::lit(user_id))
                .and(Expr::unary(UnOp::IsNotNull, Expr::col(COL_ENABLED_AT))),
        )
        .limit(1);
    Ok(!rows(catalog, Statement::from(select)).await?.is_empty())
}

/// Every account that is asked for a code at sign-in — what the users screen
/// marks.
pub async fn second_factor_users(catalog: &Catalog) -> Result<std::collections::HashSet<Uuid>> {
    let select = Select::from(Source::table(SECOND_FACTOR_TABLE))
        .columns(vec![Projection::expr(Expr::col(COL_USER))])
        .filter(Expr::unary(UnOp::IsNotNull, Expr::col(COL_ENABLED_AT)));
    Ok(rows(catalog, Statement::from(select))
        .await?
        .iter()
        .filter_map(|row| match row.get(COL_USER) {
            Some(Value::Uuid(id)) => Some(*id),
            _ => None,
        })
        .collect())
}

/// Where `user_id`'s second factor stands.
pub async fn second_factor_status(catalog: &Catalog, user_id: Uuid) -> Result<SecondFactorStatus> {
    if !second_factor_enabled(catalog, user_id).await? {
        return Ok(SecondFactorStatus::default());
    }
    let select = Select::from(Source::table(RECOVERY_CODES_TABLE))
        .columns(vec![Projection::expr(Expr::col(COL_CODE_HASH))])
        .filter(Expr::col(COL_USER).eq(Expr::lit(user_id)));
    Ok(SecondFactorStatus {
        enabled: true,
        recovery_codes_left: rows(catalog, Statement::from(select)).await?.len(),
    })
}

/// Whether the role `role` requires a second factor of everybody holding it.
pub async fn role_requires_second_factor(catalog: &Catalog, role: Role) -> Result<bool> {
    Ok(crate::roles::load_role(catalog, role)
        .await?
        .is_some_and(|r| requires_second_factor(&r.attributes)))
}

/// Whether a role's attributes require a second factor. Only `true` does: a
/// junk value must not lock everybody out of an account.
pub fn requires_second_factor(attributes: &sc_types::Attrs) -> bool {
    matches!(
        attributes.get(ATTR_REQUIRE_SECOND_FACTOR),
        Some(serde_json::Value::Bool(true))
    )
}

/// What a sign-in by `user` must do before it gets a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecondFactorNeeded {
    /// Nothing: the account has no second factor and its role requires none.
    /// The proof of that completes the sign-in
    /// ([`crate::SecondFactor::NotNeeded`]).
    No(NoSecondFactorNeeded),
    /// Type a code from the app (or a recovery code).
    Code,
    /// Set one up first: the account's role requires it and it has none.
    Setup,
}

/// What a sign-in by `user` (at the role it is served as) must do before it
/// gets a session.
pub async fn second_factor_needed(
    catalog: &Catalog,
    user: &crate::User,
) -> Result<SecondFactorNeeded> {
    if second_factor_enabled(catalog, user.id()).await? {
        return Ok(SecondFactorNeeded::Code);
    }
    if role_requires_second_factor(catalog, user.role()).await? {
        return Ok(SecondFactorNeeded::Setup);
    }
    Ok(SecondFactorNeeded::No(NoSecondFactorNeeded::new(user.id())))
}

impl SecondFactorNeeded {
    /// Whether a code (or setting one up) is asked for.
    pub fn is_needed(&self) -> bool {
        !matches!(self, SecondFactorNeeded::No(_))
    }
}

/// Start setting up a second factor for `user_id`: a new secret, waiting for
/// its first code ([`finish_second_factor_setup`]). Shown (as base32, for the
/// QR code) by the caller and by nothing else.
///
/// Refused when one is already on: replacing it takes turning it off, which
/// takes a code from it.
pub async fn begin_second_factor_setup(catalog: &Catalog, user_id: Uuid) -> Result<TotpSecret> {
    if second_factor_enabled(catalog, user_id).await? {
        return Err(Error::invalid(
            "two-factor authentication is already on; turn it off before setting it up again",
        ));
    }
    let secret = TotpSecret::generate();
    let sealed = catalog.secret_key()?.seal(&secret.expose_base32())?;
    // A setup that was started and abandoned is replaced, never resumed: its
    // secret may be on a phone that is not this one. Only a pending row: one
    // turned on since the check above stays (security review 2026-11, B6),
    // and the insert below then fails on its primary key.
    let pending = Delete::from(SECOND_FACTOR_TABLE).filter(
        Expr::col(COL_USER)
            .eq(Expr::lit(user_id))
            .and(Expr::unary(UnOp::IsNull, Expr::col(COL_ENABLED_AT))),
    );
    rows(catalog, Statement::from(pending)).await?;
    let insert = Insert::row(
        SECOND_FACTOR_TABLE,
        [COL_USER, COL_SECRET, COL_CREATED_AT]
            .iter()
            .map(|c| (*c).to_owned())
            .collect(),
        vec![
            Expr::lit(user_id),
            Expr::lit(sealed),
            Expr::Lit(Value::Timestamp(Utc::now())),
        ],
    );
    rows(catalog, Statement::from(insert)).await?;
    Ok(secret)
}

/// A second factor turned on ([`finish_second_factor_setup`]).
#[must_use = "the recovery codes are shown once, and the credential change must reach the \
              session store"]
#[derive(Debug)]
pub struct SecondFactorEnabled {
    /// The account's recovery codes — the one time they are shown.
    pub recovery_codes: Vec<RecoveryCode>,
    /// The first code was right: what completes a sign-in waiting on it.
    pub proof: SecondFactorProof,
    /// The account's epoch went up: its other sessions end.
    pub changed: CredentialsChanged,
}

/// Turn the second factor on with the first code from the app the secret of
/// [`begin_second_factor_setup`] went into. `None`: the code is wrong (or there
/// is no setup waiting), and nothing changed. The guess was counted
/// ([`reserve_attempt`]) and is settled here.
pub async fn finish_second_factor_setup(
    catalog: &Catalog,
    permit: AttemptPermit,
    user_id: Uuid,
    code: &str,
) -> Result<Option<SecondFactorEnabled>> {
    permit.require_for(user_id)?;
    let enabled = enable_with(catalog, user_id, code).await;
    permit.settle(catalog, matches!(enabled, Ok(Some(_)))).await;
    let Some(()) = enabled? else {
        return Ok(None);
    };
    let recovery_codes = new_recovery_codes(catalog, user_id).await?;
    Ok(Some(SecondFactorEnabled {
        recovery_codes,
        proof: SecondFactorProof::new(user_id),
        changed: bump_auth_epoch(catalog, user_id).await?,
    }))
}

/// [`finish_second_factor_setup`]'s check and write: `Some` when the code was
/// right and the factor is now on.
async fn enable_with(catalog: &Catalog, user_id: Uuid, code: &str) -> Result<Option<()>> {
    let Some(stored) = stored(catalog, user_id).await? else {
        return Ok(None);
    };
    if stored.enabled {
        return Err(Error::invalid("two-factor authentication is already on"));
    }
    let Some(step) = matching_step(&stored.secret, code, Utc::now().timestamp()) else {
        return Ok(None);
    };
    let update = Update {
        table: SECOND_FACTOR_TABLE.to_owned(),
        assignments: vec![
            Assignment::new(COL_ENABLED_AT, Expr::Lit(Value::Timestamp(Utc::now()))),
            Assignment::new(COL_LAST_STEP, Expr::lit(step_value(step))),
        ],
        filter: Some(
            Expr::col(COL_USER)
                .eq(Expr::lit(user_id))
                .and(Expr::unary(UnOp::IsNull, Expr::col(COL_ENABLED_AT))),
        ),
        returning: vec![Projection::expr(Expr::col(COL_USER))],
    };
    Ok((!rows(catalog, Statement::from(update)).await?.is_empty()).then_some(()))
}

/// Check `code` against `user_id`'s second factor: a code from the app, not
/// used before, or an unspent recovery code (which this spends). A right one
/// is a [`SecondFactorProof`]; the guess was counted ([`reserve_attempt`]) and
/// is settled here — given back when it was right.
///
/// `None` for an account with no second factor on: there is nothing a code
/// could be right for.
pub async fn verify_second_factor(
    catalog: &Catalog,
    permit: AttemptPermit,
    user_id: Uuid,
    code: &str,
) -> Result<Option<SecondFactorProof>> {
    permit.require_for(user_id)?;
    let right = check_code(catalog, user_id, code).await;
    permit.settle(catalog, matches!(right, Ok(true))).await;
    Ok(right?.then(|| SecondFactorProof::new(user_id)))
}

/// [`verify_second_factor`]'s check.
async fn check_code(catalog: &Catalog, user_id: Uuid, code: &str) -> Result<bool> {
    let Some(stored) = stored(catalog, user_id).await? else {
        return Ok(false);
    };
    if !stored.enabled {
        return Ok(false);
    }
    if digits_of(code).is_some() {
        let Some(step) = matching_step(&stored.secret, code, Utc::now().timestamp()) else {
            return Ok(false);
        };
        // Only a step later than the last one accepted: the same code, or an
        // older one, is a replay. One statement, so two requests racing with one
        // code cannot both pass.
        let update = Update {
            table: SECOND_FACTOR_TABLE.to_owned(),
            assignments: vec![Assignment::new(COL_LAST_STEP, Expr::lit(step_value(step)))],
            filter: Some(Expr::col(COL_USER).eq(Expr::lit(user_id)).and(
                Expr::unary(UnOp::IsNull, Expr::col(COL_LAST_STEP)).or(Expr::binary(
                    BinOp::Lt,
                    Expr::col(COL_LAST_STEP),
                    Expr::lit(step_value(step)),
                )),
            )),
            returning: vec![Projection::expr(Expr::col(COL_USER))],
        };
        return Ok(!rows(catalog, Statement::from(update)).await?.is_empty());
    }
    spend_recovery_code(catalog, user_id, &RecoveryCode::presented(code)).await
}

/// One guess at an account's code, **counted before** the code is checked
/// ([`reserve_attempt`]), so that a burst of concurrent requests cannot all be
/// checked before any of them is counted (security review 2026-11, #9 and
/// B2). It is an argument of every check of a code
/// ([`verify_second_factor`], [`finish_second_factor_setup`]), taken by
/// value, which settles it: a right code gives the guess back, a wrong one
/// leaves it counted. A permit dropped unchecked stays counted.
#[must_use = "a permit is spent by checking a code with it"]
#[derive(Debug)]
pub struct AttemptPermit {
    user: Uuid,
    counted: bool,
}

/// The answer to [`reserve_attempt`].
#[derive(Debug)]
pub enum Attempt {
    /// The guess may be checked.
    Permitted(AttemptPermit),
    /// The account has had [`MAX_GUESSES`] in the window: none is checked for
    /// this many more seconds.
    Limited { retry_after_secs: u64 },
}

/// Count one guess at `user_id`'s code, or refuse it when the account has had
/// [`MAX_GUESSES`] in the last [`GUESS_WINDOW`].
///
/// The count is on the account's row of [`SECOND_FACTOR_TABLE`], taken in one
/// statement that refuses as it counts, so two requests cannot both take the
/// last guess and every node shares the budget. An account with no row has no
/// code to guess: the guess is permitted and checks against nothing.
pub async fn reserve_attempt(catalog: &Catalog, user_id: Uuid) -> Result<Attempt> {
    let now = Utc::now();
    let cutoff = now - GUESS_WINDOW;
    let mine = || Expr::col(COL_USER).eq(Expr::lit(user_id));
    // A window that has passed (or never began) starts again with this guess.
    let fresh = || {
        Expr::unary(UnOp::IsNull, Expr::col(COL_GUESSES_SINCE))
            .or(Expr::unary(UnOp::IsNull, Expr::col(COL_GUESSES)))
            .or(Expr::binary(
                BinOp::Lt,
                Expr::col(COL_GUESSES_SINCE),
                Expr::Lit(Value::Timestamp(cutoff)),
            ))
    };
    let case = |fresh_value: Expr, counting: Expr| Expr::Case {
        operand: None,
        arms: vec![sc_query::CaseArm {
            when: fresh(),
            then: fresh_value,
        }],
        else_result: Some(Box::new(counting)),
    };
    let update = Update {
        table: SECOND_FACTOR_TABLE.to_owned(),
        assignments: vec![
            Assignment::new(
                COL_GUESSES,
                case(
                    Expr::lit(1i64),
                    Expr::binary(BinOp::Add, Expr::col(COL_GUESSES), Expr::lit(1i64)),
                ),
            ),
            Assignment::new(
                COL_GUESSES_SINCE,
                case(
                    Expr::Lit(Value::Timestamp(now)),
                    Expr::col(COL_GUESSES_SINCE),
                ),
            ),
        ],
        filter: Some(mine().and(fresh().or(Expr::binary(
            BinOp::Lt,
            Expr::col(COL_GUESSES),
            Expr::lit(MAX_GUESSES),
        )))),
        returning: vec![Projection::expr(Expr::col(COL_USER))],
    };
    if !rows(catalog, Statement::from(update)).await?.is_empty() {
        return Ok(Attempt::Permitted(AttemptPermit {
            user: user_id,
            counted: true,
        }));
    }
    let select = Select::from(Source::table(SECOND_FACTOR_TABLE))
        .columns(vec![Projection::expr(Expr::col(COL_GUESSES_SINCE))])
        .filter(mine())
        .limit(1);
    let found = rows(catalog, Statement::from(select)).await?;
    let Some(row) = found.first() else {
        return Ok(Attempt::Permitted(AttemptPermit {
            user: user_id,
            counted: false,
        }));
    };
    let since = match row.get(COL_GUESSES_SINCE) {
        Some(Value::Timestamp(t)) => *t,
        _ => now,
    };
    let left = (since + GUESS_WINDOW - now).num_seconds().max(1);
    Ok(Attempt::Limited {
        retry_after_secs: u64::try_from(left).unwrap_or(1),
    })
}

impl AttemptPermit {
    /// The account the guess was counted against.
    pub fn user(&self) -> Uuid {
        self.user
    }

    /// Refuse a permit counted against another account than the code is
    /// checked for: that guess would be counted against the wrong budget.
    fn require_for(&self, user_id: Uuid) -> Result<()> {
        if self.user == user_id {
            Ok(())
        } else {
            Err(Error::auth(
                "a guess counted against one account cannot check another's code",
            ))
        }
    }

    /// Settle the guess: given back when the code was `right`, kept counted
    /// otherwise. A refund that fails leaves the guess counted — one fewer
    /// for the window, never one more — and is logged.
    async fn settle(self, catalog: &Catalog, right: bool) {
        if !right || !self.counted {
            return;
        }
        let update = Update {
            table: SECOND_FACTOR_TABLE.to_owned(),
            assignments: vec![Assignment::new(
                COL_GUESSES,
                Expr::binary(BinOp::Sub, Expr::col(COL_GUESSES), Expr::lit(1i64)),
            )],
            filter: Some(
                Expr::col(COL_USER)
                    .eq(Expr::lit(self.user))
                    .and(Expr::binary(
                        BinOp::Gt,
                        Expr::col(COL_GUESSES),
                        Expr::lit(0i64),
                    )),
            ),
            returning: Vec::new(),
        };
        if let Err(e) = rows(catalog, Statement::from(update)).await {
            sc_log::log_warn!(
                "could not give back a right code's guess: {}",
                sc_error::format_chain(&e)
            );
        }
    }
}

/// What turning a second factor off did ([`disable_second_factor`]).
#[must_use = "a second factor turned off is a credential change for the session store"]
#[derive(Debug)]
pub struct SecondFactorRemoved {
    /// Whether there was one, on or being set up.
    pub existed: bool,
    /// The account's epoch went up, when one was on: its other sessions end.
    pub changed: Option<CredentialsChanged>,
}

/// Turn `user_id`'s second factor off, and spend its recovery codes. A factor
/// that was on is a credential change (module docs of [`crate::epoch`]); a
/// setup still waiting for its first code is not.
pub async fn disable_second_factor(
    catalog: &Catalog,
    user_id: Uuid,
) -> Result<SecondFactorRemoved> {
    let (existed, was_on) = remove_second_factor(catalog, user_id).await?;
    let changed = match was_on {
        true => Some(bump_auth_epoch(catalog, user_id).await?),
        false => None,
    };
    Ok(SecondFactorRemoved { existed, changed })
}

/// Delete `user_id`'s second factor and recovery codes: whether there was a
/// row, and whether it was on.
pub(crate) async fn remove_second_factor(catalog: &Catalog, user_id: Uuid) -> Result<(bool, bool)> {
    let codes =
        Delete::from(RECOVERY_CODES_TABLE).filter(Expr::col(COL_USER).eq(Expr::lit(user_id)));
    rows(catalog, Statement::from(codes)).await?;
    let delete = Delete {
        returning: vec![Projection::expr(Expr::col(COL_ENABLED_AT))],
        ..Delete::from(SECOND_FACTOR_TABLE).filter(Expr::col(COL_USER).eq(Expr::lit(user_id)))
    };
    let deleted = rows(catalog, Statement::from(delete)).await?;
    let was_on = deleted
        .iter()
        .any(|row| !matches!(row.get(COL_ENABLED_AT), None | Some(Value::Null)));
    Ok((!deleted.is_empty(), was_on))
}

/// Replace the account's recovery codes with new ones, returned the one time
/// they are shown. Takes a right code ([`verify_second_factor`]), which is
/// what proves the account holds its second factor.
pub async fn regenerate_recovery_codes(
    catalog: &Catalog,
    proof: SecondFactorProof,
) -> Result<Vec<RecoveryCode>> {
    let user_id = proof.user();
    if !second_factor_enabled(catalog, user_id).await? {
        return Err(Error::invalid("two-factor authentication is not on"));
    }
    new_recovery_codes(catalog, user_id).await
}

async fn new_recovery_codes(catalog: &Catalog, user_id: Uuid) -> Result<Vec<RecoveryCode>> {
    let key = catalog.secret_key()?;
    let old = Delete::from(RECOVERY_CODES_TABLE).filter(Expr::col(COL_USER).eq(Expr::lit(user_id)));
    rows(catalog, Statement::from(old)).await?;
    let now = Utc::now();
    let mut codes = Vec::with_capacity(RECOVERY_CODE_COUNT);
    for _ in 0..RECOVERY_CODE_COUNT {
        let code = RecoveryCode::generate();
        let insert = Insert::row(
            RECOVERY_CODES_TABLE,
            [COL_CODE_HASH, COL_USER, COL_CREATED_AT]
                .iter()
                .map(|c| (*c).to_owned())
                .collect(),
            vec![
                Expr::lit(RecoveryCodeHash::new(&key, user_id, &code)?.as_str()),
                Expr::lit(user_id),
                Expr::Lit(Value::Timestamp(now)),
            ],
        );
        rows(catalog, Statement::from(insert)).await?;
        codes.push(code);
    }
    Ok(codes)
}

/// Spend `code` if it is one of `user_id`'s: under its keyed hash, or — a code
/// made before codes were keyed — its old unkeyed one.
async fn spend_recovery_code(
    catalog: &Catalog,
    user_id: Uuid,
    code: &RecoveryCode,
) -> Result<bool> {
    if code.normalised().is_empty() {
        return Ok(false);
    }
    let key = catalog.secret_key()?;
    let mut hashes = Expr::col(COL_CODE_HASH).eq(Expr::lit(
        RecoveryCodeHash::new(&key, user_id, code)?.as_str(),
    ));
    if code.could_be_legacy() {
        hashes = hashes.or(Expr::col(COL_CODE_HASH)
            .eq(Expr::lit(RecoveryCodeHash::legacy(user_id, code).as_str())));
    }
    let delete = Delete {
        returning: vec![Projection::expr(Expr::col(COL_CODE_HASH))],
        ..Delete::from(RECOVERY_CODES_TABLE)
            .filter(hashes.and(Expr::col(COL_USER).eq(Expr::lit(user_id))))
    };
    Ok(!rows(catalog, Statement::from(delete)).await?.is_empty())
}

fn step_value(step: u64) -> i64 {
    i64::try_from(step).unwrap_or(i64::MAX)
}

// --- the challenge between a password and a session --------------------------

/// The token a sign-in whose password was right is given in place of a
/// session, until its second factor is given. It records the account's epoch
/// (a credential changed while the code is being typed ends it) and
/// `remember`, the sign-in's "Remember me", which the session it becomes
/// keeps.
pub async fn issue_second_factor_challenge(
    catalog: &Catalog,
    signing_in: &PasswordVerified,
    remember: bool,
) -> Result<ChallengeToken> {
    issue_challenge_token(
        catalog,
        signing_in.user().id(),
        challenge_purpose(remember),
        signing_in.epoch(),
    )
    .await
    .map(ChallengeToken::presented)
}

/// A sign-in waiting for its second factor ([`second_factor_challenge`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingChallenge {
    /// The account signing in.
    pub user: Uuid,
    /// The sign-in's "Remember me".
    pub remember: bool,
}

/// The sign-in a live challenge is for, **without spending it**: a wrong code
/// keeps the challenge for another try.
pub async fn second_factor_challenge(
    catalog: &Catalog,
    token: &ChallengeToken,
) -> Result<Option<PendingChallenge>> {
    Ok(password_token_holder(catalog, token.expose(), &CHALLENGES)
        .await?
        .map(|(user, purpose)| PendingChallenge {
            user,
            remember: purpose == PasswordTokenPurpose::SecondFactor,
        }))
}

/// A challenge spent ([`spend_second_factor_challenge`]): the sign-in it held,
/// to be completed with its second factor.
#[derive(Debug)]
pub struct SpentChallenge {
    /// The sign-in, at the epoch the challenge recorded.
    pub signing_in: PasswordVerified,
    /// The sign-in's "Remember me".
    pub remember: bool,
}

/// Spend a challenge, once its code was right: `Some` to exactly one of two
/// requests racing with it, and only while the account is there, enabled,
/// and at the epoch the challenge recorded.
///
/// `made_here` is the credential change this same sign-in made a moment ago
/// — the second factor its role requires, set up through the challenge
/// ([`finish_second_factor_setup`]): the one step past the challenge's epoch
/// it may take, and the epoch the sign-in continues at. Any other change
/// since the challenge was issued ends it.
pub async fn spend_second_factor_challenge(
    catalog: &Catalog,
    token: &ChallengeToken,
    made_here: Option<&CredentialsChanged>,
) -> Result<Option<SpentChallenge>> {
    let Some((user_id, purpose, issued_at)) =
        spend_challenge_token(catalog, token.expose(), &CHALLENGES).await?
    else {
        return Ok(None);
    };
    let user = match crate::lookup::load_user(catalog, user_id).await? {
        Some(user) if !user.is_disabled() => user,
        _ => return Ok(None),
    };
    let now = crate::epoch::current_epoch(catalog, user_id).await?;
    let epoch = match made_here {
        None if now == issued_at => now,
        Some(change)
            if change.user() == user_id && change.epoch() == now && issued_at.next() == now =>
        {
            now
        }
        _ => return Ok(None),
    };
    Ok(Some(SpentChallenge {
        signing_in: PasswordVerified::resumed(user, epoch),
        remember: purpose == PasswordTokenPurpose::SecondFactor,
    }))
}

const CHALLENGES: [PasswordTokenPurpose; 2] = [
    PasswordTokenPurpose::SecondFactor,
    PasswordTokenPurpose::SecondFactorUntilBrowserCloses,
];

fn challenge_purpose(remember: bool) -> PasswordTokenPurpose {
    if remember {
        PasswordTokenPurpose::SecondFactor
    } else {
        PasswordTokenPurpose::SecondFactorUntilBrowserCloses
    }
}

async fn rows(catalog: &Catalog, statement: Statement) -> Result<Vec<Row>> {
    catalog
        .primary(crate::INTERNAL_TABLES)
        .query(&statement)
        .await?
        .try_collect()
        .await
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// RFC 6238 appendix B's SHA-1 vectors (its secret is the ASCII
    /// `12345678901234567890`), cut to six digits.
    #[test]
    fn codes_match_the_rfc_test_vectors() {
        let secret =
            TotpSecret::from_base32(&data_encoding::BASE32_NOPAD.encode(b"12345678901234567890"))
                .unwrap();
        for (time, code) in [
            (59, "287082"),
            (1_111_111_109, "081804"),
            (1_111_111_111, "050471"),
            (1_234_567_890, "005924"),
            (2_000_000_000, "279037"),
            (20_000_000_000, "353130"),
        ] {
            assert_eq!(totp_at(&secret, time), code, "at {time}");
        }
    }

    #[test]
    fn a_code_is_good_for_a_step_either_side_and_no_further() {
        let secret = TotpSecret::generate();
        let now = 1_700_000_000;
        let code = totp_at(&secret, now);
        assert_eq!(matching_step(&secret, &code, now), Some(time_step(now)));
        assert!(matching_step(&secret, &code, now + STEP_SECONDS).is_some());
        assert!(matching_step(&secret, &code, now - STEP_SECONDS).is_some());
        assert_eq!(matching_step(&secret, &code, now + 3 * STEP_SECONDS), None);
        let spaced = format!("{} {}", &code[..3], &code[3..]);
        assert!(matching_step(&secret, &spaced, now).is_some());
        assert_eq!(matching_step(&secret, "12345", now), None);
        assert_eq!(matching_step(&secret, "abcdef", now), None);
    }

    #[test]
    fn a_secret_survives_being_copied_by_hand() {
        let secret = TotpSecret::generate().expose_base32();
        assert_eq!(secret.len(), 32, "160 bits of base32");
        let lowered = secret.to_lowercase();
        let spaced: String = lowered
            .chars()
            .enumerate()
            .flat_map(|(i, c)| (i % 4 == 0 && i > 0).then_some(' ').into_iter().chain([c]))
            .collect();
        assert_eq!(
            decode_secret(&spaced).unwrap(),
            decode_secret(&secret).unwrap()
        );
    }

    #[test]
    fn the_otpauth_url_names_the_issuer_and_the_account() {
        let url = otpauth_url("Acme Co", "ann@example.com", "JBSWY3DPEHPK3PXP");
        assert_eq!(
            url,
            "otpauth://totp/Acme%20Co%3Aann%40example.com?secret=JBSWY3DPEHPK3PXP\
             &issuer=Acme%20Co&algorithm=SHA1&digits=6&period=30"
        );
        let svg = qr_svg(&url).unwrap();
        assert!(svg.contains("<svg"), "{svg}");
    }

    #[test]
    fn only_true_requires_a_second_factor() {
        let mut attrs = sc_types::Attrs::new();
        assert!(!requires_second_factor(&attrs));
        attrs.insert(ATTR_REQUIRE_SECOND_FACTOR.into(), serde_json::json!("yes"));
        assert!(!requires_second_factor(&attrs));
        attrs.insert(ATTR_REQUIRE_SECOND_FACTOR.into(), serde_json::json!(true));
        assert!(requires_second_factor(&attrs));
    }
}
