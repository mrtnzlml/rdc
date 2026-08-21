"""Test helpers that exercise Rossum schema formulas and function hooks against
the REAL txscript runtime (`pip install txscript`), so tests verify exactly what
Rossum executes -- no stubs, no reimplemented logic.

This module is the ONLY place that touches txscript internals; a txscript
version bump should only require changes here. It supports txscript 1.1.0 and
1.2.0 from one code path.

When the formula sits in an rdc snapshot -- `<queue>/formulas/<field_id>.py`
next to `<queue>/schema.json` -- the queue's real schema is used, so field types
(number, date, enum), the line-item table structure, and the set of
fields that exist are the tenant's own. A formula with no schema beside it falls
back to a synthesized all-string schema, which is enough for testing a formula
written inline in a test.
"""
from __future__ import annotations

import datetime
import importlib.util
import json
import pathlib
import sys
from typing import Any

from txscript.formula import Formula
from txscript.txscript import TxScript

try:  # txscript >= 1.2.0 wraps a formula result; 1.1.0 has no such class
    from txscript.computed import EvalResult as _EvalResult
except ImportError:
    _EvalResult = None

# txscript >= 1.2.0 reads these annotation keys unconditionally (and
# `payload["document"]`); 1.1.0 reads none of them. Sending both shapes at once
# is what makes one payload work on both versions.
_TIMESTAMPS = (
    "created_at",
    "modified_at",
    "exported_at",
    "confirmed_at",
    "assigned_at",
    "export_failed_at",
    "deleted_at",
    "rejected_at",
    "purged_at",
)


def evaluate_formula(
    formula_path: str | pathlib.Path,
    *,
    annotation_id: int = 1,
    status: str = "exported",
    rows: dict[str, list] | None = None,
    **field_values: Any,
) -> Any:
    """Evaluate a schema-field formula file with given input field values.

    Reads the real formula source, builds a minimal annotation payload carrying
    `field_values`, and returns the value the formula produces under the real
    txscript runtime.

    With the queue's `schema.json` beside the formula, every field carries its
    real type -- so a `number` field arrives as a number and a `date` field must
    be given ISO `YYYY-MM-DD` (or a `datetime.date`), which is the only form the
    runtime parses. A field name that the schema does not define is an error
    rather than a silently invented empty field.

    Without a schema (a formula written to `tmp_path` in a test), every field is
    a string, only the fields the formula references exist, and referenced
    inputs left unsupplied default to "".

    Table columns: pass `rows={"line_items": [{...}, {...}]}` to evaluate a
    column formula once per row, which returns a list. Passing the column values
    as bare keyword arguments evaluates a single implicit row and returns that
    row's scalar.

    `annotation_id` is exposed to the formula as `annotation.id`, and
    `status` as `annotation.status`.
    """
    path = pathlib.Path(formula_path).resolve()
    schema_id = path.stem
    source = path.read_text()

    schema_content = _load_schema(path)
    values = dict(field_values)
    explicit_rows = rows is not None
    rows = dict(rows or {})

    if schema_content is None:
        formula = Formula(schema_id, source)
        # The output field plus every input field the formula references
        # (deduplicated: a formula may legitimately reference its own field).
        schema_content = _synthesized_schema(sorted(formula.dependencies | {schema_id}))
        index = _index(schema_content)
        multivalue_id = None
    else:
        index = _index(schema_content)
        if schema_id not in index:
            raise AssertionError(
                f"{path.parents[1] / 'schema.json'} defines no field '{schema_id}'; "
                f"the formula file has no field to compute"
            )
        multivalue_id = _multivalue_of(schema_id, index)
        # The parent multivalue id is how txscript rewrites a column formula's
        # `_index` dependency, so it must be passed for the checks below to see
        # the same dependency set the runtime does.
        formula = Formula(schema_id, source, multivalue_id)
        _check_fields(path, index, formula, values, rows)

    if multivalue_id is not None:
        values, rows = _route_columns(
            schema_id, multivalue_id, index, values, rows, explicit_rows
        )

    content = _content(schema_content, values, rows, [1000])
    t = TxScript.from_payload(_payload(schema_content, content, annotation_id, status))
    if t.annotation is not None and not hasattr(t.annotation, "id"):
        # txscript 1.1.0's Annotation wrapper has no `id`; the live Rossum
        # formula runtime does expose one, and 1.2.0 sets it itself.
        t.annotation.id = annotation_id

    if multivalue_id is None:
        # _readonly_context() stops formula.evaluate from writing field updates
        # back; it is a txscript-private API and the most likely breakage point
        # on a version bump.
        with t.field._readonly_context():
            return _unwrap(formula.evaluate(t))

    # A column formula evaluates once per row, inside that row's context --
    # the same three calls txscript's own eval_strings makes.
    results = []
    for row in t.field._get_field(multivalue_id).get_value():
        with row._row_formula_context(t) as row_t:
            with row._field_context(row._get_field(schema_id)):
                with t.field._readonly_context():
                    results.append(_unwrap(formula.evaluate(row_t)))
    if explicit_rows:
        return results
    return results[0] if results else None


