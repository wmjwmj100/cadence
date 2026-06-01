## ADDED Requirements

### Requirement: Bootstrap password validation
The system MUST validate the bootstrap password before injecting default connection settings, and only the configured bootstrap secret value `426341` is accepted for this flow.

#### Scenario: Correct bootstrap password
- **WHEN** the user submits password `426341` in password bootstrap mode
- **THEN** the system accepts the password and continues to default-value injection

#### Scenario: Incorrect bootstrap password
- **WHEN** the user submits any password other than `426341`
- **THEN** the system rejects the attempt, shows an authentication error, and keeps the user in the first-run login flow

### Requirement: Inject defaults from local Codex config sources
After successful bootstrap password validation, the system MUST resolve default `base_url` and `api_key` from `C:\Users\wmj20\.codex\config.toml` and `C:\Users\wmj20\.codex\auth.json`, and MUST populate `model_id` using configured default precedence.

#### Scenario: Local defaults are available
- **WHEN** bootstrap password validation succeeds and both local source files contain usable values
- **THEN** the system loads defaults from those files and prepares them as the active login configuration

#### Scenario: Local defaults are unavailable
- **WHEN** bootstrap password validation succeeds but required values are missing or unreadable from local sources
- **THEN** the system shows a recoverable setup error and returns the user to a manual configuration path

### Requirement: Persist injected defaults through standard configuration path
The system SHALL persist bootstrap-injected values using the same save path used by manual first-run configuration.

#### Scenario: Bootstrap values committed
- **WHEN** bootstrap defaults are resolved successfully
- **THEN** the system stores them via standard config/auth writers so the next app start uses the saved configuration
