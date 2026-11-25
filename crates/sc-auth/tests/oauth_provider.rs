//! The identity provider's storage (TODO IP1.1) and signing keys (IP1.2)
//! against real Postgres.
//!
//! - A client is registered on one application, its secret is shown once and
//!   stored hashed, and a lookup from another application finds nothing.
//! - A code is single-use, bound to its client, redirect URI and PKCE
//!   challenge; a second use revokes what the first got.
//! - A refresh token rotates, and a reused one revokes its family.
//! - An `id_token` signed with either key verifies against the published JWKS;
//!   a rotation keeps the old key published; a key sealed by another instance
//!   is never served.

use std::sync::Arc;

use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{DecodingKey, Validation, decode, decode_header};
use sc_auth::{
    GrantRefused, NewAuthorizationCode, NewOAuthClient, OAUTH_CLIENT_SECRET_PREFIX,
    OAUTH_CLIENTS_TABLE, OAUTH_CODES_TABLE, OAuthClient, SigningAlg, TokenSubject,
    authenticate_oauth_access_token, authenticate_oauth_client, bootstrap, delete_oauth_client,
    ensure_signing_keys, issue_authorization_code, issue_oauth_tokens, list_oauth_clients,
    load_oauth_client, pkce_s256, published_jwks, record_oauth_grant, redeem_authorization_code,
    redeem_refresh_token, regenerate_oauth_client_secret, register_oauth_client,
    rotate_signing_keys, sign_jwt,
};
use sc_catalog::{Catalog, SecretKey};
use sc_error::Result;
use sc_query::{Expr, Select, Source, Statement, Value};
use sc_test_harness::{TestDb, TestServer};
use serde_json::json;
use uuid::Uuid;

const REDIRECT: &str = "https://grafana.example.com/login/generic_oauth";
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";

struct Fixture {
    catalog: Arc<Catalog>,
    _db: TestDb,
}

async fn fixture() -> Result<Fixture> {
    let TestServer { db, catalog, .. } = TestServer::builder().build().await?;
    catalog.set_secret_key(SecretKey::generate()?);
    bootstrap(&catalog).await?;
    Ok(Fixture { catalog, _db: db })
}

async fn client(
    catalog: &Catalog,
    app: Uuid,
    confidential: bool,
) -> Result<(OAuthClient, Option<String>)> {
    let registered = register_oauth_client(
        catalog,
        NewOAuthClient {
            app_id: app,
            name: "Grafana".into(),
            confidential,
            redirect_uris: vec![REDIRECT.into()],
            scopes: vec![],
            skip_consent: false,
            created_by: None,
            claim_fields: vec![],
            post_logout_redirect_uris: vec![],
        },
    )
    .await?;
    Ok((registered.client, registered.secret))
}

fn code_for(client: &OAuthClient, user: Uuid) -> NewAuthorizationCode {
    NewAuthorizationCode {
        client_id: client.client_id.clone(),
        app_id: client.app_id,
        user_id: user,
        redirect_uri: REDIRECT.into(),
        code_challenge: pkce_s256(VERIFIER),
        nonce: Some("n-0S6_WzA2Mj".into()),
        scopes: vec!["openid".into(), "email".into()],
        auth_time: None,
        issuer: "https://blog.example.com".into(),
    }
}

/// Every column of every row of `table`, as text, for "the secret is not in
/// the table".
async fn dump(catalog: &Catalog, table: &str) -> Result<String> {
    let rows: Vec<sc_db::Row> = catalog
        .primary(sc_catalog::Unchecked::system("test: read the table"))
        .query(&Statement::from(Select::from(Source::table(table))))
        .await?
        .try_collect()
        .await?;
    Ok(format!("{rows:?}"))
}

