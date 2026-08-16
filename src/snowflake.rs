use std::{
    env, fs,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use openssl::{
    hash::MessageDigest,
    pkey::{Id, PKey, Private},
    rsa::Padding,
    sign::Signer,
};
use reqwest::{Client, StatusCode, header};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::time::{sleep, timeout};
use uuid::Uuid;

use crate::{
    auth::{ResolvedAuth, account_host},
    config::AccountConfig,
};

#[derive(Clone)]
pub struct SnowflakeClient {
    http: Client,
    endpoint: String,
    token: String,
    token_type: &'static str,
    account: AccountConfig,
    query_tag: String,
    timeout_seconds: u64,
}

#[derive(Debug, Clone, Default)]
pub struct QueryResult {
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<Option<String>>>,
}

#[derive(Debug, Clone)]
pub struct ResultColumn {
    pub name: String,
    pub data_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relation {
    pub database: String,
    pub schema: String,
    pub identifier: String,
}

#[derive(Debug, Serialize)]
struct Claims {
    iss: String,
    sub: String,
    iat: u64,
    exp: u64,
}

impl Relation {
    pub fn sql(&self) -> String {
        format!(
            "{}.{}.{}",
            quote_identifier(&self.database),
            quote_identifier(&self.schema),
            quote_identifier(&self.identifier)
        )
    }
}

impl SnowflakeClient {
    pub fn new(
        account: &AccountConfig,
        auth: &ResolvedAuth,
        query_tag: String,
        timeout_seconds: u64,
    ) -> Result<Self> {
        let (token, token_type) = match auth {
            ResolvedAuth::OAuth { token } => (token.clone(), "OAUTH"),
            ResolvedAuth::KeyPair {
                private_key_path,
                passphrase,
            } => (
                key_pair_jwt(account, private_key_path, passphrase.as_deref())?,
                "KEYPAIR_JWT",
            ),
            ResolvedAuth::ProgrammaticAccessToken { token } => {
                (token.clone(), "PROGRAMMATIC_ACCESS_TOKEN")
            }
        };
        Ok(Self {
            http: Client::builder()
                .timeout(Duration::from_secs(timeout_seconds + 30))
                .build()?,
            endpoint: format!(
                "https://{}/api/v2/statements",
                account_host(&account.account)
            ),
            token,
            token_type,
            account: account.clone(),
            query_tag,
            timeout_seconds,
        })
    }

    pub async fn execute(&self, statement: &str) -> Result<QueryResult> {
        timeout(Duration::from_secs(self.timeout_seconds + 30), async {
            let request_id = Uuid::new_v4();
            let response = self
                .http
                .post(&self.endpoint)
                .query(&[("requestId", request_id.to_string())])
                .headers(self.headers()?)
                .json(&statement_body(
                    statement,
                    &self.account,
                    &self.query_tag,
                    self.timeout_seconds,
                ))
                .send()
                .await
                .context("Snowflake SQL API request failed")?;
            self.consume_response(response, request_id, statement).await
        })
        .await
        .with_context(|| {
            format!(
                "Snowflake SQL API request exceeded {} seconds",
                self.timeout_seconds + 30
            )
        })?
    }

    async fn consume_response(
        &self,
        mut response: reqwest::Response,
        request_id: Uuid,
        statement: &str,
    ) -> Result<QueryResult> {
        loop {
            let status = response.status();
            let body: ApiResponse = response
                .json()
                .await
                .context("Snowflake returned invalid JSON")?;
            if status == StatusCode::ACCEPTED {
                let handle = body
                    .statement_handle
                    .clone()
                    .context("Snowflake async response omitted statementHandle")?;
                sleep(Duration::from_millis(500)).await;
                response = self
                    .http
                    .get(format!("{}/{}", self.endpoint, handle))
                    .query(&[("requestId", request_id.to_string())])
                    .headers(self.headers()?)
                    .send()
                    .await
                    .context("Snowflake SQL API polling failed")?;
                continue;
            }
            if !status.is_success() || !body.is_success() {
                let code = body.code.as_deref().unwrap_or("unknown");
                let message = body.message.as_deref().unwrap_or("unknown error");
                let hint = privilege_hint(code, message, statement, &self.account)
                    .map(|value| format!("; {value}"))
                    .unwrap_or_default();
                bail!("Snowflake statement failed (HTTP {status}, code {code}): {message}{hint}");
            }
            let handle = body.statement_handle.clone();
            let mut result = QueryResult {
                columns: body
                    .metadata
                    .as_ref()
                    .map(|m| {
                        m.row_type
                            .iter()
                            .map(|column| ResultColumn {
                                name: column.name.clone(),
                                data_type: column.display_type(),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                rows: parse_rows(body.data),
            };
            let partitions = body
                .metadata
                .as_ref()
                .map(|m| m.partition_info.len())
                .unwrap_or(0);
            if partitions > 1 {
                let handle =
                    handle.context("partitioned Snowflake result omitted statementHandle")?;
                for partition in 1..partitions {
                    let partition_response = self
                        .http
                        .get(format!("{}/{}", self.endpoint, handle))
                        .query(&[
                            ("partition", partition.to_string()),
                            ("requestId", Uuid::new_v4().to_string()),
                        ])
                        .headers(self.headers()?)
                        .send()
                        .await
                        .context("Snowflake partition fetch failed")?;
                    let status = partition_response.status();
                    let body: ApiResponse = partition_response
                        .json()
                        .await
                        .context("Snowflake partition returned invalid JSON")?;
                    if !status.is_success() || !body.is_success() {
                        bail!(
                            "Snowflake partition {partition} failed: {}",
                            body.message.as_deref().unwrap_or("unknown error")
                        );
                    }
                    result.rows.extend(parse_rows(body.data));
                }
            }
            return Ok(result);
        }
    }

    pub async fn create_schema(&self, database: &str, schema: &str) -> Result<()> {
        self.execute(&format!(
            "CREATE TRANSIENT SCHEMA {}.{} COMMENT = {}",
            quote_identifier(database),
            quote_identifier(schema),
            quote_string(&self.schema_ownership_marker()),
        ))
        .await?;
        Ok(())
    }

    pub async fn clone_table(&self, source: &Relation, target: &Relation) -> Result<()> {
        self.execute(&clone_table_statement(source, target)).await?;
        Ok(())
    }

    pub async fn drop_schema(&self, database: &str, schema: &str, run_schema: &str) -> Result<()> {
        if !is_managed_schema(schema, run_schema) {
            bail!("refusing to drop schema {schema}: it is not owned by this run ({run_schema})");
        }
        let ownership = self
            .execute(&format!(
                "SELECT COMMENT FROM {}.INFORMATION_SCHEMA.SCHEMATA WHERE SCHEMA_NAME = {}",
                quote_identifier(database),
                quote_string(schema),
            ))
            .await?;
        let Some(comment) = ownership
            .rows
            .first()
            .and_then(|row| row.first())
            .and_then(Option::as_deref)
        else {
            return Ok(());
        };
        let expected = self.schema_ownership_marker();
        if comment != expected {
            bail!(
                "refusing to drop schema {database}.{schema}: its ownership marker does not match this run"
            );
        }
        self.execute(&format!(
            "DROP SCHEMA IF EXISTS {}.{}",
            quote_identifier(database),
            quote_identifier(schema),
        ))
        .await?;
        Ok(())
    }

    pub async fn drop_marked_schema(
        &self,
        database: &str,
        schema: &str,
        prefix: &str,
    ) -> Result<()> {
        if !crate::clean::is_managed_prefix(schema, prefix) {
            bail!("refusing to drop schema {schema}: it is outside the managed prefix {prefix}");
        }
        let ownership = self
            .execute(&format!(
                "SELECT COMMENT FROM {}.INFORMATION_SCHEMA.SCHEMATA WHERE SCHEMA_NAME = {}",
                quote_identifier(database),
                quote_string(schema),
            ))
            .await?;
        let Some(comment) = ownership
            .rows
            .first()
            .and_then(|row| row.first())
            .and_then(Option::as_deref)
        else {
            return Ok(());
        };
        if crate::clean::parse_ownership_marker(comment).is_none() {
            bail!(
                "refusing to drop schema {database}.{schema}: its Embrasure ownership marker is invalid"
            );
        }
        self.execute(&format!(
            "DROP SCHEMA IF EXISTS {}.{}",
            quote_identifier(database),
            quote_identifier(schema),
        ))
        .await?;
        Ok(())
    }

    fn schema_ownership_marker(&self) -> String {
        format!("Temporary schema managed by Embrasure; {}", self.query_tag)
    }

    fn headers(&self) -> Result<header::HeaderMap> {
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {}", self.token).parse()?,
        );
        headers.insert(
            header::ACCEPT,
            header::HeaderValue::from_static("application/json"),
        );
        headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json"),
        );
        headers.insert(
            header::USER_AGENT,
            header::HeaderValue::from_static(concat!("embrasure/", env!("CARGO_PKG_VERSION"))),
        );
        headers.insert(
            "X-Snowflake-Authorization-Token-Type",
            header::HeaderValue::from_static(self.token_type),
        );
        Ok(headers)
    }
}

fn clone_table_statement(source: &Relation, target: &Relation) -> String {
    format!("CREATE TABLE {} CLONE {}", target.sql(), source.sql())
}

fn statement_body(
    statement: &str,
    account: &AccountConfig,
    query_tag: &str,
    timeout_seconds: u64,
) -> Value {
    json!({
        "statement": statement,
        "timeout": timeout_seconds,
        "database": account.database,
        "warehouse": account.warehouse,
        "role": account.role,
        "parameters": { "query_tag": query_tag }
    })
}

fn key_pair_jwt(
    account: &AccountConfig,
    path: &std::path::Path,
    passphrase: Option<&str>,
) -> Result<String> {
    let pem = fs::read_to_string(path)
        .with_context(|| format!("could not read private key {}", path.display()))?;
    let key: PKey<Private> = if let Some(passphrase) = passphrase {
        PKey::private_key_from_pem_passphrase(pem.as_bytes(), passphrase.as_bytes())
            .context("could not decrypt RSA private key")?
    } else {
        PKey::private_key_from_pem(pem.as_bytes())
            .context("private key must be an RSA PKCS#1 or PKCS#8 PEM")?
    };
    if key.id() != Id::RSA {
        bail!("Snowflake key-pair authentication requires an RSA private key");
    }
    let public_der = key.public_key_to_der()?;
    let fingerprint = format!("SHA256:{}", STANDARD.encode(Sha256::digest(&public_der)));
    let account_id = account.account.to_ascii_uppercase().replace('.', "-");
    let user = account.user.to_ascii_uppercase();
    let subject = format!("{account_id}.{user}");
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let claims = Claims {
        iss: format!("{subject}.{fingerprint}"),
        sub: subject,
        iat: now,
        exp: now + 3540,
    };
    sign_jwt(&key, &claims)
}

fn sign_jwt(key: &PKey<Private>, claims: &Claims) -> Result<String> {
    let header = serde_json::json!({ "alg": "RS256", "typ": "JWT" });
    let encoded_header = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header)?);
    let encoded_claims = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims)?);
    let signing_input = format!("{encoded_header}.{encoded_claims}");
    let mut signer = Signer::new(MessageDigest::sha256(), key)?;
    signer.set_rsa_padding(Padding::PKCS1)?;
    signer.update(signing_input.as_bytes())?;
    let signature = URL_SAFE_NO_PAD.encode(signer.sign_to_vec()?);
    Ok(format!("{signing_input}.{signature}"))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiResponse {
    code: Option<String>,
    message: Option<String>,
    statement_handle: Option<String>,
    #[serde(rename = "resultSetMetaData")]
    metadata: Option<ResultMetadata>,
    #[serde(default)]
    data: Vec<Vec<Value>>,
}

impl ApiResponse {
    fn is_success(&self) -> bool {
        self.code
            .as_deref()
            .is_none_or(|code| code == "0" || code == "090001")
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResultMetadata {
    #[serde(default)]
    row_type: Vec<ApiColumn>,
    #[serde(default)]
    partition_info: Vec<Map<String, Value>>,
}

#[derive(Debug, Deserialize)]
struct ApiColumn {
    name: String,
    #[serde(rename = "type")]
    data_type: String,
    precision: Option<i64>,
    scale: Option<i64>,
    length: Option<i64>,
}

impl ApiColumn {
    fn display_type(&self) -> String {
        match self.data_type.to_ascii_lowercase().as_str() {
            "fixed" => match (self.precision, self.scale) {
                (Some(precision), Some(scale)) => format!("NUMBER({precision},{scale})"),
                _ => "NUMBER".into(),
            },
            "real" => "FLOAT".into(),
            "text" => self
                .length
                .map_or_else(|| "VARCHAR".into(), |length| format!("VARCHAR({length})")),
            other => other.to_ascii_uppercase(),
        }
    }
}

fn parse_rows(rows: Vec<Vec<Value>>) -> Vec<Vec<Option<String>>> {
    rows.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| match value {
                    Value::Null => None,
                    Value::String(value) => Some(value),
                    other => Some(other.to_string()),
                })
                .collect()
        })
        .collect()
}

