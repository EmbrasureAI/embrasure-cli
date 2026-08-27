import json
import re
import sys

if len(sys.argv) > 1:
    sys.path.insert(0, sys.argv[1])

import sqlglot
from sqlglot import exp
from sqlglot.lineage import lineage


ANSI_ESCAPE = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")


def error_text(error):
    details = getattr(error, "errors", None)
    if details:
        detail = details[0]
        description = detail.get("description", type(error).__name__)
        line = detail.get("line")
        column = detail.get("col")
        if line is not None and column is not None:
            return f"{description} at line {line}, column {column}"
        return description

    cleaned = ANSI_ESCAPE.sub("", str(error))
    return " ".join(cleaned.split())[:500]


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


def normalize_unquoted(value, quoted, unquoted_case):
    if quoted or unquoted_case == "preserve":
        return value
    if unquoted_case == "lower":
        return value.lower()
    return value.upper()


def relation_sql(table, dialect):
    relation = table.copy()
    relation.set("alias", None)
    return relation.sql(dialect=dialect)


def trace_model(model, dialect, unquoted_case):
    edges = []
    gaps = []
    try:
        expression = sqlglot.parse_one(model["sql"], dialect=dialect)
    except Exception as error:
        return {
            "unique_id": model["unique_id"],
            "edges": [],
            "gaps": [f"SQLGlot could not parse the compiled SQL: {error_text(error)}"],
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
                dialect=dialect,
            )
        except Exception as error:
            gaps.append(
                f"{output_column} could not be resolved by SQLGlot: {error_text(error)}"
            )
            continue
        output_column = normalize_unquoted(
            output_column, output_is_quoted, unquoted_case
        )

        for leaf in terminal_nodes(output_node):
            table = leaf.expression
            if leaf is output_node and not output_node.downstream:
                continue
            if not isinstance(table, exp.Table):
                gaps.append(
                    f"{output_column} ends at an unsupported SQL source: "
                    f"{table.sql(dialect=dialect)}"
                )
                continue

            source = exp.to_column(leaf.name)
            source_name = source.name
            if not source_name:
                gaps.append(f"{output_column} has an unresolved source column")
                continue
            source_column = normalize_unquoted(
                source_name,
                bool(source.this and source.this.args.get("quoted")),
                unquoted_case,
            )
            if source_column == "*":
                gaps.append(
                    f"{output_column} depends on a wildcard that cannot be expanded "
                    "without an authoritative input schema"
                )
                continue
            edges.append(
                {
                    "output_column": output_column,
                    "source_column": source_column,
                    "source_database": identifier(table.args.get("catalog")),
                    "source_schema": identifier(table.args.get("db")),
                    "source_table": identifier(table.this),
                    "source_relation": relation_sql(table, dialect),
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
    dialect = request.get("dialect", "snowflake")
    unquoted_case = request.get("unquoted_case", "upper")
    response = {
        "sqlglot_version": sqlglot.__version__,
        "models": [
            trace_model(model, dialect, unquoted_case)
            for model in request["models"]
        ],
    }
    json.dump(response, sys.stdout, separators=(",", ":"))


if __name__ == "__main__":
    main()
