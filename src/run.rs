use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use tokio::{task::JoinSet, time::timeout};
use uuid::Uuid;

use crate::{
    auth,
    compare::{CompareOptions, compare_model},
    config::{
        ComparisonMode, Config, DownstreamPolicy, IncrementalMode, ModelConfig, SafetyConfig,
        Thresholds,
    },
    dbt::{self, DbtContext, ManifestNode},
    metabase,
    report::{CiSchema, CoverageGap, Finding, ModelReport, Notice, Report, SkippedModel},
    snowflake::{Relation, SnowflakeClient, is_managed_schema},
};

#[cfg(test)]
pub async fn run_check(config_path: &Path, base: &str, options: CheckOptions) -> Report {
    if base.trim().is_empty() || base.starts_with('-') {
        return error_report(
            base,
            anyhow::anyhow!("--base must be a non-empty Git revision and must not start with '-'"),
        );
    }
    let config = match load_config(config_path, &options) {
        Ok(config) => config,
        Err(error) => return error_report(base, error),
    };
    run_check_with_config(config, base, &options, false).await
}

pub fn load_config(config_path: &Path, options: &CheckOptions) -> Result<Config> {
    let mut config = Config::load(config_path)?;
    if let Some(mode) = options.mode {
        config.comparison.mode = mode;
    }
    if let Some(downstream) = options.downstream {
        config.validation.downstream = downstream;
    }
    if let Some(tags) = &options.critical_tags {
        if tags.iter().any(|tag| tag.trim().is_empty()) {
            bail!("--critical-tag must not be empty");
        }
        config.validation.critical_tags = tags.clone();
    }
    if let Some(mode) = options.incremental_mode {
        config.validation.incremental_mode = mode;
    }
    config.resolve_from(config_path)?;
    Ok(config)
}

pub async fn run_check_with_config(
    config: Config,
    base: &str,
    options: &CheckOptions,
    dry_run: bool,
) -> Report {
    if base.trim().is_empty() || base.starts_with('-') {
        return error_report(
            base,
            anyhow::anyhow!("--base must be a non-empty Git revision and must not start with '-'"),
        );
    }
    let mut report = Report::empty(base.to_owned(), config.thresholds);
    report.validation_scope.downstream = config.validation.downstream;
    let run_result = execute(&config, base, options, dry_run, &mut report).await;
    if let Err(error) = run_result {
        report.execution_errors.push(format!("{error:#}"));
    }
    report.finalize();
    report
}

pub fn failed_check(base: &str, error: anyhow::Error) -> Report {
    error_report(base, error)
}

#[derive(Debug, Clone, Default)]
pub struct CheckOptions {
    pub mode: Option<ComparisonMode>,
    pub downstream: Option<DownstreamPolicy>,
    pub critical_tags: Option<Vec<String>>,
    pub incremental_mode: Option<IncrementalMode>,
    pub select: Vec<String>,
}

async fn execute(
    config: &Config,
    base: &str,
    options: &CheckOptions,
    dry_run: bool,
    report: &mut Report,
) -> Result<()> {
    let resolved_auth = auth::resolve_all(config)
        .await
        .context("could not resolve Snowflake credentials")?;
    let schema = dbt::ci_schema_name(&config.safety.schema_prefix, &config.dbt.project_dir)?;
    let query_tag = format!("embrasure:{}:{}", env!("CARGO_PKG_VERSION"), Uuid::new_v4());
    let mut clients = Vec::new();

    let main_result = {
        let main = async {
            if !dry_run {
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
                    report.ci_schemas.push(CiSchema {
                        account: account.name.clone(),
                        database: account.database.clone(),
                        schema: schema.clone(),
                        cleaned_up: false,
                    });
                    client
                        .create_schema(&account.database, &schema)
                        .await
                        .with_context(|| {
                            format!("could not create CI schema for account {}", account.name)
                        })?;
                    clients.push(client);
                }
            }

            let mut context = dbt::prepare(config, &resolved_auth, base, &schema, &query_tag)?;
            let selections = plan_selections(config, &context, options, report)?;
            let result = if let Some(selections) = selections {
                if dry_run {
                    plan_model_reports(config, &context, &selections, "planned", report)?;
                    report.notices.push(Notice {
                        scope: "validation".into(),
                        code: "dry_run".into(),
                        message:
                            "planned validation without creating schemas or querying warehouse data"
                                .into(),
                    });
                    Ok(())
                } else {
                    execute_with_dbt(config, &mut context, &clients, &schema, &selections, report)
                        .await
                }
            } else {
                Ok(())
            };
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
        report.execution_errors.push(format!("{error:#}"));
    }
    cleanup_schemas(config, &clients, &schema, report).await;
    Ok(())
}

