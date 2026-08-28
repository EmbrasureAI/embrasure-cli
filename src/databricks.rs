use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use reqwest::{Client, StatusCode, header};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::{sleep, timeout};

use crate::{
    auth::{DatabricksResolvedAuth, ResolvedAuth},
    clean,
    config::{AccountConfig, DatabricksConfig},
    provider::{
        ProviderFuture, QueryExecutor, QueryResult, Relation, ResultColumn, SqlDialect,
        WarehouseExecution, WarehouseProvider, is_managed_schema,
    },
};

#[derive(Clone)]
pub struct DatabricksClient {
    http: Client,
    workspace_url: String,
    endpoint: String,
    token: String,
    account: DatabricksConfig,
    query_tag: String,
    timeout_seconds: u64,
    account_name: String,
    executions: Arc<Mutex<Vec<WarehouseExecution>>>,
}

impl DatabricksClient {
    pub fn new(
        account: &AccountConfig,
        auth: &ResolvedAuth,
        query_tag: String,
        timeout_seconds: u64,
    ) -> Result<Self> {
        let account_name = account.name.clone();
        let account = account
            .databricks()
            .context("Databricks client requires Databricks account configuration")?;
        let ResolvedAuth::Databricks(DatabricksResolvedAuth::Token { token }) = auth else {
            bail!("Databricks client received credentials for another provider");
        };
        let workspace_url = account.workspace_url();
        Ok(Self {
            http: Client::builder()
                .timeout(Duration::from_secs(timeout_seconds + 30))
                .build()?,
            endpoint: format!("{workspace_url}/api/2.0/sql/statements"),
            workspace_url,
            token: token.clone(),
            account: account.clone(),
            query_tag,
            timeout_seconds,
            account_name,
            executions: Arc::new(Mutex::new(vec![])),
        })
    }

    pub async fn execute(&self, statement: &str) -> Result<QueryResult> {
        timeout(Duration::from_secs(self.timeout_seconds + 30), async {
            let started = Instant::now();
            let response = self
                .http
                .post(&self.endpoint)
                .headers(self.headers()?)
                .json(&json!({
                    "statement": format!("/* {} */\n{statement}", self.query_tag),
                    "warehouse_id": self.account.warehouse_id(),
                    "catalog": self.account.catalog,
                    "schema": self.account.production_schema,
                    "format": "JSON_ARRAY",
                    "disposition": "INLINE",
                    "wait_timeout": "0s",
                    "on_wait_timeout": "CONTINUE"
                }))
                .send()
                .await
                .context("Databricks Statement Execution request failed")?;
            let mut body = parse_response(response).await?;
            if let Some(execution_id) = body.statement_id.as_deref()
                && let Ok(mut executions) = self.executions.lock()
            {
                executions.push(WarehouseExecution::databricks(
                    &self.account_name,
                    &self.workspace_url,
                    execution_id,
                ));
            }
            loop {
                match body.status.state.as_str() {
                    "PENDING" | "RUNNING" => {
                        let statement_id = body
                            .statement_id
                            .as_deref()
                            .context("Databricks response omitted statement_id")?;
                        if started.elapsed() >= Duration::from_secs(self.timeout_seconds) {
                            let _ = self
                                .http
                                .post(format!("{}/{statement_id}/cancel", self.endpoint))
                                .headers(self.headers()?)
                                .send()
                                .await;
                            bail!(
                                "Databricks statement exceeded {} seconds; cancellation was requested",
                                self.timeout_seconds
                            );
                        }
                        sleep(Duration::from_millis(500)).await;
                        body = parse_response(
                            self.http
                                .get(format!("{}/{}", self.endpoint, statement_id))
                                .headers(self.headers()?)
                                .send()
                                .await
                                .context("Databricks statement polling failed")?,
                        )
                        .await?;
                    }
                    "SUCCEEDED" => return self.result(body).await,
                    "FAILED" | "CANCELED" | "CLOSED" => {
                        let error = body.status.error.as_ref();
                        bail!(
                            "Databricks statement {}: {}",
                            body.status.state.to_ascii_lowercase(),
                            error
                                .and_then(|value| value.message.as_deref())
                                .or(body.message.as_deref())
                                .unwrap_or("unknown error")
                        );
                    }
                    state => bail!("Databricks returned unknown statement state {state}"),
                }
            }
        })
        .await
        .with_context(|| {
            format!(
                "Databricks statement exceeded {} seconds",
                self.timeout_seconds + 30
            )
        })?
    }

