#!/usr/bin/env python3
"""
Generate a machine-readable and human-readable report from a Swarm JSONL run.

Inputs:
- wecode `--json` event stream file
- stderr log file (for dispatcher runtime debug lines)
- optional model-io debug directory

Outputs:
- report markdown
- report json
"""

from __future__ import annotations

import argparse
import json
import re
from collections import Counter, defaultdict
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


ACTIVATION_INPUT_RE = re.compile(
    r"dispatcher activation input: activation_id=(?P<activation_id>\d+), "
    r"queue_entries=(?P<queue_entries>\d+), queue_lines_json=(?P<queue_lines_json>.+)$"
)
ACTIVATION_NO_REMINDER_RE = re.compile(
    r"dispatcher activation ended with no reminders "
    r"\(activation_id=(?P<activation_id>\d+), entries=(?P<entries>\d+)\)"
)
REMINDER_SENT_RE = re.compile(
    r"dispatcher reminder sent: activation_id=(?P<activation_id>\d+), "
    r"target=(?P<target>[^,]+), message_json=(?P<message_json>\".*\"), "
    r"reason_json=(?P<reason_json>\".*\")"
)
REMINDER_DEDUPE_RE = re.compile(
    r"dispatcher reminder dedupe_suppressed: activation_id=(?P<activation_id>\d+), "
    r"target=(?P<target>[^,]+), message_json=(?P<message_json>\".*\"), "
    r"reason_json=(?P<reason_json>\".*\")"
)
REMINDER_TARGET_NOT_FOUND_RE = re.compile(
    r"dispatcher reminder target_not_found: activation_id=(?P<activation_id>\d+), "
    r"target=(?P<target>[^,]+), reason_json=(?P<reason_json>\".*\")"
)
ACTIVATION_COMPLETE_RE = re.compile(
    r"dispatcher activation complete: activation_id=(?P<activation_id>\d+), "
    r"prompt_chars=(?P<prompt_chars>\d+), queue_entries=(?P<queue_entries>\d+), "
    r"reminder_attempts=(?P<reminder_attempts>\d+), "
    r"reminder_successes=(?P<reminder_successes>\d+), "
    r"reminder_failures=(?P<reminder_failures>\d+), "
    r"dedupe_suppressions=(?P<dedupe_suppressions>\d+)"
)


@dataclass
class DispatcherActivation:
    activation_id: int
    queue_entries: int | None = None
    queue_lines: list[str] = field(default_factory=list)
    no_reminders: bool = False
    reminders_sent: list[dict[str, str]] = field(default_factory=list)
    reminders_dedupe: list[dict[str, str]] = field(default_factory=list)
    reminders_target_not_found: list[dict[str, str]] = field(default_factory=list)
    metrics: dict[str, int] = field(default_factory=dict)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Parse Swarm run artifacts and emit a report.")
    parser.add_argument("--event-log", required=True, help="Path to wecode --json output.")
    parser.add_argument("--stderr-log", required=True, help="Path to captured stderr log.")
    parser.add_argument(
        "--model-io-dir",
        default="",
        help="Optional model-io debug directory (from CODEX_DEBUG_MODEL_IO_DIR).",
    )
    parser.add_argument("--report", required=True, help="Output markdown path.")
    parser.add_argument("--report-json", required=True, help="Output JSON path.")
    return parser.parse_args()


def load_jsonl(path: Path) -> tuple[list[dict[str, Any]], list[str]]:
    events: list[dict[str, Any]] = []
    parse_errors: list[str] = []
    if not path.exists():
        return events, [f"missing event log: {path}"]

    for idx, raw in enumerate(path.read_text(encoding="utf-8", errors="replace").splitlines(), start=1):
        line = raw.strip()
        if not line:
            continue
        if not line.startswith("{"):
            # stderr noise may appear if redirection mixed streams.
            continue
        try:
            events.append(json.loads(line))
        except json.JSONDecodeError as exc:
            parse_errors.append(f"{path}:{idx}: {exc}")
    return events, parse_errors


def safe_json_loads(raw: str) -> Any:
    try:
        return json.loads(raw)
    except json.JSONDecodeError:
        return None


