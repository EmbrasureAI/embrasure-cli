import json
import sys

import sqlglot
from sqlglot import exp
from sqlglot.lineage import lineage


def identifier(value):
    if value is None:
        return None
    return {"name": value.name, "quoted": bool(value.args.get("quoted"))}


def terminal_nodes(node):
    if not node.downstream:
        yield node
        return
    for child in node.downstream:
        yield from terminal_nodes(child)


def relation_sql(table):
    relation = table.copy()
    relation.set("alias", None)
    return relation.sql(dialect="snowflake")


def trace_model(model):
    edges = []
    gaps = []
    try:
        expression = sqlglot.parse_one(model["sql"], dialect="snowflake")
    except Exception as error:
        return {
            "unique_id": model["unique_id"],
            "edges": [],
            "gaps": [f"SQLGlot could not parse the compiled SQL: {error}"],
        }

    for select in expression.selects:
        output_column = select.alias_or_name
        if not output_column:
            gaps.append("an output expression has no stable column name")
            continue
        if output_column == "*":
            gaps.append("SELECT * cannot be expanded without an authoritative input schema")
            continue
        alias = select.args.get("alias")
        output_is_quoted = bool(alias and alias.args.get("quoted")) or bool(
            isinstance(select, exp.Column) and select.this.args.get("quoted")
        )
        try:
            output_node = lineage(
                exp.column(output_column, quoted=output_is_quoted),
                expression,
                dialect="snowflake",
            )
        except Exception as error:
            gaps.append(f"{output_column} could not be resolved by SQLGlot: {error}")
            continue
        if not output_is_quoted:
            output_column = output_column.upper()

        for leaf in terminal_nodes(output_node):
            table = leaf.expression
            if leaf is output_node and not output_node.downstream:
                continue
            if not isinstance(table, exp.Table):
                gaps.append(
                    f"{output_column} ends at an unsupported SQL source: "
                    f"{table.sql(dialect='snowflake')}"
                )
                continue

            source_column = exp.to_column(leaf.name).name
            if source_column == "*":
                gaps.append(
                    f"{output_column} depends on a wildcard that cannot be expanded "
                    "without an authoritative input schema"
                )
                continue
            if not source_column:
                gaps.append(f"{output_column} has an unresolved source column")
                continue

            edges.append(
                {
                    "output_column": output_column,
                    "source_column": source_column,
                    "source_database": identifier(table.args.get("catalog")),
                    "source_schema": identifier(table.args.get("db")),
                    "source_table": identifier(table.this),
                    "source_relation": relation_sql(table),
                }
            )

    edges.sort(
        key=lambda edge: (
            edge["output_column"],
            edge["source_relation"],
            edge["source_column"],
        )
    )
    return {
        "unique_id": model["unique_id"],
        "edges": edges,
        "gaps": sorted(set(gaps)),
    }


def main():
    major = int(sqlglot.__version__.split(".", 1)[0])
    if major != 30:
        raise RuntimeError(
            f"SQLGlot 30.x is required; found {sqlglot.__version__}"
        )
    request = json.load(sys.stdin)
    response = {
        "sqlglot_version": sqlglot.__version__,
        "models": [trace_model(model) for model in request["models"]],
    }
    json.dump(response, sys.stdout, separators=(",", ":"))


if __name__ == "__main__":
    main()