    async fn result(&self, body: StatementResponse) -> Result<QueryResult> {
        let columns = body
            .manifest
            .and_then(|manifest| manifest.schema)
            .map(|schema| {
                schema
                    .columns
                    .into_iter()
                    .map(|column| ResultColumn {
                        name: column.name,
                        data_type: column.type_text.or(column.type_name).unwrap_or_default(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut chunk = body.result.unwrap_or_default();
        let mut rows = parse_rows(std::mem::take(&mut chunk.data_array));
        while let Some(link) = chunk.next_chunk_internal_link.take() {
            if !link.starts_with("/api/2.0/sql/statements/") {
                bail!("Databricks returned an unsafe result chunk link");
            }
            let response = self
                .http
                .get(format!("{}{}", self.workspace_url, link))
                .headers(self.headers()?)
                .send()
                .await
                .context("Databricks result chunk request failed")?;
            let status = response.status();
            if !status.is_success() {
                bail!("Databricks result chunk failed (HTTP {status})");
            }
            chunk = response
                .json()
                .await
                .context("Databricks returned an invalid result chunk")?;
            rows.extend(parse_rows(std::mem::take(&mut chunk.data_array)));
        }
        Ok(QueryResult { columns, rows })
    }

    pub async fn create_schema(&self, catalog: &str, schema: &str) -> Result<()> {
        let dialect = SqlDialect::Databricks;
        self.execute(&format!(
            "CREATE SCHEMA {}.{} COMMENT {}",
            dialect.quote_identifier(catalog),
            dialect.quote_identifier(schema),
            quote_string(&self.schema_ownership_marker())
        ))
        .await?;
        Ok(())
    }

    pub async fn copy_table(&self, source: &Relation, target: &Relation) -> Result<()> {
        self.execute(&copy_table_statement(source, target)).await?;
        Ok(())
    }

    pub async fn seed_table(&self, source: &Relation, target: &Relation) -> Result<()> {
        self.execute(&seed_table_statement(source, target)).await?;
        Ok(())
    }

    pub async fn drop_schema(&self, catalog: &str, schema: &str, run_schema: &str) -> Result<()> {
        if !is_managed_schema(SqlDialect::Databricks, schema, run_schema) {
            bail!("refusing to drop schema {schema}: it is not owned by this run ({run_schema})");
        }
        let Some(comment) = self.schema_comment(catalog, schema).await? else {
            return Ok(());
        };
        if comment != self.schema_ownership_marker() {
            bail!(
                "refusing to drop schema {catalog}.{schema}: its ownership marker does not match this run"
            );
        }
        self.drop_schema_cascade(catalog, schema).await
    }

    pub async fn stale_managed_schemas(
        &self,
        catalog: &str,
        prefix: &str,
        older_than_hours: u64,
    ) -> Result<QueryResult> {
        let dialect = SqlDialect::Databricks;
        self.execute(&format!(
            "SELECT SCHEMA_NAME, COMMENT, CAST(CREATED AS STRING) FROM {}.INFORMATION_SCHEMA.SCHEMATA \
             WHERE SCHEMA_NAME LIKE {} ESCAPE '!' \
             AND COMMENT LIKE 'Temporary schema managed by Embrasure;%' \
             AND CREATED < CURRENT_TIMESTAMP() - INTERVAL {older_than_hours} HOURS \
             ORDER BY CREATED, SCHEMA_NAME",
            dialect.quote_identifier(catalog),
            quote_string(&format!("{prefix}!_%"))
        ))
        .await
    }

    pub async fn drop_marked_schema(
        &self,
        catalog: &str,
        schema: &str,
        prefix: &str,
    ) -> Result<()> {
        if !clean::is_managed_prefix(schema, prefix) {
            bail!("refusing to drop schema {schema}: it is outside the managed prefix {prefix}");
        }
        let Some(comment) = self.schema_comment(catalog, schema).await? else {
            return Ok(());
        };
        if clean::parse_ownership_marker(&comment).is_none() {
            bail!(
                "refusing to drop schema {catalog}.{schema}: its Embrasure ownership marker is invalid"
            );
        }
        self.drop_schema_cascade(catalog, schema).await
    }

    async fn schema_comment(&self, catalog: &str, schema: &str) -> Result<Option<String>> {
        let dialect = SqlDialect::Databricks;
        let result = self
            .execute(&format!(
                "SELECT COMMENT FROM {}.INFORMATION_SCHEMA.SCHEMATA WHERE CATALOG_NAME = {} AND SCHEMA_NAME = {}",
                dialect.quote_identifier(catalog),
                quote_string(&dialect.normalize_identifier(catalog, None)),
                quote_string(&dialect.normalize_identifier(schema, None))
            ))
            .await?;
        Ok(result
            .rows
            .first()
            .and_then(|row| row.first())
            .cloned()
            .flatten())
    }

    async fn drop_schema_cascade(&self, catalog: &str, schema: &str) -> Result<()> {
        let dialect = SqlDialect::Databricks;
        self.execute(&format!(
            "DROP SCHEMA IF EXISTS {}.{} CASCADE",
            dialect.quote_identifier(catalog),
            dialect.quote_identifier(schema)
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
        Ok(headers)
    }
}

impl QueryExecutor for DatabricksClient {
    fn dialect(&self) -> SqlDialect {
        SqlDialect::Databricks
    }

    fn execute<'a>(&'a self, statement: &'a str) -> ProviderFuture<'a, QueryResult> {
        Box::pin(DatabricksClient::execute(self, statement))
    }
}

impl WarehouseProvider for DatabricksClient {
    fn warehouse_executions(&self) -> Vec<WarehouseExecution> {
        self.executions
            .lock()
            .map(|items| items.clone())
            .unwrap_or_default()
    }

    fn create_schema<'a>(&'a self, catalog: &'a str, schema: &'a str) -> ProviderFuture<'a, ()> {
        Box::pin(DatabricksClient::create_schema(self, catalog, schema))
    }

    fn copy_table<'a>(
        &'a self,
        source: &'a Relation,
        target: &'a Relation,
    ) -> ProviderFuture<'a, ()> {
        Box::pin(DatabricksClient::copy_table(self, source, target))
    }

    fn seed_table<'a>(
        &'a self,
        source: &'a Relation,
        target: &'a Relation,
    ) -> ProviderFuture<'a, ()> {
        Box::pin(DatabricksClient::seed_table(self, source, target))
    }

    fn drop_schema<'a>(
        &'a self,
        catalog: &'a str,
        schema: &'a str,
        run_schema: &'a str,
    ) -> ProviderFuture<'a, ()> {
        Box::pin(DatabricksClient::drop_schema(
            self, catalog, schema, run_schema,
        ))
    }
}

