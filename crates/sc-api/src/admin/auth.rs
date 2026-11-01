//! The admin API's endpoints: signing in and the first user.
//!
//! `sc-server`'s `handlers::auth` holds their handlers.

use super::*;

/// Declare this domain's endpoints on `set`.
pub(super) fn register(set: &mut EndpointSet) {
    // Whether any user exists yet — drives the create-first-user screen.
    set.register(
        Endpoint::new("authStatus", Method::Get, api().lit("auth/status"))
            .output(TypeSchema::struct_of([
                StructField::new("any_user_exists", TypeSchema::bool()),
                // Whether creating the first user needs the setup link's token
                // (`first_user_token` or `FELDSPAR_FIRST_USER_TOKEN`); only ever true
                // while no user exists.
                StructField::new("first_user_token_required", TypeSchema::bool()),
                StructField::new("current_user", TypeSchema::optional(user_summary_schema())),
                // Which languages this installation serves (§16.1). Here rather
                // than on the settings payload because this is the call the SPA
                // makes before it renders anything, and three screens need the
                // list: the user menu's locale picker, the user form's language
                // select, and whatever chooses the SPA's own catalogue. It is
                // public for the same reason the rest of this response is — the
                // sign-in page is a page too, and it has a language.
                StructField::new("locales", locales_schema()),
                // Whether the signed-in admin should be shown the first-run
                // onboarding dialogs: true for an admin until anyone finishes
                // or skips them (`finishOnboarding`), false otherwise.
                StructField::new("onboarding_pending", TypeSchema::bool()),
            ]))
            .auth(AuthRequirement::Public),
    );

    // The first-run onboarding is done — finished or skipped — so it is not
    // shown again, to this admin or any other.
    set.register(
        Endpoint::new(
            "finishOnboarding",
            Method::Post,
            api().lit("onboarding/finish"),
        )
        .output(TypeSchema::struct_of([StructField::new(
            "done",
            TypeSchema::bool(),
        )]))
        .auth(AuthRequirement::admin()),
    );

    // Create the very first (admin) user. Only valid while no user exists, and
    // when the installation has a first-user token, only with it (`403` else).
    set.register(
        Endpoint::new("createFirstUser", Method::Post, api().lit("first-user"))
            .input(TypeSchema::struct_of([
                StructField::new("email", TypeSchema::text()),
                StructField::new("password", TypeSchema::text()),
                StructField::new("token", TypeSchema::optional(TypeSchema::text())),
            ]))
            .output(user_summary_schema())
            .auth(AuthRequirement::Public),
    );

    set.register(
        Endpoint::new("login", Method::Post, api().lit("login"))
            .input(credentials_schema())
            .output(user_summary_schema())
            .auth(AuthRequirement::Public),
    );

    set.register(
        Endpoint::new("logout", Method::Post, api().lit("logout")).auth(AuthRequirement::LoggedIn),
    );
}

/// The body of a user create or edit — the same shape for both, since a user
/// form is a user form.
///
/// `password` is optional and its blank means two different things by design, one
/// per operation: **on create** it asks for a generated password (returned once,
/// in the response), and **on update** it leaves the stored hash alone. The
/// alternative — making the admin type a password to change a role — is what
/// makes people reuse one.
///
/// `extra` carries the columns the admin has added to the users table (§7.1),
/// keyed by column name; the system's own columns are refused there, since each
/// has its own way in.
/// What languages this installation serves: the default, and the enabled set
/// (§16.1). `enabled` always contains `default`, and a one-element `enabled` is
/// an installation that negotiates nothing.
fn locales_schema() -> TypeSchema {
    TypeSchema::struct_of([
        StructField::new("default", TypeSchema::text()),
        StructField::new("enabled", TypeSchema::array(TypeSchema::text())),
        // The locale **this request** was negotiated into (§16.1, D8), so the
        // SPA loads the catalogue the server has already committed to in
        // `Content-Language` rather than negotiating a second time from the
        // browser's own idea of the order. Two negotiations of one request is
        // how a page ends up with a French navbar and English tables.
        StructField::new("current", TypeSchema::text()),
    ])
}
