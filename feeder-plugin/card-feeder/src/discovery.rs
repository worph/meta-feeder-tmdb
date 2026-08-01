//! Keyword-less catalog discovery: TMDB's popular / trending / top-rated lists
//! → [`Card`]s.
//!
//! The card tier's *browse* surface, as opposed to its search surface. A client
//! with a home page (meta-watch's wall) cannot ask "what should I show?" through
//! a text query — there is no text. This branch answers a query whose intent is
//! carried entirely by structured filters, by calling TMDB's catalog endpoints.
//!
//! ## Query dialect
//!
//! Deliberately identical to the indexer feeder's older `discovery.rs`, which
//! answers the same dialect with metadata-only *seeds*. Same words, so a client
//! switches tiers by swapping `fileType:video` for `fileType:card` and nothing
//! else:
//!
//! - **mode** (one of) — `popular:true` and `top_rated:true` are served by
//!   `/discover/{movie,tv}` sorted by popularity / rating (rather than the
//!   `/{movie,tv}/popular` list endpoints, which cannot carry the adult gate —
//!   see `DISCOVERY_EXCLUDED_KEYWORDS`); `trending:true` → the real
//!   `/trending/{movie,tv}/week`, the one mode with no `/discover` equivalent
//!   and therefore the one row that stays ungated.
//! - **kind** (required) — `contentKind:movie` → films,
//!   `contentKind:series` (or `episode`/`tvshow`/`tv`) → shows. Also what routes
//!   the query here at all, alongside `fileType:card`.
//! - **anime** (optional) — `anime:true` narrows `/discover` to TMDB's *anime*
//!   keyword, sorted to match the mode. Anime trending has no keyword-filtered
//!   endpoint either, so it approximates with popularity — and stays gated.
//!
//! ## One call, complete cards
//!
//! A TMDB catalog list already carries `title`, `overview`, `poster_path` and
//! `year` per entry — everything [`Card::to_record`] needs to emit a renderable
//! card. So a row costs **one** TMDB call, not one plus twenty by-id lookups.
//!
//! The price is a thinner id bag: `tvdbid`/`imdbid` come from `external_ids`,
//! which a list response has no room for, so a discovery card carries only its
//! `tmdbid`. That is the id consumers actually key on (it is what the detail
//! page's phase-2 `tmdbid:` query is built from), and the full bag is resolved
//! anyway the moment someone clicks the card — `card_by_id` is a different code
//! path and still fetches it. Paying twenty extra calls per row up front to
//! pre-fill ids nobody reads until a click would defeat the branch's purpose.
//!
//! ## Why nothing is cached here
//!
//! A catalog list is **mutable by nature** — "popular this week" is the one
//! thing that must not be frozen. The SDK's redb tables are permanent-hit caches
//! sized for immutable data (`external_ids` is explicitly "cached forever"), so
//! persisting a popularity list there would pin the wall to whatever it looked
//! like on first boot. The layer above already de-duplicates the repeat cost:
//! the gateway persists each emitted card to meta-core and its search-coverage
//! gate then skips a repeated `(upstream, query)` for an hour, serving meta-core
//! instead. Every call here is budget-gated, so a burst cannot run away.

use std::collections::HashMap;
use std::sync::Arc;

use meta_feeder_sdk::plugin::GatewayQuery;
use tracing::debug;

use crate::card::{Card, CardSource};
use crate::consts::{DISCOVERY_EXCLUDED_KEYWORDS, DISCOVERY_MAX_PAGES, DISCOVERY_MIN_VOTES};
use crate::resolve::{dedup_titles, Resolver};
use crate::tmdb_client::{TmdbHit, TmdbKind};

/// TMDB keyword id for "anime". `/discover` is the only catalog endpoint that
/// takes a keyword filter, which is why `anime:true` switches endpoints rather
/// than adding a parameter.
const TMDB_ANIME_KEYWORD: &str = "210024";

