//! TMDB HTTP client + DTOs + season/episode bounds check.
//!
//! **Verbatim lift** of the indexer feeder's `tmdb.rs` (which was itself split
//! out of the monolithic `torznab.rs`). Kept byte-compatible with that copy on
//! purpose: TMDB's API handling is fiddly — 429/`Retry-After` semantics, the
//! self-healing cache-shape checks, the AKA append — and a fix on either side
//! should port across as a straight copy. The redb cache table names match for
//! the same reason, so a warm cache is shared rather than re-earned.
//!
//! Consequence: this file carries surface the **card feeder does not use**.
//! `search` / `discovery_list` (fuzzy title search, home rows),
//! `season_episode_bounds` / `episode_in_season` (release season/episode
//! reconciliation) and the language helpers all belong to *retrieval*, which
//! stays in the indexer feeder. They are dead here by design, not by neglect —
//! hence the module-wide allow rather than a trim that would fork the file.
#![allow(dead_code)]

use meta_feeder_sdk::common::urlencode;
use std::time::Duration;

use tracing::debug;

// -- TMDB metadata enrichment ------------------------------------------------
//
// TMDB v4 bearer-token client. Two endpoints used:
//
//   GET /3/search/{movie,tv}?query=<urlencoded>&include_adult=false[&year=<YYYY>]
//   GET <image_base>/<poster_path>
//
// The token-bearing `Authorization` header gates both. Free-tier rate
// limit is ~40 req / 10 s — we don't hit it from a dev box but it's
// worth knowing. All TMDB failures degrade silently: enrichment is a
// best-effort augmentation, never a hard dep.

/// Lightweight TMDB v4 client. Cheap to clone (the underlying
/// `reqwest::Client` is `Arc`-backed).
#[derive(Clone)]
pub struct TmdbClient {
    http: reqwest::Client,
    bearer_token: String,
    api_base: String,
    image_base: String,
}

