//! HTTP 访问日志：span 可带请求头，但 `Authorization` 等敏感值一律打成 `[redacted]`。

use axum::http::{HeaderMap, HeaderName, Request};
use tower_http::classify::ServerErrorsAsFailures;
use tower_http::trace::{MakeSpan, TraceLayer};
use tracing::Span;

const SENSITIVE: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
];

pub fn is_sensitive_header(name: &HeaderName) -> bool {
    SENSITIVE.iter().any(|s| name.as_str() == *s)
}

/// 访问日志用的头列表。敏感值替换为 `[redacted]`。
pub fn redacted_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(k, v)| {
            let value = if is_sensitive_header(k) {
                "[redacted]".into()
            } else {
                v.to_str().unwrap_or("[binary]").to_string()
            };
            (k.as_str().to_string(), value)
        })
        .collect()
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RedactingMakeSpan;

impl<B> MakeSpan<B> for RedactingMakeSpan {
    fn make_span(&mut self, request: &Request<B>) -> Span {
        let headers = redacted_headers(request.headers());
        tracing::info_span!(
            "http",
            method = %request.method(),
            path = %request.uri().path(),
            headers = ?headers,
        )
    }
}

pub fn access_trace_layer(
) -> TraceLayer<tower_http::classify::SharedClassifier<ServerErrorsAsFailures>, RedactingMakeSpan> {
    TraceLayer::new_for_http().make_span_with(RedactingMakeSpan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id};
    use tracing_subscriber::layer::{Context, SubscriberExt};
    use tracing_subscriber::Layer;

    #[test]
    fn authorization_header_is_redacted() {
        let mut h = HeaderMap::new();
        h.insert(
            AUTHORIZATION,
            "Bearer super-secret-token-xyz".parse().unwrap(),
        );
        h.insert(CONTENT_TYPE, "application/json".parse().unwrap());
        h.append("cookie", "sid=abc".parse().unwrap());

        let raw = format!("{h:?}");
        assert!(
            raw.contains("super-secret-token-xyz"),
            "sanity: HeaderMap Debug would leak the bearer: {raw}"
        );

        let red = redacted_headers(&h);
        let dump = format!("{red:?}");
        assert!(!dump.contains("super-secret-token-xyz"), "{dump}");
        assert!(!dump.contains("sid=abc"), "{dump}");
        assert!(red
            .iter()
            .any(|(k, v)| k == "authorization" && v == "[redacted]"));
        assert!(red
            .iter()
            .any(|(k, v)| k == "content-type" && v == "application/json"));
        assert!(red.iter().any(|(k, v)| k == "cookie" && v == "[redacted]"));
    }

    struct Dump(Arc<Mutex<String>>);

    impl<S> Layer<S> for Dump
    where
        S: tracing::Subscriber,
    {
        fn on_new_span(&self, attrs: &Attributes<'_>, _id: &Id, _ctx: Context<'_, S>) {
            struct V<'a>(&'a mut String);
            impl Visit for V<'_> {
                fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                    use std::fmt::Write;
                    let _ = write!(self.0, " {}={:?}", field.name(), value);
                }
                fn record_str(&mut self, field: &Field, value: &str) {
                    use std::fmt::Write;
                    let _ = write!(self.0, " {}={}", field.name(), value);
                }
            }
            let mut s = self.0.lock().unwrap();
            attrs.record(&mut V(&mut s));
        }
    }

    #[test]
    fn make_span_omits_authorization_value() {
        let buf = Arc::new(Mutex::new(String::new()));
        let subscriber = tracing_subscriber::registry().with(Dump(buf.clone()));
        tracing::subscriber::with_default(subscriber, || {
            let req = Request::builder()
                .uri("/v1/me?token=query-secret")
                .header(AUTHORIZATION, "Bearer super-secret-token-xyz")
                .header("x-request-id", "rid-1")
                .body(())
                .unwrap();
            let _span = RedactingMakeSpan.make_span(&req);
        });
        let text = buf.lock().unwrap().clone();
        assert!(
            !text.contains("super-secret-token-xyz"),
            "span leaked bearer: {text}"
        );
        assert!(text.contains("[redacted]"), "{text}");
        assert!(
            text.contains("rid-1") || text.contains("x-request-id"),
            "{text}"
        );
        assert!(
            !text.contains("query-secret"),
            "span should log path only: {text}"
        );
        assert!(text.contains("/v1/me") || text.contains("path"), "{text}");
    }
}
