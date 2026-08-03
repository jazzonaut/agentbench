pub mod bench;
pub mod compare;
pub mod diagnosis;
pub mod experiment;
pub mod install;
pub mod integrations;
pub mod live_llm;
pub mod metrics;
pub mod model;
pub mod process_tree;
pub mod profile;
pub mod report;
pub mod status_report;
pub mod system;
pub mod tray;
pub mod ui;
pub mod watch;

/// Version of the public report format represented by the Serde types in [`model`].
///
/// Distinct from any on-disk database version: this governs JSON reports and what `compare` will
/// accept.
pub const SCHEMA_VERSION: u32 = 1;
