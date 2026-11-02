//! [`RichType`] and the rich-type **registry** (design §6.1).
//!
//! A rich type is one Saltcorn understands beyond "a value of the right scalar
//! family": it declares typed attributes (a `max_length`, a set of select
//! options), validates a value against them, and names the SQL types it can sit
//! on. A basic type ([`BasicType`](crate::BasicType)) is any DB type *not* mapped
//! to a rich one — usable, but only through the catch-all path. This module is
//! the "rich" half; [`TypeRef`](crate::TypeRef) is what a field carries and picks
//! between the two.
//!
//! ## The registry is the same "settings as data" move, once more
//!
//! Rich types are registered by name exactly as file-store backends
//! (`registered_backends`) and frameworks (`registered_frameworks`) are, and for
//! the same reason: the admin UI (§3.3's `listFieldTypes`) must render an
//! attribute form for a type it knows nothing about — including one arriving
//! later from a plugin through `sc-code` — so a type declares its attributes as
//! [`FormField`]s, the one vocabulary §6.2 gives every configurable extension
//! point, and the UI renders whatever it is handed. A new rich type is a change
//! *here* ([`builtin_rich_types`]), never in the admin UI.
//!
//! §2.1 built the mechanism; §2.2 fills it with the initial types (`String`,
//! `Integer`, `Email`), which live in [`crate::rich_types`] and are handed to the
//! registry by [`builtin_rich_types`].
//!
//! ## `fieldviews()` is not here
//!
//! §6.1's trait also lists `fieldviews()`. Those are React components (§6.3) and
//! are out of scope for this milestone, so the trait ships without them rather
//! than with a placeholder that has no consumer — the same rule the rest of
//! `sc-types` follows for `fieldview` and `visibility` on [`FormField`].

use std::sync::{Arc, LazyLock};

use sc_error::{Error, Result};
use sc_query::Value;

use crate::{Attrs, FormField};

/// A type Saltcorn understands: typed attributes, validation, and the SQL types
/// it can be stored as (design §6.1).
///
/// **Invariant:** [`sql_types`](RichType::sql_types) must be non-empty. A rich
/// type that maps from no SQL type at all could never be given to a column, and
/// [`RichTypeRef::sql_type`] relies on there being a first one to use for DDL.
///
/// `fieldviews()` (§6.1) is deferred with the rest of §6.3 — see the module docs.
pub trait RichType: Send + Sync {
    /// The registry key and the name a field's [`TypeRef`](crate::TypeRef)
    /// stores. Stable — it is what the `_fd_fields` overlay persists (§3.2).
    fn name(&self) -> &str;

    /// The type's attributes, as the form the admin UI renders to configure a
    /// field of this type (`max_length`, select `options`, `min`/`max`, …).
    ///
    /// Empty for a type that takes no attributes (`Email`), which is a valid and
    /// deliberate answer — the same way an extension with no settings returns an
    /// empty spec.
    fn attributes(&self) -> &[FormField];

    /// Validate a value against this type **and** the attributes a particular
    /// field set for it — the length limit, the numeric range, the option set.
    ///
    /// Takes the [`Attrs`] bag beside the value because that is where a field's
    /// per-column configuration lives; a type with no attributes ignores it.
    fn validate(&self, value: &Value, attrs: &Attrs) -> Result<()>;

    /// Bring a field's attributes into the shape they are stored in, before
    /// they are saved — so an attribute an admin may write in a friendlier form
    /// (`String`'s `options` as `Red, Green, Blue`) is stored, and read by
    /// everything downstream, in one canonical form. An attribute that cannot
    /// be read is an error, rejecting the save.
    ///
    /// The default leaves the attributes as they are.
    fn normalize_attrs(&self, _attrs: &mut Attrs) -> Result<()> {
        Ok(())
    }

    /// The backend SQL types this rich type can sit on. The first is canonical —
    /// the one [`RichTypeRef::sql_type`] emits for DDL. Must be non-empty (see
    /// the trait invariant).
    ///
    /// Introspection never consults this to *guess* a rich type from a column
    /// (§2.1's decision — see [`TypeRef::from_sql_type`](crate::TypeRef::from_sql_type)).
    /// It is used the other way: §3.2 checks that a column an overlay calls rich
    /// has a SQL type this list contains.
    fn sql_types(&self) -> &[&str];
}

/// The registered rich types — the built-ins from [`crate::rich_types`].
///
/// That module is the single place a rich type is enumerated: adding one there
/// makes it immediately listed, spec-resolvable, and referable by every
/// consumer, with no other change.
fn builtin_rich_types() -> Vec<Arc<dyn RichType>> {
    crate::rich_types::builtin_rich_types()
}

/// The process-wide registry, built once from [`builtin_rich_types`].
static REGISTRY: LazyLock<Vec<Arc<dyn RichType>>> = LazyLock::new(builtin_rich_types);

