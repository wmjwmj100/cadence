#![cfg(not(target_os = "windows"))]
#![allow(clippy::expect_used, clippy::unwrap_used)]

use core_test_support::responses;
use core_test_support::test_codex_exec::test_codex_exec;
use pretty_assertions::assert_eq;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_prefix_reads_prompt_from_stdin_and_writes_last_message_file() -> anyhow::Result<()> {
    let test = test_codex_exec();
    let last_message_file = test.cwd_path().join("last-message.txt");

    let server = responses::start_mock_server().await;
    let body = responses::sse(vec![
        responses::ev_response_created("resp1"),
        responses::ev_assistant_message("m1", "fixture hello"),
        responses::ev_completed("resp1"),
    ]);
    responses::mount_sse_once(&server, body).await;

    test.cmd_with_server(&server)
        .arg("exec")
        .arg("--skip-git-repo-check")
        .arg("--output-last-message")
        .arg(&last_message_file)
        .arg("-C")
        .arg(test.cwd_path())
        .arg("-m")
        .arg("gpt-5.1")
        .arg("-")
        .write_stdin("prompt from stdin")
        .assert()
        .success();

    let last_message = std::fs::read_to_string(&last_message_file)?;
    assert_eq!(last_message, "fixture hello");

    Ok(())
}
