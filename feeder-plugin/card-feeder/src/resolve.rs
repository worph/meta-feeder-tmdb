//! Cache- and budget-gated TMDB resolution: free text → [`Card`]s.
//!
//! A slimmed lift of the indexer feeder's `enrich.rs` `TmdbEnricher`. Only the
//! **anchor-resolution** half comes across — `principal_top_n`, `tv_details`,
//! `movie_details`, `external_ids`. The per-release fuzzy title search and its
//! single-flight map stay in the indexer feeder, which still owns release
//! enrichment (design doc §8).
//!
//! The redb table names are unchanged from the indexer feeder's cache, so a
//! warm cache carries straight over.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use meta_feeder_sdk::cache::MidhashCache;
use tracing::debug;

use crate::card::{Card, CardSource};
use crate::consts::{
    CACHED_PRINCIPAL_DEPTH, TMDB_DISCOVERY_WAIT_DEADLINE_SECS, TMDB_WAIT_DEADLINE_SECS,
};
use crate::tmdb_budget::{Lease, TmdbBudget};
use crate::tmdb_client::{
    principal_top_n, TmdbCall, TmdbClient, TmdbExternalIds, TmdbHit, TmdbKind, TmdbTvDetails,
};

/// TMDB resolution with a persistent cache in front and a token budget behind.
#[derive(Clone)]
pub(crate) struct Resolver {
    pub(crate) client: Arc<TmdbClient>,
    pub(crate) cache: MidhashCache,
    pub(crate) budget: Arc<TmdbBudget>,
    /// `genre id → name` per kind, held for the process lifetime.
    ///
    /// In memory rather than in the redb cache on purpose: the table is 16 TV /
    /// 19 movie entries and effectively static, so a process-local map costs one
    /// call per kind per boot and needs no invalidation story — whereas the redb
    /// tables are permanent-hit and shared byte-for-byte with the indexer
    /// feeder's, which this is not part of.
    genre_names: Arc<std::sync::RwLock<HashMap<&'static str, Arc<HashMap<u32, String>>>>>,
}

impl Resolver {
    pub(crate) fn new(client: Arc<TmdbClient>, cache: MidhashCache, budget: Arc<TmdbBudget>) -> Self {
        Self {
            client,
            cache,
            budget,
            genre_names: Arc::new(std::sync::RwLock::new(HashMap::new())),
        }
    }

    /// The `genre id → name` table for `kind`, fetched once and then reused.
    ///
    /// Needed only by the **list** paths (discovery/search), where TMDB gives
    /// bare `genre_ids`; a details fetch returns named genre objects directly.
    /// Best-effort: a failed or empty fetch yields an empty map (cards simply
    /// carry no genres) and is **not** cached, so the next row retries.
    pub(crate) async fn genre_names(&self, kind: TmdbKind) -> Arc<HashMap<u32, String>> {
        let key = match kind {
            TmdbKind::Tv => "tv",
            TmdbKind::Movie => "movie",
        };
        if let Some(m) = self.genre_names.read().ok().and_then(|g| g.get(key).cloned()) {
            return m;
        }
        let fetched: HashMap<u32, String> = self
            .budgeted_with_deadline(
                TMDB_DISCOVERY_WAIT_DEADLINE_SECS,
                self.client.genre_list(kind),
                |gs| {
                    gs.into_iter()
                        .filter(|g| !g.name.trim().is_empty())
                        .map(|g| (g.id, g.name))
                        .collect()
                },
            )
            .await
            .unwrap_or_default();
        let arc = Arc::new(fetched);
        if !arc.is_empty() {
            if let Ok(mut g) = self.genre_names.write() {
                g.insert(key, arc.clone());
            }
        }
        arc
    }

    /// Acquire a budget token, run `call`, map a hit through `on_hit`. A 429
    /// pauses the shared bucket globally rather than being retried here.
    async fn budgeted<T, R>(
        &self,
        call: impl std::future::Future<Output = TmdbCall<T>>,
        on_hit: impl FnOnce(T) -> R,
    ) -> Option<R> {
        self.budgeted_with_deadline(TMDB_WAIT_DEADLINE_SECS, call, on_hit)
            .await
    }