def parse_dispatcher_log(stderr_path: Path) -> tuple[dict[int, DispatcherActivation], list[str]]:
    activations: dict[int, DispatcherActivation] = {}
    parse_errors: list[str] = []
    if not stderr_path.exists():
        return activations, [f"missing stderr log: {stderr_path}"]

    lines = stderr_path.read_text(encoding="utf-8", errors="replace").splitlines()
    for idx, line in enumerate(lines, start=1):
        matched = False

        match = ACTIVATION_INPUT_RE.search(line)
        if match:
            matched = True
            activation_id = int(match.group("activation_id"))
            activation = activations.setdefault(activation_id, DispatcherActivation(activation_id=activation_id))
            activation.queue_entries = int(match.group("queue_entries"))
            queue_lines_json = match.group("queue_lines_json")
            queue_lines = safe_json_loads(queue_lines_json)
            if isinstance(queue_lines, list):
                activation.queue_lines = [str(item) for item in queue_lines]
            else:
                parse_errors.append(
                    f"{stderr_path}:{idx}: failed to parse queue_lines_json for activation {activation_id}"
                )

        match = ACTIVATION_NO_REMINDER_RE.search(line)
        if match:
            matched = True
            activation_id = int(match.group("activation_id"))
            activation = activations.setdefault(activation_id, DispatcherActivation(activation_id=activation_id))
            activation.no_reminders = True
            if activation.queue_entries is None:
                activation.queue_entries = int(match.group("entries"))

        match = REMINDER_SENT_RE.search(line)
        if match:
            matched = True
            activation_id = int(match.group("activation_id"))
            activation = activations.setdefault(activation_id, DispatcherActivation(activation_id=activation_id))
            activation.reminders_sent.append(
                {
                    "target": match.group("target").strip(),
                    "message": safe_json_loads(match.group("message_json")) or "",
                    "reason": safe_json_loads(match.group("reason_json")) or "",
                }
            )

        match = REMINDER_DEDUPE_RE.search(line)
        if match:
            matched = True
            activation_id = int(match.group("activation_id"))
            activation = activations.setdefault(activation_id, DispatcherActivation(activation_id=activation_id))
            activation.reminders_dedupe.append(
                {
                    "target": match.group("target").strip(),
                    "message": safe_json_loads(match.group("message_json")) or "",
                    "reason": safe_json_loads(match.group("reason_json")) or "",
                }
            )

        match = REMINDER_TARGET_NOT_FOUND_RE.search(line)
        if match:
            matched = True
            activation_id = int(match.group("activation_id"))
            activation = activations.setdefault(activation_id, DispatcherActivation(activation_id=activation_id))
            activation.reminders_target_not_found.append(
                {
                    "target": match.group("target").strip(),
                    "reason": safe_json_loads(match.group("reason_json")) or "",
                }
            )

        match = ACTIVATION_COMPLETE_RE.search(line)
        if match:
            matched = True
            activation_id = int(match.group("activation_id"))
            activation = activations.setdefault(activation_id, DispatcherActivation(activation_id=activation_id))
            activation.metrics = {
                "prompt_chars": int(match.group("prompt_chars")),
                "queue_entries": int(match.group("queue_entries")),
                "reminder_attempts": int(match.group("reminder_attempts")),
                "reminder_successes": int(match.group("reminder_successes")),
                "reminder_failures": int(match.group("reminder_failures")),
                "dedupe_suppressions": int(match.group("dedupe_suppressions")),
            }
            if activation.queue_entries is None:
                activation.queue_entries = activation.metrics["queue_entries"]

        if "dispatcher" in line and not matched and "activation" in line:
            parse_errors.append(f"{stderr_path}:{idx}: unparsed dispatcher line: {line}")

    return activations, parse_errors


def truncate(text: str, limit: int = 180) -> str:
    text = " ".join(text.split())
    if len(text) <= limit:
        return text
    return text[: limit - 3] + "..."


