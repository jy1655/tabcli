//! Bounded best-effort Windows console fallback, pinned to Codex 0.163.0.
//! Counts are rendered cardinality, not content receipts. No concurrent input is assumed.
use crate::native::clock::{Clock, SystemClock};
use crate::native::session::{Reader, RecordStore, turn};
use crate::native::terminal::{self, TerminalSendFailure as Failure, TerminalSendResult};
use anyhow::{Context, Result, bail};
use std::{
    io::Write,
    path::Path,
    time::{Duration, Instant},
};

const POST_TEXT_RESERVE: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(250);
const RECOVERY: &str = "Check tabcli result <session> and the surface. Only if the entire draft remains in the composer with no dialog or running turn, consider pressing Enter once. Otherwise close the session or diagnose with doctor. Bridge resends neither text nor Return.";

#[derive(Debug, PartialEq, Eq)]
enum Segment {
    Placeholder(usize),
    Inline(String),
}
#[derive(Debug)]
struct Ambiguous(&'static str);

fn blocked(screen: &str) -> bool {
    [
        "Trust this folder?",
        "Trust and continue",
        "Review hooks",
        "Trust all and continue",
    ]
    .iter()
    .any(|text| screen.contains(text))
}

// The prompt row is recorded in docs/verification/2026-10-02-windows.md.
// rust-v0.163.0 chat_composer.rs and footer.rs::context_window_line pin the
// two-column inset and footer (100% context left; ? for shortcuts when empty).
// Preserve physical row boundaries as LF for the cardinality-only fallback.
// The marker path tolerates a wrapped/scrolled body. Right padding is not content.
fn composer_segments(screen: &str) -> std::result::Result<Vec<Segment>, Ambiguous> {
    if blocked(screen) {
        return Err(Ambiguous("blocked"));
    }
    let rows: Vec<_> = screen.lines().map(str::trim_end).collect();
    let starts: Vec<_> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.starts_with("› "))
        .collect();
    if starts.len() != 1 {
        return Err(Ambiguous("missing or multiple prompt rows"));
    }
    let start = starts[0].0;
    let mut contents = vec![rows[start]["› ".len()..].to_owned()];
    let mut ended = false;
    for row in &rows[start + 1..] {
        let hint = row.trim();
        if hint.starts_with("? for shortcuts") || hint.ends_with("% context left") {
            ended = true;
            break;
        }
        if row.is_empty() {
            contents.push(String::new());
        } else if let Some(text) = row.strip_prefix("  ") {
            contents.push(text.to_owned());
        } else {
            return Err(Ambiguous("unrecognized or clipped composer row"));
        }
    }
    if !ended {
        return Err(Ambiguous("missing footer"));
    }
    // One empty layout row separates the editor and its footer.
    if contents.last().is_some_and(String::is_empty) {
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

#[derive(Debug, PartialEq, Eq)]
enum Observation {
    Complete,
    Short,
    Ambiguous,
}
fn observe(segments: &[Segment], expected: &str) -> Observation {
    let expected = expected.chars().count();
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
fn applied(io: &mut impl Input, deadline: Instant, expected: &str, tail: &str) -> Result<String> {
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
        if blocked(&screen) {
            bail!("blocked composer: dialog or hook-review picker; no further input");
        }
        let parsed = composer_segments(&screen);
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
fn classification(screen: Result<String>) -> String {
    match screen {
        Err(error) => format!("unreadable ({error})"),
        Ok(screen) if blocked(&screen) => "blocked".into(),
        Ok(screen) => match composer_segments(&screen) {
            Ok(segments) if segments == [Segment::Inline("Ask Codex to do anything".into())] => {
                "positively empty".into()
            }
            Ok(segments) if !segments.is_empty() => "populated".into(),
            Ok(_) => "ambiguous".into(),
            Err(error) => format!("ambiguous ({})", error.0),
        },
    }
}
fn run(io: &mut impl Input, prompt: &str, claim: &str, deadline: Instant) -> TerminalSendResult {
    // Windows currently writes raw UTF-8 unchanged. Tolerate at most one
    // terminal LF; never trim arbitrary prompt whitespace or multiple lines.
    let prompt = prompt.strip_suffix('\n').unwrap_or(prompt);
    let (body, tail) = prompt
        .rsplit_once('\n')
        .context("framed prompt has no marker line")
        .map_err(Failure::not_sent)?;
    if tail != format!("<!-- agent-bridge-codex-turn:{claim} -->") {
        return Err(Failure::not_sent(anyhow::anyhow!(
            "framed prompt marker does not match claim"
        )));
    }
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
        check(io, deadline)?;
        let count = io.text(&format!("\n{tail}")).map_err(Failure::into_error)?;
        check(io, deadline)?;
        if count != tail.chars().count() + 1 {
            bail!("tail scalar count mismatch");
        }
        let screen = applied(io, deadline, prompt, tail)?;
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
            let state = classification(io.screen());
            anyhow::anyhow!("no claim binding observed: {error:#}; final composer: {state}")
        })
    })();
    result.map_err(|error| Failure::delivery_uncertain(anyhow::anyhow!("{error:#}. {RECOVERY}")))
}

pub(super) fn send(
    session: &terminal::TerminalSession,
    directory: &Path,
    prompt_path: &Path,
    deadline: Instant,
) -> TerminalSendResult {
    let prompt = std::fs::read_to_string(prompt_path).map_err(|e| Failure::not_sent(e.into()))?;
    let claim = turn::current_claim_token(&Reader::open_unchecked(directory))
        .and_then(|c| c.context("no current Codex claim"))
        .map_err(Failure::not_sent)?;
    struct Console<'a> {
        session: &'a terminal::TerminalSession,
        directory: &'a Path,
        claim: &'a str,
        deadline: Instant,
    }
    impl Input for Console<'_> {
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
            terminal::read_screen(self.session, self.deadline)
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
            terminal::send_file_text_only(self.session, file.path(), self.deadline)
        }
        fn submit(&mut self, screen: &str) -> Result<bool> {
            terminal::submit_if_unchanged(self.directory, self.session, screen, self.deadline)
        }
        fn bound(&mut self) -> Result<bool> {
            Ok(super::super::lifecycle::read_binding(self.directory, self.claim)?.is_some())
        }
    }
    run(
        &mut Console {
            session,
            directory,
            claim: &claim,
            deadline,
        },
        &prompt,
        &claim,
        deadline,
    )
}

#[cfg(test)]
mod tests;
