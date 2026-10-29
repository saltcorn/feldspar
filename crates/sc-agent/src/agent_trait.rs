//! The [`AgentTrait`] extension point and the contexts it works in (§11.2).
//!
//! A **trait** is an elementary agent capability. Most contribute one or more
//! **tools** to the loop; some only change the turn (an extra paragraph of system
//! prompt, data preloaded into the conversation). This is the same "settings as
//! data" move an `Action`, a file-store backend and a
//! framework already make: a trait declares its configuration as
//! [`FormField`]s, so the admin UI renders a form for a trait it has never heard
//! of, and that same declaration is what validates a saved agent.
//!
//! This crate registers **no traits at all**. The built-in set lives in
//! `sc-core-traits` at layer 9, above the row layer, for the reason §10.1 gives
//! for `sc-core-actions`: a trait that writes a row must go through `sc-api`'s
//! rows module so the write is coerced, validated and *observed* by triggers,
//! and that module is above this one.
//!
//! ## Who a tool runs as
//!
//! Every tool executes as the [`RunCaller`] the run was created with — the user
//! who is chatting, not the server (decision 5). So `query_table` is an ordinary
//! read with that caller, §7.3's ownership and row-level security apply
//! unchanged, and an agent cannot become a way around them. A run started by a
//! trigger carries the trigger's authority instead, and that difference is set at
//! exactly one place: where the run is created.

use std::collections::BTreeMap;
use std::sync::Arc;

use sc_action::TriggerDispatcher;
use sc_auth::User;
use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_expr::JsEvaluator;
use sc_llm::{ImagePart, ModelCapabilities, ToolSpec};
use sc_types::{Attrs, FormField};
use serde_json::Value as Json;

use crate::context::Elidable;
use crate::control::Signal;
use crate::delegate::Delegator;
use crate::interact::{ComposerControl, DisplayPart, UserRequest};
use crate::ledger::CodeEvaluation;
use crate::run::{RunId, RunMode};
use crate::view::{AppPreviewer, AppRequester, BrowserDriver, HostCapabilities, ModuleManager};

/// One elementary agent capability: configurable, contributing tools.
///
/// Object-safe and dynamically dispatched, for the reason an `Action` is: which
/// traits an agent has is decided at runtime from stored configuration, and the
/// set is meant to grow from outside this crate.
#[async_trait::async_trait]
pub trait AgentTrait: Send + Sync {
    /// The name the trait is registered and stored under (`query_table`, …).
    /// Stable: it is what a saved agent references.
    fn name(&self) -> &str;

    /// One line for the admin UI's trait picker.
    fn description(&self) -> &str;

    /// The configuration this trait takes, as form fields.
    fn config_spec(&self) -> Vec<FormField>;

    /// Check a configuration beyond what [`config_spec`](AgentTrait::config_spec)
    /// can express — the part only this trait knows: that a table it names exists
    /// and can be addressed by primary key, that a trigger it exposes is real,
    /// that a named field is.
    ///
    /// Runs where the generic check runs: on save, in front of the admin, *and*
    /// again on load, so an agent whose world changed underneath it leaves the
    /// live set with a reason rather than failing mid-conversation.
    async fn validate_config(&self, check: &TraitCheck<'_>) -> Result<()> {
        let _ = check;
        Ok(())
    }

