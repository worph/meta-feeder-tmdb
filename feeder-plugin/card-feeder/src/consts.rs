//! Constants for the card feeder. The TMDB subset of the indexer feeder's
//! `consts.rs` — lifted verbatim so a warm redb cache and the observed live
//! behaviour carry over unchanged — plus the card-tier's own anchor knobs.

/// User-Agent for outbound HTTP from the card feeder.
pub(crate) const USER_AGENT: &str = concat!(
    "meta-card/",
    env!("CARGO_PKG_VERSION"),
    " (gateway:card)"
);

/// TMDB REST API base (v3).
pub(crate) const TMDB_API_BASE: &str = "https://api.themoviedb.org/3";
/// TMDB image CDN base; callers append a size segment (e.g. `/w500/<path>`).
pub(crate) const TMDB_IMAGE_BASE: &str = "https://image.tmdb.org/t/p";
/// TMDB poster size segment inserted between [`TMDB_IMAGE_BASE`] and the
/// poster path. Mandatory — TMDB 404s a path with no size. `w500` is the
/// poster-grid sweet spot (≈122 KB vs `original`'s multi-MB).
pub(crate) const TMDB_POSTER_SIZE: &str = "w500";

/// TMDB search request timeout (seconds).
pub(crate) const TMDB_SEARCH_TIMEOUT_SECS: u64 = 1000;
/// TMDB poster download timeout (seconds). The card feeder never downloads
/// poster bytes (the gateway core seeds them), but the client is shared with
/// the indexer feeder's lift, so the knob stays.
pub(crate) const TMDB_POSTER_TIMEOUT_SECS: u64 = 3000;
/// Default Retry-After (seconds) applied when TMDB returns HTTP 429 with no
/// (or an unparseable) `Retry-After` header.
pub(crate) const TMDB_RATE_LIMIT_DEFAULT_RETRY_SECS: u64 = 5;
/// Maximum time to wait for a TMDB budget permit before giving up (seconds).
pub(crate) const TMDB_WAIT_DEADLINE_SECS: u64 = 3000;

/// Budget-permit deadline for a **catalog page** ([`crate::discovery`]),
/// deliberately far shorter than [`TMDB_WAIT_DEADLINE_SECS`].
///
/// That 3000 s ceiling is sized for a query someone is *waiting on* — a user
/// typed a title and wants that specific answer, so out-waiting a 429 pause
/// beats returning nothing. A browse row is the opposite: nobody asked for
/// *this* row in particular, it refreshes on a timer, and its client keeps the
/// previously-filled row on an empty pass (meta-watch's sticky per-row merge).
/// Inheriting 3000 s would let a single 429 pin a row's search stream open for
/// most of an hour to eventually produce what a 5 s bail produces now.
pub(crate) const TMDB_DISCOVERY_WAIT_DEADLINE_SECS: u64 = 5;

/// Hard ceiling on catalog pages walked for one discovery row, whatever the
/// requested card count. Bounds a misconfigured `card_discovery_n` (or a client
/// asking for 500) to a predictable per-row TMDB cost.
pub(crate) const DISCOVERY_MAX_PAGES: u32 = 5;

/// TMDB keyword ids excluded from an `anime:true` discovery row, as
/// `without_keywords`: `hentai`, `softcore`, `erotica`, `pornography`,
/// `adult animation`.
///
/// **Why this is needed at all.** `include_adult=false` does *not* cover it:
/// TMDB's `adult` boolean is reserved for actual pornography, and every one of
/// the titles this excludes comes back `adult: false`. Measured on the live
/// endpoint, the flag removed 84 of 4465 anime series and none of the offenders.
///
/// **Why they surface in the first place.** `sort_by=popularity.desc` sorts on
/// TMDB's *traffic* metric (page views, API hits, list adds), not on audience
/// size or rating. Adult titles attract disproportionate scraping and search
/// traffic, so they score far above their audience: measured, one carried
/// popularity 313 on **26 votes** and another 242 on **7 votes**, against
/// Bleach's 92 on **2197**. Narrowing the pool to the anime keyword removes the
/// mainstream ballast that keeps them off the front of `/tv/popular`, which is
/// why only the anime rows show it.
///
/// **Why these five and not `erotic`/`ecchi`.** Those two are *content
/// descriptors* a mainstream show can legitimately carry — Mushoku Tensei
/// (tmdb 94664, 1626 votes) carries both — so excluding them costs real titles.
/// The five here tag adult *productions*, and no false positive was observed.
/// Keyword ids are stable; names resolved via `search/keyword`.
pub(crate) const DISCOVERY_EXCLUDED_KEYWORDS: &str = "198385,155477,325693,445,161919";

/// Minimum TMDB vote count for a discovery row entry.
///
/// The residue filter behind [`DISCOVERY_EXCLUDED_KEYWORDS`]: a handful of
/// erotic-tagged titles carry none of the five excluded keywords, and they are
/// uniformly low-signal (7–37 votes). A vote requires a real logged-in account,
/// which is exactly the signal the traffic-based popularity score lacks, so a
/// floor separates them from genuinely obscure-but-real titles far better than
/// popularity can.
///
/// 50 is deliberately low. It keeps Ranma½ (169) and Doraemon (237) — it is a
/// noise gate, not a popularity contest. Raising it much further starts costing
/// legitimate niche titles (Akane-banashi 25, Gravitation 17), which is why the
/// keyword exclusion carries the main load and this only sweeps up behind it.
///
/// Applies to `/discover` rows, which is now every mode except non-anime
/// **trending** — the one signal with no `/discover` equivalent, and therefore
/// the one row that cannot carry this gate. The popular/top-rated modes were
/// moved off `/{kind}/popular` and `/{kind}/top_rated` precisely so they could:
/// three hentai titles were measured at positions 4, 5 and 17 of the
/// *non-anime* `tv/popular` row.
pub(crate) const DISCOVERY_MIN_VOTES: u32 = 50;

/// Default number of cards returned for a free-text query (top-N by TMDB
/// popularity).
///
/// **This is no longer a fan-out multiplier.** In the pre-card design the same
/// knob (`DEFAULT_ANCHOR_TOP_N`) multiplied the indexer request count — every
/// anchor became its own paged search per indexer against a shared 1 req/s
/// bucket, so ten anchors meant the show the user actually wanted got a tenth
/// of the budget. A card query touches TMDB only, so N here costs at most one
/// cached `search/multi`; the indexer budget is spent later, entirely on the
/// single card the user clicks.
pub(crate) const DEFAULT_CARD_TOP_N: usize = 10;

/// How many ranked principal-search anchors to persist per query in the redb
/// cache. Sized comfortably above any reasonable top-N so flipping the knob
/// never needs a cache wipe — reads slice the cached list to the configured N.
pub(crate) const CACHED_PRINCIPAL_DEPTH: usize = 30;

/// Default number of cards a **keyword-less catalog** query returns (a home
/// row: `popular:true contentKind:series fileType:card`).
///
/// 20 is TMDB's own catalog page size, so the default row costs exactly one
/// TMDB call. Raising it past 20 is legal — [`crate::discovery`] walks further
/// pages — but each page is another call against the shared budget, paid on
/// every warm pass of every row.
pub(crate) const DEFAULT_CARD_DISCOVERY_N: usize = 20;

/// Defensive ceiling on a card's reported season count (guards a corrupt
/// `number_of_seasons`).
pub(crate) const MAX_CARD_SEASONS: u32 = 50;
