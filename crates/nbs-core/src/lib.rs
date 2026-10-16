//! Segmented, resumable HTTP download engine used by NBS Download Manager.
//!
//! * Up to eight connections per download, each fetching its own byte range
//!   into a part file.
//! * Idle connections split the largest remaining segment (dynamic
//!   segmentation), so all connections stay busy until the end.
//! * Finished segments are appended to the output file in order while the
//!   download is still running (dynamic merging), leaving almost nothing to
//!   do when the last byte arrives.
//! * Pause/resume with progress persisted to disk, global and per-download
//!   speed limits, and smoothed speed / ETA reporting.

mod engine;
mod error;
mod limiter;
mod manager;
pub mod naming;
mod probe;
mod store;
mod types;

pub use error::{Error, Result};
pub use limiter::RateLimiter;
pub use manager::{DownloadManager, EventSink};
pub use types::*;
