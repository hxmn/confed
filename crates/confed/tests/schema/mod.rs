//! A small, dependency-free JSON Schema checker for the documents in
//! `docs/reference/json/`.
//!
//! Pulling in a full JSON Schema implementation for this would add a dependency
//! tree larger than the thing being tested, so the subset the schemas actually
//! use is implemented here with `serde_json`: `$ref` to `#/$defs/…`, `type`,
//! `required`, `properties`, `items`, `enum`, `const`, `minimum`, `anyOf` and
//! `additionalProperties: false`.
//!
//! Unknown *instance* fields are allowed unless a schema says otherwise: adding
//! a field to a command's `result` is an additive, non-breaking change, and the
//! schemas document that policy rather than fighting it.

#![allow(dead_code)]

use serde_json::Value;
use std::path::PathBuf;

/// Where the published schemas live, relative to this crate.
pub fn schema_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/reference/json")
}

pub fn load(name: &str) -> Option<Value> {
    let path = schema_dir().join(format!("{name}.schema.json"));
    let text = std::fs::read_to_string(&path).ok()?;
    Some(
        serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("{} is not valid JSON: {e}", path.display())),
    )
}

/// Validate `instance` against the named schema, panicking with every problem.
pub fn check(name: &str, instance: &Value) {
    let schema = load(name)
        .unwrap_or_else(|| panic!("missing schema docs/reference/json/{name}.schema.json"));
    let errors = validate(&schema, &schema, instance, "$");
    assert!(
        errors.is_empty(),
        "{name}.schema.json rejected this document:\n  {}\n--- document ---\n{}",
        errors.join("\n  "),
        serde_json::to_string_pretty(instance).unwrap_or_default()
    );
}

/// Validate a command's `result` object, if that command publishes a schema.
pub fn check_result(command: &str, result: &Value) {
    if load(command).is_some() {
        check(command, result);
    }
}

/// The subset of JSON Schema the published documents use.
pub fn validate(root: &Value, schema: &Value, instance: &Value, path: &str) -> Vec<String> {
    let mut errors = Vec::new();

    // `$ref` replaces the schema entirely, as in draft 2020-12 for these documents.
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let Some(target) = resolve(root, reference) else {
            return vec![format!("{path}: unresolvable $ref `{reference}`")];
        };
        return validate(root, target, instance, path);
    }

    // A command whose `result` has more than one shape (`push --interactive`,
    // `log --local`) lists them under `anyOf`; one match is enough.
    for keyword in ["anyOf", "oneOf"] {
        if let Some(branches) = schema.get(keyword).and_then(Value::as_array) {
            let matched = branches
                .iter()
                .any(|branch| validate(root, branch, instance, path).is_empty());
            if !matched {
                errors.push(format!("{path}: matched none of the {keyword} branches"));
            }
        }
    }

    if let Some(expected) = schema.get("type") {
        if !type_matches(expected, instance) {
            errors.push(format!("{path}: expected type {expected}, found {}", type_name(instance)));
            return errors;
        }
    }

    if let Some(allowed) = schema.get("enum").and_then(Value::as_array) {
        if !allowed.contains(instance) {
            errors.push(format!("{path}: {instance} is not one of {}", Value::Array(allowed.clone())));
        }
    }
    if let Some(expected) = schema.get("const") {
        if instance != expected {
            errors.push(format!("{path}: expected the constant {expected}, found {instance}"));
        }
    }
    if let Some(actual) = instance.as_f64() {
        if let Some(minimum) = schema.get("minimum").and_then(Value::as_f64) {
            if actual < minimum {
                errors.push(format!("{path}: {actual} is below the minimum {minimum}"));
            }
        }
        if let Some(maximum) = schema.get("maximum").and_then(Value::as_f64) {
            if actual > maximum {
                errors.push(format!("{path}: {actual} is above the maximum {maximum}"));
            }
        }
    }

    if let Some(object) = instance.as_object() {
        for required in schema.get("required").and_then(Value::as_array).unwrap_or(&Vec::new()) {
            if let Some(key) = required.as_str() {
                if !object.contains_key(key) {
                    errors.push(format!("{path}: missing required property `{key}`"));
                }
            }
        }
        if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
            for (key, subschema) in properties {
                if let Some(child) = object.get(key) {
                    errors.extend(validate(root, subschema, child, &format!("{path}.{key}")));
                }
            }
            if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
                for key in object.keys() {
                    if !properties.contains_key(key) {
                        errors.push(format!("{path}: unexpected property `{key}`"));
                    }
                }
            }
        }
    }

    if let (Some(array), Some(items)) = (instance.as_array(), schema.get("items")) {
        for (index, item) in array.iter().enumerate() {
            errors.extend(validate(root, items, item, &format!("{path}[{index}]")));
        }
    }

    errors
}

