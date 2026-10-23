//! **LLM credits metering** (TODO-payments P6.3, design §6.3): what one model
//! call costs the person an agent run is for.
//!
//! An agent is metered when it sets a rate:
//!
//! - [`ATTR_CREDITS_PER_DOLLAR`]: credits per unit of the model's priced cost
//!   ([`Usage::cost`](sc_llm::Usage::cost)) — the rate that follows the real
//!   bill, and the one to use;
//! - [`ATTR_CREDITS_PER_1K_INPUT`] and [`ATTR_CREDITS_PER_1K_OUTPUT`]: credits
//!   per thousand tokens, for a model whose prices are unknown. When both kinds
//!   are set, a call with a known cost is charged by the dollar and one without
//!   by the token.
//!
//! [`ATTR_CREDITS_MIN_BALANCE`] is what a run needs to start.
//!
//! A metered run (the driver's half, [`Runner`](crate::Runner)):
//!
//! 1. refuses to start when the caller's balance is at or below zero, or below
//!    the minimum, and refuses an anonymous caller outright — there is nobody
//!    to charge;
//! 2. debits after **every** model call — the steps, the summaries made while
//!    compacting and the calls a tool makes for itself — keyed by the run and
//!    the step. The call has already happened, so the debit may take the
//!    balance below zero, and the next grant repays it;
//! 3. stops at the next model call once the balance is zero or below, as
//!    [`Budget::Credits`](crate::Budget::Credits).
//!
//! A sub-agent's calls are charged to the same caller, at its own rate when it
//! sets one and its parent's otherwise, so delegating is never a way round the
//! meter. A run whose caller is an **admin** is never metered: the admin is the
//! one paying the provider, and that is what keeps the admin's copilot (whose
//! caller must be an admin) and a trigger's run (which carries the admin role)
//! free.

use rust_decimal::Decimal;
use sc_llm::{Prices, Usage};
use sc_types::Attrs;
use serde_json::Value as Json;

/// Agent attribute: credits per unit of model cost (the models' priced
/// currency, usually dollars).
pub const ATTR_CREDITS_PER_DOLLAR: &str = "credits_per_dollar";
/// Agent attribute: credits per thousand input tokens, cached or not.
pub const ATTR_CREDITS_PER_1K_INPUT: &str = "credits_per_1k_input";
/// Agent attribute: credits per thousand output tokens.
pub const ATTR_CREDITS_PER_1K_OUTPUT: &str = "credits_per_1k_output";
/// Agent attribute: the balance a metered run needs to start. Absent is any
/// balance above zero.
pub const ATTR_CREDITS_MIN_BALANCE: &str = "credits_min_balance";

/// An agent's rates: what its model calls cost the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Meter {
    /// Credits per unit of priced cost.
    pub per_dollar: Option<Decimal>,
    /// Credits per thousand input tokens.
    pub per_1k_input: Option<Decimal>,
    /// Credits per thousand output tokens.
    pub per_1k_output: Option<Decimal>,
    /// What a run needs to start.
    pub min_balance: Decimal,
}

impl Meter {
    /// The meter an agent's attributes set, or `None` when it sets no rate.
    /// A rate of zero or less is no rate.
    pub fn of(attributes: &Attrs) -> Option<Meter> {
        let rate = |key: &str| attr_decimal(attributes, key).filter(|r| *r > Decimal::ZERO);
        let meter = Meter {
            per_dollar: rate(ATTR_CREDITS_PER_DOLLAR),
            per_1k_input: rate(ATTR_CREDITS_PER_1K_INPUT),
            per_1k_output: rate(ATTR_CREDITS_PER_1K_OUTPUT),
            min_balance: attr_decimal(attributes, ATTR_CREDITS_MIN_BALANCE)
                .filter(|m| *m > Decimal::ZERO)
                .unwrap_or(Decimal::ZERO),
        };
        (meter.per_dollar.is_some() || meter.charges_tokens()).then_some(meter)
    }

    /// Whether a call can be charged by the token.
    pub fn charges_tokens(&self) -> bool {
        self.per_1k_input.is_some() || self.per_1k_output.is_some()
    }

    /// Whether every call needs a priced model: a rate per dollar with no rate
    /// per token to fall back on.
    pub fn needs_prices(&self) -> bool {
        self.per_dollar.is_some() && !self.charges_tokens()
    }

