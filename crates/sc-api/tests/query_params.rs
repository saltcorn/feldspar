//! Query parameters in the endpoint model, and what the TypeScript generator
//! makes of them (design §13.1).
//!
//! A query parameter is part of the endpoint *value* so that the generated
//! client can express `?select=…` — the alternative being a hand-written
//! `fetch` beside a generated client, which is where drift starts. These tests
//! pin the three shapes a parameter can take (required, optional, repeated),
//! the signature each produces, and the encoding the client emits for it.

use sc_api::{Endpoint, EndpointSet, Method, PathSpec, QueryParam, TypeSchema, ValueType};

/// An endpoint set with one method of each interesting shape: a list endpoint
/// whose parameters are all optional (including a repeated one), and a search
/// endpoint with a required parameter.
fn sample_set() -> EndpointSet {
    EndpointSet::new()
        .with(
            Endpoint::new(
                sc_api::HandlerId::<sc_api::SignedIn>::new("listBooks"),
                Method::Get,
                PathSpec::root().lit("api/books"),
            )
            .query([
                QueryParam::new("select", ValueType::Text),
                QueryParam::new("limit", ValueType::Int),
                QueryParam::new("published", ValueType::Text).repeated(),
            ])
            .output(TypeSchema::array(TypeSchema::json())),
        )
        .with(
            Endpoint::new(
                sc_api::HandlerId::<sc_api::SignedIn>::new("searchBooks"),
                Method::Get,
                PathSpec::root().lit("api/search"),
            )
            .query([
                QueryParam::new("q", ValueType::Text).required(),
                QueryParam::new("limit", ValueType::Int),
            ])
            .output(TypeSchema::array(TypeSchema::json())),
        )
}

#[test]
fn an_endpoint_with_query_parameters_round_trips_through_serde() {
    let ep = Endpoint::new(
        sc_api::HandlerId::<sc_api::SignedIn>::new("listBooks"),
        Method::Get,
        PathSpec::root().lit("api/books"),
    )
    .query([
        QueryParam::new("select", ValueType::Text),
        QueryParam::new("published", ValueType::Date).repeated(),
        QueryParam::new("q", ValueType::Text).required(),
    ]);
    let json = serde_json::to_string(&ep).expect("serialize");
    let back: Endpoint = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(
        back, ep,
        "a projected endpoint set must survive a round trip"
    );
    assert_eq!(back.query.len(), 3);
    assert!(back.query[1].repeated && !back.query[1].required);
    assert!(back.query[2].required && !back.query[2].repeated);
}

#[test]
fn an_endpoint_without_query_parameters_is_unchanged_by_the_field() {
    // The field is skipped when empty, so every endpoint that existed before
    // query parameters serializes exactly as it did — and still deserializes
    // from a document written without the key.
    let ep = Endpoint::new(
        sc_api::HandlerId::<sc_api::SignedIn>::new("whoami"),
        Method::Get,
        PathSpec::root().lit("api/whoami"),
    );
    let json = serde_json::to_string(&ep).expect("serialize");
    assert!(!json.contains("\"query\""), "{json}");
    let back: Endpoint = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, ep);
    assert!(back.query.is_empty());
}

#[test]
fn the_generated_signature_types_each_parameter_shape() {
    let ts = sc_api::generate_client(&sample_set());

    // All-optional parameters: an optional options object, so a caller who wants
    // none of them writes `api.listBooks()` exactly as before.
    assert!(
        ts.contains("export type ListBooksQuery = { select?: string; limit?: number; published?: Array<string> };"),
        "{ts}"
    );
    assert!(
        ts.contains("listBooks(query?: ListBooksQuery): Promise<ListBooksResponse>;"),
        "{ts}"
    );

    // One required parameter makes the object itself required.
    assert!(
        ts.contains("export type SearchBooksQuery = { q: string; limit?: number };"),
        "{ts}"
    );
    assert!(
        ts.contains("searchBooks(query: SearchBooksQuery): Promise<SearchBooksResponse>;"),
        "{ts}"
    );
}

#[test]
fn the_generated_client_encodes_each_parameter_shape() {
    let ts = sc_api::generate_client(&sample_set());

    // An optional value is sent only when given, and `null` means "not given"
    // rather than the string "null".
    assert!(
        ts.contains(
            "if (query?.select !== undefined && query?.select !== null) \
             search.append(\"select\", String(query?.select));"
        ),
        "{ts}"
    );
    // A repeated parameter appends once per element: this is what makes
    // `?published=gte.2020&published=lt.2024` expressible at all.
    assert!(
        ts.contains(
            "for (const value of query?.published ?? []) search.append(\"published\", String(value));"
        ),
        "{ts}"
    );
    // A required parameter is always appended, with no guard to forget.
    assert!(
        ts.contains("search.append(\"q\", String(query.q));"),
        "{ts}"
    );
    // The query string is appended to the URL only when it is non-empty, so an
    // omitted options object produces the URL the endpoint had before.
    assert!(
        ts.contains("await doFetch(`${baseUrl}/api/books${qs ? `?${qs}` : \"\"}`,"),
        "{ts}"
    );
}

#[test]
fn an_endpoint_with_no_query_parameters_generates_exactly_what_it_did() {
    // The "no empty options argument on every method" rule of Phase 1: an
    // endpoint that declares none must not gain an argument, a `URLSearchParams`
    // or a `?` in its URL.
    let set = EndpointSet::new().with(
        Endpoint::new(
            sc_api::HandlerId::<sc_api::SignedIn>::new("whoami"),
            Method::Get,
            PathSpec::root().lit("api/whoami"),
        )
        .output(TypeSchema::json()),
    );
    let ts = sc_api::generate_client(&set);
    assert!(ts.contains("whoami(): Promise<WhoamiResponse>;"), "{ts}");
    assert!(!ts.contains("URLSearchParams"), "{ts}");
    assert!(!ts.contains("WhoamiQuery"), "{ts}");
    assert!(
        ts.contains("await doFetch(`${baseUrl}/api/whoami`,"),
        "{ts}"
    );
}

#[test]
fn the_request_helpers_keep_order_and_duplicates() {
    use sc_api::ApiRequest;

    // The shape decision 3 exists for: a range is two values under one key, and
    // both of them mean something.
    let req = ApiRequest::get("/api/books")
        .query("published", "gte.2020-01-01")
        .query("select", "title")
        .query("published", "lt.2024-01-01");

    assert_eq!(req.query_get("published"), Some("gte.2020-01-01"));
    assert_eq!(
        req.query_all("published").collect::<Vec<_>>(),
        vec!["gte.2020-01-01", "lt.2024-01-01"],
    );
    assert_eq!(req.query_get("missing"), None);
    assert_eq!(req.query_all("missing").count(), 0);
    // Order is arrival order, across keys as well as within one.
    assert_eq!(
        req.query
            .iter()
            .map(|(k, _)| k.as_str())
            .collect::<Vec<_>>(),
        vec!["published", "select", "published"],
    );
}
