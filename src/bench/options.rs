//! Caller-supplied benchmark options, independent of preset resource limits.

use crate::{bench::preset::Preset, live_llm};

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
        }
    }
}
