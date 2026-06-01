import http.client
import subprocess
import tempfile
import threading
import unittest
from urllib.parse import urlencode

from ai_office import (
    AgentRuntime,
    CollaborationRuntime,
    CollaborationScheduler,
    DockerSandboxBackend,
    InMemorySandboxBackend,
    MiddlewareGateway,
    MiddlewareHTTPInterface,
    PilotDirectory,
    PilotWebApp,
    SQLitePilotDirectory,
    make_pilot_http_server,
    ScenarioRunner,
    StepClock,
    SummaryAgent,
    TimelineService,
    ToolSpec,
    WorkspaceService,
)
from ai_office.collaboration import WaitState
from ai_office.scheduler import AgentWorkSummary
from ai_office.workspace import WorkspaceArea


class AiOfficeRoadmapTest(unittest.TestCase):
    def test_task1_runtime_only_emits_tool_intent_and_middleware_executes(self):
        backend = InMemorySandboxBackend()
        gateway = MiddlewareGateway(backend)
        gateway.register_tool(ToolSpec("echo", required_arguments=("text",)))
        runtime = AgentRuntime(gateway)

        result = runtime.request_tool("agent_a", "echo", {"text": "hello"})

        self.assertEqual(result.stdout, "hello")
        self.assertEqual(len(runtime.tool_intents), 1)
        self.assertEqual(len(backend.executed), 1)
        self.assertEqual(runtime.tool_intents[0], backend.executed[0])

    def test_task2_call_preserves_need_reply_and_reply_to_message_id(self):
        collab = CollaborationRuntime()
        collab.register("agent_a", "agent")
        collab.register("agent_b", "agent")

        request = collab.call("agent_a", "agent_b", "help", "m1", need_reply=True)
        reply = collab.call("agent_b", "agent_a", "done", "m2", reply_to_message_id="m1")

        self.assertEqual(request.message_id, "m1")
        self.assertEqual(reply.reply_to_message_id, "m1")
        self.assertTrue(collab.obligations["m1"].resolved)
        self.assertEqual(collab.obligations["m1"].resolved_by, "m2")

    def test_task3_wait_agent_is_7_minutes_and_wait_human_suspends(self):
        collab = CollaborationRuntime()
        collab.register("agent_a", "agent")
        collab.register("agent_b", "agent")
        collab.register("user_1", "human")

        agent_wait = collab.wait("agent_a", "agent_b")
        human_wait = collab.wait("agent_a", "user_1")

        self.assertEqual(agent_wait.state, WaitState.AGENT_TIMEOUT)
        self.assertEqual(agent_wait.timeout_seconds, 7 * 60)
        self.assertEqual(len(collab.wait_notices), 1)
        self.assertEqual(human_wait.state, WaitState.HUMAN_SUSPENDED)
        self.assertIsNone(human_wait.timeout_seconds)

        collab.call("agent_b", "agent_a", "unrelated agent update", "a1")
        still_waiting = collab.wait("agent_a", "user_1")
        self.assertEqual(still_waiting.state, WaitState.HUMAN_SUSPENDED)
        self.assertEqual(collab.inboxes["agent_a"][0].message_id, "a1")

        collab.call("user_1", "agent_a", "cancel that", "h1")
        delivered = collab.wait("agent_a", "user_1")
        self.assertEqual(delivered.state, WaitState.DELIVERED)
        self.assertEqual(delivered.message.content, "cancel that")

    def test_task4_workspace_private_public_rules_and_prompt(self):
        workspace = WorkspaceService()
        uploaded = workspace.upload_owner_file("agent_a", "owner.pdf", "pdf bytes")
        private_note = workspace.write_work_product("agent_a", "notes.md", "private", collaborative=False)
        public_report = workspace.write_work_product("agent_a", "report.md", "public", collaborative=True)
        migrated = workspace.publish_to_public(uploaded.path, "owner-summary.md")

        self.assertEqual(uploaded.area, WorkspaceArea.PRIVATE)
        self.assertEqual(private_note.area, WorkspaceArea.PRIVATE)
        self.assertEqual(public_report.area, WorkspaceArea.PUBLIC)
        self.assertEqual(migrated.area, WorkspaceArea.PUBLIC)
        self.assertIn("公共工作空间是团队协作的默认场所", WorkspaceService.SYSTEM_PROMPT_RULES)

    def test_task5_middleware_http_gateway_auth_validation_and_injection_defense(self):
        backend = InMemorySandboxBackend()
        gateway = MiddlewareGateway(backend, auth_tokens={"token"})
        gateway.register_tool(ToolSpec("write_file", required_arguments=("path", "content"), mutating=True))
        http = MiddlewareHTTPInterface(gateway)

        ok = http.post_tool_call(
            {"agent_id": "agent_a", "tool_name": "write_file", "arguments": {"path": "a.txt", "content": "safe"}},
            auth_token="token",
        )
        bad_auth = http.post_tool_call(
            {"agent_id": "agent_a", "tool_name": "write_file", "arguments": {"path": "a.txt", "content": "safe"}},
            auth_token="bad",
        )
        path_injection = http.post_tool_call(
            {"agent_id": "agent_a", "tool_name": "write_file", "arguments": {"path": "../secret", "content": "safe"}},
            auth_token="token",
        )
        content_is_data = http.post_tool_call(
            {"agent_id": "agent_a", "tool_name": "write_file", "arguments": {"path": "b.txt", "content": "x && rm -rf /"}},
            auth_token="token",
        )

        self.assertEqual(ok.status, 200)
        self.assertEqual(ok.body["exit_code"], 0)
        self.assertEqual(bad_auth.status, 400)
        self.assertEqual(path_injection.status, 400)
        self.assertEqual(content_is_data.status, 200)
        self.assertEqual(backend.files["b.txt"], "x && rm -rf /")


    def test_task5_docker_backend_builds_secure_runtime_command_and_cleans_up(self):
        calls = []

        def fake_runner(argv, **kwargs):
            calls.append((argv, kwargs))
            if argv[:3] == ["docker", "rm", "-f"]:
                return subprocess.CompletedProcess(argv, 0, "", "")
            return subprocess.CompletedProcess(argv, 0, "container output", "")

        with tempfile.TemporaryDirectory() as tmp_dir:
            backend = DockerSandboxBackend(
                image="python:3.12-alpine",
                workspace_root=tmp_dir,
                memory="128m",
                cpus="0.5",
                pids_limit=64,
                uid_gid="1000:1000",
                runner=fake_runner,
            )
            gateway = MiddlewareGateway(backend)
            gateway.register_tool(ToolSpec("echo", required_arguments=("text",), timeout_seconds=9))
            runtime = AgentRuntime(gateway)

            result = runtime.request_tool("agent_a", "echo", {"text": "hello from docker"})

        run_argv, run_kwargs = calls[0]
        cleanup_argv, cleanup_kwargs = calls[1]
        self.assertEqual(result.stdout, "container output")
        self.assertEqual(run_argv[:2], ["docker", "run"])
        self.assertNotIn("--privileged", run_argv)
        self.assertNotIn("--rm", run_argv)
        self.assertIn("--network", run_argv)
        self.assertEqual(run_argv[run_argv.index("--network") + 1], "none")
        self.assertIn("--read-only", run_argv)
        self.assertIn("--cap-drop", run_argv)
        self.assertEqual(run_argv[run_argv.index("--cap-drop") + 1], "ALL")
        self.assertIn("--security-opt", run_argv)
        self.assertEqual(run_argv[run_argv.index("--security-opt") + 1], "no-new-privileges")
        self.assertEqual(run_argv[run_argv.index("--memory") + 1], "128m")
        self.assertEqual(run_argv[run_argv.index("--cpus") + 1], "0.5")
        self.assertEqual(run_argv[run_argv.index("--pids-limit") + 1], "64")
        self.assertEqual(run_argv[run_argv.index("--user") + 1], "1000:1000")
        mount = run_argv[run_argv.index("--mount") + 1]
        self.assertIn("dst=/workspace", mount)
        self.assertIn("readonly", mount)
        self.assertIn("python:3.12-alpine", run_argv)
        self.assertEqual(run_kwargs["timeout"], 9)
        self.assertTrue(run_kwargs["capture_output"])
        self.assertTrue(run_kwargs["text"])
        self.assertEqual(cleanup_argv[:3], ["docker", "rm", "-f"])
        self.assertEqual(cleanup_argv[3], run_argv[run_argv.index("--name") + 1])
        self.assertEqual(cleanup_kwargs["timeout"], 10)

    def test_task5_docker_backend_mounts_writable_workspace_for_mutating_tools(self):
        calls = []

        def fake_runner(argv, **kwargs):
            calls.append(argv)
            return subprocess.CompletedProcess(argv, 0, "", "")

        with tempfile.TemporaryDirectory() as tmp_dir:
            backend = DockerSandboxBackend("python:3.12-alpine", tmp_dir, runner=fake_runner)
            gateway = MiddlewareGateway(backend)
            gateway.register_tool(ToolSpec("write_file", required_arguments=("path", "content"), mutating=True))
            result = AgentRuntime(gateway).request_tool(
                "agent_a",
                "write_file",
                {"path": "reports/today.md", "content": "plain content with && treated as data"},
            )

        run_argv = calls[0]
        mount = run_argv[run_argv.index("--mount") + 1]
        self.assertEqual(result.exit_code, 0)
        self.assertNotIn("readonly", mount)
        self.assertIn("/workspace/reports/today.md", run_argv)
        self.assertIn("plain content with && treated as data", run_argv)

    def test_task5_docker_backend_timeout_returns_124_and_removes_container(self):
        calls = []

        def fake_runner(argv, **kwargs):
            calls.append(argv)
            if argv[:2] == ["docker", "run"]:
                raise subprocess.TimeoutExpired(argv, kwargs["timeout"])
            return subprocess.CompletedProcess(argv, 0, "", "")

        with tempfile.TemporaryDirectory() as tmp_dir:
            backend = DockerSandboxBackend("python:3.12-alpine", tmp_dir, runner=fake_runner)
            gateway = MiddlewareGateway(backend)
            gateway.register_tool(ToolSpec("echo", required_arguments=("text",), timeout_seconds=2))
            result = AgentRuntime(gateway).request_tool("agent_a", "echo", {"text": "slow"})

        self.assertEqual(result.exit_code, 124)
        self.assertIn("timeout after 2s", result.stderr)
        self.assertEqual(calls[1][:3], ["docker", "rm", "-f"])
        self.assertEqual(calls[1][3], calls[0][calls[0].index("--name") + 1])

    def test_task5_docker_backend_preserves_nonzero_exit_without_raising(self):
        calls = []

        def fake_runner(argv, **kwargs):
            calls.append(argv)
            if argv[:3] == ["docker", "rm", "-f"]:
                return subprocess.CompletedProcess(argv, 0, "", "")
            return subprocess.CompletedProcess(argv, 2, "partial", "tool failed")

        with tempfile.TemporaryDirectory() as tmp_dir:
            backend = DockerSandboxBackend("python:3.12-alpine", tmp_dir, runner=fake_runner)
            gateway = MiddlewareGateway(backend)
            gateway.register_tool(ToolSpec("read_file", required_arguments=("path",)))
            result = AgentRuntime(gateway).request_tool("agent_a", "read_file", {"path": "missing.txt"})

        self.assertEqual(result.stdout, "partial")
        self.assertEqual(result.stderr, "tool failed")
        self.assertEqual(result.exit_code, 2)
        self.assertEqual(calls[1][:3], ["docker", "rm", "-f"])

    def test_task5_docker_backend_rejects_escaping_paths_before_runner(self):
        calls = []

        def fake_runner(argv, **kwargs):
            calls.append(argv)
            return subprocess.CompletedProcess(argv, 0, "", "")

        with tempfile.TemporaryDirectory() as tmp_dir:
            backend = DockerSandboxBackend("python:3.12-alpine", tmp_dir, runner=fake_runner)
            gateway = MiddlewareGateway(backend)
            gateway.register_tool(ToolSpec("read_file", required_arguments=("path",)))
            runtime = AgentRuntime(gateway)
            bad_paths = ["../secret", "/etc/passwd", "ok;rm", "line\nbreak"]
            for bad_path in bad_paths:
                with self.subTest(path=bad_path):
                    with self.assertRaises(Exception):
                        runtime.request_tool("agent_a", "read_file", {"path": bad_path})

        self.assertEqual(calls, [])

    def test_task6_global_15_steps_triggers_summary_agent(self):
        timeline = TimelineService(SummaryAgent(), StepClock(threshold=15))
        produced = []
        for index in range(5):
            produced.append(timeline.record_agent_step("agent_a", f"a loop {index}"))
            produced.append(timeline.record_agent_step("agent_b", f"b loop {index}"))
            produced.append(timeline.record_agent_step("agent_c", f"c loop {index}"))

        self.assertEqual(timeline.step_clock.total_steps, 15)
        self.assertEqual(len([item for item in produced if item is not None]), 1)
        self.assertEqual(timeline.summaries[0].step_index, 15)
        self.assertEqual(len(timeline.summaries[0].source_events), 15)

    def test_task7_scheduler_finds_high_value_overlap_not_blocker_reminders(self):
        scheduler = CollaborationScheduler()
        opportunities = scheduler.discover(
            [
                AgentWorkSummary("algorithm", "模型 压缩 方案", ("算法",)),
                AgentWorkSummary("infra", "部署 成本 压缩", ("infra",)),
                AgentWorkSummary("ops", "等待 回复 阻塞", ("运营",)),
            ]
        )

        self.assertEqual(opportunities[0].participants, ("algorithm", "infra"))
        self.assertIn("压缩", opportunities[0].reason)
        self.assertIn("不要提醒普通等待", scheduler.SYSTEM_PROMPT)

    def test_task8_minimal_real_scenarios_run(self):
        runner = ScenarioRunner()
        results = runner.run_all()
        names = [result.name for result in results]

        self.assertEqual(len(results), 5)
        self.assertIn("Research Team 调研", names)
        self.assertIn("每日复盘", names)
        self.assertTrue(runner.workspace.files)
        self.assertTrue(runner.timeline.events)

    def test_task9_browser_login_session_interview_and_reflection_routes(self):
        directory = PilotDirectory()
        app = PilotWebApp(directory)
        server = make_pilot_http_server("127.0.0.1", 0, app)
        host, port = server.server_address
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()

        try:
            connection = http.client.HTTPConnection(host, port, timeout=5)
            connection.request("GET", "/")
            response = connection.getresponse()
            body = response.read().decode("utf-8")
            self.assertEqual(response.status, 200)
            self.assertIn("AI Office 内部协作系统", body)
            self.assertIn("登录我的 Agent", body)

            form = urlencode({"username": "ceo", "password": "password"})
            connection.request(
                "POST",
                "/login",
                body=form,
                headers={"Content-Type": "application/x-www-form-urlencoded"},
            )
            response = connection.getresponse()
            response.read()
            self.assertEqual(response.status, 303)
            self.assertEqual(response.getheader("location"), "/me")
            cookie = response.getheader("set-cookie").split(";", 1)[0]

            connection.request("GET", "/me", headers={"Cookie": cookie})
            response = connection.getresponse()
            dashboard = response.read().decode("utf-8")
            self.assertEqual(response.status, 200)
            self.assertIn("agent_ceo", dashboard)
            self.assertIn("主人画像", dashboard)

            interview = urlencode(
                {
                    "work": "负责公司方向",
                    "decisions": "资源分配",
                    "strengths": "战略判断",
                    "avoid": "不要打扰实现细节",
                    "reporting": "先结论后细节",
                }
            )
            connection.request(
                "POST",
                "/api/interview",
                body=interview,
                headers={"Content-Type": "application/x-www-form-urlencoded", "Cookie": cookie},
            )
            response = connection.getresponse()
            response.read()
            self.assertEqual(response.status, 303)
            self.assertEqual(response.getheader("location"), "/me")

            reflection = urlencode({"interactions": "高质量资源判断\n拒绝实现细节打扰"})
            connection.request(
                "POST",
                "/api/reflection",
                body=reflection,
                headers={"Content-Type": "application/x-www-form-urlencoded", "Cookie": cookie},
            )
            response = connection.getresponse()
            response.read()
            self.assertEqual(response.status, 303)

            profile = directory.humans["user_1"]
            self.assertIn("战略判断", profile.capability_labels)
            self.assertIn("先结论后细节", profile.preferences)
            self.assertIn("拒绝实现细节打扰", profile.do_not_disturb)
            self.assertIsNotNone(profile.last_reflection_at)
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)

    def test_task9_sqlite_database_persists_profiles_and_web_sessions(self):
        with tempfile.TemporaryDirectory() as tmp_dir:
            db_path = f"{tmp_dir}/ai_office.sqlite3"
            directory = SQLitePilotDirectory(db_path)
            app = PilotWebApp(directory)

            login = app.login("employee_b", "password")
            self.assertEqual(login.status, 200)
            token = login.body["session_token"]
            self.assertEqual(login.body["agent_id"], "agent_employee_b")

            interview = app.submit_interview(
                token,
                {
                    "work": "负责基础设施",
                    "decisions": "算力预算",
                    "strengths": "容量规划",
                    "avoid": "不要打扰审美细节",
                    "reporting": "先风险后方案",
                },
            )
            self.assertEqual(interview.status, 200)
            reflection = app.submit_reflection(token, ["高质量容量规划", "拒绝审美细节打扰"])
            self.assertEqual(reflection.status, 200)

            restarted_directory = SQLitePilotDirectory(db_path)
            restarted_app = PilotWebApp(restarted_directory)
            persisted = restarted_app.me(token)

            self.assertEqual(persisted.status, 200)
            self.assertEqual(persisted.body["agent_id"], "agent_employee_b")
            self.assertIn("容量规划", persisted.body["capability_labels"])
            self.assertIn("先风险后方案", persisted.body["preferences"])
            self.assertIn("拒绝审美细节打扰", persisted.body["do_not_disturb"])
            self.assertIsNotNone(restarted_directory.humans["user_3"].last_reflection_at)

            restarted_app.logout(token)
            self.assertEqual(PilotWebApp(SQLitePilotDirectory(db_path)).me(token).status, 401)

    def test_task9_six_person_login_binding_interview_and_daily_reflection(self):
        directory = PilotDirectory()
        app = PilotWebApp(directory)

        self.assertEqual(len(directory.accounts), 6)
        response = app.login("ceo", "password")
        self.assertEqual(response.status, 200)
        self.assertEqual(response.body["agent_id"], "agent_ceo")

        profile = directory.run_mini_interview(
            response.body["user_id"],
            {
                "work": "负责公司方向",
                "decisions": "资源分配",
                "strengths": "战略判断",
                "avoid": "不要打扰实现细节",
                "reporting": "先结论后细节",
            },
        )
        reflected = directory.daily_reflection(profile.user_id, ["高质量资源判断", "拒绝实现细节打扰"])

        self.assertIn("战略判断", reflected.capability_labels)
        self.assertIn("拒绝实现细节打扰", reflected.do_not_disturb)
        self.assertIsNotNone(reflected.last_reflection_at)


if __name__ == "__main__":
    unittest.main()
