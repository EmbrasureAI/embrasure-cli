use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::OsString,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    dbt::{Manifest, ManifestNode},
    provider::SqlDialect,
    report::{ColumnLineageEdge, ColumnLineageGap},
};

const SCRIPT: &str = include_str!("sqlglot_lineage.py");

#[derive(Serialize)]
struct Request<'a> {
    models: Vec<ModelRequest<'a>>,
}

#[derive(Serialize)]
struct Invocation<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    dialect: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unquoted_case: Option<&'static str>,
    models: &'a [ModelRequest<'a>],
}

#[derive(Serialize)]
struct ModelRequest<'a> {
    unique_id: &'a str,
    sql: &'a str,
}

#[derive(Deserialize)]
struct Response {
    sqlglot_version: String,
    models: Vec<ModelResponse>,
}

#[derive(Deserialize)]
struct ModelResponse {
    unique_id: String,
    edges: Vec<ParsedEdge>,
    gaps: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ParsedEdge {
    output_column: String,
    source_column: String,
    source_database: Option<SqlIdentifier>,
    source_schema: Option<SqlIdentifier>,
    source_table: SqlIdentifier,
    source_relation: String,
}

#[derive(Debug, Deserialize)]
struct SqlIdentifier {
    name: String,
    quoted: bool,
}

pub struct Extraction {
    pub edges: Vec<ColumnLineageEdge>,
    pub gaps: Vec<ColumnLineageGap>,
}

pub fn extract(
    account: &str,
    dialect: SqlDialect,
    current: &Manifest,
    production: &Manifest,
    compiled: &Manifest,
    selected: &BTreeSet<String>,
) -> Result<Extraction> {
    let models = selected
        .iter()
        .filter_map(|id| {
            compiled.nodes.get(id).and_then(|node| {
                node.compiled_code
                    .as_deref()
                    .map(|sql| ModelRequest { unique_id: id, sql })
            })
        })
        .collect::<Vec<_>>();
    let missing_sql = selected
        .iter()
        .filter(|id| {
            compiled
                .nodes
                .get(*id)
                .and_then(|node| node.compiled_code.as_deref())
                .is_none()
        })
        .cloned()
        .collect::<Vec<_>>();

    let mut gaps = missing_sql
        .into_iter()
        .map(|model| ColumnLineageGap {
            account: account.to_owned(),
            model,
            reason: "dbt did not emit compiled SQL for this model".into(),
        })
        .collect::<Vec<_>>();
    if models.is_empty() {
        return Ok(Extraction {
            edges: vec![],
            gaps,
        });
    }

    let response = invoke(&Request { models }, Some(dialect))?;
    let mut edges = Vec::new();
    for model in response.models {
        let Some(target) = current.nodes.get(&model.unique_id) else {
            gaps.push(ColumnLineageGap {
                account: account.to_owned(),
                model: model.unique_id,
                reason: "compiled model is absent from the current dbt manifest".into(),
            });
            continue;
        };
        gaps.extend(model.gaps.into_iter().map(|reason| ColumnLineageGap {
            account: account.to_owned(),
            model: model.unique_id.clone(),
            reason,
        }));
        for edge in model.edges {
            match resolve_source(target, &edge, current, production) {
                SourceResolution::Dbt(source) => edges.push(ColumnLineageEdge {
                    account: account.to_owned(),
                    from: source.unique_id.clone(),
                    from_name: source.name.clone(),
                    from_column: edge.source_column,
                    to: target.unique_id.clone(),
                    to_name: target.name.clone(),
                    to_column: edge.output_column,
                }),
                SourceResolution::External => edges.push(ColumnLineageEdge {
                    account: account.to_owned(),
                    from: edge.source_relation.clone(),
                    from_name: edge.source_relation,
                    from_column: edge.source_column,
                    to: target.unique_id.clone(),
                    to_name: target.name.clone(),
                    to_column: edge.output_column,
                }),
                SourceResolution::Ambiguous => gaps.push(ColumnLineageGap {
                    account: account.to_owned(),
                    model: model.unique_id.clone(),
                    reason: format!(
                        "{} matches more than one dbt model, so its column edge was omitted",
                        edge.source_relation
                    ),
                }),
            }
        }
    }
    Ok(Extraction { edges, gaps })
}

pub fn probe() -> Result<String> {
    Ok(invoke(&Request { models: vec![] }, None)?.sqlglot_version)
}

fn invoke(request: &Request<'_>, dialect: Option<SqlDialect>) -> Result<Response> {
    let bundled = bundled_sqlglot_path()?;
    let commands = python_commands();
    let mut last_not_found = None;
    for command in commands {
        match invoke_with(request, dialect, bundled.as_deref(), &command) {
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                last_not_found = Some(error);
            }
            result => return result,
        }
    }
    Err(last_not_found.unwrap_or_else(|| anyhow::anyhow!("no Python command is configured")))
        .context("could not start Python for SQLGlot column lineage")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PythonCommand {
    program: OsString,
    args: Vec<OsString>,
}

fn invoke_with(
    request: &Request<'_>,
    dialect: Option<SqlDialect>,
    bundled: Option<&Path>,
    python: &PythonCommand,
) -> Result<Response> {
    let mut command = Command::new(&python.program);
    command.args(&python.args);
    if bundled.is_some() {
        command.arg("-I");
    }
    command.args(["-c", SCRIPT]);
    if let Some(path) = bundled {
        command.arg(path);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    serde_json::to_writer(
        child
            .stdin
            .as_mut()
            .context("SQLGlot stdin was not available")?,
        &Invocation {
            dialect: dialect.map(SqlDialect::sqlglot_name),
            unquoted_case: dialect.map(SqlDialect::sqlglot_unquoted_case),
            models: &request.models,
        },
    )?;
    child
        .stdin
        .take()
        .context("SQLGlot stdin was not available")?
        .flush()?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "SQLGlot column lineage failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    serde_json::from_slice(&output.stdout).context("SQLGlot returned invalid lineage JSON")
}

fn python_commands() -> Vec<PythonCommand> {
    python_commands_for(env::var_os("EMBRASURE_PYTHON"), cfg!(windows))
}

fn python_commands_for(configured: Option<OsString>, windows: bool) -> Vec<PythonCommand> {
    if let Some(program) = configured.filter(|value| !value.is_empty()) {
        return vec![PythonCommand {
            program,
            args: vec![],
        }];
    }
    if windows {
        vec![
            PythonCommand {
                program: OsString::from("python.exe"),
                args: vec![],
            },
            PythonCommand {
                program: OsString::from("py.exe"),
                args: vec![OsString::from("-3")],
            },
        ]
    } else {
        vec![PythonCommand {
            program: OsString::from("python3"),
            args: vec![],
        }]
    }
}

fn bundled_sqlglot_path() -> Result<Option<PathBuf>> {
    if let Some(path) = env::var_os("EMBRASURE_SQLGLOT_PATH") {
        let path = PathBuf::from(path);
        if let Some(path) = find_sqlglot(&path)? {
            return Ok(Some(path));
        }
        bail!(
            "EMBRASURE_SQLGLOT_PATH does not contain a SQLGlot package: {}",
            path.display()
        );
    }

    let Ok(executable) = env::current_exe() else {
        return Ok(None);
    };
    let executable = executable.canonicalize().unwrap_or(executable);
    let Some(bin_dir) = executable.parent() else {
        return Ok(None);
    };
    find_bundled_sqlglot(bin_dir)
}

fn find_bundled_sqlglot(bin_dir: &Path) -> Result<Option<PathBuf>> {
    let mut candidates = vec![bin_dir.join(".embrasure/python")];
    if let Some(prefix) = bin_dir.parent() {
        candidates.push(prefix.join("libexec/embrasure/python"));
    }
    candidates.push(bin_dir.join("python"));
    for candidate in candidates {
        if let Some(path) = find_sqlglot(&candidate)? {
            return Ok(Some(path));
        }
        if candidate.is_dir() {
            bail!(
                "bundled SQLGlot package is missing from {}",
                candidate.display()
            );
        }
    }
    Ok(None)
}

fn find_sqlglot(path: &Path) -> Result<Option<PathBuf>> {
    if path.is_file() {
        return Ok(path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| name.starts_with("sqlglot-") && name.ends_with(".whl"))
            .map(|_| path.to_owned()));
    }
    if !path.is_dir() {
        return Ok(None);
    }
    if path.join("sqlglot").is_dir() {
        return Ok(Some(path.to_owned()));
    }

    let mut wheels = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let candidate = entry?.path();
        if candidate
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("sqlglot-") && name.ends_with(".whl"))
        {
            wheels.push(candidate);
        }
    }
    wheels.sort();
    match wheels.len() {
        0 => Ok(None),
        1 => Ok(wheels.pop()),
        _ => bail!(
            "more than one bundled SQLGlot wheel was found in {}",
            path.display()
        ),
    }
}