#[derive(Debug, Clone)]
struct AccountSelection {
    selected: BTreeSet<String>,
    removed: BTreeSet<String>,
}

fn resolve_selected_models(
    config: &Config,
    context: &DbtContext,
    requested: &[String],
) -> Result<Vec<BTreeSet<String>>> {
    let mut chosen = vec![BTreeSet::new(); config.accounts.len()];
    for value in requested {
        let mut matches = Vec::new();
        for (index, account) in config.accounts.iter().enumerate() {
            let manifest = context.manifest(&account.name)?;
            if let Some(id) = manifest.node_id(value) {
                if manifest
                    .nodes
                    .get(&id)
                    .is_some_and(|node| node.resource_type == "model")
                {
                    matches.push((index, id));
                }
            } else if manifest
                .nodes
                .values()
                .filter(|node| node.resource_type == "model" && node.name == *value)
                .count()
                > 1
            {
                bail!(
                    "--select model {value} is ambiguous in account {}",
                    account.name
                );
            }
        }
        match matches.as_slice() {
            [] => bail!("--select model {value} is unknown"),
            [(index, id)] => {
                chosen[*index].insert(id.clone());
            }
            _ => bail!("--select model {value} is ambiguous across configured accounts"),
        }
    }
    Ok(chosen)
}