/// Look a rich type up by name in an explicit list — the resolution logic, split
/// out so it can be tested against a list of test types without touching the
/// global [`REGISTRY`].
fn find(types: &[Arc<dyn RichType>], name: &str) -> Option<Arc<dyn RichType>> {
    types.iter().find(|t| t.name() == name).cloned()
}

/// The names of every registered rich type — what §3.3's `listFieldTypes` offers
/// the admin, and the list an error message names when a type is not found.
pub fn registered_rich_types() -> Vec<String> {
    REGISTRY.iter().map(|t| t.name().to_owned()).collect()
}

/// Resolve a registered rich type by name, or `None` if nothing registers it.
pub fn rich_type(name: &str) -> Option<Arc<dyn RichType>> {
    find(&REGISTRY, name)
}

/// The attribute spec of the rich type registered under `name` — the registry
/// lookup the admin UI uses to render a field's attribute form (§3.3).
///
/// An unknown name is a configuration error naming the registered types, not a
/// type with no attributes — mirroring `backend_config_spec` and
/// `framework_config_spec`: a field whose type nothing implements cannot be
/// configured, and saying so beats accepting it and failing opaquely later.
pub fn rich_type_config_spec(name: &str) -> Result<Vec<FormField>> {
    rich_type(name)
        .map(|t| t.attributes().to_vec())
        .ok_or_else(|| unknown_rich_type(name))
}

/// The error a name that no rich type registers produces, naming what is
/// registered so the admin can see the typo.
fn unknown_rich_type(name: &str) -> Error {
    let registered = registered_rich_types();
    Error::config(if registered.is_empty() {
        format!("unknown rich type `{name}`; no rich types are registered")
    } else {
        format!(
            "unknown rich type `{name}`; the registered rich types are {}",
            registered.join(", ")
        )
    })
}

/// A field's reference to a rich type: a resolved handle to the registered
/// [`RichType`], carried by [`TypeRef::Rich`](crate::TypeRef::Rich).
///
/// Like a `FrameworkRef` names its framework, a field is *stored* as a rich
/// type's name (in the `_fd_fields` overlay, §3.2); a `RichTypeRef` is that name
/// already resolved against the registry, so [`sql_type`](RichTypeRef::sql_type),
/// [`name`](RichTypeRef::name) and [`validate`](RichTypeRef::validate) can
/// delegate to the type without a lookup each time. Build one with
/// [`resolve`](RichTypeRef::resolve).
#[derive(Clone)]
pub struct RichTypeRef(Arc<dyn RichType>);

impl RichTypeRef {
    /// Wrap an already-resolved rich type.
    pub fn new(ty: Arc<dyn RichType>) -> RichTypeRef {
        RichTypeRef(ty)
    }

    /// Resolve a rich type name against the registry, erroring if nothing
    /// registers it (naming what does).
    pub fn resolve(name: &str) -> Result<RichTypeRef> {
        rich_type(name)
            .map(RichTypeRef)
            .ok_or_else(|| unknown_rich_type(name))
    }

    /// The type's stable name — the value the overlay persists.
    pub fn name(&self) -> &str {
        self.0.name()
    }

    /// The canonical Postgres type for DDL: the first of the type's
    /// [`sql_types`](RichType::sql_types).
    ///
    /// A well-formed rich type declares at least one (the trait invariant,
    /// guarded by a registry test), so the fallback is unreachable for any
    /// registered type; `text` stands in only for a broken impl, keeping this
    /// infallible rather than panicking mid-DDL.
    pub fn sql_type(&self) -> &str {
        self.0.sql_types().first().copied().unwrap_or("text")
    }

    /// The rich type's declared attribute spec.
    pub fn attributes(&self) -> &[FormField] {
        self.0.attributes()
    }

    /// Validate a value against the type and a field's attributes.
    pub fn validate(&self, value: &Value, attrs: &Attrs) -> Result<()> {
        self.0.validate(value, attrs)
    }

    /// Bring a field's attributes into their stored shape (see
    /// [`RichType::normalize_attrs`]).
    pub fn normalize_attrs(&self, attrs: &mut Attrs) -> Result<()> {
        self.0.normalize_attrs(attrs)
    }

    /// Borrow the underlying [`RichType`].
    pub fn rich_type(&self) -> &dyn RichType {
        self.0.as_ref()
    }
}

/// Identity is the name: the registry keys on it and holds one type per name, so
/// two references naming the same type are the same reference. This is what lets
/// [`TypeRef`](crate::TypeRef) derive `PartialEq`/`Eq` despite holding a trait
/// object.
impl PartialEq for RichTypeRef {
    fn eq(&self, other: &RichTypeRef) -> bool {
        self.0.name() == other.0.name()
    }
}

impl Eq for RichTypeRef {}

