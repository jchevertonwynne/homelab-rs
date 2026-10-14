//! Strong ETags over content compiled into the binary, served so that a repeat
//! request costs a 304.
//!
//! The digest is the first 16 bytes of SHA-256, hex — homelab-go's length, so
//! an icon keeps the same validator across the rewrite and a browser that
//! cached it under Go still gets its 304.

use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};

/// A strong validator over `content`. Quoted and not weak: this is a
/// byte-for-byte comparison. Content-derived, so a deploy that leaves a file
/// alone still answers 304.
pub fn of(content: &[u8]) -> String {
    let sum = Sha256::digest(content);
    format!("\"{}\"", hex::encode(&sum[..16]))
}

/// Serves one embedded file with its ETag and `Cache-Control: no-cache` — "keep
/// it, but ask me first". A max-age is a guess at how long a file stays the
/// same, and a wrong guess cannot be corrected from the server.
///
/// The conditional and range headers are read as http.ServeContent reads
/// them, so a browser that cached an asset under the Go apps sees no
/// difference. One range is served; a request for several gets the whole file,
/// which HTTP allows and nothing here has ever asked for.
pub fn serve_bytes(
    method: &Method,
    headers: &HeaderMap,
    content_type: &'static str,
    content: &'static [u8],
    tag: &str,
) -> Response {
    let etag = HeaderValue::from_str(tag).expect("an etag from of() is a valid header");
    let cache = HeaderValue::from_static("no-cache");
    let ctype = HeaderValue::from_static(content_type);
    let get_or_head = method == Method::GET || method == Method::HEAD;

    if let Some(if_match) = header_str(headers, header::IF_MATCH)
        && !matches(if_match, tag, false)
    {
        return (
            StatusCode::PRECONDITION_FAILED,
            [
                (header::CONTENT_TYPE, ctype),
                (header::ETAG, etag),
                (header::CACHE_CONTROL, cache),
            ],
        )
            .into_response();
    }
    if let Some(inm) = header_str(headers, header::IF_NONE_MATCH)
        && matches(inm, tag, true)
    {
        if get_or_head {
            // No Content-Type or Content-Length on a 304, as ServeContent
            // strips them.
            return (
                StatusCode::NOT_MODIFIED,
                [(header::ETAG, etag), (header::CACHE_CONTROL, cache)],
            )
                .into_response();
        }
        return (
            StatusCode::PRECONDITION_FAILED,
            [
                (header::CONTENT_TYPE, ctype),
                (header::ETAG, etag),
                (header::CACHE_CONTROL, cache),
            ],
        )
            .into_response();
    }

    let common = [
        (header::ACCEPT_RANGES, HeaderValue::from_static("bytes")),
        (header::CACHE_CONTROL, cache),
        (header::CONTENT_TYPE, ctype),
        (header::ETAG, etag),
    ];
    // If-Range names the version the client holds a part of; anything else
    // and it gets the whole of this one.
    let range = header_str(headers, header::RANGE)
        .filter(|_| get_or_head)
        .filter(|_| header_str(headers, header::IF_RANGE).is_none_or(|ir| ir == tag));
    match range.map(|r| parse_range(r, content.len())) {
        Some(Range::One(start, end)) => (
            StatusCode::PARTIAL_CONTENT,
            common,
            [(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes {start}-{}/{}", end - 1, content.len()))
                    .expect("digits are a valid header"),
            )],
            &content[start..end],
        )
            .into_response(),
        Some(Range::Unsatisfiable) => (
            StatusCode::RANGE_NOT_SATISFIABLE,
            [
                (
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/plain; charset=utf-8"),
                ),
                (
                    header::X_CONTENT_TYPE_OPTIONS,
                    HeaderValue::from_static("nosniff"),
                ),
                (
                    header::CONTENT_RANGE,
                    HeaderValue::from_str(&format!("bytes */{}", content.len()))
                        .expect("digits are a valid header"),
                ),
            ],
            "invalid range: failed to overlap\n",
        )
            .into_response(),
        Some(Range::Whole) | None => (common, content).into_response(),
    }
}

fn header_str(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

#[derive(Debug, PartialEq)]
enum Range {
    /// Half-open: `start..end`.
    One(usize, usize),
    Unsatisfiable,
    /// Malformed, or more than one range: serve everything.
    Whole,
}

/// `bytes=a-b`, `bytes=a-` or `bytes=-n`, against a body of `len` bytes.
fn parse_range(spec: &str, len: usize) -> Range {
    let Some(spec) = spec.strip_prefix("bytes=") else {
        return Range::Whole;
    };
    if spec.contains(',') {
        return Range::Whole;
    }
    let Some((start, end)) = spec.trim().split_once('-') else {
        return Range::Whole;
    };
    let (start, end) = (start.trim(), end.trim());
    if start.is_empty() {
        // The last n bytes.
        let Ok(n) = end.parse::<usize>() else {
            return Range::Whole;
        };
        if n == 0 {
            return Range::Unsatisfiable;
        }
        return Range::One(len.saturating_sub(n), len);
    }
    let Ok(start) = start.parse::<usize>() else {
        return Range::Whole;
    };
    if start >= len {
        return Range::Unsatisfiable;
    }
    let end = if end.is_empty() {
        len
    } else {
        match end.parse::<usize>() {
            Ok(e) if e >= start => (e + 1).min(len),
            _ => return Range::Whole,
        }
    };
    Range::One(start, end)
}

/// A comma-separated list of entity tags, or `*`. If-None-Match compares
/// weakly (a `W/` prefix is ignored); If-Match strongly.
fn matches(list: &str, tag: &str, weak: bool) -> bool {
    list.split(',').map(str::trim).any(|candidate| {
        if candidate == "*" {
            return true;
        }
        match candidate.strip_prefix("W/") {
            Some(c) => weak && c == tag,
            None => candidate == tag,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_homelab_go_validators() {
        // etag.Of([]byte("hello")) in homelab-go.
        assert_eq!(of(b"hello"), "\"2cf24dba5fb0a30e26e83b2ac5b9e29e\"");
    }

    #[test]
    fn reads_if_none_match_lists() {
        let tag = "\"abc\"";
        assert!(matches("\"abc\"", tag, true));
        assert!(matches("\"x\", W/\"abc\"", tag, true));
        assert!(matches("*", tag, true));
        assert!(!matches("\"x\"", tag, true));
        assert!(!matches("W/\"abc\"", tag, false));
    }

    #[test]
    fn reads_ranges_as_servecontent_does() {
        assert_eq!(parse_range("bytes=0-9", 530), Range::One(0, 10));
        assert_eq!(parse_range("bytes=520-", 530), Range::One(520, 530));
        assert_eq!(parse_range("bytes=-10", 530), Range::One(520, 530));
        assert_eq!(parse_range("bytes=500-9999", 530), Range::One(500, 530));
        assert_eq!(parse_range("bytes=600-", 530), Range::Unsatisfiable);
        assert_eq!(parse_range("bytes=0-1,5-6", 530), Range::Whole);
        assert_eq!(parse_range("lines=1-2", 530), Range::Whole);
    }
}
