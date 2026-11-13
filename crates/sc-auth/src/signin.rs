//! Signing in as typestate (security review 2026-11, §2 and B8;
//! TODO-type-hardening H2.2).
//!
//! A session is started from a [`FullyAuthenticated`] and from nothing else
//! ([`SessionStore::start`](crate::SessionStore::start)). There are two ways to
//! get one, and both say how the second factor was dealt with:
//!
//! - **A sign-in**: a [`PasswordVerified`] — the first factor checked out
//!   ([`authenticate`](crate::authenticate), a password or verification link,
//!   a sign-up, an identity provider; each a [`FirstFactor`]) —
//!   [`complete`](PasswordVerified::complete)d with a [`SecondFactor`]: a
//!   [`SecondFactorProof`] (a right code, from
//!   [`verify_second_factor`](crate::verify_second_factor) or
//!   [`finish_second_factor_setup`](crate::finish_second_factor_setup)), or a
//!   [`NoSecondFactorNeeded`] (from
//!   [`second_factor_needed`](crate::second_factor_needed): the account has
//!   none and its role requires none). Neither can be made outside this crate,
//!   and either is checked against the account it is completed for.
//! - **A named waiver** ([`FullyAuthenticated::waived`]): a session started for
//!   somebody who did not sign in — an admin becoming a user, the first user of
//!   a new installation, the admin UI's move to a new host, an agent's
//!   `call_api` or browser, `feldspar auth token`. Each is a
//!   [`SecondFactorWaiver`] variant, so the full list of ways past the second
//!   factor is `grep SecondFactorWaiver::`.
//!
//! A [`PasswordVerified`] carries the account's [`AuthEpoch`] at the moment
//! the first factor was checked, and a session is refused when the account has
//! changed a credential since (a password reset by an admin while a sign-in
//! waited for its code).

use sc_catalog::Catalog;
use sc_error::{Error, Result};
use uuid::Uuid;

use crate::epoch::{AuthEpoch, current_epoch};
use crate::user::User;

/// How a sign-in's first factor was checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirstFactor {
    /// The account's password ([`authenticate`](crate::authenticate)).
    Password,
    /// A password link (an invitation or a reset), spent with a new password.
    PasswordLink,
    /// An email verification link.
    EmailLink,
    /// A sign-up that has just made the account.
    SignUp,
    /// An identity provider vouched for the account's address.
    IdentityProvider,
}

/// A sign-in whose first factor checked out, waiting for the second-factor
/// decision (module docs).
#[derive(Debug, Clone, PartialEq)]
pub struct PasswordVerified {
    user: User,
    epoch: AuthEpoch,
    how: FirstFactor,
}

impl PasswordVerified {
    /// `user` signed in by `how`, at the account's epoch now. Named, so every
    /// way a first factor is accepted can be listed by grep; it is still only
    /// half a sign-in, and [`complete`](PasswordVerified::complete) is the
    /// other half.
    pub async fn by(catalog: &Catalog, user: User, how: FirstFactor) -> Result<PasswordVerified> {
        let epoch = current_epoch(catalog, user.id()).await?;
        Ok(PasswordVerified { user, epoch, how })
    }

    /// A sign-in resumed from the challenge it was given, at the epoch the
    /// challenge recorded.
    pub(crate) fn resumed(user: User, epoch: AuthEpoch) -> PasswordVerified {
        PasswordVerified {
            user,
            epoch,
            how: FirstFactor::Password,
        }
    }

    /// The account signing in.
    pub fn user(&self) -> &User {
        &self.user
    }

    /// The account's epoch when the first factor was checked.
    pub fn epoch(&self) -> AuthEpoch {
        self.epoch
    }

    /// How the first factor was checked.
    pub fn first_factor(&self) -> FirstFactor {
        self.how
    }

    /// The account, given up: what a caller that only wanted to know the
    /// password was right (re-authentication) keeps.
    pub fn into_user(self) -> User {
        self.user
    }

