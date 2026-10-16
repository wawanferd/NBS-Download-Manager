use reqwest::header::{self, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Client, RequestBuilder, StatusCode};

use crate::error::{Error, Result};
use crate::naming::{extension_for_mime, file_name_from_url, parse_content_disposition, sanitize_file_name};
use crate::types::ProbeResult;

/// Adds user supplied headers (cookies, referrer, user agent from the browser).
pub(crate) fn with_headers(mut req: RequestBuilder, headers: &[(String, String)]) -> RequestBuilder {
    let mut map = HeaderMap::new();
    for (k, v) in headers {
        if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(v)) {
            map.insert(name, value);
        }
    }
    req = req.headers(map);
    // Ranges and compression do not mix: always ask for the raw bytes.
    req.header(header::ACCEPT_ENCODING, "identity")
}

/// Parses `bytes 0-0/1234` into the total size.
pub(crate) fn parse_content_range_total(value: &str) -> Option<u64> {
    value.trim().strip_prefix("bytes")?.trim().split_once('/')?.1.trim().parse().ok()
}

/// Parses the first byte offset of `bytes 100-199/1234`.
pub(crate) fn parse_content_range_start(value: &str) -> Option<u64> {
    value.trim().strip_prefix("bytes")?.trim().split_once('-')?.0.trim().parse().ok()
}

/// Inspects a URL with a one-byte range request to learn its size, name and
/// whether the server supports resuming / segmented downloads.
pub async fn probe(client: &Client, url: &str, headers: &[(String, String)]) -> Result<ProbeResult> {
    let parsed = reqwest::Url::parse(url).map_err(|e| Error::Invalid(format!("{url}: {e}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(Error::Invalid(format!("unsupported URL scheme: {}", parsed.scheme())));
    }

    let mut resp = with_headers(client.get(url), headers)
        .header(header::RANGE, "bytes=0-0")
        .send()
        .await?;
    if resp.status() == StatusCode::RANGE_NOT_SATISFIABLE {
        // Typically an empty file; ask again without a range.
        resp = with_headers(client.get(url), headers).send().await?;
    }
    let status = resp.status();
    if !status.is_success() {
        return Err(Error::Status(status.as_u16()));
    }

    let h = resp.headers();
    let text = |name: header::HeaderName| h.get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
    let (total_size, resumable) = if status == StatusCode::PARTIAL_CONTENT {
        let total = text(header::CONTENT_RANGE).as_deref().and_then(parse_content_range_total);
        (total, total.is_some())
    } else {
        (resp.content_length(), false)
    };
    let content_type = text(header::CONTENT_TYPE);
    let final_url = resp.url().to_string();

    let mut name = text(header::CONTENT_DISPOSITION)
        .as_deref()
        .and_then(parse_content_disposition)
        .or_else(|| file_name_from_url(&final_url))
        .or_else(|| file_name_from_url(url))
        .unwrap_or_else(|| "download".to_string());
    if !name.contains('.') {
        if let Some(ext) = content_type.as_deref().and_then(extension_for_mime) {
            name = format!("{name}.{ext}");
        }
    }

    Ok(ProbeResult {
        url: url.to_string(),
        final_url,
        file_name: sanitize_file_name(&name),
        total_size,
        resumable,
        etag: text(header::ETAG),
        last_modified: text(header::LAST_MODIFIED),
        content_type,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range() {
        assert_eq!(parse_content_range_total("bytes 0-0/1234"), Some(1234));
        assert_eq!(parse_content_range_total("bytes 0-0/*"), None);
        assert_eq!(parse_content_range_start("bytes 100-199/1234"), Some(100));
    }
}
