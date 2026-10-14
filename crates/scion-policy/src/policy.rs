//! Policy evaluation (SPEC 6.2, 12.4; P05).
//!
//! An `acl.toml` file is parsed with `toml` + `serde` (unknown keys rejected), validated and
//! normalized into a [`Policy`]:
//!
//! - path prefixes are stored without the leading `/` and without a trailing `/` (the `/` rule is
//!   the empty prefix), and are matched component-wise;
//! - `github:` logins are lowercased (logins compare case-insensitively), `github-id:` numbers
//!   are stored as integers, `service:` names are kept verbatim;
//! - groups are resolved transitively at parse time, so evaluation never walks groups.
//!
//! Implementation-defined choices (documented here, SPEC 6.2 leaves them open):
//!
//! - A `[[paths]]` rule may omit `read` or `write`. A rule governs only the permissions it
//!   names: for `read` the longest matching prefix *that has a `read` key* governs, and likewise
//!   for `write` ("independently for read and write"). An explicit empty list denies everyone.
//!   If the `/` rule omits a key, that key is treated as an empty list at `/` (default deny).
//! - A ref pattern `p/*` matches every ref name that starts with `p/` and has a non-empty
//!   remainder, which may itself contain `/` (otherwise "longest pattern wins" could never
//!   apply). The pattern `*` matches every ref name.
//! - Duplicate prefixes (after normalization) are rejected as [`PolicyError::BadPrefix`].

use crate::error::PolicyError;
use scion_core::Principal;
use scion_object::{RefName, RepoPath};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

/// SPEC 6.2 `[admin]` permissions.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AdminPerm {
    /// `admin.redact`.
    Redact,
    /// `admin.purge`.
    Purge,
    /// `admin.refs` (create/delete refs).
    Refs,
}

/// A normalized policy principal (SPEC 4.3.8, 6.2).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Member {
    /// `github:<login>`, login lowercased.
    GitHubLogin(String),
    /// `github-id:<n>`.
    GitHubId(u64),
    /// `service:<name>`.
    Service(String),
    /// `group:<name>` (only in written lists; never in resolved sets).
    Group(String),
    /// Everyone (only produced by [`Policy::allow_all_for_tests`]).
    Anyone,
}

impl Member {
    fn canonical(&self) -> String {
        match self {
            Member::GitHubLogin(l) => format!("github:{l}"),
            Member::GitHubId(n) => format!("github-id:{n}"),
            Member::Service(s) => format!("service:{s}"),
            Member::Group(g) => format!("group:{g}"),
            Member::Anyone => "*".to_owned(),
        }
    }
}

/// A permission list: the normalized written members plus the resolved (group-free) set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Grant {
    written: BTreeSet<Member>,
    resolved: BTreeSet<Member>,
}

impl Grant {
    fn anyone() -> Self {
        let set: BTreeSet<Member> = [Member::Anyone].into_iter().collect();
        Grant {
            written: set.clone(),
            resolved: set,
        }
    }

    fn allows(&self, p: &Principal) -> bool {
        if self.resolved.contains(&Member::Anyone) {
            return true;
        }
        principal_atoms(p)
            .iter()
            .any(|atom| self.resolved.contains(atom))
    }
}

/// The resolved members a concrete principal matches.
fn principal_atoms(p: &Principal) -> Vec<Member> {
    match p {
        Principal::GitHub { login, id } => {
            let mut v = vec![Member::GitHubLogin(login.to_ascii_lowercase())];
            if let Some(n) = id {
                v.push(Member::GitHubId(*n));
            }
            v
        }
        Principal::Service(name) => vec![Member::Service(name.clone())],
    }
}

/// A `[[paths]]` rule; `None` means the rule does not govern that permission.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PathRule {
    read: Option<Grant>,
    write: Option<Grant>,
}

/// A `[refs."<name>"]` rule.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct RefRule {
    update: Grant,
    force: Grant,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct AdminRules {
    redact: Grant,
    purge: Grant,
    refs: Grant,
}

/// A validated, normalized policy (SPEC 6.2).
#[derive(Clone, Debug)]
pub struct Policy {
    epoch: u64,
    /// Group name -> normalized written members.
    groups: BTreeMap<String, BTreeSet<Member>>,
    /// Group name -> resolved (transitive, group-free) members.
    closure: BTreeMap<String, BTreeSet<Member>>,
    /// Normalized prefix (no leading or trailing `/`; root = empty) -> rule.
    rules: BTreeMap<Vec<u8>, PathRule>,
    /// Exact ref names.
    ref_exact: BTreeMap<String, RefRule>,
    /// Pattern prefix (`release/` for `release/*`, empty for `*`) -> rule.
    ref_patterns: BTreeMap<String, RefRule>,
    admin: AdminRules,
}

