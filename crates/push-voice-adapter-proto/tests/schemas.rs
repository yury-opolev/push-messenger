//! The checked-in JSON Schemas are exactly what the Rust types generate, and every
//! example message fits its schema's properties and required fields.
//!
//! After changing a message: `UPDATE_VOICE_ADAPTER_SCHEMAS=1 cargo test -p
//! push-voice-adapter-proto --test schemas`.

mod common;

use std::collections::BTreeSet;
use std::path::PathBuf;

use push_voice_adapter_proto::schema::{DIR, render, schemas};
use serde_json::Value;

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(DIR)
}

#[test]
fn the_checked_in_schemas_match_the_types() {
    let dir = dir();
    let update = std::env::var_os("UPDATE_VOICE_ADAPTER_SCHEMAS").is_some();
    let mut stale = Vec::new();
    for (file, schema) in schemas() {
        let path = dir.join(file);
        let expected = render(&schema);
        if update {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(&path, &expected).unwrap();
            continue;
        }
        let actual = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .replace("\r\n", "\n");
        if actual != expected {
            stale.push(file);
        }
    }
    assert!(
        stale.is_empty(),
        "schemas out of date: {stale:?}; run with UPDATE_VOICE_ADAPTER_SCHEMAS=1"
    );
    let generated: BTreeSet<String> = schemas().iter().map(|(f, _)| f.to_string()).collect();
    let on_disk: BTreeSet<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(on_disk, generated, "files in {DIR} that no type generates");
}

/// The schema of a message file: its `properties` and `required`.
fn shape(file: &str) -> (BTreeSet<String>, BTreeSet<String>) {
    let (_, schema) = schemas()
        .into_iter()
        .find(|(name, _)| *name == file)
        .unwrap();
    let properties = schema["properties"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    let required = schema["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    (properties, required)
}

fn file_for(direction: &str, value: &Value) -> String {
    match value["type"].as_str().unwrap() {
        "hello" => format!("{direction}-hello.json"),
        other => format!("{}.json", other.replace('_', "-")),
    }
}

#[test]
fn every_example_fits_its_schema() {
    let examples = common::bridge_messages()
        .into_iter()
        .map(|(_, json)| ("bridge", json))
        .chain(
            common::adapter_messages()
                .into_iter()
                .map(|(_, json)| ("adapter", json)),
        );
    for (direction, json) in examples {
        let file = file_for(direction, &json);
        let (properties, required) = shape(&file);
        let keys: BTreeSet<String> = json.as_object().unwrap().keys().cloned().collect();
        assert!(
            keys.is_subset(&properties),
            "{file}: {keys:?} vs {properties:?}"
        );
        assert!(
            required.is_subset(&keys),
            "{file}: {required:?} vs {keys:?}"
        );
        let (_, schema) = schemas().into_iter().find(|(f, _)| *f == file).unwrap();
        assert_eq!(
            schema["properties"]["type"]["const"], json["type"],
            "{file}"
        );
    }
}
