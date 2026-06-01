use std::path::Path;

use chrono::Utc;

use axum::serve;

use codex_core::CodexAuth;
use codex_core::built_in_model_providers;
use codex_core::office::AgentSummary;
use codex_core::office::AgentWorkspace;
use codex_core::office::CollabMessage;
use codex_core::office::GatewayDecision;
use codex_core::office::HumanWaitBook;
use codex_core::office::MessageDelivery;
use codex_core::office::OFFICE_AGENT_SYSTEM_PROMPT_RULES;
use codex_core::office::OFFICE_SCHEDULER_SYSTEM_PROMPT;
use codex_core::office::OfficeMessageHub;
use codex_core::office::OfficeRuntimeStore;
use codex_core::office::OfficeTimeline;
use codex_core::office::OfficeTimelineEvent;
use codex_core::office::OfficeWebApp;
use codex_core::office::OfficeWebConfig;
use codex_core::office::OpportunityScheduler;
use codex_core::office::Participant;
use codex_core::office::ParticipantId;
use codex_core::office::ParticipantKind;
use codex_core::office::PersistentPilotDirectory;
use codex_core::office::PilotDirectory;
use codex_core::office::PilotStoreSnapshot;
use codex_core::office::PilotWebSession;
use codex_core::office::ReplyObligationBook;
use codex_core::office::ToolCapabilityPolicy;
use codex_core::office::UserProfileFactCategory;
use codex_core::office::UserProfileFactStatus;
use codex_core::office::WaitOutcome;
use codex_core::office::WaitPolicy;
use codex_core::office::WorkspaceArea;
use codex_core::office::WorkspaceItemKind;
use codex_core::office::WorkspacePolicy;
use codex_core::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::load_default_config_for_test;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_reasoning_item;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use http::header::ACCEPT;
use http::header::COOKIE;
use http::header::SET_COOKIE;
use pretty_assertions::assert_eq;
use serde_json::Value;
use std::sync::Arc;
use std::sync::OnceLock;
use tempfile::tempdir;
use tokio::net::TcpListener;

#[test]
fn office_task_1_runtime_intents_remain_behind_gateway_language() {
    let gateway = std::fs::read_to_string(manifest_path("src/tools/gateway.rs"))
        .expect("gateway source should be readable from repo root");

    assert!(gateway.contains("Middleware boundary between Agent Runtime tool intents"));
    assert!(gateway.contains("Future HTTP/Docker routing"));
    assert!(gateway.contains("ToolCapabilityPolicy"));
    assert!(gateway.contains("GatewayDecision::Denied"));
    assert!(gateway.contains("ToolRegistry"));
    assert!(!gateway.contains("pub struct Docker"));
}

#[test]
fn office_task_2_participants_messages_and_reply_obligations_match_swarm_semantics() {
    let agent = Participant::agent("agent_a", "Agent A");
    let human = Participant::human("user_1", "Owner");
    assert_eq!(agent.kind, ParticipantKind::Agent);
    assert_eq!(human.kind, ParticipantKind::Human);

    let request =
        CollabMessage::new("msg_1", agent.id.clone(), human.id.clone(), "Need input").need_reply();
    let reply = CollabMessage::new("msg_2", human.id.clone(), agent.id.clone(), "Approved")
        .reply_to("msg_1");

    let mut obligations = ReplyObligationBook::default();
    obligations.record_message(&request);
    assert!(
        !obligations
            .get("msg_1")
            .expect("obligation exists")
            .resolved()
    );

    obligations.record_message(&reply);
    let obligation = obligations.get("msg_1").expect("obligation exists");
    assert!(obligation.resolved());
    assert_eq!(obligation.resolved_by.as_deref(), Some("msg_2"));
}

#[test]
fn office_task_2_message_hub_routes_target_id_without_sender_declaring_target_kind() {
    let agent = Participant::agent("agent_a", "Agent A");
    let worker = Participant::agent("agent_worker", "Worker Agent");
    let human = Participant::human("user_1", "Owner");
    let mut hub = OfficeMessageHub::new();
    hub.register(agent.clone());
    hub.register(worker.clone());
    hub.register(human.clone());

    let agent_delivery = hub
        .send(
            CollabMessage::new(
                "agent-request",
                agent.id.clone(),
                worker.id.clone(),
                "Please estimate cost",
            )
            .need_reply(),
        )
        .expect("agent target should route");
    assert_eq!(
        agent_delivery,
        MessageDelivery::AgentQueued {
            target_id: worker.id.clone()
        }
    );

    let human_delivery = hub
        .send(
            CollabMessage::new(
                "human-request",
                agent.id.clone(),
                human.id.clone(),
                "Can you approve this budget?",
            )
            .need_reply(),
        )
        .expect("human target should route");
    assert_eq!(
        human_delivery,
        MessageDelivery::HumanQueued {
            target_id: human.id.clone()
        }
    );
    assert_eq!(hub.queued_count(&human.id), 1);
    assert!(
        !hub.reply_obligation("human-request")
            .expect("human reply obligation exists")
            .resolved()
    );

    let reply = CollabMessage::new(
        "human-reply",
        human.id.clone(),
        agent.id.clone(),
        "Approved",
    )
    .reply_to("human-request");
    hub.send(reply).expect("human reply should route to agent");
    assert_eq!(
        hub.reply_obligation("human-request")
            .expect("human reply obligation exists")
            .resolved_by
            .as_deref(),
        Some("human-reply")
    );

    let agent_message = hub
        .pop_for_from(&agent.id, &human.id)
        .expect("wait(target_id) should consume the human reply");
    assert_eq!(agent_message.message_id, "human-reply");
}

#[test]
fn office_task_3_wait_distinguishes_agent_timeout_human_suspend_and_target_filtering() {
    let policy = WaitPolicy::default();
    let agent_a = ParticipantId::new("agent_a");
    let agent_b = ParticipantId::new("agent_b");
    let human = ParticipantId::new("user_1");

    let mut inbox = vec![CollabMessage::new(
        "unrelated",
        agent_b.clone(),
        agent_a.clone(),
        "not from the requested human",
    )];

    assert!(matches!(
        policy.wait_for(&human, ParticipantKind::Human, &mut inbox),
        WaitOutcome::HumanSuspended
    ));
    assert_eq!(inbox.len(), 1, "unrelated messages must remain queued");

    assert!(matches!(
        policy.wait_for(&agent_b, ParticipantKind::Agent, &mut inbox),
        WaitOutcome::Delivered(message) if message.message_id == "unrelated"
    ));

    let mut empty_inbox = Vec::new();
    assert!(matches!(
        policy.wait_for(&agent_b, ParticipantKind::Agent, &mut empty_inbox),
        WaitOutcome::AgentTimeout { timeout } if timeout.as_secs() == 7 * 60
    ));
}

#[test]
fn office_task_3_human_wait_tickets_suspend_until_matching_human_message() {
    let agent = ParticipantId::new("agent_a");
    let human = ParticipantId::new("user_1");
    let other_human = ParticipantId::new("user_2");
    let mut waits = HumanWaitBook::default();

    let ticket = waits.suspend(
        agent.clone(),
        human.clone(),
        Some("budget-request".to_string()),
    );
    assert_eq!(ticket.waiting_agent_id, agent);
    assert_eq!(ticket.human_id, human);
    assert_eq!(waits.pending_for_agent(&agent).len(), 1);

    let unrelated = CollabMessage::new("other-reply", other_human, agent.clone(), "Not mine")
        .reply_to("budget-request");
    assert!(
        waits.wake_for_message(&unrelated).is_none(),
        "another human must not wake this Agent wait"
    );
    assert_eq!(waits.pending_for_agent(&agent).len(), 1);

    let matching = CollabMessage::new("human-reply", human, agent.clone(), "Approved")
        .reply_to("budget-request");
    let resumed = waits
        .wake_for_message(&matching)
        .expect("matching human reply should wake suspended agent");
    assert_eq!(
        resumed.requested_message_id.as_deref(),
        Some("budget-request")
    );
    assert!(waits.pending_for_agent(&agent).is_empty());
}

#[test]
fn office_task_4_workspace_policy_keeps_owner_uploads_private_and_team_work_public() {
    let policy = WorkspacePolicy;
    assert_eq!(policy.area_for_owner_upload(), WorkspaceArea::Private);
    assert_eq!(policy.area_for_work(false, false), WorkspaceArea::Private);
    assert_eq!(policy.area_for_work(true, false), WorkspaceArea::Public);
    assert_eq!(policy.area_for_work(false, true), WorkspaceArea::Public);
    assert!(policy.system_prompt_rules().contains("私有工作空间"));
    assert!(policy.system_prompt_rules().contains("公共空间"));

    let mut workspace = AgentWorkspace::new("agent_a", "/private/agent_a", "/public/team");
    let upload = workspace.receive_owner_upload("owner.md", "owner-only raw notes");
    assert_eq!(upload.area, WorkspaceArea::Private);
    assert_eq!(upload.kind, WorkspaceItemKind::PrivateMaterial);

    let work = workspace.write_work_product("research.md", "team research", true, false);
    assert_eq!(work.area, WorkspaceArea::Public);
    assert_eq!(work.kind, WorkspaceItemKind::WorkProduct);

    let migrated = workspace.migrate_summary_to_public("owner-summary.md", "safe public summary");
    assert_eq!(migrated.area, WorkspaceArea::Public);
    assert_eq!(migrated.kind, WorkspaceItemKind::SharedMaterial);
}

#[test]
fn office_task_5_tool_capabilities_allow_only_declared_tools_before_execution() {
    let policy = ToolCapabilityPolicy::default()
        .allow("shell")
        .allow("apply_patch");

    assert_eq!(policy.check("shell"), GatewayDecision::Allowed);
    assert_eq!(
        policy.check("web_search"),
        GatewayDecision::Denied {
            reason: "tool `web_search` is not allowed for this Agent profile".to_string(),
        }
    );
    assert_eq!(
        policy.allowed_tools().collect::<Vec<_>>(),
        vec!["apply_patch", "shell"]
    );
}

#[test]
fn office_task_6_timeline_records_facts_and_summarizes_every_15_agent_steps() {
    let mut timeline = OfficeTimeline::new();
    timeline.append_event(OfficeTimelineEvent::Message {
        message_id: "msg_1".to_string(),
        from: "user_1".to_string(),
        to: "agent_a".to_string(),
    });
    timeline.append_event(OfficeTimelineEvent::ToolIntent {
        agent_id: "agent_a".to_string(),
        tool_name: "shell".to_string(),
    });

    for step in 1..15 {
        assert!(
            timeline
                .record_agent_step("agent_a", format!("step {step}"))
                .is_none()
        );
    }

    let summary = timeline
        .record_agent_step("agent_a", "step 15")
        .expect("15th agent step should create summary");
    assert_eq!(summary.step_index, 15);
    assert_eq!(summary.source_event_start, 0);
    assert_eq!(summary.source_event_end, 17);
    assert_eq!(summary.source_event_count, 17);
    assert_eq!(timeline.total_steps(), 15);
    assert_eq!(timeline.summaries(), &[summary]);
    assert!(matches!(
        timeline.events().last(),
        Some(OfficeTimelineEvent::Summary { step_index: 15, .. })
    ));
}

#[test]
fn office_task_7_scheduler_finds_high_value_shared_summary_opportunities_only() {
    let scheduler = OpportunityScheduler;
    assert!(scheduler.system_prompt().contains("1+1 大于 2"));
    assert!(scheduler.system_prompt().contains("不是提醒简单阻塞"));

    let summaries = vec![
        AgentSummary::new(
            "agent_research",
            "Found market evidence",
            ["market", "pricing"],
        ),
        AgentSummary::new("agent_product", "Drafted product angle", ["market", "ux"]),
        AgentSummary::new("agent_blocked", "waiting for reply", ["market"]),
    ];

    let opportunities = scheduler.discover(&summaries);
    assert_eq!(opportunities.len(), 1);
    assert_eq!(
        opportunities[0].participants,
        ("agent_research".to_string(), "agent_product".to_string())
    );
    assert_eq!(opportunities[0].shared_topics, vec!["market".to_string()]);
    assert!(opportunities[0].expected_gain.contains("agent_research"));
    assert!(opportunities[0].expected_gain.contains("agent_product"));
    assert!(opportunities[0].expected_gain.contains("market"));
}

#[test]
fn office_task_8_minimum_real_scenarios_are_represented_as_timeline_events() {
    let scenarios = [
        "Research Team 调研",
        "产品需求评审",
        "代码问题排查",
        "会议准备",
        "每日复盘",
    ];
    let mut timeline = OfficeTimeline::new();

    for scenario in scenarios {
        timeline.append_event(OfficeTimelineEvent::AgentStep {
            agent_id: "agent_scenario".to_string(),
            summary: scenario.to_string(),
        });
    }

    let event_text = format!("{:?}", timeline.events());
    for scenario in scenarios {
        assert!(event_text.contains(scenario));
    }
}

#[test]
fn office_task_9_pilot_directory_binds_six_humans_to_agents_and_updates_profiles() {
    let mut directory = PilotDirectory::six_person_seed();
    assert_eq!(directory.account_count(), 6);
    assert!(directory.login("ceo", "wrong-password").is_none());

    let session = directory
        .login("ceo", "password")
        .expect("seed login works");
    assert_eq!(session.user_id, "user_ceo");
    assert_eq!(session.agent_id, "agent_ceo");

    let agent_profile = directory
        .agent_profile(&session.agent_id)
        .expect("agent profile exists");
    assert_eq!(agent_profile.owner_user_id, session.user_id);
    assert!(agent_profile.allowed_tools.contains(&"call".to_string()));
    assert!(agent_profile.allowed_tools.contains(&"wait".to_string()));

    let updated = directory
        .run_mini_interview(
            &session.user_id,
            "CEO / Fundraising",
            ["融资判断", "战略叙事"],
            ["低风险格式调整"],
            "先说风险，再给建议",
        )
        .expect("interview updates profile");
    assert_eq!(updated.role, "CEO / Fundraising");
    assert_eq!(updated.report_preference, "先说风险，再给建议");

    let reflection = directory
        .daily_reflection(&session.user_id, ["客户关系", "低风险格式调整"])
        .expect("daily reflection updates profile");
    assert_eq!(reflection.user_id, session.user_id);

    let profile = directory
        .human_profile(&session.user_id)
        .expect("human profile exists");
    assert!(profile.capability_labels.contains(&"客户关系".to_string()));
    assert!(
        profile
            .do_not_disturb
            .contains(&"低风险格式调整".to_string())
    );
    assert!(profile.last_reflection_at.is_some());
}

#[test]
fn office_owner_message_profile_extraction_handles_negative_like_and_dnd_suffix() {
    let mut directory = PilotDirectory::six_person_seed();
    let session = directory
        .login("employee_e", "password")
        .expect("seed login works");

    let learned = directory
        .record_owner_message_profile_evidence(
            &session.user_id,
            &session.agent_id,
            "profile-edge-demo",
            "我不喜欢长篇空话。我擅长 Rust 服务端和数据库调优。不要频繁打扰我。",
        )
        .expect("profile exists");

    assert!(learned.iter().any(|fact| {
        fact.category == UserProfileFactCategory::Dislike && fact.value == "长篇空话"
    }));
    assert!(!learned.iter().any(|fact| {
        fact.category == UserProfileFactCategory::Like && fact.value == "长篇空话"
    }));
    assert!(learned.iter().any(|fact| {
        fact.category == UserProfileFactCategory::Capability
            && fact.value == "Rust 服务端和数据库调优"
    }));
    assert!(learned.iter().any(|fact| {
        fact.category == UserProfileFactCategory::DoNotDisturb && fact.value == "频繁打扰"
    }));

    let profile = directory
        .human_profile(&session.user_id)
        .expect("human profile exists");
    assert!(!profile.likes.contains(&"长篇空话".to_string()));
    assert!(profile.dislikes.contains(&"长篇空话".to_string()));
    assert!(profile.do_not_disturb.contains(&"频繁打扰".to_string()));
    assert!(profile.profile_facts.iter().any(|fact| {
        fact.category == UserProfileFactCategory::Dislike
            && fact.value == "长篇空话"
            && fact.status == UserProfileFactStatus::Active
            && fact
                .evidence_message_ids
                .contains(&"profile-edge-demo".to_string())
    }));
}

