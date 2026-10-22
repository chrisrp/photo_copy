//! Core logic for photo_copy: scanning, hashing, indexing and duplicate detection.
//! This crate has no UI dependency. File operations only happen when a caller asks for them.

pub mod browse;
pub mod compare;
pub mod duplicates;
pub mod hash;
pub mod image_hash;
pub mod index;
pub mod meta;
pub mod opener;
pub mod ops;
pub mod scan;
pub mod thumb;
pub mod trash_check;
pub mod verified_trash;
