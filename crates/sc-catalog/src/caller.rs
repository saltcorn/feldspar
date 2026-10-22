//! Who is writing, and what led them here (§7.3, §10.2).
//!
//! A [`CallerContext`] travels with **every** row write, not only with the ones
//! on an RLS table. It started as the RLS half alone — the role and user an
//! `sc.role`/`sc.user` GUC carries into a policy — and grew the rest when table
//! events landed (Phase 4): an event has to say *who* caused the write, and the
//! row layer has no other way to know. One carrier rather than two, because a
//! second parameter threaded through the same eight entry points would be a
//! second thing to forget.
//!
//! So it answers three questions about one write:
//!
//! - **Whose authority?** [`role`](CallerContext::role) — what the policies gate
//!   on, and what an event reports.
//! - **Which user?** [`user`](CallerContext::user) — the fields the formula
//!   language binds `user` to, and the object the `sc.user` GUC carries.
//! - **What led here?** [`chain`](CallerContext::chain) — the trigger names, if
//!   any, whose actions caused this write. RLS ignores it; the dispatcher uses it
//!   to bound a cascade (`Event::firing`, §10.2's `MAX_DEPTH`). A write made by a
//!   request rather than by a trigger carries an empty chain, which is what makes
//!   depth 0 the ordinary case.

use serde_json::{Value as Json, json};
use uuid::Uuid;

/// The application a request came through (TODO-payments P1.3, design C5): what
/// a checkout's success URL and a portal's return URL are built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallingApp {
    /// The application's id.
    pub id: Uuid,
    /// Its public base URL as this request reached it — `https://shop.example.com`
    /// — with no trailing slash. An application served under two hosts reports
    /// the one this request used.
    pub url: String,
    /// The URL scheme its native apps open (`myshop`), when it has one.
    pub scheme: Option<String>,
}

impl CallingApp {
    /// The object a code body binds as `app`: `{ id, url, scheme }`.
    pub fn to_json(&self) -> Json {
        json!({ "id": self.id, "url": self.url, "scheme": self.scheme })
    }

    /// [`to_json`](CallingApp::to_json) read back — what a workflow run keeps
    /// of its event. `None` for anything that is not one.
    pub fn from_json(json: &Json) -> Option<CallingApp> {
        Some(CallingApp {
            id: json.get("id")?.as_str()?.parse().ok()?,
            url: json.get("url")?.as_str()?.to_owned(),
            scheme: json.get("scheme").and_then(Json::as_str).map(str::to_owned),
        })
    }
}

/// Where a request came from beyond who is signed in: the application it came
/// through and the **guest session** it carries (TODO-payments P1.3, P1.5).
///
/// The transport knows both, and the places that need them — an event's `app`,
/// an ownership formula's `session.id` — are many calls below it, past
/// signatures that carry only the user. So the transport runs the request
/// **inside** an origin ([`with_origin`]), and what builds a caller context or
/// an event reads it ([`current_origin`]) and carries it explicitly from there:
/// on [`CallerContext`], on the event, and back into scope by the dispatcher
/// while a trigger's action runs. Outside any request it is empty, which is
/// what a scheduled trigger, a service and a webhook all are.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Origin {
    /// The application the request came through.
    pub app: Option<CallingApp>,
    /// The **public id** of the caller's guest session — what a formula sees
    /// as `session.id` and a table stores. Never the cookie's token.
    pub session: Option<String>,
}

tokio::task_local! {
    static ORIGIN: Origin;
}

/// The origin this task is running inside, or the empty one.
pub fn current_origin() -> Origin {
    ORIGIN.try_with(Clone::clone).unwrap_or_default()
}

/// Run `fut` inside `origin`.
pub async fn with_origin<F: std::future::Future>(origin: Origin, fut: F) -> F::Output {
    ORIGIN.scope(origin, fut).await
}

/// The caller one row operation runs as.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CallerContext {
    /// The caller's role (public when anonymous).
    pub role: u8,
    /// The logged-in user's fields as a JSON object, or `None` when anonymous —
    /// in which case `sc.user` is left unset and the policies'
    /// `current_setting('sc.user', true)` reads `NULL`.
    pub user: Option<Json>,
    /// The trigger names whose actions led to this write, outermost first. Empty
    /// for a write a request made directly.
    pub chain: Vec<String>,
    /// The application the write's request came through, if any.
    pub app: Option<CallingApp>,
    /// The guest session's public id, if the caller has one — what the
    /// `sc.session` GUC carries.
    pub session: Option<String>,
}

impl CallerContext {
    /// A context for `role` carrying `user`'s fields.
    pub fn new(role: u8, user: Option<Json>) -> CallerContext {
        CallerContext {
            role,
            user,
            chain: Vec::new(),
            app: None,
            session: None,
        }
    }

    /// The same, from `origin`: the application and the guest session.
    pub fn from_origin(mut self, origin: Origin) -> CallerContext {
        self.app = origin.app;
        self.session = origin.session;
        self
    }

    /// The same, from the origin this task is running inside.
    pub fn in_current_origin(self) -> CallerContext {
        self.from_origin(current_origin())
    }

    /// The origin this context carries.
    pub fn origin(&self) -> Origin {
        Origin {
            app: self.app.clone(),
            session: self.session.clone(),
        }
    }

    /// A context for `role` with no user (anonymous).
    pub fn anonymous(role: u8) -> CallerContext {
        CallerContext::new(role, None)
    }

    /// The chain of triggers this write descends from — what
    /// `Event::firing` handed the action making it.
    pub fn chained(mut self, chain: Vec<String>) -> CallerContext {
        self.chain = chain;
        self
    }

    /// The user's fields as the JSON text the `sc.user` GUC carries.
    pub(crate) fn user_json(&self) -> Option<String> {
        self.user.as_ref().map(Json::to_string)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_caller_carries_its_role_its_user_and_what_led_here() {
        let anonymous = CallerContext::anonymous(100);
        assert_eq!(anonymous.role, 100);
        assert!(anonymous.user.is_none());
        // Nothing led here: the ordinary case is a request, at depth 0.
        assert!(anonymous.chain.is_empty());
        assert_eq!(anonymous.user_json(), None);

        let user = json!({ "id": 1, "email": "a@b.c" });
        let caller = CallerContext::new(1, Some(user.clone())).chained(vec!["audit".into()]);
        assert_eq!(caller.chain, vec!["audit".to_owned()]);
        // The GUC carries the object as compact JSON — what `UserEnv::Guc`'s
        // `current_setting('sc.user', …)::jsonb ->> 'x'` reads.
        assert_eq!(
            caller.user_json().as_deref(),
            Some(user.to_string().as_str())
        );
    }
}
