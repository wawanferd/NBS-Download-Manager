//! The download task: probes the server, plans segments, runs one worker per
//! connection, re-splits the largest remaining segment whenever a connection
//! becomes idle, and appends finished segments to the output file in order
//! while the rest are still downloading (dynamic merging).

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use reqwest::header;
use reqwest::StatusCode;
use tokio::io::{AsyncSeekExt, AsyncWriteExt, BufWriter};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::error::{Error, Result};
use crate::manager::{Core, Handle};
use crate::naming::unique_path;
use crate::probe::{parse_content_range_start, probe, with_headers};
use crate::types::{DownloadRecord, Segment, Status, MAX_CONNECTIONS};

const OUTPUT_NAME: &str = "output.part";

/// Serializes picking a free final file name across downloads.
static FINALIZE_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn temp_dir(rec: &DownloadRecord) -> PathBuf {
    rec.save_dir.join(format!(".nbs-{}", &rec.id[..rec.id.len().min(12)]))
}

fn part_path(temp: &Path, seg_id: u32) -> PathBuf {
    temp.join(format!("seg-{seg_id}.part"))
}

pub(crate) fn now_millis() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

async fn cancellable<F: Future>(cancel: &CancellationToken, fut: F) -> Result<F::Output> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(Error::Cancelled),
        v = fut => Ok(v),
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> io::Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Error::Protocol(format!("background task failed: {e}")))?
        .map_err(Error::from)
}

/// Splits `total` bytes into up to `connections` equal segments, none smaller
/// than `min_size`. Unknown or non-resumable downloads get a single segment.
pub(crate) fn plan_segments(total: Option<u64>, resumable: bool, connections: u8, min_size: u64) -> Vec<Segment> {
    match (resumable, total) {
        (true, Some(total)) => {
            let wanted = connections.clamp(1, MAX_CONNECTIONS) as u64;
            let n = wanted.min((total / min_size.max(1)).max(1));
            let base = total / n;
            (0..n)
                .map(|i| {
                    let start = i * base;
                    let len = if i == n - 1 { total - start } else { base };
                    Segment { id: i as u32, start, len: Some(len), downloaded: 0, merged: false }
                })
                .collect()
        }
        (_, total) => vec![Segment { id: 0, start: 0, len: total, downloaded: 0, merged: false }],
    }
}

/// Halves the incomplete segment with the most bytes left, creating a new
/// segment for its second half. Returns the new segment id.
pub(crate) fn split_largest(rec: &mut DownloadRecord, min_size: u64) -> Option<u32> {
    let (idx, remaining) = rec
        .segments
        .iter()
        .enumerate()
        .filter_map(|(i, s)| s.remaining().map(|r| (i, r)))
        .max_by_key(|&(_, r)| r)?;
    if remaining < min_size.max(1) * 2 {
        return None;
    }
    let victim = &mut rec.segments[idx];
    let keep = victim.downloaded + remaining / 2;
    let new_start = victim.start + keep;
    let new_len = victim.len? - keep;
    victim.len = Some(keep);

    let id = rec.next_segment_id;
    rec.next_segment_id += 1;
    rec.segments.insert(idx + 1, Segment { id, start: new_start, len: Some(new_len), downloaded: 0, merged: false });
    Some(id)
}

/// Reconciles saved progress with what is actually on disk. Returns `false`
/// when the merged output is unusable and the download must start over.
fn verify_files(rec: &mut DownloadRecord, temp: &Path) -> io::Result<bool> {
    let output = temp.join(OUTPUT_NAME);
    match fs::metadata(&output).map(|m| m.len()) {
        Ok(len) if len < rec.merged_bytes => return Ok(false),
        Ok(len) if len > rec.merged_bytes => OpenOptions::new().write(true).open(&output)?.set_len(rec.merged_bytes)?,
        Err(_) if rec.merged_bytes > 0 => return Ok(false),
        _ => {}
    }
    for seg in rec.segments.iter_mut().filter(|s| !s.merged) {
        let on_disk = fs::metadata(part_path(temp, seg.id)).map(|m| m.len()).unwrap_or(0);
        if on_disk < seg.downloaded {
            seg.downloaded = on_disk;
        }
    }
    Ok(true)
}

