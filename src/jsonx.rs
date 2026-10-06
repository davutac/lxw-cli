//! Small JSON utilities: `--set` paths, RFC 7386 merge patch, `--fields` projection.

use serde_json::{Map, Value};

/// Parses a `--set` value: valid JSON (numbers, booleans, null, quoted strings,
/// objects, arrays) is used as-is, anything else becomes a string.
pub fn parse_value(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

/// Splits `lineItems[0].unitPrice.netAmount` or `lineItems.0.name` into segments.
fn segments(path: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for part in path.split('.') {
        let mut rest = part;
        if let Some(i) = rest.find('[') {
            if i > 0 {
                out.push(rest[..i].to_string());
            }
            rest = &rest[i..];
            while let Some(stripped) = rest.strip_prefix('[') {
                let end = stripped.find(']').ok_or_else(|| format!("unclosed '[' in {path:?}"))?;
                out.push(stripped[..end].to_string());
                rest = &stripped[end + 1..];
            }
            if !rest.is_empty() {
                return Err(format!("unexpected {rest:?} in {path:?}"));
            }
        } else {
            out.push(rest.to_string());
        }
    }
    if out.iter().any(String::is_empty) {
        return Err(format!("empty segment in {path:?}"));
    }
    Ok(out)
}

/// Sets `value` at `path`, creating objects/arrays along the way. Numeric
/// segments index arrays; an index equal to the length appends.
pub fn set_path(root: &mut Value, path: &str, value: Value) -> Result<(), String> {
    set_path_impl(root, path, value, false)
}

/// `pad` fills gaps in arrays with nulls instead of failing (used for projection).
fn set_path_impl(root: &mut Value, path: &str, value: Value, pad: bool) -> Result<(), String> {
    let segs = segments(path)?;
    let mut cur = root;
    for (i, seg) in segs.iter().enumerate() {
        let last = i + 1 == segs.len();
        let index = seg.parse::<usize>().ok();
        if cur.is_null() {
            *cur = if index.is_some() {
                Value::Array(Vec::new())
            } else {
                Value::Object(Map::new())
            };
        }
        let next_is_index = segs.get(i + 1).is_some_and(|s| s.parse::<usize>().is_ok());
        let empty_child = || {
            if next_is_index {
                Value::Array(Vec::new())
            } else {
                Value::Object(Map::new())
            }
        };
        cur = match (cur, index) {
            (Value::Array(arr), Some(idx)) => {
                if pad {
                    while arr.len() < idx {
                        arr.push(Value::Null);
                    }
                }
                if idx > arr.len() {
                    return Err(format!("index {idx} out of bounds (len {}) in {path:?}", arr.len()));
                }
                if idx == arr.len() {
                    arr.push(if last { Value::Null } else { empty_child() });
                }
                &mut arr[idx]
            }
            (Value::Object(map), _) => map
                .entry(seg.clone())
                .or_insert_with(|| if last { Value::Null } else { empty_child() }),
            (other, _) => {
                return Err(format!("cannot set {seg:?} inside a {} in {path:?}", type_name(other)));
            }
        };
        if last {
            *cur = value;
            return Ok(());
        }
    }
    Ok(())
}

/// RFC 7386 JSON merge patch: objects merge recursively, `null` removes, everything else replaces.
pub fn merge_patch(target: &mut Value, patch: &Value) {
    let Value::Object(patch_map) = patch else {
        *target = patch.clone();
        return;
    };
    if !target.is_object() {
        *target = Value::Object(Map::new());
    }
    let target_map = target.as_object_mut().expect("just ensured object");
    for (key, value) in patch_map {
        if value.is_null() {
            target_map.remove(key);
        } else {
            merge_patch(target_map.entry(key.clone()).or_insert(Value::Null), value);
        }
    }
}

/// Keeps only the given dot-paths. Applies per item for arrays and for the
/// `content` array of paged responses (paging metadata is kept).
pub fn project(value: Value, fields: &[String]) -> Value {
    if fields.is_empty() {
        return value;
    }
    match value {
        Value::Array(items) => Value::Array(items.iter().map(|v| project_one(v, fields)).collect()),
        Value::Object(mut map) if map.get("content").is_some_and(Value::is_array) => {
            let content = map.get_mut("content").expect("checked above");
            *content = project(content.take(), fields);
            Value::Object(map)
        }
        other => project_one(&other, fields),
    }
}

fn project_one(value: &Value, fields: &[String]) -> Value {
    let mut out = Value::Object(Map::new());
    for field in fields {
        if let Some(v) = get_path(value, field) {
            // The path resolved in `value`, so it is valid and cannot conflict in `out`.
            let _ = set_path_impl(&mut out, field, v.clone(), true);
        }
    }
    out
}

pub fn get_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = value;
    for seg in segments(path).ok()? {
        cur = match cur {
            Value::Object(map) => map.get(&seg)?,
            Value::Array(arr) => arr.get(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_value_prefers_json() {
        assert_eq!(parse_value("19"), json!(19));
        assert_eq!(parse_value("true"), json!(true));
        assert_eq!(parse_value("\"19\""), json!("19"));
        assert_eq!(
            parse_value("2026-01-31T00:00:00.000+01:00"),
            json!("2026-01-31T00:00:00.000+01:00")
        );
        assert_eq!(parse_value("{\"a\":1}"), json!({"a": 1}));
    }

    #[test]
    fn set_path_creates_nested_structures() {
        let mut v = json!({});
        set_path(&mut v, "address.contactId", json!("abc")).unwrap();
        set_path(&mut v, "lineItems[0].name", json!("Item")).unwrap();
        set_path(&mut v, "lineItems.0.unitPrice.netAmount", json!(10.5)).unwrap();
        set_path(&mut v, "lineItems[1].type", json!("text")).unwrap();
        assert_eq!(
            v,
            json!({
                "address": {"contactId": "abc"},
                "lineItems": [{"name": "Item", "unitPrice": {"netAmount": 10.5}}, {"type": "text"}]
            })
        );
        assert!(set_path(&mut v, "lineItems[5].x", json!(1)).is_err());
        assert!(set_path(&mut v, "address.contactId.x", json!(1)).is_err());
    }

    #[test]
    fn set_path_on_root_array() {
        let mut v = Value::Null;
        set_path(&mut v, "[0].amount", json!(1)).unwrap();
        assert_eq!(v, json!([{"amount": 1}]));
    }

    #[test]
    fn merge_patch_follows_rfc7386() {
        let mut target = json!({"a": "b", "c": {"d": "e", "f": "g"}, "version": 3});
        merge_patch(&mut target, &json!({"a": "z", "c": {"f": null}, "n": [1]}));
        assert_eq!(target, json!({"a": "z", "c": {"d": "e"}, "version": 3, "n": [1]}));
    }

    #[test]
    fn project_handles_pages_and_arrays() {
        let page = json!({"content": [{"id": 1, "x": {"y": 2, "z": 3}}], "last": true});
        assert_eq!(
            project(page, &["id".into(), "x.y".into()]),
            json!({"content": [{"id": 1, "x": {"y": 2}}], "last": true})
        );
        let arr = json!([{"id": 1, "n": 2}]);
        assert_eq!(project(arr, &["n".into()]), json!([{"n": 2}]));
        let obj = json!({"items": [{"a": 1}, {"a": 2}]});
        assert_eq!(project(obj, &["items[1].a".into()]), json!({"items": [null, {"a": 2}]}));
    }
}
