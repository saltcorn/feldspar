//! A coding agent's requests to an application (`call_api`), sent through
//! this server's own router.
//!
//! **In process, not over a socket.** The request is handed to the same
//! [`Router`] the public listener serves, with the application's host in its
//! `Host` header, so it meets the same CSRF middleware, the same session
//! lookup and the same API providers a browser's request does — and the
//! server need not be listening anywhere a request could reach. No
//! `ConnectInfo` is attached, so the peer is unknown and every loopback-only
//! rule treats it as remote.
//!
//! **The session is made for the one request.** A request as a user logs that
//! user in ([`SessionStore::login`]), sends the token as the session cookie,
//! and logs it out when the answer is read, whatever the answer was. A request
//! as nobody carries no session cookie. Both carry a fresh CSRF token in the
//! cookie and the header, which is what the application's own page sends.
//!
//! **The live mount, or a run's preview.** The host is
//! `<subdomain>.<base-domain>`, and a subdomain nothing is mounted at is refused
//! here rather than falling through to the admin routes that host would
//! otherwise reach. A request naming a preview (a checklist run against an
//! agent's build, TODO-checklists C2.6) goes to the preview's host instead,
//! with a pass its run allows for this one request and revokes after it.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderName, HeaderValue, Request, header};
use futures::StreamExt;
use sc_agent::{AppHttpRequest, AppHttpResponse, AppRequester};
use sc_api::EndpointSet;
use sc_auth::SessionStore;
use sc_error::{Error, Result};
use tower::ServiceExt;

use crate::apps::AppMounts;
use crate::config::ServerConfig;
use crate::handler::HandlerRegistry;
use crate::router::build_router_with_apps;
use crate::security::{CSRF_COOKIE, CSRF_HEADER, PREVIEW_COOKIE, SESSION_COOKIE, new_csrf_token};

/// The most of a response body kept.
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// Sends `call_api`'s requests to the applications `apps` mounts.
pub struct AppRequests {
    router: Router,
    sessions: Arc<SessionStore>,
    apps: Arc<AppMounts>,
    base_domain: String,
}

impl AppRequests {
    /// A requester over `router`, the router the server serves. Its sessions
    /// are made in `sessions`, which must be the store `router` reads.
    pub fn new(
        router: Router,
        sessions: Arc<SessionStore>,
        apps: Arc<AppMounts>,
        base_domain: impl Into<String>,
    ) -> AppRequests {
        AppRequests {
            router,
            sessions,
            apps,
            base_domain: base_domain.into(),
        }
    }

