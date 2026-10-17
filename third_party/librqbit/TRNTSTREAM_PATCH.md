# librqbit 9.0.1 — TrntStream patch

Vendored from crates.io (`librqbit-9.0.1`) and used through `[patch.crates-io]`
in `rust/Cargo.toml`.

## Why

The browser shows movie previews (a few frames) for magnet/.torrent links. To
grab a frame, the app opens the video over librqbit's HTTP stream and lets mpv
seek to a keyframe. Upstream librqbit always downloads every piece of the
selected files in the background and prioritises a 32 MB window after each
stream position, so a preview would cost hundreds of MB.

## Change

`ManagedTorrent::set_preview_only(bool)` (in `src/torrent_state/streaming.rs`).
When enabled for a torrent:

- the stream window is 1 MiB after the read position instead of 32 MB (for
  small pieces this lets a sequential header read request several at once);
- the piece picker (`src/piece_tracker.rs`, `AcquireRequest::priority_only`)
  only requests pieces that streams are waiting on — nothing else is queued;
- a stream that blocks on a missing piece wakes idle peers immediately
  (`TorrentStreams::wanted_notify`, used in `src/torrent_state/live/mod.rs`)
  instead of waiting for the 5 s idle timer.

Torrents without the flag behave exactly as upstream.

## Windows sparse files

Upstream never marked files sparse on Windows, so every file was fully
allocated on disk (a preview of a 4 GB movie took 4 GB):

- `src/storage/filesystem/opened_file.rs`: `mark_file_sparse()` was called
  inside a `tracing::debug!()` field, which is not evaluated unless debug
  logging is on. It now runs unconditionally.
- `src/storage/filesystem/sparse.rs`: `DeviceIoControl(FSCTL_SET_SPARSE)` now
  passes `lpBytesReturned`, which Win32 requires when no OVERLAPPED is given.

Files now only use disk space for downloaded pieces. This also helps normal
streaming and downloads. The network test in `rust/src/api/engine.rs` checks
the sparse attribute of a preview file.

All changes are marked with `TRNTSTREAM` comments.

## Updating

Copy the new librqbit release over this folder and re-apply the `TRNTSTREAM`
hunks, or drop the patch if upstream gains a piece-level selection API.
