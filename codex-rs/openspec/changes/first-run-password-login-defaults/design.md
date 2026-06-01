## Context

The first-run authentication flow currently expects users to manually provide connection settings before they can proceed. The proposal adds a dual-path login flow (manual config vs password bootstrap) and introduces stricter handling for bootstrap credentials so default values are usable without exposing secrets in UI, logs, or obvious static constants.

Primary constraints:
- Reuse existing config/auth persistence flows (`config.toml` + `auth.json`) rather than introducing a parallel storage system.
- Keep first-run UX simple while making security limits explicit.
- Acknowledge that client-side binaries cannot guarantee perfect secrecy against reverse engineering.

## Goals / Non-Goals

**Goals:**
- Add a first-run mode selector with two supported paths: manual entry and password bootstrap.
- Support bootstrap password validation (`426341`) and then inject default `base_url`, `api_key`, and `model_id` into the existing runtime/config pipeline.
- Prevent straightforward plaintext secret disclosure in source, logs, snapshots, and obvious binary strings.
- Preserve backward compatibility for users who already configure credentials manually.

**Non-Goals:**
- Replacing the long-term authentication/config model used after first run.
- Providing cryptographic guarantees that embedded client-side secrets are unrecoverable.
- Introducing new external auth services or server-side key escrow.

## Decisions

1. Introduce explicit first-run auth state with two modes.
   - Decision: model first-run as a small state machine (`choose mode` -> `manual form` or `password form` -> `persist + continue`).
   - Rationale: keeps flow deterministic and easy to snapshot-test.
   - Alternative considered: two independent screens with ad-hoc navigation; rejected because it increases duplicated validation and edge-case drift.

2. Validate bootstrap password via derived form, not a raw plaintext literal compare.
   - Decision: store a derived verifier for the default password and compare in constant time.
   - Rationale: avoids obvious plaintext password extraction and timing side channels.
   - Alternative considered: direct string constant compare; rejected due trivial static extraction.

3. Resolve default connection values through a credential provider abstraction.
   - Decision: add a provider that reads/parses canonical local config/auth files and returns normalized defaults for injection.
   - Rationale: aligns with existing user environment and avoids shipping plaintext API key literals.
   - Alternative considered: embedding raw default `api_key`/`base_url` constants in code; rejected because static extraction from binaries is feasible.

4. If product requirements mandate embedded fallback credentials, treat them as obfuscated fallback only.
   - Decision: support optional embedded fallback through split/encoded fragments reconstructed at runtime, with mandatory redaction and no direct string literals.
   - Rationale: lowers accidental exposure while documenting that this is not strong secrecy.
   - Alternative considered: refusing embedded fallback entirely; deferred pending requirement confirmation.

5. Persist injected values only through existing config writers.
   - Decision: after successful bootstrap, write values through the same code paths used by manual entry.
   - Rationale: one persistence path simplifies rollback/testing and avoids format drift.
   - Alternative considered: special-case bootstrap writes; rejected for maintainability risk.

## Risks / Trade-offs

- [Client binary reverse engineering can still recover embedded secrets] -> Prefer file-derived defaults; if embedded fallback is required, document residual risk and rotate credentials regularly.
- [Default password can be discovered/shared] -> Restrict usage to first-run bootstrap path, support immediate password or credential override, and log only high-level events.
- [Missing or malformed local config/auth files] -> Fail gracefully to manual path with actionable error text.
- [Added complexity in first-run UX] -> Keep mode selector minimal and add snapshot + behavior tests for each transition.

## Migration Plan

1. Add first-run mode-selection UI and routing without changing existing manual behavior.
2. Introduce bootstrap validation + credential provider abstraction behind first-run flow.
3. Wire injected values into existing persistence path and add redaction safeguards.
4. Add tests (state transitions, validation, and persistence) and TUI snapshots for all first-run variants.
5. Roll out with clear release notes describing security limits and override behavior.

## Open Questions

- Must `base_url` and `api_key` be truly embedded in the binary, or is runtime loading from local config/auth files acceptable for bootstrap defaults?
- Should bootstrap password login remain permanently available, or only until first successful configuration?
- Is `model_id` required in bootstrap defaults when absent from local config, and what should fallback precedence be?