/// Results TMDB returns per catalog page — used to translate a desired card
/// count into a page count.
const TMDB_PAGE_SIZE: usize = 20;

/// The catalog-list flavour a discovery query asks for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DiscoveryMode {
    Popular,
    Trending,
    TopRated,
}

impl DiscoveryMode {
    /// The query-filter key that selects this mode.
    ///
    /// It is also echoed onto every emitted record — not here, but by
    /// [`Card::to_record`]'s trailing filter echo, which stamps back every
    /// filter the query carried that the card did not already set. That echo is
    /// load-bearing: both the gateway dispatcher and meta-search's consumer-side
    /// `record_matches` re-apply the query's filters to each record, and a
    /// *missing* field fails the match, so an un-echoed `popular` would see
    /// every card dropped at one of the two tiers.
    fn marker(self) -> &'static str {
        match self {
            DiscoveryMode::Popular => "popular",
            DiscoveryMode::Trending => "trending",
            DiscoveryMode::TopRated => "top_rated",
        }
    }

    /// `sort_by` value for the `/discover` (anime) path. `/discover` has no
    /// "trending", so trending approximates to popularity there — the non-anime
    /// path still uses the real `/trending` endpoint.
    fn discover_sort(self) -> &'static str {
        match self {
            DiscoveryMode::TopRated => "vote_average.desc",
            DiscoveryMode::Popular | DiscoveryMode::Trending => "popularity.desc",
        }
    }
}

/// True when this is a keyword-less catalog query this branch should answer.
///
/// Three conditions, and each excludes a query that belongs to a *different*
/// branch of `handle_query`: no free text (that is the principal search), no
/// `tmdbid:` filter (that is the direct by-id lookup a deep link uses), and a
/// truthy mode marker. Note `contentKind` alone deliberately does **not** select
/// this branch — a text query carries it too.
pub(crate) fn is_discovery_query(query: &GatewayQuery) -> bool {
    query.free_text.trim().is_empty()
        && !query.filters.contains_key("tmdbid")
        && discovery_mode(query).is_some()
}

/// First truthy mode marker on the query, in precedence order.
fn discovery_mode(query: &GatewayQuery) -> Option<DiscoveryMode> {
    for mode in [
        DiscoveryMode::Trending,
        DiscoveryMode::Popular,
        DiscoveryMode::TopRated,
    ] {
        if filter_is_true(query, mode.marker()) {
            return Some(mode);
        }
    }
    None
}

/// True iff `query.filters[key]` carries a truthy value.
fn filter_is_true(query: &GatewayQuery, key: &str) -> bool {
    query
        .filters
        .get(key)
        .is_some_and(|v| v.iter().any(|s| s.eq_ignore_ascii_case("true")))
}

/// Map the query's `contentKind` filter to a TMDB media kind.
///
/// `series` is the canonical spelling for a card (it describes a whole work),
/// but `episode`/`tvshow`/`tv` are accepted so the dialect stays a superset of
/// the indexer feeder's — a client that has not yet switched its row queries
/// still resolves the right endpoint here, even though its records will then be
/// dropped downstream for the mismatch.
fn discovery_kind(query: &GatewayQuery) -> Option<TmdbKind> {
    let values = query.filters.get("contentKind")?;
    for v in values {
        match v.trim().to_ascii_lowercase().as_str() {
            "movie" => return Some(TmdbKind::Movie),
            "series" | "episode" | "tvshow" | "tv" => return Some(TmdbKind::Tv),
            _ => {}
        }
    }
    None
}

