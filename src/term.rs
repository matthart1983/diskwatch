//! Raw mode and the alternate screen, and getting out of both. The terminal
//! comes back after a normal quit, an error, a panic on the UI thread, and
//! SIGTERM, SIGINT, SIGHUP or SIGQUIT. Up to 0.7.1 each of those signals
//! left the shell raw and on the alternate screen, and so did a panic.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::thread::ThreadId;

use crossterm::cursor::Show;
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};

/// Set once the terminal is back to normal. The panic hook and the
/// session's drop both run on a panic, and leaving the alternate screen a
/// second time can move the cursor back over the panic message.
static RESTORED: AtomicBool = AtomicBool::new(true);
static UI_THREAD: OnceLock<ThreadId> = OnceLock::new();

/// Raw mode and the alternate screen for as long as it lives.
pub struct Session(());

impl Session {
    pub fn enter() -> io::Result<Self> {
        install_panic_hook();
        #[cfg(unix)]
        signals::install();
        enable_raw_mode()?;
        RESTORED.store(false, Ordering::SeqCst);
        // Made before the alternate screen, so a failure there still
        // turns raw mode off.
        let session = Session(());
        execute!(io::stdout(), EnterAlternateScreen)?;
        Ok(session)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        restore();
    }
}

fn restore() {
    if RESTORED.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
}

fn install_panic_hook() {
    if UI_THREAD.set(std::thread::current().id()).is_err() {
        return;
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // A collector thread that panics leaves the UI drawing, so only
        // the UI thread hands the terminal back. Restoring first puts the
        // message on the normal screen, where it stays readable.
        if UI_THREAD.get() == Some(&std::thread::current().id()) {
            restore();
        }
        previous(info);
    }));
}

/// The signal that asked diskwatch to stop, if one has. The main loop
/// checks this every frame.
pub fn stop_signal() -> Option<i32> {
    #[cfg(unix)]
    {
        signals::caught()
    }
    #[cfg(not(unix))]
    {
        None
    }
}

#[cfg(unix)]
mod signals {
    use std::sync::atomic::{AtomicI32, Ordering};
    use std::time::Duration;

    const STOP: [libc::c_int; 4] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGQUIT];

    /// How long the main loop gets to see a signal before the watchdog
    /// restores the terminal and exits for it. The loop notices within a
    /// frame, unless a dead mount has it stuck in a syscall (#34). Before
    /// these handlers, SIGTERM still ended a stuck diskwatch, and the
    /// watchdog keeps that true.
    const GRACE: Duration = Duration::from_secs(1);

    /// The first stop signal received, or 0.
    static CAUGHT: AtomicI32 = AtomicI32::new(0);
    /// Write end of the pipe that wakes the watchdog.
    static WAKE: AtomicI32 = AtomicI32::new(-1);

    pub(super) fn caught() -> Option<i32> {
        match CAUGHT.load(Ordering::SeqCst) {
            0 => None,
            sig => Some(sig),
        }
    }

    extern "C" fn on_signal(sig: libc::c_int) {
        // Async-signal-safe calls only: atomics and write(2).
        let _ = CAUGHT.compare_exchange(0, sig, Ordering::SeqCst, Ordering::SeqCst);
        let fd = WAKE.load(Ordering::SeqCst);
        if fd >= 0 {
            let byte = 1u8;
            // SAFETY: fd is the pipe's write end, open for the process's
            // life, and the buffer is one valid byte.
            unsafe { libc::write(fd, (&byte as *const u8).cast(), 1) };
        }
    }

    pub(super) fn install() {
        if WAKE.load(Ordering::SeqCst) >= 0 {
            return;
        }
        let mut fds = [-1 as libc::c_int; 2];
        // SAFETY: fds has room for the two descriptors pipe(2) writes.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            // Without a watchdog a handler could leave a stuck diskwatch
            // unkillable by SIGTERM, so keep the default dispositions.
            return;
        }
        for fd in fds {
            // SAFETY: fd was just returned by pipe(2). smartctl and zpool
            // children shouldn't inherit it.
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        }
        let [read_end, write_end] = fds;
        let spawned = std::thread::Builder::new()
            .name("signal-watchdog".into())
            .spawn(move || watchdog(read_end));
        if spawned.is_err() {
            // SAFETY: both fds are ours and nothing else holds them yet.
            unsafe {
                libc::close(read_end);
                libc::close(write_end);
            }
            return;
        }
        WAKE.store(write_end, Ordering::SeqCst);
        for sig in STOP {
            // SAFETY: a zeroed sigaction is a valid starting value, and
            // on_signal only touches atomics and calls write(2).
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = on_signal as *const () as usize;
                action.sa_flags = libc::SA_RESTART;
                libc::sigemptyset(&mut action.sa_mask);
                libc::sigaction(sig, &action, std::ptr::null_mut());
            }
        }
    }

    /// Sleeps in read(2) until a signal arrives, then gives the main loop
    /// [`GRACE`] to quit before doing it itself.
    fn watchdog(fd: libc::c_int) {
        let mut byte = 0u8;
        loop {
            // SAFETY: fd is the pipe's read end and byte is one valid byte.
            let n = unsafe { libc::read(fd, (&mut byte as *mut u8).cast(), 1) };
            if n == 1 {
                break;
            }
            if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        std::thread::sleep(GRACE);
        // Still running, so the main thread is stuck. Write the escapes
        // straight to fd 1: it may hold the stdout lock.
        if !super::RESTORED.swap(true, Ordering::SeqCst) {
            let _ = crossterm::terminal::disable_raw_mode();
            let leave = b"\x1b[?1049l\x1b[?25h";
            // SAFETY: fd 1 and a valid byte buffer.
            unsafe { libc::write(1, leave.as_ptr().cast(), leave.len()) };
        }
        let sig = caught().unwrap_or(libc::SIGTERM);
        // SAFETY: _exit ends the process at once, without running
        // destructors that could block on the same stuck mount.
        unsafe { libc::_exit(128 + sig) };
    }
}