    /// The tools this configuration contributes. May be empty — a trait that only
    /// changes the turn contributes none.
    ///
    /// **Tool names are derived from the configuration**, not fixed by the trait:
    /// one `query_table` configured against `books` and another against `orders`
    /// are two tools the model must be able to tell apart, so they are
    /// `query_books` and `query_orders`. A collision between two enabled traits is
    /// refused on save by [`validate_agent`](crate::validate_agent), where it is
    /// fixable, rather than discovered when the model picks the wrong one.
    ///
    /// The context carries the `catalog`, because a tool's **description and JSON
    /// schema are generated from the thing it is configured against** (§11.3):
    /// `query_books` tells the model which fields it may filter on rather than
    /// leaving it to guess, and a guess that misses costs a turn. It also carries
    /// the run's **mode** — a read-only `plan` or `explore` run is offered no
    /// edit tools — and the answering model's **capabilities**, so a tool may be
    /// shaped for what the model can do.
    ///
    /// Validation checks names over the union of every mode, so a tool offered
    /// only in one mode still cannot collide with another trait's.
    ///
    /// Infallible, because it is called wherever the tool set is needed —
    /// including while reporting *why* an agent is invalid. A trait whose target
    /// has since been dropped should still return its tool under the name the
    /// configuration gives it, described as best it can: dropping the tool
    /// silently would turn "this agent names a table that is gone" into "this
    /// agent has no tools", and the second is not a repairable message.
    fn tools(&self, cx: &ToolsContext<'_>, config: &Attrs) -> Vec<ToolSpec>;

    /// Run one of this trait's tools.
    ///
    /// `config` is *this enabled instance's* configuration, `tool` is one of the
    /// names [`tools`](AgentTrait::tools) produced for it, and `args` is the
    /// arguments object the model sent — already parsed, never a fragment.
    ///
    /// An `Err` here is not a failure of the run: the loop turns it into the tool
    /// *result* the model reads (§11.2), because "no such table" is something a
    /// model can act on and an exception is not. So an implementation should
    /// return errors that read as instructions to a reader who cannot see the
    /// stack.
    async fn call(
        &self,
        config: &Attrs,
        tool: &str,
        args: &Json,
        ctx: &mut TraitContext<'_>,
    ) -> Result<Json>;

    /// This trait's **static contribution** to the system prompt: text that
    /// follows the agent's own prompt, in trait order (TODO §9's stable prefix).
    ///
    /// Given what [`tools`](AgentTrait::tools) is given, so the text can match
    /// the tools it describes: `coding` says different things in `plan` and in
    /// `act`, and names only the edit format it offers. It **must be a function
    /// of its inputs**, the same on every step of a session, because it is part
    /// of the cached prefix. What changes from step to step belongs in a tool
    /// result, and what is known once per session in
    /// [`session_header`](AgentTrait::session_header).
    fn prompt(&self, cx: &ToolsContext<'_>, config: &Attrs) -> Option<String> {
        let _ = (cx, config);
        None
    }

    /// The mode a run this configuration's agent **starts** should be in, or
    /// `None` to leave it to the other traits and then to `act` (TODO §5).
    ///
    /// `coding` answers `plan` under its `planned` workflow, so a chat with the
    /// builder agent begins by planning. Read where a run is created, not on
    /// every step: a run carries its mode on its row, and a conversation keeps
    /// the mode it started in. The first trait, in the agent's order, that
    /// names one wins.
    fn starting_mode(&self, config: &Attrs) -> Option<RunMode> {
        let _ = config;
        None
    }

    /// Change the turn without adding a tool — an extra paragraph of system
    /// prompt, data preloaded into the conversation.
    ///
    /// Called before **every** model call, not once per run, so a trait whose
    /// contribution depends on what has happened so far can say something
    /// different on the second step than on the first.
    ///
    /// **Anything appended here breaks prompt caching.** The system prompt is
    /// the front of the cached prefix (TODO §9), so text that changes between
    /// steps makes every request a cache miss from its first token. What a trait
    /// knows once per session belongs in
    /// [`session_header`](AgentTrait::session_header), and what changes belongs
    /// in a tool result, and what is always the same in
    /// [`prompt`](AgentTrait::prompt).
    async fn on_turn(&self, config: &Attrs, turn: &mut Turn<'_>) -> Result<()> {
        let _ = (config, turn);
        Ok(())
    }

    /// What this trait puts in the **session header**: the first user-side
    /// message of a run, after the stable prefix and before the history (TODO
    /// §9). `coding` puts `AGENTS.md`, a small repo map and the recent `git log`
    /// there.
    ///
    /// Called **once per session**, before the first model call. The loop stores
    /// the result with the run, so it is not rebuilt on the next step, when the
    /// conversation continues, or on resume — which is what lets it be cached.
    /// Every trait's header is joined in the agent's trait order.
    ///
    /// An `Err` fails the run: a header is context the agent was promised.
    async fn session_header(
        &self,
        config: &Attrs,
        cx: &mut SessionContext<'_>,
    ) -> Result<Option<String>> {
        let _ = (config, cx);
        Ok(None)
    }

    /// The stub an old result of one of this trait's tools is replaced by when
    /// the loop clears the context (TODO §9, pass 1), or `None` to keep it whole.
    ///
    /// The default is [`Elidable::default_stub`]:
    /// `[elided: N characters of <tool> output]`. A trait can say more for less —
    /// an old check result as one line — or see from the transcript that a read
    /// was made stale by a later edit. Only the result is replaced: the call and
    /// its arguments stay, so the model still sees what it did.
    fn elide(&self, config: &Attrs, old: &Elidable<'_>) -> Option<String> {
        let _ = config;
        Some(old.default_stub())
    }

    /// Called once after **all** of a model turn's tool calls have finished,
    /// for each trait that owned at least one of them (TODO 5.10).
    ///
    /// Work that should happen once per turn rather than once per call goes
    /// here: `coding` formats the files the turn edited and type-checks them
    /// once, however many edits the model sent. Text returned is appended to
    /// the result of this trait's **last** call in the turn, so it arrives in a
    /// tool result, where it does not break the cached prefix.
    ///
    /// An `Err` is logged and otherwise ignored: the turn's own results stand.
    async fn after_tools(
        &self,
        config: &Attrs,
        cx: &mut AfterToolsContext<'_>,
    ) -> Result<Option<String>> {
        let _ = (config, cx);
        Ok(None)
    }

    /// What makes two calls of `tool` **the same call**, for the doom-loop
    /// detectors (TODO §10).
    ///
    /// The loop fingerprints a call as the tool's name plus the canonical JSON
    /// of what this returns: the arguments themselves by default, with keys
    /// sorted and whitespace normalised. Override it to ignore what does not
    /// change the call's meaning — `read_file` of the same file at another
    /// offset is still the same file read again.
    fn fingerprint(&self, config: &Attrs, tool: &str, args: &Json) -> Json {
        let _ = (config, tool);
        args.clone()
    }

    /// Called when a drive of `run` stops, however it stops: answered, failed,
    /// out of budget, or dropped mid-step because the run was aborted or the
    /// chat disconnected (TODO 6a.4).
    ///
    /// For releasing what a trait holds **outside** the run's state on the
    /// run's behalf — `coding`'s managed processes. Synchronous, because it is
    /// also called from a destructor, where nothing can be awaited: start the
    /// release and return.
    fn run_ended(&self, config: &Attrs, run: RunId) {
        let _ = (config, run);
    }

    /// Turn the person's answer to a question this trait asked
    /// ([`TraitContext::await_user`]) into the text the model reads — the next
    /// user message (TODO-agent-skills AS6.2).
    ///
    /// Called with an answer already checked against the request. The default
    /// says the question and the answer in words.
    async fn on_answer(
        &self,
        config: &Attrs,
        request: &UserRequest,
        value: &Json,
    ) -> Result<String> {
        let _ = config;
        Ok(request.answer_text(value))
    }

    /// [`on_answer`](AgentTrait::on_answer), with a tool call's context: for a
    /// trait whose answer *does* something before the model hears of it — an
    /// approved proposal applied as the person who approved it
    /// (`library_tools`). The default is `on_answer`.
    ///
    /// The request is the one the run stored, so its payload is what the
    /// trait put there, never what the client sent; only `value` is theirs.
    async fn on_answer_in(
        &self,
        config: &Attrs,
        request: &UserRequest,
        value: &Json,
        ctx: &mut TraitContext<'_>,
    ) -> Result<String> {
        let _ = ctx;
        self.on_answer(config, request, value).await
    }

    /// The controls this trait adds to the chat's input row (AS6.4). Their
    /// values reach [`on_turn`](AgentTrait::on_turn) as [`Turn::controls`].
    fn controls(&self, config: &Attrs) -> Vec<ComposerControl> {
        let _ = config;
        Vec::new()
    }

    /// The controls this trait adds for **this caller**, where they depend on
    /// what the catalog holds — `prompt_picker`'s options read from a table,
    /// as the caller (TODO-agent-skills AS10.5). The default is
    /// [`controls`](AgentTrait::controls). What a chat surface lists, and what
    /// a message's controls are checked against.
    async fn controls_in(
        &self,
        config: &Attrs,
        cx: &ControlsContext<'_>,
    ) -> Result<Vec<ComposerControl>> {
        let _ = cx;
        Ok(self.controls(config))
    }

    /// A [`Button`](crate::ControlKind::Button) control this trait declared,
    /// `control`, was pressed: the message that starts a turn carries it as
    /// `true` (AS10.3).
    ///
    /// `Ok(Some(text))` answers the press here: `text` is the turn's answer,
    /// **without a model call** — `run_js_code`'s button runs its code and
    /// says what it returned. `Ok(None)` (the default) leaves the press an
    /// ordinary message the model answers. `row` is the row whose event
    /// started the run, when one did. An `Err` is the answer too, said as a
    /// failure, so a press always ends its turn.
    async fn on_press(
        &self,
        config: &Attrs,
        control: &str,
        row: Option<&Json>,
        ctx: &mut TraitContext<'_>,
    ) -> Result<Option<String>> {
        let _ = (config, control, row, ctx);
        Ok(None)
    }

    /// Called once when the model gives a turn's **final answer** — a reply
    /// with no tool calls that ends the turn — for every enabled trait, in
    /// trait order (TODO-agent-skills AS11.2). `speech` reads the answer aloud
    /// here and attaches the audio as a display part.
    ///
    /// What it shows ([`AnswerContext::show`]) is attached to the answer, not
    /// to a tool call, and the model never sees it; a model call it makes for
    /// itself ([`AnswerContext::record_call`]) is ledgered and charged as a
    /// tool's is. An `Err` is logged and otherwise ignored: the answer stands.
    async fn on_final_answer(&self, config: &Attrs, cx: &mut AnswerContext<'_>) -> Result<()> {
        let _ = (config, cx);
        Ok(())
    }
}

/// What [`AgentTrait::controls_in`] is given: where the options can come from,
/// and who they are for.
pub struct ControlsContext<'a> {
    /// The live catalog.
    pub catalog: &'a Catalog,
    /// Who is chatting: a table read for the options is theirs.
    pub caller: &'a RunCaller,
    /// The name of the agent, for error messages.
    pub agent: &'a str,
}

