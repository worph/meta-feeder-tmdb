//! The card model and its projection to a `DiscoveryRecord`.
//!
//! A **card** identifies a *work* — a series, a film — not a file. It carries
//! the display trio meta-watch's quality gate needs (title, poster,
//! description) plus an **id bag**: every cross-source identifier the metadata
//! bridge could resolve. Nothing here is a byte locator; the card's own CID is
//! a `0x1007` card-locator derived from `(source, id)` and nothing is ever
//! fetchable by it.
//!
//! This is the lifted-and-renamed `ResolvedAnchor` from the indexer feeder's
//! `torznab/mod.rs`, promoted from a private query-time struct to the feeder's
//! public output. The projection below is its `build_anchor_record`, minus the
//! two fields that only made sense while a card was pretending to be a video
//! (`anchorPlaceholder`, `videoType`) — see
//! `meta-gateway/docs/others/card-tier-search.md` §2.1.

use std::collections::BTreeMap;
use std::sync::Arc;

use meta_feeder_sdk::hash::{compute_card_cid, compute_url_cid};
use meta_feeder_sdk::types::DiscoveryRecord;

use crate::consts::MAX_CARD_SEASONS;
use crate::tmdb_client::{
    iso639_1_to_3, norm_tokens, normalize_title_name, title_member_key, TmdbClient, TmdbImage,
    TmdbKind, TmdbSeasonSummary,
};

/// The metadata source that published a card. One variant per bridge; the
/// string form is the CID's source namespace, so **changing it re-mints every
/// card CID from that source** — treat these as wire constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CardSource {
    Tmdb,
}

impl CardSource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            CardSource::Tmdb => "tmdb",
        }
    }
}

/// A resolved work. Immutable by contract: cards carry no rating, popularity,
/// or any other value that drifts, precisely so the identity and the content
/// stay in agreement forever (design doc §5). A newly aired season is a *new*
/// record, never an edit to this one.
#[derive(Debug, Clone)]
pub(crate) struct Card {
    pub(crate) source: CardSource,
    pub(crate) kind: TmdbKind,
    /// Source-local id for the work — `"tv:95479"` / `"movie:27205"` for TMDB.
    /// Half of the card CID's preimage; see [`Card::record_id`].
    pub(crate) source_id: String,

    // -- the id bag ---------------------------------------------------------
    // Every cross-source identifier resolved for this work. The phase-2 query
    // selects the best one for a given indexer's capabilities rather than
    // assuming `tmdbid`, so a future MyAnimeList-only card needs no new filter
    // name on the wire (design doc §10b).
    pub(crate) tmdbid: Option<u64>,
    /// `i64` to match TMDB's `external_ids` payload verbatim — no lossy cast.
    pub(crate) tvdb_id: Option<i64>,
    pub(crate) imdb_id: Option<String>,

    // -- display ------------------------------------------------------------
    pub(crate) title: String,
    /// Canonical + original + AKA titles. Rides along so the *content* feeders
    /// can run their resemblance guard against the card instead of re-querying
    /// TMDB for it (phase 2, once the indexer feeder's own anchor resolution is
    /// retired).
    ///
    /// Read here for one thing: picking `searchTitle` ([`search_aka`]).
    pub(crate) guard_titles: Arc<Vec<String>>,
    /// Every name of the work as `(lang3, name)`, filed on the record as the
    /// `titles/{lang3}/{name}` key-set (METADATA_KEYS.md). Built by
    /// `tmdb_client::title_names`: original under its language, `title` under
    /// `eng` when it differs, AKAs by market. Same names as `guard_titles`, with
    /// the language each is written in.
    pub(crate) names: Vec<(&'static str, String)>,
    pub(crate) overview: Option<String>,
    pub(crate) poster_path: Option<String>,
    /// Editorial genre names (`"Animation"`, `"Drama"`, …) — METADATA_KEYS
    /// §`genres`. The *work's* categorisation, distinct from `categories/*`,
    /// which is the Newznab/Prowlarr distribution taxonomy a release is filed
    /// under and which a card (having no release) never carries.
    ///
    /// Free on both resolution paths: a details fetch returns named genre
    /// objects, and a list hit's bare `genre_ids` are mapped through
    /// `Resolver::genre_names`. May be empty — TMDB genre coverage is good but
    /// not universal, and consumers must treat "no genres" as unknown rather
    /// than as "none".
    pub(crate) genres: Vec<String>,
    pub(crate) year: Option<u16>,

    // -- structure (TV only) ------------------------------------------------
    pub(crate) seasons: u32,
    /// Per-season episode counts. Surfaced on the record only as `seasonCount`
    /// today; the full summaries are what a *season-scoped* phase 2 would fan
    /// out over. Deliberately descoped for v1 (design doc §12.1) — note that a
    /// season list is the one thing an immutable card cannot carry forever, so
    /// when that lands it comes either from season cards or a live lookup, not
    /// from a mutated series card.
    #[allow(dead_code)]
    pub(crate) season_summaries: Arc<Vec<TmdbSeasonSummary>>,

    /// Poster candidates (TMDB `images.posters`), filed as the
    /// `posters/{lang3}/{cid}` key-set by [`poster_member_keys`]. Only the by-id
    /// path has them; discovery/search cards leave this empty and file no set.
    pub(crate) posters: Arc<Vec<TmdbImage>>,
}

impl Card {
    /// `contentKind` for this card: a work-level facet, orthogonal to
    /// `fileType=card`. A whole show is `series` (an individual instalment
    /// would be `episode`, which a card never is).
    pub(crate) fn content_kind(&self) -> &'static str {
        match self.kind {
            TmdbKind::Tv => "series",
            TmdbKind::Movie => "movie",
        }
    }

