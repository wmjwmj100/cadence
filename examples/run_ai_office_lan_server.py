from pathlib import Path
import os
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from ai_office import PilotWebApp, SQLitePilotDirectory, make_pilot_http_server


def main() -> None:
    host = os.environ.get("AI_OFFICE_HOST", "0.0.0.0")
    port = int(os.environ.get("AI_OFFICE_PORT", "8080"))
    db_path = Path(os.environ.get("AI_OFFICE_DB", "/work/Cadence/data/ai_office.sqlite3"))
    app = PilotWebApp(SQLitePilotDirectory(db_path))
    server = make_pilot_http_server(host, port, app)
    print(f"AI Office LAN server running at http://{host}:{port}")
    print(f"SQLite database: {db_path}")
    print("Open http://<server-lan-ip>:8080 in a browser and login with your pilot account.")
    server.serve_forever()


if __name__ == "__main__":
    main()
