//! `llm` — the configured LLM providers, used from a code body.
//!
//! A code body reaches a provider through the `db` host, as `op: "llm"`
//! requests, rather than as a surface of its own — `models`' reason: what it
//! reads is system tables (`_fd_llm_providers`, `_fd_llm_models`), it is
//! bounded by the same call budget, and it is the same seam in both languages,
//! the JavaScript and Python `llm` objects being prelude over it.
//!
//! ```json
//! { "op": "llm", "what": "generate", "provider": "OpenAI", "model": "gpt-5-mini",
//!   "system": "Answer briefly.", "messages": [{ "role": "user", "content": "Hi" }],
//!   "temperature": 0.2, "max_tokens": 400 }
//! { "op": "llm", "what": "embed", "input": ["a text", "another"] }
//! ```
//!
//! A blank `provider` is the installation's default provider, and a blank
//! `model` that provider's default model of the kind asked for — the rule an
//! agent and an `embed_from` field follow, so a body and an agent configured
//! the same way call the same model.
//!
//! `generate` answers `{ text, provider, model, usage, cost, credits }` and
//! `embed` one vector per input. The call goes through the provider's call log
//! like every other model call.
//!
//! ## Charging LLM credits
//!
//! By default a call is the installation's and is charged to nobody: a code
//! body is admin-authored configuration, as an `embed_from` field is. A call
//! with a `charge` is charged to the **run's caller** — the person whose
//! request or write the body is answering — at the rates it names, which are an
//! agent's metering attributes under the same names and with the same meaning
//! ([`sc_llm::Meter`]):
//!
//! ```json
//! { "op": "llm", "what": "generate", "messages": […],
//!   "charge": { "credits_per_dollar": 100, "credits_min_balance": 1 } }
//! ```
//!
//! On a metered agent's terms, so a code body is no way round the meter:
//!
//! - an **admin** caller is not charged (the admin pays the provider), and the
//!   answer's `credits` is `null`;
//! - a call with **nobody** to charge — an anonymous request, a scheduled
//!   trigger — is refused before the model is called;
//! - so is one whose caller's balance is at or below zero, or below
//!   `credits_min_balance`, and one that could not be priced (a rate per dollar
//!   only, and a model with no prices);
//! - the debit is made **after** the call, whatever the balance by then — the
//!   call has happened — and outside the run's transaction, so a workflow step
//!   or a `run_code` rolled back still leaves it charged.
//!
//! Refused on a [`viewer_only`](super::TableHost::viewer_only) host: the body
//! there is not the admin's to vouch for — an agent's sandbox runs code the
//! model wrote, and a provider reached from it would be a model call outside
//! the agent's own budget and its user's credits.

use rust_decimal::Decimal;
use sc_catalog::Authority;
use sc_error::{Error, Result};
use sc_llm::{LlmMessage, LlmRequest, Meter, ModelKind, Prices, Usage};
use serde_json::{Value as Json, json};
use uuid::Uuid;

use super::TableHost;

/// The most texts one `llm.embed` call takes.
pub const CODE_MAX_EMBED_INPUTS: usize = 2048;

/// One message of a `generate` request, as the guest spells it.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Message {
    /// `system`, `user` or `assistant`.
    role: String,
    /// The text.
    content: String,
}

/// One `llm` request.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LlmPlan {
    /// Always `llm`; read before the plan is.
    #[allow(dead_code)]
    op: String,
    /// `generate` or `embed`.
    what: String,
    /// The provider's name; blank is the default provider.
    #[serde(default)]
    provider: Option<String>,
    /// The model's name; blank is the provider's default of the kind asked for.
    #[serde(default)]
    model: Option<String>,
    /// `generate`: the system prompt.
    #[serde(default)]
    system: Option<String>,
    /// `generate`: the conversation, oldest first.
    #[serde(default)]
    messages: Option<Vec<Message>>,
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    max_tokens: Option<u32>,
    /// `embed`: the texts.
    #[serde(default)]
    input: Option<Vec<String>>,
    /// Charge the call to the run's caller, at these rates.
    #[serde(default)]
    charge: Option<Json>,
}

/// The keys a `charge` takes: an agent's metering attributes.
const CHARGE_KEYS: [&str; 4] = [
    sc_llm::ATTR_CREDITS_PER_DOLLAR,
    sc_llm::ATTR_CREDITS_PER_1K_INPUT,
    sc_llm::ATTR_CREDITS_PER_1K_OUTPUT,
    sc_llm::ATTR_CREDITS_MIN_BALANCE,
];

/// Who a charged call is charged to, at what rates.
struct Payer {
    user: Uuid,
    meter: Meter,
}

