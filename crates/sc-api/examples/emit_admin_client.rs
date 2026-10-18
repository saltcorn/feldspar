//! Emit the admin UI's generated TypeScript client.
//!
//! The `ui/admin` SPA consumes a typed client generated from the admin endpoint
//! set (design §13.1). That client is a **generated** artifact: this example is
//! the single command that produces it, so the checked-in `ui/admin/src/client.ts`
//! never drifts from the Rust endpoint contract. A test
//! (`tests/admin_client_sync.rs`) asserts the committed files match this output.
//!
//! Two files, not one: the client imports the generic half of itself — how a
//! request is made, how a failure is reported, the types a table's reads are
//! expressed in — from a `helper.ts` beside it, which is the same text for every
//! generated client. Given a path, this writes both.
//!
//! Usage:
//!
//! ```text
//! # print the client to stdout
//! cargo run -p sc-api --example emit_admin_client
//! # or write the client and its helper into the SPA source tree
//! cargo run -p sc-api --example emit_admin_client -- ui/admin/src/client.ts
//! ```

use std::io::Write;

fn main() -> std::io::Result<()> {
    let endpoints = sc_api::admin_endpoints();
    let client = sc_api::generate_client(&endpoints);
    match std::env::args().nth(1) {
        Some(path) => {
            let path = std::path::PathBuf::from(path);
            for (name, text) in sc_api::client_runtime(sc_api::has_offline_sync(&endpoints)) {
                std::fs::write(path.with_file_name(name), text)?;
            }
            std::fs::write(path, client)
        }
        None => std::io::stdout().write_all(client.as_bytes()),
    }
}
