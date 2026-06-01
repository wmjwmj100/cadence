use codex_core::protocol::EventMsg;
use codex_core::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_message_item_added;
use core_test_support::responses::ev_output_text_delta;
use core_test_support::responses::ev_response_created;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use tokio::sync::oneshot;

fn ev_message_item_done(id: &str, text: &str) -> Value {
    serde_json::json!({
        "type": "response.output_item.done",
        "item": {
            "type": "message",
            "role": "assistant",
            "id": id,
            "content": [{"type": "output_text", "text": text}]
        }
    })
}

fn sse_event(event: Value) -> String {
    responses::sse(vec![event])
}

fn message_input_texts(body: &Value, role: &str) -> Vec<String> {
    body.get("input")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("message"))
        .filter(|item| item.get("role").and_then(Value::as_str) == Some(role))
        .filter_map(|item| item.get("content").and_then(Value::as_array))
        .flatten()
        .filter(|span| span.get("type").and_then(Value::as_str) == Some("input_text"))
        .filter_map(|span| span.get("text").and_then(Value::as_str).map(str::to_owned))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "TODO(aibrahim): flaky"]
async fn injected_user_input_triggers_follow_up_request_with_deltas() {
    let (gate_completed_tx, gate_completed_rx) = oneshot::channel();

    let first_chunks = vec![
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_response_created("resp-1")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_message_item_added("msg-1", "")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_output_text_delta("first ")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_output_text_delta("turn")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_message_item_done("msg-1", "first turn")),
        },
        StreamingSseChunk {
            gate: Some(gate_completed_rx),
            body: sse_event(ev_completed("resp-1")),
        },
    ];

    let second_chunks = vec![
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_response_created("resp-2")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_completed("resp-2")),
        },
    ];

    let (server, _completions) =
        start_streaming_sse_server(vec![first_chunks, second_chunks]).await;

    let codex = test_codex()
        .with_model("gpt-5.1")
        .build_with_streaming_server(&server)
        .await
        .unwrap()
        .codex;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "first prompt".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            origin: codex_protocol::protocol::UserInputOrigin::User,
        })
        .await
        .unwrap();

    wait_for_event(&codex, |event| {
        matches!(event, EventMsg::AgentMessageContentDelta(_))
    })
    .await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "second prompt".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            origin: codex_protocol::protocol::UserInputOrigin::User,
        })
        .await
        .unwrap();

    let _ = gate_completed_tx.send(());

    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;

    let requests = server.requests().await;
    assert_eq!(requests.len(), 2);

    let first_body: Value = serde_json::from_slice(&requests[0]).expect("parse first request");
    let second_body: Value = serde_json::from_slice(&requests[1]).expect("parse second request");

    let first_texts = message_input_texts(&first_body, "user");
    assert!(first_texts.iter().any(|text| text == "first prompt"));
    assert!(!first_texts.iter().any(|text| text == "second prompt"));

    let second_texts = message_input_texts(&second_body, "user");
    assert!(second_texts.iter().any(|text| text == "first prompt"));
    assert!(second_texts.iter().any(|text| text == "second prompt"));

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn termination_judge_can_inject_continue_follow_up_request() {
    let first_chunks = vec![
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_response_created("resp-1")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_message_item_added("msg-1", "")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_output_text_delta("Next I will finish the task.")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_message_item_done(
                "msg-1",
                "Next I will finish the task.",
            )),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_completed("resp-1")),
        },
    ];

    let judge_chunks = vec![
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_response_created("resp-judge")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_message_item_added("msg-judge", "")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_output_text_delta(
                r#"{"decision":"continue","user_next_steps":null}"#,
            )),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_message_item_done(
                "msg-judge",
                r#"{"decision":"continue","user_next_steps":null}"#,
            )),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_completed("resp-judge")),
        },
    ];

    let second_chunks = vec![
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_response_created("resp-2")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_message_item_added("msg-2", "")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_output_text_delta("Done.")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_message_item_done("msg-2", "Done.")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_completed("resp-2")),
        },
    ];

    let judge_finalize_chunks = vec![
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_response_created("resp-judge-2")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_message_item_added("msg-judge-2", "")),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_output_text_delta(
                r#"{"decision":"finalize","user_next_steps":"You can now review or run tests."}"#,
            )),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_message_item_done(
                "msg-judge-2",
                r#"{"decision":"finalize","user_next_steps":"You can now review or run tests."}"#,
            )),
        },
        StreamingSseChunk {
            gate: None,
            body: sse_event(ev_completed("resp-judge-2")),
        },
    ];

    let (server, _completions) = start_streaming_sse_server(vec![
        first_chunks,
        judge_chunks,
        second_chunks,
        judge_finalize_chunks,
    ])
    .await;

    let codex = test_codex()
        .with_model("gpt-5.1")
        .build_with_streaming_server(&server)
        .await
        .unwrap()
        .codex;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "first prompt".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            origin: codex_protocol::protocol::UserInputOrigin::User,
        })
        .await
        .unwrap();

    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;

    let requests = server.requests().await;
    assert_eq!(requests.len(), 4);

    let first_body: Value = serde_json::from_slice(&requests[0]).expect("parse first request");
    let second_body: Value =
        serde_json::from_slice(&requests[1]).expect("parse first judge request");
    let third_body: Value = serde_json::from_slice(&requests[2]).expect("parse continued request");
    let fourth_body: Value =
        serde_json::from_slice(&requests[3]).expect("parse final judge request");

    let first_texts = message_input_texts(&first_body, "user");
    assert!(first_texts.iter().any(|text| text.contains("first prompt")));
    assert!(!first_texts.iter().any(|text| text == "continue"));

    let judge_texts = message_input_texts(&second_body, "user");
    assert_eq!(judge_texts.len(), 1);
    assert!(judge_texts[0].contains("Recent assistant messages"));
    assert!(judge_texts[0].contains("Next I will finish the task."));
    assert!(!judge_texts[0].contains("Done."));

    let continued_texts = message_input_texts(&third_body, "user");
    assert!(
        continued_texts
            .iter()
            .any(|text| text.contains("first prompt"))
    );
    assert!(
        continued_texts
            .iter()
            .any(|text| text.starts_with("continue"))
    );

    let final_judge_texts = message_input_texts(&fourth_body, "user");
    assert_eq!(final_judge_texts.len(), 1);
    assert!(final_judge_texts[0].contains("Next I will finish the task."));
    assert!(final_judge_texts[0].contains("Done."));

    server.shutdown().await;
}