/// The meter a `charge` option sets, or `None` for a call that is not charged.
fn meter_of(charge: Option<&Json>) -> Result<Option<Meter>> {
    let Some(charge) = charge.filter(|c| !c.is_null()) else {
        return Ok(None);
    };
    let Json::Object(rates) = charge else {
        return Err(Error::invalid(format!(
            "an llm call's `charge` is its rates, as in {{ {}: 100 }}",
            sc_llm::ATTR_CREDITS_PER_DOLLAR
        )));
    };
    if let Some(key) = rates.keys().find(|k| !CHARGE_KEYS.contains(&k.as_str())) {
        return Err(Error::invalid(format!(
            "`{key}` is not a rate of an llm call's `charge`; they are: {}",
            CHARGE_KEYS.join(", ")
        )));
    }
    Meter::of(rates).map(Some).ok_or_else(|| {
        Error::invalid(format!(
            "an llm call's `charge` needs a rate above zero: {}, {} or {}",
            sc_llm::ATTR_CREDITS_PER_DOLLAR,
            sc_llm::ATTR_CREDITS_PER_1K_INPUT,
            sc_llm::ATTR_CREDITS_PER_1K_OUTPUT
        ))
    })
}

/// Who `meter` charges in `host`'s run, checked before the model is called:
/// `None` for an admin caller, refused for nobody, and refused for a balance
/// at or below zero or below the minimum.
async fn payer(host: &TableHost<'_>, meter: Option<Meter>) -> Result<Option<Payer>> {
    let Some(meter) = meter else {
        return Ok(None);
    };
    if host.caller.role().is_admin() {
        return Ok(None);
    }
    let user = host
        .caller
        .user()
        .and_then(|u| u.get("id"))
        .and_then(Json::as_str)
        .and_then(|id| Uuid::parse_str(id).ok())
        .ok_or_else(|| {
            Error::auth(
                "this llm call charges LLM credits, so it needs a signed-in user to charge, \
                 and this code is running for nobody",
            )
        })?;
    let balance = sc_auth::credit_balance(host.catalog, user).await?.balance;
    if balance <= Decimal::ZERO || balance < meter.min_balance {
        return Err(Error::invalid(format!(
            "not enough LLM credits: the balance is {balance}{}",
            if meter.min_balance > Decimal::ZERO {
                format!(" and this call needs {}", meter.min_balance)
            } else {
                String::new()
            }
        )));
    }
    Ok(Some(Payer { user, meter }))
}

/// Refuse, before calling, a charged call whose cost could not be worked out.
fn require_priced(payer: Option<&Payer>, priced: bool, model: &str) -> Result<()> {
    match payer {
        Some(p) if p.meter.needs_prices() && !priced => Err(Error::config(format!(
            "the model `{model}` has no prices, so its call cannot be charged in LLM credits \
             per dollar; set its prices, or charge per 1,000 tokens as well"
        ))),
        _ => Ok(()),
    }
}

/// Debit `amount` from the payer for one call, answering `{ charged, balance }`.
async fn debit(
    host: &TableHost<'_>,
    payer: &Payer,
    amount: Option<Decimal>,
    model: &str,
    usage: Usage,
    cost: Option<f64>,
) -> Result<Json> {
    // `require_priced` refused the calls that cannot be priced, so this is a
    // model whose prices went between the check and the answer.
    let amount = amount.ok_or_else(|| {
        Error::config(format!(
            "the model `{model}` could not be priced, so its call was not charged"
        ))
    })?;
    let note = match host.caller.chain().last() {
        Some(trigger) => format!("llm in the code of `{trigger}`"),
        None => "llm in a code body".to_owned(),
    };
    let debited = sc_auth::debit_credits(
        host.catalog,
        sc_auth::CreditDebit {
            user_id: payer.user,
            amount,
            key: format!("code:{}", Uuid::new_v4()),
            run: None,
            agent: None,
            model: Some(model.to_owned()),
            input_tokens: Some(usage.input_tokens),
            output_tokens: Some(usage.output_tokens),
            cost: cost
                .and_then(|c| Decimal::try_from(c).ok())
                .map(sc_auth::credits),
            note: Some(note),
        },
        Authority::Caller(&host.caller),
    )
    .await?;
    Ok(json!({
        "charged": sc_auth::credits(amount).to_string(),
        "balance": debited.balance.to_string(),
    }))
}

/// Answer one `llm` request of `host`'s run.
pub(super) async fn answer(host: &TableHost<'_>, request: Json) -> Result<Json> {
    let plan: LlmPlan = serde_json::from_value(request).map_err(|e| {
        Error::invalid(format!(
            "this llm request is not one the server understands: {e}"
        ))
    })?;
    if host.viewer_only {
        return Err(Error::invalid(
            "`llm` is not available here: this code runs at its viewer's authority, and \
             a model call is the installation's to make (use it in a trigger, a workflow \
             step or a custom query)",
        ));
    }
    match plan.what.as_str() {
        "generate" => generate(host, plan).await,
        "embed" => embed(host, plan).await,
        other => Err(Error::invalid(format!(
            "`llm` has no `{other}`; it has generate and embed"
        ))),
    }
}

