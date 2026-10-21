//! **Throwaway** — the harness of the IMAP spike (TODO-imap.md M1.4). Delete it
//! with `plugins/imap/spike/`.
//!
//! It installs the scratch module `plugins/imap/spike/module` with `net: ["*"]`
//! and runs its actions on this server's Deno worker against a local Dovecot
//! and SMTP sink, asking the questions §2.1 rests on: does imapflow log in over
//! `node:tls` with a UTF-8 password, does IDLE stay open between module calls,
//! does mailparser parse a 20 MB mail inside the heap bound, does nodemailer
//! send through 465 and 587. The findings are written up in TODO-imap.md.
//!
//! `plugins/imap/spike/run-spike.sh` starts the servers and runs this; on its
//! own it skips unless `IMAP_SPIKE_DIR` is set.

#![cfg(feature = "deno-host")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crate::common;

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{have_npm, temp_root};
use sc_module::{ANY_HOST, CallHosts, Installer, ModuleHost, ModulePermissions, ModuleSource};
use serde_json::{Value as Json, json};

const PASSWORD: &str = "Grüße-€-pässwort";

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

async fn call(host: &ModuleHost, name: &str, action: &str, row: Json) -> Json {
    let started = std::time::Instant::now();
    let value = host
        .run(name, action, json!({ "row": row }), CallHosts::default())
        .await
        .unwrap_or_else(|e| panic!("{action}: {e}"));
    println!(
        "SPIKE {action} ({} ms): {value}",
        started.elapsed().as_millis()
    );
    value
}

fn deliver(spike: &PathBuf, eml: &str) {
    let status = std::process::Command::new(spike.join("servers/dovecot.sh"))
        .arg("deliver")
        .arg(spike.join("../test/fixtures/generated").join(eml))
        .status()
        .unwrap();
    assert!(status.success(), "doveadm save failed");
}

