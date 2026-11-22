//! Type-ahead captured while a turn runs.
//!
//! Nothing reads stdin during a turn: the REPL is *inside* the turn, so the only
//! thing holding what the user types is the terminal's line discipline. That
//! already half-works by accident -- rustyline picks the leftovers up when the turn
//! ends -- but it is lossy (one line survives, the rest are dropped), invisible,
//! and unsafe: a prompt raised mid-turn (tool confirmation, `ask_user_question`,
//! the sudo password) reads that buffered text, so a queued message gets eaten and
//! a bare Enter there used to mean "1 = Execute".
//!
//! So during a turn this module owns stdin. A thread polls fd 0, completes lines,
//! and hands them back to the main thread, which prints them and queues them. Every
//! prompt calls [`suspend`] first: that stops the thread *and* flushes a half-typed
//! line, so a prompt can only ever see what was typed at that prompt.
//!
//! Nothing here is persisted -- a message that was never sent must not end up in
//! `chats.json` as if it had been -- and it is all a no-op when stdin is not a
//! terminal (cron, piped scripts) or on Windows.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

/// Lines the user typed during a turn. Global because the prompt path, the turn
/// loop and the sudo callback all need it and all live behind `&mut self` in
/// different places.
fn queues() -> &'static Mutex<VecDeque<String>> {
    static QUEUE: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();
    QUEUE.get_or_init(|| Mutex::new(VecDeque::new()))
}

fn pending() -> &'static Mutex<VecDeque<String>> {
    static PENDING: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(VecDeque::new()))
}

fn handle() -> &'static Mutex<Option<std::thread::JoinHandle<()>>> {
    static HANDLE: OnceLock<Mutex<Option<std::thread::JoinHandle<()>>>> = OnceLock::new();
    HANDLE.get_or_init(|| Mutex::new(None))
}

static ACTIVE: AtomicBool = AtomicBool::new(false);
static STOP: AtomicBool = AtomicBool::new(false);
/// Parked: the queue is not drained after a turn (a Ctrl-C stopped that turn, so
/// spending what is waiting is the user's call, not ours).
static HELD: AtomicBool = AtomicBool::new(false);

