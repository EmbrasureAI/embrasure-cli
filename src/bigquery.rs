use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use reqwest::{Client, RequestBuilder, Response, StatusCode, Url, header};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::sleep;
use uuid::Uuid;

use crate::{
    auth::{BigQueryResolvedAuth, ResolvedAuth},
    clean,
    config::{AccountConfig, BigQueryConfig},
    provider::{
        ProviderFuture, QueryExecutor, QueryResult, Relation, ResultColumn, SqlDialect,
        WarehouseProvider, is_managed_schema,
    },
};

const API_ROOT: &str = "https://bigquery.googleapis.com/bigquery/v2/";
const BIGQUERY_SCOPE: &str = "https://www.googleapis.com/auth/bigquery";
const MANAGED_LABEL: &str = "embrasure_managed";

#[derive(Clone)]
pub struct BigQueryClient {
    http: Client,
    auth: Arc<dyn gcp_auth::TokenProvider>,
    account: BigQueryConfig,
    query_tag: String,
    timeout_seconds: u64,
    api_root: Url,
}

impl BigQueryClient {
    pub fn new(
        account: &AccountConfig,
        auth: &ResolvedAuth,
        query_tag: String,
        timeout_seconds: u64,
    ) -> Result<Self> {
        let account = account
            .bigquery()
            .context("BigQuery client requires BigQuery account configuration")?;
        let ResolvedAuth::BigQuery(BigQueryResolvedAuth { token_provider }) = auth else {
            bail!("BigQuery client received credentials for another provider");
        };
        let auth = token_provider
            .clone()
            .context("BigQuery Application Default Credentials were not resolved")?;
        Ok(Self {
            http: Client::builder()
                .timeout(Duration::from_secs(timeout_seconds + 30))
                .build()?,
            auth,
            account: account.clone(),
            query_tag,
            timeout_seconds,
            api_root: Url::parse(API_ROOT)?,
        })
    }

    pub async fn execute(&self, statement: &str) -> Result<QueryResult> {
        let started = Instant::now();
        let timeout_ms = self.timeout_seconds.saturating_mul(1_000);
        let url = self.api_url(&["projects", &self.account.project, "queries"])?;
        let response = self
            .send(self.http.post(url).json(&json!({
                "query": format!("/* {} */\n{statement}", self.query_tag),
                "useLegacySql": false,
                "location": self.account.location,
                "defaultDataset": {
                    "projectId": self.account.project,
                    "datasetId": self.account.production_schema,
                },
                "timeoutMs": timeout_ms.min(200_000),
                "jobTimeoutMs": timeout_ms.to_string(),
                "maxResults": 10_000,
                "requestId": Uuid::new_v4().to_string(),
                "labels": { "embrasure_managed": "true" },
            })))
            .await?;
        let mut page: QueryResponse = parse_response(response, "BigQuery query request").await?;
        let job = page
            .job_reference
            .clone()
            .context("BigQuery query response omitted its job reference")?;

        while !page.job_complete {
            if started.elapsed() >= Duration::from_secs(self.timeout_seconds) {
                let _ = self.cancel_job(&job).await;
                bail!(
                    "BigQuery job exceeded {} seconds; cancellation was requested",
                    self.timeout_seconds
                );
            }
            sleep(Duration::from_millis(500)).await;
            page = self.query_results(&job, None).await?;
        }
        fail_for_query_errors(&page.errors)?;

        let mut columns = page.schema.take().map(table_columns).unwrap_or_default();
        let mut rows = parse_rows(std::mem::take(&mut page.rows));
        let mut page_token = page.page_token.take();
        while let Some(token) = page_token {
            let mut next = self.query_results(&job, Some(&token)).await?;
            if !next.job_complete {
                bail!("BigQuery returned an incomplete result page for a completed job");
            }
            fail_for_query_errors(&next.errors)?;
            if columns.is_empty() {
                columns = next.schema.take().map(table_columns).unwrap_or_default();
            }
            rows.extend(parse_rows(std::mem::take(&mut next.rows)));
            page_token = next.page_token.take();
        }
        Ok(QueryResult { columns, rows })
    }

