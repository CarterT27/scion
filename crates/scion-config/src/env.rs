//! Environment lookup for `$NAME` values and the shared TOML decoding pipeline (SPEC 6).
//!
//! Every configuration file goes through [`decode`]: TOML syntax is parsed into a
//! [`toml::Table`], every string of the exact form `$NAME` is replaced by the value of the
//! environment variable `NAME` (keeping the dotted key path for errors), and the result is
//! deserialized through `serde_path_to_error` so that unknown keys, missing keys and invalid
//! values are reported with their full dotted key.

use crate::error::{ConfigError, from_path_error};
use serde::de::DeserializeOwned;
use std::collections::HashMap;

/// A source of environment variables (SPEC 6: `$NAME` values resolve from the environment).
pub trait Env: Send + Sync {
    /// The value of `name`, if set.
    fn var(&self, name: &str) -> Option<String>;
}

/// The process environment (SPEC 6).
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessEnv;

impl Env for ProcessEnv {
    fn var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

/// A fixed map (tests; SPEC 19.15).
#[derive(Clone, Debug, Default)]
pub struct MapEnv(pub HashMap<String, String>);

impl MapEnv {
    /// Build a map environment from `(name, value)` pairs (SPEC 19.15 tests).
    pub fn from_pairs<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        MapEnv(
            pairs
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }
}

impl Env for MapEnv {
    fn var(&self, name: &str) -> Option<String> {
        self.0.get(name).cloned()
    }
}

/// If `s` has the exact form `$NAME` (NAME = `[A-Za-z_][A-Za-z0-9_]*`), return `NAME`.
pub(crate) fn var_reference(s: &str) -> Option<&str> {
    let name = s.strip_prefix('$')?;
    let mut chars = name.chars();
    let first = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    if chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Some(name)
    } else {
        None
    }
}

fn join_key(parent: &str, key: &str) -> String {
    if parent.is_empty() {
        key.to_string()
    } else {
        format!("{parent}.{key}")
    }
}

/// Replace every `$NAME` string value in `value` (recursively) with the variable's value.
/// `path` is the dotted key of `value`; array elements are `key[i]`.
pub(crate) fn resolve_vars(
    value: &mut toml::Value,
    path: &str,
    env: &dyn Env,
) -> Result<(), ConfigError> {
    match value {
        toml::Value::String(s) => {
            if let Some(name) = var_reference(s) {
                match env.var(name) {
                    Some(v) => *s = v,
                    None => {
                        return Err(ConfigError::UnresolvedVar {
                            name: name.to_string(),
                            key: path.to_string(),
                        });
                    }
                }
            }
            Ok(())
        }
        toml::Value::Array(items) => {
            for (i, item) in items.iter_mut().enumerate() {
                resolve_vars(item, &format!("{path}[{i}]"), env)?;
            }
            Ok(())
        }
        toml::Value::Table(table) => resolve_table(table, path, env),
        _ => Ok(()),
    }
}

fn resolve_table(table: &mut toml::Table, path: &str, env: &dyn Env) -> Result<(), ConfigError> {
    for (key, item) in table.iter_mut() {
        resolve_vars(item, &join_key(path, key), env)?;
    }
    Ok(())
}

/// Parse `text` as TOML, resolve `$NAME` values from `env`, and deserialize into `T` with
/// full-key error reporting (SPEC 6, 19.15).
pub(crate) fn decode<T: DeserializeOwned>(text: &str, env: &dyn Env) -> Result<T, ConfigError> {
    let mut table: toml::Table =
        toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
    resolve_table(&mut table, "", env)?;
    serde_path_to_error::deserialize(toml::Value::Table(table)).map_err(from_path_error)
}

/// Read a configuration file to a string (`ConfigError::Io` names the path).
pub(crate) fn read_file(p: &std::path::Path) -> Result<String, ConfigError> {
    std::fs::read_to_string(p).map_err(|e| ConfigError::Io(format!("{}: {e}", p.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn var_reference_shapes() {
        assert_eq!(var_reference("$A"), Some("A"));
        assert_eq!(var_reference("$SCION_DB_URL"), Some("SCION_DB_URL"));
        assert_eq!(var_reference("$_x1"), Some("_x1"));
        assert_eq!(var_reference("$"), None);
        assert_eq!(var_reference("$1A"), None);
        assert_eq!(var_reference("$A-B"), None);
        assert_eq!(var_reference("a$A"), None);
        assert_eq!(var_reference("$A B"), None);
        assert_eq!(var_reference("plain"), None);
    }

    #[test]
    fn resolve_nested_and_arrays() {
        let env = MapEnv::from_pairs([("X", "1"), ("Y", "2")]);
        let mut v = toml::Value::Table(
            toml::from_str("a = \"$X\"\n[s]\nb = [\"lit\", \"$Y\"]\n[[t]]\nc = \"$X\"\n").unwrap(),
        );
        resolve_vars(&mut v, "", &env).unwrap();
        assert_eq!(v["a"].as_str(), Some("1"));
        assert_eq!(v["s"]["b"][0].as_str(), Some("lit"));
        assert_eq!(v["s"]["b"][1].as_str(), Some("2"));
        assert_eq!(v["t"][0]["c"].as_str(), Some("1"));

        let mut v = toml::Value::Table(toml::from_str("[[t]]\nc = \"$NOPE\"\n").unwrap());
        assert_eq!(
            resolve_vars(&mut v, "", &env),
            Err(ConfigError::UnresolvedVar {
                name: "NOPE".into(),
                key: "t[0].c".into()
            })
        );
    }

    #[test]
    fn parse_errors_are_typed() {
        let r: Result<toml::Table, _> = decode("a = ", &MapEnv::default());
        assert!(matches!(r, Err(ConfigError::Parse(_))));
        let r = read_file(std::path::Path::new("/nonexistent/scion/config.toml"));
        assert!(matches!(r, Err(ConfigError::Io(m)) if m.contains("/nonexistent/scion")));
    }
}
