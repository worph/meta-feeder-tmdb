//! `card-feeder` library surface.
//!
//! The **discovery** half of gateway search. Bridges metadata sources into
//! `fileType=card` records — poster + description + an id bag describing a
//! *work*, never a file — so the question "which work is this?" is answered
//! cheaply and separately from "which releases exist for it?".
//!
//! One upstream today (`tmdb`). MyAnimeList will be a sibling `FeederPlugin`
//! in this same binary, declaring the same `served_file_types = ["card"]`.
//!
//! Design: `meta-gateway/docs/others/card-tier-search.md`.

pub mod card;
pub mod consts;
pub mod discovery;
pub mod resolve;
pub mod tmdb;
pub mod tmdb_budget;
pub mod tmdb_client;