def load_hook(hook_path: str | pathlib.Path):
    """Import a (possibly hyphenated) Rossum function-hook .py file as a module,
    with the real txscript package importable, and return the module so its
    helper functions / rossum_hook_request_handler can be called directly.
    """
    path = pathlib.Path(hook_path)
    spec = importlib.util.spec_from_file_location(path.stem.replace("-", "_"), path)
    if spec is None or spec.loader is None:
        raise FileNotFoundError(f"Cannot load hook module from {path}")
    module = importlib.util.module_from_spec(spec)
    # Registered BEFORE execution: dataclasses, pickle and get_type_hints all
    # look the defining module up in sys.modules while the module body runs, and
    # a hook using any of them fails to import if it is not there yet.
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


# --- schema ---------------------------------------------------------------


def _load_schema(formula_path: pathlib.Path) -> list | None:
    """The queue's schema content, if the formula sits in an rdc snapshot.

    rdc lays a queue out as `<queue>/schema.json` alongside
    `<queue>/formulas/<field_id>.py`, so the schema is one directory up.
    """
    if len(formula_path.parents) < 2:
        return None
    candidate = formula_path.parents[1] / "schema.json"
    if not candidate.is_file():
        return None
    return json.loads(candidate.read_text())["content"]


def _synthesized_schema(field_ids: list[str]) -> list:
    return [
        {
            "category": "section",
            "id": "section",
            "children": [
                {"category": "datapoint", "id": fid, "type": "string"} for fid in field_ids
            ],
        }
    ]


def _index(nodes: list, parent: dict | None = None, out: dict | None = None) -> dict:
    """schema_id -> (node, parent node) for every node in the schema tree."""
    out = {} if out is None else out
    for node in nodes:
        out[node["id"]] = (node, parent)
        children = node.get("children")
        if isinstance(children, dict):  # a multivalue carries a single child
            children = [children]
        if children:
            _index(children, node, out)
    return out


def _multivalue_of(schema_id: str, index: dict) -> str | None:
    """The multivalue a table column belongs to, or None for a plain field."""
    parent = index[schema_id][1]
    if parent is None or parent["category"] != "tuple":
        return None
    return index[parent["id"]][1]["id"]


def _check_fields(path, index, formula, values, rows) -> None:
    """Reject names the queue's schema does not define. A synthesized schema
    invents whatever it is handed, which lets a typo -- or a formula referencing
    a field since deleted -- pass a test and fail in the tenant.
    """
    schema_json = path.parents[1] / "schema.json"
    row_columns = {column for row in rows.values() for entry in row if isinstance(entry, dict) for column in entry}
    for label, names in (
        ("input", set(values)),
        ("table", set(rows)),
        ("row column", row_columns),
    ):
        unknown = sorted(names - set(index))
        if unknown:
            raise AssertionError(f"{schema_json} defines no {label} field(s): {unknown}")
    absent = sorted(formula.dependencies - set(index))
    if absent:
        raise AssertionError(
            f"{path.name} references field(s) absent from {schema_json}: {absent}"
        )


