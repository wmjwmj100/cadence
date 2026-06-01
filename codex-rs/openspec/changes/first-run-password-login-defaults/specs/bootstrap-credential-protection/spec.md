## ADDED Requirements

### Requirement: Secret redaction in user-visible and test-visible outputs
The system MUST prevent plaintext bootstrap credentials and injected API keys from appearing in UI text, logs, telemetry payloads, and snapshot fixtures.

#### Scenario: Authentication failure message
- **WHEN** bootstrap authentication fails
- **THEN** the error text does not echo the submitted password or any API key value

#### Scenario: Configuration success output
- **WHEN** bootstrap configuration succeeds
- **THEN** confirmation output indicates success without printing raw credential values

### Requirement: No direct plaintext API key literals in source-controlled first-run logic
The system MUST avoid storing bootstrap API key defaults as direct plaintext string literals in first-run source code.

#### Scenario: Build artifact generated from source
- **WHEN** the application is built with bootstrap support enabled
- **THEN** first-run source code paths do not contain a direct plaintext API key constant

### Requirement: Explicit reverse-engineering risk disclosure
The system SHALL document that client-side defenses reduce accidental disclosure but do not guarantee secrecy against a determined reverse engineer.

#### Scenario: Security documentation review
- **WHEN** a developer reviews the first-run bootstrap security notes
- **THEN** the documentation states that binary extraction risk remains and credentials must be rotatable
