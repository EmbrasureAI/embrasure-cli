<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/embrasure-lockup-ivory.svg">
    <img src="assets/embrasure-lockup-ink.svg" alt="Embrasure" width="300">
  </picture>
</p>

<p align="center"><sub><em>“An embrasure is an opening in a fortified wall, built to see and respond without giving up protection.”</em></sub></p>

<p align="center">
  <a href="https://github.com/EmbrasureAI/embrasure-cli/actions/workflows/github-code-scanning/codeql"><img src="https://github.com/EmbrasureAI/embrasure-cli/actions/workflows/github-code-scanning/codeql/badge.svg" alt="CodeQL"></a>
</p>

<p align="center"><strong>Catch unexpected data changes before a dbt PR is reviewed.</strong></p>

Embrasure is open-source, local dbt PR validation for Snowflake:

- Builds changed models and critical downstream paths in temporary schemas, then cleans them up.
- Runs dbt tests and compares schema, row counts, null rates, cardinality, ranges, and primary keys with production.
- Shows affected downstream models and columns.
- Connects directly to Snowflake, with no Embrasure account or data sent to Embrasure.
- Includes a [`verify`](.agents/skills/verify/SKILL.md) skill that runs the agent check-and-fix loop.

`embrasure auth login` uses Snowflake OAuth. Your role needs warehouse access, production read access, and permission to create and remove temporary schemas.

## Quickstart

From an existing Snowflake dbt Core project where `dbt` already runs:

```sh
brew install embrasureai/tap/embrasure
embrasure init
embrasure auth login
embrasure doctor
embrasure check --dry-run
embrasure check
```

By default, `check` compares your branch with `origin/main`.

<details>
<summary>If dbt is not available</summary>

Embrasure uses the dbt Core and Snowflake adapter versions already installed by your project. Activate the project's normal environment, or set `dbt.command` in `embrasure-check.yml` to the wrapper your project uses.

</details>

If you do not use Homebrew, use the installer:

```sh
curl -fsSL https://raw.githubusercontent.com/EmbrasureAI/embrasure-cli/main/install.sh | sh
```

Use Embrasure from an existing Snowflake dbt project whose unchanged production models are already materialized. Embrasure uses those existing relations as the comparison baseline.

`init` reads the active dbt profile and asks only for missing values. Continue when `embrasure doctor` reports `READY`.

Example result:

```text
✓ Safe to continue

The change passed across every selected dbt model.

Scope
  5 affected models
  2 selected for validation

Evidence
  2 / 2 models built · 2 compared with production
  151,615,312 candidate rows evaluated
  Schema, row counts, nulls, cardinality, and distributions checked
  1 primary key checked
  0 findings · 3 unvalidated models
  Temporary Snowflake schema removed

Lineage impact
  fct_orders
  └─ finance_daily
     ├─ executive_revenue
     ├─ regional_margin
     └─ revenue_forecast_input

Completed in 2m11s
```

In an interactive terminal, `check` shows live progress while it runs. Redirected output, CI, and `--json` stay plain.

## Focus and preview

The default validates changed models and every path to a critical model. Critical targets are tagged `critical`, configured with `critical: true`, or used directly by a dbt exposure. Use `--downstream all` for every downstream model or `--downstream none` for changed models only. Impact is always computed from the full changed set.

Intersect the changed set with one or more explicit models:

```sh
embrasure check --select orders --select order_items
embrasure check --select orders --downstream none
```

An unknown, ambiguous, unchanged, or out-of-scope selection fails instead of returning a misleading pass.

Preview the plan without creating schemas or querying warehouse data:

```sh
embrasure check --dry-run
embrasure check --dry-run --json
```

Dry runs use local dbt parsing but do not resolve credentials, create Snowflake schemas, or query warehouse data.

## Reports and exit codes

JSON output is versioned and stably ordered. Progress goes to stderr, so stdout contains one JSON document.

```sh
embrasure check --json
embrasure check --json --markdown embrasure-check.md
embrasure check --json --report-version 1
```

Published contracts: [v1](schemas/report-v1.schema.json), [v2](schemas/report-v2.schema.json), [v3](schemas/report-v3.schema.json), and [v4](schemas/report-v4.schema.json). V4 adds column lineage and is the default; older versions remain available with `--report-version`.

| Exit code | Meaning |
|---:|---|
| `0` | The check passed |
| `1` | The data or code needs a fix |
| `2` | A requested check could not be completed |
| `3` | Setup, execution, or cleanup failed |

Agent loop:

```text
Run `embrasure check --base origin/main --json`.
Exit 1: fix every finding and rerun.
Exit 2: resolve or explain every coverage gap.
Exit 3: fix the setup or execution failure.
Request review only after exit 0.
```

For a faster first pass on large tables, add `--mode quick`. Quick mode estimates cardinality and skips percentiles. Deep mode is the default. Primary-key integrity stays exact in both modes.

