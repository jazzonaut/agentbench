//! Reading and writing the pieces of a Task Scheduler definition, without touching Task Scheduler.
//!
//! Deliberately not behind `#[cfg(windows)]`. Parsing is where the bugs in this feature will be, and the
//! project's lint job runs on Linux — so anything hidden behind that attribute is code CI never checks. The
//! process spawning and the UTF-16 decoding have to be Windows-only; recognising a `<Delay>PT2M</Delay>`
//! does not, so it is tested everywhere instead.
//!
//! No XML crate. The only documents this reads are ones `schtasks /Query /XML` produced moments earlier
//! from a task this program registered, and the only one it writes is [`document`] — so the general problem
//! of XML does not arise, and a dependency to solve a problem the input cannot pose is a dependency that
//! earns nothing.

// The cost of that choice: on a platform with no Task Scheduler nothing calls any of this, and `-D warnings`
// reads "unused" as "broken". Silenced here rather than fixed with `#[cfg(windows)]`, because compiling on
// every platform is the whole point of the module and the tests below are the ones doing the checking.
#![cfg_attr(not(windows), allow(dead_code))]

use std::time::Duration;

/// Longest delay this writes, in minutes.
///
/// Not a Task Scheduler limit — `<Delay>` is an `xs:duration` and would take far more. The cap is what
/// [`parse_delay`] can read back, and the registered task is the only record of this setting: a delay the
/// screen could no longer show after registering it is one the user has lost sight of.
const MAX_DELAY_MINUTES: u64 = 9_999;

/// The text between the first `<tag>` and the next `</tag>`.
///
/// Returns the inner slice so calls can be nested: find the trigger, then the delay inside it, rather than
/// finding whichever delay happens to appear first in the whole document.
pub(super) fn element<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(&xml[start..end])
}

/// An ISO 8601 duration in the subset Task Scheduler writes: `PT2M`, `PT30S`, `PT1H30M`.
///
/// Anything outside that subset returns `None` rather than a guess. A delay this cannot read is reported to
/// the user as an unrecognised task, which is recoverable; a delay silently read as zero would move
/// collection into the login storm and quietly cost them their comparable samples.
pub(super) fn parse_delay(text: &str) -> Option<Duration> {
    let rest = text.trim().strip_prefix("PT")?;
    if rest.is_empty() {
        return None;
    }
    let mut seconds: u64 = 0;
    let mut digits = String::new();
    for character in rest.chars() {
        if character.is_ascii_digit() {
            digits.push(character);
            continue;
        }
        let amount: u64 = digits.parse().ok()?;
        digits.clear();
        let multiplier = match character {
            'H' => 3_600,
            'M' => 60,
            'S' => 1,
            _ => return None,
        };
        seconds = seconds.checked_add(amount.checked_mul(multiplier)?)?;
    }
    // Trailing digits with no unit is malformed, not zero.
    if digits.is_empty() {
        Some(Duration::from_secs(seconds))
    } else {
        None
    }
}

/// The `<Delay>` a logon trigger takes, or `None` when there is no delay at all.
///
/// Zero is expressed by leaving the element out rather than by writing `PT0S`, because a trigger with no
/// `<Delay>` is what Task Scheduler itself writes for "start immediately" — and so it is the shape
/// [`parse_delay`]'s caller already reads as zero.
///
/// Clamped rather than refused: the value comes from a screen where the user picked a number of minutes, and
/// the largest expressible delay is a better answer to "wait a very long time" than an error is.
pub(super) fn delay_element(delay: Duration) -> Option<String> {
    let total = delay.as_secs().min(MAX_DELAY_MINUTES * 60 + 59);
    if total == 0 {
        return None;
    }
    let mut text = String::from("PT");
    for (amount, unit) in [
        (total / 3_600, 'H'),
        (total % 3_600 / 60, 'M'),
        (total % 60, 'S'),
    ] {
        if amount > 0 {
            text.push_str(&format!("{amount}{unit}"));
        }
    }
    Some(text)
}

