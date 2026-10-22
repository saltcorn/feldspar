//! Typed `run…` methods: an exposed trigger's endpoint typed from what its
//! action declares it takes and answers ([`Action::payload_type`],
//! [`Action::returns_type`]).
//!
//! A trigger's endpoint used to be `json` in and `json` out, because the body
//! *is* the event's payload, whatever the action reads out of it. An action
//! that knows its own payload and answer — a module's `mail_test_account`
//! answering `{ imap, smtp, error }` — can now say so, in the type language a
//! code query's `returns` already uses ([`super::returns`]). The payload is the
//! **declared part of an open object**, as a code query's parameters are: the
//! action still reads whatever arrives, so the client may send more.
//!
//! Like `returns`, both are a contract for the client, not a filter: nothing is
//! checked against them at run time.

use sc_action::Action;

use super::returns::parse_returns;
use crate::schema::TypeSchema;

/// What an action declares, lowered to the schemas a client is generated from.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ActionTypes {
    /// The payload's declared fields: always a non-empty struct.
    pub payload: Option<TypeSchema>,
    /// The answer.
    pub returns: Option<TypeSchema>,
}

impl ActionTypes {
    /// Whether the action declared nothing.
    pub fn is_empty(&self) -> bool {
        self.payload.is_none() && self.returns.is_none()
    }
}

/// Lower `action`'s declarations. The error names which declaration is wrong
/// and where, in [`parse_returns`]' words.
pub fn action_types(action: &dyn Action) -> Result<ActionTypes, String> {
    let payload = match action.payload_type() {
        None => None,
        Some(declared) => match parse_returns(&declared) {
            Ok(schema @ TypeSchema::Struct(_)) => Some(schema),
            Ok(_) => {
                return Err(
                    "payload: should be { \"object\": { name: type, … } }, the fields \
                     the action reads from its payload"
                        .to_owned(),
                );
            }
            Err(e) => return Err(format!("payload{}", e.trim_start_matches("returns"))),
        },
    };
    let returns = match action.returns_type() {
        None => None,
        Some(declared) => Some(parse_returns(&declared)?),
    };
    Ok(ActionTypes { payload, returns })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_action::ActionContext;
    use sc_error::Result;
    use sc_types::FormField;
    use serde_json::{Value as Json, json};

    struct Declared(Option<Json>, Option<Json>);

    #[async_trait::async_trait]
    impl Action for Declared {
        fn name(&self) -> &str {
            "declared"
        }
        fn description(&self) -> &str {
            ""
        }
        fn config_spec(&self) -> Vec<FormField> {
            Vec::new()
        }
        fn payload_type(&self) -> Option<Json> {
            self.0.clone()
        }
        fn returns_type(&self) -> Option<Json> {
            self.1.clone()
        }
        async fn run(&self, _ctx: &mut ActionContext<'_>) -> Result<Json> {
            Ok(Json::Null)
        }
    }

    #[test]
    fn declarations_lower_and_a_bad_one_says_which() {
        let types = action_types(&Declared(
            Some(json!({ "object": { "email": { "optional": "string" } } })),
            Some(json!({ "object": { "imap": "bool", "error": { "optional": "string" } } })),
        ))
        .unwrap();
        assert!(matches!(types.payload, Some(TypeSchema::Struct(ref f)) if f.len() == 1));
        assert!(types.returns.is_some());
        assert!(action_types(&Declared(None, None)).unwrap().is_empty());

        let not_an_object = action_types(&Declared(Some(json!("string")), None)).unwrap_err();
        assert!(not_an_object.starts_with("payload:"), "{not_an_object}");
        let bad_payload =
            action_types(&Declared(Some(json!({ "object": { "a": "strin" } })), None)).unwrap_err();
        assert!(bad_payload.starts_with("payload.object.a"), "{bad_payload}");
        let bad_returns = action_types(&Declared(None, Some(json!({ "array": 3 })))).unwrap_err();
        assert!(bad_returns.starts_with("returns"), "{bad_returns}");
    }
}
