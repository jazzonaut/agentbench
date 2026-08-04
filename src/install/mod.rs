//! Making this executable reachable: a durable location, `PATH`, and starting at login.
//!
//! Three concerns in one module because they share a premise. A `PATH` entry and a scheduled task both
//! record where the executable *is*, and on a development machine that is `target\release\agentbench.exe`
//! — a path `cargo clean` deletes. Wire either of them to that and it works until the next clean, then
//! fails silently: `agentbench` becomes "command not found", and the login task starts nothing at all with
//! no error anybody sees. So the durable location comes first and the other two point at it.
//!
//! Platform-specific work is confined to [`imp`]. Everything that can be decided without touching the
//! registry or the task scheduler — in particular the `PATH` string edits, which are where the real bugs
//! live — is in this file and therefore tested on every platform CI runs, not only on Windows.

mod taskxml;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as imp;

#[cfg(not(windows))]
mod fallback;
#[cfg(not(windows))]
use fallback as imp;

use anyhow::{Context, Result};
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    time::Duration,
};

/// Directory name used inside the per-user programs directory.
const PROGRAM_DIR: &str = "AgentBench";

/// Name of the scheduled task that starts collection at login.
pub const TASK_NAME: &str = "AgentBench dashboard";

/// Suffix on the file name of the windowless build that shows a tray icon.
///
/// Shared rather than duplicated because it is read in both directions: [`build_for`] uses it to name the
/// executable a tray choice means, and the task reader uses it to recognise which one a registered task
/// starts. Two spellings of this would round-trip wrongly and look like the setting had not been saved.
pub const TRAY_SUFFIX: &str = "-tray";

/// Default delay between logging in and starting collection.
///
/// Not politeness: probes that fire during the login storm are counted as contended and drop out of the
/// baseline entirely, so a daemon that starts immediately collects samples it cannot later compare. Two
/// minutes is past the worst of the indexer and antivirus activity on the machines this was built for.
pub const DEFAULT_DELAY: Duration = Duration::from_secs(120);

/// Whether something is possible here, and why not when it is not.
///
/// Reported rather than hidden, following [`Capability`] in the daemon's platform layer: a screen that
/// simply omitted the startup section on Linux would look like a missing feature rather than a platform
/// that has not been taught this yet.
///
/// [`Capability`]: crate::watch::platform::Capability
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Support {
    Yes,
    No(&'static str),
}

impl Support {
    pub fn is_supported(&self) -> bool {
        matches!(self, Self::Yes)
    }

    pub fn reason(&self) -> Option<&'static str> {
        match self {
            Self::Yes => None,
            Self::No(reason) => Some(reason),
        }
    }
}

/// Whether the running executable is somewhere that will still exist tomorrow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// Running from the per-user install directory.
    Installed(PathBuf),
    /// Running out of a Cargo build directory, which is not a durable path.
    BuildTree(PathBuf),
    /// Somewhere else, which is taken to be deliberate.
    Elsewhere(PathBuf),
}

impl Origin {
    pub fn path(&self) -> &Path {
        match self {
            Self::Installed(path) | Self::BuildTree(path) | Self::Elsewhere(path) => path,
        }
    }

    /// Whether it is safe to record this path in `PATH` or a scheduled task.
    pub fn is_durable(&self) -> bool {
        !matches!(self, Self::BuildTree(_))
    }
}

/// Where a durable copy of the executable belongs.
pub fn install_dir() -> Result<PathBuf> {
    Ok(imp::programs_dir()?.join(PROGRAM_DIR))
}

