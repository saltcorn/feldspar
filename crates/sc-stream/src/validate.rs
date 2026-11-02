//! Validating a [`Stream`] before it is stored (TODO task 2.2).
//!
//! Every way a stream can be wrong is checked in one place, and checked **on
//! save**, because that is when the admin is standing in front of the form: a
//! provider nothing implements, a broker setting the provider does not declare,
//! a `payload = json` with no keys, a name that cannot be a URL path segment.
//!
//! The cost of not checking is higher here than for a model and higher than for
//! a trigger, and it is worth saying why. A model that fails validation does not
//! fit; a trigger that fails does not fire. A stream that is wrong **connects
//! anyway** — the supervisor subscribes to whatever the row says, and a
//! mis-declared element type means every payload from a healthy broker is
//! counted as malformed and thrown away, silently, at the rate the sensor
//! publishes. So the last moment anybody can act on it cheaply is the form.
//!
//! ## Everything but uniqueness is a pure function
//!
//! [`check_stream`] takes a registry and a value and nothing else, so it is
//! asserted without a database — `sc-viewpattern`'s split, for its reason. Only
//! one check needs to ask the database a question ("does another stream already
//! answer to this name?"), and [`validate_stream`] is that one plus this one.
//! What that buys is that the interesting half — every refusal an admin will
//! actually meet — is a unit test rather than a live-database test.
//!
//! ## Why the name is an identifier and not just "not empty"
//!
//! A stream's name is not only a label. It appears, unescaped, in four places:
//!
//! - the **path segment** of the observe socket, `{mount}/streams/{name}/observe`
//!   and the admin's own (§9, §10);
//! - a trigger's **`channel`**, which is matched as a string (§8);
//! - the generated client's **`observeStream_{name}()`**, which has to be a
//!   legal TypeScript identifier (§10, task 8.3);
//! - MQTT's default **`client_id`**, `feldspar-{name}` (§11).
//!
//! A view's name is allowed spaces because v1 named views `List Books` and a
//! restored backup has to be accepted. A stream has no v1 and no backup — it is
//! new in this milestone — so the strict rule costs an admin one underscore and
//! saves every consumer downstream from deciding whether to percent-encode,
//! quote or mangle. `boiler_temp`, not `boiler temp` and not `boiler/temp`.

use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_types::validate_attrs;

use crate::registry::StreamRegistry;
use crate::store::{STREAMS_TABLE, load_stream_by_name};
use crate::stream::Stream;

/// Check everything about `stream` that can be checked without subscribing to
/// it: [`check_stream`], plus the one question only the database can answer.
///
/// Called by [`save_stream`](crate::save_stream), which calls it **first**.
pub async fn validate_stream(
    catalog: &Catalog,
    registry: &StreamRegistry,
    stream: &Stream,
) -> Result<()> {
    // Ask the provider for the element type **before** checking anything, so
    // that a provider whose declaration lives on the far side of a seam — a
    // module's, over `PollingProvider` — has answered by the time the
    // synchronous checks below read it. For every compiled-in provider this is
    // the same pure call `check_stream` is about to make.
    //
    // A failure here is not reported from this line: `check_stream` makes the
    // same call a moment later and names the stream and the problem the way the
    // form wants to hear it.
    if let Ok(provider) = registry.require(stream.provider.trim()) {
        let _ = provider.resolve_element_type(&stream.configuration).await;
    }
    check_stream(registry, stream)?;

    // The part of the configuration only the schema can judge — a row topic's
    // table, a presence field's column. `check_stream` has already proved the
    // provider is registered, so the lookup cannot fail here.
    if let Ok(provider) = registry.require(stream.provider.trim()) {
        provider
            .validate_in(&stream.configuration, catalog)
            .map_err(|e| Error::invalid(format!("stream `{}`: {e}", stream.name.trim())))?;
    }

    // Unique, because the name *is* the reference: a trigger's channel, an
    // application's `StreamRef` and a socket path all resolve through it, and
    // two streams answering to one name means the flow a trigger fires on
    // depends on which row was read first. The database says so too
    // (`_fd_streams.name` is UNIQUE), and this is the half that produces a
    // sentence rather than a driver's constraint-violation text.
    //
    // A catalog with no `_fd_streams` table has nothing to collide with, which
    // is the state a validation run before `bootstrap_streams` is in.
    let name = stream.name.trim();
    if catalog.get(STREAMS_TABLE)?.is_some()
        && let Some(other) = load_stream_by_name(catalog, name).await?
        && other.id != stream.id
    {
        return Err(Error::invalid(format!(
            "stream `{name}`: a stream with this name already exists; each stream is referenced \
             by its own name"
        )));
    }

    Ok(())
}

