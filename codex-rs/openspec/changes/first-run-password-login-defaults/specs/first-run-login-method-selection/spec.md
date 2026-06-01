## ADDED Requirements

### Requirement: First-run login mode chooser
The system SHALL display a first-run login chooser that offers exactly two modes: manual configuration and password bootstrap.

#### Scenario: Chooser is shown for unconfigured users
- **WHEN** the application starts without a usable persisted login configuration
- **THEN** the first-run screen shows both manual configuration and password bootstrap options

#### Scenario: User selects manual configuration mode
- **WHEN** the user chooses manual configuration from the chooser
- **THEN** the system shows editable fields for `base_url`, `api_key`, and `model_id`

#### Scenario: User selects password bootstrap mode
- **WHEN** the user chooses password bootstrap from the chooser
- **THEN** the system shows a password input flow and does not require manual config fields before password validation

### Requirement: Mode switching remains user-controlled before submit
The system SHALL allow the user to switch between the two first-run modes before completing login.

#### Scenario: User returns from password mode to manual mode
- **WHEN** the user is on the password bootstrap form and navigates back to mode selection
- **THEN** the chooser is shown again and the user can continue with manual configuration
