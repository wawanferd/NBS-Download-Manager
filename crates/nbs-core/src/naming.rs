//! File name detection and sanitizing.

use std::path::{Path, PathBuf};

use percent_encoding::percent_decode_str;

/// Extracts the file name from a `Content-Disposition` header value,
/// preferring the RFC 5987 `filename*` parameter.
pub fn parse_content_disposition(value: &str) -> Option<String> {
    let mut plain = None;
    let mut extended = None;
    for part in split_params(value) {
        let Some((key, val)) = part.split_once('=') else { continue };
        let key = key.trim().to_ascii_lowercase();
        let val = val.trim();
        match key.as_str() {
            "filename*" => {
                // charset'language'percent-encoded-value
                let encoded = val.splitn(3, '\'').nth(2).unwrap_or(val);
                let decoded = percent_decode_str(unquote(encoded)).decode_utf8_lossy().into_owned();
                if !decoded.is_empty() {
                    extended = Some(decoded);
                }
            }
            "filename" => {
                let v = unquote(val);
                let decoded = percent_decode_str(v).decode_utf8_lossy().into_owned();
                if !decoded.is_empty() {
                    plain = Some(decoded);
                }
            }
            _ => {}
        }
    }
    extended.or(plain)
}

/// Splits header parameters on `;`, ignoring separators inside quotes.
fn split_params(value: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut in_quotes = false;
    let mut start = 0;
    for (i, c) in value.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            ';' if !in_quotes => {
                parts.push(&value[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&value[start..]);
    parts
}

fn unquote(s: &str) -> &str {
    s.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(s)
}

/// Derives a file name from the last path segment of a URL.
pub fn file_name_from_url(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let last = parsed.path_segments()?.rev().find(|s| !s.is_empty())?;
    let decoded = percent_decode_str(last).decode_utf8_lossy().into_owned();
    (!decoded.is_empty()).then_some(decoded)
}

/// Best-effort extension for common content types, used when a name has none.
pub fn extension_for_mime(mime: &str) -> Option<&'static str> {
    let mime = mime.split(';').next()?.trim().to_ascii_lowercase();
    Some(match mime.as_str() {
        "application/zip" => "zip",
        "application/pdf" => "pdf",
        "application/x-7z-compressed" => "7z",
        "application/x-rar-compressed" | "application/vnd.rar" => "rar",
        "application/gzip" => "gz",
        "application/x-msdownload" => "exe",
        "application/x-iso9660-image" => "iso",
        "application/json" => "json",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/x-matroska" => "mkv",
        "audio/mpeg" => "mp3",
        "audio/ogg" => "ogg",
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "text/plain" => "txt",
        "text/html" => "html",
        _ => return None,
    })
}

/// Makes a name safe to use as a file name on every desktop platform.
pub fn sanitize_file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let mut out: String = base
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = out.trim().trim_end_matches(['.', ' ']).to_string();
    out = trimmed;
    if out.chars().count() > 200 {
        let (stem, ext) = split_ext(&out);
        let keep = 200usize.saturating_sub(ext.chars().count() + 1);
        let stem: String = stem.chars().take(keep).collect();
        out = if ext.is_empty() { stem } else { format!("{stem}.{ext}") };
    }
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let stem_upper = split_ext(&out).0.to_ascii_uppercase();
    if RESERVED.contains(&stem_upper.as_str()) {
        out.insert(0, '_');
    }
    if out.is_empty() || out == "." || out == ".." {
        "download".to_string()
    } else {
        out
    }
}

fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i + 1..]),
        _ => (name, ""),
    }
}

/// Returns `dir/name`, or `dir/name (n).ext` if that path is taken.
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = split_ext(name);
    (1..)
        .map(|n| {
            let file = if ext.is_empty() { format!("{stem} ({n})") } else { format!("{stem} ({n}).{ext}") };
            dir.join(file)
        })
        .find(|p| !p.exists())
        .expect("unbounded iterator")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_disposition_variants() {
        assert_eq!(parse_content_disposition("attachment; filename=\"a b.zip\"").as_deref(), Some("a b.zip"));
        assert_eq!(parse_content_disposition("attachment; filename=plain.iso").as_deref(), Some("plain.iso"));
        assert_eq!(
            parse_content_disposition("attachment; filename=\"fallback.txt\"; filename*=UTF-8''r%C3%A9sum%C3%A9.pdf").as_deref(),
            Some("résumé.pdf")
        );
        assert_eq!(parse_content_disposition("attachment; filename=\"semi;colon.txt\"").as_deref(), Some("semi;colon.txt"));
        assert_eq!(parse_content_disposition("inline"), None);
    }

    #[test]
    fn url_names() {
        assert_eq!(file_name_from_url("https://x.org/a/b/file%20name.tar.gz?x=1").as_deref(), Some("file name.tar.gz"));
        assert_eq!(file_name_from_url("https://x.org/dir/").as_deref(), Some("dir"));
        assert_eq!(file_name_from_url("https://x.org/"), None);
    }

    #[test]
    fn sanitizing() {
        assert_eq!(sanitize_file_name("a<b>:c?.txt"), "a_b__c_.txt");
        assert_eq!(sanitize_file_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_file_name("CON.txt"), "_CON.txt");
        assert_eq!(sanitize_file_name("  "), "download");
        assert_eq!(sanitize_file_name("name. "), "name");
    }

    #[test]
    fn unique_names() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(unique_path(dir.path(), "f.bin"), dir.path().join("f.bin"));
        std::fs::write(dir.path().join("f.bin"), b"x").unwrap();
        assert_eq!(unique_path(dir.path(), "f.bin"), dir.path().join("f (1).bin"));
    }
}