enum SourceResolution<'a> {
    Dbt(&'a ManifestNode),
    External,
    Ambiguous,
}

fn resolve_source<'a>(
    target: &ManifestNode,
    source: &ParsedEdge,
    current: &'a Manifest,
    production: &'a Manifest,
) -> SourceResolution<'a> {
    let preferred = target
        .depends_on
        .nodes
        .iter()
        .filter_map(|id| current.nodes.get(id).or_else(|| production.nodes.get(id)))
        .filter(|node| relation_matches(node, source))
        .collect::<Vec<_>>();
    if let Some(resolution) = unique_model(preferred) {
        return resolution;
    }

    let candidates = current
        .nodes
        .values()
        .chain(production.nodes.values())
        .filter(|node| node.resource_type == "model" && relation_matches(node, source))
        .collect::<Vec<_>>();
    unique_model(candidates).unwrap_or(SourceResolution::External)
}

fn unique_model(candidates: Vec<&ManifestNode>) -> Option<SourceResolution<'_>> {
    let unique = candidates
        .into_iter()
        .map(|node| (node.unique_id.as_str(), node))
        .collect::<BTreeMap<_, _>>();
    match unique.len() {
        0 => None,
        1 => Some(SourceResolution::Dbt(unique.values().next().unwrap())),
        _ => Some(SourceResolution::Ambiguous),
    }
}

