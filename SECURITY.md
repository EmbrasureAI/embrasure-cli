# Security

Please do not open a public issue for a vulnerability or possible credential exposure. Report it through GitHub's private vulnerability reporting for this repository.

Embrasure reads production data. Use a dedicated least-privilege Snowflake role and warehouse. The role should be able to read only the production relations under test and create/drop schemas only in the configured CI database.

See [Security and data flow](docs/security-and-data-flow.md) for runtime connections, local storage, data handling, cleanup guarantees, and release verification.
