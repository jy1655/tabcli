//! The submit-delay budget of a native Windows console send, and what the console
//! control helper reports about it. Pure: the console adapter applies it, every target
//! compiles and tests it.
use super::TerminalKind;
use anyhow::{Result, bail};
use std::time::{Duration, Instant};

pub(super) fn windows_console_helper_reports_missing(message: &str) -> bool {
    matches!(
        message.trim(),
        "console process is no longer available" | "Error: console process is no longer available"
    )
}

pub(super) fn windows_console_helper_reports_send_not_started(message: &str) -> bool {
    matches!(
        message.trim(),
        "Windows console submission delays do not fit inside the remaining turn timeout"
            | "Error: Windows console submission delays do not fit inside the remaining turn timeout"
    )
}

pub(super) fn windows_console_extra_submit_delay() -> Duration {
    Duration::from_secs(2)
}

pub(super) fn windows_console_immediate_submit_count(submit_count: usize) -> usize {
    usize::from(submit_count == 1)
}

pub(super) fn windows_console_submit_delays_fit(submit_count: usize, budget: Duration) -> bool {
    let delayed = submit_count.saturating_sub(windows_console_immediate_submit_count(submit_count));
    u32::try_from(delayed)
        .ok()
        .and_then(|count| windows_console_extra_submit_delay().checked_mul(count))
        .is_some_and(|required| required < budget)
}

pub(super) fn remaining_send_budget_at(deadline: Instant, now: Instant) -> Result<Duration> {
    match deadline.checked_duration_since(now) {
        Some(remaining) if !remaining.is_zero() => Ok(remaining),
        _ => bail!("terminal send deadline is exhausted"),
    }
}

pub(super) fn validate_terminal_send_budget_for_platform(
    submit_count: usize,
    terminal_kind: TerminalKind,
    timeout: Duration,
    windows: bool,
) -> Result<()> {
    if windows
        && terminal_kind == TerminalKind::WindowsConsole
        && !windows_console_submit_delays_fit(submit_count, timeout)
    {
        bail!("Windows console submission delays do not fit inside the remaining turn timeout")
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_console_access_denied_is_not_a_missing_surface() {
        assert!(windows_console_helper_reports_missing(
            "Error: console process is no longer available"
        ));
        assert!(!windows_console_helper_reports_missing(
            "Error: console process is no longer available: Access is denied. (os error 5)"
        ));
    }

    #[test]
    fn windows_console_extra_submit_waits_for_paste_confirmation() {
        assert_eq!(windows_console_extra_submit_delay(), Duration::from_secs(2));
    }

    #[test]
    fn windows_console_separates_codex_paste_confirmation_from_the_text_batch() {
        assert_eq!(windows_console_immediate_submit_count(1), 1);
        assert_eq!(windows_console_immediate_submit_count(2), 0);
    }

    #[test]
    fn windows_console_rejects_a_submit_plan_that_cannot_fit_the_remaining_budget() {
        assert!(windows_console_submit_delays_fit(
            0,
            Duration::from_millis(1)
        ));
        assert!(!windows_console_submit_delays_fit(
            2,
            Duration::from_secs(4)
        ));
        assert!(windows_console_submit_delays_fit(2, Duration::from_secs(5)));
        assert!(windows_console_submit_delays_fit(
            1,
            Duration::from_millis(1)
        ));
    }

    #[test]
    fn terminal_delivery_preflight_rejects_only_the_unstarted_windows_submit_plan() {
        assert!(
            validate_terminal_send_budget_for_platform(
                2,
                TerminalKind::WindowsConsole,
                Duration::from_secs(4),
                true,
            )
            .is_err()
        );
        assert!(
            validate_terminal_send_budget_for_platform(
                2,
                TerminalKind::WindowsConsole,
                Duration::from_secs(5),
                true,
            )
            .is_ok()
        );
        assert!(
            validate_terminal_send_budget_for_platform(
                1,
                TerminalKind::WindowsConsole,
                Duration::from_millis(1),
                true,
            )
            .is_ok()
        );
        assert!(
            validate_terminal_send_budget_for_platform(
                2,
                TerminalKind::Iterm2,
                Duration::from_millis(1),
                false,
            )
            .is_ok()
        );
    }

    #[test]
    fn windows_helper_budget_rejection_proves_send_never_started() {
        assert!(windows_console_helper_reports_send_not_started(
            "Error: Windows console submission delays do not fit inside the remaining turn timeout"
        ));
        assert!(!windows_console_helper_reports_send_not_started(
            "Error: Windows console control helper timed out"
        ));
    }

    // The remaining send budget is measured from the original deadline, not from the
    // moment the preflight work ended.
    #[test]
    fn terminal_delivery_keeps_the_original_deadline_after_preflight_work() {
        let started = Instant::now();
        let deadline = started + Duration::from_secs(5);

        assert_eq!(
            remaining_send_budget_at(deadline, started + Duration::from_secs(3)).unwrap(),
            Duration::from_secs(2)
        );
        assert!(remaining_send_budget_at(deadline, deadline).is_err());
    }
}