/// A complete definition for `schtasks /Create /XML`.
///
/// Written by hand for the same reason the reading side parses by hand: there is one document shape and this
/// module knows all of it.
///
/// The trigger names a user, and that is the entire reason this function exists rather than a
/// `/Create /SC ONLOGON` command line. `schtasks` has no way to scope a logon trigger, so it writes one with
/// no `<UserId>` — a trigger that fires at *any* user's logon, which only an administrator may register.
/// Unelevated that fails outright with "Access is denied"; elevated it succeeds and produces a task whose
/// security descriptor grants Administrators full control and the registering user only read access, so the
/// same program can then see the task it made but never remove it.
///
/// Element order is not decoration. The schema declares sequences — `<Delay>` before `<UserId>` inside the
/// trigger — and Task Scheduler reports a document that violates one as `The system cannot find the file
/// specified`: an error about the file, for a fault in its contents.
pub(super) fn document(
    program: &str,
    arguments: Option<&str>,
    user: &str,
    delay: Duration,
) -> String {
    let user = escape(user);
    // Both optional elements carry their own line ending, so an absent one leaves no blank line behind.
    let delay = delay_element(delay)
        .map(|value| format!("      <Delay>{value}</Delay>\n"))
        .unwrap_or_default();
    let arguments = arguments
        .map(|value| format!("      <Arguments>{}</Arguments>\n", escape(value)))
        .unwrap_or_default();
    // `ExecutionTimeLimit` is the one setting here that is not a `schtasks` default: without it a task is
    // stopped after three days, which for a daemon whose whole purpose is a long baseline means the
    // collection quietly ends on the fourth. The battery settings are `schtasks` defaults inverted for the
    // same reason — it registers tasks that refuse to start on battery and are killed when a laptop is
    // unplugged, and a metrics daemon that stops when the power does measures the wrong machine.
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
{delay}      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{program}</Command>
{arguments}    </Exec>
  </Actions>
</Task>
"#,
        program = escape(program),
    )
}

/// XML element text.
///
/// Three characters rather than five: this only ever writes text between tags, never an attribute value, so
/// quotes need no escaping. An installation path containing `&` is the case that actually occurs.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from what `schtasks /Query /TN "AgentBench dashboard" /XML ONE` actually prints, with the
    /// account and install path replaced by placeholders.
    const TASK_XML: &str = r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Date>2026-08-03T10:00:00</Date>
    <Author>DESKTOP\user</Author>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <Delay>PT2M</Delay>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>DESKTOP\user</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Actions Context="Author">
    <Exec>
      <Command>C:\Users\user\AppData\Local\Programs\AgentBench\agentbench.exe</Command>
      <Arguments>dashboard</Arguments>
    </Exec>
  </Actions>
