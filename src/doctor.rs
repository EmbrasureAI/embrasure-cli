use std::{path::Path, process::Command};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use uuid::Uuid;

use crate::{
    auth,
    config::Config,
    metabase,
    snowflake::{QueryResult, Relation, SnowflakeClient, quote_identifier},
    style::Style,
};

#[derive(Debug, Serialize)]
pub struct DoctorReport {
    pub schema_version: u8,
    pub ready: bool,
    pub checks: Vec<Diagnostic>,
}

#[derive(Debug, Serialize)]
pub struct Diagnostic {
    pub scope: String,
    pub check: String,
    pub status: &'static str,
    pub message: String,
}

pub async fn run(config_path: &Path, write_test: bool) -> DoctorReport {
    let mut report = DoctorReport {
        schema_version: 1,
        ready: true,
        checks: vec![],
    };
    let mut config = match Config::load(config_path) {
        Ok(config) => {
            report.pass(
                "local",
                "config",
                format!("loaded {}", config_path.display()),
            );
            config
        }
        Err(error) => {
            report.fail("local", "config", format!("{error:#}"));
            return report;
        }
    };
    if let Err(error) = config.resolve_from(config_path) {
        report.fail("local", "paths", format!("{error:#}"));
        return report;
    }
    report.tool("git", "git");
    report.tool("dbt", &config.dbt.command);

    for account in &config.accounts {
        let scope = format!("snowflake:{}", account.name);
        let auth_status = match auth::status(account) {
            Ok(status) => status,
            Err(error) => {
                report.fail(&scope, "credential", format!("{error:#}"));
                continue;
            }
        };
        if auth_status.ready {
            report.pass(
                &scope,
                "credential",
                format!("{}: {}", auth_status.method, auth_status.status),
            );
        } else {
            report.fail(&scope, "credential", auth_status.status);
            continue;
        }
        let resolved = match auth::resolve(account).await {
            Ok(auth) => auth,
            Err(error) => {
                report.fail(&scope, "authentication", format!("{error:#}"));
                continue;
            }
        };
        let query_tag = format!("embrasure:doctor:{}", Uuid::new_v4());
        let client = match SnowflakeClient::new(
            account,
            &resolved,
            query_tag,
            config.safety.statement_timeout_seconds,
        ) {
            Ok(client) => client,
            Err(error) => {
                report.fail(&scope, "authentication", format!("{error:#}"));
                continue;
            }
        };
        match client
            .execute("SELECT CURRENT_ACCOUNT(), CURRENT_USER(), CURRENT_ROLE(), CURRENT_WAREHOUSE(), CURRENT_DATABASE()")
            .await
        {
            Ok(result) => match verify_session(account, &result) {
                Ok(identity) => report.pass(&scope, "session", identity),
                Err(error) => {
                    report.fail(&scope, "session", format!("{error:#}"));
                    continue;
                }
            },
            Err(error) => {
                report.fail(&scope, "session", format!("{error:#}"));
                continue;
            }
        }
        let mut clone_source = None;
        let production = format!(
            "SHOW TABLES IN SCHEMA {}.{}",
            quote_identifier(&account.database),
            quote_identifier(&account.production_schema)
        );
        match client.execute(&production).await {
            Ok(tables) => {
                let mut relations = relation_names(&tables);
                clone_source = relations.first().cloned();
                let views = format!(
                    "SHOW VIEWS IN SCHEMA {}.{}",
                    quote_identifier(&account.database),
                    quote_identifier(&account.production_schema)
                );
                match client.execute(&views).await {
                    Ok(views) => relations.extend(relation_names(&views)),
                    Err(error) => {
                        report.fail(&scope, "production_read", format!("{error:#}"));
                        continue;
                    }
                }
                relations.sort();
                relations.dedup();
                if let Some(relation) = relations.first() {
                    let sample = format!(
                        "SELECT * FROM {}.{}.{} LIMIT 0",
                        quote_identifier(&account.database),
                        quote_identifier(&account.production_schema),
                        quote_identifier(relation)
                    );
                    match client.execute(&sample).await {
                        Ok(_) => report.pass(
                            &scope,
                            "production_read",
                            format!(
                                "can read production; {} visible table(s) or view(s)",
                                relations.len()
                            ),
                        ),
                        Err(error) => report.fail(&scope, "production_read", format!("{error:#}")),
                    }
                } else {
                    report.pass(
                        &scope,
                        "production_read",
                        "production schema is visible but contains no tables or views",
                    );
                }
            }
            Err(error) => report.fail(&scope, "production_read", format!("{error:#}")),
        }

        if write_test {
            let schema = format!(
                "{}_DOCTOR_{}",
                config.safety.schema_prefix.to_ascii_uppercase(),
                Uuid::new_v4().simple()
            );
            match client.create_schema(&account.database, &schema).await {
                Ok(()) => {
                    if let Some(identifier) = clone_source {
                        let source = Relation {
                            database: account.database.clone(),
                            schema: account.production_schema.clone(),
                            identifier,
                        };
                        let target = Relation {
                            database: account.database.clone(),
                            schema: schema.clone(),
                            identifier: "EMBRASURE_CLONE_CHECK".into(),
                        };
                        match client.clone_table(&source, &target).await {
                            Ok(()) => report.pass(
                                &scope,
                                "incremental_clone",
                                "can zero-copy clone a production table",
                            ),
                            Err(error) => report.fail(
                                &scope,
                                "incremental_clone",
                                format!(
                                    "cannot clone a production table: {error:#}; confirm the relation type and update grants"
                                ),
                            ),
                        }
                    } else {
                        report.skip(
                            &scope,
                            "incremental_clone",
                            "no production table was available to test (views cannot be cloned)",
                        );
                    }
                    match client
                        .drop_schema(&account.database, &schema, &schema)
                        .await
                    {
                        Ok(()) => report.pass(
                            &scope,
                            "ci_schema_lifecycle",
                            "created and removed a temporary schema",
                        ),
                        Err(error) => report.fail(
                            &scope,
                            "ci_schema_lifecycle",
                            format!("created {schema}, but cleanup failed: {error:#}"),
                        ),
                    }
                }
                Err(error) => report.fail(
                    &scope,
                    "ci_schema_lifecycle",
                    format!("could not create a temporary schema: {error:#}"),
                ),
            }
        } else {
            report.skip(&scope, "ci_schema_lifecycle", "skipped by --read-only");
            report.skip(&scope, "incremental_clone", "skipped by --read-only");
        }
    }

    if let Some(config) = &config.metabase {
        match metabase::check_connection(config).await {
            Ok(count) => report.pass(
                "metabase",
                "api",
                format!("authenticated; can inspect {count} card(s)"),
            ),
            Err(error) => report.fail("metabase", "api", format!("{error:#}")),
        }
    } else {
        report.skip("metabase", "api", "not configured (optional)");
    }
    report
}

