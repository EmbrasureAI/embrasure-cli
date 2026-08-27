use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};

use crate::{
    config::SafetyConfig,
    provider::{QueryExecutor, Relation, ResultColumn, SqlDialect},
    report::{
        QueryCheckReport, QueryCheckStatus, QueryColumnComparison, QueryColumnMismatch,
        QueryComparison, QueryDiffExample,
    },
};

#[cfg(test)]
use crate::provider::{ProviderFuture, QueryResult};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct RefTarget {
    pub package: Option<String>,
    pub name: String,
}

impl RefTarget {
    pub fn display(&self) -> String {
        self.package.as_ref().map_or_else(
            || self.name.clone(),
            |package| format!("{package}.{}", self.name),
        )
    }
}

#[derive(Debug, Clone)]
enum TemplatePart {
    Sql(String),
    Ref(RefTarget),
}

#[derive(Debug, Clone)]
pub(crate) struct QueryTemplate {
    parts: Vec<TemplatePart>,
    refs: Vec<RefTarget>,
}

impl QueryTemplate {
    pub fn parse(sql: &str) -> Result<Self> {
        let normalized = normalize_statement(sql)?;
        let first = first_keyword(&normalized).context("SQL query is empty")?;
        if !matches!(first.as_str(), "SELECT" | "WITH" | "VALUES") {
            bail!("SQL must be one SELECT, WITH, or VALUES query expression");
        }
        let mut parts = Vec::new();
        let mut refs = Vec::new();
        let mut literal_start = 0;
        let mut index = 0;
        let bytes = normalized.as_bytes();
        let mut state = LexState::Normal;
        while index < bytes.len() {
            match state {
                LexState::Normal => {
                    if starts(bytes, index, b"--") {
                        state = LexState::LineComment;
                        index += 2;
                    } else if starts(bytes, index, b"/*") {
                        state = LexState::BlockComment;
                        index += 2;
                    } else if starts(bytes, index, b"$$") {
                        state = LexState::DollarString;
                        index += 2;
                    } else if bytes[index] == b'\'' {
                        state = LexState::SingleQuote;
                        index += 1;
                    } else if bytes[index] == b'"' {
                        state = LexState::DoubleQuote;
                        index += 1;
                    } else if starts(bytes, index, b"{{") {
                        if literal_start < index {
                            parts.push(TemplatePart::Sql(normalized[literal_start..index].into()));
                        }
                        let end = normalized[index + 2..]
                            .find("}}")
                            .map(|offset| index + 2 + offset)
                            .context("unterminated Jinja expression")?;
                        let target = parse_ref(&normalized[index + 2..end])?;
                        refs.push(target.clone());
                        parts.push(TemplatePart::Ref(target));
                        index = end + 2;
                        literal_start = index;
                    } else if starts(bytes, index, b"{%") || starts(bytes, index, b"{#") {
                        bail!("only dbt ref() expressions are supported in query checks");
                    } else {
                        index += 1;
                    }
                }
                LexState::SingleQuote => {
                    if bytes[index] == b'\'' {
                        if bytes.get(index + 1) == Some(&b'\'') {
                            index += 2;
                        } else {
                            state = LexState::Normal;
                            index += 1;
                        }
                    } else {
                        index += 1;
                    }
                }
                LexState::DoubleQuote => {
                    if bytes[index] == b'"' {
                        if bytes.get(index + 1) == Some(&b'"') {
                            index += 2;
                        } else {
                            state = LexState::Normal;
                            index += 1;
                        }
                    } else {
                        index += 1;
                    }
                }
                LexState::LineComment => {
                    if bytes[index] == b'\n' {
                        state = LexState::Normal;
                    }
                    index += 1;
                }
                LexState::BlockComment => {
                    if starts(bytes, index, b"*/") {
                        state = LexState::Normal;
                        index += 2;
                    } else {
                        index += 1;
                    }
                }
                LexState::DollarString => {
                    if starts(bytes, index, b"$$") {
                        state = LexState::Normal;
                        index += 2;
                    } else {
                        index += 1;
                    }
                }
            }
        }
        ensure_terminated(state)?;
        if literal_start < normalized.len() {
            parts.push(TemplatePart::Sql(normalized[literal_start..].into()));
        }
        refs.sort();
        refs.dedup();
        Ok(Self { parts, refs })
    }

    pub fn refs(&self) -> &[RefTarget] {
        &self.refs
    }

    pub fn render(&self, mut resolve: impl FnMut(&RefTarget) -> Result<String>) -> Result<String> {
        let mut sql = String::new();
        for part in &self.parts {
            match part {
                TemplatePart::Sql(value) => sql.push_str(value),
                TemplatePart::Ref(target) => sql.push_str(&resolve(target)?),
            }
        }
        Ok(sql)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LexState {
    Normal,
    SingleQuote,
    DoubleQuote,
    LineComment,
    BlockComment,
    DollarString,
}

fn normalize_statement(sql: &str) -> Result<String> {
    if sql.trim().is_empty() {
        bail!("SQL query is empty");
    }
    let bytes = sql.as_bytes();
    let mut state = LexState::Normal;
    let mut semicolons = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        match state {
            LexState::Normal => {
                if starts(bytes, index, b"--") {
                    state = LexState::LineComment;
                    index += 2;
                } else if starts(bytes, index, b"/*") {
                    state = LexState::BlockComment;
                    index += 2;
                } else if starts(bytes, index, b"$$") {
                    state = LexState::DollarString;
                    index += 2;
                } else if bytes[index] == b'\'' {
                    state = LexState::SingleQuote;
                    index += 1;
                } else if bytes[index] == b'"' {
                    state = LexState::DoubleQuote;
                    index += 1;
                } else {
                    if bytes[index] == b';' {
                        semicolons.push(index);
                    }
                    index += 1;
                }
            }
            LexState::SingleQuote => {
                if bytes[index] == b'\'' {
                    if bytes.get(index + 1) == Some(&b'\'') {
                        index += 2;
                    } else {
                        state = LexState::Normal;
                        index += 1;
                    }
                } else {
                    index += 1;
                }
            }
            LexState::DoubleQuote => {
                if bytes[index] == b'"' {
                    if bytes.get(index + 1) == Some(&b'"') {
                        index += 2;
                    } else {
                        state = LexState::Normal;
                        index += 1;
                    }
                } else {
                    index += 1;
                }
            }
            LexState::LineComment => {
                if bytes[index] == b'\n' {
                    state = LexState::Normal;
                }
                index += 1;
            }
            LexState::BlockComment => {
                if starts(bytes, index, b"*/") {
                    state = LexState::Normal;
                    index += 2;
                } else {
                    index += 1;
                }
            }
            LexState::DollarString => {
                if starts(bytes, index, b"$$") {
                    state = LexState::Normal;
                    index += 2;
                } else {
                    index += 1;
                }
            }
        }
    }
    ensure_terminated(state)?;
    match semicolons.as_slice() {
        [] => Ok(sql.trim().into()),
        [semicolon] if comment_or_whitespace_tail(&sql[semicolon + 1..])? => {
            let mut normalized = String::with_capacity(sql.len() - 1);
            normalized.push_str(&sql[..*semicolon]);
            normalized.push_str(&sql[semicolon + 1..]);
            Ok(normalized.trim().into())
        }
        _ => bail!("SQL must contain one query expression, not multiple statements"),
    }
}

fn comment_or_whitespace_tail(tail: &str) -> Result<bool> {
    let bytes = tail.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index].is_ascii_whitespace() {
            index += 1;
        } else if starts(bytes, index, b"--") {
            index = bytes[index + 2..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(bytes.len(), |offset| index + 3 + offset);
        } else if starts(bytes, index, b"/*") {
            let Some(offset) = tail[index + 2..].find("*/") else {
                bail!("unterminated block comment");
            };
            index += offset + 4;
        } else {
            return Ok(false);
        }
    }
    Ok(true)
}