    /// The feeder-scoped record id, `"<source>:<source_id>"` — also the exact
    /// preimage half of the card CID, so `compute_outcomes` can round-trip a
    /// record id straight back to a CID with no lookup.
    pub(crate) fn record_id(&self) -> String {
        format!("{}:{}", self.source.as_str(), self.source_id)
    }

    /// This card's `0x1007` locator CID. Deterministic in `(source, id)`, so
    /// any peer holding a tmdb id derives the same address offline.
    ///
    /// `compute_outcomes` doesn't use this — it derives the CID straight from
    /// the record id string (`split_record_id`), deliberately, so deriving an
    /// address never needs a resolved `Card`. [`Card::to_record`] does use it,
    /// to stamp the `cids/` key-set member (see there for why that matters).
    pub(crate) fn cid(&self) -> Option<String> {
        compute_card_cid(self.source.as_str(), &self.source_id)
    }

    /// Clamp a TMDB season count into the defensive ceiling.
    pub(crate) fn clamp_seasons(n: u32) -> u32 {
        n.clamp(1, MAX_CARD_SEASONS)
    }

    /// The card's synopsis, trimmed and non-empty.
    pub(crate) fn overview_text(&self) -> Option<&str> {
        self.overview.as_deref().map(str::trim).filter(|s| !s.is_empty())
    }

    /// The card's poster path, non-empty.
    pub(crate) fn poster(&self) -> Option<&str> {
        self.poster_path.as_deref().filter(|s| !s.is_empty())
    }

    /// Would this card actually render? Exactly [`Card::to_record`]'s emit
    /// condition, hoisted so a caller can predict the drop *before* projecting.
    ///
    /// [`crate::discovery`] needs this: it walks catalog pages until it has N
    /// cards, and counting raw TMDB hits would overcount — a hit with no poster
    /// or no overview is silently declined at projection time, leaving a short
    /// row. The two must not drift, hence one predicate rather than two.
    pub(crate) fn is_displayable(&self) -> bool {
        self.overview_text().is_some() && self.poster().is_some()
    }

