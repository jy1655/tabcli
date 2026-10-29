//! Bounded best-effort Windows console fallback, pinned to Codex 0.163.0.
//! Counts are rendered cardinality, not content receipts. No concurrent input is assumed.
use super::super::correlation::split_marker_line;
use super::lexical::{
    REVIEW_HOOKS, TRUST_ALL_AND_CONTINUE, confirmation_hint, numbered_label, pinned_option,
};
#[cfg(windows)]
use crate::native::clock::{Clock, SystemClock};
#[cfg(windows)]
use crate::native::console_control;
#[cfg(windows)]
use crate::native::session::RecordStore;
use crate::native::session::{Reader, turn};
use crate::native::terminal::{self, TerminalSendFailure as Failure, TerminalSendResult};
use anyhow::{Context, Result, bail};
#[cfg(windows)]
use std::io::Write;
use std::{
    path::Path,
    time::{Duration, Instant},
};

const POST_TEXT_RESERVE: Duration = Duration::from_secs(20);
const PASTE_SPLIT_GAP: Duration = Duration::from_millis(150);
const POLL: Duration = Duration::from_millis(250);
const RECOVERY: &str = "Check tabcli result <session> and the surface. Only if the entire draft remains in the composer with no dialog or running turn, consider pressing Enter once. Otherwise close the session or diagnose with doctor. Bridge resends neither text nor Return.";

#[derive(Debug, PartialEq, Eq)]
enum Segment {
    Placeholder(usize),
    Inline(String),
}
#[derive(Debug)]
struct Ambiguous(&'static str);

#[derive(PartialEq)]
enum Overlay {
    Clear,
    Blocked,
    Ambiguous,
}

// One addressed span is shared by parsing, overlay recognition and classification.
// The exact marker outranks footer-shaped content; menu selections are not starts.
fn composer_span(rows: &[&str], tail: &str, workspace: &Path) -> Option<std::ops::Range<usize>> {
    let start = rows.iter().position(|r| r.starts_with("› "))?;
    if menu_evidence(rows[start]) {
        return None;
    }
    let marker = (start..rows.len()).find(|&i| {
        !tail.is_empty()
            && rows[i]
                .strip_prefix("› ")
                .or_else(|| rows[i].strip_prefix("  "))
                == Some(tail)
    });
    let end = marker
        .map(|i| i + 1)
        .or_else(|| (start + 1..rows.len()).find(|&i| footer(rows[i], workspace)))?;
    Some(start..end)
}
fn menu_evidence(row: &str) -> bool {
    row.strip_prefix("› ")
        .or_else(|| row.strip_prefix("  "))
        .is_some_and(|text| numbered_label(text).is_some_and(pinned_option))
        || confirmation_hint(row.trim())
}
fn overlay(screen: &str, tail: &str, workspace: &Path) -> Overlay {
    let rows: Vec<_> = screen.lines().map(str::trim_end).collect();
    let span = composer_span(&rows, tail, workspace);
    let outside: Vec<_> = rows
        .iter()
        .enumerate()
        .filter(|(i, _)| !span.as_ref().is_some_and(|span| span.contains(i)))
        .map(|(_, row)| row.trim().trim_start_matches("› "))
        .collect();
    let has = |value: &str| outside.contains(&value);
    let picker = has("Hooks need review")
        && has("1. Review hooks")
        && has("2. Trust all and continue")
        && outside
            .iter()
            .any(|r| r.starts_with("3. Continue without trusting"))
        && has("enter confirm · esc skip");
    let trust = outside
        .iter()
        .any(|r| *r == "Trust this folder?" || r.starts_with("Trust this folder? Codex "))
        && has("1. Trust and continue")
        && (has("2. Don't trust") || has("2. Quit"));
    if picker || trust {
        return Overlay::Blocked;
    }
    // Even inside a draft, numbered pinned labels and hints cannot be distinguished
    // from clipped UI. Outside the single span, every prompt glyph is unresolved UI.
    if rows.iter().enumerate().any(|(i, row)| {
        menu_evidence(row)
            || (!span.as_ref().is_some_and(|span| span.contains(&i)) && row.starts_with("› "))
    }) || outside.iter().any(|r| {
        matches!(
            *r,
            "Hooks need review"
                | "Trust this folder?"
                | REVIEW_HOOKS
                | "Trust and continue"
                | TRUST_ALL_AND_CONTINUE
                | "2. Don't trust"
                | "2. Quit"
                | "enter confirm · esc skip"
                | "enter continue · esc quit"
        ) || r.starts_with("Trust this folder? Codex ")
            || r.starts_with("1. Review hooks")
            || r.starts_with("1. Trust and continue")
            || r.starts_with("2. Trust all and continue")
            || r.starts_with("3. Continue without trusting")
    }) {
        Overlay::Ambiguous
    } else {
        Overlay::Clear
    }
}

// rust-v0.163.0 footer.rs: left_side_line, footer_from_props_lines,
// context_window_line. Unknown custom status lines need an inline marker.
fn normalized_workspace(value: &str) -> String {
    let value = value.replace('/', "\\");
    let value = if let Some(rest) = value.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else {
        value.strip_prefix(r"\\?\").unwrap_or(&value).to_owned()
    };
    value.to_lowercase()
}
fn footer(row: &str, workspace: &Path) -> bool {
    let hint = row.trim();
    hint.starts_with("? for shortcuts")
        || hint.contains(" · ? for shortcuts")
        || hint.ends_with("% context left")
        || hint.contains("warning · f2 to view")
        || hint.contains("warnings · f2 to view")
        || hint.starts_with("esc to interrupt")
        || hint.starts_with("tab to queue")
        || matches!(
            hint,
            "esc again to edit previous message"
                | "esc esc to edit previous message"
                | "ctrl+c again to quit"
                | "ctrl+d again to quit"
                | "Plan mode"
                | "Plan mode (shift+tab to cycle)"
        )
        || hint.strip_suffix(" used").is_some_and(|n| {
            !n.is_empty()
                && n.chars()
                    .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'K' | 'M' | 'B' | 'T'))
        })
        || hint.rsplit_once(" · ").is_some_and(|(_, path)| {
            normalized_workspace(path) == normalized_workspace(&workspace.to_string_lossy())
        })
}

