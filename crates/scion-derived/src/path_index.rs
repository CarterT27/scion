//! SPEC 9.2 `path_index` (eager): rows in `object_paths` for every blob introduced by a `Put`
//! and every new tree at its directory path. The rows are computed by
//! `History::create_changeset` (SPEC 5.3 step 2) and inserted with the changeset; this crate
//! registers the type as a descriptor only (`Registry::v1_default`), so that it is listed among
//! the required eager types checked by fsck (SPEC 4.5).

/// Type name.
pub const NAME: &str = "path_index";
/// Type version.
pub const VERSION: u32 = 1;
