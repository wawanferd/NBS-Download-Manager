//! BitTorrent support for NBS Download Manager, built on librqbit.
//!
//! * Torrents come from magnet links, info-hashes, .torrent URLs or files. The
//!   metadata is resolved first so the user can pick files.
//! * **Download** mode saves the selected files to the download folder, with
//!   pause/resume that survives restarts.
//! * **Stream** mode saves to a cache folder and serves files over a local
//!   HTTP server (127.0.0.1) that supports Range requests. librqbit fetches
//!   the pieces around the current read position first, so playback starts
//!   while the torrent downloads and seeking works.
//! * Upload modes: keep seeding, stop seeding when done, or never upload.

mod engine;
mod types;
mod util;

pub use engine::TorrentEngine;
pub use types::*;
pub use util::{kind_of, normalize_link, to_webvtt};