fn first_keyword(sql: &str) -> Option<String> {
    let bytes = sql.as_bytes();
    let mut index = 0;
    loop {
        while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
            index += 1;
        }
        if starts(bytes, index, b"--") {
            index = bytes[index + 2..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(bytes.len(), |offset| index + 3 + offset);
        } else if starts(bytes, index, b"/*") {
            let offset = sql[index + 2..].find("*/")?;
            index += offset + 4;
        } else {
            break;
        }
    }
    let start = index;
    while bytes
        .get(index)
        .is_some_and(|byte| byte.is_ascii_alphabetic())
    {
        index += 1;
    }
    (index > start).then(|| sql[start..index].to_ascii_uppercase())
}

fn ensure_terminated(state: LexState) -> Result<()> {
    match state {
        LexState::Normal | LexState::LineComment => Ok(()),
        LexState::SingleQuote => bail!("unterminated single-quoted string"),
        LexState::DoubleQuote => bail!("unterminated quoted identifier"),
        LexState::BlockComment => bail!("unterminated block comment"),
        LexState::DollarString => bail!("unterminated dollar-quoted string"),
    }
}

fn parse_ref(expression: &str) -> Result<RefTarget> {
    let expression = expression.trim();
    let arguments = expression
        .strip_prefix("ref")
        .and_then(|rest| {
            let rest = rest.trim();
            rest.strip_prefix('(')?.strip_suffix(')')
        })
        .context("only dbt ref() expressions are supported in query checks")?;
    let arguments = split_ref_arguments(arguments)?;
    match arguments.as_slice() {
        [name] => Ok(RefTarget {
            package: None,
            name: name.clone(),
        }),
        [package, name] => Ok(RefTarget {
            package: Some(package.clone()),
            name: name.clone(),
        }),
        _ => bail!("ref() must have one model name or a package and model name"),
    }
}

fn split_ref_arguments(input: &str) -> Result<Vec<String>> {
    let mut values = Vec::new();
    let mut index = 0;
    let bytes = input.as_bytes();
    while index < bytes.len() {
        while bytes
            .get(index)
            .is_some_and(|byte| byte.is_ascii_whitespace())
        {
            index += 1;
        }
        let quote = *bytes
            .get(index)
            .context("ref() arguments must be quoted strings")?;
        if !matches!(quote, b'\'' | b'"') {
            bail!("ref() arguments must be quoted strings");
        }
        index += 1;
        let start = index;
        while bytes.get(index).is_some_and(|byte| *byte != quote) {
            index += 1;
        }
        let end = index;
        if bytes.get(index) != Some(&quote) {
            bail!("unterminated ref() argument");
        }
        values.push(input[start..end].to_owned());
        index += 1;
        while bytes
            .get(index)
            .is_some_and(|byte| byte.is_ascii_whitespace())
        {
            index += 1;
        }
        if index == bytes.len() {
            break;
        }
        if bytes[index] != b',' {
            bail!("ref() arguments must be comma-separated quoted strings");
        }
        index += 1;
    }
    if values.iter().any(String::is_empty) {
        bail!("ref() arguments must not be empty");
    }
    Ok(values)
}

fn starts(bytes: &[u8], index: usize, value: &[u8]) -> bool {
    bytes.get(index..index + value.len()) == Some(value)
}

pub(crate) struct QueryDiffInput<'a> {
    pub name: &'a str,
    pub account: &'a str,
    pub current_refs: Vec<String>,
    pub production_refs: Vec<String>,
    pub candidate_sql: &'a str,
    pub production_sql: &'a str,
    pub candidate: &'a Relation,
    pub production: &'a Relation,
    pub primary_key: &'a [String],
    pub safety: &'a SafetyConfig,
}

#[derive(Clone, Copy)]
struct ExampleLimits {
    rows: usize,
    value_chars: usize,
}

