//! Wires the request middleware onto a router, and keeps the parts of
//! net/http's behaviour that the signals depend on.
//!
//! The route a request matched is the label every per-route panel groups by
//! and the name of every request span, so it is spelled as the Go apps spelled
//! it — the `http.ServeMux` pattern, `"GET /entries/{id}"` — rather than as
//! axum does. Without that a port would split each endpoint's history in two
//! at the cutover. [`go_pattern`] is the translation.

use axum::Router;
use axum::extract::{MatchedPath, Request};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{from_fn, map_response};
use axum::response::{IntoResponse, Response};

/// Every app's router goes out through this: net/http's 404, inside the
/// metrics middleware, inside the trace middleware — the same nesting as
/// `tracing.Middleware(name, metrics.Instrument(mux))` — and net/http's 405
/// around the lot.
///
/// Call it after every route is added. axum applies a layer only to the routes
/// that exist when it is applied.
pub fn instrument(router: Router) -> Router {
    crate::metrics::register();
    let inner = router
        .fallback(not_found)
        .layer(from_fn(crate::metrics::middleware))
        .layer(from_fn(crate::trace::middleware));
    // axum sets a 405's Allow header after every route layer has run, so the
    // rewrite into net/http's shape has to wrap the whole router from outside.
    Router::new()
        .fallback_service(inner)
        .layer(map_response(method_not_allowed))
}

/// The route a request matched, from inside the router.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Route {
    /// The Go path pattern, `/entries/{id}`, or None for no match.
    pub path: Option<String>,
}

impl Route {
    pub(crate) fn of(req: &Request) -> Self {
        Route {
            path: req
                .extensions()
                .get::<MatchedPath>()
                .map(|p| go_pattern(p.as_str())),
        }
    }

    /// A 405 is a path that matched with a method that did not. net/http
    /// reports no pattern for it, so neither does this.
    pub(crate) fn settle(self, status: StatusCode) -> Self {
        if status == StatusCode::METHOD_NOT_ALLOWED {
            Route { path: None }
        } else {
            self
        }
    }

    /// The full ServeMux pattern, method included — the metrics' route label.
    /// HEAD is served by a GET route in both routers, and the pattern is the
    /// GET one.
    pub(crate) fn pattern(&self, method: &Method) -> Option<String> {
        let method = if method == Method::HEAD {
            &Method::GET
        } else {
            method
        };
        self.path.as_ref().map(|p| format!("{method} {p}"))
    }
}

/// An axum path as the equivalent `http.ServeMux` pattern.
///
/// Parameters are spelled the same in both (`{id}`). The two differences are
/// at the end: an axum path ending in `/` matches only itself, which ServeMux
/// writes `/{$}`, and an axum trailing catch-all `/{*rest}` is a ServeMux
/// prefix pattern, written with just the trailing `/`.
pub fn go_pattern(axum_path: &str) -> String {
    if axum_path.ends_with('/') {
        return format!("{axum_path}{{$}}");
    }
    if let Some(slash) = axum_path.rfind('/') {
        let last = &axum_path[slash + 1..];
        if last.starts_with("{*") && last.ends_with('}') {
            return axum_path[..=slash].to_string();
        }
    }
    axum_path.to_string()
}

/// What ServeMux does with a path no route matched: a path that is not in
/// canonical form (`//`, `/a/../b`, `/./x`) is redirected to the form that is,
/// and anything else is `http.NotFound`, byte for byte.
///
/// ServeMux cleans before matching, so it labels a redirect with the pattern
/// the cleaned path would have matched. This runs after matching, so it
/// cannot know that pattern, and the redirect is labelled `unmatched`.
pub async fn not_found(req: Request) -> Response {
    let raw = req.uri().path();
    let clean = clean_path(raw);
    if req.method() != Method::CONNECT && clean != raw {
        let location = match req.uri().query() {
            Some(q) if !q.is_empty() => format!("{clean}?{q}"),
            _ => clean,
        };
        return redirect(req.method(), &location);
    }
    plain(StatusCode::NOT_FOUND, "404 page not found\n")
}

/// `http.Redirect` with 307: a short HTML body for GET and HEAD, none
/// otherwise.
fn redirect(method: &Method, location: &str) -> Response {
    let Ok(loc) = HeaderValue::from_str(location) else {
        return plain(StatusCode::NOT_FOUND, "404 page not found\n");
    };
    let mut res = if method == Method::GET || method == Method::HEAD {
        let href = location
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&#34;")
            .replace('\'', "&#39;");
        (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            format!("<a href=\"{href}\">Temporary Redirect</a>.\n\n"),
        )
            .into_response()
    } else {
        ().into_response()
    };
    *res.status_mut() = StatusCode::TEMPORARY_REDIRECT;
    res.headers_mut().insert(header::LOCATION, loc);
    res
}

/// net/http's cleanPath: `path.Clean`, rooted, keeping a trailing slash.
/// Applied to the path as sent, so `%2e%2e` is not a `..`, as it is not in Go.
fn clean_path(p: &str) -> String {
    if p.is_empty() {
        return "/".into();
    }
    let mut parts: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            seg => parts.push(seg),
        }
    }
    let mut out = format!("/{}", parts.join("/"));
    if p.ends_with('/') && out != "/" {
        out.push('/');
    }
    out
}

/// axum's 405 carries the Allow header and nothing else. net/http's carries a
/// body too, and spells Allow with a space after each comma.
async fn method_not_allowed(mut res: Response) -> Response {
    if res.status() != StatusCode::METHOD_NOT_ALLOWED {
        return res;
    }
    let allow = res.headers_mut().remove(header::ALLOW).and_then(|v| {
        v.to_str()
            .ok()
            .map(|s| s.split(',').map(str::trim).collect::<Vec<_>>().join(", "))
    });
    let mut res = plain(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed\n");
    if let Some(allow) = allow.and_then(|a| HeaderValue::from_str(&a).ok()) {
        res.headers_mut().insert(header::ALLOW, allow);
    }
    res
}

/// `http.Error`'s headers: text, and not to be sniffed into anything else.
fn plain(status: StatusCode, body: &'static str) -> Response {
    (
        status,
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spells_routes_as_servemux_does() {
        assert_eq!(go_pattern("/"), "/{$}");
        assert_eq!(go_pattern("/healthz"), "/healthz");
        assert_eq!(go_pattern("/entries/{id}"), "/entries/{id}");
        assert_eq!(go_pattern("/lists/"), "/lists/{$}");
        assert_eq!(go_pattern("/static/{*path}"), "/static/");
        assert_eq!(go_pattern("/{*rest}"), "/");
    }

    #[test]
    fn cleans_paths_as_servemux_does() {
        assert_eq!(clean_path("/"), "/");
        assert_eq!(clean_path("//"), "/");
        assert_eq!(clean_path("/a//b/"), "/a/b/");
        assert_eq!(clean_path("/a/./b/../c"), "/a/c");
        assert_eq!(clean_path("/../x"), "/x");
        assert_eq!(clean_path("/%2e%2e/"), "/%2e%2e/");
    }

    #[test]
    fn head_reports_the_get_pattern() {
        let route = Route {
            path: Some("/{$}".into()),
        };
        assert_eq!(route.pattern(&Method::HEAD).as_deref(), Some("GET /{$}"));
        assert_eq!(route.pattern(&Method::POST).as_deref(), Some("POST /{$}"));
    }
}
