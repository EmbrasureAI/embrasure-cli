use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    io::Write,
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    dbt::{Manifest, ManifestNode},
    report::{ColumnLineageEdge, ColumnLineageGap},
};

const SCRIPT: &str = include_str!("sqlglot_lineage.py");

#[derive(Serialize)]
struct Request<'a> {
    models: Vec<ModelRequest<'a>>,
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

#[derive(Deserialize)]
struct ParsedEdge {
    output_column: String,
    source_column: String,
    source_database: Option<SqlIdentifier>,
    source_schema: Option<SqlIdentifier>,
    source_table: SqlIdentifier,
    source_relation: String,
}

#[derive(Deserialize)]
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

    let response = invoke(&Request { models })?;
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
    Ok(invoke(&Request { models: vec![] })?.sqlglot_version)
}

fn invoke(request: &Request<'_>) -> Result<Response> {
    let mut child = Command::new(python_command())
        .args(["-c", SCRIPT])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("could not start Python for SQLGlot column lineage")?;
    serde_json::to_writer(
        child
            .stdin
            .as_mut()
            .context("SQLGlot stdin was not available")?,
        request,
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

fn python_command() -> String {
    env::var("EMBRASURE_PYTHON").unwrap_or_else(|_| "python3".into())
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
    fn sqlglot_traces_aliases_expressions_and_ctes() {
        let request = Request {
            models: vec![ModelRequest {
                unique_id: "model.project.summary",
                sql: "with orders as (select customer_id, amount from raw.orders) select customer_id, sum(amount) as total from orders group by 1",
            }],
        };
        let response = invoke(&request).unwrap();
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
    fn sqlglot_marks_wildcards_as_gaps_instead_of_guessing() {
        let request = Request {
            models: vec![ModelRequest {
                unique_id: "model.project.orders",
                sql: "select * from raw.orders",
            }],
        };
        let response = invoke(&request).unwrap();
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
        let response = invoke(&request).unwrap();
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
        let response = invoke(&request).unwrap();
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
        let response = invoke(&request).unwrap();
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
}
