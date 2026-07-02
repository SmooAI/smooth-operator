#!/usr/bin/env python3
"""Codegen: emit ``src/smooth_operator/_generated.py`` from the language-neutral
JSON Schemas in ``../spec`` using datamodel-code-generator (pydantic v2).

Strategy
--------
Every schema file in the spec is self-contained: all ``$ref``\\ s point at internal
``#/$defs/...`` definitions (verified — no cross-file refs). The *interesting*
shapes live under ``$defs``:

  * ``envelope.schema.json`` — top-level ``oneOf`` of ``ActionEnvelope`` /
    ``EventEnvelope``; the real shapes (incl. ``ErrorObject``) are under ``$defs``.
  * ``actions/*.schema.json`` — top-level ``oneOf`` over ``$defs`` (``Request`` /
    ``Response`` + shared helpers like ``GeneralAgentResponse``,
    ``ConversationMessage``).
  * ``events/*.schema.json`` — a flat top-level object (the event frame itself).
  * ``domain/*.schema.json`` — a flat top-level object (the domain entity), some
    with their own ``$defs`` (``MessageContent`` / ``ContentItem``).

We merge everything into **one** synthetic JSON Schema whose ``$defs`` holds every
named model we want emitted, deduplicating shared defs by ``title`` (``ErrorObject``,
``ConversationMessage``, ``GeneralAgentResponse`` appear in more than one file).
datamodel-code-generator then emits one pydantic model per ``$def`` in a single
pass, so cross-references between them resolve cleanly.

Run via ``uv run python scripts/generate.py``.
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
SPEC_DIR = HERE.parent.parent / "spec"
OUT_FILE = HERE.parent / "src" / "smooth_operator" / "_generated.py"

# Order matters only for readability of the merged doc; codegen sorts its output.
SUBDIRS = ["", "actions", "events", "domain"]


def list_schemas() -> list[Path]:
    out: list[Path] = []
    for sub in SUBDIRS:
        d = SPEC_DIR / sub if sub else SPEC_DIR
        for p in sorted(d.glob("*.schema.json")):
            out.append(p)
    return out


def title_for(name: str, schema: dict, file_title: str) -> str:
    """Pick a stable model name for a ``$def`` (its ``title`` if present)."""
    return str(schema.get("title") or f"{file_title}{name}")


def make_rewriter(rename: dict[str, str]):
    """Rewrite intra-file ``#/$defs/X`` refs to point at the merged top-level
    ``#/$defs/<Title>`` namespace.

    Every named ``$def`` is re-keyed under its model *title* in the merged doc, so a
    ref like ``#/$defs/Request`` (inside ``send-message``) must become
    ``#/$defs/SendMessageRequest`` (the title of that def).
    """

    def walk(node: object) -> object:
        if isinstance(node, dict):
            new: dict = {}
            for k, v in node.items():
                if k == "$ref" and isinstance(v, str) and v.startswith("#/$defs/"):
                    local = v[len("#/$defs/") :]
                    target = rename.get(local, local)
                    new[k] = f"#/$defs/{target}"
                else:
                    new[k] = walk(v)
            return new
        if isinstance(node, list):
            return [walk(x) for x in node]
        return node

    return walk


def build_merged_schema() -> dict:
    merged_defs: dict[str, dict] = {}
    seen: set[str] = set()

    for path in list_schemas():
        raw = json.loads(path.read_text())
        file_title = str(raw.get("title") or path.stem)

        local_defs = raw.get("$defs") or {}
        has_one_of = isinstance(raw.get("oneOf"), list)

        if has_one_of and local_defs:
            # Map each local $def name -> its global title, for ref rewriting.
            rename = {name: title_for(name, sub, file_title) for name, sub in local_defs.items()}
            rewrite = make_rewriter(rename)
            for name, sub in local_defs.items():
                title = rename[name]
                if title in seen:
                    continue  # shared def already emitted from an earlier file
                seen.add(title)
                model = {k: v for k, v in sub.items() if k != "$defs"}
                model = rewrite(model)  # fix internal refs
                model["title"] = title
                merged_defs[title] = model
        else:
            # Flat top-level object (events, domain). It may carry its own $defs.
            rename = {name: title_for(name, sub, file_title) for name, sub in local_defs.items()}
            rewrite = make_rewriter(rename)

            # Promote the file's own $defs into the merged namespace.
            for name, sub in local_defs.items():
                title = rename[name]
                if title not in seen:
                    seen.add(title)
                    model = {k: v for k, v in sub.items() if k != "$defs"}
                    model = rewrite(model)
                    model["title"] = title
                    merged_defs[title] = model

            title = file_title
            if title in seen:
                continue
            seen.add(title)
            top = {k: v for k, v in raw.items() if k not in ("$defs", "$schema", "$id")}
            top = rewrite(top)
            top["title"] = title
            merged_defs[title] = top

    return {
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": "https://smooth-agent.dev/spec/_merged.schema.json",
        "title": "SmoothAgentProtocol",
        "type": "object",
        "$defs": merged_defs,
    }


HEADER = '''"""AUTO-GENERATED — do not edit by hand.

Generated from the JSON Schemas in ../spec by scripts/generate.py
Run ``uv run python scripts/generate.py`` to regenerate after a schema change.

These are faithful pydantic v2 reflections of the wire schemas: snake_case Python
attributes with camelCase aliases (``populate_by_name = True``), so attribute access
is idiomatic while ``model_dump(by_alias=True)`` round-trips the camelCase wire form.
"""
# ruff: noqa
# fmt: off
'''


def main() -> int:
    merged = build_merged_schema()

    tmp = OUT_FILE.parent / "_merged.schema.json"
    OUT_FILE.parent.mkdir(parents=True, exist_ok=True)
    tmp.write_text(json.dumps(merged, indent=2))

    raw_out = OUT_FILE.parent / "_generated_raw.py"
    cmd = [
        sys.executable,
        "-m",
        "datamodel_code_generator",
        "--input",
        str(tmp),
        "--input-file-type",
        "jsonschema",
        "--output",
        str(raw_out),
        "--output-model-type",
        "pydantic_v2.BaseModel",
        "--target-python-version",
        "3.11",
        "--use-annotated",
        "--use-field-description",
        "--snake-case-field",  # snake_case attrs
        "--use-default-kwarg",
        "--reuse-model",
        "--use-standard-collections",
        "--use-union-operator",
        "--collapse-root-models",
        "--disable-timestamp",
        "--allow-population-by-field-name",  # populate_by_name = True
    ]
    print("Running:", " ".join(cmd[1:]))
    subprocess.run(cmd, check=True)

    body = raw_out.read_text()
    # datamodel-code-generator writes its own header; replace it with ours.
    if body.startswith('"""'):
        # drop everything up to and including the first generated header block
        idx = body.find("from __future__")
        if idx == -1:
            idx = body.find("\n\n")
        body = body[idx:]
    OUT_FILE.write_text(HEADER + "\n" + body.lstrip())

    raw_out.unlink(missing_ok=True)
    tmp.unlink(missing_ok=True)
    print(f"Wrote {OUT_FILE}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
