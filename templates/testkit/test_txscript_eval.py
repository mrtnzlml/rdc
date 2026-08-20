"""Self-tests for the testkit harness, independent of any repo formula/hook.

These also guarantee `pytest -q` collects something in a project that has not
written any tests of its own yet: with nothing collected pytest exits 5, and the
pipeline's test job goes red.
"""
import datetime
import json
import textwrap

import pytest

from testkit import evaluate_formula, load_hook


# --- no schema beside the formula: every field is a string ------------------


def test_evaluate_formula_runs_real_txscript(tmp_path):
    formula = tmp_path / "demo.py"
    formula.write_text('default_to(field.a, "").upper()')
    assert evaluate_formula(formula, a="hello") == "HELLO"
    assert evaluate_formula(formula) == ""  # unset input defaults to empty string


def test_evaluate_formula_uses_dependencies(tmp_path):
    formula = tmp_path / "pick.py"
    formula.write_text('default_to(field.manual, "").strip() or default_to(field.captured, "")')
    assert evaluate_formula(formula, manual="M", captured="C") == "M"
    assert evaluate_formula(formula, manual="", captured="C") == "C"


def test_evaluate_formula_exposes_annotation_id(tmp_path):
    # The live Rossum formula runtime exposes annotation.id; the harness must too.
    formula = tmp_path / "name.py"
    formula.write_text('f"{field.branch or \'X\'}_{annotation.id}.json"')
    assert evaluate_formula(formula, annotation_id=42, branch="MAIN") == "MAIN_42.json"
    assert evaluate_formula(formula, branch="MAIN") == "MAIN_1.json"  # default id


def test_evaluate_formula_exposes_annotation_status(tmp_path):
    formula = tmp_path / "state.py"
    formula.write_text("annotation.status")
    assert evaluate_formula(formula) == "exported"
    assert evaluate_formula(formula, status="to_review") == "to_review"


def test_load_hook_imports_with_real_txscript(tmp_path):
    hook = tmp_path / "my-hook.py"
    hook.write_text(textwrap.dedent('''
        from txscript import is_set
        def keep(v):
            return v if is_set(v) else "fallback"
    '''))
    m = load_hook(hook)
    assert m.keep("x") == "x"
    assert m.keep("") == "fallback"


# --- with the queue's real schema: real field types ------------------------


def queue(tmp_path, **formulas):
    """An rdc-shaped queue directory: schema.json plus formulas/<field_id>.py.

    The schema mirrors the shape rdc snapshots: typed datapoints in a section,
    and a line-item table whose columns include a formula column.
    """
    root = tmp_path / "queues" / "invoices"
    (root / "formulas").mkdir(parents=True)
    schema = {
        "content": [
            {
                "category": "section",
                "id": "totals_section",
                "children": [
                    {"category": "datapoint", "id": "amount", "type": "number"},
                    {"category": "datapoint", "id": "doubled", "type": "number"},
                    {"category": "datapoint", "id": "due_date", "type": "date"},
                    {"category": "datapoint", "id": "due_date_out", "type": "date"},
                    {"category": "datapoint", "id": "terms", "type": "enum", "enum_value_type": "string"},
                    {"category": "datapoint", "id": "terms_out", "type": "string"},
                    {"category": "datapoint", "id": "terms_numeric", "type": "enum", "enum_value_type": "number"},
                    {"category": "datapoint", "id": "row_total", "type": "number"},
                ],
            },
            {
                "category": "section",
                "id": "line_items_section",
                "children": [
                    {
                        "category": "multivalue",
                        "id": "line_items",
                        "children": {
                            "category": "tuple",
                            "id": "line_item",
                            "children": [
                                {"category": "datapoint", "id": "item_qty", "type": "number"},
                                {"category": "datapoint", "id": "item_price", "type": "number"},
                                {"category": "datapoint", "id": "item_total", "type": "number"},
                            ],
                        },
                    }
                ],
            },
            {
                "category": "section",
                "id": "charges_section",
                "children": [
                    {
                        "category": "multivalue",
                        "id": "charges",
                        "children": {
                            "category": "tuple",
                            "id": "charge",
                            "children": [
                                {"category": "datapoint", "id": "charge_amount", "type": "number"}
                            ],
                        },
                    }
                ],
            },
        ]
    }
    (root / "schema.json").write_text(json.dumps(schema))
    for field_id, source in formulas.items():
        (root / "formulas" / f"{field_id}.py").write_text(source)
    return root / "formulas"