def _route_columns(schema_id, multivalue_id, index, values, rows, explicit_rows):
    """Column values passed as bare keyword arguments become one implicit row."""
    if explicit_rows and multivalue_id not in rows:
        # Naming some other table would otherwise leave this one with the
        # default single empty row and return a puzzling empty result.
        raise AssertionError(
            f"'{schema_id}' is a column of '{multivalue_id}', but rows were given for "
            f"{sorted(rows)}; pass rows={{'{multivalue_id}': [...]}}"
        )
    tuple_id = index[schema_id][1]["id"]
    columns = {
        name: value
        for name, value in values.items()
        if (index[name][1] or {}).get("id") == tuple_id
    }
    if not columns:
        rows.setdefault(multivalue_id, [{}])
        return values, rows
    if multivalue_id in rows:
        raise AssertionError(
            f"pass the column value(s) {sorted(columns)} either as keyword arguments "
            f"or inside rows[{multivalue_id!r}], not both"
        )
    remaining = {k: v for k, v in values.items() if k not in columns}
    rows[multivalue_id] = [columns]
    return remaining, rows


# --- payload --------------------------------------------------------------


def _serialize(value: Any) -> str:
    """Annotation content stores strings. A date is normalized to ISO, the only
    form the runtime parses for a `date` field.
    """
    if value is None:
        return ""
    if isinstance(value, datetime.datetime):
        return value.date().isoformat()
    if isinstance(value, datetime.date):
        return value.isoformat()
    return str(value)


def _datapoint(node, value, counter):
    counter[0] += 1
    return {
        "id": counter[0],
        "schema_id": node["id"],
        "category": "datapoint",
        "content": {"value": _serialize(value), "normalized_value": None},
    }


def _content(nodes, values, rows, counter) -> list:
    """Mirror the schema tree as annotation content, filling in the given values."""
    out = []
    for node in nodes:
        category = node["category"]
        if category == "datapoint":
            out.append(_datapoint(node, values.get(node["id"]), counter))
            continue
        counter[0] += 1
        base = {"id": counter[0], "schema_id": node["id"], "category": category}
        if category == "multivalue":
            child = node["children"]
            out.append(
                {
                    **base,
                    "children": [_row(child, row, counter) for row in rows.get(node["id"], [])],
                }
            )
        else:  # section, or a tuple outside a multivalue
            out.append({**base, "children": _content(node.get("children", []), values, rows, counter)})
    return out


def _row(child, row, counter):
    if child["category"] != "tuple":
        # a multivalue of bare datapoints: the row IS the value
        return _datapoint(child, row, counter)
    counter[0] += 1
    return {
        "id": counter[0],
        "schema_id": child["id"],
        "category": "tuple",
        "children": _content(child["children"], row, {}, counter),
    }


def _payload(schema_content, annotation_content, annotation_id, status) -> dict:
    return {
        "event": "annotation_content",
        "schemas": [{"content": schema_content}],
        # status + url make txscript build the Annotation object (it returns
        # None otherwise); content carries the field values.
        "annotation": {
            "id": annotation_id,
            "url": f"https://example.rossum.app/api/v1/annotations/{annotation_id}",
            "status": status,
            "content": annotation_content,
            "automated": False,
            "automatically_rejected": False,
            "einvoice": False,
            "metadata": {},
            **{key: None for key in _TIMESTAMPS},
        },
        "document": {
            "id": 1,
            "url": "https://example.rossum.app/api/v1/documents/1",
            "arrived_at": None,
            "created_at": None,
            "original_file_name": "example.pdf",
            "metadata": {},
            "mime_type": "application/pdf",
        },
    }


def _unwrap(result: Any) -> Any:
    """txscript 1.2.0 wraps a formula result in EvalResult; 1.1.0 returns it raw.

    Tested by type, never by `getattr(result, "value", result)`: a formula that
    returns a field directly (`default_to(field.a, field.b)`) hands back a
    FieldValueBase proxy whose `.value` is the datapoint's raw *string*, so the
    duck-typed form would silently strip a date or number back down to text.
    """
    if _EvalResult is not None and isinstance(result, _EvalResult):
        return result.value
    return result
