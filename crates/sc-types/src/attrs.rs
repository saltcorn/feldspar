//! [`Attrs`]: a bag of configuration values (design §6.1, §9).

use serde_json::Value as Json;

/// A bag of configuration values: a JSON object, always an object.
///
/// This is what a field's type-specific attributes
/// ([`BaseField::attributes`](crate::BaseField), §6.2), a table's or an
/// application's sparse `attributes` (§9), and a configurable extension's
/// `config` (§13.2) all are.
///
/// What may go *in* one is described by a [`FormField`](crate::FormField) per
/// entry (§6.1, §13.3) — the same type that describes a field in any other form,
/// because "what should the admin be asked for this setting?" is the same
/// question a row editor answers.
///
/// It lives in `sc-types` rather than `sc-catalog`, where it started, because it
/// is not a catalog concept: `sc-types` is layer 3 and cannot depend on layer 4,
/// but `FormField` must describe an `Attrs` entry and §6.1 puts them together.
pub type Attrs = serde_json::Map<String, Json>;
