//! The certificate's name set is **live**: creating an application puts its
//! subdomain on the certificate without a restart (design §13.5).
//!
//! An application is served on a subdomain of the base domain, and a subdomain
//! the certificate does not cover is a name a browser refuses before it reads a
//! byte of the app. The ACME client takes its domain list at construction, so
//! this used to be settled at boot and an application created afterwards was
//! served on a name nothing had certified — which made restarting the server
//! look like part of creating an application.
//!
//! Two claims, one per half of the seam:
//!
//! - [`AppMounts`] tells the certificate what is served on every mount and
//!   unmount, and installing one orders nothing (the boot path's first order
//!   already covers what it mounted).
//! - [`AcmeCertificate`] turns that into a name set that **grows** — the union of
//!   what it covers and what is served — and never shrinks while the process
//!   runs, because each order is charged against the CA's rate limits and a name
//!   that has stopped being served resolves to nothing either way.
//!
//! Neither reaches a CA: the certificate here is pointed at a directory URL
//! nothing answers on, because what is under test is the name set, not ACME.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use sc_app::{Application, AssetBundle, CodeFramework, FrameworkRef};
use sc_catalog::Catalog;
use sc_db::DatabaseDriver;
use sc_db_postgres::PgDriver;
use sc_server::{AcmeCertificate, AppMounts, Certificate, MountedApp, TlsNames};
use sc_test_harness::TestDb;

/// The domain applications are subdomains of in these tests.
const BASE_DOMAIN: &str = "example.com";

/// A certificate that records what it was told, in place of an ACME client.
#[derive(Default)]
struct Recorder {
    told: Mutex<Vec<Vec<String>>>,
}

impl Recorder {
    fn told(&self) -> Vec<Vec<String>> {
        self.told.lock().unwrap().clone()
    }
}

impl Certificate for Recorder {
    fn subdomains_changed(&self, subdomains: &[String]) {
        self.told.lock().unwrap().push(subdomains.to_vec());
    }
}

/// An application of the `code` framework serving nothing, which is all a mount
/// needs to be a mount.
fn mounted(catalog: &Arc<Catalog>, subdomain: &str) -> sc_error::Result<MountedApp> {
    let app = Application::new(subdomain, subdomain, FrameworkRef::new("code"));
    let framework = Arc::new(CodeFramework::new("code", AssetBundle::new()));
    MountedApp::new(app, framework, catalog)
}

async fn catalog() -> sc_error::Result<(TestDb, Arc<Catalog>)> {
    let db = TestDb::new().await?;
    let driver = Arc::new(PgDriver::from_pool(db.pool().clone()));
    let catalog = Arc::new(Catalog::init(driver as Arc<dyn DatabaseDriver>).await?);
    db.on_teardown(catalog.teardown_hook());
    Ok((db, catalog))
}

#[tokio::test]
async fn every_mount_tells_the_certificate_what_is_served() -> sc_error::Result<()> {
    let (_db, catalog) = catalog().await?;
    let apps = AppMounts::new(catalog.clone()).with_base_domain(Some(BASE_DOMAIN.to_owned()));
    let certificate = Arc::new(Recorder::default());
    apps.set_certificate(certificate.clone());

    // Installing orders nothing: the boot path mounts before it builds the
    // serving plan, so the first order already covers what is up.
    assert!(
        certificate.told().is_empty(),
        "installing ordered something"
    );

    // A created application: the certificate hears its subdomain.
    apps.mount(mounted(&catalog, "blog")?)?;
    assert_eq!(certificate.told().last(), Some(&vec!["blog".to_owned()]));

    // A second one: what is served is the whole set, not the delta — the
    // certificate covers names, not changes.
    apps.mount(mounted(&catalog, "shop")?)?;
    assert_eq!(
        certificate.told().last(),
        Some(&vec!["blog".to_owned(), "shop".to_owned()])
    );

    // A rebuild re-mounts the same subdomain. It is still reported; deciding
    // that nothing is missing is the certificate's job, not the registry's.
    let before = certificate.told().len();
    apps.remount(mounted(&catalog, "shop")?);
    assert_eq!(certificate.told().len(), before + 1);
    assert_eq!(
        certificate.told().last(),
        Some(&vec!["blog".to_owned(), "shop".to_owned()])
    );

    // A deleted application is reported too, so a rename cannot leave the
    // certificate behind whichever half of it runs last.
    assert!(apps.unmount("blog"));
    assert_eq!(certificate.told().last(), Some(&vec!["shop".to_owned()]));
    // A subdomain that was never mounted changes nothing.
    let before = certificate.told().len();
    assert!(!apps.unmount("blog"));
    assert_eq!(certificate.told().len(), before);
    Ok(())
}

#[tokio::test]
async fn the_acme_name_set_grows_with_the_mounts_and_never_shrinks() -> sc_error::Result<()> {
    let (_db, catalog) = catalog().await?;
    let certificate = AcmeCertificate::new(
        TlsNames::new(
            Some(BASE_DOMAIN.to_owned()),
            vec!["blog".to_owned()],
            vec!["vanity.example.net".to_owned()],
        ),
        "admin@example.com".to_owned(),
        // Nothing answers here, which is the point: this is about the names.
        "http://127.0.0.1:9/directory".to_owned(),
        sc_config::AcmeCache::new(catalog.clone()),
    );
    // Nothing is covered until the serving path starts the first order.
    assert!(certificate.domains().is_empty());

    certificate.start();
    // The base domain first — it is what the certificate is *about* — then the
    // rest sorted, so two ways of arriving at one set produce one cache key.
    assert_eq!(
        certificate.domains(),
        ["example.com", "blog.example.com", "vanity.example.net"]
    );

    // A created application: a new order for the union, with no restart.
    certificate.subdomains_changed(&["blog".to_owned(), "shop".to_owned()]);
    assert_eq!(
        certificate.domains(),
        [
            "example.com",
            "blog.example.com",
            "shop.example.com",
            "vanity.example.net"
        ]
    );

    // The same set again is the certificate already in force: no order, so a
    // re-mount cannot spend a rate limit on a certificate the cache holds.
    let covered = certificate.domains();
    certificate.subdomains_changed(&["blog".to_owned(), "shop".to_owned()]);
    assert_eq!(certificate.domains(), covered);

    // A deleted application does not shrink the certificate.
    certificate.subdomains_changed(&["shop".to_owned()]);
    assert_eq!(certificate.domains(), covered);
    Ok(())
}
