//! Opt-in bearer-token gate for the whole HTTP/WS surface (#162).
//!
//! Default = today's open shape: loopback dev, the DSH plugin and carrier
//! keep zero-config access, byte for byte. When
//! `AGINXBROWSER_TOKEN` is set, every route except `/health` demands the
//! token — `Authorization: Bearer <t>` or `?token=<t>` (the query form
//! exists because header-less GETs — WebSocket upgrades, curl one-liners —
//! cannot carry an Authorization header; Chrome's own path-uuid-as-
//! capability shape; lightpanda#3452 absorption).
//!
//! The token charset is deliberately URL-safe so the header form and the
//! `?token=` form are the same string — no percent-encoding ambiguity
//! between the two doors.

use axum::extract::Request;
use axum::http::{header::AUTHORIZATION, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

pub const TOKEN_ENV: &str = "AGINXBROWSER_TOKEN";

/// 16+ chars of `[A-Za-z0-9_-]`. Shorter than 16 is a misconfiguration
/// masquerading as protection; the caller refuses to boot rather than
/// serve a half-open surface silently (#664: name the layer, give the way
/// out).
pub fn token_shape_valid(t: &str) -> bool {
    t.len() >= 16
        && t.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Read + validate the env knob. The error never echoes the value — a
/// malformed secret is still a secret-shaped string.
pub fn token_from_env() -> Result<Option<String>, String> {
    match std::env::var(TOKEN_ENV) {
        Err(_) => Ok(None),
        Ok(t) if token_shape_valid(&t) => Ok(Some(t)),
        Ok(_) => Err(format!(
            "{TOKEN_ENV} is set but not a valid token: expected 16+ chars \
             of [A-Za-z0-9_-] (URL-safe so it can ride ?token= without \
             encoding). Refusing to serve a half-open surface — fix the \
             value or unset it"
        )),
    }
}

/// Constant-time equality. Differing lengths return fast — the length is
/// public (it rides every discovery URL anyway); the byte walk folds
/// without early exit so matching position doesn't leak.
pub fn token_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `Authorization: Bearer <t>` — scheme match is case-insensitive per
/// RFC 9110 §11.6.2's note on scheme case; clients send both shapes.
fn bearer_token(headers: &axum::http::HeaderMap) -> Option<String> {
    let v = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, rest) = v.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let rest = rest.trim();
    (!rest.is_empty()).then(|| rest.to_string())
}

/// `?token=<t>` — the header-less door (WebSocket-style GETs and plain
/// curl one-liners).
fn query_token(uri: &axum::http::Uri) -> Option<String> {
    let q = uri.query()?;
    q.split('&').find_map(|pair| {
        let v = pair.strip_prefix("token=")?;
        (!v.is_empty()).then(|| v.to_string())
    })
}

pub fn presented_token(req: &Request) -> Option<String> {
    bearer_token(req.headers()).or_else(|| query_token(req.uri()))
}

/// The middleware body. `/health` stays reachable with no token at all —
/// runbooks, supervisors and the hosted nginx check hit it for liveness and
/// must keep getting a 200. But with the gate on, the build identity behind
/// it (version/commit/V8/UA/TLS fingerprint/capabilities) is operator
/// detail, and the hosted nginx @proxy shape puts that JSON on the public
/// internet — so without a valid token /health serves exactly
/// `{"status":"ok"}` and nothing else (#246).
/// Every other route demands the token; a miss is a 401 that names the
/// remedy instead of a bare wall.
pub async fn gate(expected: String, req: Request, next: Next) -> Response {
    let token_ok = presented_token(&req).is_some_and(|t| token_eq(&t, &expected));
    if req.uri().path() == "/health" {
        if token_ok {
            return next.run(req).await;
        }
        return axum::Json(serde_json::json!({"status": "ok"})).into_response();
    }
    if token_ok {
        return next.run(req).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        format!(
            "unauthorized: {TOKEN_ENV} is set on this instance — send \
             `Authorization: Bearer <token>` or `?token=<token>` (#162)"
        ),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::routing::get;
    use tower::util::ServiceExt;

    fn req(uri: &str, auth: Option<&str>) -> Request {
        let mut b = Request::builder().uri(uri);
        if let Some(a) = auth {
            b = b.header(AUTHORIZATION, a);
        }
        b.body(Body::empty()).unwrap()
    }

    #[test]
    fn shape_matrix() {
        assert!(token_shape_valid("abcdefghijklmnop"));
        assert!(token_shape_valid("A1-_B2-_C3-_D4-_"));
        assert!(!token_shape_valid("short"), "15 chars is not a gate");
        assert!(!token_shape_valid(""), "empty is unset's job, not a token");
        assert!(!token_shape_valid("has space in it!!"), "not URL-safe");
        assert!(!token_shape_valid("percent%2Fencoded"), "not URL-safe");
    }

    #[test]
    fn token_eq_corners() {
        assert!(token_eq("abcdefabcdefabcd", "abcdefabcdefabcd"));
        assert!(!token_eq("abcdefabcdefabcd", "abcdefabcdefabcX"));
        assert!(!token_eq("abcdefabcdefabcd", "abcdefabcdefabc"));
        assert!(token_eq("", ""));
    }

    #[test]
    fn bearer_parsing() {
        assert_eq!(
            bearer_token(req("/x", Some("Bearer t-ok-0123456789")).headers()),
            Some("t-ok-0123456789".into())
        );
        // Scheme case-insensitive, trailing space tolerated.
        assert_eq!(
            bearer_token(req("/x", Some("bearer t-ok-0123456789 ")).headers()),
            Some("t-ok-0123456789".into())
        );
        assert_eq!(bearer_token(req("/x", None).headers()), None);
        assert_eq!(
            bearer_token(req("/x", Some("Basic dXNlcjpwdw==")).headers()),
            None,
            "other schemes are not tokens"
        );
        assert_eq!(bearer_token(req("/x", Some("Bearer ")).headers()), None);
    }

    #[test]
    fn query_parsing() {
        assert_eq!(
            query_token(req("/x?token=t-ok-0123456789&a=b", None).uri()),
            Some("t-ok-0123456789".into())
        );
        assert_eq!(
            query_token(req("/x?a=b&token=t-ok-0123456789", None).uri()),
            Some("t-ok-0123456789".into()),
            "token may ride anywhere in the query"
        );
        assert_eq!(query_token(req("/x", None).uri()), None);
        assert_eq!(query_token(req("/x?other=1", None).uri()), None);
        assert_eq!(query_token(req("/x?token=", None).uri()), None);
    }

    fn gated() -> axum::Router {
        let expected = "t-ok-0123456789".to_string();
        axum::Router::new()
            .route("/health", get(|| async { "ok" }))
            .route("/secret", get(|| async { "data" }))
            .layer(axum::middleware::from_fn(move |req, next| {
                gate(expected.clone(), req, next)
            }))
    }

    async fn body_text(res: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(res.into_body(), 4096)
            .await
            .unwrap()
            .to_vec();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[tokio::test]
    async fn gate_blocks_bare_and_wrong_token() {
        let res = gated().oneshot(req("/secret", None)).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let body = body_text(res).await;
        assert!(body.contains(TOKEN_ENV), "names the layer: {body}");
        assert!(body.contains("Bearer"), "names the way out: {body}");

        let res = gated()
            .oneshot(req("/secret", Some("Bearer wrong-token-value")))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn gate_passes_bearer_query_and_health() {
        let res = gated()
            .oneshot(req("/secret", Some("Bearer t-ok-0123456789")))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(body_text(res).await, "data");

        let res = gated()
            .oneshot(req("/secret?token=t-ok-0123456789", None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // Liveness stays open with no token at all — but the gate on means
        // the detail body behind /health is operator-only (#246).
        let res = gated().oneshot(req("/health", None)).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(body_text(res).await, r#"{"status":"ok"}"#);

        let res = gated()
            .oneshot(req("/health", Some("Bearer wrong-token-value")))
            .await
            .unwrap();
        assert_eq!(body_text(res).await, r#"{"status":"ok"}"#);

        // A valid token gets the real /health body (the route's own).
        let res = gated()
            .oneshot(req("/health", Some("Bearer t-ok-0123456789")))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(body_text(res).await, "ok");
    }
}
