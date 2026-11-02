//! A host's auth refusal that a Python body lets escape ends the run **as that
//! refusal** — the failure text is unchanged, and the refusal is underneath it
//! for a transport to answer with a 403 naming it (`Error::auth_message`).
//! Anything else the body raises, a refusal it caught and replaced included,
//! is the ordinary failure it always was.

use async_trait::async_trait;
use sc_error::{Error, Result};
use sc_expr::{CodeCall, CodeHost};
use sc_python::PythonRuntime;
use serde_json::Value as Json;

/// Refuses every call: as an auth refusal, or as some other error.
struct Refusing {
    auth: bool,
}

#[async_trait]
impl CodeHost for Refusing {
    async fn call(&self, _request: Json) -> Result<Json> {
        match self.auth {
            true => Err(Error::auth("you may not read `secrets`")),
            false => Err(Error::invalid("there is no table `secrets`")),
        }
    }
}

async fn failure(auth: bool, code: &str) -> Error {
    let host = Refusing { auth };
    let result = PythonRuntime::new()
        .run(CodeCall {
            code: code.to_owned(),
            host: Some(&host as &dyn CodeHost),
            ..CodeCall::default()
        })
        .await;
    match result {
        Ok(value) => panic!("the body was supposed to fail, and answered {value}"),
        Err(e) => e,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_uncaught_auth_refusal_ends_the_run_as_that_refusal() {
    let err = failure(true, "return db.secrets.rows()\n").await;
    assert_eq!(err.auth_message(), Some("you may not read `secrets`"));
    // What a log or an admin reads is the failure it always was.
    assert!(
        err.to_string().starts_with("this Python code failed"),
        "{err}"
    );
    assert!(sc_expr::code_failure(&err).is_some(), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_caught_and_replaced_refusal_and_any_other_error_are_not_refusals() {
    let replaced = failure(
        true,
        "import saltcorn as sc\ntry:\n    db.secrets.rows()\nexcept sc.DbError:\n    \
         raise ValueError('no secrets for you')\n",
    )
    .await;
    assert_eq!(replaced.auth_message(), None, "{replaced}");

    // The same exception re-raised is still the refusal.
    let reraised = failure(
        true,
        "import saltcorn as sc\ntry:\n    db.secrets.rows()\nexcept sc.DbError:\n    raise\n",
    )
    .await;
    assert_eq!(
        reraised.auth_message(),
        Some("you may not read `secrets`"),
        "{reraised}"
    );

    let other = failure(false, "return db.secrets.rows()\n").await;
    assert_eq!(other.auth_message(), None, "{other}");
}
