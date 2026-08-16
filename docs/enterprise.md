# Enterprise setup

Each Snowflake account gets its own `accounts` entry, dbt selector, user, role, warehouse, and credential. Run `embrasure doctor` after setup to verify each account independently.

## 1. Grant a narrow validation role

Run equivalent grants in every Snowflake account. Replace the example object names.

```sql
USE ROLE SECURITYADMIN;
CREATE ROLE IF NOT EXISTS DBT_CHANGE_VALIDATOR;

USE ROLE SYSADMIN;
GRANT USAGE ON WAREHOUSE DBT_CI_WH TO ROLE DBT_CHANGE_VALIDATOR;
GRANT USAGE ON DATABASE ANALYTICS TO ROLE DBT_CHANGE_VALIDATOR;
GRANT CREATE SCHEMA ON DATABASE ANALYTICS TO ROLE DBT_CHANGE_VALIDATOR;
GRANT USAGE ON SCHEMA ANALYTICS.PROD TO ROLE DBT_CHANGE_VALIDATOR;
GRANT SELECT ON ALL TABLES IN SCHEMA ANALYTICS.PROD TO ROLE DBT_CHANGE_VALIDATOR;
GRANT SELECT ON FUTURE TABLES IN SCHEMA ANALYTICS.PROD TO ROLE DBT_CHANGE_VALIDATOR;
GRANT SELECT ON ALL VIEWS IN SCHEMA ANALYTICS.PROD TO ROLE DBT_CHANGE_VALIDATOR;
GRANT SELECT ON FUTURE VIEWS IN SCHEMA ANALYTICS.PROD TO ROLE DBT_CHANGE_VALIDATOR;
```

`SELECT` on a source table plus ownership of the run-created target schema permits table cloning. `embrasure doctor` creates a temporary schema, clones one visible production table, and removes the schema. Use `embrasure doctor --read-only` when this write test is not allowed.

Grant the role to each person or service user that runs the CLI. The role owns only its temporary schemas. If selected models live in other schemas or databases, add the corresponding `USAGE`, `SELECT`, and `CREATE SCHEMA` grants. Keep warehouse resource monitors, size, and auto-suspend under normal Snowflake administration.

## 2. Choose authentication

### Local browser login

`oauth_local` is the default for people and local coding agents. Snowflake handles SSO and MFA in the browser; the CLI receives no password.

```yaml
auth:
  type: oauth_local
```

```sh
embrasure auth login --account primary
embrasure doctor
```

Snowflake's built-in `SNOWFLAKE$LOCAL_APPLICATION` integration uses Authorization Code with PKCE and a loopback callback. Account administrators retain their network policies and token lifetime controls.

### CI credentials

Use a role-restricted programmatic access token or a read-only mounted RSA key. Store secrets in the CI secret manager.

Programmatic access token:

```sql
USE ROLE USERADMIN;
CREATE USER IF NOT EXISTS DBT_CHANGE_VALIDATOR_CI
  TYPE = SERVICE
  DEFAULT_ROLE = DBT_CHANGE_VALIDATOR;
USE ROLE SECURITYADMIN;
GRANT ROLE DBT_CHANGE_VALIDATOR TO USER DBT_CHANGE_VALIDATOR_CI;

ALTER USER DBT_CHANGE_VALIDATOR_CI
  ADD PROGRAMMATIC ACCESS TOKEN EMBRASURE_CHECK_CI
  ROLE_RESTRICTION = 'DBT_CHANGE_VALIDATOR'
  DAYS_TO_EXPIRY = 90;
```

```yaml
auth:
  type: programmatic_access_token
  token_env: SNOWFLAKE_PROGRAMMATIC_ACCESS_TOKEN
```

RSA key pair:

```yaml
auth:
  type: key_pair
  private_key_path: /run/secrets/snowflake_key.p8
  passphrase_env: SNOWFLAKE_PRIVATE_KEY_PASSPHRASE
```

For an existing external OAuth flow, set `type: oauth` and `token_env`. The organization must issue and refresh that token before each run.

## 3. Configure multiple accounts

Use a unique environment variable and dbt selector per account:

