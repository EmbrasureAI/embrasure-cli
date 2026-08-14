use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};

use crate::{
    config::{SafetyConfig, Thresholds},
    report::{ColumnComparison, ColumnMetrics, Finding, ModelComparison, PrimaryKeyComparison},
    snowflake::{Relation, ResultColumn, SnowflakeClient, quote_identifier},
};

pub async fn compare_model(
    client: &SnowflakeClient,
    model_id: &str,
    ci: &Relation,
    production: &Relation,
    primary_key: &[String],
    safety: &SafetyConfig,
    thresholds: Thresholds,
) -> Result<(ModelComparison, Vec<Finding>)> {
    let ci_columns = relation_columns(client, ci)
        .await
        .with_context(|| format!("could not inspect CI relation {}", ci.sql()))?;
    let production_columns = relation_columns(client, production)
        .await
        .with_context(|| format!("could not inspect production relation {}", production.sql()))?;
    let ci_map = column_map(&ci_columns);
    let production_map = column_map(&production_columns);
    let all_names = ci_map
        .keys()
        .chain(production_map.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    if all_names.len() > safety.max_columns_per_model {
        bail!(
            "model {model_id} has {} columns, above safety.max_columns_per_model {}",
            all_names.len(),
            safety.max_columns_per_model
        );
    }
    let common = all_names
        .iter()
        .filter(|name| ci_map.contains_key(*name) && production_map.contains_key(*name))
        .cloned()
        .collect::<Vec<_>>();

    let ci_metrics = relation_metrics(client, ci, &common, &ci_map).await?;
    let prod_metrics = relation_metrics(client, production, &common, &production_map).await?;
    let mut findings = vec![];
    let row_change = relative_change(ci_metrics.row_count as f64, prod_metrics.row_count as f64);
    if row_change > thresholds.row_count_relative {
        findings.push(finding(
            model_id,
            "row_count",
            format!(
                "CI has {} rows vs {} in production (relative change {:.6}, allowed {:.6})",
                ci_metrics.row_count,
                prod_metrics.row_count,
                row_change,
                thresholds.row_count_relative,
            ),
        ));
    }

    let mut columns = vec![];
    for name in all_names {
        let ci_column = ci_map.get(&name);
        let prod_column = production_map.get(&name);
        if ci_column.is_none() {
            findings.push(finding(
                model_id,
                "column_removed",
                format!("column {name} exists in production but not CI"),
            ));
        } else if prod_column.is_none() {
            findings.push(finding(
                model_id,
                "column_added",
                format!("column {name} exists in CI but not production"),
            ));
        } else if let (Some(ci_column), Some(prod_column)) = (ci_column, prod_column)
            && !equivalent_type(&ci_column.data_type, &prod_column.data_type)
        {
            findings.push(finding(
                model_id,
                "column_type",
                format!(
                    "column {name} changed from {} to {}",
                    prod_column.data_type, ci_column.data_type,
                ),
            ));
        }
        let ci_value = ci_metrics.columns.get(&name).cloned();
        let prod_value = prod_metrics.columns.get(&name).cloned();
        if let (Some(ci_value), Some(prod_value)) = (&ci_value, &prod_value) {
            let null_delta = (ci_value.null_rate - prod_value.null_rate).abs();
            if null_delta > thresholds.null_rate_absolute {
                findings.push(finding(model_id, "null_rate", format!(
                    "column {name} null rate is {:.6} in CI vs {:.6} in production (delta {:.6}, allowed {:.6})",
                    ci_value.null_rate, prod_value.null_rate, null_delta, thresholds.null_rate_absolute,
                )));
            }
            let cardinality_change =
                relative_change(ci_value.cardinality as f64, prod_value.cardinality as f64);
            if cardinality_change > thresholds.cardinality_relative {
                findings.push(finding(model_id, "cardinality", format!(
                    "column {name} cardinality is {} in CI vs {} in production (relative change {:.6}, allowed {:.6})",
                    ci_value.cardinality, prod_value.cardinality, cardinality_change, thresholds.cardinality_relative,
                )));
            }
            let numeric = ci_column.is_some_and(|column| is_numeric(&column.data_type))
                && prod_column.is_some_and(|column| is_numeric(&column.data_type));
            for (metric, ci_number, prod_number) in [
                ("average", ci_value.average, prod_value.average),
                ("p05", ci_value.p05, prod_value.p05),
                ("p50", ci_value.p50, prod_value.p50),
                ("p95", ci_value.p95, prod_value.p95),
            ] {
                if numeric && let (Some(ci_number), Some(prod_number)) = (ci_number, prod_number) {
                    let change = relative_change(ci_number, prod_number);
                    if change > thresholds.numeric_relative {
                        findings.push(finding(model_id, "distribution", format!(
                            "column {name} {metric} is {ci_number:.6} in CI vs {prod_number:.6} in production (relative change {change:.6}, allowed {:.6})",
                            thresholds.numeric_relative,
                        )));
                    }
                }
            }
            for (metric, ci_extreme, prod_extreme) in [
                ("min", ci_value.min.as_deref(), prod_value.min.as_deref()),
                ("max", ci_value.max.as_deref(), prod_value.max.as_deref()),
            ] {
                if numeric {
                    if let (Some(ci_number), Some(prod_number)) = (
                        ci_extreme.and_then(|value| value.parse::<f64>().ok()),
                        prod_extreme.and_then(|value| value.parse::<f64>().ok()),
                    ) {
                        let change = relative_change(ci_number, prod_number);
                        if change > thresholds.numeric_relative {
                            findings.push(finding(model_id, "distribution", format!(
                                "column {name} {metric} is {ci_number:.6} in CI vs {prod_number:.6} in production (relative change {change:.6}, allowed {:.6})",
                                thresholds.numeric_relative,
                            )));
                        }
                    }
                } else if let (Some(ci_extreme), Some(prod_extreme)) = (ci_extreme, prod_extreme)
                    && ci_extreme != prod_extreme
                {
                    findings.push(finding(model_id, "range", format!(
                        "column {name} {metric} is {ci_extreme:?} in CI vs {prod_extreme:?} in production"
                    )));
                }
            }
        }
        columns.push(ColumnComparison {
            name,
            ci_type: ci_column.map(|column| column.data_type.clone()),
            production_type: prod_column.map(|column| column.data_type.clone()),
            ci: ci_value,
            production: prod_value,
        });
    }

    let primary_key = if primary_key.is_empty() {
        None
    } else if primary_key.iter().all(|key| {
        matches!(
            (resolve_column(&ci_map, key), resolve_column(&production_map, key)),
            (Some(ci_name), Some(production_name)) if ci_name == production_name
        )
    }) {
        let resolved_primary_key = primary_key
            .iter()
            .filter_map(|key| resolve_column(&ci_map, key))
            .collect::<Vec<_>>();
        let comparison = compare_primary_key(
            client,
            ci,
            production,
            &resolved_primary_key,
            safety.primary_key_sample_limit,
        )
        .await?;
        if comparison.ci_only_count > 0 || comparison.production_only_count > 0 {
            findings.push(finding(
                model_id,
                "primary_key",
                format!(
                    "{} primary-key values exist only in CI and {} only in production",
                    comparison.ci_only_count, comparison.production_only_count,
                ),
            ));
        }
        Some(comparison)
    } else {
        findings.push(finding(
            model_id,
            "primary_key",
            "one or more configured primary-key columns are missing".into(),
        ));
        None
    };

    Ok((
        ModelComparison {
            ci_row_count: ci_metrics.row_count,
            production_row_count: prod_metrics.row_count,
            row_count_relative_change: row_change,
            columns,
            primary_key,
        },
        findings,
    ))
}

async fn relation_columns(
    client: &SnowflakeClient,
    relation: &Relation,
) -> Result<Vec<ResultColumn>> {
    Ok(client
        .execute(&format!("SELECT * FROM {} LIMIT 0", relation.sql()))
        .await?
        .columns)
}

#[derive(Debug)]
struct RelationMetrics {
    row_count: u64,
    columns: BTreeMap<String, ColumnMetrics>,
}

async fn relation_metrics(
    client: &SnowflakeClient,
    relation: &Relation,
    names: &[String],
    types: &BTreeMap<String, ResultColumn>,
) -> Result<RelationMetrics> {
    let mut expressions = vec!["COUNT(*)".to_owned()];
    for name in names {
        let column = quote_identifier(name);
        expressions.push(format!("COUNT_IF({column} IS NULL)"));
        expressions.push(format!("COUNT(DISTINCT {column})"));
        if types
            .get(name)
            .is_some_and(|value| is_orderable(&value.data_type))
        {
            expressions.push(format!("MIN({column})::VARCHAR"));
            expressions.push(format!("MAX({column})::VARCHAR"));
        } else {
            expressions.push("NULL::VARCHAR".into());
            expressions.push("NULL::VARCHAR".into());
        }
        if types
            .get(name)
            .is_some_and(|value| is_numeric(&value.data_type))
        {
            expressions.extend([
                format!("AVG({column})::DOUBLE"),
                format!("APPROX_PERCENTILE({column}, 0.05)::DOUBLE"),
                format!("APPROX_PERCENTILE({column}, 0.50)::DOUBLE"),
                format!("APPROX_PERCENTILE({column}, 0.95)::DOUBLE"),
            ]);
        }
    }
    let result = client
        .execute(&format!(
            "SELECT {} FROM {}",
            expressions.join(", "),
            relation.sql()
        ))
        .await?;
    let row = result
        .rows
        .first()
        .context("Snowflake aggregate returned no row")?;
    let row_count = parse_u64(row.first().and_then(Option::as_deref))?;
    let mut index = 1;
    let mut columns = BTreeMap::new();
    for name in names {
        let null_count = parse_u64(row.get(index).and_then(Option::as_deref))?;
        let cardinality = parse_u64(row.get(index + 1).and_then(Option::as_deref))?;
        let min = row.get(index + 2).cloned().flatten();
        let max = row.get(index + 3).cloned().flatten();
        index += 4;
        let mut metrics = ColumnMetrics {
            null_count,
            null_rate: if row_count == 0 {
                0.0
            } else {
                null_count as f64 / row_count as f64
            },
            cardinality,
            min,
            max,
            ..ColumnMetrics::default()
        };
        if types
            .get(name)
            .is_some_and(|value| is_numeric(&value.data_type))
        {
            metrics.average = parse_optional_f64(row.get(index).and_then(Option::as_deref))?;
            metrics.p05 = parse_optional_f64(row.get(index + 1).and_then(Option::as_deref))?;
            metrics.p50 = parse_optional_f64(row.get(index + 2).and_then(Option::as_deref))?;
            metrics.p95 = parse_optional_f64(row.get(index + 3).and_then(Option::as_deref))?;
            index += 4;
        }
        columns.insert(name.clone(), metrics);
    }
    Ok(RelationMetrics { row_count, columns })
}

async fn compare_primary_key(
    client: &SnowflakeClient,
    ci: &Relation,
    production: &Relation,
    keys: &[String],
    sample_limit: usize,
) -> Result<PrimaryKeyComparison> {
    let selected = keys
        .iter()
        .map(|key| quote_identifier(key))
        .collect::<Vec<_>>()
        .join(", ");
    let ci_only = format!(
        "SELECT {selected} FROM {} MINUS SELECT {selected} FROM {}",
        ci.sql(),
        production.sql()
    );
    let production_only = format!(
        "SELECT {selected} FROM {} MINUS SELECT {selected} FROM {}",
        production.sql(),
        ci.sql()
    );
    let counts = client
        .execute(&format!(
            "SELECT (SELECT COUNT(*) FROM ({ci_only})), (SELECT COUNT(*) FROM ({production_only}))"
        ))
        .await?;
    let row = counts
        .rows
        .first()
        .context("primary-key count returned no row")?;
    let ci_only_count = parse_u64(row.first().and_then(Option::as_deref))?;
    let production_only_count = parse_u64(row.get(1).and_then(Option::as_deref))?;
    let ci_examples = if ci_only_count == 0 {
        vec![]
    } else {
        client
            .execute(&format!(
                "SELECT * FROM ({ci_only}) ORDER BY {selected} LIMIT {sample_limit}"
            ))
            .await?
            .rows
    };
    let production_examples = if production_only_count == 0 {
        vec![]
    } else {
        client
            .execute(&format!(
                "SELECT * FROM ({production_only}) ORDER BY {selected} LIMIT {sample_limit}"
            ))
            .await?
            .rows
    };
    Ok(PrimaryKeyComparison {
        columns: keys.to_vec(),
        ci_only_count,
        production_only_count,
        ci_only_examples: ci_examples,
        production_only_examples: production_examples,
    })
}

fn column_map(columns: &[ResultColumn]) -> BTreeMap<String, ResultColumn> {
    columns
        .iter()
        .map(|column| (column.name.clone(), column.clone()))
        .collect()
}

fn resolve_column(columns: &BTreeMap<String, ResultColumn>, configured: &str) -> Option<String> {
    if columns.contains_key(configured) {
        return Some(configured.to_owned());
    }
    let mut matches = columns
        .keys()
        .filter(|name| name.eq_ignore_ascii_case(configured));
    let found = matches.next()?.clone();
    if matches.next().is_some() {
        None
    } else {
        Some(found)
    }
}

fn is_numeric(data_type: &str) -> bool {
    let normalized = data_type.to_ascii_lowercase();
    normalized.starts_with("number(")
        || normalized.starts_with("numeric(")
        || normalized.starts_with("decimal(")
        || matches!(
            normalized.as_str(),
            "fixed"
                | "real"
                | "number"
                | "numeric"
                | "decimal"
                | "float"
                | "double"
                | "int"
                | "integer"
                | "bigint"
                | "smallint"
        )
}

fn is_orderable(data_type: &str) -> bool {
    !matches!(
        data_type
            .to_ascii_uppercase()
            .split('(')
            .next()
            .unwrap_or_default(),
        "ARRAY" | "OBJECT" | "VARIANT" | "BINARY" | "GEOGRAPHY" | "GEOMETRY"
    )
}

fn equivalent_type(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

fn relative_change(current: f64, production: f64) -> f64 {
    if current == production {
        0.0
    } else {
        (current - production).abs() / production.abs().max(1.0)
    }
}

fn parse_u64(value: Option<&str>) -> Result<u64> {
    value
        .context("Snowflake returned NULL for an integer metric")?
        .parse()
        .context("Snowflake returned an invalid integer metric")
}

fn parse_optional_f64(value: Option<&str>) -> Result<Option<f64>> {
    value
        .map(|value| {
            let parsed: f64 = value
                .parse()
                .context("Snowflake returned an invalid numeric metric")?;
            if !parsed.is_finite() {
                bail!("Snowflake returned a non-finite numeric metric: {value}");
            }
            Ok(parsed)
        })
        .transpose()
}

fn finding(model: &str, check: &str, message: String) -> Finding {
    Finding {
        model: model.to_owned(),
        check: check.to_owned(),
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_changes_handle_zero() {
        assert_eq!(relative_change(0.0, 0.0), 0.0);
        assert_eq!(relative_change(1.0, 0.0), 1.0);
        assert_eq!(relative_change(90.0, 100.0), 0.1);
    }

    #[test]
    fn non_finite_metrics_cannot_break_json_output() {
        assert!(parse_optional_f64(Some("NaN")).is_err());
        assert!(parse_optional_f64(Some("inf")).is_err());
    }
}