/// What [`AgentTrait::on_final_answer`] is given: the run, the answer, and
/// where to put what it shows and what it spent.
pub struct AnswerContext<'a> {
    /// The live catalog.
    pub catalog: &'a Catalog,
    /// Who the run is for.
    pub caller: &'a RunCaller,
    /// The name of the agent.
    pub agent: &'a str,
    /// The run.
    pub run: RunId,
    /// The run's mode.
    pub mode: RunMode,
    /// The final answer's text.
    pub answer: &'a str,
    /// This trait instance's per-run state, as [`TraitContext::state`] reaches
    /// it.
    pub trait_state: &'a mut Json,
    /// Parts shown to the person beside the answer. Reach it through
    /// [`show`](AnswerContext::show).
    pub display: Vec<DisplayPart>,
    /// Non-chat model calls made, for the ledger and the payer. Reach it
    /// through [`record_call`](AnswerContext::record_call).
    pub calls: Vec<sc_llm::ModelCall>,
}

impl AnswerContext<'_> {
    /// Show `part` to the person beside the answer. Refused when it is too
    /// large.
    pub fn show(&mut self, part: DisplayPart) -> Result<()> {
        part.check()?;
        self.display.push(part);
        Ok(())
    }

    /// Record a non-chat model call made for the answer (speech), to be
    /// ledgered as an aside and charged to the run's payer.
    pub fn record_call(&mut self, call: sc_llm::ModelCall) {
        self.calls.push(call);
    }
}