def summarize_events(events: list[dict[str, Any]]) -> dict[str, Any]:
    event_types = Counter()
    item_types = Counter()
    collab_starts: list[dict[str, Any]] = []
    collab_completions: dict[str, dict[str, Any]] = {}
    agent_messages: list[str] = []

    for event in events:
        event_type = event.get("type", "")
        event_types[event_type] += 1
        item = event.get("item")
        if isinstance(item, dict):
            item_type = item.get("type", "")
            if item_type:
                item_types[item_type] += 1
            if event_type == "item.started" and item_type == "collab_tool_call":
                collab_starts.append(
                    {
                        "id": item.get("id"),
                        "tool": item.get("tool"),
                        "sender_thread_id": item.get("sender_thread_id"),
                        "receiver_thread_ids": item.get("receiver_thread_ids", []),
                        "prompt": item.get("prompt"),
                        "status": item.get("status"),
                    }
                )
            if event_type == "item.completed" and item_type == "collab_tool_call":
                collab_completions[item.get("id", "")] = item
            if event_type == "item.completed" and item_type == "agent_message":
                text = item.get("text")
                if isinstance(text, str) and text.strip():
                    agent_messages.append(text.strip())

    collab_calls: list[dict[str, Any]] = []
    tool_counts = Counter()
    for idx, start in enumerate(collab_starts, start=1):
        call_id = start.get("id") or f"call-{idx}"
        completion = collab_completions.get(call_id, {})
        prompt = start.get("prompt")
        prompt_header = ""
        if isinstance(prompt, str) and prompt.strip():
            prompt_header = prompt.splitlines()[0].strip()

        entry = {
            "seq": idx,
            "id": call_id,
            "tool": start.get("tool"),
            "sender_thread_id": start.get("sender_thread_id"),
            "receiver_thread_ids": start.get("receiver_thread_ids", []),
            "status_start": start.get("status"),
            "status_end": completion.get("status"),
            "prompt_header": prompt_header,
        }
        collab_calls.append(entry)
        if entry["tool"]:
            tool_counts[str(entry["tool"])] += 1

    return {
        "event_types": dict(event_types),
        "item_types": dict(item_types),
        "collab_calls": collab_calls,
        "collab_tool_counts": dict(tool_counts),
        "agent_messages": agent_messages,
    }


def summarize_model_io(model_io_dir: Path | None) -> dict[str, Any]:
    if model_io_dir is None or not model_io_dir.exists():
        return {"enabled": False, "threads": {}, "files": []}

    ai_flow_dir = model_io_dir / "ai-flow"
    if not ai_flow_dir.exists():
        return {"enabled": True, "threads": {}, "files": []}

    threads: dict[str, dict[str, Any]] = {}
    files: list[str] = []
    for file in sorted(ai_flow_dir.glob("model-io-*.ai.jsonl")):
        files.append(str(file))
        event_count = 0
        tool_name_counts = Counter()
        parse_errors = 0
        thread_id = file.stem.replace("model-io-", "").replace(".ai", "")
        for line in file.read_text(encoding="utf-8", errors="replace").splitlines():
            if not line.strip():
                continue
            try:
                data = json.loads(line)
            except json.JSONDecodeError:
                parse_errors += 1
                continue
            event_count += 1
            retrieval = data.get("retrieval", {})
            tool_name = retrieval.get("tool_name")
            if isinstance(tool_name, str) and tool_name:
                tool_name_counts[tool_name] += 1
            maybe_thread = data.get("thread_id")
            if isinstance(maybe_thread, str) and maybe_thread.strip():
                thread_id = maybe_thread

        threads[thread_id] = {
            "event_count": event_count,
            "tool_name_counts": dict(tool_name_counts),
            "parse_errors": parse_errors,
            "file": str(file),
        }

    return {"enabled": True, "threads": threads, "files": files}