pub(crate) async fn run_query_diff<E: QueryExecutor + ?Sized>(
    executor: &E,
    input: QueryDiffInput<'_>,
) -> Result<QueryCheckReport> {
    let dialect = executor.dialect();
    let mut report = QueryCheckReport {
        name: input.name.into(),
        account: input.account.into(),
        status: QueryCheckStatus::Incomplete,
        current_refs: input.current_refs,
        production_refs: input.production_refs,
        primary_key: input.primary_key.to_vec(),
        candidate_relation: Some(input.candidate.sql(dialect)),
        production_relation: Some(input.production.sql(dialect)),
        candidate_row_count: None,
        production_row_count: None,
        columns: vec![],
        comparison: None,
        reason: None,
        invalid_primary_key_reason: None,
        examples_truncated: false,
    };
    let candidate_columns = preflight(executor, input.candidate_sql).await?;
    let production_columns = preflight(executor, input.production_sql).await?;
    report.invalid_primary_key_reason = primary_key_metadata_error(
        dialect,
        input.primary_key,
        &candidate_columns,
        &production_columns,
    );
    if let Some(failure) = validate_preflight(
        dialect,
        &candidate_columns,
        &production_columns,
        input.safety.max_columns_per_model,
    ) {
        report.reason = Some(failure);
        report.columns = schema_evidence(&candidate_columns, &production_columns);
        return Ok(report);
    }

    executor
        .execute(&dialect.create_table_as(
            input.candidate,
            &format!("SELECT * FROM (\n{}\n)", input.candidate_sql),
        ))
        .await
        .context("could not materialize candidate query")?;
    executor
        .execute(&dialect.create_table_as(
            input.production,
            &format!("SELECT * FROM (\n{}\n)", input.production_sql),
        ))
        .await
        .context("could not materialize production query")?;

    report.columns = schema_evidence(&candidate_columns, &production_columns);
    let (candidate_rows, production_rows) =
        row_counts(executor, input.candidate, input.production).await?;
    report.candidate_row_count = Some(candidate_rows);
    report.production_row_count = Some(production_rows);
    let Some(pairs) = paired_columns(&candidate_columns, &production_columns) else {
        report.status = QueryCheckStatus::Findings;
        report.reason = Some(
            "candidate and production query schemas differ; value comparison was skipped".into(),
        );
        return Ok(report);
    };
    let comparison = if input.primary_key.is_empty() {
        let limits = ExampleLimits {
            rows: input.safety.primary_key_sample_limit,
            value_chars: input.safety.max_example_value_chars,
        };
        compare_unkeyed(
            executor,
            input.candidate,
            input.production,
            &pairs,
            limits,
            &mut report.examples_truncated,
        )
        .await?
    } else {
        let keys = match resolve_keys(input.primary_key, &pairs) {
            Ok(keys) => keys,
            Err(reason) => {
                report.invalid_primary_key_reason = Some(reason.clone());
                report.reason = Some(reason);
                return Ok(report);
            }
        };
        let candidate_integrity = key_integrity(executor, input.candidate, &keys, true).await?;
        let production_integrity = key_integrity(executor, input.production, &keys, false).await?;
        if candidate_integrity.0 > 0
            || candidate_integrity.2 > 0
            || production_integrity.0 > 0
            || production_integrity.2 > 0
        {
            let examples = key_integrity_examples(
                executor,
                input.candidate,
                input.production,
                &keys,
                ExampleLimits {
                    rows: input.safety.primary_key_sample_limit,
                    value_chars: input.safety.max_example_value_chars,
                },
                &mut report.examples_truncated,
            )
            .await?;
            report.status = QueryCheckStatus::Findings;
            report.reason = Some(format!(
                "key integrity blocks value comparison: candidate has {} duplicate keys and {} null-key rows; production has {} duplicate keys and {} null-key rows",
                candidate_integrity.0,
                candidate_integrity.2,
                production_integrity.0,
                production_integrity.2
            ));
            report.comparison = Some(QueryComparison {
                candidate_only_rows: 0,
                production_only_rows: 0,
                changed_rows: 0,
                candidate_duplicate_keys: candidate_integrity.0,
                production_duplicate_keys: production_integrity.0,
                candidate_duplicate_rows: candidate_integrity.1,
                production_duplicate_rows: production_integrity.1,
                candidate_null_key_rows: candidate_integrity.2,
                production_null_key_rows: production_integrity.2,
                column_mismatches: vec![],
                examples,
            });
            return Ok(report);
        }
        compare_keyed(
            executor,
            input.candidate,
            input.production,
            &pairs,
            &keys,
            ExampleLimits {
                rows: input.safety.primary_key_sample_limit,
                value_chars: input.safety.max_example_value_chars,
            },
            &mut report.examples_truncated,
        )
        .await?
    };
    let differs = comparison.candidate_only_rows > 0
        || comparison.production_only_rows > 0
        || comparison.changed_rows > 0;
    report.status = if differs {
        QueryCheckStatus::Findings
    } else {
        QueryCheckStatus::Pass
    };
    if differs {
        report.reason = Some(format!(
            "{} candidate-only, {} production-only, and {} changed rows",
            comparison.candidate_only_rows,
            comparison.production_only_rows,
            comparison.changed_rows
        ));
    }
    report.comparison = Some(comparison);
    Ok(report)
}

async fn preflight<E: QueryExecutor + ?Sized>(
    executor: &E,
    sql: &str,
) -> Result<Vec<ResultColumn>> {
    Ok(executor
        .execute(&format!("SELECT * FROM (\n{sql}\n) LIMIT 0"))
        .await?
        .columns)
}

fn validate_preflight(
    dialect: SqlDialect,
    candidate: &[ResultColumn],
    production: &[ResultColumn],
    max_columns: usize,
) -> Option<String> {
    for (side, columns) in [("candidate", candidate), ("production", production)] {
        if columns.is_empty() {
            return Some(format!("{side} query returned no columns"));
        }
        if columns.len() > max_columns {
            return Some(format!(
                "{side} query has {} columns, above safety.max_columns_per_model {max_columns}",
                columns.len()
            ));
        }
        let mut names = BTreeSet::new();
        for column in columns {
            if dialect.is_unsupported_value(&column.data_type) {
                return Some(format!(
                    "{side} query column {} has unsupported type {}; cast semi-structured, vector, or geospatial values to a scalar type",
                    column.name, column.data_type
                ));
            }
            if column.name.to_ascii_uppercase().starts_with("__EMBRASURE_") {
                return Some(format!(
                    "{side} query column {} uses the reserved __EMBRASURE_ prefix; add a different alias",
                    column.name
                ));
            }
            if !names.insert(column.name.to_ascii_lowercase()) {
                return Some(format!(
                    "{side} query returns duplicate column name {}; add unique aliases",
                    column.name
                ));
            }
        }
    }
    None
}

fn primary_key_metadata_error(
    dialect: SqlDialect,
    configured: &[String],
    candidate: &[ResultColumn],
    production: &[ResultColumn],
) -> Option<String> {
    configured.iter().find_map(|key| {
        let candidate_matches = candidate
            .iter()
            .filter(|column| column.name.eq_ignore_ascii_case(key))
            .collect::<Vec<_>>();
        let production_matches = production
            .iter()
            .filter(|column| column.name.eq_ignore_ascii_case(key))
            .collect::<Vec<_>>();
        match (candidate_matches.as_slice(), production_matches.as_slice()) {
            ([candidate], [production]) => {
                if !candidate
                    .data_type
                    .eq_ignore_ascii_case(&production.data_type)
                {
                    Some(format!(
                        "primary-key column {key} has incompatible types {} and {}",
                        candidate.data_type, production.data_type
                    ))
                } else if dialect.is_unsupported_value(&candidate.data_type)
                    || dialect.is_unsupported_value(&production.data_type)
                {
                    Some(format!(
                        "primary-key column {key} has unsupported type {}",
                        candidate.data_type
                    ))
                } else {
                    None
                }
            }
            ([], _) | (_, []) => Some(format!(
                "primary-key column {key} is missing from one or both query results"
            )),
            _ => Some(format!("primary-key column {key} is ambiguous")),
        }
    })
}

fn schema_evidence(
    candidate: &[ResultColumn],
    production: &[ResultColumn],
) -> Vec<QueryColumnComparison> {
    let candidate = column_map(candidate);
    let production = column_map(production);
    candidate
        .keys()
        .chain(production.keys())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|name| QueryColumnComparison {
            name: candidate
                .get(&name)
                .or_else(|| production.get(&name))
                .map(|column| column.name.clone())
                .unwrap_or(name.clone()),
            candidate_type: candidate.get(&name).map(|column| column.data_type.clone()),
            production_type: production.get(&name).map(|column| column.data_type.clone()),
        })
        .collect()
}

fn paired_columns<'a>(
    candidate: &'a [ResultColumn],
    production: &'a [ResultColumn],
) -> Option<Vec<(&'a ResultColumn, &'a ResultColumn)>> {
    let candidate_map = column_map(candidate);
    let production_map = column_map(production);
    if candidate_map.len() != production_map.len() || candidate_map.keys().ne(production_map.keys())
    {
        return None;
    }
    candidate_map
        .into_iter()
        .map(|(name, candidate)| {
            let production = production_map.get(&name)?;
            candidate
                .data_type
                .eq_ignore_ascii_case(&production.data_type)
                .then_some((candidate, *production))
        })
        .collect()
}

fn column_map(columns: &[ResultColumn]) -> BTreeMap<String, &ResultColumn> {
    columns
        .iter()
        .map(|column| (column.name.to_ascii_lowercase(), column))
        .collect()
}