// Internal empty rows and physical LF are content; right padding is not.
// The addressed marker is primary: rows after it do not enter the draft.
fn composer_segments(
    screen: &str,
    tail: &str,
    workspace: &Path,
) -> std::result::Result<Vec<Segment>, Ambiguous> {
    match overlay(screen, tail, workspace) {
        Overlay::Blocked => return Err(Ambiguous("blocked")),
        Overlay::Ambiguous => return Err(Ambiguous("unresolved overlay")),
        Overlay::Clear => {}
    }
    let rows: Vec<_> = screen.lines().map(str::trim_end).collect();
    let span = composer_span(&rows, tail, workspace).ok_or(Ambiguous(
        "no composer span: missing prompt, footer or marker",
    ))?;
    let mut contents = vec![rows[span.start]["› ".len()..].to_owned()];
    for row in &rows[span.start + 1..span.end] {
        if row.is_empty() {
            contents.push(String::new());
        } else if let Some(text) = row.strip_prefix("  ") {
            contents.push(text.to_owned());
        } else {
            return Err(Ambiguous("unrecognized continuation row"));
        }
    }
    while contents.last().is_some_and(String::is_empty) {
        contents.pop();
    }
    let text = contents.join("\n");
    let mut rest = text.as_str();
    let mut segments = Vec::new();
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("[Pasted Content ") {
            let Some((digits, suffix)) = after.split_once(" chars]") else {
                return Err(Ambiguous("clipped placeholder"));
            };
            if digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit()) {
                return Err(Ambiguous("invalid placeholder"));
            }
            let n = digits
                .parse()
                .map_err(|_| Ambiguous("placeholder overflow"))?;
            rest = suffix;
            if let Some(after_hash) = rest.strip_prefix(" #") {
                let len = after_hash.bytes().take_while(u8::is_ascii_digit).count();
                if len == 0 {
                    return Err(Ambiguous("clipped placeholder suffix"));
                }
                rest = &after_hash[len..];
            }
            segments.push(Segment::Placeholder(n));
        } else {
            let end = rest.find("[Pasted Content ").unwrap_or(rest.len());
            let inline = &rest[..end];
            // Keep LF separate so the final inline segment is the claim marker line.
            for (i, line) in inline.split('\n').enumerate() {
                if i > 0 {
                    segments.push(Segment::Inline("\n".into()));
                }
                if !line.is_empty() {
                    segments.push(Segment::Inline(line.into()));
                }
            }
            rest = &rest[end..];
        }
    }
    Ok(segments)
}

// rust-v0.163.0 chat_composer/paste_input.rs:145-176 normalizes CRLF/CR,
// sanitizes controls/CSI (history_cell/messages.rs:25-85), then counts scalars.
// It does NOT collapse triple LF. Bridge trims the source before framing,
// explaining the real 25596->25595 / 1094->1093 differences.
// Blockquote expansion at 163-175 depends on insertion target and burst
// boundaries, which captures cannot establish. Refuse that fallback.
fn normalized_count(text: &str) -> usize {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut chars = text.chars().peekable();
    let mut sanitized = String::new();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        } else if !c.is_control() || matches!(c, '\n' | '\t') {
            sanitized.push(c);
        }
    }
    sanitized.chars().count()
}