#[test]
fn office_persistent_directory_survives_restart_with_sessions_and_profile_updates() {
    let temp = tempdir().expect("tempdir");
    let store_path = temp.path().join("office-store.json");

    let mut directory =
        PersistentPilotDirectory::open(store_path.clone()).expect("store should open");
    assert_eq!(directory.account_count(), 6);
    let session = directory
        .login("employee_c", "password")
        .expect("seed login works");
    assert_eq!(session.user_id, "user_c");
    assert_eq!(session.agent_id, "agent_c");
    directory
        .create_web_session("session-c".to_string(), session.clone())
        .expect("session persists");
    directory
        .run_mini_interview(
            &session.user_id,
            "产品 / 增长",
            ["增长实验", "访谈归纳"],
            ["低价值同步会"],
            "先给决策建议",
        )
        .expect("interview persists")
        .expect("profile exists");
    directory
        .daily_reflection(&session.user_id, ["客户洞察", "低价值同步会"])
        .expect("reflection persists")
        .expect("profile exists");

    let reopened = PersistentPilotDirectory::open(store_path).expect("store should reopen");
    assert_eq!(reopened.account_count(), 6);
    let web_session = reopened.web_session("session-c").expect("session survives");
    assert_eq!(web_session.user_id, "user_c");
    let profile = reopened.human_profile("user_c").expect("profile survives");
    assert_eq!(profile.role, "产品 / 增长");
    assert!(profile.capability_labels.contains(&"增长实验".to_string()));
    assert!(profile.capability_labels.contains(&"客户洞察".to_string()));
    assert!(profile.do_not_disturb.contains(&"低价值同步会".to_string()));
    assert_eq!(profile.report_preference, "先给决策建议");
    assert!(profile.last_reflection_at.is_some());
}

