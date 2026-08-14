use std::{path::Path, process::Command};

use serde::Serialize;
use uuid::Uuid;

use crate::{
    auth,
    config::Config,
    metabase,
    snowflake::{SnowflakeClient, quote_identifier},
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

impl DoctorReport {
    pub fn human(&self) -> String {
        let mut output = format!(
            "embrasure-check doctor: {}\n",
            if self.ready { "READY" } else { "NOT READY" }
        );
        for item in &self.checks {
            let icon = match item.status {
                "pass" => "✓",
                "skip" => "-",
                _ => "✗",
            };
            output.push_str(&format!(
                "  {icon} {} / {}: {}\n",
                item.scope, item.check, item.message
            ));
        }
        output
    }
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
        let query_tag = format!("embrasure-check:doctor:{}", Uuid::new_v4());
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
            Ok(result) => {
                let identity = result
                    .rows
                    .first()
                    .map(|row| row.iter().map(|value| value.as_deref().unwrap_or("NULL")).collect::<Vec<_>>().join(" / "))
                    .unwrap_or_else(|| "connected".into());
                report.pass(&scope, "session", identity);
            }
            Err(error) => {
                report.fail(&scope, "session", format!("{error:#}"));
                continue;
            }
        }
        let production = format!(
            "SHOW TABLES IN SCHEMA {}.{}",
            quote_identifier(&account.database),
            quote_identifier(&account.production_schema)
        );
        match client.execute(&production).await {
            Ok(result) => {
                let table_name_index = result
                    .columns
                    .iter()
                    .position(|column| column.name.eq_ignore_ascii_case("name"));
                let first_table = table_name_index.and_then(|index| {
                    result
                        .rows
                        .first()
                        .and_then(|row| row.get(index))
                        .and_then(|value| value.as_deref())
                });
                if let Some(table) = first_table {
                    let sample = format!(
                        "SELECT * FROM {}.{}.{} LIMIT 0",
                        quote_identifier(&account.database),
                        quote_identifier(&account.production_schema),
                        quote_identifier(table)
                    );
                    match client.execute(&sample).await {
                        Ok(_) => report.pass(
                            &scope,
                            "production_read",
                            format!(
                                "can read production; {} visible table(s)",
                                result.rows.len()
                            ),
                        ),
                        Err(error) => report.fail(&scope, "production_read", format!("{error:#}")),
                    }
                } else {
                    report.pass(
                        &scope,
                        "production_read",
                        "production schema is visible but contains no tables",
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
            match client.create_schema(&schema).await {
                Ok(()) => match client
                    .drop_schema(&schema, &config.safety.schema_prefix)
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
                },
                Err(error) => report.fail(
                    &scope,
                    "ci_schema_lifecycle",
                    format!("could not create a temporary schema: {error:#}"),
                ),
            }
        } else {
            report.skip(&scope, "ci_schema_lifecycle", "skipped by --read-only");
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

impl DoctorReport {
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
}
