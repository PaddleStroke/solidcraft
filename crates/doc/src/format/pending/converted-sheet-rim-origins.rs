//! Converted sheet panels keep exact construction boundary origins and rip origins, carried
//! with their source regions and joints. Converted rim roles name generated walls after the
//! source edge's incident face pair, rather than asserting the old adjacent face was kept.
//! Older designs have no stored boundary origins and retain their existing unverified fallback
//! where no producer provenance is known. The last release opens a newer design with a warning
//! and preserves fields and role variants it cannot read (see `crates/doc/src/newer_file.rs`).