    /// Project to the wire record.
    ///
    /// Returns `None` when the card lacks a poster or a description — it would
    /// fail meta-watch's quality gate and render as a hole in the grid, so the
    /// feeder declines to emit it at all rather than shipping an unrenderable
    /// card.
    ///
    /// `query_filters` echoes back every structured filter the query carried
    /// (except `languages`, which fails open on a record that has none) so the
    /// record survives the gateway's and meta-search's `record_matches`
    /// re-filter on the way out.
    pub(crate) fn to_record(
        &self,
        tmdb: &TmdbClient,
        query_filters: &BTreeMap<String, Vec<String>>,
    ) -> Option<DiscoveryRecord> {
        // Same two conditions as `Card::is_displayable`, which the discovery page
        // walk uses to predict this drop — keep them in one place.
        let overview = self.overview_text()?;
        let poster_path = self.poster()?;

        let mut fields: BTreeMap<String, String> = BTreeMap::new();
        // Type axes. `card` on the fileType axis and the work kind on the
        // contentKind axis stay orthogonal, so `fileType:card contentKind:series`
        // is a well-formed query and routing needs no special case.
        fields.insert("fileType".to_string(), "card".to_string());
        let content_kind = self.content_kind();
        fields.insert("contentKind".to_string(), content_kind.to_string());
        // Third and fourth axes: which app the card is destined for, and what
        // shape of work it is. `fileType=card` says "no bytes", `contentKind`
        // says which work, `domain` says who wants it — meta-watch's wall is a
        // `domain:screen` query — and `workForm` says film-or-serial, the split
        // `domain` stopped carrying when film and tv merged (METADATA_KEYS.md
        // §1, §14.17).
        if let Some(domain) = meta_feeder_sdk::domain::domain_for_content_kind(content_kind) {
            fields.insert("domain".to_string(), domain.to_string());
        }
        if let Some(work_form) =
            meta_feeder_sdk::domain::work_form_for_content_kind(content_kind)
        {
            fields.insert("workForm".to_string(), work_form.to_string());
        }
        fields.insert("title".to_string(), self.title.clone());
        // The keyword a consumer should search indexers with, only when `title`
        // itself can't be searched (`3%` → `3 percent`). meta-watch's title page
        // has no TMDB access of its own and reads this off the card; without it
        // its free-text query sends `3%`, which one newznab answers with nothing
        // and another floods with every release containing a `3`.
        if let Some(aka) = search_aka(&self.title, self.guard_titles.iter()) {
            // Normalised like a `titles/*/*` member name: `searchTitle` is always
            // one of the record's names, spelled the same way.
            fields.insert("searchTitle".to_string(), normalize_title_name(aka));
        }
        // Every name the work is known by, as the language-nested key-set. A
        // key-set so two peers resolving different AKAs union on key-merge.
        for (lang3, name) in &self.names {
            if let Some(key) = title_member_key(lang3, name) {
                fields.insert(key, "true".to_string());
            }
        }

        // The id bag.
        if let Some(id) = self.tmdbid {
            fields.insert("tmdbid".to_string(), id.to_string());
        }
        if let Some(id) = self.tvdb_id {
            fields.insert("tvdbid".to_string(), id.to_string());
        }
        if let Some(id) = self.imdb_id.as_deref().filter(|s| !s.is_empty()) {
            fields.insert("imdbid".to_string(), id.to_string());
        }
        if let Some(y) = self.year {
            fields.insert("movieYear".to_string(), y.to_string());
        }
        if self.kind == TmdbKind::Tv && self.seasons > 0 {
            fields.insert("seasonCount".to_string(), self.seasons.to_string());
        }

        // Synopsis under the namespaced key meta-search indexes (not a flat
        // `overview`).
        fields.insert("description/eng".to_string(), overview.to_string());

        // Genres as a KEY-SET (`genres/<Name> = "true"`), not the legacy
        // comma-joined `genres` value the tmdb/jellyfin plugins still write.
        //
        // METADATA_KEYS §14.12 is explicit that `csv-set` is the shape being
        // migrated *away* from and that "new writers must not introduce more
        // csv-set fields — reach for key-set, even if the surrounding family is
        // still on the legacy shape". This is a new writer, and the reason bites
        // here specifically: a card is the one record type multiple peers derive
        // independently at the same CID (§9.4), so two peers resolving different
        // genre sets must union by key-merge rather than diverge into two
        // comma-joined strings that need string-diffing to reconcile.
        for g in &self.genres {
            let g = g.trim();
            if !g.is_empty() {
                fields.insert(format!("genres/{g}"), "true".to_string());
            }
        }

        // The bare-CID key-set member, exactly as every other feeder stamps on
        // its search records (`indexer-feeder`'s `torznab/xml.rs` does the same
        // with the release's infohash).
        //
        // **Load-bearing, and not merely cosmetic.** The gateway's search path
        // persists a hit to meta-core only if `url_key_for` can find a CID on
        // it (`dispatch.rs` — no key ⇒ `continue`, silently unpersisted). A card
        // is the one record type whose CID isn't discovered from an upstream but
        // *derived*, and `compute_outcomes` (the other place it's derived) is not
        // on the search path — so without this line a card is never persisted at
        // all. That interacts badly with the search-coverage gate, which stamps
        // `(upstream, query)` as covered whenever the feeder produced records and
        // thereafter skips the feeder for an hour, serving meta-core instead:
        // coverage marked + nothing persisted = **an identical card search
        // returns empty for the rest of the window**. Observed live before this
        // was added.
        //
        // Stamping it also delivers §4's addressability and §9.4's free
        // cross-peer convergence: two peers that resolve the same work publish
        // the same locator, so meta-core's reverse index folds them into one
        // record with no merge logic.
        if let Some(cid) = self.cid() {
            fields.insert(format!("cids/{cid}"), "true".to_string());
        }

        // Poster: the transient `posterPath` is rewritten to `poster_url` below,
        // and the GATEWAY CORE seeds that URL into a content-addressed `poster`
        // cid (`blockstore::seed_preview`). The feeder never fetches poster
        // bytes — invariant 10.
        fields.insert(
            "poster_url".to_string(),
            tmdb.poster_cdn_url(poster_path),
        );
        // The alternative posters, as the language-nested key-set (METADATA_KEYS
        // §6 `posters/{lang3}/{cid}`). Every member is a `url` locator, so listing
        // them costs no fetch. The primary member wraps the exact `poster_url`
        // string above: the gateway matches on it and renames the member to the
        // content cid it seeds into `poster` — that is how "`poster` is a member"
        // holds for a feeder card.
        for key in poster_member_keys(tmdb, poster_path, &self.posters) {
            fields.insert(key, "true".to_string());
        }

        for (key, allowed) in query_filters {
            // `genres` joins `languages` as an echo exclusion, for the same
            // reason: it is stored as a key-set, so the flat field this loop
            // would write is not the shape a reader looks for. Worse, the filter
            // value is a *slug* (`action-adventure` — the query DSL can't spell
            // "Action & Adventure", space being its token separator), so echoing
            // it would persist a slug masquerading as a genre name alongside the
            // real `genres/<Name>` members. Both re-validating tiers read the
            // key-set, so nothing needs the echo.
            if key == "languages"
                || key == "genres"
                || fields.contains_key(key)
                || allowed.is_empty()
            {
                continue;
            }
            fields.insert(key.clone(), allowed.join(","));
        }

        Some(DiscoveryRecord {
            upstream_id: self.source.as_str().to_string(),
            record_id: self.record_id(),
            fields,
        })
    }
}

