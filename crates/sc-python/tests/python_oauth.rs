//! `oauth` from a Python body: `oauth.begin` and `oauth.access_token` lower to
//! the `op: "oauth"` requests on the `db` host that the JavaScript `oauth`
//! sends, which is what `sc-api`'s host answers (`sc-server`'s `app_oauth.rs`,
//! end to end).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sc_error::Result;
use sc_expr::{CodeCall, CodeHost};
use sc_python::PythonRuntime;
use serde_json::{Value as Json, json};

/// Keeps every request; answers `begin` with a url and `access_token` with a
/// token.
#[derive(Default)]
struct Echo {
    requests: Mutex<Vec<Json>>,
}

#[async_trait]
impl CodeHost for Echo {
    async fn call(&self, request: Json) -> Result<Json> {
        self.requests.lock().unwrap().push(request.clone());
        Ok(match request["what"].as_str() {
            Some("begin") => json!({ "url": "/_feldspar/oauth/consent/x", "expires_in": 600 }),
            _ => json!({ "access_token": "tok", "expires_at": null, "refreshed": false }),
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oauth_lowers_to_the_requests_the_javascript_oauth_sends() {
    let host = Arc::new(Echo::default());
    let answer = PythonRuntime::new()
        .run(CodeCall {
            code: r#"
store = {"table": "connections", "id": 1, "fields": {"access_token": "a"}}
url = oauth.begin(provider="google", client_id="cid", client_secret="cs",
                  scope="s", params={"login_hint": "ada"}, return_to="/x", store=store)["url"]
token = oauth.access_token(provider="google", client_id="cid", store=store, refused="old")
full = oauth.access_token(token_url="https://t", client_id="cid", store=store, detail=True)
try:
    oauth.begin(provider="yahoo", client_id="c", scope="s", store=store)
    refused = None
except TypeError as e:
    refused = str(e)
return [url, token, full["refreshed"], refused]
"#
            .to_owned(),
            host: Some(host.as_ref() as &dyn CodeHost),
            ..CodeCall::default()
        })
        .await
        .unwrap();
    assert_eq!(answer[0], json!("/_feldspar/oauth/consent/x"));
    assert_eq!(answer[1], json!("tok"));
    assert_eq!(answer[2], json!(false));
    assert!(answer[3].as_str().unwrap().contains("yahoo"), "{answer}");
    let requests = host.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 3, "{requests:?}");
    let store = json!({ "table": "connections", "id": 1, "fields": { "access_token": "a" } });
    assert_eq!(
        requests[0],
        json!({ "op": "oauth", "what": "begin", "store": store, "client_id": "cid",
                "client_secret": "cs", "scope": "s", "return_to": "/x",
                "authorization_url": "https://accounts.google.com/o/oauth2/v2/auth",
                "token_url": "https://oauth2.googleapis.com/token",
                "params": { "access_type": "offline", "prompt": "consent",
                            "login_hint": "ada" } })
    );
    assert_eq!(
        requests[1],
        json!({ "op": "oauth", "what": "access_token", "store": store, "client_id": "cid",
                "token_url": "https://oauth2.googleapis.com/token", "refused": "old" })
    );
    assert_eq!(
        requests[2],
        json!({ "op": "oauth", "what": "access_token", "store": store, "client_id": "cid",
                "token_url": "https://t" })
    );
}