// ---------------------------------------------------------------------------------------------
// Raw serde model.

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPolicy {
    #[allow(dead_code)] // checked before typed deserialization
    version: toml::Value,
    #[serde(default)]
    groups: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    paths: Vec<RawPathRule>,
    #[serde(default)]
    refs: BTreeMap<String, RawRefRule>,
    #[serde(default)]
    admin: RawAdmin,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPathRule {
    prefix: String,
    #[serde(default)]
    read: Option<Vec<String>>,
    #[serde(default)]
    write: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRefRule {
    #[serde(default)]
    update: Vec<String>,
    #[serde(default)]
    force: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawAdmin {
    #[serde(default)]
    redact: Vec<String>,
    #[serde(default)]
    purge: Vec<String>,
    #[serde(default)]
    refs: Vec<String>,
}

// ---------------------------------------------------------------------------------------------
// Validation helpers.

/// Same character rules as `scion_core::Principal::parse` (SPEC 4.3.8).
fn valid_ident(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && s.bytes()
            .all(|b| b.is_ascii_graphic() && b != b':' && b != b'/')
}

fn parse_member(s: &str) -> Result<Member, PolicyError> {
    let bad = || PolicyError::BadPrincipal(s.to_owned());
    if let Some(login) = s.strip_prefix("github:") {
        if valid_ident(login) {
            return Ok(Member::GitHubLogin(login.to_ascii_lowercase()));
        }
    } else if let Some(n) = s.strip_prefix("github-id:") {
        if !n.is_empty() && n.len() <= 20 && n.bytes().all(|b| b.is_ascii_digit()) {
            return n.parse::<u64>().map(Member::GitHubId).map_err(|_| bad());
        }
    } else if let Some(name) = s.strip_prefix("service:") {
        if valid_ident(name) {
            return Ok(Member::Service(name.to_owned()));
        }
    } else if let Some(g) = s.strip_prefix("group:")
        && valid_ident(g)
    {
        return Ok(Member::Group(g.to_owned()));
    }
    Err(bad())
}

fn parse_members(list: &[String]) -> Result<BTreeSet<Member>, PolicyError> {
    list.iter().map(|s| parse_member(s)).collect()
}

/// Normalize a written prefix (`/`, `/a/b`, `/a/b/`) into repository-path bytes (SPEC 4.4).
fn normalize_prefix(p: &str) -> Result<Vec<u8>, PolicyError> {
    let bad = || PolicyError::BadPrefix(p.to_owned());
    let rest = p.strip_prefix('/').ok_or_else(bad)?;
    if rest.is_empty() {
        return Ok(Vec::new());
    }
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    if rest.is_empty() {
        return Err(bad());
    }
    if rest.len() > 4096 {
        return Err(bad());
    }
    for c in rest.split('/') {
        let ok = !c.is_empty()
            && c.len() <= 255
            && c != "."
            && c != ".."
            && !c.as_bytes().contains(&0)
            && !c.eq_ignore_ascii_case(".scion");
        if !ok {
            return Err(bad());
        }
    }
    Ok(rest.as_bytes().to_vec())
}

/// SPEC 4.3.6 ref-name grammar: `^[a-z0-9][a-z0-9._/-]{0,199}$`, no `..`, no `//`, no
/// trailing `/`.
pub(crate) fn valid_ref_name(s: &str) -> bool {
    let b = s.as_bytes();
    let Some(first) = b.first() else {
        return false;
    };
    b.len() <= 200
        && (first.is_ascii_lowercase() || first.is_ascii_digit())
        && b.iter().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'_' | b'/' | b'-')
        })
        && !s.contains("..")
        && !s.contains("//")
        && !s.ends_with('/')
}

enum RefKey {
    Exact(String),
    Pattern(String),
}

fn parse_ref_key(k: &str) -> Result<RefKey, PolicyError> {
    let bad = || PolicyError::BadRefPattern(k.to_owned());
    if !k.contains('*') {
        return if valid_ref_name(k) {
            Ok(RefKey::Exact(k.to_owned()))
        } else {
            Err(bad())
        };
    }
    if k == "*" {
        return Ok(RefKey::Pattern(String::new()));
    }
    let base = k.strip_suffix("/*").ok_or_else(bad)?;
    if base.contains('*') || !valid_ref_name(base) {
        return Err(bad());
    }
    Ok(RefKey::Pattern(format!("{base}/")))
}

