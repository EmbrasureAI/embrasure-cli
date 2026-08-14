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
use tokio::time::sleep;
use uuid::Uuid;

use crate::{
    auth::{ResolvedAuth, account_host},
    config::AccountConfig,
};

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
        self.consume_response(response, request_id).await
    }

    async fn consume_response(
        &self,
        mut response: reqwest::Response,
        request_id: Uuid,
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
                bail!(
                    "Snowflake statement failed (HTTP {status}, code {}): {}",
                    body.code.as_deref().unwrap_or("unknown"),
                    body.message.as_deref().unwrap_or("unknown error")
                );
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

    pub async fn create_schema(&self, schema: &str) -> Result<()> {
        self.execute(&format!(
            "CREATE TRANSIENT SCHEMA IF NOT EXISTS {}.{} COMMENT = 'Temporary schema managed by embrasure-check'",
            quote_identifier(&self.account.database), quote_identifier(schema),
        )).await?;
        Ok(())
    }

    pub async fn drop_schema(&self, schema: &str, expected_prefix: &str) -> Result<()> {
        let prefix = format!("{}_", expected_prefix.to_ascii_uppercase());
        if !schema.to_ascii_uppercase().starts_with(&prefix) {
            bail!(
                "refusing to drop schema {schema}: it does not start with safety prefix {expected_prefix}_"
            );
        }
        self.execute(&format!(
            "DROP SCHEMA IF EXISTS {}.{}",
            quote_identifier(&self.account.database),
            quote_identifier(schema),
        ))
        .await?;
        Ok(())
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
            header::HeaderValue::from_static(concat!(
                "embrasure-check/",
                env!("CARGO_PKG_VERSION")
            )),
        );
        headers.insert(
            "X-Snowflake-Authorization-Token-Type",
            header::HeaderValue::from_static(self.token_type),
        );
        Ok(headers)
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AuthConfig;
    use openssl::{rsa::Rsa, sign::Verifier};

    #[test]
    fn identifiers_are_always_quoted() {
        assert_eq!(quote_identifier("odd\"name"), "\"odd\"\"name\"");
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
}
