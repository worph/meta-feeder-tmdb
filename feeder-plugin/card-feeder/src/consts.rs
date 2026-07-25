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

/// Defensive ceiling on a card's reported season count (guards a corrupt
/// `number_of_seasons`).
pub(crate) const MAX_CARD_SEASONS: u32 = 50;