async fn row_counts<E: QueryExecutor + ?Sized>(
    executor: &E,
    candidate: &Relation,
    production: &Relation,
) -> Result<(u64, u64)> {
    let dialect = executor.dialect();
    let result = executor
        .execute(&format!(
            "SELECT (SELECT COUNT(*) FROM {}), (SELECT COUNT(*) FROM {})",
            candidate.sql(dialect),
            production.sql(dialect)
        ))
        .await?;
    let row = result
        .rows
        .first()
        .context("row-count query returned no row")?;
    Ok((parse_u64(row.first())?, parse_u64(row.get(1))?))
}

type ColumnPair<'a> = (&'a ResultColumn, &'a ResultColumn);

fn resolve_keys<'a>(
    configured: &[String],
    pairs: &[ColumnPair<'a>],
) -> Result<Vec<ColumnPair<'a>>, String> {
    let mut resolved = Vec::new();
    for key in configured {
        let matches = pairs
            .iter()
            .filter(|(candidate, production)| {
                candidate.name.eq_ignore_ascii_case(key)
                    || production.name.eq_ignore_ascii_case(key)
            })
            .copied()
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [pair] => resolved.push(*pair),
            [] => {
                return Err(format!(
                    "primary-key column {key} is missing from one or both query results"
                ));
            }
            _ => return Err(format!("primary-key column {key} is ambiguous")),
        }
    }
    Ok(resolved)
}

