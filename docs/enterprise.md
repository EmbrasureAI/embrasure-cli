# Enterprise setup

The clean enterprise split is:

- Developers and local agents: `oauth_local`. Snowflake handles SSO/MFA in the browser; no password is given to the CLI.
- CI service users: a role-restricted programmatic access token or RSA key pair stored in the CI secret manager.
- Existing identity platform: an externally issued OAuth token supplied through an environment variable.

Each Snowflake account gets its own `accounts` entry, dbt selector, user, role, warehouse, and credential. Run `embrasure doctor` after setup; it verifies each one independently.

## Large projects

Embrasure reports the complete downstream blast radius. By default, it validates changed models and every model on the path to a critical downstream target. A target is critical when it has a configured critical dbt tag, `critical: true` in Embrasure, or directly supports a dbt exposure.

```yaml
validation:
  downstream: critical # none | critical | all
  critical_tags: [critical, tier_1]
  incremental_mode: clone
```

Override this for one run with `--downstream` and repeatable `--critical-tag`. Policy-excluded models remain in the report as not validated. If the requested models exceed `safety.max_models`, Embrasure exits `2` before it creates any Snowflake schema; it never truncates the selection.

Independent Snowflake comparisons run concurrently, bounded by `comparison.concurrency`.

Use quick mode for an inexpensive first pass:

```sh
embrasure check --base origin/main --mode quick
```

Quick mode estimates cardinality, ignores estimated cardinality changes below 2%, and skips numeric percentiles. Deep mode is the default and runs exact cardinality plus percentiles. Both modes still check schema, row counts, null rates, ranges, averages, dbt tests, downstream impact, and primary keys exactly.

Set a total comparison budget and limit scans on large fact tables:

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

Explicit `primary_key` takes precedence over a simple dbt `unique_key`. Identifier strings and lists are inferred; SQL expressions are not guessed. By default, duplicate and null keys fail only when CI introduces or worsens them. Set `key_policy: strict` on a model to require zero CI duplicate and null keys.

The predicate is applied to both relations. Use a stable boundary so both sides cover the same data. Snowflake also enforces `safety.statement_timeout_seconds` on each statement.

## Incremental models

The default `clone` mode tests the next incremental run without copying production data:

1. Clone the production table into a run-owned baseline schema.
2. Clone that baseline into the exact dbt candidate relation.
3. Run dbt incrementally against the candidate.
4. Compare the result with the unchanged baseline.

All required baseline and candidate clones are created before dbt starts. An unsupported relation or permission error stops the run; there is no automatic full refresh. The report marks this strategy as `incremental_clone` and notes that historical recomputation was not tested.

Use `--incremental-mode full-refresh` to build each candidate from scratch. Embrasure still creates a stable baseline clone for comparison, so production changes during the run cannot move the reference point. A new incremental model gets a normal first build and a report entry that no production comparison exists.

Table-level zero-copy clones preserve Snowflake micro-partitions and avoid cloning unrelated schema objects. Views, hybrid tables, external tables, and other objects that do not support `CREATE TABLE … CLONE` must be redesigned or excluded; Embrasure does not copy them with CTAS.

## 1. Grant a narrow validation role

Run equivalent grants in every Snowflake account. Replace the example object names with yours.

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

`SELECT` on each source table plus ownership of the run-created target schema permits table cloning. `embrasure doctor` creates a temporary schema, zero-copy clones one visible production table, and removes the schema. Use `embrasure doctor --read-only` when this write test is not allowed.

Grant this role to each person or service user that runs the CLI. The role owns only the temporary schemas it creates; the CLI refuses to remove schemas outside its configured prefix and ownership marker.

If selected models live in more schemas or databases, add `USAGE`, `SELECT`, and `CREATE SCHEMA` grants for those databases too. Keep the warehouse resource monitor, size, and auto-suspend policy under normal Snowflake administration.

## 2. Choose authentication

### Human or local coding agent

```yaml
auth:
  type: oauth_local
```

```sh
embrasure auth login --account primary
embrasure doctor
```

Snowflake's built-in `SNOWFLAKE$LOCAL_APPLICATION` integration uses Authorization Code + PKCE and a loopback callback. Account administrators can apply their normal network policies and tune the integration's token lifetime.

### CI with a programmatic access token

Create a service user, grant the validation role, and create a token restricted to that role:

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

The token secret appears only in that command's result. Put it directly in the CI secret manager, then configure:

```yaml
auth:
  type: programmatic_access_token
  token_env: SNOWFLAKE_PROGRAMMATIC_ACCESS_TOKEN
```

Rotate the token on your organization's normal schedule. Snowflake authentication and network policies still apply.

### CI with an RSA key pair

```yaml
auth:
  type: key_pair
  private_key_path: /run/secrets/snowflake_key.p8
  passphrase_env: SNOWFLAKE_PRIVATE_KEY_PASSPHRASE
```

Mount the private key read-only and keep its passphrase in the CI secret manager. Never commit either value.

### Existing external OAuth

```yaml
auth:
  type: oauth
  token_env: SNOWFLAKE_OAUTH_TOKEN
```

The organization remains responsible for issuing and refreshing that token before each run.

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

`doctor` checks both accounts. A normal run creates and cleans run-owned CI and baseline schemas in each required account/database and reports declared cross-account dependencies.

## 4. Optional Metabase impact

Create a read-only Metabase API key that can list cards and dashboards, then configure:

```yaml
metabase:
  url: https://metabase.example.com
  api_key_env: METABASE_API_KEY
```

`doctor` proves the key can read card metadata. Validation matches native-SQL cards conservatively. Query-builder/MBQL lineage that cannot be proven is reported as incomplete coverage, never silently treated as safe.

## 5. CI gate

```sh
embrasure doctor --json
embrasure check --base origin/main --json --markdown embrasure-check.md
```

Treat exit `0` as ready for review, `1` as a data finding, `2` as missing evidence, and `3` as setup/execution failure.

Report v2 is the default JSON contract and includes validation scope, skipped models, notices, duplicate/null-key metrics, build strategy, and dbt topology changes. Legacy consumers can request the unchanged v1 shape only with JSON output:

```sh
embrasure check --json --report-version 1
```

## Lineage boundary

Embrasure treats dbt model lineage, dbt exposures, declared cross-account edges, and optional Metabase SQL matches as the evidence available locally. It does not claim complete column-to-dashboard lineage from dbt artifacts.

Authoritative warehouse-to-BI column lineage requires a separate provider using Snowflake access history and BI APIs. Enterprise limits include additional Snowflake permissions, metadata latency, dynamic SQL that cannot be resolved, separate BI credentials, and gaps for inaccessible assets.
