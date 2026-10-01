//! Keeps the keyboard where it was while a managed surface is created (issue #58).
//!
//! Windows Terminal brings a window to the front whenever a command line is dispatched
//! to it, and a console window handed to the default terminal is created in front. No
//! option creates either surface behind the window the user is typing in, so the launch
//! remembers that window and gives the foreground back, but only while the window that
//! hosts the new surface holds it. A window the user goes to in the meantime is
//! remembered instead and gets the keyboard back from then on, and the surface's own
//! window keeps the keyboard when it comes to the front again later than a window does
//! on its own. Everything here is best effort and never fails a launch.
//!
//! What the user did cannot be told from what a window did. The surface's window in
//! front before the keyboard was given back, or shortly after, is taken as the launch's
//! doing, also when the user happened to select it in that moment: leaving the keyboard
//! in a session the user did not choose is the worse mistake.
//!
//! The foreground is watched by a thread of its own, because "shortly after" is only
//! known to someone who looks all the time, and the launch itself waits on files and
//! processes in between. The launch never waits for that thread longer than a moment:
//! giving the foreground back goes through the input queue of the window that holds
//! it, and a window that has stopped answering can hold such a call for good.
//!
//! The window that hosts the surface is never guessed: a process inside the console
//! reads it from the console's own window (`attached_console_host_window`). In a tab that
//! process is the tab host, which reports the window before the root is started. For a
//! console window of its own it is a control helper that attaches to the started root's
//! console. The console window says nothing to a process outside the console: under
//! Windows Terminal it belongs to the terminal's console host process, not to the root
//! (measured 2026-10-01, Windows Terminal 1.24). A terminal window that merely appeared
//! during the launch is not attributed to it; the user may have opened it.
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicIsize, Ordering},
        mpsc::Receiver,
    },
    time::{Duration, Instant},
};

use windows_sys::Win32::{
    Foundation::HWND,
    System::{
        Console::GetConsoleWindow,
        Threading::{AttachThreadInput, GetCurrentThreadId},
    },
    UI::WindowsAndMessaging::{
        BringWindowToTop, GA_ROOTOWNER, GetAncestor, GetClassNameW, GetForegroundWindow,
        GetWindowThreadProcessId, IsWindow, SetForegroundWindow,
    },
};

const POLL_INTERVAL: Duration = Duration::from_millis(10);
// A window that was sent to the back can come to the front again on its own while it
// starts (measured 2026-10-01, Windows Terminal 1.24: 54 ms after the first time). Only
// within this time after the keyboard was given back is that still the launch's doing;
// later, the user has chosen the window.
const REACTIVATION_WINDOW: Duration = Duration::from_millis(200);
// How long the launch waits for the watcher's last word. A call into the window system
// can hang when the window that holds the foreground has stopped answering, and the
// launch must not hang with it.
const STOP_WAIT: Duration = Duration::from_millis(250);
// The window of a console host, which shows the console itself.
const CONSOLE_HOST_CLASS: &str = "ConsoleWindowClass";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ForegroundAction {
    Wait,
    GiveBack,
    Adopt,
    Stop,
}

// `remembered` is the window the keyboard goes back to, and `owned` the window that hosts
// the new surface, once it is known. Any other window in front is where the user went,
// as far as is known: until `owned` is known it may be the surface's own window, which
// is remembered like any other and forgotten again when the surface names it.
//
// `since_give_back` is the time since the keyboard was last given back. The surface's
// window in front again long after that was brought there by the user, who keeps it.
fn foreground_action(
    foreground: isize,
    remembered: isize,
    owned: Option<isize>,
    since_give_back: Option<Duration>,
) -> ForegroundAction {
    if foreground == 0 || foreground == remembered {
        return ForegroundAction::Wait;
    }
    match owned {
        Some(owned) if owned == foreground => match since_give_back {
            Some(elapsed) if elapsed > REACTIVATION_WINDOW => ForegroundAction::Stop,
            _ => ForegroundAction::GiveBack,
        },
        _ => ForegroundAction::Adopt,
    }
}

// What the watcher knows and has done. It decides from what it is shown, and the caller
// acts on the window it names, so the rules are tested without any window.
#[derive(Debug)]
struct Watch {
    // The window that had the keyboard when the launch began.
    origin: isize,
    // The windows the user went to since, oldest first.
    adopted: Vec<isize>,
    stopped: bool,
    given_back: u32,
    given_back_at: Option<Instant>,
    host: Option<isize>,
}