    /// Send one request with the given session, and read the answer.
    async fn send(
        &self,
        request: &AppHttpRequest<'_>,
        session: Option<&str>,
        pass: Option<&str>,
    ) -> Result<AppHttpResponse> {
        let host = match request.preview {
            Some(preview) => preview.host.clone(),
            None => format!("{}.{}", request.subdomain, self.base_domain),
        };
        let csrf = new_csrf_token();
        let mut cookie = match session {
            Some(token) => format!("{SESSION_COOKIE}={token}; {CSRF_COOKIE}={csrf}"),
            None => format!("{CSRF_COOKIE}={csrf}"),
        };
        if let Some(pass) = pass {
            cookie.push_str(&format!("; {PREVIEW_COOKIE}={pass}"));
        }
        let mut builder = Request::builder()
            .method(request.method.as_str())
            .uri(&request.path);
        for (name, value) in &request.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        let mut http = builder
            .body(Body::from(request.body.clone()))
            .map_err(|e| Error::invalid(format!("the request could not be made: {e}")))?;
        // Set last, so no header the caller gave can stand in for them.
        let headers = http.headers_mut();
        for (name, value) in [
            (header::HOST, host.as_str()),
            (header::COOKIE, cookie.as_str()),
            (HeaderName::from_static(CSRF_HEADER), csrf.as_str()),
        ] {
            let value = HeaderValue::from_str(value)
                .map_err(|e| Error::invalid(format!("`{name}` could not be set: {e}")))?;
            headers.insert(name, value);
        }

        let deadline = tokio::time::Instant::now() + request.timeout;
        let seconds = request.timeout.as_secs();
        let response = tokio::time::timeout_at(deadline, self.router.clone().oneshot(http))
            .await
            .map_err(|_| {
                Error::invalid(format!(
                    "`{}` sent no answer to {} {} within {seconds} seconds",
                    request.subdomain, request.method, request.path
                ))
            })?
            .map_err(|e| Error::msg(format!("the router failed: {e}")))?;

        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect();
        let mut stream = response.into_body().into_data_stream();
        let mut body = Vec::new();
        let mut truncated = None;
        loop {
            match tokio::time::timeout_at(deadline, stream.next()).await {
                Ok(None) => break,
                Ok(Some(Ok(chunk))) => {
                    let room = MAX_RESPONSE_BYTES - body.len();
                    if chunk.len() > room {
                        body.extend_from_slice(&chunk[..room]);
                        truncated = Some(format!(
                            "it is over {} KB, and only that much was read",
                            MAX_RESPONSE_BYTES / 1024
                        ));
                        break;
                    }
                    body.extend_from_slice(&chunk);
                }
                Ok(Some(Err(e))) => {
                    truncated = Some(format!("reading it failed: {e}"));
                    break;
                }
                Err(_) => {
                    truncated = Some(format!(
                        "it was still arriving after {seconds} seconds (a stream?)"
                    ));
                    break;
                }
            }
        }
        Ok(AppHttpResponse {
            status,
            headers,
            body,
            truncated,
        })
    }
}

#[async_trait::async_trait]
impl AppRequester for AppRequests {
    async fn request(&self, request: AppHttpRequest<'_>) -> Result<AppHttpResponse> {
        if request.preview.is_none() && self.apps.get(request.subdomain).is_none() {
            return Err(Error::invalid(format!(
                "no application is being served at `{}`: it has no live mount (build it, or \
                 check the server's log for why it did not mount)",
                request.subdomain
            )));
        }
        let session = match request.user {
            Some(user) => Some(self.sessions.login(user.clone()).await?),
            None => None,
        };
        let pass = request.preview.map(|preview| {
            let pass = new_csrf_token();
            self.apps
                .allow_preview_token(sc_agent::RunId(preview.owner), &pass);
            (preview.owner, pass)
        });
        let result = self
            .send(&request, session.as_deref(), pass.as_ref().map(|(_, p)| p.as_str()))
            .await;
        if let Some((owner, pass)) = &pass {
            self.apps
                .revoke_preview_token(sc_agent::RunId(*owner), pass);
        }
        if let Some(token) = &session {
            if let Err(e) = self.sessions.logout(token).await {
                sc_log::log_warn!("call_api's session could not be deleted: {e}");
            }
        }
        result
    }
}

/// Install `call_api`'s requester into the agents' view services, where this
/// server has applications (a base domain) and agents. Returns it.
///
/// Its router is built like the browser's loopback one, without `Secure`
/// cookies: the requests never leave the process, and what the router sets
/// is only read back as text.
pub fn install_app_requests(
    config: &ServerConfig,
    endpoints: &EndpointSet,
    handlers: &HandlerRegistry,
    sessions: &Arc<SessionStore>,
    apps: &Arc<AppMounts>,
) -> Result<Option<Arc<AppRequests>>> {
    let (Some(base_domain), Some(agents)) = (config.base_domain.clone(), apps.agents()) else {
        return Ok(None);
    };
    let local = ServerConfig {
        secure_cookies: false,
        ..config.clone()
    };
    let router = build_router_with_apps(
        endpoints,
        handlers.clone(),
        sessions.clone(),
        &local,
        apps.clone(),
    )?;
    let requests = Arc::new(AppRequests::new(
        router,
        sessions.clone(),
        apps.clone(),
        base_domain,
    ));
    agents
        .registry()
        .view_services()
        .set_requests(requests.clone());
    Ok(Some(requests))
}
