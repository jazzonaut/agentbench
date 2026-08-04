//! What the benchmark page may ask for, and what it may not.
//!
//! This is the whole of the trust boundary between a browser and a command line. Everything here arrives
//! as JSON from a page, and everything here ends up as arguments to this program's own executable — so the
//! rule is that a [`BenchRequest`] cannot be turned into arguments at all until it has been validated, and
//! [`BenchRequest::to_args`] is only reachable through [`BenchRequest::validate`].
//!
//! Two properties are load-bearing and neither is obvious from reading the happy path:
//!
//! - **Arguments are a `Vec<String>`, never a joined string.** They go to `Command::args`, which passes
//!   them to the operating system as a vector. Nothing is parsed by a shell, so a target directory called
//!   `foo & shutdown /s` is a directory name and not two commands. This is why `install::run_detached` is
//!   not used here: it takes one argument *string*, which is the wrong shape for input that came off a
//!   network socket, quite apart from it being Windows-only.
//! - **The set of flags is closed.** The page cannot name a subcommand, cannot add a flag this module does
//!   not know, and cannot pass `--elevated`. What it chooses is which values go into a fixed template.

use crate::bench::Preset;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Longest live-LLM cost cap the dashboard will accept, in US dollars.
///
/// The command line has no ceiling beyond "positive and finite", and that is right for a person who typed
/// the number themselves. It is not right for a browser: the same request shape that spends five dollars
/// spends five thousand with two more characters, and the gate in `origin` is a mitigation rather than a
/// guarantee. A page that needs more than this has the command line, where the intent is unambiguous.
pub const MAX_COST_CAP_USD: f64 = 20.0;

/// Longest model name accepted, in characters.
const MAX_MODEL_LEN: usize = 64;

/// What the page asks for.
///
/// Every field is optional and defaults to what `bench` itself would default to, so the page can send only
/// what the user changed. `elevated` is deliberately absent rather than optional: see [`Self::validate`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchRequest {
    /// `quick`, `standard`, or `stress`.
    pub preset: Option<String>,
    /// Directory whose volume the filesystem workloads describe.
    pub target_dir: Option<String>,
    /// Where the filesystem workloads write, if not inside the target directory.
    pub scratch_dir: Option<String>,
    /// Skip the standalone HTTPS probe.
    pub offline: Option<bool>,
    /// Run paid live-Claude cases. Defaults to off for every preset, unlike the command line.
    pub live_llm: Option<bool>,
    /// Which live-Claude routes to exercise.
    pub llm_route: Option<String>,
    /// Model alias or identifier.
    pub llm_model: Option<String>,
    /// Ceiling on live-LLM spend for this run.
    pub llm_cost_cap_usd: Option<f64>,
    /// Port a Headroom proxy is expected on.
    pub headroom_port: Option<u16>,
}

/// A [`BenchRequest`] that has been checked, and the only thing that can produce arguments.
///
/// A separate type rather than a validated flag on the same one, so that "has this been checked?" is a
/// question the compiler answers instead of a comment asking the next reader to remember.
#[derive(Debug, Clone)]
pub struct ValidRequest {
    preset: Preset,
    target_dir: PathBuf,
    scratch_dir: Option<PathBuf>,
    offline: bool,
    live_llm: bool,
    llm_route: &'static str,
    llm_model: String,
    llm_cost_cap_usd: f64,
    headroom_port: u16,
}

