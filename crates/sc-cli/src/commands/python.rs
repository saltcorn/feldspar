//! `feldspar python …`: the embedded Python.

use sc_error::Result;

/// `feldspar python check` (TODO-portable-release R1.7): start the embedded
/// interpreter with no database, print what it found, and exit non-zero when a
/// check failed. A build without the `python` feature says so and fails.
pub(crate) async fn python_command(args: &[String]) -> Result<()> {
    use sc_cli::python::{PythonArgs, format_check, verdict};

    let PythonArgs::Check { url, imports, env } = PythonArgs::parse(args)?;
    let pip = env.interpreter();
    let runtime = sc_python::PythonRuntime::new().with_env(env);
    // On a blocking thread: the HTTPS request waits on the network with the
    // interpreter's thread, and this command has nothing else to do meanwhile.
    let found = tokio::task::spawn_blocking(move || runtime.self_check(url.as_deref(), &imports))
        .await
        .map_err(|e| sc_error::Error::msg(format!("the Python self-check panicked: {e}")))??;
    print!("{}", format_check(&found, &pip));
    verdict(&found)
}
