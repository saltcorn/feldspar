//! Sending one request under a [`HostPolicy`], and reading a bounded answer.
//!
//! Redirects are followed **here**, not by reqwest, because each hop is a new
//! URL the policy has to admit: an allow-listed host that redirects to one that
//! is not must stop at the redirect, and so must a public page that redirects
//! to `http://169.254.169.254/`. Following them by hand is also what lets the
//! configured headers be dropped when a hop changes host, the way a browser
//! drops `Authorization`.

use std::time::{Duration, Instant};

use reqwest::header::{ACCEPT, CONTENT_TYPE, COOKIE, HeaderMap, HeaderValue, LOCATION, SET_COOKIE};
use reqwest::{Method, StatusCode, Url};
use sc_error::{Error, Result, format_causes};
use serde_json::Value as Json;

use super::cookies::CookieJar;
use super::guard::{HostPolicy, PublicResolver};

/// The most of a response body that is read. A documentation page is a few
/// hundred kilobytes; past this the rest is not read at all, and the result
/// says so.
pub const MAX_DOWNLOAD_BYTES: usize = 5 * 1024 * 1024;

/// The most redirects one request follows.
pub const MAX_REDIRECTS: usize = 5;

/// What a request asks for when the caller does not say: Markdown first — a
/// growing number of documentation sites serve it to agents that ask, and it
/// needs no conversion — then HTML, then anything textual.
const DEFAULT_ACCEPT: &str =
    "text/markdown, text/html;q=0.9, text/plain;q=0.8, application/json;q=0.8, */*;q=0.1";

/// The two clients a policy chooses between: one that resolves names to
/// public addresses only, and one that does not filter.
pub struct HttpClient {
    public: reqwest::Client,
    open: reqwest::Client,
}

impl HttpClient {
    /// Build both clients. Fallible because building a client initialises TLS,
    /// and a server where that fails should say so at boot.
    pub fn new() -> Result<HttpClient> {
        Ok(HttpClient {
            public: builder()
                .dns_resolver(PublicResolver)
                .build()
                .map_err(build_failed)?,
            open: builder().build().map_err(build_failed)?,
        })
    }

    /// Send `request` under `policy`, following redirects the policy admits,
    /// and read at most [`MAX_DOWNLOAD_BYTES`] of the final response.
    ///
    /// A non-2xx status is **not** an error here: the caller decides, because a
    /// 404 page is still something a model can be shown a line of.
    pub async fn send(&self, policy: &HostPolicy, request: Request) -> Result<Response> {
        self.send_with_jar(policy, request, None).await
    }

    /// [`send`](HttpClient::send), with a cookie jar: every hop carries the
    /// cookies `jar` holds for its host, and what each hop sets is kept in it
    /// (TODO-agent-skills AS9.3).
    pub async fn send_with_jar(
        &self,
        policy: &HostPolicy,
        request: Request,
        mut jar: Option<&mut CookieJar>,
    ) -> Result<Response> {
        let client = match policy.private_network() {
            true => &self.open,
            false => &self.public,
        };
        let deadline = Instant::now() + request.timeout;
        let Request {
            mut method,
            mut url,
            mut headers,
            mut body,
            ..
        } = request;
        policy.check(&url)?;
        let accept = headers
            .get(ACCEPT)
            .cloned()
            .unwrap_or_else(|| HeaderValue::from_static(DEFAULT_ACCEPT));
        headers.insert(ACCEPT, accept.clone());
        let mut redirects = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(timed_out(&url));
            }
            let mut hop_headers = headers.clone();
            if let Some(cookie) = jar.as_deref().and_then(|j| j.header_for(&url))
                && let Ok(value) = HeaderValue::from_str(&cookie)
            {
                hop_headers.insert(COOKIE, value);
            }
            let mut builder = client
                .request(method.clone(), url.clone())
                .timeout(remaining)
                .headers(hop_headers);
            builder = match &body {
                None => builder,
                Some(Json::String(text)) => builder.body(text.clone()),
                Some(json) => builder.json(json),
            };
            let mut response = builder.send().await.map_err(|e| send_failed(&url, &e))?;
            let status = response.status();
            if let Some(jar) = jar.as_deref_mut() {
                for set in response.headers().get_all(SET_COOKIE) {
                    if let Ok(set) = set.to_str() {
                        jar.store(&url, set);
                    }
                }
            }

