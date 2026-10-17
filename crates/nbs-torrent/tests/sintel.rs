//! Network test: `cargo test -p nbs-torrent -- --ignored --nocapture`.
//! Fetches the metadata of Sintel (a Creative Commons film), streams its first
//! 256 KiB through the local HTTP server and reads a subtitle.

use std::sync::Arc;
use std::time::Duration;

use nbs_torrent::*;

const SINTEL: &str = "magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10&dn=Sintel&tr=udp%3A%2F%2Fexplodie.org%3A6969&tr=udp%3A%2F%2Ftracker.opentrackr.org%3A1337&tr=udp%3A%2F%2Ftracker.leechers-paradise.org%3A6969&tr=udp%3A%2F%2Ftracker.empire-js.us%3A1337&tr=wss%3A%2F%2Ftracker.webtorrent.dev";

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn inspect_stream_and_remove_sintel() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = TorrentEngine::start(
        TorrentConfig {
            data_dir: tmp.path().join("data"),
            download_dir: tmp.path().join("dl"),
            cache_dir: tmp.path().join("cache"),
            listen_port: None,
            download_limit: None,
            upload_limit: None,
            upload_mode: UploadMode::Disabled,
        },
        Arc::new(|_| {}),
    )
    .await
    .unwrap();

    let preview = engine
        .inspect(
            TorrentSource::Link(SINTEL.into()),
            &[],
            Duration::from_secs(120),
        )
        .await
        .unwrap();
    println!("{} ({} bytes)", preview.name, preview.total_size);
    let video = preview
        .files
        .iter()
        .filter(|f| f.kind == FileKind::Video)
        .max_by_key(|f| f.size)
        .unwrap()
        .index;
    let subtitle = preview
        .files
        .iter()
        .find(|f| f.name.ends_with(".srt"))
        .unwrap()
        .index;

    let view = engine
        .add(AddTorrent {
            id: preview.id.clone(),
            mode: AddMode::Stream,
            files: vec![],
            save_dir: None,
            start: true,
            source: None,
        })
        .await
        .unwrap();
    assert!(view.stream);
    assert!(view.files.iter().any(|f| f.index == video && f.included));
    assert!(
        view.files.iter().any(|f| f.index == subtitle && f.included),
        "subtitles are included when streaming"
    );

    let url = engine.stream_url(&view.id, video).await.unwrap();
    println!("stream {url}");
    let resp = reqwest::Client::new()
        .get(&url)
        .header("Range", "bytes=0-262143")
        .timeout(Duration::from_secs(180))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 206);
    assert_eq!(resp.bytes().await.unwrap().len(), 262_144);

    let vtt = engine.subtitle(&view.id, subtitle).await.unwrap();
    assert!(vtt.starts_with("WEBVTT"), "{}", &vtt[..vtt.len().min(200)]);

    let kept = engine.keep(&view.id).await.unwrap();
    assert!(!kept.stream && kept.save_dir.starts_with(tmp.path().join("dl")));
    assert_eq!(engine.get(&view.id).unwrap().id, view.id);

    engine.remove(&view.id, true).await.unwrap();
    assert!(engine.list().is_empty());
}
