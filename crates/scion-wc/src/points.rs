//! Client fault point names (SPEC 19.0, 19.10, 19.16; PLAN 2.17).

/// Goto: journal written, before touching files.
pub const GOTO_AFTER_JOURNAL: &str = "wc.goto.after_journal";
/// Goto: in the middle of writing files.
pub const GOTO_MID_WRITE: &str = "wc.goto.mid_write";
/// Commit: journal written.
pub const COMMIT_AFTER_JOURNAL: &str = "wc.commit.after_journal";
/// Commit: blobs uploaded.
pub const COMMIT_AFTER_UPLOAD: &str = "wc.commit.after_upload";
/// Commit: CreateChangeset returned, base not yet moved.
pub const COMMIT_AFTER_CREATE: &str = "wc.commit.after_create";
/// Rebase: journal written.
pub const REBASE_AFTER_JOURNAL: &str = "wc.rebase.after_journal";
/// Rebase: between stack members.
pub const REBASE_MID_MEMBER: &str = "wc.rebase.mid_member";
/// Rebase: conflict files written, pending not yet updated.
pub const REBASE_AFTER_CONFLICT_WRITE: &str = "wc.rebase.after_conflict_write";
/// Sparse change: in the middle of writing files.
pub const SPARSE_MID_WRITE: &str = "wc.sparse.mid_write";
/// Checkout: in the middle of writing files.
pub const CHECKOUT_MID_WRITE: &str = "wc.checkout.mid_write";

/// Every client fault point.
pub const ALL: &[&str] = &[
    GOTO_AFTER_JOURNAL,
    GOTO_MID_WRITE,
    COMMIT_AFTER_JOURNAL,
    COMMIT_AFTER_UPLOAD,
    COMMIT_AFTER_CREATE,
    REBASE_AFTER_JOURNAL,
    REBASE_MID_MEMBER,
    REBASE_AFTER_CONFLICT_WRITE,
    SPARSE_MID_WRITE,
    CHECKOUT_MID_WRITE,
];
