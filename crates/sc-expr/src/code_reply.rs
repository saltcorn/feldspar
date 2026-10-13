//! What a code body answers an HTTP caller with, beyond a JSON value (TODO
//! F1.4; design §10.1 "Status codes and error bodies").
//!
//! Three outcomes leave a code body that is serving a request:
//!
//! - `throw httpError(status, message, body?)` is an `Err` carrying
//!   [`sc_error::Repr::Http`]: a deliberate answer, served as it is;
//! - `return response({ status, headers, body })` is an `Ok` value wrapped
//!   under [`response_key`], which [`take_code_response`] unwraps. The key is
//!   random per process and only the host writes it (`op_sc_respond`), so a
//!   body that echoes its caller's JSON cannot forge a status or a header;
//! - anything else thrown is a **code failure** ([`code_failure`]), which the
//!   transport answers as a 500 (or a 503 for a clock or memory trip) without
//!   the stack to anyone who is not an admin.

use std::hash::{BuildHasher, Hasher};
use std::sync::OnceLock;

use sc_error::Error;
use serde_json::Value as Json;

/// The headers a code body may not set through `response(...)`: the server
/// owns them (CSP, cookies, framing, the transport's own), or they would let
/// a JSON body be served as something else. Checked in the guest, so the
/// author is told, and again here, so a header that slipped past is dropped.
pub const SERVER_OWNED_HEADERS: [&str; 16] = [
    "set-cookie",
    "content-security-policy",
    "content-security-policy-report-only",
    "x-frame-options",
    "x-content-type-options",
    "strict-transport-security",
    "cross-origin-opener-policy",
    "content-type",
    "content-length",
    "content-encoding",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "upgrade",
    "trailer",
    "te",
];

/// A body's `response(...)`, as the transport serves it.
#[derive(Debug, Clone, PartialEq)]
pub struct CodeResponse {
    /// 200–599, checked in the guest.
    pub status: u16,
    /// Lower-case names, none of them [`SERVER_OWNED_HEADERS`].
    pub headers: Vec<(String, String)>,
    /// The JSON body.
    pub body: Json,
    /// Raw bytes to serve in place of `body` (TODO F2.9): `response({ body:
    /// uint8array })`, base64 as it crossed.
    pub raw: Option<RawReply>,
    /// A store file to serve in place of `body` (TODO F2.10): `response({
    /// file })`. Not yet checked: the transport decides whether this run's
    /// caller may read it.
    pub file: Option<FileReply>,
    /// A stream handle (TODO F7): `return live.stream(name, key, { write,
    /// watch })`. Only a query whose `response_type` is `stream`, opened over
    /// the live socket, may answer with one.
    pub stream: Option<StreamReply>,
}

/// The handle of a `live.stream(name, key, { write, watch })`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamReply {
    /// The stream's name.
    pub name: String,
    /// The row's key: the topic.
    pub key: String,
    /// Whether the caller may edit the document or publish on the topic.
    pub write: bool,
    /// The tables the body asked to be re-checked on, beside those it read.
    pub watch: Vec<String>,
    /// The tables the run's database calls named, as the host recorded them.
    pub read: Vec<String>,
}

/// The bytes of a `response({ body: bytes })`.
#[derive(Debug, Clone, PartialEq)]
pub struct RawReply {
    /// The bytes, base64-encoded.
    pub base64: String,
    /// The `content-type` the body named, `application/octet-stream` otherwise.
    pub content_type: String,
}

/// The file of a `response({ file, filename, download })`.
#[derive(Debug, Clone, PartialEq)]
pub struct FileReply {
    /// The store's name.
    pub store: String,
    /// The store-relative path.
    pub path: String,
    /// `admin` or `user`: the authority the handle was opened with. A `user`
    /// handle is checked against the caller when it is served.
    pub authority: String,
    /// The `Content-Disposition` name, in place of the file's own.
    pub filename: Option<String>,
    /// An `attachment` rather than `inline`.
    pub download: bool,
    /// The `content-type`, in place of the one the path implies.
    pub content_type: Option<String>,
}