#[tokio::test]
async fn office_web_browser_and_json_api_login_profile_interview_reflection_logout() {
    let _office_web_lock = office_web_test_lock().lock().await;
    let temp = tempdir().expect("tempdir");
    let store_path = temp.path().join("office-web-store.json");
    let codex_home = temp.path().join("codex-home");
    let app = OfficeWebApp::open(
        OfficeWebConfig::new(store_path.clone()).with_codex_home(codex_home.clone()),
    )
    .expect("office web app opens");
    let (base_url, handle) = spawn_office_web(app).await;
    let client = reqwest::Client::new();

    let login_page = client
        .get(format!("{base_url}/login"))
        .send()
        .await
        .expect("login page response");
    assert_eq!(login_page.status(), reqwest::StatusCode::OK);
    assert!(
        login_page
            .text()
            .await
            .expect("html")
            .contains("Codex Office 登录")
    );

    let bad_login = client
        .post(format!("{base_url}/api/login"))
        .json(&serde_json::json!({ "username": "employee_b", "password": "wrong" }))
        .send()
        .await
        .expect("bad login response");
    assert_eq!(bad_login.status(), reqwest::StatusCode::UNAUTHORIZED);

    let login = client
        .post(format!("{base_url}/api/login"))
        .json(&serde_json::json!({ "username": "employee_b", "password": "password" }))
        .send()
        .await
        .expect("login response");
    assert_eq!(login.status(), reqwest::StatusCode::OK);
    let cookie = login_cookie(&login);
    assert!(cookie.starts_with("codex_office_session="));
    let login_body: Value = login.json().await.expect("login json");
    let token = login_body["session_token"]
        .as_str()
        .expect("session token")
        .to_string();
    assert!(!token.is_empty());
    assert_eq!(login_body["me"]["user_id"].as_str(), Some("user_b"));
    assert_eq!(login_body["me"]["agent_id"].as_str(), Some("agent_b"));

    let me = client
        .get(format!("{base_url}/api/me"))
        .header(ACCEPT, "application/json")
        .header(COOKIE, cookie.as_str())
        .send()
        .await
        .expect("me response");
    assert_eq!(me.status(), reqwest::StatusCode::OK);
    let me_body: Value = me.json().await.expect("me json");
    assert_eq!(me_body["username"].as_str(), Some("employee_b"));
    assert_eq!(me_body["agent_id"].as_str(), Some("agent_b"));
    assert_eq!(
        me_body["agent_profile"]["owner_user_id"].as_str(),
        Some("user_b")
    );
    assert_eq!(
        me_body["owner_binding"]["owner_user_id"].as_str(),
        Some("user_b")
    );
    assert_eq!(
        me_body["owner_binding"]["binding_type"].as_str(),
        Some("primary_owner")
    );
    let runtime_after_me = OfficeRuntimeStore::open(temp.path().join("office-runtime.json"))
        .expect("runtime store opens after me");
    assert_eq!(
        runtime_after_me
            .get("agent_b")
            .map(|record| record.owner_user_id.as_str()),
        Some("user_b")
    );
    let owner_profile_prompt_path = codex_home.join("agents/agent_b/owner_profile.md");
    assert!(
        !owner_profile_prompt_path.exists(),
        "owner profile prompt should be manually synced, not updated on login"
    );
    let office_memory_owner_profile_path = temp
        .path()
        .join("office-memory/agents/agent_b/owner_profile.md");
    let initial_office_memory_owner_profile =
        std::fs::read_to_string(&office_memory_owner_profile_path)
            .expect("office memory owner profile template exists");
    assert_eq!(initial_office_memory_owner_profile, "# Owner Profile\n\n");
    assert_eq!(me_body["agent_state"]["state"].as_str(), Some("idle"));
    assert_public_agent_state(&me_body["agent_state"]["state"]);
    assert_eq!(me_body["agent_inbox"]["agent_id"].as_str(), Some("agent_b"));
    assert_eq!(me_body["agent_inbox"]["queued_count"].as_u64(), Some(0));
    assert_eq!(
        me_body["agent_activity"]["agent_id"].as_str(),
        Some("agent_b")
    );
    assert!(
        json_array(
            &me_body["agent_activity"]["entries"],
            "agent activity entries"
        )
        .is_empty()
    );
    assert!(
        me_body["pending_owner_replies"]
            .as_array()
            .expect("pending owner replies array")
            .is_empty()
    );

    let owner_message = client
        .post(format!("{base_url}/api/inbox"))
        .header(COOKIE, cookie.as_str())
        .json(&serde_json::json!({
            "message_id": "owner-msg-b",
            "content": "Please inspect deployment risk。请以后先给结论再给成本风险。我喜欢灰度发布方案。",
            "need_reply": true,
            "target_agent_id": "agent_c"
        }))
        .send()
        .await
        .expect("owner message response");
    assert_eq!(owner_message.status(), reqwest::StatusCode::OK);
    let owner_message_body: Value = owner_message.json().await.expect("owner message json");
    assert_eq!(
        owner_message_body["queued_to_agent_id"].as_str(),
        Some("agent_b")
    );
    assert_eq!(
        owner_message_body["message"]["message_id"].as_str(),
        Some("owner-msg-b")
    );
    assert_eq!(
        owner_message_body["message"]["from"].as_str(),
        Some("user_b")
    );
    assert_eq!(
        owner_message_body["message"]["to"].as_str(),
        Some("agent_b")
    );
    assert_eq!(
        owner_message_body["message"]["content"].as_str(),
        Some("Please inspect deployment risk。请以后先给结论再给成本风险。我喜欢灰度发布方案。")
    );
    let learned_profile_facts = owner_message_body["learned_profile_facts"]
        .as_array()
        .expect("learned profile facts array");
    assert!(learned_profile_facts.iter().any(|fact| {
        fact["category"].as_str() == Some("communication_style")
            && fact["value"]
                .as_str()
                .is_some_and(|value| value.contains("先给结论再给成本风险"))
    }));
    assert!(learned_profile_facts.iter().any(|fact| {
        fact["category"].as_str() == Some("like")
            && fact["value"]
                .as_str()
                .is_some_and(|value| value.contains("灰度发布方案"))
    }));
    assert!(
        !owner_profile_prompt_path.exists(),
        "owner message should learn HumanProfile facts without syncing the prompt file"
    );
    assert_eq!(
        std::fs::read_to_string(&office_memory_owner_profile_path)
            .expect("office memory owner profile remains template"),
        "# Owner Profile\n\n"
    );
    let sync_owner_profile = client
        .post(format!("{base_url}/api/owner-profile/sync"))
        .header(COOKIE, cookie.as_str())
        .header(ACCEPT, "application/json")
        .send()
        .await
        .expect("manual owner profile sync response");
    assert_eq!(sync_owner_profile.status(), reqwest::StatusCode::OK);
    let sync_owner_profile_body: Value = sync_owner_profile
        .json()
        .await
        .expect("manual owner profile sync json");
    assert_eq!(
        sync_owner_profile_body["synced_agent_id"].as_str(),
        Some("agent_b")
    );
    assert_eq!(
        sync_owner_profile_body["owner_user_id"].as_str(),
        Some("user_b")
    );
    assert_eq!(
        sync_owner_profile_body["owner_profile_path"].as_str(),
        Some(owner_profile_prompt_path.display().to_string().as_str())
    );
    let synced_owner_profile_prompt = std::fs::read_to_string(&owner_profile_prompt_path)
        .expect("manual owner profile prompt is synced");
    assert!(synced_owner_profile_prompt.contains("role: Infra"));
    assert!(synced_owner_profile_prompt.contains("先给结论再给成本风险"));
    assert!(synced_owner_profile_prompt.contains("灰度发布方案"));
    let synced_office_memory_owner_profile =
        std::fs::read_to_string(&office_memory_owner_profile_path)
            .expect("manual office memory owner profile is synced");
    assert_eq!(
        synced_office_memory_owner_profile,
        synced_owner_profile_prompt
    );
    assert_eq!(
        owner_message_body["message"]["need_reply"].as_bool(),
        Some(true)
    );
    assert_eq!(
        owner_message_body["me"]["agent_inbox"]["queued_count"].as_u64(),
        Some(1)
    );
    assert_eq!(
        owner_message_body["me"]["agent_activity"]["agent_id"].as_str(),
        Some("agent_b")
    );
    let owner_activity_entries = owner_message_body["me"]["agent_activity"]
        .get("entries")
        .and_then(Value::as_array)
        .expect("owner activity entries array");
    assert_eq!(owner_activity_entries.len(), 1);
    assert!(owner_activity_entries.iter().all(|entry| {
        entry["agent_id"].as_str() == Some("agent_b")
            && entry["kind"].as_str() == Some("owner_message_queued")
            && entry["message_id"].as_str() == Some("owner-msg-b")
            && entry["source"].as_str() == Some("owner_inbox")
            && entry["title"].as_str() == Some("收到主人请求")
            && entry_text_contains(entry, "Please inspect deployment risk")
            && !entry_text_contains(entry, "created")
            && !entry_text_contains(entry, "planning")
            && !entry_text_contains(entry, "executing")
            && !entry_text_contains(entry, "waiting_for_human")
    }));
    assert!(owner_activity_entries.iter().any(|entry| {
        entry["summary"]
            .as_str()
            .is_some_and(|summary| summary.contains("owner-msg-b"))
            && entry["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains("Please inspect deployment risk"))
    }));
    let owner_dashboard = client
        .get(format!("{base_url}/me"))
        .header(COOKIE, cookie.as_str())
        .send()
        .await
        .expect("owner dashboard response");
    assert_eq!(owner_dashboard.status(), reqwest::StatusCode::OK);
    let owner_dashboard_html = owner_dashboard.text().await.expect("owner dashboard html");
    assert!(owner_dashboard_html.contains("Agent 正在做什么 / 活动流"));
    assert!(owner_dashboard_html.contains(r#"name="need_reply" value="true""#));
    assert!(owner_dashboard_html.contains(r#"fetch("/api/me""#));
    assert!(owner_dashboard_html.contains(r#"fetch("/api/inbox""#));
    assert!(owner_dashboard_html.contains("reply_to_message_id: replyToMessageId"));
    assert!(owner_dashboard_html.contains("正在编辑回复，暂停刷新 Human Inbox"));
    assert!(owner_dashboard_html.contains("setInterval(refresh, 1500)"));
    assert!(owner_dashboard_html.contains("Please inspect deployment risk"));
    assert!(!owner_dashboard_html.contains("Please summarize user launch feedback"));
    assert!(!owner_dashboard_html.contains("created"));
    assert!(!owner_dashboard_html.contains("planning"));
    assert!(!owner_dashboard_html.contains("executing"));
    assert!(!owner_dashboard_html.contains("waiting_for_human"));
    assert_eq!(
        owner_message_body["me"]["agent_inbox"]["messages"][0]["to"].as_str(),
        Some("agent_b")
    );
    assert_eq!(
        owner_message_body["me"]["pending_owner_replies"][0]["message_id"].as_str(),
        Some("owner-msg-b")
    );

    let other_login = client
        .post(format!("{base_url}/api/login"))
        .json(&serde_json::json!({ "username": "employee_c", "password": "password" }))
        .send()
        .await
        .expect("other login response");
    assert_eq!(other_login.status(), reqwest::StatusCode::OK);
    let other_cookie = login_cookie(&other_login);
    let other_me = client
        .get(format!("{base_url}/api/me"))
        .header(ACCEPT, "application/json")
        .header(COOKIE, other_cookie.as_str())
        .send()
        .await
        .expect("other me response");
    assert_eq!(other_me.status(), reqwest::StatusCode::OK);
    let other_me_body: Value = other_me.json().await.expect("other me json");
    assert_eq!(other_me_body["user_id"].as_str(), Some("user_c"));
    assert_eq!(other_me_body["agent_id"].as_str(), Some("agent_c"));
    assert_eq!(
        other_me_body["agent_inbox"]["queued_count"].as_u64(),
        Some(0)
    );
    assert_eq!(
        other_me_body["agent_activity"]["agent_id"].as_str(),
        Some("agent_c")
    );
    assert!(
        json_array(
            &other_me_body["agent_activity"]["entries"],
            "other activity entries"
        )
        .is_empty()
    );
    assert!(
        other_me_body["agent_inbox"]["messages"]
            .as_array()
            .expect("other inbox messages array")
            .is_empty()
    );

    let other_owner_message = client
        .post(format!("{base_url}/api/inbox"))
        .header(COOKIE, other_cookie.as_str())
        .json(&serde_json::json!({
            "message_id": "owner-msg-c",
            "content": "Please summarize user launch feedback",
            "need_reply": true,
            "target_agent_id": "agent_b"
        }))
        .send()
        .await
        .expect("other owner message response");
    assert_eq!(other_owner_message.status(), reqwest::StatusCode::OK);
    let other_owner_message_body: Value = other_owner_message
        .json()
        .await
        .expect("other owner message json");
    assert_eq!(
        other_owner_message_body["queued_to_agent_id"].as_str(),
        Some("agent_c")
    );
    assert_eq!(
        other_owner_message_body["message"]["from"].as_str(),
        Some("user_c")
    );
    assert_eq!(
        other_owner_message_body["message"]["to"].as_str(),
        Some("agent_c")
    );
    assert_eq!(
        other_owner_message_body["me"]["agent_inbox"]["queued_count"].as_u64(),
        Some(1)
    );
    assert_eq!(
        other_owner_message_body["me"]["agent_activity"]["agent_id"].as_str(),
        Some("agent_c")
    );
    let other_activity_entries = other_owner_message_body["me"]["agent_activity"]
        .get("entries")
        .and_then(Value::as_array)
        .expect("other activity entries array");
    assert_eq!(other_activity_entries.len(), 1);
    assert!(other_activity_entries.iter().all(|entry| {
        entry["agent_id"].as_str() == Some("agent_c")
            && entry["kind"].as_str() == Some("owner_message_queued")
            && entry["message_id"].as_str() == Some("owner-msg-c")
            && entry["source"].as_str() == Some("owner_inbox")
            && entry["title"].as_str() == Some("收到主人请求")
            && entry_text_contains(entry, "Please summarize user launch feedback")
            && !entry_text_contains(entry, "created")
            && !entry_text_contains(entry, "planning")
            && !entry_text_contains(entry, "executing")
            && !entry_text_contains(entry, "waiting_for_human")
    }));
    let other_dashboard = client
        .get(format!("{base_url}/me"))
        .header(COOKIE, other_cookie.as_str())
        .send()
        .await
        .expect("other dashboard response");
    assert_eq!(other_dashboard.status(), reqwest::StatusCode::OK);
    let other_dashboard_html = other_dashboard.text().await.expect("other dashboard html");
    assert!(other_dashboard_html.contains("Agent 正在做什么 / 活动流"));
    assert!(other_dashboard_html.contains(r#"name="need_reply" value="true""#));
    assert!(other_dashboard_html.contains("Please summarize user launch feedback"));
    assert!(!other_dashboard_html.contains("Please inspect deployment risk"));
    assert!(!other_dashboard_html.contains("created"));
    assert!(!other_dashboard_html.contains("planning"));
    assert!(!other_dashboard_html.contains("executing"));
    assert!(!other_dashboard_html.contains("waiting_for_human"));
    assert_eq!(
        other_owner_message_body["me"]["agent_inbox"]["messages"][0]["message_id"].as_str(),
        Some("owner-msg-c")
    );

    let me_after_other_message = client
        .get(format!("{base_url}/api/me"))
        .header(ACCEPT, "application/json")
        .header(COOKIE, cookie.as_str())
        .send()
        .await
        .expect("me after other owner message response");
    assert_eq!(me_after_other_message.status(), reqwest::StatusCode::OK);
    let me_after_other_message_body: Value = me_after_other_message
        .json()
        .await
        .expect("me after other owner message json");
    assert_eq!(
        me_after_other_message_body["agent_inbox"]["agent_id"].as_str(),
        Some("agent_b")
    );
    assert_eq!(
        me_after_other_message_body["agent_inbox"]["queued_count"].as_u64(),
        Some(1)
    );
    assert_eq!(
        me_after_other_message_body["agent_activity"]["agent_id"].as_str(),
        Some("agent_b")
    );
    let me_after_other_activity_entries = me_after_other_message_body["agent_activity"]
        .get("entries")
        .and_then(Value::as_array)
        .expect("me after other activity entries array");
    assert_eq!(me_after_other_activity_entries.len(), 1);
    assert!(me_after_other_activity_entries.iter().all(|entry| {
        entry["agent_id"].as_str() == Some("agent_b")
            && entry["kind"].as_str() == Some("owner_message_queued")
            && entry["message_id"].as_str() == Some("owner-msg-b")
            && entry["source"].as_str() == Some("owner_inbox")
            && entry_text_contains(entry, "Please inspect deployment risk")
            && !entry_text_contains(entry, "created")
            && !entry_text_contains(entry, "planning")
            && !entry_text_contains(entry, "executing")
            && !entry_text_contains(entry, "waiting_for_human")
    }));
    assert!(me_after_other_activity_entries.iter().all(|entry| {
        entry["message_id"].as_str() != Some("owner-msg-c")
            && !entry_text_contains(entry, "Please summarize user launch feedback")
    }));
    assert_eq!(
        me_after_other_message_body["agent_inbox"]["messages"][0]["message_id"].as_str(),
        Some("owner-msg-b")
    );
    assert_ne!(
        me_after_other_message_body["agent_inbox"]["messages"][0]["message_id"].as_str(),
        Some("owner-msg-c")
    );

    let runtime_store_path = store_path.with_file_name("office-runtime.json");
    let mut runtime_store =
        OfficeRuntimeStore::open(runtime_store_path).expect("runtime store opens");
    runtime_store
        .mark_owner_reply_ready(
            "agent_b",
            "user_b",
            "owner-msg-b",
            "reply_to: owner-msg-b\nDeployment risk reviewed.",
        )
        .expect("owner reply is recorded");
    runtime_store
        .mark_owner_reply_ready(
            "agent_b",
            "user_b",
            "owner-msg-b",
            "reply_to: owner-msg-b\nDeployment risk reviewed.",
        )
        .expect("duplicate owner reply is ignored");

    let me_after_runtime_reply = client
        .get(format!("{base_url}/api/me"))
        .header(ACCEPT, "application/json")
        .header(COOKIE, cookie.as_str())
        .send()
        .await
        .expect("me after runtime owner reply response");
    assert_eq!(me_after_runtime_reply.status(), reqwest::StatusCode::OK);
    let me_after_runtime_reply_body: Value = me_after_runtime_reply
        .json()
        .await
        .expect("me after runtime owner reply json");
    assert_eq!(
        me_after_runtime_reply_body["agent_state"]["state"].as_str(),
        Some("idle")
    );
    assert!(
        json_array(
            &me_after_runtime_reply_body["pending_owner_replies"],
            "pending owner replies"
        )
        .is_empty()
    );
    let replied_activity_entries = me_after_runtime_reply_body["agent_activity"]["entries"]
        .as_array()
        .expect("replied activity entries array");
    let owner_reply_entries = replied_activity_entries
        .iter()
        .filter(|entry| {
            entry["kind"].as_str() == Some("owner_reply_ready")
                && entry["message_id"].as_str() == Some("owner-msg-b")
        })
        .collect::<Vec<_>>();
    assert_eq!(owner_reply_entries.len(), 1);
    assert!(
        owner_reply_entries[0]["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("Deployment risk reviewed."))
    );

    let interview = client
        .post(format!("{base_url}/api/interview"))
        .header(COOKIE, cookie.as_str())
        .json(&serde_json::json!({
            "role": "Infra / 平台",
            "capabilities": ["成本治理", "发布自动化"],
            "avoid": ["无背景设计争论"],
            "report_preference": "先风险再方案"
        }))
        .send()
        .await
        .expect("interview response");
    assert_eq!(interview.status(), reqwest::StatusCode::OK);
    let interview_body: Value = interview.json().await.expect("interview json");
    assert_eq!(
        interview_body["human_profile"]["role"].as_str(),
        Some("Infra / 平台")
    );
    assert!(json_array_contains(
        &interview_body["human_profile"]["capability_labels"],
        "成本治理"
    ));
    assert!(json_array_contains(
        &interview_body["human_profile"]["do_not_disturb"],
        "无背景设计争论"
    ));

    let reflection = client
        .post(format!("{base_url}/api/reflection"))
        .header(COOKIE, cookie.as_str())
        .json(&serde_json::json!({ "profile_updates": ["SLO 复盘", "无背景设计争论"] }))
        .send()
        .await
        .expect("reflection response");
    assert_eq!(reflection.status(), reqwest::StatusCode::OK);
    let reflection_body: Value = reflection.json().await.expect("reflection json");
    assert!(json_array_contains(
        &reflection_body["human_profile"]["capability_labels"],
        "SLO 复盘"
    ));
    assert!(
        reflection_body["reflection"]["reflected_at"]
            .as_str()
            .is_some()
    );
    let owner_profile_before_manual_resync = std::fs::read_to_string(&owner_profile_prompt_path)
        .expect("owner profile prompt before manual resync");
    assert!(!owner_profile_before_manual_resync.contains("Infra / 平台"));
    assert!(!owner_profile_before_manual_resync.contains("SLO 复盘"));

    let resync_owner_profile = client
        .post(format!("{base_url}/api/owner-profile/sync"))
        .header(COOKIE, cookie.as_str())
        .header(ACCEPT, "application/json")
        .send()
        .await
        .expect("manual owner profile resync response");
    assert_eq!(resync_owner_profile.status(), reqwest::StatusCode::OK);
    let owner_profile_after_manual_resync = std::fs::read_to_string(&owner_profile_prompt_path)
        .expect("owner profile prompt after manual resync");
    assert!(owner_profile_after_manual_resync.contains("role: Infra / 平台"));
    assert!(owner_profile_after_manual_resync.contains("SLO 复盘"));
    assert!(owner_profile_after_manual_resync.contains("无背景设计争论"));

    let persisted = PersistentPilotDirectory::open(store_path.clone()).expect("reopen store");
    assert!(persisted.web_session(&token).is_some());
    let persisted_profile = persisted
        .human_profile("user_b")
        .expect("persisted profile");
    assert!(
        persisted_profile
            .capability_labels
            .contains(&"SLO 复盘".to_string())
    );
    assert!(
        persisted_profile
            .do_not_disturb
            .contains(&"无背景设计争论".to_string())
    );

    let logout = client
        .post(format!("{base_url}/api/logout"))
        .header(ACCEPT, "application/json")
        .header(COOKIE, cookie.as_str())
        .send()
        .await
        .expect("logout response");
    assert_eq!(logout.status(), reqwest::StatusCode::OK);

    let after_logout = client
        .get(format!("{base_url}/api/me"))
        .header(ACCEPT, "application/json")
        .header(COOKIE, cookie.as_str())
        .send()
        .await
        .expect("me after logout");
    assert_eq!(after_logout.status(), reqwest::StatusCode::UNAUTHORIZED);
    let persisted_after_logout = PersistentPilotDirectory::open(store_path).expect("reopen store");
    assert!(persisted_after_logout.web_session(&token).is_none());

    handle.abort();
}

#[tokio::test]
async fn office_web_rejects_session_with_mismatched_owner_agent_binding() {
    let _office_web_lock = office_web_test_lock().lock().await;
    let temp = tempdir().expect("tempdir");
    let store_path = temp.path().join("office-web-corrupt-store.json");
    let mut snapshot = PilotStoreSnapshot::seeded();
    snapshot.web_sessions.insert(
        "corrupt-session".to_string(),
        PilotWebSession {
            token: "corrupt-session".to_string(),
            user_id: "user_b".to_string(),
            agent_id: "agent_c".to_string(),
            created_at: Utc::now(),
        },
    );
    std::fs::write(
        &store_path,
        serde_json::to_string_pretty(&snapshot).expect("snapshot serializes"),
    )
    .expect("corrupt snapshot is written");

    let app = OfficeWebApp::open(OfficeWebConfig::new(store_path)).expect("office web app opens");
    let (base_url, handle) = spawn_office_web(app).await;
    let client = reqwest::Client::new();

    let response = client
        .get(format!("{base_url}/api/me"))
        .header(ACCEPT, "application/json")
        .header(COOKIE, "codex_office_session=corrupt-session")
        .send()
        .await
        .expect("corrupt me response");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::INTERNAL_SERVER_ERROR
    );
    let body: Value = response.json().await.expect("error json");
    assert_eq!(body["error"].as_str(), Some("profile not found"));

    handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn office_inbox_delivery_creates_runtime_thread_and_wakes_agent() {
    let _office_web_lock = office_web_test_lock().lock().await;
    let temp = tempdir().expect("tempdir");
    let server = start_mock_server().await;
    let _turn = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("office-runtime-resp"),
            ev_reasoning_item("reasoning-1", &["Inspecting runtime delivery"], &[]),
            ev_function_call(
                "msg-1",
                "call",
                &serde_json::json!({
                    "target_id": "user_b",
                    "message_id": "owner-runtime-wakeup-reply",
                    "reply_to_message_id": "owner-runtime-wakeup",
                    "content": "Runtime delivery is working."
                })
                .to_string(),
            ),
            ev_completed("office-runtime-resp"),
        ]),
    )
    .await;
    let _turn_wrapup = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("office-runtime-wrapup-resp"),
            ev_completed("office-runtime-wrapup-resp"),
        ]),
    )
    .await;

    let mut agent_config = load_default_config_for_test(&temp).await;
    agent_config.cwd = temp.path().join("workspace");
    std::fs::create_dir_all(&agent_config.cwd).expect("workspace dir");
    agent_config.model = Some("office-test-model".to_string());
    agent_config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
    agent_config.agent_max_threads = Some(6);

    let thread_manager = Arc::new(
        codex_core::test_support::thread_manager_with_models_provider_and_home(
            CodexAuth::from_api_key("dummy"),
            built_in_model_providers()["openai"].clone(),
            agent_config.codex_home.clone(),
        ),
    );
    assert!(thread_manager.list_thread_ids().await.is_empty());

    let store_path = temp.path().join("office-runtime-bridge-store.json");
    let app = OfficeWebApp::open_with_thread_manager(
        OfficeWebConfig::new(store_path),
        Arc::clone(&thread_manager),
        agent_config,
    )
    .expect("office web app opens");
    let (base_url, handle) = spawn_office_web(app).await;
    let client = reqwest::Client::new();

    let login = client
        .post(format!("{base_url}/api/login"))
        .json(&serde_json::json!({ "username": "employee_b", "password": "password" }))
        .send()
        .await
        .expect("login response");
    assert_eq!(login.status(), reqwest::StatusCode::OK);
    let cookie = login_cookie(&login);

    let owner_message = client
        .post(format!("{base_url}/api/inbox"))
        .header(COOKIE, cookie.as_str())
        .json(&serde_json::json!({
            "message_id": "owner-runtime-wakeup",
            "content": "Please wake and inspect runtime delivery",
            "need_reply": true
        }))
        .send()
        .await
        .expect("owner message response");
    assert_eq!(owner_message.status(), reqwest::StatusCode::OK);
    let body: Value = owner_message.json().await.expect("owner message json");
    assert_eq!(body["queued_to_agent_id"].as_str(), Some("agent_b"));
    assert_eq!(body["me"]["agent_state"]["state"].as_str(), Some("working"));
    assert_eq!(body["me"]["agent_inbox"]["queued_count"].as_u64(), Some(1));

    let thread_ids = thread_manager.list_thread_ids().await;
    assert_eq!(thread_ids.len(), 1);
    let runtime_thread_id = thread_ids[0];

    let captured = codex_core::test_support::captured_thread_manager_ops(&thread_manager);
    let sent_owner_message = captured.iter().any(|(thread_id, op)| {
        *thread_id == runtime_thread_id
            && matches!(
                op,
                Op::UserInput {
                    items,
                    origin: codex_core::protocol::UserInputOrigin::AgentCall,
                    final_output_json_schema: None,
                } if matches!(
                    items.as_slice(),
                    [UserInput::Text { text, text_elements }]
                        if text.contains("message_id[owner-runtime-wakeup]")
                            && text.contains("Please wake and inspect runtime delivery")
                            && text_elements.is_empty()
                )
            )
    });
    assert!(
        sent_owner_message,
        "owner inbox message should be delivered through AgentControl::send_input"
    );

    let mut me_body = None;
    for _ in 0..20 {
        let me = client
            .get(format!("{base_url}/api/me"))
            .header(ACCEPT, "application/json")
            .header(COOKIE, cookie.as_str())
            .send()
            .await
            .expect("me response");
        let status = me.status();
        let body_text = me.text().await.expect("me body text");
        assert_eq!(status, reqwest::StatusCode::OK, "{body_text}");
        let body: Value = serde_json::from_str(&body_text).expect("me json");
        if body["agent_activity"]["entries"]
            .as_array()
            .expect("activity entries array")
            .iter()
            .any(|entry| {
                entry["kind"].as_str() == Some("owner_reply_ready")
                    && entry["message_id"].as_str() == Some("owner-runtime-wakeup")
            })
        {
            me_body = Some(body);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let me_body = me_body.expect("runtime activity should be visible through /api/me polling");
    let activity_entries = me_body["agent_activity"]["entries"]
        .as_array()
        .expect("activity entries array");
    assert!(activity_entries.iter().any(|entry| {
        entry["kind"].as_str() == Some("owner_message_queued")
            && entry["message_id"].as_str() == Some("owner-runtime-wakeup")
    }));
    assert!(activity_entries.iter().any(|entry| {
        entry["kind"].as_str() == Some("runtime_thread_resumed")
            && entry["summary"]
                .as_str()
                .is_some_and(|summary| summary.contains(&runtime_thread_id.to_string()))
    }));
    assert!(activity_entries.iter().any(|entry| {
        entry["kind"].as_str() == Some("runtime_reasoning")
            && entry["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains("Inspecting runtime delivery"))
    }));
    assert!(activity_entries.iter().any(|entry| {
        entry["kind"].as_str() == Some("owner_reply_ready")
            && entry["message_id"].as_str() == Some("owner-runtime-wakeup")
            && entry["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains("Runtime delivery is working."))
    }));
    assert!(activity_entries.iter().any(|entry| {
        entry["kind"].as_str() == Some("owner_reply_ready")
            && entry["message_id"].as_str() == Some("owner-runtime-wakeup")
            && entry["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains("Runtime delivery is working."))
    }));

    handle.abort();
    thread_manager
        .remove_and_close_all_threads()
        .await
        .expect("shutdown runtime threads");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn office_ceo_delegates_to_multiple_employee_agents_and_synthesizes_replies() {
    let _office_web_lock = office_web_test_lock().lock().await;
    let temp = tempdir().expect("tempdir");
    let server = start_mock_server().await;

    mount_sse_once_match(
        &server,
        request_contains_all(&[
            "message_id[owner-ceo-launch-risk]",
            "明天内部全面上线",
            "agent_a",
            "agent_b",
            "agent_c",
        ]),
        sse(vec![
            ev_response_created("ceo-delegate-resp"),
            ev_function_call(
                "ceo-call-a",
                "call",
                &serde_json::json!({
                    "target_agent_name": "agent_a",
                    "message_id": "ceo-a-launch-risk",
                    "need_reply": true,
                    "content": "请从算法角度评估明天内部全面上线风险。输出必须包含风险、验证办法、是否阻塞上线。"
                })
                .to_string(),
            ),
            ev_function_call(
                "ceo-call-b",
                "call",
                &serde_json::json!({
                    "target_agent_name": "agent_b",
                    "message_id": "ceo-b-launch-risk",
                    "need_reply": true,
                    "content": "请从基础设施角度评估明天内部全面上线风险。输出必须包含风险、验证办法、是否阻塞上线。"
                })
                .to_string(),
            ),
            ev_function_call(
                "ceo-call-c",
                "call",
                &serde_json::json!({
                    "target_agent_name": "agent_c",
                    "message_id": "ceo-c-launch-risk",
                    "need_reply": true,
                    "content": "请从产品流程角度评估明天内部全面上线风险。输出必须包含风险、验证办法、是否阻塞上线。"
                })
                .to_string(),
            ),
            ev_function_call(
                "ceo-wait-a",
                "wait",
                &serde_json::json!({
                    "target_id": "agent_a",
                    "timeout_ms": 5000
                })
                .to_string(),
            ),
            ev_function_call(
                "ceo-wait-b",
                "wait",
                &serde_json::json!({
                    "target_id": "agent_b",
                    "timeout_ms": 5000
                })
                .to_string(),
            ),
            ev_function_call(
                "ceo-wait-c",
                "wait",
                &serde_json::json!({
                    "target_id": "agent_c",
                    "timeout_ms": 5000
                })
                .to_string(),
            ),
            ev_completed("ceo-delegate-resp"),
        ]),
    )
    .await;

    let employee_a_mock = mount_sse_once_match(
        &server,
        request_contains_all(&["ceo-a-launch-risk", "算法角度", "target_agent_name"]),
        sse(vec![
            ev_response_created("agent-a-resp"),
            ev_function_call(
                "agent-a-reply",
                "call",
                &serde_json::json!({
                    "target_agent_name": "agent_ceo",
                    "message_id": "a-ceo-launch-risk-reply",
                    "reply_to_message_id": "ceo-a-launch-risk",
                    "content": "算法结论：非阻塞。风险是评估样本不足，建议灰度并监控异常率。"
                })
                .to_string(),
            ),
            ev_completed("agent-a-resp"),
        ]),
    )
    .await;
    let employee_b_mock = mount_sse_once_match(
        &server,
        request_contains_all(&["ceo-b-launch-risk", "基础设施角度", "target_agent_name"]),
        sse(vec![
            ev_response_created("agent-b-resp"),
            ev_function_call(
                "agent-b-reply",
                "call",
                &serde_json::json!({
                    "target_agent_name": "agent_ceo",
                    "message_id": "b-ceo-launch-risk-reply",
                    "reply_to_message_id": "ceo-b-launch-risk",
                    "content": "Infra 结论：有条件放行。需要先确认所有员工默认访问权限和回滚脚本。"
                })
                .to_string(),
            ),
            ev_completed("agent-b-resp"),
        ]),
    )
    .await;
    let employee_c_mock = mount_sse_once_match(
        &server,
        request_contains_all(&["ceo-c-launch-risk", "产品流程角度", "target_agent_name"]),
        sse(vec![
            ev_response_created("agent-c-resp"),
            ev_function_call(
                "agent-c-reply",
                "call",
                &serde_json::json!({
                    "target_agent_name": "agent_ceo",
                    "message_id": "c-ceo-launch-risk-reply",
                    "reply_to_message_id": "ceo-c-launch-risk",
                    "content": "产品结论：有条件放行。首次入口文案和失败态需要补齐。"
                })
                .to_string(),
            ),
            ev_completed("agent-c-resp"),
        ]),
    )
    .await;
    let ceo_final_mock = mount_sse_once_match(
        &server,
        request_contains_all(&[
            "ceo-wait-a",
            "ceo-wait-b",
            "ceo-wait-c",
            "function_call_output",
            "算法结论：非阻塞",
            "Infra 结论：有条件放行",
            "产品结论：有条件放行",
        ]),
        sse(vec![
            ev_response_created("ceo-final-resp"),
            ev_function_call(
                "ceo-final-msg",
                "call",
                &serde_json::json!({
                    "target_id": "user_ceo",
                    "message_id": "owner-ceo-launch-risk-reply",
                    "reply_to_message_id": "owner-ceo-launch-risk",
                    "content": "可以有条件上线：算法非阻塞，Infra 需确认默认访问和回滚脚本，产品需补齐入口文案和失败态。"
                })
                .to_string(),
            ),
            ev_completed("ceo-final-resp"),
        ]),
    )
    .await;
    let _ceo_final_wrapup = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("ceo-final-wrapup-resp"),
            ev_completed("ceo-final-wrapup-resp"),
        ]),
    )
    .await;

    let mut agent_config = load_default_config_for_test(&temp).await;
    agent_config.cwd = temp.path().join("workspace");
    std::fs::create_dir_all(&agent_config.cwd).expect("workspace dir");
    agent_config.model = Some("office-ceo-test-model".to_string());
    agent_config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
    agent_config.agent_max_threads = Some(8);

    let thread_manager = Arc::new(
        codex_core::test_support::thread_manager_with_models_provider_and_home(
            CodexAuth::from_api_key("dummy"),
            built_in_model_providers()["openai"].clone(),
            agent_config.codex_home.clone(),
        ),
    );
    let store_path = temp.path().join("office-ceo-delegation-store.json");
    let app = OfficeWebApp::open_with_thread_manager(
        OfficeWebConfig::new(store_path),
        Arc::clone(&thread_manager),
        agent_config,
    )
    .expect("office web app opens");
    let (base_url, handle) = spawn_office_web(app).await;
    let client = reqwest::Client::new();

    let login = client
        .post(format!("{base_url}/api/login"))
        .json(&serde_json::json!({ "username": "ceo", "password": "password" }))
        .send()
        .await
        .expect("login response");
    assert_eq!(login.status(), reqwest::StatusCode::OK);
    let cookie = login_cookie(&login);

    let owner_message = client
        .post(format!("{base_url}/api/inbox"))
        .header(COOKIE, cookie.as_str())
        .json(&serde_json::json!({
            "message_id": "owner-ceo-launch-risk",
            "content": "明天内部全面上线，请协调算法、Infra、产品三个 Agent 给出上线风险评估和是否阻塞。",
            "need_reply": true
        }))
        .send()
        .await
        .expect("owner message response");
    assert_eq!(owner_message.status(), reqwest::StatusCode::OK);

    let mut final_me_body = None;
    for _ in 0..80 {
        let me = client
            .get(format!("{base_url}/api/me"))
            .header(ACCEPT, "application/json")
            .header(COOKIE, cookie.as_str())
            .send()
            .await
            .expect("me response");
        assert_eq!(me.status(), reqwest::StatusCode::OK);
        let body: Value = me.json().await.expect("me json");
        if body["agent_activity"]["entries"]
            .as_array()
            .expect("activity entries array")
            .iter()
            .any(|entry| {
                entry["kind"].as_str() == Some("owner_reply_ready")
                    && entry["message_id"].as_str() == Some("owner-ceo-launch-risk")
                    && entry["detail"]
                        .as_str()
                        .is_some_and(|detail| detail.contains("可以有条件上线"))
            })
        {
            final_me_body = Some(body);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let final_me_body = final_me_body.expect("CEO final owner reply should become visible");
    assert_eq!(
        final_me_body["pending_owner_replies"]
            .as_array()
            .expect("pending owner replies array")
            .len(),
        0
    );

    let captured = codex_core::test_support::captured_thread_manager_ops(&thread_manager);
    let agent_a_thread_id = assert_sent_user_input(&captured, &["ceo-a-launch-risk", "算法角度"]);
    let agent_b_thread_id =
        assert_sent_user_input(&captured, &["ceo-b-launch-risk", "基础设施角度"]);
    let agent_c_thread_id =
        assert_sent_user_input(&captured, &["ceo-c-launch-risk", "产品流程角度"]);
    let ceo_thread_id = assert_sent_user_input(&captured, &["message_id[owner-ceo-launch-risk]"]);
    assert_sent_user_input(
        &captured,
        &["a-ceo-launch-risk-reply", "reply_to[ceo-a-launch-risk]"],
    );
    assert_sent_user_input(
        &captured,
        &["b-ceo-launch-risk-reply", "reply_to[ceo-b-launch-risk]"],
    );
    assert_sent_user_input(
        &captured,
        &["c-ceo-launch-risk-reply", "reply_to[ceo-c-launch-risk]"],
    );
    assert_ne!(agent_a_thread_id, ceo_thread_id);
    assert_ne!(agent_b_thread_id, ceo_thread_id);
    assert_ne!(agent_c_thread_id, ceo_thread_id);

    let final_requests = ceo_final_mock.requests();
    let wait_a_output = function_call_output_text_from_requests(&final_requests, "ceo-wait-a");
    let wait_b_output = function_call_output_text_from_requests(&final_requests, "ceo-wait-b");
    let wait_c_output = function_call_output_text_from_requests(&final_requests, "ceo-wait-c");
    assert_eq!(
        wait_a_output,
        "算法结论：非阻塞。风险是评估样本不足，建议灰度并监控异常率。"
    );
    assert_eq!(
        wait_b_output,
        "Infra 结论：有条件放行。需要先确认所有员工默认访问权限和回滚脚本。"
    );
    assert_eq!(
        wait_c_output,
        "产品结论：有条件放行。首次入口文案和失败态需要补齐。"
    );

    assert!(
        !employee_a_mock.requests().is_empty(),
        "agent_a should receive delegated work"
    );
    assert!(
        !employee_b_mock.requests().is_empty(),
        "agent_b should receive delegated work"
    );
    assert!(
        !employee_c_mock.requests().is_empty(),
        "agent_c should receive delegated work"
    );

    handle.abort();
    thread_manager
        .remove_and_close_all_threads()
        .await
        .expect("shutdown runtime threads");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn office_employee_owner_reply_continues_existing_agent_thread() {
    let _office_web_lock = office_web_test_lock().lock().await;
    let temp = tempdir().expect("tempdir");
    let server = start_mock_server().await;

    mount_sse_once_match(
        &server,
        request_contains_all(&["message_id[owner-agent-b-question]", "允许所有员工默认访问"]),
        sse(vec![
            ev_response_created("agent-b-question-resp"),
            ev_function_call(
                "agent-b-question-msg",
                "call",
                &serde_json::json!({
                    "target_id": "user_b",
                    "message_id": "owner-agent-b-question-human",
                    "reply_to_message_id": "owner-agent-b-question",
                    "need_reply": true,
                    "content": "我需要你确认默认访问权限策略。不要先回复最终结论。"
                })
                .to_string(),
            ),
            ev_completed("agent-b-question-resp"),
        ]),
    )
    .await;
    let _agent_b_question_wrapup = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("agent-b-question-wrapup-resp"),
            ev_completed("agent-b-question-wrapup-resp"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        request_contains_all(&[
            "message_id[owner-agent-b-answer]",
            "reply_to[owner-agent-b-question]",
            "允许所有员工默认访问",
        ]),
        sse(vec![
            ev_response_created("agent-b-answer-resp"),
            ev_function_call(
                "agent-b-answer-msg",
                "call",
                &serde_json::json!({
                    "target_id": "user_b",
                    "message_id": "owner-agent-b-answer-reply",
                    "reply_to_message_id": "owner-agent-b-question",
                    "content": "已根据主人确认继续：允许所有员工默认访问，建议保留审计日志和紧急回滚开关。"
                })
                .to_string(),
            ),
            ev_completed("agent-b-answer-resp"),
        ]),
    )
    .await;
    let _agent_b_answer_wrapup = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("agent-b-answer-wrapup-resp"),
            ev_completed("agent-b-answer-wrapup-resp"),
        ]),
    )
    .await;

    let mut agent_config = load_default_config_for_test(&temp).await;
    agent_config.cwd = temp.path().join("workspace");
    std::fs::create_dir_all(&agent_config.cwd).expect("workspace dir");
    agent_config.model = Some("office-owner-reply-test-model".to_string());
    agent_config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
    agent_config.agent_max_threads = Some(8);

    let thread_manager = Arc::new(
        codex_core::test_support::thread_manager_with_models_provider_and_home(
            CodexAuth::from_api_key("dummy"),
            built_in_model_providers()["openai"].clone(),
            agent_config.codex_home.clone(),
        ),
    );
    let store_path = temp.path().join("office-owner-reply-store.json");
    let runtime_store_path = store_path.with_file_name("office-runtime.json");
    let app = OfficeWebApp::open_with_thread_manager(
        OfficeWebConfig::new(store_path),
        Arc::clone(&thread_manager),
        agent_config,
    )
    .expect("office web app opens");
    let (base_url, handle) = spawn_office_web(app).await;
    let client = reqwest::Client::new();

    let login = client
        .post(format!("{base_url}/api/login"))
        .json(&serde_json::json!({ "username": "employee_b", "password": "password" }))
        .send()
        .await
        .expect("login response");
    assert_eq!(login.status(), reqwest::StatusCode::OK);
    let cookie = login_cookie(&login);

    let question = client
        .post(format!("{base_url}/api/inbox"))
        .header(COOKIE, cookie.as_str())
        .json(&serde_json::json!({
            "message_id": "owner-agent-b-question",
            "content": "请评估上线前是否可以允许所有员工默认访问。遇到策略选择时请向我确认。",
            "need_reply": true
        }))
        .send()
        .await
        .expect("owner question response");
    assert_eq!(question.status(), reqwest::StatusCode::OK);

    let agent_b_thread_id = wait_for_captured_input(
        &thread_manager,
        None,
        &["owner-agent-b-question", "评估上线前"],
    )
    .await;
    let thread_count_after_question = thread_manager.list_thread_ids().await.len();

    let mut saw_agent_human_question = false;
    for _ in 0..60 {
        let me = client
            .get(format!("{base_url}/api/me"))
            .header(ACCEPT, "application/json")
            .header(COOKIE, cookie.as_str())
            .send()
            .await
            .expect("me response");
        assert_eq!(me.status(), reqwest::StatusCode::OK);
        let body: Value = me.json().await.expect("me json");
        if body["human_inbox"]["messages"]
            .as_array()
            .expect("human inbox messages array")
            .iter()
            .any(|message| {
                message["message_id"].as_str() == Some("owner-agent-b-question-human")
                    && message["need_reply"].as_bool() == Some(true)
                    && message["reply_to_message_id"].as_str() == Some("owner-agent-b-question")
            })
        {
            saw_agent_human_question = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        saw_agent_human_question,
        "agent should ask the owner for the missing policy choice before owner follow-up"
    );

    let answer = client
        .post(format!("{base_url}/api/inbox"))
        .header(COOKIE, cookie.as_str())
        .json(&serde_json::json!({
            "message_id": "owner-agent-b-answer",
            "content": "允许所有员工默认访问，但必须保留审计日志和紧急回滚开关。",
            "need_reply": false,
            "reply_to_message_id": "owner-agent-b-question"
        }))
        .send()
        .await
        .expect("owner answer response");
    assert_eq!(answer.status(), reqwest::StatusCode::OK);
    let answer_body: Value = answer.json().await.expect("owner answer json");
    assert_eq!(answer_body["queued_to_agent_id"].as_str(), Some("agent_b"));
    assert_eq!(
        answer_body["message"]["reply_to_message_id"].as_str(),
        Some("owner-agent-b-question")
    );
    assert!(
        answer_body["pending_owner_replies"].is_null(),
        "pending owner replies is nested under me, not the top-level response"
    );
    assert!(
        answer_body["me"]["pending_owner_replies"]
            .as_array()
            .expect("pending owner replies array")
            .iter()
            .all(|message| message["message_id"].as_str() != Some("owner-agent-b-answer")),
        "non-reply owner follow-up must not be treated as a pending final owner request"
    );
    let runtime_store =
        OfficeRuntimeStore::open(runtime_store_path.clone()).expect("runtime store opens");
    assert!(runtime_store.has_owner_message_queued("agent_b", "owner-agent-b-question"));
    assert!(
        !runtime_store.has_owner_message_queued("agent_b", "owner-agent-b-answer"),
        "non-reply owner follow-up must not be recorded as a new owner task"
    );
    assert_eq!(
        runtime_store.latest_unanswered_owner_message_id("agent_b"),
        Some("owner-agent-b-question".to_string())
    );

    let follow_up_thread_id = wait_for_captured_input(
        &thread_manager,
        Some(agent_b_thread_id),
        &[
            "owner-agent-b-answer",
            "reply_to[owner-agent-b-question]",
            "允许所有员工默认访问",
        ],
    )
    .await;
    assert_eq!(follow_up_thread_id, agent_b_thread_id);
    let follow_up_prompt = captured_user_input_text(
        &codex_core::test_support::captured_thread_manager_ops(&thread_manager),
        follow_up_thread_id,
        &["message_id[owner-agent-b-answer]", "允许所有员工默认访问"],
    );
    assert!(follow_up_prompt.contains("## Office Turn Context"));
    assert!(follow_up_prompt.contains("owner_message_id: owner-agent-b-answer"));
    assert!(follow_up_prompt.contains("owner_reply_target_message_id: owner-agent-b-question"));
    assert!(
        !follow_up_prompt.contains("owner_reply_target_message_id: owner-agent-b-answer"),
        "owner follow-up prompt must not retarget final reply closure to the follow-up message"
    );
    assert_eq!(
        thread_manager.list_thread_ids().await.len(),
        thread_count_after_question,
        "owner follow-up reply should continue the existing agent_b runtime thread"
    );

    let mut final_me_body = None;
    for _ in 0..60 {
        let me = client
            .get(format!("{base_url}/api/me"))
            .header(ACCEPT, "application/json")
            .header(COOKIE, cookie.as_str())
            .send()
            .await
            .expect("me response");
        assert_eq!(me.status(), reqwest::StatusCode::OK);
        let body: Value = me.json().await.expect("me json");
        if body["agent_activity"]["entries"]
            .as_array()
            .expect("activity entries array")
            .iter()
            .any(|entry| {
                entry["kind"].as_str() == Some("owner_reply_ready")
                    && entry["message_id"].as_str() == Some("owner-agent-b-question")
                    && entry["detail"]
                        .as_str()
                        .is_some_and(|detail| detail.contains("允许所有员工默认访问"))
            })
        {
            final_me_body = Some(body);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let final_me_body =
        final_me_body.expect("employee agent final reply should become visible to owner");
    assert!(
        final_me_body["pending_owner_replies"]
            .as_array()
            .expect("pending owner replies array")
            .is_empty()
    );

    handle.abort();
    thread_manager
        .remove_and_close_all_threads()
        .await
        .expect("shutdown runtime threads");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn office_human_inbox_marks_agent_questions_replied_after_human_answer() {
    let _office_web_lock = office_web_test_lock().lock().await;
    let temp = tempdir().expect("tempdir");
    let server = start_mock_server().await;

    mount_sse_once_match(
        &server,
        request_contains_all(&["message_id[owner-human-status-question]", "请先问我"]),
        sse(vec![
            ev_response_created("agent-b-human-status-resp"),
            ev_function_call(
                "agent-b-human-status-call",
                "call",
                &serde_json::json!({
                    "target_id": "user_b",
                    "message_id": "human-status-check",
                    "reply_to_message_id": "owner-human-status-question",
                    "need_reply": true,
                    "content": "请确认这个上线判断是否可以按灰度口径推进。"
                })
                .to_string(),
            ),
            ev_completed("agent-b-human-status-resp"),
        ]),
    )
    .await;
    let _agent_b_question_wrapup = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("agent-b-human-status-wrapup-resp"),
            ev_completed("agent-b-human-status-wrapup-resp"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        request_contains_all(&[
            "message_id[human-status-answer]",
            "reply_to[human-status-check]",
            "同意灰度推进",
        ]),
        sse(vec![
            ev_response_created("agent-b-human-status-answer-resp"),
            ev_completed("agent-b-human-status-answer-resp"),
        ]),
    )
    .await;

    let mut agent_config = load_default_config_for_test(&temp).await;
    agent_config.cwd = temp.path().join("workspace");
    std::fs::create_dir_all(&agent_config.cwd).expect("workspace dir");
    agent_config.model = Some("office-human-inbox-status-test-model".to_string());
    agent_config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
    agent_config.agent_max_threads = Some(8);

    let thread_manager = Arc::new(
        codex_core::test_support::thread_manager_with_models_provider_and_home(
            CodexAuth::from_api_key("dummy"),
            built_in_model_providers()["openai"].clone(),
            agent_config.codex_home.clone(),
        ),
    );
    let app = OfficeWebApp::open_with_thread_manager(
        OfficeWebConfig::new(temp.path().join("office-human-inbox-status-store.json")),
        Arc::clone(&thread_manager),
        agent_config,
    )
    .expect("office web app opens");
    let (base_url, handle) = spawn_office_web(app).await;
    let client = reqwest::Client::new();

    let login = client
        .post(format!("{base_url}/api/login"))
        .json(&serde_json::json!({ "username": "employee_b", "password": "password" }))
        .send()
        .await
        .expect("login response");
    assert_eq!(login.status(), reqwest::StatusCode::OK);
    let cookie = login_cookie(&login);

    let owner_question = client
        .post(format!("{base_url}/api/inbox"))
        .header(COOKIE, cookie.as_str())
        .json(&serde_json::json!({
            "message_id": "owner-human-status-question",
            "content": "请先问我上线口径，再继续形成结论。",
            "need_reply": true
        }))
        .send()
        .await
        .expect("owner question response");
    assert_eq!(owner_question.status(), reqwest::StatusCode::OK);

    let agent_b_thread_id =
        wait_for_captured_input(&thread_manager, None, &["owner-human-status-question"]).await;

    let mut pending_me_body = None;
    for _ in 0..60 {
        let me = client
            .get(format!("{base_url}/api/me"))
            .header(ACCEPT, "application/json")
            .header(COOKIE, cookie.as_str())
            .send()
            .await
            .expect("me response");
        assert_eq!(me.status(), reqwest::StatusCode::OK);
        let body: Value = me.json().await.expect("me json");
        if body["human_inbox"]["messages"]
            .as_array()
            .expect("human inbox messages array")
            .iter()
            .any(|message| {
                message["message_id"].as_str() == Some("human-status-check")
                    && message["reply_status"].as_str() == Some("pending")
            })
        {
            pending_me_body = Some(body);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let pending_me_body = pending_me_body.expect("human inbox question should be pending");
    assert_eq!(
        pending_me_body["human_inbox"]["queued_count"].as_u64(),
        Some(1)
    );
    let pending_dashboard = client
        .get(format!("{base_url}/me"))
        .header(COOKIE, cookie.as_str())
        .send()
        .await
        .expect("pending dashboard response");
    assert_eq!(pending_dashboard.status(), reqwest::StatusCode::OK);
    let pending_dashboard_html = pending_dashboard
        .text()
        .await
        .expect("pending dashboard html");
    assert!(pending_dashboard_html.contains("human-status-check"));
    assert!(pending_dashboard_html.contains("请确认这个上线判断是否可以按灰度口径推进。"));
    assert!(pending_dashboard_html.contains(r#"class="human-reply-form""#));
    assert!(pending_dashboard_html.contains(r#"name="need_reply" value="false""#));
    assert!(
        pending_dashboard_html.contains(r#"name="reply_to_message_id" value="human-status-check""#)
    );
    assert!(pending_dashboard_html.contains("回复这条消息"));

    let answer = client
        .post(format!("{base_url}/api/inbox"))
        .header(COOKIE, cookie.as_str())
        .json(&serde_json::json!({
            "message_id": "human-status-answer",
            "content": "同意灰度推进，但先控制在内部试点用户。",
            "need_reply": false,
            "reply_to_message_id": "human-status-check"
        }))
        .send()
        .await
        .expect("human answer response");
    assert_eq!(answer.status(), reqwest::StatusCode::OK);
    let answer_body: Value = answer.json().await.expect("human answer json");
    assert_eq!(
        answer_body["message"]["reply_status"].as_str(),
        Some("not_required")
    );
    assert_eq!(
        answer_body["me"]["human_inbox"]["queued_count"].as_u64(),
        Some(0)
    );
    let replied_question = answer_body["me"]["human_inbox"]["messages"]
        .as_array()
        .expect("human inbox messages array")
        .iter()
        .find(|message| message["message_id"].as_str() == Some("human-status-check"))
        .expect("human question remains visible as history");
    assert_eq!(replied_question["need_reply"].as_bool(), Some(true));
    assert_eq!(replied_question["reply_status"].as_str(), Some("replied"));
    let replied_dashboard = client
        .get(format!("{base_url}/me"))
        .header(COOKIE, cookie.as_str())
        .send()
        .await
        .expect("replied dashboard response");
    assert_eq!(replied_dashboard.status(), reqwest::StatusCode::OK);
    let replied_dashboard_html = replied_dashboard
        .text()
        .await
        .expect("replied dashboard html");
    let question_index = replied_dashboard_html
        .find(r#"data-message-id="human-status-check""#)
        .expect("replied question remains visible");
    let after_question = &replied_dashboard_html[question_index..];
    let question_item = &after_question[..after_question
        .find("</li>")
        .expect("replied question list item closes")];
    assert!(question_item.contains("已回复"));
    assert!(!question_item.contains(r#"class="human-reply-form""#));

    wait_for_captured_input(
        &thread_manager,
        Some(agent_b_thread_id),
        &["human-status-answer", "reply_to[human-status-check]"],
    )
    .await;

    handle.abort();
    thread_manager
        .remove_and_close_all_threads()
        .await
        .expect("shutdown runtime threads");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn office_ceo_multi_agent_flow_can_request_multiple_human_owners() {
    let _office_web_lock = office_web_test_lock().lock().await;
    let temp = tempdir().expect("tempdir");
    let server = start_mock_server().await;

    mount_sse_once_match(
        &server,
        request_contains_all(&[
            "message_id[owner-ceo-autonomy-launch]",
            "agent_b",
            "agent_c",
        ]),
        sse(vec![
            ev_response_created("ceo-human-flow-delegate-resp"),
            ev_function_call(
                "ceo-call-b-human-flow",
                "call",
                &serde_json::json!({
                    "target_agent_name": "agent_b",
                    "message_id": "ceo-b-autonomy-launch",
                    "need_reply": true,
                    "content": "请从 Infra 角度评估自治多智能体研究评审上线风险。若缺少预算/权限判断，请自行向合适的人类 owner 求助。"
                })
                .to_string(),
            ),
            ev_function_call(
                "ceo-call-c-human-flow",
                "call",
                &serde_json::json!({
                    "target_agent_name": "agent_c",
                    "message_id": "ceo-c-autonomy-launch",
                    "need_reply": true,
                    "content": "请从产品角度评估自治多智能体研究评审上线风险。若缺少体验/优先级判断，请自行向合适的人类 owner 求助。"
                })
                .to_string(),
            ),
            ev_function_call(
                "ceo-wait-b-human-flow",
                "wait",
                &serde_json::json!({
                    "target_id": "agent_b",
                    "timeout_ms": 60000
                })
                .to_string(),
            ),
            ev_function_call(
                "ceo-wait-c-human-flow",
                "wait",
                &serde_json::json!({
                    "target_id": "agent_c",
                    "timeout_ms": 60000
                })
                .to_string(),
            ),
            ev_completed("ceo-human-flow-delegate-resp"),
        ]),
    )
    .await;
    let agent_b_first_mock = mount_sse_once_match(
        &server,
        request_contains_all(&["ceo-b-autonomy-launch", "Infra 角度"]),
        sse(vec![
            ev_response_created("agent-b-human-question-resp"),
            ev_function_call(
                "agent-b-call-human",
                "call",
                &serde_json::json!({
                    "target_id": "user_b",
                    "message_id": "b-human-budget-check",
                    "reply_to_message_id": "ceo-b-autonomy-launch",
                    "need_reply": true,
                    "content": "需要你作为 Infra owner 判断：本周上线是否允许临时增加 30% 推理预算并开启紧急回滚权限？默认方案是不扩大范围，只灰度给内部测试组。"
                })
                .to_string(),
            ),
            ev_function_call(
                "agent-b-wait-human",
                "wait",
                &serde_json::json!({
                    "target_id": "user_b",
                    "timeout_ms": 60000
                })
                .to_string(),
            ),
            ev_completed("agent-b-human-question-resp"),
        ]),
    )
    .await;
    let agent_c_first_mock = mount_sse_once_match(
        &server,
        request_contains_all(&["ceo-c-autonomy-launch", "产品角度"]),
        sse(vec![
            ev_response_created("agent-c-human-question-resp"),
            ev_function_call(
                "agent-c-call-human",
                "call",
                &serde_json::json!({
                    "target_id": "user_c",
                    "message_id": "c-human-priority-check",
                    "reply_to_message_id": "ceo-c-autonomy-launch",
                    "need_reply": true,
                    "content": "需要你作为产品 owner 判断：首轮上线优先验证任务自主分配，还是优先验证人类求助体验？默认方案是先验证人类求助体验。"
                })
                .to_string(),
            ),
            ev_function_call(
                "agent-c-wait-human",
                "wait",
                &serde_json::json!({
                    "target_id": "user_c",
                    "timeout_ms": 60000
                })
                .to_string(),
            ),
            ev_completed("agent-c-human-question-resp"),
        ]),
    )
    .await;
    let agent_b_after_human_mock = mount_sse_once_match(
        &server,
        request_contains_all(&[
            "agent-b-wait-human",
            "function_call_output",
            "批准 30% 预算",
        ]),
        sse(vec![
            ev_response_created("agent-b-human-answer-resp"),
            ev_function_call(
                "agent-b-reply-ceo",
                "call",
                &serde_json::json!({
                    "target_agent_name": "agent_ceo",
                    "message_id": "b-ceo-autonomy-launch-reply",
                    "reply_to_message_id": "ceo-b-autonomy-launch",
                    "content": "Infra 结论：有条件放行。user_b 已批准 30% 推理预算和紧急回滚权限；建议仅灰度给内部测试组并保留审计日志。"
                })
                .to_string(),
            ),
            ev_completed("agent-b-human-answer-resp"),
        ]),
    )
    .await;
    let agent_c_after_human_mock = mount_sse_once_match(
        &server,
        request_contains_all(&[
            "agent-c-wait-human",
            "function_call_output",
            "优先验证人类求助体验",
        ]),
        sse(vec![
            ev_response_created("agent-c-human-answer-resp"),
            ev_function_call(
                "agent-c-reply-ceo",
                "call",
                &serde_json::json!({
                    "target_agent_name": "agent_ceo",
                    "message_id": "c-ceo-autonomy-launch-reply",
                    "reply_to_message_id": "ceo-c-autonomy-launch",
                    "content": "产品结论：有条件放行。user_c 建议首轮优先验证人类求助体验，并把任务自主分配作为第二观察指标。"
                })
                .to_string(),
            ),
            ev_completed("agent-c-human-answer-resp"),
        ]),
    )
    .await;
    let ceo_final_mock = mount_sse_once_match(
        &server,
        request_contains_all(&[
            "ceo-wait-b-human-flow",
            "ceo-wait-c-human-flow",
            "function_call_output",
            "user_b 已批准",
            "user_c 建议",
        ]),
        sse(vec![
            ev_response_created("ceo-human-flow-final-resp"),
            ev_function_call(
                "ceo-final-human-flow",
                "call",
                &serde_json::json!({
                    "target_id": "user_ceo",
                    "message_id": "owner-ceo-autonomy-launch-reply",
                    "reply_to_message_id": "owner-ceo-autonomy-launch",
                    "content": "结论：可以有条件上线。Infra 人类 owner 已批准预算和回滚权限；产品人类 owner 建议首轮优先验证人类求助体验。建议灰度给内部测试组，记录 agent-agent 与 agent-human 协作轨迹。"
                })
                .to_string(),
            ),
            ev_completed("ceo-human-flow-final-resp"),
        ]),
    )
    .await;
    let _ceo_final_wrapup = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("ceo-human-flow-wrapup-resp"),
            ev_completed("ceo-human-flow-wrapup-resp"),
        ]),
    )
    .await;

    let mut agent_config = load_default_config_for_test(&temp).await;
    agent_config.cwd = temp.path().join("workspace");
    std::fs::create_dir_all(&agent_config.cwd).expect("workspace dir");
    agent_config.model = Some("office-human-flow-test-model".to_string());
    agent_config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
    agent_config.agent_max_threads = Some(8);

    let thread_manager = Arc::new(
        codex_core::test_support::thread_manager_with_models_provider_and_home(
            CodexAuth::from_api_key("dummy"),
            built_in_model_providers()["openai"].clone(),
            agent_config.codex_home.clone(),
        ),
    );
    let store_path = temp.path().join("office-human-flow-store.json");
    let app = OfficeWebApp::open_with_thread_manager(
        OfficeWebConfig::new(store_path),
        Arc::clone(&thread_manager),
        agent_config,
    )
    .expect("office web app opens");
    let (base_url, handle) = spawn_office_web(app).await;
    let client = reqwest::Client::new();

    let ceo_login = client
        .post(format!("{base_url}/api/login"))
        .json(&serde_json::json!({ "username": "ceo", "password": "password" }))
        .send()
        .await
        .expect("ceo login response");
    assert_eq!(ceo_login.status(), reqwest::StatusCode::OK);
    let ceo_cookie = login_cookie(&ceo_login);
    let user_b_login = client
        .post(format!("{base_url}/api/login"))
        .json(&serde_json::json!({ "username": "employee_b", "password": "password" }))
        .send()
        .await
        .expect("user_b login response");
    assert_eq!(user_b_login.status(), reqwest::StatusCode::OK);
    let user_b_cookie = login_cookie(&user_b_login);
    let user_c_login = client
        .post(format!("{base_url}/api/login"))
        .json(&serde_json::json!({ "username": "employee_c", "password": "password" }))
        .send()
        .await
        .expect("user_c login response");
    assert_eq!(user_c_login.status(), reqwest::StatusCode::OK);
    let user_c_cookie = login_cookie(&user_c_login);

    let owner_message = client
        .post(format!("{base_url}/api/inbox"))
        .header(COOKIE, ceo_cookie.as_str())
        .json(&serde_json::json!({
            "message_id": "owner-ceo-autonomy-launch",
            "content": "请自主协调团队评估自治多智能体研究评审是否可以本周灰度上线。不要让我指定分工；需要人类判断时自己向对应人类 owner 求助。",
            "need_reply": true
        }))
        .send()
        .await
        .expect("owner message response");
    assert_eq!(owner_message.status(), reqwest::StatusCode::OK);

    let agent_b_thread_id = wait_for_captured_input(
        &thread_manager,
        None,
        &["ceo-b-autonomy-launch", "Infra 角度"],
    )
    .await;
    let agent_c_thread_id = wait_for_captured_input(
        &thread_manager,
        None,
        &["ceo-c-autonomy-launch", "产品角度"],
    )
    .await;
    let ceo_thread_id =
        wait_for_captured_input(&thread_manager, None, &["owner-ceo-autonomy-launch"]).await;
    assert_ne!(agent_b_thread_id, ceo_thread_id);
    assert_ne!(agent_c_thread_id, ceo_thread_id);

    for _ in 0..60 {
        let user_b_me = client
            .get(format!("{base_url}/api/me"))
            .header(ACCEPT, "application/json")
            .header(COOKIE, user_b_cookie.as_str())
            .send()
            .await
            .expect("user_b me response");
        assert_eq!(user_b_me.status(), reqwest::StatusCode::OK);
        let user_b_body: Value = user_b_me.json().await.expect("user_b me json");
        let user_c_me = client
            .get(format!("{base_url}/api/me"))
            .header(ACCEPT, "application/json")
            .header(COOKIE, user_c_cookie.as_str())
            .send()
            .await
            .expect("user_c me response");
        assert_eq!(user_c_me.status(), reqwest::StatusCode::OK);
        let user_c_body: Value = user_c_me.json().await.expect("user_c me json");
        let saw_user_b_question = user_b_body["human_inbox"]["messages"]
            .as_array()
            .expect("user_b human inbox array")
            .iter()
            .any(|message| {
                message["message_id"].as_str() == Some("b-human-budget-check")
                    && message["need_reply"].as_bool() == Some(true)
            });
        let saw_user_c_question = user_c_body["human_inbox"]["messages"]
            .as_array()
            .expect("user_c human inbox array")
            .iter()
            .any(|message| {
                message["message_id"].as_str() == Some("c-human-priority-check")
                    && message["need_reply"].as_bool() == Some(true)
            });
        if saw_user_b_question && saw_user_c_question {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let user_b_answer = client
        .post(format!("{base_url}/api/inbox"))
        .header(COOKIE, user_b_cookie.as_str())
        .json(&serde_json::json!({
            "message_id": "user-b-budget-answer",
            "content": "批准 30% 预算和紧急回滚权限，但只能灰度给内部测试组，并必须保留审计日志。",
            "need_reply": false,
            "reply_to_message_id": "ceo-b-autonomy-launch"
        }))
        .send()
        .await
        .expect("user_b answer response");
    assert_eq!(user_b_answer.status(), reqwest::StatusCode::OK);
    let user_c_answer = client
        .post(format!("{base_url}/api/inbox"))
        .header(COOKIE, user_c_cookie.as_str())
        .json(&serde_json::json!({
            "message_id": "user-c-priority-answer",
            "content": "优先验证人类求助体验；任务自主分配作为第二观察指标。上线文案要明确这是灰度。",
            "need_reply": false,
            "reply_to_message_id": "ceo-c-autonomy-launch"
        }))
        .send()
        .await
        .expect("user_c answer response");
    assert_eq!(user_c_answer.status(), reqwest::StatusCode::OK);

    wait_for_captured_input(
        &thread_manager,
        Some(agent_b_thread_id),
        &["user-b-budget-answer", "批准 30% 预算"],
    )
    .await;
    wait_for_captured_input(
        &thread_manager,
        Some(agent_c_thread_id),
        &["user-c-priority-answer", "优先验证人类求助体验"],
    )
    .await;

    let mut final_me_body = None;
    for _ in 0..100 {
        let me = client
            .get(format!("{base_url}/api/me"))
            .header(ACCEPT, "application/json")
            .header(COOKIE, ceo_cookie.as_str())
            .send()
            .await
            .expect("ceo me response");
        assert_eq!(me.status(), reqwest::StatusCode::OK);
        let body: Value = me.json().await.expect("ceo me json");
        if body["agent_activity"]["entries"]
            .as_array()
            .expect("activity entries array")
            .iter()
            .any(|entry| {
                entry["kind"].as_str() == Some("owner_reply_ready")
                    && entry["message_id"].as_str() == Some("owner-ceo-autonomy-launch")
                    && entry["detail"]
                        .as_str()
                        .is_some_and(|detail| detail.contains("可以有条件上线"))
            })
        {
            final_me_body = Some(body);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let final_me_body = final_me_body.expect("CEO final human-assisted reply should be visible");
    assert!(
        final_me_body["pending_owner_replies"]
            .as_array()
            .expect("pending owner replies array")
            .is_empty()
    );

    assert!(!agent_b_first_mock.requests().is_empty());
    assert!(!agent_c_first_mock.requests().is_empty());
    assert!(!agent_b_after_human_mock.requests().is_empty());
    assert!(!agent_c_after_human_mock.requests().is_empty());
    assert!(!ceo_final_mock.requests().is_empty());

    handle.abort();
    thread_manager
        .remove_and_close_all_threads()
        .await
        .expect("shutdown runtime threads");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn office_ceo_agent_centered_workflow_embeds_human_team_collaboration() {
    let _office_web_lock = office_web_test_lock().lock().await;
    let temp = tempdir().expect("tempdir");
    let server = start_mock_server().await;

    mount_sse_once_match(
        &server,
        request_contains_all(&[
            "owner-ceo-human-centered-pilot",
            "企业客户试点",
            "agent_a",
            "agent_b",
            "agent_c",
            "agent_d",
            "agent_e",
        ]),
        sse(vec![
            ev_response_created("ceo-agent-centered-delegate-resp"),
            ev_function_call(
                "ceo-call-a-pilot",
                "call",
                &serde_json::json!({
                    "target_agent_name": "agent_a",
                    "message_id": "ceo-a-pilot-eval",
                    "need_reply": true,
                    "content": "请从算法角度判断企业客户试点的模型评估门槛和失败风险。需要研究判断或评估标准时，请向你的 owner 求助。"
                })
                .to_string(),
            ),
            ev_function_call(
                "ceo-call-b-pilot",
                "call",
                &serde_json::json!({
                    "target_agent_name": "agent_b",
                    "message_id": "ceo-b-pilot-infra",
                    "need_reply": true,
                    "content": "请从 Infra 角度判断两周企业客户试点的预算、SLA、回滚和观测要求。需要预算或权限判断时，请向你的 owner 求助。"
                })
                .to_string(),
            ),
            ev_function_call(
                "ceo-call-c-pilot",
                "call",
                &serde_json::json!({
                    "target_agent_name": "agent_c",
                    "message_id": "ceo-c-pilot-product",
                    "need_reply": true,
                    "content": "请从产品角度判断首批企业客户、试点边界和验收指标。需要优先级或客户场景判断时，请向你的 owner 求助。"
                })
                .to_string(),
            ),
            ev_function_call(
                "ceo-call-d-pilot",
                "call",
                &serde_json::json!({
                    "target_agent_name": "agent_d",
                    "message_id": "ceo-d-pilot-engineering",
                    "need_reply": true,
                    "content": "请从工程角度判断两周内可交付范围、代码冻结风险和应急修复机制。需要技术风险接受判断时，请向你的 owner 求助。"
                })
                .to_string(),
            ),
            ev_function_call(
                "ceo-call-e-pilot",
                "call",
                &serde_json::json!({
                    "target_agent_name": "agent_e",
                    "message_id": "ceo-e-pilot-launch",
                    "need_reply": true,
                    "content": "请从运营设计角度判断企业客户试点的沟通口径、演示 Demo 和对外风险表述。需要表达或审美判断时，请向你的 owner 求助。"
                })
                .to_string(),
            ),
            ev_function_call(
                "ceo-wait-a-pilot",
                "wait",
                &serde_json::json!({ "target_id": "agent_a", "timeout_ms": 60000 }).to_string(),
            ),
            ev_function_call(
                "ceo-wait-b-pilot",
                "wait",
                &serde_json::json!({ "target_id": "agent_b", "timeout_ms": 60000 }).to_string(),
            ),
            ev_function_call(
                "ceo-wait-c-pilot",
                "wait",
                &serde_json::json!({ "target_id": "agent_c", "timeout_ms": 60000 }).to_string(),
            ),
            ev_function_call(
                "ceo-wait-d-pilot",
                "wait",
                &serde_json::json!({ "target_id": "agent_d", "timeout_ms": 60000 }).to_string(),
            ),
            ev_function_call(
                "ceo-wait-e-pilot",
                "wait",
                &serde_json::json!({ "target_id": "agent_e", "timeout_ms": 60000 }).to_string(),
            ),
            ev_completed("ceo-agent-centered-delegate-resp"),
        ]),
    )
    .await;

    let agent_a_first_mock = mount_sse_once_match(
        &server,
        request_contains_all(&["ceo-a-pilot-eval", "算法角度"]),
        sse(vec![
            ev_response_created("agent-a-human-question-resp"),
            ev_function_call(
                "agent-a-call-human",
                "call",
                &serde_json::json!({
                    "target_id": "user_a",
                    "message_id": "a-human-eval-standard",
                    "reply_to_message_id": "ceo-a-pilot-eval",
                    "need_reply": true,
                    "content": "需要你作为算法 owner 判断：企业客户试点的最低通过标准应设为 95% 任务完成率还是 90% 即可？默认方案是 95%，并保留人工复核。"
                })
                .to_string(),
            ),
            ev_function_call(
                "agent-a-wait-human",
                "wait",
                &serde_json::json!({ "target_id": "user_a", "timeout_ms": 60000 }).to_string(),
            ),
            ev_completed("agent-a-human-question-resp"),
        ]),
    )
    .await;
    let agent_b_first_mock = mount_sse_once_match(
        &server,
        request_contains_all(&["ceo-b-pilot-infra", "Infra 角度"]),
        sse(vec![
            ev_response_created("agent-b-human-question-resp"),
            ev_function_call(
                "agent-b-call-human",
                "call",
                &serde_json::json!({
                    "target_id": "user_b",
                    "message_id": "b-human-budget-sla",
                    "reply_to_message_id": "ceo-b-pilot-infra",
                    "need_reply": true,
                    "content": "需要你作为 Infra owner 判断：是否批准两周试点 40% 额外推理预算和 99.5% 内部 SLA？默认方案是不承诺 SLA，只做 best effort。"
                })
                .to_string(),
            ),
            ev_function_call(
                "agent-b-wait-human",
                "wait",
                &serde_json::json!({ "target_id": "user_b", "timeout_ms": 60000 }).to_string(),
            ),
            ev_completed("agent-b-human-question-resp"),
        ]),
    )
    .await;
    let agent_c_first_mock = mount_sse_once_match(
        &server,
        request_contains_all(&["ceo-c-pilot-product", "产品角度"]),
        sse(vec![
            ev_response_created("agent-c-human-question-resp"),
            ev_function_call(
                "agent-c-call-human",
                "call",
                &serde_json::json!({
                    "target_id": "user_c",
                    "message_id": "c-human-segment-priority",
                    "reply_to_message_id": "ceo-c-pilot-product",
                    "need_reply": true,
                    "content": "需要你作为产品 owner 判断：首批试点应优先选择内部运营团队还是外部设计合作方？默认方案是内部运营团队。"
                })
                .to_string(),
            ),
            ev_function_call(
                "agent-c-wait-human",
                "wait",
                &serde_json::json!({ "target_id": "user_c", "timeout_ms": 60000 }).to_string(),
            ),
            ev_completed("agent-c-human-question-resp"),
        ]),
    )
    .await;
    let agent_d_first_mock = mount_sse_once_match(
        &server,
        request_contains_all(&["ceo-d-pilot-engineering", "工程角度"]),
        sse(vec![
            ev_response_created("agent-d-human-question-resp"),
            ev_function_call(
                "agent-d-call-human",
                "call",
                &serde_json::json!({
                    "target_id": "user_d",
                    "message_id": "d-human-code-freeze-risk",
                    "reply_to_message_id": "ceo-d-pilot-engineering",
                    "need_reply": true,
                    "content": "需要你作为工程 owner 判断：是否允许试点期间绕过常规冻结窗口做紧急修复？默认方案是只允许配置级回滚，不允许代码热修。"
                })
                .to_string(),
            ),
            ev_function_call(
                "agent-d-wait-human",
                "wait",
                &serde_json::json!({ "target_id": "user_d", "timeout_ms": 60000 }).to_string(),
            ),
            ev_completed("agent-d-human-question-resp"),
        ]),
    )
    .await;
    let agent_e_first_mock = mount_sse_once_match(
        &server,
        request_contains_all(&["ceo-e-pilot-launch", "运营设计角度"]),
        sse(vec![
            ev_response_created("agent-e-human-question-resp"),
            ev_function_call(
                "agent-e-call-human",
                "call",
                &serde_json::json!({
                    "target_id": "user_e",
                    "message_id": "e-human-launch-narrative",
                    "reply_to_message_id": "ceo-e-pilot-launch",
                    "need_reply": true,
                    "content": "需要你作为运营设计 owner 判断：Demo 应强调效率提升还是新型人机协作范式？默认方案是强调协作范式，并明确灰度范围。"
                })
                .to_string(),
            ),
            ev_function_call(
                "agent-e-wait-human",
                "wait",
                &serde_json::json!({ "target_id": "user_e", "timeout_ms": 60000 }).to_string(),
            ),
            ev_completed("agent-e-human-question-resp"),
        ]),
    )
    .await;

    let agent_a_after_human_mock = mount_sse_once_match(
        &server,
        request_contains_all(&["agent-a-wait-human", "function_call_output", "95%"]),
        sse(vec![
            ev_response_created("agent-a-human-answer-resp"),
            ev_function_call(
                "agent-a-reply-ceo",
                "call",
                &serde_json::json!({
                    "target_agent_name": "agent_ceo",
                    "message_id": "a-ceo-pilot-reply",
                    "reply_to_message_id": "ceo-a-pilot-eval",
                    "content": "算法结论：建议设 95% 任务完成率、关键案例人工复核、失败样本进入次日复盘。user_a 已确认标准不应降到 90%。"
                })
                .to_string(),
            ),
            ev_completed("agent-a-human-answer-resp"),
        ]),
    )
    .await;
    let agent_b_after_human_mock = mount_sse_once_match(
        &server,
        request_contains_all(&["agent-b-wait-human", "function_call_output", "40%"]),
        sse(vec![
            ev_response_created("agent-b-human-answer-resp"),
            ev_function_call(
                "agent-b-reply-ceo",
                "call",
                &serde_json::json!({
                    "target_agent_name": "agent_ceo",
                    "message_id": "b-ceo-pilot-reply",
                    "reply_to_message_id": "ceo-b-pilot-infra",
                    "content": "Infra 结论：有条件放行。user_b 批准 40% 额外推理预算，但 SLA 只对内部试点承诺 99.5%，必须保留回滚开关和审计日志。"
                })
                .to_string(),
            ),
            ev_completed("agent-b-human-answer-resp"),
        ]),
    )
    .await;
    let agent_c_after_human_mock = mount_sse_once_match(
        &server,
        request_contains_all(&["agent-c-wait-human", "function_call_output", "内部运营团队"]),
        sse(vec![
            ev_response_created("agent-c-human-answer-resp"),
            ev_function_call(
                "agent-c-reply-ceo",
                "call",
                &serde_json::json!({
                    "target_agent_name": "agent_ceo",
                    "message_id": "c-ceo-pilot-reply",
                    "reply_to_message_id": "ceo-c-pilot-product",
                    "content": "产品结论：首批选择内部运营团队。user_c 要求验收指标包括任务完成率、人工求助质量和跨 agent 分派清晰度。"
                })
                .to_string(),
            ),
            ev_completed("agent-c-human-answer-resp"),
        ]),
    )
    .await;
    let agent_d_after_human_mock = mount_sse_once_match(
        &server,
        request_contains_all(&["agent-d-wait-human", "function_call_output", "配置级回滚"]),
        sse(vec![
            ev_response_created("agent-d-human-answer-resp"),
            ev_function_call(
                "agent-d-reply-ceo",
                "call",
                &serde_json::json!({
                    "target_agent_name": "agent_ceo",
                    "message_id": "d-ceo-pilot-reply",
                    "reply_to_message_id": "ceo-d-pilot-engineering",
                    "content": "工程结论：可做两周试点，但 user_d 不允许绕过冻结窗口做代码热修；只允许配置级回滚和预置修复开关。"
                })
                .to_string(),
            ),
            ev_completed("agent-d-human-answer-resp"),
        ]),
    )
    .await;
    let agent_e_after_human_mock = mount_sse_once_match(
        &server,
        request_contains_all(&["agent-e-wait-human", "function_call_output", "协作范式"]),
        sse(vec![
            ev_response_created("agent-e-human-answer-resp"),
            ev_function_call(
                "agent-e-reply-ceo",
                "call",
                &serde_json::json!({
                    "target_agent_name": "agent_ceo",
                    "message_id": "e-ceo-pilot-reply",
                    "reply_to_message_id": "ceo-e-pilot-launch",
                    "content": "运营设计结论：Demo 应强调 Agent 为中心的人类协作范式。user_e 要求文案明确灰度、可回滚、不是替代人类判断。"
                })
                .to_string(),
            ),
            ev_completed("agent-e-human-answer-resp"),
        ]),
    )
    .await;

    let ceo_final_mock = mount_sse_once_match(
        &server,
        request_contains_all(&[
            "ceo-wait-a-pilot",
            "ceo-wait-b-pilot",
            "ceo-wait-c-pilot",
            "ceo-wait-d-pilot",
            "ceo-wait-e-pilot",
            "算法结论",
            "Infra 结论",
            "产品结论",
            "工程结论",
            "运营设计结论",
        ]),
        sse(vec![
            ev_response_created("ceo-agent-centered-final-resp"),
            ev_function_call(
                "ceo-final-pilot",
                "call",
                &serde_json::json!({
                    "target_id": "user_ceo",
                    "message_id": "owner-ceo-human-centered-pilot-reply",
                    "reply_to_message_id": "owner-ceo-human-centered-pilot",
                    "content": "结论：建议启动两周 Agent 中心协作企业客户试点。CEO 只需要和 agent_ceo 对齐目标；agent_ceo 已协调算法、Infra、产品、工程、运营设计五个 agent，并且五个 agent 都在关键不确定点向各自 human owner 求助。建议范围：内部运营团队灰度；95% 任务完成率；40% 额外预算；内部 99.5% SLA；只允许配置级回滚；Demo 强调人类与 Agent 同构协作。"
                })
                .to_string(),
            ),
            ev_completed("ceo-agent-centered-final-resp"),
        ]),
    )
    .await;
    let _ceo_final_wrapup = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("ceo-agent-centered-wrapup-resp"),
            ev_completed("ceo-agent-centered-wrapup-resp"),
        ]),
    )
    .await;

    let mut agent_config = load_default_config_for_test(&temp).await;
    agent_config.cwd = temp.path().join("workspace");
    std::fs::create_dir_all(&agent_config.cwd).expect("workspace dir");
    agent_config.model = Some("office-agent-centered-test-model".to_string());
    agent_config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
    agent_config.agent_max_threads = Some(8);

    let thread_manager = Arc::new(
        codex_core::test_support::thread_manager_with_models_provider_and_home(
            CodexAuth::from_api_key("dummy"),
            built_in_model_providers()["openai"].clone(),
            agent_config.codex_home.clone(),
        ),
    );
    let store_path = temp.path().join("office-agent-centered-store.json");
    let app = OfficeWebApp::open_with_thread_manager(
        OfficeWebConfig::new(store_path),
        Arc::clone(&thread_manager),
        agent_config,
    )
    .expect("office web app opens");
    let (base_url, handle) = spawn_office_web(app).await;
    let client = reqwest::Client::new();

    let mut cookies = std::collections::HashMap::new();
    for username in [
        "ceo",
        "employee_a",
        "employee_b",
        "employee_c",
        "employee_d",
        "employee_e",
    ] {
        let login = client
            .post(format!("{base_url}/api/login"))
            .json(&serde_json::json!({ "username": username, "password": "password" }))
            .send()
            .await
            .expect("login response");
        assert_eq!(login.status(), reqwest::StatusCode::OK);
        cookies.insert(username, login_cookie(&login));
    }

    let owner_message = client
        .post(format!("{base_url}/api/inbox"))
        .header(COOKIE, cookies["ceo"].as_str())
        .json(&serde_json::json!({
            "message_id": "owner-ceo-human-centered-pilot",
            "content": "我想在两周内做一个企业客户试点，验证 Agent 中心的人类团队协作方式。不要让我指定分工，你自己判断该找谁、怎么对齐、什么时候找人类 owner。",
            "need_reply": true
        }))
        .send()
        .await
        .expect("owner message response");
    assert_eq!(owner_message.status(), reqwest::StatusCode::OK);

    let ceo_thread_id =
        wait_for_captured_input(&thread_manager, None, &["owner-ceo-human-centered-pilot"]).await;
    let agent_a_thread_id =
        wait_for_captured_input(&thread_manager, None, &["ceo-a-pilot-eval", "算法角度"]).await;
    let agent_b_thread_id =
        wait_for_captured_input(&thread_manager, None, &["ceo-b-pilot-infra", "Infra 角度"]).await;
    let agent_c_thread_id =
        wait_for_captured_input(&thread_manager, None, &["ceo-c-pilot-product", "产品角度"]).await;
    let agent_d_thread_id = wait_for_captured_input(
        &thread_manager,
        None,
        &["ceo-d-pilot-engineering", "工程角度"],
    )
    .await;
    let agent_e_thread_id = wait_for_captured_input(
        &thread_manager,
        None,
        &["ceo-e-pilot-launch", "运营设计角度"],
    )
    .await;
    for employee_thread_id in [
        agent_a_thread_id,
        agent_b_thread_id,
        agent_c_thread_id,
        agent_d_thread_id,
        agent_e_thread_id,
    ] {
        assert_ne!(employee_thread_id, ceo_thread_id);
    }
    assert_eq!(
        thread_manager.list_thread_ids().await.len(),
        6,
        "CEO owner message should use the fixed six-agent roster without ad-hoc spawned agents"
    );

    let human_questions = [
        ("employee_a", "a-human-eval-standard"),
        ("employee_b", "b-human-budget-sla"),
        ("employee_c", "c-human-segment-priority"),
        ("employee_d", "d-human-code-freeze-risk"),
        ("employee_e", "e-human-launch-narrative"),
    ];
    let mut saw_all_human_questions = false;
    for _ in 0..80 {
        let mut seen = 0usize;
        for (username, message_id) in human_questions {
            let me = client
                .get(format!("{base_url}/api/me"))
                .header(ACCEPT, "application/json")
                .header(COOKIE, cookies[username].as_str())
                .send()
                .await
                .expect("employee me response");
            assert_eq!(me.status(), reqwest::StatusCode::OK);
            let body: Value = me.json().await.expect("employee me json");
            if body["human_inbox"]["messages"]
                .as_array()
                .expect("human inbox array")
                .iter()
                .any(|message| {
                    message["message_id"].as_str() == Some(message_id)
                        && message["need_reply"].as_bool() == Some(true)
                })
            {
                seen += 1;
            }
        }
        if seen == human_questions.len() {
            saw_all_human_questions = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(saw_all_human_questions);

    for (username, message_id, content, reply_to_message_id) in [
        (
            "employee_a",
            "user-a-eval-answer",
            "坚持 95% 任务完成率，关键失败案例必须人工复核，不能为了演示降到 90%。",
            "ceo-a-pilot-eval",
        ),
        (
            "employee_b",
            "user-b-budget-answer",
            "批准 40% 额外推理预算；内部试点可以承诺 99.5% SLA，但必须保留回滚开关和审计日志。",
            "ceo-b-pilot-infra",
        ),
        (
            "employee_c",
            "user-c-priority-answer",
            "首批选择内部运营团队，验收指标要包含任务完成率、人工求助质量和跨 agent 分派清晰度。",
            "ceo-c-pilot-product",
        ),
        (
            "employee_d",
            "user-d-risk-answer",
            "不允许绕过冻结窗口做代码热修；只允许配置级回滚和预置修复开关。",
            "ceo-d-pilot-engineering",
        ),
        (
            "employee_e",
            "user-e-narrative-answer",
            "Demo 应强调 Agent 为中心的人类协作范式，同时明确灰度、可回滚、不是替代人类判断。",
            "ceo-e-pilot-launch",
        ),
    ] {
        let answer = client
            .post(format!("{base_url}/api/inbox"))
            .header(COOKIE, cookies[username].as_str())
            .json(&serde_json::json!({
                "message_id": message_id,
                "content": content,
                "need_reply": false,
                "reply_to_message_id": reply_to_message_id
            }))
            .send()
            .await
            .expect("human answer response");
        assert_eq!(answer.status(), reqwest::StatusCode::OK);
    }

    wait_for_captured_input(
        &thread_manager,
        Some(agent_a_thread_id),
        &["user-a-eval-answer", "95% 任务完成率"],
    )
    .await;
    wait_for_captured_input(
        &thread_manager,
        Some(agent_b_thread_id),
        &["user-b-budget-answer", "40% 额外推理预算"],
    )
    .await;
    wait_for_captured_input(
        &thread_manager,
        Some(agent_c_thread_id),
        &["user-c-priority-answer", "内部运营团队"],
    )
    .await;
    wait_for_captured_input(
        &thread_manager,
        Some(agent_d_thread_id),
        &["user-d-risk-answer", "配置级回滚"],
    )
    .await;
    wait_for_captured_input(
        &thread_manager,
        Some(agent_e_thread_id),
        &["user-e-narrative-answer", "协作范式"],
    )
    .await;

    let mut final_me_body = None;
    for _ in 0..120 {
        let me = client
            .get(format!("{base_url}/api/me"))
            .header(ACCEPT, "application/json")
            .header(COOKIE, cookies["ceo"].as_str())
            .send()
            .await
            .expect("ceo me response");
        assert_eq!(me.status(), reqwest::StatusCode::OK);
        let body: Value = me.json().await.expect("ceo me json");
        if body["agent_activity"]["entries"]
            .as_array()
            .expect("activity entries array")
            .iter()
            .any(|entry| {
                entry["kind"].as_str() == Some("owner_reply_ready")
                    && entry["message_id"].as_str() == Some("owner-ceo-human-centered-pilot")
                    && entry["detail"]
                        .as_str()
                        .is_some_and(|detail| detail.contains("Agent 中心协作企业客户试点"))
            })
        {
            final_me_body = Some(body);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let final_me_body =
        final_me_body.expect("CEO final agent-centered workflow reply should be visible");
    assert!(
        final_me_body["pending_owner_replies"]
            .as_array()
            .expect("pending owner replies array")
            .is_empty()
    );

    assert!(!agent_a_first_mock.requests().is_empty());
    assert!(!agent_b_first_mock.requests().is_empty());
    assert!(!agent_c_first_mock.requests().is_empty());
    assert!(!agent_d_first_mock.requests().is_empty());
    assert!(!agent_e_first_mock.requests().is_empty());
    assert!(!agent_a_after_human_mock.requests().is_empty());
    assert!(!agent_b_after_human_mock.requests().is_empty());
    assert!(!agent_c_after_human_mock.requests().is_empty());
    assert!(!agent_d_after_human_mock.requests().is_empty());
    assert!(!agent_e_after_human_mock.requests().is_empty());
    assert!(!ceo_final_mock.requests().is_empty());

    handle.abort();
    thread_manager
        .remove_and_close_all_threads()
        .await
        .expect("shutdown runtime threads");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn office_human_reply_with_reply_to_message_id_becomes_visible_to_wait() {
    let _office_web_lock = office_web_test_lock().lock().await;
    let temp = tempdir().expect("tempdir");
    let server = start_mock_server().await;
    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("office-human-wait-resp"),
            ev_function_call(
                "msg-1",
                "wait",
                &serde_json::json!({ "target_id": "user_b", "timeout_ms": 60000 }).to_string(),
            ),
            ev_completed("office-human-wait-resp"),
        ]),
    )
    .await;
    let wait_output_mock = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            let body = std::str::from_utf8(&req.body).unwrap_or("");
            body.contains("msg-1") && body.contains("function_call_output")
        },
        sse(vec![
            ev_response_created("office-human-wait-output-resp"),
            ev_completed("office-human-wait-output-resp"),
        ]),
    )
    .await;

    let mut agent_config = load_default_config_for_test(&temp).await;
    agent_config.cwd = temp.path().join("workspace");
    std::fs::create_dir_all(&agent_config.cwd).expect("workspace dir");
    agent_config.model = Some("office-test-model".to_string());
    agent_config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
    agent_config.agent_max_threads = Some(6);

    let thread_manager = Arc::new(
        codex_core::test_support::thread_manager_with_models_provider_and_home(
            CodexAuth::from_api_key("dummy"),
            built_in_model_providers()["openai"].clone(),
            agent_config.codex_home.clone(),
        ),
    );
    let store_path = temp.path().join("office-human-wait-store.json");
    let app = OfficeWebApp::open_with_thread_manager(
        OfficeWebConfig::new(store_path),
        Arc::clone(&thread_manager),
        agent_config,
    )
    .expect("office web app opens");
    let (base_url, handle) = spawn_office_web(app).await;
    let client = reqwest::Client::new();

    let login = client
        .post(format!("{base_url}/api/login"))
        .json(&serde_json::json!({ "username": "employee_b", "password": "password" }))
        .send()
        .await
        .expect("login response");
    assert_eq!(login.status(), reqwest::StatusCode::OK);
    let cookie = login_cookie(&login);

    let owner_message = client
        .post(format!("{base_url}/api/inbox"))
        .header(COOKIE, cookie.as_str())
        .json(&serde_json::json!({
            "message_id": "owner-human-wait",
            "content": "请先等待我的回复。",
            "need_reply": true
        }))
        .send()
        .await
        .expect("owner message response");
    assert_eq!(owner_message.status(), reqwest::StatusCode::OK);

    let agent_thread_id = wait_for_captured_input(
        &thread_manager,
        None,
        &["owner-human-wait", "请先等待我的回复"],
    )
    .await;

    let human_reply = client
        .post(format!("{base_url}/api/inbox"))
        .header(COOKIE, cookie.as_str())
        .json(&serde_json::json!({
            "message_id": "human-wait-reply",
            "content": "已回复。",
            "need_reply": false,
            "reply_to_message_id": "owner-human-wait"
        }))
        .send()
        .await
        .expect("human reply response");
    assert_eq!(human_reply.status(), reqwest::StatusCode::OK);

    let received_thread_id = wait_for_captured_input(
        &thread_manager,
        Some(agent_thread_id),
        &["human-wait-reply", "reply_to[owner-human-wait]"],
    )
    .await;
    assert_eq!(received_thread_id, agent_thread_id);

    let mut wait_output = None;
    for _ in 0..40 {
        wait_output = wait_output_mock
            .requests()
            .iter()
            .find_map(|req| req.function_call_output_content_and_success("msg-1"));
        if wait_output.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let wait_output = wait_output.unwrap_or_else(|| {
        let requests = wait_output_mock.requests();
        let bodies = requests
            .iter()
            .map(|req| req.body_json())
            .collect::<Vec<_>>();
        panic!(
            "wait output should be captured, got {} requests: {}",
            bodies.len(),
            serde_json::to_string_pretty(&bodies).expect("request bodies serialize")
        );
    });
    assert_eq!(wait_output.0.as_deref(), Some("已回复。"));

    let me = client
        .get(format!("{base_url}/api/me"))
        .header(ACCEPT, "application/json")
        .header(COOKIE, cookie.as_str())
        .send()
        .await
        .expect("me response");
    assert_eq!(me.status(), reqwest::StatusCode::OK);
    let body: Value = me.json().await.expect("me json");
    assert!(
        body["agent_inbox"]["messages"]
            .as_array()
            .expect("agent inbox messages array")
            .iter()
            .any(|message| {
                message["message_id"].as_str() == Some("human-wait-reply")
                    && message["reply_to_message_id"].as_str() == Some("owner-human-wait")
            }),
        "agent inbox should contain the human reply"
    );

    handle.abort();
    thread_manager
        .remove_and_close_all_threads()
        .await
        .expect("shutdown runtime threads");
}

#[test]
fn office_and_docker_gateway_startup_entries_are_documented_and_wired() {
    let office_bin = std::fs::read_to_string(manifest_path("src/bin/codex-office-server.rs"))
        .expect("office server bin is readable");
    assert!(office_bin.contains("AI_OFFICE_DB"));
    assert!(office_bin.contains("AI_OFFICE_HOST"));
    assert!(office_bin.contains("AI_OFFICE_PORT"));
    assert!(office_bin.contains("ThreadManager::new"));
    assert!(office_bin.contains("OfficeWebApp::open_with_thread_manager"));
    assert!(office_bin.contains("app.serve(addr)"));

    let docker_bin = std::fs::read_to_string(manifest_path("src/bin/codex-docker-gateway.rs"))
        .expect("docker gateway bin is readable");
    assert!(docker_bin.contains("CODEX_DOCKER_GATEWAY_IMAGE"));
    assert!(docker_bin.contains("CODEX_DOCKER_GATEWAY_WORKSPACE_ROOT"));
    assert!(docker_bin.contains("CODEX_DOCKER_GATEWAY_BEARER_TOKEN"));
    assert!(docker_bin.contains("CODEX_DOCKER_GATEWAY_COMPANY_ID"));
    assert!(docker_bin.contains("CODEX_DOCKER_GATEWAY_CONTAINER_PREFIX"));
    assert!(docker_bin.contains("DockerGatewayBackend::with_config"));
    assert!(docker_bin.contains("with_default_company_id"));
    assert!(docker_bin.contains("with_container_name_prefix"));
    assert!(docker_bin.contains("GatewayHttpServer::new"));
    assert!(docker_bin.contains("server.serve(addr)"));

    let readme =
        std::fs::read_to_string(manifest_path("README.md")).expect("core README is readable");
    assert!(readme.contains("codex-office-server"));
    assert!(readme.contains("codex-docker-gateway"));
    assert!(readme.contains("CODEX_TOOL_GATEWAY_URL"));
    assert!(readme.contains("CODEX_TOOL_GATEWAY_COMPANY_ID"));
    assert!(readme.contains("CODEX_TOOL_GATEWAY_PROJECT_ID"));
    assert!(readme.contains("CODEX_DOCKER_GATEWAY_CONTAINER_PREFIX"));
    assert!(readme.contains("/tools/dispatch"));
    assert!(readme.contains("one persistent container"));
    assert!(readme.contains("docker exec"));
    assert!(readme.contains("/workspace/public"));
    assert!(readme.contains("/workspace/agents/<agent_id>/private"));
    assert!(readme.contains("/workspace/projects/<project_id>"));
}

#[test]
fn office_owner_reply_state_survives_activity_trimming() {
    let temp = tempdir().expect("tempdir");
    let runtime_store_path = temp.path().join("office-runtime.json");
    let mut runtime_store =
        OfficeRuntimeStore::open(runtime_store_path.clone()).expect("runtime store opens");

    runtime_store
        .mark_owner_message_queued("agent_ceo", "user_ceo", "owner-msg", "Need review")
        .expect("owner message is queued");
    runtime_store
        .mark_owner_reply_ready(
            "agent_ceo",
            "user_ceo",
            "owner-msg",
            "reply_to: owner-msg\nReviewed.",
        )
        .expect("owner reply is recorded");

    for i in 0..100 {
        runtime_store
            .record_runtime_event(
                "agent_ceo",
                "user_ceo",
                codex_core::office::OfficeActivityKind::RuntimeAgentMessage,
                "test",
                "progress",
                format!("progress {i}"),
                None,
                None,
                None,
            )
            .expect("runtime activity recorded");
    }

    let runtime_store =
        OfficeRuntimeStore::open(runtime_store_path).expect("runtime store reloads");
    assert!(runtime_store.has_owner_message_queued("agent_ceo", "owner-msg"));
    assert!(runtime_store.has_owner_reply_ready("agent_ceo", "owner-msg"));
    assert_eq!(
        runtime_store.latest_unanswered_owner_message_id("agent_ceo"),
        None
    );
    assert!(
        !runtime_store
            .activity_for_agent("agent_ceo")
            .iter()
            .any(|entry| entry.message_id.as_deref() == Some("owner-msg"))
    );
}

#[test]
fn office_late_runtime_events_do_not_reopen_completed_owner_work() {
    let temp = tempdir().expect("tempdir");
    let runtime_store_path = temp.path().join("office-runtime.json");
    let mut runtime_store =
        OfficeRuntimeStore::open(runtime_store_path).expect("runtime store opens");

    runtime_store
        .mark_owner_message_queued("agent_ceo", "user_ceo", "owner-msg", "Need review")
        .expect("owner message is queued");
    runtime_store
        .mark_owner_reply_ready(
            "agent_ceo",
            "user_ceo",
            "owner-msg",
            "reply_to: owner-msg\nReviewed.",
        )
        .expect("owner reply is recorded");
    runtime_store
        .record_runtime_event(
            "agent_ceo",
            "user_ceo",
            codex_core::office::OfficeActivityKind::RuntimeAgentMessage,
            "assistant_message",
            "Agent 输出",
            "Duplicate late reply ignored.",
            Some("Duplicate late reply ignored.".to_string()),
            None,
            None,
        )
        .expect("late runtime event recorded");

    let record = runtime_store.get("agent_ceo").expect("agent record exists");
    assert_eq!(record.state, codex_core::office::AgentRuntimeState::Idle);
    assert!(record.waiting_reason.is_none());
    assert_eq!(
        runtime_store.latest_unanswered_owner_message_id("agent_ceo"),
        None
    );
}

#[test]
fn office_prompt_constants_are_embedded_in_swarm_root_templates() {
    let swarm_main =
        std::fs::read_to_string(manifest_path("templates/collaboration_mode/swarm_main.md"))
            .expect("swarm main template is readable");
    let swarm_complex = std::fs::read_to_string(manifest_path(
        "templates/collaboration_mode/swarm_main_complex.md",
    ))
    .expect("swarm complex template is readable");

    for template in [&swarm_main, &swarm_complex] {
        assert!(template.contains("Office Workspace Semantics"));
        assert!(template.contains("私有工作空间"));
        assert!(template.contains("公共空间"));
        assert!(template.contains("1+1 大于 2"));
        assert!(template.contains("每累计 15 个 Agent step"));
        assert!(template.contains("wait(target_id)"));
    }

    assert!(OFFICE_AGENT_SYSTEM_PROMPT_RULES.contains("主人上传的文件先进入私有空间"));
    assert!(OFFICE_SCHEDULER_SYSTEM_PROMPT.contains("每个 Agent 自己能感知"));
}

fn office_web_test_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

async fn spawn_office_web(app: OfficeWebApp) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test office server");
    let addr = listener.local_addr().expect("local addr");
    let handle = tokio::spawn(async move {
        serve(listener, app.into_router())
            .await
            .expect("office server should run");
    });
    (format!("http://{addr}"), handle)
}

fn login_cookie(response: &reqwest::Response) -> String {
    response
        .headers()
        .get(SET_COOKIE)
        .expect("set-cookie header")
        .to_str()
        .expect("set-cookie is ascii")
        .split(';')
        .next()
        .expect("cookie pair")
        .to_string()
}

fn json_array_contains(value: &Value, needle: &str) -> bool {
    value
        .as_array()
        .is_some_and(|items| items.iter().any(|item| item.as_str() == Some(needle)))
}

fn json_array<'a>(value: &'a Value, description: &str) -> &'a [Value] {
    value
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_else(|| panic!("{description} should be an array"))
}

fn assert_public_agent_state(value: &Value) {
    let state = value.as_str().expect("agent state should be a string");
    assert!(matches!(state, "idle" | "working" | "waiting"));
    assert_ne!(state, "blocked");
    assert!(!matches!(
        state,
        "created" | "planning" | "executing" | "waiting_for_human"
    ));
}

fn entry_text_contains(entry: &Value, needle: &str) -> bool {
    ["title", "summary", "detail"]
        .iter()
        .filter_map(|field| entry.get(field).and_then(Value::as_str))
        .any(|text| text.contains(needle))
}

fn request_contains_all(
    needles: &'static [&'static str],
) -> impl wiremock::Match + Send + Sync + 'static {
    move |req: &wiremock::Request| {
        let body_bytes = if req
            .headers
            .get("content-encoding")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(',')
                    .any(|entry| entry.trim().eq_ignore_ascii_case("zstd"))
            }) {
            zstd::stream::decode_all(std::io::Cursor::new(&req.body)).unwrap_or_default()
        } else {
            req.body.clone()
        };
        let body = std::str::from_utf8(&body_bytes).unwrap_or("");
        needles.iter().all(|needle| body.contains(needle))
    }
}

fn assert_sent_user_input(
    captured: &[(codex_protocol::ThreadId, Op)],
    needles: &[&str],
) -> codex_protocol::ThreadId {
    captured
        .iter()
        .find_map(|(captured_thread_id, op)| {
            user_input_contains_all(op, needles).then_some(*captured_thread_id)
        })
        .unwrap_or_else(|| panic!("expected UserInput containing all of {needles:?}"))
}

async fn wait_for_captured_input(
    thread_manager: &Arc<codex_core::ThreadManager>,
    thread_id: Option<codex_protocol::ThreadId>,
    needles: &[&str],
) -> codex_protocol::ThreadId {
    for _ in 0..100 {
        let captured = codex_core::test_support::captured_thread_manager_ops(thread_manager);
        if let Some(found_thread_id) = captured.iter().find_map(|(captured_thread_id, op)| {
            if thread_id.is_some_and(|thread_id| thread_id != *captured_thread_id) {
                return None;
            }
            user_input_contains_all(op, needles).then_some(*captured_thread_id)
        }) {
            return found_thread_id;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let captured = codex_core::test_support::captured_thread_manager_ops(thread_manager);
    let seen = captured
        .iter()
        .filter_map(|(captured_thread_id, op)| {
            if thread_id.is_some_and(|thread_id| thread_id != *captured_thread_id) {
                return None;
            }
            match op {
                Op::UserInput { items, .. } => items.iter().find_map(|item| match item {
                    UserInput::Text { text, .. } => Some(format!(
                        "{captured_thread_id}: {}",
                        text.chars().take(400).collect::<String>()
                    )),
                    _ => None,
                }),
                _ => None,
            }
        })
        .collect::<Vec<_>>();
    panic!(
        "thread {thread_id:?} should receive UserInput containing all of {needles:?}. Seen inputs: {seen:#?}"
    );
}

fn user_input_contains_all(op: &Op, needles: &[&str]) -> bool {
    matches!(
        op,
        Op::UserInput { items, .. }
            if items.iter().any(|item| match item {
                UserInput::Text { text, .. } => {
                    needles.iter().all(|needle| text.contains(needle))
                }
                _ => false,
            })
    )
}

fn captured_user_input_text(
    captured: &[(codex_protocol::ThreadId, Op)],
    thread_id: codex_protocol::ThreadId,
    needles: &[&str],
) -> String {
    captured
        .iter()
        .find_map(|(captured_thread_id, op)| {
            if *captured_thread_id != thread_id {
                return None;
            }
            match op {
                Op::UserInput { items, .. } => items.iter().find_map(|item| match item {
                    UserInput::Text { text, .. }
                        if needles.iter().all(|needle| text.contains(needle)) =>
                    {
                        Some(text.clone())
                    }
                    _ => None,
                }),
                _ => None,
            }
        })
        .unwrap_or_else(|| {
            panic!("thread {thread_id} should receive UserInput containing all of {needles:?}")
        })
}

fn function_call_output_text_from_requests(
    requests: &[core_test_support::responses::ResponsesRequest],
    call_id: &str,
) -> String {
    for request in requests {
        if let Some(text) = function_call_output_text(request, call_id) {
            return text;
        }
    }
    let inputs = requests
        .iter()
        .map(core_test_support::responses::ResponsesRequest::input)
        .collect::<Vec<_>>();
    eprintln!(
        "request inputs for missing {call_id}: {}",
        serde_json::to_string_pretty(&inputs).expect("request input serializes")
    );
    panic!("function call output {call_id} should be present");
}

fn function_call_output_text(
    request: &core_test_support::responses::ResponsesRequest,
    call_id: &str,
) -> Option<String> {
    let input = request.input();
    let Some(item) = input.iter().find(|item| {
        item.get("type").and_then(Value::as_str) == Some("function_call_output")
            && item.get("call_id").and_then(Value::as_str) == Some(call_id)
    }) else {
        return None;
    };
    Some(match item.get("output") {
        Some(Value::String(text)) => text.to_string(),
        Some(Value::Object(obj)) => obj
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("function call output {call_id} should contain text"))
            .to_string(),
        other => panic!("function call output {call_id} should contain text, got {other:?}"),
    })
}

fn manifest_path(relative: impl AsRef<Path>) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}