</Task>"#;

    #[test]
    fn the_command_and_arguments_are_read_from_the_exec_block() {
        let exec = element(TASK_XML, "Exec").expect("an Exec block");
        assert_eq!(
            element(exec, "Command").unwrap(),
            r"C:\Users\user\AppData\Local\Programs\AgentBench\agentbench.exe"
        );
        assert_eq!(element(exec, "Arguments").unwrap(), "dashboard");
    }

    /// The reason `element` returns the inner slice: `<Enabled>` appears in more than one block, and the
    /// delay wanted is the trigger's.
    #[test]
    fn nesting_finds_the_delay_belonging_to_the_trigger() {
        let trigger = element(TASK_XML, "LogonTrigger").expect("a LogonTrigger");
        assert_eq!(
            parse_delay(element(trigger, "Delay").unwrap()),
            Some(Duration::from_secs(120))
        );
    }

    #[test]
    fn the_run_level_is_read_from_the_principal() {
        let principal = element(TASK_XML, "Principals").expect("a Principals block");
        assert_eq!(element(principal, "RunLevel").unwrap(), "LeastPrivilege");
    }

    #[test]
    fn a_missing_element_is_absent_rather_than_empty() {
        assert_eq!(element(TASK_XML, "NoSuchElement"), None);
        let exec = element(TASK_XML, "Exec").unwrap();
        assert_eq!(element(exec, "WorkingDirectory"), None);
    }

    /// An unterminated tag must not be read as everything after it.
    #[test]
    fn an_unclosed_element_is_absent() {
        assert_eq!(element("<Command>C:\\x.exe", "Command"), None);
    }

    #[test]
    fn delays_parse_across_the_units_task_scheduler_uses() {
        assert_eq!(parse_delay("PT30S"), Some(Duration::from_secs(30)));
        assert_eq!(parse_delay("PT2M"), Some(Duration::from_secs(120)));
        assert_eq!(parse_delay("PT1H"), Some(Duration::from_secs(3_600)));
        assert_eq!(parse_delay("PT1H30M"), Some(Duration::from_secs(5_400)));
        assert_eq!(parse_delay(" PT15M "), Some(Duration::from_secs(900)));
    }

    /// Every one of these must be `None`, not a zero delay — see the function's own comment for why.
    #[test]
    fn a_malformed_delay_is_refused_rather_than_read_as_zero() {
        for text in ["", "PT", "2M", "PT2X", "PT2", "P2D", "PTM"] {
            assert_eq!(parse_delay(text), None, "{text:?} should not parse");
        }
    }

    #[test]
    fn a_delay_element_names_only_the_units_it_needs() {
        assert_eq!(delay_element(Duration::from_secs(30)).unwrap(), "PT30S");
        assert_eq!(delay_element(Duration::from_secs(120)).unwrap(), "PT2M");
        assert_eq!(delay_element(Duration::from_secs(90)).unwrap(), "PT1M30S");
        assert_eq!(delay_element(Duration::from_secs(3_600)).unwrap(), "PT1H");
        assert_eq!(
            delay_element(Duration::from_secs(5_400)).unwrap(),
            "PT1H30M"
        );
    }

    /// No delay is an absent element, not `PT0S`. See the function's own comment.
    #[test]
    fn no_delay_writes_no_element() {
        assert_eq!(delay_element(Duration::ZERO), None);
    }

    #[test]
    fn an_enormous_delay_is_clamped_to_one_that_can_be_read_back() {
        let element = delay_element(Duration::from_secs(u64::MAX)).unwrap();
        assert_eq!(element, "PT166H39M59S");
        assert_eq!(parse_delay(&element), Some(Duration::from_secs(599_999)));
    }

    /// What is written has to be readable back, since the task is the source of truth for this setting.
    #[test]
    fn a_written_delay_survives_a_round_trip_through_the_scheduler_format() {
        for seconds in [30, 60, 90, 120, 900, 3_600, 5_400, 599_999] {
            let element = delay_element(Duration::from_secs(seconds)).expect("a non-zero delay");
            assert_eq!(
                parse_delay(&element),
                Some(Duration::from_secs(seconds)),
                "{element} did not round-trip"
            );
        }
    }

    /// The whole point of the document: a trigger scoped to one user. A trigger with no `<UserId>` is the
    /// administrator-only "any user" form this function exists to avoid.
    #[test]
    fn the_logon_trigger_names_the_user() {
        let xml = document(
            r"C:\Users\user\AppData\Local\Programs\AgentBench\agentbench.exe",
            Some("dashboard"),
            r"DESKTOP\user",
            Duration::from_secs(120),
        );
        let trigger = element(&xml, "LogonTrigger").expect("a LogonTrigger");
        assert_eq!(element(trigger, "UserId"), Some(r"DESKTOP\user"));
    }

    /// The schema sequence the error message hides: `<Delay>` before `<UserId>`.
    #[test]
    fn the_delay_precedes_the_user_inside_the_trigger() {
        let xml = document("x.exe", None, "u", Duration::from_secs(120));
        let trigger = element(&xml, "LogonTrigger").expect("a LogonTrigger");
        assert!(
            trigger.find("<Delay>") < trigger.find("<UserId>"),
            "{trigger}"
        );
    }

    /// Every field this writes has to survive the reader, since the task is where the settings live.
    #[test]
    fn a_written_document_reads_back_as_what_was_asked_for() {
        let xml = document(
            r"C:\Programs\AgentBench\agentbench.exe",
            Some("dashboard"),
            r"DESKTOP\user",
            Duration::from_secs(120),
        );
        let exec = element(&xml, "Exec").expect("an Exec block");
        assert_eq!(
            element(exec, "Command"),
            Some(r"C:\Programs\AgentBench\agentbench.exe")
        );
        assert_eq!(element(exec, "Arguments"), Some("dashboard"));
        let trigger = element(&xml, "LogonTrigger").expect("a LogonTrigger");
        assert_eq!(
            element(trigger, "Delay").and_then(parse_delay),
            Some(Duration::from_secs(120))
        );
        let principals = element(&xml, "Principals").expect("a Principals block");
        assert_eq!(element(principals, "RunLevel"), Some("LeastPrivilege"));
        assert_eq!(element(principals, "LogonType"), Some("InteractiveToken"));
    }

    /// The tray build is the daemon and takes no subcommand, so the element goes away rather than emptying.
    #[test]
    fn a_program_with_no_arguments_writes_no_arguments_element() {
        let xml = document(
            r"C:\Programs\AgentBench\agentbench-tray.exe",
            None,
            "u",
            Duration::ZERO,
        );
        let exec = element(&xml, "Exec").expect("an Exec block");
        assert_eq!(element(exec, "Arguments"), None);
        assert_eq!(element(&xml, "Delay"), None);
        assert!(
            !xml.contains("\n\n"),
            "an absent element left a blank line:\n{xml}"
        );
    }

    /// An installation directory containing `&` is a valid path and an invalid document.
    #[test]
    fn an_ampersand_in_a_path_is_escaped() {
        let xml = document(
            r"C:\R&D\agentbench.exe",
            Some("dashboard"),
            "u",
            Duration::ZERO,
        );
        assert!(
            xml.contains(r"<Command>C:\R&amp;D\agentbench.exe</Command>"),
            "{xml}"
        );
    }

    #[test]
    fn escaping_covers_the_characters_that_end_an_element() {
        assert_eq!(escape("a & b < c > d"), "a &amp; b &lt; c &gt; d");
    }

    /// `&` has to be replaced first, or the `&` in `&lt;` is escaped a second time.
    #[test]
    fn escaping_does_not_double_escape_its_own_output() {
        assert_eq!(escape("<"), "&lt;");
        assert_eq!(escape("&lt;"), "&amp;lt;");
    }
}