impl BenchRequest {
    /// Check everything, or refuse with the reason.
    ///
    /// `default_target` is where a request that names no target directory runs: the data directory, which is
    /// somewhere the daemon already writes. A missing target is not defaulted to the current working
    /// directory, because a daemon started by a logon task has one nobody chose.
    pub fn validate(&self, default_target: &Path) -> Result<ValidRequest> {
        let preset = match self.preset.as_deref().unwrap_or("standard") {
            "quick" => Preset::Quick,
            "standard" => Preset::Standard,
            "stress" => Preset::Stress,
            other => bail!("unknown preset {other:?}; use quick, standard, or stress"),
        };
        // Canonicalised here rather than left to the child, so a mistyped path is a 400 on the page instead
        // of a child process that starts and immediately dies with a message only the log will hold.
        let target_dir = match self.target_dir.as_deref() {
            Some(path) => directory(path, "target_dir")?,
            None => default_target.to_path_buf(),
        };
        let scratch_dir = match self.scratch_dir.as_deref() {
            Some(path) => Some(directory(path, "scratch_dir")?),
            None => None,
        };
        let llm_route = match self.llm_route.as_deref().unwrap_or("auto") {
            "auto" => "auto",
            "direct" => "direct",
            "headroom" => "headroom",
            "both" => "both",
            other => bail!("unknown llm_route {other:?}; use auto, direct, headroom, or both"),
        };
        let llm_model = match self.llm_model.as_deref() {
            Some(model) => model_name(model)?,
            None => "sonnet".to_string(),
        };
        let llm_cost_cap_usd = match self.llm_cost_cap_usd {
            Some(value) if !value.is_finite() || value <= 0.0 => {
                bail!("llm_cost_cap_usd must be a positive finite number")
            }
            Some(value) if value > MAX_COST_CAP_USD => bail!(
                "llm_cost_cap_usd is capped at ${MAX_COST_CAP_USD} from the dashboard; \
                 use the command line for more"
            ),
            Some(value) => value,
            None => 5.0,
        };
        // Zero is what `--headroom-port` rejects on the command line too, and for the same reason: there is
        // no port zero to connect to.
        let headroom_port = match self.headroom_port {
            Some(0) => bail!("headroom_port must be between 1 and 65535"),
            Some(port) => port,
            None => 8787,
        };
        Ok(ValidRequest {
            preset,
            target_dir,
            scratch_dir,
            offline: self.offline.unwrap_or(false),
            // Off unless asked for, whatever the preset says. `BenchOptions::for_preset` turns live cases
            // *on* for standard and stress, which is right for a command someone typed and wrong for a form
            // whose default would then spend money on submission.
            live_llm: self.live_llm.unwrap_or(false),
            llm_route,
            llm_model,
            llm_cost_cap_usd,
            headroom_port,
        })
    }
}

impl ValidRequest {
    /// The full argument vector for this program's own executable.
    ///
    /// `--no-tui` is unconditional: the child's stdout is a pipe, so there is no terminal to draw into, and
    /// the `[n/8]` lines are what the run supervisor reads. `--output` is unconditional too — the daemon
    /// chooses where the report lands rather than leaving it in whatever directory the daemon was started
    /// from, which for a logon task is not a directory the user would think to look in.
    pub fn to_args(&self, report_path: &Path) -> Vec<String> {
        let mut args = vec![
            "bench".to_string(),
            "--preset".to_string(),
            self.preset.name().to_string(),
            "--target-dir".to_string(),
            self.target_dir.display().to_string(),
            "--no-tui".to_string(),
            "--output".to_string(),
            report_path.display().to_string(),
            "--llm-route".to_string(),
            self.llm_route.to_string(),
            "--llm-model".to_string(),
            self.llm_model.clone(),
            "--llm-cost-cap-usd".to_string(),
            self.llm_cost_cap_usd.to_string(),
            "--headroom-port".to_string(),
            self.headroom_port.to_string(),
        ];
        if let Some(scratch) = &self.scratch_dir {
            args.push("--scratch-dir".to_string());
            args.push(scratch.display().to_string());
        }
        if self.offline {
            args.push("--offline".to_string());
        }
        // Both spellings are sent explicitly rather than relying on the preset's default, so what the run
        // does is decided by this request and is visible in the summary the page displays.
        args.push(if self.live_llm {
            "--live-llm".to_string()
        } else {
            "--no-live-llm".to_string()
        });
        args
    }

    pub fn preset(&self) -> Preset {
        self.preset
    }

    pub fn live_llm(&self) -> bool {
        self.live_llm
    }

