use crate::default_client::CodexHttpClient;
use crate::default_client::CodexRequestBuilder;
use crate::error::TransportError;
use crate::request::Request;
use crate::request::RequestCompression;
use crate::request::Response;
use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use futures::stream::BoxStream;
use http::HeaderMap;
use http::Method;
use http::StatusCode;
use std::error::Error as StdError;
use tokio::time::timeout;
use tracing::Level;
use tracing::enabled;
use tracing::trace;

pub type ByteStream = BoxStream<'static, Result<Bytes, TransportError>>;

pub struct StreamResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub bytes: ByteStream,
}

#[async_trait]
pub trait HttpTransport: Send + Sync {
    async fn execute(&self, req: Request) -> Result<Response, TransportError>;
    async fn stream(&self, req: Request) -> Result<StreamResponse, TransportError>;
}

#[derive(Clone, Debug)]
pub struct ReqwestTransport {
    client: CodexHttpClient,
}

impl ReqwestTransport {
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client: CodexHttpClient::new(client),
        }
    }

    fn build(&self, req: Request) -> Result<CodexRequestBuilder, TransportError> {
        let Request {
            method,
            url,
            mut headers,
            body,
            compression,
            timeout,
        } = req;

        let mut builder = self.client.request(
            Method::from_bytes(method.as_str().as_bytes()).unwrap_or(Method::GET),
            &url,
        );

        if let Some(timeout) = timeout {
            builder = builder.timeout(timeout);
        }

        if let Some(body) = body {
            if compression != RequestCompression::None {
                if headers.contains_key(http::header::CONTENT_ENCODING) {
                    return Err(TransportError::Build(
                        "request compression was requested but content-encoding is already set"
                            .to_string(),
                    ));
                }

                let json = serde_json::to_vec(&body)
                    .map_err(|err| TransportError::Build(err.to_string()))?;
                let pre_compression_bytes = json.len();
                let compression_start = std::time::Instant::now();
                let (compressed, content_encoding) = match compression {
                    RequestCompression::None => unreachable!("guarded by compression != None"),
                    RequestCompression::Zstd => (
                        zstd::stream::encode_all(std::io::Cursor::new(json), 3)
                            .map_err(|err| TransportError::Build(err.to_string()))?,
                        http::HeaderValue::from_static("zstd"),
                    ),
                };
                let post_compression_bytes = compressed.len();
                let compression_duration = compression_start.elapsed();

                // Ensure the server knows to unpack the request body.
                headers.insert(http::header::CONTENT_ENCODING, content_encoding);
                if !headers.contains_key(http::header::CONTENT_TYPE) {
                    headers.insert(
                        http::header::CONTENT_TYPE,
                        http::HeaderValue::from_static("application/json"),
                    );
                }

                tracing::info!(
                    pre_compression_bytes,
                    post_compression_bytes,
                    compression_duration_ms = compression_duration.as_millis(),
                    "Compressed request body with zstd"
                );

                builder = builder.headers(headers).body(compressed);
            } else {
                builder = builder.headers(headers).json(&body);
            }
        } else {
            builder = builder.headers(headers);
        }
        Ok(builder)
    }

    fn map_error(err: reqwest::Error) -> TransportError {
        if err.is_timeout() {
            TransportError::Timeout
        } else {
            TransportError::Network(format_reqwest_error_chain(&err))
        }
    }
}

fn format_reqwest_error_chain(err: &reqwest::Error) -> String {
    let mut message = err.to_string();
    let mut source = err.source();
    let mut depth = 0;

    while let Some(cause) = source {
        let cause_message = cause.to_string();
        if !cause_message.is_empty() && !message.contains(&cause_message) {
            message.push_str("; caused by: ");
            message.push_str(&cause_message);
        }
        source = cause.source();
        depth += 1;
        if depth >= 8 {
            break;
        }
    }

    message
}

#[async_trait]
impl HttpTransport for ReqwestTransport {
    async fn execute(&self, req: Request) -> Result<Response, TransportError> {
        if enabled!(Level::TRACE) {
            trace!(
                "{} to {}: {}",
                req.method,
                req.url,
                req.body.as_ref().unwrap_or_default()
            );
        }

        let url = req.url.clone();
        let builder = self.build(req)?;
        let resp = builder.send().await.map_err(Self::map_error)?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = resp.bytes().await.map_err(Self::map_error)?;
        if !status.is_success() {
            let body = String::from_utf8(bytes.to_vec()).ok();
            return Err(TransportError::Http {
                status,
                url: Some(url),
                headers: Some(headers),
                body,
            });
        }
        Ok(Response {
            status,
            headers,
            body: bytes,
        })
    }

    async fn stream(&self, req: Request) -> Result<StreamResponse, TransportError> {
        if enabled!(Level::TRACE) {
            trace!(
                "{} to {}: {}",
                req.method,
                req.url,
                req.body.as_ref().unwrap_or_default()
            );
        }

        let url = req.url.clone();
        let request_timeout = req.timeout;
        let builder = self.build(Request {
            timeout: None,
            ..req
        })?;
        let resp = if let Some(request_timeout) = request_timeout {
            timeout(request_timeout, builder.send())
                .await
                .map_err(|_| TransportError::Timeout)?
                .map_err(Self::map_error)?
        } else {
            builder.send().await.map_err(Self::map_error)?
        };
        let status = resp.status();
        let headers = resp.headers().clone();
        if !status.is_success() {
            let body = resp.text().await.ok();
            return Err(TransportError::Http {
                status,
                url: Some(url),
                headers: Some(headers),
                body,
            });
        }
        let stream = resp
            .bytes_stream()
            .map(|result| result.map_err(Self::map_error));
        Ok(StreamResponse {
            status,
            headers,
            bytes: Box::pin(stream),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn network_errors_include_reqwest_source_chain() {
        let transport = ReqwestTransport::new(reqwest::Client::new());
        let request = Request::new(
            Method::GET,
            "http://127.0.0.1:1/source-chain-test".to_string(),
        );

        let err = transport
            .execute(request)
            .await
            .expect_err("request should fail against a closed local port");
        let TransportError::Network(message) = err else {
            panic!("expected network error, got {err:?}");
        };

        assert!(message.contains("error sending request"));
        assert!(
            message.contains("caused by:"),
            "network error should include source chain, got: {message}"
        );
    }

    #[tokio::test]
    async fn stream_network_errors_include_reqwest_source_chain() {
        let transport = ReqwestTransport::new(reqwest::Client::new());
        let request = Request::new(
            Method::GET,
            "http://127.0.0.1:1/source-chain-stream-test".to_string(),
        );

        let err = match transport.stream(request).await {
            Ok(_) => panic!("stream request should fail against a closed local port"),
            Err(err) => err,
        };
        let TransportError::Network(message) = err else {
            panic!("expected network error, got {err:?}");
        };

        assert!(message.contains("error sending request"));
        assert!(
            message.contains("caused by:"),
            "stream network error should include source chain, got: {message}"
        );
    }
}