fn verify_session(account: &crate::config::AccountConfig, result: &QueryResult) -> Result<String> {
    let row = result
        .rows
        .first()
        .context("Snowflake session query returned no rows")?;
    let values = (0..5)
        .map(|index| {
            row.get(index)
                .and_then(|value| value.as_deref())
                .with_context(|| {
                    format!(
                        "Snowflake session query returned NULL at column {}",
                        index + 1
                    )
                })
        })
        .collect::<Result<Vec<_>>>()?;
    for (label, actual, expected) in [
        ("user", values[1], account.user.as_str()),
        ("role", values[2], account.role.as_str()),
        ("warehouse", values[3], account.warehouse.as_str()),
        ("database", values[4], account.database.as_str()),
    ] {
        if !actual.eq_ignore_ascii_case(expected) {
            bail!(
                "requested {label} {expected}, but Snowflake activated {actual}; check grants and configuration"
            );
        }
    }
    Ok(values.join(" / "))
}

fn relation_names(result: &QueryResult) -> Vec<String> {
    let Some(index) = result
        .columns
        .iter()
        .position(|column| column.name.eq_ignore_ascii_case("name"))
    else {
        return vec![];
    };
    result
        .rows
        .iter()
        .filter_map(|row| row.get(index).and_then(|value| value.clone()))
        .collect()
}

