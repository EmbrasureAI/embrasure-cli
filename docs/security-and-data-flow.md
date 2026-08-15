# Security and data flow

Embrasure runs on your machine or runner. It does not require an Embrasure account, hosted service, API key, or network connection to Embrasure.

## Data flow

```text
Local Git repository + local dbt + Embrasure CLI
                         |
                         | SQL and authentication
                         v
                 Your Snowflake account
                         |
                         | Aggregate metrics and optional key examples
                         v
                 Local terminal or report

Optional: Embrasure reads metadata from your configured Metabase URL.
```

Validation SQL runs in Snowflake. Embrasure does not upload warehouse data, dbt artifacts, reports, credentials, or usage information to Embrasure.

## Outbound connections

The CLI itself makes only these connections:

- `https://<account>.snowflakecomputing.com` for browser authentication and Snowflake SQL API requests.
- The configured Metabase URL when the optional Metabase integration is enabled.

There is no telemetry, update check, analytics SDK, or call to `embrasure.ai`. The local `dbt` process may connect to Snowflake or download packages according to the dbt project's own configuration. Installing a release may connect to GitHub or Homebrew; normal validation does not.

## Data returned to the local process

Snowflake returns the comparison evidence used in the local report:

- Database, schema, table, and column names
- Snowflake types
- Row counts, null rates, cardinality, min/max values, averages, and percentiles
- Counts of primary-key values found on only one side
- Optional primary-key examples, up to `safety.primary_key_sample_limit`

Set `primary_key_sample_limit: 0` when key values must not appear in process memory or JSON output. Reports are written to stdout unless `--markdown <path>` is provided.

## Credentials and local files

- The configuration file contains Snowflake identifiers and environment-variable names, not secret values.
- Programmatic access tokens and external OAuth tokens are read from environment variables.
- RSA private keys are read from the configured local path.
- Browser OAuth sessions are cached under `~/.config/embrasure-check/oauth/` by default. Files and directories use owner-only permissions on Unix. Run `embrasure auth logout` to remove a cached session.
- Temporary dbt profiles, manifests, target directories, and the detached base worktree live under an operating-system temporary directory and are removed after the process exits normally.

Credentials are not included in normal terminal output, JSON reports, or Markdown reports.

## Snowflake permissions

Use a dedicated role and warehouse. The role needs:

- `USAGE` on the validation warehouse and database
- Read access only to the production relations under test
- Permission to create and drop temporary schemas in the configured validation database

The [enterprise setup guide](enterprise.md) contains example grants. `embrasure doctor --read-only` checks authentication and production visibility without creating a schema. A normal check must create and remove its validation schema.

## Schema cleanup

Each run uses a unique schema name and writes an ownership marker to that schema. Cleanup requires both the expected name prefix and matching marker. The CLI refuses to drop a schema that fails either check.

Cleanup is attempted after success, findings, execution failures, Ctrl-C, and normal termination signals. No process can clean up after `SIGKILL`, a machine crash, or power loss. Administrators should periodically remove stale schemas with the configured prefix after confirming their ownership markers.

## Release integrity

Releases include native archives for macOS and Linux on Intel and ARM, a `SHA256SUMS` file, and an SPDX JSON software bill of materials.

Verify an archive checksum:

```sh
archive=embrasure-0.3.2-aarch64-apple-darwin.tar.gz
grep " ${archive}$" SHA256SUMS | shasum -a 256 --check
```

Verify its GitHub-signed build provenance:

```sh
gh attestation verify embrasure-*.tar.gz --repo EmbrasureAI/embrasure-cli
```

Release attestations use short-lived Sigstore-backed identities issued to the GitHub Actions release workflow. The source commit, workflow, and artifact digest are included in the verification result.

## Vulnerability reporting

Do not open a public issue for a suspected vulnerability or credential exposure. Use GitHub's private vulnerability reporting from the repository's Security tab.
