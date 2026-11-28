//! **Agents and applications for modules** (TODO-app-constructor AC12.2,
//! AC12.3).
//!
//! A module can work with agents and applications:
//!
//! ```js
//! await agents.save({ name: "notetaker", provider: "house", traits: [{ trait: "notes" }] });
//! const { id } = await agents.start({ agent: "notetaker", prompt: "Write it down" });
//! const run = await agents.wait(id, { timeout_ms: 60000 });
//! await apps.write("crm", "src/App.tsx", source);
//! ```
//!
//! A route gets these as `req.feldspar.agents` and `req.feldspar.apps`; a
//! service and the application-deleted hook get them as `ctx.agents` and
//! `ctx.apps`.
//!
//! **Why this is a trait.** Starting a run needs the agent registry, the LLM
//! providers and a runner; building an application needs the application
//! mounts. Those live in `sc-server`, which depends on this crate, so this
//! crate cannot call them. Instead the server implements [`ServerAsks`] and
//! hands it over at startup, and this crate asks it for a handler each time it
//! sets up a module call.
//!
//! **What this crate decides.** Which calls get the handlers, and as whom:
//! - a route acts as the signed-in caller;
//! - a service and the hook act as the module itself (no caller).
//!
//! The handler is also told what an admin allowed the module
//! ([`AgentGrant`], [`AppsGrant`]); a module allowed nothing gets a handler
//! that refuses, saying how to allow it.

use sc_catalog::Principal;

use crate::host::Ask;
use crate::permissions::{AgentGrant, AppsGrant};

/// Makes the handlers for a module call's `agents` and `apps`; implemented by
/// the server.
///
/// Called once per module call, for the module `module`, with what an admin
/// allowed it (`grant`) and who is calling: `Some(caller)` for a route, `None`
/// for a service or the hook.
pub trait ServerAsks: Send + Sync {
    /// The agents the module makes and the runs it starts.
    fn agents(
        &self,
        module: &str,
        grant: Option<AgentGrant>,
        caller: Option<&Principal>,
    ) -> Box<dyn Ask>;

    /// The applications the module reads, changes and builds.
    fn apps(
        &self,
        module: &str,
        grant: Option<AppsGrant>,
        caller: Option<&Principal>,
    ) -> Box<dyn Ask>;
}