/// Everything about a stream that is a function of the stream and the registry
/// — which is everything except whether the name is taken.
///
/// The checks, in the order a form fills them in:
///
/// 1. the name is present and is an identifier (see the module docs);
/// 2. `min_role` is on the role scale;
/// 3. the provider is registered;
/// 4. the configuration is what the provider's
///    [`config_spec`](crate::StreamProvider::config_spec) declares;
/// 5. the element type it yields for that configuration exists and is one the
///    runtime can honour;
/// 6. whatever else the provider itself wants to say.
///
/// The error names the stream, then the problem, because the admin is looking
/// at a list of streams or at the form when they see it.
pub fn check_stream(registry: &StreamRegistry, stream: &Stream) -> Result<()> {
    let name = stream.name.trim();
    check_stream_name(name)?;
    let problem = |msg: String| Error::invalid(format!("stream `{name}`: {msg}"));

    // Roles are a fixed 1–100 scale, and this crate sits below `sc-auth`, so
    // what is checked is the scale rather than the existence of the row in
    // `_fd_roles` — the check a `Trigger`'s and a file store's `min_role` make,
    // for the same reason. The admin form offers the live role list (task 7.3),
    // which is where "40 is not one of your roles" is answerable.
    // The provider must exist. The registry's error already names the
    // alternatives, which is what an admin whose module was uninstalled needs.
    let provider = registry
        .require(stream.provider.trim())
        .map_err(|e| problem(e.to_string()))?;

    // The settings are of the shapes the provider declares, and there are no
    // others: an unknown setting is a typo or a stale configuration, and a
    // stream carrying one would connect with the setting the admin thinks they
    // changed still at its default.
    let spec = provider.config_spec();
    validate_attrs(&spec, &stream.configuration).map_err(|e| problem(e.to_string()))?;

    // What the elements *are*, computed rather than assumed (§3). Asked for
    // here whatever the provider does with `validate` below, because the
    // element type is what the Observe screen renders, what a trigger's
    // `only_if` reads and what a client is typed from — and a stream whose type
    // cannot be computed is one where all three of those are undefined.
    provider
        .element_type(&stream.configuration)
        .and_then(|ty| ty.validate())
        .map_err(|e| problem(e.to_string()))?;

    // Then the part only this provider knows: that a topic filter's wildcards
    // are where MQTT allows them, that a port is a port. The default
    // implementation re-asks for the element type, which is cheap and is the
    // only overlap with the check above.
    provider
        .validate(&stream.configuration)
        .map_err(|e| problem(e.to_string()))?;

    Ok(())
}

