//! Git-to-Scion mappings (SPEC 14; PLAN 6.4; P29).

use scion_core::{ChangeId, ErrorCode, Mode, Result, ScionError};
use scion_object::Timestamp;
use scion_object::enc::{Decoder, Encoder};

/// Domain separator of [`change_id_for`] (PLAN 6.4).
pub const CHANGE_ID_DOMAIN: &[u8] = b"scion-import-v1";

/// Git mode of a regular file (SPEC 4.3.2).
pub const GIT_MODE_FILE: u32 = 0o100644;
/// Group-writable regular file, as written by old git versions; maps to File (SPEC 14).
pub const GIT_MODE_FILE_GROUP_WRITABLE: u32 = 0o100664;
/// Git mode of an executable file (SPEC 4.3.2).
pub const GIT_MODE_EXECUTABLE: u32 = 0o100755;
/// Git mode of a symlink (SPEC 4.3.2).
pub const GIT_MODE_SYMLINK: u32 = 0o120000;
/// Git mode of a subtree (SPEC 4.3.2).
pub const GIT_MODE_TREE: u32 = 0o040000;
/// Git mode of a gitlink (submodule commit; SPEC 14).
pub const GIT_MODE_GITLINK: u32 = 0o160000;

/// `BLAKE3("scion-import-v1" || git commit id)[0..16]` (SPEC 14; PLAN 6.4): re-imports of the
/// same git commit always carry the same change id.
pub fn change_id_for(git_commit: &[u8; 20]) -> ChangeId {
    let mut h = blake3::Hasher::new();
    h.update(CHANGE_ID_DOMAIN);
    h.update(git_commit);
    let digest = h.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest.as_bytes()[..16]);
    ChangeId(out)
}

/// 100644/100664 -> File, 100755 -> Executable, 120000 -> Symlink, 040000 -> `None` (tree);
/// 160000 (gitlink) and anything else -> error (SPEC 14, 4.3.2).
///
/// Errors are INVALID_ARGUMENT. The importer handles gitlinks before calling this: they fail
/// the import unless `--skip-submodules` is given.
pub fn map_mode(git_mode: u32) -> Result<Option<Mode>> {
    match git_mode {
        GIT_MODE_FILE | GIT_MODE_FILE_GROUP_WRITABLE => Ok(Some(Mode::File)),
        GIT_MODE_EXECUTABLE => Ok(Some(Mode::Executable)),
        GIT_MODE_SYMLINK => Ok(Some(Mode::Symlink)),
        GIT_MODE_TREE => Ok(None),
        GIT_MODE_GITLINK => Err(ScionError::new(
            ErrorCode::InvalidArgument,
            "git mode 160000 (gitlink/submodule) has no Scion equivalent; use --skip-submodules",
        )),
        other => Err(ScionError::new(
            ErrorCode::InvalidArgument,
            format!("unsupported git mode {other:o}"),
        )),
    }
}

/// Encode the Meta extension tag 4 value (PLAN 6.4): `list<(path bytes, 20-byte sha)>` in
/// the SPEC 4.1 primitives (uvarint count; per item a length-prefixed path and 20 raw bytes).
pub(crate) fn encode_skipped_gitlinks(links: &[(Vec<u8>, [u8; 20])]) -> Vec<u8> {
    let mut e = Encoder::new();
    e.uvarint(links.len() as u64);
    for (path, sha) in links {
        e.bytes(path);
        e.raw(sha);
    }
    e.finish()
}

/// Decode a Meta extension tag 4 value (inverse of [`encode_skipped_gitlinks`]).
#[cfg_attr(not(feature = "testing"), allow(dead_code))] // Used by `testing` and unit tests.
pub(crate) fn decode_skipped_gitlinks(b: &[u8]) -> Result<Vec<(Vec<u8>, [u8; 20])>> {
    let mut d = Decoder::new(b);
    let n = d.list_len()?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let path = d.bytes()?.to_vec();
        let sha = d.fixed::<20>()?;
        out.push((path, sha));
    }
    d.finish()?;
    Ok(out)
}

/// One parsed git signature value: `<ident> <seconds> <tz>` (SPEC 14 Meta fields).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GitSignature {
    /// The identity bytes verbatim, conventionally `Name <email>`.
    pub ident: Vec<u8>,
    /// Seconds since the epoch and the zone offset in minutes.
    pub time: Timestamp,
}

fn bad_signature(what: &str, value: &[u8]) -> ScionError {
    ScionError::new(
        ErrorCode::InvalidArgument,
        format!(
            "malformed git {what} line {:?}",
            String::from_utf8_lossy(value)
        ),
    )
}

/// Parse a git signature value (an `author`/`committer` header without the keyword).
///
/// The identity is everything before the last two space-separated fields, byte for byte; the
/// zone `[+-]HHMM` becomes signed minutes (`-0000` reads as 0).
pub(crate) fn parse_signature(what: &str, value: &[u8]) -> Result<GitSignature> {
    let mut parts = value.rsplitn(3, |b| *b == b' ');
    let tz = parts.next().ok_or_else(|| bad_signature(what, value))?;
    let secs = parts.next().ok_or_else(|| bad_signature(what, value))?;
    let ident = parts.next().ok_or_else(|| bad_signature(what, value))?;
    let seconds: i64 = std::str::from_utf8(secs)
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| bad_signature(what, value))?;
    let tz_offset_minutes = parse_tz(tz).ok_or_else(|| bad_signature(what, value))?;
    Ok(GitSignature {
        ident: ident.to_vec(),
        time: Timestamp {
            seconds,
            tz_offset_minutes,
        },
    })
}