/// Most members a record's `posters/*` key-set may carry, the primary included
/// (METADATA_KEYS.md §6, writer rule 1).
pub(crate) const MAX_POSTER_MEMBERS: usize = 10;

/// The `posters/{lang3}/{cid}` keys for a card: the primary poster first, then
/// the best-voted alternatives, capped at [`MAX_POSTER_MEMBERS`]. Empty when
/// TMDB returned no image list — without one the primary's language is unknown,
/// and a set is better absent than mislabelled.
///
/// Ranked by `vote_average`, then `vote_count`, then `file_path`, the last so
/// two peers holding the same payload emit the same keys. The language is that
/// of the text printed on the poster: `zxx` for a textless one (TMDB `null`),
/// `und` for a code outside [`iso639_1_to_3`] or a primary TMDB did not list.
///
/// CROSS-BINARY CONTRACT: same ranking, cap, CDN size and language rule as the
/// tmdb plugin's `posterMembers` (`metamesh-plugin-tmdb/src/posters.ts`).
pub(crate) fn poster_member_keys(
    tmdb: &TmdbClient,
    primary_path: &str,
    candidates: &[TmdbImage],
) -> Vec<String> {
    if candidates.is_empty() {
        return Vec::new();
    }
    let lang3 = |img: &TmdbImage| -> &'static str {
        match img.iso_639_1.as_deref().map(str::trim) {
            None | Some("") => "zxx",
            Some(code) => iso639_1_to_3(code).unwrap_or("und"),
        }
    };
    let mut keys: Vec<String> = Vec::new();
    let primary_lang = candidates
        .iter()
        .find(|img| img.file_path == primary_path)
        .map_or("und", lang3);
    if let Some(cid) = compute_url_cid(&tmdb.poster_cdn_url(primary_path)) {
        keys.push(format!("posters/{primary_lang}/{cid}"));
    }
    let mut ranked: Vec<&TmdbImage> = candidates
        .iter()
        .filter(|img| !img.file_path.trim().is_empty() && img.file_path != primary_path)
        .collect();
    ranked.sort_by(|a, b| {
        b.vote_average
            .total_cmp(&a.vote_average)
            .then(b.vote_count.cmp(&a.vote_count))
            .then(a.file_path.cmp(&b.file_path))
    });
    for img in ranked {
        if keys.len() >= MAX_POSTER_MEMBERS {
            break;
        }
        if let Some(cid) = compute_url_cid(&tmdb.poster_cdn_url(&img.file_path)) {
            let key = format!("posters/{}/{cid}", lang3(img));
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
    }
    keys
}

/// Punctuation keyword searches fold to a space. Only used to *exclude* these
/// from [`is_search_hostile_symbol`] — a folded character searches fine.
///
/// CROSS-BINARY CONTRACT: identical to the indexer feeder's
/// `torznab/mod.rs::KEYWORD_PUNCTUATION`, and to meta-watch's
/// `clean_title` + `sanitize_free_text` sets combined.
const KEYWORD_PUNCTUATION: [char; 11] = ['.', '_', ':', ';', ',', '-', '/', '(', ')', '=', '!'];

