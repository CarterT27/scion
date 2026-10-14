//! Tree entry modes (SPEC 4.3.2).

use crate::error::{Result, ScionError};
use crate::subcode;

/// SPEC 4.3.2 entry mode: 1 File, 2 Executable, 3 Symlink, 4 Tree.
#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Mode {
    /// Regular file (git 100644).
    File = 1,
    /// Executable file (git 100755).
    Executable = 2,
    /// Symbolic link; the blob holds the target (git 120000).
    Symlink = 3,
    /// Directory (git 040000).
    Tree = 4,
}

impl Mode {
    /// Parse a mode byte; other values are INVALID_ARGUMENT/invalid_mode.
    pub fn from_u8(v: u8) -> Result<Self> {
        match v {
            1 => Ok(Mode::File),
            2 => Ok(Mode::Executable),
            3 => Ok(Mode::Symlink),
            4 => Ok(Mode::Tree),
            _ => Err(ScionError::invalid(
                subcode::INVALID_MODE,
                format!("invalid mode {v}"),
            )),
        }
    }

    /// True for modes 1-3 (entries whose id is a Blob).
    pub fn is_file_like(self) -> bool {
        !matches!(self, Mode::Tree)
    }

    /// The corresponding git mode (SPEC 4.3.2): 0o100644, 0o100755, 0o120000, 0o040000.
    pub fn git_mode(self) -> u32 {
        match self {
            Mode::File => 0o100644,
            Mode::Executable => 0o100755,
            Mode::Symlink => 0o120000,
            Mode::Tree => 0o040000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes() {
        for (v, m) in [
            (1, Mode::File),
            (2, Mode::Executable),
            (3, Mode::Symlink),
            (4, Mode::Tree),
        ] {
            assert_eq!(Mode::from_u8(v).unwrap(), m);
            assert_eq!(m as u8, v);
        }
        for v in [0u8, 5, 255] {
            assert_eq!(
                Mode::from_u8(v).unwrap_err().subcode(),
                Some(subcode::INVALID_MODE)
            );
        }
        assert!(
            Mode::File.is_file_like()
                && Mode::Executable.is_file_like()
                && Mode::Symlink.is_file_like()
        );
        assert!(!Mode::Tree.is_file_like());
        assert_eq!(Mode::File.git_mode(), 0o100644);
        assert_eq!(Mode::Executable.git_mode(), 0o100755);
        assert_eq!(Mode::Symlink.git_mode(), 0o120000);
        assert_eq!(Mode::Tree.git_mode(), 0o040000);
    }
}
