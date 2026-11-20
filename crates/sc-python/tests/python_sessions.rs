//! Notebook sessions (analytics TODO A10.3): a cell's globals outlive it, the
//! last expression is its value, a session's runner decides what a cell
//! answers, and ending a session forgets what it held.

use std::time::Duration;

use sc_expr::CodeCall;
use sc_python::PythonRuntime;
use serde_json::json;

/// A session key no other test uses.
fn session() -> String {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    format!(
        "t-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    )
}

fn cell(code: &str) -> CodeCall<'static> {
    CodeCall {
        code: code.to_owned(),
        timeout: Some(Duration::from_secs(2)),
        ..CodeCall::default()
    }
}

#[tokio::test]
async fn a_global_defined_by_one_cell_is_read_by_the_next_until_the_session_ends() {
    let rt = PythonRuntime::new();
    let s = session();
    assert_eq!(
        rt.run_cell(&s, "", cell("x = 41")).await.unwrap(),
        json!(null)
    );
    assert_eq!(rt.run_cell(&s, "", cell("x + 1")).await.unwrap(), json!(42));
    // Another session has globals of its own.
    let other = session();
    let err = rt.run_cell(&other, "", cell("x")).await.unwrap_err();
    assert!(err.to_string().contains("NameError"), "{err}");
    // A runaway cell is stopped, and the session still runs the next.
    let err = rt
        .run_cell(&s, "", cell("while True:\n    pass"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("time limit"), "{err}");
    assert_eq!(rt.run_cell(&s, "", cell("x")).await.unwrap(), json!(41));
    // Ended, it is forgotten.
    assert!(rt.end_session(&s));
    assert!(!rt.end_session(&s));
    let err = rt.run_cell(&s, "", cell("x")).await.unwrap_err();
    assert!(err.to_string().contains("NameError"), "{err}");
}

#[tokio::test]
async fn a_sessions_runner_answers_for_its_cells_and_a_syntax_error_has_its_line() {
    let rt = PythonRuntime::new();
    let s = session();
    let setup = "def __session_cell__(code, last):\n    \
                 exec(code, globals())\n    \
                 return {'value': None if last is None else eval(last, globals()), 'n': n}\n\
                 n = 7\n";
    assert_eq!(
        rt.run_cell(&s, setup, cell("y = 2\ny * 3")).await.unwrap(),
        json!({ "value": 6, "n": 7 })
    );
    let err = rt
        .run_cell(&s, setup, cell("a = 1\n\nb = = 2"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("line 3"), "{err}");
    rt.end_session(&s);
}