    pub async fn create_schema(&self, project: &str, schema: &str) -> Result<()> {
        let url = self.api_url(&["projects", project, "datasets"])?;
        let response = self
            .send(self.http.post(url).json(&json!({
                "datasetReference": {
                    "projectId": project,
                    "datasetId": schema,
                },
                "location": self.account.location,
                "description": self.schema_ownership_marker(),
                "labels": { (MANAGED_LABEL): "true" },
                "access": [],
            })))
            .await?;
        ensure_success(response, "BigQuery dataset creation").await
    }

    pub async fn copy_table(&self, source: &Relation, target: &Relation) -> Result<()> {
        self.execute(&copy_table_statement(source, target)).await?;
        Ok(())
    }

    pub async fn seed_table(&self, source: &Relation, target: &Relation) -> Result<()> {
        self.execute(&seed_table_statement(source, target)).await?;
        Ok(())
    }

    pub async fn drop_schema(&self, project: &str, schema: &str, run_schema: &str) -> Result<()> {
        if !is_managed_schema(SqlDialect::BigQuery, schema, run_schema) {
            bail!("refusing to drop dataset {schema}: it is not owned by this run ({run_schema})");
        }
        let Some(dataset) = self.dataset(project, schema).await? else {
            return Ok(());
        };
        if dataset.description.as_deref() != Some(&self.schema_ownership_marker())
            || dataset.labels.get(MANAGED_LABEL).map(String::as_str) != Some("true")
        {
            bail!(
                "refusing to drop dataset {project}.{schema}: its ownership marker does not match this run"
            );
        }
        self.drop_dataset(project, schema).await
    }

    pub async fn stale_managed_schemas(
        &self,
        project: &str,
        prefix: &str,
        older_than_hours: u64,
    ) -> Result<QueryResult> {
        let mut datasets = Vec::new();
        let mut page_token = None;
        loop {
            let url = self.api_url(&["projects", project, "datasets"])?;
            let mut request = self.http.get(url).query(&[
                ("all", "true"),
                ("filter", "labels.embrasure_managed:true"),
                ("maxResults", "1000"),
            ]);
            if let Some(token) = page_token.as_deref() {
                request = request.query(&[("pageToken", token)]);
            }
            let response = self.send(request).await?;
            let page: DatasetList = parse_response(response, "BigQuery dataset listing").await?;
            datasets.extend(page.datasets);
            page_token = page.next_page_token;
            if page_token.is_none() {
                break;
            }
        }

        let cutoff = SystemTime::now()
            .checked_sub(Duration::from_secs(older_than_hours.saturating_mul(3_600)))
            .unwrap_or(UNIX_EPOCH)
            .duration_since(UNIX_EPOCH)?
            .as_millis();
        let mut rows = Vec::new();
        for item in datasets {
            let Some(reference) = item.dataset_reference else {
                continue;
            };
            if !clean::is_managed_prefix(&reference.dataset_id, prefix)
                || !item
                    .location
                    .as_deref()
                    .is_some_and(|location| location.eq_ignore_ascii_case(&self.account.location))
            {
                continue;
            }
            let Some(dataset) = self.dataset(project, &reference.dataset_id).await? else {
                continue;
            };
            let Some(marker) = dataset.description.as_deref() else {
                continue;
            };
            if dataset.labels.get(MANAGED_LABEL).map(String::as_str) != Some("true")
                || clean::parse_ownership_marker(marker).is_none()
            {
                continue;
            }
            let created_ms = dataset
                .creation_time
                .as_deref()
                .and_then(|value| value.parse::<u128>().ok())
                .unwrap_or(u128::MAX);
            if created_ms >= cutoff {
                continue;
            }
            let created = i64::try_from(created_ms)
                .ok()
                .and_then(DateTime::<Utc>::from_timestamp_millis)
                .map(|value| value.to_rfc3339())
                .unwrap_or_else(|| "unknown".into());
            rows.push(vec![
                Some(reference.dataset_id),
                Some(marker.to_owned()),
                Some(created),
            ]);
        }
        rows.sort_by(|left, right| left[2].cmp(&right[2]).then(left[0].cmp(&right[0])));
        Ok(QueryResult {
            columns: vec![
                ResultColumn {
                    name: "schema_name".into(),
                    data_type: "STRING".into(),
                },
                ResultColumn {
                    name: "description".into(),
                    data_type: "STRING".into(),
                },
                ResultColumn {
                    name: "created".into(),
                    data_type: "STRING".into(),
                },
            ],
            rows,
        })
    }

