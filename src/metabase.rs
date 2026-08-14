use std::{collections::BTreeSet, env};

use anyhow::{Context, Result, bail};
use reqwest::{
    Client,
    header::{HeaderMap, HeaderValue},
};
use serde_json::Value;

use crate::{
    config::MetabaseConfig,
    report::{CoverageGap, ImpactedAsset},
    snowflake::Relation,
};

pub async fn find_dashboard_impact(
    config: &MetabaseConfig,
    relations: &[(String, Relation)],
) -> Result<(Vec<ImpactedAsset>, Vec<CoverageGap>)> {
    let key = env::var(&config.api_key_env).with_context(|| {
        format!(
            "missing Metabase API key environment variable {}",
            config.api_key_env
        )
    })?;
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-api-key",
        HeaderValue::from_str(&key).context("invalid Metabase API key")?,
    );
    let client = Client::builder().default_headers(headers).build()?;
    let base = config.url.trim_end_matches('/');
    let cards = get_json(&client, &format!("{base}/api/card?f=all")).await?;
    let card_values = array_payload(&cards).context("Metabase cards response was not an array")?;
    let needles = relations
        .iter()
        .flat_map(|(_, relation)| {
            let full = format!(
                "{}.{}.{}",
                relation.database, relation.schema, relation.identifier
            )
            .to_ascii_uppercase();
            let schema_table =
                format!("{}.{}", relation.schema, relation.identifier).to_ascii_uppercase();
            [full, schema_table]
        })
        .collect::<Vec<_>>();
    let mut matched_cards = BTreeSet::new();
    let mut mbql_seen = false;
    for card in card_values {
        let Some(id) = card.get("id").and_then(Value::as_u64) else {
            continue;
        };
        if let Some(query) = native_query(card) {
            let upper = query.to_ascii_uppercase();
            if needles
                .iter()
                .any(|needle| contains_relation(&upper, needle))
            {
                matched_cards.insert(id);
            }
        } else {
            mbql_seen = true;
        }
    }
    if matched_cards.is_empty() {
        let gaps = if mbql_seen {
            vec![CoverageGap {
            scope: "metabase".into(), check: "dashboard_lineage".into(),
            reason: "Metabase has MBQL/query-builder cards that cannot be matched safely from SQL text".into(),
        }]
        } else {
            vec![]
        };
        return Ok((vec![], gaps));
    }

    let dashboards = get_json(&client, &format!("{base}/api/dashboard")).await?;
    let dashboard_values =
        array_payload(&dashboards).context("Metabase dashboards response was not an array")?;
    let mut impacted = vec![];
    for summary in dashboard_values {
        let Some(id) = summary.get("id").and_then(Value::as_u64) else {
            continue;
        };
        let detail = get_json(&client, &format!("{base}/api/dashboard/{id}")).await?;
        if dashboard_card_ids(&detail)
            .iter()
            .any(|card| matched_cards.contains(card))
        {
            impacted.push(ImpactedAsset {
                id: format!("metabase.dashboard.{id}"),
                name: summary
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("Untitled dashboard")
                    .to_owned(),
                url: Some(format!("{base}/dashboard/{id}")),
            });
        }
    }
    let gaps = if mbql_seen {
        vec![CoverageGap {
        scope: "metabase".into(), check: "dashboard_lineage".into(),
        reason: "native SQL dashboards were checked, but MBQL/query-builder card lineage remains unknown".into(),
    }]
    } else {
        vec![]
    };
    impacted.sort();
    Ok((impacted, gaps))
}

async fn get_json(client: &Client, url: &str) -> Result<Value> {
    let response = client
        .get(url)
        .send()
        .await
        .context("Metabase request failed")?;
    let status = response.status();
    if !status.is_success() {
        bail!("Metabase returned HTTP {status}");
    }
    response
        .json()
        .await
        .context("Metabase returned invalid JSON")
}

fn array_payload(value: &Value) -> Option<&Vec<Value>> {
    value
        .as_array()
        .or_else(|| value.get("data").and_then(Value::as_array))
}

fn native_query(card: &Value) -> Option<&str> {
    card.pointer("/dataset_query/native/query")
        .and_then(Value::as_str)
}

fn dashboard_card_ids(value: &Value) -> BTreeSet<u64> {
    let mut found = BTreeSet::new();
    for dashcard in value
        .get("dashcards")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(id) = dashcard
            .pointer("/card/id")
            .and_then(Value::as_u64)
            .or_else(|| dashcard.get("card_id").and_then(Value::as_u64))
        {
            found.insert(id);
        }
        for series in dashcard
            .get("series")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(id) = series.get("id").and_then(Value::as_u64) {
                found.insert(id);
            }
        }
    }
    found
}

fn contains_relation(sql: &str, relation: &str) -> bool {
    let stripped_sql = sql.replace('"', "");
    let stripped_relation = relation.replace('"', "");
    stripped_sql
        .split(|character: char| {
            !(character.is_ascii_alphanumeric() || matches!(character, '_' | '$' | '.'))
        })
        .map(|token| token.trim_matches('.'))
        .any(|token| token == stripped_relation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_dashboard_cards() {
        let value =
            json!({"dashcards": [{"card": {"id": 4}}, {"card_id": 5, "series": [{"id": 6}]}]});
        assert_eq!(dashboard_card_ids(&value), BTreeSet::from([4, 5, 6]));
    }

    #[test]
    fn relation_matching_requires_a_token_boundary() {
        assert!(contains_relation(
            "select * from PROD.ORDERS",
            "PROD.ORDERS"
        ));
        assert!(!contains_relation(
            "select * from PROD.ORDERS_ARCHIVE",
            "PROD.ORDERS"
        ));
    }
}
