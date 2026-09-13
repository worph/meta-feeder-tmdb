//! End-to-end contract test for the card feeder's HTTP surface: boots the real
//! `serve_feeders` router (card-feeder hosting the TMDB plugin) on an ephemeral
//! port, pointed at a wiremock TMDB, and drives `/manifest`, `/query` and
//! `/compute` over HTTP. Deterministic — no live TMDB.
//!
//! What this pins is the *card contract* specifically:
//! - the manifest advertises `fileType=card`, which is the only reason
//!   meta-search routes a `fileType:card` query here;
//! - a query yields records carrying both type axes plus the whole id bag;
//! - `/compute` yields a **byte-less** `card_locator` outcome — the shape that
//!   lands in the gateway core's metadata-only auto-store branch.

use card_feeder::tmdb::TmdbCardPlugin;
use meta_feeder_sdk::{
    configure_plugins, router, ComputeRequest, ComputeResponse, GatewayQuery, HashKindDto,
    ManifestResponse, QueryRequest, QueryResponse,
};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Spawn the feeder router (TMDB pointed at `upstream`) on an ephemeral port.
/// The TempDir is returned so the per-plugin cache outlives the test.
async fn spawn_feeder(upstream: &MockServer) -> (String, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let plugin = TmdbCardPlugin::with_api_base("test-token", upstream.uri(), upstream.uri());
    let plugins = configure_plugins(vec![Box::new(plugin)], dir.path()).expect("configure plugins");
    let app = router(plugins, "card-feeder-test".to_string(), dir.path());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    (format!("http://{addr}"), dir)
}

/// `search/multi` → one confident TV hit for "frieren".
fn multi_json() -> serde_json::Value {
    serde_json::json!({
        "results": [{
            "id": 95479,
            "media_type": "tv",
            "name": "Frieren: Beyond Journey's End",
            "original_name": "\u{845}\u{9001}\u{306e}\u{30d5}\u{30ea}\u{30fc}\u{30ec}\u{30f3}",
            "overview": "The story follows the elf mage Frieren.",
            "poster_path": "/poster.jpg",
            "first_air_date": "2023-09-29",
            "popularity": 300.0,
            "vote_count": 900
        }]
    })
}

fn tv_details_json() -> serde_json::Value {
    serde_json::json!({
        "id": 95479,
        "name": "Frieren: Beyond Journey's End",
        "original_name": "\u{845}\u{9001}\u{306e}\u{30d5}\u{30ea}\u{30fc}\u{30ec}\u{30f3}",
        "original_language": "ja",
        "overview": "The story follows the elf mage Frieren.",
        "first_air_date": "2023-09-29",
        "poster_path": "/poster.jpg",
        "number_of_seasons": 1,
        "seasons": [{"season_number": 1, "episode_count": 28}],
        "alternative_titles": { "results": [{"iso_3166_1": "JP", "title": "Sousou no Frieren"}] }
    })
}

fn external_ids_json() -> serde_json::Value {
    serde_json::json!({ "imdb_id": "tt22248376", "tvdb_id": 367189 })
}