impl TmdbClient {
    pub fn new(bearer_token: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(TMDB_POSTER_TIMEOUT_SECS))
            .user_agent(USER_AGENT)
            .build()
            .expect("rustls reqwest client build infallible");
        Self {
            http,
            bearer_token,
            api_base: TMDB_API_BASE.to_string(),
            image_base: TMDB_IMAGE_BASE.to_string(),
        }
    }

    /// **The one deviation from the verbatim lift**: the indexer feeder gates
    /// this on `#[cfg(test)]`. The card feeder's contract test is an
    /// *integration* test, which compiles the lib without `cfg(test)`, so it
    /// needs a real constructor to point the client at a wiremock TMDB.
    pub fn with_bases(token: String, api_base: String, image_base: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(TMDB_POSTER_TIMEOUT_SECS))
            .user_agent(USER_AGENT)
            .build()
            .expect("rustls reqwest client build infallible");
        Self {
            http,
            bearer_token: token,
            api_base,
            image_base,
        }
    }

    /// Search TMDB for a movie/TV title. Returns [`TmdbCall::Hit`] with the
    /// top result, [`TmdbCall::Miss`] when there are no matches (or any
    /// transient/timeout error — best-effort degrade), or
    /// [`TmdbCall::RateLimited`] carrying the Retry-After window on a 429 so
    /// the caller can pause the shared budget globally. Wraps the whole call
    /// (including JSON decode) in [`TMDB_SEARCH_TIMEOUT_SECS`].
    pub(crate) async fn search(
        &self,
        kind: TmdbKind,
        title: &str,
        year: Option<u16>,
    ) -> TmdbCall<TmdbHit> {
        let endpoint = match kind {
            TmdbKind::Movie => "search/movie",
            TmdbKind::Tv => "search/tv",
        };
        let mut url = format!(
            "{}/{}?query={}&include_adult=false",
            self.api_base.trim_end_matches('/'),
            endpoint,
            urlencode(title),
        );
        if let Some(y) = year {
            // Movie uses `year`, TV uses `first_air_date_year`; both
            // are documented but movies accept `year` for backward-
            // compat which is what we want.
            url.push_str(&format!("&year={y}"));
        }
        let fut = self
            .http
            .get(&url)
            .bearer_auth(&self.bearer_token)
            .header("accept", "application/json")
            .send();
        let resp =
            match tokio::time::timeout(Duration::from_secs(TMDB_SEARCH_TIMEOUT_SECS), fut).await {
                Ok(Ok(r)) => r,
                _ => return TmdbCall::Miss,
            };
        if resp.status().as_u16() == 429 {
            return TmdbCall::RateLimited(parse_tmdb_retry_after(&resp));
        }
        if !resp.status().is_success() {
            debug!(
                target: "meta-share::gateway",
                upstream = "prowlarr",
                tmdb_url = %url,
                status = %resp.status(),
                "tmdb search non-2xx; degrading"
            );
            return TmdbCall::Miss;
        }
        let body: TmdbSearchResponse =
            match tokio::time::timeout(Duration::from_secs(TMDB_SEARCH_TIMEOUT_SECS), resp.json())
                .await
            {
                Ok(Ok(b)) => b,
                _ => return TmdbCall::Miss,
            };
        match body.results.into_iter().next() {
            Some(top) => TmdbCall::Hit(top.into_hit(kind)),
            None => TmdbCall::Miss,
        }
    }

    /// Build the public TMDB poster CDN URL for `poster_path` (no fetch). In the
    /// feeder model the feeder no longer fetches+stores the poster bytes; it
    /// emits this URL as a `poster_url` field and the **gateway core** seeds it
    /// into a content-addressed `poster` cid (same path as giphy/wikicommons
    /// previews). Inserts the required [`TMDB_POSTER_SIZE`] segment.
    pub(crate) fn poster_cdn_url(&self, poster_path: &str) -> String {
        format!(
            "{}/{}/{}",
            self.image_base.trim_end_matches('/'),
            TMDB_POSTER_SIZE,
            poster_path.trim_start_matches('/'),
        )
    }

    /// Fetch a poster image's raw bytes. `poster_path` is the
    /// TMDB-supplied path (typically `"/abcdef.jpg"` with leading slash).
    ///
    /// `image_base` is the bare CDN root (`https://image.tmdb.org/t/p`); TMDB
    /// requires a **size segment** (`/w500`, `/original`, …) between the root
    /// and the file or it 404s. The const comment documents this contract and
    /// this caller honors it by inserting [`TMDB_POSTER_SIZE`].
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) async fn fetch_poster(&self, poster_path: &str) -> Option<bytes::Bytes> {
        let url = format!(
            "{}/{}/{}",
            self.image_base.trim_end_matches('/'),
            TMDB_POSTER_SIZE,
            poster_path.trim_start_matches('/'),
        );
        let resp = self.http.get(&url).send().await.ok()?;
        if !resp.status().is_success() {
            debug!(
                target: "meta-share::gateway",
                upstream = "prowlarr",
                poster_url = %url,
                status = %resp.status(),
                "tmdb poster fetch non-2xx; skipping"
            );
            return None;
        }
        resp.bytes().await.ok()
    }

    /// Fetch a TV show's authoritative structure (`number_of_seasons`
    /// plus the per-season `episode_count`s) via `GET /3/tv/{id}`. Used
    /// only to bounds-check a title-parsed season/episode — see
    /// [`season_episode_bounds`]. Wrapped in the same search timeout;
    /// any failure returns `None` so validation degrades to "accept".
    pub(crate) async fn tv_details(&self, tmdbid: u64) -> TmdbCall<TmdbTvDetails> {
        let url = format!(
            "{}/tv/{}?append_to_response=alternative_titles,images",
            self.api_base.trim_end_matches('/'),
            tmdbid,
        );
        let fut = self
            .http
            .get(&url)
            .bearer_auth(&self.bearer_token)
            .header("accept", "application/json")
            .send();
        let resp =
            match tokio::time::timeout(Duration::from_secs(TMDB_SEARCH_TIMEOUT_SECS), fut).await {
                Ok(Ok(r)) => r,
                _ => return TmdbCall::Miss,
            };
        if resp.status().as_u16() == 429 {
            return TmdbCall::RateLimited(parse_tmdb_retry_after(&resp));
        }
        if !resp.status().is_success() {
            debug!(
                target: "meta-share::gateway",
                upstream = "prowlarr",
                tmdb_url = %url,
                status = %resp.status(),
                "tmdb tv-details non-2xx; skipping season/episode validation"
            );
            return TmdbCall::Miss;
        }
        match tokio::time::timeout(Duration::from_secs(TMDB_SEARCH_TIMEOUT_SECS), resp.json()).await
        {
            Ok(Ok(d)) => TmdbCall::Hit(d),
            _ => TmdbCall::Miss,
        }
    }

    /// Fetch cross-database ids (`tvdb_id`, `imdb_id`) for a known tmdbid via
    /// `GET /3/{tv,movie}/{id}/external_ids`. The anchored torznab path needs
    /// `tvdb_id` to query indexers by `tvsearch&tvdbid=` (TMDB's id is not a
    /// standard `tvsearch` param). Same timeout/429 contract as [`tv_details`].
    pub(crate) async fn external_ids(
        &self,
        kind: TmdbKind,
        tmdbid: u64,
    ) -> TmdbCall<TmdbExternalIds> {
        let seg = match kind {
            TmdbKind::Movie => "movie",
            TmdbKind::Tv => "tv",
        };
        let url = format!(
            "{}/{}/{}/external_ids",
            self.api_base.trim_end_matches('/'),
            seg,
            tmdbid,
        );
        self.get_json(&url, "external_ids").await
    }

    /// Fetch a movie's canonical details via `GET /3/movie/{id}` (the anchored
    /// enrichment source for a known movie tmdbid — resolves title/overview/
    /// poster/year/imdb_id without a fuzzy title search). Same contract as
    /// [`tv_details`].
    pub(crate) async fn movie_details(&self, tmdbid: u64) -> TmdbCall<TmdbMovieDetails> {
        let url = format!(
            "{}/movie/{}?append_to_response=alternative_titles,images",
            self.api_base.trim_end_matches('/'),
            tmdbid
        );
        self.get_json(&url, "movie-details").await
    }

    /// Principal search via `GET /3/search/multi` — maps a bare keyword to a
    /// canonical tmdbid + media type so the gateway can then query indexers
    /// structurally (the "TMDB as the front door" identification step
    /// Sonarr/Radarr use). Returns the mixed movie/tv/person result list; the
    /// caller picks the top-N confident anchors via [`principal_top_n`].
    pub(crate) async fn search_multi(&self, query: &str) -> TmdbCall<Vec<TmdbMultiItem>> {
        let url = format!(
            "{}/search/multi?query={}&include_adult=false",
            self.api_base.trim_end_matches('/'),
            urlencode(query),
        );
        match self
            .get_json::<TmdbMultiResponse>(&url, "search/multi")
            .await
        {
            TmdbCall::Hit(r) => TmdbCall::Hit(r.results),
            TmdbCall::Miss => TmdbCall::Miss,
            TmdbCall::RateLimited(d) => TmdbCall::RateLimited(d),
        }
    }

    /// Fetch a TMDB **discovery list** (trending / popular / top-rated / a
    /// `/discover` query) and map each result to a [`TmdbHit`]. Powers the
    /// keyword-less discovery branch (see [`super::discovery`]): the caller
    /// composes `path_and_query` (e.g. `"movie/popular"`,
    /// `"trending/tv/week"`, or `"discover/tv?with_keywords=210024&sort_by=…"`)
    /// and passes the `kind` so movie-vs-TV title/date fields decode correctly.
    /// All these endpoints share the `{results: [TmdbSearchItem]}` shape, so one
    /// method covers them. Returns the page's hits, or `Miss`/`RateLimited` under
    /// the usual best-effort contract.
    pub(crate) async fn discovery_list(
        &self,
        kind: TmdbKind,
        path_and_query: &str,
    ) -> TmdbCall<Vec<TmdbHit>> {
        let url = format!(
            "{}/{}",
            self.api_base.trim_end_matches('/'),
            path_and_query.trim_start_matches('/'),
        );
        match self.get_json::<TmdbSearchResponse>(&url, "discovery").await {
            TmdbCall::Hit(r) => {
                TmdbCall::Hit(r.results.into_iter().map(|i| i.into_hit(kind)).collect())
            }
            TmdbCall::Miss => TmdbCall::Miss,
            TmdbCall::RateLimited(d) => TmdbCall::RateLimited(d),
        }
    }

    /// `GET /3/genre/{tv,movie}/list` — the id → name table for the `genre_ids`
    /// a discovery/search list hit carries.
    ///
    /// TMDB returns genres as bare ids on list responses and as `{id, name}`
    /// objects only on a details fetch, so the *browse* path has no way to name
    /// a genre without this table. It is tiny (16 TV / 19 movie entries) and
    /// effectively static, which is why the caller holds it for the process
    /// lifetime instead of paying it per row.
    pub(crate) async fn genre_list(&self, kind: TmdbKind) -> TmdbCall<Vec<TmdbGenre>> {
        let seg = match kind {
            TmdbKind::Movie => "movie",
            TmdbKind::Tv => "tv",
        };
        let url = format!("{}/genre/{seg}/list", self.api_base.trim_end_matches('/'));
        match self.get_json::<TmdbGenreList>(&url, "genre_list").await {
            TmdbCall::Hit(r) => TmdbCall::Hit(r.genres),
            TmdbCall::Miss => TmdbCall::Miss,
            TmdbCall::RateLimited(d) => TmdbCall::RateLimited(d),
        }
    }

    /// Shared `GET <url>` → JSON helper with the standard bearer auth, search
    /// timeout, 429→`RateLimited`, non-2xx/transient→`Miss` contract. Factored
    /// out of [`tv_details`]/[`external_ids`]/[`movie_details`] (identical
    /// modulo the decoded type and the log label).
    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str, what: &str) -> TmdbCall<T> {
        let fut = self
            .http
            .get(url)
            .bearer_auth(&self.bearer_token)
            .header("accept", "application/json")
            .send();
        let resp =
            match tokio::time::timeout(Duration::from_secs(TMDB_SEARCH_TIMEOUT_SECS), fut).await {
                Ok(Ok(r)) => r,
                // Transport error / timeout. Logged at debug so the otherwise
                // silent best-effort degrade is diagnosable.
                Ok(Err(e)) => {
                    debug!(target: "meta-share::gateway", upstream="prowlarr", tmdb_url=%url, what, error=%e, "tmdb call send error; degrading");
                    return TmdbCall::Miss;
                }
                Err(_) => {
                    debug!(target: "meta-share::gateway", upstream="prowlarr", tmdb_url=%url, what, "tmdb call send timeout; degrading");
                    return TmdbCall::Miss;
                }
            };
        if resp.status().as_u16() == 429 {
            return TmdbCall::RateLimited(parse_tmdb_retry_after(&resp));
        }
        if !resp.status().is_success() {
            debug!(
                target: "meta-share::gateway",
                upstream = "prowlarr",
                tmdb_url = %url,
                status = %resp.status(),
                what,
                "tmdb call non-2xx; degrading"
            );
            return TmdbCall::Miss;
        }
        match tokio::time::timeout(Duration::from_secs(TMDB_SEARCH_TIMEOUT_SECS), resp.json()).await {
            Ok(Ok(d)) => TmdbCall::Hit(d),
            // A decode failure here folds into `Miss` (best-effort), but log it
            // at debug: a single malformed element fails the whole `Vec` decode,
            // which would otherwise silently disable e.g. multi-anchor.
            Ok(Err(e)) => {
                debug!(target: "meta-share::gateway", upstream="prowlarr", tmdb_url=%url, what, error=%e, "tmdb call decode error; degrading");
                TmdbCall::Miss
            }
            Err(_) => {
                debug!(target: "meta-share::gateway", upstream="prowlarr", tmdb_url=%url, what, "tmdb call decode timeout; degrading");
                TmdbCall::Miss
            }
        }
    }
}

