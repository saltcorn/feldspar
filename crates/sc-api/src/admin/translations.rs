//! The admin API's endpoints: translations of an application and of the instance.
//!
//! `sc-server`'s `handlers::translations` holds their handlers.

use super::*;

/// Declare this domain's endpoints on `set`.
pub(super) fn register(set: &mut EndpointSet) {
    // An application's own strings — type B, the admin's, written while they
    // built the application. One read and three writes, because the screen is
    // one table: what the source says, what each locale has, what nobody
    // wrapped, and what is left over.

    // The whole screen in one call. Untyped `messages` and `locales` because a
    // catalogue is a *map* whose keys are English sentences: there is no struct
    // to declare, and an endpoint schema that pretended otherwise would be
    // describing a shape that does not exist.
    set.register(
        Endpoint::new(
            ids::GET_TRANSLATIONS,
            Method::Get,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("translations"),
        )
        .output(TypeSchema::json()),
    );

    // Which locales the application serves, and which one it falls back to.
    // Turning one off does **not** delete its catalogue.
    set.register(
        Endpoint::new(
            ids::SET_APPLICATION_LOCALES,
            Method::Put,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("locales"),
        )
        .input(TypeSchema::struct_of([
            StructField::new("locales", TypeSchema::array(TypeSchema::text())),
            StructField::new("default_locale", TypeSchema::optional(TypeSchema::text())),
        ]))
        .output(TypeSchema::json()),
    );

    // Save one locale's catalogue, whole. Refused — naming the key — for a
    // translation whose placeholders or plural categories differ from its
    // key's, which is the same check the LLM's answers get: the admin typing
    // one by hand is owed the same guarantee.
    set.register(
        Endpoint::new(
            ids::SAVE_TRANSLATIONS,
            Method::Put,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("translations")
                .param("locale", ValueType::Text),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "messages",
            TypeSchema::json(),
        )]))
        .output(TypeSchema::json()),
    );

    // **Translate missing**: fill everything this locale has not got through
    // the configured LLM, and save it. The answer names every message the
    // placeholder check rejected (D9).
    set.register(
        Endpoint::new(
            ids::TRANSLATE_MISSING,
            Method::Post,
            api()
                .lit("applications")
                .param("id", ValueType::Uuid)
                .lit("translations")
                .param("locale", ValueType::Text)
                .lit("fill"),
        )
        .output(TypeSchema::json()),
    );

    // Strings every application serves under its own: the same screen payload,
    // save and fill as an application's, without the application.

    set.register(
        Endpoint::new(
            ids::GET_INSTANCE_TRANSLATIONS,
            Method::Get,
            api().lit("translations"),
        )
        .output(TypeSchema::json()),
    );

    // Save one locale of the shared catalogue, whole: a key left out is
    // removed. Placeholders are checked as an application's are.
    set.register(
        Endpoint::new(
            ids::SAVE_INSTANCE_TRANSLATIONS,
            Method::Put,
            api().lit("translations").param("locale", ValueType::Text),
        )
        .input(TypeSchema::struct_of([StructField::new(
            "messages",
            TypeSchema::json(),
        )]))
        .output(TypeSchema::json()),
    );

    // **Translate missing** for the shared catalogue: every key another locale
    // of it has, filled into this one by the configured LLM.
    set.register(
        Endpoint::new(
            ids::TRANSLATE_INSTANCE_MISSING,
            Method::Post,
            api()
                .lit("translations")
                .param("locale", ValueType::Text)
                .lit("fill"),
        )
        .output(TypeSchema::json()),
    );
}
