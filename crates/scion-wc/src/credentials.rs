//! Client credentials file (SPEC 6.4, 12, 17; PLAN 6.15; P11).
//!
//! TOML `{ token, github_token? }`, created with mode `0600` (its parent directory, when
//! created, `0700`). Reading refuses a file that is readable by group or others.

use crate::fs_safe::{io_error, write_atomic};
use scion_core::{ErrorCode, Result, ScionError};
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::Path;

/// `{token, github_token?}`; `Debug` never prints the secrets (SPEC 6.4).
#[derive(Clone, PartialEq)]
pub struct Credentials {
    /// Scion token.
    pub token: String,
    /// GitHub OAuth token for silent re-exchange.
    pub github_token: Option<String>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("token", &"***")
            .field("github_token", &self.github_token.as_ref().map(|_| "***"))
            .finish()
    }
}

/// On-disk form.
#[derive(serde::Serialize, serde::Deserialize)]
struct File {
    token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    github_token: Option<String>,
}

/// Group and other read bits.
const GROUP_WORLD_READ: u32 = 0o044;

/// Write with mode 0600, atomically; a missing parent directory is created with mode 0700
/// (SPEC 6.4, 17).
pub fn write(path: &Path, c: &Credentials) -> Result<()> {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let name = path.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
        ScionError::new(
            ErrorCode::InvalidArgument,
            format!("invalid credentials path {}", path.display()),
        )
    })?;
    if !parent.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|e| io_error("create credentials directory", e))?;
    }
    let text = toml::to_string(&File {
        token: c.token.clone(),
        github_token: c.github_token.clone(),
    })
    .map_err(|e| {
        ScionError::new(
            ErrorCode::InvalidArgument,
            format!("cannot serialize credentials: {e}"),
        )
    })?;
    write_atomic(parent, name, text.as_bytes(), 0o600, true)
}

/// Read, refusing group- or world-readable files with PERMISSION_DENIED (SPEC 6.4, 17).
pub fn read(path: &Path) -> Result<Credentials> {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ScionError::not_found(format!(
                "no credentials at {}; run `scion login`",
                path.display()
            )));
        }
        Err(e) => return Err(io_error("open credentials file", e)),
    };
    let md = file
        .metadata()
        .map_err(|e| io_error("stat credentials file", e))?;
    let mode = md.permissions().mode() & 0o777;
    if mode & GROUP_WORLD_READ != 0 {
        return Err(ScionError::permission_denied(format!(
            "refusing to read credentials file {}: it is readable by group or others (mode {mode:04o}); \
             run `chmod 600 {}`",
            path.display(),
            path.display()
        )));
    }
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|e| io_error("read credentials file", e))?;
    let f: File = toml::from_str(&text).map_err(|e| {
        ScionError::new(
            ErrorCode::InvalidArgument,
            format!(
                "malformed credentials file {}: {}",
                path.display(),
                e.message()
            ),
        )
    })?;
    Ok(Credentials {
        token: f.token,
        github_token: f.github_token,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode_of(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn created_0600_in_0700_parent_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cfg/scion/credentials");
        let c = Credentials {
            token: "v4.public.secret".into(),
            github_token: Some("gho_secret".into()),
        };
        write(&path, &c).unwrap();
        assert_eq!(mode_of(&path), 0o600);
        assert_eq!(mode_of(&dir.path().join("cfg/scion")), 0o700);
        assert_eq!(mode_of(&dir.path().join("cfg")), 0o700);
        assert_eq!(read(&path).unwrap(), c);

        let c2 = Credentials {
            token: "t2".into(),
            github_token: None,
        };
        write(&path, &c2).unwrap();
        assert_eq!(mode_of(&path), 0o600);
        assert_eq!(read(&path).unwrap(), c2);
        let dbg = format!("{c:?}");
        assert!(!dbg.contains("secret"), "{dbg}");
    }

    #[test]
    fn group_or_world_readable_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials");
        write(
            &path,
            &Credentials {
                token: "t".into(),
                github_token: None,
            },
        )
        .unwrap();
        for bad in [0o640, 0o604, 0o644, 0o666] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(bad)).unwrap();
            let err = read(&path).unwrap_err();
            assert_eq!(err.code, ErrorCode::PermissionDenied, "{bad:o}");
            assert!(err.message.contains("chmod 600"), "{}", err.message);
        }
        for ok in [0o600, 0o400] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(ok)).unwrap();
            assert_eq!(read(&path).unwrap().token, "t");
        }
        assert_eq!(
            read(&dir.path().join("missing")).unwrap_err().code,
            ErrorCode::NotFound
        );
    }
}