    /// What `usage` at `prices` costs in credits, to the ledger's precision;
    /// `None` when it cannot be worked out — a rate per dollar, no rate per
    /// token, and a model with no input or output price.
    ///
    /// A cost unknown only because the model has no **cache** price (a call
    /// that read or wrote the cache) is worked out at the input price for those
    /// tokens rather than refused: what the call cost is then nearly known, and
    /// charging nothing for it would be the wrong way to be wrong.
    pub fn charge(&self, usage: &Usage, prices: &Prices) -> Option<Decimal> {
        if let Some(rate) = self.per_dollar {
            let cost = usage.cost(prices).or_else(|| {
                Usage {
                    cached_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    ..*usage
                }
                .cost(prices)
            });
            if let Some(cost) = cost.and_then(|c| Decimal::try_from(c).ok()) {
                return Some(sc_auth::credits(cost * rate));
            }
        }
        if self.charges_tokens() {
            let per_k = |tokens: u64, rate: Option<Decimal>| {
                rate.map_or(Decimal::ZERO, |r| {
                    Decimal::from(tokens) * r / Decimal::ONE_THOUSAND
                })
            };
            return Some(sc_auth::credits(
                per_k(usage.input_tokens, self.per_1k_input)
                    + per_k(usage.output_tokens, self.per_1k_output),
            ));
        }
        None
    }
}

/// A number attribute read exactly: a JSON number's own text, or a string.
fn attr_decimal(attributes: &Attrs, key: &str) -> Option<Decimal> {
    match attributes.get(key)? {
        Json::Number(n) => n.to_string().parse().ok(),
        Json::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn attrs(value: Json) -> Attrs {
        value.as_object().cloned().unwrap_or_default()
    }

    fn priced() -> Prices {
        Prices {
            input: Some(3.0),
            cached_input: None,
            cache_write: None,
            output: Some(15.0),
        }
    }

    #[test]
    fn an_agent_with_no_rate_is_not_metered() {
        assert_eq!(Meter::of(&attrs(json!({}))), None);
        assert_eq!(
            Meter::of(&attrs(json!({ ATTR_CREDITS_PER_DOLLAR: 0 }))),
            None
        );
        assert_eq!(
            Meter::of(&attrs(json!({ ATTR_CREDITS_MIN_BALANCE: 5 }))),
            None,
            "a minimum with no rate meters nothing"
        );
        let m = Meter::of(&attrs(
            json!({ ATTR_CREDITS_PER_DOLLAR: 20, ATTR_CREDITS_MIN_BALANCE: 1.5 }),
        ))
        .unwrap();
        assert_eq!(m.per_dollar, Some(Decimal::from(20)));
        assert_eq!(m.min_balance, Decimal::new(15, 1));
        assert!(m.needs_prices());
    }

    #[test]
    fn a_call_is_charged_by_the_dollar_when_its_cost_is_known() {
        let m = Meter::of(&attrs(json!({ ATTR_CREDITS_PER_DOLLAR: 20 }))).unwrap();
        let usage = Usage {
            input_tokens: 10_000,
            output_tokens: 1_000,
            ..Usage::default()
        };
        // 10k × 3 + 1k × 15 per million = 0.045; × 20 = 0.9 credits.
        assert_eq!(m.charge(&usage, &priced()), Some(Decimal::new(9, 1)));
        // No prices and no token rate: nothing can be charged.
        assert_eq!(m.charge(&usage, &Prices::default()), None);
        // Cache tokens with no cache price are charged at the input price.
        let cached = Usage {
            cached_input_tokens: 4_000,
            ..usage
        };
        assert_eq!(m.charge(&cached, &priced()), Some(Decimal::new(9, 1)));
    }

    #[test]
    fn a_call_is_charged_by_the_token_when_its_cost_is_not() {
        let m = Meter::of(&attrs(json!({
            ATTR_CREDITS_PER_DOLLAR: 20,
            ATTR_CREDITS_PER_1K_INPUT: "0.1",
            ATTR_CREDITS_PER_1K_OUTPUT: 0.5,
        })))
        .unwrap();
        assert!(!m.needs_prices());
        let usage = Usage {
            input_tokens: 2_500,
            output_tokens: 300,
            ..Usage::default()
        };
        // Unpriced: 2.5 × 0.1 + 0.3 × 0.5 = 0.4.
        assert_eq!(
            m.charge(&usage, &Prices::default()),
            Some(Decimal::new(4, 1))
        );
        // Priced: the dollar rate wins.
        assert_eq!(
            m.charge(&usage, &priced()),
            Some(sc_auth::credits(Decimal::new(12, 3) * Decimal::from(20)))
        );
    }
}