/// True when there is a terminal to capture from.
pub fn is_tty() -> bool {
    #[cfg(unix)]
    unsafe {
        libc::isatty(libc::STDIN_FILENO) == 1
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Begin capturing keystrokes. No-op when not a tty or already capturing.
pub fn start() {
    if !is_tty() || ACTIVE.swap(true, Ordering::SeqCst) {
        return;
    }
    STOP.store(false, Ordering::SeqCst);
    #[cfg(unix)]
    {
        let thread = std::thread::spawn(capture_loop);
        *handle().lock().unwrap() = Some(thread);
    }
    #[cfg(not(unix))]
    ACTIVE.store(false, Ordering::SeqCst);
}

/// Stop capturing and return whatever had arrived but was not yet collected, so
/// the caller can print it. Leftover lines are *not* queued here: the caller owns
/// that decision (a prompt keeps them silent, the end of a turn announces them).
pub fn stop() -> Vec<String> {
    if !ACTIVE.swap(false, Ordering::SeqCst) {
        return Vec::new();
    }
    STOP.store(true, Ordering::SeqCst);
    if let Some(thread) = handle().lock().unwrap().take() {
        let _ = thread.join();
    }
    let mut out: Vec<String> = Vec::new();
    if let Ok(mut p) = pending().lock() {
        out.extend(p.drain(..));
    }
    out
}

/// Stop capturing for the duration of a blocking prompt, and drop a half-typed
/// line: it is not an answer to the question about to be asked. Returns whether
/// capturing was active, for [`resume`].
pub fn suspend() -> bool {
    let was_active = ACTIVE.load(Ordering::SeqCst);
    if !was_active {
        return false;
    }
    let leftovers = stop();
    // Whatever was captured before the prompt is a real message, not an answer.
    enqueue_all(leftovers);
    #[cfg(unix)]
    unsafe {
        libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
    }
    true
}

/// Resume capturing after a prompt.
pub fn resume(was_active: bool) {
    if was_active {
        start();
    }
}

/// Drain the lines captured so far, so the caller can announce and enqueue them.
/// Does *not* queue them itself: the caller owns the announcement, and doing both
/// here would enqueue every line twice.
pub fn collect() -> Vec<String> {
    match pending().lock() {
        Ok(mut p) => p.drain(..).collect(),
        Err(_) => Vec::new(),
    }
}

fn enqueue_all(lines: Vec<String>) {
    if let Ok(mut q) = queues().lock() {
        for line in lines {
            q.push_back(line);
        }
    }
}

pub fn enqueue(line: &str) {
    if line.trim().is_empty() {
        return;
    }
    // Typing while a turn runs means "send when you are done", so a new message
    // un-parks the queue.
    HELD.store(false, Ordering::SeqCst);
    queues().lock().unwrap().push_back(line.to_string());
}

/// Park or release the queue (see [`HELD`]).
pub fn set_held(held: bool) {
    HELD.store(held, Ordering::SeqCst);
}

pub fn is_held() -> bool {
    HELD.load(Ordering::SeqCst)
}

pub fn len() -> usize {
    queues().lock().map(|q| q.len()).unwrap_or(0)
}

pub fn is_empty() -> bool {
    len() == 0
}

/// Oldest first: what a finished turn sends next.
pub fn take_oldest() -> Option<String> {
    queues().lock().unwrap().pop_front()
}

/// Newest: what Up recalls for editing.
pub fn take_newest() -> Option<String> {
    queues().lock().unwrap().pop_back()
}

pub fn clear() {
    queues().lock().unwrap().clear();
    HELD.store(false, Ordering::SeqCst);
}

/// One elided single line per queued message, for `/queue` and the prompt label.
pub fn previews(max_chars: usize) -> Vec<String> {
    let q = queues().lock().unwrap();
    q.iter()
        .map(|text| {
            let first = text.trim().lines().next().unwrap_or("").trim().to_string();
            if max_chars > 1 && first.chars().count() > max_chars {
                let mut s: String = first.chars().take(max_chars - 1).collect();
                s.push('\u{2026}');
                s
            } else {
                first
            }
        })
        .collect()
}

#[cfg(unix)]
fn capture_loop() {
    let mut buf = [0u8; 4096];
    let mut partial = String::new();
    while !STOP.load(Ordering::SeqCst) {
        let mut pfd = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, 100) };
        if rc < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if rc == 0 {
            continue; // timeout: re-check STOP
        }
        let n = unsafe {
            libc::read(
                libc::STDIN_FILENO,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
            )
        };
        if n <= 0 {
            break; // EOF or read error
        }
        let chunk = String::from_utf8_lossy(&buf[..n as usize]).into_owned();
        partial.push_str(&chunk);
        while let Some(pos) = partial.find('\n') {
            let line: String = partial.drain(..=pos).collect();
            let line = line.trim_end_matches(['\n', '\r']).to_string();
            if !line.trim().is_empty() {
                pending().lock().unwrap().push_back(line);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The queue is process-global (the prompt path, the turn loop and the sudo
    /// callback all reach it), and cargo runs tests on threads: without this,
    /// one test's reset() empties the queue another test is mid-way through.
    /// Cost me a red CI run on the v1.16.1 tag.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn reset() {
        clear();
        let _ = stop();
        if let Ok(mut p) = pending().lock() {
            p.clear();
        }
    }

    #[test]
    fn queue_is_fifo_for_sending_and_lifo_for_recall() {
        let _serial = serial();
        reset();
        enqueue("first");
        enqueue("second");
        enqueue("third");
        assert_eq!(len(), 3);
        assert_eq!(take_oldest().as_deref(), Some("first")); // a finished turn sends this
        assert_eq!(take_newest().as_deref(), Some("third")); // Up recalls this
        assert_eq!(take_oldest().as_deref(), Some("second"));
        assert!(is_empty());
    }

    #[test]
    fn blank_lines_are_not_queue_entries() {
        let _serial = serial();
        reset();
        enqueue("");
        enqueue("   ");
        enqueue("\n");
        assert!(is_empty(), "a stray Enter must not queue anything");
    }

    #[test]
    fn previews_are_single_line_and_elided() {
        let _serial = serial();
        reset();
        enqueue("read the logs\nand then summarise");
        enqueue(&"x".repeat(200));
        let p = previews(20);
        assert_eq!(p[0], "read the logs");
        assert_eq!(p[1].chars().count(), 20);
        assert!(p[1].ends_with('\u{2026}'));
        assert_eq!(previews(0)[1].chars().count(), 200);
    }

    #[test]
    fn held_flag_parks_and_releases() {
        let _serial = serial();
        reset();
        enqueue("parked");
        assert!(!is_held());
        set_held(true);
        assert!(is_held());
        // Typing something new means "send it when you are done": un-park.
        enqueue("fresh");
        assert!(!is_held());
        set_held(true);
        clear();
        assert!(!is_held());
    }

    #[test]
    fn taking_from_an_empty_queue_is_harmless() {
        let _serial = serial();
        reset();
        assert_eq!(take_oldest(), None);
        assert_eq!(take_newest(), None);
        assert!(previews(10).is_empty());
    }

    #[test]
    fn suspend_without_capture_is_a_no_op() {
        let _serial = serial();
        reset();
        assert!(!suspend());
        // resume(false) must not start anything
        resume(false);
        assert!(!ACTIVE.load(Ordering::SeqCst));
    }
}
