# Warehouse provider API

## Goal

Keep Snowflake, Databricks, and BigQuery behind a small internal warehouse API so another SQL warehouse can be added without branching the validation pipeline or copying comparison logic.

This is an internal Rust API, not a dynamic plugin ABI. Providers are compiled into the CLI and selected from configuration.

## Boundaries

The provider boundary owns the behavior that changes by warehouse:

- executing SQL and decoding result metadata;
- quoting and normalizing identifiers;
- rendering the few SQL fragments that are not portable;
- creating, cloning, and safely removing temporary schemas and relations;
- generating the provider-specific dbt profile output;
- resolving credentials and running provider-specific doctor and cleanup checks;
- choosing the SQLGlot dialect used for compiled SQL.

Git inspection, dbt artifact parsing, model selection, thresholds, findings, report contracts, concurrency, and progress remain provider-neutral.

## Core API

Common value types move out of `snowflake.rs`:

```rust
pub struct Relation {
    pub database: String,
    pub schema: String,
    pub identifier: String,
}

pub struct QueryResult {
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<Option<String>>>,
}

pub struct ResultColumn {
    pub name: String,
    pub data_type: String,
}
```

Read-only SQL consumers depend on a narrow executor:

```rust
pub trait QueryExecutor: Send + Sync {
    fn dialect(&self) -> SqlDialect;
    fn execute<'a>(&'a self, statement: &'a str) -> ProviderFuture<'a, QueryResult>;
}

pub type ProviderFuture<'a, T> =
    Pin<Box<dyn Future<Output = anyhow::Result<T>> + Send + 'a>>;
```

The validation runner additionally needs temporary-object lifecycle operations:

```rust
pub trait WarehouseProvider: QueryExecutor {
    fn create_schema<'a>(&'a self, database: &'a str, schema: &'a str)
        -> ProviderFuture<'a, ()>;
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
```

The runner stores `Arc<dyn WarehouseProvider>`, which keeps concurrent jobs cheap to clone without requiring a second abstraction or an async-trait dependency.

`SqlDialect` is a small copyable enum. It renders the warehouse differences already present in the shared SQL: identifier quoting and normalization, create-table-as, text/numeric casts, null-safe equality, conditionals and conditional counts, approximate distinct counts and percentiles, stable row hashes, and type classification. It also supplies the SQLGlot dialect name. Prefer portable SQL expressions when supported providers agree. Common query construction stays in `compare.rs` and `query.rs`; the enum should grow only when a real provider proves another fragment is different. Databricks and BigQuery complex or geospatial values retain schema evidence but skip column metrics that the warehouse cannot compare reliably; query diffs require an explicit scalar cast.

`Relation::sql(dialect)` renders a fully qualified name. A relation does not remember a global or implicit dialect.

## Provider selection and configuration

One factory converts an account's provider configuration and resolved credential into `Arc<dyn WarehouseProvider>`. The runner does not construct provider clients directly. The typed configuration enum and dialect provide the only dispatch needed.

Version 1 Snowflake configuration remains backward compatible. Version 2 uses a tagged provider enum rather than adding optional fields to `AccountConfig`:

```yaml
accounts:
  - name: primary
    selector: tag:primary
    provider:
      type: databricks
      host: https://example.cloud.databricks.com
      http_path: /sql/1.0/warehouses/...
      catalog: analytics
      production_schema: prod
      auth:
        type: token
        token_env: DATABRICKS_TOKEN
```

Each provider owns a typed configuration structure. There is no string-keyed property map and no common credential enum containing every provider's fields.

BigQuery uses the same version 2 shape with `type: bigquery`, `project`, `location`, `production_schema`, and `auth: { type: application_default }`. The provider resolves ADC for REST requests and writes a dbt-bigquery `method: oauth` profile so dbt uses the same credential chain.

Provider-specific entry points for auth, dbt profile generation, `doctor`, and `clean` are dispatched by the same tagged configuration. They do not belong on `WarehouseProvider` because the validation runner does not need them.

## Incremental models and capabilities

The runner asks the provider to copy a production table into a stable baseline for every existing incremental model. Snowflake implements the semantic operation with a zero-copy clone, Databricks uses a Unity Catalog shallow clone of a managed Delta table, and BigQuery uses a table clone. `incremental_mode: clone` additionally seeds the candidate from that baseline. Snowflake can clone again, Databricks uses CTAS because Unity Catalog does not permit a shallow clone of another shallow clone, and BigQuery uses a table copy to avoid extending a clone chain. `full_refresh` skips only candidate seeding, not the stable baseline. A provider may reject an unsupported relation with an actionable error. Provider-specific `doctor` checks are the authoritative preflight; there is no general capability registry.

Another provider may implement the stable copy with an equivalent snapshot operation or reject incremental models until it can preserve a stable baseline.

## Implementation sequence

1. Add the common provider types, dialect descriptor, executor trait, and lifecycle trait.
2. Implement them for `SnowflakeClient`, `DatabricksClient`, and `BigQueryClient`, selected by one factory.
3. Change the runner and comparison/query code to depend on trait objects and explicit dialect rendering, including identifier normalization and type classification.
4. Pass each account's provider dialect through SQLGlot parsing, lineage, rendering, and output-name normalization, and into provider-specific dbt profile generation.
5. Keep version 1 behavior and configuration unchanged, add the tagged version 2 shape, and use only focused provider-contract tests.

## Acceptance criteria

- The validation pipeline contains no direct `SnowflakeClient` dependency.
- Comparison and query-diff code does not import from `snowflake.rs`.
- Common relation/result types live in the provider module.
- SQLGlot receives the selected dialect per account instead of assuming Snowflake.
- Snowflake output, cleanup safety, report contracts, and CLI behavior remain unchanged.
- Snowflake, Databricks, and BigQuery share the same validation runner.
- No dynamic loading, generic property bags, speculative capability matrices, or duplicate Snowflake wrappers are introduced.