/// Outcome of a TMDB API call. Distinguishes a 429 rate-limit (which feeds
/// `Retry-After` into the shared [`TmdbBudget`] so all enrichment pauses
/// globally) from a plain miss or transient error (best-effort degrade).
pub(crate) enum TmdbCall<T> {
    Hit(T),
    Miss,
    RateLimited(Duration),
}

/// Parse a TMDB `Retry-After` header into a pause duration, defaulting to
/// [`TMDB_RATE_LIMIT_DEFAULT_RETRY_SECS`] when absent/unparseable. Mirrors
/// the indexer-path logic in [`map_status`].
pub(crate) fn parse_tmdb_retry_after(resp: &reqwest::Response) -> Duration {
    let secs = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(TMDB_RATE_LIMIT_DEFAULT_RETRY_SECS);
    Duration::from_secs(secs)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TmdbKind {
    Movie,
    Tv,
}

/// One `{id, name}` genre entry, as returned by `genre/{kind}/list` and by the
/// details endpoints' `genres` array.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub(crate) struct TmdbGenre {
    pub(crate) id: u32,
    #[serde(default)]
    pub(crate) name: String,
}

#[derive(serde::Deserialize)]
struct TmdbGenreList {
    #[serde(default)]
    genres: Vec<TmdbGenre>,
}

#[derive(serde::Deserialize)]
pub(crate) struct TmdbSearchResponse {
    #[serde(default)]
    pub(crate) results: Vec<TmdbSearchItem>,
}

#[derive(serde::Deserialize)]
pub(crate) struct TmdbSearchItem {
    pub(crate) id: u64,
    #[serde(default)]
    pub(crate) title: Option<String>, // movie
    #[serde(default)]
    pub(crate) name: Option<String>, // tv
    #[serde(default)]
    pub(crate) original_title: Option<String>, // movie
    #[serde(default)]
    pub(crate) original_name: Option<String>, // tv
    #[serde(default)]
    pub(crate) overview: Option<String>,
    #[serde(default)]
    pub(crate) release_date: Option<String>, // movie, YYYY-MM-DD
    #[serde(default)]
    pub(crate) first_air_date: Option<String>, // tv,    YYYY-MM-DD
    #[serde(default)]
    pub(crate) poster_path: Option<String>,
    #[serde(default)]
    pub(crate) genre_ids: Vec<u32>,
    /// ISO 639-1 (2-letter) language the title was originally produced in.
    /// Mapped to `lang3` and used to file `original_title` as a
    /// `titles/{lang3}/{name}` member (METADATA_KEYS.md §3).
    #[serde(default)]
    pub(crate) original_language: Option<String>,
}