def test_number_field_arrives_as_a_number(tmp_path):
    """An all-string schema makes this string repetition ('1010'), not arithmetic."""
    formulas = queue(tmp_path, doubled="field.amount * 2")
    assert evaluate_formula(formulas / "doubled.py", amount="10") == 20.0


def test_date_field_arrives_as_a_date(tmp_path):
    formulas = queue(tmp_path, due_date_out="field.due_date")
    assert evaluate_formula(formulas / "due_date_out.py", due_date="2026-09-30") == datetime.date(
        2026, 9, 30
    )
    # a date object is accepted and normalized to the ISO form the runtime parses
    assert evaluate_formula(
        formulas / "due_date_out.py", due_date=datetime.date(2026, 9, 30)
    ) == datetime.date(2026, 9, 30)


def test_a_date_the_runtime_cannot_parse_reads_empty(tmp_path):
    """The schema's display format (M/D/YYYY) is not what a date field stores;
    the runtime parses ISO only, and anything else reads as empty -- exactly as
    it does in the tenant."""
    formulas = queue(tmp_path, due_date_out='"empty" if is_empty(field.due_date) else "set"')
    assert evaluate_formula(formulas / "due_date_out.py", due_date="9/30/2026") == "empty"
    assert evaluate_formula(formulas / "due_date_out.py", due_date="2026-09-30") == "set"


def test_enum_field_behaves_as_its_value_type(tmp_path):
    """enum_value_type decides the Python type: the common `string` case reads as
    text, while `number` really arrives as a number -- which an all-string
    synthesized schema cannot reproduce."""
    formulas = queue(
        tmp_path,
        terms_out='default_to(field.terms, "").upper()',
        row_total="field.terms_numeric * 2",
    )
    assert evaluate_formula(formulas / "terms_out.py", terms="net30") == "NET30"
    assert evaluate_formula(formulas / "row_total.py", terms_numeric="30") == 60.0


def test_unknown_input_field_is_rejected(tmp_path):
    """A synthesized schema invents whatever it is handed, so a typo passes."""
    formulas = queue(tmp_path, doubled="field.amount * 2")
    with pytest.raises(AssertionError, match="no input field"):
        evaluate_formula(formulas / "doubled.py", amountt="10")


def test_formula_referencing_an_absent_field_is_rejected(tmp_path):
    formulas = queue(tmp_path, doubled="field.gone * 2")
    with pytest.raises(AssertionError, match="absent from"):
        evaluate_formula(formulas / "doubled.py")


def test_column_formula_evaluates_one_implicit_row(tmp_path):
    formulas = queue(tmp_path, item_total="field.item_qty * field.item_price")
    assert evaluate_formula(formulas / "item_total.py", item_qty="3", item_price="4") == 12.0


def test_column_formula_evaluates_every_explicit_row(tmp_path):
    formulas = queue(tmp_path, item_total="field.item_qty * field.item_price")
    assert evaluate_formula(
        formulas / "item_total.py",
        rows={"line_items": [{"item_qty": "3", "item_price": "4"}, {"item_qty": "2", "item_price": "5"}]},
    ) == [12.0, 10.0]


def test_whole_column_is_reachable_from_a_plain_field(tmp_path):
    """`.all_values` needs the real table structure; a synthesized schema has none."""
    formulas = queue(tmp_path, row_total="sum(field.item_total.all_values)")
    assert evaluate_formula(
        formulas / "row_total.py",
        rows={"line_items": [{"item_total": "10"}, {"item_total": "20"}]},
    ) == 30.0


def test_wrong_table_named_in_rows_is_rejected(tmp_path):
    """A schema can hold several tables. Naming the wrong one would leave this
    column's table with the default empty row and return a puzzling empty
    result instead of an error."""
    formulas = queue(tmp_path, item_total="field.item_qty * field.item_price")
    with pytest.raises(AssertionError, match="column of 'line_items'"):
        evaluate_formula(formulas / "item_total.py", rows={"charges": [{"charge_amount": "1"}]})


def test_table_name_absent_from_the_schema_is_rejected(tmp_path):
    formulas = queue(tmp_path, item_total="field.item_qty * field.item_price")
    with pytest.raises(AssertionError, match="no table field"):
        evaluate_formula(formulas / "item_total.py", rows={"not_a_table": [{}]})


def test_unknown_column_inside_a_row_is_rejected(tmp_path):
    formulas = queue(tmp_path, item_total="field.item_qty * field.item_price")
    with pytest.raises(AssertionError, match="no row column field"):
        evaluate_formula(formulas / "item_total.py", rows={"line_items": [{"item_qtyy": "3"}]})