    /// [`Self::budgeted`] with a caller-chosen permit deadline, for callers whose
    /// cost of waiting differs from a user-facing lookup's — see
    /// [`TMDB_DISCOVERY_WAIT_DEADLINE_SECS`].
    async fn budgeted_with_deadline<T, R>(
        &self,
        deadline_secs: u64,
        call: impl std::future::Future<Output = TmdbCall<T>>,
        on_hit: impl FnOnce(T) -> R,
    ) -> Option<R> {
        if matches!(
            self.budget.acquire(Duration::from_secs(deadline_secs)).await,
            Lease::DeadlineExceeded
        ) {
            return None;
        }
        match call.await {
            TmdbCall::Hit(x) => Some(on_hit(x)),
            TmdbCall::Miss => None,
            TmdbCall::RateLimited(retry) => {
                self.budget.note_429(retry);
                None
            }
        }
    }

    /// Cached `GET /3/tv/{id}`. Entries written before the display/AKA fields
    /// existed self-heal with one refetch.
    pub(crate) async fn tv_details(&self, tmdbid: u64) -> Option<TmdbTvDetails> {
        let key = tmdbid.to_string();
        if let Ok(Some(json)) = self.cache.get_tmdb_tvdetails(&key) {
            if let Ok(details) = serde_json::from_str::<TmdbTvDetails>(&json) {
                if details.has_display() && details.has_akas() {
                    return Some(details);
                }
            }
        }
        self.budgeted(self.client.tv_details(tmdbid), |details| {
            if let Ok(json) = serde_json::to_string(&details) {
                let _ = self.cache.put_tmdb_tvdetails(&key, &json);
            }
            details
        })
        .await
    }

    /// Cached `GET /3/movie/{id}`.
    pub(crate) async fn movie_hit(&self, tmdbid: u64) -> Option<TmdbHit> {
        let key = tmdbid.to_string();
        if let Ok(Some(json)) = self.cache.get_tmdb_moviedetails(&key) {
            if json != "null" {
                if let Ok(hit) = serde_json::from_str::<TmdbHit>(&json) {
                    return Some(hit);
                }
            }
        }
        self.budgeted(self.client.movie_details(tmdbid), |details| {
            let hit = details.into_hit();
            if let Ok(json) = serde_json::to_string(&hit) {
                let _ = self.cache.put_tmdb_moviedetails(&key, &json);
            }
            hit
        })
        .await
    }

    /// Cached `external_ids` — the source of the card's `tvdbid` / `imdbid`.
    /// External ids are immutable, so a hit is cached forever.
    pub(crate) async fn external_ids(
        &self,
        kind: TmdbKind,
        tmdbid: u64,
    ) -> Option<TmdbExternalIds> {
        let key = tmdbid.to_string();
        if let Ok(Some(json)) = self.cache.get_tmdb_extids(&key) {
            return serde_json::from_str(&json).ok();
        }
        self.budgeted(self.client.external_ids(kind, tmdbid), |ids| {
            if let Ok(json) = serde_json::to_string(&ids) {
                let _ = self.cache.put_tmdb_extids(&key, &json);
            }
            ids
        })
        .await
    }

    /// One page of a TMDB **catalog list** (popular / trending / top-rated /
    /// `discover`) — the keyword-less browse primitive behind
    /// [`crate::discovery`]. `None` on a budget timeout, a 429, or a miss, which
    /// the caller treats as "stop walking pages and keep what you have".
    ///
    /// Deliberately **uncached**, unlike every other method here: a catalog list
    /// is the one TMDB response that is *supposed* to change under you, and the
    /// redb tables in front of the others are permanent-hit caches with no TTL.
    /// See the module doc of [`crate::discovery`] for why the repeat cost is
    /// already absorbed a layer up.
    pub(crate) async fn discovery_page(
        &self,
        kind: TmdbKind,
        path_and_query: &str,
    ) -> Option<Vec<TmdbHit>> {
        self.budgeted_with_deadline(
            TMDB_DISCOVERY_WAIT_DEADLINE_SECS,
            self.client.discovery_list(kind, path_and_query),
            |hits| hits,
        )
        .await
    }

