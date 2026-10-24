//! Storage diagnostics: OS-reported space, APFS topology and snapshots, permission
//! coverage and common storage consumers. Everything here is read-only.

pub mod access;
pub mod apfs;
pub mod apps;
pub mod classify;
pub mod command;
pub mod doctor;
pub mod openfiles;
pub mod suspects;
pub mod usage;