impl TmdbSearchItem {
    pub(crate) fn into_hit(self, kind: TmdbKind) -> TmdbHit {
        let title = match kind {
            TmdbKind::Movie => self.title,
            TmdbKind::Tv => self.name,
        }
        .unwrap_or_else(|| "(untitled)".to_string());
        let original_title = match kind {
            TmdbKind::Movie => self.original_title,
            TmdbKind::Tv => self.original_name,
        };
        let date = match kind {
            TmdbKind::Movie => self.release_date,
            TmdbKind::Tv => self.first_air_date,
        };
        let year = date
            .as_deref()
            .filter(|d| d.len() >= 4)
            .and_then(|d| d[..4].parse::<u16>().ok());
        TmdbHit {
            tmdbid: self.id,
            title,
            original_title,
            original_language: self.original_language,
            overview: self.overview,
            year,
            poster_path: self.poster_path,
            genre_ids: self.genre_ids,
            // search-list items carry no AKAs (a details/append fetch does)
            alt_titles: Vec::new(),
            akas: Vec::new(),
            posters: Vec::new(),
        }
    }
}

/// Map an ISO 639-1 (2-letter) code to its ISO 639-3 (`lang3`) equivalent,
/// matching the store's convention (`eng`, `jpn`, `fra`; the 639-2/T variant
/// where B/T differ, e.g. `deu` not `ger`). Covers the languages TMDB
/// commonly returns; unknown codes return `None` so the caller files the name under `und`
/// rather than persisting a non-`lang3` key.
pub(crate) fn iso639_1_to_3(code: &str) -> Option<&'static str> {
    Some(match code.to_ascii_lowercase().as_str() {
        "en" => "eng",
        "ja" => "jpn",
        "fr" => "fra",
        "de" => "deu",
        "es" => "spa",
        "it" => "ita",
        "ru" => "rus",
        "ko" => "kor",
        "zh" => "zho",
        "pt" => "por",
        "nl" => "nld",
        "sv" => "swe",
        "no" => "nor",
        "da" => "dan",
        "fi" => "fin",
        "pl" => "pol",
        "tr" => "tur",
        "ar" => "ara",
        "hi" => "hin",
        "th" => "tha",
        "vi" => "vie",
        "id" => "ind",
        "cs" => "ces",
        "el" => "ell",
        "he" => "heb",
        "hu" => "hun",
        "ro" => "ron",
        "uk" => "ukr",
        "fa" => "fas",
        "ms" => "msa",
        "tl" => "tgl",
        "ca" => "cat",
        "nb" => "nob",
        _ => return None,
    })
}

/// The language TMDB's `name`/`title` is written in. Every request goes out
/// without a `language` param, so TMDB answers in its default, en-US.
pub(crate) const METADATA_LANG3: &str = "eng";

/// A title as a `titles/{lang3}/{name}` member name (METADATA_KEYS.md): trimmed,
/// internal whitespace collapsed to one space, case/diacritics/`%` verbatim.
/// The `/` → `∕` mapping is the key's job ([`title_member_key`]), not the name's.
pub(crate) fn normalize_title_name(name: &str) -> String {
    name.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The `titles/{lang3}/{name}` key-set member for `name`, or `None` when the
/// name normalises to empty. `/` is the key-set separator, so a `/` inside a
/// name (`Fate/Zero`) is written as U+2215 `∕` (METADATA_KEYS.md).
///
/// CROSS-BINARY CONTRACT: the indexer feeder's `tmdb.rs::title_member_key`
/// writes the same shape; two peers must produce the same key for one name.
pub(crate) fn title_member_key(lang3: &str, name: &str) -> Option<String> {
    let name = normalize_title_name(name);
    if name.is_empty() {
        return None;
    }
    Some(format!("titles/{lang3}/{}", name.replace('/', "\u{2215}")))
}

/// The language a TMDB AKA market (`iso_3166_1`) clearly implies, or `und` for
/// a multilingual or unknown market (METADATA_KEYS.md `titles/{lang3}/{name}`:
/// map what is unambiguous, never guess). `CA`, `CH`, `BE`, `IN`, `HK`, `SG`,
/// empty, … are `und` on purpose.
pub(crate) fn market_lang3(market: &str) -> &'static str {
    match market.trim().to_ascii_uppercase().as_str() {
        "US" | "GB" | "AU" | "NZ" | "IE" => "eng",
        "FR" => "fra",
        "DE" | "AT" => "deu",
        "ES" | "MX" | "AR" | "CO" | "CL" | "PE" | "VE" | "UY" => "spa",
        "BR" | "PT" => "por",
        "IT" => "ita",
        "JP" => "jpn",
        "KR" => "kor",
        "CN" | "TW" => "zho",
        "RU" => "rus",
        "UA" => "ukr",
        "PL" => "pol",
        "NL" => "nld",
        "SE" => "swe",
        "NO" => "nor",
        "DK" => "dan",
        "FI" => "fin",
        "TR" => "tur",
        "GR" => "ell",
        "HU" => "hun",
        "CZ" => "ces",
        "SK" => "slk",
        "RO" => "ron",
        "BG" => "bul",
        "RS" => "srp",
        "HR" => "hrv",
        "IL" => "heb",
        "IR" => "fas",
        "TH" => "tha",
        "VN" => "vie",
        "ID" => "ind",
        _ => "und",
    }
}