/// What [`AgentTrait::session_header`] is given: the run, and this trait
/// instance's per-run state.
pub struct SessionContext<'a> {
    /// The live catalog.
    pub catalog: &'a Catalog,
    /// Who the run is for.
    pub caller: &'a RunCaller,
    /// The name of the agent.
    pub agent: &'a str,
    /// The run whose session this is.
    pub run: RunId,
    /// The run's mode.
    pub mode: RunMode,
    /// The first thing the person (or the delegating run) said: the request or
    /// the feature brief.
    pub brief: &'a str,
    /// This trait instance's per-run state, as [`TraitContext::state`] reaches
    /// it.
    pub trait_state: &'a mut Json,
    /// The JavaScript engine, where the run has one (as
    /// [`TraitContext::evaluator`]).
    pub evaluator: Option<&'a Arc<dyn JsEvaluator>>,
    /// The trigger dispatcher, where the run has one (as
    /// [`TraitContext::triggers`]): what a code body in the header runs
    /// through (`preload_code`, AS8.2).
    pub triggers: Option<&'a Arc<TriggerDispatcher>>,
    /// The row whose event started this run, for a run a trigger started on a
    /// row event (`Runner::with_row`); `None` for a chat.
    pub row: Option<&'a Json>,
}

/// What [`AgentTrait::after_tools`] is given: the run, this trait instance's
/// per-run state, and the calls of the turn that were its own.
pub struct AfterToolsContext<'a> {
    /// The live catalog.
    pub catalog: &'a Catalog,
    /// Who the run is for.
    pub caller: &'a RunCaller,
    /// The name of the agent.
    pub agent: &'a str,
    /// The run.
    pub run: RunId,
    /// The run's mode.
    pub mode: RunMode,
    /// This trait's calls in the turn, in the order they ran.
    pub calls: &'a [sc_llm::ToolCall],
    /// This trait instance's per-run state, as [`TraitContext::state`] reaches
    /// it.
    pub trait_state: &'a mut Json,
}

/// What [`AgentTrait::tools`] is told about the run it is offering tools to.
#[derive(Clone, Copy)]
pub struct ToolsContext<'a> {
    /// The live catalog.
    pub catalog: &'a Catalog,
    /// The run's mode.
    pub mode: RunMode,
    /// What the model answering the run can do.
    pub capabilities: &'a ModelCapabilities,
    /// Who the run is for, or `None` when the tools are listed for no run in
    /// particular (validation, the admin UI). A tool offered only to some
    /// callers — `coding`'s shell, to admins — is withheld when this is `None`.
    pub caller: Option<&'a RunCaller>,
}

impl<'a> ToolsContext<'a> {
    /// A context for `mode`, answered by a model with `capabilities`.
    pub fn new(
        catalog: &'a Catalog,
        mode: RunMode,
        capabilities: &'a ModelCapabilities,
    ) -> ToolsContext<'a> {
        ToolsContext {
            catalog,
            mode,
            capabilities,
            caller: None,
        }
    }

    /// The same context, for a run on behalf of `caller`.
    pub fn for_caller(mut self, caller: &'a RunCaller) -> ToolsContext<'a> {
        self.caller = Some(caller);
        self
    }
}

/// What a trait's own configuration check ([`AgentTrait::validate_config`]) gets.
pub struct TraitCheck<'a> {
    /// The live catalog: what tables, fields and triggers exist.
    pub catalog: &'a Catalog,
    /// The configuration being validated, keyed by
    /// [`config_spec`](AgentTrait::config_spec) field names.
    pub config: &'a Attrs,
    /// The name of the agent this instance is enabled on, for messages that have
    /// to be actionable in a list of agents.
    pub agent: &'a str,
    /// What the server found on its host: a grant needing a browser is refused
    /// on a host without one.
    pub host: &'a HostCapabilities,
}

/// Who a run's tools execute as (decision 5).
///
/// Three shapes: a **user**, which is a chat turn — the person is there, and
/// every tool is their read or their write — the **system**, which is a
/// trigger-started run carrying the trigger's own authority, and an
/// **anonymous** visitor of an application that exposes an agent to people who
/// are not signed in, who holds the public role and nothing more. There is no
/// default: [`Run::new`](crate::Run::new) takes one, so a run that never decided
/// cannot exist.
///
/// The two fields are what a trait hands to the row layer:
/// `sc_api::caller_context_at(caller.role, caller.user.as_ref())` is the whole
/// conversion, and it is spelled at the call site rather than here because
/// `sc-api` is above this crate.
#[derive(Debug, Clone, PartialEq)]
pub struct RunCaller {
    /// The user every tool runs as, or `None` for a system or anonymous run.
    pub user: Option<User>,
    /// The role every tool's access is checked at — the user's own, admin for
    /// a system run, public for an anonymous one.
    pub role: u8,
    /// Whether a trigger started this run ([`RunCaller::triggered`]): it reads
    /// and writes rows with the trigger's authority, but is never offered the
    /// tools that act as the server itself — the shell, module installs, the
    /// project's scripts and checks, the admin copilot.
    pub triggered: bool,
    /// Whether this run is a chat with an agent an application exposes
    /// ([`RunCaller::exposed`], [`RunCaller::anonymous`]). Like a triggered
    /// run, it keeps its caller's authority over rows but is never offered the
    /// tools that act as the server: the request came from an application's
    /// origin, where any script on the page holds the visitor's cookie and
    /// CSRF token, and an admin signed in to the application is not therefore
    /// at the admin console (security review #21).
    pub exposed: bool,
    /// The trigger chain that started this run, when a trigger did
    /// ([`chained`](Self::chained)): what a trigger run or a write made by one
    /// of its tools carries on, so the cascade limit counts a trigger → agent →
    /// trigger loop like any other. Empty for a chat.
    pub chain: Vec<String>,
}

impl RunCaller {
    /// A run on behalf of `user`, at that user's role. This is a chat turn.
    pub fn user(user: User) -> RunCaller {
        RunCaller {
            role: user.role,
            user: Some(user),
            triggered: false,
            exposed: false,
            chain: Vec::new(),
        }
    }