/// The key a `response(...)` answer is wrapped under on its way out of the
/// runtime: `$sc_response$` and 128 random bits, fixed for the process.
pub fn response_key() -> &'static str {
    static KEY: OnceLock<String> = OnceLock::new();
    KEY.get_or_init(|| {
        // `RandomState` is seeded from the OS per instance; two of them are
        // 128 bits nobody outside this process can know.
        let random = || {
            std::collections::hash_map::RandomState::new()
                .build_hasher()
                .finish()
        };
        format!("$sc_response${:016x}{:016x}", random(), random())
    })
}

/// The key a binding of raw bytes is wrapped under on its way into a run
/// (TODO F2.9): `$sc_bytes$` and 128 random bits, fixed for the process. The
/// prelude turns a top-level binding `{ <key>: base64 }` into a `Uint8Array`,
/// and since only the host knows the key, a JSON body cannot pose as one.
pub fn bytes_key() -> &'static str {
    static KEY: OnceLock<String> = OnceLock::new();
    KEY.get_or_init(|| {
        let random = || {
            std::collections::hash_map::RandomState::new()
                .build_hasher()
                .finish()
        };
        format!("$sc_bytes${:016x}{:016x}", random(), random())
    })
}

/// A binding that reaches a JavaScript body as a `Uint8Array` of the bytes
/// `base64` encodes.
pub fn bytes_binding(base64: String) -> Json {
    let mut out = serde_json::Map::new();
    out.insert(bytes_key().to_owned(), Json::String(base64));
    Json::Object(out)
}

/// Wrap a guest's `response(...)` (the JSON `{status, headers, body}`) for the
/// trip out of the runtime.
#[cfg(feature = "eval")]
pub(crate) fn wrap_response(spec: Json) -> Json {
    let mut out = serde_json::Map::new();
    out.insert(response_key().to_owned(), spec);
    Json::Object(out)
}

/// The `response(...)` a run answered with, or the plain value back.
pub fn take_code_response(value: Json) -> std::result::Result<CodeResponse, Json> {
    let is_wrapped = matches!(&value, Json::Object(map)
        if map.len() == 1 && map.contains_key(response_key()));
    if !is_wrapped {
        return Err(value);
    }
    let Json::Object(mut map) = value else {
        unreachable!("checked above");
    };
    let spec = map.remove(response_key()).unwrap_or(Json::Null);
    let status = spec
        .get("status")
        .and_then(Json::as_u64)
        .and_then(|s| u16::try_from(s).ok())
        .filter(|s| (200..=599).contains(s))
        .unwrap_or(200);
    let headers = spec
        .get("headers")
        .and_then(Json::as_array)
        .map(|pairs| {
            pairs
                .iter()
                .filter_map(|pair| {
                    let name = pair.get(0)?.as_str()?.to_ascii_lowercase();
                    let value = pair.get(1)?.as_str()?.to_owned();
                    allowed_header(&name, &value).then_some((name, value))
                })
                .collect()
        })
        .unwrap_or_default();
    let body = spec.get("body").cloned().unwrap_or(Json::Null);
    let text = |key: &str| spec.get(key).and_then(Json::as_str).map(str::to_owned);
    let content_type = text("contentType").filter(|v| allowed_value(v));
    let raw = text("bodyBase64").map(|base64| RawReply {
        base64,
        content_type: content_type
            .clone()
            .unwrap_or_else(|| "application/octet-stream".to_owned()),
    });
    let file = spec.get("file").and_then(|file| {
        Some(FileReply {
            store: file.get("store")?.as_str()?.to_owned(),
            path: file.get("path")?.as_str()?.to_owned(),
            authority: file.get("authority")?.as_str()?.to_owned(),
            filename: text("filename").filter(|v| !v.is_empty()),
            download: spec
                .get("download")
                .and_then(Json::as_bool)
                .unwrap_or(false),
            content_type: content_type.clone(),
        })
    });
    let stream = spec.get("stream").and_then(|stream| {
        let names = |key: &str| -> Vec<String> {
            stream
                .get(key)
                .and_then(Json::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(Json::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        };
        Some(StreamReply {
            name: stream.get("name")?.as_str()?.to_owned(),
            key: stream.get("key")?.as_str()?.to_owned(),
            write: stream.get("write").and_then(Json::as_bool).unwrap_or(false),
            watch: names("watch"),
            read: names("read"),
        })
    });
    Ok(CodeResponse {
        status,
        headers,
        body,
        raw,
        file,
        stream,
    })
}

/// Whether a code body may send this header: a token name that the server does
/// not own, and a value with no line break in it.
pub fn allowed_header(name: &str, value: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
        && !SERVER_OWNED_HEADERS.contains(&name)
        && allowed_value(value)
}

/// Whether a header value has no line break or null in it.
fn allowed_value(value: &str) -> bool {
    !value.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0)
}

/// What kind of code failure an error is, when it is one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeFailure {
    /// The body threw, or a promise it awaited rejected.
    Threw,
    /// A clock or the heap stopped it: its wall clock, its JavaScript slice,
    /// the isolate's memory, or a co-resident that tripped one of those.
    Timeout,
}