def build_markdown_report(summary: dict[str, Any]) -> str:
    generated_at = datetime.now(timezone.utc).isoformat()
    lines: list[str] = []
    lines.append("# Swarm Auto Test Report")
    lines.append("")
    lines.append(f"- generated_at_utc: `{generated_at}`")
    lines.append(f"- event_log: `{summary['artifacts']['event_log']}`")
    lines.append(f"- stderr_log: `{summary['artifacts']['stderr_log']}`")
    model_io_dir = summary["artifacts"].get("model_io_dir")
    if model_io_dir:
        lines.append(f"- model_io_dir: `{model_io_dir}`")
    lines.append("")

    event_summary = summary["event_summary"]
    lines.append("## Event Summary")
    lines.append("")
    lines.append(f"- thread.started: `{event_summary['event_types'].get('thread.started', 0)}`")
    lines.append(f"- turn.started: `{event_summary['event_types'].get('turn.started', 0)}`")
    lines.append(f"- turn.completed: `{event_summary['event_types'].get('turn.completed', 0)}`")
    lines.append(f"- item.started: `{event_summary['event_types'].get('item.started', 0)}`")
    lines.append(f"- item.completed: `{event_summary['event_types'].get('item.completed', 0)}`")
    lines.append(f"- collab_tool_call starts: `{len(event_summary['collab_calls'])}`")
    lines.append(
        "- collab tool counts: "
        + ", ".join(
            f"`{name}`={count}"
            for name, count in sorted(event_summary["collab_tool_counts"].items())
        )
    )
    lines.append("")

    lines.append("## Collaboration Calls")
    lines.append("")
    lines.append("| # | tool | sender_thread_id | receivers | end_status | prompt_header |")
    lines.append("|---|---|---|---|---|---|")
    for call in event_summary["collab_calls"]:
        receivers = ",".join(call["receiver_thread_ids"]) if call["receiver_thread_ids"] else "-"
        lines.append(
            f"| {call['seq']} | {call.get('tool') or '-'} | {call.get('sender_thread_id') or '-'} | "
            f"{receivers} | {call.get('status_end') or '-'} | {truncate(call.get('prompt_header') or '-', 120)} |"
        )
    lines.append("")

    lines.append("## Agent Message Snippets")
    lines.append("")
    if event_summary["agent_messages"]:
        for idx, text in enumerate(event_summary["agent_messages"][:12], start=1):
            lines.append(f"{idx}. {truncate(text, 260)}")
        if len(event_summary["agent_messages"]) > 12:
            lines.append(
                f"- ... {len(event_summary['agent_messages']) - 12} more messages omitted for brevity."
            )
    else:
        lines.append("- none")
    lines.append("")

    dispatcher = summary["dispatcher"]
    lines.append("## Dispatcher Activations")
    lines.append("")
    lines.append(f"- detected_activations: `{dispatcher['activation_count']}`")
    lines.append(f"- reminders_sent_total: `{dispatcher['reminders_sent_total']}`")
    lines.append(f"- dedupe_suppressed_total: `{dispatcher['dedupe_suppressed_total']}`")
    lines.append(f"- target_not_found_total: `{dispatcher['target_not_found_total']}`")
    lines.append("")
    for activation in dispatcher["activations"]:
        lines.append(f"### activation_id={activation['activation_id']}")
        lines.append(f"- queue_entries: `{activation.get('queue_entries')}`")
        lines.append(f"- no_reminders: `{activation.get('no_reminders')}`")
        metrics = activation.get("metrics") or {}
        if metrics:
            lines.append(
                "- metrics: "
                + ", ".join(f"{k}={v}" for k, v in sorted(metrics.items()))
            )
        queue_lines = activation.get("queue_lines") or []
        if queue_lines:
            lines.append("- queue_lines_sample:")
            for item in queue_lines[:6]:
                lines.append(f"  - {truncate(item, 180)}")
            if len(queue_lines) > 6:
                lines.append(f"  - ... {len(queue_lines) - 6} more")
        sent = activation.get("reminders_sent") or []
        if sent:
            lines.append("- reminders_sent:")
            for entry in sent:
                lines.append(
                    "  - "
                    + f"target=`{entry.get('target')}` "
                    + f"message={truncate(entry.get('message') or '', 140)} "
                    + f"reason={truncate(entry.get('reason') or '', 140)}"
                )
        dedupe = activation.get("reminders_dedupe") or []
        if dedupe:
            lines.append("- reminders_dedupe:")
            for entry in dedupe:
                lines.append(
                    "  - "
                    + f"target=`{entry.get('target')}` "
                    + f"message={truncate(entry.get('message') or '', 140)}"
                )
        missing = activation.get("reminders_target_not_found") or []
        if missing:
            lines.append("- reminders_target_not_found:")
            for entry in missing:
                lines.append(
                    "  - "
                    + f"target=`{entry.get('target')}` "
                    + f"reason={truncate(entry.get('reason') or '', 140)}"
                )
        lines.append("")

    model_io = summary["model_io"]
    lines.append("## Model I/O Debug Files")
    lines.append("")
    lines.append(f"- enabled: `{model_io.get('enabled')}`")
    if model_io.get("threads"):
        lines.append("| thread_id | event_count | top_tools | parse_errors | file |")
        lines.append("|---|---:|---|---:|---|")
        for thread_id, info in sorted(model_io["threads"].items()):
            top_tools = ", ".join(
                f"{name}:{count}"
                for name, count in sorted(info.get("tool_name_counts", {}).items())
            )
            lines.append(
                f"| {thread_id} | {info.get('event_count', 0)} | {top_tools or '-'} | "
                f"{info.get('parse_errors', 0)} | {info.get('file', '-')} |"
            )
    else:
        lines.append("- no model-io files found")
    lines.append("")

    parse_errors = summary.get("parse_errors", [])
    lines.append("## Parse Notes")
    lines.append("")
    if parse_errors:
        for err in parse_errors:
            lines.append(f"- {err}")
    else:
        lines.append("- none")
    lines.append("")

    return "\n".join(lines)