#[tokio::test]
async fn a_client_belongs_to_one_application_and_its_secret_is_shown_once() -> Result<()> {
    let f = fixture().await?;
    let (app, other_app) = (Uuid::new_v4(), Uuid::new_v4());
    let (client, secret) = client(&f.catalog, app, true).await?;
    let secret = secret.expect("a confidential client gets a secret");
    assert!(secret.starts_with(OAUTH_CLIENT_SECRET_PREFIX));
    assert!(
        !dump(&f.catalog, OAUTH_CLIENTS_TABLE)
            .await?
            .contains(&secret)
    );

    // Found on its application, and nowhere else.
    assert_eq!(
        load_oauth_client(&f.catalog, app, &client.client_id).await?,
        Some(client.clone())
    );
    assert_eq!(
        load_oauth_client(&f.catalog, other_app, &client.client_id).await?,
        None
    );
    assert_eq!(list_oauth_clients(&f.catalog, app).await?.len(), 1);
    assert!(list_oauth_clients(&f.catalog, other_app).await?.is_empty());

    // The secret, and only the secret, authenticates it, and only here.
    let auth = |app, secret: Option<&str>| {
        let catalog = f.catalog.clone();
        let id = client.client_id.clone();
        let secret = secret.map(str::to_owned);
        async move { authenticate_oauth_client(&catalog, app, &id, secret.as_deref()).await }
    };
    assert!(auth(app, Some(&secret)).await?.is_ok());
    assert!(auth(app, Some("fdcs_wrong")).await?.is_err());
    assert!(auth(app, None).await?.is_err());
    assert!(auth(other_app, Some(&secret)).await?.is_err());

    // A new secret replaces the old one at once.
    let fresh = regenerate_oauth_client_secret(&f.catalog, app, &client.client_id)
        .await?
        .unwrap();
    assert!(auth(app, Some(&secret)).await?.is_err());
    assert!(auth(app, Some(&fresh)).await?.is_ok());

    // A public client has no secret and must not send one.
    let (public, none) = self::client(&f.catalog, app, false).await?;
    assert!(none.is_none());
    let public_auth = |secret: Option<&'static str>| {
        let catalog = f.catalog.clone();
        let id = public.client_id.clone();
        async move { authenticate_oauth_client(&catalog, app, &id, secret).await }
    };
    assert!(public_auth(None).await?.is_ok());
    assert!(public_auth(Some("anything")).await?.is_err());

    // Registration refuses what would make the provider an open redirect.
    for bad in [
        "http://grafana.example.com/cb",
        "/relative",
        "https://x.example/cb#f",
    ] {
        let refused = register_oauth_client(
            &f.catalog,
            NewOAuthClient {
                app_id: app,
                name: "x".into(),
                confidential: true,
                redirect_uris: vec![bad.into()],
                scopes: vec![],
                skip_consent: false,
                created_by: None,
                claim_fields: vec![],
                post_logout_redirect_uris: vec![],
            },
        )
        .await;
        assert!(refused.is_err(), "{bad}");
    }

    // Deleting it from another application does nothing; from its own, it goes.
    assert!(!delete_oauth_client(&f.catalog, other_app, &client.client_id).await?);
    assert!(delete_oauth_client(&f.catalog, app, &client.client_id).await?);
    assert_eq!(
        load_oauth_client(&f.catalog, app, &client.client_id).await?,
        None
    );
    Ok(())
}

