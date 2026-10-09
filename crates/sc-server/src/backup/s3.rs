//! The three S3 requests an automated backup makes — put an object, list the
//! backups, delete one — signed with AWS Signature Version 4.
//!
//! Signed by hand rather than through an AWS SDK: the SDK brings an HTTP stack
//! and a runtime of its own for what is three requests and one HMAC chain, and
//! the signing is a pure function ([`authorization`]) checked against AWS's own
//! published examples in the tests below (which check the HMAC with it).
//!
//! **Any S3-compatible service**, not just AWS: the endpoint is the admin's
//! (MinIO, Cloudflare R2, Backblaze B2, Wasabi, DigitalOcean Spaces, …), and
//! requests use **path-style** addresses (`{endpoint}/{bucket}/{key}`), the one
//! form every one of them accepts. Virtual-hosted addresses
//! (`{bucket}.{endpoint}`) need a wildcard DNS name that a self-hosted MinIO
//! usually does not have.

use std::time::Duration;

use chrono::{DateTime, Utc};
use sc_error::{Error, Result};
use sha2::{Digest, Sha256};

/// How long one request may take, the upload included. Generous: a backup can
/// be large and a link slow, and this runs in the background.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// The region a bucket is signed for when the admin leaves it empty — what
/// AWS calls its default, and what MinIO answers to out of the box.
pub const DEFAULT_REGION: &str = "us-east-1";

/// The SHA-256 of nothing, which is what a request without a body signs.
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// A bucket, and the credentials to reach it.
pub struct S3Client {
    http: reqwest::Client,
    /// `scheme://host[:port]`, from the endpoint.
    origin: String,
    /// The `Host` header: the endpoint's host, with its port when it is not
    /// the scheme's default (which is what `reqwest` sends, and so what has
    /// to be signed).
    host: String,
    /// The endpoint's path, if it has one, without a trailing `/` — a service
    /// mounted under a path prefix.
    base_path: String,
    bucket: String,
    region: String,
    access_key: String,
    secret_key: String,
}

impl S3Client {
    pub fn new(
        endpoint: &str,
        bucket: &str,
        region: &str,
        access_key: &str,
        secret_key: &str,
    ) -> Result<S3Client> {
        let url = reqwest::Url::parse(endpoint)
            .map_err(|e| Error::invalid(format!("`{endpoint}` is not a URL: {e}")))?;
        let host = url
            .host_str()
            .ok_or_else(|| Error::invalid(format!("`{endpoint}` has no host name")))?;
        let host = match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_owned(),
        };
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| Error::msg(format!("building the S3 client: {e}")))?;
        Ok(S3Client {
            http,
            origin: format!("{}://{host}", url.scheme()),
            host,
            base_path: url.path().trim_end_matches('/').to_owned(),
            bucket: bucket.to_owned(),
            region: region.to_owned(),
            access_key: access_key.to_owned(),
            secret_key: secret_key.to_owned(),
        })
    }

    /// Store `body` as `key`. An S3 put is atomic — the object appears whole
    /// or not at all — so there is no temporary name to rename from.
    pub async fn put(&self, key: &str, body: Vec<u8>) -> Result<()> {
        let hash = hex(&Sha256::digest(&body));
        self.send("PUT", Some(key), &[], body, &hash).await?;
        Ok(())
    }

    /// Delete `key`. Deleting a key that is not there is not an error in S3.
    pub async fn delete(&self, key: &str) -> Result<()> {
        self.send("DELETE", Some(key), &[], Vec::new(), EMPTY_SHA256)
            .await?;
        Ok(())
    }

    /// Every key starting with `prefix`, across as many pages as the listing
    /// takes (ListObjectsV2 returns at most a thousand at a time).
    pub async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let mut keys = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut query = vec![
                ("list-type".to_owned(), "2".to_owned()),
                ("prefix".to_owned(), prefix.to_owned()),
            ];
            if let Some(token) = &token {
                query.push(("continuation-token".to_owned(), token.clone()));
            }
            let body = self
                .send("GET", None, &query, Vec::new(), EMPTY_SHA256)
                .await?;
            let page = ListPage::parse(&body);
            keys.extend(page.keys);
            match page.next {
                Some(next) if page.truncated => token = Some(next),
                _ => break,
            }
        }
        Ok(keys)
    }

    /// Where `key` is, as a URL — what the status line says was written.
    pub fn url_of(&self, key: &str) -> String {
        format!("{}{}", self.origin, self.path(Some(key)))
    }

    /// The request path, URI-encoded as S3 signs it: the bucket, then the key.
    fn path(&self, key: Option<&str>) -> String {
        let mut path = format!("{}/{}", self.base_path, uri_encode(&self.bucket, false));
        if let Some(key) = key {
            path.push('/');
            path.push_str(&uri_encode(key, false));
        }
        path
    }

    async fn send(
        &self,
        method: &str,
        key: Option<&str>,
        query: &[(String, String)],
        body: Vec<u8>,
        payload_hash: &str,
    ) -> Result<String> {
        let path = self.path(key);
        let now = Utc::now();
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let headers = vec![
            ("host".to_owned(), self.host.clone()),
            ("x-amz-content-sha256".to_owned(), payload_hash.to_owned()),
            ("x-amz-date".to_owned(), amz_date.clone()),
        ];
        let auth = authorization(&Signing {
            method,
            path: &path,
            query,
            headers: &headers,
            payload_hash,
            access_key: &self.access_key,
            secret_key: &self.secret_key,
            region: &self.region,
            at: now,
        });
        let query_string = canonical_query(query);
        let url = if query_string.is_empty() {
            format!("{}{path}", self.origin)
        } else {
            format!("{}{path}?{query_string}", self.origin)
        };
        let method_value = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|e| Error::msg(format!("S3 method `{method}`: {e}")))?;
        let what = match key {
            Some(key) => format!("{method} {}", self.url_of(key)),
            None => format!("{method} {}{}", self.origin, self.path(None)),
        };
        let response = self
            .http
            .request(method_value, &url)
            .header("x-amz-content-sha256", payload_hash)
            .header("x-amz-date", amz_date)
            .header("authorization", auth)
            .body(body)
            .send()
            .await
            .map_err(|e| Error::file(format!("{what}: {}", sc_error::format_chain(&e))))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| Error::file(format!("{what}: reading the response: {e}")))?;
        if status.is_success() {
            return Ok(text);
        }
        Err(Error::file(format!(
            "{what}: the server answered {status}{}",
            error_detail(&text)
        )))
    }
}

