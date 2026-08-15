# Embrasure

Embrasure is an open-source Rust CLI that lets an agent build a dbt change in an isolated Snowflake schema, compare it with production data, understand downstream impact, fix problems, and rerun until the report is clean.

It talks directly to Snowflake's SQL API. It does not require Embrasure Cloud, a UI, or MCP.

```console
$ embrasure check --base origin/main --json
{
  "schema_version": 1,
  "status": "pass",
  "exit_code": 0,
  "summary": { "models_selected": 2, "models_compared": 2, "findings": 0, "coverage_gaps": 0 },
  ...
}
```

## What it checks

- Finds modified dbt models and downstream dependents with dbt state selection.
- Supports mappings for changes that originate outside dbt.
- Builds into a uniquely named transient CI schema with production deferral.
- Enforces a dedicated warehouse, query tags, timeouts, and a maximum model count.
- Compares columns, precise Snowflake types, exact row counts, null rates, cardinality, min/max ranges, numeric averages, and p05/p50/p95 distributions.
- Compares primary-key values when configured.
- Runs the selected models' existing dbt tests through `dbt build`.
- Reports dbt descendants, exposures, declared cross-account dependencies, and optional Metabase dashboards.
- Labels unavailable column lineage or BI evidence as `unknown` instead of guessing.
- Drops every CI schema it creates after pass, finding, incomplete, or execution-failure outcomes; a cleanup problem is itself an execution failure.

## Exit codes

| Code | Meaning |
|---:|---|
| `0` | Pass |
| `1` | Findings need a code or data fix |
| `2` | Incomplete coverage; at least one requested check is unknown |
| `3` | Execution failed |

Argument parsing errors use clap's conventional exit code `2` before a report can be created.

## Requirements

- Rust 1.85+ to build the CLI.
- Git and dbt Core with the Snowflake adapter on `PATH` to run a check.
- A dbt Snowflake user with permission to use the configured warehouse/database, create and drop schemas, and read production relations.
- A supported Snowflake credential. Credentials are never written to reports or normal terminal output.

## Install

```sh
cargo install --git https://github.com/EmbrasureAI/embrasure-cli --tag v0.3.0 --locked
```

## Configure

Copy [`embrasure-check.example.yml`](embrasure-check.example.yml) to `embrasure-check.yml` in the dbt repository. The CLI generates a temporary `profiles.yml`; existing user profiles are not changed.

The configuration filename and local credential-cache path retain the `embrasure-check` name so existing installations continue to work. The previous `run` subcommand also remains available as an alias for `check`.

For a local developer or coding agent, use Snowflake browser login:

```sh
embrasure auth login
embrasure doctor
```

This uses Snowflake's [built-in local OAuth application](https://docs.snowflake.com/en/user-guide/oauth-local-applications) and PKCE. The short-lived access token and refresh token are stored under `~/.config/embrasure-check/oauth/` with owner-only permissions on Unix. Use `auth status` to inspect readiness and `auth logout` to remove it.

For CI, use a role-restricted [programmatic access token](https://docs.snowflake.com/en/user-guide/programmatic-access-tokens):

```yaml
auth:
  type: programmatic_access_token
  token_env: SNOWFLAKE_PROGRAMMATIC_ACCESS_TOKEN
```

RSA key-pair auth is also supported with `private_key_path` and optional `passphrase_env`; both PKCS#1 and PKCS#8 PEM keys work. Existing external OAuth pipelines can use `type: oauth` plus `token_env`. The selected credential is reused by dbt and the Snowflake SQL API.

With two Snowflake accounts, add two `accounts` entries and give each a dbt `selector` so a model is built only in its owning account.

See the [enterprise setup guide](docs/enterprise.md) for least-privilege Snowflake grants, service-user auth, multiple accounts, and Metabase.

## Verify setup

Run this before the first validation or after changing credentials:

```sh
embrasure auth status
embrasure doctor
embrasure doctor --json
```

`doctor` connects to every configured Snowflake account, confirms the requested role/warehouse/database, checks production read access, creates and removes a uniquely named temporary schema, and authenticates to Metabase when configured. Use `--read-only` when an administrator wants a non-mutating diagnostic, although that cannot prove CI schema permissions.

## Run

```sh
embrasure check --base origin/main
embrasure check --base origin/main --json
embrasure check --base origin/main --markdown embrasure-check.md
```

If `dbt.state_dir` is omitted, the CLI creates a temporary detached Git worktree at `--base`, runs `dbt deps` when needed, and parses a production-state manifest for every account target. If CI already downloads production artifacts, point `dbt.state_dir` at the directory containing `manifest.json`. For target-specific artifacts, place them at `<state_dir>/<account name>/manifest.json`.

The JSON schema is versioned and arrays are sorted before serialization. The v1 contract is checked in at [`schemas/report-v1.schema.json`](schemas/report-v1.schema.json). Progress is written to stderr; `--json` reserves stdout for exactly one JSON document.

Deleted models are findings by default and their base-revision descendants and exposures are still reported. After confirming an intentional deletion and its migration, acknowledge it explicitly with `models.<unique_id>.allow_removal: true`; downstream models are still rebuilt when they remain in the current project.

An agent can use this loop:

```text
Make the dbt change. Run `embrasure check --base origin/main --json`.
For exit 1, fix every finding and rerun. For exit 2, explain and close every
unknown coverage gap. For exit 3, fix the execution problem. Do not request
review until the command exits 0.
```

## Safety model

The generated schema name contains the configured prefix, commit, timestamp, and random suffix. Cleanup requires both the exact per-run schema namespace and a matching ownership marker stored on the schema. Custom `generate_schema_name` macros are supported when they retain the complete target schema; the run stops before building if they do not. SQL API requests carry a query tag and explicit timeout; the generated dbt profile applies equivalent session parameters. A run also stops before building if selection exceeds `safety.max_models`.

Cleanup is attempted after every handled outcome, including Ctrl-C and normal CI termination signals. A cleanup failure changes the result to execution failure and prints the exact schema that an operator must remove. No process can clean up after `SIGKILL`, power loss, or a machine crash, so operators should also expire stale schemas by prefix as a backstop.

Snowflake protocol details follow the official [SQL API reference](https://docs.snowflake.com/en/developer-guide/sql-api/reference) and [authentication guide](https://docs.snowflake.com/en/developer-guide/sql-api/authenticating).

## Current lineage boundary

dbt manifest node lineage and exposures are authoritative. Column-level lineage is reported as unknown unless a declared cross-account dependency supplies column evidence. Metabase matching is deliberately conservative: native SQL cards are matched to fully qualified production relations, and dashboards containing those cards are reported. MBQL or inaccessible metadata becomes an explicit coverage gap.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

Contributions are welcome under the Apache-2.0 license.