fn relation_matches(node: &ManifestNode, source: &ParsedEdge) -> bool {
    identifier_matches(
        &node.alias,
        node.config.quoting.identifier,
        &source.source_table,
    ) && source
        .source_schema
        .as_ref()
        .is_none_or(|schema| identifier_matches(&node.schema, node.config.quoting.schema, schema))
        && source.source_database.as_ref().is_none_or(|database| {
            node.database.as_deref().is_some_and(|value| {
                identifier_matches(value, node.config.quoting.database, database)
            })
        })
}

fn identifier_matches(value: &str, quoted: Option<bool>, parsed: &SqlIdentifier) -> bool {
    if quoted.unwrap_or(false) || parsed.quoted {
        value == parsed.name
    } else {
        value.eq_ignore_ascii_case(&parsed.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbt::{DependsOn, NodeConfig};

    fn model(id: &str, schema: &str, compiled_code: Option<&str>) -> ManifestNode {
        ManifestNode {
            unique_id: id.into(),
            name: id.rsplit('.').next().unwrap().into(),
            resource_type: "model".into(),
            database: Some("DB".into()),
            schema: schema.into(),
            alias: id.rsplit('.').next().unwrap().into(),
            fqn: id.split('.').map(ToOwned::to_owned).collect(),
            tags: vec![],
            depends_on: DependsOn::default(),
            config: NodeConfig::default(),
            compiled_code: compiled_code.map(ToOwned::to_owned),
        }
    }

    fn manifest(nodes: Vec<ManifestNode>) -> Manifest {
        Manifest {
            nodes: nodes
                .into_iter()
                .map(|node| (node.unique_id.clone(), node))
                .collect(),
            exposures: BTreeMap::new(),
            child_map: BTreeMap::new(),
        }
    }

    #[test]
    fn python_resolution_is_platform_aware_and_overrideable() {
        assert_eq!(
            python_commands_for(None, false),
            vec![PythonCommand {
                program: OsString::from("python3"),
                args: vec![],
            }]
        );
        assert_eq!(
            python_commands_for(None, true),
            vec![
                PythonCommand {
                    program: OsString::from("python.exe"),
                    args: vec![],
                },
                PythonCommand {
                    program: OsString::from("py.exe"),
                    args: vec![OsString::from("-3")],
                },
            ]
        );
        assert_eq!(
            python_commands_for(Some(OsString::from("C:\\Tools\\python.exe")), true),
            vec![PythonCommand {
                program: OsString::from("C:\\Tools\\python.exe"),
                args: vec![],
            }]
        );
    }

    #[test]
    fn bundled_sqlglot_accepts_a_wheel_or_package_directory() {
        let directory = tempfile::tempdir().unwrap();
        let wheel = directory.path().join("sqlglot-30.7.0-py3-none-any.whl");
        std::fs::write(&wheel, []).unwrap();
        assert_eq!(find_sqlglot(directory.path()).unwrap(), Some(wheel.clone()));
        assert_eq!(find_sqlglot(&wheel).unwrap(), Some(wheel));

        std::fs::remove_file(directory.path().join("sqlglot-30.7.0-py3-none-any.whl")).unwrap();
        std::fs::create_dir(directory.path().join("sqlglot")).unwrap();
        assert_eq!(
            find_sqlglot(directory.path()).unwrap(),
            Some(directory.path().to_owned())
        );
    }

    #[test]
    fn bundled_sqlglot_rejects_ambiguous_wheels() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("sqlglot-30.6.0-py3-none-any.whl"), []).unwrap();
        std::fs::write(directory.path().join("sqlglot-30.7.0-py3-none-any.whl"), []).unwrap();
        assert!(
            find_sqlglot(directory.path())
                .unwrap_err()
                .to_string()
                .contains("more than one")
        );
    }

    #[test]
    fn bundled_sqlglot_finds_release_installer_and_homebrew_layouts() {
        for relative in [
            "bin/python",
            "bin/.embrasure/python",
            "libexec/embrasure/python",
        ] {
            let prefix = tempfile::tempdir().unwrap();
            let bin = prefix.path().join("bin");
            let python = prefix.path().join(relative);
            std::fs::create_dir_all(&bin).unwrap();
            std::fs::create_dir_all(&python).unwrap();
            let wheel = python.join("sqlglot-30.7.0-py3-none-any.whl");
            std::fs::write(&wheel, []).unwrap();
            assert_eq!(find_bundled_sqlglot(&bin).unwrap(), Some(wheel));
        }
    }

    #[test]
    fn bundled_sqlglot_reports_an_incomplete_install() {
        let prefix = tempfile::tempdir().unwrap();
        let bin = prefix.path().join("bin");
        std::fs::create_dir_all(bin.join(".embrasure/python")).unwrap();
        assert!(
            find_bundled_sqlglot(&bin)
                .unwrap_err()
                .to_string()
                .contains("bundled SQLGlot package is missing")
        );
    }

    #[test]
    fn sqlglot_traces_aliases_expressions_and_ctes() {
        let request = Request {
            models: vec![ModelRequest {
                unique_id: "model.project.summary",
                sql: "with orders as (select customer_id, amount from raw.orders) select customer_id, sum(amount) as total from orders group by 1",
            }],
        };
        let response = invoke(&request, Some(SqlDialect::Snowflake)).unwrap();
        let model = &response.models[0];
        assert!(model.gaps.is_empty());
        assert!(model.edges.iter().any(|edge| {
            edge.output_column == "CUSTOMER_ID"
                && edge.source_relation == "RAW.ORDERS"
                && edge.source_column == "CUSTOMER_ID"
        }));
        assert!(model.edges.iter().any(|edge| {
            edge.output_column == "TOTAL"
                && edge.source_relation == "RAW.ORDERS"
                && edge.source_column == "AMOUNT"
        }));
    }

    #[test]
    fn sqlglot_uses_databricks_identifier_rules() {
        let request = Request {
            models: vec![ModelRequest {
                unique_id: "model.project.summary",
                sql: "select Customer_ID, cast(Amount as double) as Total from Analytics.Raw.Orders",
            }],
        };
        let response = invoke(&request, Some(SqlDialect::Databricks)).unwrap();
        let edges = &response.models[0].edges;
        assert!(response.models[0].gaps.is_empty());
        assert!(edges.iter().any(|edge| {
            edge.output_column == "customer_id"
                && edge.source_relation == "analytics.raw.orders"
                && edge.source_column == "customer_id"
        }));
    }

    #[test]
    fn sqlglot_uses_bigquery_identifier_rules() {
        let request = Request {
            models: vec![ModelRequest {
                unique_id: "model.project.summary",
                sql: "select Customer_ID, safe_cast(Amount as float64) as Total from `Analytics.Raw.Orders`",
            }],
        };
        let response = invoke(&request, Some(SqlDialect::BigQuery)).unwrap();
        let edges = &response.models[0].edges;
        assert!(response.models[0].gaps.is_empty());
        assert!(
            edges.iter().any(|edge| {
                edge.output_column == "Customer_ID"
                    && edge.source_relation == "`Analytics.Raw.Orders`"
                    && edge.source_column == "customer_id"
            }),
            "{edges:#?}"
        );
    }

    #[test]
    fn sqlglot_bigquery_unnest_sources_do_not_crash_lineage_extraction() {
        let request = Request {
            models: vec![ModelRequest {
                unique_id: "model.project.generated",
                sql: "select order_id, concat('category_', cast(mod(order_id, 7) as string)) as category from unnest(generate_array(1, 10)) as order_id",
            }],
        };
        let response = invoke(&request, Some(SqlDialect::BigQuery)).unwrap();
        assert_eq!(response.models.len(), 1);
        assert_eq!(response.models[0].unique_id, "model.project.generated");
    }

    #[test]
    fn sqlglot_traces_common_snowflake_patterns() {
        let request = Request {
            models: vec![
                ModelRequest {
                    unique_id: "model.project.case_join",
                    sql: "select coalesce(o.customer_id, c.id) as customer_id, case when o.status = 'paid' then o.amount else 0 end as paid_amount from raw.orders o left join raw.customers c on o.customer_id = c.id",
                },
                ModelRequest {
                    unique_id: "model.project.qualify",
                    sql: "select customer_id, row_number() over (partition by customer_id order by created_at desc) as order_rank from raw.orders qualify order_rank = 1",
                },
                ModelRequest {
                    unique_id: "model.project.variant",
                    sql: "select payload:customer.id::number as customer_id from raw.events",
                },
                ModelRequest {
                    unique_id: "model.project.flatten",
                    sql: "select f.value:name::string as item_name from raw.events e, lateral flatten(input => e.payload:items) f",
                },
            ],
        };
        let response = invoke(&request, Some(SqlDialect::Snowflake)).unwrap();
        assert!(response.models.iter().all(|model| model.gaps.is_empty()));

        let case_join = response
            .models
            .iter()
            .find(|model| model.unique_id.ends_with("case_join"))
            .unwrap();
        assert!(case_join.edges.iter().any(|edge| {
            edge.output_column == "PAID_AMOUNT"
                && edge.source_relation == "RAW.ORDERS"
                && edge.source_column == "STATUS"
        }));
        assert!(case_join.edges.iter().any(|edge| {
            edge.output_column == "CUSTOMER_ID"
                && edge.source_relation == "RAW.CUSTOMERS"
                && edge.source_column == "ID"
        }));

        let qualify = response
            .models
            .iter()
            .find(|model| model.unique_id.ends_with("qualify"))
            .unwrap();
        assert!(qualify.edges.iter().any(|edge| {
            edge.output_column == "ORDER_RANK" && edge.source_column == "CREATED_AT"
        }));

        let variant = response
            .models
            .iter()
            .find(|model| model.unique_id.ends_with("variant"))
            .unwrap();
        assert!(variant.edges.iter().any(|edge| {
            edge.output_column == "CUSTOMER_ID" && edge.source_column == "PAYLOAD"
        }));

        let flatten = response
            .models
            .iter()
            .find(|model| model.unique_id.ends_with("flatten"))
            .unwrap();
        assert!(
            flatten.edges.iter().any(|edge| {
                edge.output_column == "ITEM_NAME" && edge.source_column == "PAYLOAD"
            })
        );
    }

    #[test]
    fn sqlglot_parse_gaps_are_stable_and_terminal_safe() {
        let request = Request {
            models: vec![ModelRequest {
                unique_id: "model.project.invalid",
                sql: "select from definitely not valid",
            }],
        };
        let response = invoke(&request, Some(SqlDialect::Snowflake)).unwrap();
        let gap = &response.models[0].gaps[0];
        assert_eq!(
            gap,
            "SQLGlot could not parse the compiled SQL: Invalid expression / Unexpected token at line 1, column 26"
        );
        assert!(!gap.contains('\u{1b}'));
        assert!(!gap.contains("definitely not valid"));
    }

    #[test]
    fn sqlglot_marks_wildcards_as_gaps_instead_of_guessing() {
        let request = Request {
            models: vec![ModelRequest {
                unique_id: "model.project.orders",
                sql: "select * from raw.orders",
            }],
        };
        let response = invoke(&request, Some(SqlDialect::Snowflake)).unwrap();
        assert!(response.models[0].edges.is_empty());
        assert!(response.models[0].gaps[0].contains("cannot be expanded"));
    }

    #[test]
    fn constants_do_not_create_fake_lineage_gaps() {
        let request = Request {
            models: vec![ModelRequest {
                unique_id: "model.project.loaded",
                sql: "select current_timestamp() as loaded_at",
            }],
        };
        let response = invoke(&request, Some(SqlDialect::Snowflake)).unwrap();
        assert!(response.models[0].edges.is_empty());
        assert!(response.models[0].gaps.is_empty());
    }

    #[test]
    fn quoted_column_names_are_returned_without_sql_quotes() {
        let request = Request {
            models: vec![ModelRequest {
                unique_id: "model.project.orders",
                sql: "select source.\"CamelCase\" as \"OutputCase\" from raw.orders source",
            }],
        };
        let response = invoke(&request, Some(SqlDialect::Snowflake)).unwrap();
        assert_eq!(response.models[0].edges[0].source_column, "CamelCase");
        assert_eq!(response.models[0].edges[0].output_column, "OutputCase");
    }

    #[test]
    fn unions_keep_each_output_column_separate() {
        let request = Request {
            models: vec![ModelRequest {
                unique_id: "model.project.orders",
                sql: "select id, amount from raw.orders union all select id, amount from raw.archive",
            }],
        };
        let response = invoke(&request, Some(SqlDialect::Snowflake)).unwrap();
        let output_columns = response.models[0]
            .edges
            .iter()
            .map(|edge| edge.output_column.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(output_columns, BTreeSet::from(["AMOUNT", "ID"]));
        assert_eq!(response.models[0].edges.len(), 4);
    }

    #[test]
    fn extraction_maps_compiled_relations_back_to_dbt_models() {
        let orders = model("model.project.orders", "CI", None);
        let mut summary = model("model.project.summary", "CI", None);
        summary.depends_on.nodes = vec![orders.unique_id.clone()];
        let current = manifest(vec![orders, summary]);
        let production = manifest(vec![
            model("model.project.orders", "PROD", None),
            model("model.project.summary", "PROD", None),
        ]);
        let compiled = manifest(vec![model(
            "model.project.summary",
            "CI",
            Some("select order_id, amount * 2 as gross from DB.CI.orders"),
        )]);

        let extraction = extract(
            "primary",
            SqlDialect::Snowflake,
            &current,
            &production,
            &compiled,
            &BTreeSet::from(["model.project.summary".into()]),
        )
        .unwrap();

        assert!(extraction.gaps.is_empty());
        assert!(extraction.edges.iter().any(|edge| {
            edge.from == "model.project.orders"
                && edge.from_column == "ORDER_ID"
                && edge.to == "model.project.summary"
                && edge.to_column == "ORDER_ID"
        }));
        assert!(extraction.edges.iter().any(|edge| {
            edge.from == "model.project.orders"
                && edge.from_column == "AMOUNT"
                && edge.to_column == "GROSS"
        }));
    }

    #[test]
    fn extraction_keeps_multi_model_column_paths_composable() {
        let staging = model("model.project.staging", "CI", None);
        let mut summary = model("model.project.summary", "CI", None);
        summary.depends_on.nodes = vec![staging.unique_id.clone()];
        let current = manifest(vec![staging, summary]);
        let production = manifest(vec![
            model("model.project.staging", "PROD", None),
            model("model.project.summary", "PROD", None),
        ]);
        let compiled = manifest(vec![
            model(
                "model.project.staging",
                "CI",
                Some("select order_id, amount from RAW.ORDERS"),
            ),
            model(
                "model.project.summary",
                "CI",
                Some("select order_id, amount * 2 as gross from DB.CI.staging"),
            ),
        ]);

        let extraction = extract(
            "primary",
            SqlDialect::Snowflake,
            &current,
            &production,
            &compiled,
            &BTreeSet::from([
                "model.project.staging".into(),
                "model.project.summary".into(),
            ]),
        )
        .unwrap();

        assert!(extraction.gaps.is_empty());
        assert!(extraction.edges.iter().any(|edge| {
            edge.from == "RAW.ORDERS"
                && edge.from_column == "AMOUNT"
                && edge.to == "model.project.staging"
                && edge.to_column == "AMOUNT"
        }));
        assert!(extraction.edges.iter().any(|edge| {
            edge.from == "model.project.staging"
                && edge.from_column == "AMOUNT"
                && edge.to == "model.project.summary"
                && edge.to_column == "GROSS"
        }));
    }

    #[test]
    fn extraction_reports_missing_sql_and_ambiguous_relations() {
        let mut left = model("model.project.left", "CI", None);
        left.alias = "shared".into();
        let mut right = model("model.project.right", "CI", None);
        right.alias = "shared".into();
        let target = model("model.project.target", "CI", None);
        let missing = model("model.project.missing", "CI", None);
        let current = manifest(vec![left, right, target, missing]);
        let production = manifest(vec![]);
        let compiled = manifest(vec![model(
            "model.project.target",
            "CI",
            Some("select id from DB.CI.shared"),
        )]);

        let extraction = extract(
            "primary",
            SqlDialect::Snowflake,
            &current,
            &production,
            &compiled,
            &BTreeSet::from([
                "model.project.missing".into(),
                "model.project.target".into(),
            ]),
        )
        .unwrap();

        assert!(extraction.edges.is_empty());
        assert!(extraction.gaps.iter().any(|gap| {
            gap.model == "model.project.missing"
                && gap.reason == "dbt did not emit compiled SQL for this model"
        }));
        assert!(extraction.gaps.iter().any(|gap| {
            gap.model == "model.project.target"
                && gap.reason.contains("matches more than one dbt model")
        }));
    }

    #[test]
    fn extraction_respects_quoted_relation_names() {
        let mut source = model("model.project.source", "CaseSchema", None);
        source.alias = "CaseTable".into();
        source.config.quoting.schema = Some(true);
        source.config.quoting.identifier = Some(true);
        let mut target = model("model.project.target", "CI", None);
        target.depends_on.nodes = vec![source.unique_id.clone()];
        let current = manifest(vec![source, target]);
        let compiled = manifest(vec![model(
            "model.project.target",
            "CI",
            Some("select \"CaseColumn\" from DB.\"CaseSchema\".\"CaseTable\""),
        )]);

        let extraction = extract(
            "primary",
            SqlDialect::Snowflake,
            &current,
            &manifest(vec![]),
            &compiled,
            &BTreeSet::from(["model.project.target".into()]),
        )
        .unwrap();

        assert!(extraction.gaps.is_empty());
        assert_eq!(extraction.edges.len(), 1);
        assert_eq!(extraction.edges[0].from, "model.project.source");
        assert_eq!(extraction.edges[0].from_column, "CaseColumn");
    }
}