#[derive(Debug, PartialEq, Eq)]
enum Observation {
    Complete,
    Short,
    Ambiguous,
}
fn observe(segments: &[Segment], expected: &str) -> Observation {
    if expected
        .replace('\r', "\n")
        .lines()
        .any(|line| line.starts_with("> "))
    {
        return Observation::Ambiguous;
    }
    let expected = normalized_count(expected);
    let mut offset: usize = 0;
    for segment in segments {
        let count = match segment {
            Segment::Placeholder(n) => *n,
            Segment::Inline(s) => s.chars().count(),
        };
        let Some(end) = offset.checked_add(count) else {
            return Observation::Ambiguous;
        };
        if end > expected {
            return Observation::Ambiguous;
        }
        offset = end;
    }
    if offset == expected {
        Observation::Complete
    } else {
        Observation::Short
    }
}

trait Input {
    fn now(&mut self) -> Instant;
    fn sleep(&mut self, duration: Duration);
    fn current(&mut self) -> Result<()>;
    fn screen(&mut self) -> Result<String>;
    fn text(&mut self, text: &str) -> std::result::Result<usize, Failure>;
    fn submit(&mut self, screen: &str) -> Result<bool>;
    fn bound(&mut self) -> Result<bool>;
}
fn check(io: &mut impl Input, deadline: Instant) -> Result<()> {
    io.current()?;
    if io.now() >= deadline {
        bail!("request deadline expired");
    }
    Ok(())
}
fn wait(io: &mut impl Input, deadline: Instant) {
    let remaining = deadline.saturating_duration_since(io.now());
    io.sleep(POLL.min(remaining));
}
fn applied(
    io: &mut impl Input,
    deadline: Instant,
    expected: &str,
    tail: &str,
    workspace: &Path,
) -> Result<String> {
    let mut observed = "unreadable: no screen observed".to_owned();
    loop {
        check(io, deadline).with_context(|| format!("paste observation: {observed}"))?;
        let screen = io.screen();
        // A blocking read cannot extend the request budget or authorize a stale claim.
        let screen = match screen {
            Ok(s) => s,
            Err(e) => {
                bail!("unreadable composer: {e}");
            }
        };
        if overlay(&screen, tail, workspace) == Overlay::Blocked {
            bail!("blocked composer: dialog or hook-review picker; no further input");
        }
        let parsed = composer_segments(&screen, tail, workspace);
        observed = format!("{parsed:?}");
        check(io, deadline).with_context(|| format!("paste observation: {observed}"))?;
        if let Ok(segments) = parsed {
            // FIFO processing makes the final rendered marker the completion
            // evidence. A merged tail has only cardinality evidence, not identity.
            let complete = match segments.last() {
                Some(Segment::Inline(last)) => last == tail,
                Some(Segment::Placeholder(_)) => {
                    observe(&segments, expected) == Observation::Complete
                }
                None => false,
            };
            if complete {
                return Ok(screen);
            }
        }
        wait(io, deadline);
    }
}
fn classification(screen: Result<String>, tail: &str, workspace: &Path) -> String {
    match screen {
        Err(error) => format!("unreadable ({error})"),
        Ok(screen) if overlay(&screen, tail, workspace) == Overlay::Blocked => "blocked".into(),
        Ok(screen) => match composer_segments(&screen, tail, workspace) {
            Ok(segments) if segments == [Segment::Inline("Ask Codex to do anything".into())] => {
                "positively empty".into()
            }
            Ok(segments) if !segments.is_empty() => "populated".into(),
            Ok(_) => "ambiguous".into(),
            Err(error) => format!("ambiguous ({})", error.0),
        },
    }
}
fn run(
    io: &mut impl Input,
    prompt: &str,
    claim: &str,
    deadline: Instant,
    workspace: &Path,
) -> TerminalSendResult {
    // Windows currently writes raw UTF-8 unchanged. The grammar owner tolerates at most one
    // terminal LF; the frame observed on screen is body, LF and marker line without it.
    let (body, tail) = split_marker_line(prompt, claim).map_err(Failure::not_sent)?;
    let prompt = &prompt[..body.len() + 1 + tail.len()];
    check(io, deadline).map_err(Failure::not_sent)?;
    if deadline.saturating_duration_since(io.now()) < POST_TEXT_RESERVE {
        return Err(Failure::not_sent(anyhow::anyhow!(
            "not enough time left to observe the paste"
        )));
    }
    // The first helper owns its explicit pre-write failure classification. Once it
    // succeeds, every subsequent failure is sticky Uncertain, including helper setup.
    let count = io.text(body)?;
    let result = (|| -> Result<()> {
        check(io, deadline)?;
        if count != body.chars().count() {
            bail!("body scalar count mismatch");
        }
        // Heuristic only: usually closes the 60 ms Windows idle burst.
        // Scheduling can still merge the tail; retain cardinality fallback.
        let gap = PASTE_SPLIT_GAP.min(deadline.saturating_duration_since(io.now()));
        io.sleep(gap);
        check(io, deadline)?;
        let count = io.text(&format!("\n{tail}")).map_err(Failure::into_error)?;
        check(io, deadline)?;
        if count != tail.chars().count() + 1 {
            bail!("tail scalar count mismatch");
        }
        let screen = applied(io, deadline, prompt, tail, workspace)?;
        check(io, deadline)?;
        if !io.submit(&screen)? {
            bail!("composer changed before guarded submission");
        }
        let binding_result = (|| -> Result<()> {
            check(io, deadline)?;
            loop {
                let bound = io.bound()?;
                check(io, deadline)?;
                if bound {
                    return Ok(());
                }
                wait(io, deadline);
                check(io, deadline)?;
            }
        })();
        binding_result.map_err(|error| {
            // Diagnostic only. The bounded reader may refuse an expired deadline;
            // report unreadable rather than pretending the last draft is still there.
            let state = classification(io.screen(), tail, workspace);
            anyhow::anyhow!("no claim binding observed: {error:#}; final composer: {state}")
        })
    })();
    result.map_err(|error| Failure::delivery_uncertain(anyhow::anyhow!("{error:#}. {RECOVERY}")))
}