def main() -> int:
    args = parse_args()
    event_log = Path(args.event_log).resolve()
    stderr_log = Path(args.stderr_log).resolve()
    report_path = Path(args.report).resolve()
    report_json_path = Path(args.report_json).resolve()
    model_io_dir = Path(args.model_io_dir).resolve() if args.model_io_dir else None

    events, event_parse_errors = load_jsonl(event_log)
    event_summary = summarize_events(events)
    dispatcher_activations, dispatcher_parse_errors = parse_dispatcher_log(stderr_log)
    model_io_summary = summarize_model_io(model_io_dir)

    activations_sorted = []
    sent_total = 0
    dedupe_total = 0
    missing_total = 0
    for activation_id in sorted(dispatcher_activations):
        activation = dispatcher_activations[activation_id]
        sent_total += len(activation.reminders_sent)
        dedupe_total += len(activation.reminders_dedupe)
        missing_total += len(activation.reminders_target_not_found)
        activations_sorted.append(
            {
                "activation_id": activation.activation_id,
                "queue_entries": activation.queue_entries,
                "queue_lines": activation.queue_lines,
                "no_reminders": activation.no_reminders,
                "reminders_sent": activation.reminders_sent,
                "reminders_dedupe": activation.reminders_dedupe,
                "reminders_target_not_found": activation.reminders_target_not_found,
                "metrics": activation.metrics,
            }
        )

    summary = {
        "artifacts": {
            "event_log": str(event_log),
            "stderr_log": str(stderr_log),
            "model_io_dir": str(model_io_dir) if model_io_dir else "",
        },
        "event_summary": event_summary,
        "dispatcher": {
            "activation_count": len(activations_sorted),
            "reminders_sent_total": sent_total,
            "dedupe_suppressed_total": dedupe_total,
            "target_not_found_total": missing_total,
            "activations": activations_sorted,
        },
        "model_io": model_io_summary,
        "parse_errors": event_parse_errors + dispatcher_parse_errors,
    }

    report_markdown = build_markdown_report(summary)
    report_path.parent.mkdir(parents=True, exist_ok=True)
    report_json_path.parent.mkdir(parents=True, exist_ok=True)
    report_path.write_text(report_markdown, encoding="utf-8")
    report_json_path.write_text(json.dumps(summary, ensure_ascii=False, indent=2), encoding="utf-8")

    print(f"report_md={report_path}")
    print(f"report_json={report_json_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