/// Appends a finished part file to the output (or moves it into place when
/// it is the first one) and deletes it.
fn append_part(part: &Path, output: &Path, len: u64, merged: u64) -> io::Result<()> {
    let part_len = match fs::metadata(part) {
        Ok(m) => m.len(),
        Err(_) if len == 0 => 0,
        Err(e) => return Err(e),
    };
    if part_len < len {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "part file is shorter than its segment"));
    }
    if merged == 0 && !output.exists() {
        if len == 0 {
            File::create(output)?;
            let _ = fs::remove_file(part);
        } else {
            OpenOptions::new().write(true).open(part)?.set_len(len)?;
            fs::rename(part, output)?;
        }
        return Ok(());
    }
    let mut dst = OpenOptions::new().write(true).create(true).truncate(false).open(output)?;
    dst.set_len(merged)?;
    dst.seek(SeekFrom::End(0))?;
    if len > 0 {
        let src = File::open(part)?;
        let copied = io::copy(&mut src.take(len), &mut dst)?;
        if copied != len {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short copy while merging"));
        }
    }
    dst.sync_data()?;
    drop(dst);
    let _ = fs::remove_file(part);
    Ok(())
}

/// Entry point of a download task.
pub(crate) async fn run(core: Arc<Core>, h: Arc<Handle>, cancel: CancellationToken, gen: u64) {
    let result = run_inner(&core, &h, &cancel).await;

    let is_current = h.control.lock().as_ref().map(|c| c.gen) == Some(gen);
    if !is_current {
        // A newer task took over (e.g. quick pause/resume); it owns the status.
        return;
    }
    let completed = {
        let mut rec = h.record.lock();
        match &result {
            Ok(()) => {}
            Err(Error::Cancelled) => {
                if rec.status.is_active() {
                    rec.status = Status::Paused;
                }
            }
            Err(e) => {
                rec.status = Status::Failed;
                rec.error = Some(e.to_string());
            }
        }
        rec.status == Status::Completed
    };
    h.active_segments.lock().clear();
    h.speed.lock().reset();
    {
        let mut control = h.control.lock();
        if control.as_ref().map(|c| c.gen) == Some(gen) {
            *control = None;
        }
    }
    core.persist();
    core.emit_update(&h);
    if completed {
        core.emit_completed(&h);
    }
    core.schedule();
}

fn set_status(core: &Core, h: &Handle, cancel: &CancellationToken, status: Status) {
    if cancel.is_cancelled() {
        return;
    }
    h.record.lock().status = status;
    core.emit_update(h);
}

