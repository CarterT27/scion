//! Environment lookup for `$NAME` values (SPEC 6).

use std::collections::HashMap;

/// A source of environment variables.
pub trait Env: Send + Sync {
    /// The value of `name`, if set.
    fn var(&self, name: &str) -> Option<String>;
}

/// The process environment.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessEnv;

impl Env for ProcessEnv {
    fn var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

/// A fixed map (tests).
#[derive(Clone, Debug, Default)]
pub struct MapEnv(pub HashMap<String, String>);

impl Env for MapEnv {
    fn var(&self, name: &str) -> Option<String> {
        self.0.get(name).cloned()
    }
}