### Arbitrary SQL checks

Query-diff checks compare any two read-only query results exactly. `production_sql` defaults to `sql`, and each dbt `ref()` is rendered against the candidate or production-state manifest. Checks with no refs, or definitions changed since `--base`, run even when no model changed.

```yaml
checks:
  - type: query_diff
    name: paid order totals
    sql: |
      select customer_id, sum(amount) as paid_amount
      from {{ ref('orders') }}
      where status = 'paid'
      group by customer_id
    primary_key: [customer_id]
```

With a primary key, Embrasure reports added, removed, and changed rows plus per-column mismatch counts. Null or duplicate keys block the value join. Without a key, grouped rows and their multiplicities preserve duplicate-only differences. Query examples are bounded by the configured sample, column, and value limits.

Only persisted dbt models are supported in `ref()`. Query checks accept one `SELECT`, `WITH`, or `VALUES` expression; other Jinja and multi-statement SQL are rejected. Removed checks are reported as incomplete coverage instead of silently passing.

## GitHub Actions

The composite action installs Embrasure. Install dbt with your project's normal locked setup; this example uses `requirements.txt`. Keep the check in a visible `run` step so exit codes and secrets remain explicit.

In `embrasure-check.yml`, configure the account to read the CI secret:

```yaml
auth:
  type: programmatic_access_token
  token_env: SNOWFLAKE_PROGRAMMATIC_ACCESS_TOKEN
```

```yaml
jobs:
  embrasure:
    runs-on: ubuntu-24.04
    permissions:
      contents: read
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - uses: actions/setup-python@v6
        with:
          python-version: "3.12"
      - run: python3 -m pip install -r requirements.txt
      - uses: EmbrasureAI/embrasure-cli@v1
      - run: embrasure check --base origin/main --json
        env:
          SNOWFLAKE_PROGRAMMATIC_ACCESS_TOKEN: ${{ secrets.SNOWFLAKE_PROGRAMMATIC_ACCESS_TOKEN }}
```

`fetch-depth: 0` is required because selection compares the working tree with the base revision.

## Maintenance

List managed temporary schemas older than six hours:

```sh
embrasure clean
embrasure clean --older-than 24 --yes
```

`clean` searches only configured account databases and verifies the prefix and ownership marker before removal.

Check for or install an update:

```sh
embrasure update --check
embrasure update
```

Generate shell completion scripts:

```sh
embrasure completion bash
embrasure completion zsh
embrasure completion fish
```

## Troubleshooting

### Generated schema is outside the run namespace

Your `generate_schema_name` macro must preserve the complete target schema. Make custom schemas children of `target.schema`; do not replace it.

### Incremental relation cannot be cloned

Snowflake zero-copy `CREATE TABLE ... CLONE` supports tables, not every relation type. Use `--incremental-mode full-refresh` when a full rebuild is acceptable, or exclude the model from this validation path.

### Incremental candidate seeding fails

The validation role needs `SELECT` on the production source and `CREATE TABLE` in the target schema. Run `embrasure doctor`. If the relation should not use clone mode, rerun with `--incremental-mode full-refresh`.

## Configuration and safety

Use `--config <path>` before or after any subcommand to choose another config file.

See the [example configuration](embrasure-check.example.yml) and [enterprise setup guide](docs/enterprise.md) for service credentials, multiple accounts, model policies, filters, thresholds, concurrency, external changes, cross-account dependencies, Metabase, and grants.

Every temporary schema has a unique name and ownership marker. Query results are materialized in a dedicated run-owned schema so they cannot collide with dbt model aliases. Embrasure checks ownership before removal and treats cleanup failures as execution failures. Use a dedicated role that can read and clone only the production tables under test and create temporary schemas in the required databases. SQL validation is not a side-effect sandbox, so the role must not be able to call unsafe procedures, functions, or external integrations.

[Security and data flow](docs/security-and-data-flow.md) documents network connections, local files, returned data, credentials, cleanup, updates, and release verification.

## Current limits

- Snowflake is the only supported warehouse.
- Column lineage covers compiled dbt SQL that SQLGlot can resolve. Wildcards without an input schema and dynamic SQL are reported as unresolved.
- Dashboard column lineage is not inferred from model lineage.
- Metabase matching covers native SQL cards that reference fully qualified production relations. Unsupported or inaccessible metadata becomes a coverage gap.

## Development

Rust 1.88 or newer is required when building from source.

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --locked
```

The opt-in Snowflake suite uses `EMBRASURE_RUN_SNOWFLAKE_TESTS=1` and the `EMBRASURE_TEST_SNOWFLAKE_*` account, user, role, database, warehouse, and token variables. It covers exact keyed passes and changes, duplicate-only unkeyed differences, incremental cloning, cleanup, and 100,000 synthetic rows.

Contributions are welcome under the [Apache 2.0 license](LICENSE).
