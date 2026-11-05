//! Hot-reloadable JSON merge patches, for finding what the host checks without rebuilding or
//! reconnecting. A file in the state directory is read each time a message is built and applied
//! as an RFC 7386 merge patch: objects merge recursively and `null` removes a key.

use std::io::ErrorKind;
use std::path::Path;

use serde_json::{Value, json};

pub fn merge_patch(target: &mut Value, patch: &Value) {
    let Value::Object(patch) = patch else {
        *target = patch.clone();
        return;
    };
    if !target.is_object() {
        *target = json!({});
    }
    let target = target.as_object_mut().expect("just made an object");
    for (key, value) in patch {
        if value.is_null() {
            target.remove(key);
        } else {
            merge_patch(target.entry(key.clone()).or_insert(Value::Null), value);
        }
    }
}

/// Applies `<dir>/<file>` to `target` if it exists and holds a JSON object. Returns a line for
/// the log saying what happened; a missing or broken file leaves `target` unchanged.
pub fn apply_file(dir: &Path, file: &str, label: &str, target: &mut Value) -> String {
    let path = dir.join(file);
    match std::fs::read_to_string(&path) {
        Err(e) if e.kind() == ErrorKind::NotFound => format!("no {label} patch"),
        Err(e) => format!("cannot read {}: {e}", path.display()),
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(patch) if patch.is_object() => {
                merge_patch(target, &patch);
                format!("{label} patch applied: {patch}")
            }
            Ok(_) => format!("{} ignored: not a JSON object", path.display()),
            Err(e) => format!("{} ignored: {e}", path.display()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_patch_follows_rfc_7386() {
        let mut v = json!({"a": {"b": 1, "c": 2}, "d": 3});
        merge_patch(
            &mut v,
            &json!({"a": {"b": 9, "c": null, "e": 5}, "f": {"g": 1}}),
        );
        assert_eq!(v, json!({"a": {"b": 9, "e": 5}, "d": 3, "f": {"g": 1}}));
        merge_patch(&mut v, &json!({"d": [1, 2]}));
        assert_eq!(v["d"], json!([1, 2]));
    }

    #[test]
    fn missing_and_broken_files_leave_the_target_alone() {
        let dir =
            std::env::temp_dir().join(format!("teams-redir-patch-proto-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut v = json!({"a": 1});
        assert_eq!(apply_file(&dir, "p.json", "x", &mut v), "no x patch");
        std::fs::write(dir.join("p.json"), "{ nope").unwrap();
        assert!(apply_file(&dir, "p.json", "x", &mut v).contains("ignored"));
        std::fs::write(dir.join("p.json"), "[1]").unwrap();
        assert!(apply_file(&dir, "p.json", "x", &mut v).contains("not a JSON object"));
        std::fs::write(dir.join("p.json"), r#"{"b":2}"#).unwrap();
        assert!(apply_file(&dir, "p.json", "x", &mut v).starts_with("x patch applied"));
        assert_eq!(v, json!({"a": 1, "b": 2}));
        let _ = std::fs::remove_dir_all(dir);
    }
}