    /// Cached principal `search/multi`: free text → up to `n` confident
    /// `(tmdbid, kind)` candidates, most popular first.
    ///
    /// The full ranked list (up to [`CACHED_PRINCIPAL_DEPTH`]) is persisted and
    /// sliced on read, so changing the top-N knob never needs a cache wipe.
    /// A transient TMDB failure is **not** negative-cached — the client folds
    /// "no results" and "timeout" into the same `Miss`, and caching the latter
    /// once left a keyword permanently card-less until the next cache wipe.
    pub(crate) async fn principal_top_n(&self, query: &str, n: usize) -> Vec<(u64, TmdbKind)> {
        let key = query.trim().to_lowercase();
        if key.is_empty() || n == 0 {
            return Vec::new();
        }
        if let Ok(Some(json)) = self.cache.get_tmdb_principal_topn(&key) {
            let mut list = decode_principal_list(&json);
            list.truncate(n);
            return list;
        }
        if matches!(
            self.budget
                .acquire(Duration::from_secs(TMDB_WAIT_DEADLINE_SECS))
                .await,
            Lease::DeadlineExceeded
        ) {
            return Vec::new(); // transient — don't poison the cache
        }
        let ranked: Vec<(u64, TmdbKind)> = match self.client.search_multi(query).await {
            TmdbCall::Hit(hits) => principal_top_n(&hits, query, CACHED_PRINCIPAL_DEPTH)
                .into_iter()
                .filter_map(|h| h.kind().map(|k| (h.id, k)))
                .collect(),
            TmdbCall::Miss => return Vec::new(),
            TmdbCall::RateLimited(retry) => {
                self.budget.note_429(retry);
                return Vec::new();
            }
        };
        // An empty list here is a genuine negative ("TMDB answered, nothing
        // passed the relevance gate") and is safe to remember.
        let _ = self
            .cache
            .put_tmdb_principal_topn(&key, &encode_principal_list(&ranked));
        let mut out = ranked;
        out.truncate(n);
        out
    }

    /// Resolve one `(tmdbid, kind)` into a full [`Card`] — details plus the
    /// cross-source id bag. `None` when the by-id lookup fails.
    pub(crate) async fn card_by_id(&self, tmdbid: u64, kind: TmdbKind) -> Option<Card> {
        let ext = self.external_ids(kind, tmdbid).await.unwrap_or_default();
        match kind {
            TmdbKind::Tv => {
                let details = self.tv_details(tmdbid).await?;
                let year = details
                    .first_air_date
                    .as_deref()
                    .and_then(|d| d.get(0..4))
                    .and_then(|y| y.parse::<u16>().ok());
                Some(Card {
                    source: CardSource::Tmdb,
                    kind,
                    source_id: format!("tv:{tmdbid}"),
                    tmdbid: Some(tmdbid),
                    tvdb_id: ext.tvdb_id,
                    imdb_id: ext.imdb_id,
                    title: details.name.clone(),
                    guard_titles: Arc::new(dedup_titles(details.guard_titles())),
                    overview: details.overview.clone().filter(|s| !s.is_empty()),
                    poster_path: details.poster_path.clone().filter(|s| !s.is_empty()),
                    genres: genre_names_of(&details.genres),
                    year,
                    seasons: Card::clamp_seasons(details.number_of_seasons),
                    season_summaries: Arc::new(details.seasons.clone()),
                })
            }
            TmdbKind::Movie => {
                let hit = self.movie_hit(tmdbid).await?;
                // The movie path round-trips through `TmdbHit` (that is what the
                // details cache stores), which keeps genres as bare ids — so
                // unlike the TV branch above it needs the id → name table.
                let gmap = self.genre_names(TmdbKind::Movie).await;
                let genres: Vec<String> = hit
                    .genre_ids
                    .iter()
                    .filter_map(|id| gmap.get(id).cloned())
                    .collect();
                let mut titles = vec![hit.title.clone()];
                if let Some(o) = &hit.original_title {
                    titles.push(o.clone());
                }
                titles.extend(hit.alt_titles.iter().cloned());
                Some(Card {
                    source: CardSource::Tmdb,
                    kind,
                    source_id: format!("movie:{tmdbid}"),
                    tmdbid: Some(tmdbid),
                    tvdb_id: None,
                    imdb_id: ext.imdb_id,
                    title: hit.title,
                    guard_titles: Arc::new(dedup_titles(titles)),
                    overview: hit.overview.filter(|s| !s.is_empty()),
                    poster_path: hit.poster_path.filter(|s| !s.is_empty()),
                    genres,
                    year: hit.year,
                    seasons: 0,
                    season_summaries: Arc::new(Vec::new()),
                })
            }
        }
    }