    /// A one-line description of what was asked for, for the page and the operational log.
    ///
    /// Names the live-LLM decision explicitly in both directions. A run that spent money and a run that did
    /// not are the two runs a reader most needs to tell apart afterwards, and "nothing was said about it" is
    /// not a state this summary should be able to describe.
    pub fn summary(&self) -> Summary {
        Summary {
            preset: self.preset.name(),
            target_dir: self.target_dir.display().to_string(),
            scratch_dir: self.scratch_dir.as_ref().map(|p| p.display().to_string()),
            offline: self.offline,
            live_llm: self.live_llm,
            llm_route: self.live_llm.then_some(self.llm_route),
            llm_model: self.live_llm.then(|| self.llm_model.clone()),
            llm_cost_cap_usd: self.live_llm.then_some(self.llm_cost_cap_usd),
        }
    }
}

/// What a run was asked to do, as the page displays it back.
#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub preset: &'static str,
    pub target_dir: String,
    pub scratch_dir: Option<String>,
    pub offline: bool,
    pub live_llm: bool,
    /// Absent when live cases were not requested, rather than describing a route nothing will use.
    pub llm_route: Option<&'static str>,
    pub llm_model: Option<String>,
    pub llm_cost_cap_usd: Option<f64>,
}

/// Resolve a path that must already be a directory.
///
/// Canonicalised, which settles what the path actually refers to before a child process is pointed at it, and
/// on Windows also settles the separator and the case. The error names the field, since a page sending two
/// paths needs to know which one was wrong.
///
/// The `\\?\` prefix `canonicalize` adds on Windows is then removed, through the same helper the installer
/// uses. It is not cosmetic here: this path is displayed in the page's run summary, where nobody should have
/// to decode it, and it is handed to the child as `--target-dir`, where a verbatim path is a worse thing to
/// pass on than the plain one it came from.
fn directory(path: &str, field: &str) -> Result<PathBuf> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        bail!("{field} is empty");
    }
    let resolved = Path::new(trimmed)
        .canonicalize()
        .map_err(|error| anyhow::anyhow!("{field} {trimmed:?} cannot be used: {error}"))?;
    if !resolved.is_dir() {
        bail!("{field} {trimmed:?} is not a directory");
    }
    Ok(crate::install::plain(resolved))
}