            if status.is_redirection()
                && let Some(location) = response.headers().get(LOCATION)
            {
                if redirects.len() >= MAX_REDIRECTS {
                    return Err(Error::invalid(format!(
                        "{url} redirected more than {MAX_REDIRECTS} times"
                    )));
                }
                let location = location.to_str().unwrap_or_default();
                let next = url.join(location).map_err(|e| {
                    Error::invalid(format!(
                        "{url} redirected to `{location}`, which is not a URL: {e}"
                    ))
                })?;
                policy
                    .check(&next)
                    .map_err(|e| Error::auth(format!("{url} redirected to {next}, and {e}")))?;
                // 303 always, and 301/302 after a POST in practice, turn into a
                // GET with no body; 307 and 308 repeat the request as it was.
                if status == StatusCode::SEE_OTHER
                    || (matches!(status, StatusCode::MOVED_PERMANENTLY | StatusCode::FOUND)
                        && method == Method::POST)
                {
                    method = Method::GET;
                    body = None;
                }
                // The configured headers carry credentials, and those belong
                // to the host they were configured for.
                if next.host_str() != url.host_str() {
                    headers.clear();
                    headers.insert(ACCEPT, accept.clone());
                }
                redirects.push(url);
                url = next;
                continue;
            }

            let content_type = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let response_headers = response
                .headers()
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str().to_owned(),
                        value.to_str().unwrap_or_default().to_owned(),
                    )
                })
                .collect();
            let mut bytes = Vec::new();
            let mut truncated = false;
            while let Some(chunk) = response.chunk().await.map_err(|e| send_failed(&url, &e))? {
                let room = MAX_DOWNLOAD_BYTES - bytes.len();
                if chunk.len() > room {
                    bytes.extend_from_slice(&chunk[..room]);
                    truncated = true;
                    break;
                }
                bytes.extend_from_slice(&chunk);
            }
            return Ok(Response {
                url,
                redirects,
                status: status.as_u16(),
                content_type,
                headers: response_headers,
                body: bytes,
                truncated,
            });
        }
    }
}

/// What every client here starts from: no automatic redirects (see the module
/// comment), no proxy from the environment — a proxy resolves the name itself,
/// which would take the address check out of this process — and a user agent
/// that says what it is.
fn builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(Duration::from_secs(10))
        .user_agent(concat!("feldspar-agent/", env!("CARGO_PKG_VERSION")))
}

fn build_failed(e: reqwest::Error) -> Error {
    Error::config(format!("could not build the HTTP client: {e}"))
}

fn timed_out(url: &Url) -> Error {
    Error::invalid(format!("the request to {url} timed out"))
}

fn send_failed(url: &Url, e: &reqwest::Error) -> Error {
    if e.is_timeout() {
        return timed_out(url);
    }
    Error::invalid(format!("the request to {url} failed: {}", format_causes(e)))
}

/// One request, as the trait builds it from a tool call.
pub struct Request {
    pub method: Method,
    pub url: Url,
    /// The configured headers. Sent to the URL's host only: a redirect to
    /// another host is sent with `Accept` alone.
    pub headers: HeaderMap,
    /// A JSON body, or a string sent as it is.
    pub body: Option<Json>,
    /// The whole request's budget, redirects and body included.
    pub timeout: Duration,
}

/// What came back.
pub struct Response {
    /// The URL that answered, after redirects.
    pub url: Url,
    /// The URLs that redirected, in order.
    pub redirects: Vec<Url>,
    pub status: u16,
    pub content_type: Option<String>,
    /// The final response's headers, in order.
    pub headers: Vec<(String, String)>,
    /// At most [`MAX_DOWNLOAD_BYTES`] of the body.
    pub body: Vec<u8>,
    /// Whether the body was longer than what was read.
    pub truncated: bool,
}
