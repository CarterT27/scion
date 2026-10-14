//! Conversions between `v1` messages and scion-core / scion-object types (PLAN 2.13, P08).
//!
//! `From` / `TryFrom` impls live here. `TryFrom` failures are INVALID_ARGUMENT with subcode
//! `missing_field` (absent required field) or `bad_id_text` (malformed id). Unknown proto
//! fields are ignored (prost default).