    /// A chat turn of `user` with an agent an application exposes: the user's
    /// own authority over rows, without the server's own tools (see
    /// [`exposed`](Self::exposed)).
    pub fn exposed(user: User) -> RunCaller {
        RunCaller {
            exposed: true,
            ..RunCaller::user(user)
        }
    }

    /// A run with the authority of the trigger that started it: admin, no user.
    ///
    /// Named for what it is rather than called a default, because the whole point
    /// of decision 5 is that the choice is visible at the one place a run is
    /// created.
    pub fn system() -> RunCaller {
        RunCaller {
            user: None,
            role: 1,
            triggered: false,
            exposed: false,
            chain: Vec::new(),
        }
    }

    /// A run a trigger started: [`system`](Self::system)'s authority over rows,
    /// without the tools that act as the server's operating-system user or
    /// rewrite the application.
    ///
    /// The difference is where the prompt came from. A trigger's prompt is a
    /// formula over the event — a row somebody inserted, a webhook somebody
    /// sent — so a run that holds the admin's shell because a trigger started it
    /// hands the shell to whoever can cause the event, by prompt injection. The
    /// `run_agent` action makes this one unless its trigger is marked as
    /// running on trusted input.
    pub fn triggered() -> RunCaller {
        RunCaller {
            triggered: true,
            ..RunCaller::system()
        }
    }

    /// A run for somebody who is not signed in: no user, the public role.
    ///
    /// Only an application that exposes an agent with `anonymous` set makes
    /// one. Every tool then sees exactly what a signed-out request to the same
    /// table would see, which is the point: being anonymous must not be a way
    /// to borrow anybody's authority, least of all the system's.
    pub fn anonymous() -> RunCaller {
        RunCaller {
            user: None,
            role: 100,
            triggered: false,
            exposed: true,
            chain: Vec::new(),
        }
    }

    /// A run a trigger started **as whoever caused the event**: `user` at
    /// their own role, or — nobody signed in — the public role, exactly as
    /// [`anonymous`](Self::anonymous). Never the server's own tools, because the
    /// prompt is still computed from the event ([`triggered`](Self::triggered)).
    ///
    /// What `run_agent`'s `run_as: event_user` makes, so an application that
    /// starts an agent through `POST /api/actions/<trigger>` gets tools that see
    /// what the signed-in person may see, and a run that records them.
    pub fn event_user(user: Option<User>) -> RunCaller {
        let caller = match user {
            Some(user) => RunCaller::user(user),
            None => RunCaller::anonymous(),
        };
        RunCaller {
            triggered: true,
            ..caller
        }
    }

    /// This caller, carrying `chain` — the firing chain of the trigger that
    /// started the run — on to what its tools do.
    pub fn chained(mut self, chain: Vec<String>) -> RunCaller {
        self.chain = chain;
        self
    }

    /// Whether this is an anonymous visitor's run ([`anonymous`](Self::anonymous)),
    /// as opposed to a system or triggered one, which also has no user. A tool
    /// that falls back to a configured account when "nobody is chatting" must
    /// not fall back for a visitor, who is somebody (security review #20).
    pub fn is_anonymous(&self) -> bool {
        self.user.is_none() && self.exposed
    }

    /// Whether the prompt this run acts on came from outside the admin
    /// console — an event a trigger fired on, or an application's page — so
    /// that holding role 1 is not being an administrator at the console. Such a
    /// run is never offered the tools that act as the server.
    pub fn untrusted_origin(&self) -> bool {
        self.triggered || self.exposed
    }

    /// The trigger chain a write or a trigger run made by one of this run's
    /// tools carries: the [`chain`](Self::chain) that started the run — empty
    /// for a chat, since a tool call is not itself a trigger firing — and, for
    /// a run of [untrusted origin](Self::untrusted_origin), the
    /// [`UNTRUSTED_ORIGIN`](sc_catalog::UNTRUSTED_ORIGIN) mark, so a
    /// `trusted_input` trigger the write fires does not hand the server's
    /// tools back to the prompt that caused it (security review #32).
    pub fn tool_chain(&self) -> Vec<String> {
        if self.untrusted_origin() {
            sc_catalog::untrusted_chain(self.chain.clone())
        } else {
            self.chain.clone()
        }
    }

    /// Whether this caller meets a role floor — how an agent's `min_role` and a
    /// trigger's are both checked.
    ///
    /// Roles descend: 1 is admin and 100 is public, so *meeting* a floor is
    /// having a number no larger than it.
    pub fn meets_role(&self, min_role: Option<u8>) -> bool {
        self.role <= min_role.unwrap_or(1)
    }
}

