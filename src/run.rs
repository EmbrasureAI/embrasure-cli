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
        ComparisonMode, Config, DownstreamPolicy, IncrementalMode, ModelConfig, QueryDiffConfig,
        SafetyConfig, Thresholds,
    },
    dbt::{self, DbtContext, Manifest, ManifestNode},
    git, lineage, metabase,
    query::{QueryDiffInput, QueryTemplate, RefTarget, run_query_diff},
    report::{
        CiSchema, ColumnLineageGap, CoverageGap, Finding, ModelReport, Notice, QueryCheckReport,
        QueryCheckStatus, Report, SkippedModel,
    },
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
    let resolved_auth = if dry_run {
        config
            .accounts
            .iter()
            .map(|account| {
                (
                    account.name.clone(),
                    auth::ResolvedAuth::ProgrammaticAccessToken {
                        token: "dry-run-not-used".into(),
                    },
                )
            })
            .collect()
    } else {
        auth::resolve_all(config)
            .await
            .context("could not resolve Snowflake credentials")?
    };
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
            let query_check_changes =
                query_check_changes(config, base, &context.repo_root, report)?;
            let selections =
                plan_selections(config, &context, options, &query_check_changes, report)?;
            if dry_run {
                report.notices.push(Notice {
                    scope: "validation".into(),
                    code: "dry_run".into(),
                    message:
                        "planned validation without creating schemas or querying warehouse data"
                            .into(),
                });
            }
            let result = if let Some(selections) = selections {
                if dry_run {
                    plan_model_reports(config, &context, &selections, "planned", report)?;
                    plan_query_reports(config, &context, &selections, report);
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
    query_checks: Vec<PlannedQueryCheck>,
}

#[derive(Debug, Clone)]
struct PlannedQueryCheck {
    config: QueryDiffConfig,
    current: QueryTemplate,
    production: QueryTemplate,
    current_refs: Vec<ResolvedRef>,
    production_refs: Vec<ResolvedRef>,
}

#[derive(Debug, Clone)]
struct ResolvedRef {
    target: RefTarget,
    node_id: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Default)]
struct QueryCheckChanges {
    changed: BTreeSet<String>,
}

fn query_check_changes(
    config: &Config,
    base: &str,
    repo_root: &Path,
    report: &mut Report,
) -> Result<QueryCheckChanges> {
    let all_current_changed = || QueryCheckChanges {
        changed: config
            .checks
            .iter()
            .map(|check| check.query_diff().name.to_ascii_lowercase())
            .collect(),
    };
    let Some(source_path) = config.source_path.as_deref() else {
        return Ok(all_current_changed());
    };
    let Ok(relative) = source_path.strip_prefix(repo_root) else {
        if !config.checks.is_empty() {
            report.notices.push(Notice {
                scope: "query_checks".into(),
                code: "query_check_base_unavailable".into(),
                message: format!(
                    "config {} is outside the Git repository; current query checks will run, but removed checks cannot be compared with {base}",
                    source_path.display()
                ),
            });
        }
        return Ok(all_current_changed());
    };
    let relative = relative.to_string_lossy().replace('\\', "/");
    let object = format!("{base}:{relative}");
    let exists = git::output(repo_root, &["cat-file", "-e", &object])?;
    if !exists.status.success() {
        return Ok(all_current_changed());
    }
    let yaml = git::text(repo_root, &["show", &object])?;
    let base_config: Config =
        serde_yaml::from_str(&yaml).with_context(|| format!("invalid config at {object}"))?;
    base_config
        .validate()
        .with_context(|| format!("invalid config at {object}"))?;
    Ok(compare_query_check_definitions(
        config,
        &base_config,
        base,
        report,
    ))
}

