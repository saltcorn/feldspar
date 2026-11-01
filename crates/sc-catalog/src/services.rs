//! The services a layer above installs on a [`Catalog`](crate::Catalog).
//!
//! The row layer, the schema editor, the read path and the code host all hold a
//! catalog and nothing else, while what they call out to — the trigger
//! dispatcher, the mount registry, the module workers, the live hub, the models,
//! the admin handlers, the formula evaluator — sits in crates above this one.
//! Each of those is a field of [`Services`], typed by the trait this crate
//! declares for it, and the catalog holds one `Services` value.
//!
//! The services are installed after the catalog exists, because most of them
//! hold an `Arc<Catalog>` of their own: a builder that wanted every service at
//! construction would need the catalog before it could make one. That is also
//! why a catalog with services installed is in a reference cycle, and why
//! [`Catalog::teardown`](crate::Catalog::teardown) empties the whole value at
//! once: a field added here is cleared with the others, with nothing to
//! remember.
//!
//! Two services are types of crates above this one that no trait here could
//! describe — `sc-llm`'s host LLM provider and `sc-stream`'s provider registry.
//! They are [`Extensions`], keyed by their own type: the crate that owns the
//! type is the one that sets and reads it, and the type parameter is the key,
//! so a value of the wrong type cannot be stored under it.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

/// The services installed on one catalog. `Default` is "nothing installed",
/// which is what a catalog starts as and what a torn-down one goes back to.
#[derive(Default)]
pub struct Services {
    /// Who observes row writes, by key: `sc-action`'s trigger dispatcher and
    /// the live hub's row-topic re-checks.
    ///
    /// A list, because installing a second kind of observer must not switch the
    /// first off. A write asks every listener whether it observes and hands the
    /// event only to those that do.
    pub(crate) table_events: Vec<(String, Arc<dyn crate::events::TableEvents>)>,
    /// Who observes schema changes: `sc-server`'s mount registry.
    pub(crate) schema_observer: Option<Arc<dyn crate::observer::SchemaObserver>>,
    /// What follows a table being renamed into the rows other crates own.
    pub(crate) rename_hook: Option<Arc<dyn crate::observer::TableRenameHook>>,
    /// The module functions a formula may hoist and a code body may call:
    /// `sc-module`'s worker pool.
    pub(crate) module_functions: Option<Arc<dyn sc_expr::ModuleFnHost>>,
    /// Live updates' publishing side: `sc-live`'s hub.
    pub(crate) live_host: Option<Arc<dyn sc_expr::LiveHost>>,
    /// The models a `predict("…")` and a code body's model handle reach:
    /// `sc-server`'s `ModelServices`.
    pub(crate) model_host: Option<Arc<dyn crate::model_host::ModelHost>>,
    /// The admin operations the administrative tools run (§13.6).
    pub(crate) admin_host: Option<Arc<dyn crate::admin_host::AdminHost>>,
    /// The formula evaluator the read path computes an untranslatable
    /// calculated field with.
    pub(crate) formula_evaluator: Option<Arc<dyn sc_expr::JsEvaluator>>,
    /// What supplies table providers (§8.3): `sc-module`'s worker pool.
    pub(crate) table_providers: Option<Arc<dyn crate::provider::TableProviderHost>>,
    /// Values of crates above this one, keyed by their type.
    pub(crate) extensions: Extensions,
}

/// Values a crate above this one keeps per catalog, one per type.
///
/// For a service whose type no trait in this crate can describe. The type is
/// the key, so [`get`](Extensions::get) answers the value
/// [`set`](Extensions::set) stored or nothing; it cannot answer a value of
/// another type.
#[derive(Default)]
pub struct Extensions(HashMap<TypeId, Arc<dyn Any + Send + Sync>>);

impl Extensions {
    /// Store `value` as the `T` of this catalog, or remove it with `None`.
    pub fn set<T: Send + Sync + 'static>(&mut self, value: Option<Arc<T>>) {
        match value {
            Some(value) => {
                self.0.insert(TypeId::of::<T>(), value);
            }
            None => {
                self.0.remove(&TypeId::of::<T>());
            }
        }
    }

    /// The `T` of this catalog, if one was stored.
    pub fn get<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        let value = Arc::clone(self.0.get(&TypeId::of::<T>())?);
        // Stored under `TypeId::of::<T>()` only by `set::<T>`, so this is a `T`.
        value.downcast::<T>().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    struct Registry(&'static str);

    #[derive(Debug, PartialEq)]
    struct Provider(&'static str);

    #[test]
    fn an_extension_is_keyed_by_its_type() {
        let mut extensions = Extensions::default();
        assert!(extensions.get::<Registry>().is_none());
        extensions.set(Some(Arc::new(Registry("streams"))));
        extensions.set(Some(Arc::new(Provider("llm"))));
        assert_eq!(
            extensions.get::<Registry>().as_deref(),
            Some(&Registry("streams"))
        );
        assert_eq!(
            extensions.get::<Provider>().as_deref(),
            Some(&Provider("llm"))
        );
        extensions.set::<Registry>(None);
        assert!(extensions.get::<Registry>().is_none());
        assert!(extensions.get::<Provider>().is_some());
    }

    #[test]
    fn nothing_installed_is_the_default() {
        let services = Services::default();
        assert!(services.table_events.is_empty());
        assert!(services.schema_observer.is_none());
        assert!(services.model_host.is_none());
        assert!(services.extensions.get::<Registry>().is_none());
    }
}