/// Every name of a work as `(lang3, name)` — what the card files under
/// `titles/{lang3}/{name}`:
///
/// - `original` under its `original_language` (`und` when unmapped/absent);
/// - `title` under [`METADATA_LANG3`], **unless it equals the original** — TMDB
///   falls back to the original name when it has no en-US translation, so `3%`
///   is filed once, as `titles/por/3%`, not also as an English name;
/// - every AKA under the language its market implies ([`market_lang3`]).
///
/// Names are normalised ([`normalize_title_name`]); blanks, TMDB's
/// `(untitled)` placeholder and exact `(lang3, name)` repeats are dropped.
pub(crate) fn title_names(
    title: &str,
    original: Option<&str>,
    original_language: Option<&str>,
    akas: &[TmdbAltTitle],
) -> Vec<(&'static str, String)> {
    let mut out: Vec<(&'static str, String)> = Vec::new();
    let mut push = |lang3: &'static str, name: &str| {
        let name = normalize_title_name(name);
        if !name.is_empty() && !out.iter().any(|(l, n)| *l == lang3 && *n == name) {
            out.push((lang3, name));
        }
    };
    let original = original.map(normalize_title_name).filter(|o| !o.is_empty());
    if let Some(o) = &original {
        let lang3 = original_language
            .and_then(|l| iso639_1_to_3(l.trim()))
            .unwrap_or("und");
        push(lang3, o);
    }
    let title = normalize_title_name(title);
    if title != "(untitled)" && original.as_deref() != Some(title.as_str()) {
        push(METADATA_LANG3, &title);
    }
    for aka in akas {
        push(market_lang3(&aka.iso_3166_1), &aka.title);
    }
    out
}

// `Clone` is required so the single-flight `Shared` future (whose `Output`
// must be `Clone`) can hand the same hit to every coalesced caller; serde so
// hits persist in the redb TMDB-search cache.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct TmdbHit {
    pub(crate) tmdbid: u64,
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) original_title: Option<String>,
    #[serde(default)]
    pub(crate) original_language: Option<String>,
    #[serde(default)]
    pub(crate) overview: Option<String>,
    #[serde(default)]
    pub(crate) year: Option<u16>,
    #[serde(default)]
    pub(crate) poster_path: Option<String>,
    #[serde(default)]
    #[allow(dead_code)] // surfaced as CSV of ids; name resolution would need a second call
    pub(crate) genre_ids: Vec<u32>,
    /// TMDB alternative / AKA titles (romaji + foreign), used by the id-based-job
    /// relevance guard so a differently-titled release still resembles the show.
    /// Empty for hits built from a search list (which carries no AKAs).
    #[serde(default)]
    pub(crate) alt_titles: Vec<String>,
    /// The same AKAs **with their market**, which the card needs to file each
    /// under `titles/{lang3}/{name}` ([`title_names`]). Empty on a movie-details
    /// cache entry written before it existed — `Resolver::movie_hit` refetches
    /// those once.
    #[serde(default)]
    pub(crate) akas: Vec<TmdbAltTitle>,
    /// Poster candidates from the details `images` append, filed by the card as
    /// `posters/{lang3}/{cid}` (METADATA_KEYS.md §6). Empty for list hits and for
    /// a movie-details cache entry written before it existed — deliberately NOT a
    /// self-heal trigger in `Resolver::movie_hit`: no backfill, no TMDB budget.
    #[serde(default)]
    pub(crate) posters: Vec<TmdbImage>,
}

impl TmdbHit {
    /// TMDB `original_language` (ISO 639-1) mapped to the store's `lang3`
    /// (ISO 639-3), or `None` when the language is outside the common set or
    /// absent. Used to file `original_title` under `titles/{lang3}/{name}` (§3).
    pub(crate) fn original_lang3(&self) -> Option<&'static str> {
        iso639_1_to_3(self.original_language.as_deref()?.trim())
    }
}

/// One poster from a details payload's `images` append — a candidate for the
/// card's `posters/{lang3}/{cid}` key-set ([`crate::card::poster_member_keys`]).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct TmdbImage {
    #[serde(default)]
    pub(crate) file_path: String,
    /// Language of the text printed on the poster (ISO 639-1); `None` = textless.
    #[serde(default)]
    pub(crate) iso_639_1: Option<String>,
    #[serde(default)]
    pub(crate) vote_average: f64,
    #[serde(default)]
    pub(crate) vote_count: u32,
}

/// The `images` object `?append_to_response=images` adds to `GET /3/{tv,movie}/{id}`.
/// Only `posters` is decoded. No `language` param is sent, so TMDB returns the
/// posters of every language.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct TmdbImages {
    #[serde(default)]
    pub(crate) posters: Vec<TmdbImage>,
}

/// Authoritative TV structure from `GET /3/tv/{id}`, used to bounds-check
/// a title-parsed season/episode. Only the two structural fields are
/// decoded; everything else in the (large) details payload is ignored.
// `Clone` + `Serialize` added so details persist in the redb TMDB-tvdetails
// cache and can be cheaply handed around.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct TmdbTvDetails {
    #[serde(default)]
    pub(crate) number_of_seasons: u32,
    /// Genre objects (`{id, name}`) — present on a details fetch, unlike the
    /// bare `genre_ids` a list response carries. Free here: same payload.
    #[serde(default)]
    pub(crate) genres: Vec<TmdbGenre>,
    /// One entry per season TMDB knows about, including season 0
    /// ("Specials"). `episode_count` is the released-episode total.
    #[serde(default)]
    pub(crate) seasons: Vec<TmdbSeasonSummary>,
    // -- Display fields ------------------------------------------------------
    // Decoded from the same `GET /3/tv/{id}` payload so an anchored TV record
    // (known tmdbid) enriches directly from its canonical entry instead of a
    // fuzzy title search. Old cache entries (written before these fields
    // existed) deserialize them to empty/None; [`TmdbTvDetails::has_display`]
    // detects that so the cached-fetch path can self-heal with one refetch.
    #[serde(default)]
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) original_name: Option<String>,
    #[serde(default)]
    pub(crate) original_language: Option<String>,
    #[serde(default)]
    pub(crate) overview: Option<String>,
    #[serde(default)]
    pub(crate) first_air_date: Option<String>,
    #[serde(default)]
    pub(crate) poster_path: Option<String>,
    /// AKA titles from `?append_to_response=alternative_titles`. `None` on a
    /// pre-AKA cache entry → [`TmdbTvDetails::has_akas`] is false so the cached
    /// fetch self-heals with one refetch (same pattern as the display fields).
    #[serde(default)]
    pub(crate) alternative_titles: Option<TmdbAltTitles>,
    /// Poster candidates from `?append_to_response=images`. `None` on a cache
    /// entry written before the append — NOT a self-heal trigger (no backfill).
    #[serde(default)]
    pub(crate) images: Option<TmdbImages>,
}