fn plan_selections(
    config: &Config,
    context: &DbtContext,
    options: &CheckOptions,
    report: &mut Report,
) -> Result<Option<Vec<AccountSelection>>> {
    let changed_paths = context.changed_paths(&report.base)?;
    let critical_tags = config
        .validation
        .critical_tags
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut selections = Vec::new();
    let mut selected_count = 0;
    let chosen = resolve_selected_models(config, context, &options.select)?;
    for (account_index, account) in config.accounts.iter().enumerate() {
        let manifest = context.manifest(&account.name)?;
        let production = context.production_manifest(&account.name)?;
        report
            .impact
            .dbt_lineage_changes
            .extend(manifest.lineage_changes(production));
        let changed = dbt::select_changed_models(config, context, account, &changed_paths)
            .with_context(|| format!("could not select dbt models for account {}", account.name))?;
        let removed = production.removed_models(manifest);
        let current_impacted = manifest.model_descendants(&changed);
        let removed_impacted = production.model_descendants(&removed);
        let surviving_removed_impact = removed_impacted
            .iter()
            .filter(|id| manifest.nodes.contains_key(*id))
            .cloned()
            .collect::<BTreeSet<_>>();

        let current_impact = manifest.impact(&changed);
        report.impact.dbt_models.extend(current_impact.dbt_models);
        report
            .impact
            .dbt_exposures
            .extend(current_impact.dbt_exposures);
        report.impact.dbt_lineage.extend(current_impact.dbt_lineage);
        let removal_impact = production.impact(&removed);
        report.impact.dbt_models.extend(removal_impact.dbt_models);
        report
            .impact
            .dbt_exposures
            .extend(removal_impact.dbt_exposures);
        report.impact.dbt_lineage.extend(removal_impact.dbt_lineage);

        let configured_critical = manifest
            .nodes
            .values()
            .filter(|node| {
                node.resource_type == "model"
                    && model_config(config, &node.unique_id, &node.name).critical
            })
            .map(|node| node.unique_id.clone())
            .collect::<BTreeSet<_>>();
        let production_configured_critical = production
            .nodes
            .values()
            .filter(|node| {
                node.resource_type == "model"
                    && model_config(config, &node.unique_id, &node.name).critical
            })
            .map(|node| node.unique_id.clone())
            .collect::<BTreeSet<_>>();
        let mut selected = match config.validation.downstream {
            DownstreamPolicy::None => changed.clone(),
            DownstreamPolicy::All => current_impacted
                .union(&surviving_removed_impact)
                .cloned()
                .collect(),
            DownstreamPolicy::Critical => {
                let current_targets = manifest.critical_targets(
                    &current_impacted,
                    &critical_tags,
                    &configured_critical,
                );
                let mut selected = manifest.paths_between(&changed, &current_targets);
                let production_targets = production.critical_targets(
                    &removed_impacted,
                    &critical_tags,
                    &production_configured_critical,
                );
                selected.extend(
                    production
                        .paths_between(&removed, &production_targets)
                        .into_iter()
                        .filter(|id| manifest.nodes.contains_key(id)),
                );
                selected
            }
        };
        let mut select_excluded = BTreeSet::new();
        if !options.select.is_empty() {
            for id in &chosen[account_index] {
                if !selected.contains(id) {
                    bail!(
                        "--select model {id} is not changed or is outside the requested downstream scope"
                    );
                }
            }
            for id in selected.difference(&chosen[account_index]) {
                select_excluded.insert(id.clone());
                report.validation_scope.skipped_models.push(SkippedModel {
                    id: format!("{}:{id}", account.name),
                    reason: "excluded by --select".into(),
                });
            }
            selected.retain(|id| chosen[account_index].contains(id));
        }
        let impacted = current_impacted
            .union(&removed_impacted)
            .cloned()
            .collect::<BTreeSet<_>>();
        report.validation_scope.impacted_models += impacted.len();
        report.validation_scope.requested_models += selected.len();
        for id in impacted.difference(&selected) {
            if select_excluded.contains(id) {
                continue;
            }
            report.validation_scope.skipped_models.push(SkippedModel {
                id: format!("{}:{id}", account.name),
                reason: if removed.contains(id) {
                    "removed model cannot be built".into()
                } else {
                    "outside the configured downstream validation policy".into()
                },
            });
        }
        selected_count += selected.len();
        selections.push(AccountSelection { selected, removed });
    }
    if !report.impact.dbt_models.is_empty() {
        report.notices.push(Notice {
            scope: "dbt".into(),
            code: "column_lineage_unavailable".into(),
            message: "dbt artifacts provide model-level, not authoritative column-level, dependency edges".into(),
        });
    }
    if selected_count > config.safety.max_models {
        for (account, selection) in config.accounts.iter().zip(&selections) {
            for id in &selection.selected {
                report.validation_scope.skipped_models.push(SkippedModel {
                    id: format!("{}:{id}", account.name),
                    reason: "validation stopped because the requested model count exceeded safety.max_models".into(),
                });
            }
        }
        report.coverage_gaps.push(CoverageGap {
            scope: "validation".into(),
            check: "model_budget".into(),
            reason: format!(
                "{selected_count} account/model builds were requested, above safety.max_models {}; increase the limit or narrow downstream validation",
                config.safety.max_models
            ),
        });
        return Ok(None);
    }
    Ok(Some(selections))
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

fn plan_model_reports(
    config: &Config,
    context: &DbtContext,
    selections: &[AccountSelection],
    dbt_build: &str,
    report: &mut Report,
) -> Result<Vec<(String, Relation)>> {
    let mut production_relations = Vec::new();
    for (account, selection) in config.accounts.iter().zip(selections) {
        let manifest = context.manifest(&account.name)?;
        let production = context.production_manifest(&account.name)?;
        for id in &selection.removed {
            let node = &production.nodes[id];
            production_relations.push((
                id.clone(),
                relation_for(node, &account.database, &node.schema),
            ));
            if !model_config(config, id, &node.name).allow_removal {
                report.findings.push(Finding {
                    check: "model_removed".into(),
                    model: id.clone(),
                    message: "dbt model exists at the base revision but is absent from the current manifest; set models.<unique_id>.allow_removal only after confirming the deletion and downstream migration".into(),
                });
            }
        }
        for id in &selection.selected {
            let node = manifest.nodes.get(id).with_context(|| {
                format!("selected dbt model {id} is absent from current manifest")
            })?;
            let ci_relation = relation_for(node, &account.database, &node.schema);
            let production_relation = production
                .nodes
                .get(id)
                .map(|production| relation_for(production, &account.database, &production.schema));
            production_relations.push((
                id.clone(),
                production_relation.clone().unwrap_or_else(|| {
                    relation_for(node, &account.database, &account.production_schema)
                }),
            ));
            let build_strategy = if manifest.is_incremental(id) {
                if production_relation.is_none() {
                    "first_build"
                } else {
                    match config.validation.incremental_mode {
                        IncrementalMode::Clone => "incremental_clone",
                        IncrementalMode::FullRefresh => "full_refresh",
                    }
                }
            } else {
                "standard"
            };
            report.models.push(ModelReport {
                unique_id: id.clone(),
                name: node.name.clone(),
                account: account.name.clone(),
                ci_relation: ci_relation.sql(),
                production_relation: production_relation.as_ref().map(Relation::sql),
                dbt_build: dbt_build.into(),
                build_strategy: build_strategy.into(),
                comparison: None,
            });
        }
    }
    Ok(production_relations)
}

async fn execute_with_dbt(
    config: &Config,
    context: &mut DbtContext,
    clients: &[SnowflakeClient],
    ci_schema: &str,
    selections: &[AccountSelection],
    report: &mut Report,
) -> Result<()> {
    let removed_production_relations =
        plan_model_reports(config, context, selections, "pending", report)?;
    let mut all_selected = BTreeSet::new();
    for selection in selections {
        all_selected.extend(selection.removed.iter().cloned());
        all_selected.extend(selection.selected.iter().cloned());
    }

    for (index, selection) in selections.iter().enumerate() {
        let selected = &selection.selected;
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

    let mut baseline_relations = BTreeMap::<(String, String), Relation>::new();
    for (index, (account, selection)) in config.accounts.iter().zip(selections).enumerate() {
        let manifest = context.manifest(&account.name)?;
        let production = context.production_manifest(&account.name)?;
        let incrementals = selection
            .selected
            .iter()
            .filter(|id| manifest.is_incremental(id) && production.nodes.contains_key(*id))
            .cloned()
            .collect::<Vec<_>>();
        if incrementals.is_empty() {
            continue;
        }
        let baseline_schema = format!("{ci_schema}_BASELINE");
        let baseline_databases = incrementals
            .iter()
            .map(|id| {
                let node = &production.nodes[id];
                snowflake_identifier(
                    node.database.as_deref().unwrap_or(&account.database),
                    node.config.quoting.database,
                )
            })
            .collect::<BTreeSet<_>>();
        for baseline_database in baseline_databases {
            clients[index]
                .create_schema(&baseline_database, &baseline_schema)
                .await
                .with_context(|| {
                    format!(
                        "could not create incremental baseline schema in {baseline_database} for account {}",
                        account.name
                    )
                })?;
            report.ci_schemas.push(CiSchema {
                account: account.name.clone(),
                database: baseline_database,
                schema: baseline_schema.clone(),
                cleaned_up: false,
            });
        }
        for (position, id) in incrementals.into_iter().enumerate() {
            let current_node = &manifest.nodes[&id];
            let production_node = &production.nodes[&id];
            let source = relation_for(production_node, &account.database, &production_node.schema);
            let baseline = Relation {
                database: source.database.clone(),
                schema: baseline_schema.clone(),
                identifier: format!("EMBRASURE_BASELINE_{position}"),
            };
            clients[index]
                .clone_table(&source, &baseline)
                .await
                .with_context(|| {
                    format!(
                        "could not create a stable baseline clone for incremental model {id}; confirm the production relation is a Snowflake table and grant SELECT on it"
                    )
                })?;
            if config.validation.incremental_mode == IncrementalMode::Clone {
                let candidate = relation_for(current_node, &account.database, &current_node.schema);
                clients[index]
                    .clone_table(&baseline, &candidate)
                    .await
                    .with_context(|| {
                        format!(
                            "could not seed incremental model {id} from its baseline clone; rerun with --incremental-mode full-refresh"
                        )
                    })?;
                report.notices.push(Notice {
                    scope: id.clone(),
                    code: "incremental_history_not_recomputed".into(),
                    message: "validation mirrors the next incremental run; historical rows were not recomputed".into(),
                });
            }
            baseline_relations.insert((account.name.clone(), id), baseline);
        }
    }

    let mut downstream = BTreeSet::new();
    for (account, selection) in config.accounts.iter().zip(selections) {
        let selected = &selection.selected;
        let manifest = context.manifest(&account.name)?;
        downstream.extend(manifest.descendants(selected));
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
    let production_relations = removed_production_relations;
    let mut comparison_jobs = Vec::new();
    for (index, (account, selection)) in config.accounts.iter().zip(selections).enumerate() {
        let selected = &selection.selected;
        let manifest = context.manifest(&account.name)?;
        let production_manifest = context.production_manifest(&account.name)?;
        let build = dbt::build_models(
            config,
            context,
            account,
            selected,
            config.validation.incremental_mode == IncrementalMode::FullRefresh,
        )
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
            report
                .validation_scope
                .validated_models
                .push(format!("{}:{id}", account.name));
            let Some(production_node) = production_manifest.nodes.get(id) else {
                report.coverage_gaps.push(CoverageGap {
                    scope: id.clone(),
                    check: "production_comparison".into(),
                    reason: "new dbt model has no relation in the production-state manifest".into(),
                });
                continue;
            };
            let ci_relation = relation_for(node, &account.database, &node.schema);
            let production_relation = baseline_relations
                .get(&(account.name.clone(), id.clone()))
                .cloned()
                .unwrap_or_else(|| {
                    relation_for(production_node, &account.database, &production_node.schema)
                });
            let model_config = model_config(config, id, &node.name);
            let primary_key = if model_config.primary_key.is_empty() {
                match manifest.inferred_primary_key(id) {
                    Ok(Some(key)) => key,
                    Ok(None) => vec![],
                    Err(error) => {
                        report.notices.push(Notice {
                            scope: id.clone(),
                            code: "unique_key_not_inferred".into(),
                            message: error.to_string(),
                        });
                        vec![]
                    }
                }
            } else {
                model_config.primary_key.clone()
            };
            comparison_jobs.push(ComparisonJob {
                client: clients[index].clone(),
                model_id: id.clone(),
                account: account.name.clone(),
                ci: ci_relation,
                production: production_relation,
                primary_key,
                key_policy: model_config.key_policy,
                where_clause: model_config.where_clause.clone(),
                thresholds: model_config.thresholds.apply(config.thresholds),
            });
        }
    }

    let completed = timeout(
        Duration::from_secs(config.comparison.timeout_seconds),
        run_comparisons(
            comparison_jobs,
            config.comparison.concurrency,
            config.comparison.mode,
            config.safety.clone(),
        ),
    )
    .await
    .with_context(|| {
        format!(
            "Snowflake comparisons exceeded comparison.timeout_seconds ({})",
            config.comparison.timeout_seconds
        )
    })??;
    for item in completed {
        match item.result {
            Ok((comparison, findings)) => {
                if let Some(model) = find_model_mut(report, &item.model_id, &item.account) {
                    model.comparison = Some(comparison);
                }
                report.findings.extend(findings);
            }
            Err(error) => report.execution_errors.push(format!("{error:#}")),
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

struct ComparisonJob {
    client: SnowflakeClient,
    model_id: String,
    account: String,
    ci: Relation,
    production: Relation,
    primary_key: Vec<String>,
    key_policy: crate::config::KeyPolicy,
    where_clause: Option<String>,
    thresholds: Thresholds,
}

struct CompletedComparison {
    model_id: String,
    account: String,
    result: Result<(crate::report::ModelComparison, Vec<Finding>)>,
}

async fn run_comparisons(
    jobs: Vec<ComparisonJob>,
    concurrency: usize,
    mode: ComparisonMode,
    safety: SafetyConfig,
) -> Result<Vec<CompletedComparison>> {
    let mut pending = jobs.into_iter();
    let mut running = JoinSet::new();
    let mut completed = Vec::new();

    for _ in 0..concurrency {
        let Some(job) = pending.next() else { break };
        spawn_comparison(&mut running, job, mode, safety.clone());
    }
    while let Some(result) = running.join_next().await {
        completed.push(result.context("comparison worker stopped unexpectedly")?);
        if let Some(job) = pending.next() {
            spawn_comparison(&mut running, job, mode, safety.clone());
        }
    }
    Ok(completed)
}

fn spawn_comparison(
    running: &mut JoinSet<CompletedComparison>,
    job: ComparisonJob,
    mode: ComparisonMode,
    safety: SafetyConfig,
) {
    running.spawn(async move {
        let result = compare_model(
            &job.client,
            &job.model_id,
            &job.ci,
            &job.production,
            CompareOptions {
                primary_key: &job.primary_key,
                where_clause: job.where_clause.as_deref(),
                mode,
                key_policy: job.key_policy,
                safety: &safety,
                thresholds: job.thresholds,
            },
        )
        .await
        .with_context(|| {
            format!(
                "comparison failed for {} in account {}",
                job.model_id, job.account
            )
        });
        CompletedComparison {
            model_id: job.model_id,
            account: job.account,
            result,
        }
    });
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
                "CI schema cleanup failed for {}.{}; run `embrasure clean` or remove it manually: {error:#}",
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
    critical: false,
    key_policy: crate::config::KeyPolicy::Regression,
    thresholds: crate::config::ThresholdOverrides {
        row_count_relative: None,
        null_rate_absolute: None,
        cardinality_relative: None,
        numeric_relative: None,
    },
    where_clause: None,
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
    report.execution_errors.push(format!("{error:#}"));
    report.finalize();
    report
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
            tags: vec![],
            depends_on: DependsOn::default(),
            config: NodeConfig::default(),
        };
        assert_eq!(relation_for(&node, "OTHER", "CHECK").schema, "CHECK");
    }

    #[tokio::test]
    async fn git_revisions_cannot_be_parsed_as_options() {
        let report = run_check(Path::new("unused.yml"), "--help", CheckOptions::default()).await;
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
            tags: vec![],
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
            tags: vec![],
            depends_on: DependsOn::default(),
            config: NodeConfig {
                quoting: QuotePolicy {
                    database: Some(true),
                    schema: Some(true),
                    identifier: Some(true),
                },
                materialized: String::new(),
                unique_key: None,
            },
        };

        assert_eq!(
            relation_for(&node, "other", "CiSchema").sql(),
            "\"Analytics\".\"CiSchema\".\"OrderLines\""
        );
    }
}
