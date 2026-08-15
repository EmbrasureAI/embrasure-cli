# Enterprise setup

The clean enterprise split is:

- Developers and local agents: `oauth_local`. Snowflake handles SSO/MFA in the browser; no password is given to the CLI.
- CI service users: a role-restricted programmatic access token or RSA key pair stored in the CI secret manager.
- Existing identity platform: an externally issued OAuth token supplied through an environment variable.

Each Snowflake account gets its own `accounts` entry, dbt selector, user, role, warehouse, and credential. Run `embrasure doctor` after setup; it verifies each one independently.

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

Grant this role to each person or service user that runs the CLI. The role owns only the temporary schemas it creates; the CLI refuses to remove schemas outside its configured prefix.

If production parents live in more schemas or databases, add `USAGE` and `SELECT` grants for those objects too. Keep the warehouse resource monitor, size, and auto-suspend policy under normal Snowflake administration.

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

`doctor` checks both accounts. A normal run creates and cleans a separate CI schema in each account and reports declared cross-account dependencies; missing column evidence remains explicitly `unknown`.

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