/// An ASCII symbol an indexer's search box can neither carry nor safely lose
/// (`%` in `3%`): punctuation outside [`KEYWORD_PUNCTUATION`], apostrophe
/// excepted. See the indexer feeder's `is_search_hostile_symbol` for the
/// measurements.
///
/// CROSS-BINARY CONTRACT: same rule as the indexer feeder's
/// `torznab/mod.rs::is_search_hostile_symbol` and meta-watch's
/// `catalog::query::has_search_hostile_symbol`.
fn is_search_hostile_symbol(c: char) -> bool {
    c.is_ascii_punctuation() && c != '\'' && !KEYWORD_PUNCTUATION.contains(&c)
}

/// The AKA to search with in place of `title` — the card's `searchTitle`.
/// `None` unless `title` carries a [search-hostile symbol](is_search_hostile_symbol)
/// **and** an AKA is symbol-free and strictly more specific (all of the title's
/// word tokens, plus at least one): `3 percent` for `3%`, never the bare `3`.
/// First qualifying AKA wins; `TmdbAltTitles::all` orders English markets first.
///
/// CROSS-BINARY CONTRACT: mirrors the indexer feeder's
/// `torznab/mod.rs::search_aka`, which picks the anchored jobs' `q=` the same
/// way. Change both.
pub(crate) fn search_aka<'a>(
    title: &str,
    akas: impl IntoIterator<Item = &'a String>,
) -> Option<&'a str> {
    if !title.chars().any(is_search_hostile_symbol) {
        return None;
    }
    let want = norm_tokens(title);
    if want.is_empty() {
        return None;
    }
    akas.into_iter()
        .filter(|aka| !aka.chars().any(is_search_hostile_symbol))
        .find(|aka| {
            let have = norm_tokens(aka);
            have.len() > want.len() && want.iter().all(|w| have.contains(w))
        })
        .map(String::as_str)
}