#[tokio::test]
async fn a_code_is_single_use_and_bound_to_its_client_uri_and_challenge() -> Result<()> {
    let f = fixture().await?;
    let app = Uuid::new_v4();
    let user = Uuid::new_v4();
    let (client, _) = client(&f.catalog, app, true).await?;
    let (other, _) = self::client(&f.catalog, app, true).await?;

    let code = issue_authorization_code(&f.catalog, code_for(&client, user)).await?;
    assert!(!dump(&f.catalog, OAUTH_CODES_TABLE).await?.contains(&code));

    // Another client cannot spend it, and trying leaves it for its own.
    let theirs = redeem_authorization_code(&f.catalog, &other, &code, REDIRECT, VERIFIER).await?;
    assert!(
        matches!(theirs, Err(GrantRefused { ref reason, .. }) if reason.contains("another client"))
    );

    let redeemed = redeem_authorization_code(&f.catalog, &client, &code, REDIRECT, VERIFIER)
        .await?
        .expect("the code is good once");
    assert_eq!(redeemed.user_id, user);
    assert_eq!(redeemed.nonce.as_deref(), Some("n-0S6_WzA2Mj"));
    assert_eq!(redeemed.scopes, ["openid", "email"]);

    // Tokens from it, then the code again: refused, and the tokens revoked.
    let grant = record_oauth_grant(&f.catalog, user, &client, &redeemed.scopes).await?;
    let tokens = issue_oauth_tokens(
        &f.catalog,
        &TokenSubject {
            client_id: client.client_id.clone(),
            app_id: app,
            user_id: user,
            scopes: redeemed.scopes.clone(),
            family: redeemed.family,
            grant_id: grant.id,
            auth_time: None,
            issuer: redeemed.issuer.clone(),
            auth_epoch: redeemed.auth_epoch,
        },
    )
    .await?;
    assert!(
        authenticate_oauth_access_token(&f.catalog, app, &tokens.access_token)
            .await?
            .is_some()
    );
    let again = redeem_authorization_code(&f.catalog, &client, &code, REDIRECT, VERIFIER).await?;
    assert!(
        matches!(again, Err(GrantRefused { ref reason, .. }) if reason.contains("already used"))
    );
    assert!(
        authenticate_oauth_access_token(&f.catalog, app, &tokens.access_token)
            .await?
            .is_none(),
        "a reused code revokes what the first use was given"
    );

    // A wrong verifier and a wrong redirect URI each spend the code for nothing.
    let code = issue_authorization_code(&f.catalog, code_for(&client, user)).await?;
    let wrong = redeem_authorization_code(
        &f.catalog,
        &client,
        &code,
        REDIRECT,
        "wrong-verifier-wrong-verifier-wrong-verifier-x",
    )
    .await?;
    assert!(
        matches!(wrong, Err(GrantRefused { ref reason, .. }) if reason.contains("code_challenge"))
    );
    let code = issue_authorization_code(&f.catalog, code_for(&client, user)).await?;
    let elsewhere = redeem_authorization_code(
        &f.catalog,
        &client,
        &code,
        "https://grafana.example.com/other",
        VERIFIER,
    )
    .await?;
    assert!(
        matches!(elsewhere, Err(GrantRefused { ref reason, .. }) if reason.contains("redirect_uri"))
    );

    // An unknown code.
    let none =
        redeem_authorization_code(&f.catalog, &client, "fdac_nope", REDIRECT, VERIFIER).await?;
    assert!(
        matches!(none, Err(GrantRefused { ref reason, .. }) if reason.contains("not recognised"))
    );

    // A lapsed one.
    let code = issue_authorization_code(&f.catalog, code_for(&client, user)).await?;
    f.catalog
        .primary(sc_catalog::Unchecked::system("test: age a code"))
        .query(&Statement::from(
            sc_query::Update::new(
                OAUTH_CODES_TABLE,
                vec![sc_query::Assignment::new(
                    "expires_at".to_owned(),
                    Expr::Lit(Value::Timestamp(
                        chrono::Utc::now() - chrono::Duration::seconds(1),
                    )),
                )],
            )
            .filter(Expr::col("hash").eq(Expr::lit(sc_auth::bearer_secret_hash(&code)))),
        ))
        .await?
        .try_collect()
        .await?;
    let lapsed = redeem_authorization_code(&f.catalog, &client, &code, REDIRECT, VERIFIER).await?;
    assert!(matches!(lapsed, Err(GrantRefused { ref reason, .. }) if reason.contains("expired")));
    Ok(())
}

#[tokio::test]
async fn a_refresh_token_rotates_and_a_reused_one_revokes_its_family() -> Result<()> {
    let f = fixture().await?;
    let app = Uuid::new_v4();
    let user = Uuid::new_v4();
    let (client, _) = client(&f.catalog, app, true).await?;
    let (other, _) = self::client(&f.catalog, app, true).await?;
    let grant = record_oauth_grant(&f.catalog, user, &client, &["openid".into()]).await?;
    let subject = TokenSubject {
        client_id: client.client_id.clone(),
        app_id: app,
        user_id: user,
        scopes: vec!["openid".into(), "email".into()],
        family: Uuid::new_v4(),
        grant_id: grant.id,
        auth_time: None,
        issuer: "https://blog.example.com".into(),
        auth_epoch: sc_auth::AuthEpoch::INITIAL,
    };
    let first = issue_oauth_tokens(&f.catalog, &subject).await?;

    // Another client's refresh is refused, and the token is not spent.
    let theirs = redeem_refresh_token(&f.catalog, &other, &first.refresh_token, None).await?;
    assert!(theirs.is_err());

    // Narrowing is allowed, widening is not.
    let next = redeem_refresh_token(
        &f.catalog,
        &client,
        &first.refresh_token,
        Some(&["openid".into()]),
    )
    .await?
    .expect("a fresh refresh token is good once");
    assert_eq!(next.scopes, ["openid"]);
    let second = issue_oauth_tokens(&f.catalog, &next).await?;

    // The first, again: refused, and everything from the sign-in revoked.
    let reused = redeem_refresh_token(&f.catalog, &client, &first.refresh_token, None).await?;
    assert!(
        matches!(reused, Err(GrantRefused { ref reason, .. }) if reason.contains("already used"))
    );
    for access in [&first.access_token, &second.access_token] {
        assert!(
            authenticate_oauth_access_token(&f.catalog, app, access)
                .await?
                .is_none()
        );
    }
    let revoked = redeem_refresh_token(&f.catalog, &client, &second.refresh_token, None).await?;
    assert!(matches!(revoked, Err(GrantRefused { ref reason, .. }) if reason.contains("revoked")));

    // An access token is never a refresh token, and is only good on its app.
    let third = issue_oauth_tokens(
        &f.catalog,
        &TokenSubject {
            family: Uuid::new_v4(),
            ..subject
        },
    )
    .await?;
    assert!(
        redeem_refresh_token(&f.catalog, &client, &third.access_token, None)
            .await?
            .is_err()
    );
    assert!(
        authenticate_oauth_access_token(&f.catalog, Uuid::new_v4(), &third.access_token)
            .await?
            .is_none()
    );
    assert!(
        authenticate_oauth_access_token(&f.catalog, app, &third.refresh_token)
            .await?
            .is_none()
    );
    Ok(())
}