/// The request `plan` describes: a system prompt (the plan's own, then every
/// `system` message, in order) and the user and assistant turns.
fn chat_request(plan: &LlmPlan) -> Result<LlmRequest> {
    let mut system: Vec<String> = plan.system.iter().cloned().collect();
    let mut messages = Vec::new();
    for message in plan.messages.iter().flatten() {
        match message.role.as_str() {
            "system" => system.push(message.content.clone()),
            "user" => messages.push(LlmMessage::user(&message.content)),
            "assistant" => messages.push(LlmMessage::assistant(&message.content)),
            other => {
                return Err(Error::invalid(format!(
                    "an llm message's role is system, user or assistant, not `{other}`"
                )));
            }
        }
    }
    if !messages
        .iter()
        .any(|m| matches!(m, LlmMessage::User { .. }))
    {
        return Err(Error::invalid(
            "llm.generate() needs something to answer: a prompt, or messages with a user turn",
        ));
    }
    let system = system
        .into_iter()
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<_>>();
    Ok(LlmRequest {
        system: (!system.is_empty()).then(|| system.join("\n\n")),
        messages,
        max_tokens: plan.max_tokens,
        temperature: plan.temperature,
        ..LlmRequest::default()
    })
}

async fn generate(host: &TableHost<'_>, plan: LlmPlan) -> Result<Json> {
    if plan.input.is_some() {
        return Err(Error::invalid(
            "`input` is llm.embed()'s; llm.generate() takes a prompt or messages",
        ));
    }
    let request = chat_request(&plan)?;
    let meter = meter_of(plan.charge.as_ref())?;
    let catalog = host.catalog;
    let provider = sc_llm::embedding_provider(catalog, plan.provider.as_deref()).await?;
    let model = sc_llm::require_llm_model(catalog, &provider, plan.model.as_deref()).await?;
    let connected = sc_llm::connect_model(&provider, &model)?;
    let payer = payer(host, meter).await?;
    require_priced(
        payer.as_ref(),
        priced(&connected.prices),
        connected.provider.model(),
    )?;
    let answer = connected.provider.stream(request).await?.collect().await?;
    let usage = answer.usage;
    let credits = match &payer {
        Some(payer) => {
            let amount = payer.meter.charge(&usage, &connected.prices);
            let cost = usage.cost(&connected.prices);
            debit(host, payer, amount, connected.provider.model(), usage, cost).await?
        }
        None => Json::Null,
    };
    Ok(json!({
        "text": answer.content,
        "provider": provider.name,
        "model": connected.provider.model(),
        "usage": {
            "input_tokens": usage.input_tokens,
            "output_tokens": usage.output_tokens,
        },
        "cost": usage.cost(&connected.prices),
        "credits": credits,
    }))
}

/// Whether a chat call at `prices` can be priced: an input and an output price.
fn priced(prices: &Prices) -> bool {
    prices.input.is_some() && prices.output.is_some()
}

async fn embed(host: &TableHost<'_>, plan: LlmPlan) -> Result<Json> {
    if plan.messages.is_some() || plan.system.is_some() {
        return Err(Error::invalid(
            "llm.embed() takes text, not messages or a system prompt",
        ));
    }
    let texts = plan.input.unwrap_or_default();
    if texts.is_empty() {
        return Err(Error::invalid(
            "llm.embed() needs a text, or a list of them",
        ));
    }
    if texts.len() > CODE_MAX_EMBED_INPUTS {
        return Err(Error::invalid(format!(
            "llm.embed() takes at most {CODE_MAX_EMBED_INPUTS} texts in one call, not {}",
            texts.len()
        )));
    }
    let meter = meter_of(plan.charge.as_ref())?;
    let (provider, model) = sc_llm::resolve_model(
        host.catalog,
        plan.provider.as_deref(),
        plan.model.as_deref(),
        ModelKind::Embedding,
    )
    .await?;
    let payer = payer(host, meter).await?;
    require_priced(payer.as_ref(), model.prices().input.is_some(), &model.name)?;
    let embedded = sc_llm::embed(&provider, &model, &texts).await?;
    if let Some(payer) = &payer {
        let call = &embedded.call;
        let amount = payer.meter.charge_call(call);
        debit(host, payer, amount, &model.name, call.usage, call.cost).await?;
    }
    serde_json::to_value(embedded.vectors).map_err(|e| Error::msg(format!("embed: {e}")))
}