fn compare_query_check_definitions(
    config: &Config,
    base_config: &Config,
    base: &str,
    report: &mut Report,
) -> QueryCheckChanges {
    let current = config
        .checks
        .iter()
        .map(|check| {
            (
                check.query_diff().name.to_ascii_lowercase(),
                check.query_diff(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let previous = base_config
        .checks
        .iter()
        .map(|check| {
            (
                check.query_diff().name.to_ascii_lowercase(),
                check.query_diff(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let changed = current
        .iter()
        .filter(|(name, check)| previous.get(*name).copied() != Some(**check))
        .map(|(name, _)| name.clone())
        .collect();
    for (name, check) in previous {
        if current.contains_key(&name) {
            continue;
        }
        let account = check
            .account
            .clone()
            .or_else(|| {
                (base_config.accounts.len() == 1).then(|| base_config.accounts[0].name.clone())
            })
            .unwrap_or_else(|| "unassigned".into());
        report.coverage_gaps.push(CoverageGap {
            scope: query_scope(&check.name),
            check: "query_diff_removed".into(),
            reason: format!(
                "query check was present at {base} for account {account} but is absent from the current configuration"
            ),
        });
    }
    QueryCheckChanges { changed }
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
    query_check_changes: &QueryCheckChanges,
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
        let query_checks = plan_query_checks_for_account(
            config,
            &account.name,
            manifest,
            production,
            &changed,
            &removed,
            &current_impacted,
            &surviving_removed_impact,
            query_check_changes,
            &mut selected,
            report,
        )?;
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
        selections.push(AccountSelection {
            selected,
            removed,
            query_checks,
        });
    }
    if !report.impact.dbt_models.is_empty() {
        report.notices.push(Notice {
            scope: "dbt".into(),
            code: "column_lineage_unavailable".into(),
            message: "dbt artifacts provide model-level, not authoritative column-level, dependency edges".into(),
        });
    }
    if selected_count > config.safety.max_models {
        let mut has_runnable_query = false;
        for (account, selection) in config.accounts.iter().zip(&mut selections) {
            for id in &selection.selected {
                report.validation_scope.skipped_models.push(SkippedModel {
                    id: format!("{}:{id}", account.name),
                    reason: "validation stopped because the requested model count exceeded safety.max_models".into(),
                });
            }
            let selected = selection.selected.clone();
            let mut runnable = Vec::new();
            for check in std::mem::take(&mut selection.query_checks) {
                let needs_build = check.current_refs.iter().any(|resolved| {
                    resolved
                        .node_id
                        .as_ref()
                        .is_some_and(|id| selected.contains(id))
                });
                if needs_build {
                    let current_refs = check
                        .current_refs
                        .iter()
                        .map(|resolved| resolved.target.display())
                        .collect();
                    let production_refs = check
                        .production_refs
                        .iter()
                        .map(|resolved| resolved.target.display())
                        .collect();
                    push_incomplete_query(
                        report,
                        &check,
                        &account.name,
                        current_refs,
                        production_refs,
                        "query check requires candidate models, but safety.max_models stopped the build"
                            .into(),
                    );
                } else {
                    runnable.push(check);
                }
            }
            selection.query_checks = runnable;
            selection.selected.clear();
            has_runnable_query |= !selection.query_checks.is_empty();
        }
        report.coverage_gaps.push(CoverageGap {
            scope: "validation".into(),
            check: "model_budget".into(),
            reason: format!(
                "{selected_count} account/model builds were requested, above safety.max_models {}; increase the limit or narrow downstream validation",
                config.safety.max_models
            ),
        });
        return Ok(has_runnable_query.then_some(selections));
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

fn plan_query_reports(
    config: &Config,
    context: &DbtContext,
    selections: &[AccountSelection],
    report: &mut Report,
) {
    for (account, selection) in config.accounts.iter().zip(selections) {
        let Ok(current_manifest) = context.manifest(&account.name) else {
            continue;
        };
        let Ok(production_manifest) = context.production_manifest(&account.name) else {
            continue;
        };
        for check in &selection.query_checks {
            let current_refs = check
                .current_refs
                .iter()
                .map(|resolved| resolved.target.display())
                .collect::<Vec<_>>();
            let production_refs = check
                .production_refs
                .iter()
                .map(|resolved| resolved.target.display())
                .collect::<Vec<_>>();
            let invalid = check
                .current_refs
                .iter()
                .chain(&check.production_refs)
                .find_map(|resolved| resolved.error.clone())
                .or_else(|| ephemeral_ref_reason(check, current_manifest, production_manifest));
            if let Some(reason) = invalid {
                push_incomplete_query(
                    report,
                    check,
                    &account.name,
                    current_refs,
                    production_refs,
                    reason,
                );
                continue;
            }
            report.query_checks.push(QueryCheckReport {
                name: check.config.name.clone(),
                account: account.name.clone(),
                status: QueryCheckStatus::Planned,
                current_refs,
                production_refs,
                primary_key: check.config.primary_key.clone(),
                candidate_relation: None,
                production_relation: None,
                candidate_row_count: None,
                production_row_count: None,
                columns: vec![],
                comparison: None,
                reason: Some("planned by --dry-run; query was not executed".into()),
                invalid_primary_key_reason: None,
                examples_truncated: false,
            });
        }
    }
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

    let occupied_schemas = report
        .ci_schemas
        .iter()
        .map(|item| item.schema.to_ascii_uppercase())
        .collect::<BTreeSet<_>>();
    let query_schema = unique_query_schema(ci_schema, &occupied_schemas);
    for (index, (account, selection)) in config.accounts.iter().zip(selections).enumerate() {
        if selection.query_checks.is_empty() {
            continue;
        }
        clients[index]
            .create_schema(&account.database, &query_schema)
            .await
            .with_context(|| {
                format!(
                    "could not create query-check schema for account {}",
                    account.name
                )
            })?;
        report.ci_schemas.push(CiSchema {
            account: account.name.clone(),
            database: account.database.clone(),
            schema: query_schema.clone(),
            cleaned_up: false,
        });
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

        match context.build_manifest(&account.name).and_then(|compiled| {
            lineage::extract(
                &account.name,
                manifest,
                production_manifest,
                &compiled,
                selected,
            )
        }) {
            Ok(extraction) => {
                report.impact.column_lineage.extend(extraction.edges);
                report.impact.column_lineage_gaps.extend(extraction.gaps);
            }
            Err(error) => report.impact.column_lineage_gaps.push(ColumnLineageGap {
                account: account.name.clone(),
                model: "selected models".into(),
                reason: format!("column lineage was unavailable: {error:#}"),
            }),
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

    let query_jobs = prepare_query_jobs(
        config,
        context,
        clients,
        &query_schema,
        selections,
        &baseline_relations,
        report,
    );
    let (completed, completed_queries) = timeout(
        Duration::from_secs(config.comparison.timeout_seconds),
        async {
            let models = run_comparisons(
                comparison_jobs,
                config.comparison.concurrency,
                config.comparison.mode,
                config.safety.clone(),
            )
            .await?;
            let queries = run_query_comparisons(query_jobs, config.comparison.concurrency).await?;
            Ok::<_, anyhow::Error>((models, queries))
        },
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
    for item in completed_queries {
        match item.result {
            Ok(check) => {
                record_query_outcome(report, &check);
                report.query_checks.push(check);
            }
            Err(error) => {
                let message = format!("{error:#}");
                report.execution_errors.push(format!(
                    "query-diff check {} in account {} failed: {message}",
                    item.name, item.account
                ));
                report.query_checks.push(QueryCheckReport {
                    name: item.name,
                    account: item.account,
                    status: QueryCheckStatus::ExecutionFailure,
                    current_refs: item.current_refs,
                    production_refs: item.production_refs,
                    primary_key: item.primary_key,
                    candidate_relation: Some(item.candidate.sql()),
                    production_relation: Some(item.production.sql()),
                    candidate_row_count: None,
                    production_row_count: None,
                    columns: vec![],
                    comparison: None,
                    reason: Some(message),
                    invalid_primary_key_reason: None,
                    examples_truncated: false,
                });
            }
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

struct QueryComparisonJob {
    client: SnowflakeClient,
    name: String,
    account: String,
    current_refs: Vec<String>,
    production_refs: Vec<String>,
    candidate_sql: String,
    production_sql: String,
    candidate: Relation,
    production: Relation,
    primary_key: Vec<String>,
    safety: SafetyConfig,
}

struct CompletedQueryComparison {
    name: String,
    account: String,
    current_refs: Vec<String>,
    production_refs: Vec<String>,
    primary_key: Vec<String>,
    candidate: Relation,
    production: Relation,
    result: Result<QueryCheckReport>,
}

fn prepare_query_jobs(
    config: &Config,
    context: &DbtContext,
    clients: &[SnowflakeClient],
    ci_schema: &str,
    selections: &[AccountSelection],
    baseline_relations: &BTreeMap<(String, String), Relation>,
    report: &mut Report,
) -> Vec<QueryComparisonJob> {
    let mut jobs = Vec::new();
    for (account_index, (account, selection)) in config.accounts.iter().zip(selections).enumerate()
    {
        let Ok(current_manifest) = context.manifest(&account.name) else {
            continue;
        };
        let Ok(production_manifest) = context.production_manifest(&account.name) else {
            continue;
        };
        for (check_index, check) in selection.query_checks.iter().enumerate() {
            let current_refs = check
                .current_refs
                .iter()
                .map(|resolved| resolved.target.display())
                .collect::<Vec<_>>();
            let production_refs = check
                .production_refs
                .iter()
                .map(|resolved| resolved.target.display())
                .collect::<Vec<_>>();
            let resolution_error = check
                .current_refs
                .iter()
                .chain(&check.production_refs)
                .find_map(|resolved| resolved.error.clone());
            if let Some(reason) = resolution_error {
                push_incomplete_query(
                    report,
                    check,
                    &account.name,
                    current_refs,
                    production_refs,
                    reason,
                );
                continue;
            }
            let ephemeral = ephemeral_ref_reason(check, current_manifest, production_manifest);
            if let Some(reason) = ephemeral {
                push_incomplete_query(
                    report,
                    check,
                    &account.name,
                    current_refs,
                    production_refs,
                    reason,
                );
                continue;
            }
            let candidate_sql = check.current.render(|target| {
                let id = resolved_id(&check.current_refs, target)?;
                if selection.selected.contains(id) {
                    let built = report.models.iter().any(|model| {
                        model.account == account.name
                            && model.unique_id == id
                            && model.dbt_build == "passed"
                    });
                    if !built {
                        bail!(
                            "current ref {} was not built successfully",
                            target.display()
                        );
                    }
                    let node = current_manifest
                        .nodes
                        .get(id)
                        .context("resolved current ref disappeared from manifest")?;
                    Ok(relation_for(node, &account.database, &node.schema).sql())
                } else {
                    let node = production_manifest.nodes.get(id).with_context(|| {
                        format!(
                            "unchanged current ref {} has no production-state relation",
                            target.display()
                        )
                    })?;
                    Ok(baseline_relations
                        .get(&(account.name.clone(), id.to_owned()))
                        .cloned()
                        .unwrap_or_else(|| relation_for(node, &account.database, &node.schema))
                        .sql())
                }
            });
            let production_sql = check.production.render(|target| {
                let id = resolved_id(&check.production_refs, target)?;
                let node = production_manifest
                    .nodes
                    .get(id)
                    .context("resolved production ref disappeared from manifest")?;
                Ok(baseline_relations
                    .get(&(account.name.clone(), id.to_owned()))
                    .cloned()
                    .unwrap_or_else(|| relation_for(node, &account.database, &node.schema))
                    .sql())
            });
            let (candidate_sql, production_sql) = match (candidate_sql, production_sql) {
                (Ok(candidate_sql), Ok(production_sql)) => (candidate_sql, production_sql),
                (Err(error), _) | (_, Err(error)) => {
                    push_incomplete_query(
                        report,
                        check,
                        &account.name,
                        current_refs,
                        production_refs,
                        error.to_string(),
                    );
                    continue;
                }
            };
            jobs.push(QueryComparisonJob {
                client: clients[account_index].clone(),
                name: check.config.name.clone(),
                account: account.name.clone(),
                current_refs,
                production_refs,
                candidate_sql,
                production_sql,
                candidate: Relation {
                    database: account.database.clone(),
                    schema: ci_schema.into(),
                    identifier: format!("EMBRASURE_QUERY_{check_index}_CANDIDATE"),
                },
                production: Relation {
                    database: account.database.clone(),
                    schema: ci_schema.into(),
                    identifier: format!("EMBRASURE_QUERY_{check_index}_PRODUCTION"),
                },
                primary_key: check.config.primary_key.clone(),
                safety: config.safety.clone(),
            });
        }
    }
    jobs
}

fn resolved_id<'a>(resolved: &'a [ResolvedRef], target: &RefTarget) -> Result<&'a str> {
    resolved
        .iter()
        .find(|item| item.target == *target)
        .and_then(|item| item.node_id.as_deref())
        .with_context(|| format!("ref {} could not be resolved", target.display()))
}

fn ephemeral_ref_reason(
    check: &PlannedQueryCheck,
    current_manifest: &Manifest,
    production_manifest: &Manifest,
) -> Option<String> {
    check
        .current_refs
        .iter()
        .filter_map(|resolved| resolved.node_id.as_ref())
        .find_map(|id| {
            current_manifest.nodes.get(id).and_then(|node| {
                node.config
                    .materialized
                    .eq_ignore_ascii_case("ephemeral")
                    .then(|| format!(
                        "current ref {} is ephemeral; reference a persisted model or inline equivalent SQL",
                        node.name
                    ))
            })
        })
        .or_else(|| {
            check
                .production_refs
                .iter()
                .filter_map(|resolved| resolved.node_id.as_ref())
                .find_map(|id| {
                    production_manifest.nodes.get(id).and_then(|node| {
                        node.config
                            .materialized
                            .eq_ignore_ascii_case("ephemeral")
                            .then(|| format!(
                                "production ref {} is ephemeral; reference a persisted model or inline equivalent SQL",
                                node.name
                            ))
                    })
                })
        })
}

fn push_incomplete_query(
    report: &mut Report,
    check: &PlannedQueryCheck,
    account: &str,
    current_refs: Vec<String>,
    production_refs: Vec<String>,
    reason: String,
) {
    report.coverage_gaps.push(CoverageGap {
        scope: query_scope(&check.config.name),
        check: "query_diff".into(),
        reason: reason.clone(),
    });
    report.query_checks.push(QueryCheckReport {
        name: check.config.name.clone(),
        account: account.into(),
        status: QueryCheckStatus::Incomplete,
        current_refs,
        production_refs,
        primary_key: check.config.primary_key.clone(),
        candidate_relation: None,
        production_relation: None,
        candidate_row_count: None,
        production_row_count: None,
        columns: vec![],
        comparison: None,
        reason: Some(reason),
        invalid_primary_key_reason: None,
        examples_truncated: false,
    });
}

fn record_query_outcome(report: &mut Report, check: &QueryCheckReport) {
    let scope = query_scope(&check.name);
    if let Some(key_reason) = &check.invalid_primary_key_reason {
        report.findings.push(Finding {
            model: scope.clone(),
            check: "query_diff_primary_key".into(),
            message: key_reason.clone(),
        });
    }
    match check.status {
        QueryCheckStatus::Findings => {
            report.findings.push(Finding {
                model: scope.clone(),
                check: "query_diff".into(),
                message: check
                    .reason
                    .clone()
                    .unwrap_or_else(|| "query results differ".into()),
            });
            if check
                .reason
                .as_deref()
                .is_some_and(|reason| reason.starts_with("key integrity"))
            {
                report.coverage_gaps.push(CoverageGap {
                    scope: scope.clone(),
                    check: "query_diff_values".into(),
                    reason: "value comparison was blocked by null or duplicate primary keys".into(),
                });
            }
        }
        QueryCheckStatus::Incomplete => {
            let reason = check
                .reason
                .clone()
                .unwrap_or_else(|| "query comparison was incomplete".into());
            report.coverage_gaps.push(CoverageGap {
                scope,
                check: "query_diff".into(),
                reason,
            });
        }
        QueryCheckStatus::Planned
        | QueryCheckStatus::Pass
        | QueryCheckStatus::Skipped
        | QueryCheckStatus::ExecutionFailure => {}
    }
}

fn query_scope(name: &str) -> String {
    format!("query:{name}")
}

async fn run_query_comparisons(
    jobs: Vec<QueryComparisonJob>,
    concurrency: usize,
) -> Result<Vec<CompletedQueryComparison>> {
    let mut pending = jobs.into_iter();
    let mut running = JoinSet::new();
    let mut completed = Vec::new();
    for _ in 0..concurrency {
        let Some(job) = pending.next() else { break };
        spawn_query_comparison(&mut running, job);
    }
    while let Some(result) = running.join_next().await {
        completed.push(result.context("query-comparison worker stopped unexpectedly")?);
        if let Some(job) = pending.next() {
            spawn_query_comparison(&mut running, job);
        }
    }
    Ok(completed)
}

fn spawn_query_comparison(
    running: &mut JoinSet<CompletedQueryComparison>,
    job: QueryComparisonJob,
) {
    running.spawn(async move {
        let result = run_query_diff(
            &job.client,
            QueryDiffInput {
                name: &job.name,
                account: &job.account,
                current_refs: job.current_refs.clone(),
                production_refs: job.production_refs.clone(),
                candidate_sql: &job.candidate_sql,
                production_sql: &job.production_sql,
                candidate: &job.candidate,
                production: &job.production,
                primary_key: &job.primary_key,
                safety: &job.safety,
            },
        )
        .await;
        CompletedQueryComparison {
            name: job.name,
            account: job.account,
            current_refs: job.current_refs,
            production_refs: job.production_refs,
            primary_key: job.primary_key,
            candidate: job.candidate,
            production: job.production,
            result,
        }
    });
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

fn unique_query_schema(ci_schema: &str, occupied: &BTreeSet<String>) -> String {
    loop {
        let random = Uuid::new_v4().simple().to_string()[..8].to_ascii_uppercase();
        let candidate = format!("{ci_schema}_Q_{random}");
        if !occupied.contains(&candidate.to_ascii_uppercase()) {
            return candidate;
        }
    }
}

fn model_config<'a>(config: &'a Config, id: &str, name: &str) -> &'a ModelConfig {
    config
        .models
        .get(id)
        .or_else(|| config.models.get(name))
        .unwrap_or(&EMPTY_MODEL_CONFIG)
}

fn query_configs_for_account<'a>(config: &'a Config, account: &str) -> Vec<&'a QueryDiffConfig> {
    config
        .checks
        .iter()
        .map(|check| check.query_diff())
        .filter(|check| {
            check.account.as_deref() == Some(account)
                || (check.account.is_none() && config.accounts.len() == 1)
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn plan_query_checks_for_account(
    config: &Config,
    account: &str,
    current_manifest: &Manifest,
    production_manifest: &Manifest,
    changed: &BTreeSet<String>,
    removed: &BTreeSet<String>,
    current_impacted: &BTreeSet<String>,
    surviving_removed_impact: &BTreeSet<String>,
    changes: &QueryCheckChanges,
    selected: &mut BTreeSet<String>,
    report: &mut Report,
) -> Result<Vec<PlannedQueryCheck>> {
    let current_query_impacted = current_impacted
        .union(surviving_removed_impact)
        .cloned()
        .collect::<BTreeSet<_>>();
    let production_roots = changed
        .iter()
        .filter(|id| production_manifest.nodes.contains_key(*id))
        .cloned()
        .chain(removed.iter().cloned())
        .collect::<BTreeSet<_>>();
    let production_query_impacted = production_manifest.model_descendants(&production_roots);
    let mut planned = Vec::new();
    for check in query_configs_for_account(config, account) {
        let current = QueryTemplate::parse(&check.sql)?;
        let production =
            QueryTemplate::parse(check.production_sql.as_deref().unwrap_or(&check.sql))?;
        let current_refs = resolve_refs(current_manifest, current.refs());
        let production_refs = resolve_refs(production_manifest, production.refs());
        let mut targets = current.refs().iter().cloned().collect::<BTreeSet<_>>();
        targets.extend(production.refs().iter().cloned());
        let exists_on_one_side = targets.iter().any(|target| {
            resolve_ref(current_manifest, target)
                .ok()
                .flatten()
                .is_some()
                != resolve_ref(production_manifest, target)
                    .ok()
                    .flatten()
                    .is_some()
        });
        let has_resolution_error = current_refs
            .iter()
            .chain(&production_refs)
            .any(|resolved| resolved.node_id.is_none());
        let active = (current.refs().is_empty() && production.refs().is_empty())
            || changes.changed.contains(&check.name.to_ascii_lowercase())
            || current_refs.iter().any(|resolved| {
                resolved
                    .node_id
                    .as_ref()
                    .is_some_and(|id| current_query_impacted.contains(id))
            })
            || production_refs.iter().any(|resolved| {
                resolved
                    .node_id
                    .as_ref()
                    .is_some_and(|id| production_query_impacted.contains(id))
            })
            || exists_on_one_side
            || has_resolution_error;
        if !active {
            report.query_checks.push(skipped_query_check(
                check,
                account,
                &current_refs,
                &production_refs,
                "none of the referenced dbt models are impacted",
            ));
            continue;
        }
        let query_targets = current_refs
            .iter()
            .filter_map(|resolved| resolved.node_id.as_ref())
            .filter(|id| current_query_impacted.contains(*id))
            .filter(|id| {
                current_manifest
                    .nodes
                    .get(*id)
                    .is_some_and(|node| !node.config.materialized.eq_ignore_ascii_case("ephemeral"))
            })
            .cloned()
            .collect::<BTreeSet<_>>();
        selected.extend(current_manifest.paths_between(changed, &query_targets));
        selected.extend(query_targets);
        planned.push(PlannedQueryCheck {
            config: check.clone(),
            current,
            production,
            current_refs,
            production_refs,
        });
    }
    Ok(planned)
}

fn resolve_ref(manifest: &Manifest, target: &RefTarget) -> Result<Option<String>> {
    let mut matches = manifest.nodes.values().filter(|node| {
        node.resource_type == "model"
            && node.name == target.name
            && target.package.as_ref().is_none_or(|package| {
                node.unique_id
                    .split('.')
                    .nth(1)
                    .is_some_and(|node_package| node_package == package)
            })
    });
    let Some(node) = matches.next() else {
        return Ok(None);
    };
    if matches.next().is_some() {
        bail!("ref {} is ambiguous in the dbt manifest", target.display());
    }
    Ok(Some(node.unique_id.clone()))
}

fn resolve_refs(manifest: &Manifest, targets: &[RefTarget]) -> Vec<ResolvedRef> {
    targets
        .iter()
        .map(|target| match resolve_ref(manifest, target) {
            Ok(Some(node_id)) => ResolvedRef {
                target: target.clone(),
                node_id: Some(node_id),
                error: None,
            },
            Ok(None) => ResolvedRef {
                target: target.clone(),
                node_id: None,
                error: Some(format!(
                    "ref {} does not resolve to a dbt model in this manifest",
                    target.display()
                )),
            },
            Err(error) => ResolvedRef {
                target: target.clone(),
                node_id: None,
                error: Some(error.to_string()),
            },
        })
        .collect()
}

fn skipped_query_check(
    check: &QueryDiffConfig,
    account: &str,
    current_refs: &[ResolvedRef],
    production_refs: &[ResolvedRef],
    reason: &str,
) -> QueryCheckReport {
    QueryCheckReport {
        name: check.name.clone(),
        account: account.into(),
        status: QueryCheckStatus::Skipped,
        current_refs: current_refs
            .iter()
            .map(|resolved| resolved.target.display())
            .collect(),
        production_refs: production_refs
            .iter()
            .map(|resolved| resolved.target.display())
            .collect(),
        primary_key: check.primary_key.clone(),
        candidate_relation: None,
        production_relation: None,
        candidate_row_count: None,
        production_row_count: None,
        columns: vec![],
        comparison: None,
        reason: Some(reason.into()),
        invalid_primary_key_reason: None,
        examples_truncated: false,
    }
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
    use crate::{
        auth::ResolvedAuth,
        dbt::{DependsOn, ManifestNode, NodeConfig, QuotePolicy},
        query::QueryExecutor,
        snowflake::{QueryResult, ResultColumn},
    };
    use std::{collections::VecDeque, future::Future, path::PathBuf, pin::Pin, sync::Mutex};

    struct FakeExecutor(Mutex<VecDeque<QueryResult>>);

    impl QueryExecutor for FakeExecutor {
        fn execute<'a>(
            &'a self,
            _statement: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<QueryResult>> + Send + 'a>> {
            Box::pin(async move {
                self.0
                    .lock()
                    .unwrap()
                    .pop_front()
                    .context("fake executor ran out of responses")
            })
        }
    }

    fn manifest(nodes: impl IntoIterator<Item = ManifestNode>) -> Manifest {
        let nodes = nodes
            .into_iter()
            .map(|node| (node.unique_id.clone(), node))
            .collect();
        Manifest {
            nodes,
            exposures: BTreeMap::new(),
            child_map: BTreeMap::new(),
        }
    }

    #[cfg(unix)]
    fn fake_dbt(directory: &Path, changed_model: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = directory.join("fake-dbt");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\ncase \" $* \" in\n  *\" --resource-type model \"*) echo '{{\"unique_id\":\"{changed_model}\"}}' ;;\nesac\n"
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
        path
    }

    fn model(id: &str, materialized: &str) -> ManifestNode {
        ManifestNode {
            unique_id: id.into(),
            name: id.rsplit('.').next().unwrap().into(),
            resource_type: "model".into(),
            database: Some("DB".into()),
            schema: "PROD".into(),
            alias: id.rsplit('.').next().unwrap().into(),
            fqn: id.split('.').map(ToOwned::to_owned).collect(),
            tags: vec![],
            depends_on: DependsOn::default(),
            config: NodeConfig {
                materialized: materialized.into(),
                ..NodeConfig::default()
            },
            compiled_code: None,
        }
    }

    fn query_config(check: &str) -> Config {
        serde_yaml::from_str(&format!(
            r#"
version: 1
accounts:
  - name: primary
    account: org-one
    user: ci
    role: ci
    database: DB
    warehouse: ci
    production_schema: PROD
    auth: {{ type: oauth, token_env: TOKEN }}
checks:
{check}
"#
        ))
        .unwrap()
    }

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
            compiled_code: None,
        };
        assert_eq!(relation_for(&node, "OTHER", "CHECK").schema, "CHECK");
    }

    #[test]
    fn query_schema_cannot_reuse_a_dbt_custom_schema() {
        let occupied = BTreeSet::from(["RUN_QUERY".into()]);
        let schema = unique_query_schema("RUN", &occupied);
        assert_ne!(schema, "RUN_QUERY");
        assert!(schema.starts_with("RUN_Q_"));
        assert!(is_managed_schema(&schema, "RUN"));
        let longest_run_schema = "R".repeat(241);
        let longest_query_schema = unique_query_schema(&longest_run_schema, &BTreeSet::new());
        assert!(longest_query_schema.len() <= 255);
        assert!(is_managed_schema(
            &longest_query_schema,
            &longest_run_schema
        ));
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
            compiled_code: None,
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
            compiled_code: None,
        };

        assert_eq!(
            relation_for(&node, "other", "CiSchema").sql(),
            "\"Analytics\".\"CiSchema\".\"OrderLines\""
        );
    }

    #[tokio::test]
    async fn ref_free_query_runs_without_changed_models_and_controls_exit_status() {
        let config =
            query_config("  - { type: query_diff, name: always, sql: \"select 1 as id\" }");
        config.validate().unwrap();
        let current = manifest([]);
        let production = manifest([]);
        let mut selected = BTreeSet::new();
        let mut report = Report::empty("main".into(), config.thresholds);
        let planned = plan_query_checks_for_account(
            &config,
            "primary",
            &current,
            &production,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &QueryCheckChanges::default(),
            &mut selected,
            &mut report,
        )
        .unwrap();
        assert!(selected.is_empty());
        assert_eq!(planned.len(), 1);
        let candidate_sql = planned[0].current.render(|_| unreachable!()).unwrap();
        let production_sql = planned[0].production.render(|_| unreachable!()).unwrap();
        let metadata = || QueryResult {
            columns: vec![ResultColumn {
                name: "ID".into(),
                data_type: "NUMBER(38,0)".into(),
            }],
            rows: vec![],
        };
        let rows = |values: &[&str]| QueryResult {
            columns: vec![],
            rows: vec![values.iter().map(|value| Some((*value).into())).collect()],
        };
        let executor = FakeExecutor(Mutex::new(
            vec![
                metadata(),
                metadata(),
                QueryResult::default(),
                QueryResult::default(),
                rows(&["2", "1"]),
                rows(&["1", "0"]),
                rows(&["1", "1", "2", "1"]),
            ]
            .into(),
        ));
        let candidate = Relation {
            database: "DB".into(),
            schema: "RUN".into(),
            identifier: "C".into(),
        };
        let production_relation = Relation {
            database: "DB".into(),
            schema: "RUN".into(),
            identifier: "P".into(),
        };
        let outcome = run_query_diff(
            &executor,
            QueryDiffInput {
                name: "always",
                account: "primary",
                current_refs: vec![],
                production_refs: vec![],
                candidate_sql: &candidate_sql,
                production_sql: &production_sql,
                candidate: &candidate,
                production: &production_relation,
                primary_key: &[],
                safety: &config.safety,
            },
        )
        .await
        .unwrap();
        record_query_outcome(&mut report, &outcome);
        report.query_checks.push(outcome);
        report.finalize();
        assert_eq!(report.exit_code, crate::report::EXIT_FINDINGS);
        assert_eq!(report.summary.query_checks_run, 1);
    }

    #[tokio::test]
    async fn base_only_removed_ref_activates_renders_and_compares() {
        let config = query_config(
            r#"  - type: query_diff
    name: removed model
    sql: select 1 as id
    production_sql: select id from {{ ref('removed') }}"#,
        );
        config.validate().unwrap();
        let current = manifest([]);
        let production = manifest([model("model.project.removed", "table")]);
        let removed = BTreeSet::from(["model.project.removed".into()]);
        let mut selected = BTreeSet::new();
        let mut report = Report::empty("main".into(), config.thresholds);
        let planned = plan_query_checks_for_account(
            &config,
            "primary",
            &current,
            &production,
            &BTreeSet::new(),
            &removed,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &QueryCheckChanges::default(),
            &mut selected,
            &mut report,
        )
        .unwrap();
        assert_eq!(planned.len(), 1);
        assert!(selected.is_empty());
        let rendered = planned[0]
            .production
            .render(|target| {
                let id = resolved_id(&planned[0].production_refs, target)?;
                Ok(relation_for(&production.nodes[id], "DB", "PROD").sql())
            })
            .unwrap();
        assert_eq!(rendered, r#"select id from "DB"."PROD"."REMOVED""#);
        let candidate_sql = planned[0].current.render(|_| unreachable!()).unwrap();
        let metadata = || QueryResult {
            columns: vec![ResultColumn {
                name: "ID".into(),
                data_type: "NUMBER(38,0)".into(),
            }],
            rows: vec![],
        };
        let metrics = |values: &[&str]| QueryResult {
            columns: vec![],
            rows: vec![values.iter().map(|value| Some((*value).into())).collect()],
        };
        let executor = FakeExecutor(Mutex::new(
            vec![
                metadata(),
                metadata(),
                QueryResult::default(),
                QueryResult::default(),
                metrics(&["1", "1"]),
                metrics(&["0", "0"]),
            ]
            .into(),
        ));
        let candidate = Relation {
            database: "DB".into(),
            schema: "RUN".into(),
            identifier: "C".into(),
        };
        let production_relation = Relation {
            database: "DB".into(),
            schema: "RUN".into(),
            identifier: "P".into(),
        };
        let outcome = run_query_diff(
            &executor,
            QueryDiffInput {
                name: "removed model",
                account: "primary",
                current_refs: vec![],
                production_refs: vec!["removed".into()],
                candidate_sql: &candidate_sql,
                production_sql: &rendered,
                candidate: &candidate,
                production: &production_relation,
                primary_key: &[],
                safety: &config.safety,
            },
        )
        .await
        .unwrap();
        assert_eq!(outcome.status, QueryCheckStatus::Pass);
        assert!(outcome.comparison.is_some());
    }

    #[test]
    fn modified_and_removed_query_checks_are_never_silent() {
        let current = query_config(
            r#"  - type: query_diff
    name: orders check
    sql: select id from {{ ref('orders') }} where id > 0"#,
        );
        let previous = query_config(
            r#"  - type: query_diff
    name: orders check
    sql: select id from {{ ref('orders') }}
  - type: query_diff
    name: removed check
    sql: select id from {{ ref('orders') }}"#,
        );
        let mut report = Report::empty("main".into(), current.thresholds);
        let changes = compare_query_check_definitions(&current, &previous, "main", &mut report);
        assert!(changes.changed.contains("orders check"));
        assert!(report.coverage_gaps.iter().any(|gap| {
            gap.scope == "query:removed check" && gap.check == "query_diff_removed"
        }));

        let current_manifest = manifest([model("model.project.orders", "table")]);
        let production_manifest = current_manifest.clone();
        let mut selected = BTreeSet::new();
        let planned = plan_query_checks_for_account(
            &current,
            "primary",
            &current_manifest,
            &production_manifest,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeSet::new(),
            &changes,
            &mut selected,
            &mut report,
        )
        .unwrap();
        assert_eq!(planned.len(), 1);
        assert!(selected.is_empty());

        let selections = vec![AccountSelection {
            selected,
            removed: BTreeSet::new(),
            query_checks: planned,
        }];
        let context = DbtContext::for_test(
            PathBuf::new(),
            BTreeMap::from([("primary".into(), current_manifest)]),
            BTreeMap::from([("primary".into(), production_manifest)]),
        )
        .unwrap();
        plan_query_reports(&current, &context, &selections, &mut report);
        assert!(report.query_checks.iter().any(|check| {
            check.name == "orders check" && check.status == QueryCheckStatus::Planned
        }));
    }

    #[test]
    fn config_outside_repo_runs_current_checks_without_base_comparison() {
        let mut config =
            query_config("  - { type: query_diff, name: outside, sql: \"select 1 as id\" }");
        config.source_path = Some(PathBuf::from("/outside/embrasure-check.yml"));
        let mut report = Report::empty("main".into(), config.thresholds);
        let changes = query_check_changes(&config, "main", Path::new("/repo"), &mut report)
            .expect("an unversioned config should not stop validation");
        assert!(changes.changed.contains("outside"));
        assert!(report.notices.iter().any(|notice| {
            notice.code == "query_check_base_unavailable"
                && notice.message.contains("removed checks cannot be compared")
        }));

        config.checks.clear();
        report.notices.clear();
        query_check_changes(&config, "main", Path::new("/repo"), &mut report).unwrap();
        assert!(report.notices.is_empty());
    }

    #[test]
    fn ephemeral_refs_are_incomplete_on_either_side() {
        let config = query_config(
            r#"  - type: query_diff
    name: ephemeral
    sql: select id from {{ ref('orders') }}"#,
        );
        let id = "model.project.orders";
        for (current_kind, production_kind, expected) in [
            ("ephemeral", "table", "current ref orders is ephemeral"),
            ("table", "ephemeral", "production ref orders is ephemeral"),
        ] {
            let current = manifest([model(id, current_kind)]);
            let production = manifest([model(id, production_kind)]);
            let changed = BTreeSet::from([id.into()]);
            let mut selected = BTreeSet::new();
            let mut report = Report::empty("main".into(), config.thresholds);
            let planned = plan_query_checks_for_account(
                &config,
                "primary",
                &current,
                &production,
                &changed,
                &BTreeSet::new(),
                &changed,
                &BTreeSet::new(),
                &QueryCheckChanges::default(),
                &mut selected,
                &mut report,
            )
            .unwrap();
            assert!(
                ephemeral_ref_reason(&planned[0], &current, &production)
                    .unwrap()
                    .starts_with(expected)
            );
        }
    }

    #[test]
    fn invalid_query_keys_are_findings_and_incomplete_with_namespaced_scope() {
        let mut report = Report::empty("main".into(), Thresholds::default());
        let check = QueryCheckReport {
            name: "orders".into(),
            account: "primary".into(),
            status: QueryCheckStatus::Incomplete,
            current_refs: vec![],
            production_refs: vec![],
            primary_key: vec!["missing_id".into()],
            candidate_relation: None,
            production_relation: None,
            candidate_row_count: None,
            production_row_count: None,
            columns: vec![],
            comparison: None,
            reason: Some(
                "primary-key column missing_id is missing from one or both query results".into(),
            ),
            invalid_primary_key_reason: Some(
                "primary-key column missing_id is missing from one or both query results".into(),
            ),
            examples_truncated: false,
        };
        record_query_outcome(&mut report, &check);
        assert_eq!(report.findings[0].model, "query:orders");
        assert_eq!(report.coverage_gaps[0].scope, "query:orders");

        let mut unrelated = Report::empty("main".into(), Thresholds::default());
        let mut duplicate = check.clone();
        duplicate.primary_key = vec!["id".into()];
        duplicate.reason =
            Some("candidate query returns duplicate column name amount; add unique aliases".into());
        duplicate.invalid_primary_key_reason = None;
        record_query_outcome(&mut unrelated, &duplicate);
        assert!(unrelated.findings.is_empty());
        duplicate.reason = Some(
            "candidate query returns duplicate column name order;id; add unique aliases".into(),
        );
        duplicate.invalid_primary_key_reason =
            Some("primary-key column order;id is ambiguous".into());
        record_query_outcome(&mut unrelated, &duplicate);
        assert_eq!(unrelated.findings[0].check, "query_diff_primary_key");
        assert_eq!(
            unrelated.findings[0].message,
            "primary-key column order;id is ambiguous"
        );
        assert!(
            unrelated.coverage_gaps[1]
                .reason
                .contains("duplicate column")
        );
    }

    #[test]
    fn key_integrity_is_a_finding_with_incomplete_value_evidence() {
        let mut report = Report::empty("main".into(), Thresholds::default());
        let check = QueryCheckReport {
            name: "orders".into(),
            account: "primary".into(),
            status: QueryCheckStatus::Findings,
            current_refs: vec![],
            production_refs: vec![],
            primary_key: vec!["id".into()],
            candidate_relation: None,
            production_relation: None,
            candidate_row_count: Some(2),
            production_row_count: Some(1),
            columns: vec![],
            comparison: None,
            reason: Some(
                "key integrity blocks value comparison: candidate has 1 duplicate keys and 0 null-key rows; production has 0 duplicate keys and 0 null-key rows".into(),
            ),
            invalid_primary_key_reason: None,
            examples_truncated: false,
        };
        record_query_outcome(&mut report, &check);
        assert_eq!(report.findings[0].model, "query:orders");
        assert_eq!(report.coverage_gaps[0].check, "query_diff_values");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn orchestration_expands_selection_uses_baseline_and_records_build_and_ref_gaps() {
        let changed_id = "model.project.changed";
        let target_id = "model.project.target";
        let mut changed = model(changed_id, "table");
        changed.schema = "RUN".into();
        let mut target = model(target_id, "incremental");
        target.schema = "RUN".into();
        target.depends_on.nodes.push(changed_id.into());
        let mut current = manifest([changed.clone(), target.clone()]);
        current
            .child_map
            .insert(changed_id.into(), vec![target_id.into()]);
        let production = manifest([
            model(changed_id, "table"),
            model(target_id, "incremental"),
            model("model.project.removed", "table"),
        ]);
        let harness = tempfile::tempdir().unwrap();
        let repo_output = std::process::Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .unwrap();
        let repo = PathBuf::from(String::from_utf8(repo_output.stdout).unwrap().trim());
        let mut config = query_config(
            r#"  - type: query_diff
    name: target check
    sql: select id from {{ ref('target') }}
  - type: query_diff
    name: unresolved check
    sql: select id from {{ ref('missing') }}
  - type: query_diff
    name: always check
    sql: select 1 as id
  - type: query_diff
    name: removed check
    sql: select 1 as id
    production_sql: select id from {{ ref('removed') }}"#,
        );
        config.dbt.project_dir = repo.clone();
        config.dbt.command = fake_dbt(harness.path(), changed_id)
            .to_string_lossy()
            .into_owned();
        config.validation.downstream = DownstreamPolicy::None;
        let context = DbtContext::for_test(
            repo,
            BTreeMap::from([("primary".into(), current)]),
            BTreeMap::from([("primary".into(), production)]),
        )
        .unwrap();
        let mut report = Report::empty("HEAD".into(), config.thresholds);
        let selections = plan_selections(
            &config,
            &context,
            &CheckOptions::default(),
            &QueryCheckChanges::default(),
            &mut report,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            selections[0].selected,
            BTreeSet::from([changed_id.into(), target_id.into()])
        );

        report.models.push(ModelReport {
            unique_id: target_id.into(),
            name: "target".into(),
            account: "primary".into(),
            ci_relation: r#""DB"."RUN"."TARGET""#.into(),
            production_relation: Some(r#""DB"."PROD"."TARGET""#.into()),
            dbt_build: "passed".into(),
            build_strategy: "incremental_clone".into(),
            comparison: None,
        });
        let baseline = Relation {
            database: "DB".into(),
            schema: "RUN_BASELINE".into(),
            identifier: "TARGET".into(),
        };
        let baselines = BTreeMap::from([(("primary".into(), target_id.into()), baseline.clone())]);
        let client = SnowflakeClient::new(
            &config.accounts[0],
            &ResolvedAuth::ProgrammaticAccessToken {
                token: "unused".into(),
            },
            "test".into(),
            30,
        )
        .unwrap();
        let jobs = prepare_query_jobs(
            &config,
            &context,
            &[client],
            "RUN_QUERY",
            &selections,
            &baselines,
            &mut report,
        );
        assert_eq!(jobs.len(), 3);
        assert!(jobs.iter().all(|job| {
            job.candidate.schema == "RUN_QUERY" && job.production.schema == "RUN_QUERY"
        }));
        let target_job = jobs.iter().find(|job| job.name == "target check").unwrap();
        assert!(target_job.candidate_sql.contains(r#""DB"."RUN"."TARGET""#));
        assert!(target_job.production_sql.contains(&baseline.sql()));
        assert!(jobs.iter().any(|job| {
            job.name == "always check"
                && job.current_refs.is_empty()
                && job.production_refs.is_empty()
        }));
        assert!(jobs.iter().any(|job| {
            job.name == "removed check" && job.production_sql.contains(r#""DB"."PROD"."REMOVED""#)
        }));
        assert!(report.query_checks.iter().any(|check| {
            check.name == "unresolved check" && check.status == QueryCheckStatus::Incomplete
        }));

        let metadata = || QueryResult {
            columns: vec![ResultColumn {
                name: "ID".into(),
                data_type: "NUMBER(38,0)".into(),
            }],
            rows: vec![],
        };
        let metrics = |values: &[&str]| QueryResult {
            columns: vec![],
            rows: vec![values.iter().map(|value| Some((*value).into())).collect()],
        };
        for job in &jobs {
            let executor = FakeExecutor(Mutex::new(
                vec![
                    metadata(),
                    metadata(),
                    QueryResult::default(),
                    QueryResult::default(),
                    metrics(&["1", "1"]),
                    metrics(&["0", "0"]),
                ]
                .into(),
            ));
            let outcome = run_query_diff(
                &executor,
                QueryDiffInput {
                    name: &job.name,
                    account: &job.account,
                    current_refs: job.current_refs.clone(),
                    production_refs: job.production_refs.clone(),
                    candidate_sql: &job.candidate_sql,
                    production_sql: &job.production_sql,
                    candidate: &job.candidate,
                    production: &job.production,
                    primary_key: &job.primary_key,
                    safety: &job.safety,
                },
            )
            .await
            .unwrap();
            assert_eq!(outcome.status, QueryCheckStatus::Pass);
            record_query_outcome(&mut report, &outcome);
            report.query_checks.push(outcome);
        }
        report.finalize();
        assert_eq!(report.exit_code, crate::report::EXIT_INCOMPLETE);

        let mut failed = Report::empty("HEAD".into(), config.thresholds);
        failed.models.push(ModelReport {
            unique_id: target_id.into(),
            name: "target".into(),
            account: "primary".into(),
            ci_relation: r#""DB"."RUN"."TARGET""#.into(),
            production_relation: Some(r#""DB"."PROD"."TARGET""#.into()),
            dbt_build: "failed".into(),
            build_strategy: "incremental_clone".into(),
            comparison: None,
        });
        let failed_jobs = prepare_query_jobs(
            &config,
            &context,
            &[SnowflakeClient::new(
                &config.accounts[0],
                &ResolvedAuth::ProgrammaticAccessToken {
                    token: "unused".into(),
                },
                "test".into(),
                30,
            )
            .unwrap()],
            "RUN_QUERY",
            &selections,
            &baselines,
            &mut failed,
        );
        assert_eq!(failed_jobs.len(), 2);
        assert!(!failed_jobs.iter().any(|job| job.name == "target check"));
        assert!(failed.query_checks.iter().any(|check| {
            check.name == "target check"
                && check.status == QueryCheckStatus::Incomplete
                && check
                    .reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("not built"))
        }));
    }
}
