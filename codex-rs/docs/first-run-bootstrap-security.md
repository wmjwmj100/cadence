# First-Run Bootstrap Credential Security Notes

The first-run password bootstrap flow is a convenience path, not a strong secret-protection boundary.

## Reverse-engineering risk

- The client uses layered protections (derived verifier checks, constant-time compare paths, and UI redaction), but a determined reverse engineer can still extract logic or recover runtime secrets.
- Treat bootstrap credentials as recoverable by an attacker with binary access.

## Operational guidance

- Rotate bootstrap credentials and default API credentials on a regular schedule.
- Prefer loading defaults from local protected files rather than embedding plaintext credential values in source paths.
- Keep first-run failures recoverable so operators can switch to manual entry without exposing secrets in logs or UI.
- Monitor authentication failures and rotate immediately if bootstrap credentials are suspected to be shared.