#[derive(Debug, Deserialize)]
struct StatementResponse {
    statement_id: Option<String>,
    #[serde(default)]
    status: StatementStatus,
    manifest: Option<Manifest>,
    result: Option<ResultChunk>,
    message: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct StatementStatus {
    #[serde(default)]
    state: String,
    error: Option<StatementError>,
}

#[derive(Debug, Deserialize)]
struct StatementError {
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Manifest {
    schema: Option<ResultSchema>,
}

#[derive(Debug, Deserialize)]
struct ResultSchema {
    #[serde(default)]
    columns: Vec<ApiColumn>,
}

#[derive(Debug, Deserialize)]
struct ApiColumn {
    name: String,
    type_text: Option<String>,
    type_name: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ResultChunk {
    #[serde(default)]
    data_array: Vec<Vec<Value>>,
    next_chunk_internal_link: Option<String>,
}

async fn parse_response(response: reqwest::Response) -> Result<StatementResponse> {
    let status = response.status();
    let body: StatementResponse = response
        .json()
        .await
        .context("Databricks returned invalid JSON")?;
    if status != StatusCode::OK {
        bail!(
            "Databricks Statement Execution request failed (HTTP {status}): {}",
            body.message.as_deref().unwrap_or("unknown error")
        );
    }
    Ok(body)
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

fn quote_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn copy_table_statement(source: &Relation, target: &Relation) -> String {
    let dialect = SqlDialect::Databricks;
    format!(
        "CREATE TABLE {} SHALLOW CLONE {}",
        target.sql(dialect),
        source.sql(dialect)
    )
}

fn seed_table_statement(source: &Relation, target: &Relation) -> String {
    let dialect = SqlDialect::Databricks;
    dialect.create_table_as(target, &format!("SELECT * FROM {}", source.sql(dialect)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_rows_and_identifiers_match_the_api_contract() {
        let body: StatementResponse = serde_json::from_value(json!({
            "statement_id": "01abc",
            "status": { "state": "SUCCEEDED" },
            "manifest": {
                "schema": {
                    "columns": [{ "name": "id", "type_name": "LONG", "type_text": "BIGINT" }]
                }
            },
            "result": { "data_array": [["1"], [null]] }
        }))
        .unwrap();
        assert_eq!(body.status.state, "SUCCEEDED");
        assert_eq!(
            body.manifest.unwrap().schema.unwrap().columns[0]
                .type_text
                .as_deref(),
            Some("BIGINT")
        );
        assert_eq!(
            parse_rows(body.result.unwrap().data_array),
            vec![vec![Some("1".into())], vec![None]]
        );
        let relation = Relation {
            database: "main".into(),
            schema: "ci".into(),
            identifier: "orders".into(),
        };
        assert_eq!(relation.sql(SqlDialect::Databricks), "`main`.`ci`.`orders`");
        let target = Relation {
            identifier: "candidate".into(),
            ..relation.clone()
        };
        assert_eq!(
            copy_table_statement(&relation, &target),
            "CREATE TABLE `main`.`ci`.`candidate` SHALLOW CLONE `main`.`ci`.`orders`"
        );
        assert_eq!(
            seed_table_statement(&relation, &target),
            "CREATE TABLE `main`.`ci`.`candidate` AS SELECT * FROM `main`.`ci`.`orders`"
        );
    }
}