/// Everything one tool call has access to.
///
/// Borrowed rather than owned (hence the lifetime): a call is a single `await` on
/// the driver's stack.
pub struct TraitContext<'a> {
    /// The data layer. A trait that touches rows goes through the row layer
    /// (`sc-api`, above this crate) with [`caller`](TraitContext::caller), rather
    /// than reaching the driver directly.
    pub catalog: &'a Catalog,
    /// Who this tool runs as.
    pub caller: &'a RunCaller,
    /// The name of the agent whose tool this is, for error messages: a tool
    /// failure the admin cannot attribute to an agent is one they cannot fix.
    pub agent: &'a str,
    /// The run this call belongs to, so a trait can record against it.
    pub run: RunId,
    /// The run's mode.
    pub mode: RunMode,
    /// This enabled trait instance's per-run state: `null` until the trait
    /// writes something. Saved with the run after the step and restored on
    /// resume. Reach it through [`state`](TraitContext::state).
    pub trait_state: &'a mut Json,
    /// The JavaScript engine, where the deployment has one.
    ///
    /// A tool that reads rows needs it exactly when the table it reads has an
    /// **untranslatable** ownership formula (§7.3): the rows come back and the
    /// formula decides per row, in V8. It is carried rather than reached for
    /// because a run may be driven from a context that has no engine, and the
    /// honest answer there is [`require_evaluator`](TraitContext::require_evaluator)'s
    /// configuration error rather than a read that quietly skips the check.
    pub evaluator: Option<&'a Arc<dyn JsEvaluator>>,
    /// The trigger dispatcher, where the deployment has one.
    ///
    /// A tool that runs a trigger (§11.3's `run_trigger`) needs *the* dispatcher
    /// — the same one every other event fires on — so what an agent runs is the
    /// trigger the admin configured, with its `only_if`, its cascade bound and
    /// its action. Carried rather than reached for, and for the same reason the
    /// evaluator is: a run may be driven from a context that has none, and the
    /// honest answer there is
    /// [`require_triggers`](TraitContext::require_triggers)' configuration error
    /// rather than a second way to fire an event.
    pub triggers: Option<&'a Arc<TriggerDispatcher>>,
    /// How this run delegates to another agent, where it can.
    ///
    /// The machinery already running this agent (`Runner`), offered back as a
    /// capability — because a trait that names a sub-agent has no registry, no
    /// provider connector and no idea how deep the chain already is, and every
    /// one of those is needed to start a run. Carried rather than reached for,
    /// like the two above it and for the same reason: a deployment that never
    /// assembled a provider connector cannot delegate, and
    /// [`require_delegate`](TraitContext::require_delegate)'s configuration error
    /// is the honest answer rather than a second, weaker way to run an agent.
    pub delegate: Option<&'a dyn Delegator>,
    /// How this run mounts a preview of an application, where the server can
    /// (TODO §7b). Carried for the evaluator's reason: a context without one
    /// says so through [`require_previews`](TraitContext::require_previews).
    pub previews: Option<&'a dyn AppPreviewer>,
    /// The headless browser, where the server has one. See
    /// [`require_browser`](TraitContext::require_browser).
    pub browser: Option<&'a dyn BrowserDriver>,
    /// How this run sends a request to an application, where the server can.
    /// See [`require_requests`](TraitContext::require_requests).
    pub requests: Option<&'a dyn AppRequester>,
    /// How this run lists and installs modules, where the server can.
    /// See [`require_modules`](TraitContext::require_modules).
    pub modules: Option<&'a dyn ModuleManager>,
    /// The signals this call has raised so far. Reach it through
    /// [`signal`](TraitContext::signal).
    pub signals: Vec<Signal>,
    /// Images this call's result carries beside its text. Reach it through
    /// [`attach_image`](TraitContext::attach_image).
    pub images: Vec<ImagePart>,
    /// What this call shows the person, asks them, and ran (TODO-agent-skills
    /// AS5, AS6). Reach it through [`show`](TraitContext::show),
    /// [`await_user`](TraitContext::await_user) and
    /// [`record_code`](TraitContext::record_code).
    pub extras: CallExtras,
}

/// What a tool call produces besides its result, for the loop to take after
/// the call.
#[derive(Debug, Default)]
pub struct CallExtras {
    /// Parts shown to the person and not the model.
    pub display: Vec<DisplayPart>,
    /// A question that ends the turn until the person answers.
    pub awaiting: Option<UserRequest>,
    /// Model-written code this call evaluated, for the ledger.
    pub code: Vec<CodeEvaluation>,
}

