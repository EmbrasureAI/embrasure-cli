use std::{future::Future, pin::Pin, sync::Arc};

use anyhow::Result;

use crate::{
    auth::ResolvedAuth,
    config::{AccountConfig, ProviderConfig},
    databricks::DatabricksClient,
    snowflake::SnowflakeClient,
};

pub type ProviderFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

#[derive(Debug, Clone, Default)]
pub struct QueryResult {
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<Option<String>>>,
}

#[derive(Debug, Clone)]
pub struct ResultColumn {
    pub name: String,
    pub data_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relation {
    pub database: String,
    pub schema: String,
    pub identifier: String,
}

impl Relation {
    pub fn sql(&self, dialect: SqlDialect) -> String {
        format!(
            "{}.{}.{}",
            dialect.quote_identifier(&self.database),
            dialect.quote_identifier(&self.schema),
            dialect.quote_identifier(&self.identifier)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlDialect {
    Snowflake,
    Databricks,
}

impl SqlDialect {
    pub fn sqlglot_name(self) -> &'static str {
        match self {
            Self::Snowflake => "snowflake",
            Self::Databricks => "databricks",
        }
    }

    pub fn sqlglot_unquoted_case(self) -> &'static str {
        match self {
            Self::Snowflake => "upper",
            Self::Databricks => "lower",
        }
    }

    pub fn dbt_adapter_name(self) -> &'static str {
        match self {
            Self::Snowflake => "snowflake",
            Self::Databricks => "databricks",
        }
    }

    pub fn quote_identifier(self, value: &str) -> String {
        match self {
            Self::Snowflake => format!("\"{}\"", value.replace('"', "\"\"")),
            Self::Databricks => format!("`{}`", value.replace('`', "``")),
        }
    }

    pub fn normalize_identifier(self, value: &str, quoted: Option<bool>) -> String {
        match self {
            Self::Snowflake if !quoted.unwrap_or(false) => value.to_ascii_uppercase(),
            Self::Snowflake => value.to_owned(),
            Self::Databricks if !quoted.unwrap_or(false) => value.to_ascii_lowercase(),
            Self::Databricks => value.to_owned(),
        }
    }

    pub fn create_table_as(self, target: &Relation, query: &str) -> String {
        match self {
            Self::Snowflake => {
                format!("CREATE TRANSIENT TABLE {} AS {query}", target.sql(self))
            }
            Self::Databricks => format!("CREATE TABLE {} AS {query}", target.sql(self)),
        }
    }

    pub fn cast_text(self, expression: &str) -> String {
        match self {
            Self::Snowflake => format!("{expression}::VARCHAR"),
            Self::Databricks => format!("CAST({expression} AS STRING)"),
        }
    }

    pub fn to_text(self, expression: &str) -> String {
        match self {
            Self::Snowflake => format!("TO_VARCHAR({expression})"),
            Self::Databricks => format!("CAST({expression} AS STRING)"),
        }
    }

    pub fn cast_float(self, expression: &str) -> String {
        match self {
            Self::Snowflake => format!("{expression}::DOUBLE"),
            Self::Databricks => format!("CAST({expression} AS DOUBLE)"),
        }
    }

    pub fn null_safe_equal(self, left: &str, right: &str) -> String {
        match self {
            Self::Snowflake => format!("EQUAL_NULL({left}, {right})"),
            Self::Databricks => format!("{left} <=> {right}"),
        }
    }

    pub fn conditional(self, condition: &str, when_true: &str, when_false: &str) -> String {
        match self {
            Self::Snowflake => format!("IFF({condition}, {when_true}, {when_false})"),
            Self::Databricks => {
                format!("CASE WHEN {condition} THEN {when_true} ELSE {when_false} END")
            }
        }
    }

    pub fn count_if(self, condition: &str) -> String {
        format!("COUNT_IF({condition})")
    }

    pub fn approximate_distinct(self, expression: &str) -> String {
        format!("APPROX_COUNT_DISTINCT({expression})")
    }

    pub fn supports_column_metrics(self, data_type: &str) -> bool {
        self != Self::Databricks || !self.is_unsupported_value(data_type)
    }

    pub fn approximate_percentile(self, expression: &str, percentile: &str) -> String {
        match self {
            Self::Snowflake => format!("APPROX_PERCENTILE({expression}, {percentile})"),
            Self::Databricks => format!("PERCENTILE_APPROX({expression}, {percentile})"),
        }
    }

    pub fn stable_hash(self, expressions: &[String]) -> String {
        format!("HASH({})", expressions.join(", "))
    }

