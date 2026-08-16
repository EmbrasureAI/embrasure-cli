<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/embrasure-lockup-ivory.svg">
    <img src="assets/embrasure-lockup-ink.svg" alt="Embrasure" width="300">
  </picture>
</p>

<p align="center"><strong>Catch unexpected data changes before a dbt PR is reviewed.</strong></p>

Embrasure builds changed dbt models and paths to critical downstream models in temporary Snowflake schemas. It runs existing dbt tests, compares candidate data with production, reports downstream impact, and removes the schemas.

Local checks connect directly to Snowflake. They require no Embrasure account or hosted service.

## What it catches

- Columns added, removed, renamed, or changed to an incompatible type
- Shifts in row counts, null rates, cardinality, ranges, averages, and percentiles
- Primary-key values that appear or disappear
- Duplicate or null primary keys introduced by a branch
- Existing dbt test failures
- Affected dbt models and exposures
- Declared cross-account dependencies
- Optional Metabase dashboards
- Models mapped to non-dbt file changes

## Install

You need Git, dbt Core 1.5 or newer, and dbt-snowflake 1.5 or newer. Embrasure uses `dbt parse --target-path`, state selection, JSON `--output-keys`, and deferred builds. These interfaces are present in dbt Core 1.5. You do not need Rust.

```sh
python -m pip install "dbt-core>=1.5,<2" "dbt-snowflake>=1.5,<2"
```

Install Embrasure on macOS or Linux with Homebrew:

```sh
brew install embrasureai/tap/embrasure
```

Or use the verified installer:

```sh
curl -fsSL https://raw.githubusercontent.com/EmbrasureAI/embrasure-cli/main/install.sh | sh
```

The installer verifies `SHA256SUMS` and writes to `/usr/local/bin` when writable, otherwise `~/.local/bin`. Set `EMBRASURE_INSTALL_DIR` to choose another directory.

## First check

Run from the dbt project directory:

```sh
embrasure init
embrasure auth login
embrasure doctor
embrasure check --base origin/main
```

`init` reads the active dbt profile and asks only for missing values. Use `--config <path>` before or after any subcommand to choose another config file.

Example result:

```text
embrasure: PASS
2 selected · 2 built · 2 compared · 0 findings · 0 coverage gaps
5 impacted · 2 validated · 3 not validated
```

The default validates changed models and every path to a critical model. Critical targets are tagged `critical`, configured with `critical: true`, or used directly by a dbt exposure. Use `--downstream all` for every downstream model or `--downstream none` for changed models only. Impact is always computed from the full changed set.

## Focus and preview

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

Dry runs still resolve credentials and run local dbt parsing. `--dry-run` cannot be combined with `--cloud`.

## Reports and exit codes

JSON output is versioned and stably ordered. Progress goes to stderr, so stdout contains one JSON document.

```sh
embrasure check --json
embrasure check --json --markdown embrasure-check.md
embrasure check --json --report-version 1
```

Published contracts: [report v1](schemas/report-v1.schema.json) and [report v2](schemas/report-v2.schema.json).

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

## GitHub Actions

The composite action installs the CLI. Keep the check in a visible `run` step so exit codes and secrets remain explicit.

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
      - uses: actions/setup-python@v5
        with:
          python-version: "3.12"
      - run: python -m pip install "dbt-core>=1.5,<2" "dbt-snowflake>=1.5,<2"
      - uses: EmbrasureAI/embrasure-cli@v1
      - run: embrasure check --base origin/main --json
        env:
          SNOWFLAKE_PROGRAMMATIC_ACCESS_TOKEN: ${{ secrets.SNOWFLAKE_PROGRAMMATIC_ACCESS_TOKEN }}
```

`fetch-depth: 0` is required because selection compares the working tree with the base revision.

## Cloud handoff

Cloud handoff is optional. It sends the exact reviewed state and local evidence to a durable agent:

```sh
embrasure cloud login
embrasure cloud whoami
embrasure check --cloud \
  --context "Preserve one row per order. Refunds reduce net revenue. Missing discounts are zero."
embrasure cloud status
```

`--cloud` requires business intent and prints every eligible path before upload. A plain `check` primes the local review cache. The next `check --cloud` reuses it only when the repository, dbt root, base SHA, snapshot, CLI version, and check configuration match. Run `embrasure auth status` or `embrasure cloud whoami` to inspect session readiness without printing secrets.

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

See the [example configuration](embrasure-check.example.yml) and [enterprise setup guide](docs/enterprise.md) for service credentials, multiple accounts, model policies, filters, thresholds, concurrency, external changes, cross-account dependencies, Metabase, and grants.

Every temporary schema has a unique name and ownership marker. Embrasure checks both before removal and treats cleanup failures as execution failures. Use a dedicated role that can read and clone only the production tables under test and create temporary schemas in the required databases.

[Security and data flow](docs/security-and-data-flow.md) documents network connections, local files, returned data, credentials, cleanup, updates, and release verification.

## Current limits

- Snowflake is the only supported warehouse.
- dbt artifacts provide model lineage and exposures, not authoritative warehouse-to-dashboard column lineage.
- Metabase matching covers native SQL cards that reference fully qualified production relations. Unsupported or inaccessible metadata becomes a coverage gap.

## Development

Rust 1.85 or newer is required when building from source.

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --locked
```

The opt-in Snowflake suite uses `EMBRASURE_RUN_SNOWFLAKE_TESTS=1` and the `EMBRASURE_TEST_SNOWFLAKE_*` account, user, role, database, warehouse, and token variables.

Contributions are welcome under the [Apache 2.0 license](LICENSE).
