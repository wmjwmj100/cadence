//! Verifies that the agent retries when the SSE stream terminates before
//! delivering a `response.completed` event.

use codex_core::ModelProviderInfo;
use codex_core::ProviderProfile;
use codex_core::WireApi;
use codex_core::protocol::EventMsg;
use codex_core::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::load_sse_fixture;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_match;
use core_test_support::wait_for_event_with_timeout;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::Request;
use wiremock::Respond;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

fn sse_incomplete() -> String {
    load_sse_fixture("tests/fixtures/incomplete_sse.json")
}

fn test_model_provider(base_url: String) -> ModelProviderInfo {
    ModelProviderInfo {
        name: "openai".into(),
        base_url: Some(base_url),
        env_key: Some("PATH".into()),
        env_key_instructions: None,
        experimental_bearer_token: None,
        wire_api: WireApi::Responses,
        query_params: None,
        http_headers: None,
        env_http_headers: None,
        request_max_retries: Some(1),
        stream_max_retries: Some(1),
        stream_idle_timeout_ms: Some(2_000),
        requires_openai_auth: false,
        supports_websockets: false,
        profile: ProviderProfile::default(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retries_on_early_close() {
    skip_if_no_network!();

    let server = MockServer::start().await;

    struct SeqResponder;
    impl Respond for SeqResponder {
        fn respond(&self, _: &Request) -> ResponseTemplate {
            use std::sync::atomic::AtomicUsize;
            use std::sync::atomic::Ordering;
            static CALLS: AtomicUsize = AtomicUsize::new(0);
            let n = CALLS.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(sse_incomplete(), "text/event-stream")
            } else {
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(
                        sse(vec![
                            ev_response_created("resp_ok"),
                            ev_completed("resp_ok"),
                        ]),
                        "text/event-stream",
                    )
            }
        }
    }

    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(SeqResponder {})
        .expect(1)
        .mount(&server)
        .await;

    // Configure retry behavior explicitly to avoid mutating process-wide
    // environment variables.

    let mut model_provider = test_model_provider(format!("{}/v1", server.uri()));
    model_provider.request_max_retries = Some(0);

    let TestCodex { codex, .. } = test_codex()
        .with_config(move |config| {
            config.model_provider = model_provider;
        })
        .build(&server)
        .await
        .unwrap();

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            origin: codex_protocol::protocol::UserInputOrigin::User,
        })
        .await
        .unwrap();

    // Wait until TurnComplete (should succeed after retry).
    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retries_when_response_headers_arrive_too_late() {
    skip_if_no_network!();

    let server = MockServer::start().await;

    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    struct SeqResponder {
        calls: Arc<AtomicUsize>,
    }

    impl Respond for SeqResponder {
        fn respond(&self, _: &Request) -> ResponseTemplate {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let template = ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    sse(vec![
                        ev_response_created(&format!("resp-{n}")),
                        ev_completed(&format!("resp-{n}")),
                    ]),
                    "text/event-stream",
                );
            if n == 0 {
                template.set_delay(Duration::from_secs(3))
            } else {
                template
            }
        }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(SeqResponder {
            calls: Arc::clone(&calls),
        })
        .mount(&server)
        .await;

    let model_provider = test_model_provider(format!("{}/v1", server.uri()));

    let TestCodex { codex, .. } = test_codex()
        .with_config(move |config| {
            config.model_provider = model_provider;
        })
        .build(&server)
        .await
        .unwrap();

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            origin: codex_protocol::protocol::UserInputOrigin::User,
        })
        .await
        .unwrap();

    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn caps_response_header_start_timeout_retries_at_eight_attempts() {
    skip_if_no_network!();

    let server = MockServer::start().await;

    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    struct SlowResponder {
        calls: Arc<AtomicUsize>,
    }

    impl Respond for SlowResponder {
        fn respond(&self, _: &Request) -> ResponseTemplate {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    sse(vec![
                        ev_response_created(&format!("resp-{n}")),
                        ev_completed(&format!("resp-{n}")),
                    ]),
                    "text/event-stream",
                )
                .set_delay(Duration::from_millis(800))
        }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(SlowResponder {
            calls: Arc::clone(&calls),
        })
        .mount(&server)
        .await;

    let mut model_provider = test_model_provider(format!("{}/v1", server.uri()));
    model_provider.stream_idle_timeout_ms = Some(400);

    let TestCodex { codex, .. } = test_codex()
        .with_config(move |config| {
            config.model_provider = model_provider;
        })
        .build(&server)
        .await
        .unwrap();

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            origin: codex_protocol::protocol::UserInputOrigin::User,
        })
        .await
        .unwrap();

    let error_event = wait_for_event_with_timeout(
        &codex,
        |event| matches!(event, EventMsg::Error(_)),
        Duration::from_secs(120),
    )
    .await;
    let EventMsg::Error(error) = error_event else {
        panic!("expected error event");
    };
    assert!(
        error
            .message
            .contains("timed out waiting for model response stream to start"),
        "unexpected error message: {}",
        error.message
    );
    assert_eq!(calls.load(Ordering::SeqCst), 8);
}