impl TraitContext<'_> {
    /// This trait instance's per-run state, for reading and writing.
    ///
    /// Scoped to the one enabled instance: two `coding` traits on one agent
    /// keep two states. It is JSON because the loop stores it and knows nothing
    /// of its shape.
    pub fn state(&mut self) -> &mut Json {
        self.trait_state
    }

    /// Tell the loop that something it should count happened — an edit that
    /// failed after the whole cascade, a check with new errors (TODO §10).
    ///
    /// Raising a signal changes nothing about this call's result. Repeated
    /// signals of one kind climb the escalation ladder.
    pub fn signal(&mut self, signal: Signal) {
        self.signals.push(signal);
    }

    /// Send `image` with this call's result, such as a screenshot (TODO §7b).
    /// The model sees it only where its provider takes images in a tool
    /// result; offering an image tool is the trait's decision.
    pub fn attach_image(&mut self, image: ImagePart) {
        self.images.push(image);
    }

    /// Show `part` to the person, beside this call's result, without the model
    /// seeing it (TODO-agent-skills AS6.3). Refused when it is too large.
    pub fn show(&mut self, part: DisplayPart) -> Result<()> {
        part.check()?;
        self.extras.display.push(part);
        Ok(())
    }

    /// Ask the person `request`: the turn ends after this round of tools, and
    /// the run waits for the answer, which reaches this trait's
    /// [`on_answer`](AgentTrait::on_answer) (AS6.1). One request per round: a
    /// second is refused.
    pub fn await_user(&mut self, request: UserRequest) -> Result<()> {
        request.check()?;
        if self.extras.awaiting.is_some() {
            return Err(Error::invalid(
                "this tool has already asked the person something this turn",
            ));
        }
        self.extras.awaiting = Some(request);
        Ok(())
    }

    /// Keep `evaluation` — the source of model-written code and how it went —
    /// in the run's ledger, for review (AS5.3).
    pub fn record_code(&mut self, evaluation: CodeEvaluation) {
        self.extras.code.push(evaluation);
    }

    /// How to mount a preview, or the configuration error that says this
    /// context cannot.
    pub fn require_previews(&self) -> Result<&dyn AppPreviewer> {
        self.previews.ok_or_else(|| {
            Error::config(format!(
                "agent `{}`: this needs the server's application previews, \
                 and this context has none",
                self.agent
            ))
        })
    }

    /// The headless browser, or the configuration error that says this context
    /// has none.
    pub fn require_browser(&self) -> Result<&dyn BrowserDriver> {
        self.browser.ok_or_else(|| {
            Error::config(format!(
                "agent `{}`: this needs the server's headless browser, \
                 and this context has none",
                self.agent
            ))
        })
    }

    /// How to send a request to an application, or the configuration error that
    /// says this context cannot.
    pub fn require_requests(&self) -> Result<&dyn AppRequester> {
        self.requests.ok_or_else(|| {
            Error::config(format!(
                "agent `{}`: this needs the server's application router, \
                 and this context has none",
                self.agent
            ))
        })
    }

    /// How to list and install modules, or the configuration error that says
    /// this context cannot.
    pub fn require_modules(&self) -> Result<&dyn ModuleManager> {
        self.modules.ok_or_else(|| {
            Error::config(format!(
                "agent `{}`: this needs the server's module installer, \
                 and this context has none",
                self.agent
            ))
        })
    }

    /// The engine, or the configuration error that says the server has none.
    ///
    /// Fail closed and fail loudly: a trait that cannot evaluate an ownership
    /// formula must not fall back to reading the rows anyway.
    pub fn require_evaluator(&self) -> Result<&Arc<dyn JsEvaluator>> {
        self.evaluator.ok_or_else(|| {
            Error::config(format!(
                "agent `{}`: this needs the JavaScript evaluator, \
                 and none is configured on this server",
                self.agent
            ))
        })
    }

    /// The trigger dispatcher, or the configuration error that says this context
    /// has none.
    pub fn require_triggers(&self) -> Result<&Arc<TriggerDispatcher>> {
        self.triggers.ok_or_else(|| {
            Error::config(format!(
                "agent `{}`: this needs the trigger dispatcher, \
                 and none is available in this context",
                self.agent
            ))
        })
    }

    /// How to run another agent, or the configuration error that says this
    /// context cannot.
    pub fn require_delegate(&self) -> Result<&dyn Delegator> {
        self.delegate.ok_or_else(|| {
            Error::config(format!(
                "agent `{}`: this needs to run another agent, \
                 and this context cannot delegate",
                self.agent
            ))
        })
    }
}

/// One turn about to be sent to the model, as [`AgentTrait::on_turn`] may change
/// it.
///
/// Deliberately narrow: a trait may *append* to the system prompt and it may
/// append messages. It cannot rewrite the agent's own prompt or edit the
/// conversation, because a trait that could silently change what the admin wrote
/// is one whose effect nobody could read off the agent's definition.
pub struct Turn<'a> {
    /// Who the run is for.
    pub caller: &'a RunCaller,
    /// The name of the agent taking the turn.
    pub agent: &'a str,
    /// Which model call this is, counting from 1 — so a trait can say something
    /// different on a later step than on the first.
    pub step: u32,
    /// The run's mode.
    pub mode: RunMode,
    /// The values of the chat's composer controls sent with the message that
    /// started this turn, by control name (TODO-agent-skills AS6.4); empty when
    /// none were sent.
    pub controls: BTreeMap<String, Json>,
    /// This trait instance's per-run state, read-only, as
    /// [`session_header`](AgentTrait::session_header) and its tools left it
    /// (`None` before it wrote any). Set by the loop before each trait's
    /// [`on_turn`](AgentTrait::on_turn), so a trait that computed something once
    /// per session (`preload_rows` placed in the system prompt, AS8.1) can
    /// append that same text on every step without computing it again.
    pub state: Option<&'a Json>,
    /// The live catalog, for a trait that reads rows into the turn
    /// (`search_table` and `similar_rows` in their `every_message` mode,
    /// TODO-agent-skills AS7.1, AS7.3). `None` for a turn built outside a run.
    pub catalog: Option<&'a Catalog>,
    /// The JavaScript engine, where the run has one (as
    /// [`TraitContext::evaluator`]): a read under an ownership formula that
    /// does not translate to SQL needs it.
    pub evaluator: Option<&'a Arc<dyn JsEvaluator>>,
    /// The conversation as it is about to be sent, so a trait can read the
    /// message that started this turn ([`Turn::user_message`]). Empty for a
    /// turn built outside a run.
    pub messages: &'a [sc_llm::LlmMessage],
    /// Paragraphs appended to the system prompt, in the order traits added them.
    extra_system: Vec<String>,
    /// Paragraphs sent after the conversation, in the order traits added them.
    extra_context: Vec<String>,
}