    pub async fn drop_marked_schema(
        &self,
        project: &str,
        schema: &str,
        prefix: &str,
    ) -> Result<()> {
        if !clean::is_managed_prefix(schema, prefix) {
            bail!("refusing to drop dataset {schema}: it is outside the managed prefix {prefix}");
        }
        let Some(dataset) = self.dataset(project, schema).await? else {
            return Ok(());
        };
        if dataset.labels.get(MANAGED_LABEL).map(String::as_str) != Some("true")
            || dataset
                .description
                .as_deref()
                .and_then(clean::parse_ownership_marker)
                .is_none()
        {
            bail!(
                "refusing to drop dataset {project}.{schema}: its Embrasure ownership marker is invalid"
            );
        }
        self.drop_dataset(project, schema).await
    }

    async fn query_results(
        &self,
        job: &JobReference,
        page_token: Option<&str>,
    ) -> Result<QueryResponse> {
        let url = self.api_url(&["projects", &job.project_id, "queries", &job.job_id])?;
        let mut request = self.http.get(url).query(&[
            ("location", job.location.as_str()),
            ("timeoutMs", "10000"),
            ("maxResults", "10000"),
        ]);
        if let Some(token) = page_token {
            request = request.query(&[("pageToken", token)]);
        }
        let response = self.send(request).await?;
        parse_response(response, "BigQuery result request").await
    }

    async fn cancel_job(&self, job: &JobReference) -> Result<()> {
        let url = self.api_url(&["projects", &job.project_id, "jobs", &job.job_id, "cancel"])?;
        let response = self
            .send(self.http.post(url).query(&[("location", &job.location)]))
            .await?;
        ensure_success(response, "BigQuery job cancellation").await
    }

    async fn dataset(&self, project: &str, schema: &str) -> Result<Option<DatasetResource>> {
        let url = self.api_url(&["projects", project, "datasets", schema])?;
        let response = self.send(self.http.get(url)).await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        parse_response(response, "BigQuery dataset lookup")
            .await
            .map(Some)
    }

    async fn drop_dataset(&self, project: &str, schema: &str) -> Result<()> {
        let url = self.api_url(&["projects", project, "datasets", schema])?;
        let response = self
            .send(self.http.delete(url).query(&[("deleteContents", "true")]))
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(());
        }
        ensure_success(response, "BigQuery dataset deletion").await
    }

    async fn send(&self, request: RequestBuilder) -> Result<Response> {
        let token = self
            .auth
            .token(&[BIGQUERY_SCOPE])
            .await
            .context("could not obtain a BigQuery access token")?;
        request
            .bearer_auth(token.as_str())
            .header(
                header::USER_AGENT,
                concat!("embrasure/", env!("CARGO_PKG_VERSION")),
            )
            .send()
            .await
            .context("BigQuery API request failed")
    }

    fn api_url(&self, segments: &[&str]) -> Result<Url> {
        joined_api_url(&self.api_root, segments)
    }

    fn schema_ownership_marker(&self) -> String {
        format!("Temporary schema managed by Embrasure; {}", self.query_tag)
    }
}

fn joined_api_url(root: &Url, segments: &[&str]) -> Result<Url> {
    let mut url = root.clone();
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("BigQuery API endpoint cannot contain path segments"))?
        .pop_if_empty()
        .extend(segments);
    Ok(url)
}

