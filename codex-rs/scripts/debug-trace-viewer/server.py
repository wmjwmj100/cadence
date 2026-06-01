#!/usr/bin/env python3
"""
Debug trace viewer server.

Serves a small frontend and exposes JSON APIs for reading canonical debug traces:
- GET /api/health
- GET /api/conversations
- GET /api/conversations/<conversation_id>/snapshot
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import webbrowser
from dataclasses import dataclass
from datetime import datetime, timezone
from http import HTTPStatus
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any
from urllib.parse import unquote, urlparse


SCRIPT_DIR = Path(__file__).resolve().parent
STATIC_DIR = SCRIPT_DIR / "static"
HISTORY_FILE = "history.latest.json"
METADATA_FILE = "metadata.json"


def default_trace_root() -> Path:
    env_override = os.environ.get("CODEX_DEBUG_TRACE_DIR", "").strip()
    if env_override:
        return Path(env_override).expanduser().resolve()

    codex_home = os.environ.get("CODEX_HOME", "").strip()
    if codex_home:
        return (Path(codex_home).expanduser() / "debug" / "conversations").resolve()

    return (Path.home() / ".codex" / "debug" / "conversations").resolve()


def iso_from_unix_sec(unix_sec: int | None) -> str | None:
    if unix_sec is None:
        return None
    try:
        return datetime.fromtimestamp(unix_sec, tz=timezone.utc).isoformat()
    except Exception:
        return None


def load_json(path: Path) -> Any:
    with path.open("r", encoding="utf-8") as f:
        return json.load(f)


@dataclass
class ViewerConfig:
    trace_root: Path


class ViewerHandler(SimpleHTTPRequestHandler):
    config: ViewerConfig

    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=str(STATIC_DIR), **kwargs)

    def do_GET(self) -> None:  # noqa: N802
        parsed = urlparse(self.path)
        path = parsed.path

        if path == "/api/health":
            self._write_json(HTTPStatus.OK, {"ok": True})
            return

        if path == "/api/conversations":
            self._handle_list_conversations()
            return

        if path.startswith("/api/conversations/") and path.endswith("/snapshot"):
            self._handle_get_snapshot(path)
            return

        super().do_GET()

    def log_message(self, fmt: str, *args) -> None:
        sys.stderr.write(
            "%s - - [%s] %s\n"
            % (self.address_string(), self.log_date_time_string(), fmt % args)
        )

    def _handle_list_conversations(self) -> None:
        root = self.config.trace_root
        root.mkdir(parents=True, exist_ok=True)

        items: list[dict[str, Any]] = []
        for child in root.iterdir():
            if not child.is_dir():
                continue

            history_path = child / HISTORY_FILE
            if not history_path.exists():
                continue

            metadata_path = child / METADATA_FILE
            metadata: dict[str, Any] | None = None
            if metadata_path.exists():
                try:
                    data = load_json(metadata_path)
                    if isinstance(data, dict):
                        metadata = data
                except Exception:
                    metadata = None

            updated_unix = None
            updated_rfc = None
            entry_count = None
            lane_count = None
            if metadata:
                updated_unix = metadata.get("updated_at_unix_sec")
                updated_rfc = metadata.get("updated_at_rfc3339_sec")
                entry_count = metadata.get("entry_count")
                lane_count = metadata.get("lane_count")

            if not isinstance(updated_unix, int):
                updated_unix = int(history_path.stat().st_mtime)
            if not isinstance(updated_rfc, str):
                updated_rfc = iso_from_unix_sec(updated_unix)

            items.append(
                {
                    "conversationId": child.name,
                    "updatedAtUnixSec": updated_unix,
                    "updatedAtRfc3339Sec": updated_rfc,
                    "entryCount": entry_count,
                    "laneCount": lane_count,
                    "historyPath": str(history_path),
                    "metadataPath": str(metadata_path) if metadata_path.exists() else None,
                }
            )

        items.sort(key=lambda x: x.get("updatedAtUnixSec", 0), reverse=True)
        self._write_json(HTTPStatus.OK, {"conversations": items, "traceRoot": str(root)})

    def _handle_get_snapshot(self, path: str) -> None:
        prefix = "/api/conversations/"
        suffix = "/snapshot"
        conversation_id = unquote(path[len(prefix) : -len(suffix)]).strip()

        if not self._is_valid_conversation_id(conversation_id):
            self._write_json(HTTPStatus.BAD_REQUEST, {"error": "invalid conversation id"})
            return

        root = self.config.trace_root
        conversation_dir = (root / conversation_id).resolve()
        root_resolved = root.resolve()
        if conversation_dir.parent != root_resolved:
            self._write_json(HTTPStatus.BAD_REQUEST, {"error": "invalid conversation id path"})
            return

        history_path = conversation_dir / HISTORY_FILE
        if not history_path.exists():
            self._write_json(HTTPStatus.NOT_FOUND, {"error": "history file not found"})
            return

        try:
            snapshot = load_json(history_path)
        except Exception as exc:
            self._write_json(
                HTTPStatus.INTERNAL_SERVER_ERROR,
                {"error": f"failed to parse history: {exc}"},
            )
            return

        metadata = None
        metadata_path = conversation_dir / METADATA_FILE
        if metadata_path.exists():
            try:
                metadata = load_json(metadata_path)
            except Exception:
                metadata = None

        self._write_json(
            HTTPStatus.OK,
            {
                "conversationId": conversation_id,
                "snapshot": snapshot,
                "metadata": metadata,
                "historyPath": str(history_path),
                "metadataPath": str(metadata_path) if metadata_path.exists() else None,
            },
        )

    @staticmethod
    def _is_valid_conversation_id(conversation_id: str) -> bool:
        if not conversation_id:
            return False
        if any(sep in conversation_id for sep in ["/", "\\", "\x00"]):
            return False
        return True

    def _write_json(self, status: HTTPStatus, payload: Any) -> None:
        body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        self.send_response(status.value)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Cache-Control", "no-store")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def build_handler(config: ViewerConfig):
    class _Handler(ViewerHandler):
        pass

    _Handler.config = config
    return _Handler


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Serve a browser UI for Codex canonical debug traces."
    )
    parser.add_argument(
        "--root",
        default=str(default_trace_root()),
        help="Trace root directory containing <conversation_id>/history.latest.json",
    )
    parser.add_argument("--host", default="127.0.0.1", help="Host to bind")
    parser.add_argument("--port", type=int, default=8765, help="Port to bind")
    parser.add_argument(
        "--open",
        action="store_true",
        help="Open browser automatically after startup",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    trace_root = Path(args.root).expanduser().resolve()
    config = ViewerConfig(trace_root=trace_root)
    handler = build_handler(config)
    server = ThreadingHTTPServer((args.host, args.port), handler)

    url = f"http://{args.host}:{args.port}"
    print(f"[debug-trace-viewer] serving: {url}")
    print(f"[debug-trace-viewer] trace root: {trace_root}")
    print("[debug-trace-viewer] press Ctrl+C to stop")

    if args.open:
        webbrowser.open(url)

    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