#[tokio::test(start_paused = true)]
async fn retries_response_body_decode_eof_before_first_event_at_eight_attempts() {
    skip_if_no_network!();

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind truncated chunked server");
    let uri = format!("http://{}", listener.local_addr().expect("server address"));

    let server = tokio::spawn(async move {
        for _ in 0..8 {
            let (mut stream, _) = listener.accept().await.expect("accept request");
            read_http_request(&mut stream).await.expect("read request");
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
                )
                .await
                .expect("write response headers");
            stream
                .write_all(b"1")
                .await
                .expect("write partial chunk size");
            let _ = stream.shutdown().await;
        }
    });

    let mut model_provider = test_model_provider(format!("{uri}/v1"));
    model_provider.request_max_retries = Some(0);

    let TestCodex { codex, .. } = test_codex()
        .with_config(move |config| {
            config.model_provider = model_provider;
        })
        .build_with_base_url(format!("{uri}/v1"))
        .await
        .unwrap();

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            origin: codex_protocol::protocol::UserInputOrigin::User,
        })
        .await
        .unwrap();

    let error_event = wait_for_event_with_timeout(
        &codex,
        |event| matches!(event, EventMsg::Error(_)),
        Duration::from_secs(120),
    )
    .await;
    let EventMsg::Error(error) = error_event else {
        panic!("expected error event");
    };
    assert!(
        error.message.contains("error decoding response body")
            && error
                .message
                .contains("unexpected EOF during chunk size line"),
        "unexpected error message: {}",
        error.message
    );

    server.await.expect("truncated chunked server completes");
}

async fn read_http_request(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut request = Vec::new();
    let mut scratch = [0u8; 1024];
    let header_end = loop {
        let read = stream.read(&mut scratch).await?;
        if read == 0 {
            return Ok(request);
        }
        request.extend_from_slice(&scratch[..read]);
        if let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break header_end + 4;
        }
    };

    let headers = String::from_utf8_lossy(&request[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    let received_body_len = request.len().saturating_sub(header_end);
    let remaining_body_len = content_length.saturating_sub(received_body_len);
    if remaining_body_len > 0 {
        let mut rest = vec![0u8; remaining_body_len];
        stream.read_exact(&mut rest).await?;
        request.extend_from_slice(&rest);
    }

    Ok(request)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fails_fast_when_first_sse_event_arrives_too_late() {
    skip_if_no_network!();

    let (gate_tx, gate_rx) = oneshot::channel();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(3)).await;
        let _ = gate_tx.send(());
    });

    let first_attempt = vec![StreamingSseChunk {
        gate: Some(gate_rx),
        body: sse(vec![ev_response_created("resp-late")]),
    }];
    let second_attempt = vec![StreamingSseChunk {
        gate: None,
        body: sse(vec![
            ev_response_created("resp-ok"),
            ev_completed("resp-ok"),
        ]),
    }];
    let (server, _completions) =
        start_streaming_sse_server(vec![first_attempt, second_attempt]).await;

    let model_provider = test_model_provider(format!("{}/v1", server.uri()));

    let TestCodex { codex, .. } = test_codex()
        .with_config(move |config| {
            config.model_provider = model_provider;
        })
        .build_with_streaming_server(&server)
        .await
        .unwrap();

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            origin: codex_protocol::protocol::UserInputOrigin::User,
        })
        .await
        .unwrap();

    let error = wait_for_event_match(&codex, |event| match event {
        EventMsg::Error(ev) => Some(ev.clone()),
        _ => None,
    })
    .await;
    assert!(
        error
            .message
            .contains("timed out waiting for first model response event"),
        "unexpected error message: {}",
        error.message
    );

    assert_eq!(server.requests().await.len(), 1);
    server.shutdown().await;
}