async fn run_inner(core: &Arc<Core>, h: &Arc<Handle>, cancel: &CancellationToken) -> Result<()> {
    set_status(core, h, cancel, Status::Connecting);
    let (url, headers) = {
        let rec = h.record.lock();
        (rec.url.clone(), rec.headers.clone())
    };
    let info = cancellable(cancel, probe(&core.client, &url, &headers)).await??;
    let min_size = core.config.read().min_segment_size;

    let (temp, reset) = {
        let mut rec = h.record.lock();
        let changed_remotely = rec.total_size != info.total_size
            || matches!((&rec.etag, &info.etag), (Some(a), Some(b)) if a != b);
        let mut reset = rec.segments.is_empty() || !rec.resumable || !info.resumable || changed_remotely;
        let temp = temp_dir(&rec);
        if !reset {
            reset = !verify_files(&mut rec, &temp)?;
        }
        if reset {
            let resumable = info.resumable && info.total_size.is_some();
            rec.segments = plan_segments(info.total_size, resumable, rec.connections, min_size);
            rec.next_segment_id = rec.segments.len() as u32;
            rec.merged_bytes = 0;
            rec.total_size = info.total_size;
            rec.resumable = resumable;
            rec.etag = info.etag.clone();
            rec.last_modified = info.last_modified.clone();
            rec.content_type = info.content_type.clone();
            if rec.auto_name {
                rec.file_name = info.file_name.clone();
            }
        }
        rec.error = None;
        (temp, reset)
    };
    {
        let temp = temp.clone();
        blocking(move || {
            if reset && temp.exists() {
                fs::remove_dir_all(&temp)?;
            }
            fs::create_dir_all(&temp)
        })
        .await?;
    }
    core.persist();
    set_status(core, h, cancel, Status::Downloading);

    merge_ready(h, &temp).await?;
    let mut run = TaskRun {
        core: core.clone(),
        h: h.clone(),
        temp: temp.clone(),
        cancel: cancel.child_token(),
        active: HashSet::new(),
        workers: JoinSet::new(),
    };
    run.fill();
    while !run.workers.is_empty() {
        tokio::select! {
            joined = run.workers.join_next() => {
                let Some(joined) = joined else { break };
                let (seg_id, result) = joined.map_err(|e| Error::Protocol(format!("worker crashed: {e}")))?;
                run.active.remove(&seg_id);
                h.active_segments.lock().remove(&seg_id);
                match result {
                    Ok(()) => {
                        merge_ready(h, &temp).await?;
                        core.persist();
                        run.fill();
                    }
                    Err(e) => {
                        run.cancel.cancel();
                        while run.workers.join_next().await.is_some() {}
                        return Err(if cancel.is_cancelled() { Error::Cancelled } else { e });
                    }
                }
            }
            _ = h.rebalance.notified() => run.fill(),
        }
    }
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    if !h.record.lock().segments.iter().all(Segment::is_complete) {
        return Err(Error::Protocol("download ended with incomplete segments".into()));
    }

    set_status(core, h, cancel, Status::Merging);
    merge_ready(h, &temp).await?;
    let (dir, name) = {
        let rec = h.record.lock();
        (rec.save_dir.clone(), rec.file_name.clone())
    };
    let final_path = blocking(move || {
        let _guard = FINALIZE_LOCK.lock();
        fs::create_dir_all(&dir)?;
        let target = unique_path(&dir, &name);
        let output = temp.join(OUTPUT_NAME);
        if fs::rename(&output, &target).is_err() {
            fs::copy(&output, &target)?;
        }
        let _ = fs::remove_dir_all(&temp);
        Ok(target)
    })
    .await?;

    let mut rec = h.record.lock();
    rec.file_name = final_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(rec.file_name.clone());
    rec.final_path = Some(final_path);
    rec.total_size = Some(rec.downloaded());
    rec.status = Status::Completed;
    rec.completed_at = Some(now_millis());
    Ok(())
}

/// Appends every finished segment that directly follows the merged prefix.
///
/// A worker claims its last bytes in the record before writing and flushing
/// them, so a segment counts as finished only once its worker has returned.
async fn merge_ready(h: &Arc<Handle>, temp: &Path) -> Result<()> {
    loop {
        let next = {
            let rec = h.record.lock();
            let active = h.active_segments.lock();
            let merged = rec.merged_bytes;
            rec.segments
                .iter()
                .find(|s| !s.merged && s.start == merged && s.is_complete() && !active.contains(&s.id))
                .map(|s| (s.id, s.len.unwrap_or(0), merged))
        };
        let Some((id, len, merged)) = next else { return Ok(()) };
        let part = part_path(temp, id);
        let output = temp.join(OUTPUT_NAME);
        blocking(move || append_part(&part, &output, len, merged)).await?;
        let mut rec = h.record.lock();
        if let Some(seg) = rec.segments.iter_mut().find(|s| s.id == id) {
            seg.merged = true;
        }
        rec.merged_bytes = merged + len;
    }
}

struct TaskRun {
    core: Arc<Core>,
    h: Arc<Handle>,
    temp: PathBuf,
    cancel: CancellationToken,
    active: HashSet<u32>,
    workers: JoinSet<(u32, Result<()>)>,
}

impl TaskRun {
    /// Starts workers until the connection count is reached, splitting
    /// running segments when there is no unassigned work left.
    fn fill(&mut self) {
        let min_size = self.core.config.read().min_segment_size;
        loop {
            let next = {
                let mut rec = self.h.record.lock();
                let target = if rec.resumable { rec.connections.clamp(1, MAX_CONNECTIONS) as usize } else { 1 };
                if self.active.len() >= target {
                    None
                } else if let Some(seg) = rec.segments.iter().find(|s| !s.is_complete() && !self.active.contains(&s.id)) {
                    Some(seg.id)
                } else if rec.resumable {
                    split_largest(&mut rec, min_size)
                } else {
                    None
                }
            };
            let Some(seg_id) = next else { return };
            self.active.insert(seg_id);
            self.h.active_segments.lock().insert(seg_id);
            let (core, h, temp, cancel) = (self.core.clone(), self.h.clone(), self.temp.clone(), self.cancel.clone());
            self.workers.spawn(async move { (seg_id, segment_worker(core, h, seg_id, temp, cancel).await) });
        }
    }
}

