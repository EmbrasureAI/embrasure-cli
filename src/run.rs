use std::{collections::BTreeSet, path::Path};

use anyhow::{Context, Result, bail};
use uuid::Uuid;

use crate::{
    auth,
    compare::compare_model,
    config::{Config, ModelConfig},
    dbt::{self, DbtContext, ManifestNode},
    metabase,
    report::{CiSchema, CoverageGap, Finding, ModelReport, Report},
    snowflake::{Relation, SnowflakeClient, is_managed_schema},
};

pub async fn run_check(config_path: &Path, base: &str) -> Report {
    if base.trim().is_empty() || base.starts_with('-') {
        return error_report(
            base,
            anyhow::anyhow!("--base must be a non-empty Git revision and must not start with '-'"),
        );
    }
    let mut config = match Config::load(config_path) {
        Ok(config) => config,
        Err(error) => return error_report(base, error),
    };
    if let Err(error) = config.resolve_from(config_path) {
        return error_report(base, error);
    }
    let mut report = Report::empty(base.to_owned(), config.thresholds);
    let run_result = execute(&config, base, &mut report).await;
    if let Err(error) = run_result {
        report.execution_errors.push(format_error(&error));
    }
    report.finalize();
    report
}

async fn execute(config: &Config, base: &str, report: &mut Report) -> Result<()> {
    let resolved_auth = auth::resolve_all(config)
        .await
        .context("could not resolve Snowflake credentials")?;
    let schema = dbt::ci_schema_name(&config.safety.schema_prefix, &config.dbt.project_dir)?;
    let query_tag = format!("embrasure:{}:{}", env!("CARGO_PKG_VERSION"), Uuid::new_v4());
    let mut clients = Vec::new();

    let main_result = {
        let main = async {
            for account in &config.accounts {
                let client = SnowflakeClient::new(
                    account,
                    resolved_auth
                        .get(&account.name)
                        .context("resolved Snowflake credential was not retained")?,
                    query_tag.clone(),
                    config.safety.statement_timeout_seconds,
                )
                .with_context(|| {
                    format!("could not initialize Snowflake account {}", account.name)
                })?;
                clients.push(client);
                report.ci_schemas.push(CiSchema {
                    account: account.name.clone(),
                    database: account.database.clone(),
                    schema: schema.clone(),
                    cleaned_up: false,
                });
                clients
                    .last()
                    .context("internal error: Snowflake client was not retained for cleanup")?
                    .create_schema(&account.database, &schema)
                    .await
                    .with_context(|| {
                        format!("could not create CI schema for account {}", account.name)
                    })?;
            }

            let mut context = dbt::prepare(config, &resolved_auth, base, &schema, &query_tag)?;
            let result = execute_with_dbt(config, &mut context, &clients, &schema, report).await;
            if let Err(error) = context.cleanup_worktree() {
                report
                    .execution_errors
                    .push(format!("temporary Git worktree cleanup failed: {error:#}"));
            }
            result
        };
        tokio::pin!(main);
        tokio::select! {
            result = &mut main => result,
            result = termination_signal() => {
                result?;
                Err(anyhow::anyhow!("received a termination signal; validation stopped and cleanup was attempted"))
            }
        }
    };

    if let Err(error) = main_result {
        report.execution_errors.push(format_error(&error));
    }
    cleanup_schemas(config, &clients, &schema, report).await;
    Ok(())
}

#[cfg(unix)]
async fn termination_signal() -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result?,
        _ = terminate.recv() => {},
    }
    Ok(())
}

#[cfg(not(unix))]
async fn termination_signal() -> Result<()> {
    tokio::signal::ctrl_c().await?;
    Ok(())
}