/// Verify `token` against the published JWKS, as a relying party would.
async fn verify(catalog: &Catalog, token: &str) -> Result<serde_json::Value> {
    let jwks: JwkSet = serde_json::from_value(published_jwks(catalog).await?).unwrap();
    let header = decode_header(token).unwrap();
    let kid = header.kid.expect("a kid");
    let jwk = jwks.find(&kid).expect("the signing key is published");
    let mut validation = Validation::new(header.alg);
    validation.set_audience(&["the-client"]);
    validation.set_issuer(&["https://blog.example.com"]);
    let data =
        decode::<serde_json::Value>(token, &DecodingKey::from_jwk(jwk).unwrap(), &validation)
            .map_err(|e| sc_error::Error::invalid(format!("{e}")))?;
    Ok(data.claims)
}

#[tokio::test]
async fn an_id_token_verifies_against_the_jwks_and_rotation_keeps_the_old_key_published()
-> Result<()> {
    let f = fixture().await?;
    let made = ensure_signing_keys(&f.catalog).await?;
    assert_eq!(made.created.len(), 2, "one RS256 key and one ES256 key");
    // A second call makes nothing.
    assert!(ensure_signing_keys(&f.catalog).await?.created.is_empty());

    let now = chrono::Utc::now().timestamp();
    let claims = json!({
        "iss": "https://blog.example.com", "sub": "user", "aud": "the-client",
        "iat": now, "exp": now + 600,
    });
    let mut old_kids = Vec::new();
    for alg in SigningAlg::ALL {
        let token = sign_jwt(&f.catalog, alg, &claims).await?;
        let header = decode_header(&token).unwrap();
        assert_eq!(format!("{:?}", header.alg), alg.name());
        assert_eq!(verify(&f.catalog, &token).await?["sub"], "user");
        old_kids.push((token, header.kid.unwrap()));
    }
    // The JWKS never carries a private half.
    let jwks = published_jwks(&f.catalog).await?.to_string();
    for private in ["\"d\"", "\"p\"", "\"q\"", "PRIVATE"] {
        assert!(!jwks.contains(private), "{private} in {jwks}");
    }

    // Rotation: new keys sign, and a token from the old ones still verifies.
    let fresh = rotate_signing_keys(&f.catalog).await?;
    for (token, kid) in &old_kids {
        assert!(!fresh.contains(kid));
        assert_eq!(verify(&f.catalog, token).await?["sub"], "user");
    }
    for alg in SigningAlg::ALL {
        let token = sign_jwt(&f.catalog, alg, &claims).await?;
        let kid = decode_header(&token).unwrap().kid.unwrap();
        assert!(fresh.contains(&kid));
        verify(&f.catalog, &token).await?;
    }
    Ok(())
}

#[tokio::test]
async fn keys_sealed_by_another_instance_are_never_served_and_are_replaced() -> Result<()> {
    let f = fixture().await?;
    let first = ensure_signing_keys(&f.catalog).await?.created;
    // As after restoring a backup onto an instance with another key.
    f.catalog.set_secret_key(SecretKey::generate()?);
    assert_eq!(
        published_jwks(&f.catalog).await?["keys"],
        json!([]),
        "keys this instance cannot open are not published"
    );
    assert!(
        sign_jwt(&f.catalog, SigningAlg::Rs256, &json!({}))
            .await
            .is_err()
    );

    let check = ensure_signing_keys(&f.catalog).await?;
    let mut unreadable = check.unreadable.clone();
    unreadable.sort();
    let mut expected = first.clone();
    expected.sort();
    assert_eq!(unreadable, expected, "it says which keys it could not open");
    assert_eq!(check.created.len(), 2);
    let published = published_jwks(&f.catalog).await?;
    let kids: Vec<&str> = published["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k["kid"].as_str().unwrap())
        .collect();
    assert_eq!(kids.len(), 2);
    assert!(
        kids.iter()
            .all(|kid| check.created.iter().any(|c| c == kid))
    );
    Ok(())
}
