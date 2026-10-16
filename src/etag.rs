//! Strong ETags over content compiled into the binary, served so that a repeat
//! request costs a 304.
//!
//! The digest is the first 16 bytes of SHA-256, hex. That length is a
//! convention rather than a requirement, so it lives here: changing it changes
//! every validator at once, and a validator that disagrees with the one a
//! client cached costs a full response where a 304 would have done.

use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
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
/// same, and a wrong guess cannot be corrected from the server. A request that
/// already holds this version (`If-None-Match`) gets a 304 and no body.
pub fn serve_bytes(
    headers: &HeaderMap,
    content_type: &'static str,
    content: &'static [u8],
    tag: &str,
) -> Response {
    let etag = HeaderValue::from_str(tag).expect("an etag from of() is a valid header");
    let cache = HeaderValue::from_static("no-cache");
    let held = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|list| matches(list, tag));
    if held {
        return (
            StatusCode::NOT_MODIFIED,
            [(header::ETAG, etag), (header::CACHE_CONTROL, cache)],
        )
            .into_response();
    }
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
            (header::ETAG, etag),
            (header::CACHE_CONTROL, cache),
        ],
        content,
    )
        .into_response()
}

/// An If-None-Match list: entity tags separated by commas, compared weakly (a
/// `W/` prefix is ignored), or `*`.
fn matches(list: &str, tag: &str) -> bool {
    list.split(',')
        .map(str::trim)
        .any(|c| c == "*" || c.strip_prefix("W/").unwrap_or(c) == tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_sixteen_bytes_of_sha256() {
        assert_eq!(of(b"hello"), "\"2cf24dba5fb0a30e26e83b2ac5b9e29e\"");
    }

    #[test]
    fn reads_if_none_match_lists() {
        let tag = "\"abc\"";
        assert!(matches("\"abc\"", tag));
        assert!(matches("\"x\", W/\"abc\"", tag));
        assert!(matches("*", tag));
        assert!(!matches("\"x\"", tag));
    }

    #[test]
    fn a_held_version_is_a_304_without_a_body() {
        let mut h = HeaderMap::new();
        h.insert(header::IF_NONE_MATCH, of(b"x").parse().unwrap());
        let res = serve_bytes(&h, "text/plain", b"x", &of(b"x"));
        assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
        assert!(!res.headers().contains_key(header::CONTENT_TYPE));
        let res = serve_bytes(&HeaderMap::new(), "text/plain", b"x", &of(b"x"));
        assert_eq!(res.status(), StatusCode::OK);
    }
}