async fn execute_with_dbt(
    config: &Config,
    context: &mut DbtContext,
    clients: &[SnowflakeClient],
    ci_schema: &str,
    report: &mut Report,
) -> Result<()> {
    let changed_paths = context.changed_paths(&report.base)?;
    let mut selected_by_account = Vec::new();
    let mut all_selected = BTreeSet::new();
    let mut removed_production_relations = Vec::new();
    for account in &config.accounts {
        let manifest = context.manifest(&account.name)?;
        let production_manifest = context.production_manifest(&account.name)?;
        let removed = production_manifest.removed_models(manifest);
        all_selected.extend(removed.iter().cloned());
        let removal_impact = production_manifest.impact(&removed);
        report.impact.dbt_models.extend(removal_impact.dbt_models);
        report
            .impact
            .dbt_exposures
            .extend(removal_impact.dbt_exposures);
        for model in &removed {
            let production_node = &production_manifest.nodes[model];
            removed_production_relations.push((
                model.clone(),
                relation_for(production_node, &account.database, &production_node.schema),
            ));
            if !model_config(config, model, &production_node.name).allow_removal {
                report.findings.push(Finding {
                    check: "model_removed".into(),
                    model: model.clone(),
                    message: "dbt model exists at the base revision but is absent from the current manifest; set models.<unique_id>.allow_removal only after confirming the deletion and downstream migration".into(),
                });
            }
        }
        let selected = dbt::select_models(config, context, account, &changed_paths)
            .with_context(|| format!("could not select dbt models for account {}", account.name))?;
        all_selected.extend(selected.iter().cloned());
        selected_by_account.push(selected);
    }
    let selected_count: usize = selected_by_account.iter().map(BTreeSet::len).sum();
    if selected_count > config.safety.max_models {
        bail!(
            "dbt selected {selected_count} account/model builds, above safety.max_models {}",
            config.safety.max_models
        );
    }

    for (index, selected) in selected_by_account.iter().enumerate() {
        let account = &config.accounts[index];
        let manifest = context.manifest(&account.name)?;
        let derived_schemas = selected
            .iter()
            .filter_map(|id| manifest.nodes.get(id))
            .map(|node| {
                (
                    snowflake_identifier(
                        node.database.as_deref().unwrap_or(&account.database),
                        node.config.quoting.database,
                    ),
                    snowflake_identifier(&node.schema, node.config.quoting.schema),
                )
            })
            .collect::<BTreeSet<_>>();
        for (derived_database, derived_schema) in derived_schemas {
            if !is_managed_schema(&derived_schema, ci_schema) {
                bail!(
                    "dbt generated schema {derived_schema} outside this run's namespace {ci_schema}; update generate_schema_name so derived schemas retain the complete target schema"
                );
            }
            if report.ci_schemas.iter().any(|item| {
                item.account == account.name
                    && item.database.eq_ignore_ascii_case(&derived_database)
                    && item.schema.eq_ignore_ascii_case(&derived_schema)
            }) {
                continue;
            }
            report.ci_schemas.push(CiSchema {
                account: account.name.clone(),
                database: derived_database.clone(),
                schema: derived_schema.clone(),
                cleaned_up: false,
            });
            clients[index]
                .create_schema(&derived_database, &derived_schema)
                .await
                .with_context(|| {
                    format!(
                        "could not create derived dbt schema {derived_schema} for account {}",
                        account.name
                    )
                })?;
        }
    }

    let mut downstream = BTreeSet::new();
    for (account, selected) in config.accounts.iter().zip(&selected_by_account) {
        let manifest = context.manifest(&account.name)?;
        let impact = manifest.impact(selected);
        report.impact.dbt_models.extend(impact.dbt_models);
        report.impact.dbt_exposures.extend(impact.dbt_exposures);
        downstream.extend(manifest.descendants(selected));
        report
            .coverage_gaps
            .extend(dbt::coverage_gaps(manifest, selected));
    }
    report.impact.cross_account_dependencies = config
        .cross_account_dependencies
        .iter()
        .filter(|edge| downstream.contains(&edge.from) || all_selected.contains(&edge.from))
        .cloned()
        .collect();
    for edge in &report.impact.cross_account_dependencies {
        if edge.columns.is_empty() {
            report.coverage_gaps.push(CoverageGap {
                scope: format!("{} -> {}", edge.from, edge.to),
                check: "cross_account_column_lineage".into(),
                reason: "the dependency is declared, but affected columns are unknown".into(),
            });
        }
    }
    let mut production_relations = removed_production_relations;
    for (index, (account, selected)) in config.accounts.iter().zip(&selected_by_account).enumerate()
    {
        let manifest = context.manifest(&account.name)?;
        let production_manifest = context.production_manifest(&account.name)?;
        for id in selected {
            let node = manifest.nodes.get(id).with_context(|| {
                format!("selected dbt model {id} is absent from current manifest")
            })?;
            let ci_relation = relation_for(node, &account.database, &node.schema);
            let production_relation = production_manifest
                .nodes
                .get(id)
                .map(|production| relation_for(production, &account.database, &production.schema));
            production_relations.push((
                id.clone(),
                production_relation.clone().unwrap_or_else(|| {
                    relation_for(node, &account.database, &account.production_schema)
                }),
            ));
            report.models.push(ModelReport {
                unique_id: id.clone(),
                name: node.name.clone(),
                account: account.name.clone(),
                ci_relation: ci_relation.sql(),
                production_relation: production_relation.as_ref().map(Relation::sql),
                dbt_build: "pending".into(),
                comparison: None,
            });
        }

        let build = dbt::build_models(config, context, account, selected)
            .with_context(|| format!("could not execute dbt build for account {}", account.name))?;
        if !build.passed {
            for id in selected {
                if let Some(model) = find_model_mut(report, id, &account.name) {
                    model.dbt_build = "failed".into();
                }
            }
            for failure in build.failures {
                report.findings.push(Finding {
                    check: if failure.unique_id.starts_with("test.") {
                        "dbt_test".into()
                    } else {
                        "dbt_build".into()
                    },
                    model: failure.unique_id,
                    message: failure.message,
                });
            }
            continue;
        }

        for id in selected {
            let node = &manifest.nodes[id];
            if let Some(model) = find_model_mut(report, id, &account.name) {
                model.dbt_build = "passed".into();
            }
            let Some(production_node) = production_manifest.nodes.get(id) else {
                report.coverage_gaps.push(CoverageGap {
                    scope: id.clone(),
                    check: "production_comparison".into(),
                    reason: "new dbt model has no relation in the production-state manifest".into(),
                });
                continue;
            };
            let ci_relation = relation_for(node, &account.database, &node.schema);
            let production_relation =
                relation_for(production_node, &account.database, &production_node.schema);
            let model_config = model_config(config, id, &node.name);
            let (comparison, findings) = compare_model(
                &clients[index],
                id,
                &ci_relation,
                &production_relation,
                &model_config.primary_key,
                &config.safety,
                config.thresholds,
            )
            .await
            .with_context(|| format!("comparison failed for {id} in account {}", account.name))?;
            if let Some(model) = find_model_mut(report, id, &account.name) {
                model.comparison = Some(comparison);
            }
            report.findings.extend(findings);
        }
    }

    if let Some(metabase_config) = &config.metabase {
        match metabase::find_dashboard_impact(metabase_config, &production_relations).await {
            Ok((assets, gaps)) => {
                report.impact.metabase_dashboards = assets;
                report.coverage_gaps.extend(gaps);
            }
            Err(error) => report.coverage_gaps.push(CoverageGap {
                scope: "metabase".into(),
                check: "dashboard_lineage".into(),
                reason: format!("Metabase impact could not be checked: {error:#}"),
            }),
        }
    }
    Ok(())
}

