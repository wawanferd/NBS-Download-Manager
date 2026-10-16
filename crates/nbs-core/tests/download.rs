use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nbs_core::{DownloadManager, DownloadView, EngineConfig, NewDownload, Status};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

#[derive(Clone, Default)]
struct ServerOpts {
    ranges: bool,
    chunk: usize,
    delay: Option<Duration>,
    disposition: Option<&'static str>,
}

#[derive(Default)]
struct Stats {
    current: AtomicUsize,
    max_concurrent: AtomicUsize,
    requests: AtomicUsize,
}

fn test_data(len: usize) -> Arc<Vec<u8>> {
    let mut x: u32 = 0x1234_5678;
    Arc::new(
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect(),
    )
}

async fn serve(data: Arc<Vec<u8>>, opts: ServerOpts) -> (SocketAddr, Arc<Stats>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let stats = Arc::new(Stats::default());
    let st = stats.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else { return };
            let (data, opts, st) = (data.clone(), opts.clone(), st.clone());
            tokio::spawn(async move {
                let (read, mut write) = stream.into_split();
                let mut reader = BufReader::new(read);
                let mut range = None;
                let mut line = String::new();
                loop {
                    line.clear();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let l = line.trim_end();
                    if l.is_empty() {
                        break;
                    }
                    if let Some(v) = l.to_ascii_lowercase().strip_prefix("range: bytes=") {
                        let (a, b) = v.split_once('-').unwrap();
                        let a: usize = a.parse().unwrap();
                        let b: usize = if b.is_empty() { data.len() - 1 } else { b.parse().unwrap() };
                        range = Some((a, b.min(data.len() - 1)));
                    }
                }
                st.requests.fetch_add(1, Ordering::SeqCst);
                let now = st.current.fetch_add(1, Ordering::SeqCst) + 1;
                st.max_concurrent.fetch_max(now, Ordering::SeqCst);

                let mut head = String::new();
                let body = match (opts.ranges, range) {
                    (true, Some((a, b))) => {
                        head.push_str("HTTP/1.1 206 Partial Content\r\n");
                        head.push_str(&format!("Content-Range: bytes {a}-{b}/{}\r\n", data.len()));
                        &data[a..=b]
                    }
                    _ => {
                        head.push_str("HTTP/1.1 200 OK\r\n");
                        &data[..]
                    }
                };
                head.push_str(&format!("Content-Length: {}\r\nConnection: close\r\n", body.len()));
                if opts.ranges {
                    head.push_str("Accept-Ranges: bytes\r\n");
                }
                if let Some(d) = opts.disposition {
                    head.push_str(&format!("Content-Disposition: {d}\r\n"));
                }
                head.push_str("\r\n");
                if write.write_all(head.as_bytes()).await.is_ok() {
                    for chunk in body.chunks(opts.chunk.max(1)) {
                        if write.write_all(chunk).await.is_err() {
                            break;
                        }
                        if let Some(d) = opts.delay {
                            tokio::time::sleep(d).await;
                        }
                    }
                }
                let _ = write.shutdown().await;
                st.current.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
    (addr, stats)
}

fn manager(dir: &Path) -> DownloadManager {
    let config = EngineConfig {
        default_dir: dir.join("out"),
        min_segment_size: 32 * 1024,
        max_retries: 2,
        ..EngineConfig::default()
    };
    DownloadManager::new(dir.join("state.json"), config, None, Arc::new(|_| {})).unwrap()
}

async fn wait_until(m: &DownloadManager, id: &str, timeout: Duration, f: impl Fn(&DownloadView) -> bool) -> DownloadView {
    let deadline = Instant::now() + timeout;
    loop {
        let v = m.get(id).unwrap();
        if f(&v) {
            return v;
        }
        assert!(v.status != Status::Failed || f(&v), "download failed: {:?}", v.error);
        assert!(Instant::now() < deadline, "timed out; last state: {:?} {}/{:?}", v.status, v.downloaded, v.total_size);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn request(url: String, connections: u8) -> NewDownload {
    NewDownload { url, connections: Some(connections), start: true, ..Default::default() }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn segmented_download_produces_identical_file() {
    let data = test_data(3 * 1024 * 1024 + 77);
    let opts = ServerOpts { ranges: true, chunk: 64 * 1024, delay: Some(Duration::from_millis(2)), ..Default::default() };
    let (addr, stats) = serve(data.clone(), opts).await;
    let dir = tempfile::tempdir().unwrap();
    let m = manager(dir.path());

    let d = m.add(request(format!("http://{addr}/files/data.bin"), 6)).unwrap();
    let done = wait_until(&m, &d.id, Duration::from_secs(30), |v| v.status == Status::Completed).await;

    let path = done.final_path.unwrap();
    assert_eq!(path.file_name().unwrap(), "data.bin");
    assert_eq!(std::fs::read(&path).unwrap(), *data);
    assert!(done.resumable);
    assert!(stats.max_concurrent.load(Ordering::SeqCst) >= 4, "segments should download in parallel");
    // Temporary part files are gone.
    let leftovers: Vec<_> = std::fs::read_dir(dir.path().join("out")).unwrap().collect();
    assert_eq!(leftovers.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn server_without_ranges_uses_one_connection() {
    let data = test_data(500_000);
    let opts = ServerOpts { ranges: false, chunk: 32 * 1024, ..Default::default() };
    let (addr, stats) = serve(data.clone(), opts).await;
    let dir = tempfile::tempdir().unwrap();
    let m = manager(dir.path());

    let d = m.add(request(format!("http://{addr}/plain"), 8)).unwrap();
    let done = wait_until(&m, &d.id, Duration::from_secs(30), |v| v.status == Status::Completed).await;

    assert!(!done.resumable);
    assert_eq!(std::fs::read(done.final_path.unwrap()).unwrap(), *data);
    assert_eq!(stats.max_concurrent.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_and_resume_keeps_progress() {
    let data = test_data(2 * 1024 * 1024);
    let opts = ServerOpts { ranges: true, chunk: 8 * 1024, delay: Some(Duration::from_millis(10)), ..Default::default() };
    let (addr, _) = serve(data.clone(), opts).await;
    let dir = tempfile::tempdir().unwrap();
    let m = manager(dir.path());

    let d = m.add(request(format!("http://{addr}/big.iso"), 4)).unwrap();
    wait_until(&m, &d.id, Duration::from_secs(30), |v| v.downloaded > 300_000).await;
    m.pause(&d.id).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let paused = m.get(&d.id).unwrap();
    assert_eq!(paused.status, Status::Paused);
    assert!(paused.downloaded < data.len() as u64);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(m.get(&d.id).unwrap().downloaded, paused.downloaded, "nothing downloads while paused");

    m.resume(&d.id).unwrap();
    // Progress must continue from where it stopped rather than from zero.
    let resumed = wait_until(&m, &d.id, Duration::from_secs(5), |v| v.status == Status::Downloading).await;
    assert!(resumed.downloaded >= paused.downloaded / 2);
    let done = wait_until(&m, &d.id, Duration::from_secs(60), |v| v.status == Status::Completed).await;
    assert_eq!(std::fs::read(done.final_path.unwrap()).unwrap(), *data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_after_restart_from_saved_state() {
    let data = test_data(1024 * 1024);
    let opts = ServerOpts { ranges: true, chunk: 8 * 1024, delay: Some(Duration::from_millis(10)), ..Default::default() };
    let (addr, _) = serve(data.clone(), opts).await;
    let dir = tempfile::tempdir().unwrap();

    let id = {
        let m = manager(dir.path());
        let d = m.add(request(format!("http://{addr}/persist.bin"), 3)).unwrap();
        wait_until(&m, &d.id, Duration::from_secs(30), |v| v.downloaded > 200_000).await;
        m.pause(&d.id).unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        d.id
    };

    let m = manager(dir.path());
    let restored = m.get(&id).unwrap();
    assert_eq!(restored.status, Status::Paused);
    assert!(restored.downloaded > 0);
    m.resume(&id).unwrap();
    let done = wait_until(&m, &id, Duration::from_secs(60), |v| v.status == Status::Completed).await;
    assert_eq!(std::fs::read(done.final_path.unwrap()).unwrap(), *data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn speed_limit_is_respected() {
    let data = test_data(600 * 1024);
    let opts = ServerOpts { ranges: true, chunk: 16 * 1024, ..Default::default() };
    let (addr, _) = serve(data.clone(), opts).await;
    let dir = tempfile::tempdir().unwrap();
    let m = manager(dir.path());

    let started = Instant::now();
    let mut req = request(format!("http://{addr}/limited"), 4);
    req.speed_limit = Some(300 * 1024);
    let d = m.add(req).unwrap();
    let done = wait_until(&m, &d.id, Duration::from_secs(30), |v| v.status == Status::Completed).await;
    let elapsed = started.elapsed();

    assert_eq!(std::fs::read(done.final_path.unwrap()).unwrap(), *data);
    assert!(elapsed >= Duration::from_millis(1600), "finished too fast: {elapsed:?}");
    assert!(elapsed <= Duration::from_secs(5), "finished too slow: {elapsed:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uses_server_file_name_and_avoids_overwriting() {
    let data = test_data(10_000);
    let opts = ServerOpts { ranges: true, chunk: 4096, disposition: Some("attachment; filename=\"report 2024.pdf\""), ..Default::default() };
    let (addr, _) = serve(data.clone(), opts).await;
    let dir = tempfile::tempdir().unwrap();
    let m = manager(dir.path());

    let a = m.add(request(format!("http://{addr}/get?id=1"), 2)).unwrap();
    let a = wait_until(&m, &a.id, Duration::from_secs(30), |v| v.status == Status::Completed).await;
    let b = m.add(request(format!("http://{addr}/get?id=1"), 2)).unwrap();
    let b = wait_until(&m, &b.id, Duration::from_secs(30), |v| v.status == Status::Completed).await;

    assert_eq!(a.file_name, "report 2024.pdf");
    assert_eq!(b.file_name, "report 2024 (1).pdf");
    assert_eq!(std::fs::read(b.final_path.unwrap()).unwrap(), *data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queue_limits_concurrent_downloads() {
    let data = test_data(200_000);
    let opts = ServerOpts { ranges: true, chunk: 8 * 1024, delay: Some(Duration::from_millis(5)), ..Default::default() };
    let (addr, _) = serve(data.clone(), opts).await;
    let dir = tempfile::tempdir().unwrap();
    let m = manager(dir.path());
    m.update_config(|c| c.max_concurrent = 1);

    let a = m.add(request(format!("http://{addr}/a"), 2)).unwrap();
    let b = m.add(request(format!("http://{addr}/b"), 2)).unwrap();
    assert_eq!(m.get(&b.id).unwrap().status, Status::Queued);
    wait_until(&m, &a.id, Duration::from_secs(30), |v| v.status == Status::Completed).await;
    wait_until(&m, &b.id, Duration::from_secs(30), |v| v.status == Status::Completed).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_connections_split_segments_and_merge_early() {
    let data = test_data(2 * 1024 * 1024);
    let opts = ServerOpts { ranges: true, chunk: 8 * 1024, delay: Some(Duration::from_millis(8)), ..Default::default() };
    let (addr, stats) = serve(data.clone(), opts).await;
    let dir = tempfile::tempdir().unwrap();
    let m = manager(dir.path());

    let d = m.add(request(format!("http://{addr}/split.bin"), 2)).unwrap();
    wait_until(&m, &d.id, Duration::from_secs(30), |v| v.downloaded > 100_000).await;
    assert_eq!(m.get(&d.id).unwrap().segments.len(), 2);

    // More connections than segments: running segments get split.
    m.set_connections(&d.id, 6).unwrap();
    let busy = wait_until(&m, &d.id, Duration::from_secs(10), |v| v.active_connections == 6).await;
    assert!(busy.segments.len() >= 6);

    // The first segment is appended to the output before the download ends.
    wait_until(&m, &d.id, Duration::from_secs(60), |v| {
        v.status == Status::Completed || (v.segments[0].merged && v.downloaded < v.total_size.unwrap())
    })
    .await;
    let done = wait_until(&m, &d.id, Duration::from_secs(60), |v| v.status == Status::Completed).await;
    assert_eq!(std::fs::read(done.final_path.unwrap()).unwrap(), *data);
    assert!(stats.max_concurrent.load(Ordering::SeqCst) >= 6);
}
