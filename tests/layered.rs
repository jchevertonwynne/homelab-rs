//! `instrument_layered`, in the shape list has: probes beside an
//! authenticated app, with its live stream kept out of the metrics.

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::{Next, from_fn};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;

/// Refuses anything without an `x-user` header, as an authentication layer
/// refuses a request with no identity.
async fn auth(req: Request, next: Next) -> Response {
    if req.headers().contains_key("x-user") {
        next.run(req).await
    } else {
        StatusCode::FORBIDDEN.into_response()
    }
}

fn app() -> Router {
    let outer = Router::new().route("/lhealthz", get(|| async { "ok\n" }));
    let app = Router::new()
        .route("/", get(|| async { "index" }))
        .route(
            "/lcollections/{collection}",
            get(|| async { "c" }).delete(|| async { "d" }),
        )
        .route("/lcollections", post(|| async { "made" }))
        .route("/llive", get(|| async { "stream" }))
        .fallback(|| async { StatusCode::NOT_FOUND })
        .layer(from_fn(auth));
    homelab::http::instrument_layered(outer, app, &["GET /llive"])
}

async fn send(method: &str, path: &str, user: bool) -> StatusCode {
    let mut req = Request::builder().method(method).uri(path);
    if user {
        req = req.header("x-user", "a");
    }
    app()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}

async fn metrics() -> String {
    let res = Router::new()
        .route("/metrics", get(homelab::metrics::handler))
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap()
}

#[tokio::test]
async fn labels_by_the_route_matched_in_whichever_router_served_it() {
    assert_eq!(send("GET", "/lhealthz", false).await, StatusCode::OK);
    assert_eq!(send("GET", "/", true).await, StatusCode::OK);
    assert_eq!(send("GET", "/lcollections/7", true).await, StatusCode::OK);
    // Refused by the wrapper, still counted under the route it was for.
    assert_eq!(
        send("GET", "/lcollections/7", false).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(send("GET", "/lnope", true).await, StatusCode::NOT_FOUND);
    assert_eq!(
        send("PUT", "/lcollections/7", true).await,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(send("POST", "/lcollections", true).await, StatusCode::OK);
    assert_eq!(send("GET", "/llive", true).await, StatusCode::OK);

    let body = metrics().await;
    for series in [
        r#"{method="GET",route="GET /lhealthz",status="200"}"#,
        r#"{method="GET",route="GET /",status="200"}"#,
        r#"{method="GET",route="GET /lcollections/{collection}",status="200"}"#,
        r#"{method="GET",route="GET /lcollections/{collection}",status="403"}"#,
        r#"{method="GET",route="unmatched",status="404"}"#,
        r#"{method="PUT",route="PUT /lcollections/{collection}",status="405"}"#,
        r#"{method="POST",route="POST /lcollections",status="200"}"#,
    ] {
        assert!(
            body.contains(&format!("http_request_duration_seconds_count{series} 1")),
            "missing {series} in\n{body}"
        );
    }
    assert!(
        !body.contains("llive"),
        "an excluded route was recorded:\n{body}"
    );
}