/// Resolve every group transitively; detect cycles and unknown groups (SPEC 6.2).
fn resolve_groups(
    groups: &BTreeMap<String, BTreeSet<Member>>,
) -> Result<BTreeMap<String, BTreeSet<Member>>, PolicyError> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        Visiting,
        Done,
    }
    fn visit(
        name: &str,
        groups: &BTreeMap<String, BTreeSet<Member>>,
        marks: &mut BTreeMap<String, Mark>,
        stack: &mut Vec<String>,
        out: &mut BTreeMap<String, BTreeSet<Member>>,
    ) -> Result<(), PolicyError> {
        match marks.get(name) {
            Some(Mark::Done) => return Ok(()),
            Some(Mark::Visiting) => {
                let start = stack.iter().position(|g| g == name).unwrap_or(0);
                let mut cycle: Vec<String> = stack[start..].to_vec();
                cycle.push(name.to_owned());
                return Err(PolicyError::GroupCycle(cycle));
            }
            None => {}
        }
        let members = groups
            .get(name)
            .ok_or_else(|| PolicyError::UnknownGroup(name.to_owned()))?;
        marks.insert(name.to_owned(), Mark::Visiting);
        stack.push(name.to_owned());
        let mut resolved = BTreeSet::new();
        for m in members {
            match m {
                Member::Group(g) => {
                    visit(g, groups, marks, stack, out)?;
                    if let Some(sub) = out.get(g) {
                        resolved.extend(sub.iter().cloned());
                    }
                }
                other => {
                    resolved.insert(other.clone());
                }
            }
        }
        stack.pop();
        marks.insert(name.to_owned(), Mark::Done);
        out.insert(name.to_owned(), resolved);
        Ok(())
    }

    let mut marks = BTreeMap::new();
    let mut out = BTreeMap::new();
    for name in groups.keys() {
        let mut stack = Vec::new();
        visit(name, groups, &mut marks, &mut stack, &mut out)?;
    }
    Ok(out)
}

fn make_grant(
    list: &[String],
    closure: &BTreeMap<String, BTreeSet<Member>>,
) -> Result<Grant, PolicyError> {
    let written = parse_members(list)?;
    let mut resolved = BTreeSet::new();
    for m in &written {
        match m {
            Member::Group(g) => {
                let sub = closure
                    .get(g)
                    .ok_or_else(|| PolicyError::UnknownGroup(g.clone()))?;
                resolved.extend(sub.iter().cloned());
            }
            other => {
                resolved.insert(other.clone());
            }
        }
    }
    Ok(Grant { written, resolved })
}

/// Map a serde error to `UnknownKey` (with the full dotted key path) or `Toml`.
fn map_serde_error(e: serde_path_to_error::Error<toml::de::Error>) -> PolicyError {
    let path = e.path().to_string();
    let msg = e.inner().message().to_owned();
    if let Some(rest) = msg.strip_prefix("unknown field `")
        && let Some(end) = rest.find('`')
    {
        let field = &rest[..end];
        // serde_path_to_error records the offending key as the last path segment.
        let full = if path == "." || path.is_empty() {
            field.to_owned()
        } else {
            path
        };
        return PolicyError::UnknownKey(full);
    }
    if path == "." || path.is_empty() {
        PolicyError::Toml(msg)
    } else {
        PolicyError::Toml(format!("{path}: {msg}"))
    }
}

// ---------------------------------------------------------------------------------------------
// Canonical serialization for the epoch (PLAN 6.2).

struct Canon(Vec<u8>);

impl Canon {
    fn tag(&mut self, t: u8) {
        self.0.push(t);
    }
    fn bytes(&mut self, b: &[u8]) {
        self.0.extend_from_slice(&(b.len() as u64).to_be_bytes());
        self.0.extend_from_slice(b);
    }
    fn count(&mut self, n: usize) {
        self.0.extend_from_slice(&(n as u64).to_be_bytes());
    }
    fn members(&mut self, set: &BTreeSet<Member>) {
        // Sorted by canonical text so the order is independent of the enum layout.
        let texts: BTreeSet<String> = set.iter().map(Member::canonical).collect();
        self.count(texts.len());
        for t in texts {
            self.bytes(t.as_bytes());
        }
    }
    fn grant(&mut self, g: &Grant) {
        self.members(&g.written);
    }
    fn opt_grant(&mut self, g: &Option<Grant>) {
        match g {
            None => self.tag(0),
            Some(g) => {
                self.tag(1);
                self.grant(g);
            }
        }
    }
}