pub fn quote_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn quote_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

pub fn is_managed_schema(schema: &str, run_schema: &str) -> bool {
    let schema = schema.to_ascii_uppercase();
    let run_schema = run_schema.to_ascii_uppercase();
    schema == run_schema || schema.starts_with(&format!("{run_schema}_"))
}

fn privilege_hint(
    code: &str,
    message: &str,
    statement: &str,
    account: &AccountConfig,
) -> Option<String> {
    let upper = statement.trim_start().to_ascii_uppercase();
    let insufficient = code == "003001"
        || message
            .to_ascii_lowercase()
            .contains("insufficient privilege");
    let hint = if code == "002003" {
        "the object may not exist, or the role lacks USAGE on its database or schema".to_owned()
    } else if insufficient && upper.starts_with("CREATE TRANSIENT SCHEMA") {
        format!(
            "role {} needs CREATE SCHEMA on database {}",
            account.role, account.database
        )
    } else if insufficient && upper.starts_with("CREATE TABLE") && upper.contains(" CLONE ") {
        format!(
            "role {} needs SELECT on the source and CREATE TABLE on the target schema",
            account.role
        )
    } else if insufficient && upper.starts_with("DROP SCHEMA") {
        format!(
            "role {} needs ownership or compatible future grants for the temporary schema",
            account.role
        )
    } else if insufficient {
        format!("role {} lacks a required Snowflake privilege", account.role)
    } else {
        return None;
    };
    Some(format!("{hint}; run `embrasure doctor`"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        auth::ResolvedAuth,
        config::{AuthConfig, SafetyConfig},
        query::{QueryDiffInput, run_query_diff},
        report::QueryCheckStatus,
    };
    use openssl::{rsa::Rsa, sign::Verifier};

    #[test]
    fn identifiers_are_always_quoted() {
        assert_eq!(quote_identifier("odd\"name"), "\"odd\"\"name\"");
        assert_eq!(quote_string("run's marker"), "'run''s marker'");
        assert_eq!(
            Relation {
                database: "D".into(),
                schema: "S".into(),
                identifier: "T".into()
            }
            .sql(),
            "\"D\".\"S\".\"T\""
        );
    }

    #[test]
    fn cleanup_is_limited_to_the_exact_run_namespace() {
        let run = "EMBRASURE_CHECK_SHA_TIME_RANDOM";
        assert!(is_managed_schema(run, run));
        assert!(is_managed_schema(
            "EMBRASURE_CHECK_SHA_TIME_RANDOM_MARKETING",
            run
        ));
        assert!(!is_managed_schema("EMBRASURE_CHECK_PROD", run));
        assert!(!is_managed_schema("EMBRASURE_CHECK_SHA_TIME_RANDOMLY", run));
    }

    #[test]
    fn privilege_errors_include_actionable_hints() {
        let account = AccountConfig {
            name: "primary".into(),
            account: "org-account".into(),
            user: "DBT_CI".into(),
            role: "DBT_CI_ROLE".into(),
            database: "ANALYTICS".into(),
            warehouse: "DBT_CI_WH".into(),
            production_schema: "PROD".into(),
            selector: None,
            auth: AuthConfig::OauthLocal,
        };
        assert!(
            privilege_hint("003001", "denied", "CREATE TRANSIENT SCHEMA x.y", &account)
                .unwrap()
                .contains("CREATE SCHEMA on database ANALYTICS")
        );
        assert!(
            privilege_hint(
                "003001",
                "denied",
                "CREATE TABLE x.y.z CLONE a.b.c",
                &account
            )
            .unwrap()
            .contains("SELECT on the source")
        );
        assert!(
            privilege_hint("003001", "denied", "DROP SCHEMA x.y", &account)
                .unwrap()
                .contains("ownership")
        );
        assert!(
            privilege_hint("002003", "missing", "SELECT 1", &account)
                .unwrap()
                .contains("may not exist")
        );
    }

    #[test]
    fn clone_seed_is_zero_copy_and_quotes_every_identifier() {
        let source = Relation {
            database: "PROD DB".into(),
            schema: "ANALYTICS".into(),
            identifier: "Order Facts".into(),
        };
        let target = Relation {
            database: "CI DB".into(),
            schema: "EMBRASURE_RUN_BASELINE".into(),
            identifier: "MODEL_0".into(),
        };
        let sql = clone_table_statement(&source, &target);
        assert_eq!(
            sql,
            r#"CREATE TABLE "CI DB"."EMBRASURE_RUN_BASELINE"."MODEL_0" CLONE "PROD DB"."ANALYTICS"."Order Facts""#
        );
        assert!(!sql.contains(" AS SELECT "));
        assert!(!sql.contains("INSERT"));
    }

    #[test]
    fn result_types_preserve_precision_and_length() {
        assert_eq!(
            ApiColumn {
                name: "N".into(),
                data_type: "fixed".into(),
                precision: Some(18),
                scale: Some(2),
                length: None
            }
            .display_type(),
            "NUMBER(18,2)"
        );
        assert_eq!(
            ApiColumn {
                name: "T".into(),
                data_type: "text".into(),
                precision: None,
                scale: None,
                length: Some(50)
            }
            .display_type(),
            "VARCHAR(50)"
        );
    }

    #[test]
    fn jwt_uses_a_valid_rs256_signature() {
        let key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
        let claims = Claims {
            iss: "ACCOUNT.USER.SHA256:fingerprint".into(),
            sub: "ACCOUNT.USER".into(),
            iat: 1,
            exp: 2,
        };
        let jwt = sign_jwt(&key, &claims).unwrap();
        let parts = jwt.split('.').collect::<Vec<_>>();
        assert_eq!(parts.len(), 3);
        let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        let mut verifier = Verifier::new(MessageDigest::sha256(), &key).unwrap();
        verifier.set_rsa_padding(Padding::PKCS1).unwrap();
        verifier
            .update(format!("{}.{}", parts[0], parts[1]).as_bytes())
            .unwrap();
        assert!(verifier.verify(&signature).unwrap());
    }

    #[test]
    fn sql_api_body_uses_supported_request_fields() {
        let account = AccountConfig {
            name: "one".into(),
            account: "org-account".into(),
            user: "CI".into(),
            role: "ROLE".into(),
            database: "DB".into(),
            warehouse: "WH".into(),
            production_schema: "PROD".into(),
            selector: None,
            auth: AuthConfig::Oauth {
                token_env: "TOKEN".into(),
            },
        };
        let body = statement_body("SELECT 1", &account, "check:1", 60);
        assert_eq!(body["timeout"], 60);
        assert_eq!(
            body["parameters"],
            serde_json::json!({ "query_tag": "check:1" })
        );
    }

    #[tokio::test]
    async fn snowflake_incremental_strategies_and_scale() {
        if env::var("EMBRASURE_RUN_SNOWFLAKE_TESTS").as_deref() != Ok("1") {
            return;
        }
        let required = |name: &str| {
            env::var(name).unwrap_or_else(|_| {
                panic!("{name} is required when EMBRASURE_RUN_SNOWFLAKE_TESTS=1")
            })
        };
        let account = AccountConfig {
            name: "integration".into(),
            account: required("EMBRASURE_TEST_SNOWFLAKE_ACCOUNT"),
            user: required("EMBRASURE_TEST_SNOWFLAKE_USER"),
            role: required("EMBRASURE_TEST_SNOWFLAKE_ROLE"),
            database: required("EMBRASURE_TEST_SNOWFLAKE_DATABASE"),
            warehouse: required("EMBRASURE_TEST_SNOWFLAKE_WAREHOUSE"),
            production_schema: "unused".into(),
            selector: None,
            auth: AuthConfig::ProgrammaticAccessToken {
                token_env: "EMBRASURE_TEST_SNOWFLAKE_TOKEN".into(),
            },
        };
        let auth = ResolvedAuth::ProgrammaticAccessToken {
            token: required("EMBRASURE_TEST_SNOWFLAKE_TOKEN"),
        };
        let run_schema = format!("EMBRASURE_IT_{}", Uuid::new_v4().simple()).to_ascii_uppercase();
        let second_schema = format!("{run_schema}_SECOND");
        let client = SnowflakeClient::new(
            &account,
            &auth,
            format!("embrasure:integration:{run_schema}"),
            300,
        )
        .unwrap();
        client
            .create_schema(&account.database, &run_schema)
            .await
            .unwrap();
        client
            .create_schema(&account.database, &second_schema)
            .await
            .unwrap();

        let source = Relation {
            database: account.database.clone(),
            schema: run_schema.clone(),
            identifier: "SOURCE".into(),
        };
        let baseline = Relation {
            database: account.database.clone(),
            schema: second_schema.clone(),
            identifier: "BASELINE".into(),
        };
        let candidate = Relation {
            database: account.database.clone(),
            schema: second_schema.clone(),
            identifier: "CANDIDATE".into(),
        };
        let result = async {
            client
                .execute(&format!(
                    "CREATE TABLE {} AS SELECT ID, MOD(ID, 100) AS SEGMENT FROM (SELECT ROW_NUMBER() OVER (ORDER BY SEQ4()) - 1 AS ID FROM TABLE(GENERATOR(ROWCOUNT => 100000)))",
                    source.sql()
                ))
                .await?;
            client.clone_table(&source, &baseline).await?;
            client.clone_table(&baseline, &candidate).await?;
            client
                .execute(&format!(
                    "MERGE INTO {} C USING (SELECT 100000 AS ID, 0 AS SEGMENT) N ON C.ID = N.ID WHEN NOT MATCHED THEN INSERT (ID, SEGMENT) VALUES (N.ID, N.SEGMENT)",
                    candidate.sql()
                ))
                .await?;
            let incremental = client
                .execute(&format!("SELECT COUNT(*) FROM {}", candidate.sql()))
                .await?;
            assert_eq!(incremental.rows[0][0].as_deref(), Some("100001"));

            client
                .execute(&format!(
                    "CREATE OR REPLACE TABLE {} AS SELECT * FROM {} WHERE ID < 50000 UNION ALL SELECT 1, 1",
                    candidate.sql(), baseline.sql()
                ))
                .await?;
            let integrity = client
                .execute(&format!(
                    "SELECT COUNT(*) AS ROWS, COUNT_IF(ID IS NULL) AS NULL_KEYS, COUNT(*) - COUNT(DISTINCT ID) AS DUPLICATE_ROWS FROM {}",
                    candidate.sql()
                ))
                .await?;
            assert_eq!(integrity.rows[0][0].as_deref(), Some("50001"));
            assert_eq!(integrity.rows[0][1].as_deref(), Some("0"));
            assert_eq!(integrity.rows[0][2].as_deref(), Some("1"));

            let query_candidate = Relation {
                database: account.database.clone(),
                schema: second_schema.clone(),
                identifier: "QUERY_CANDIDATE".into(),
            };
            let query_production = Relation {
                database: account.database.clone(),
                schema: second_schema.clone(),
                identifier: "QUERY_PRODUCTION".into(),
            };
            let candidate_sql = format!("SELECT ID, SEGMENT FROM {}", candidate.sql());
            let production_sql = format!(
                "SELECT ID, SEGMENT FROM {} WHERE ID < 50000",
                baseline.sql()
            );
            let query_report = run_query_diff(
                &client,
                QueryDiffInput {
                    name: "duplicate multiplicity",
                    account: "integration",
                    current_refs: vec![],
                    production_refs: vec![],
                    candidate_sql: &candidate_sql,
                    production_sql: &production_sql,
                    candidate: &query_candidate,
                    production: &query_production,
                    primary_key: &[],
                    safety: &SafetyConfig::default(),
                },
            )
            .await?;
            assert_eq!(query_report.status, QueryCheckStatus::Findings);
            let comparison = query_report.comparison.unwrap();
            assert_eq!(comparison.candidate_only_rows, 1);
            assert_eq!(comparison.production_only_rows, 0);
            assert_eq!(comparison.examples[0].candidate_multiplicity, Some(2));
            assert_eq!(comparison.examples[0].production_multiplicity, Some(1));

            let pass_candidate = Relation {
                database: account.database.clone(),
                schema: second_schema.clone(),
                identifier: "QUERY_PASS_CANDIDATE".into(),
            };
            let pass_production = Relation {
                database: account.database.clone(),
                schema: second_schema.clone(),
                identifier: "QUERY_PASS_PRODUCTION".into(),
            };
            let pass_sql = format!("SELECT ID, SEGMENT FROM {}", baseline.sql());
            let pass = run_query_diff(
                &client,
                QueryDiffInput {
                    name: "exact keyed pass",
                    account: "integration",
                    current_refs: vec![],
                    production_refs: vec![],
                    candidate_sql: &pass_sql,
                    production_sql: &pass_sql,
                    candidate: &pass_candidate,
                    production: &pass_production,
                    primary_key: &["ID".into()],
                    safety: &SafetyConfig::default(),
                },
            )
            .await?;
            assert_eq!(pass.status, QueryCheckStatus::Pass);

            let keyed_candidate = Relation {
                database: account.database.clone(),
                schema: second_schema.clone(),
                identifier: "QUERY_KEYED_CANDIDATE".into(),
            };
            let keyed_production = Relation {
                database: account.database.clone(),
                schema: second_schema.clone(),
                identifier: "QUERY_KEYED_PRODUCTION".into(),
            };
            let candidate_sql = "SELECT COLUMN1::NUMBER(38,0) AS ID, COLUMN2::VARCHAR(20) AS VALUE FROM VALUES (1, 'same'), (2, 'new'), (4, 'added')";
            let production_sql = "SELECT COLUMN1::NUMBER(38,0) AS ID, COLUMN2::VARCHAR(20) AS VALUE FROM VALUES (1, 'same'), (2, 'old'), (3, 'removed')";
            let keyed = run_query_diff(
                &client,
                QueryDiffInput {
                    name: "keyed changes",
                    account: "integration",
                    current_refs: vec![],
                    production_refs: vec![],
                    candidate_sql,
                    production_sql,
                    candidate: &keyed_candidate,
                    production: &keyed_production,
                    primary_key: &["ID".into()],
                    safety: &SafetyConfig::default(),
                },
            )
            .await?;
            assert_eq!(keyed.status, QueryCheckStatus::Findings);
            let comparison = keyed.comparison.unwrap();
            assert_eq!(comparison.candidate_only_rows, 1);
            assert_eq!(comparison.production_only_rows, 1);
            assert_eq!(comparison.changed_rows, 1);
            assert_eq!(comparison.column_mismatches[0].column, "VALUE");
            assert_eq!(comparison.column_mismatches[0].rows, 1);
            Ok::<(), anyhow::Error>(())
        }
        .await;

        let second_cleanup = client
            .drop_schema(&account.database, &second_schema, &run_schema)
            .await;
        let first_cleanup = client
            .drop_schema(&account.database, &run_schema, &run_schema)
            .await;
        result.unwrap();
        second_cleanup.unwrap();
        first_cleanup.unwrap();
    }
}