async fn cleanup_schemas(
    config: &Config,
    clients: &[SnowflakeClient],
    run_schema: &str,
    report: &mut Report,
) {
    let cleanup_targets = report
        .ci_schemas
        .iter()
        .enumerate()
        .map(|(index, item)| {
            (
                index,
                item.account.clone(),
                item.database.clone(),
                item.schema.clone(),
            )
        })
        .collect::<Vec<_>>();
    for (record_index, account_name, database, target_schema) in cleanup_targets.into_iter().rev() {
        let Some(account_index) = config
            .accounts
            .iter()
            .position(|account| account.name == account_name)
        else {
            report.execution_errors.push(format!(
                "could not map CI schema {database}.{target_schema} back to its account for cleanup"
            ));
            continue;
        };
        match clients[account_index]
            .drop_schema(&database, &target_schema, run_schema)
            .await
        {
            Ok(()) => {
                if let Some(record) = report.ci_schemas.get_mut(record_index) {
                    record.cleaned_up = true;
                }
            }
            Err(error) => report.execution_errors.push(format!(
                "CI schema cleanup failed for {}.{}; remove it manually: {error:#}",
                database, target_schema,
            )),
        }
    }
}

fn relation_for(node: &ManifestNode, default_database: &str, schema: &str) -> Relation {
    Relation {
        database: snowflake_identifier(
            node.database.as_deref().unwrap_or(default_database),
            node.config.quoting.database,
        ),
        schema: snowflake_identifier(schema, node.config.quoting.schema),
        identifier: snowflake_identifier(&node.alias, node.config.quoting.identifier),
    }
}

