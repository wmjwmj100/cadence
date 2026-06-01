#![allow(clippy::unwrap_used, clippy::expect_used)]

use anyhow::Result;
use codex_core::config::Constrained;
use codex_core::protocol::AgentStatus;
use codex_core::protocol::AskForApproval;
use codex_core::protocol::SandboxPolicy;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use serde_json::Value;
use serde_json::json;

fn find_call_output_content_and_success(
    requests: &[ResponsesRequest],
    call_id: &str,
) -> (String, Option<bool>) {
    let output = requests
        .iter()
        .flat_map(ResponsesRequest::input)
        .find(|item| {
            item.get("type").and_then(Value::as_str) == Some("function_call_output")
                && item.get("call_id").and_then(Value::as_str) == Some(call_id)
        })
        .and_then(|item| item.get("output").cloned())
        .expect("call output present");
    match output {
        Value::String(content) => (content, None),
        Value::Object(obj) => {
            let content = obj
                .get("content")
                .and_then(Value::as_str)
                .expect("call output content present")
                .to_string();
            let success = obj.get("success").and_then(Value::as_bool);
            (content, success)
        }
        _ => panic!("call output content present"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call_with_message_id_dispatches_and_returns_success() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex();
    let test = builder.build(&server).await?;

    let mut child_config = test.config.clone();
    child_config.permissions.approval_policy = Constrained::allow_any(AskForApproval::OnRequest);
    let child = test.thread_manager.start_thread(child_config).await?;
    let child_id = child.thread_id;

    let call_id = "call-dispatch";
    let call_args = json!({
        "target_agent_name": child_id.to_string(),
        "summary": "delegating to child",
        "content": "run in background",
        "message_id": "1322",
        "need_reply": false
    });

    // Root model asks to invoke call.
    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_function_call(call_id, "call", &serde_json::to_string(&call_args)?),
            ev_completed("resp-1"),
        ]),
    )
    .await;

    // Child agent consumes the delegated prompt and returns a final message.
    let child_mock = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            let body = std::str::from_utf8(&req.body).unwrap_or("");
            body.contains("1322")
                && body.contains("run in background")
                && !body.contains("function_call_output")
        },
        sse(vec![
            ev_response_created("resp-child"),
            ev_assistant_message("msg-child", "subagent done"),
            ev_completed("resp-child"),
        ]),
    )
    .await;

    // Root model receives tool output and completes.
    let output_mock = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            let body = std::str::from_utf8(&req.body).unwrap_or("");
            body.contains("call-dispatch")
                && body.contains("function_call_output")
                && body.contains("call dispatched successfully")
        },
        sse(vec![
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-2"),
        ]),
    )
    .await;

    test.submit_turn_with_policies(
        "delegate and wait",
        AskForApproval::Never,
        SandboxPolicy::DangerFullAccess,
    )
    .await?;

    let output = find_call_output_content_and_success(&output_mock.requests(), call_id);
    assert_eq!(
        output.0,
        "call dispatched successfully; continue without waiting"
    );
    assert_eq!(
        output.1, None,
        "FunctionCallOutputPayload.success is internal metadata and is not serialized on the wire"
    );

    let child_user_texts = child_mock
        .requests()
        .iter()
        .flat_map(|request| request.message_input_texts("user"))
        .collect::<Vec<_>>();
    assert!(
        child_user_texts
            .iter()
            .any(|text| text.contains("Collaborating agents and summary lists")),
        "expected child request to include collaborator status context"
    );
    assert!(
        child_user_texts.iter().any(|text| text.contains("1322")),
        "expected child request to include delegated call message id"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_preserves_fifo_for_self_targeted_request_before_callback_reply() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex();
    let test = builder.build(&server).await?;
    let root_thread_id = test.session_configured.session_id.to_string();

    let call_request_id = "callback-request";
    let call_id = "call-request";
    let wait_call_id = "wait-callback";

    let call_args = json!({
        "target_agent_name": root_thread_id,
        "summary": "requesting callback reply",
        "content": "please reply",
        "message_id": call_request_id,
        "need_reply": true
    });
    let wait_args = json!({
        "summary": "waiting for callback reply",
        "timeout_ms": 300
    });

    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_function_call(call_id, "call", &serde_json::to_string(&call_args)?),
            ev_function_call(wait_call_id, "wait", &serde_json::to_string(&wait_args)?),
            ev_completed("resp-1"),
        ]),
    )
    .await;

    let output_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "handled"),
            ev_completed("resp-2"),
        ]),
    )
    .await;

    test.submit_turn_with_policies(
        "dispatch then wait for correlated callback",
        AskForApproval::Never,
        SandboxPolicy::DangerFullAccess,
    )
    .await?;

    let (wait_output, wait_success) =
        find_call_output_content_and_success(&output_mock.requests(), wait_call_id);
    assert_eq!(
        wait_success, None,
        "FunctionCallOutputPayload.success is internal metadata and is not serialized on the wire"
    );

    assert!(wait_output.contains("please reply"));
    assert!(!wait_output.contains("timedOut"));
    assert!(!wait_output.contains("pendingReplyTargets"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_times_out_when_target_completes_without_callback() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex();
    let test = builder.build(&server).await?;

    let mut child_config = test.config.clone();
    child_config.permissions.approval_policy = Constrained::allow_any(AskForApproval::OnRequest);
    let child = test.thread_manager.start_thread(child_config).await?;
    let child_id = child.thread_id;

    let call_id = "call-no-callback";
    let wait_call_id = "wait-no-callback";
    let request_message_id = "wait-no-callback-request";
    let call_args = json!({
        "target_agent_name": child_id.to_string(),
        "summary": "requesting child callback",
        "content": "please reply",
        "message_id": request_message_id,
        "need_reply": true
    });
    let wait_args = json!({
        "summary": "waiting for mismatched reply probe",
        "timeout_ms": 120
    });

    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_function_call(call_id, "call", &serde_json::to_string(&call_args)?),
            ev_function_call(wait_call_id, "wait", &serde_json::to_string(&wait_args)?),
            ev_completed("resp-1"),
        ]),
    )
    .await;

    let child_mock = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            let body = std::str::from_utf8(&req.body).unwrap_or("");
            body.contains("wait-no-callback-request")
                && body.contains("please reply")
                && !body.contains("function_call_output")
        },
        sse(vec![
            ev_response_created("resp-child"),
            ev_assistant_message("msg-child", "done without callback"),
            ev_completed("resp-child"),
        ]),
    )
    .await;

    let output_mock = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            let body = std::str::from_utf8(&req.body).unwrap_or("");
            body.contains("wait-no-callback")
                && body.contains("function_call_output")
                && body.contains("timedOut")
        },
        sse(vec![
            ev_assistant_message("msg-1", "handled"),
            ev_completed("resp-2"),
        ]),
    )
    .await;

    test.submit_turn_with_policies(
        "wait should timeout when target completes without callback",
        AskForApproval::Never,
        SandboxPolicy::DangerFullAccess,
    )
    .await?;

    let child_user_texts = child_mock
        .requests()
        .iter()
        .flat_map(|request| request.message_input_texts("user"))
        .collect::<Vec<_>>();
    assert!(
        child_user_texts
            .iter()
            .any(|text| text.contains(request_message_id)),
        "expected child to receive delegated request message id"
    );

    let child_thread = test.thread_manager.get_thread(child_id).await?;
    assert!(matches!(
        child_thread.agent_status().await,
        AgentStatus::Completed(_)
    ));

    let (wait_output, wait_success) =
        find_call_output_content_and_success(&output_mock.requests(), wait_call_id);
    assert_eq!(
        wait_success, None,
        "FunctionCallOutputPayload.success is internal metadata and is not serialized on the wire"
    );

    let wait_json: Value = serde_json::from_str(&wait_output)?;
    assert_eq!(wait_json["timedOut"], Value::Bool(true));
    assert_eq!(
        wait_json["messages"].as_array().map(std::vec::Vec::len),
        Some(0)
    );
    assert_eq!(
        wait_json["satisfiedTargets"]
            .as_array()
            .map(std::vec::Vec::len),
        Some(0)
    );
    assert_eq!(
        wait_json["unsatisfiedTargets"]
            .as_array()
            .map(std::vec::Vec::len),
        Some(1)
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_completes_on_first_fifo_inbox_message_even_with_later_mismatched_reply_to_message_id()
-> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex();
    let test = builder.build(&server).await?;
    let root_thread_id = test.session_configured.session_id.to_string();

    let expected_message_id = "expected-message-id";
    let call_id = "call-mismatch-request";
    let wait_call_id = "wait-mismatch";

    let call_args = json!({
        "target_agent_name": root_thread_id,
        "summary": "requesting mismatched reply probe",
        "content": "please reply",
        "message_id": expected_message_id,
        "need_reply": true
    });
    let wait_args = json!({
        "summary": "waiting for callback result",
        "timeout_ms": 120
    });

    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_function_call(call_id, "call", &serde_json::to_string(&call_args)?),
            ev_function_call(wait_call_id, "wait", &serde_json::to_string(&wait_args)?),
            ev_completed("resp-1"),
        ]),
    )
    .await;

    let output_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "handled"),
            ev_completed("resp-2"),
        ]),
    )
    .await;

    test.submit_turn_with_policies(
        "wait should complete on any inbox message",
        AskForApproval::Never,
        SandboxPolicy::DangerFullAccess,
    )
    .await?;

    let (wait_output, wait_success) =
        find_call_output_content_and_success(&output_mock.requests(), wait_call_id);
    assert_eq!(
        wait_success, None,
        "FunctionCallOutputPayload.success is internal metadata and is not serialized on the wire"
    );

    assert!(wait_output.contains("please reply"));
    assert!(!wait_output.contains("timedOut"));
    assert!(!wait_output.contains("pendingReplyTargets"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call_continue_missing_target_maps_to_target_not_found() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex();
    let test = builder.build(&server).await?;

    let missing_target = codex_protocol::ThreadId::new();
    let call_id = "call-missing-target";
    let call_args = json!({
        "target_agent_name": missing_target.to_string(),
        "summary": "probing missing target dispatch",
        "content": "hello",
        "message_id": "m-1"
    });

    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_function_call(call_id, "call", &serde_json::to_string(&call_args)?),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    let output_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "handled"),
            ev_completed("resp-2"),
        ]),
    )
    .await;

    test.submit_turn_with_policies(
        "delegate without wait",
        AskForApproval::Never,
        SandboxPolicy::DangerFullAccess,
    )
    .await?;

    let output = output_mock
        .single_request()
        .function_call_output_content_and_success(call_id)
        .expect("call output present");
    assert_eq!(
        output.0.as_deref(),
        Some("call failed; continue without waiting")
    );
    assert_eq!(
        output.1, None,
        "FunctionCallOutputPayload.success is internal metadata and is not serialized on the wire"
    );

    Ok(())
}
