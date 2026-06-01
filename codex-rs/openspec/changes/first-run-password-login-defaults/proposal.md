## Why

First-run setup currently forces users to manually enter endpoint and credential fields, which slows onboarding for controlled deployments. We need a dual-login flow that keeps manual configuration available while adding a password bootstrap path with clear security requirements for handling default credentials.

## What Changes

- Add a first-run login chooser with two modes: manual entry (`base_url`, `api_key`, `model_id`) and password bootstrap.
- Add password bootstrap login using default password `426341`; on success, inject default connection values sourced from the local Codex config/auth files (`C:\Users\wmj20\.codex\config.toml` and `C:\Users\wmj20\.codex\auth.json`).
- Ensure injected values are persisted through existing configuration mechanisms so the app can continue with normal auth behavior after first-run setup.
- Define security behavior for default credentials: do not store plaintext API keys directly in code literals or expose them in logs/UI; require defense-in-depth protections and document residual reverse-engineering risk.

## Capabilities

### New Capabilities
- `first-run-login-method-selection`: Present and handle two first-run login paths (manual config vs password bootstrap).
- `password-bootstrap-default-injection`: Validate bootstrap password and populate runtime auth/config fields from predefined local sources.
- `bootstrap-credential-protection`: Specify how bootstrap-provided credentials are protected at rest/in memory and how secret exposure is minimized.

### Modified Capabilities
- None.

## Impact

- Affected code: first-run/login UI flow, config/auth loading path, and bootstrap credential injection logic.
- Affected tests: TUI snapshot coverage for the new chooser/password flow and core tests for bootstrap validation + config population.
- Affected docs: first-run login behavior and security notes in relevant docs.
- External dependencies/APIs: no new external API surface expected.
