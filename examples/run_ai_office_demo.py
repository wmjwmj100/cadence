from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from ai_office import (
    AgentRuntime,
    CollaborationScheduler,
    InMemorySandboxBackend,
    MiddlewareGateway,
    PilotDirectory,
    ScenarioRunner,
    ToolSpec,
)
from ai_office.scheduler import AgentWorkSummary


def main() -> None:
    backend = InMemorySandboxBackend()
    gateway = MiddlewareGateway(backend)
    gateway.register_tool(ToolSpec("echo", required_arguments=("text",)))
    runtime = AgentRuntime(gateway)
    tool_result = runtime.request_tool("agent_ceo", "echo", {"text": "AI Office MVP"})

    directory = PilotDirectory()
    session = directory.login("ceo", "password")

    runner = ScenarioRunner()
    scenarios = runner.run_all()

    opportunities = CollaborationScheduler().discover(
        [
            AgentWorkSummary("algorithm_agent", "模型 压缩 方案", ("算法",)),
            AgentWorkSummary("infra_agent", "部署 成本 压缩", ("infra",)),
        ]
    )

    print("tool_stdout=", tool_result.stdout)
    print("login_agent=", session.agent_id)
    print("scenario_count=", len(scenarios))
    print("opportunity_count=", len(opportunities))
    if runner.timeline.summaries:
        print("timeline_summary=", runner.timeline.summaries[-1].text)


if __name__ == "__main__":
    main()