/// Classify where the running executable is.
pub fn origin() -> Result<Origin> {
    let exe = std::env::current_exe().context("locate the running executable")?;
    // Canonicalised so a path reached through a symlink or a relative invocation compares equal to the
    // install directory. A failure is survivable: the uncanonicalised path is still worth classifying.
    // Both sides are put back into plain form afterwards, and both have to be: the comparison below is
    // between a path and a parent, and one of them still carrying `\\?\` would never match the other.
    let exe = plain(exe.canonicalize().unwrap_or(exe));
    let installed = install_dir()
        .ok()
        .and_then(|dir| dir.canonicalize().ok().map(plain).or(Some(dir)));
    if let Some(installed) = installed
        && exe.parent() == Some(installed.as_path())
    {
        return Ok(Origin::Installed(exe));
    }
    if in_build_tree(&exe) {
        return Ok(Origin::BuildTree(exe));
    }
    Ok(Origin::Elsewhere(exe))
}

/// A path without the `\\?\` extended-length prefix Windows' `canonicalize` adds.
///
/// The prefix means something only to the file APIs, and most things downstream of a `canonicalize` are not
/// one: it is written into a scheduled task, where it survives but is no longer the path anybody typed; it
/// is printed on the control centre, where `\\?\C:\Users\...` is something the reader has to decode; and it
/// is handed to the benchmark page, which both displays it and passes it to a child process as
/// `--target-dir`.
///
/// Public for that last caller. The dashboard canonicalises a directory a browser sent, for the same reason
/// [`origin`] does — to settle what the path actually refers to before anything acts on it — and it has the
/// same reason to undo the prefix afterwards.
///
/// Not behind `#[cfg(windows)]` — this is string work, so it is checked by CI on every platform, and no
/// path on the others can begin with the prefix it looks for.
pub fn plain(path: PathBuf) -> PathBuf {
    // A UNC path canonicalises to `\\?\UNC\server\share`, where dropping the prefix alone would leave
    // `UNC\server\share`: a relative path, and a wrong one.
    let stripped = path.to_str().and_then(|text| {
        text.strip_prefix(r"\\?\UNC\")
            .map(|rest| format!(r"\\{rest}"))
            .or_else(|| text.strip_prefix(r"\\?\").map(str::to_owned))
    });
    match stripped {
        Some(text) => PathBuf::from(text),
        None => path,
    }
}

/// The executable a tray choice means, beside `program`.
///
/// The two builds live in the same directory and differ only by [`TRAY_SUFFIX`], so this is a rename rather
/// than a lookup. It has to be applied both ways round: the control centre may be running from either build,
/// and the answer depends on what was asked for, not on which one is asking.
pub fn build_for(program: &Path, tray: bool) -> PathBuf {
    let Some(stem) = program.file_stem().and_then(OsStr::to_str) else {
        return program.to_path_buf();
    };
    let base = stem.strip_suffix(TRAY_SUFFIX).unwrap_or(stem);
    let mut name = String::from(base);
    if tray {
        name.push_str(TRAY_SUFFIX);
    }
    if let Some(extension) = program.extension().and_then(OsStr::to_str) {
        name.push('.');
        name.push_str(extension);
    }
    program.with_file_name(name)
}

/// Whether `program` is the windowless build.
pub fn is_tray_build(program: &Path) -> bool {
    program
        .file_stem()
        .and_then(OsStr::to_str)
        .is_some_and(|stem| stem.ends_with(TRAY_SUFFIX))
}

/// Whether `exe` sits under a Cargo build directory.
///
/// Recognises `target/debug` and `target/release` as well as the cross-compilation shape
/// `target/<triple>/release`, because a path is only durable if nothing routinely deletes it and
/// `cargo clean` deletes all of them.
fn in_build_tree(exe: &Path) -> bool {
    let mut components = exe
        .components()
        .map(|component| component.as_os_str().to_ascii_lowercase())
        .collect::<Vec<_>>();
    components.pop();
    let profile_at = components.iter().rposition(|component| {
        component == OsStr::new("debug") || component == OsStr::new("release")
    });
    match profile_at {
        // `target` is either the parent of the profile directory or its grandparent, the latter when a
        // target triple sits between them.
        Some(index) if index > 0 => components[..index]
            .iter()
            .rev()
            .take(2)
            .any(|component| component == OsStr::new("target")),
        _ => false,
    }
}

/// Copy the running executable into the install directory, replacing any previous copy.
///
/// Returns the installed path. Copying rather than moving, because the source may be a build output the
/// user is still iterating on, and because moving the file a running process was started from is a
/// different proposition on every platform.
pub fn install() -> Result<PathBuf> {
    let source = std::env::current_exe().context("locate the running executable")?;
    let directory = install_dir()?;
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("create {}", directory.display()))?;
    let installed = copy_into(&source, &directory)?;
    // The other build too, when it is there to copy. "Start in tray" registers a task pointing at
    // `agentbench-tray.exe`, and installing only the executable that happens to be running would leave that
    // row able to name a file the install never produced — a task that starts nothing, once per login,
    // silently. Absent is not an error: a `cargo build` of one target, or a hand-copied executable, is a
    // reasonable thing to be running from.
    let counterpart = build_for(&source, !is_tray_build(&source));
    if counterpart.is_file() {
        copy_into(&counterpart, &directory)?;
    }
    Ok(installed)
}

/// Copy one executable into `directory`, returning where it landed.
fn copy_into(source: &Path, directory: &Path) -> Result<PathBuf> {
    let file_name = source
        .file_name()
        .with_context(|| format!("{} has no file name", source.display()))?;
    let destination = directory.join(file_name);
    if destination == *source {
        return Ok(destination);
    }
    // Written beside the target and renamed over it, so a copy interrupted halfway does not leave a
    // truncated executable that `PATH` now points at.
    let staged = destination.with_extension("new");
    std::fs::copy(source, &staged)
        .with_context(|| format!("copy {} to {}", source.display(), staged.display()))?;
    std::fs::rename(&staged, &destination)
        .with_context(|| format!("replace {}", destination.display()))?;
    Ok(destination)
}

/// Whether the user's `PATH` can be edited here.
pub fn path_support() -> Support {
    imp::path_support()
}

/// Whether `directory` is already on the user's `PATH`.
pub fn on_path(directory: &Path) -> Result<bool> {
    Ok(path_contains(&imp::read_user_path()?, directory))
}

/// Add `directory` to the user's `PATH`, if it is not there already.
///
/// Returns whether anything changed.
pub fn add_to_path(directory: &Path) -> Result<bool> {
    let current = imp::read_user_path()?;
    match path_with(&current, directory) {
        Some(updated) => {
            imp::write_user_path(&updated)?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Remove `directory` from the user's `PATH`, if it is there.
///
/// Returns whether anything changed.
pub fn remove_from_path(directory: &Path) -> Result<bool> {
    let current = imp::read_user_path()?;
    match path_without(&current, directory) {
        Some(updated) => {
            imp::write_user_path(&updated)?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// What to run at login, and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Autostart {
    /// The executable the task launches.
    pub program: PathBuf,
    /// Whether it starts without a console window and shows a tray icon instead.
    pub tray: bool,
    /// How long after login to wait before starting.
    pub delay: Duration,
}

/// Whether a login task is registered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutostartState {
    /// Not possible here.
    Unsupported(&'static str),
    /// Possible, and not currently registered.
    Absent,
    /// Registered, as described.
    Present(Autostart),
}

impl AutostartState {
    pub fn is_enabled(&self) -> bool {
        matches!(self, Self::Present(_))
    }
}

/// Whether a login task can be registered here.
pub fn autostart_support() -> Support {
    imp::autostart_support()
}

/// Read the registered login task, if any.
pub fn autostart_state() -> Result<AutostartState> {
    imp::autostart_state()
}

/// Register or replace the login task.
pub fn enable_autostart(autostart: &Autostart) -> Result<()> {
    imp::enable_autostart(autostart)
}

/// Remove the login task, if it exists. Returns whether anything was removed.
pub fn disable_autostart() -> Result<bool> {
    imp::disable_autostart()
}

/// Open a URL or file with whatever the user has associated with it.
///
/// Used for the dashboard. Worth noting that this is a plain shell open with no elevation anywhere near
/// it: the daemon runs unelevated by design, so there is no risk here of handing the browser an elevated
/// token — which both Chrome and Edge refuse to run under, and which would make "open dashboard" appear
/// broken for reasons nobody could guess from the button.
pub fn open(target: &str) -> Result<()> {
    imp::open(target)
}

/// Launch a program with arguments in its own window, unelevated.
///
/// Used for a benchmark started from the control centre. It needs its own console: the control centre owns
/// this one and is drawing into the alternate screen buffer.
pub fn run_detached(program: &Path, arguments: &str) -> Result<()> {
    imp::run_detached(program, arguments)
}

/// Re-launch a program with an elevation prompt, returning once the prompt is answered.
///
/// Nothing in this design elevates at login — Windows refuses elevation prompts for `Run`-key and
/// Startup-folder entries, and the logon task deliberately runs at least privilege — so a prompt only ever
/// appears where the user asked for the thing that needs it, which is what they can connect it to. Two
/// places do: the elevated benchmark, and removing a logon task left by an elevated session of an older
/// version, which belongs to Administrators and cannot be deleted any other way.
pub fn run_elevated(program: &Path, arguments: &str) -> Result<()> {
    imp::run_elevated(program, arguments)
}

/// Whether this process is already running with an elevated token.
///
/// Answered by the daemon's platform layer rather than a second implementation, since the question and the
/// call are identical and two of them would be two things to keep in step.
pub fn is_elevated() -> bool {
    crate::watch::platform::is_elevated()
}

/// Compare two directories the way the host filesystem does.
///
/// Case-insensitive and separator-insensitive, with trailing separators ignored. All three matter for the
/// question actually being asked — "is this directory already on `PATH`?" — because the entry the user or
/// an installer wrote will not necessarily be spelled the way this program spells it.
fn same_directory(left: &str, right: &str) -> bool {
    fn normalise(value: &str) -> String {
        value
            .trim()
            .trim_end_matches(['\\', '/'])
            .replace('\\', "/")
            .to_lowercase()
    }
    !left.trim().is_empty() && normalise(left) == normalise(right)
}

/// Whether `directory` appears in a `PATH` value.
fn path_contains(path_value: &str, directory: &Path) -> bool {
    let directory = directory.to_string_lossy();
    path_value
        .split(';')
        .any(|entry| same_directory(entry, &directory))
}

/// The value with `directory` appended, or `None` if it is already present.
///
/// Appended rather than prepended: putting a directory at the front of `PATH` changes which build of every
/// other tool the user's shell resolves, which is not a side effect an "add to PATH" checkbox should have.
fn path_with(path_value: &str, directory: &Path) -> Option<String> {
    if path_contains(path_value, directory) {
        return None;
    }
    let directory = directory.to_string_lossy();
    let trimmed = path_value.trim_end_matches(';');
    if trimmed.is_empty() {
        Some(directory.into_owned())
    } else {
        Some(format!("{trimmed};{directory}"))
    }
}

/// The value with every occurrence of `directory` removed, or `None` if there were none.
fn path_without(path_value: &str, directory: &Path) -> Option<String> {
    if !path_contains(path_value, directory) {
        return None;
    }
    let directory = directory.to_string_lossy();
    let kept = path_value
        .split(';')
        .filter(|entry| !same_directory(entry, &directory))
        // Empty entries are dropped along the way. An empty `PATH` entry means "the current directory" on
        // Windows, which is a long-standing security wart, and removing one while editing is a small
        // improvement rather than a surprise.
        .filter(|entry| !entry.trim().is_empty())
        .collect::<Vec<_>>();
    Some(kept.join(";"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(value: &str) -> PathBuf {
        PathBuf::from(value)
    }

    /// The prefix `canonicalize` adds, which has no business in a task definition or on the screen.
    #[test]
    fn the_extended_length_prefix_is_dropped() {
        assert_eq!(
            plain(dir(
                r"\\?\C:\Users\user\AppData\Local\Programs\AgentBench\agentbench.exe"
            )),
            dir(r"C:\Users\user\AppData\Local\Programs\AgentBench\agentbench.exe")
        );
    }

    /// Dropping the prefix alone would turn a share into a relative path.
    #[test]
    fn a_canonicalised_unc_path_keeps_its_two_leading_separators() {
        assert_eq!(
            plain(dir(r"\\?\UNC\build\tools\agentbench.exe")),
            dir(r"\\build\tools\agentbench.exe")
        );
    }

    #[test]
    fn a_path_without_the_prefix_is_left_alone() {
        for path in [r"C:\Programs\agentbench.exe", "/usr/local/bin/agentbench"] {
            assert_eq!(plain(dir(path)), dir(path));
        }
    }

    #[test]
    fn the_tray_build_is_named_beside_the_console_one() {
        let console = dir(r"C:\Programs\AgentBench\agentbench.exe");
        let tray = dir(r"C:\Programs\AgentBench\agentbench-tray.exe");
        assert_eq!(build_for(&console, true), tray);
        assert_eq!(build_for(&tray, false), console);
    }

    /// Asking for the build you are already running has to be the identity, or installing would copy a file
    /// onto itself and the task would name something with two suffixes.
    #[test]
    fn asking_for_the_build_already_in_hand_changes_nothing() {
        let console = dir(r"C:\Programs\AgentBench\agentbench.exe");
        let tray = dir(r"C:\Programs\AgentBench\agentbench-tray.exe");
        assert_eq!(build_for(&console, false), console);
        assert_eq!(build_for(&tray, true), tray);
    }

    /// No extension is the shape on every platform but this feature's own, and it must not gain one.
    #[test]
    fn a_program_with_no_extension_keeps_none() {
        assert_eq!(
            build_for(&dir("/usr/local/bin/agentbench"), true),
            dir("/usr/local/bin/agentbench-tray")
        );
    }

    #[test]
    fn the_tray_build_is_recognised_by_its_suffix() {
        assert!(is_tray_build(&dir(r"C:\Programs\agentbench-tray.exe")));
        assert!(!is_tray_build(&dir(r"C:\Programs\agentbench.exe")));
    }

    /// What the writing and reading sides have to agree on: the suffix goes on, and comes back off.
    #[test]
    fn the_tray_choice_round_trips_through_the_program_name() {
        let program = dir(r"C:\Programs\AgentBench\agentbench.exe");
        for tray in [true, false] {
            assert_eq!(is_tray_build(&build_for(&program, tray)), tray);
        }
    }

    /// Assemble a path from components rather than writing it as a string, so these run the same on every
    /// platform. A backslash is not a separator on Unix, so `r"D:\...\target\release\x.exe"` arrives there
    /// as one long component and every assertion about its shape silently tests nothing — which is exactly
    /// how the first version of these two tests passed on Windows and failed on macOS.
    fn exe(parts: &[&str]) -> PathBuf {
        parts.iter().collect()
    }

    #[test]
    fn a_directory_already_present_is_recognised() {
        let path = r"C:\Windows;C:\Users\x\AppData\Local\Programs\AgentBench;C:\Tools";
        assert!(path_contains(
            path,
            &dir(r"C:\Users\x\AppData\Local\Programs\AgentBench")
        ));
    }

    /// The three spellings a real `PATH` uses for the same directory.
    #[test]
    fn presence_ignores_case_separators_and_trailing_slashes() {
        let target = dir(r"C:\Program Files\AgentBench");
        for entry in [
            r"C:\Program Files\AgentBench",
            r"c:\program files\agentbench",
            r"C:/Program Files/AgentBench",
            r"C:\Program Files\AgentBench\",
            r"  C:\Program Files\AgentBench  ",
        ] {
            assert!(
                path_contains(&format!("C:\\Windows;{entry};C:\\Tools"), &target),
                "{entry:?} should be recognised"
            );
        }
    }

    /// The idempotency that stops a checkbox appending a duplicate every time it is ticked.
    #[test]
    fn adding_a_directory_twice_changes_nothing_the_second_time() {
        let target = dir(r"C:\Tools\AgentBench");
        let first =
            path_with(r"C:\Windows", &target).expect("the first add should change the value");
        assert_eq!(first, r"C:\Windows;C:\Tools\AgentBench");
        assert_eq!(path_with(&first, &target), None);
    }

    #[test]
    fn adding_to_an_empty_path_does_not_leave_a_leading_separator() {
        let target = dir(r"C:\Tools\AgentBench");
        assert_eq!(path_with("", &target).unwrap(), r"C:\Tools\AgentBench");
        assert_eq!(path_with(";", &target).unwrap(), r"C:\Tools\AgentBench");
    }

    #[test]
    fn removing_a_directory_leaves_the_others_in_order() {
        let target = dir(r"C:\Tools\AgentBench");
        let path = r"C:\Windows;C:\Tools\AgentBench;C:\Tools";
        assert_eq!(path_without(path, &target).unwrap(), r"C:\Windows;C:\Tools");
    }

    #[test]
    fn removing_a_directory_that_is_not_there_changes_nothing() {
        assert_eq!(
            path_without(r"C:\Windows;C:\Tools", &dir(r"C:\Tools\AgentBench")),
            None
        );
    }

    /// A duplicate that a previous buggy version might have left behind should all go.
    #[test]
    fn removing_a_directory_removes_every_copy_of_it() {
        let target = dir(r"C:\Tools\AgentBench");
        let path = r"C:\Tools\AgentBench;C:\Windows;c:\tools\agentbench\";
        assert_eq!(path_without(path, &target).unwrap(), r"C:\Windows");
    }

    /// An entry that is only whitespace must not be mistaken for a match against anything.
    #[test]
    fn an_empty_path_entry_matches_nothing() {
        assert!(!path_contains(r"C:\Windows;;C:\Tools", &dir("")));
        assert!(!same_directory("", ""));
        assert!(!same_directory("   ", ""));
    }

    #[test]
    fn a_build_directory_is_recognised_as_not_durable() {
        for parts in [
            &["Stuff", "AgentBench", "target", "release", "agentbench"][..],
            &["Stuff", "AgentBench", "target", "debug", "agentbench"][..],
            &[
                "Stuff",
                "AgentBench",
                "target",
                "x86_64-pc-windows-msvc",
                "release",
                "agentbench",
            ][..],
            &["home", "x", "agentbench", "target", "release", "agentbench"][..],
        ] {
            let path = exe(parts);
            assert!(
                in_build_tree(&path),
                "{} should be a build path",
                path.display()
            );
        }
    }

    #[test]
    fn an_installed_directory_is_not_mistaken_for_a_build_one() {
        for parts in [
            &["AppData", "Local", "Programs", "AgentBench", "agentbench"][..],
            &["Program Files", "AgentBench", "agentbench"][..],
            // A directory that merely happens to be called "release" is not a Cargo profile directory.
            &["Tools", "release", "agentbench"][..],
            &["usr", "local", "bin", "agentbench"][..],
            // A profile directory with nothing above it has no room for a `target`.
            &["release", "agentbench"][..],
        ] {
            let path = exe(parts);
            assert!(
                !in_build_tree(&path),
                "{} should not be a build path",
                path.display()
            );
        }
    }

    #[test]
    fn an_origin_reports_whether_it_is_safe_to_record() {
        assert!(!Origin::BuildTree(dir("t")).is_durable());
        assert!(Origin::Installed(dir("i")).is_durable());
        assert!(Origin::Elsewhere(dir("e")).is_durable());
    }

    /// Whatever this platform supports, asking must not panic and must explain a refusal.
    #[test]
    fn support_is_reported_with_a_reason_when_absent() {
        for support in [path_support(), autostart_support()] {
            assert_eq!(support.is_supported(), support.reason().is_none());
        }
    }
}
