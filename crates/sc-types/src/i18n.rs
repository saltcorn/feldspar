//! Translating a declared settings spec (decision D5).
//!
//! **The server translates everything the server says.** An admin's browser
//! shows two kinds of English: literals in `ui/admin/src/*.tsx`, and text that
//! *arrived from the server* — a `FormField` label out of a stream provider's
//! `config_spec`, a settings section heading, a framework's one-sentence
//! description. The second kind is translated here, against the request's
//! negotiated locale, before it is serialised. The SPA's own catalogue covers
//! the SPA's own literals and nothing else.
//!
//! The alternative — ship the `core` catalogue to the browser and translate
//! there — fails on the first message carrying a value computed server-side, and
//! makes every other client of the API (the MCP server, the `feldspar` CLI, a
//! generated app client) responsible for a job the server already has the locale
//! for.
//!
//! # Why this lives in `sc-types` and not in `sc-i18n`
//!
//! `sc-i18n` is layer 0 so that `sc-types` and `sc-auth` can call `t!`. A
//! function that walks a [`FormField`] therefore cannot live there — the
//! dependency would be a cycle. It lives with the type it walks, which is also
//! where a new field of that type would be noticed.
//!
//! # What is translated, and what is not
//!
//! The **label**, and nothing else. In particular:
//!
//! - A field's **name** is an identifier — it is the key in an [`Attrs`] bag and
//!   the thing a save is validated against — so translating it would rename the
//!   setting.
//! - **Options** are *values*, not labels: `FormField::options(["vite",
//!   "webpack"])` restricts what may be stored, and a translated `"vite"` is a
//!   value that fails its own validation. When options grow labels distinct from
//!   their values, this is where they get translated.
//! - **Help text** is not on `FormField` at all; it hangs off `sc_config`'s
//!   `ConfigDef`, which translates it where it serialises it.
//!
//! [`Attrs`]: crate::Attrs

use sc_i18n::Locale;

use crate::FormField;

/// Translate every label in `spec` in place, against `locale`.
///
/// A label with no catalogue entry comes back **untouched**, which is D1 and D11
/// together: the key is the English, so a missing translation is already the
/// right answer, and a server with no catalogues does one failed lookup per
/// field and changes nothing.
pub fn translate_spec(spec: &mut [FormField], locale: &Locale) {
    for field in spec.iter_mut() {
        field.base.label = sc_i18n::core::translate(locale, &field.base.label, &[]);
        if let Some(section) = &field.section {
            field.section = Some(sc_i18n::core::translate(locale, section, &[]));
        }
        if let Some(sublabel) = &field.sublabel {
            field.sublabel = Some(sc_i18n::core::translate(locale, sublabel, &[]));
        }
        if let Some(message) = &field.pattern_error {
            field.pattern_error = Some(sc_i18n::core::translate(locale, message, &[]));
        }
    }
}

/// [`translate_spec`] for one field — the same rule, where a caller has a single
/// declaration rather than a form.
pub fn translate_field(field: &mut FormField, locale: &Locale) {
    translate_spec(std::slice::from_mut(field), locale);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BasicType;

    #[test]
    fn a_spec_with_no_catalogue_entry_comes_back_untouched() {
        // D1 and D11 together: a label the catalogue has never heard of comes
        // back as itself, because the label already *is* English.
        //
        // The labels here are deliberately not sentences anybody would declare.
        // The extractor reads `.label("…")` where it is written (task 2.3), so
        // a real setting's label — `Source directory`, `Port` — is in the
        // shipped `fr` catalogue and *would* move, which is the facility
        // working rather than this test failing.
        let locale = Locale::parse("fr").unwrap();
        let mut spec = vec![
            FormField::new("source", BasicType::Text)
                .label("A label no shipped catalogue will ever hold"),
            FormField::new("port", BasicType::Int).label("Nor this one"),
        ];
        let before = spec.clone();
        translate_spec(&mut spec, &locale);
        assert_eq!(spec, before);
    }

    #[test]
    fn a_label_the_catalogue_has_is_the_one_that_comes_back() {
        // The other half, and the one the settings screen depends on: a
        // declared label is *data*, so this lookup is the only thing standing
        // between a French admin and an English form.
        let fr = Locale::parse("fr").unwrap();
        let mut spec = vec![FormField::new("port", BasicType::Int).label("Port")];
        translate_spec(&mut spec, &fr);
        assert_eq!(spec[0].base.label, "Port", "`Port` is `Port` in French");

        let mut spec = vec![FormField::new("name", BasicType::Text).label("Site name")];
        translate_spec(&mut spec, &fr);
        assert_eq!(spec[0].base.label, "Nom du site");

        // …and English is untouched, because there is no `en` catalogue to
        // touch it with.
        let mut spec = vec![FormField::new("name", BasicType::Text).label("Site name")];
        translate_spec(&mut spec, &Locale::source());
        assert_eq!(spec[0].base.label, "Site name");
    }

    #[test]
    fn the_name_is_never_touched() {
        let locale = Locale::parse("fr").unwrap();
        let mut field = FormField::new("source", BasicType::Text).label("Source directory");
        translate_field(&mut field, &locale);
        // The identifier a save is keyed by, before and after.
        assert_eq!(field.name(), "source");
    }

    #[test]
    fn options_are_values_and_stay_values() {
        let locale = Locale::parse("fr").unwrap();
        let mut spec = vec![
            FormField::new("bundler", BasicType::Text)
                .label("Bundler")
                .options(["vite", "webpack"]),
        ];
        translate_spec(&mut spec, &locale);
        assert_eq!(
            spec[0].static_options(),
            [serde_json::json!("vite"), serde_json::json!("webpack")]
        );
    }
}