#[tokio::test]
#[ignore = "the M1.4 spike: needs plugins/imap/spike/run-spike.sh's servers, and npm downloads"]
async fn imap_spike() {
    let Some(spike) = std::env::var_os("IMAP_SPIKE_DIR").map(PathBuf::from) else {
        eprintln!("skipping: IMAP_SPIKE_DIR is not set (run plugins/imap/spike/run-spike.sh)");
        return;
    };
    skip_without!(have_npm(), "npm is not on the PATH");
    let ca = std::fs::read_to_string(spike.join("run/certs/ca.crt")).unwrap();

    let root = temp_root("imap-spike");
    let installer = Installer::new(&root);
    let package = installer
        .install(
            ModuleSource::Local,
            &spike.join("module").display().to_string(),
        )
        .await
        .unwrap_or_else(|e| panic!("installing the spike module: {e}"));
    let name = package.name;
    let host = ModuleHost::new(&root);
    let permissions = ModulePermissions {
        net: vec![ANY_HOST.to_owned()],
        ..ModulePermissions::closed()
    };
    let manifest = host
        .load(
            &name,
            &installer.package_dir(&name),
            &json!({}),
            &permissions,
        )
        .await
        .unwrap();
    assert!(manifest.issues.is_empty(), "{:?}", manifest.issues);

    let account = |port: u16| json!({ "port": port, "ca": ca, "user": "spike", "pass": PASSWORD });

    // 1. Log in over TLS (AUTHENTICATE PLAIN, imapflow's choice) and over
    //    STARTTLS with LOGIN, both with a UTF-8 password.
    let tls = call(&host, &name, "imap_login", account(10993)).await;
    assert_eq!(tls["authenticated"], json!(true));
    assert_eq!(tls["secure"], json!(true));
    let mut starttls = account(10143);
    starttls["loginMethod"] = json!("LOGIN");
    let starttls = call(&host, &name, "imap_login", starttls).await;
    assert_eq!(starttls["authenticated"], json!(true));

    // A wrong password fails as an authentication failure and is not echoed.
    let mut bad = account(10993);
    bad["pass"] = json!("falsches-pässwort");
    let bad = call(&host, &name, "imap_bad_login", bad).await;
    assert_eq!(bad["code"], json!("auth_failed"));
    assert!(!bad.to_string().contains("falsches"), "{bad}");

    // Certificates are verified (§5.1): without the spike's CA the server's
    // certificate is refused, by IMAP and by SMTP.
    let mut no_ca = account(10993);
    no_ca["ca"] = Json::Null;
    let refused = call(&host, &name, "imap_bad_login", no_ca).await;
    assert!(refused["error"].is_string(), "{refused}");
    assert_ne!(refused["code"], json!("auth_failed"), "{refused}");
    let mut no_ca = account(10465);
    no_ca["ca"] = Json::Null;
    let refused = call(&host, &name, "smtp_send", no_ca).await;
    assert!(refused["error"].is_string(), "{refused}");

    // Connecting to the checked address (an IP) while verifying the
    // certificate against the name (§5.1, M3.3): a wrong `servername` is
    // refused even though the certificate also names 127.0.0.1, so the name is
    // what is checked.
    let mut wrong_name = account(10993);
    wrong_name["servername"] = json!("wrong.example");
    let refused = call(&host, &name, "imap_bad_login", wrong_name).await;
    assert!(refused["error"].is_string(), "{refused}");
    let mut right_name = account(10993);
    right_name["servername"] = json!("localhost");
    let ok = call(&host, &name, "imap_login", right_name).await;
    assert_eq!(ok["authenticated"], json!(true));

    // 2. IDLE across calls. The mail is delivered by doveadm, outside any IMAP
    //    session, and no call runs for three seconds: an `exists` stamped near
    //    the delivery means the worker's event loop ran between calls.
    let started = call(&host, &name, "idle_start", account(10993)).await;
    let before = started["exists"].as_i64().unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let delivered_at = now_ms();
    deliver(&spike, "base64-windows-1252.eml");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let poll = call(&host, &name, "idle_poll", json!({})).await;
    let exists: Vec<&Json> = poll["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "exists")
        .collect();
    assert_eq!(exists.len(), 1, "{poll}");
    assert_eq!(exists[0]["count"], json!(before + 1));
    let lag = exists[0]["at"].as_i64().unwrap() - delivered_at;
    println!("SPIKE idle: exists arrived {lag} ms after delivery, with no call in flight");
    assert!(lag < 1500, "exists arrived {lag} ms after delivery");

    // Other calls in between — including a long parse — and a second delivery
    // while the parse is running.
    let parse = {
        let host = &host;
        let name = &name;
        async move { call(host, name, "parse_big", json!({ "mb": 20 })).await }
    };
    let second = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        deliver(&spike, "single-part-html.eml");
    };
    let (big, ()) = tokio::join!(parse, second);
    assert_eq!(big["intact"], json!(true));
    tokio::time::sleep(Duration::from_secs(2)).await;
    let poll = call(&host, &name, "idle_poll", json!({})).await;
    let counts: Vec<i64> = poll["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "exists")
        .map(|e| e["count"].as_i64().unwrap())
        .collect();
    assert_eq!(counts, [before + 1, before + 2], "{poll}");
    assert_eq!(poll["usable"], json!(true));

    // UID FETCH and mailparser on what arrived.
    let newest = call(&host, &name, "fetch_newest", account(10143)).await;
    assert_eq!(newest["subject"], json!("Single-part HTML"));

    // 3. mailparser at 20 MB and at the 25 MB cap, inside the heap bound.
    for mb in [20, 25] {
        let parsed = call(&host, &name, "parse_big", json!({ "mb": mb })).await;
        assert_eq!(parsed["intact"], json!(true), "{parsed}");
        assert_eq!(parsed["text"], json!("A big one."));
    }

    // 4. nodemailer through 465 (TLS, AUTH PLAIN) and 587 (STARTTLS, AUTH
    //    LOGIN), with the UTF-8 password.
    let sink_log = spike.join("run/sink/log.jsonl");
    let sent_before = std::fs::read_to_string(&sink_log)
        .unwrap_or_default()
        .lines()
        .count();
    let mut smtps = account(10465);
    smtps["method"] = json!("PLAIN");
    let sent = call(&host, &name, "smtp_send", smtps).await;
    assert_eq!(sent["accepted"], json!(["orders@feldspar.test"]), "{sent}");
    let mut submission = account(10587);
    submission["method"] = json!("LOGIN");
    let sent = call(&host, &name, "smtp_send", submission).await;
    assert_eq!(sent["accepted"], json!(["orders@feldspar.test"]), "{sent}");
    let log = std::fs::read_to_string(&sink_log).unwrap();
    let new: Vec<Json> = log
        .lines()
        .skip(sent_before)
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(new.len(), 2, "{log}");
    assert!(
        new.iter()
            .all(|l| l["auth"] == "spike" && l["secure"] == true),
        "{log}"
    );

    let poll = call(&host, &name, "idle_poll", json!({})).await;
    assert_eq!(
        poll["usable"],
        json!(true),
        "IDLE survived everything above"
    );
    call(&host, &name, "idle_stop", json!({})).await;

    host.shutdown().await;
    let _ = std::fs::remove_dir_all(installer.root());
}