impl DoctorReport {
    #[cfg(test)]
    pub fn human(&self) -> String {
        self.human_styled(&Style::plain())
    }

    pub fn human_styled(&self, style: &Style) -> String {
        let readiness = if self.ready {
            style.good(&style.bold("READY"))
        } else {
            style.bad(&style.bold("NOT READY"))
        };
        let mut output = format!("embrasure doctor: {readiness}\n",);
        for item in &self.checks {
            let icon = match item.status {
                "pass" => style.good("✓"),
                "skip" => style.warn("-"),
                _ => style.bad("✗"),
            };
            output.push_str(&format!(
                "  {icon} {} / {}: {}\n",
                item.scope, item.check, item.message
            ));
        }
        output
    }

    fn tool(&mut self, name: &str, command: &str) {
        match Command::new(command).arg("--version").output() {
            Ok(output) if output.status.success() => {
                let version = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .take(2)
                    .collect::<Vec<_>>()
                    .join(" ");
                self.pass("local", name, version);
            }
            Ok(output) => self.fail(
                "local",
                name,
                format!("{command} --version exited {}", output.status),
            ),
            Err(error) => self.fail("local", name, format!("could not run {command}: {error}")),
        }
    }

    fn pass(
        &mut self,
        scope: impl Into<String>,
        check: impl Into<String>,
        message: impl Into<String>,
    ) {
        self.checks.push(Diagnostic {
            scope: scope.into(),
            check: check.into(),
            status: "pass",
            message: message.into(),
        });
    }

    fn fail(
        &mut self,
        scope: impl Into<String>,
        check: impl Into<String>,
        message: impl Into<String>,
    ) {
        self.ready = false;
        self.checks.push(Diagnostic {
            scope: scope.into(),
            check: check.into(),
            status: "fail",
            message: message.into(),
        });
    }

    fn skip(
        &mut self,
        scope: impl Into<String>,
        check: impl Into<String>,
        message: impl Into<String>,
    ) {
        self.checks.push(Diagnostic {
            scope: scope.into(),
            check: check.into(),
            status: "skip",
            message: message.into(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::AuthConfig, snowflake::ResultColumn};

    #[test]
    fn a_failure_makes_the_report_not_ready() {
        let mut report = DoctorReport {
            schema_version: 1,
            ready: true,
            checks: vec![],
        };
        report.pass("local", "git", "ready");
        report.fail("snowflake:one", "session", "denied");
        assert!(!report.ready);
        assert!(report.human().contains("NOT READY"));
    }

    #[test]
    fn session_check_rejects_a_role_snowflake_did_not_activate() {
        let account = crate::config::AccountConfig {
            name: "one".into(),
            account: "org-account".into(),
            user: "CI_USER".into(),
            role: "DBT_CI".into(),
            database: "ANALYTICS".into(),
            warehouse: "DBT_CI_WH".into(),
            production_schema: "PROD".into(),
            selector: None,
            auth: AuthConfig::Oauth {
                token_env: "TOKEN".into(),
            },
        };
        let result = QueryResult {
            columns: (0..5)
                .map(|index| ResultColumn {
                    name: index.to_string(),
                    data_type: "TEXT".into(),
                })
                .collect(),
            rows: vec![vec![
                Some("ACCOUNT".into()),
                Some("CI_USER".into()),
                Some("PUBLIC".into()),
                Some("DBT_CI_WH".into()),
                Some("ANALYTICS".into()),
            ]],
        };
        assert!(
            verify_session(&account, &result)
                .unwrap_err()
                .to_string()
                .contains("role")
        );
    }
}
