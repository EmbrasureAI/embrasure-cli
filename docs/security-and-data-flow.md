# Security and data flow

Embrasure runs on your machine or runner. `embrasure check` does not require an Embrasure account, hosted service, API key, or network connection to Embrasure.

## Data flow

```text
Local Git repository + local dbt + Embrasure CLI
                         |
                         | SQL and authentication
                         v
              Your configured warehouse
                         |
                         | Aggregate metrics and optional key examples
                         v
Local terminal or report

Optional: Embrasure reads metadata from your configured Metabase URL.
```

Validation SQL runs in Snowflake, Databricks, or BigQuery. Embrasure does not upload warehouse data, dbt artifacts, reports, credentials, or usage information to Embrasure.

## Outbound connections

The CLI itself makes only these connections:

- `https://<account>.snowflakecomputing.com` for browser authentication and Snowflake SQL API requests.
- The configured Databricks workspace host for Statement Execution API requests.
- `https://bigquery.googleapis.com` for BigQuery jobs and temporary dataset lifecycle requests, plus the Google OAuth token or Google Cloud metadata endpoint selected by Application Default Credentials.
- The configured Metabase URL when the optional Metabase integration is enabled.
- `https://api.github.com/repos/EmbrasureAI/embrasure-cli/releases/latest` for `embrasure update --check`, `embrasure update`, and the gated doctor notice.
- `https://github.com/EmbrasureAI/embrasure-cli/releases/download/...` when `embrasure update` downloads a release and its checksums.

There is no telemetry or analytics SDK. `embrasure update` is opt-in. Human, interactive `embrasure doctor` output may check for a release at most once every 24 hours. The notice is disabled for JSON output, piped stderr, `CI`, or `NO_UPDATE_NOTIFIER`, and network failures are silent. The local `dbt` process may connect to the configured warehouse or download packages according to the dbt project's own configuration. Normal local validation does not contact Embrasure.

## Data returned to the local process

The configured warehouse returns the comparison evidence used in the local report:

- Database, schema, table, and column names
- Warehouse column types
- Row counts, null rates, cardinality, min/max values, averages, and percentiles
- Counts of primary-key values found on only one side, duplicate rows, and null-key rows
- Optional stably ordered primary-key and duplicate-key examples, up to `safety.primary_key_sample_limit`
- Optional deterministic query-diff row values, bounded by `safety.primary_key_sample_limit`, `safety.max_columns_per_model`, and `safety.max_example_value_chars`

Set `primary_key_sample_limit: 0` when key or query-diff example values must not appear in process memory or JSON output. Reports are written to stdout unless `--markdown <path>` is provided.

Query checks accept a single read-only query expression, but syntax validation is not a side-effect sandbox for functions invoked by that query. Use a least-privilege warehouse identity that cannot call unsafe procedures, user-defined functions, or external integrations. Query materializations use a dedicated run-owned schema and follow the same ownership-checked cleanup path as model schemas.

## Credentials and local files

- The configuration file contains warehouse identifiers and environment-variable names, not secret values.
- Programmatic access tokens and external OAuth tokens are read from environment variables.
- Databricks tokens are read from the configured environment variable and sent only to the configured workspace host and the temporary dbt profile.
- BigQuery uses Google Application Default Credentials. Depending on the environment, those credentials come from a service-account JSON file named by `GOOGLE_APPLICATION_CREDENTIALS`, the local ADC file created by gcloud, or an attached Google Cloud service account. Access tokens are sent only to Google APIs and are not written to Embrasure configuration or reports.
- RSA private keys are read from the configured local path.
- Browser OAuth sessions are cached under `~/.config/embrasure-check/oauth/` by default on Unix, with owner-only file and directory permissions. Windows sessions are encrypted for the current user with DPAPI and stored under `%APPDATA%\embrasure-check\oauth\`. Run `embrasure auth logout` to remove a cached session.
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

For Databricks, use a Unity Catalog SQL warehouse. The identity needs `USE CATALOG`, production schema/table read access, and permission to create schemas and tables in the configured catalog. Incremental baselines currently use managed Delta shallow clones; candidate seeding uses CTAS so it does not create an unsupported nested shallow clone.

For BigQuery, the identity needs permission to create query jobs, read production table data and metadata, create and delete temporary datasets, and create, update, and delete tables inside those datasets. Typical predefined roles are BigQuery Job User on the project plus Data Viewer on production datasets and Data Editor or Data Owner on the temporary-dataset project. Embrasure creates each temporary dataset with an explicit empty legacy access list so BigQuery does not add its default project-wide dataset ACL entries; inherited IAM still applies. Incremental baselines use writable table clones; candidate seeding uses table copies. BigQuery requires clone sources and destinations to share a location and organization, and recently streamed rows in write-optimized storage are not included in a clone.

## Schema cleanup

Each run uses unique candidate and baseline schema names and writes an ownership marker to every schema. Cleanup requires both the exact run namespace and matching marker. The CLI refuses to drop a schema that fails either check.

Snowflake schemas are dropped with `RESTRICT` instead of the Snowflake `CASCADE` default, so cleanup never removes foreign keys held by objects outside the schema. Databricks schemas are dropped with `CASCADE` only after both the run namespace and ownership marker are verified; the cascade is limited to objects inside that temporary Unity Catalog schema. BigQuery datasets are deleted with their contents only after the dataset name, managed label, and exact ownership description are verified.

Cleanup is attempted after success, findings, execution failures, Ctrl-C, and normal termination signals. No process can clean up after `SIGKILL`, a machine crash, or power loss. `embrasure clean` lists old marked schemas or datasets by default and removes them only with `--yes`. It searches only each configured account database or project and verifies both the configured prefix and an Embrasure ownership marker before removal.

## Release integrity

Releases include native archives for macOS and Linux on Intel and ARM, plus a portable ZIP and PowerShell installer for Windows x64. Every release includes `SHA256SUMS` and an SPDX JSON software bill of materials bound to the release artifacts by attestation.

Verify an archive checksum:

```sh
archive=embrasure-<version>-aarch64-apple-darwin.tar.gz
grep " ${archive}$" SHA256SUMS | shasum -a 256 --check
```

Verify its GitHub-signed build provenance:

```sh
gh attestation verify embrasure-*.tar.gz --repo EmbrasureAI/embrasure-cli
```

On Windows, WinGet and Scoop verify the ZIP against the hash in their reviewed package manifest. The direct `install.ps1` downloads only from the Embrasure GitHub repository, requires exactly one matching entry in `SHA256SUMS`, rejects unsafe or oversized ZIP contents, verifies the executable version and bundled SQLGlot wheel, then replaces the current-user installation with rollback on failure. `embrasure update` downloads and verifies the same ZIP, writes the exact installer script embedded in the running binary to a temporary directory, and launches it with a process-scoped `Bypass` policy. The helper revalidates the archive after the CLI exits before performing the same atomic replacement. It does not change the user's or machine's execution policy.

The Windows executable and PowerShell script are not currently Authenticode-signed. Checksums and GitHub attestations protect artifact integrity, but they do not establish a Windows publisher identity. Direct downloads may show Microsoft Defender SmartScreen warnings; use WinGet or Scoop when local policy blocks unsigned downloads.

Release attestations use short-lived Sigstore-backed identities issued to the GitHub Actions release workflow. The source commit, workflow, and artifact digest are included in the verification result.

## Vulnerability reporting

Do not open a public issue for a suspected vulnerability or credential exposure. Use GitHub's private vulnerability reporting from the repository's Security tab.