impl<'a> Turn<'a> {
    /// A turn with nothing added yet.
    pub fn new(caller: &'a RunCaller, agent: &'a str, step: u32) -> Turn<'a> {
        Turn {
            caller,
            agent,
            step,
            mode: RunMode::Act,
            controls: BTreeMap::new(),
            state: None,
            catalog: None,
            evaluator: None,
            messages: &[],
            extra_system: Vec::new(),
            extra_context: Vec::new(),
        }
    }

    /// Append a paragraph to the system prompt for this turn only.
    ///
    /// For text that is the **same on every step** of a session — rows
    /// preloaded once, a picked prompt. The system prompt comes before the
    /// conversation, so text that changes from one message to the next would
    /// make the host's prompt cache miss on the whole conversation after it;
    /// that is [`append_context`](Turn::append_context)'s.
    ///
    /// Blank text is dropped rather than stored, so a trait that computed nothing
    /// to say does not push a stray blank line into the prompt.
    pub fn append_system(&mut self, text: impl Into<String>) {
        let text = text.into();
        if !text.trim().is_empty() {
            self.extra_system.push(text);
        }
    }

    /// Append a paragraph of context for this turn only, sent **after the
    /// conversation**, as a last user message the run does not keep.
    ///
    /// For what changes with every message — rows found for the user's latest
    /// question — so the system prompt, the tools and the conversation so far
    /// stay a byte-identical prefix the host can cache. Blank text is dropped.
    pub fn append_context(&mut self, text: impl Into<String>) {
        let text = text.into();
        if !text.trim().is_empty() {
            self.extra_context.push(text);
        }
    }

    /// Everything [`append_context`](Turn::append_context) added, in order.
    pub fn extra_context(&self) -> &[String] {
        &self.extra_context
    }

    /// The trailing message the context is sent as, or `None` when there is
    /// none.
    pub fn context_message(&self) -> Option<sc_llm::LlmMessage> {
        (!self.extra_context.is_empty()).then(|| {
            sc_llm::LlmMessage::user(format!(
                "<context>\nThe system added this for the latest message; it is not \
                 from the user.\n\n{}\n</context>",
                self.extra_context.join("\n\n")
            ))
        })
    }

    /// The text of the latest user message in the conversation — the one this
    /// turn answers — or `None` when there is none (AS7.1's `every_message`).
    pub fn user_message(&self) -> Option<&str> {
        self.messages.iter().rev().find_map(|m| match m {
            sc_llm::LlmMessage::User { content, .. } if !content.trim().is_empty() => {
                Some(content.as_str())
            }
            _ => None,
        })
    }

    /// Everything appended, in order.
    pub fn extra_system(&self) -> &[String] {
        &self.extra_system
    }

    /// The agent's system prompt with every appended paragraph after it, blank
    /// line separated — what the request's `system` carries.
    pub fn system_prompt(&self, base: &str) -> String {
        let mut parts: Vec<&str> = Vec::with_capacity(self.extra_system.len() + 1);
        if !base.trim().is_empty() {
            parts.push(base);
        }
        parts.extend(self.extra_system.iter().map(String::as_str));
        parts.join("\n\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn a_system_run_is_admin_with_no_user() {
        let caller = RunCaller::system();
        assert_eq!(caller.role, 1);
        assert!(caller.user.is_none());
        // A trigger's authority reaches everything a role floor could ask for.
        assert!(caller.meets_role(None));
        assert!(caller.meets_role(Some(80)));
    }

    #[test]
    fn an_anonymous_run_is_public_with_no_user() {
        let caller = RunCaller::anonymous();
        assert_eq!(caller.role, 100);
        assert!(caller.user.is_none());
        // Only a floor of public is met: anything signed in is out of reach.
        assert!(caller.meets_role(Some(100)));
        assert!(!caller.meets_role(Some(80)));
        assert!(!caller.meets_role(None));
        // A visitor is somebody, and not a system run.
        assert!(caller.is_anonymous() && caller.untrusted_origin());
        assert!(!RunCaller::system().is_anonymous());
        assert!(!RunCaller::triggered().is_anonymous());
        assert!(RunCaller::triggered().untrusted_origin());
    }

    #[test]
    fn an_exposed_run_is_its_user_from_an_untrusted_origin() {
        let admin = User::new(Uuid::new_v4(), 1).unwrap();
        let caller = RunCaller::exposed(admin.clone());
        assert_eq!(caller.role, 1);
        assert!(caller.untrusted_origin() && !caller.is_anonymous());
        assert!(!RunCaller::user(admin).untrusted_origin());
    }

    #[test]
    fn a_user_run_carries_that_users_role() {
        let user = User::new(Uuid::new_v4(), 40).unwrap();
        let caller = RunCaller::user(user);
        assert_eq!(caller.role, 40);
        assert!(caller.user.is_some());
        // 40 meets a floor of 80 (roles descend) but not an admin-only agent's.
        assert!(caller.meets_role(Some(80)));
        assert!(caller.meets_role(Some(40)));
        assert!(!caller.meets_role(Some(10)));
        assert!(!caller.meets_role(None));
    }

    #[test]
    fn a_turn_appends_to_the_prompt_without_rewriting_it() {
        let caller = RunCaller::system();
        let mut turn = Turn::new(&caller, "librarian", 1);
        assert_eq!(
            turn.system_prompt("You answer questions."),
            "You answer questions."
        );

        turn.append_system("The books table has 12 rows.");
        // Blank additions are dropped rather than stored.
        turn.append_system("   ");
        assert_eq!(turn.extra_system().len(), 1);
        assert_eq!(
            turn.system_prompt("You answer questions."),
            "You answer questions.\n\nThe books table has 12 rows."
        );
    }

    #[test]
    fn an_agent_with_no_prompt_of_its_own_still_gets_the_additions() {
        let caller = RunCaller::system();
        let mut turn = Turn::new(&caller, "a", 2);
        turn.append_system("first");
        turn.append_system("second");
        assert_eq!(turn.system_prompt(""), "first\n\nsecond");
    }
}