fn resolve<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    let mut current = root;
    for segment in reference.trim_start_matches("#/").split('/') {
        if segment.is_empty() || reference == "#" {
            continue;
        }
        current = current.get(segment.replace("~1", "/").replace("~0", "~"))?;
    }
    Some(current)
}

fn type_matches(expected: &Value, instance: &Value) -> bool {
    match expected {
        Value::String(name) => is_type(name, instance),
        Value::Array(names) => names
            .iter()
            .filter_map(Value::as_str)
            .any(|name| is_type(name, instance)),
        _ => true,
    }
}

fn is_type(name: &str, instance: &Value) -> bool {
    match name {
        "object" => instance.is_object(),
        "array" => instance.is_array(),
        "string" => instance.is_string(),
        "boolean" => instance.is_boolean(),
        "null" => instance.is_null(),
        "number" => instance.is_number(),
        "integer" => instance.as_i64().is_some() || instance.as_u64().is_some(),
        _ => true,
    }
}

fn type_name(instance: &Value) -> &'static str {
    match instance {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

mod tests {
    use super::*;
    use serde_json::json;

    fn errors(schema: Value, instance: Value) -> Vec<String> {
        validate(&schema, &schema, &instance, "$")
    }

    #[test]
    fn required_properties_are_enforced() {
        let schema = json!({"type": "object", "required": ["a"]});
        assert!(errors(schema.clone(), json!({"a": 1})).is_empty());
        assert_eq!(errors(schema, json!({})).len(), 1);
    }

    #[test]
    fn types_enums_and_constants_are_checked() {
        assert_eq!(errors(json!({"type": "string"}), json!(3)).len(), 1);
        assert!(errors(json!({"type": ["string", "null"]}), json!(null)).is_empty());
        assert_eq!(errors(json!({"enum": ["a", "b"]}), json!("c")).len(), 1);
        assert_eq!(errors(json!({"const": 1}), json!(2)).len(), 1);
        assert_eq!(errors(json!({"type": "integer", "minimum": 0}), json!(-1)).len(), 1);
    }

    #[test]
    fn array_items_and_refs_are_followed() {
        let schema = json!({
            "type": "array",
            "items": { "$ref": "#/$defs/entry" },
            "$defs": { "entry": { "type": "object", "required": ["path"] } }
        });
        assert!(errors(schema.clone(), json!([{ "path": "a.md" }])).is_empty());
        assert_eq!(errors(schema, json!([{}])).len(), 1);
    }

    #[test]
    fn unknown_properties_are_additive_unless_forbidden() {
        let open = json!({"type": "object", "properties": {"a": {"type": "integer"}}});
        assert!(errors(open, json!({"a": 1, "b": 2})).is_empty());

        let closed = json!({
            "type": "object",
            "properties": {"a": {"type": "integer"}},
            "additionalProperties": false
        });
        assert_eq!(errors(closed, json!({"a": 1, "b": 2})).len(), 1);
    }

    /// Every published schema must itself be loadable and well formed.
    #[test]
    fn the_published_schemas_parse_and_declare_themselves() {
        let dir = schema_dir();
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".schema.json"))
            .collect();
        assert!(entries.len() >= 10, "expected a schema per documented command");

        for entry in entries {
            let text = std::fs::read_to_string(entry.path()).unwrap();
            let schema: Value = serde_json::from_str(&text)
                .unwrap_or_else(|e| panic!("{}: {e}", entry.path().display()));
            assert_eq!(
                schema["$schema"], json!("https://json-schema.org/draft/2020-12/schema"),
                "{} must declare its dialect", entry.path().display()
            );
            assert!(
                schema["title"].is_string(),
                "{} needs a title", entry.path().display()
            );
        }
    }
}