/// The genre slugs a `genres:` filter requested, folded for comparison.
///
/// Multi-valued = OR, matching the filter semantics on both re-validating tiers.
fn requested_genres(query: &GatewayQuery) -> Vec<String> {
    query
        .filters
        .get("genres")
        .map(|vs| {
            vs.iter()
                .map(|v| genre_fold(v))
                .filter(|v| !v.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Fold a genre label for comparison: lowercase, alphanumerics only.
///
/// Must stay identical to `genres_filter_matches`'s fold in the SDK and in
/// meta-search — those two re-apply the filter to every record this emits, so a
/// divergence here means the row's own results get dropped downstream.
fn genre_fold(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Resolve the requested genre slugs to TMDB genre ids via the id → name table.
/// Unknown slugs are dropped; if none resolve the caller omits `with_genres`
/// rather than sending an empty filter that TMDB would reject.
fn with_genres_ids(wanted: &[String], names: &HashMap<u32, String>) -> Vec<u32> {
    let mut ids: Vec<u32> = names
        .iter()
        .filter(|(_, name)| wanted.contains(&genre_fold(name)))
        .map(|(id, _)| *id)
        .collect();
    ids.sort_unstable();
    ids
}

/// Build the `path_and_query` for `TmdbClient::discovery_list` from the mode,
/// kind, anime flag, requested genre ids, and 1-based `page`.
fn build_path_and_query(
    mode: DiscoveryMode,
    kind: TmdbKind,
    anime: bool,
    genre_ids: &[u32],
    page: u32,
) -> String {
    let seg = match kind {
        TmdbKind::Movie => "movie",
        TmdbKind::Tv => "tv",
    };
    // Trending is the one mode with no `/discover` equivalent: `/trending` is a
    // distinct, TMDB-computed "this week" signal, not a sort order. So it keeps
    // the list endpoint — and, being a list endpoint, **cannot carry the adult
    // gate below**. The anime variant still routes through `/discover` (there is
    // no keyword-filtered trending at all), approximating it with popularity.
    // …and a genre-scoped row can't use it either: `/trending` takes no
    // `with_genres`, and returning an unfiltered trending page for an "Action"
    // row would fill it with whatever is trending. `/discover` sorted by
    // popularity is the closest filterable approximation.
    if matches!(mode, DiscoveryMode::Trending) && !anime && genre_ids.is_empty() {
        return format!("trending/{seg}/week?page={page}");
    }

    // Everything else goes through `/discover`, including the non-anime rows
    // that used to use `/{seg}/popular` and `/{seg}/top_rated`.
    //
    // Those fixed list endpoints accept neither `without_keywords` nor
    // `vote_count.gte`, and measurement showed they genuinely need them: three
    // hentai titles reached positions 4, 5 and 17 of the *non-anime* `tv/popular`
    // row for the same traffic-metric reason described on
    // [`DISCOVERY_EXCLUDED_KEYWORDS`]. `/discover` with `sort_by=popularity.desc`
    // is the filterable equivalent, so one code path now carries the gate for
    // every mode that can hold it.
    //
    // Top-rated floors higher than the shared gate: sorting by rating puts a
    // single 10.0 vote on top, which a 50-vote gate doesn't stop.
    let votes = match mode {
        DiscoveryMode::TopRated => 200,
        _ => DISCOVERY_MIN_VOTES,
    };
    let keyword = if anime {
        format!("with_keywords={TMDB_ANIME_KEYWORD}&")
    } else {
        String::new()
    };
    // `with_genres` is comma-joined = AND on TMDB's side, pipe-joined = OR. A
    // multi-valued filter means OR everywhere else in the DSL, so join with `|`
    // to keep the wire semantics and the upstream semantics saying the same
    // thing. URL-encoded, since a bare `|` is not valid in a query string.
    let genres = if genre_ids.is_empty() {
        String::new()
    } else {
        let joined = genre_ids
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join("%7C");
        format!("with_genres={joined}&")
    };
    format!(
        "discover/{seg}?{keyword}{genres}sort_by={}&include_adult=false\
         &without_keywords={DISCOVERY_EXCLUDED_KEYWORDS}&vote_count.gte={votes}&page={page}",
        mode.discover_sort(),
    )
}

/// Project one catalog hit into a [`Card`], with no further network calls.
///
/// `source_id` uses the same `"tv:{id}"` / `"movie:{id}"` shape
/// `Resolver::card_by_id` produces, so a discovery card's `record_id` — and
/// therefore its `0x1007` locator CID — is byte-identical to the one the by-id
/// path derives for the same work. That is what lets a card the wall surfaced
/// and a card the search box surfaced converge on one record instead of two.
fn card_from_hit(hit: &TmdbHit, kind: TmdbKind, genres: &HashMap<u32, String>) -> Card {
    let seg = match kind {
        TmdbKind::Movie => "movie",
        TmdbKind::Tv => "tv",
    };
    let mut titles = vec![hit.title.clone()];
    if let Some(o) = &hit.original_title {
        titles.push(o.clone());
    }
    titles.extend(hit.alt_titles.iter().cloned());
    Card {
        source: CardSource::Tmdb,
        kind,
        source_id: format!("{seg}:{}", hit.tmdbid),
        tmdbid: Some(hit.tmdbid),
        // Not available on a list response — see the module doc. Resolved on
        // the click-time `card_by_id` path instead.
        tvdb_id: None,
        imdb_id: None,
        title: hit.title.clone(),
        guard_titles: Arc::new(dedup_titles(titles)),
        overview: hit.overview.clone().filter(|s| !s.trim().is_empty()),
        poster_path: hit.poster_path.clone().filter(|s| !s.trim().is_empty()),
        // A list hit carries bare genre ids; the table names them.
        genres: hit.genre_ids.iter().filter_map(|id| genres.get(id).cloned()).collect(),
        year: hit.year,
        // A list response carries no season structure. `seasonCount` is simply
        // omitted from the record (`Card::to_record` skips it at `seasons == 0`).
        seasons: 0,
        season_summaries: Arc::new(Vec::new()),
    }
}

/// Resolve a keyword-less catalog query to up to `max_results` cards, walking
/// TMDB pages until the cap is reached or a page comes back empty.
///
/// Best-effort throughout: no mode or no `contentKind` yields `[]`, and a
/// rate-limit or miss breaks the walk and returns what was already gathered.
/// The branch never errors — an empty row is the failure mode, matching
/// `Resolver::cards_for_text`.
pub(crate) async fn cards_for_discovery(
    resolver: &Resolver,
    query: &GatewayQuery,
    max_results: usize,
) -> Vec<Card> {
    let (Some(mode), Some(kind)) = (discovery_mode(query), discovery_kind(query)) else {
        return Vec::new();
    };
    let anime = filter_is_true(query, "anime");
    // Fetched once per process, not per row (see `Resolver::genre_names`).
    let gmap = resolver.genre_names(kind).await;
    // A `genres:` filter narrows the row upstream via `with_genres`, so the row
    // is a full page OF that genre rather than a page filtered down to it.
    let wanted = requested_genres(query);
    let genre_ids = with_genres_ids(&wanted, &gmap);
    if !wanted.is_empty() && genre_ids.is_empty() {
        // Asked for a genre this kind doesn't have (e.g. `genres:kids` on the
        // movie side, where TMDB files it as "Family"). Returning an unfiltered
        // page would silently fill the row with the wrong thing.
        debug!(
            target: "meta-share::gateway", upstream = "tmdb",
            ?wanted, "discovery: no TMDB genre id matched; row left empty"
        );
        return Vec::new();
    }
    // One page of headroom over the arithmetic minimum: `out` counts *displayable*
    // cards, and a catalog page reliably contains a few entries with no overview
    // or no poster (newly-added or non-English works), which are declined at
    // projection. Without the spare page a 20-card row comes back at 17.
    let pages = (max_results.div_ceil(TMDB_PAGE_SIZE).max(1) as u32 + 1).min(DISCOVERY_MAX_PAGES);
    let mut out: Vec<Card> = Vec::new();
    for page in 1..=pages {
        if out.len() >= max_results {
            break;
        }
        let path_and_query = build_path_and_query(mode, kind, anime, &genre_ids, page);
        let Some(hits) = resolver.discovery_page(kind, &path_and_query).await else {
            break; // budget exhausted, 429, or miss — keep what we have
        };
        if hits.is_empty() {
            break; // walked past the last catalog page
        }
        out.extend(
            hits.iter()
                .map(|h| card_from_hit(h, kind, &gmap))
                .filter(Card::is_displayable),
        );
    }
    out.truncate(max_results);
    debug!(
        target: "meta-share::gateway",
        upstream = "tmdb",
        mode = mode.marker(),
        anime,
        count = out.len(),
        "resolved discovery cards"
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn query(filters: &[(&str, &str)]) -> GatewayQuery {
        let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (k, v) in filters {
            map.entry(k.to_string()).or_default().push(v.to_string());
        }
        GatewayQuery {
            raw_text: String::new(),
            free_text: String::new(),
            filters: map,
            ranges: Vec::new(),
            negations: Vec::new(),
        }
    }

    #[test]
    fn selects_only_keyword_less_mode_queries() {
        assert!(is_discovery_query(&query(&[
            ("popular", "true"),
            ("contentKind", "series"),
            ("fileType", "card"),
        ])));
        // A mode marker but with free text → the principal search owns it.
        let mut with_text = query(&[("popular", "true"), ("contentKind", "series")]);
        with_text.free_text = "frieren".to_string();
        assert!(!is_discovery_query(&with_text));
        // An explicit id → the by-id lookup owns it.
        assert!(!is_discovery_query(&query(&[
            ("popular", "true"),
            ("tmdbid", "95479"),
        ])));
        // contentKind alone must NOT select this branch.
        assert!(!is_discovery_query(&query(&[
            ("contentKind", "series"),
            ("fileType", "card"),
        ])));
        // A falsey marker is not a marker.
        assert!(!is_discovery_query(&query(&[("popular", "false")])));
    }

    /// `contentKind:series` is the canonical card spelling; the indexer feeder's
    /// older `episode` spelling still resolves the same endpoint.
    #[test]
    fn content_kind_maps_series_and_the_legacy_spellings() {
        assert_eq!(
            discovery_kind(&query(&[("contentKind", "series")])),
            Some(TmdbKind::Tv)
        );
        assert_eq!(
            discovery_kind(&query(&[("contentKind", "episode")])),
            Some(TmdbKind::Tv)
        );
        assert_eq!(
            discovery_kind(&query(&[("contentKind", "movie")])),
            Some(TmdbKind::Movie)
        );
        assert_eq!(discovery_kind(&query(&[("contentKind", "audiobook")])), None);
        assert_eq!(discovery_kind(&query(&[])), None);
    }

    #[test]
    fn builds_the_catalog_endpoints() {
        // Popular / top-rated route through /discover (so the gate applies);
        // only the sort and the vote floor differ.
        let pop = build_path_and_query(DiscoveryMode::Popular, TmdbKind::Movie, false, &[], 1);
        assert!(pop.starts_with("discover/movie?"), "{pop}");
        assert!(pop.contains("sort_by=popularity.desc"), "{pop}");
        assert!(pop.contains("page=1"), "{pop}");

        let paged = build_path_and_query(DiscoveryMode::Popular, TmdbKind::Tv, false, &[], 2);
        assert!(paged.starts_with("discover/tv?"), "{paged}");
        assert!(paged.contains("page=2"), "{paged}");

        let top = build_path_and_query(DiscoveryMode::TopRated, TmdbKind::Movie, false, &[], 1);
        assert!(top.contains("sort_by=vote_average.desc"), "{top}");
        assert!(top.contains("vote_count.gte=200"), "{top}");

        // Trending keeps the real endpoint — see the dedicated test.
        assert_eq!(
            build_path_and_query(DiscoveryMode::Trending, TmdbKind::Tv, false, &[], 1),
            "trending/tv/week?page=1"
        );
    }

    #[test]
    fn anime_switches_to_the_keyword_filtered_discover_endpoint() {
        let p = build_path_and_query(DiscoveryMode::Popular, TmdbKind::Tv, true, &[], 1);
        assert!(p.starts_with("discover/tv?"), "{p}");
        assert!(p.contains("with_keywords=210024"), "{p}");
        assert!(p.contains("sort_by=popularity.desc"), "{p}");
        let top = build_path_and_query(DiscoveryMode::TopRated, TmdbKind::Movie, true, &[], 1);
        assert!(top.contains("sort_by=vote_average.desc"), "{top}");
        // Sorting by rating puts a 1-vote 10.0 on top, so top-rated floors higher.
        assert!(top.contains("vote_count.gte=200"), "{top}");
    }

    /// The gate is not anime-specific. Three hentai titles were measured at
    /// positions 4, 5 and 17 of the **non-anime** `tv/popular` row, which is why
    /// the plain popular/top-rated modes were moved off the fixed list endpoints
    /// (which cannot carry `without_keywords`) and onto `/discover`.
    #[test]
    fn non_anime_popular_and_top_rated_also_gate() {
        for mode in [DiscoveryMode::Popular, DiscoveryMode::TopRated] {
            let p = build_path_and_query(mode, TmdbKind::Tv, false, &[], 1);
            assert!(p.starts_with("discover/tv?"), "{p}");
            assert!(!p.contains("with_keywords="), "no anime narrowing here: {p}");
            assert!(p.contains("without_keywords="), "{p}");
            assert!(p.contains("vote_count.gte="), "{p}");
        }
    }

    /// Trending is the documented exception: `/trending` is a TMDB-computed
    /// signal with no `/discover` sort equivalent, so the non-anime trending row
    /// keeps the list endpoint and cannot carry the gate.
    #[test]
    fn non_anime_trending_keeps_the_real_trending_endpoint() {
        let p = build_path_and_query(DiscoveryMode::Trending, TmdbKind::Tv, false, &[], 1);
        assert_eq!(p, "trending/tv/week?page=1");
        // …but the anime variant has no keyword-filtered trending at all, so it
        // still routes through /discover and stays gated.
        let a = build_path_and_query(DiscoveryMode::Trending, TmdbKind::Tv, true, &[], 1);
        assert!(a.starts_with("discover/tv?"), "{a}");
        assert!(a.contains("without_keywords="), "{a}");
    }

    /// A genre-scoped row narrows **upstream** via `with_genres`, so the row is a
    /// full page of that genre rather than a page filtered down to a handful.
    #[test]
    fn a_genre_row_narrows_upstream() {
        let p = build_path_and_query(DiscoveryMode::Popular, TmdbKind::Tv, false, &[10759], 1);
        assert!(p.contains("with_genres=10759"), "{p}");
        // Multi-valued = OR, so pipe-joined (comma would mean AND to TMDB).
        let multi = build_path_and_query(DiscoveryMode::Popular, TmdbKind::Tv, false, &[16, 35], 1);
        assert!(multi.contains("with_genres=16%7C35"), "{multi}");
        // No genre filter ⇒ the parameter is absent, not empty.
        let none = build_path_and_query(DiscoveryMode::Popular, TmdbKind::Tv, false, &[], 1);
        assert!(!none.contains("with_genres"), "{none}");
    }

    /// A genre row can't use `/trending` (it takes no `with_genres`), so it falls
    /// back to the filterable `/discover` rather than returning an unfiltered
    /// trending page under a genre heading.
    #[test]
    fn a_genre_scoped_trending_row_falls_back_to_discover() {
        let p = build_path_and_query(DiscoveryMode::Trending, TmdbKind::Tv, false, &[10759], 1);
        assert!(p.starts_with("discover/tv?"), "{p}");
        assert!(p.contains("with_genres=10759"), "{p}");
    }

    /// Slugs resolve to TMDB ids through the id → name table, folded on both
    /// sides so the DSL never has to spell a space or an ampersand.
    #[test]
    fn genre_slugs_resolve_to_tmdb_ids() {
        let names = HashMap::from([
            (16u32, "Animation".to_string()),
            (10759, "Action & Adventure".to_string()),
            (10765, "Sci-Fi & Fantasy".to_string()),
        ]);
        let want = |s: &str| with_genres_ids(&[genre_fold(s)], &names);
        assert_eq!(want("action-adventure"), vec![10759]);
        assert_eq!(want("Action & Adventure"), vec![10759]);
        assert_eq!(want("scififantasy"), vec![10765]);
        assert_eq!(want("animation"), vec![16]);
        assert!(want("horror").is_empty(), "unknown slug resolves to nothing");
    }

    /// The fold must stay identical to the one both re-validating tiers use, or
    /// a row's own results get dropped downstream.
    #[test]
    fn genre_fold_matches_the_consumer_side_rule() {
        assert_eq!(genre_fold("Action & Adventure"), "actionadventure");
        assert_eq!(genre_fold("action-adventure"), "actionadventure");
        assert_eq!(genre_fold("Sci-Fi & Fantasy"), "scififantasy");
        assert_eq!(genre_fold("Science Fiction"), "sciencefiction");
        assert_eq!(genre_fold("  "), "");
    }

    /// The adult-content gate on anime rows, and *why* it is two filters.
    ///
    /// `include_adult=false` is not enough on its own — measured live, every
    /// offending title returns `adult: false` (TMDB reserves that flag for
    /// pornography), so the keyword exclusion carries the real load and the vote
    /// floor sweeps up the erotic-tagged residue behind it. Dropping either one
    /// puts hentai back on the home page.
    #[test]
    fn anime_rows_gate_adult_content_three_ways() {
        for mode in [
            DiscoveryMode::Popular,
            DiscoveryMode::Trending,
            DiscoveryMode::TopRated,
        ] {
            for kind in [TmdbKind::Tv, TmdbKind::Movie] {
                let p = build_path_and_query(mode, kind, true, &[], 1);
                assert!(p.contains("include_adult=false"), "{p}");
                assert!(
                    p.contains(&format!("without_keywords={DISCOVERY_EXCLUDED_KEYWORDS}")),
                    "{p}"
                );
                assert!(p.contains("vote_count.gte="), "{p}");
            }
        }
    }

    fn genre_table() -> HashMap<u32, String> {
        HashMap::from([(16, "Animation".to_string()), (18, "Drama".to_string())])
    }

    fn hit() -> TmdbHit {
        TmdbHit {
            tmdbid: 95479,
            title: "Frieren: Beyond Journey's End".to_string(),
            original_title: Some("葬送のフリーレン".to_string()),
            original_language: Some("ja".to_string()),
            overview: Some("A mage reflects on her long life.".to_string()),
            year: Some(2023),
            poster_path: Some("/poster.jpg".to_string()),
            genre_ids: vec![16],
            alt_titles: Vec::new(),
        }
    }

    /// A list hit must carry everything a renderable card needs, so the branch
    /// costs one TMDB call per page and no by-id follow-ups.
    #[test]
    fn a_list_hit_becomes_a_complete_card() {
        let c = card_from_hit(&hit(), TmdbKind::Tv, &genre_table());
        assert_eq!(c.tmdbid, Some(95479));
        assert!(c.overview.is_some(), "overview is what to_record requires");
        assert!(c.poster_path.is_some(), "poster is what to_record requires");
        assert_eq!(c.year, Some(2023));
        assert_eq!(c.content_kind(), "series");
        // Genres come from the id table — the category axis a card used to lack.
        assert_eq!(c.genres, vec!["Animation".to_string()]);
        // The id bag beyond tmdbid needs external_ids — deliberately absent.
        assert_eq!(c.tvdb_id, None);
        assert_eq!(c.imdb_id, None);
    }

    /// The locator CID is derived from `record_id`, so a discovery card and a
    /// by-id card for the same work must address identically or the two tiers
    /// would publish two records for one work.
    #[test]
    fn record_id_matches_the_by_id_path() {
        assert_eq!(card_from_hit(&hit(), TmdbKind::Tv, &genre_table()).record_id(), "tmdb:tv:95479");
        assert_eq!(
            card_from_hit(&hit(), TmdbKind::Movie, &genre_table()).record_id(),
            "tmdb:movie:95479"
        );
    }

    /// The whole point of the branch: `contentKind` on the emitted record is the
    /// *card's* kind, never an echo of the query filter. A row asking
    /// `contentKind:episode` must still produce `series` here — which is exactly
    /// why a client's row query has to ask for `series` (the mismatch is then
    /// caught by `record_matches` downstream rather than silently mislabelling).
    /// **The invariant this whole branch rests on.** The mode/anime markers are
    /// stamped onto the record by `Card::to_record`'s trailing filter echo, not
    /// by anything here — and both the gateway dispatcher and meta-search
    /// re-apply the query's filters to every record, where a *missing* field
    /// fails the match. So if that echo is ever "tidied up", every discovery
    /// card is silently dropped in transit and the wall goes blank with no
    /// error. This test fails that day.
    #[test]
    fn a_discovery_card_survives_the_consumer_re_filter() {
        let q = query(&[
            ("popular", "true"),
            ("contentKind", "series"),
            ("fileType", "card"),
            ("anime", "true"),
        ]);
        let card = card_from_hit(&hit(), TmdbKind::Tv, &genre_table());
        let tmdb = crate::tmdb_client::TmdbClient::new("token".to_string());
        let rec = card.to_record(&tmdb, &q.filters).expect("displayable card emits");

        assert_eq!(rec.fields.get("popular").map(String::as_str), Some("true"));
        assert_eq!(rec.fields.get("anime").map(String::as_str), Some("true"));
        assert_eq!(rec.fields.get("fileType").map(String::as_str), Some("card"));
        assert_eq!(
            rec.fields.get("contentKind").map(String::as_str),
            Some("series"),
            "the card's own kind must win over the query filter"
        );
        assert!(rec.fields.contains_key("description/eng"));
        assert!(rec.fields.contains_key("poster_url"));
        // Genres ride as a key-set, never the legacy comma-joined value
        // (METADATA_KEYS §14.12 — new writers must not add csv-set fields).
        assert_eq!(rec.fields.get("genres/Animation").map(String::as_str), Some("true"));
        assert!(!rec.fields.contains_key("genres"), "must not write the legacy csv-set");
        // The thin id bag: resolved lazily on the click-time by-id path.
        assert!(!rec.fields.contains_key("tvdbid"));
        assert!(!rec.fields.contains_key("imdbid"));
        assert!(!rec.fields.contains_key("seasonCount"));

        assert!(
            meta_feeder_sdk::query_eval::record_matches(&rec.fields, &q),
            "a discovery card must survive the same filter re-application the \
             dispatcher and meta-search perform"
        );
    }

    /// A hit TMDB has not written up yet is declined at projection — which is
    /// why the page walk counts displayable cards, not raw hits.
    #[test]
    fn an_incomplete_hit_is_not_displayable() {
        let mut no_overview = hit();
        no_overview.overview = None;
        assert!(!card_from_hit(&no_overview, TmdbKind::Tv, &genre_table()).is_displayable());

        let mut no_poster = hit();
        no_poster.poster_path = None;
        assert!(!card_from_hit(&no_poster, TmdbKind::Tv, &genre_table()).is_displayable());

        assert!(card_from_hit(&hit(), TmdbKind::Tv, &genre_table()).is_displayable());
    }

    #[test]
    fn content_kind_comes_from_the_card_not_the_filter() {
        let tv = card_from_hit(&hit(), TmdbKind::Tv, &genre_table());
        assert_eq!(tv.content_kind(), "series");
        let movie = card_from_hit(&hit(), TmdbKind::Movie, &genre_table());
        assert_eq!(movie.content_kind(), "movie");
    }
}
