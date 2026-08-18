use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;
use uuid::Uuid;

use crate::{
    auth,
    config::Config,
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
            let client = SnowflakeClient::new(
                account,
                resolved
                    .get(&account.name)
                    .context("resolved Snowflake credential was not retained")?,
                format!("embrasure:clean:{}", Uuid::new_v4()),
                config.safety.statement_timeout_seconds,
            )?;
            let prefix = config.safety.schema_prefix.to_ascii_uppercase();
            let query = format!(
                "SELECT SCHEMA_NAME, COMMENT, TO_VARCHAR(CREATED) FROM {}.INFORMATION_SCHEMA.SCHEMATA WHERE SCHEMA_NAME LIKE {} ESCAPE '!' AND COMMENT LIKE 'Temporary schema managed by Embrasure;%' AND CREATED < DATEADD('hour', -{}, CURRENT_TIMESTAMP()) ORDER BY CREATED, SCHEMA_NAME",
                quote_identifier(&account.database),
                quote_string(&format!("{prefix}!_%")),
                older_than_hours,
            );
            let rows = client.execute(&query).await?;
            for row in rows.rows {
                let Some(schema) = row.first().and_then(Option::as_deref) else {
                    continue;
                };
                let Some(comment) = row.get(1).and_then(Option::as_deref) else {
                    continue;
                };
                let created = row
                    .get(2)
                    .and_then(Option::as_deref)
                    .unwrap_or("unknown")
                    .to_owned();
                if !is_managed_prefix(schema, &prefix) || parse_ownership_marker(comment).is_none()
                {
                    continue;
                }
                if remove {
                    client
                        .drop_marked_schema(&account.database, schema, &prefix)
                        .await?;
                }
                report.candidates.push(CleanedSchema {
                    account: account.name.clone(),
                    database: account.database.clone(),
                    schema: schema.to_owned(),
                    created,
                    removed: remove,
                });
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
