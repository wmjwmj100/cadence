## 1. First-run flow scaffolding

- [x] 1.1 Locate current first-run login entry points and add a mode-selection state (`choose`, `manual`, `password`) without regressing existing manual startup.
- [x] 1.2 Implement a first-run chooser view that presents manual configuration and password bootstrap as the only two options.
- [x] 1.3 Add navigation so users can switch between chooser/manual/password flows before submit.

## 2. Manual and bootstrap input handling

- [x] 2.1 Keep manual mode editing for `base_url`, `api_key`, and `model_id` wired to existing validation and submit paths.
- [x] 2.2 Add password bootstrap form state, submission handling, and user-facing error messaging for failed authentication.
- [x] 2.3 Ensure bootstrap failure paths stay inside first-run flow and allow returning to manual mode.

## 3. Bootstrap validation and default injection

- [x] 3.1 Implement bootstrap password verification for `426341` using a derived verifier and constant-time comparison path.
- [x] 3.2 Add a credential provider that reads default values from `C:\Users\wmj20\.codex\config.toml` and `C:\Users\wmj20\.codex\auth.json`.
- [x] 3.3 Define deterministic precedence for `model_id` and fail with recoverable errors when required defaults are missing/unreadable.
- [x] 3.4 Persist bootstrap-injected values through the same config/auth writer path used by manual first-run save.

## 4. Credential protection and documentation

- [x] 4.1 Audit first-run UI/log/telemetry paths to ensure passwords and API keys are always redacted.
- [x] 4.2 Remove or avoid direct plaintext API key literals in first-run source-controlled code paths.
- [x] 4.3 Update docs with reverse-engineering risk disclosure and operational guidance for rotating bootstrap credentials.

## 5. Verification coverage

- [x] 5.1 Add/extend unit tests for first-run mode transitions and bootstrap password success/failure behavior.
- [x] 5.2 Add tests for default-source resolution, missing-file fallback behavior, and persistence through standard writers.
- [x] 5.3 Add/update TUI snapshot tests for chooser, password form, failure state, and success confirmation text with redaction.
