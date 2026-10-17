use std::path::Path;

use crate::types::FileKind;

pub fn kind_of(name: &str) -> FileKind {
    let ext = Path::new(name).extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "mp4" | "mkv" | "webm" | "avi" | "mov" | "m4v" | "wmv" | "flv" | "ts" | "m2ts" | "mpg" | "mpeg" | "3gp" | "ogv" => {
            FileKind::Video
        }
        "mp3" | "flac" | "aac" | "m4a" | "ogg" | "opus" | "wav" | "wma" => FileKind::Audio,
        "srt" | "ass" | "ssa" | "vtt" | "sub" => FileKind::Subtitle,
        _ => FileKind::Other,
    }
}

/// Turns a bare info-hash (40 hex or 32 base32 characters) into a magnet link.
pub fn normalize_link(link: &str) -> String {
    let link = link.trim();
    let is_hash = (link.len() == 40 && link.chars().all(|c| c.is_ascii_hexdigit()))
        || (link.len() == 32 && link.chars().all(|c| c.is_ascii_alphanumeric()));
    if is_hash {
        format!("magnet:?xt=urn:btih:{link}")
    } else {
        link.to_string()
    }
}

/// Converts SubRip subtitles to WebVTT, which is what `<track>` understands.
/// WebVTT input is returned unchanged.
pub fn to_webvtt(text: &str) -> String {
    let text = text.trim_start_matches('\u{feff}').replace("\r\n", "\n").replace('\r', "\n");
    if text.trim_start().starts_with("WEBVTT") {
        return text;
    }
    let mut out = String::from("WEBVTT\n\n");
    for line in text.lines() {
        if line.contains("-->") {
            // 00:00:01,000 --> 00:00:02,500  =>  00:00:01.000 --> 00:00:02.500
            out.push_str(&line.replace(',', "."));
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

pub fn now_millis() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_file_kinds() {
        assert_eq!(kind_of("Movie.MKV"), FileKind::Video);
        assert_eq!(kind_of("a.en.srt"), FileKind::Subtitle);
        assert_eq!(kind_of("song.flac"), FileKind::Audio);
        assert_eq!(kind_of("poster.jpg"), FileKind::Other);
    }

    #[test]
    fn info_hashes_become_magnets() {
        let hex = "08ada5a7a6183aae1e09d831df6748d566095a10";
        assert_eq!(normalize_link(hex), format!("magnet:?xt=urn:btih:{hex}"));
        assert_eq!(normalize_link(" magnet:?xt=urn:btih:abc "), "magnet:?xt=urn:btih:abc");
        assert_eq!(normalize_link("https://x.org/a.torrent"), "https://x.org/a.torrent");
    }

    #[test]
    fn converts_srt_to_vtt() {
        let srt =
            "\u{feff}1\r\n00:00:01,000 --> 00:00:02,500\r\nHello, world\r\n\r\n2\r\n00:01:00,000 --> 00:01:01,000\r\nBye\r\n";
        let vtt = to_webvtt(srt);
        assert!(vtt.starts_with("WEBVTT\n\n1\n00:00:01.000 --> 00:00:02.500\nHello, world\n"));
        assert!(vtt.contains("00:01:00.000 --> 00:01:01.000\nBye"));
        assert_eq!(to_webvtt("WEBVTT\n\n00:01.000 --> 00:02.000\nx"), "WEBVTT\n\n00:01.000 --> 00:02.000\nx");
    }
}
