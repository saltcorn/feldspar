# Review B: authentication and sessions (changes since f4008729)

Scope: 85d18cd8 + f37869ec (two-factor), ab470064 (email verification), 1e6b9753 / 158face8
(persistent cookie, session lifetime), 38bb1b22 (email_verified, atomic password links),
3dad56d9 (first-user token), d49da199 (add-user), fd0e3b17 (users self-read rule), 42a2a299
(null owner), e2d5d9a4 (csrf / app-agent runs), fd9e1cc2 (onboarding). I also read sc-auth and the
session, login, OAuth and router code around them. This was a read-only review. "Verified" means I
traced the path in code. Nothing was run.

Not re-reported: login timing enumeration (SECURITY_REVIEW #14+, `login.rs:51-58`), and the
other items in both earlier reviews.

---

## B1. Medium: the two-factor routes accept a session on every host, so app-origin script can enroll a second factor on the visitor's account

**Evidence.** The routes are mounted on every host (`sc-server/src/router.rs:466-491`, module docs
`second_factor.rs:5-9`). `signed_in()` (`second_factor.rs:229-252`) refuses only an *app-scope*
token off an app host. An admin-scope token on an application host is accepted, and so is an
app-scope token on its own app host. `setup` (`:487`) and `enable` (`:513`) need nothing beyond
that session. They ask for no password and no current code. `begin_second_factor_setup` returns
the secret to the caller (`sc-auth/src/second_factor.rs:371`), and `enable` returns the recovery
codes. The CSRF double-submit does not stop this, because the script is same-origin and can read
the token.

**Attack.** Shared sign-in is on (Settings → Authentication, cookie `Domain=` the base domain).
An admin who has no second factor visits `shop.example.com`. That application's JavaScript was
written by a non-admin developer or an agent, or carries an XSS. The script POSTs
`/_feldspar/second-factor/setup`, computes a TOTP from the returned `secret`, and POSTs
`/enable`. The admin's account now has a second factor that only the attacker holds. The admin
is locked out at their next sign-in, and the attacker has the 10 recovery codes. The same
works against any ordinary user of any application on the instance, because the second factor is
instance-wide and so outlives the application's own `max_role` cap. This undoes the boundary that
security review #15/#21 drew, where app-origin script must not reach account-level authority.

Status: verified by reading. Not exercised.

**Fix.** Serve the management routes (`status/setup/enable/disable/recovery-codes`) only on the
admin host, or only for a session whose scope matches the host. Require re-authentication
(password or a current code) to enroll.

**Type-system fix.** Make "may change credentials" a distinct extractor that can be built only
from a session whose scope matches the host it arrived on, and only after a recent re-auth:

```rust
/// Proof that this request's session was issued for this host and re-proved
/// the password within the last N minutes. Not constructible elsewhere.
pub struct CredentialAuthority { user: AuthenticatedUser, _p: PhantomData<*const ()> }

impl<S> FromRequestParts<S> for CredentialAuthority { /* scope == host && reauth_at > now-5m */ }

pub async fn begin_second_factor_setup(c: &Catalog, who: &CredentialAuthority)
    -> Result<PendingSecret>;
```

---

## B2. Medium: the per-account limit on wrong codes is in-process, and checked before it is counted (peek, then hit)

**Evidence.** `sc-api/src/throttle.rs:94-112`. `second_factor_attempt` only `peek`s the per-user
bucket. `second_factor_failed` `hit`s it after `check_code` returns `false`
(`second_factor.rs:298-306`, `:597-606`). Between the two sit a DB select, an AES open and an
UPDATE. The limiter is `sc_ratelimit::global()`, a `Mutex<HashMap>` held in each process
(`sc-ratelimit/src/lib.rs:197-243`). So every node has its own budget, which goes against the
multi-node design that the reset-email throttle got right (`password_tokens.rs:211-216`). The
per-IP budget (20/min) is `hit`, but it is per IP. With many source addresses, or an IPv6 /64, or
no peer address, it does not apply.

**Attack.** The attacker has the password and so a challenge. They open about 1,000 concurrent
`POST /verify` requests. All of them pass `peek` before any `hit` lands. Each guess has a 3/10⁶
chance, because three steps are accepted. One burst of 1,000 guesses is a ~0.3% chance; after
that the account is locked for 15 minutes. About 230 bursts (~58 h) give a ~50% chance per node,
and fewer with N nodes. The documented claim that "five in fifteen minutes … makes guessing
hopeless" (`second_factor.rs:39-41`) does not hold under concurrency.

Status: verified by reading.

**Fix.** Count the attempt before checking it (`hit`, then refund on success). Or store the
failure counter in `_fd_second_factor` (`failed_count`, `locked_until`) and update it in the
same statement that checks `last_step`.

**Type-system fix.** Return a reservation from the limiter that must be consumed:

```rust
#[must_use = "a guess must be settled as a success or a failure"]
pub struct GuessPermit { key: String }          // counted at creation
impl GuessPermit { pub fn succeeded(self) { /* refund */ } pub fn failed(self) {} }

pub fn second_factor_attempt(ip, user) -> Result<GuessPermit, Limited>; // hit, not peek
pub async fn verify_second_factor(c, p: GuessPermit, user, code) -> Result<Option<SecondFactorProof>>;
```

`verify_second_factor` takes the permit by value, so a code cannot be checked without one having
been counted.

---

## B3. Medium: a sign-in through an identity provider skips the second factor by default, including for accounts that turned one on and roles that require one

**Evidence.** `sc-server/src/oauth.rs:854-871`. Unless `second_factor_for_identity_providers` is
`true`, which is off by default (`sc-config/src/authentication.rs:34-37, 199-204`), the session is
`StartVerified`. That is decided before `second_factor_needed` is ever consulted. Accounts are
matched **by email** (`account_for`, `oauth.rs:726-783`), not by a stored provider subject. So
the role attribute `require_second_factor` ("mandatory for every account holding the role",
`second_factor.rs:30-35`) is silently not enforced for any account whose address an enabled
provider will vouch for.

**Attack.** An admin role requires 2FA. A generic OIDC provider (Keycloak, Entra, a corporate IdP)
is enabled, and its directory administrator can set a user's email and `email_verified`. Or an
attacker phishes the admin's IdP session, or the IdP has no MFA. Either way they sign in to the
Feldspar admin host as that admin with no TOTP. The setting does not say "this bypasses roles
that require it", and an account that *enabled* 2FA is not told either.

Status: verified by reading. The default is a design choice; it is reported because it
contradicts the role attribute's contract.

**Fix.** Always challenge when the account has 2FA on or its role requires it. Let a provider
waive the challenge only if it is marked as asserting MFA (`amr`/`acr` claims) and the waiver is
recorded. Bind accounts to `(provider, sub)` after the first link.

**Type-system fix.** Remove the bare `StartVerified` (see B8). Have the OAuth path produce a
`SecondFactorWaiver::IdentityProvider { amr_mfa: true }`, which can only be constructed from
verified `amr` claims:

```rust
pub enum SecondFactorWaiver { BecomeUser(Admin), FirstUser(FirstUserClaim), IdpMfa(IdpMfaClaim) }
```

---

## B4. Medium-Low: changing a second factor (enable, role requirement, admin password change) ends no other session

**Evidence.**
- Enabling 2FA through `ENABLE_ROUTE` with `Actor::SignedIn` (`second_factor.rs:545-550`) returns
  the codes and leaves every other session of the account live. Disabling it (`:687-690`) does the
  same.
- Role gaining `require_second_factor`: this is checked only in `apply_session`
  (`router.rs:3160`). Sessions that already exist keep working for their whole lifetime, which
  can be up to `MAX_SESSION_TTL_HOURS` = 1 year, and with a persistent cookie since 1e6b9753.
  `update_user` changing a role also leaves sessions alone (`manage.rs:87-89`; the module says so
  deliberately, `manage.rs:19-23`).
- An admin setting a password through `update_user` / `setRandomPassword` (`handlers/users.rs:458-476`)
  ends no sessions, and leaves the account's outstanding reset links and 2FA **challenges** in
  `_fd_password_tokens` live (`manage.rs:152-165` does not call
  `delete_password_tokens_for_user`). Only `redeem_password_token` cleans up (`password_tokens.rs:248-270`).

**Attack.** A user thinks their account is compromised and turns on 2FA, the usual advice. The
attacker's stolen session or cookie, persistent for up to a year, keeps working. Or: an admin
marks the admin role as requiring 2FA after an incident, and every existing admin session stays
valid without one. Or: an admin resets a password to lock someone out, but a 2FA challenge the
attacker already holds still completes the sign-in if the attacker also has a code or recovery
code.

Status: verified by reading.

**Fix.** On enable, disable and regenerate, end every other session of the user and keep the
current one, by rotating it. On a role or attribute change that turns the requirement on, end the
role's sessions, or stamp `second_factor_at` on the session and recheck it in `user_for`. Make
`set_user_password` delete the user's password tokens.

**Type-system fix.** Make credential changes return an effect that must be applied:

```rust
#[must_use = "a credential change must end the account's other sessions"]
pub struct CredentialsChanged { user: Uuid, keep: Option<SessionId> }
pub async fn finish_second_factor_setup(..) -> Result<Option<(RecoveryCodes, CredentialsChanged)>>;
pub async fn set_user_password(..) -> Result<(User, CredentialsChanged)>;
// HandlerResponse::with(CredentialsChanged) is the only way to consume it.
```

Alternatively, carry an `auth_epoch: i64` on users. Have the session row store the epoch it was
minted at, and have `user_for` reject a mismatch. Every credential change then becomes `epoch += 1`,
in one place.

---

## B5. Low-Medium: recovery codes are ~49 bits under a fast unpeppered hash, so a DB dump yields them offline

**Evidence.** `new_recovery_code` (`sc-auth/src/second_factor.rs:537-546`): 10 symbols from a
30-letter alphabet, which is log₂(30¹⁰) ≈ 49.1 bits, with a modulo bias (256 mod 30 = 16). The
stored value is `SHA-256(user_id ‖ code)` (`:556-565`). There is no key and no slow hash. The
TOTP secret, by contrast, is sealed with the instance key.

**Attack.** Someone with a backup or DB dump who also cracks or phishes the argon2 password can
search one account's 10 codes at about 10¹⁰ SHA-256/s per GPU. The expected time to the first
hit is 2⁴⁹ / 10 / 10¹⁰ ≈ 1.6 h. That defeats the second factor offline, without the instance
key.

Status: verified by arithmetic.

**Fix.** Use HMAC-SHA-256 keyed with the instance `SecretKey` (a pepper), or argon2id, which is
cheap enough for 10 codes. Lengthen the codes to 16 symbols (~78 bits). Use rejection sampling to
remove the bias.

**Type-system fix.** Use `RecoveryCode(Secret<String>)` with no `Display` and no `Debug`. Make
`RecoveryCodeHash::new(&SecretKey, Uuid, &RecoveryCode)` the only constructor of the stored
value, so a bare `Sha256` cannot be written in its place.

---

## B6. Low: `begin_second_factor_setup` can delete an enabled second factor (check-then-act), and a cross-site GET can trigger it

**Evidence.** `second_factor.rs:371-399`. It checks `second_factor_enabled`, then
`DELETE … WHERE user_id = ?` with **no `enabled_at IS NULL`** guard, then inserts a pending row.
`page` (GET, `sc-server/src/second_factor.rs:742-836`, `:813`) calls `setup_for` as a side effect.
The challenge cookie is `SameSite=Lax`, so a cross-site top-level navigation carries it.

**Attack.** Two requests can interleave: `setup` checks (not enabled), then `enable` sets
`enabled_at`, then `setup` deletes the row and inserts a pending one. 2FA is now silently off,
and orphan recovery codes remain. Separately, a cross-site page can navigate the victim to
`PAGE_ROUTE` during their 10-minute required-setup window, which replaces the secret they just
scanned. That is a nuisance-level DoS.

Status: verified by reading. It is a race.

**Fix.** `DELETE … WHERE user_id = ? AND enabled_at IS NULL`. Make the page read-only, so a POST
issues the secret.

**Type-system fix.** Split the table API into `PendingSetup` and `EnabledFactor` handles that come
from one atomic select. `begin_second_factor_setup` takes `NoEnabledFactor` (a proof obtained
`FOR UPDATE`) and can only delete pending rows.

---

## B7. Low: the challenge is not bound to the host or scope it was issued on

**Evidence.** `finish_sign_in` (`second_factor.rs:373-411`) chooses the session's client and
scope from the *verify* request's host (`:392-395`), not from where the password was checked. An
application's REST `login` authenticates **any** role (`rest/mod.rs:1186-1206`), while the admin
host's `login` authenticates admins only (`handlers/auth.rs:113-117`).

**Attack.** A non-admin with 2FA signs in on `app.example.com`, then posts the challenge plus a
code to `admin.example.com/_feldspar/second-factor/verify`. The result is an **admin-scope**
session for a non-admin, which no other path can mint. Today that session reaches only
`register_signed_in` handlers (logout) and the admin-host module routes "as the person calling".
The impact is small, but it breaks the "admin host signs in admins" invariant that #15 relies on.

Status: verified by reading.

**Fix.** Store the scope and app id in the challenge row and reuse them.

**Type-system fix.** `issue_second_factor_challenge(.., scope: SessionScope, app: Option<AppId>)`,
and have `spend_second_factor_challenge` return `PasswordVerified { user, scope, app, remember }`
(B8). The session is then minted from that value, not from the current request.

---

## B8. Structural: "every session starts in `apply_session`" is a convention, not a type

**Evidence.**
- `SessionAction::Start(User)` and `StartVerified { user: User, .. }` are public variants of a
  public enum (`sc-api/src/provider.rs:209-236`). Any handler can put a `User` it got from
  `load_user` into one. `HandlerResponse::start_verified_session` (`handler.rs:401-409`) skips 2FA
  for anything it is given.
- `SessionStore::login / login_in / login_for / login_scoped(User, …)` take a plain `User`. They
  are called outside `apply_session` by `admin_handoff` (`router.rs:1588`), `call_api`
  (`requests.rs:187`), the browser driver (`browser/driver.rs:426`) and the CLI `create_session`
  (`sc-cli/src/auth.rs:561`). Each is legitimate today, but the module doc (`second_factor.rs:13-15`)
  and the comment at `router.rs:3155-3159` claim a single gate.
- `verify_second_factor -> Result<bool>`, `finish_second_factor_setup -> Result<Option<Vec<String>>>`
  and `second_factor_challenge -> Option<(Uuid, bool)>` (the bool is "remember") are easy to
  ignore or mix up.

**Type-system fix (typestate).**

```rust
// sc-auth
pub struct PasswordVerified { user: User, scope: SessionScope, remember: bool }   // from authenticate()
pub struct SecondFactorProof(Uuid);                                                // from verify_second_factor
pub enum SignIn {                                      // sealed: fields private
    NoFactorNeeded(PasswordVerified, NoFactorNeededProof),   // second_factor_needed() == No
    Factor(PasswordVerified, SecondFactorProof),
    Waived(User, SecondFactorWaiver),                       // BecomeUser(Admin) / FirstUser / IdpMfa
}
pub struct FullyAuthenticated(User, SessionScope, bool);
impl SignIn { pub fn finish(self) -> FullyAuthenticated { … } }   // checks proof.0 == user.id()

impl SessionStore { pub async fn login(&self, who: FullyAuthenticated, ttl: Duration) -> Result<SessionToken>; }

// sc-api
pub enum SessionAction { Keep, Start(SignInRequest /* = PasswordVerified */), End, … }
```

`authenticate` returns `Option<PasswordVerified>`, not `Option<User>`. `apply_session` is then the
only thing that can turn a `PasswordVerified` into a `FullyAuthenticated`, because it holds the
code that calls `second_factor_needed`. The handoff, `call_api` and the browser driver need their
own named `SecondFactorWaiver` variants, which makes every bypass greppable and reviewable.
`SessionToken`, `ChallengeToken`, `TotpSecret` and `RecoveryCode` should be newtypes over
`Secret<String>` with constant-time `PartialEq`, no `Debug`/`Display` and `Zeroize`. Today they
are `String` and `Vec<u8>`, for example `Actor::Challenge { token: String }`
(`second_factor.rs:326-330`) and `Stored { secret: Vec<u8> }`.

---

## B9. Low: notes

- **The challenge response reveals the account.** `challenge_for` returns `user_summary_json`
  (id, email, role, `email_verified`) before the second factor (`second_factor.rs:140-145`). A
  password-only party learns the user id and whether the account is an admin. Return only
  `{second_factor:{token, setup}}`.
- **A grant can lift the requirement.** The role requirement is per exact *effective* role
  (`second_factor_needed` uses `user.role()` after grants, `sc-auth/src/second_factor.rs:352-365`).
  A user whose base role requires 2FA but who holds a grant to a more privileged role that does
  not escapes the requirement. Consider "required if the base or the effective role requires it",
  or make the attribute hierarchical.
- **TOFU enrollment.** An attacker with the password of an account whose role requires 2FA but
  which has not set one up enrolls *their* authenticator through the `setup:true` challenge
  (`second_factor.rs:498-574`). That is inherent to "enroll at sign-in", but the docs should say
  so. Consider requiring an emailed confirmation for first enrollment.
- **The email-verification link is a 48-hour sign-in link** (`rest/verify.rs:247-262`,
  `VERIFY_TTL_HOURS=48`). It does go through the challenge. Consider not starting a session, or
  shortening the TTL.
- **The OAuth binding cookie is compared with `==`** (`oauth.rs:806`). It is random and
  single-use, so this is hardening only. Use the `tokens_match` from `security.rs`.
- **First user.** `create_first_user` re-checks inside a READ COMMITTED transaction
  (`first_user.rs:97-107`), so two concurrent requests on two nodes can both insert an admin. The
  doc admits this. Each creator needed the token anyway. A `pg_advisory_xact_lock` or `LOCK TABLE
  users IN SHARE ROW EXCLUSIVE MODE` closes it. `createFirstUser` token attempts are not throttled.
- **e2d5d9a4.** App-agent runs are no longer tied to an application (`app_agents.rs:388-399`).
  Any application that exposes the same agent can list and continue a user's conversations that
  were started in another application. If those applications have different authors, application
  B's script reads A's transcripts. This is cross-application data exposure, low.
- **42a2a299.** Tables with RLS keep the old null-owner-leaking policy until they are re-saved
  (CHANGELOG). The project's rules call for a migration that regenerates the RLS policies
  (`sc-migrate`) rather than an operator step.

---

## Checked and sound

- **The challenge gate covers the password paths.** These all go through `apply_session`'s
  challenge: the admin `login` (`handlers/auth.rs:122`), the app REST `login`
  (`rest/mod.rs:1194`), `signup` (`:1317`), `set-password` (reset and invite,
  `rest/password.rs:353`), `verify-email` (`rest/verify.rs:251`), Saltcorn-UI `/auth/login`
  (`framework.rs:863,1666`), and OAuth when `ask` is set. Password reset does not clear 2FA.
- **Challenge token.** 256-bit, stored as SHA-256, 10-minute TTL, purpose-filtered on read and
  spend (`password_tokens.rs:313-417`). Spent by an atomic `DELETE … RETURNING`. A challenge
  cannot be used as a reset or verify token, and a reset or verify token cannot be used as a
  challenge.
- **TOTP.** RFC 6238 vectors pass, ±1 step. Replay is prevented by an atomic
  `UPDATE … WHERE last_step < step RETURNING`. The secret is 160-bit from OsRng and sealed with
  the instance key. Disable and regenerate require a current code. The role-requirement check
  happens before a code is spent.
- **Recovery codes.** Single use, through an atomic `DELETE … RETURNING` scoped to the user. They
  are hashed per user. Their strength is covered in B5.
- **Session fixation.** Every start mints a fresh token, and the arriving token is logged out
  (`router.rs:3190-3210`). `becomeUser` is a swap. Tokens are stored as SHA-256 with the scope
  prefix inside the hash.
- **Password links.** `redeem_password_token` is one transaction: spend, set the hash, set
  `email_verified`, delete the other tokens, end sessions (38bb1b22). Verify tokens are refused
  as password links.
- **Email verification cap.** `cap_role` is applied in `admit` (`sc-app/src/access.rs:118-135`).
  `app_caller` caps the `AuthenticatedUser` it hands to handlers (`router.rs:2494-2498`). The
  live socket re-admits (`live.rs:1720`). OAuth claims an unproven account and wipes the
  password, tokens and sessions (`oauth.rs:774-781`).
- **First-user token.** The comparison hashes both sides and runs in constant time
  (`first_user.rs:37-52`). The config definition is `secret`. Clear-all keeps it.
- **Users self-read (fd0e3b17).** `read_rule` ORs `_read && id === user.id` on every read path.
  Writes still use `ownership`. `password_hash` is hidden from reads and filters
  (`user_rows.rs:66-91`, `query_string.rs:163`). RLS folds `_read` per operation, so the clause
  grants no UPDATE/DELETE. With 42a2a299, an anonymous caller's null `user.id` matches nothing.
- **CSRF (e2d5d9a4).** Any request carrying a session cookie now needs the token, even on public
  endpoints. That is stricter than before.
- **add-user (d49da199).** It no longer reads stdin implicitly and reads one line from fd 0. No
  password lands in argv unless `--password=VALUE` is used, which the docs describe.