async fn mount_tmdb(upstream: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/search/multi"))
        .respond_with(ResponseTemplate::new(200).set_body_json(multi_json()))
        .mount(upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/tv/95479"))
        .respond_with(ResponseTemplate::new(200).set_body_json(tv_details_json()))
        .mount(upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/tv/95479/external_ids"))
        .respond_with(ResponseTemplate::new(200).set_body_json(external_ids_json()))
        .mount(upstream)
        .await;
}

fn card_query(text: &str) -> GatewayQuery {
    let mut q = GatewayQuery::from_free_text(text);
    q.filters
        .insert("fileType".to_string(), vec!["card".to_string()]);
    q
}

/// The routing declaration. `served_file_types = ["card"]` is what makes
/// meta-search fan a `fileType:card` query out to this gateway at all.
#[tokio::test]
async fn manifest_advertises_the_card_file_type() {
    let upstream = MockServer::start().await;
    let (base, _dir) = spawn_feeder(&upstream).await;

    let manifest: ManifestResponse = reqwest::Client::new()
        .get(format!("{base}/manifest"))
        .send()
        .await
        .expect("GET /manifest")
        .json()
        .await
        .expect("decode manifest");

    assert_eq!(manifest.plugins.len(), 1);
    let p = &manifest.plugins[0];
    assert_eq!(p.id, "tmdb");
    assert_eq!(p.served_file_types, vec!["card".to_string()]);
    assert_eq!(
        p.served_content_kinds,
        vec!["movie".to_string(), "series".to_string()]
    );
}

#[tokio::test]
async fn query_returns_a_card_with_both_axes_and_the_id_bag() {
    let upstream = MockServer::start().await;
    mount_tmdb(&upstream).await;
    let (base, _dir) = spawn_feeder(&upstream).await;

    let resp: QueryResponse = reqwest::Client::new()
        .post(format!("{base}/query"))
        .json(&QueryRequest {
            upstream_id: "tmdb".to_string(),
            query: card_query("frieren"),
            max_results: 10,
        })
        .send()
        .await
        .expect("POST /query")
        .json()
        .await
        .expect("decode query response");

    assert_eq!(resp.records.len(), 1, "one confident TMDB hit → one card");
    let f = &resp.records[0].fields;
    assert_eq!(f["fileType"], "card");
    assert_eq!(f["contentKind"], "series");
    assert_eq!(f["title"], "Frieren: Beyond Journey's End");
    // Every name as the language-nested key-set: original (ja), the en-US
    // title, and the JP-market AKA.
    for key in [
        "titles/jpn/\u{845}\u{9001}\u{306e}\u{30d5}\u{30ea}\u{30fc}\u{30ec}\u{30f3}",
        "titles/eng/Frieren: Beyond Journey's End",
        "titles/jpn/Sousou no Frieren",
    ] {
        assert_eq!(f.get(key).map(String::as_str), Some("true"), "{key}");
    }
    // The id bag — phase 2 selects from this rather than assuming `tmdbid`.
    assert_eq!(f["tmdbid"], "95479");
    assert_eq!(f["tvdbid"], "367189");
    assert_eq!(f["imdbid"], "tt22248376");
    // Display trio that meta-watch's quality gate requires.
    assert!(f.contains_key("description/eng"));
    assert!(f["poster_url"].contains("poster.jpg"));
    // The record id is the CID preimage.
    assert_eq!(resp.records[0].record_id, "tmdb:tv:95479");
}

/// The core of the design: a card resolves to a locator with **no bytes**.
/// That `bytes_b64: None` is what routes it to `store_metadata_only` in the
/// gateway core's three-branch auto-store.
#[tokio::test]
async fn compute_returns_a_byteless_card_locator() {
    let upstream = MockServer::start().await;
    mount_tmdb(&upstream).await;
    let (base, _dir) = spawn_feeder(&upstream).await;

    let resp: ComputeResponse = reqwest::Client::new()
        .post(format!("{base}/compute"))
        .json(&ComputeRequest {
            upstream_id: "tmdb".to_string(),
            record_id: "tmdb:tv:95479".to_string(),
        })
        .send()
        .await
        .expect("POST /compute")
        .json()
        .await
        .expect("decode compute response");

    assert_eq!(resp.outcomes.len(), 1);
    let o = &resp.outcomes[0];
    assert!(matches!(o.hash_kind, HashKindDto::CardLocator));
    assert!(o.bytes_b64.is_none(), "a card has no bytes, ever");
    assert!(o.file_extension.is_none());
    assert!(o.record.is_some(), "the record IS the payload");
    // Deterministic in (source, id) — any peer derives the same address.
    assert_eq!(
        o.hash,
        meta_feeder_sdk::hash::compute_card_cid("tmdb", "tv:95479").unwrap()
    );
}

/// A card query never reaches an indexer, and a *typed* query for something the
/// card tier does not serve short-circuits before TMDB is touched at all.
#[tokio::test]
async fn a_video_query_does_not_hit_tmdb() {
    let upstream = MockServer::start().await;
    // Deliberately mount nothing: any outbound call would 404 and surface as an
    // empty result, but the assertion below is that we never even get there.
    Mock::given(method("GET"))
        .and(path("/search/multi"))
        .and(query_param("query", "frieren"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&upstream)
        .await;
    let (base, _dir) = spawn_feeder(&upstream).await;

    let mut q = GatewayQuery::from_free_text("frieren");
    q.filters
        .insert("fileType".to_string(), vec!["video".to_string()]);

    let resp: QueryResponse = reqwest::Client::new()
        .post(format!("{base}/query"))
        .json(&QueryRequest {
            upstream_id: "tmdb".to_string(),
            query: q,
            max_results: 10,
        })
        .send()
        .await
        .expect("POST /query")
        .json()
        .await
        .expect("decode query response");

    assert!(resp.records.is_empty());
    // `expect(0)` above is asserted on MockServer drop.
}
