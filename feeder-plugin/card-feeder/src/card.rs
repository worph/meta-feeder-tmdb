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

use meta_feeder_sdk::hash::compute_card_cid;
use meta_feeder_sdk::types::DiscoveryRecord;

use crate::consts::MAX_CARD_SEASONS;
use crate::tmdb_client::{TmdbClient, TmdbKind, TmdbSeasonSummary};

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
    /// TMDB for it.
    ///
    /// Not read here yet — it is consumed by phase 2, which still resolves its
    /// own anchor until the indexer feeder's `build_anchor_record` path is
    /// retired. Resolving it now means the card is already complete when that
    /// switch happens.
    #[allow(dead_code)]
    pub(crate) guard_titles: Arc<Vec<String>>,
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
        // Third axis: which app the card is destined for. `fileType=card` says
        // "no bytes", `contentKind` says which work, `domain` says who wants it
        // — meta-watch's wall is a `domain:film|tv` query (METADATA_KEYS.md §1).
        if let Some(domain) = meta_feeder_sdk::domain::domain_for_content_kind(content_kind) {
            fields.insert("domain".to_string(), domain.to_string());
        }
        fields.insert("title".to_string(), self.title.clone());

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
            overview: Some("The story follows the elf mage Frieren.".to_string()),
            poster_path: Some("/dqZENchTd7lp5zit1Q7Bkjzcxpi.jpg".to_string()),
            genres: vec!["Animation".to_string(), "Sci-Fi & Fantasy".to_string()],
            year: Some(2023),
            seasons: 1,
            season_summaries: Arc::new(Vec::new()),
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
}
