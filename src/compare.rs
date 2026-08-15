use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};

use crate::{
    config::{ComparisonMode, KeyPolicy, SafetyConfig, Thresholds},
    report::{ColumnComparison, ColumnMetrics, Finding, ModelComparison, PrimaryKeyComparison},
    snowflake::{Relation, ResultColumn, SnowflakeClient, quote_identifier},
};

pub async fn compare_model(
    client: &SnowflakeClient,
    model_id: &str,
    ci: &Relation,
    production: &Relation,
    options: CompareOptions<'_>,
) -> Result<(ModelComparison, Vec<Finding>)> {
    let CompareOptions {
        primary_key,
        where_clause,
        mode,
        key_policy,
        safety,
        thresholds,
    } = options;
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

    let ci_metrics = relation_metrics(client, ci, &common, &ci_map, where_clause, mode).await?;
    let prod_metrics = relation_metrics(
        client,
        production,
        &common,
        &production_map,
        where_clause,
        mode,
    )
    .await?;
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
            let cardinality_threshold = effective_cardinality_threshold(mode, thresholds);
            if cardinality_change > cardinality_threshold {
                let qualifier = if mode == ComparisonMode::Quick {
                    "estimated "
                } else {
                    ""
                };
                findings.push(finding(model_id, "cardinality", format!(
                    "column {name} {qualifier}cardinality is {} in CI vs {} in production (relative change {:.6}, allowed {:.6})",
                    ci_value.cardinality, prod_value.cardinality, cardinality_change, cardinality_threshold,
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
            where_clause,
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
        if key_integrity_fails(&comparison, key_policy) {
            findings.push(finding(
                model_id,
                "primary_key",
                format!(
                    "CI has {} duplicate keys ({} extra rows) and {} null-key rows vs {} ({} extra rows) and {} in production ({})",
                    comparison.ci_duplicate_key_count,
                    comparison.ci_duplicate_rows,
                    comparison.ci_null_key_rows,
                    comparison.production_duplicate_key_count,
                    comparison.production_duplicate_rows,
                    comparison.production_null_key_rows,
                    match key_policy {
                        KeyPolicy::Regression => "regressions fail",
                        KeyPolicy::Strict => "strict policy requires zero",
                    }
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

fn key_integrity_fails(comparison: &PrimaryKeyComparison, policy: KeyPolicy) -> bool {
    match policy {
        KeyPolicy::Regression => {
            comparison.ci_duplicate_key_count > comparison.production_duplicate_key_count
                || comparison.ci_duplicate_rows > comparison.production_duplicate_rows
                || comparison.ci_null_key_rows > comparison.production_null_key_rows
        }
        KeyPolicy::Strict => {
            comparison.ci_duplicate_key_count > 0 || comparison.ci_null_key_rows > 0
        }
    }
}

pub struct CompareOptions<'a> {
    pub primary_key: &'a [String],
    pub where_clause: Option<&'a str>,
    pub mode: ComparisonMode,
    pub key_policy: KeyPolicy,
    pub safety: &'a SafetyConfig,
    pub thresholds: Thresholds,
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
    where_clause: Option<&str>,
    mode: ComparisonMode,
) -> Result<RelationMetrics> {
    let mut expressions = vec!["COUNT(*)".to_owned()];
    for name in names {
        let column = quote_identifier(name);
        expressions.push(format!("COUNT_IF({column} IS NULL)"));
        expressions.push(match mode {
            ComparisonMode::Quick => format!("APPROX_COUNT_DISTINCT({column})"),
            ComparisonMode::Deep => format!("COUNT(DISTINCT {column})"),
        });
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
            expressions.push(format!("AVG({column})::DOUBLE"));
            if mode == ComparisonMode::Deep {
                expressions.extend([
                    format!("APPROX_PERCENTILE({column}, 0.05)::DOUBLE"),
                    format!("APPROX_PERCENTILE({column}, 0.50)::DOUBLE"),
                    format!("APPROX_PERCENTILE({column}, 0.95)::DOUBLE"),
                ]);
            }
        }
    }
    let source = filtered_relation(relation, where_clause);
    let result = client
        .execute(&format!(
            "SELECT {} FROM {}",
            expressions.join(", "),
            source
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
            index += 1;
            if mode == ComparisonMode::Deep {
                metrics.p05 = parse_optional_f64(row.get(index).and_then(Option::as_deref))?;
                metrics.p50 = parse_optional_f64(row.get(index + 1).and_then(Option::as_deref))?;
                metrics.p95 = parse_optional_f64(row.get(index + 2).and_then(Option::as_deref))?;
                index += 3;
            }
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
    where_clause: Option<&str>,
) -> Result<PrimaryKeyComparison> {
    let selected = keys
        .iter()
        .map(|key| quote_identifier(key))
        .collect::<Vec<_>>()
        .join(", ");
    let ci_source = filtered_relation(ci, where_clause);
    let production_source = filtered_relation(production, where_clause);
    let ci_keys = format!(
        "SELECT {selected}, COUNT(*) AS KEY_ROWS, 1 AS PRESENT FROM {ci_source} GROUP BY {selected}"
    );
    let production_keys = format!(
        "SELECT {selected}, COUNT(*) AS KEY_ROWS, 1 AS PRESENT FROM {production_source} GROUP BY {selected}"
    );
    let join = keys
        .iter()
        .map(|key| {
            let key = quote_identifier(key);
            format!("EQUAL_NULL(C.{key}, P.{key})")
        })
        .collect::<Vec<_>>()
        .join(" AND ");
    let ci_null = keys
        .iter()
        .map(|key| format!("C.{} IS NULL", quote_identifier(key)))
        .collect::<Vec<_>>()
        .join(" OR ");
    let production_null = keys
        .iter()
        .map(|key| format!("P.{} IS NULL", quote_identifier(key)))
        .collect::<Vec<_>>()
        .join(" OR ");
    let ci_null_unqualified = keys
        .iter()
        .map(|key| format!("{} IS NULL", quote_identifier(key)))
        .collect::<Vec<_>>()
        .join(" OR ");
    let counts = client
        .execute(&format!(
            "WITH CI_KEYS AS ({ci_keys}), PRODUCTION_KEYS AS ({production_keys}) \
             SELECT COUNT_IF(P.PRESENT IS NULL), COUNT_IF(C.PRESENT IS NULL), \
             COUNT_IF(C.PRESENT IS NOT NULL AND NOT ({ci_null}) AND C.KEY_ROWS > 1), \
             COUNT_IF(P.PRESENT IS NOT NULL AND NOT ({production_null}) AND P.KEY_ROWS > 1), \
             COALESCE(SUM(IFF(C.PRESENT IS NOT NULL AND NOT ({ci_null}), GREATEST(C.KEY_ROWS - 1, 0), 0)), 0), \
             COALESCE(SUM(IFF(P.PRESENT IS NOT NULL AND NOT ({production_null}), GREATEST(P.KEY_ROWS - 1, 0), 0)), 0), \
             COALESCE(SUM(IFF(C.PRESENT IS NOT NULL AND ({ci_null}), C.KEY_ROWS, 0)), 0), \
             COALESCE(SUM(IFF(P.PRESENT IS NOT NULL AND ({production_null}), P.KEY_ROWS, 0)), 0) \
             FROM CI_KEYS C FULL OUTER JOIN PRODUCTION_KEYS P ON {join}"
        ))
        .await?;
    let row = counts
        .rows
        .first()
        .context("primary-key count returned no row")?;
    let ci_only_count = parse_u64(row.first().and_then(Option::as_deref))?;
    let production_only_count = parse_u64(row.get(1).and_then(Option::as_deref))?;
    let ci_duplicate_key_count = parse_u64(row.get(2).and_then(Option::as_deref))?;
    let production_duplicate_key_count = parse_u64(row.get(3).and_then(Option::as_deref))?;
    let ci_duplicate_rows = parse_u64(row.get(4).and_then(Option::as_deref))?;
    let production_duplicate_rows = parse_u64(row.get(5).and_then(Option::as_deref))?;
    let ci_null_key_rows = parse_u64(row.get(6).and_then(Option::as_deref))?;
    let production_null_key_rows = parse_u64(row.get(7).and_then(Option::as_deref))?;
    let ci_selected = keys
        .iter()
        .map(|key| format!("C.{}", quote_identifier(key)))
        .collect::<Vec<_>>()
        .join(", ");
    let production_selected = keys
        .iter()
        .map(|key| format!("P.{}", quote_identifier(key)))
        .collect::<Vec<_>>()
        .join(", ");
    let ci_examples = if ci_only_count == 0 {
        vec![]
    } else {
        client
            .execute(&format!(
                "WITH CI_KEYS AS ({ci_keys}), PRODUCTION_KEYS AS ({production_keys}) \
                 SELECT {ci_selected} FROM CI_KEYS C LEFT JOIN PRODUCTION_KEYS P ON {join} \
                 WHERE P.PRESENT IS NULL ORDER BY {ci_selected} LIMIT {sample_limit}"
            ))
            .await?
            .rows
    };
    let production_examples = if production_only_count == 0 {
        vec![]
    } else {
        client
            .execute(&format!(
                "WITH CI_KEYS AS ({ci_keys}), PRODUCTION_KEYS AS ({production_keys}) \
                 SELECT {production_selected} FROM PRODUCTION_KEYS P LEFT JOIN CI_KEYS C ON {join} \
                 WHERE C.PRESENT IS NULL ORDER BY {production_selected} LIMIT {sample_limit}"
            ))
            .await?
            .rows
    };
    let ci_duplicate_examples = if ci_duplicate_rows == 0 {
        vec![]
    } else {
        client
            .execute(&format!(
                "SELECT {selected} FROM (SELECT * FROM {ci_source}) AS CI_SOURCE \
                 WHERE NOT ({ci_null_unqualified}) \
                 GROUP BY {selected} HAVING COUNT(*) > 1 \
                 ORDER BY COUNT(*) DESC, {selected} LIMIT {sample_limit}"
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
        ci_duplicate_key_count,
        production_duplicate_key_count,
        ci_duplicate_rows,
        production_duplicate_rows,
        ci_null_key_rows,
        production_null_key_rows,
        ci_duplicate_examples,
    })
}

fn filtered_relation(relation: &Relation, where_clause: Option<&str>) -> String {
    match where_clause {
        Some(predicate) => format!("{} WHERE ({predicate})", relation.sql()),
        None => relation.sql(),
    }
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

fn effective_cardinality_threshold(mode: ComparisonMode, thresholds: Thresholds) -> f64 {
    match mode {
        // Snowflake's HLL estimate is fast but should not be treated as sub-percent precision.
        ComparisonMode::Quick => thresholds.cardinality_relative.max(0.02),
        ComparisonMode::Deep => thresholds.cardinality_relative,
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

    #[test]
    fn filters_are_applied_inside_each_relation() {
        let relation = Relation {
            database: "D".into(),
            schema: "S".into(),
            identifier: "ORDERS".into(),
        };
        assert_eq!(
            filtered_relation(&relation, Some("ORDER_DATE >= CURRENT_DATE - 30")),
            r#""D"."S"."ORDERS" WHERE (ORDER_DATE >= CURRENT_DATE - 30)"#
        );
    }

    #[test]
    fn quick_cardinality_has_an_estimation_margin() {
        let thresholds = Thresholds::default();
        assert_eq!(
            effective_cardinality_threshold(ComparisonMode::Quick, thresholds),
            0.02
        );
        assert_eq!(
            effective_cardinality_threshold(ComparisonMode::Deep, thresholds),
            thresholds.cardinality_relative
        );
    }

    fn key_metrics(
        ci_duplicate_keys: u64,
        production_duplicate_keys: u64,
        ci_duplicate_rows: u64,
        production_duplicate_rows: u64,
        ci_nulls: u64,
        production_nulls: u64,
    ) -> PrimaryKeyComparison {
        PrimaryKeyComparison {
            columns: vec!["ID".into()],
            ci_only_count: 0,
            production_only_count: 0,
            ci_only_examples: vec![],
            production_only_examples: vec![],
            ci_duplicate_key_count: ci_duplicate_keys,
            production_duplicate_key_count: production_duplicate_keys,
            ci_duplicate_rows,
            production_duplicate_rows,
            ci_null_key_rows: ci_nulls,
            production_null_key_rows: production_nulls,
            ci_duplicate_examples: vec![],
        }
    }

    #[test]
    fn regression_policy_allows_existing_key_debt_but_not_worse_ci() {
        assert!(!key_integrity_fails(
            &key_metrics(1, 1, 4, 4, 2, 2),
            KeyPolicy::Regression
        ));
        assert!(key_integrity_fails(
            &key_metrics(2, 1, 4, 4, 2, 2),
            KeyPolicy::Regression
        ));
        assert!(key_integrity_fails(
            &key_metrics(1, 1, 4, 4, 3, 2),
            KeyPolicy::Regression
        ));
    }

    #[test]
    fn strict_policy_requires_clean_ci_keys() {
        assert!(key_integrity_fails(
            &key_metrics(1, 10, 1, 20, 0, 0),
            KeyPolicy::Strict
        ));
        assert!(!key_integrity_fails(
            &key_metrics(0, 10, 0, 20, 0, 5),
            KeyPolicy::Strict
        ));
    }

    #[test]
    fn key_columns_resolve_case_only_when_unambiguous() {
        let unique = column_map(&[ResultColumn {
            name: "ORDER_ID".into(),
            data_type: "NUMBER".into(),
        }]);
        assert_eq!(resolve_column(&unique, "order_id"), Some("ORDER_ID".into()));

        let ambiguous = column_map(&[
            ResultColumn {
                name: "ID".into(),
                data_type: "NUMBER".into(),
            },
            ResultColumn {
                name: "id".into(),
                data_type: "NUMBER".into(),
            },
        ]);
        assert_eq!(resolve_column(&ambiguous, "Id"), None);
    }
}
