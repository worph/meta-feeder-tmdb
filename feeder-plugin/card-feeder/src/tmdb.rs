//! The `tmdb` upstream — a **card** feeder plugin.
//!
//! Answers the discovery half of gateway search: free text in, `fileType=card`
//! records out. It never calls an indexer and never serves bytes, which is the
//! whole point — a card query costs at most one cached TMDB `search/multi`, so
//! the expensive indexer budget is spent later, entirely on the single card the
//! user clicks (design doc §1.2).
//!
//! When MyAnimeList lands it becomes a sibling `FeederPlugin` in this same
//! binary, declaring the same `served_file_types = ["card"]`.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use meta_feeder_sdk::cache::MidhashCache;
use meta_feeder_sdk::config::ConfigSchema;
use meta_feeder_sdk::plugin::{
    ConfigError, FeederPlugin, GatewayQuery, HashKind, HashOutcome,
};
use meta_feeder_sdk::types::{DiscoveryRecord, GatewayError, Hash, PluginHealth};
use tracing::warn;

use crate::card::{split_record_id, Card, CardSource};
use crate::consts::DEFAULT_CARD_TOP_N;
use crate::resolve::Resolver;
use crate::tmdb_budget::{TmdbBudget, DEFAULT_TMDB_BURST, DEFAULT_TMDB_REFILL_PER_SEC};
use crate::tmdb_client::TmdbClient;

/// Config the operator supplies via the feeder's own web form (invariant 12 —
/// per-plugin config lives on the feeder, never on the gateway).
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct TmdbConfig {
    #[serde(default)]
    pub tmdb_token: String,
    #[serde(default)]
    pub tmdb_rate_per_sec: Option<f64>,
    #[serde(default)]
    pub tmdb_burst: Option<f64>,
    #[serde(default)]
    pub card_top_n: Option<usize>,
}

impl TmdbConfig {
    /// Env seed, used on first boot before any `config.json` exists.
    pub fn from_env() -> Self {
        Self {
            tmdb_token: std::env::var("TMDB_TOKEN").unwrap_or_default(),
            tmdb_rate_per_sec: std::env::var("TMDB_RATE_PER_SEC")
                .ok()
                .and_then(|v| v.parse().ok()),
            tmdb_burst: std::env::var("TMDB_BURST").ok().and_then(|v| v.parse().ok()),
            card_top_n: std::env::var("CARD_TOP_N").ok().and_then(|v| v.parse().ok()),
        }
    }
}

pub struct TmdbCardPlugin {
    config: TmdbConfig,
    /// `None` until `configure` runs, and stays `None` when no TMDB token was
    /// supplied — the soft-skip path. The feeder still serves `/health` so
    /// `depends_on` is satisfied; queries just return nothing.
    resolver: Option<Resolver>,
    top_n: usize,
    /// Test hook: `(api_base, image_base)` overriding TMDB's real endpoints, so
    /// the contract test can point the client at a wiremock server.
    api_bases: Option<(String, String)>,
}

impl Default for TmdbCardPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl TmdbCardPlugin {
    pub fn new() -> Self {
        Self {
            config: TmdbConfig::from_env(),
            resolver: None,
            top_n: DEFAULT_CARD_TOP_N,
            api_bases: None,
        }
    }

    /// Test constructor: a fixed token plus TMDB endpoint overrides, so the
    /// contract test can drive the real router against a wiremock TMDB.
    pub fn with_api_base(token: &str, api_base: String, image_base: String) -> Self {
        let mut p = Self::new();
        p.config.tmdb_token = token.to_string();
        p.api_bases = Some((api_base, image_base));
        p
    }