/// `[+-]HHMM` -> signed minutes.
fn parse_tz(tz: &[u8]) -> Option<i64> {
    let (sign, digits) = match tz.split_first()? {
        (b'+', rest) => (1i64, rest),
        (b'-', rest) => (-1i64, rest),
        _ => return None,
    };
    if digits.len() != 4 || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let n: i64 = std::str::from_utf8(digits).ok()?.parse().ok()?;
    Some(sign * ((n / 100) * 60 + n % 100))
}

/// The Meta-relevant parts of a raw git commit object (SPEC 14).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommitHeader {
    /// The `author` signature.
    pub author: GitSignature,
    /// The `committer` signature.
    pub committer: GitSignature,
    /// The message: every byte after the blank line that ends the headers.
    pub message: Vec<u8>,
}

/// Parse the `author`, `committer` and message of a raw commit object. Continuation lines
/// (multi-line headers such as `gpgsig`) are skipped; the message is kept verbatim.
pub(crate) fn parse_commit_header(raw: &[u8]) -> Result<CommitHeader> {
    let (headers, message) = match raw.windows(2).position(|w| w == b"\n\n") {
        Some(p) => (&raw[..p], raw[p + 2..].to_vec()),
        None => (raw.strip_suffix(b"\n").unwrap_or(raw), Vec::new()),
    };
    let mut author = None;
    let mut committer = None;
    for line in headers.split(|b| *b == b'\n') {
        if line.first() == Some(&b' ') {
            continue;
        }
        if author.is_none()
            && let Some(v) = line.strip_prefix(b"author ")
        {
            author = Some(parse_signature("author", v)?);
            continue;
        }
        if committer.is_none()
            && let Some(v) = line.strip_prefix(b"committer ")
        {
            committer = Some(parse_signature("committer", v)?);
        }
    }
    let missing = |what: &str| {
        ScionError::new(
            ErrorCode::InvalidArgument,
            format!("git commit has no {what} header"),
        )
    };
    Ok(CommitHeader {
        author: author.ok_or_else(|| missing("author"))?,
        committer: committer.ok_or_else(|| missing("committer"))?,
        message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn change_id_matches_definition() {
        let id = [7u8; 20];
        let mut input = b"scion-import-v1".to_vec();
        input.extend_from_slice(&id);
        let want = blake3::hash(&input);
        assert_eq!(change_id_for(&id).0, want.as_bytes()[..16]);
        assert_ne!(change_id_for(&id), change_id_for(&[8u8; 20]));
    }

    #[test]
    fn modes() {
        assert_eq!(map_mode(0o100644).unwrap(), Some(Mode::File));
        assert_eq!(map_mode(0o100664).unwrap(), Some(Mode::File));
        assert_eq!(map_mode(0o100755).unwrap(), Some(Mode::Executable));
        assert_eq!(map_mode(0o120000).unwrap(), Some(Mode::Symlink));
        assert_eq!(map_mode(0o040000).unwrap(), None);
        assert_eq!(
            map_mode(0o160000).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        assert!(map_mode(0o100600).is_err());
        assert!(map_mode(0).is_err());
    }

    #[test]
    fn signatures() {
        let s = parse_signature("author", b"A U Thor <a@example.com> 1700000000 +0130").unwrap();
        assert_eq!(s.ident, b"A U Thor <a@example.com>");
        assert_eq!(s.time.seconds, 1_700_000_000);
        assert_eq!(s.time.tz_offset_minutes, 90);
        let s = parse_signature("author", b"X <x> -5 -0800").unwrap();
        assert_eq!(s.time.seconds, -5);
        assert_eq!(s.time.tz_offset_minutes, -480);
        let s = parse_signature("author", b"<> 0 -0000").unwrap();
        assert_eq!(s.ident, b"<>");
        assert_eq!(s.time.tz_offset_minutes, 0);
        assert!(parse_signature("author", b"X <x> 12").is_err());
        assert!(parse_signature("author", b"X <x> abc +0000").is_err());
        assert!(parse_signature("author", b"X <x> 1 0000").is_err());
        assert!(parse_signature("author", b"X <x> 1 +00000").is_err());
    }

    #[test]
    fn commit_header() {
        let raw = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
parent 0123456789abcdef0123456789abcdef01234567\n\
author A <a@x> 10 +0100\n\
committer C <c@x> 20 -0230\n\
gpgsig -----BEGIN PGP SIGNATURE-----\n \n author fake <f> 1 +0000\n -----END PGP SIGNATURE-----\n\
\n\
subject\n\nbody\n";
        let h = parse_commit_header(raw).unwrap();
        assert_eq!(h.author.ident, b"A <a@x>");
        assert_eq!(h.author.time.tz_offset_minutes, 60);
        assert_eq!(h.committer.ident, b"C <c@x>");
        assert_eq!(h.committer.time.seconds, 20);
        assert_eq!(h.committer.time.tz_offset_minutes, -150);
        assert_eq!(h.message, b"subject\n\nbody\n");

        let no_msg = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nauthor A <a> 1 +0000\ncommitter A <a> 1 +0000\n";
        assert_eq!(parse_commit_header(no_msg).unwrap().message, b"");
        assert!(parse_commit_header(b"tree x\n\nmsg").is_err());
    }

    #[test]
    fn gitlink_extension_round_trip() {
        let links = vec![
            (b"vendor/sub".to_vec(), [1u8; 20]),
            (b"x".to_vec(), [2u8; 20]),
        ];
        let enc = encode_skipped_gitlinks(&links);
        assert_eq!(decode_skipped_gitlinks(&enc).unwrap(), links);
        assert_eq!(
            decode_skipped_gitlinks(&encode_skipped_gitlinks(&[])).unwrap(),
            vec![]
        );
        assert!(decode_skipped_gitlinks(&enc[..enc.len() - 1]).is_err());
    }
}