impl QueryExecutor for BigQueryClient {
    fn dialect(&self) -> SqlDialect {
        SqlDialect::BigQuery
    }

    fn execute<'a>(&'a self, statement: &'a str) -> ProviderFuture<'a, QueryResult> {
        Box::pin(BigQueryClient::execute(self, statement))
    }
}

impl WarehouseProvider for BigQueryClient {
    fn create_schema<'a>(&'a self, project: &'a str, schema: &'a str) -> ProviderFuture<'a, ()> {
        Box::pin(BigQueryClient::create_schema(self, project, schema))
    }

    fn copy_table<'a>(
        &'a self,
        source: &'a Relation,
        target: &'a Relation,
    ) -> ProviderFuture<'a, ()> {
        Box::pin(BigQueryClient::copy_table(self, source, target))
    }

    fn seed_table<'a>(
        &'a self,
        source: &'a Relation,
        target: &'a Relation,
    ) -> ProviderFuture<'a, ()> {
        Box::pin(BigQueryClient::seed_table(self, source, target))
    }

    fn drop_schema<'a>(
        &'a self,
        project: &'a str,
        schema: &'a str,
        run_schema: &'a str,
    ) -> ProviderFuture<'a, ()> {
        Box::pin(BigQueryClient::drop_schema(
            self, project, schema, run_schema,
        ))
    }
}

fn copy_table_statement(source: &Relation, target: &Relation) -> String {
    let dialect = SqlDialect::BigQuery;
    format!(
        "CREATE TABLE {} CLONE {}",
        target.sql(dialect),
        source.sql(dialect)
    )
}

fn seed_table_statement(source: &Relation, target: &Relation) -> String {
    let dialect = SqlDialect::BigQuery;
    format!(
        "CREATE TABLE {} COPY {}",
        target.sql(dialect),
        source.sql(dialect)
    )
}

async fn ensure_success(response: Response, label: &str) -> Result<()> {
    let status = response.status();
    let body = response.bytes().await?;
    if status.is_success() {
        return Ok(());
    }
    bail!("{label} failed (HTTP {status}): {}", api_error(&body));
}

async fn parse_response<T: for<'de> Deserialize<'de>>(
    response: Response,
    label: &str,
) -> Result<T> {
    let status = response.status();
    let body = response.bytes().await?;
    if !status.is_success() {
        bail!("{label} failed (HTTP {status}): {}", api_error(&body));
    }
    serde_json::from_slice(&body).with_context(|| format!("{label} returned invalid JSON"))
}

fn api_error(body: &[u8]) -> String {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| String::from_utf8_lossy(body).trim().to_owned())
}

fn fail_for_query_errors(errors: &[ApiError]) -> Result<()> {
    if errors.is_empty() {
        return Ok(());
    }
    let detail = errors
        .iter()
        .map(|error| match error.reason.as_deref() {
            Some(reason) => format!("{reason}: {}", error.message),
            None => error.message.clone(),
        })
        .collect::<Vec<_>>()
        .join("; ");
    bail!("BigQuery job failed: {detail}")
}

fn table_columns(schema: TableSchema) -> Vec<ResultColumn> {
    schema
        .fields
        .into_iter()
        .map(|field| {
            let data_type = field.data_type();
            ResultColumn {
                name: field.name,
                data_type,
            }
        })
        .collect()
}

fn parse_rows(rows: Vec<TableRow>) -> Vec<Vec<Option<String>>> {
    rows.into_iter()
        .map(|row| {
            row.fields
                .into_iter()
                .map(|cell| cell_text(cell.value))
                .collect()
        })
        .collect()
}