    /// Free text → up to `top_n` cards, most popular first.
    pub(crate) async fn cards_for_text(&self, free_text: &str, top_n: usize) -> Vec<Card> {
        let mut cards = Vec::new();
        for (id, kind) in self.principal_top_n(free_text, top_n).await {
            if let Some(card) = self.card_by_id(id, kind).await {
                cards.push(card);
            }
        }
        debug!(
            target: "meta-share::gateway",
            upstream = "tmdb",
            query = free_text,
            count = cards.len(),
            "resolved cards"
        );
        cards
    }
}

/// Genre display names off a details payload's `{id, name}` array, trimmed and
/// de-duplicated, order preserved.
pub(crate) fn genre_names_of(genres: &[crate::tmdb_client::TmdbGenre]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    genres
        .iter()
        .map(|g| g.name.trim().to_string())
        .filter(|n| !n.is_empty() && seen.insert(n.to_lowercase()))
        .collect()
}

/// Case-insensitive de-dup preserving first-seen order.
pub(crate) fn dedup_titles(titles: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    titles
        .into_iter()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty() && seen.insert(t.to_lowercase()))
        .collect()
}

/// JSON shape persisted in the principal-search cache. Kept byte-identical to
/// the indexer feeder's so a warm cache is shared, not re-earned.
#[derive(serde::Serialize, serde::Deserialize)]
struct PrincipalEntry {
    tmdbid: u64,
    kind: String,
}

fn kind_str(k: TmdbKind) -> &'static str {
    match k {
        TmdbKind::Movie => "movie",
        TmdbKind::Tv => "tv",
    }
}

fn kind_from_str(s: &str) -> Option<TmdbKind> {
    match s {
        "movie" => Some(TmdbKind::Movie),
        "tv" => Some(TmdbKind::Tv),
        _ => None,
    }
}

fn encode_principal_list(list: &[(u64, TmdbKind)]) -> String {
    let entries: Vec<PrincipalEntry> = list
        .iter()
        .map(|&(tmdbid, kind)| PrincipalEntry {
            tmdbid,
            kind: kind_str(kind).to_string(),
        })
        .collect();
    serde_json::to_string(&entries).unwrap_or_else(|_| "[]".to_string())
}

fn decode_principal_list(json: &str) -> Vec<(u64, TmdbKind)> {
    serde_json::from_str::<Vec<PrincipalEntry>>(json)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|e| kind_from_str(&e.kind).map(|k| (e.tmdbid, k)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cache format must stay byte-compatible with the indexer feeder's —
    /// the two share redb table names, so a drift here silently invalidates a
    /// warm cache instead of failing loudly.
    #[test]
    fn principal_list_round_trips() {
        let list = vec![(95479, TmdbKind::Tv), (27205, TmdbKind::Movie)];
        let json = encode_principal_list(&list);
        assert_eq!(json, r#"[{"tmdbid":95479,"kind":"tv"},{"tmdbid":27205,"kind":"movie"}]"#);
        assert_eq!(decode_principal_list(&json), list);
    }

    #[test]
    fn principal_list_tolerates_garbage_and_unknown_kinds() {
        assert!(decode_principal_list("not json").is_empty());
        assert!(decode_principal_list(r#"[{"tmdbid":1,"kind":"person"}]"#).is_empty());
        assert!(decode_principal_list("[]").is_empty());
    }

    #[test]
    fn dedup_titles_is_case_insensitive_and_order_preserving() {
        let out = dedup_titles(vec![
            "Frieren".to_string(),
            "  frieren  ".to_string(),
            "Sousou no Frieren".to_string(),
            "".to_string(),
        ]);
        assert_eq!(out, vec!["Frieren", "Sousou no Frieren"]);
    }
}