    /// The other half: the second factor, given or not needed. Refused when
    /// the proof is for another account.
    pub fn complete(self, second: SecondFactor) -> Result<FullyAuthenticated> {
        let for_user = match &second {
            SecondFactor::Proof(proof) => proof.user,
            SecondFactor::NotNeeded(not_needed) => not_needed.user,
        };
        if for_user != self.user.id() {
            return Err(Error::auth(
                "a second factor proved for one account cannot sign in another",
            ));
        }
        Ok(FullyAuthenticated {
            user: self.user,
            epoch: Some(self.epoch),
            how: How::SignedIn(second),
        })
    }
}

impl std::ops::Deref for PasswordVerified {
    type Target = User;

    fn deref(&self) -> &User {
        &self.user
    }
}

/// A right code for `user`'s second factor: made only by
/// [`verify_second_factor`](crate::verify_second_factor) and
/// [`finish_second_factor_setup`](crate::finish_second_factor_setup).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecondFactorProof {
    user: Uuid,
}

impl SecondFactorProof {
    pub(crate) fn new(user: Uuid) -> SecondFactorProof {
        SecondFactorProof { user }
    }

    /// The account the code was right for.
    pub fn user(&self) -> Uuid {
        self.user
    }
}

/// That `user` has no second factor and its role requires none: made only by
/// [`second_factor_needed`](crate::second_factor_needed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoSecondFactorNeeded {
    user: Uuid,
}

impl NoSecondFactorNeeded {
    pub(crate) fn new(user: Uuid) -> NoSecondFactorNeeded {
        NoSecondFactorNeeded { user }
    }
}

/// How a sign-in dealt with its second factor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecondFactor {
    /// A right code.
    Proof(SecondFactorProof),
    /// None was needed.
    NotNeeded(NoSecondFactorNeeded),
}

/// A session started for somebody who did not sign in, past the second
/// factor, and why (module docs). The whole list of such ways is this enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecondFactorWaiver {
    /// An administrator became this user (`becomeUser`): the admin signed in
    /// with their own second factor, and could reset this account's.
    BecomeUser {
        /// The administrator.
        admin: Uuid,
    },
    /// The first user of a new installation, made a moment ago from the setup
    /// screen (or its token link).
    FirstUser,
    /// The admin UI moved to a new host and handed the administrator's
    /// session over with a one-time token minted from that session.
    AdminHandoff,
    /// An agent's `call_api` request to an application, as the user the run
    /// acts for; the session ends with the request.
    CallApi,
    /// An agent's or a checklist's browser on an application, as the user
    /// the run acts for.
    BrowserDriver,
    /// `feldspar auth token`, run by whoever holds the database credentials.
    Cli,
    /// A test's session. Only with sc-auth's `testing` feature, which no
    /// ordinary dependency enables.
    #[cfg(any(test, feature = "testing"))]
    Test,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum How {
    SignedIn(SecondFactor),
    Waived(SecondFactorWaiver),
}

/// An account past both factors (or a named waiver): the only thing a session
/// is started from (module docs).
#[derive(Debug, Clone, PartialEq)]
pub struct FullyAuthenticated {
    user: User,
    /// The epoch the first factor was checked at; `None` for a waiver, which
    /// is issued at whatever epoch the account is at when it starts.
    epoch: Option<AuthEpoch>,
    how: How,
}

impl FullyAuthenticated {
    /// A session for `user` past the second factor, for the reason `waiver`
    /// names.
    pub fn waived(user: User, waiver: SecondFactorWaiver) -> FullyAuthenticated {
        FullyAuthenticated {
            user,
            epoch: None,
            how: How::Waived(waiver),
        }
    }

    /// The account.
    pub fn user(&self) -> &User {
        &self.user
    }

    /// The waiver, when this is one.
    pub fn waiver(&self) -> Option<&SecondFactorWaiver> {
        match &self.how {
            How::Waived(waiver) => Some(waiver),
            How::SignedIn(_) => None,
        }
    }

    /// The epoch the sign-in was checked at, which must still be the
    /// account's when its session starts.
    pub(crate) fn epoch(&self) -> Option<AuthEpoch> {
        self.epoch
    }

    /// The account, given up.
    pub fn into_user(self) -> User {
        self.user
    }
}
