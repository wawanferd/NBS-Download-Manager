# librqbit-tracker-comms 9.0.1 — NBS patch

Vendored from crates.io (`librqbit-tracker-comms-9.0.1`) and used through
`[patch.crates-io]` in the workspace `Cargo.toml`.

`UdpTrackerClient::new` (`src/tracker_comms_udp.rs`) always bound a dual-stack
socket on `[::]:0`. On systems without IPv6 (for example Linux booted with
`ipv6.disable=1`) that fails with "Address family not supported", and with it
the whole librqbit session, so NBS could not start its torrent engine. The
client now falls back to an IPv4 socket on `0.0.0.0:0`. `nbs-torrent` likewise
retries the session in IPv4-only mode.

The change is marked with an `NBS:` comment. Drop this folder once upstream
handles hosts without IPv6.
