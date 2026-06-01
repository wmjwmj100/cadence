# codex-core

This crate implements the business logic for Codex. It is designed to be used by the various Codex UIs written in Rust.


## AI Office Internal Pilot

`codex-core` includes two opt-in binaries for running the six-person AI Office
pilot from the Rust main project.

Start the browser login/API server on your LAN:

```bash
cargo run -p codex-core --bin codex-office-server -- \
  --db /srv/codex-office/office-store.json \
  --host 0.0.0.0 \
  --port 8080
```

Open `http://<server-ip>:8080/login`. The seeded MVP accounts are `ceo` and
`employee_a` through `employee_e`, with the development password `password`.
Change these credentials before using the server outside a trusted internal
pilot. The store file persists accounts, human profiles, Agent bindings,
browser sessions, interview answers, and daily reflection updates across
restart.

The same server exposes JSON routes for integrations:

- `POST /api/login` creates an HTTP-only session cookie.
- `GET /api/me` returns the logged-in user's account, Human profile, and Agent profile.
- `POST /api/interview` updates the Human profile from onboarding answers.
- `POST /api/reflection` records daily profile updates.
- `POST /api/logout` revokes the persisted browser session.

Start the Docker execution gateway separately, preferably bound to localhost or
an internal service network:

```bash
cargo run -p codex-core --bin codex-docker-gateway -- \
  --image <sandbox-image> \
  --workspace-root /srv/codex-workspaces \
  --company-id acme \
  --container-name-prefix codex-company \
  --host 127.0.0.1 \
  --port 8090 \
  --bearer-token <shared-secret>
```

Point the runtime-side gateway client at it:

```bash
export CODEX_TOOL_GATEWAY_URL=http://127.0.0.1:8090/tools/dispatch
export CODEX_TOOL_GATEWAY_BEARER_TOKEN=<shared-secret>
export CODEX_TOOL_GATEWAY_COMPANY_ID=acme
export CODEX_TOOL_GATEWAY_PROJECT_ID=project-alpha
```

The runtime still emits tool intents through `ToolGateway`; it does not build
Docker commands. The Docker gateway backend owns Docker command construction,
workspace mounting, permission/resource flags, timeout handling, and the
company-container lifecycle. A gateway instance keeps one persistent container
per company id, named from `CODEX_DOCKER_GATEWAY_CONTAINER_PREFIX` plus the
sanitized company id, and executes tool calls through `docker exec` instead of
creating a fresh per-call container. If the company container is stopped, the
backend restarts it; if it does not exist, the backend creates it once with a
long-lived `sleep infinity` entrypoint.

The mounted workspace root is shared by the company container. The backend
creates `/workspace/public` for shared material, `/workspace/agents/<agent_id>/private`
for agent-private material, and `/workspace/projects/<project_id>` for project
sub-workspaces. Switching `CODEX_TOOL_GATEWAY_PROJECT_ID` changes the default
`docker exec -w` directory without rebuilding or replacing the company
container.

## Dependencies

Note that `codex-core` makes some assumptions about certain helper utilities being available in the environment. Currently, this support matrix is:

### macOS

Expects `/usr/bin/sandbox-exec` to be present.

When using the workspace-write sandbox policy, the Seatbelt profile allows
writes under the configured writable roots while keeping `.git` (directory or
pointer file), the resolved `gitdir:` target, and `.codex` read-only.

Network access and filesystem read/write roots are controlled by
`SandboxPolicy`. Seatbelt consumes the resolved policy and enforces it.

Seatbelt also supports macOS permission-profile extensions layered on top of
`SandboxPolicy`:

- no extension profile provided:
  keeps legacy default preferences read access (`user-preference-read`).
- extension profile provided with no `macos_preferences` grant:
  does not add preferences access clauses.
- `macos_preferences = "readonly"`:
  enables cfprefs read clauses and `user-preference-read`.
- `macos_preferences = "readwrite"`:
  includes readonly clauses plus `user-preference-write` and cfprefs shm write
  clauses.
- `macos_automation = true`:
  enables broad Apple Events send permissions.
- `macos_automation = ["com.apple.Notes", ...]`:
  enables Apple Events send only to listed bundle IDs.
- `macos_accessibility = true`:
  enables `com.apple.axserver` mach lookup.
- `macos_calendar = true`:
  enables `com.apple.CalendarAgent` mach lookup.

### Linux

Expects the binary containing `codex-core` to run the equivalent of `codex sandbox linux` (legacy alias: `codex debug landlock`) when `arg0` is `codex-linux-sandbox`. See the `codex-arg0` crate for details.

### All Platforms

Expects the binary containing `codex-core` to simulate the virtual `apply_patch` CLI when `arg1` is `--codex-run-as-apply-patch`. See the `codex-arg0` crate for details.
