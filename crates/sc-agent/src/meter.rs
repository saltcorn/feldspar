//! **LLM credits metering** (TODO-payments P6.3): an agent's rates. The meter
//! itself is `sc_llm::meter`, one layer down, because a code body's charged
//! `llm` call (`sc-api`) prices a call by the same rule; this re-exports it
//! where the agent loop has always found it.

pub use sc_llm::meter::*;

#[cfg(test)]
mod tests {
    /// The meter rounds to the ledger's precision, which it cannot name.
    #[test]
    fn the_meter_rounds_as_the_ledger_does() {
        assert_eq!(sc_llm::meter::CREDIT_DECIMALS, sc_auth::CREDIT_DECIMALS);
        let amount = rust_decimal::Decimal::new(1_234_567_891, 9);
        assert_eq!(sc_llm::meter::credits(amount), sc_auth::credits(amount));
    }
}