fn snowflake_identifier(value: &str, quoted: Option<bool>) -> String {
    if quoted.unwrap_or(false) {
        value.to_owned()
    } else {
        value.to_ascii_uppercase()
    }
}

fn model_config<'a>(config: &'a Config, id: &str, name: &str) -> &'a ModelConfig {
    config
        .models
        .get(id)
        .or_else(|| config.models.get(name))
        .unwrap_or(&EMPTY_MODEL_CONFIG)
}

static EMPTY_MODEL_CONFIG: ModelConfig = ModelConfig {
    primary_key: vec![],
    allow_removal: false,
};

fn find_model_mut<'a>(
    report: &'a mut Report,
    id: &str,
    account: &str,
) -> Option<&'a mut ModelReport> {
    report
        .models
        .iter_mut()
        .find(|model| model.unique_id == id && model.account == account)
}

fn error_report(base: &str, error: anyhow::Error) -> Report {
    let mut report = Report::empty(base.to_owned(), crate::config::Thresholds::default());
    report.execution_errors.push(format_error(&error));
    report.finalize();
    report
}

fn format_error(error: &anyhow::Error) -> String {
    format!("{error:#}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbt::{DependsOn, ManifestNode, NodeConfig, QuotePolicy};

    #[test]
    fn ci_relation_never_uses_production_schema() {
        let node = ManifestNode {
            unique_id: "model.x.y".into(),
            name: "y".into(),
            resource_type: "model".into(),
            database: Some("D".into()),
            schema: "PROD".into(),
            alias: "Y".into(),
            fqn: vec!["x".into(), "y".into()],
            depends_on: DependsOn::default(),
            config: NodeConfig::default(),
        };
        assert_eq!(relation_for(&node, "OTHER", "CHECK").schema, "CHECK");
    }

    #[tokio::test]
    async fn git_revisions_cannot_be_parsed_as_options() {
        let report = run_check(Path::new("unused.yml"), "--help").await;
        assert_eq!(report.status, crate::report::Status::ExecutionFailure);
        assert!(report.execution_errors[0].contains("must not start"));
    }

    #[test]
    fn relation_uses_snowflake_case_for_unquoted_dbt_identifiers() {
        let node = ManifestNode {
            unique_id: "model.x.orders".into(),
            name: "orders".into(),
            resource_type: "model".into(),
            database: Some("analytics".into()),
            schema: "prod".into(),
            alias: "orders".into(),
            fqn: vec!["x".into(), "orders".into()],
            depends_on: DependsOn::default(),
            config: NodeConfig::default(),
        };

        assert_eq!(
            relation_for(&node, "other", "ci_schema").sql(),
            "\"ANALYTICS\".\"CI_SCHEMA\".\"ORDERS\""
        );
    }

    #[test]
    fn relation_preserves_explicitly_quoted_dbt_identifiers() {
        let node = ManifestNode {
            unique_id: "model.x.orders".into(),
            name: "orders".into(),
            resource_type: "model".into(),
            database: Some("Analytics".into()),
            schema: "Prod".into(),
            alias: "OrderLines".into(),
            fqn: vec!["x".into(), "orders".into()],
            depends_on: DependsOn::default(),
            config: NodeConfig {
                quoting: QuotePolicy {
                    database: Some(true),
                    schema: Some(true),
                    identifier: Some(true),
                },
            },
        };

        assert_eq!(
            relation_for(&node, "other", "CiSchema").sql(),
            "\"Analytics\".\"CiSchema\".\"OrderLines\""
        );
    }
}