    /// File wins over env, exactly like every other feeder: `config.json` in the
    /// per-plugin cache dir is read at `configure` time, falling back to the env
    /// seed. No hot reload — bounce the feeder.
    fn load_config(&mut self, cache_dir: &Path) {
        let path = cache_dir.join("config.json");
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok(file_cfg) = serde_json::from_slice::<TmdbConfig>(&bytes) {
                if !file_cfg.tmdb_token.trim().is_empty() {
                    self.config.tmdb_token = file_cfg.tmdb_token;
                }
                if file_cfg.tmdb_rate_per_sec.is_some() {
                    self.config.tmdb_rate_per_sec = file_cfg.tmdb_rate_per_sec;
                }
                if file_cfg.tmdb_burst.is_some() {
                    self.config.tmdb_burst = file_cfg.tmdb_burst;
                }
                if file_cfg.card_top_n.is_some() {
                    self.config.card_top_n = file_cfg.card_top_n;
                }
            }
        }
    }

    fn resolver(&self) -> Option<&Resolver> {
        self.resolver.as_ref()
    }
}

#[async_trait]
impl FeederPlugin for TmdbCardPlugin {
    fn upstream_id(&self) -> &'static str {
        "tmdb"
    }

    /// **The routing declaration.** `card` on the fileType axis is what makes
    /// meta-search fan a `fileType:card` query out to this gateway — no
    /// meta-search change was needed to add the tier (design doc §6). The
    /// content kinds are the *work* kinds a card can describe.
    fn served_file_types(&self) -> &'static [&'static str] {
        &["card"]
    }

    fn served_content_kinds(&self) -> &'static [&'static str] {
        &["movie", "series"]
    }

    fn configure(&mut self, cache_dir: &Path) -> Result<(), ConfigError> {
        self.load_config(cache_dir);
        let cache: MidhashCache = meta_feeder_sdk::common::open_midhash_cache(cache_dir, "tmdb")?;
        self.top_n = self.config.card_top_n.filter(|n| *n > 0).unwrap_or(DEFAULT_CARD_TOP_N);

        let token = self.config.tmdb_token.trim().to_string();
        if token.is_empty() {
            // Soft-skip, matching every other opt-in upstream: stay healthy so
            // the gateway's `depends_on` is satisfied, but answer nothing.
            warn!(
                target: "meta-share::gateway",
                upstream = "tmdb",
                "no TMDB token configured; card queries will return nothing \
                 (set it on the feeder's config page or via TMDB_TOKEN)"
            );
            return Ok(());
        }
        // `TmdbBudget::new` already returns an `Arc`.
        let budget = TmdbBudget::new(
            self.config
                .tmdb_rate_per_sec
                .filter(|v| *v > 0.0)
                .unwrap_or(DEFAULT_TMDB_REFILL_PER_SEC),
            self.config
                .tmdb_burst
                .filter(|v| *v > 0.0)
                .unwrap_or(DEFAULT_TMDB_BURST),
        );
        let client = match &self.api_bases {
            Some((api, image)) => TmdbClient::with_bases(token, api.clone(), image.clone()),
            None => TmdbClient::new(token),
        };
        self.resolver = Some(Resolver::new(Arc::new(client), cache, budget));
        Ok(())
    }

    fn health(&self) -> PluginHealth {
        PluginHealth::Ok
    }

    async fn handle_query(
        &self,
        query: &GatewayQuery,
        max_results: usize,
    ) -> Result<Vec<DiscoveryRecord>, GatewayError> {
        // Layer A early-return: this plugin only ever serves cards.
        if !meta_feeder_sdk::query_eval::query_accepts_plugin(
            query,
            self.served_file_types(),
            self.served_content_kinds(),
        ) {
            return Ok(Vec::new());
        }
        let Some(resolver) = self.resolver() else {
            return Ok(Vec::new()); // soft-skipped (no token)
        };

        // An explicit `tmdbid:` filter resolves exactly one card — the direct
        // "give me this work" lookup a deep link uses. Otherwise the free text
        // goes through the principal search.
        let cards: Vec<Card> = if let Some(id) = query
            .filters
            .get("tmdbid")
            .and_then(|v| v.first())
            .and_then(|s| s.trim().parse::<u64>().ok())
        {
            let kind = kind_hint(query, resolver, id).await;
            resolver.card_by_id(id, kind).await.into_iter().collect()
        } else {
            let free_text = query.free_text.trim();
            if free_text.is_empty() {
                return Ok(Vec::new());
            }
            let n = self.top_n.min(max_results.max(1));
            resolver.cards_for_text(free_text, n).await
        };

        let client = &resolver.client;
        Ok(cards
            .iter()
            .filter_map(|c| c.to_record(client, &query.filters))
            .take(max_results)
            .collect())
    }

    /// Resolve a card `record_id` back into its locator CID.
    ///
    /// This is **not** a stub for a byte-less plugin — it is the card path. The
    /// outcome carries `bytes: None`, so the gateway core's three-branch
    /// auto-store routes it to `store_metadata_only`: the record lands in
    /// meta-core, nothing is written to WebDAV, and nothing is seeded to
    /// bitswap (seeding is gated on `Sha2_256`). The record IS the payload.
    ///
    /// The CID needs no network call — it is a pure function of the record id,
    /// which is itself `"<source>:<source_id>"`. We re-resolve the card anyway
    /// so the stored record is complete; a resolution failure still yields the
    /// CID, because the identity is derivable regardless.
    async fn compute_outcomes(&self, record_id: &str) -> Result<Vec<HashOutcome>, GatewayError> {
        let (source, source_id) = split_record_id(record_id).ok_or_else(|| {
            GatewayError::Permanent(format!("malformed card record_id '{record_id}'"))
        })?;
        if source != CardSource::Tmdb.as_str() {
            return Err(GatewayError::NotFound);
        }
        let hash = meta_feeder_sdk::hash::compute_card_cid(source, source_id).ok_or_else(|| {
            GatewayError::Permanent(format!("card id too long to encode: '{record_id}'"))
        })?;

        let record = match (self.resolver(), parse_source_id(source_id)) {
            (Some(resolver), Some((kind, tmdbid))) => resolver
                .card_by_id(tmdbid, kind)
                .await
                .and_then(|c| c.to_record(&resolver.client, &Default::default())),
            _ => None,
        };

        Ok(vec![HashOutcome {
            hash: Hash(hash),
            hash_kind: HashKind::CardLocator,
            bytes: None,
            record,
            file_extension: None,
        }])
    }

    fn config_schema(&self) -> ConfigSchema {
        use meta_feeder_sdk::config::{ConfigField as F, ConfigSchema};
        ConfigSchema {
            fields: vec![
                F::secret("tmdb_token", "TMDB v4 token").with_help(
                    "Read-access token from themoviedb.org. Without it this feeder \
                     soft-skips and card searches return nothing.",
                ),
                F::number("tmdb_rate_per_sec", "TMDB rate (req/s)").with_help(
                    "Sustained TMDB request rate. Blank keeps the built-in default \
                     (20/s). Takes effect on the next feeder restart.",
                ),
                F::number("tmdb_burst", "TMDB burst (tokens)").with_help(
                    "Short-burst TMDB token ceiling. Blank keeps the built-in \
                     default (20). Takes effect on the next feeder restart.",
                ),
                F::number("card_top_n", "Cards per search").with_help(
                    "How many works a free-text search returns, most popular first. \
                     Blank keeps the built-in default (10). Unlike the old anchor \
                     top-N this costs no indexer requests — only TMDB.",
                ),
            ],
        }
    }

    fn config_values(&self) -> serde_json::Value {
        serde_json::json!({
            "tmdb_token": self.config.tmdb_token,
            "tmdb_rate_per_sec": self.config.tmdb_rate_per_sec,
            "tmdb_burst": self.config.tmdb_burst,
            "card_top_n": self.config.card_top_n,
        })
    }
}