fn cell_text(value: Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(value) => Some(value),
        Value::Bool(value) => Some(value.to_string()),
        Value::Number(value) => Some(value.to_string()),
        value => serde_json::to_string(&value).ok(),
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueryResponse {
    #[serde(default)]
    schema: Option<TableSchema>,
    #[serde(default)]
    job_reference: Option<JobReference>,
    #[serde(default)]
    page_token: Option<String>,
    #[serde(default)]
    job_complete: bool,
    #[serde(default)]
    rows: Vec<TableRow>,
    #[serde(default)]
    errors: Vec<ApiError>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JobReference {
    project_id: String,
    job_id: String,
    location: String,
}

#[derive(Debug, Clone, Deserialize)]
struct TableSchema {
    #[serde(default)]
    fields: Vec<TableField>,
}

#[derive(Debug, Clone, Deserialize)]
struct TableField {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    fields: Vec<TableField>,
}

impl TableField {
    fn data_type(&self) -> String {
        let kind = if self.kind.eq_ignore_ascii_case("RECORD") {
            if self.fields.is_empty() {
                "STRUCT".into()
            } else {
                format!(
                    "STRUCT<{}>",
                    self.fields
                        .iter()
                        .map(|field| format!("{} {}", field.name, field.data_type()))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        } else {
            self.kind.clone()
        };
        if self.mode.as_deref() == Some("REPEATED") {
            format!("ARRAY<{kind}>")
        } else {
            kind
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct TableRow {
    #[serde(rename = "f", default)]
    fields: Vec<TableCell>,
}

#[derive(Debug, Clone, Deserialize)]
struct TableCell {
    #[serde(rename = "v")]
    value: Value,
}

#[derive(Debug, Clone, Deserialize)]
struct ApiError {
    #[serde(default)]
    reason: Option<String>,
    message: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DatasetResource {
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    labels: BTreeMap<String, String>,
    #[serde(default)]
    creation_time: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DatasetList {
    #[serde(default)]
    datasets: Vec<DatasetListItem>,
    #[serde(default)]
    next_page_token: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DatasetListItem {
    #[serde(default)]
    dataset_reference: Option<DatasetReference>,
    #[serde(default)]
    location: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DatasetReference {
    dataset_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_clone_and_copy_operations() {
        let source = Relation {
            database: "analytics-prod".into(),
            schema: "prod".into(),
            identifier: "orders".into(),
        };
        let target = Relation {
            database: "analytics-dev".into(),
            schema: "check".into(),
            identifier: "orders".into(),
        };
        assert_eq!(
            copy_table_statement(&source, &target),
            "CREATE TABLE `analytics-dev`.`check`.`orders` CLONE `analytics-prod`.`prod`.`orders`"
        );
        assert_eq!(
            seed_table_statement(&source, &target),
            "CREATE TABLE `analytics-dev`.`check`.`orders` COPY `analytics-prod`.`prod`.`orders`"
        );
    }

    #[test]
    fn api_urls_do_not_keep_the_root_trailing_slash_as_an_empty_segment() {
        let root = Url::parse(API_ROOT).unwrap();
        let url = joined_api_url(&root, &["projects", "analytics-prod", "queries"]).unwrap();
        assert_eq!(
            url.as_str(),
            "https://bigquery.googleapis.com/bigquery/v2/projects/analytics-prod/queries"
        );
    }

    #[test]
    fn decodes_scalar_rows_and_marks_complex_columns() {
        let response: QueryResponse = serde_json::from_str(
            r#"{
              "jobReference":{"projectId":"p","jobId":"j","location":"US"},
              "jobComplete":true,
              "schema":{"fields":[
                {"name":"count","type":"INT64","mode":"NULLABLE"},
                {"name":"tags","type":"STRING","mode":"REPEATED"},
                {"name":"metadata","type":"RECORD","mode":"NULLABLE"}
              ]},
              "rows":[{"f":[{"v":"42"},{"v":[{"v":"a"}]},{"v":null}]}]
            }"#,
        )
        .unwrap();
        let columns = table_columns(response.schema.unwrap());
        assert_eq!(columns[0].data_type, "INT64");
        assert_eq!(columns[1].data_type, "ARRAY<STRING>");
        assert_eq!(columns[2].data_type, "STRUCT");
        assert_eq!(
            parse_rows(response.rows),
            vec![vec![Some("42".into()), Some(r#"[{"v":"a"}]"#.into()), None]]
        );
    }
}