/// Check a model name against the characters model names actually contain.
///
/// An allow-list rather than an escape: the value becomes an argument, and while `Command::args` means it
/// cannot become a second command, a name carrying a newline would still land in the report and in the
/// operational log as two lines. Nothing legitimate needs anything outside this set.
fn model_name(model: &str) -> Result<String> {
    let trimmed = model.trim();
    if trimmed.is_empty() {
        bail!("llm_model is empty");
    }
    if trimmed.chars().count() > MAX_MODEL_LEN {
        bail!("llm_model is longer than {MAX_MODEL_LEN} characters");
    }
    if let Some(bad) = trimmed
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_')))
    {
        bail!("llm_model contains {bad:?}, which is not part of a model name");
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(json: &str) -> BenchRequest {
        serde_json::from_str(json).expect("the fixture parses")
    }

    /// An empty request is a standard run against the data directory that spends nothing.
    #[test]
    fn the_defaults_are_a_standard_run_with_no_live_cases() {
        let temp = tempfile::tempdir().unwrap();
        let valid = request("{}").validate(temp.path()).expect("valid");
        assert_eq!(valid.preset().name(), "standard");
        assert!(!valid.live_llm(), "a form submission must not spend money");

        let args = valid.to_args(Path::new("report.json"));
        assert!(args.contains(&"--no-live-llm".to_string()), "{args:?}");
        assert!(args.contains(&"--no-tui".to_string()), "{args:?}");
        assert_eq!(args[0], "bench");
    }

    /// The one difference from the command line's own defaults, and the reason for it.
    #[test]
    fn live_cases_stay_off_for_presets_that_would_enable_them_on_the_command_line() {
        let temp = tempfile::tempdir().unwrap();
        for preset in ["standard", "stress"] {
            let valid = request(&format!("{{\"preset\":\"{preset}\"}}"))
                .validate(temp.path())
                .expect("valid");
            assert!(!valid.live_llm(), "{preset}");
        }
        // And they can still be asked for.
        let valid = request("{\"preset\":\"standard\",\"live_llm\":true}")
            .validate(temp.path())
            .expect("valid");
        assert!(valid.live_llm());
        assert!(
            valid
                .to_args(Path::new("r.json"))
                .contains(&"--live-llm".to_string())
        );
    }

    #[test]
    fn an_unknown_preset_or_route_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let error = request("{\"preset\":\"turbo\"}")
            .validate(temp.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown preset"), "{error}");

        let error = request("{\"llm_route\":\"sideways\"}")
            .validate(temp.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown llm_route"), "{error}");
    }

    /// A field this module does not know is a page and a server that disagree, not a value to ignore.
    #[test]
    fn an_unknown_field_is_refused_rather_than_dropped() {
        let error = serde_json::from_str::<BenchRequest>("{\"elevated\":true}")
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown field"), "{error}");
    }

    /// The path that does not exist, and the path that is a file.
    #[test]
    fn a_target_that_is_not_a_directory_is_refused_by_name() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("report.json");
        std::fs::write(&file, b"{}").unwrap();

        let error = request(&format!(
            "{{\"target_dir\":{}}}",
            serde_json::to_string(&file.display().to_string()).unwrap()
        ))
        .validate(temp.path())
        .unwrap_err()
        .to_string();
        assert!(error.contains("target_dir"), "{error}");

        let error = request("{\"scratch_dir\":\"D:/no/such/directory/anywhere\"}")
            .validate(temp.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("scratch_dir"), "{error}");

        let error = request("{\"target_dir\":\"   \"}")
            .validate(temp.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("empty"), "{error}");
    }

    /// The money check, in both directions.
    #[test]
    fn the_cost_cap_is_bounded_above_and_must_be_a_real_number() {
        let temp = tempfile::tempdir().unwrap();
        let refuse =
            |json: &str| -> String { request(json).validate(temp.path()).unwrap_err().to_string() };
        assert!(refuse("{\"llm_cost_cap_usd\":0}").contains("positive finite"));
        assert!(refuse("{\"llm_cost_cap_usd\":-1}").contains("positive finite"));
        assert!(
            refuse(&format!(
                "{{\"llm_cost_cap_usd\":{}}}",
                MAX_COST_CAP_USD + 1.0
            ))
            .contains("capped")
        );
        // A finite number far above the cap is caught by the cap, not by the finiteness check.
        assert!(refuse("{\"llm_cost_cap_usd\":1e300}").contains("capped"));
        // An infinity cannot be written in JSON at all, so serde refuses it before validation sees it. The
        // `is_finite` guard therefore protects a caller that is not this endpoint — which is worth keeping
        // and worth recording as unreachable from here, rather than testing through a door that is shut.
        assert!(
            serde_json::from_str::<BenchRequest>("{\"llm_cost_cap_usd\":1e999}").is_err(),
            "JSON has no infinity for the finiteness check to catch"
        );
        // The ceiling itself is permitted.
        assert!(
            request(&format!("{{\"llm_cost_cap_usd\":{MAX_COST_CAP_USD}}}"))
                .validate(temp.path())
                .is_ok()
        );
    }

    /// A model name is an argument, so it is checked against what model names are made of.
    #[test]
    fn a_model_name_outside_the_allowed_characters_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        for model in [
            "",
            "  ",
            "sonnet\nrm -rf",
            "sonnet; whoami",
            "sonnet --elevated",
            "so\"nnet",
        ] {
            let json = format!(
                "{{\"llm_model\":{}}}",
                serde_json::to_string(model).unwrap()
            );
            assert!(
                request(&json).validate(temp.path()).is_err(),
                "{model:?} should be refused"
            );
        }
        for model in [
            "sonnet",
            "claude-opus-4-1",
            "claude-sonnet-4-5-20250929",
            "a_b.c-1",
        ] {
            let json = format!(
                "{{\"llm_model\":{}}}",
                serde_json::to_string(model).unwrap()
            );
            assert!(
                request(&json).validate(temp.path()).is_ok(),
                "{model:?} should be accepted"
            );
        }
        let long = "a".repeat(MAX_MODEL_LEN + 1);
        assert!(
            request(&format!("{{\"llm_model\":\"{long}\"}}"))
                .validate(temp.path())
                .is_err()
        );
    }

    /// A resolved directory must not carry the prefix `canonicalize` adds on Windows.
    ///
    /// It reaches two places that cannot use it: the run summary the page prints, and the child's
    /// `--target-dir`. The whole path is compared rather than only the prefix, because a helper that stripped
    /// too much would pass a check that only looked at the start.
    #[test]
    fn a_resolved_directory_is_a_path_a_reader_can_use() {
        let temp = tempfile::tempdir().unwrap();
        let json = format!(
            "{{\"target_dir\":{}}}",
            serde_json::to_string(&temp.path().display().to_string()).unwrap()
        );
        let summary = request(&json)
            .validate(temp.path())
            .expect("valid")
            .summary();
        assert!(
            !summary.target_dir.starts_with(r"\\?\"),
            "the verbatim prefix reached the summary: {}",
            summary.target_dir
        );
        // And it still names the directory that was asked for.
        assert_eq!(
            std::fs::canonicalize(&summary.target_dir).unwrap(),
            std::fs::canonicalize(temp.path()).unwrap()
        );
    }

    #[test]
    fn port_zero_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let error = request("{\"headroom_port\":0}")
            .validate(temp.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("headroom_port"), "{error}");
    }

    /// A path with the shell metacharacters a filename may legally contain stays exactly one argument.
    ///
    /// The property that makes the whole module safe. Nothing joins these into a command line, so there is
    /// no quoting to get wrong and nothing for a shell to reinterpret. The name deliberately holds
    /// everything a `cmd.exe` or a `sh` would act on and a filesystem still permits — a double quote is
    /// absent because Windows refuses to create it, not because it would be safe.
    #[test]
    fn a_hostile_directory_name_remains_exactly_one_argument() {
        let temp = tempfile::tempdir().unwrap();
        let awkward = temp.path().join("a dir & echo %PATH% ^ $(id) 'quoted'");
        std::fs::create_dir(&awkward).unwrap();
        let json = format!(
            "{{\"target_dir\":{}}}",
            serde_json::to_string(&awkward.display().to_string()).unwrap()
        );
        let args = request(&json)
            .validate(temp.path())
            .expect("an awkward name is still a directory")
            .to_args(Path::new("r.json"));

        let index = args
            .iter()
            .position(|arg| arg == "--target-dir")
            .expect("the flag is present");
        let value = &args[index + 1];
        assert!(value.contains('&'), "{value}");
        assert!(value.contains("%PATH%"), "{value}");
        assert!(value.contains("$(id)"), "{value}");
        assert_eq!(
            args.iter().filter(|arg| arg.contains('&')).count(),
            1,
            "the name must not have been split: {args:?}"
        );
    }

    /// The summary describes the run rather than the request's defaults.
    #[test]
    fn the_summary_omits_live_llm_detail_when_no_live_cases_will_run() {
        let temp = tempfile::tempdir().unwrap();
        let quiet = request("{}").validate(temp.path()).unwrap().summary();
        assert!(!quiet.live_llm);
        assert!(quiet.llm_route.is_none());
        assert!(quiet.llm_model.is_none());
        assert!(quiet.llm_cost_cap_usd.is_none());

        let spending = request("{\"live_llm\":true,\"llm_route\":\"direct\"}")
            .validate(temp.path())
            .unwrap()
            .summary();
        assert!(spending.live_llm);
        assert_eq!(spending.llm_route, Some("direct"));
        assert_eq!(spending.llm_cost_cap_usd, Some(5.0));
    }
}