/// Split a card `record_id` (`"tmdb:tv:95479"`) back into `(source, source_id)`
/// — the two halves of the CID preimage. `None` when the id carries no source
/// prefix.
pub(crate) fn split_record_id(record_id: &str) -> Option<(&str, &str)> {
    record_id.split_once(':').filter(|(s, id)| !s.is_empty() && !id.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tv_card() -> Card {
        Card {
            source: CardSource::Tmdb,
            kind: TmdbKind::Tv,
            source_id: "tv:95479".to_string(),
            tmdbid: Some(95479),
            tvdb_id: Some(367189),
            imdb_id: Some("tt22248376".to_string()),
            title: "Frieren: Beyond Journey's End".to_string(),
            guard_titles: Arc::new(vec!["Sousou no Frieren".to_string()]),
            names: vec![
                ("jpn", "葬送のフリーレン".to_string()),
                ("eng", "Frieren: Beyond Journey's End".to_string()),
                ("jpn", "Sousou no Frieren".to_string()),
            ],
            overview: Some("The story follows the elf mage Frieren.".to_string()),
            poster_path: Some("/dqZENchTd7lp5zit1Q7Bkjzcxpi.jpg".to_string()),
            genres: vec!["Animation".to_string(), "Sci-Fi & Fantasy".to_string()],
            year: Some(2023),
            seasons: 1,
            season_summaries: Arc::new(Vec::new()),
            posters: Arc::new(Vec::new()),
        }
    }

    #[test]
    fn record_id_round_trips_to_the_cid_preimage() {
        let card = tv_card();
        let record_id = card.record_id();
        assert_eq!(record_id, "tmdb:tv:95479");
        let (source, id) = split_record_id(&record_id).unwrap();
        assert_eq!((source, id), ("tmdb", "tv:95479"));
        // The id a peer would derive offline must equal the one we publish.
        assert_eq!(compute_card_cid(source, id), card.cid());
    }

    #[test]
    fn record_carries_both_type_axes_and_the_whole_id_bag() {
        let tmdb = TmdbClient::new("token".to_string());
        let rec = tv_card().to_record(&tmdb, &BTreeMap::new()).expect("record");
        assert_eq!(rec.fields["fileType"], "card");
        assert_eq!(rec.fields["contentKind"], "series");
        assert_eq!(rec.fields["tmdbid"], "95479");
        assert_eq!(rec.fields["tvdbid"], "367189");
        assert_eq!(rec.fields["imdbid"], "tt22248376");
        assert!(rec.fields["poster_url"].contains("dqZENchTd7lp5zit1Q7Bkjzcxpi.jpg"));
        assert!(rec.fields.contains_key("description/eng"));
        // A card is not a video pretending to have no stream — it is its own
        // type. The two markers the placeholder needed are gone.
        assert!(!rec.fields.contains_key("videoType"));
        assert!(!rec.fields.contains_key("anchorPlaceholder"));
        // And it must never carry a poster *path* (the transient field the
        // gateway core would not know how to seed).
        assert!(!rec.fields.contains_key("posterPath"));
    }

    /// Regression: the record MUST carry a `cids/` key-set member, because the
    /// gateway's search-persist path silently drops any record `url_key_for`
    /// finds no CID on. Without it a card is never persisted, and the
    /// search-coverage gate then blanks every repeat search for an hour (see
    /// the comment in `to_record`). This failed live before the fix.
    #[test]
    fn record_carries_its_locator_as_a_cids_keyset_member() {
        let tmdb = TmdbClient::new("token".to_string());
        let card = tv_card();
        let rec = card.to_record(&tmdb, &BTreeMap::new()).expect("record");
        let cid = card.cid().expect("locator");
        assert_eq!(
            rec.fields.get(&format!("cids/{cid}")).map(String::as_str),
            Some("true"),
            "a card must be persistable; see to_record's cids/ comment"
        );
        // And it must be the derived locator, not some other cid shape.
        assert!(cid.starts_with('b'));
    }

    /// The quality gate lives here, not in the consumer: a card with no poster
    /// or no synopsis would render as a hole in meta-watch's grid.
    #[test]
    fn card_without_poster_or_overview_is_not_emitted() {
        let tmdb = TmdbClient::new("token".to_string());
        let mut no_poster = tv_card();
        no_poster.poster_path = None;
        assert!(no_poster.to_record(&tmdb, &BTreeMap::new()).is_none());

        let mut no_overview = tv_card();
        no_overview.overview = Some("   ".to_string());
        assert!(no_overview.to_record(&tmdb, &BTreeMap::new()).is_none());
    }

    /// `3%` (TMDB tv 68467): the card spells the symbol out for searchers, and
    /// keeps the real title for display.
    #[test]
    fn symbol_title_card_carries_a_search_title() {
        let tmdb = TmdbClient::new("token".to_string());
        let mut card = tv_card();
        card.title = "3%".to_string();
        card.guard_titles = Arc::new(
            ["3%", "3 %", "3", "Three Percent", "3 percent", "3 Pourcent"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        let rec = card.to_record(&tmdb, &BTreeMap::new()).expect("record");
        assert_eq!(rec.fields["title"], "3%");
        assert_eq!(rec.fields.get("searchTitle").map(String::as_str), Some("3 percent"));
        // A searchable title gets no searchTitle — Frieren's apostrophe and
        // colon are not hostile.
        let rec = tv_card().to_record(&tmdb, &BTreeMap::new()).expect("record");
        assert!(!rec.fields.contains_key("searchTitle"));
    }

    #[test]
    fn record_files_every_name_as_a_language_nested_key_set() {
        let tmdb = TmdbClient::new("token".to_string());
        let rec = tv_card().to_record(&tmdb, &BTreeMap::new()).expect("record");
        for key in [
            "titles/jpn/葬送のフリーレン",
            "titles/eng/Frieren: Beyond Journey's End",
            "titles/jpn/Sousou no Frieren",
        ] {
            assert_eq!(rec.fields.get(key).map(String::as_str), Some("true"), "{key}");
        }
        // Leaves only: no scalar at the language level.
        assert!(!rec.fields.keys().any(|k| k.starts_with("titles/") && k.matches('/').count() == 1));
    }

    /// The METADATA_KEYS.md example, end to end from TMDB's data.
    #[test]
    fn title_names_match_the_three_percent_example() {
        use crate::tmdb_client::{title_names, TmdbAltTitle};
        let aka = |m: &str, t: &str| TmdbAltTitle { title: t.to_string(), iso_3166_1: m.to_string() };
        let names = title_names(
            "3%",
            Some("3%"),
            Some("pt"),
            &[aka("US", "3 Percent"), aka("FR", "3 Pourcent"), aka("BR", "3%"), aka("CA", "3 Por  Cento ")],
        );
        assert_eq!(
            names,
            vec![
                ("por", "3%".to_string()),
                ("eng", "3 Percent".to_string()),
                ("fra", "3 Pourcent".to_string()),
                ("und", "3 Por Cento".to_string()),
            ],
            "title == original is filed once; BR `3%` repeats por/3%; CA is multilingual → und"
        );
        // A translated title is filed under eng beside the original.
        let names = title_names("Naruto", Some("ナルト"), Some("ja"), &[]);
        assert_eq!(names, vec![("jpn", "ナルト".to_string()), ("eng", "Naruto".to_string())]);
        // Unknown original language → und; TMDB's placeholder is not a name.
        assert_eq!(title_names("(untitled)", Some("X"), Some("xx"), &[]), vec![("und", "X".to_string())]);
    }

    #[test]
    fn title_member_key_normalises_like_two_peers_must() {
        assert_eq!(title_member_key("jpn", "Fate/Zero").as_deref(), Some("titles/jpn/Fate\u{2215}Zero"));
        assert_eq!(title_member_key("eng", "  3   Percent ").as_deref(), Some("titles/eng/3 Percent"));
        assert_eq!(title_member_key("eng", "Pokémon").as_deref(), Some("titles/eng/Pokémon"));
        assert_eq!(title_member_key("eng", "   "), None);
    }

    #[test]
    fn search_aka_never_picks_something_weaker() {
        let akas = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(search_aka("M*A*S*H", akas(&["MASH"]).iter()), None);
        assert_eq!(search_aka("3%", akas(&["3", "3 %"]).iter()), None);
        assert_eq!(search_aka("Law & Order", akas(&["Law and Order"]).iter()), Some("Law and Order"));
        assert_eq!(search_aka("Naruto", akas(&["Naruto Shippuden"]).iter()), None);
    }

    #[test]
    fn tmdb_alt_titles_put_english_markets_first() {
        let alt: crate::tmdb_client::TmdbAltTitles = serde_json::from_str(
            r#"{"results":[{"iso_3166_1":"FR","title":"3 Pourcent"},{"iso_3166_1":"US","title":"3 percent"},{"title":"no market"}]}"#,
        )
        .expect("decode");
        assert_eq!(alt.all(), vec!["3 percent", "3 Pourcent", "no market"]);
        assert!(alt.has_markets());
        let legacy: crate::tmdb_client::TmdbAltTitles =
            serde_json::from_str(r#"{"results":[{"title":"3 Pourcent"},{"title":"3 percent"}]}"#).expect("decode");
        assert!(!legacy.has_markets(), "a pre-market cache entry must refetch");
        assert!(crate::tmdb_client::TmdbAltTitles::default().has_markets(), "no AKAs is complete");
    }

    #[test]
    fn movie_card_uses_the_movie_content_kind() {
        let mut card = tv_card();
        card.kind = TmdbKind::Movie;
        card.source_id = "movie:27205".to_string();
        assert_eq!(card.content_kind(), "movie");
        assert_eq!(card.record_id(), "tmdb:movie:27205");
        // Distinct id namespace → distinct CID from the tv card.
        assert_ne!(card.cid(), tv_card().cid());
    }

    /// Query filters are echoed so the record survives `record_matches` on the
    /// way out — except the two key-set fields, which a reader resolves from
    /// their `<prefix>/<member>` members rather than from a flat value.
    #[test]
    fn query_filters_are_echoed_except_the_key_set_fields() {
        let tmdb = TmdbClient::new("token".to_string());
        let mut filters = BTreeMap::new();
        filters.insert("fileType".to_string(), vec!["card".to_string()]);
        filters.insert("popular".to_string(), vec!["true".to_string()]);
        filters.insert("genres".to_string(), vec!["action-adventure".to_string()]);
        filters.insert("languages".to_string(), vec!["jpn".to_string()]);
        let rec = tv_card().to_record(&tmdb, &filters).expect("record");

        assert_eq!(rec.fields["popular"], "true", "ordinary filters echo");
        assert_eq!(rec.fields["fileType"], "card", "own value wins the echo");
        assert!(!rec.fields.contains_key("languages"));
        // The genre filter carries a *slug* the DSL can spell; echoing it would
        // persist "action-adventure" as if it were a genre name, beside the real
        // `genres/Action & Adventure` members.
        assert!(
            !rec.fields.contains_key("genres"),
            "the genre slug must not be echoed as a flat field"
        );
        assert_eq!(rec.fields["genres/Animation"], "true");
        assert_eq!(rec.fields["genres/Sci-Fi & Fantasy"], "true");
    }

    fn img(path: &str, lang: Option<&str>, avg: f64, count: u32) -> TmdbImage {
        TmdbImage {
            file_path: path.to_string(),
            iso_639_1: lang.map(str::to_string),
            vote_average: avg,
            vote_count: count,
        }
    }

    /// METADATA_KEYS §6 `posters/{lang3}/{cid}`: the primary is a member, filed
    /// under its own language, as the locator of the exact `poster_url` the
    /// gateway seeds — that equality is what lets the gateway rename it.
    #[test]
    fn record_files_the_primary_poster_as_a_locator_member() {
        let tmdb = TmdbClient::new("token".to_string());
        let mut card = tv_card();
        let primary = card.poster_path.clone().expect("fixture poster");
        card.posters = Arc::new(vec![
            img("/alt-fr.jpg", Some("fr"), 5.5, 10),
            img(&primary, Some("ja"), 5.0, 3),
            img("/textless.jpg", None, 6.0, 1),
        ]);
        let rec = card.to_record(&tmdb, &BTreeMap::new()).expect("record");

        let primary_cid = compute_url_cid(&rec.fields["poster_url"]).expect("locator");
        assert_eq!(
            rec.fields.get(&format!("posters/jpn/{primary_cid}")).map(String::as_str),
            Some("true")
        );
        let fr = compute_url_cid(&tmdb.poster_cdn_url("/alt-fr.jpg")).expect("locator");
        let textless = compute_url_cid(&tmdb.poster_cdn_url("/textless.jpg")).expect("locator");
        assert!(rec.fields.contains_key(&format!("posters/fra/{fr}")));
        assert!(rec.fields.contains_key(&format!("posters/zxx/{textless}")));
        let members = rec.fields.keys().filter(|k| k.starts_with("posters/")).count();
        assert_eq!(members, 3, "the primary is listed once, not twice");
        // Leaves only, and never a 639-2/B code.
        assert!(!rec.fields.keys().any(|k| k.starts_with("posters/") && k.matches('/').count() != 2));
        assert!(!rec.fields.keys().any(|k| k.starts_with("posters/fre/")));
    }

    #[test]
    fn poster_members_rank_by_votes_and_cap_at_ten() {
        let tmdb = TmdbClient::new("token".to_string());
        let candidates: Vec<TmdbImage> = (0..15)
            .map(|i| img(&format!("/p{i:02}.jpg"), Some("en"), f64::from(i), 1))
            .collect();
        let keys = poster_member_keys(&tmdb, "/primary.jpg", &candidates);
        assert_eq!(keys.len(), MAX_POSTER_MEMBERS);
        // Primary first; TMDB didn't list it among its images, so `und`.
        let primary = compute_url_cid(&tmdb.poster_cdn_url("/primary.jpg")).expect("locator");
        assert_eq!(keys[0], format!("posters/und/{primary}"));
        // Then the best vote_average: p14, p13, … p06 — p05 is cut.
        let best = compute_url_cid(&tmdb.poster_cdn_url("/p14.jpg")).expect("locator");
        assert_eq!(keys[1], format!("posters/eng/{best}"));
        let cut = compute_url_cid(&tmdb.poster_cdn_url("/p05.jpg")).expect("locator");
        assert!(!keys.contains(&format!("posters/eng/{cut}")));
        // vote_count breaks a vote_average tie.
        let tie = poster_member_keys(
            &tmdb,
            "/primary.jpg",
            &[img("/few.jpg", None, 5.0, 1), img("/many.jpg", None, 5.0, 9)],
        );
        let many = compute_url_cid(&tmdb.poster_cdn_url("/many.jpg")).expect("locator");
        assert_eq!(tie[1], format!("posters/zxx/{many}"));
        // An unmapped language is `und`, never a 2-letter key.
        let odd = poster_member_keys(&tmdb, "/primary.jpg", &[img("/x.jpg", Some("xx"), 1.0, 1)]);
        assert!(odd[1].starts_with("posters/und/"), "{odd:?}");
    }

    /// No image list (a discovery hit, a pre-append cache entry): no set at all,
    /// rather than a primary filed under a guessed `und`.
    #[test]
    fn card_without_an_image_list_files_no_poster_set() {
        let tmdb = TmdbClient::new("token".to_string());
        let rec = tv_card().to_record(&tmdb, &BTreeMap::new()).expect("record");
        assert!(rec.fields.contains_key("poster_url"), "poster itself is unaffected");
        assert!(!rec.fields.keys().any(|k| k.starts_with("posters/")));
    }

    #[test]
    fn tmdb_images_append_decodes() {
        let d: crate::tmdb_client::TmdbTvDetails = serde_json::from_str(
            r#"{"name":"X","images":{"posters":[{"file_path":"/a.jpg","iso_639_1":null,"vote_average":5.3,"vote_count":4},{"file_path":"/b.jpg","iso_639_1":"fr","vote_average":0}]}}"#,
        )
        .expect("decode");
        let posters = d.images.expect("images").posters;
        assert_eq!(posters.len(), 2);
        assert_eq!(posters[0].iso_639_1, None);
        assert_eq!(posters[1].iso_639_1.as_deref(), Some("fr"));
        assert_eq!(posters[1].vote_count, 0);
        // A pre-append cache entry still decodes, with no image list.
        let old: crate::tmdb_client::TmdbTvDetails =
            serde_json::from_str(r#"{"name":"X"}"#).expect("decode");
        assert!(old.images.is_none());
    }
}
