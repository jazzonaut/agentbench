//! Reading and writing the pieces of a Task Scheduler definition, without touching Task Scheduler.
//!
//! Deliberately not behind `#[cfg(windows)]`. Parsing is where the bugs in this feature will be, and the
//! project's lint job runs on Linux — so anything hidden behind that attribute is code CI never checks. The
//! process spawning and the UTF-16 decoding have to be Windows-only; recognising a `<Delay>PT2M</Delay>`
//! does not, so it is tested everywhere instead.
//!
//! No XML crate. The only documents this reads are ones `schtasks /Query /XML` produced moments earlier
//! from a task this program registered, so the general problem of parsing XML does not arise — and a
//! dependency to solve a problem the input cannot pose is a dependency that earns nothing.

// The cost of that choice: on a platform with no Task Scheduler nothing calls any of this, and `-D warnings`
// reads "unused" as "broken". Silenced here rather than fixed with `#[cfg(windows)]`, because compiling on
// every platform is the whole point of the module and the tests below are the ones doing the checking.
#![cfg_attr(not(windows), allow(dead_code))]

use std::time::Duration;

/// Longest delay `schtasks /Create /DELAY` can express, in its `mmmm:ss` form.
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

/// The `mmmm:ss` form `schtasks /Create /DELAY` accepts.
///
/// Clamped rather than refused: the value comes from a screen where the user picked a number of minutes,
/// and the largest expressible delay is a better answer to "wait a very long time" than an error is.
pub(super) fn delay_argument(delay: Duration) -> String {
    let total = delay.as_secs();
    let minutes = (total / 60).min(MAX_DELAY_MINUTES);
    let seconds = if minutes == MAX_DELAY_MINUTES {
        59
    } else {
        total % 60
    };
    format!("{minutes:04}:{seconds:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from what `schtasks /Query /TN "AgentBench dashboard" /XML ONE` actually prints.
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
    fn a_delay_argument_uses_the_four_digit_minute_form() {
        assert_eq!(delay_argument(Duration::from_secs(0)), "0000:00");
        assert_eq!(delay_argument(Duration::from_secs(120)), "0002:00");
        assert_eq!(delay_argument(Duration::from_secs(90)), "0001:30");
        assert_eq!(delay_argument(Duration::from_secs(3_600)), "0060:00");
    }

    /// The clamp, and that it stays inside the field's width.
    #[test]
    fn an_enormous_delay_is_clamped_to_the_largest_expressible_one() {
        let argument = delay_argument(Duration::from_secs(u64::MAX));
        assert_eq!(argument, "9999:59");
        assert_eq!(argument.len(), 7);
    }

    /// What is written has to be readable back, since the task is the source of truth for this setting.
    #[test]
    fn a_written_delay_survives_a_round_trip_through_the_scheduler_format() {
        for seconds in [0, 30, 60, 90, 120, 900, 3_600, 5_400] {
            let argument = delay_argument(Duration::from_secs(seconds));
            let (minutes, remainder) = argument.split_once(':').expect("mmmm:ss");
            let parsed: u64 =
                minutes.parse::<u64>().unwrap() * 60 + remainder.parse::<u64>().unwrap();
            assert_eq!(parsed, seconds, "{argument} did not round-trip");
        }
    }
}
