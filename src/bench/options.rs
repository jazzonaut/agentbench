//! Caller-supplied benchmark options, independent of preset resource limits.

use crate::{bench::preset::Preset, live_llm};
use std::path::PathBuf;

/// Choices the caller makes about *what* to run, as distinct from the preset's resource ceilings.
#[derive(Debug, Clone)]
pub struct BenchOptions {
    pub offline: bool,
    pub elevated: bool,
    pub live_llm: bool,
    pub llm_route: live_llm::LlmRoute,
    pub llm_model: String,
    pub llm_cost_cap_usd: f64,
    pub headroom_port: u16,
    /// Where the filesystem workloads write, if not inside the target directory.
    ///
    /// The default is the target directory, because the point of the disk numbers is to describe the volume
    /// the user's code lives on. The cost of that default is that up to two gigabytes are written *inside*
    /// a repository, where an IDE indexer, a `tsc --watch` or a file-watching test runner will notice —
    /// noise the report then attributes to the disk. Pointing this at another directory on the same volume
    /// keeps the measurement and loses the watchers. On a different volume it measures a different disk,
    /// which is the caller's decision to make.
    pub scratch_dir: Option<PathBuf>,
}

impl BenchOptions {
    /// Defaults for a preset.
    ///
    /// Only `quick` omits live Claude calls by default, so a sub-minute smoke test never spends money
    /// without being asked to.
    pub fn for_preset(preset: Preset) -> Self {
        Self {
            offline: false,
            elevated: false,
            live_llm: !matches!(preset, Preset::Quick),
            llm_route: live_llm::LlmRoute::Auto,
            llm_model: "sonnet".into(),
            llm_cost_cap_usd: 5.0,
            headroom_port: 8787,
            scratch_dir: None,
        }
    }
}
