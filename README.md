# meta-feeder-card

The **card tier** feeder for the MetaMesh gateway — the *discovery* half of
gateway search.

A **card** is a metadata-only record identifying a *work* (a series, a film),
not a file: title, poster, description, and an **id bag** of every cross-source
identifier the bridge could resolve. It has no bytes, and by design never will.

```
Phase 1 — CARD (this feeder)          Phase 2 — RETRIEVAL (content feeders)
  <free text> fileType:card              fileType:video + the card's id bag
  one cached TMDB search/multi           the full indexer budget, ONE work
  → a poster grid of works               → the episode / version list
                        ── user clicks a card ──
```

Splitting the two halves is the point. Previously one query resolved ten
speculative candidate works *and* searched every indexer for all of them, against
a shared 1 req/s bucket — so the work the user actually wanted got a tenth of
the budget. A card query touches TMDB only; the indexer budget is spent later,
entirely on the card that was clicked.

## Upstreams

| upstream_id | source | status |
|---|---|---|
| `tmdb` | themoviedb.org | shipping |
| `myanimelist` | myanimelist.net | planned — a sibling `FeederPlugin` in this same binary |

Both declare `served_file_types = ["card"]`, which is the whole routing story:
meta-search fans a `fileType:card` query out to every gateway advertising it, so
adding this tier needed no meta-search change.

## Card identity

A card's CID is a **`0x1007` card locator** — an identity-multihash CIDv1 whose
payload is `varint(len(source)) ‖ source ‖ id`, e.g. `("tmdb", "tv:95479")`.

- **Deterministic in `(source, id)`**, so any peer holding a tmdb id derives the
  address offline, with no discovery round-trip — and the same card found by two
  peers converges on one meta-core record for free.
- **Ranks 5** (opaque-locator tier), so it never outranks a real digest. That is
  what will make a future cross-source merge — one meta-core record carrying
  *both* a TMDB and a MyAnimeList locator — safe.
- **Never seeded to bitswap** (seeding is gated on sha2-256). Nothing to seed.

Cards are treated as **immutable**: no TTL, no refresh, and no drifting values
(rating, popularity) stored on them. Growth is append-only — a future *season
card* is an addition beside the series card, never an edit to it.

## Config

Per-plugin config lives on the feeder (gateway invariant 12), served as a
schema-driven form at `GET /config`:

| key | meaning |
|---|---|
| `tmdb_token` | TMDB v4 read token. **Required** — without it the feeder soft-skips (stays healthy, returns no cards). |
| `tmdb_rate_per_sec` | Sustained TMDB request rate. Default 20/s. |
| `tmdb_burst` | Short-burst token ceiling. Default 20. |
| `card_top_n` | Works returned per free-text search. Default 10. Costs no indexer requests. |

Env (`TMDB_TOKEN`, `TMDB_RATE_PER_SEC`, `TMDB_BURST`, `CARD_TOP_N`) seeds the
first boot; the config file wins from then on. No hot reload — bounce the feeder.

## Build / run

```bash
cargo build --release --bin card-feeder
cargo test -p card-feeder

# Image (build context is the repo root so the vendored SDK path dep resolves)
docker build -f feeder-plugin/card-feeder/Dockerfile -t ghcr.io/worph/meta-feeder-card:dev .
```

Register it with a gateway by adding it to that gateway's `gateway-config.json`
`feeders` map (or via the dashboard) and restarting:

```json
{ "feeders": { "card-feeder": { "url": "http://card-feeder-hub:8080" } } }
```

## Layout

```
crates/meta-feeder-sdk/        vendored SDK copy (mirrored from meta-gateway)
feeder-plugin/card-feeder/
  src/tmdb.rs                  the FeederPlugin impl — routing + query + outcomes
  src/card.rs                  the Card model + its DiscoveryRecord projection
  src/resolve.rs               cache- and budget-gated TMDB resolution
  src/tmdb_client.rs           TMDB v3 HTTP client + DTOs
  src/tmdb_budget.rs           token bucket
  src/consts.rs
```

Design rationale, including what was deliberately *not* built:
`meta-gateway/docs/others/card-tier-search.md`.
