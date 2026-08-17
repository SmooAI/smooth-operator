//! Rust validates `spec/conformance/fixtures.json` — the check every other server already had.
//!
//! th-57b43a: Go, TypeScript, Python and .NET all validate the shared fixtures against their
//! declared schemas. Rust did not, which made the reference implementation the one implementation
//! that could not catch a spec/code divergence. That is not hypothetical: th-68897a shipped a
//! server change against a stale `required` list and Rust noticed nothing, while .NET failed on
//! first contact purely because it validates.
//!
//! ponytail: this asserts REQUIRED KEYS only, not full JSON Schema. That is the exact class that
//! bit us, and it needs no new dependency — a real validator (`jsonschema`) would be the first
//! one in this workspace. Upgrade to it if we ever need type/format/nested-$ref validation.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;

fn spec_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../spec")
}

/// Resolve `"actions/foo.schema.json#/$defs/Response"` to that subschema.
fn resolve(schema_ref: &str) -> Value {
    let (file, pointer) = schema_ref.split_once('#').unwrap_or((schema_ref, ""));
    let raw = std::fs::read_to_string(spec_dir().join(file))
        .unwrap_or_else(|e| panic!("reading {file}: {e}"));
    let doc: Value = serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parsing {file}: {e}"));
    if pointer.is_empty() {
        return doc;
    }
    doc.pointer(pointer)
        .unwrap_or_else(|| panic!("{schema_ref}: pointer not found"))
        .clone()
}

#[test]
fn every_fixture_has_its_schemas_required_keys() {
    let raw = std::fs::read_to_string(spec_dir().join("conformance/fixtures.json"))
        .expect("reading fixtures.json");
    let fixtures: BTreeMap<String, Value> = serde_json::from_str(&raw).expect("parsing fixtures");

    let mut checked = 0usize;
    for (name, fixture) in &fixtures {
        if name.starts_with('$') {
            continue; // leading $comment, not a fixture
        }
        let schema_ref = fixture["$schema_ref"]
            .as_str()
            .unwrap_or_else(|| panic!("{name}: no $schema_ref"));
        let instance = &fixture["instance"];
        let schema = resolve(schema_ref);

        for key in schema["required"].as_array().unwrap_or(&Vec::new()) {
            let key = key.as_str().expect("required entries are strings");
            assert!(
                instance.get(key).is_some(),
                "fixture `{name}` is missing `{key}`, which {schema_ref} lists as required"
            );
        }
        checked += 1;
    }

    // Guard the guard: a glob that silently matched nothing would pass every assert above.
    assert!(
        checked >= 20,
        "only {checked} fixtures checked — did the file move?"
    );
}