impl Policy {
    fn compute_epoch(&self) -> u64 {
        let mut c = Canon(Vec::new());
        c.bytes(b"scion-acl-v1");
        c.tag(b'G');
        c.count(self.groups.len());
        for (name, members) in &self.groups {
            c.bytes(name.as_bytes());
            c.members(members);
        }
        c.tag(b'P');
        c.count(self.rules.len());
        for (prefix, rule) in &self.rules {
            c.bytes(prefix);
            c.opt_grant(&rule.read);
            c.opt_grant(&rule.write);
        }
        c.tag(b'R');
        c.count(self.ref_exact.len());
        for (name, r) in &self.ref_exact {
            c.bytes(name.as_bytes());
            c.grant(&r.update);
            c.grant(&r.force);
        }
        c.tag(b'*');
        c.count(self.ref_patterns.len());
        for (prefix, r) in &self.ref_patterns {
            c.bytes(prefix.as_bytes());
            c.grant(&r.update);
            c.grant(&r.force);
        }
        c.tag(b'A');
        c.grant(&self.admin.redact);
        c.grant(&self.admin.purge);
        c.grant(&self.admin.refs);
        let digest = blake3::hash(&c.0);
        let mut first = [0u8; 8];
        first.copy_from_slice(&digest.as_bytes()[..8]);
        u64::from_be_bytes(first)
    }

    /// Parse and validate `acl.toml` text (SPEC 6.2). Unknown keys are rejected with their
    /// full dotted name; `version` must be 1; a `/` rule is REQUIRED; groups may nest but must
    /// not form cycles or reference undefined groups.
    pub fn parse(toml: &str) -> Result<Self, PolicyError> {
        let table: toml::Table = toml
            .parse()
            .map_err(|e: toml::de::Error| PolicyError::Toml(e.message().to_owned()))?;
        match table.get("version") {
            None => return Err(PolicyError::Toml("missing field `version`".to_owned())),
            Some(toml::Value::Integer(1)) => {}
            Some(toml::Value::Integer(v)) => return Err(PolicyError::UnsupportedVersion(*v)),
            Some(other) => {
                return Err(PolicyError::Toml(format!(
                    "version: expected integer, found {}",
                    other.type_str()
                )));
            }
        }
        let raw: RawPolicy =
            serde_path_to_error::deserialize(serde::de::IntoDeserializer::into_deserializer(table))
                .map_err(map_serde_error)?;

        // Groups.
        let mut groups = BTreeMap::new();
        for (name, members) in &raw.groups {
            if !valid_ident(name) {
                return Err(PolicyError::BadPrincipal(format!("group:{name}")));
            }
            groups.insert(name.clone(), parse_members(members)?);
        }
        let closure = resolve_groups(&groups)?;

        // Path rules.
        let mut rules = BTreeMap::new();
        for r in &raw.paths {
            let prefix = normalize_prefix(&r.prefix)?;
            let read = r
                .read
                .as_deref()
                .map(|l| make_grant(l, &closure))
                .transpose()?;
            let write = r
                .write
                .as_deref()
                .map(|l| make_grant(l, &closure))
                .transpose()?;
            if rules.insert(prefix, PathRule { read, write }).is_some() {
                return Err(PolicyError::BadPrefix(format!(
                    "duplicate prefix {}",
                    r.prefix
                )));
            }
        }
        let Some(root) = rules.get_mut(&Vec::new()) else {
            return Err(PolicyError::MissingRootRule);
        };
        // The root rule governs every permission it does not name as an empty grant.
        root.read.get_or_insert_with(Grant::default);
        root.write.get_or_insert_with(Grant::default);

        // Refs.
        let mut ref_exact = BTreeMap::new();
        let mut ref_patterns = BTreeMap::new();
        for (key, r) in &raw.refs {
            let rule = RefRule {
                update: make_grant(&r.update, &closure)?,
                force: make_grant(&r.force, &closure)?,
            };
            let dup = match parse_ref_key(key)? {
                RefKey::Exact(n) => ref_exact.insert(n, rule).is_some(),
                RefKey::Pattern(p) => ref_patterns.insert(p, rule).is_some(),
            };
            if dup {
                return Err(PolicyError::BadRefPattern(format!("duplicate {key}")));
            }
        }

        let admin = AdminRules {
            redact: make_grant(&raw.admin.redact, &closure)?,
            purge: make_grant(&raw.admin.purge, &closure)?,
            refs: make_grant(&raw.admin.refs, &closure)?,
        };

        let mut p = Policy {
            epoch: 0,
            groups,
            closure,
            rules,
            ref_exact,
            ref_patterns,
            admin,
        };
        p.epoch = p.compute_epoch();
        Ok(p)
    }