#[cfg(windows)]
struct Console<'a, H> {
    client: &'a mut console_control::Client<H>,
    session: &'a terminal::TerminalSession,
    directory: &'a Path,
    claim: &'a str,
    deadline: Instant,
}
#[cfg(windows)]
impl<H: console_control::HelperProcess> Input for Console<'_, H> {
    fn now(&mut self) -> Instant {
        Instant::now()
    }
    fn sleep(&mut self, duration: Duration) {
        SystemClock.sleep(duration);
    }
    fn current(&mut self) -> Result<()> {
        let reader = Reader::open_unchecked(self.directory);
        if turn::current_claim_token(&reader)?.as_deref() != Some(self.claim) {
            bail!("Codex claim changed");
        }
        if reader.status()?.state.ended() {
            bail!("Codex session ended");
        }
        Ok(())
    }
    fn screen(&mut self) -> Result<String> {
        self.client.read_screen(self.session, self.deadline)
    }
    fn text(&mut self, text: &str) -> std::result::Result<usize, Failure> {
        let file = (|| -> Result<_> {
            let mut file = tempfile::Builder::new()
                .prefix("pending-prompt-")
                .suffix(".txt")
                .tempfile_in(self.directory)?;
            RecordStore::set_file_private(file.as_file())?;
            file.write_all(text.as_bytes())?;
            file.flush()?;
            self.current()?;
            Ok(file)
        })()
        .map_err(Failure::not_sent)?;
        self.client
            .send_file_text_only(self.session, file.path(), self.claim, self.deadline)
    }
    fn submit(&mut self, screen: &str) -> Result<bool> {
        self.client.submit_if_unchanged(
            self.directory,
            self.session,
            screen,
            Some(self.claim),
            self.deadline,
        )
    }
    fn bound(&mut self) -> Result<bool> {
        Ok(super::super::turn::read_binding(self.directory, self.claim)?.is_some())
    }
}

#[cfg(windows)]
pub(super) fn send(
    session: &terminal::TerminalSession,
    directory: &Path,
    prompt_path: &Path,
    deadline: Instant,
) -> TerminalSendResult {
    let workspace = Reader::open_unchecked(directory)
        .manifest()
        .map_err(Failure::not_sent)?
        .workspace;
    let prompt = std::fs::read_to_string(prompt_path).map_err(|e| Failure::not_sent(e.into()))?;
    let claim = turn::current_claim_token(&Reader::open_unchecked(directory))
        .and_then(|c| c.context("no current Codex claim"))
        .map_err(Failure::not_sent)?;
    let mut client = console_control::installed();
    run(
        &mut Console {
            client: &mut client,
            session,
            directory,
            claim: &claim,
            deadline,
        },
        &prompt,
        &claim,
        deadline,
        &workspace,
    )
}

#[cfg(test)]
mod tests;