impl Watch {
    fn new(foreground: isize) -> Self {
        Self {
            origin: foreground,
            adopted: Vec::new(),
            stopped: false,
            given_back: 0,
            given_back_at: None,
            host: None,
        }
    }

    // One look at the foreground. Returns the window to give the keyboard back to.
    // `exists` says whether a remembered window is still there to take it.
    fn look(
        &mut self,
        foreground: isize,
        owned: Option<isize>,
        now: Instant,
        exists: impl Fn(isize) -> bool,
    ) -> Option<isize> {
        if let Some(owned) = owned {
            self.host = Some(owned);
            // The surface's window was in front before it was known for what it is.
            self.adopted.retain(|window| *window != owned);
        }
        if self.stopped {
            return None;
        }
        let remembered = self.adopted.last().copied().unwrap_or(self.origin);
        match foreground_action(
            foreground,
            remembered,
            owned,
            self.given_back_at
                .map(|at| now.saturating_duration_since(at)),
        ) {
            ForegroundAction::Wait => None,
            ForegroundAction::Adopt => {
                self.adopted.push(foreground);
                None
            }
            ForegroundAction::Stop => {
                self.stopped = true;
                None
            }
            // The newest window the user was in that is still there.
            ForegroundAction::GiveBack => self
                .adopted
                .iter()
                .rev()
                .chain(std::iter::once(&self.origin))
                .copied()
                .find(|window| exists(*window)),
        }
    }

    // The keyboard was given back, as `look` asked. The time is taken after the calls
    // into the window system have returned: they can take long, and what counts is how
    // soon the surface's window comes back after it was really sent away.
    fn gave_back(&mut self, now: Instant) {
        self.given_back += 1;
        self.given_back_at = Some(now);
    }

    fn summary(&self, foreground: isize) -> String {
        let state = if foreground == self.origin {
            "kept"
        } else if self.stopped {
            "left with the surface the user selected"
        } else if self.host.is_none() {
            // The window in front may be the surface's own: it never said which it is.
            "with a window that could not be told from the surface's"
        } else if self.adopted.contains(&foreground) {
            "kept"
        } else {
            "held by a window that was not attributed to this surface"
        };
        let host = match self.host {
            Some(host) => format!("host window {host:#x}"),
            None => "host window not identified".to_owned(),
        };
        format!(
            "foreground={state}; given back {} time(s); {host}",
            self.given_back
        )
    }
}

struct Shared {
    // The window that hosts the surface; 0 until it is known.
    host: AtomicIsize,
    done: AtomicBool,
}

pub(super) struct ForegroundGuard {
    shared: Arc<Shared>,
    // The watcher's last word. The watcher is never joined: one that is stuck in a call
    // into the window system is left behind, and ends with the process.
    finished: Option<Receiver<Watch>>,
}

impl ForegroundGuard {
    /// Remembers the window that has the keyboard and starts watching the foreground.
    /// `None` when no window has it (a locked or headless desktop) or no watcher could be
    /// started.
    pub(super) fn capture() -> Option<Self> {
        let previous = unsafe { GetForegroundWindow() } as isize;
        if previous == 0 {
            return None;
        }
        let shared = Arc::new(Shared {
            host: AtomicIsize::new(0),
            done: AtomicBool::new(false),
        });
        let watched = shared.clone();
        let (last_word, finished) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("foreground-guard".to_owned())
            .spawn(move || {
                let mut watch = Watch::new(previous);
                while !watched.done.load(Ordering::Acquire) {
                    let foreground = unsafe { GetForegroundWindow() } as isize;
                    let owned =
                        Some(watched.host.load(Ordering::Acquire)).filter(|host| *host != 0);
                    let target = watch.look(foreground, owned, Instant::now(), |window| {
                        (unsafe { IsWindow(window as HWND) }) != 0
                    });
                    if let Some(target) = target {
                        give_foreground_to(target, foreground);
                        watch.gave_back(Instant::now());
                    }
                    std::thread::sleep(POLL_INTERVAL);
                }
                let _ = last_word.send(watch);
            })
            .ok()?;
        Some(Self {
            shared,
            finished: Some(finished),
        })
    }

    /// Tells the watcher which window hosts the surface.
    pub(super) fn host_is(&self, window: isize) {
        self.shared.host.store(window, Ordering::Release);
    }

