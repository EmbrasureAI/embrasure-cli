use std::{fmt::Write as _, fs, path::Path};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::config::{CrossAccountDependency, DownstreamPolicy, Thresholds};

pub const EXIT_PASS: u8 = 0;
pub const EXIT_FINDINGS: u8 = 1;
pub const EXIT_INCOMPLETE: u8 = 2;
pub const EXIT_EXECUTION: u8 = 3;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pass,
    Findings,
    Incomplete,
    ExecutionFailure,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub schema_version: u8,
    pub status: Status,
    pub exit_code: u8,
    pub base: String,
    pub ci_schemas: Vec<CiSchema>,
    pub thresholds: Thresholds,
    pub summary: Summary,
    pub validation_scope: ValidationScope,
    pub models: Vec<ModelReport>,
    pub impact: ImpactReport,
    pub findings: Vec<Finding>,
    pub coverage_gaps: Vec<CoverageGap>,
    pub notices: Vec<Notice>,
    pub execution_errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CiSchema {
    pub account: String,
    pub database: String,
    pub schema: String,
    pub cleaned_up: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Summary {
    pub models_selected: usize,
    pub models_built: usize,
    pub models_compared: usize,
    pub findings: usize,
    pub coverage_gaps: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelReport {
    pub unique_id: String,
    pub name: String,
    pub account: String,
    pub ci_relation: String,
    pub production_relation: Option<String>,
    pub dbt_build: String,
    pub build_strategy: String,
    pub comparison: Option<ModelComparison>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelComparison {
    pub ci_row_count: u64,
    pub production_row_count: u64,
    pub row_count_relative_change: f64,
    pub columns: Vec<ColumnComparison>,
    pub primary_key: Option<PrimaryKeyComparison>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ColumnComparison {
    pub name: String,
    pub ci_type: Option<String>,
    pub production_type: Option<String>,
    pub ci: Option<ColumnMetrics>,
    pub production: Option<ColumnMetrics>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ColumnMetrics {
    pub null_count: u64,
    pub null_rate: f64,
    pub cardinality: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub average: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p05: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p50: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p95: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PrimaryKeyComparison {
    pub columns: Vec<String>,
    pub ci_only_count: u64,
    pub production_only_count: u64,
    pub ci_only_examples: Vec<Vec<Option<String>>>,
    pub production_only_examples: Vec<Vec<Option<String>>>,
    pub ci_duplicate_key_count: u64,
    pub production_duplicate_key_count: u64,
    pub ci_duplicate_rows: u64,
    pub production_duplicate_rows: u64,
    pub ci_null_key_rows: u64,
    pub production_null_key_rows: u64,
    pub ci_duplicate_examples: Vec<Vec<Option<String>>>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct Finding {
    pub model: String,
    pub check: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct CoverageGap {
    pub scope: String,
    pub check: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct Notice {
    pub scope: String,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ValidationScope {
    pub downstream: DownstreamPolicy,
    pub impacted_models: usize,
    pub requested_models: usize,
    pub validated_models: Vec<String>,
    pub skipped_models: Vec<SkippedModel>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct SkippedModel {
    pub id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ImpactReport {
    pub dbt_models: Vec<ImpactedAsset>,
    pub dbt_exposures: Vec<ImpactedAsset>,
    pub dbt_lineage: Vec<LineageEdge>,
    pub dbt_lineage_changes: Vec<LineageChange>,
    pub metabase_dashboards: Vec<ImpactedAsset>,
    pub cross_account_dependencies: Vec<CrossAccountDependency>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct ImpactedAsset {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct LineageEdge {
    pub from: String,
    pub from_name: String,
    pub to: String,
    pub to_name: String,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum LineageChangeKind {
    Added,
    Removed,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct LineageChange {
    pub change: LineageChangeKind,
    pub edge: LineageEdge,
}

impl Report {
    pub fn empty(base: String, thresholds: Thresholds) -> Self {
        Self {
            schema_version: 2,
            status: Status::Pass,
            exit_code: EXIT_PASS,
            base,
            ci_schemas: vec![],
            thresholds,
            summary: Summary::default(),
            validation_scope: ValidationScope::default(),
            models: vec![],
            impact: ImpactReport::default(),
            findings: vec![],
            coverage_gaps: vec![],
            notices: vec![],
            execution_errors: vec![],
        }
    }

    pub fn finalize(&mut self) {
        self.models.sort_by(|a, b| {
            a.unique_id
                .cmp(&b.unique_id)
                .then(a.account.cmp(&b.account))
        });
        self.findings.sort();
        self.findings.dedup();
        self.coverage_gaps.sort();
        self.coverage_gaps.dedup();
        self.notices.sort();
        self.notices.dedup();
        self.validation_scope.validated_models.sort();
        self.validation_scope.validated_models.dedup();
        self.validation_scope.skipped_models.sort();
        self.validation_scope.skipped_models.dedup();
        self.impact.dbt_models.sort();
        self.impact.dbt_models.dedup();
        self.impact.dbt_exposures.sort();
        self.impact.dbt_exposures.dedup();
        self.impact.dbt_lineage.sort();
        self.impact.dbt_lineage.dedup();
        self.impact.dbt_lineage_changes.sort();
        self.impact.dbt_lineage_changes.dedup();
        self.impact.metabase_dashboards.sort();
        self.impact.metabase_dashboards.dedup();
        for dependency in &mut self.impact.cross_account_dependencies {
            dependency.columns.sort();
            dependency.columns.dedup();
        }
        self.impact.cross_account_dependencies.sort();
        self.impact.cross_account_dependencies.dedup();
        self.execution_errors.sort();
        self.execution_errors.dedup();
        self.ci_schemas.sort_by(|a, b| {
            a.account
                .cmp(&b.account)
                .then(a.database.cmp(&b.database))
                .then(a.schema.cmp(&b.schema))
        });

        self.summary.models_selected = self.models.len();
        self.summary.models_built = self
            .models
            .iter()
            .filter(|m| m.dbt_build == "passed")
            .count();
        self.summary.models_compared = self
            .models
            .iter()
            .filter(|m| m.comparison.is_some())
            .count();
        self.summary.findings = self.findings.len();
        self.summary.coverage_gaps = self.coverage_gaps.len();

        (self.status, self.exit_code) = if !self.execution_errors.is_empty() {
            (Status::ExecutionFailure, EXIT_EXECUTION)
        } else if !self.findings.is_empty() {
            (Status::Findings, EXIT_FINDINGS)
        } else if !self.coverage_gaps.is_empty() {
            (Status::Incomplete, EXIT_INCOMPLETE)
        } else {
            (Status::Pass, EXIT_PASS)
        };
    }

    pub fn write_markdown(&self, path: &Path) -> Result<()> {
        fs::write(path, self.markdown())
            .with_context(|| format!("could not write Markdown report {}", path.display()))
    }

    pub fn human(&self, verbose: bool) -> String {
        let label = match self.status {
            Status::Pass => "PASS",
            Status::Findings => "FINDINGS",
            Status::Incomplete => "INCOMPLETE",
            Status::ExecutionFailure => "EXECUTION FAILURE",
        };
        let mut output = format!(
            "embrasure: {label}\n{} selected · {} built · {} compared · {} findings · {} coverage gaps\n{} impacted · {} validated · {} not validated\n",
            self.summary.models_selected,
            self.summary.models_built,
            self.summary.models_compared,
            self.summary.findings,
            self.summary.coverage_gaps,
            self.validation_scope.impacted_models,
            self.validation_scope.validated_models.len(),
            self.validation_scope.skipped_models.len(),
        );
        self.write_human_findings(&mut output, verbose);
        for gap in &self.coverage_gaps {
            let _ = writeln!(
                output,
                "- [unknown:{}] {}: {}",
                gap.check, gap.scope, gap.reason
            );
        }
        for error in &self.execution_errors {
            let _ = writeln!(output, "- [error] {error}");
        }
        let visible_notices = if verbose { self.notices.len() } else { 3 };
        for notice in self.notices.iter().take(visible_notices) {
            let _ = writeln!(
                output,
                "- [note:{}] {}: {}",
                notice.code, notice.scope, notice.message
            );
        }
        if self.notices.len() > visible_notices {
            let _ = writeln!(
                output,
                "- {} more notices; rerun with --verbose",
                self.notices.len() - visible_notices
            );
        }
        if verbose {
            for skipped in &self.validation_scope.skipped_models {
                let _ = writeln!(
                    output,
                    "- [not-validated] {}: {}",
                    skipped.id, skipped.reason
                );
            }
        }
        for model in self.models.iter().filter(|_| verbose) {
            if let Some(comparison) = &model.comparison {
                let _ = writeln!(
                    output,
                    "- [evidence] {}: {} CI rows vs {} production rows across {} columns",
                    model.unique_id,
                    comparison.ci_row_count,
                    comparison.production_row_count,
                    comparison.columns.len(),
                );
            }
        }
        self.write_human_lineage(&mut output, verbose);
        if !self.impact.dbt_lineage_changes.is_empty() {
            output.push_str("Lineage changes\n");
            for change in &self.impact.dbt_lineage_changes {
                let marker = match change.change {
                    LineageChangeKind::Added => "+",
                    LineageChangeKind::Removed => "-",
                };
                let _ = writeln!(
                    output,
                    "  {marker} {} -> {}",
                    change.edge.from_name, change.edge.to_name
                );
            }
        }
        for asset in &self.impact.metabase_dashboards {
            let _ = writeln!(output, "- [impact:metabase] {}", asset.name);
        }
        for edge in &self.impact.cross_account_dependencies {
            let _ = writeln!(
                output,
                "- [impact:cross-account] {} -> {}",
                edge.from, edge.to
            );
        }
        output
    }

    pub fn json(&self, version: u8) -> serde_json::Result<String> {
        match version {
            1 => serde_json::to_string_pretty(&ReportV1::from(self)),
            _ => serde_json::to_string_pretty(self),
        }
    }

    pub fn markdown(&self) -> String {
        let mut output = String::from("# Embrasure check report\n\n");
        let _ = writeln!(output, "**Status:** `{:?}`  ", self.status);
        let _ = writeln!(output, "**Base:** `{}`  ", self.base);
        let _ = writeln!(output, "**Exit code:** `{}`\n", self.exit_code);
        let _ = writeln!(
            output,
            "| Selected | Built | Compared | Findings | Coverage gaps |\n|---:|---:|---:|---:|---:|\n| {} | {} | {} | {} | {} |\n",
            self.summary.models_selected,
            self.summary.models_built,
            self.summary.models_compared,
            self.summary.findings,
            self.summary.coverage_gaps,
        );
        output.push_str("## Validation scope\n\n");
        let _ = writeln!(
            output,
            "- Downstream policy: `{:?}`",
            self.validation_scope.downstream
        );
        let _ = writeln!(
            output,
            "- Impacted models: `{}`",
            self.validation_scope.impacted_models
        );
        let _ = writeln!(
            output,
            "- Requested models: `{}`",
            self.validation_scope.requested_models
        );
        let _ = writeln!(
            output,
            "- Validated models: `{}`",
            self.validation_scope.validated_models.len()
        );
        for skipped in &self.validation_scope.skipped_models {
            let _ = writeln!(
                output,
                "- Not validated: `{}` — {}",
                skipped.id, skipped.reason
            );
        }
        output.push('\n');
        output.push_str("## Findings\n\n");
        if self.findings.is_empty() {
            output.push_str("None.\n\n");
        }
        for finding in &self.findings {
            let _ = writeln!(
                output,
                "- **{} · {}:** {}",
                finding.model, finding.check, finding.message
            );
        }
        output.push_str("\n## Unknown coverage\n\n");
        if self.coverage_gaps.is_empty() {
            output.push_str("None.\n\n");
        }
        for gap in &self.coverage_gaps {
            let _ = writeln!(
                output,
                "- **{} · {}:** {}",
                gap.scope, gap.check, gap.reason
            );
        }
        output.push_str("\n## Notices\n\n");
        if self.notices.is_empty() {
            output.push_str("None.\n\n");
        }
        for notice in &self.notices {
            let _ = writeln!(
                output,
                "- **{} · {}:** {}",
                notice.scope, notice.code, notice.message
            );
        }
        output.push_str("\n## Model evidence\n\n");
        if self.models.is_empty() {
            output.push_str("No models selected.\n\n");
        }
        for model in &self.models {
            let _ = writeln!(output, "### `{}` ({})\n", model.unique_id, model.account);
            let _ = writeln!(output, "- dbt build: `{}`", model.dbt_build);
            let _ = writeln!(output, "- Build strategy: `{}`", model.build_strategy);
            let _ = writeln!(output, "- CI relation: `{}`", model.ci_relation);
            if let Some(relation) = &model.production_relation {
                let _ = writeln!(output, "- Production relation: `{relation}`");
            }
            let Some(comparison) = &model.comparison else {
                output.push('\n');
                continue;
            };
            let _ = writeln!(
                output,
                "- Rows: CI `{}` · production `{}` · relative change `{:.6}`\n",
                comparison.ci_row_count,
                comparison.production_row_count,
                comparison.row_count_relative_change,
            );
            output.push_str("| Column | CI type | Production type | CI null rate | Production null rate | CI cardinality | Production cardinality | CI min | Production min | CI max | Production max | CI avg | Production avg | CI p05 | Production p05 | CI p50 | Production p50 | CI p95 | Production p95 |\n");
            output.push_str("|---|---|---|---:|---:|---:|---:|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|\n");
            for column in &comparison.columns {
                let ci = column.ci.as_ref();
                let production = column.production.as_ref();
                let _ = writeln!(
                    output,
                    "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                    markdown_cell(&column.name),
                    markdown_option(column.ci_type.as_deref()),
                    markdown_option(column.production_type.as_deref()),
                    optional_number(ci.map(|value| value.null_rate)),
                    optional_number(production.map(|value| value.null_rate)),
                    ci.map(|value| value.cardinality.to_string())
                        .unwrap_or_else(|| "—".into()),
                    production
                        .map(|value| value.cardinality.to_string())
                        .unwrap_or_else(|| "—".into()),
                    markdown_option(ci.and_then(|value| value.min.as_deref())),
                    markdown_option(production.and_then(|value| value.min.as_deref())),
                    markdown_option(ci.and_then(|value| value.max.as_deref())),
                    markdown_option(production.and_then(|value| value.max.as_deref())),
                    optional_number(ci.and_then(|value| value.average)),
                    optional_number(production.and_then(|value| value.average)),
                    optional_number(ci.and_then(|value| value.p05)),
                    optional_number(production.and_then(|value| value.p05)),
                    optional_number(ci.and_then(|value| value.p50)),
                    optional_number(production.and_then(|value| value.p50)),
                    optional_number(ci.and_then(|value| value.p95)),
                    optional_number(production.and_then(|value| value.p95)),
                );
            }
            if let Some(primary_key) = &comparison.primary_key {
                let _ = writeln!(
                    output,
                    "\nPrimary key `{}`: {} CI-only values; {} production-only values.\n",
                    primary_key.columns.join(", "),
                    primary_key.ci_only_count,
                    primary_key.production_only_count,
                );
            }
            output.push('\n');
        }
        output.push_str("\n## Downstream impact\n\n");
        for asset in &self.impact.dbt_models {
            let _ = writeln!(output, "- dbt model: `{}`", asset.id);
        }
        for asset in &self.impact.dbt_exposures {
            let _ = writeln!(output, "- dbt exposure: `{}`", asset.id);
        }
        for asset in &self.impact.metabase_dashboards {
            if let Some(url) = &asset.url {
                let _ = writeln!(output, "- Metabase: [{}]({})", asset.name, url);
            } else {
                let _ = writeln!(output, "- Metabase: {}", asset.name);
            }
        }
        for dependency in &self.impact.cross_account_dependencies {
            let _ = writeln!(
                output,
                "- cross-account: `{}` → `{}`",
                dependency.from, dependency.to
            );
        }
        if !self.impact.dbt_lineage_changes.is_empty() {
            output.push_str("\n## Lineage changes\n\n");
            for change in &self.impact.dbt_lineage_changes {
                let label = match change.change {
                    LineageChangeKind::Added => "Added",
                    LineageChangeKind::Removed => "Removed",
                };
                let _ = writeln!(
                    output,
                    "- **{label}:** `{}` → `{}`",
                    change.edge.from_name, change.edge.to_name
                );
            }
        }
        if !self.execution_errors.is_empty() {
            output.push_str("\n## Execution errors\n\n");
            for error in &self.execution_errors {
                let _ = writeln!(output, "- {error}");
            }
        }
        output
    }

    fn write_human_findings(&self, output: &mut String, verbose: bool) {
        use std::collections::BTreeMap;

        let mut prioritized = self.findings.iter().collect::<Vec<_>>();
        prioritized.sort_by_key(|finding| {
            (
                finding_priority(&finding.check),
                &finding.model,
                &finding.check,
            )
        });
        let visible = if verbose {
            prioritized.len()
        } else {
            prioritized.len().min(3)
        };
        let mut grouped = BTreeMap::<&str, Vec<&Finding>>::new();
        for finding in prioritized.iter().take(visible) {
            grouped.entry(&finding.model).or_default().push(finding);
        }
        for (model, findings) in grouped {
            let _ = writeln!(output, "- {model}:");
            for finding in findings {
                let _ = writeln!(output, "  - [{}] {}", finding.check, finding.message);
            }
        }
        if self.findings.len() > visible {
            let _ = writeln!(
                output,
                "- {} more findings; rerun with --verbose",
                self.findings.len() - visible
            );
        }
    }

    fn write_human_lineage(&self, output: &mut String, verbose: bool) {
        if self.impact.dbt_models.is_empty() && self.impact.dbt_exposures.is_empty() {
            return;
        }

        use std::collections::{BTreeMap, BTreeSet};

        let model_ids = self
            .impact
            .dbt_models
            .iter()
            .map(|asset| asset.id.as_str())
            .collect::<BTreeSet<_>>();
        let mut names = self
            .impact
            .dbt_models
            .iter()
            .chain(&self.impact.dbt_exposures)
            .map(|asset| (asset.id.as_str(), asset.name.as_str()))
            .collect::<BTreeMap<_, _>>();
        let mut children = BTreeMap::<&str, Vec<&LineageEdge>>::new();
        let mut model_children = BTreeSet::new();
        for edge in &self.impact.dbt_lineage {
            names.entry(&edge.from).or_insert(&edge.from_name);
            names.entry(&edge.to).or_insert(&edge.to_name);
            children.entry(&edge.from).or_default().push(edge);
            if model_ids.contains(edge.to.as_str()) {
                model_children.insert(edge.to.as_str());
            }
        }

        let mut roots = self
            .impact
            .dbt_models
            .iter()
            .filter(|asset| !model_children.contains(asset.id.as_str()))
            .map(|asset| asset.id.as_str())
            .collect::<Vec<_>>();
        if roots.is_empty() {
            roots.extend(model_ids.iter().copied());
        }

        output.push_str("Lineage impact\n");
        let mut state = LineageRenderState {
            rendered: BTreeSet::new(),
            count: 0,
            limit: if verbose { usize::MAX } else { 20 },
        };
        for root in roots {
            write_lineage_node(output, root, "  ", None, &names, &children, &mut state);
        }
        for asset in &self.impact.dbt_exposures {
            if state.count < state.limit && state.rendered.insert(asset.id.as_str()) {
                let _ = writeln!(output, "  exposure: {}", asset.name);
                state.count += 1;
            }
        }
        let total = self.impact.dbt_models.len() + self.impact.dbt_exposures.len();
        if state.count < total {
            let _ = writeln!(
                output,
                "  ... {} more; rerun with --verbose",
                total - state.count
            );
        }
    }
}

fn finding_priority(check: &str) -> u8 {
    match check {
        "model_removed" | "dbt_build" | "dbt_test" | "primary_key" => 0,
        "column_removed" | "column_type" | "column_added" => 1,
        "row_count" | "null_rate" => 2,
        _ => 3,
    }
}

struct LineageRenderState<'a> {
    rendered: std::collections::BTreeSet<&'a str>,
    count: usize,
    limit: usize,
}

fn write_lineage_node<'a>(
    output: &mut String,
    id: &'a str,
    prefix: &str,
    connector: Option<&str>,
    names: &std::collections::BTreeMap<&'a str, &'a str>,
    children: &std::collections::BTreeMap<&'a str, Vec<&'a LineageEdge>>,
    state: &mut LineageRenderState<'a>,
) {
    if state.count >= state.limit {
        return;
    }
    let label = names.get(id).copied().unwrap_or(id);
    let kind = if id.starts_with("exposure.") {
        "exposure: "
    } else {
        ""
    };
    let _ = writeln!(
        output,
        "{prefix}{}{kind}{label}",
        connector.unwrap_or_default()
    );
    state.count += 1;
    if !state.rendered.insert(id) {
        return;
    }
    let edges = children.get(id).map(Vec::as_slice).unwrap_or_default();
    let child_prefix = match connector {
        Some("├─ ") => format!("{prefix}│  "),
        Some(_) => format!("{prefix}   "),
        None => prefix.to_owned(),
    };
    for (index, edge) in edges.iter().enumerate() {
        let child_connector = if index + 1 == edges.len() {
            "└─ "
        } else {
            "├─ "
        };
        write_lineage_node(
            output,
            &edge.to,
            &child_prefix,
            Some(child_connector),
            names,
            children,
            state,
        );
    }
}

#[derive(Serialize)]
struct ReportV1<'a> {
    schema_version: u8,
    status: Status,
    exit_code: u8,
    base: &'a str,
    ci_schemas: &'a [CiSchema],
    thresholds: Thresholds,
    summary: &'a Summary,
    models: Vec<ModelReportV1<'a>>,
    impact: ImpactReportV1<'a>,
    findings: &'a [Finding],
    coverage_gaps: &'a [CoverageGap],
    execution_errors: &'a [String],
}

#[derive(Serialize)]
struct ModelReportV1<'a> {
    unique_id: &'a str,
    name: &'a str,
    account: &'a str,
    ci_relation: &'a str,
    production_relation: &'a Option<String>,
    dbt_build: &'a str,
    comparison: Option<ModelComparisonV1<'a>>,
}

#[derive(Serialize)]
struct ModelComparisonV1<'a> {
    ci_row_count: u64,
    production_row_count: u64,
    row_count_relative_change: f64,
    columns: &'a [ColumnComparison],
    primary_key: Option<PrimaryKeyComparisonV1<'a>>,
}

#[derive(Serialize)]
struct PrimaryKeyComparisonV1<'a> {
    columns: &'a [String],
    ci_only_count: u64,
    production_only_count: u64,
    ci_only_examples: &'a [Vec<Option<String>>],
    production_only_examples: &'a [Vec<Option<String>>],
}

#[derive(Serialize)]
struct ImpactReportV1<'a> {
    dbt_models: &'a [ImpactedAsset],
    dbt_exposures: &'a [ImpactedAsset],
    metabase_dashboards: &'a [ImpactedAsset],
    cross_account_dependencies: &'a [CrossAccountDependency],
}

impl<'a> From<&'a Report> for ReportV1<'a> {
    fn from(report: &'a Report) -> Self {
        Self {
            schema_version: 1,
            status: report.status,
            exit_code: report.exit_code,
            base: &report.base,
            ci_schemas: &report.ci_schemas,
            thresholds: report.thresholds,
            summary: &report.summary,
            models: report.models.iter().map(ModelReportV1::from).collect(),
            impact: ImpactReportV1 {
                dbt_models: &report.impact.dbt_models,
                dbt_exposures: &report.impact.dbt_exposures,
                metabase_dashboards: &report.impact.metabase_dashboards,
                cross_account_dependencies: &report.impact.cross_account_dependencies,
            },
            findings: &report.findings,
            coverage_gaps: &report.coverage_gaps,
            execution_errors: &report.execution_errors,
        }
    }
}

impl<'a> From<&'a ModelReport> for ModelReportV1<'a> {
    fn from(model: &'a ModelReport) -> Self {
        Self {
            unique_id: &model.unique_id,
            name: &model.name,
            account: &model.account,
            ci_relation: &model.ci_relation,
            production_relation: &model.production_relation,
            dbt_build: &model.dbt_build,
            comparison: model
                .comparison
                .as_ref()
                .map(|comparison| ModelComparisonV1 {
                    ci_row_count: comparison.ci_row_count,
                    production_row_count: comparison.production_row_count,
                    row_count_relative_change: comparison.row_count_relative_change,
                    columns: &comparison.columns,
                    primary_key: comparison.primary_key.as_ref().map(|key| {
                        PrimaryKeyComparisonV1 {
                            columns: &key.columns,
                            ci_only_count: key.ci_only_count,
                            production_only_count: key.production_only_count,
                            ci_only_examples: &key.ci_only_examples,
                            production_only_examples: &key.production_only_examples,
                        }
                    }),
                }),
        }
    }
}

fn markdown_cell(value: &str) -> String {
    value.replace('|', "\\|").replace('\n', " ")
}

fn markdown_option(value: Option<&str>) -> String {
    value.map(markdown_cell).unwrap_or_else(|| "—".into())
}

fn optional_number(value: Option<f64>) -> String {
    value
        .map(|number| format!("{number:.6}"))
        .unwrap_or_else(|| "—".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn representative_report() -> Report {
        let mut report = Report::empty("main".into(), Thresholds::default());
        report.validation_scope.downstream = DownstreamPolicy::Critical;
        report.validation_scope.impacted_models = 2;
        report.validation_scope.requested_models = 1;
        report.validation_scope.validated_models = vec!["primary:model.project.orders".into()];
        report.validation_scope.skipped_models = vec![SkippedModel {
            id: "primary:model.project.other".into(),
            reason: "outside policy".into(),
        }];
        report.models.push(ModelReport {
            unique_id: "model.project.orders".into(),
            name: "orders".into(),
            account: "primary".into(),
            ci_relation: r#""DB"."CI"."ORDERS""#.into(),
            production_relation: Some(r#""DB"."PROD"."ORDERS""#.into()),
            dbt_build: "passed".into(),
            build_strategy: "incremental_clone".into(),
            comparison: Some(ModelComparison {
                ci_row_count: 10,
                production_row_count: 9,
                row_count_relative_change: 1.0 / 9.0,
                columns: vec![ColumnComparison {
                    name: "ID".into(),
                    ci_type: Some("NUMBER".into()),
                    production_type: Some("NUMBER".into()),
                    ci: Some(ColumnMetrics::default()),
                    production: Some(ColumnMetrics::default()),
                }],
                primary_key: Some(PrimaryKeyComparison {
                    columns: vec!["ID".into()],
                    ci_only_count: 1,
                    production_only_count: 0,
                    ci_only_examples: vec![vec![Some("10".into())]],
                    production_only_examples: vec![],
                    ci_duplicate_key_count: 1,
                    production_duplicate_key_count: 0,
                    ci_duplicate_rows: 1,
                    production_duplicate_rows: 0,
                    ci_null_key_rows: 0,
                    production_null_key_rows: 0,
                    ci_duplicate_examples: vec![vec![Some("10".into())]],
                }),
            }),
        });
        report.notices.push(Notice {
            scope: "model.project.orders".into(),
            code: "incremental_history_not_recomputed".into(),
            message: "historical rows were not recomputed".into(),
        });
        report.finalize();
        report
    }

    fn assert_matches_schema(report: &Report, version: u8, schema: &str) {
        let schema: serde_json::Value = serde_json::from_str(schema).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let instance: serde_json::Value =
            serde_json::from_str(&report.json(version).unwrap()).unwrap();
        if let Err(error) = validator.validate(&instance) {
            panic!("report v{version} violates its schema: {error}");
        }
    }

    #[test]
    fn coverage_is_distinct_from_findings() {
        let mut report = Report::empty("main".into(), Thresholds::default());
        report.findings.push(Finding {
            model: "m".into(),
            check: "rows".into(),
            message: "changed".into(),
        });
        report.finalize();
        assert_eq!(report.exit_code, EXIT_FINDINGS);
        report.coverage_gaps.push(CoverageGap {
            scope: "m".into(),
            check: "lineage".into(),
            reason: "unknown".into(),
        });
        report.finalize();
        assert_eq!(report.exit_code, EXIT_FINDINGS);
    }

    #[test]
    fn human_report_shows_lineage_tree_and_dependency_changes() {
        let mut report = Report::empty("main".into(), Thresholds::default());
        report.impact.dbt_models = vec![
            ImpactedAsset {
                id: "model.project.orders".into(),
                name: "orders".into(),
                url: None,
            },
            ImpactedAsset {
                id: "model.project.order_summary".into(),
                name: "order_summary".into(),
                url: None,
            },
        ];
        let edge = LineageEdge {
            from: "model.project.orders".into(),
            from_name: "orders".into(),
            to: "model.project.order_summary".into(),
            to_name: "order_summary".into(),
        };
        report.impact.dbt_lineage.push(edge.clone());
        report.impact.dbt_lineage_changes.push(LineageChange {
            change: LineageChangeKind::Added,
            edge,
        });
        report.finalize();

        let output = report.human(false);
        assert!(output.contains("Lineage impact\n  orders\n  └─ order_summary"));
        assert!(output.contains("Lineage changes\n  + orders -> order_summary"));
    }

    #[test]
    fn reports_match_their_published_json_schemas() {
        let report = representative_report();
        assert_matches_schema(&report, 1, include_str!("../schemas/report-v1.schema.json"));
        assert_matches_schema(&report, 2, include_str!("../schemas/report-v2.schema.json"));

        let v1: serde_json::Value = serde_json::from_str(&report.json(1).unwrap()).unwrap();
        assert!(v1.get("validation_scope").is_none());
        assert!(v1["models"][0].get("build_strategy").is_none());
        assert!(
            v1["models"][0]["comparison"]["primary_key"]
                .get("ci_duplicate_rows")
                .is_none()
        );
    }

    #[test]
    fn compact_terminal_output_caps_findings_and_lineage() {
        let mut report = Report::empty("main".into(), Thresholds::default());
        for index in 0..5 {
            report.findings.push(Finding {
                model: format!("model.project.m{index}"),
                check: "row_count".into(),
                message: format!("change {index}"),
            });
        }
        for index in 0..25 {
            report.impact.dbt_models.push(ImpactedAsset {
                id: format!("model.project.lineage_{index:02}"),
                name: format!("lineage_{index:02}"),
                url: None,
            });
        }
        report.finalize();

        let compact = report.human(false);
        assert!(compact.contains("2 more findings; rerun with --verbose"));
        assert!(!compact.contains("change 3"));
        assert!(compact.contains("... 5 more; rerun with --verbose"));
        assert!(!compact.contains("lineage_20"));

        let verbose = report.human(true);
        assert!(verbose.contains("change 4"));
        assert!(verbose.contains("lineage_24"));
    }
}