fn segment_progress(h: &Handle, seg_id: u32) -> u64 {
    h.record.lock().segments.iter().find(|s| s.id == seg_id).map(|s| s.downloaded).unwrap_or(0)
}

/// Downloads one segment, retrying transient failures with backoff.
async fn segment_worker(core: Arc<Core>, h: Arc<Handle>, seg_id: u32, temp: PathBuf, cancel: CancellationToken) -> Result<()> {
    let max_retries = core.config.read().max_retries;
    let mut attempt = 0u32;
    loop {
        let before = segment_progress(&h, seg_id);
        let err = match fetch_segment(&core, &h, seg_id, &temp, &cancel).await {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if segment_progress(&h, seg_id) > before {
            attempt = 0;
        }
        if !err.is_retryable() || attempt >= max_retries {
            return Err(err);
        }
        attempt += 1;
        let delay = Duration::from_millis(500 * 2u64.pow(attempt.min(6))).min(Duration::from_secs(30));
        cancellable(&cancel, tokio::time::sleep(delay)).await?;
    }
}

async fn fetch_segment(core: &Core, h: &Handle, seg_id: u32, temp: &Path, cancel: &CancellationToken) -> Result<()> {
    let (url, headers, resumable, seg) = {
        let rec = h.record.lock();
        let seg = rec.segments.iter().find(|s| s.id == seg_id).cloned().ok_or(Error::Protocol(format!("segment {seg_id} vanished")))?;
        (rec.url.clone(), rec.headers.clone(), rec.resumable, seg)
    };
    if seg.is_complete() {
        return Ok(());
    }

    let path = part_path(temp, seg_id);
    let mut file = tokio::fs::OpenOptions::new().create(true).write(true).truncate(false).open(&path).await?;
    let on_disk = file.metadata().await?.len();
    // Without range support every attempt starts from scratch.
    let downloaded = if resumable { seg.downloaded.min(on_disk) } else { 0 };
    if downloaded != seg.downloaded {
        let mut rec = h.record.lock();
        if let Some(s) = rec.segments.iter_mut().find(|s| s.id == seg_id) {
            s.downloaded = downloaded;
        }
    }
    file.set_len(downloaded).await?;
    file.seek(SeekFrom::Start(downloaded)).await?;

    let pos = seg.start + downloaded;
    let mut req = with_headers(core.client.get(&url), &headers);
    if resumable {
        let range = match seg.len {
            Some(len) => format!("bytes={pos}-{}", seg.start + len - 1),
            None => format!("bytes={pos}-"),
        };
        req = req.header(header::RANGE, range);
    }
    let mut resp = cancellable(cancel, req.send()).await??;
    let status = resp.status();
    if resumable {
        if status != StatusCode::PARTIAL_CONTENT {
            return Err(if status.is_success() {
                Error::Protocol("the server stopped honoring range requests".into())
            } else {
                Error::Status(status.as_u16())
            });
        }
        let start = resp.headers().get(header::CONTENT_RANGE).and_then(|v| v.to_str().ok()).and_then(parse_content_range_start);
        if start.is_some_and(|s| s != pos) {
            return Err(Error::Protocol(format!("server returned the wrong range (expected offset {pos})")));
        }
    } else if !status.is_success() {
        return Err(Error::Status(status.as_u16()));
    }

    let mut writer = BufWriter::with_capacity(256 * 1024, file);
    loop {
        let chunk = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                let _ = writer.flush().await;
                return Err(Error::Cancelled);
            }
            chunk = resp.chunk() => chunk?,
        };
        let Some(chunk) = chunk else { break };

        if cancellable(cancel, async {
            core.global.acquire(chunk.len()).await;
            h.limiter.acquire(chunk.len()).await;
        })
        .await
        .is_err()
        {
            let _ = writer.flush().await;
            return Err(Error::Cancelled);
        }

        // Claim the bytes before writing; the segment may have been shortened
        // by a split in the meantime.
        let (allowed, done) = {
            let mut rec = h.record.lock();
            let seg = rec.segments.iter_mut().find(|s| s.id == seg_id).ok_or(Error::Protocol(format!("segment {seg_id} vanished")))?;
            let allowed = match seg.remaining() {
                Some(rem) => rem.min(chunk.len() as u64) as usize,
                None => chunk.len(),
            };
            seg.downloaded += allowed as u64;
            (allowed, seg.is_complete())
        };
        h.counter.fetch_add(allowed as u64, Ordering::Relaxed);
        writer.write_all(&chunk[..allowed]).await?;
        if done {
            break;
        }
    }
    writer.flush().await?;
    writer.into_inner().sync_data().await?;

    let mut rec = h.record.lock();
    let seg = rec.segments.iter_mut().find(|s| s.id == seg_id).ok_or(Error::Protocol(format!("segment {seg_id} vanished")))?;
    match seg.len {
        None => {
            // Unknown size: the stream ending is the end of the file.
            seg.len = Some(seg.downloaded);
            Ok(())
        }
        Some(_) if seg.is_complete() => Ok(()),
        Some(_) => Err(Error::Incomplete),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(segments: Vec<Segment>) -> DownloadRecord {
        DownloadRecord {
            id: "0123456789abcdef".into(),
            url: "http://x".into(),
            file_name: "f".into(),
            auto_name: false,
            save_dir: PathBuf::new(),
            final_path: None,
            total_size: None,
            resumable: true,
            connections: 4,
            speed_limit: None,
            status: Status::Paused,
            error: None,
            next_segment_id: segments.len() as u32,
            segments,
            merged_bytes: 0,
            headers: vec![],
            etag: None,
            last_modified: None,
            content_type: None,
            source: None,
            created_at: 0,
            completed_at: None,
        }
    }

    #[test]
    fn plans_cover_whole_file() {
        let segs = plan_segments(Some(10_000_003), true, 8, 1024);
        assert_eq!(segs.len(), 8);
        assert_eq!(segs[0].start, 0);
        let mut next = 0;
        for s in &segs {
            assert_eq!(s.start, next);
            next = s.start + s.len.unwrap();
        }
        assert_eq!(next, 10_000_003);
    }

    #[test]
    fn small_files_get_fewer_segments() {
        assert_eq!(plan_segments(Some(1000), true, 8, 400).len(), 2);
        assert_eq!(plan_segments(Some(0), true, 8, 400).len(), 1);
        assert_eq!(plan_segments(Some(1 << 30), false, 8, 400).len(), 1);
        assert_eq!(plan_segments(None, true, 8, 400)[0].len, None);
    }

    #[test]
    fn split_halves_largest_remaining() {
        let mut rec = record(plan_segments(Some(4000), true, 2, 100));
        rec.segments[0].downloaded = 1500; // 500 left
        rec.segments[1].downloaded = 200; // 1800 left
        let id = split_largest(&mut rec, 100).unwrap();
        assert_eq!(id, 2);
        assert_eq!(rec.segments.len(), 3);
        let (a, b) = (&rec.segments[1], &rec.segments[2]);
        assert_eq!(a.len, Some(200 + 900));
        assert_eq!(b.start, a.start + 1100);
        assert_eq!(b.end_exclusive(), Some(4000));
        // Segments stay sorted and contiguous.
        let mut next = 0;
        for s in &rec.segments {
            assert_eq!(s.start, next);
            next = s.end_exclusive().unwrap();
        }
    }

    #[test]
    fn split_refuses_tiny_segments() {
        let mut rec = record(plan_segments(Some(300), true, 1, 100));
        rec.segments[0].downloaded = 150;
        assert_eq!(split_largest(&mut rec, 100), None);
    }

    #[test]
    fn merging_appends_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join(OUTPUT_NAME);
        let p0 = part_path(dir.path(), 0);
        let p1 = part_path(dir.path(), 1);
        fs::write(&p0, b"hello ").unwrap();
        fs::write(&p1, b"world!!").unwrap();
        append_part(&p0, &out, 6, 0).unwrap();
        append_part(&p1, &out, 5, 6).unwrap();
        assert_eq!(fs::read(&out).unwrap(), b"hello world");
        assert!(!p0.exists() && !p1.exists());
    }
}