    /// Keeps watching until `duration` has passed: a new window can take the foreground
    /// more than once while it starts. A surface whose host window is not known yet is
    /// asked for it through `find` until it is. Returns what became of the foreground,
    /// for the launch log.
    pub(super) fn settle(
        mut self,
        owned: Option<isize>,
        mut find: impl FnMut() -> Option<isize>,
        duration: Duration,
    ) -> String {
        let end = Instant::now() + duration;
        let mut owned = owned;
        while Instant::now() < end {
            if owned.is_none() {
                owned = find();
            }
            if let Some(owned) = owned {
                self.host_is(owned);
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        match self.stop() {
            Some(watch) => watch.summary(unsafe { GetForegroundWindow() } as isize),
            None => "foreground=unknown; the watcher did not answer in time".to_owned(),
        }
    }

    // Tells the watcher to end and waits a moment for what it saw. A watcher that does
    // not answer is stuck in the window system; it is left behind rather than waited for.
    fn stop(&mut self) -> Option<Watch> {
        self.shared.done.store(true, Ordering::Release);
        self.finished.take()?.recv_timeout(STOP_WAIT).ok()
    }
}

impl Drop for ForegroundGuard {
    fn drop(&mut self) {
        // Only the signal: nothing is waited for on a launch that is already failing.
        self.shared.done.store(true, Ordering::Release);
    }
}

fn class_name(window: isize) -> String {
    let mut buffer = [0u16; 64];
    let length = unsafe { GetClassNameW(window as HWND, buffer.as_mut_ptr(), buffer.len() as i32) };
    String::from_utf16_lossy(&buffer[..usize::try_from(length).unwrap_or(0)])
}

fn has_class(window: isize, classes: &[&str]) -> bool {
    window != 0 && classes.contains(&class_name(window).as_str())
}

/// The window that shows the console this process is attached to. `None` while the
/// console has no window that a user sees, which is also the state of a console in a
/// terminal until the terminal has taken its hidden window.
pub(super) fn attached_console_host_window() -> Option<isize> {
    let console = unsafe { GetConsoleWindow() };
    if console.is_null() {
        return None;
    }
    let owner = unsafe { GetAncestor(console, GA_ROOTOWNER) } as isize;
    host_window_of(
        console as isize,
        owner,
        has_class(console as isize, &[CONSOLE_HOST_CLASS]),
    )
}

// A console in a terminal has a hidden window that the terminal's window owns. A console
// host shows the console in the console window itself, which nothing owns.
fn host_window_of(console: isize, root_owner: isize, console_host: bool) -> Option<isize> {
    if root_owner != 0 && root_owner != console {
        Some(root_owner)
    } else {
        console_host.then_some(console)
    }
}

// A process that is not in the foreground may not set the foreground window. Sharing the
// input state of the thread that holds it lifts that restriction for this one call.
fn give_foreground_to(target: isize, holder: isize) {
    let holder_thread = unsafe { GetWindowThreadProcessId(holder as HWND, std::ptr::null_mut()) };
    let current_thread = unsafe { GetCurrentThreadId() };
    let attached =
        holder_thread != 0 && unsafe { AttachThreadInput(current_thread, holder_thread, 1) } != 0;
    unsafe {
        SetForegroundWindow(target as HWND);
        BringWindowToTop(target as HWND);
    }
    if attached {
        unsafe { AttachThreadInput(current_thread, holder_thread, 0) };
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{ForegroundAction, REACTIVATION_WINDOW, Watch, foreground_action, host_window_of};

    const USER: isize = 10;
    const SURFACE: isize = 20;
    const OTHER: isize = 30;

    #[test]
    fn nothing_is_done_while_the_remembered_window_has_the_keyboard() {
        assert_eq!(
            foreground_action(USER, USER, Some(SURFACE), None),
            ForegroundAction::Wait
        );
        assert_eq!(
            foreground_action(0, USER, None, None),
            ForegroundAction::Wait
        );
    }

    #[test]
    fn the_keyboard_is_given_back_only_from_the_window_that_hosts_the_surface() {
        assert_eq!(
            foreground_action(SURFACE, USER, Some(SURFACE), None),
            ForegroundAction::GiveBack
        );
    }

    #[test]
    fn a_window_the_user_went_to_is_remembered_instead() {
        // Whether or not the surface's window is known, and whatever kind of window it
        // is: a terminal window that appeared during the launch may be one the user
        // opened.
        assert_eq!(
            foreground_action(OTHER, USER, None, None),
            ForegroundAction::Adopt
        );
        assert_eq!(
            foreground_action(OTHER, USER, Some(SURFACE), None),
            ForegroundAction::Adopt
        );
    }

    #[test]
    fn the_surfaces_window_keeps_the_keyboard_when_the_user_selects_it_later() {
        let surface_in_front =
            |since_give_back| foreground_action(SURFACE, USER, Some(SURFACE), since_give_back);
        // A window comes to the front again on its own shortly after it was sent back.
        assert_eq!(
            surface_in_front(Some(Duration::from_millis(54))),
            ForegroundAction::GiveBack
        );
        assert_eq!(
            surface_in_front(Some(REACTIVATION_WINDOW)),
            ForegroundAction::GiveBack
        );
        // Later, it is in front because the user put it there.
        assert_eq!(
            surface_in_front(Some(REACTIVATION_WINDOW + Duration::from_millis(1))),
            ForegroundAction::Stop
        );
    }

    fn at(start: Instant, milliseconds: u64) -> Instant {
        start + Duration::from_millis(milliseconds)
    }

    // What the watcher thread does with one look: it gives the keyboard back to the
    // window that `look` names, and tells the watch when that was done.
    fn look_and_give_back(
        watch: &mut Watch,
        foreground: isize,
        owned: Option<isize>,
        now: Instant,
        exists: impl Fn(isize) -> bool,
    ) -> Option<isize> {
        let target = watch.look(foreground, owned, now, exists);
        if target.is_some() {
            watch.gave_back(now);
        }
        target
    }

    // The user leaves for another application while the launch waits for the tab, and
    // only then does the terminal bring the surface's window to the front.
    #[test]
    fn the_keyboard_goes_back_to_the_window_the_user_went_to_meanwhile() {
        let start = Instant::now();
        let mut watch = Watch::new(USER);
        let all_exist = |_| true;

        assert_eq!(watch.look(OTHER, None, at(start, 100), all_exist), None);
        assert_eq!(watch.look(OTHER, None, at(start, 110), all_exist), None);
        // The surface's window takes the foreground: it goes back to where the user was.
        assert_eq!(
            look_and_give_back(
                &mut watch,
                SURFACE,
                Some(SURFACE),
                at(start, 300),
                all_exist
            ),
            Some(OTHER)
        );
        assert_eq!(
            watch.look(OTHER, Some(SURFACE), at(start, 310), all_exist),
            None
        );
        assert_eq!(
            watch.summary(OTHER),
            "foreground=kept; given back 1 time(s); host window 0x14"
        );
    }

    // The window the user went to is another terminal window, and the surface has not
    // said yet which window is its own. It is remembered all the same, and nothing is
    // taken from it.
    #[test]
    fn a_terminal_window_the_user_went_to_before_the_surface_was_known_gets_the_keyboard_back() {
        let start = Instant::now();
        let mut watch = Watch::new(USER);
        let all_exist = |_| true;

        assert_eq!(watch.look(OTHER, None, at(start, 100), all_exist), None);
        assert_eq!(
            watch.look(SURFACE, Some(SURFACE), at(start, 300), all_exist),
            Some(OTHER)
        );
    }

    // The surface's own window comes to the front before the surface has said which
    // window that is. It is remembered like any other, and forgotten when it is named.
    #[test]
    fn the_surfaces_window_is_not_kept_as_a_window_the_user_went_to() {
        let start = Instant::now();
        let mut watch = Watch::new(USER);
        assert_eq!(watch.look(SURFACE, None, at(start, 100), |_| true), None);
        assert_eq!(
            watch.look(SURFACE, Some(SURFACE), at(start, 200), |_| true),
            Some(USER)
        );
    }

    #[test]
    fn a_remembered_window_that_is_gone_is_passed_over() {
        let start = Instant::now();
        let mut watch = Watch::new(USER);
        assert_eq!(watch.look(OTHER, None, at(start, 100), |_| true), None);
        // The window the user went to has closed since: the one before it gets the keyboard.
        assert_eq!(
            watch.look(SURFACE, Some(SURFACE), at(start, 300), |window| window
                != OTHER),
            Some(USER)
        );
        // No remembered window is left: nothing is done.
        let mut watch = Watch::new(USER);
        assert_eq!(
            watch.look(SURFACE, Some(SURFACE), at(start, 300), |_| false),
            None
        );
    }

    // The reactivation is judged by when it is seen, so it has to be seen in time: the
    // watcher looks every few milliseconds on its own thread.
    #[test]
    fn a_surface_that_comes_back_at_once_is_sent_back_again_and_a_later_one_is_kept() {
        let start = Instant::now();
        let mut watch = Watch::new(USER);
        let all_exist = |_| true;

        assert_eq!(
            look_and_give_back(
                &mut watch,
                SURFACE,
                Some(SURFACE),
                at(start, 100),
                all_exist
            ),
            Some(USER)
        );
        // Windows Terminal brings its window forward once more, 54 ms later.
        assert_eq!(
            look_and_give_back(
                &mut watch,
                SURFACE,
                Some(SURFACE),
                at(start, 154),
                all_exist
            ),
            Some(USER)
        );
        assert_eq!(
            watch.look(USER, Some(SURFACE), at(start, 164), all_exist),
            None
        );
        // Half a second later the user selects the new session's window. It keeps the
        // keyboard, and the watcher does nothing from then on.
        assert_eq!(
            watch.look(SURFACE, Some(SURFACE), at(start, 700), all_exist),
            None
        );
        assert_eq!(
            watch.look(SURFACE, Some(SURFACE), at(start, 710), all_exist),
            None
        );
        assert_eq!(
            watch.summary(SURFACE),
            "foreground=left with the surface the user selected; given back 2 time(s); host window 0x14"
        );
    }

    // Giving the keyboard back can take long. The time that counts starts when it was
    // done, not when it was asked for.
    #[test]
    fn a_surface_that_comes_back_soon_after_a_slow_give_back_is_still_the_windows_doing() {
        let start = Instant::now();
        let mut watch = Watch::new(USER);
        let all_exist = |_| true;

        // Asked for at 0 ms; the calls return at 300 ms.
        assert_eq!(
            watch.look(SURFACE, Some(SURFACE), start, all_exist),
            Some(USER)
        );
        watch.gave_back(at(start, 300));
        // 50 ms after the keyboard really went back, the window comes forward again.
        assert_eq!(
            look_and_give_back(
                &mut watch,
                SURFACE,
                Some(SURFACE),
                at(start, 350),
                all_exist
            ),
            Some(USER)
        );
        assert_eq!(
            watch.summary(USER),
            "foreground=kept; given back 2 time(s); host window 0x14"
        );
    }
    // With the tab in the user's own window, that window hosts the surface and had the
    // keyboard before: there is nothing to give back, and nothing is taken.
    #[test]
    fn a_surface_in_the_window_that_had_the_keyboard_changes_nothing() {
        let mut watch = Watch::new(USER);
        assert_eq!(watch.look(USER, Some(USER), Instant::now(), |_| true), None);
        assert_eq!(
            watch.summary(USER),
            "foreground=kept; given back 0 time(s); host window 0xa"
        );
    }

    #[test]
    fn a_window_in_front_is_not_called_kept_when_the_surface_never_named_its_own() {
        let mut watch = Watch::new(USER);
        assert_eq!(watch.look(OTHER, None, Instant::now(), |_| true), None);
        assert_eq!(
            watch.summary(OTHER),
            "foreground=with a window that could not be told from the surface's; given back 0 time(s); host window not identified"
        );
        assert_eq!(
            watch.summary(USER),
            "foreground=kept; given back 0 time(s); host window not identified"
        );
    }

    #[test]
    fn the_host_window_is_the_terminal_that_owns_the_console_window_or_the_console_host() {
        const CONSOLE: isize = 40;
        // In a terminal: the terminal's window owns the hidden console window.
        assert_eq!(host_window_of(CONSOLE, SURFACE, false), Some(SURFACE));
        // Under a console host: the console window is the window.
        assert_eq!(host_window_of(CONSOLE, CONSOLE, true), Some(CONSOLE));
        assert_eq!(host_window_of(CONSOLE, 0, true), Some(CONSOLE));
        // A hidden console window that no terminal has taken yet is not a host window.
        assert_eq!(host_window_of(CONSOLE, CONSOLE, false), None);
        assert_eq!(host_window_of(CONSOLE, 0, false), None);
    }
}
