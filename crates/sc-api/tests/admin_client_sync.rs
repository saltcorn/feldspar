//! The checked-in admin clients (`ui/admin/src/client.ts`, `ui/builder/src/client.ts`,
//! `ui/ide/src/client.ts` and `ui/analytics/src/client.ts`) are **generated** artifacts and must not drift from the
//! endpoint contract they are generated from.
//!
//! Both bundles consume `client.ts` directly, so if someone changes an admin
//! endpoint without regenerating them, they would be typed against a stale
//! contract. This test regenerates the client from [`sc_api::admin_endpoints`] and
//! asserts each committed file byte-for-byte equals the output, pointing at the one
//! command that refreshes it.
//!
//! There are two copies rather than one shared file because the IDE is a separate
//! project with its own `tsconfig.json` and no import path into the SPA's sources
//! (design §12.1); this test is what keeps the copies honest.

use std::path::PathBuf;

#[test]
fn committed_admin_clients_match_generator() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    // Both halves: the client is this endpoint set's, the helper is every
    // client's, and a stale helper beside a fresh client is the same drift by
    // another route.
    let files = [
        (
            "client.ts",
            sc_api::generate_client(&sc_api::admin_endpoints()),
        ),
        (sc_api::CLIENT_HELPER_FILE, sc_api::client_helper()),
    ];

    for ui in ["ui/admin", "ui/ide", "ui/builder", "ui/analytics"] {
        for (name, generated) in &files {
            let path = root.join(ui).join("src").join(name);
            let committed = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            assert_eq!(
                &committed, generated,
                "{ui}/src/{name} is stale. Regenerate it with:\n  \
                 cargo run -p sc-api --example emit_admin_client -- {ui}/src/client.ts"
            );
        }
    }
}
