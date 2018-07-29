extern crate libc;
extern crate nix;

use self::libc::{pthread_kill, SIGPROF};
use self::nix::sys::signal::{sigaction, Signal};
use self::nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet};
use std::cell::UnsafeCell;
use std::error;
use std::fs;
use std::io;
use std::iter;
use std::mem;
use std::os::unix::thread::JoinHandleExt;
use std::process;
use std::ptr;
use std::sync::Arc;
use std::sync::Barrier;
use std::thread::spawn;
use std::thread::Thread;

/// wraps a POSIX semaphore
///
/// We need to use these as only sem_post is required to be signal safe.
struct PosixSemaphore {
    sem: UnsafeCell<libc::sem_t>,
}

impl PosixSemaphore {
    /// Returns a new semaphore if initialization succeeded.
    ///
    /// TODO: Consider exposing error code.
    pub fn new(value: u32) -> io::Result<PosixSemaphore> {
        let mut sem: libc::sem_t = unsafe { mem::uninitialized() };
        let r = unsafe {
            libc::sem_init(&mut sem, 0 /* not shared */, value)
        };
        if r == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(PosixSemaphore {
            sem: UnsafeCell::new(sem),
        })
    }

    pub fn post(&self) -> io::Result<()> {
        if unsafe { libc::sem_post(self.sem.get()) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub fn wait(&self) -> io::Result<()> {
        if unsafe { libc::sem_wait(self.sem.get()) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub fn wait_through_intr(&self) -> io::Result<()> {
        loop {
            match self.wait() {
                Err(os_error) => {
                    let err = os_error.raw_os_error().expect("os error");
                    if err == libc::EINTR {
                        continue;
                    }
                    return Err(os_error);
                }
                _ => return Ok(()),
            }
        }
    }
}

unsafe impl Sync for PosixSemaphore {}

impl Drop for PosixSemaphore {
    fn drop(&mut self) {
        unsafe { libc::sem_destroy(self.sem.get()) };
    }
}

/// Iterates over task threads by reading /proc.
pub fn thread_iterator() -> io::Result<impl Iterator<Item = io::Result<libc::pid_t>>> {
    fs::read_dir("/proc/self/task").map(|r| {
        r.map(|entry| {
            entry.map(|dir_entry| {
                let file = dir_entry.file_name().into_string().expect("valid utf8");
                file.parse::<libc::pid_t>().expect("tid should be pid_t")
            })
        })
    })
}

struct SharedState {
    // "msg1" is the signal.
    msg2: Option<PosixSemaphore>,
    msg3: Option<PosixSemaphore>,
    msg4: Option<PosixSemaphore>,
    context: Option<libc::ucontext_t>,
}

// TODO: Think about how we can use some rust typisms to make this cleaner.
static mut shared_state: SharedState = SharedState {
    msg2: None,
    msg3: None,
    msg4: None,
    context: None,
};

fn clear_shared_state() {
    unsafe {
        shared_state.msg2 = None;
        shared_state.msg3 = None;
        shared_state.msg4 = None;
        shared_state.context = None;
    }
}

fn reset_shared_state() {
    unsafe {
        shared_state.msg2 = Some(PosixSemaphore::new(0).expect("valid semaphore"));
        shared_state.msg3 = Some(PosixSemaphore::new(0).expect("valid semaphore"));
        shared_state.msg4 = Some(PosixSemaphore::new(0).expect("valid semaphore"));
        shared_state.context = None;
    }
}

/// Set's up the SIGPROF handler.
///
/// Dropping this reset's the handler.
pub struct Sampler {
    old_handler: SigAction,
}

impl Sampler {
    pub fn new() -> Sampler {
        let handler = SigHandler::SigAction(sigprof_handler);
        let action = SigAction::new(
            handler,
            SaFlags::SA_RESTART | SaFlags::SA_SIGINFO,
            SigSet::empty(),
        );
        let old = unsafe { sigaction(Signal::SIGPROF, &action).expect("signal handler set") };

        Sampler { old_handler: old }
    }

    pub fn suspend_and_resume_thread<F>(&self, tid: libc::pid_t, callback: F) -> ()
    where
        F: Fn(&mut libc::ucontext_t) -> (),
    {
        // TODO: In particular, this should ensure that we only call it after the SIGPROF handler has
        // been registered correctly.

        // first we reinitialize the semaphores
        reset_shared_state();

        // signal the thread, wait for it to tell us state was copied.
        send_sigprof(tid);
        unsafe {
            shared_state
                .msg2
                .as_ref()
                .unwrap()
                .wait_through_intr()
                .expect("msg2 wait succeeded");
        }

        unsafe { callback(&mut shared_state.context.expect("valid context")) };

        // signal the thread to continue.
        unsafe {
            shared_state.msg3.as_ref().unwrap().post();
        }

        // wait for thread to continue.
        unsafe {
            shared_state
                .msg4
                .as_ref()
                .unwrap()
                .wait_through_intr()
                .expect("msg4 wait succeeded");
        }

        clear_shared_state();
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        unsafe {
            sigaction(Signal::SIGPROF, &self.old_handler).expect("previous signal handler restored")
        };
    }
}

extern "C" fn sigprof_handler(
    sig: libc::c_int,
    info: *mut libc::siginfo_t,
    ctx: *mut libc::c_void,
) {
    assert_eq!(sig, libc::SIGPROF);
    unsafe {
        // copy the context.
        let context: libc::ucontext_t = *(ctx as *mut libc::ucontext_t);
        shared_state.context = Some(context);
        // Tell the sampler we copied the context.
        shared_state.msg2.as_ref().unwrap().post();

        // Wait for sampling to finish.
        shared_state
            .msg3
            .as_ref()
            .unwrap()
            .wait_through_intr()
            .expect("msg3 wait succeeded");

        // OK we are done!
        shared_state.msg4.as_ref().unwrap().post();
        // DO NOT TOUCH shared state here onwards.
    }
}

/// `to` is a Linux task ID.
fn send_sigprof(to: libc::pid_t) {
    unsafe {
        libc::syscall(libc::SYS_tgkill, process::id(), to, libc::SIGPROF);
    }
}

/// TODO: Next step is to add criterion based benchmarks.

// WARNING WARNING WARNING WARNING WARNING
//
// These tests MUST be run sequentially (`cargo test -- --test-threads 1`) as they install signal
// handlers that are process-wide!
#[cfg(test)]
mod tests {
    extern crate libc;
    extern crate nix;
    extern crate rustc_demangle;
    extern crate std;
    extern crate unwind_sys;

    use super::*;

    use self::rustc_demangle::demangle;
    use self::unwind_sys::*;
    use std::sync::mpsc::channel;

    static mut signal_received: bool = false;

    extern "C" fn acknowledge_sigprof(
        sig: libc::c_int,
        info: *mut libc::siginfo_t,
        ctx: *mut libc::c_void,
    ) {
        assert_eq!(sig, libc::SIGPROF);
        unsafe {
            signal_received = true;
        }
    }

    #[test]
    fn test_sigprof() {
        let handler = SigHandler::SigAction(acknowledge_sigprof);
        let action = SigAction::new(
            handler,
            SaFlags::SA_RESTART | SaFlags::SA_SIGINFO,
            SigSet::empty(),
        );
        unsafe {
            sigaction(Signal::SIGPROF, &action).expect("signal handler set");
        }

        let (tx, rx) = channel();
        // Just to get the thread to wait until the signal is sent.
        let (tx2, rx2) = channel();
        let handle = spawn(move || {
            let tid = unsafe { libc::syscall(libc::SYS_gettid) as libc::pid_t };
            tx.send(tid).unwrap();
            rx2.recv().unwrap();
        });

        let to = rx.recv().unwrap();
        send_sigprof(to);
        tx2.send(()).unwrap();
        handle.join().expect("successful join");
        unsafe {
            assert!(signal_received);
        }
    }

    #[test]
    fn test_semaphore() {
        let semaphore = Arc::new(PosixSemaphore::new(0).expect("init"));
        let semaphoret = semaphore.clone();

        let handle = spawn(move || {
            semaphoret.post();
        });

        semaphore.wait();
        handle.join().expect("successful join");
    }

    #[test]
    fn test_thread_iterator() {
        let tid = unsafe { libc::syscall(libc::SYS_gettid) as libc::pid_t };
        let tasks: Vec<libc::pid_t> = thread_iterator()
            .expect("threads")
            .map(|x| x.expect("tid listed"))
            .collect();
        assert!(tasks.contains(&tid));
    }

    #[test]
    fn test_suspend_resume() {
        let sampler = Sampler::new();
        let (tx, rx) = channel();
        // Just to get the thread to wait until the test is done.
        let (tx2, rx2) = channel();
        let handle = spawn(move || {
            let tid = unsafe { libc::syscall(libc::SYS_gettid) as libc::pid_t };
            tx.send(tid).unwrap();
            rx2.recv().unwrap();
        });

        let to = rx.recv().unwrap();
        sampler.suspend_and_resume_thread(to, |context| unsafe {
            // TODO: This is where we would want to use libunwind in a real program.
            assert!(context.uc_stack.ss_size > 0);

            // we can tell the thread to shutdown once it is resumed.
            tx2.send(()).unwrap();
        });

        handle.join().unwrap();
        // make sure we cleaned up.
        unsafe {
            assert!(shared_state.context.is_none());
        }
    }

    #[test]
    #[ignore] // Useful for playing around, but not required.
    fn test_suspend_resume_unwind() {
        let sampler = Sampler::new();
        let (tx, rx) = channel();
        // Just to get the thread to wait until the test is done.
        let (tx2, rx2) = channel();
        let handle = spawn(move || {
            let tid = unsafe { libc::syscall(libc::SYS_gettid) as libc::pid_t };
            let baz = || {
                tx.send(tid).unwrap();
                rx2.recv().unwrap();
            };
            let bar = || {
                baz();
            };

            let foo = || {
                bar();
            };

            foo();
        });

        let to = rx.recv().unwrap();
        sampler.suspend_and_resume_thread(to, |context| unsafe {
            // TODO: This is where we would want to use libunwind in a real program.
            assert!(context.uc_stack.ss_size > 0);

            let mut cursor: unw_cursor_t = mem::uninitialized();
            let mut offset = 0;
            // A unw_context_t is an alias to the ucontext_t as clarified by the docs, so we can
            // use the signal context.
            unw_init_local(&mut cursor, context);
            while unw_step(&mut cursor) > 0 {
                let mut buf = vec![0; 256];
                // This won't actually work in non-debug info ELFs.
                // Plus it hurts timing.
                let r = unw_get_proc_name(
                    &mut cursor,
                    buf.as_mut_ptr() as *mut i8,
                    buf.len(),
                    &mut offset,
                );
                if r < 0 {
                    eprintln!("error {}", r);
                } else {
                    let len = buf.iter().position(|b| *b == 0).unwrap();
                    buf.truncate(len);
                    let name = String::from_utf8_lossy(&buf).into_owned();
                    eprintln!("fn {:#}", demangle(&name));
                }
            }

            // we can tell the thread to shutdown once it is resumed.
            tx2.send(()).unwrap();
        });

        handle.join().unwrap();
        // make sure we cleaned up.
        unsafe {
            assert!(shared_state.context.is_none());
        }
    }
}
