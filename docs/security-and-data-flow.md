# Security and data flow

By default, Embrasure runs on your machine or runner. Local `embrasure check` does not require an Embrasure account, hosted service, API key, or network connection to Embrasure. Cloud handoff is a separate, explicit opt-in through `embrasure check --cloud`.

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

Optional and explicit: `embrasure check --cloud` sends a bounded source snapshot and local review evidence to Embrasure Cloud. The CLI prints every included path and the total size before upload.
```

Local validation SQL runs in Snowflake. Without `--cloud`, Embrasure does not upload warehouse data, dbt artifacts, reports, credentials, or usage information to Embrasure.

With `--cloud`, the CLI sends only:

- GitHub repository owner and name, dbt subdirectory, base reference, and commit hashes
- Eligible changed dbt text files, their paths, statuses, and SHA-256 hashes
- The local review report and selected lineage evidence
- The business intent supplied through `--context` or `--context-file`
- CLI version and operating-system name
- `notify_slack: true`, which asks the service to send its configured Slack notification

Cloud handoff never sends Snowflake credentials, Embrasure access or refresh tokens, `profiles.yml`, `.env*`, private keys, binaries, symlinks, `target`, `logs`, `dbt_packages`, or virtual environments. Before upload, a nine-substring denylist rejects common credential patterns. The Cloud API contract also requires the server to repeat path, size, encoding, hash, and secret checks.

## Outbound connections

The CLI itself makes only these connections:

- `https://<account>.snowflakecomputing.com` for browser authentication and Snowflake SQL API requests.
- The configured Metabase URL when the optional Metabase integration is enabled.
- `https://app.embrasure.ai` and `https://api.embrasure.ai` only after `embrasure cloud login` or an explicit `embrasure check --cloud`. Development can override the API with `EMBRASURE_API_URL` and the browser origin with `EMBRASURE_WEB_URL`.
- `https://api.github.com/repos/EmbrasureAI/embrasure-cli/releases/latest` for `embrasure update --check`, `embrasure update`, and the gated doctor notice.
- `https://github.com/EmbrasureAI/embrasure-cli/releases/download/...` when `embrasure update` downloads a release and its checksums.

There is no telemetry or analytics SDK. `embrasure update` is opt-in. Human, interactive `embrasure doctor` output may check for a release at most once every 24 hours. The notice is disabled for JSON output, piped stderr, `CI`, or `NO_UPDATE_NOTIFIER`, and network failures are silent. The local `dbt` process may connect to Snowflake or download packages according to the dbt project's own configuration. Normal local validation does not contact Embrasure.

## Data returned to the local process

Snowflake returns the comparison evidence used in the local report:

- Database, schema, table, and column names
- Snowflake types
- Row counts, null rates, cardinality, min/max values, averages, and percentiles
- Counts of primary-key values found on only one side, duplicate rows, and null-key rows
- Optional stably ordered primary-key and duplicate-key examples, up to `safety.primary_key_sample_limit`

Set `primary_key_sample_limit: 0` when key values must not appear in process memory or JSON output. Reports are written to stdout unless `--markdown <path>` is provided.

## Credentials and local files

- The configuration file contains Snowflake identifiers and environment-variable names, not secret values.
- Programmatic access tokens and external OAuth tokens are read from environment variables.
- RSA private keys are read from the configured local path.
- Browser OAuth sessions are cached under `~/.config/embrasure-check/oauth/` by default. Files and directories use owner-only permissions on Unix. Run `embrasure auth logout` to remove a cached session.
- Embrasure Cloud access and refresh tokens use the OS credential store under the separate `ai.embrasure.cli.cloud` service. Run `embrasure cloud logout` to revoke and remove that session. They are never stored in the local handoff receipt.
- The OS application cache stores the last local-review fingerprint and report so an unchanged review can be reused. The handoff receipt stores only run IDs, URLs, hashes, and timestamps. Neither file stores uploaded source content or credentials.
- Temporary dbt profiles, manifests, target directories, and the detached base worktree live under an operating-system temporary directory and are removed after the process exits normally.

Credentials are not included in normal terminal output, JSON reports, or Markdown reports.

## Snowflake permissions

Use a dedicated role and warehouse. The role needs:

- `USAGE` on the validation warehouse and database
- Read access only to the production relations under test
- `SELECT` on production tables that must be zero-copy cloned
- Permission to create temporary run-owned schemas in every database containing selected models

The [enterprise setup guide](enterprise.md) contains example grants. `embrasure doctor --read-only` checks authentication and production visibility without creating a schema. A normal `doctor` run also proves that a visible production table can be cloned. A normal check creates and removes candidate and baseline schemas.

Incremental baselines use Snowflake table-level zero-copy clones. Embrasure never seeds them with CTAS, `INSERT`, or a local data copy. Clone metadata remains in Snowflake; only aggregate comparison results and bounded examples return to the local process.

## Schema cleanup

Each run uses unique candidate and baseline schema names and writes an ownership marker to every schema. Cleanup requires both the exact run namespace and matching marker. The CLI refuses to drop a schema that fails either check.

Cleanup is attempted after success, findings, execution failures, Ctrl-C, and normal termination signals. No process can clean up after `SIGKILL`, a machine crash, or power loss. `embrasure clean` lists old marked schemas by default and removes them only with `--yes`. It searches only each configured account database and verifies both the configured prefix and an Embrasure ownership marker before removal.

## Release integrity

Releases include native archives for macOS and Linux on Intel and ARM, a `SHA256SUMS` file, and one source-tree SPDX JSON software bill of materials bound to each archive by attestation.

Verify an archive checksum:

```sh
archive=embrasure-<version>-aarch64-apple-darwin.tar.gz
grep " ${archive}$" SHA256SUMS | shasum -a 256 --check
```

Verify its GitHub-signed build provenance:

```sh
gh attestation verify embrasure-*.tar.gz --repo EmbrasureAI/embrasure-cli
```

Release attestations use short-lived Sigstore-backed identities issued to the GitHub Actions release workflow. The source commit, workflow, and artifact digest are included in the verification result.

## Vulnerability reporting

Do not open a public issue for a suspected vulnerability or credential exposure. Use GitHub's private vulnerability reporting from the repository's Security tab.
