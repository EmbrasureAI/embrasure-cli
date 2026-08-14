use std::{fmt::Write as _, fs, path::Path};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::config::{CrossAccountDependency, Thresholds};

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
    pub models: Vec<ModelReport>,
    pub impact: ImpactReport,
    pub findings: Vec<Finding>,
    pub coverage_gaps: Vec<CoverageGap>,
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

#[derive(Debug, Clone, Default, Serialize)]
pub struct ImpactReport {
    pub dbt_models: Vec<ImpactedAsset>,
    pub dbt_exposures: Vec<ImpactedAsset>,
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

impl Report {
    pub fn empty(base: String, thresholds: Thresholds) -> Self {
        Self {
            schema_version: 1,
            status: Status::Pass,
            exit_code: EXIT_PASS,
            base,
            ci_schemas: vec![],
            thresholds,
            summary: Summary::default(),
            models: vec![],
            impact: ImpactReport::default(),
            findings: vec![],
            coverage_gaps: vec![],
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
        self.impact.dbt_models.sort();
        self.impact.dbt_models.dedup();
        self.impact.dbt_exposures.sort();
        self.impact.dbt_exposures.dedup();
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

    pub fn human(&self) -> String {
        let label = match self.status {
            Status::Pass => "PASS",
            Status::Findings => "FINDINGS",
            Status::Incomplete => "INCOMPLETE",
            Status::ExecutionFailure => "EXECUTION FAILURE",
        };
        let mut output = format!(
            "embrasure-check: {label}\n{} selected · {} built · {} compared · {} findings · {} coverage gaps\n",
            self.summary.models_selected,
            self.summary.models_built,
            self.summary.models_compared,
            self.summary.findings,
            self.summary.coverage_gaps,
        );
        for finding in &self.findings {
            let _ = writeln!(
                output,
                "- [{}] {}: {}",
                finding.check, finding.model, finding.message
            );
        }
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
        for model in &self.models {
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
        for asset in &self.impact.dbt_models {
            let _ = writeln!(output, "- [impact:dbt] {}", asset.id);
        }
        for asset in &self.impact.dbt_exposures {
            let _ = writeln!(output, "- [impact:exposure] {}", asset.id);
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

    pub fn markdown(&self) -> String {
        let mut output = String::from("# embrasure-check report\n\n");
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
        output.push_str("\n## Model evidence\n\n");
        if self.models.is_empty() {
            output.push_str("No models selected.\n\n");
        }
        for model in &self.models {
            let _ = writeln!(output, "### `{}` ({})\n", model.unique_id, model.account);
            let _ = writeln!(output, "- dbt build: `{}`", model.dbt_build);
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
        if !self.execution_errors.is_empty() {
            output.push_str("\n## Execution errors\n\n");
            for error in &self.execution_errors {
                let _ = writeln!(output, "- {error}");
            }
        }
        output
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
}
