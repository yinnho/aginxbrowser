//! Request-boundary panic guard: a handler panic becomes a JSON 500 for that
//! one request instead of a dropped connection with no reply. The process was
//! never at risk (tokio isolates the task), but the caller got nothing and the
//! panic went unlogged as a request failure.

use axum::{
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use std::any::Any;
use tower_http::catch_panic::CatchPanicLayer;

fn respond(panic: Box<dyn Any + Send + 'static>) -> Response {
    let detail = panic
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_string());
    tracing::error!("handler panicked: {detail}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        [(header::CONTENT_TYPE, "application/json")],
        r#"{"ok":false,"error":"internal_panic"}"#,
    )
        .into_response()
}

pub fn layer() -> CatchPanicLayer<fn(Box<dyn Any + Send + 'static>) -> Response> {
    CatchPanicLayer::custom(respond as fn(_) -> _)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request, routing::get, Router};
    use tower::ServiceExt;

    fn app() -> Router {
        Router::new()
            .route("/boom", get(|| async {
                if true {
                    panic!("kaboom");
                }
                "unreachable"
            }))
            .route("/ok", get(|| async { "fine" }))
            .layer(layer())
    }

    #[tokio::test]
    async fn panic_becomes_json_500_and_service_survives() {
        let res = app()
            .oneshot(Request::get("/boom").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], br#"{"ok":false,"error":"internal_panic"}"#);

        let res = app()
            .oneshot(Request::get("/ok").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }
}
