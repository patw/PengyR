//! SIGINT / SIGTERM / SIGHUP during a turn.
//!
//! pengy-cli is mostly run by other programs as a one-shot subagent, and a
//! parent that gives up on it sends SIGTERM (or SIGINT/SIGHUP).  With the
//! default actions the process died on the spot: the running tool's process
//! group (its own session, so the signal never reached it) kept running, a sudo
//! askpass password file stayed in the temp directory, and the dangling
//! tool_calls repair never ran.  Ctrl-C in the REPL killed the CLI too.
//!
//! While a turn runs, [`TurnGuard`] routes those signals here instead:
//!
//! - the handler only records the signal and writes a byte to a pipe;
//! - a watcher thread then kills the tool's process group, removes askpass
//!   files, and -- when the signal ends the process -- re-raises it if the main
//!   thread has not finished within [`GRACE`] (it can be blocked in a prompt);
//! - `drive_chat` polls [`pending`], abandons the turn, repairs and saves the
//!   chat, then returns to the prompt (interactive Ctrl-C) or calls [`die`].
//!
//! [`die`] re-raises the signal with its default action, so the parent sees
//! exactly what it saw before this existed (killed by SIGTERM, status 143).  A
//! second signal while the first is handled ends the process at once.  Keep
//! identical in the Python and C++ editions.

use pengy_core::tools::{self, ToolContext};
use std::sync::Arc;

#[cfg(unix)]
mod imp {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    /// The main thread gets this long after the tool is killed to repair,
    /// save and exit on its own before the watcher ends the process.
    const GRACE: Duration = Duration::from_secs(1);
    const SIGNALS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

    static PENDING: AtomicI32 = AtomicI32::new(0);
    static PIPE_WR: AtomicI32 = AtomicI32::new(-1);
    /// Single-shot: SIGINT ends the process like SIGTERM does.
    static SIGINT_TERMINATES: AtomicBool = AtomicBool::new(false);
    static WATCHER: OnceLock<()> = OnceLock::new();
    static CONTEXT: Mutex<Option<Arc<ToolContext>>> = Mutex::new(None);
    /// The terminal as the turn found it.  Dying from the watcher while a
    /// prompt holds the terminal raw (or a sudo prompt has echo off) must not
    /// leave the user's shell that way.
    static SAVED_TERMIOS: Mutex<Option<libc::termios>> = Mutex::new(None);

    extern "C" fn on_signal(sig: libc::c_int) {
        // Async-signal-safe only: an atomic, write(2), signal(2), raise(3).
        if PENDING.compare_exchange(0, sig, Ordering::SeqCst, Ordering::SeqCst).is_err() {
            unsafe {
                libc::signal(sig, libc::SIG_DFL);
                libc::raise(sig);
            }
            return;
        }
        let fd = PIPE_WR.load(Ordering::SeqCst);
        if fd >= 0 {
            let byte = sig as u8;
            unsafe {
                libc::write(fd, &byte as *const u8 as *const libc::c_void, 1);
            }
        }
    }

    fn start_watcher() {
        WATCHER.get_or_init(|| {
            let mut fds = [0 as libc::c_int; 2];
            if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
                return;
            }
            for fd in fds {
                // Tool children must not inherit the pipe.
                unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
            }
            PIPE_WR.store(fds[1], Ordering::SeqCst);
            let rd = fds[0];
            std::thread::spawn(move || loop {
                let mut byte = 0u8;
                let n = unsafe { libc::read(rd, &mut byte as *mut u8 as *mut libc::c_void, 1) };
                if n <= 0 {
                    if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return;
                }
                let sig = PENDING.load(Ordering::SeqCst);
                if sig == 0 {
                    continue;
                }
                if let Some(ctx) = CONTEXT.lock().unwrap_or_else(|e| e.into_inner()).clone() {
                    ctx.kill_all();
                }
                tools::cleanup_askpass_helpers();
                if terminates(sig) {
                    std::thread::sleep(GRACE);
                    die(sig);
                }
            });
        });
    }

    /// Installs the turn's signal handlers; dropping it restores the previous ones.
    pub struct TurnGuard {
        old: Vec<(libc::c_int, libc::sigaction)>,
    }

    impl TurnGuard {
        pub fn install(ctx: Arc<ToolContext>, single_shot: bool) -> Self {
            start_watcher();
            *CONTEXT.lock().unwrap_or_else(|e| e.into_inner()) = Some(ctx);
            SIGINT_TERMINATES.store(single_shot, Ordering::SeqCst);
            PENDING.store(0, Ordering::SeqCst);
            *SAVED_TERMIOS.lock().unwrap_or_else(|e| e.into_inner()) = unsafe {
                let mut t: libc::termios = std::mem::zeroed();
                (libc::isatty(0) == 1 && libc::tcgetattr(0, &mut t) == 0).then_some(t)
            };
            let mut old = Vec::new();
            for sig in SIGNALS {
                unsafe {
                    let mut action: libc::sigaction = std::mem::zeroed();
                    action.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as usize;
                    action.sa_flags = libc::SA_RESTART;
                    libc::sigemptyset(&mut action.sa_mask);
                    let mut prev: libc::sigaction = std::mem::zeroed();
                    if libc::sigaction(sig, &action, &mut prev) == 0 {
                        old.push((sig, prev));
                    }
                }
            }
            TurnGuard { old }
        }
    }

    impl Drop for TurnGuard {
        fn drop(&mut self) {
            for (sig, prev) in &self.old {
                unsafe { libc::sigaction(*sig, prev, std::ptr::null_mut()) };
            }
        }
    }

    /// The signal received during this turn, or 0.
    pub fn pending() -> i32 {
        PENDING.load(Ordering::SeqCst)
    }

    /// Whether *sig* ends the process (rather than just the turn).
    pub fn terminates(sig: i32) -> bool {
        sig != libc::SIGINT || SIGINT_TERMINATES.load(Ordering::SeqCst)
    }

    pub fn name(sig: i32) -> String {
        match sig {
            libc::SIGINT => "SIGINT".into(),
            libc::SIGTERM => "SIGTERM".into(),
            libc::SIGHUP => "SIGHUP".into(),
            n => format!("signal {n}"),
        }
    }

    /// Exit as if *sig* had killed the process, after removing askpass files.
    pub fn die(sig: i32) -> ! {
        tools::cleanup_askpass_helpers();
        if let Ok(saved) = SAVED_TERMIOS.try_lock() {
            if let Some(t) = saved.as_ref() {
                unsafe { libc::tcsetattr(0, libc::TCSANOW, t) };
            }
        }
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
            libc::raise(sig);
        }
        std::process::exit(128 + sig);
    }
}

#[cfg(not(unix))]
mod imp {
    use super::*;

    pub struct TurnGuard;

    impl TurnGuard {
        pub fn install(_ctx: Arc<ToolContext>, _single_shot: bool) -> Self {
            TurnGuard
        }
    }

    pub fn pending() -> i32 {
        0
    }

    pub fn terminates(_sig: i32) -> bool {
        true
    }

    pub fn name(sig: i32) -> String {
        format!("signal {sig}")
    }

    pub fn die(sig: i32) -> ! {
        tools::cleanup_askpass_helpers();
        std::process::exit(128 + sig);
    }
}

pub use imp::{die, name, pending, terminates, TurnGuard};
