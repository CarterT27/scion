//! `--json` output envelope and published schemas (SPEC 11.8, 19.10; PLAN 6.15; P31).
//!
//! Every command run with `--json` prints exactly one line on stdout:
//! `{"schema":"scion.cli.<cmd>.v1","data":...}`. A failing command prints the
//! `scion.cli.error.v1` envelope instead ([`views::ErrorData`]). The JSON Schemas are generated
//! from the Rust types with `schemars` and committed under `schemas/v1/<cmd>.schema.json`; a
//! test regenerates them and fails on any difference.

use schemars::JsonSchema;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

pub mod views;

use views::*;

/// Version of every published `--json` schema.
pub const SCHEMA_VERSION: u32 = 1;

/// `{"schema":"scion.cli.<cmd>.v1","data":...}`.
#[derive(serde::Serialize, schemars::JsonSchema)]
pub struct Envelope<T> {
    /// `scion.cli.<cmd>.v1`.
    pub schema: String,
    /// Command-specific payload.
    pub data: T,
}

/// Name of the error pseudo-command (`scion.cli.error.v1`, PLAN 6.15).
pub const ERROR_SCHEMA: &str = "error";

/// `scion.cli.<cmd>.v<SCHEMA_VERSION>` (PLAN 6.15).
pub fn schema_id(cmd: &str) -> String {
    format!("scion.cli.{cmd}.v{SCHEMA_VERSION}")
}

/// File name of the published schema of `cmd`: `<cmd>.schema.json`.
pub fn schema_file_name(cmd: &str) -> String {
    format!("{cmd}.schema.json")
}

/// Wrap `data` in the envelope of `cmd` (PLAN 6.15).
pub fn envelope<T: serde::Serialize>(cmd: &str, data: T) -> Envelope<T> {
    Envelope {
        schema: schema_id(cmd),
        data,
    }
}

macro_rules! commands {
    ($($name:literal => $ty:ty),* $(,)?) => {
        const COMMANDS: &[&str] = &[$($name),*];

        fn data_schema(cmd: &str) -> Option<Value> {
            match cmd {
                $($name => Some(envelope_schema::<$ty>($name)),)*
                ERROR_SCHEMA => Some(envelope_schema::<ErrorData>(ERROR_SCHEMA)),
                _ => None,
            }
        }
    };
}

commands! {
    "login" => LoginData,
    "whoami" => WhoamiData,
    "checkout" => CheckoutData,
    "sparse" => SparseData,
    "status" => StatusData,
    "diff" => DiffData,
    "commit" => CommitData,
    "amend" => CommitData,
    "goto" => GotoReport,
    "rebase" => RebaseData,
    "resolve" => ResolveData,
    "land" => LandReport,
    "log" => LogData,
    "blame" => BlameData,
    "show" => ShowData,
    "cat" => CatData,
    "redact" => RedactData,
}

/// Recursively rebuild `v` with object keys in sorted order, so the published bytes do not
/// depend on whether `serde_json` preserves insertion order in a given build.
fn canonical(v: Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut entries: Vec<(String, Value)> = m.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let mut out = Map::new();
            for (k, v) in entries {
                out.insert(k, canonical(v));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.into_iter().map(canonical).collect()),
        other => other,
    }
}

/// The JSON Schema of `Envelope<T>` for `cmd`, with `schema` pinned to its constant.
fn envelope_schema<T: JsonSchema>(cmd: &str) -> Value {
    let mut v = schemars::schema_for!(Envelope<T>).to_value();
    if let Some(obj) = v.as_object_mut() {
        obj.insert("title".to_owned(), json!(schema_id(cmd)));
        obj.insert(
            "description".to_owned(),
            json!(format!(
                "`scion {cmd} --json` output envelope (SPEC 11.8; PLAN 6.15)."
            )),
        );
        obj.insert("additionalProperties".to_owned(), json!(false));
        if let Some(props) = obj.get_mut("properties").and_then(Value::as_object_mut) {
            props.insert(
                "schema".to_owned(),
                json!({
                    "description": "Schema identifier.",
                    "type": "string",
                    "const": schema_id(cmd),
                }),
            );
        }
    }
    canonical(v)
}

/// The JSON Schema of `cmd` (a name from [`command_names`] or [`ERROR_SCHEMA`]).
pub fn schema_for(cmd: &str) -> Option<Value> {
    data_schema(cmd)
}

/// The published text of the schema of `cmd`: pretty JSON with sorted keys and a trailing
/// newline.
pub fn schema_text(cmd: &str) -> Option<String> {
    let v = schema_for(cmd)?;
    serde_json::to_string_pretty(&v).ok().map(|mut s| {
        s.push('\n');
        s
    })
}

/// Write `schemas/v1/<cmd>.schema.json` for every command; returns the paths.
///
/// Besides one file per [`command_names`] entry, `error.schema.json` describes the error
/// envelope printed by failing commands under `--json`.
pub fn write_schemas(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    std::fs::create_dir_all(dir)?;
    let mut out = Vec::new();
    for cmd in command_names().iter().copied().chain([ERROR_SCHEMA]) {
        let text = schema_text(cmd).ok_or_else(|| {
            std::io::Error::other(format!("no schema registered for command {cmd}"))
        })?;
        let path = dir.join(schema_file_name(cmd));
        std::fs::write(&path, text)?;
        out.push(path);
    }
    Ok(out)
}

/// Every command with a `--json` schema.
pub fn command_names() -> &'static [&'static str] {
    COMMANDS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_has_a_valid_schema() {
        assert_eq!(command_names().len(), 17);
        for cmd in command_names().iter().copied().chain([ERROR_SCHEMA]) {
            let s = schema_for(cmd).unwrap();
            assert_eq!(s["title"], json!(schema_id(cmd)));
            assert_eq!(s["properties"]["schema"]["const"], json!(schema_id(cmd)));
            jsonschema::validator_for(&s).unwrap();
        }
        assert!(schema_for("nope").is_none());
    }

    #[test]
    fn envelope_validates_and_rejects_wrong_schema_name() {
        let s = schema_for("resolve").unwrap();
        let ok = serde_json::to_value(envelope(
            "resolve",
            ResolveData {
                path: "a".into(),
                remaining: vec![],
            },
        ))
        .unwrap();
        assert!(jsonschema::is_valid(&s, &ok));
        let mut bad = ok.clone();
        bad["schema"] = json!("scion.cli.status.v1");
        assert!(!jsonschema::is_valid(&s, &bad));
        let mut extra = ok;
        extra["data"]["unexpected"] = json!(1);
        assert!(!jsonschema::is_valid(&s, &extra));
    }

    #[test]
    fn write_schemas_writes_one_file_per_command_plus_error() {
        let dir = tempfile::tempdir().unwrap();
        let paths = write_schemas(dir.path()).unwrap();
        assert_eq!(paths.len(), command_names().len() + 1);
        for p in paths {
            let text = std::fs::read_to_string(&p).unwrap();
            assert!(text.ends_with('\n'));
            let _: Value = serde_json::from_str(&text).unwrap();
        }
    }
}
