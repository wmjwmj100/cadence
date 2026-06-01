use crate::auth::AuthProvider;
use crate::endpoint::session::EndpointSession;
use crate::error::ApiError;
use crate::provider::Provider;
use codex_client::HttpTransport;
use codex_client::RequestTelemetry;
use codex_client::StreamResponse;
use http::HeaderMap;
use http::HeaderValue;
use http::Method;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

const INITIAL_STREAM_START_TIMEOUT: Duration = Duration::from_secs(80);

pub struct ChatCompletionsClient<T: HttpTransport, A: AuthProvider> {
    session: EndpointSession<T, A>,
}

impl<T: HttpTransport, A: AuthProvider> ChatCompletionsClient<T, A> {
    pub fn new(transport: T, provider: Provider, auth: A) -> Self {
        Self {
            session: EndpointSession::new(transport, provider, auth),
        }
    }

    pub fn with_telemetry(self, request: Option<Arc<dyn RequestTelemetry>>) -> Self {
        Self {
            session: self.session.with_request_telemetry(request),
        }
    }

    fn path() -> &'static str {
        "chat/completions"
    }

    pub async fn stream(
        &self,
        body: Value,
        extra_headers: HeaderMap,
    ) -> Result<StreamResponse, ApiError> {
        self.session
            .stream_with(
                Method::POST,
                Self::path(),
                extra_headers,
                Some(body),
                |req| {
                    req.headers.insert(
                        http::header::ACCEPT,
                        HeaderValue::from_static("text/event-stream"),
                    );
                    req.timeout = Some(INITIAL_STREAM_START_TIMEOUT);
                },
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::RetryConfig;
    use async_trait::async_trait;
    use codex_client::Request;
    use codex_client::Response;
    use codex_client::TransportError;
    use futures::StreamExt;
    use http::StatusCode;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::sync::Mutex;

    #[derive(Clone)]
    struct CapturingTransport {
        last_request: Arc<Mutex<Option<Request>>>,
    }

    impl CapturingTransport {
        fn new() -> Self {
            Self {
                last_request: Arc::new(Mutex::new(None)),
            }
        }
    }

    #[async_trait]
    impl HttpTransport for CapturingTransport {
        async fn execute(&self, _req: Request) -> Result<Response, TransportError> {
            Err(TransportError::Build("execute should not run".to_string()))
        }

        async fn stream(&self, req: Request) -> Result<StreamResponse, TransportError> {
            *self.last_request.lock().expect("lock request store") = Some(req);
            Ok(StreamResponse {
                status: StatusCode::OK,
                headers: HeaderMap::new(),
                bytes: futures::stream::empty().boxed(),
            })
        }
    }

    #[derive(Clone, Default)]
    struct DummyAuth;

    impl AuthProvider for DummyAuth {
        fn bearer_token(&self) -> Option<String> {
            Some("test-token".to_string())
        }
    }

    fn provider(base_url: &str) -> Provider {
        let mut headers = HeaderMap::new();
        headers.insert("x-provider-default", HeaderValue::from_static("present"));
        Provider {
            name: "test".to_string(),
            base_url: base_url.to_string(),
            query_params: None,
            headers,
            retry: RetryConfig {
                max_attempts: 1,
                base_delay: Duration::from_millis(1),
                retry_429: false,
                retry_5xx: false,
                retry_transport: false,
            },
            stream_idle_timeout: Duration::from_secs(1),
        }
    }

    #[tokio::test]
    async fn stream_posts_chat_completions_sse_request() {
        let transport = CapturingTransport::new();
        let captured = Arc::clone(&transport.last_request);
        let client =
            ChatCompletionsClient::new(transport, provider("https://example.com/v1"), DummyAuth);
        let mut extra_headers = HeaderMap::new();
        extra_headers.insert("x-extra", HeaderValue::from_static("extra"));

        let body = json!({
            "model": "kimi-k2.6",
            "messages": [{"role": "user", "content": "hello"}],
            "stream": true
        });
        let _stream = client
            .stream(body.clone(), extra_headers)
            .await
            .expect("stream response");

        let request = captured
            .lock()
            .expect("lock request store")
            .take()
            .expect("captured request");
        assert_eq!(request.method, Method::POST);
        assert_eq!(request.url, "https://example.com/v1/chat/completions");
        assert_eq!(request.body, Some(body));
        assert_eq!(
            request
                .headers
                .get(http::header::ACCEPT)
                .and_then(|v| v.to_str().ok()),
            Some("text/event-stream")
        );
        assert_eq!(
            request
                .headers
                .get(http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
            Some("Bearer test-token")
        );
        assert_eq!(
            request
                .headers
                .get("x-provider-default")
                .and_then(|v| v.to_str().ok()),
            Some("present")
        );
        assert_eq!(
            request.headers.get("x-extra").and_then(|v| v.to_str().ok()),
            Some("extra")
        );
        assert_eq!(request.timeout, Some(INITIAL_STREAM_START_TIMEOUT));
    }
}