    /// `BLAKE3(normalized policy)[0..8]` big-endian (PLAN 6.2, SPEC 6.2 reload semantics).
    /// Equivalent files (reordered, reformatted, differently-cased logins) agree.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The longest matching rule (component-wise) that governs the selected permission.
    fn governing<'a>(
        &'a self,
        path: &[u8],
        pick: impl Fn(&'a PathRule) -> Option<&'a Grant>,
    ) -> Option<(&'a [u8], &'a Grant)> {
        // Candidates from longest to shortest: the path itself, then each ancestor, then root.
        let mut end = path.len();
        loop {
            let cand = &path[..end];
            if let Some((k, rule)) = self.rules.get_key_value(cand)
                && let Some(g) = pick(rule)
            {
                return Some((k.as_slice(), g));
            }
            if end == 0 {
                return None;
            }
            end = path[..end].iter().rposition(|b| *b == b'/').unwrap_or(0);
        }
    }

    /// Normalize raw path bytes: tolerate a leading or trailing `/`.
    fn norm(path: &[u8]) -> &[u8] {
        let p = path.strip_prefix(b"/").unwrap_or(path);
        p.strip_suffix(b"/").unwrap_or(p)
    }

    /// [`Policy::can_read`] over raw repository-path bytes (SPEC 4.4 form; root = empty).
    /// SPEC 6.2: the longest matching `read` rule governs.
    pub fn can_read_bytes(&self, p: &Principal, path: &[u8]) -> bool {
        self.governing(Self::norm(path), |r| r.read.as_ref())
            .is_some_and(|(_, g)| g.allows(p))
    }

    /// [`Policy::can_write`] over raw repository-path bytes (SPEC 4.4 form; root = empty).
    /// SPEC 6.2: the longest matching `write` rule governs.
    pub fn can_write_bytes(&self, p: &Principal, path: &[u8]) -> bool {
        self.governing(Self::norm(path), |r| r.write.as_ref())
            .is_some_and(|(_, g)| g.allows(p))
    }

    /// [`Policy::read_governed_by_root`] over raw repository-path bytes (SPEC 12.5).
    pub fn read_governed_by_root_bytes(&self, path: &[u8]) -> bool {
        self.governing(Self::norm(path), |r| r.read.as_ref())
            .is_some_and(|(k, _)| k.is_empty())
    }

    /// The governing ref rule: exact name first, else the longest matching pattern.
    fn ref_rule(&self, name: &str) -> Option<&RefRule> {
        if let Some(r) = self.ref_exact.get(name) {
            return Some(r);
        }
        self.ref_patterns
            .iter()
            .filter(|(prefix, _)| name.len() > prefix.len() && name.starts_with(prefix.as_str()))
            .max_by_key(|(prefix, _)| prefix.len())
            .map(|(_, r)| r)
    }

    /// [`Policy::can_update_ref`] by ref-name string (SPEC 6.2, 12.4).
    pub fn can_update_ref_name(&self, p: &Principal, name: &str) -> bool {
        self.ref_rule(name).is_some_and(|r| r.update.allows(p))
    }

    /// [`Policy::can_force_ref`] by ref-name string (SPEC 6.2, 12.4).
    pub fn can_force_ref_name(&self, p: &Principal, name: &str) -> bool {
        self.ref_rule(name).is_some_and(|r| r.force.allows(p))
    }

    /// Longest matching read rule grants `p` (SPEC 6.2, 12.4).
    pub fn can_read(&self, p: &Principal, path: &RepoPath) -> bool {
        self.can_read_bytes(p, path.as_bytes())
    }

    /// Longest matching write rule grants `p` (SPEC 6.2, 12.4).
    pub fn can_write(&self, p: &Principal, path: &RepoPath) -> bool {
        self.can_write_bytes(p, path.as_bytes())
    }