impl std::fmt::Debug for RichTypeRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("RichTypeRef").field(&self.0.name()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BasicType;
    use serde_json::json;

    /// A rich type used only to exercise the mechanism: `text` with a
    /// `max_length` attribute, standing in for §2.2's real `String` so §2.1 can
    /// test the trait, the registry and delegation without pre-empting that
    /// section's content.
    struct TestString {
        attributes: Vec<FormField>,
    }

    impl TestString {
        fn new() -> TestString {
            TestString {
                attributes: vec![FormField::new("max_length", BasicType::Int)],
            }
        }
    }

    impl RichType for TestString {
        fn name(&self) -> &str {
            "test_string"
        }
        fn attributes(&self) -> &[FormField] {
            &self.attributes
        }
        fn validate(&self, value: &Value, attrs: &Attrs) -> Result<()> {
            let Value::Text(s) = value else {
                if value.is_null() {
                    return Ok(());
                }
                return Err(Error::invalid("test_string: not text"));
            };
            if let Some(max) = attrs.get("max_length").and_then(|v| v.as_i64()) {
                if s.chars().count() as i64 > max {
                    return Err(Error::invalid(format!(
                        "test_string: longer than {max} characters"
                    )));
                }
            }
            Ok(())
        }
        fn sql_types(&self) -> &[&str] {
            &["text", "varchar"]
        }
    }

    #[test]
    fn a_rich_type_declares_attributes_sql_types_and_validates() {
        let t = TestString::new();
        assert_eq!(t.name(), "test_string");
        assert_eq!(t.sql_types(), ["text", "varchar"]);
        assert_eq!(t.attributes().len(), 1);

        // No attribute: any text passes.
        assert!(
            t.validate(&Value::Text("hello".into()), &Attrs::new())
                .is_ok()
        );
        // Non-text is rejected; null is not (nullability is a field concern).
        assert!(t.validate(&Value::Int(1), &Attrs::new()).is_err());
        assert!(t.validate(&Value::Null, &Attrs::new()).is_ok());

        // The attribute drives validation.
        let mut attrs = Attrs::new();
        attrs.insert("max_length".to_owned(), json!(3));
        assert!(t.validate(&Value::Text("abc".into()), &attrs).is_ok());
        let err = t
            .validate(&Value::Text("abcd".into()), &attrs)
            .unwrap_err()
            .to_string();
        assert!(err.contains("3"), "{err}");
    }

    #[test]
    fn find_resolves_by_name_and_misses_cleanly() {
        let types: Vec<Arc<dyn RichType>> = vec![Arc::new(TestString::new())];
        assert!(find(&types, "test_string").is_some());
        assert!(find(&types, "nope").is_none());
    }

    #[test]
    fn rich_type_ref_delegates_to_the_type() {
        let r = RichTypeRef::new(Arc::new(TestString::new()));
        assert_eq!(r.name(), "test_string");
        // Canonical SQL type is the first declared.
        assert_eq!(r.sql_type(), "text");
        assert_eq!(r.attributes().len(), 1);

        let mut attrs = Attrs::new();
        attrs.insert("max_length".to_owned(), json!(2));
        assert!(r.validate(&Value::Text("too long".into()), &attrs).is_err());
        assert!(r.validate(&Value::Text("ok".into()), &attrs).is_ok());
    }

    #[test]
    fn rich_type_ref_identity_is_the_name() {
        let a = RichTypeRef::new(Arc::new(TestString::new()));
        let b = RichTypeRef::new(Arc::new(TestString::new()));
        assert_eq!(a, b);
        assert_eq!(format!("{a:?}"), r#"RichTypeRef("test_string")"#);
    }

    #[test]
    fn the_builtin_registry_lists_2_2s_types_and_reports_unknown_ones() {
        // §2.2 populated the registry; the mechanism resolves what it holds and
        // names what it holds when asked for something it does not.
        let names = registered_rich_types();
        for expected in ["string", "integer"] {
            assert!(names.iter().any(|n| n == expected), "missing {expected}");
        }
        assert!(rich_type("integer").is_some());
        assert!(rich_type("nope").is_none());
        assert!(RichTypeRef::resolve("string").is_ok());

        let err = rich_type_config_spec("nope").unwrap_err().to_string();
        assert!(err.contains("nope"), "{err}");
        assert!(
            err.contains("string"),
            "should name what is registered: {err}"
        );
    }

    #[test]
    fn every_registered_rich_type_declares_at_least_one_sql_type() {
        // The invariant `RichTypeRef::sql_type` relies on. Vacuously true now;
        // the guard that keeps §2.2's additions honest.
        for t in super::REGISTRY.iter() {
            assert!(
                !t.sql_types().is_empty(),
                "rich type `{}` declares no sql types",
                t.name()
            );
        }
    }
}