```yaml
accounts:
  - name: account_a
    account: org-account_a
    user: DBT_CHANGE_VALIDATOR_CI
    role: DBT_CHANGE_VALIDATOR
    database: ANALYTICS
    warehouse: DBT_CI_WH
    production_schema: PROD
    selector: tag:account_a
    auth:
      type: programmatic_access_token
      token_env: SNOWFLAKE_ACCOUNT_A_PAT

  - name: account_b
    account: org-account_b
    user: DBT_CHANGE_VALIDATOR_CI
    role: DBT_CHANGE_VALIDATOR
    database: ANALYTICS
    warehouse: DBT_CI_WH
    production_schema: PROD
    selector: tag:account_b
    auth:
      type: programmatic_access_token
      token_env: SNOWFLAKE_ACCOUNT_B_PAT
```

`doctor` checks both accounts. A check creates and cleans run-owned candidate and baseline schemas in each required account and database. Declared cross-account dependencies appear in the impact report.

## 4. Optional Metabase impact

Create a read-only Metabase API key that can list cards and dashboards:

```yaml
metabase:
  url: https://metabase.example.com
  api_key_env: METABASE_API_KEY
```

`doctor` proves the key can read card metadata. Validation matches native SQL cards conservatively. Query-builder or MBQL lineage that cannot be proven becomes a coverage gap.

## 5. CI gate

```sh
embrasure doctor --json
embrasure check --base origin/main --json --markdown embrasure-check.md
```

Exit `0` is ready for review, `1` is a finding, `2` is missing evidence, and `3` is a setup or execution failure.

Report v2 is the default JSON contract. It includes validation scope, skipped models, notices, duplicate and null-key metrics, build strategy, and dbt topology changes. Legacy consumers can request v1 with `--json --report-version 1`.

## Reference

### Large projects

Embrasure computes impact from the full changed set. By default, it validates changed models and paths to critical downstream targets. A target is critical when it has a configured tag, `critical: true`, or directly supports a dbt exposure.

```yaml
validation:
  downstream: critical # none | critical | all
  critical_tags: [critical, tier_1]
  incremental_mode: clone
```

Override policy with `--downstream` and repeatable `--critical-tag`. Use repeatable `--select` to intersect the resulting validation set. Excluded models remain visible as not validated.

If selection exceeds `safety.max_models`, Embrasure exits `2` before dbt builds or comparisons. It does not truncate the set. Independent comparisons run concurrently up to `comparison.concurrency`.

Quick mode is the inexpensive first pass:

```sh
embrasure check --mode quick
```

Quick mode estimates cardinality, ignores estimated changes below 2%, and skips percentiles. Deep mode uses exact cardinality and percentiles. Both check schema, row counts, null rates, ranges, averages, dbt tests, impact, and primary keys.

Limit total time and scans on large fact tables:

```yaml
comparison:
  mode: deep
  concurrency: 4
  timeout_seconds: 900

models:
  model.analytics.orders:
    primary_key: [order_id]
    where: "order_date >= DATEADD(day, -30, CURRENT_DATE)"
    thresholds:
      row_count_relative: 0.05
```

An explicit `primary_key` takes precedence over a simple dbt `unique_key`. Embrasure infers identifier strings and lists, not SQL expressions. The default regression policy fails when CI introduces or worsens duplicate and null keys. Set `key_policy: strict` to require zero CI duplicate and null keys.

The filter applies to both relations. Use a stable boundary so both sides cover the same data. Snowflake enforces `safety.statement_timeout_seconds` on each statement.

### Incremental models

The default `clone` mode tests the next incremental run without copying production data:

1. Clone the production table into a run-owned baseline schema.
2. Clone the baseline into the dbt candidate relation.
3. Run dbt incrementally against the candidate.
4. Compare the result with the unchanged baseline.

All baseline and candidate clones are created before dbt starts. Unsupported relations or permission failures stop the run. The report uses `incremental_clone` and notes that historical recomputation was not tested.

Use `--incremental-mode full-refresh` to build candidates from scratch. Embrasure still creates a stable baseline clone, so production changes during the run cannot move the reference. A new incremental model receives a normal first build and a coverage gap because no production relation exists.

Table-level zero-copy clones preserve Snowflake micro-partitions. Views, hybrid tables, external tables, and other objects that do not support `CREATE TABLE ... CLONE` must use another validation path.

### Lineage boundary

Local evidence includes dbt model lineage, dbt exposures, declared cross-account edges, and optional Metabase SQL matches. It does not prove warehouse-to-dashboard column lineage.

Authoritative column lineage requires Snowflake access history and BI APIs. Its limits include extra Snowflake permissions, metadata latency, dynamic SQL, separate BI credentials, and inaccessible assets.