    /// True when the longest matching read rule for `path` is `/` (root access class,
    /// SPEC 12.5).
    pub fn read_governed_by_root(&self, path: &RepoPath) -> bool {
        self.read_governed_by_root_bytes(path.as_bytes())
    }

    /// `update` on ref `r` (exact name beats patterns; longest pattern wins; SPEC 6.2, 12.4).
    pub fn can_update_ref(&self, p: &Principal, r: &RefName) -> bool {
        self.can_update_ref_name(p, r.as_str())
    }

    /// `force` on ref `r` (SPEC 6.2, 12.4).
    pub fn can_force_ref(&self, p: &Principal, r: &RefName) -> bool {
        self.can_force_ref_name(p, r.as_str())
    }

    /// An `[admin]` permission (SPEC 6.2, 12.4).
    pub fn can_admin(&self, p: &Principal, a: AdminPerm) -> bool {
        match a {
            AdminPerm::Redact => self.admin.redact.allows(p),
            AdminPerm::Purge => self.admin.purge.allows(p),
            AdminPerm::Refs => self.admin.refs.allows(p),
        }
    }

    /// Groups `p` belongs to (transitively), sorted (SPEC 6.2).
    pub fn groups_of(&self, p: &Principal) -> Vec<String> {
        let atoms = principal_atoms(p);
        self.closure
            .iter()
            .filter(|(_, members)| atoms.iter().any(|a| members.contains(a)))
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// A policy granting everything to everyone (tests).
    pub fn allow_all_for_tests() -> Self {
        let mut rules = BTreeMap::new();
        rules.insert(
            Vec::new(),
            PathRule {
                read: Some(Grant::anyone()),
                write: Some(Grant::anyone()),
            },
        );
        let mut ref_patterns = BTreeMap::new();
        ref_patterns.insert(
            String::new(),
            RefRule {
                update: Grant::anyone(),
                force: Grant::anyone(),
            },
        );
        let mut p = Policy {
            epoch: 0,
            groups: BTreeMap::new(),
            closure: BTreeMap::new(),
            rules,
            ref_exact: BTreeMap::new(),
            ref_patterns,
            admin: AdminRules {
                redact: Grant::anyone(),
                purge: Grant::anyone(),
                refs: Grant::anyone(),
            },
        };
        p.epoch = p.compute_epoch();
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gh(login: &str) -> Principal {
        Principal::GitHub {
            login: login.into(),
            id: None,
        }
    }

    #[test]
    fn prefix_normalization() {
        assert_eq!(normalize_prefix("/").unwrap(), b"");
        assert_eq!(normalize_prefix("/a/b/").unwrap(), b"a/b");
        for bad in ["", "a", "//", "/a//b", "/./x", "/../x", "/.scion", "/a\0b"] {
            assert!(normalize_prefix(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn ref_keys() {
        assert!(matches!(parse_ref_key("main"), Ok(RefKey::Exact(n)) if n == "main"));
        assert!(matches!(parse_ref_key("release/*"), Ok(RefKey::Pattern(p)) if p == "release/"));
        assert!(matches!(parse_ref_key("*"), Ok(RefKey::Pattern(p)) if p.is_empty()));
        for bad in [
            "", "Main", "rel*", "a/*/b", "a/**", "*/x", "a//b", "a/", "a..b",
        ] {
            assert!(parse_ref_key(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn members() {
        assert_eq!(
            parse_member("github:Alice").unwrap(),
            Member::GitHubLogin("alice".into())
        );
        assert_eq!(parse_member("github-id:42").unwrap(), Member::GitHubId(42));
        for bad in [
            "github-id:",
            "github-id:+1",
            "github-id:x",
            "github-id:99999999999999999999",
            "alice",
            "group:",
            "service:a b",
            "user:x",
        ] {
            assert!(parse_member(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn governing_walks_components() {
        let p = Policy::parse(
            r#"
version = 1
[[paths]]
prefix = "/"
read = ["github:a"]
[[paths]]
prefix = "/x/y"
read = ["github:b"]
"#,
        )
        .unwrap();
        assert!(p.can_read_bytes(&gh("b"), b"x/y/z/w"));
        assert!(p.can_read_bytes(&gh("a"), b"x/yz"));
        assert!(p.can_read_bytes(&gh("a"), b"x"));
        assert!(!p.can_write_bytes(&gh("a"), b"x"));
        assert!(p.read_governed_by_root_bytes(b""));
        assert!(!p.read_governed_by_root_bytes(b"x/y"));
    }
}