    pub fn is_numeric(self, data_type: &str) -> bool {
        match self {
            Self::Snowflake => {
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
            Self::Databricks => {
                let normalized = data_type.to_ascii_lowercase();
                normalized.starts_with("decimal(")
                    || matches!(
                        normalized.as_str(),
                        "byte"
                            | "tinyint"
                            | "short"
                            | "smallint"
                            | "int"
                            | "integer"
                            | "long"
                            | "bigint"
                            | "float"
                            | "double"
                            | "decimal"
                    )
            }
        }
    }

    pub fn is_orderable(self, data_type: &str) -> bool {
        match self {
            Self::Snowflake => !matches!(
                data_type
                    .to_ascii_uppercase()
                    .split('(')
                    .next()
                    .unwrap_or_default(),
                "ARRAY" | "OBJECT" | "VARIANT" | "BINARY" | "GEOGRAPHY" | "GEOMETRY"
            ),
            Self::Databricks => !matches!(
                data_type
                    .to_ascii_uppercase()
                    .split(['(', '<'])
                    .next()
                    .unwrap_or_default(),
                "ARRAY" | "MAP" | "STRUCT" | "VARIANT" | "BINARY" | "GEOGRAPHY" | "GEOMETRY"
            ),
        }
    }

    pub fn is_unsupported_value(self, data_type: &str) -> bool {
        match self {
            Self::Snowflake => matches!(
                data_type
                    .split(['(', ' '])
                    .next()
                    .unwrap_or(data_type)
                    .to_ascii_uppercase()
                    .as_str(),
                "ARRAY" | "OBJECT" | "VARIANT" | "GEOGRAPHY" | "GEOMETRY" | "MAP" | "VECTOR"
            ),
            Self::Databricks => matches!(
                data_type
                    .split(['(', '<', ' '])
                    .next()
                    .unwrap_or(data_type)
                    .to_ascii_uppercase()
                    .as_str(),
                "ARRAY" | "MAP" | "STRUCT" | "VARIANT" | "GEOGRAPHY" | "GEOMETRY"
            ),
        }
    }
}

pub trait QueryExecutor: Send + Sync {
    fn dialect(&self) -> SqlDialect;

    fn execute<'a>(&'a self, statement: &'a str) -> ProviderFuture<'a, QueryResult>;
}

pub trait WarehouseProvider: QueryExecutor {
    fn create_schema<'a>(&'a self, database: &'a str, schema: &'a str) -> ProviderFuture<'a, ()>;

    fn copy_table<'a>(
        &'a self,
        source: &'a Relation,
        target: &'a Relation,
    ) -> ProviderFuture<'a, ()>;

    fn seed_table<'a>(
        &'a self,
        source: &'a Relation,
        target: &'a Relation,
    ) -> ProviderFuture<'a, ()>;

    fn drop_schema<'a>(
        &'a self,
        database: &'a str,
        schema: &'a str,
        run_schema: &'a str,
    ) -> ProviderFuture<'a, ()>;
}

pub fn connect(
    account: &AccountConfig,
    auth: &ResolvedAuth,
    query_tag: String,
    timeout_seconds: u64,
) -> Result<Arc<dyn WarehouseProvider>> {
    match &account.provider {
        ProviderConfig::Snowflake(_) => Ok(Arc::new(SnowflakeClient::new(
            account,
            auth,
            query_tag,
            timeout_seconds,
        )?)),
        ProviderConfig::Databricks(_) => Ok(Arc::new(DatabricksClient::new(
            account,
            auth,
            query_tag,
            timeout_seconds,
        )?)),
    }
}

pub fn dialect(account: &AccountConfig) -> SqlDialect {
    match &account.provider {
        ProviderConfig::Snowflake(_) => SqlDialect::Snowflake,
        ProviderConfig::Databricks(_) => SqlDialect::Databricks,
    }
}

pub fn is_managed_schema(dialect: SqlDialect, schema: &str, run_schema: &str) -> bool {
    let schema = dialect.normalize_identifier(schema, None);
    let run_schema = dialect.normalize_identifier(run_schema, None);
    schema == run_schema || schema.starts_with(&format!("{run_schema}_"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialects_render_their_provider_contracts() {
        let dialect = SqlDialect::Snowflake;
        let relation = Relation {
            database: "ANALYTICS".into(),
            schema: "CI".into(),
            identifier: "odd\"name".into(),
        };
        assert_eq!(relation.sql(dialect), r#""ANALYTICS"."CI"."odd""name""#);
        assert_eq!(
            dialect.create_table_as(&relation, "SELECT 1"),
            r#"CREATE TRANSIENT TABLE "ANALYTICS"."CI"."odd""name" AS SELECT 1"#
        );
        assert_eq!(dialect.normalize_identifier("mixed", None), "MIXED");
        let databricks = SqlDialect::Databricks;
        assert_eq!(databricks.quote_identifier("odd`name"), "`odd``name`");
        assert_eq!(databricks.normalize_identifier("Mixed", None), "mixed");
        assert!(!databricks.supports_column_metrics("MAP<STRING, STRING>"));
        assert!(!databricks.supports_column_metrics("GEOGRAPHY(4326)"));
        assert!(databricks.is_unsupported_value("GEOMETRY(ANY)"));
        assert_eq!(
            databricks.null_safe_equal("left", "right"),
            "left <=> right"
        );
    }
}