/// The phrases the runtime writes when a body threw. Kept beside the code that
/// writes them (`op_sc_fail`, `sc-python`'s interpreter).
const THREW: [&str; 2] = ["JavaScript code failed", "Python code failed"];
/// The phrases the runtime writes when a clock or the heap stopped a body
/// (`blame`, `bystander`, and the ops' deadline refusals).
const TIMED_OUT: [&str; 4] = [
    "ms time limit",
    "without awaiting anything",
    "used more memory than the JavaScript engine has",
    "another code body on the same isolate",
];

/// Whether `err` is a code body's failure — something the body did, rather
/// than a refusal before it ran — and which kind. A deliberate
/// [`httpError`](sc_error::Repr::Http) is not one.
pub fn code_failure(err: &Error) -> Option<CodeFailure> {
    if err.http_reply().is_some() {
        return None;
    }
    let text = err.causes();
    if TIMED_OUT.iter().any(|p| text.contains(p)) {
        return Some(CodeFailure::Timeout);
    }
    THREW
        .iter()
        .any(|p| text.contains(p))
        .then_some(CodeFailure::Threw)
}

#[cfg(all(test, feature = "eval"))]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_the_hosts_wrapper_is_a_response() {
        let wrapped = wrap_response(json!({
            "status": 201,
            "headers": [["x-id", "7"], ["set-cookie", "a=b"], ["X-Up", "1"], ["bad", "a\r\nb"]],
            "body": { "id": 7 },
        }));
        let reply = take_code_response(wrapped).unwrap();
        assert_eq!(reply.status, 201);
        assert_eq!(
            reply.headers,
            vec![
                ("x-id".to_owned(), "7".to_owned()),
                ("x-up".to_owned(), "1".to_owned())
            ],
            "server-owned and broken headers are dropped"
        );
        assert_eq!(reply.body, json!({ "id": 7 }));

        // A body echoing a lookalike is just a value.
        let forged = json!({ "$sc_response$0000": { "status": 302 } });
        assert_eq!(take_code_response(forged.clone()), Err(forged));
        assert!(response_key().len() > 40);
    }

    #[test]
    fn failures_are_classified_and_an_http_error_is_not_one() {
        let threw =
            Error::invalid("custom query `q`: `code`: invalid: JavaScript code failed: Error: x");
        assert_eq!(code_failure(&threw), Some(CodeFailure::Threw));
        let slice = Error::invalid("this code ran for 1000 ms without awaiting anything");
        assert_eq!(code_failure(&slice), Some(CodeFailure::Timeout));
        let rethrown = Error::invalid(
            "JavaScript code failed: Error: this code exceeded its 30000 ms time limit",
        );
        assert_eq!(code_failure(&rethrown), Some(CodeFailure::Timeout));
        assert_eq!(code_failure(&Error::http(403, "no", None)), None);
        assert_eq!(
            code_failure(&Error::invalid("this query needs the argument `n`")),
            None
        );
    }
}

/// The two globals, through a real isolate.
#[cfg(all(test, feature = "eval"))]
mod js_tests {
    use serde_json::json;

    use super::*;
    use crate::code::{CodeCall, CodeRuntime};