impl TmdbTvDetails {
    /// True when the cached payload carries the display fields (i.e. was
    /// written by the current decoder). A `false` here on a cache hit means a
    /// pre-display entry — the caller refetches once to upgrade it.
    pub(crate) fn has_display(&self) -> bool {
        !self.name.trim().is_empty()
    }

    /// True when the AKA field was populated by the current (append-enabled)
    /// decoder. A `false` on a cache hit means a pre-AKA entry → the cached
    /// fetch refetches once to upgrade it (mirrors [`has_display`]).
    ///
    /// Also `false` for an AKA list decoded before `iso_3166_1` was: the
    /// search-term pick (`card::search_aka`) prefers English-market AKAs, and a
    /// market-less list would silently keep TMDB's order (`3 Pourcent` over
    /// `3 percent`) forever. One refetch per such entry upgrades it.
    pub(crate) fn has_akas(&self) -> bool {
        self.alternative_titles
            .as_ref()
            .is_some_and(TmdbAltTitles::has_markets)
    }

    /// The show's canonical + original + AKA titles, for the id-based-job
    /// relevance guard.
    pub(crate) fn guard_titles(&self) -> Vec<String> {
        let mut out = vec![self.name.clone()];
        if let Some(o) = &self.original_name {
            out.push(o.clone());
        }
        if let Some(a) = &self.alternative_titles {
            out.extend(a.all());
        }
        out
    }

    /// Build a [`TmdbHit`] (resolved by id) from the canonical TV entry, for
    /// the anchored enrichment path. `None` when display fields are absent.
    pub(crate) fn as_hit(&self, tmdbid: u64) -> Option<TmdbHit> {
        if !self.has_display() {
            return None;
        }
        let year = self
            .first_air_date
            .as_deref()
            .filter(|d| d.len() >= 4)
            .and_then(|d| d[..4].parse::<u16>().ok());
        Some(TmdbHit {
            tmdbid,
            title: self.name.clone(),
            original_title: self.original_name.clone(),
            original_language: self.original_language.clone(),
            overview: self.overview.clone(),
            year,
            poster_path: self.poster_path.clone(),
            genre_ids: Vec::new(),
            alt_titles: self
                .alternative_titles
                .as_ref()
                .map(TmdbAltTitles::all)
                .unwrap_or_default(),
            akas: self
                .alternative_titles
                .as_ref()
                .map(TmdbAltTitles::entries)
                .unwrap_or_default(),
            posters: self
                .images
                .as_ref()
                .map(|i| i.posters.clone())
                .unwrap_or_default(),
        })
    }
}

/// Alternative / AKA titles from `?append_to_response=alternative_titles`.
/// TMDB nests them under `results` for `/tv/{id}` and under `titles` for
/// `/movie/{id}`; both entries carry a `title`. Decoded so the id-based-job
/// relevance guard ([`super::torznab::apply_record_tag`]) can match a
/// romaji/foreign release title (e.g. `Sousou no Frieren`) against a show whose
/// canonical `name` is the English one (`Frieren: Beyond Journey's End`).
///
/// `Option<TmdbAltTitles>` on the details structs doubles as a fetched-marker:
/// `None` means "written by a decoder that didn't append AKAs" (an old cache
/// entry) and triggers a one-shot self-heal refetch; `Some` (even empty) means
/// "AKAs were fetched, this show simply has none".
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct TmdbAltTitles {
    #[serde(default)]
    pub(crate) results: Vec<TmdbAltTitle>, // tv
    #[serde(default)]
    pub(crate) titles: Vec<TmdbAltTitle>, // movie
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct TmdbAltTitle {
    #[serde(default)]
    pub(crate) title: String,
    /// The market the AKA is used in (`US`, `FR`, …). Empty on a cache entry
    /// written before it was decoded — ordering then falls back to TMDB's.
    #[serde(default)]
    pub(crate) iso_3166_1: String,
}

impl TmdbAltTitles {
    /// Was this list decoded with markets? TMDB stamps `iso_3166_1` on every
    /// AKA, so a non-empty list where none carries one is a pre-market cache
    /// entry. An empty list is complete ("fetched, no AKAs").
    pub(crate) fn has_markets(&self) -> bool {
        let mut titles = self.results.iter().chain(self.titles.iter()).peekable();
        titles.peek().is_none() || titles.any(|t| !t.iso_3166_1.is_empty())
    }

    /// The non-empty AKA titles across both the TV (`results`) and movie
    /// (`titles`) shapes, **English-market (`US`, `GB`) titles first**, TMDB's
    /// order otherwise. The relevance guard only asks "any of these?", so the
    /// order is free for it; it matters to the search-term AKA pick
    /// (`card::search_aka`), where `3 percent` (US) must beat `3 Pourcent` (FR).
    pub(crate) fn all(&self) -> Vec<String> {
        self.entries().into_iter().map(|t| t.title).collect()
    }

    /// [`Self::all`] with each AKA's market kept — same order, same filtering
    /// (titles trimmed, blanks dropped).
    pub(crate) fn entries(&self) -> Vec<TmdbAltTitle> {
        let mut titles: Vec<&TmdbAltTitle> = self.results.iter().chain(self.titles.iter()).collect();
        titles.sort_by_key(|t| !matches!(t.iso_3166_1.as_str(), "US" | "GB"));
        titles
            .into_iter()
            .map(|t| TmdbAltTitle {
                title: t.title.trim().to_string(),
                iso_3166_1: t.iso_3166_1.trim().to_string(),
            })
            .filter(|t| !t.title.is_empty())
            .collect()
    }
}

/// TMDB cross-database ids from `GET /3/{tv,movie}/{id}/external_ids`. Only the
/// two the anchored torznab path needs are decoded.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct TmdbExternalIds {
    #[serde(default)]
    pub(crate) tvdb_id: Option<i64>,
    #[serde(default)]
    pub(crate) imdb_id: Option<String>,
}