/// Decide whether an explicit `tmdbid:` refers to a show or a film: an explicit
/// `contentKind` filter wins, else probe TV details (a movie id has none).
async fn kind_hint(query: &GatewayQuery, resolver: &Resolver, id: u64) -> crate::tmdb_client::TmdbKind {
    use crate::tmdb_client::TmdbKind;
    match query
        .filters
        .get("contentKind")
        .and_then(|v| v.first())
        .map(|s| s.as_str())
    {
        Some("movie") => return TmdbKind::Movie,
        Some("series") | Some("episode") | Some("tvshow") => return TmdbKind::Tv,
        _ => {}
    }
    match resolver.tv_details(id).await {
        Some(d) if d.number_of_seasons > 0 => TmdbKind::Tv,
        _ => TmdbKind::Movie,
    }
}

/// `"tv:95479"` / `"movie:27205"` → `(kind, tmdbid)`.
fn parse_source_id(source_id: &str) -> Option<(crate::tmdb_client::TmdbKind, u64)> {
    use crate::tmdb_client::TmdbKind;
    let (kind, id) = source_id.split_once(':')?;
    let kind = match kind {
        "tv" => TmdbKind::Tv,
        "movie" => TmdbKind::Movie,
        _ => return None,
    };
    Some((kind, id.trim().parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmdb_client::TmdbKind;

    #[test]
    fn source_id_parses_both_kinds() {
        assert!(matches!(
            parse_source_id("tv:95479"),
            Some((TmdbKind::Tv, 95479))
        ));
        assert!(matches!(
            parse_source_id("movie:27205"),
            Some((TmdbKind::Movie, 27205))
        ));
        assert!(parse_source_id("person:123").is_none());
        assert!(parse_source_id("tv:notanumber").is_none());
        assert!(parse_source_id("95479").is_none());
    }

    /// The routing declaration is load-bearing: `card` here is the only reason
    /// meta-search fans a `fileType:card` query out to this gateway.
    #[test]
    fn declares_the_card_file_type() {
        let p = TmdbCardPlugin::new();
        assert_eq!(p.served_file_types(), &["card"]);
        assert_eq!(p.served_content_kinds(), &["movie", "series"]);
        assert_eq!(p.upstream_id(), "tmdb");
    }

    /// A card CID is derivable with no network and no resolver, so
    /// `compute_outcomes` must still produce one when the feeder is
    /// soft-skipped — only the record is missing.
    #[tokio::test]
    async fn compute_outcomes_yields_a_byteless_locator_without_a_token() {
        let p = TmdbCardPlugin::new();
        let outcomes = p.compute_outcomes("tmdb:tv:95479").await.expect("outcomes");
        assert_eq!(outcomes.len(), 1);
        let o = &outcomes[0];
        assert_eq!(o.hash_kind, HashKind::CardLocator);
        assert!(o.bytes.is_none(), "a card has no bytes, ever");
        assert!(o.file_extension.is_none());
        assert_eq!(
            o.hash.0,
            meta_feeder_sdk::hash::compute_card_cid("tmdb", "tv:95479").unwrap()
        );
    }

    #[tokio::test]
    async fn compute_outcomes_rejects_a_foreign_or_malformed_id() {
        let p = TmdbCardPlugin::new();
        assert!(p.compute_outcomes("mal:52991").await.is_err());
        assert!(p.compute_outcomes("nosource").await.is_err());
    }

    /// Soft-skip: no token means no cards, not a hard failure — the feeder
    /// stays healthy so the gateway's `depends_on` is satisfied.
    #[tokio::test]
    async fn queries_return_nothing_without_a_token() {
        let p = TmdbCardPlugin::new();
        let q = GatewayQuery::from_free_text("frieren");
        assert!(p.handle_query(&q, 10).await.unwrap().is_empty());
        assert!(matches!(p.health(), PluginHealth::Ok));
    }

    /// Layer-A routing: a query for something this plugin cannot serve returns
    /// immediately without touching TMDB.
    #[tokio::test]
    async fn rejects_a_query_for_a_type_it_does_not_serve() {
        let p = TmdbCardPlugin::new();
        let mut q = GatewayQuery::from_free_text("frieren");
        q.filters
            .insert("fileType".to_string(), vec!["video".to_string()]);
        assert!(p.handle_query(&q, 10).await.unwrap().is_empty());
    }
}