/// Everything one signature covers.
pub struct Signing<'a> {
    pub method: &'a str,
    /// Already URI-encoded, as it is sent.
    pub path: &'a str,
    /// Raw names and values; encoded and sorted here.
    pub query: &'a [(String, String)],
    /// Every header that is signed, with lower-case names. `host`,
    /// `x-amz-date` and `x-amz-content-sha256` must be among them.
    pub headers: &'a [(String, String)],
    pub payload_hash: &'a str,
    pub access_key: &'a str,
    pub secret_key: &'a str,
    pub region: &'a str,
    pub at: DateTime<Utc>,
}

/// The `Authorization` header for a request, by Signature Version 4.
pub fn authorization(s: &Signing<'_>) -> String {
    let amz_date = s.at.format("%Y%m%dT%H%M%SZ").to_string();
    let date = s.at.format("%Y%m%d").to_string();

    let mut headers: Vec<(String, String)> = s
        .headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    headers.sort();
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_headers = headers
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let canonical_request = format!(
        "{}\n{}\n{}\n{canonical_headers}\n{signed_headers}\n{}",
        s.method,
        s.path,
        canonical_query(s.query),
        s.payload_hash
    );
    let scope = format!("{date}/{}/s3/aws4_request", s.region);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex(&Sha256::digest(canonical_request.as_bytes()))
    );

    let key = hmac(format!("AWS4{}", s.secret_key).as_bytes(), date.as_bytes());
    let key = hmac(&key, s.region.as_bytes());
    let key = hmac(&key, b"s3");
    let key = hmac(&key, b"aws4_request");
    let signature = hex(&hmac(&key, string_to_sign.as_bytes()));

    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope},SignedHeaders={signed_headers},Signature={signature}",
        s.access_key
    )
}

