//! Compile-time preparation of the embedded data, so the binary carries only
//! what it reads at runtime, deflate-compressed (~7x smaller):
//! - catalog/operations.toml -> compact JSON (no TOML parser in the binary);
//! - catalog/docs.json -> only the sections the catalog references, without
//!   the section text (that is used by tests only), minified.
//!
//! catalog/docs.json is an extract of Lexware's documentation, regenerated with
//! scripts/extract_docs.py. Without it, or with `LXW_NO_DOCS=1`, the field
//! reference and examples are left out and the `lxw_docs` cfg is not set.

use serde_json::{Map, Value};
use std::{env, fs, path::Path};

/// Docs fields the CLI reads at runtime.
const RUNTIME_FIELDS: &[&str] = &["objects", "required", "requestExample", "responseExample", "source"];

fn main() {
    println!("cargo:rerun-if-changed=catalog");
    println!("cargo:rerun-if-env-changed=LXW_NO_DOCS");
    println!("cargo::rustc-check-cfg=cfg(lxw_docs)");
    let out = env::var("OUT_DIR").expect("cargo sets OUT_DIR");

    let toml_text = fs::read_to_string("catalog/operations.toml").expect("read catalog/operations.toml");
    let catalog: Value = toml::from_str(&toml_text).expect("catalog/operations.toml is not valid TOML");

    let mut referenced: Vec<&str> = Vec::new();
    for key in ["resources", "operations"] {
        for entry in catalog[key].as_array().into_iter().flatten() {
            referenced.extend(entry["docs"].as_str());
            referenced.extend(
                entry["schema"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str),
            );
        }
    }

    let skip = env::var("LXW_NO_DOCS").is_ok_and(|v| v == "1");
    let docs = match fs::read_to_string("catalog/docs.json") {
        Ok(text) if !skip => {
            println!("cargo:rustc-cfg=lxw_docs");
            serde_json::from_str(&text).expect("catalog/docs.json is not valid JSON")
        }
        _ => {
            println!(
                "cargo:warning=building without Lexware's docs extract: `lxw schema` will link to the online docs instead \
                 of showing field references and examples. Regenerate it with scripts/extract_docs.py."
            );
            serde_json::json!({ "sections": {} })
        }
    };
    let all = docs["sections"].as_object().expect("docs.json has sections");
    let referenced: Vec<&str> = if all.is_empty() { Vec::new() } else { referenced };
    let mut sections = Map::new();
    for id in referenced {
        let section = all
            .get(id)
            .unwrap_or_else(|| panic!("catalog references missing docs section {id}"));
        let kept: Map<String, Value> = RUNTIME_FIELDS
            .iter()
            .filter_map(|f| section.get(*f).map(|v| (f.to_string(), v.clone())))
            .collect();
        sections.insert(id.to_string(), Value::Object(kept));
    }

    write(&out, "operations.json.deflate", &catalog);
    write(&out, "docs.json.deflate", &serde_json::json!({ "sections": sections }));
}

fn write(dir: &str, name: &str, value: &Value) {
    let text = serde_json::to_vec(value).expect("serializes");
    let packed = miniz_oxide::deflate::compress_to_vec(&text, 10);
    fs::write(Path::new(dir).join(name), packed).unwrap_or_else(|e| panic!("write {name}: {e}"));
}
