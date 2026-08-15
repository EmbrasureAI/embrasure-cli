<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/embrasure-lockup-ivory.svg">
    <img src="assets/embrasure-lockup-ink.svg" alt="Embrasure" width="300">
  </picture>
</p>

<p align="center"><strong>Catch unexpected data changes before a dbt PR is reviewed.</strong></p>

Embrasure builds the dbt models changed on your branch and their downstream dependents in a temporary Snowflake schema. It compares them with production, runs your existing dbt tests, reports downstream impact, and removes the schema when it finishes.

Use it locally or in CI. It connects directly to Snowflake, so no Embrasure account or hosted service is required.

## What it catches

- Columns added, removed, renamed, or changed to an incompatible type
- Unexpected shifts in row counts, null rates, cardinality, ranges, averages, and percentiles
- Primary-key values that appear or disappear
- Failures in your existing dbt tests
- Downstream dbt models, exposures, cross-account dependencies, and optional Metabase dashboards affected by the change
- Models affected by non-dbt changes you map in the configuration

## Run it locally

You need Git and dbt Core with the Snowflake adapter. You do not need Rust or an Embrasure account.

Install the CLI on macOS or Linux:

```sh
brew install embrasureai/tap/embrasure
```

From your dbt project, run the guided setup. It reads your existing dbt profile and only asks for anything it cannot find.

```sh
embrasure init
embrasure auth login
embrasure doctor
embrasure check --base origin/main
```

Example result:

```text
embrasure: PASS
2 selected · 2 built · 2 compared · 0 findings · 0 coverage gaps
```

## Use it with an agent or CI

JSON output is versioned and deterministic. Progress goes to stderr so stdout contains one JSON document.

```sh
embrasure check --base origin/main --json
embrasure check --base origin/main --json --markdown embrasure-check.md
```

For a faster first pass on large tables, add `--mode quick`. Deep mode remains the default and includes exact cardinality plus numeric percentiles.

The intended agent loop is simple:

```text
Run `embrasure check --base origin/main --json`.
Exit 1: fix every finding and rerun.
Exit 2: resolve or explain every coverage gap.
Exit 3: fix the setup or execution failure.
Do not request review until it exits 0.
```

| Exit code | Meaning |
|---:|---|
| `0` | The check passed |
| `1` | The data or code needs a fix |
| `2` | A requested check could not be completed |
| `3` | Setup, execution, or cleanup failed |

## Advanced setup

The guided setup creates the smallest configuration needed for one Snowflake account. For service credentials, multiple accounts, primary keys, large-table filters, concurrency and time limits, custom thresholds, non-dbt changes, cross-account dependencies, or Metabase, use the [example configuration](embrasure-check.example.yml) and [enterprise setup guide](docs/enterprise.md).

## Safety

Embrasure uses a dedicated warehouse, query tags, statement timeouts, and a configurable model limit. Every temporary schema has a unique name and ownership marker. The CLI checks both before dropping it and treats cleanup failures as execution failures.

Use a Snowflake role that can read the production relations under test and create temporary schemas in the configured CI database. Credentials are never written to reports or normal terminal output.

See [Security and data flow](docs/security-and-data-flow.md) for the exact network connections, local files, data returned from Snowflake, credential handling, cleanup behavior, and release-verification steps.

## Releases

Intel and ARM downloads for macOS and Linux are available on the [releases page](https://github.com/EmbrasureAI/embrasure-cli/releases). Each release includes SHA-256 checksums, an SPDX software bill of materials, and GitHub-signed build-provenance attestations.

If you previously installed `@embrasure/cli` with npm, uninstall it first with `npm uninstall -g @embrasure/cli` so the two executables do not conflict.

## Current limits

- Snowflake is the only supported warehouse.
- dbt model lineage and exposures are authoritative. Missing column-level lineage is reported as unknown.
- Metabase matching covers native SQL cards that reference fully qualified production relations. Unsupported or inaccessible metadata is reported as a coverage gap.

## Development

Rust 1.85 or newer is required only when building from source.

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --locked
```

Contributions are welcome under the [Apache 2.0 license](LICENSE).