/// The query string as signed and as sent: each name and value encoded, then
/// sorted by name.
fn canonical_query(query: &[(String, String)]) -> String {
    let mut pairs: Vec<(String, String)> = query
        .iter()
        .map(|(k, v)| (uri_encode(k, true), uri_encode(v, true)))
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// S3's URI encoding: everything but the unreserved characters is
/// percent-encoded, upper case, and `/` too unless it separates a path.
fn uri_encode(raw: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b'/' if !encode_slash => out.push('/'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// HMAC-SHA256 (RFC 2104), over `sha2` directly: the construction is two
/// hashes, and the `hmac` crate's constructor returns a `Result` for key
/// lengths that, for HMAC, cannot fail.
fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    const BLOCK: usize = 64;
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let pad = |byte: u8| block.iter().map(|b| b ^ byte).collect::<Vec<u8>>();
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(data)
        .finalize();
    Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .to_vec()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// One page of a ListObjectsV2 answer: the keys, and where the next page
/// starts. Read with string searches — the answer is a flat, fixed document,
/// and the three elements wanted never nest in one another.
#[derive(Debug, Default, PartialEq)]
struct ListPage {
    keys: Vec<String>,
    truncated: bool,
    next: Option<String>,
}

impl ListPage {
    fn parse(xml: &str) -> ListPage {
        let mut page = ListPage::default();
        let mut rest = xml;
        while let Some(start) = rest.find("<Contents>") {
            let after = &rest[start..];
            let end = after.find("</Contents>").unwrap_or(after.len());
            if let Some(key) = element(&after[..end], "Key") {
                page.keys.push(key);
            }
            rest = &after[end..];
        }
        page.truncated = element(xml, "IsTruncated").as_deref() == Some("true");
        page.next = element(xml, "NextContinuationToken");
        page
    }
}

/// The text of the first `<name>…</name>` in `xml`, entities decoded.
fn element(xml: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = xml.find(&open)? + open.len();
    let end = start + xml[start..].find(&close)?;
    Some(unescape(&xml[start..end]))
}

fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// What an S3 error document says, as `: Code: Message`, or nothing when
/// the body is not one.
fn error_detail(body: &str) -> String {
    match (element(body, "Code"), element(body, "Message")) {
        (Some(code), Some(message)) => format!(": {code}: {message}"),
        (Some(code), None) => format!(": {code}"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const ACCESS_KEY: &str = "AKIAIOSFODNN7EXAMPLE";
    const SECRET_KEY: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

    fn example_headers(extra: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut headers = vec![
            (
                "host".to_owned(),
                "examplebucket.s3.amazonaws.com".to_owned(),
            ),
            ("x-amz-content-sha256".to_owned(), EMPTY_SHA256.to_owned()),
            ("x-amz-date".to_owned(), "20130524T000000Z".to_owned()),
        ];
        headers.extend(
            extra
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned())),
        );
        headers
    }

    /// AWS's own worked example, "GET Object", from the Signature Version 4
    /// documentation for S3 (Authenticating Requests: Using the Authorization
    /// Header).
    #[test]
    fn signs_the_aws_get_object_example() {
        let headers = example_headers(&[("range", "bytes=0-9")]);
        let auth = authorization(&Signing {
            method: "GET",
            path: "/test.txt",
            query: &[],
            headers: &headers,
            payload_hash: EMPTY_SHA256,
            access_key: ACCESS_KEY,
            secret_key: SECRET_KEY,
            region: "us-east-1",
            at: Utc.with_ymd_and_hms(2013, 5, 24, 0, 0, 0).unwrap(),
        });
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request,\
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date,\
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    /// The same page's "GET Bucket (List Objects)" example: a query string,
    /// sorted and encoded.
    #[test]
    fn signs_the_aws_list_objects_example() {
        let headers = example_headers(&[]);
        let auth = authorization(&Signing {
            method: "GET",
            path: "/",
            query: &[
                ("prefix".to_owned(), "J".to_owned()),
                ("max-keys".to_owned(), "2".to_owned()),
            ],
            headers: &headers,
            payload_hash: EMPTY_SHA256,
            access_key: ACCESS_KEY,
            secret_key: SECRET_KEY,
            region: "us-east-1",
            at: Utc.with_ymd_and_hms(2013, 5, 24, 0, 0, 0).unwrap(),
        });
        assert!(
            auth.ends_with(
                "Signature=34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
            ),
            "{auth}"
        );
    }

    /// RFC 4231's test cases 2 (a short key) and 6 (a key longer than the
    /// block, which is hashed first).
    #[test]
    fn hmac_matches_rfc_4231() {
        assert_eq!(
            hex(&hmac(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(
            hex(&hmac(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn encodes_as_s3_does() {
        assert_eq!(uri_encode("a b/c~d", false), "a%20b/c~d");
        assert_eq!(uri_encode("a b/c+d=", true), "a%20b%2Fc%2Bd%3D");
        assert_eq!(
            canonical_query(&[
                ("prefix".to_owned(), "feldspar-backup-".to_owned()),
                ("list-type".to_owned(), "2".to_owned()),
            ]),
            "list-type=2&prefix=feldspar-backup-"
        );
    }

    #[test]
    fn the_endpoint_gives_host_and_path() {
        let c = S3Client::new("http://localhost:9000/", "backups", "us-east-1", "a", "s").unwrap();
        assert_eq!(c.host, "localhost:9000");
        assert_eq!(
            c.url_of("feldspar-backup-x.zip"),
            "http://localhost:9000/backups/feldspar-backup-x.zip"
        );
        let c = S3Client::new(
            "https://s3.eu-west-1.amazonaws.com",
            "b",
            "eu-west-1",
            "a",
            "s",
        )
        .unwrap();
        assert_eq!(c.host, "s3.eu-west-1.amazonaws.com");
        let c = S3Client::new("https://example.com/storage/", "b", "r", "a", "s").unwrap();
        assert_eq!(c.url_of("k"), "https://example.com/storage/b/k");
        assert!(S3Client::new("not a url", "b", "r", "a", "s").is_err());
    }

    /// The three requests against a real S3-compatible service, when one is
    /// named — the check that the signing agrees with somebody else's
    /// implementation and not only with AWS's examples:
    ///
    /// ```text
    /// FELDSPAR_TEST_S3="http://localhost:7070 bucket us-east-1 ACCESS SECRET" \
    ///   cargo test -p sc-server --lib -- --ignored a_real_service
    /// ```
    #[tokio::test]
    #[ignore = "needs an S3-compatible service; see FELDSPAR_TEST_S3"]
    async fn a_real_service_accepts_put_list_and_delete() {
        let spec = std::env::var("FELDSPAR_TEST_S3").expect("FELDSPAR_TEST_S3");
        let [endpoint, bucket, region, access, secret] =
            <[&str; 5]>::try_from(spec.split_whitespace().collect::<Vec<_>>()).unwrap();
        let client = S3Client::new(endpoint, bucket, region, access, secret).unwrap();
        let prefix = format!("feldspar-backup-test-{}-", uuid::Uuid::new_v4());
        let keys: Vec<String> = (0..3).map(|i| format!("{prefix}{i}.zip")).collect();
        for key in &keys {
            client.put(key, key.as_bytes().to_vec()).await.unwrap();
        }
        let mut listed = client.list(&prefix).await.unwrap();
        listed.sort();
        assert_eq!(listed, keys);
        for key in &keys {
            client.delete(key).await.unwrap();
        }
        assert!(client.list(&prefix).await.unwrap().is_empty());

        let wrong = S3Client::new(endpoint, bucket, region, access, "wrong").unwrap();
        let err = wrong.list(&prefix).await.unwrap_err().to_string();
        assert!(err.contains("403"), "{err}");
    }

    #[test]
    fn reads_a_list_page_and_an_error() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>b</Name><Prefix>feldspar-backup-</Prefix><KeyCount>2</KeyCount>
  <IsTruncated>true</IsTruncated>
  <NextContinuationToken>1ueGcxLPRx1Tr/XYExHnhbYLgveDs2J/wm36Hy4vbOwM=</NextContinuationToken>
  <Contents><Key>feldspar-backup-2026-01-01-020000.zip</Key><Size>10</Size></Contents>
  <Contents><Key>feldspar-backup-a&amp;b.zip</Key><Size>10</Size></Contents>
</ListBucketResult>"#;
        assert_eq!(
            ListPage::parse(xml),
            ListPage {
                keys: vec![
                    "feldspar-backup-2026-01-01-020000.zip".into(),
                    "feldspar-backup-a&b.zip".into()
                ],
                truncated: true,
                next: Some("1ueGcxLPRx1Tr/XYExHnhbYLgveDs2J/wm36Hy4vbOwM=".into()),
            }
        );
        assert_eq!(
            error_detail(
                "<Error><Code>AccessDenied</Code><Message>Access Denied</Message></Error>"
            ),
            ": AccessDenied: Access Denied"
        );
        assert_eq!(error_detail(""), "");
    }
}