async fn key_integrity<E: QueryExecutor + ?Sized>(
    executor: &E,
    relation: &Relation,
    keys: &[ColumnPair<'_>],
    candidate_side: bool,
) -> Result<(u64, u64, u64)> {
    let dialect = executor.dialect();
    let names = keys
        .iter()
        .map(|pair| {
            dialect.quote_identifier(if candidate_side {
                &pair.0.name
            } else {
                &pair.1.name
            })
        })
        .collect::<Vec<_>>();
    let nulls = names
        .iter()
        .map(|name| format!("{name} IS NULL"))
        .collect::<Vec<_>>()
        .join(" OR ");
    let duplicate_keys =
        dialect.conditional(&format!("NOT ({nulls}) AND __EMBRASURE_N > 1"), "1", "0");
    let duplicate_rows = dialect.conditional(
        &format!("NOT ({nulls}) AND __EMBRASURE_N > 1"),
        "__EMBRASURE_N - 1",
        "0",
    );
    let null_rows = dialect.conditional(&format!("({nulls})"), "__EMBRASURE_N", "0");
    let result = executor
        .execute(&format!(
            "SELECT COALESCE(SUM({duplicate_keys}), 0), \
             COALESCE(SUM({duplicate_rows}), 0), \
             COALESCE(SUM({null_rows}), 0) \
             FROM (SELECT {}, COUNT(*) AS __EMBRASURE_N FROM {} GROUP BY {})",
            names.join(", "),
            relation.sql(dialect),
            names.join(", ")
        ))
        .await?;
    let row = result
        .rows
        .first()
        .context("key-integrity query returned no row")?;
    Ok((
        parse_u64(row.first())?,
        parse_u64(row.get(1))?,
        parse_u64(row.get(2))?,
    ))
}

async fn key_integrity_examples<E: QueryExecutor + ?Sized>(
    executor: &E,
    candidate: &Relation,
    production: &Relation,
    keys: &[ColumnPair<'_>],
    limits: ExampleLimits,
    examples_truncated: &mut bool,
) -> Result<Vec<QueryDiffExample>> {
    if limits.rows == 0 {
        return Ok(vec![]);
    }
    let dialect = executor.dialect();
    let canonical = keys
        .iter()
        .map(|pair| dialect.quote_identifier(&pair.0.name))
        .collect::<Vec<_>>();
    let candidate_keys = keys
        .iter()
        .map(|pair| {
            format!(
                "{} AS {}",
                dialect.quote_identifier(&pair.0.name),
                dialect.quote_identifier(&pair.0.name)
            )
        })
        .collect::<Vec<_>>();
    let production_keys = keys
        .iter()
        .map(|pair| {
            format!(
                "{} AS {}",
                dialect.quote_identifier(&pair.1.name),
                dialect.quote_identifier(&pair.0.name)
            )
        })
        .collect::<Vec<_>>();
    let nulls = canonical
        .iter()
        .map(|name| format!("{name} IS NULL"))
        .collect::<Vec<_>>()
        .join(" OR ");
    let values = keys
        .iter()
        .map(|pair| bounded_value(dialect, "X", &pair.0.name, limits.value_chars))
        .collect::<Vec<_>>();
    let ordering = canonical
        .iter()
        .map(|name| format!("X.{name} NULLS FIRST"))
        .chain(std::iter::once("X.__EMBRASURE_SIDE".into()))
        .collect::<Vec<_>>()
        .join(", ");
    let result = executor
        .execute(&format!(
            "WITH C_KEYS AS (SELECT {}, COUNT(*) AS __EMBRASURE_N FROM {} GROUP BY {}), \
             P_KEYS AS (SELECT {}, COUNT(*) AS __EMBRASURE_N FROM {} GROUP BY {}), \
             ISSUES AS (\
               SELECT 'candidate' AS __EMBRASURE_SIDE, * FROM C_KEYS WHERE __EMBRASURE_N > 1 OR ({nulls}) \
               UNION ALL \
               SELECT 'production' AS __EMBRASURE_SIDE, * FROM P_KEYS WHERE __EMBRASURE_N > 1 OR ({nulls})\
             ) \
             SELECT X.__EMBRASURE_SIDE, {}, {} FROM ISSUES X \
             ORDER BY {ordering} LIMIT {}",
            candidate_keys.join(", "),
            candidate.sql(dialect),
            canonical.join(", "),
            production_keys.join(", "),
            production.sql(dialect),
            canonical.join(", "),
            values.join(", "),
            dialect.cast_text("X.__EMBRASURE_N"),
            limits.rows
        ))
        .await?;
    result
        .rows
        .into_iter()
        .map(|mut row| {
            let side = row
                .first()
                .and_then(Option::as_deref)
                .context("key-integrity example omitted its side")?
                .to_owned();
            let end = 1 + keys.len();
            let multiplicity = parse_u64(row.get(end))?;
            truncate_row(&mut row[1..end], limits.value_chars, examples_truncated);
            Ok(QueryDiffExample {
                key: row[1..end].to_vec(),
                candidate: vec![],
                production: vec![],
                candidate_multiplicity: (side == "candidate").then_some(multiplicity),
                production_multiplicity: (side == "production").then_some(multiplicity),
            })
        })
        .collect()
}

async fn compare_keyed<E: QueryExecutor + ?Sized>(
    executor: &E,
    candidate: &Relation,
    production: &Relation,
    pairs: &[ColumnPair<'_>],
    keys: &[ColumnPair<'_>],
    limits: ExampleLimits,
    examples_truncated: &mut bool,
) -> Result<QueryComparison> {
    let dialect = executor.dialect();
    let join = keys
        .iter()
        .map(|pair| {
            dialect.full_join_equal(
                &format!("C.{}", dialect.quote_identifier(&pair.0.name)),
                &format!("P.{}", dialect.quote_identifier(&pair.1.name)),
            )
        })
        .collect::<Vec<_>>()
        .join(" AND ");
    let value_pairs = pairs
        .iter()
        .filter(|pair| {
            !keys
                .iter()
                .any(|key| key.0.name.eq_ignore_ascii_case(&pair.0.name))
        })
        .copied()
        .collect::<Vec<_>>();
    let changed = if value_pairs.is_empty() {
        "FALSE".into()
    } else {
        value_pairs
            .iter()
            .map(|pair| {
                format!(
                    "NOT {}",
                    dialect.null_safe_equal(
                        &format!("C.{}", dialect.quote_identifier(&pair.0.name)),
                        &format!("P.{}", dialect.quote_identifier(&pair.1.name)),
                    )
                )
            })
            .collect::<Vec<_>>()
            .join(" OR ")
    };
    let candidate_present = format!(
        "C.{} IS NOT NULL",
        dialect.quote_identifier(&keys[0].0.name)
    );
    let production_present = format!(
        "P.{} IS NOT NULL",
        dialect.quote_identifier(&keys[0].1.name)
    );
    let mut metrics = vec![
        format!(
            "COALESCE({}, 0)",
            dialect.count_if(&format!("NOT ({production_present})"))
        ),
        format!(
            "COALESCE({}, 0)",
            dialect.count_if(&format!("NOT ({candidate_present})"))
        ),
        format!(
            "COALESCE({}, 0)",
            dialect.count_if(&format!(
                "({candidate_present}) AND ({production_present}) AND ({changed})"
            ))
        ),
    ];
    metrics.extend(value_pairs.iter().map(|pair| {
        let different = format!(
            "NOT {}",
            dialect.null_safe_equal(
                &format!("C.{}", dialect.quote_identifier(&pair.0.name)),
                &format!("P.{}", dialect.quote_identifier(&pair.1.name)),
            )
        );
        format!(
            "COALESCE({}, 0)",
            dialect.count_if(&format!(
                "({candidate_present}) AND ({production_present}) AND {different}"
            ))
        )
    }));
    let counts = executor
        .execute(&format!(
            "SELECT {} FROM {} C FULL OUTER JOIN {} P ON {join}",
            metrics.join(", "),
            candidate.sql(dialect),
            production.sql(dialect)
        ))
        .await?;
    let row = counts
        .rows
        .first()
        .context("keyed comparison returned no row")?;
    let candidate_only_rows = parse_u64(row.first())?;
    let production_only_rows = parse_u64(row.get(1))?;
    let changed_rows = parse_u64(row.get(2))?;
    let column_mismatches = value_pairs
        .iter()
        .enumerate()
        .map(|(index, pair)| {
            Ok(QueryColumnMismatch {
                column: pair.0.name.clone(),
                rows: parse_u64(row.get(index + 3))?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let examples = if candidate_only_rows + production_only_rows + changed_rows == 0 {
        vec![]
    } else {
        keyed_examples(
            executor,
            candidate,
            production,
            pairs,
            keys,
            &join,
            &changed,
            &candidate_present,
            &production_present,
            limits.rows,
            limits.value_chars,
            examples_truncated,
        )
        .await?
    };
    Ok(QueryComparison {
        candidate_only_rows,
        production_only_rows,
        changed_rows,
        candidate_duplicate_keys: 0,
        production_duplicate_keys: 0,
        candidate_duplicate_rows: 0,
        production_duplicate_rows: 0,
        candidate_null_key_rows: 0,
        production_null_key_rows: 0,
        column_mismatches,
        examples,
    })
}

#[allow(clippy::too_many_arguments)]
async fn keyed_examples<E: QueryExecutor + ?Sized>(
    executor: &E,
    candidate: &Relation,
    production: &Relation,
    pairs: &[ColumnPair<'_>],
    keys: &[ColumnPair<'_>],
    join: &str,
    changed: &str,
    candidate_present: &str,
    production_present: &str,
    sample_limit: usize,
    value_limit: usize,
    examples_truncated: &mut bool,
) -> Result<Vec<QueryDiffExample>> {
    let dialect = executor.dialect();
    let key_values = keys
        .iter()
        .map(|pair| bounded_coalesce(dialect, "C", &pair.0.name, "P", &pair.1.name, value_limit))
        .collect::<Vec<_>>();
    let candidate_values = pairs
        .iter()
        .map(|pair| bounded_value(dialect, "C", &pair.0.name, value_limit))
        .collect::<Vec<_>>();
    let production_values = pairs
        .iter()
        .map(|pair| bounded_value(dialect, "P", &pair.1.name, value_limit))
        .collect::<Vec<_>>();
    let selected = key_values
        .iter()
        .chain(&candidate_values)
        .chain(&production_values)
        .cloned()
        .collect::<Vec<_>>();
    let ordering = keys
        .iter()
        .map(|pair| {
            format!(
                "COALESCE(C.{}, P.{}) NULLS FIRST",
                dialect.quote_identifier(&pair.0.name),
                dialect.quote_identifier(&pair.1.name)
            )
        })
        .chain(pairs.iter().flat_map(|pair| {
            [
                format!("C.{} NULLS FIRST", dialect.quote_identifier(&pair.0.name)),
                format!("P.{} NULLS FIRST", dialect.quote_identifier(&pair.1.name)),
            ]
        }))
        .collect::<Vec<_>>()
        .join(", ");
    let result = executor
        .execute(&format!(
            "SELECT {} FROM {} C FULL OUTER JOIN {} P ON {join} \
             WHERE NOT ({candidate_present}) OR NOT ({production_present}) OR ({changed}) \
             ORDER BY {ordering} LIMIT {sample_limit}",
            selected.join(", "),
            candidate.sql(dialect),
            production.sql(dialect)
        ))
        .await?;
    Ok(result
        .rows
        .into_iter()
        .map(|row| {
            let key_end = keys.len();
            let candidate_end = key_end + pairs.len();
            let production_end = candidate_end + pairs.len();
            let mut values = row;
            truncate_row(&mut values, value_limit, examples_truncated);
            QueryDiffExample {
                key: values[..key_end].to_vec(),
                candidate: values[key_end..candidate_end].to_vec(),
                production: values[candidate_end..production_end].to_vec(),
                candidate_multiplicity: None,
                production_multiplicity: None,
            }
        })
        .collect())
}

async fn compare_unkeyed<E: QueryExecutor + ?Sized>(
    executor: &E,
    candidate: &Relation,
    production: &Relation,
    pairs: &[ColumnPair<'_>],
    limits: ExampleLimits,
    examples_truncated: &mut bool,
) -> Result<QueryComparison> {
    let dialect = executor.dialect();
    let candidate_columns = pairs
        .iter()
        .map(|pair| {
            format!(
                "{} AS {}",
                dialect.quote_identifier(&pair.0.name),
                dialect.quote_identifier(&pair.0.name)
            )
        })
        .collect::<Vec<_>>();
    let production_columns = pairs
        .iter()
        .map(|pair| {
            format!(
                "{} AS {}",
                dialect.quote_identifier(&pair.1.name),
                dialect.quote_identifier(&pair.0.name)
            )
        })
        .collect::<Vec<_>>();
    let canonical = pairs
        .iter()
        .map(|pair| dialect.quote_identifier(&pair.0.name))
        .collect::<Vec<_>>();
    let join = canonical
        .iter()
        .map(|name| dialect.full_join_equal(&format!("C.{name}"), &format!("P.{name}")))
        .collect::<Vec<_>>()
        .join(" AND ");
    let cte = format!(
        "C AS (SELECT {}, COUNT(*) AS __EMBRASURE_N FROM {} GROUP BY {}), \
         P AS (SELECT {}, COUNT(*) AS __EMBRASURE_N FROM {} GROUP BY {})",
        candidate_columns.join(", "),
        candidate.sql(dialect),
        canonical.join(", "),
        production_columns.join(", "),
        production.sql(dialect),
        canonical.join(", ")
    );
    let counts = executor
        .execute(&format!(
            "WITH {cte} SELECT \
             COALESCE(SUM(GREATEST(COALESCE(C.__EMBRASURE_N, 0) - COALESCE(P.__EMBRASURE_N, 0), 0)), 0), \
             COALESCE(SUM(GREATEST(COALESCE(P.__EMBRASURE_N, 0) - COALESCE(C.__EMBRASURE_N, 0), 0)), 0) \
             FROM C FULL OUTER JOIN P ON {join}"
        ))
        .await?;
    let row = counts
        .rows
        .first()
        .context("unkeyed comparison returned no row")?;
    let candidate_only_rows = parse_u64(row.first())?;
    let production_only_rows = parse_u64(row.get(1))?;
    let examples = if candidate_only_rows + production_only_rows == 0 {
        vec![]
    } else {
        let candidate_values = pairs
            .iter()
            .map(|pair| bounded_value(dialect, "C", &pair.0.name, limits.value_chars))
            .collect::<Vec<_>>();
        let production_values = pairs
            .iter()
            .map(|pair| bounded_value(dialect, "P", &pair.0.name, limits.value_chars))
            .collect::<Vec<_>>();
        let row_values = pairs
            .iter()
            .map(|pair| {
                format!(
                    "COALESCE(C.{0}, P.{0})",
                    dialect.quote_identifier(&pair.0.name)
                )
            })
            .collect::<Vec<_>>();
        let ordering = std::iter::once(dialect.stable_hash(&row_values))
            .chain(
                row_values
                    .iter()
                    .map(|value| format!("{value} NULLS FIRST")),
            )
            .chain([
                "COALESCE(C.__EMBRASURE_N, 0)".into(),
                "COALESCE(P.__EMBRASURE_N, 0)".into(),
            ])
            .collect::<Vec<_>>()
            .join(", ");
        let selected = candidate_values
            .iter()
            .chain(&production_values)
            .cloned()
            .chain([
                dialect.cast_text("COALESCE(C.__EMBRASURE_N, 0)"),
                dialect.cast_text("COALESCE(P.__EMBRASURE_N, 0)"),
            ])
            .collect::<Vec<_>>();
        let result = executor
            .execute(&format!(
                "WITH {cte} SELECT {} FROM C FULL OUTER JOIN P ON {join} \
                 WHERE COALESCE(C.__EMBRASURE_N, 0) != COALESCE(P.__EMBRASURE_N, 0) \
                 ORDER BY {ordering} LIMIT {sample_limit}",
                selected.join(", "),
                sample_limit = limits.rows
            ))
            .await?;
        result
            .rows
            .into_iter()
            .map(|mut row| {
                let candidate_end = pairs.len();
                let production_end = candidate_end + pairs.len();
                let candidate_multiplicity = parse_u64(row.get(production_end))?;
                let production_multiplicity = parse_u64(row.get(production_end + 1))?;
                truncate_row(
                    &mut row[..production_end],
                    limits.value_chars,
                    examples_truncated,
                );
                Ok(QueryDiffExample {
                    key: vec![],
                    candidate: row[..candidate_end].to_vec(),
                    production: row[candidate_end..production_end].to_vec(),
                    candidate_multiplicity: Some(candidate_multiplicity),
                    production_multiplicity: Some(production_multiplicity),
                })
            })
            .collect::<Result<Vec<_>>>()?
    };
    Ok(QueryComparison {
        candidate_only_rows,
        production_only_rows,
        changed_rows: 0,
        candidate_duplicate_keys: 0,
        production_duplicate_keys: 0,
        candidate_duplicate_rows: 0,
        production_duplicate_rows: 0,
        candidate_null_key_rows: 0,
        production_null_key_rows: 0,
        column_mismatches: vec![],
        examples,
    })
}

fn bounded_value(dialect: SqlDialect, alias: &str, column: &str, limit: usize) -> String {
    let qualified = format!("{alias}.{}", dialect.quote_identifier(column));
    let value = dialect.to_text(&qualified);
    let bounded = dialect.conditional(
        &format!("LENGTH({value}) > {limit}"),
        &format!("CONCAT(LEFT({value}, {limit}), '<truncated>')"),
        &value,
    );
    dialect.conditional(&format!("{qualified} IS NULL"), "NULL", &bounded)
}

fn bounded_coalesce(
    dialect: SqlDialect,
    candidate_alias: &str,
    candidate_column: &str,
    production_alias: &str,
    production_column: &str,
    limit: usize,
) -> String {
    let value = format!(
        "COALESCE({}.{}, {}.{})",
        candidate_alias,
        dialect.quote_identifier(candidate_column),
        production_alias,
        dialect.quote_identifier(production_column)
    );
    let text = dialect.to_text(&value);
    let bounded = dialect.conditional(
        &format!("LENGTH({text}) > {limit}"),
        &format!("CONCAT(LEFT({text}, {limit}), '<truncated>')"),
        &text,
    );
    dialect.conditional(&format!("{value} IS NULL"), "NULL", &bounded)
}

fn truncate_row(row: &mut [Option<String>], limit: usize, truncated: &mut bool) {
    for value in row.iter_mut().flatten() {
        let marked = value.ends_with("<truncated>");
        if marked {
            *truncated = true;
        }
        let allowed = limit + usize::from(marked) * "<truncated>".chars().count();
        if value.chars().count() > allowed {
            let mut shortened = value.chars().take(limit).collect::<String>();
            shortened.push_str("<truncated>");
            *value = shortened;
            *truncated = true;
        }
    }
}

fn parse_u64(value: Option<&Option<String>>) -> Result<u64> {
    value
        .and_then(Option::as_deref)
        .context("warehouse metric was null")?
        .parse()
        .context("warehouse metric was not an unsigned integer")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::VecDeque, sync::Mutex};

    struct FakeExecutor {
        responses: Mutex<VecDeque<QueryResult>>,
        statements: Mutex<Vec<String>>,
    }

    impl FakeExecutor {
        fn new(responses: Vec<QueryResult>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                statements: Mutex::new(vec![]),
            }
        }
    }

    impl QueryExecutor for FakeExecutor {
        fn dialect(&self) -> SqlDialect {
            SqlDialect::Snowflake
        }

        fn execute<'a>(&'a self, statement: &'a str) -> ProviderFuture<'a, QueryResult> {
            Box::pin(async move {
                self.statements.lock().unwrap().push(statement.into());
                self.responses
                    .lock()
                    .unwrap()
                    .pop_front()
                    .context("fake executor ran out of responses")
            })
        }
    }

    fn columns() -> Vec<ResultColumn> {
        vec![
            ResultColumn {
                name: "ID".into(),
                data_type: "NUMBER(38,0)".into(),
            },
            ResultColumn {
                name: "VALUE".into(),
                data_type: "VARCHAR(100)".into(),
            },
        ]
    }

    fn rows(values: &[&[Option<&str>]]) -> QueryResult {
        QueryResult {
            columns: vec![],
            rows: values
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|value| value.map(ToOwned::to_owned))
                        .collect()
                })
                .collect(),
        }
    }

    fn relation(identifier: &str) -> Relation {
        Relation {
            database: "DB".into(),
            schema: "RUN".into(),
            identifier: identifier.into(),
        }
    }

    #[test]
    fn parses_and_renders_refs_without_touching_literals_or_comments() {
        let template = QueryTemplate::parse(
            "SELECT '{{ ref('ignored') }}', $$; {{ nope() }}$$ FROM {{ ref('pkg', 'orders') }}; -- ok",
        )
        .unwrap();
        assert_eq!(template.refs()[0].display(), "pkg.orders");
        let rendered = template
            .render(|target| Ok(format!("RELATION_{}", target.name)))
            .unwrap();
        assert!(rendered.contains("FROM RELATION_orders"));
        assert!(rendered.contains("'{{ ref('ignored') }}'"));
        assert!(rendered.contains("$$; {{ nope() }}$$"));
    }

    #[test]
    fn rejects_multiple_statements_and_unsupported_jinja() {
        assert!(QueryTemplate::parse("SELECT 1; DROP TABLE X").is_err());
        assert!(QueryTemplate::parse("DELETE FROM X").is_err());
        assert!(QueryTemplate::parse("TABLE(GENERATOR(ROWCOUNT => 1))").is_err());
        assert!(QueryTemplate::parse("SELECT {{ var('x') }}").is_err());
    }

    #[test]
    fn trailing_comments_and_semicolons_inside_strings_are_valid() {
        QueryTemplate::parse("SELECT ';' AS X; /* trailing */ -- done").unwrap();
        QueryTemplate::parse("SELECT $$;$$ AS X").unwrap();
    }

    #[test]
    fn duplicate_columns_and_width_are_rejected_before_materialization() {
        let duplicate = vec![
            ResultColumn {
                name: "ID".into(),
                data_type: "NUMBER".into(),
            },
            ResultColumn {
                name: "id".into(),
                data_type: "NUMBER".into(),
            },
        ];
        assert!(
            validate_preflight(SqlDialect::Snowflake, &duplicate, &[], 10)
                .unwrap()
                .contains("duplicate")
        );
        assert!(
            validate_preflight(SqlDialect::Snowflake, &duplicate[..1], &[], 0)
                .unwrap()
                .contains("above")
        );

        let punctuation = vec![
            ResultColumn {
                name: "order;id".into(),
                data_type: "NUMBER".into(),
            },
            ResultColumn {
                name: "ORDER;ID".into(),
                data_type: "NUMBER".into(),
            },
        ];
        assert_eq!(
            primary_key_metadata_error(
                SqlDialect::Snowflake,
                &["order;id".into()],
                &punctuation,
                &punctuation[..1],
            )
            .as_deref(),
            Some("primary-key column order;id is ambiguous")
        );

        let unrelated_then_key = vec![
            ResultColumn {
                name: "amount".into(),
                data_type: "NUMBER".into(),
            },
            ResultColumn {
                name: "AMOUNT".into(),
                data_type: "NUMBER".into(),
            },
            ResultColumn {
                name: "order;id".into(),
                data_type: "NUMBER".into(),
            },
            ResultColumn {
                name: "ORDER;ID".into(),
                data_type: "NUMBER".into(),
            },
        ];
        assert!(
            validate_preflight(SqlDialect::Snowflake, &unrelated_then_key, &[], 10)
                .unwrap()
                .contains("AMOUNT")
        );
        assert_eq!(
            primary_key_metadata_error(
                SqlDialect::Snowflake,
                &["order;id".into()],
                &unrelated_then_key,
                &punctuation[..1]
            )
            .as_deref(),
            Some("primary-key column order;id is ambiguous")
        );

        let typed_key = |data_type: &str| {
            vec![ResultColumn {
                name: "id".into(),
                data_type: data_type.into(),
            }]
        };
        assert!(
            primary_key_metadata_error(
                SqlDialect::Snowflake,
                &["id".into()],
                &typed_key("NUMBER"),
                &typed_key("VARCHAR"),
            )
            .unwrap()
            .contains("incompatible types")
        );
    }

    #[test]
    fn unsupported_values_require_explicit_casts() {
        let dialect = SqlDialect::Snowflake;
        assert!(dialect.is_unsupported_value("VARIANT"));
        assert!(dialect.is_unsupported_value("GEOGRAPHY"));
        assert!(!dialect.is_unsupported_value("VARCHAR(100)"));
    }

    #[test]
    fn report_values_are_defensively_bounded() {
        let mut row = vec![Some("abcdefgh".into()), Some("ok".into())];
        let mut truncated = false;
        truncate_row(&mut row, 4, &mut truncated);
        assert_eq!(row[0].as_deref(), Some("abcd<truncated>"));
        assert_eq!(row[1].as_deref(), Some("ok"));
        assert!(truncated);
    }

    #[tokio::test]
    async fn unsupported_types_stop_before_materialization() {
        let metadata = |data_type: &str| QueryResult {
            columns: vec![ResultColumn {
                name: "VALUE".into(),
                data_type: data_type.into(),
            }],
            rows: vec![],
        };
        let executor = FakeExecutor::new(vec![metadata("VARIANT"), metadata("VARIANT")]);
        let candidate = relation("CANDIDATE");
        let production = relation("PRODUCTION");
        let report = run_query_diff(
            &executor,
            QueryDiffInput {
                name: "unsupported",
                account: "primary",
                current_refs: vec![],
                production_refs: vec![],
                candidate_sql: "SELECT PARSE_JSON('{}') AS VALUE",
                production_sql: "SELECT PARSE_JSON('{}') AS VALUE",
                candidate: &candidate,
                production: &production,
                primary_key: &[],
                safety: &SafetyConfig::default(),
            },
        )
        .await
        .unwrap();
        assert_eq!(report.status, QueryCheckStatus::Incomplete);
        let statements = executor.statements.lock().unwrap();
        assert_eq!(statements.len(), 2);
        assert!(!statements.iter().any(|sql| sql.starts_with("CREATE")));
    }

    #[tokio::test]
    async fn schema_mismatch_materializes_counts_but_skips_value_sql() {
        let executor = FakeExecutor::new(vec![
            QueryResult {
                columns: vec![ResultColumn {
                    name: "ID".into(),
                    data_type: "NUMBER(38,0)".into(),
                }],
                rows: vec![],
            },
            QueryResult {
                columns: columns(),
                rows: vec![],
            },
            QueryResult::default(),
            QueryResult::default(),
            rows(&[&[Some("1"), Some("1")]]),
        ]);
        let candidate = relation("CANDIDATE");
        let production = relation("PRODUCTION");
        let report = run_query_diff(
            &executor,
            QueryDiffInput {
                name: "schema",
                account: "primary",
                current_refs: vec![],
                production_refs: vec![],
                candidate_sql: "SELECT ID FROM C",
                production_sql: "SELECT ID, VALUE FROM P",
                candidate: &candidate,
                production: &production,
                primary_key: &[],
                safety: &SafetyConfig::default(),
            },
        )
        .await
        .unwrap();
        assert_eq!(report.status, QueryCheckStatus::Findings);
        assert!(report.comparison.is_none());
        let statements = executor.statements.lock().unwrap();
        assert_eq!(statements.len(), 5);
        assert!(!statements.iter().any(|sql| sql.contains("FULL OUTER JOIN")));
    }

    #[tokio::test]
    async fn key_integrity_blocks_join_and_returns_ordered_bounded_examples() {
        let metadata = || QueryResult {
            columns: columns(),
            rows: vec![],
        };
        let executor = FakeExecutor::new(vec![
            metadata(),
            metadata(),
            QueryResult::default(),
            QueryResult::default(),
            rows(&[&[Some("3"), Some("1")]]),
            rows(&[&[Some("1"), Some("1"), Some("1")]]),
            rows(&[&[Some("0"), Some("0"), Some("0")]]),
            rows(&[
                &[Some("candidate"), None, Some("1")],
                &[Some("candidate"), Some("1"), Some("2")],
            ]),
        ]);
        let candidate = relation("CANDIDATE");
        let production = relation("PRODUCTION");
        let report = run_query_diff(
            &executor,
            QueryDiffInput {
                name: "integrity",
                account: "primary",
                current_refs: vec![],
                production_refs: vec![],
                candidate_sql: "SELECT ID, VALUE FROM C",
                production_sql: "SELECT ID, VALUE FROM P",
                candidate: &candidate,
                production: &production,
                primary_key: &["ID".into()],
                safety: &SafetyConfig::default(),
            },
        )
        .await
        .unwrap();
        assert_eq!(report.status, QueryCheckStatus::Findings);
        let comparison = report.comparison.unwrap();
        assert_eq!(comparison.candidate_duplicate_keys, 1);
        assert_eq!(comparison.candidate_null_key_rows, 1);
        assert_eq!(comparison.examples.len(), 2);
        assert_eq!(comparison.examples[0].key, vec![None]);
        assert_eq!(comparison.examples[1].candidate_multiplicity, Some(2));
        let statements = executor.statements.lock().unwrap();
        assert!(!statements.iter().any(|sql| sql.contains("FULL OUTER JOIN")));
        assert!(
            statements
                .last()
                .unwrap()
                .contains("ORDER BY X.\"ID\" NULLS FIRST")
        );
    }

    #[tokio::test]
    async fn unkeyed_diff_preserves_duplicate_multiplicity_end_to_end() {
        let metadata = || QueryResult {
            columns: columns(),
            rows: vec![],
        };
        let executor = FakeExecutor::new(vec![
            metadata(),
            metadata(),
            QueryResult::default(),
            QueryResult::default(),
            rows(&[&[Some("123"), Some("1")]]),
            rows(&[&[Some("122"), Some("0")]]),
            rows(&[&[
                Some("1"),
                Some("same"),
                Some("1"),
                Some("same"),
                Some("123"),
                Some("1"),
            ]]),
        ]);
        let safety = SafetyConfig {
            max_example_value_chars: 1,
            ..SafetyConfig::default()
        };
        let candidate = relation("CANDIDATE");
        let production = relation("PRODUCTION");
        let report = run_query_diff(
            &executor,
            QueryDiffInput {
                name: "duplicate_only",
                account: "primary",
                current_refs: vec![],
                production_refs: vec![],
                candidate_sql: "SELECT ID, VALUE FROM SOURCE",
                production_sql: "SELECT ID, VALUE FROM SOURCE",
                candidate: &candidate,
                production: &production,
                primary_key: &[],
                safety: &safety,
            },
        )
        .await
        .unwrap();

        assert_eq!(report.status, QueryCheckStatus::Findings);
        let comparison = report.comparison.unwrap();
        assert_eq!(comparison.candidate_only_rows, 122);
        assert_eq!(comparison.production_only_rows, 0);
        assert_eq!(comparison.examples[0].candidate_multiplicity, Some(123));
        assert_eq!(comparison.examples[0].production_multiplicity, Some(1));
        let statements = executor.statements.lock().unwrap();
        assert_eq!(
            statements
                .iter()
                .filter(|sql| sql.starts_with("CREATE TRANSIENT TABLE"))
                .count(),
            2
        );
        assert!(
            statements
                .iter()
                .any(|sql| sql.contains("COUNT(*) AS __EMBRASURE_N"))
        );
        assert!(!statements.iter().any(|sql| sql.contains(" MINUS ")));
    }

    #[tokio::test]
    async fn keyed_diff_reports_changed_rows_and_columns_end_to_end() {
        let metadata = || QueryResult {
            columns: columns(),
            rows: vec![],
        };
        let executor = FakeExecutor::new(vec![
            metadata(),
            metadata(),
            QueryResult::default(),
            QueryResult::default(),
            rows(&[&[Some("2"), Some("2")]]),
            rows(&[&[Some("0"), Some("0"), Some("0")]]),
            rows(&[&[Some("0"), Some("0"), Some("0")]]),
            rows(&[&[Some("0"), Some("0"), Some("1"), Some("1")]]),
            rows(&[&[Some("2"), Some("2"), Some("new"), Some("2"), Some("old")]]),
        ]);
        let safety = SafetyConfig::default();
        let candidate = relation("CANDIDATE");
        let production = relation("PRODUCTION");
        let report = run_query_diff(
            &executor,
            QueryDiffInput {
                name: "keyed",
                account: "primary",
                current_refs: vec!["orders".into()],
                production_refs: vec!["orders".into()],
                candidate_sql: "SELECT ID, VALUE FROM CANDIDATE_SOURCE",
                production_sql: "SELECT ID, VALUE FROM PRODUCTION_SOURCE",
                candidate: &candidate,
                production: &production,
                primary_key: &["id".into()],
                safety: &safety,
            },
        )
        .await
        .unwrap();

        assert_eq!(report.status, QueryCheckStatus::Findings);
        let comparison = report.comparison.unwrap();
        assert_eq!(comparison.changed_rows, 1);
        assert_eq!(comparison.column_mismatches[0].column, "VALUE");
        assert_eq!(comparison.column_mismatches[0].rows, 1);
        assert_eq!(comparison.examples[0].key, vec![Some("2".into())]);
        assert_eq!(comparison.examples[0].candidate[1], Some("new".into()));
        assert_eq!(comparison.examples[0].production[1], Some("old".into()));
        assert!(
            executor
                .statements
                .lock()
                .unwrap()
                .iter()
                .any(|sql| sql.contains("COALESCE(COUNT_IF") && sql.contains("FULL OUTER JOIN"))
        );
    }
}