    fn call(code: &str) -> CodeCall<'static> {
        CodeCall {
            code: code.to_owned(),
            ..CodeCall::default()
        }
    }

    #[tokio::test]
    async fn http_error_is_a_deliberate_answer_and_response_a_wrapped_one() {
        let rt = CodeRuntime::with_workers(1);

        let err = rt
            .run(call(r#"throw httpError(401, "sign in");"#))
            .await
            .unwrap_err();
        let reply = err.http_reply().expect("an http reply");
        assert_eq!(
            (reply.status, reply.message.as_str(), reply.body.as_deref()),
            (401, "sign in", None)
        );
        assert_eq!(code_failure(&err), None);

        // Called without `throw` it still throws, and a body travels as JSON.
        let err = rt
            .run(call(
                r#"httpError(409, "taken", { field: "email" }); return 1;"#,
            ))
            .await
            .unwrap_err();
        let reply = err.http_reply().unwrap();
        assert_eq!(reply.status, 409);
        assert_eq!(reply.body.as_deref(), Some(r#"{"field":"email"}"#));

        // It is an Error a body can catch and inspect, like any other.
        let caught = rt
            .run(call(
                r#"try { httpError(404, "gone"); } catch (e) {
                     return [e instanceof Error, e.name, e.status, e.message, Object.isFrozen(e)]; }"#,
            ))
            .await
            .unwrap();
        assert_eq!(caught, json!([true, "HttpError", 404, "gone", true]));

        // A plain throw is a failure, as before.
        let err = rt
            .run(call(r#"throw new Error("boom");"#))
            .await
            .unwrap_err();
        assert_eq!(code_failure(&err), Some(CodeFailure::Threw));

        let ok = rt
            .run(call(
                r#"return response({ status: 201, headers: { "X-Id": "7" }, body: { id: 7 } });"#,
            ))
            .await
            .unwrap();
        let reply = take_code_response(ok).expect("a response");
        assert_eq!(reply.status, 201);
        assert_eq!(reply.headers, vec![("x-id".to_owned(), "7".to_owned())]);
        assert_eq!(reply.body, json!({ "id": 7 }));

        // The server's own headers are refused where they are written.
        let err = rt
            .run(call(
                r#"return response({ headers: { "Set-Cookie": "sid=1" }, body: {} });"#,
            ))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("may not set set-cookie"), "{err}");

        // A plain object that looks like one is a plain value.
        let plain = rt
            .run(call(r#"return { status: 201, headers: [], body: 1 };"#))
            .await
            .unwrap();
        assert!(take_code_response(plain).is_err());
    }

    /// TODO F2.9: a body bound as bytes arrives as a `Uint8Array`, and a
    /// response whose body is bytes carries them out with its content type.
    #[tokio::test]
    async fn bytes_in_and_bytes_out() {
        let rt = CodeRuntime::with_workers(1);
        let mut c = call(
            r#"if (!(body instanceof Uint8Array)) throw new Error("not bytes: " + typeof body);
               const out = new Uint8Array(body.length);
               for (let i = 0; i < body.length; i++) out[i] = body[body.length - 1 - i];
               return response({ body: out, headers: { "Content-Type": "image/png", "x-n": String(body.length) } });"#,
        );
        c.bindings
            .insert("body".to_owned(), bytes_binding("AQID/w==".to_owned()));
        let ok = rt.run(c).await.unwrap();
        let reply = take_code_response(ok).expect("a response");
        let raw = reply.raw.expect("raw bytes");
        assert_eq!(raw.base64, "/wMCAQ==");
        assert_eq!(raw.content_type, "image/png");
        assert_eq!(reply.headers, vec![("x-n".to_owned(), "4".to_owned())]);

        // Without a content type the bytes are octet-stream; a JSON body may
        // still not name one.
        let ok = rt
            .run(call(r#"return response({ body: new Uint8Array([1]) });"#))
            .await
            .unwrap();
        let raw = take_code_response(ok).unwrap().raw.unwrap();
        assert_eq!(raw.content_type, "application/octet-stream");
        let err = rt
            .run(call(
                r#"return response({ body: { a: 1 }, headers: { "content-type": "text/html" } });"#,
            ))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("may not set content-type"), "{err}");

        // A JSON object shaped like the wrapper, under a guessed key, stays JSON.
        let mut c = call(r#"return body instanceof Uint8Array;"#);
        c.bindings
            .insert("body".to_owned(), json!({ "$sc_bytes$00": "AQID" }));
        assert_eq!(rt.run(c).await.unwrap(), json!(false));

        // `filename` belongs to a file response, and a misspelt option is named.
        let err = rt
            .run(call(r#"return response({ body: {}, filename: "a.txt" });"#))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("serves a file"), "{err}");
        let err = rt
            .run(call(r#"return response({ bdy: {} });"#))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("`bdy` is not a response option"), "{err}");
    }
}