/// Refuse a stream name that could not be a path segment, a channel, a
/// TypeScript identifier and an MQTT client id all at once — see the module
/// docs for the four places it appears unescaped.
///
/// ASCII letters, digits and `_`, not starting with a digit. The error names
/// the character it refused, because "invalid name" sends an admin guessing and
/// "`/` cannot appear in a stream name" does not.
pub fn check_stream_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::invalid("a stream needs a name"));
    }
    if let Some(c) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || *c == '_'))
    {
        return Err(Error::invalid(format!(
            "stream name `{}` contains {}; a stream's name is a URL path segment, a trigger's \
             channel and part of a generated function name, so it may hold only letters, digits \
             and `_`",
            name.escape_debug(),
            match c {
                c if c.is_control() => "a control character".to_owned(),
                ' ' => "a space".to_owned(),
                c => format!("`{c}`"),
            }
        )));
    }
    if name.starts_with(|c: char| c.is_ascii_digit()) {
        return Err(Error::invalid(format!(
            "stream name `{name}` starts with a digit; it becomes part of a generated function \
             name, which cannot"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use sc_types::{Attrs, BasicType, FormField, TypeRef};

    use super::*;
    use crate::element::{ElementField, ElementType};
    use crate::provider::{StreamProvider, StreamSink};
    use crate::stream::StreamId;
    use crate::subscription::Subscription;
    use sc_types::Role;

    /// A provider with settings whose element type is a function of them — the
    /// shape every real one has, small enough to assert against.
    ///
    /// `payload` picks the element type: `text` is UTF-8, `broken` is a
    /// configuration the provider refuses outright, and `keyless` is one it
    /// accepts but which yields a type the runtime will not honour. Those last
    /// two are different failures and the validator has to make both.
    struct Configurable;

    #[async_trait]
    impl StreamProvider for Configurable {
        fn name(&self) -> &str {
            "configurable"
        }
        fn description(&self) -> &str {
            "a provider with settings, for tests"
        }
        fn config_spec(&self) -> Vec<FormField> {
            vec![
                FormField::new("topic", TypeRef::Basic(BasicType::Text)).required(),
                FormField::new("payload", TypeRef::Basic(BasicType::Text)),
                FormField::new("port", TypeRef::Basic(BasicType::Int)),
            ]
        }
        fn element_type(&self, config: &Attrs) -> Result<ElementType> {
            match config.get("payload").and_then(|v| v.as_str()) {
                Some("text") => Ok(ElementType::text()),
                Some("keyless") => Ok(ElementType::json([])),
                Some("broken") => Err(Error::invalid(
                    "`payload` is `broken`, which is not a payload this provider reads",
                )),
                _ => Ok(ElementType::json([ElementField::new(
                    "temperature",
                    BasicType::Float,
                )])),
            }
        }
        fn validate(&self, config: &Attrs) -> Result<()> {
            self.element_type(config)?.validate()?;
            // The part only this provider knows: a topic filter's `#` is its
            // last character or it matches nothing.
            let topic = config.get("topic").and_then(|v| v.as_str()).unwrap_or("");
            if topic.contains('#') && !topic.ends_with('#') {
                return Err(Error::invalid(format!(
                    "the topic filter `{topic}` has `#` before its end, where it matches nothing"
                )));
            }
            Ok(())
        }
        async fn subscribe(
            &self,
            _stream: &str,
            _config: &Attrs,
            _sink: Arc<dyn StreamSink>,
        ) -> Result<Subscription> {
            Ok(Subscription::spawn(|mut stop| async move {
                stop.stopped().await;
            }))
        }
    }

    fn registry() -> StreamRegistry {
        let mut registry = StreamRegistry::new();
        registry.register(Arc::new(Configurable)).unwrap();
        registry
    }

    /// A stream that passes, to be damaged one field at a time.
    fn stream() -> Stream {
        Stream::with_id(StreamId(uuid::Uuid::nil()), "boiler", "configurable")
            .config("topic", "house/boiler/#")
    }

    /// What `check_stream` said, as a string.
    fn refusal(stream: &Stream) -> String {
        check_stream(&registry(), stream)
            .expect_err("expected this stream to be refused")
            .to_string()
    }

    #[test]
    fn a_well_formed_stream_passes() {
        check_stream(&registry(), &stream()).unwrap();
        check_stream(&registry(), &stream().config("payload", "text")).unwrap();
        // Including a disabled one: whether it is subscribed to is the
        // supervisor's business, and an admin must be able to save a stream
        // switched off.
        check_stream(&registry(), &stream().enabled(false)).unwrap();
    }

    #[test]
    fn an_unknown_provider_is_refused_naming_the_alternatives() {
        let stream = Stream::with_id(StreamId(uuid::Uuid::nil()), "boiler", "mqtt");
        let err = refusal(&stream);
        assert!(
            err.contains("`boiler`") && err.contains("`mqtt`") && err.contains("configurable"),
            "{err}"
        );
    }

    #[test]
    fn a_setting_the_provider_does_not_declare_is_refused() {
        let err = refusal(&stream().config("broker", "test.mosquitto.org"));
        assert!(
            err.contains("`boiler`") && err.contains("broker") && err.contains("known settings"),
            "{err}"
        );
    }

    #[test]
    fn a_missing_required_setting_and_one_of_the_wrong_type_are_both_refused() {
        let bare = Stream::with_id(StreamId(uuid::Uuid::nil()), "boiler", "configurable");
        let err = refusal(&bare);
        assert!(err.contains("topic"), "{err}");

        let err = refusal(&stream().config("port", "eighteen eighty three"));
        assert!(err.contains("port"), "{err}");
    }

    #[test]
    fn a_configuration_with_no_element_type_is_refused_while_the_admin_is_looking() {
        // The provider refuses the configuration outright.
        let err = refusal(&stream().config("payload", "broken"));
        assert!(err.contains("`boiler`") && err.contains("broken"), "{err}");

        // And a type it computes happily but which the runtime will not honour:
        // a `json` element with no declared keys has no shape for the Observe
        // screen, a trigger or a client to read.
        let err = refusal(&stream().config("payload", "keyless"));
        assert!(
            err.contains("`boiler`") && err.contains("at least one key"),
            "{err}"
        );
    }

    #[test]
    fn the_providers_own_check_runs_too() {
        let stream = Stream::with_id(StreamId(uuid::Uuid::nil()), "boiler", "configurable")
            .config("topic", "house/#/boiler");
        let err = refusal(&stream);
        assert!(err.contains("`#` before its end"), "{err}");
    }

    #[test]
    fn both_ends_of_the_scale_are_roles() {
        // A role off the scale cannot be constructed (`Role`). The two ends are
        // roles, and admin-only (`None`) is not a role off the scale.
        for role in [Some(Role::ADMIN), Some(Role::PUBLIC), None] {
            let mut stream = self::stream();
            stream.min_role = role;
            check_stream(&registry(), &stream).unwrap();
        }
    }

    #[test]
    fn a_name_that_could_not_be_a_path_segment_is_refused_naming_the_character() {
        for (name, expect) in [
            ("boiler/temp", "`/`"),
            ("boiler temp", "a space"),
            ("boiler-temp", "`-`"),
            ("boiler%2f", "`%`"),
            ("boiler.temp", "`.`"),
            ("café", "`é`"),
        ] {
            let err = check_stream_name(name).unwrap_err().to_string();
            assert!(err.contains(expect), "`{name}`: {err}");
        }

        let err = check_stream_name("").unwrap_err().to_string();
        assert!(err.contains("needs a name"), "{err}");

        // A generated `observeStream_1boiler()` would not parse, so a leading
        // digit is refused with the reason rather than with the character.
        let err = check_stream_name("1boiler").unwrap_err().to_string();
        assert!(err.contains("starts with a digit"), "{err}");
    }

    #[test]
    fn an_identifier_is_a_name_whatever_else_it_holds() {
        for name in ["boiler", "boiler_temp", "Boiler2", "_internal", "s1"] {
            check_stream_name(name).unwrap_or_else(|e| panic!("`{name}`: {e}"));
        }
    }

    #[test]
    fn the_name_is_trimmed_and_then_judged() {
        let mut stream = stream();
        stream.name = "  boiler  ".to_owned();
        check_stream(&registry(), &stream).unwrap();
        // Whitespace *inside* a name is not whitespace around one.
        stream.name = " boiler temp ".to_owned();
        assert!(refusal(&stream).contains("a space"));
    }
}
