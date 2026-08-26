use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;
use uuid::Uuid;

use crate::{
    auth,
    config::{Config, ProviderConfig},
    databricks::DatabricksClient,
    provider::{QueryResult, dialect},
    snowflake::{SnowflakeClient, quote_identifier},
};

const MARKER_PREFIX: &str = "Temporary schema managed by Embrasure; ";

#[derive(Debug, Serialize)]
pub struct CleanReport {
    pub schema_version: u8,
    pub older_than_hours: u64,
    pub candidates: Vec<CleanedSchema>,
    pub errors: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct CleanedSchema {
    pub account: String,
    pub database: String,
    pub schema: String,
    pub created: String,
    pub removed: bool,
}

pub async fn run(config_path: &Path, older_than_hours: u64, remove: bool) -> CleanReport {
    let mut report = CleanReport {
        schema_version: 1,
        older_than_hours,
        candidates: vec![],
        errors: vec![],
    };
    if let Err(error) = run_inner(config_path, older_than_hours, remove, &mut report).await {
        report.errors.push(format!("{error:#}"));
    }
    report
}

async fn run_inner(
    config_path: &Path,
    older_than_hours: u64,
    remove: bool,
    report: &mut CleanReport,
) -> Result<()> {
    let mut config = Config::load(config_path)?;
    config.resolve_from(config_path)?;
    let resolved = auth::resolve_all(&config).await?;
    for account in &config.accounts {
        let result = async {
            let credential = resolved
                .get(&account.name)
                .context("resolved warehouse credential was not retained")?;
            let query_tag = format!("embrasure:clean:{}", Uuid::new_v4());
            let prefix = dialect(account)
                .normalize_identifier(&config.safety.schema_prefix, None);
            match &account.provider {
                ProviderConfig::Snowflake(provider) => {
                    let client = SnowflakeClient::new(
                        account,
                        credential,
                        query_tag,
                        config.safety.statement_timeout_seconds,
                    )?;
                    let query = format!(
                        "SELECT SCHEMA_NAME, COMMENT, TO_VARCHAR(CREATED) FROM {}.INFORMATION_SCHEMA.SCHEMATA WHERE SCHEMA_NAME LIKE {} ESCAPE '!' AND COMMENT LIKE 'Temporary schema managed by Embrasure;%' AND CREATED < DATEADD('hour', -{}, CURRENT_TIMESTAMP()) ORDER BY CREATED, SCHEMA_NAME",
                        quote_identifier(&provider.database),
                        quote_string(&format!("{prefix}!_%")),
                        older_than_hours,
                    );
                    for candidate in managed_candidates(client.execute(&query).await?, &prefix) {
                        if remove {
                            client
                                .drop_marked_schema(&provider.database, &candidate.schema, &prefix)
                                .await?;
                        }
                        report.candidates.push(candidate.finish(
                            &account.name,
                            &provider.database,
                            remove,
                        ));
                    }
                }
                ProviderConfig::Databricks(provider) => {
                    let client = DatabricksClient::new(
                        account,
                        credential,
                        query_tag,
                        config.safety.statement_timeout_seconds,
                    )?;
                    let rows = client
                        .stale_managed_schemas(
                            &provider.catalog,
                            &prefix,
                            older_than_hours,
                        )
                        .await?;
                    for candidate in managed_candidates(rows, &prefix) {
                        if remove {
                            client
                                .drop_marked_schema(&provider.catalog, &candidate.schema, &prefix)
                                .await?;
                        }
                        report.candidates.push(candidate.finish(
                            &account.name,
                            &provider.catalog,
                            remove,
                        ));
                    }
                }
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if let Err(error) = result {
            report
                .errors
                .push(format!("account {}: {error:#}", account.name));
        }
    }
    Ok(())
}

struct ManagedCandidate {
    schema: String,
    created: String,
}

impl ManagedCandidate {
    fn finish(self, account: &str, database: &str, removed: bool) -> CleanedSchema {
        CleanedSchema {
            account: account.to_owned(),
            database: database.to_owned(),
            schema: self.schema,
            created: self.created,
            removed,
        }
    }
}

fn managed_candidates(result: QueryResult, prefix: &str) -> Vec<ManagedCandidate> {
    result
        .rows
        .into_iter()
        .filter_map(|row| {
            let schema = row.first()?.as_deref()?;
            let comment = row.get(1)?.as_deref()?;
            if !is_managed_prefix(schema, prefix) || parse_ownership_marker(comment).is_none() {
                return None;
            }
            Some(ManagedCandidate {
                schema: schema.to_owned(),
                created: row
                    .get(2)
                    .and_then(Option::as_deref)
                    .unwrap_or("unknown")
                    .to_owned(),
            })
        })
        .collect()
}

pub(crate) fn parse_ownership_marker(comment: &str) -> Option<Uuid> {
    let tag = comment.strip_prefix(MARKER_PREFIX)?;
    let mut parts = tag.split(':');
    if parts.next()? != "embrasure" || parts.next()?.is_empty() {
        return None;
    }
    let run_id = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    Uuid::parse_str(run_id).ok()
}

pub(crate) fn is_managed_prefix(schema: &str, prefix: &str) -> bool {
    schema
        .to_ascii_uppercase()
        .starts_with(&format!("{}_", prefix.to_ascii_uppercase()))
}

fn quote_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

impl CleanReport {
    pub fn human(&self) -> String {
        let mut output = String::new();
        if self.candidates.is_empty() && self.errors.is_empty() {
            output.push_str("No managed temporary schemas matched.\n");
        }
        for item in &self.candidates {
            let action = if item.removed {
                "removed"
            } else {
                "would remove"
            };
            output.push_str(&format!(
                "{action} {}.{} ({}, account {})\n",
                item.database, item.schema, item.created, item.account
            ));
        }
        for error in &self.errors {
            output.push_str(&format!("error: {error}\n"));
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ownership_markers_and_prefixes_are_strict() {
        assert!(parse_ownership_marker(
            "Temporary schema managed by Embrasure; embrasure:0.4.0:550e8400-e29b-41d4-a716-446655440000"
        )
        .is_some());
        assert!(parse_ownership_marker("Temporary schema managed by Embrasure; other").is_none());
        assert!(is_managed_prefix("EMBRASURE_CHECK_ABC", "EMBRASURE_CHECK"));
        assert!(!is_managed_prefix("EMBRASURE_CHECK", "EMBRASURE_CHECK"));
        assert!(!is_managed_prefix("PROD", "EMBRASURE_CHECK"));
    }
}
