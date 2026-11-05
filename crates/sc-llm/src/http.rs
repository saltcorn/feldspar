//! The HTTP client a streaming model call is made on (security review 2026-11
//! F-11, TODO-type-hardening H5.1).
//!
//! rig builds a bare `reqwest::Client` when it is not handed one: no connect
//! timeout and no read timeout, so a provider that accepts the connection and
//! then says nothing holds the turn — and the agent run, and whoever is waiting
//! on it — for ever. A total timeout would be wrong here: a long answer streams
//! for minutes, and cutting it at a deadline cuts a working answer. What is
//! bounded is **silence**: the connect ([`sc_net::client_builder`]'s), and the
//! time between two reads of the response ([`PROVIDER_READ_TIMEOUT`]).

use std::time::Duration;

use sc_error::{Error, Result};

/// How long a model call may go without a byte arriving — before the first
/// token, or between two — before it is given up as a dead connection.
///
/// Generous on purpose: a reasoning model may think for minutes before it
/// streams anything, and a local model on a CPU can take as long to read a
/// long prompt. A run's own budgets (`wall_time`) are what bound a slow answer;
/// this bounds one that is not coming.
pub const PROVIDER_READ_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// The client a provider adapter streams on, with `read_timeout` between
/// reads.
pub(crate) fn provider_client(read_timeout: Duration) -> Result<reqwest::Client> {
    sc_net::client_builder()
        .read_timeout(read_timeout)
        .build()
        .map_err(|e| {
            Error::config(format!(
                "could not build the model provider's HTTP client: {e}"
            ))
        })
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "test code: the stub's own accept loop"
)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::anthropic::Anthropic;
    use crate::capabilities::ModelCapabilities;
    use crate::message::LlmRequest;
    use crate::provider::LlmProvider as _;

    /// A provider that takes the connection and then says nothing at all.
    async fn silent() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn a_provider_that_goes_silent_fails_the_call_at_the_read_timeout() {
        let base = silent().await;
        let provider = Anthropic::with_http(
            &base,
            "sk-ant-test",
            "claude-sonnet-4-5",
            ModelCapabilities::built_in(crate::ANTHROPIC_BACKEND, "claude-sonnet-4-5"),
            provider_client(Duration::from_millis(300)).unwrap(),
        )
        .unwrap();
        let started = Instant::now();
        let outcome = tokio::time::timeout(Duration::from_secs(20), async {
            match provider.stream(LlmRequest::prompt("hello")).await {
                Err(e) => Err(e),
                Ok(mut stream) => match stream.next().await {
                    Some(Err(e)) => Err(e),
                    other => Ok(other.is_some()),
                },
            }
        })
        .await
        .expect("the call is not held for ever");
        assert!(outcome.is_err(), "a silent provider produced {outcome:?}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "took {:?}",
            started.elapsed()
        );
    }
}