/// Authoritative movie structure from `GET /3/movie/{id}`. Decodes only the
/// display fields the anchored path needs; `imdb_id` (movie details carry it
/// inline, no separate `external_ids` call) feeds the `t=movie&imdbid=` query
/// fallback.
#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct TmdbMovieDetails {
    pub(crate) id: u64,
    /// See [`TmdbTvDetails::genres`].
    #[serde(default)]
    pub(crate) genres: Vec<TmdbGenre>,
    #[serde(default)]
    pub(crate) title: Option<String>,
    #[serde(default)]
    pub(crate) original_title: Option<String>,
    #[serde(default)]
    pub(crate) original_language: Option<String>,
    #[serde(default)]
    pub(crate) overview: Option<String>,
    #[serde(default)]
    pub(crate) release_date: Option<String>,
    #[serde(default)]
    pub(crate) poster_path: Option<String>,
    #[serde(default)]
    pub(crate) alternative_titles: Option<TmdbAltTitles>,
    /// See [`TmdbTvDetails::images`].
    #[serde(default)]
    pub(crate) images: Option<TmdbImages>,
}

impl TmdbMovieDetails {
    pub(crate) fn into_hit(self) -> TmdbHit {
        let year = self
            .release_date
            .as_deref()
            .filter(|d| d.len() >= 4)
            .and_then(|d| d[..4].parse::<u16>().ok());
        let alt_titles = self
            .alternative_titles
            .as_ref()
            .map(TmdbAltTitles::all)
            .unwrap_or_default();
        let akas = self
            .alternative_titles
            .as_ref()
            .map(TmdbAltTitles::entries)
            .unwrap_or_default();
        let posters = self.images.map(|i| i.posters).unwrap_or_default();
        TmdbHit {
            tmdbid: self.id,
            title: self.title.unwrap_or_else(|| "(untitled)".to_string()),
            original_title: self.original_title,
            original_language: self.original_language,
            overview: self.overview,
            year,
            poster_path: self.poster_path,
            genre_ids: Vec::new(),
            alt_titles,
            akas,
            posters,
        }
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct TmdbMultiResponse {
    #[serde(default)]
    pub(crate) results: Vec<TmdbMultiItem>,
}

/// One `search/multi` result. `media_type` discriminates movie / tv / person;
/// we anchor only on movie/tv.
#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct TmdbMultiItem {
    pub(crate) id: u64,
    #[serde(default)]
    pub(crate) media_type: Option<String>,
    #[serde(default)]
    pub(crate) title: Option<String>, // movie
    #[serde(default)]
    pub(crate) name: Option<String>, // tv
    #[serde(default)]
    pub(crate) popularity: f64,
}

impl TmdbMultiItem {
    pub(crate) fn kind(&self) -> Option<TmdbKind> {
        match self.media_type.as_deref() {
            Some("movie") => Some(TmdbKind::Movie),
            Some("tv") => Some(TmdbKind::Tv),
            _ => None,
        }
    }
    pub(crate) fn display_title(&self) -> &str {
        self.title.as_deref().or(self.name.as_deref()).unwrap_or("")
    }
}

/// Lowercase alphanumeric word tokens of `s` (separators dropped). Shared by
/// the principal-search confidence check.
pub(crate) fn norm_tokens(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect()
}

/// Pick up to `n` **confident** anchors from `search/multi` hits for `query`,
/// most-popular first (or `[]` → the caller falls back to today's generic
/// `t=search&q=`). For each movie/tv hit, accept it only if its title shares at
/// least half of the query's word tokens (the per-candidate relevance gate),
/// then rank the survivors by popularity and take the top `n`. This binds a
/// vague keyword like "black" to the *several* real shows TMDB returns (Black
/// Mirror, Black Butler, Black Lagoon, …) instead of hijacking the whole query
/// onto the single most-popular title, while still dropping unrelated noise
/// (e.g. "zzz qqq www" → nothing). `n == 1` recovers the old single-anchor
/// "principal_confident" behaviour (modulo the gate being applied before the
/// popularity pick rather than after).
pub(crate) fn principal_top_n<'a>(
    hits: &'a [TmdbMultiItem],
    query: &str,
    n: usize,
) -> Vec<&'a TmdbMultiItem> {
    let q_tokens = norm_tokens(query);
    if q_tokens.is_empty() || n == 0 {
        return Vec::new();
    }
    let mut cands: Vec<&TmdbMultiItem> = hits
        .iter()
        .filter(|h| h.kind().is_some())
        .filter(|h| {
            let t_tokens = norm_tokens(h.display_title());
            let overlap = q_tokens.iter().filter(|t| t_tokens.contains(t)).count();
            (overlap as f64) >= (q_tokens.len() as f64) * 0.5
        })
        .collect();
    cands.sort_by(|a, b| {
        b.popularity
            .partial_cmp(&a.popularity)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    cands.truncate(n);
    cands
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct TmdbSeasonSummary {
    #[serde(default)]
    pub(crate) season_number: i64,
    #[serde(default)]
    pub(crate) episode_count: u32,
}

/// Verdict of [`season_episode_bounds`] — how a title-parsed `(season,
/// episode)` lines up against TMDB's actual structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SeasonEpisodeBounds {
    /// Consistent with TMDB, or unverifiable — keep the record as-is.
    Ok,
    /// The parsed season sits past TMDB's known seasons and isn't otherwise
    /// listed in `seasons[]`. The canonical case is anime where fansubbers
    /// number a later cour `S2` of a show TMDB models as a **single** season
    /// (Frieren, Jujutsu Kaisen, …). The TMDB match itself is sound — only the
    /// season number is a naming-convention artifact — so the caller keeps the
    /// record but strips the misleading `season` (the frontend buckets
    /// season-less episodes into a flat "Episodes" list).
    SeasonOverflow,
    /// The season IS a real TMDB season, but the parsed episode runs past that
    /// season's `episode_count`. The canonical case is absolute-numbered anime
    /// that an indexer has relabelled onto season 1 (`S01E96` on a 28-episode
    /// show): the `S01` is a numbering artifact, not a trustworthy token, so —
    /// exactly like [`SeasonOverflow`] — the caller strips the season and keeps
    /// the record (season-less episode, bucketed under the show's tmdbid). We
    /// deliberately do NOT drop it: an out-of-bounds episode means the season
    /// was never reliable, not that the whole release is bogus.
    EpisodeOverflow,
    /// A contradiction neither overflow allowance can explain — currently only a
    /// negative season (codec noise like `x.265` → "S0E265" is caught upstream;
    /// a genuinely garbled parse). The title parse is wrong — the caller drops
    /// the record.
    Contradiction,
}

/// Pure bounds check: is a title-parsed `(season, episode)` consistent
/// with what TMDB says the show actually has? Deliberately lenient —
/// it only returns a verdict other than [`SeasonEpisodeBounds::Ok`] on a
/// *positive* divergence, so anything ambiguous or unverifiable is accepted:
///
/// - No parsed season → [`Ok`]. Episode-only titles (absolute-numbered
///   anime, trailing `- 117`) are legitimately unbounded; the season-
///   based bound doesn't apply.
/// - TMDB gave us no structure (`number_of_seasons == 0` and no
///   `seasons[]`) → [`Ok`]; we can't contradict what we don't know.
/// - Season exceeds `number_of_seasons` AND isn't otherwise listed in
///   `seasons[]` → [`SeasonOverflow`] (the `S2`-of-a-1-TMDB-season anime
///   case; keep but strip `season`).
/// - Negative season → [`Contradiction`].
/// - Episode exceeds the matched season's `episode_count` (when that
///   count is known) → [`EpisodeOverflow`]. This fires for explicit `SxxEyy`
///   titles whose episode overruns the season — overwhelmingly absolute-
///   numbered anime relabelled onto season 1 — so we strip the season and keep
///   the record rather than dropping it.
///
/// [`Ok`]: SeasonEpisodeBounds::Ok
/// [`SeasonOverflow`]: SeasonEpisodeBounds::SeasonOverflow
/// [`EpisodeOverflow`]: SeasonEpisodeBounds::EpisodeOverflow
/// [`Contradiction`]: SeasonEpisodeBounds::Contradiction
pub(crate) fn season_episode_bounds(
    details: &TmdbTvDetails,
    season: Option<i64>,
    episode: Option<i64>,
) -> SeasonEpisodeBounds {
    use SeasonEpisodeBounds::*;
    let season = match season {
        Some(s) => s,
        None => return Ok,
    };
    if details.number_of_seasons == 0 && details.seasons.is_empty() {
        return Ok;
    }
    if season < 0 {
        return Contradiction;
    }
    let matched = details.seasons.iter().find(|s| s.season_number == season);
    if season > details.number_of_seasons as i64 && matched.is_none() {
        return SeasonOverflow;
    }
    if let (Some(ep), Some(sm)) = (episode, matched) {
        if sm.episode_count > 0 && ep > sm.episode_count as i64 {
            return EpisodeOverflow;
        }
    }
    Ok
}

/// True when `episode` is a valid episode of `season` per TMDB's per-season
/// `episode_count` (the season must be listed and the count known). Used to
/// decide whether a bare title number is an in-season episode before falling
/// back to absolute-number interpretation.
pub(crate) fn episode_in_season(details: &TmdbTvDetails, season: i64, episode: i64) -> bool {
    details
        .seasons
        .iter()
        .find(|s| s.season_number == season)
        .map(|s| s.episode_count > 0 && episode >= 1 && episode <= s.episode_count as i64)
        .unwrap_or(false)
}

use crate::consts::*;

#[cfg(test)]
mod season_bounds_tests {
    use super::*;

    fn details(num_seasons: u32, seasons: &[(i64, u32)]) -> TmdbTvDetails {
        TmdbTvDetails {
            number_of_seasons: num_seasons,
            seasons: seasons
                .iter()
                .map(|&(season_number, episode_count)| TmdbSeasonSummary {
                    season_number,
                    episode_count,
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn bounds_ok_within_season() {
        let d = details(2, &[(1, 26), (2, 24)]);
        assert_eq!(season_episode_bounds(&d, Some(1), Some(5)), SeasonEpisodeBounds::Ok);
        assert_eq!(season_episode_bounds(&d, Some(2), Some(24)), SeasonEpisodeBounds::Ok);
    }

    #[test]
    fn bounds_no_season_or_unknown_structure_is_ok() {
        let d = details(2, &[(1, 26), (2, 24)]);
        assert_eq!(season_episode_bounds(&d, None, Some(999)), SeasonEpisodeBounds::Ok);
        let unknown = details(0, &[]);
        assert_eq!(season_episode_bounds(&unknown, Some(5), Some(40)), SeasonEpisodeBounds::Ok);
    }

    #[test]
    fn bounds_season_past_total_is_overflow() {
        // Fansub "S2" of a show TMDB models as a single season → strip-and-keep.
        let d = details(1, &[(1, 28)]);
        assert_eq!(season_episode_bounds(&d, Some(2), Some(3)), SeasonEpisodeBounds::SeasonOverflow);
    }

    #[test]
    fn bounds_episode_past_matched_season_is_episode_overflow() {
        // "S1E27" on a show whose season 1 has 26 episodes → the `S01` is an
        // absolute-numbering artifact, not a trustworthy token. The enrich path
        // strips the season and KEEPS the record (season-less absolute episode),
        // rather than dropping it.
        let d = details(4, &[(1, 26), (2, 13), (3, 13), (4, 12)]);
        assert_eq!(season_episode_bounds(&d, Some(1), Some(27)), SeasonEpisodeBounds::EpisodeOverflow);
    }

    #[test]
    fn bounds_negative_season_is_contradiction() {
        // A genuinely garbled parse (negative season) is still a hard drop.
        let d = details(4, &[(1, 26), (2, 13), (3, 13), (4, 12)]);
        assert_eq!(season_episode_bounds(&d, Some(-1), None), SeasonEpisodeBounds::Contradiction);
    }
}
