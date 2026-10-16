//! Running work on untrusted metadata so that a defect it reaches (a panic)
//! or a loop it makes (a hang) ends that work, not the process: `spaces
//! attach` checks every pool of a system in one process, as root, and one
//! hostile or damaged disk must not keep the others from being attached.

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use crate::report::{Check, Evidence, Status};

/// Why work stopped before its end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stopped {
    /// It panicked (a defect the metadata reached): the message.
    Panicked(String),
    /// It did not finish in this time (a slow disk, or a loop the metadata
    /// makes).
    TimedOut(Duration),
}

impl std::fmt::Display for Stopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Stopped::Panicked(m) => write!(f, "stopped on an internal error: {m}"),
            Stopped::TimedOut(d) => write!(f, "did not finish in {} s", d.as_secs()),
        }
    }
}

impl Stopped {
    /// The check `id` that reports it for `what` ("the checks of the
    /// space"): failed after a panic (nothing reads what the code does not
    /// understand), suspect after a time-out (a slow disk is no damage, but
    /// what was not checked is read only when asked).
    pub fn check(&self, id: &str, what: &str) -> Check {
        match self {
            Stopped::Panicked(m) => Check::new(id, Status::Failed, format!("{what} stopped on an internal error"))
                .evidence(Evidence::new(what).expected("to finish").found(format!("a panic: {m}")))
                .advice(
                    "The metadata reached a defect of this program: please report it (spaces check \
                     --bundle packs the metadata of a pool for that). Nothing reads it until then.",
                ),
            Stopped::TimedOut(d) => Check::new(
                id,
                Status::Suspect,
                format!("{what} did not finish in {} s", d.as_secs()),
            )
            .evidence(
                Evidence::new(what)
                    .expected(format!("to finish in {} s", d.as_secs()))
                    .found("still running"),
            )
            .advice(
                "A slow or failing disk, or metadata that makes the checks loop. SPACES_CHECK_TIMEOUT sets \
                 the limit in seconds (0: none).",
            ),
        }
    }
}

/// The message of a panic.
fn message(payload: &(dyn Any + Send)) -> String {
    match payload.downcast_ref::<&str>() {
        Some(s) => (*s).to_owned(),
        None => payload
            .downcast_ref::<String>()
            .cloned()
            .unwrap_or_else(|| "a panic without a message".into()),
    }
}

/// Runs `work` on this thread; a panic in it is [`Stopped::Panicked`] (the
/// panic hook still reports it on standard error).
pub fn caught<T>(work: impl FnOnce() -> T) -> Result<T, Stopped> {
    catch_unwind(AssertUnwindSafe(work)).map_err(|p| Stopped::Panicked(message(&*p)))
}

/// Runs `work` on a thread of its own and waits at most `deadline` for it
/// (`None`: as long as it takes). A panic in it is [`Stopped::Panicked`];
/// past the deadline it is [`Stopped::TimedOut`], and the thread is left to
/// run until the process ends.
pub fn isolated<T: Send + 'static>(
    deadline: Option<Duration>,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Stopped> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("checks".into())
        // As much stack as the main thread has.
        .stack_size(8 << 20)
        .spawn(move || {
            let _ = tx.send(caught(work));
        })
        .map_err(|e| Stopped::Panicked(format!("no thread to run it on: {e}")))?;
    let lost = || Stopped::Panicked("the thread ended without a result".into());
    match deadline {
        Some(d) => rx.recv_timeout(d).map_err(|e| match e {
            RecvTimeoutError::Timeout => Stopped::TimedOut(d),
            RecvTimeoutError::Disconnected => lost(),
        })?,
        None => rx.recv().map_err(|_| lost())?,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::Verdict;

    #[test]
    fn a_panic_or_a_hang_stops_the_work_not_the_caller() {
        assert_eq!(isolated(Some(Duration::from_secs(5)), || 6 * 7), Ok(42));
        assert_eq!(isolated(None, || "done"), Ok("done"));
        let stopped = isolated(Some(Duration::from_secs(5)), || -> u8 { panic!("boom {}", 1) }).unwrap_err();
        assert_eq!(stopped, Stopped::Panicked("boom 1".into()));
        assert_eq!(
            isolated(None, || -> u8 { panic!("static") }),
            Err(Stopped::Panicked("static".into()))
        );
        // A loop that never ends (sleeping, so that the abandoned thread
        // costs nothing until the tests end).
        let stopped = isolated(Some(Duration::from_millis(100)), || -> u8 {
            loop {
                std::thread::sleep(Duration::from_millis(50));
            }
        })
        .unwrap_err();
        assert_eq!(stopped, Stopped::TimedOut(Duration::from_millis(100)));
        let overflow = caught(|| {
            let x = std::hint::black_box(u64::MAX);
            x.checked_add(1).expect("overflow")
        });
        assert_eq!(overflow, Err(Stopped::Panicked("overflow".into())));
    }

    #[test]
    fn a_panic_fails_and_a_time_out_is_suspect() {
        let c =
            Stopped::Panicked("attempt to add with overflow".into()).check("space.checks", "the checks of the space");
        assert_eq!((c.id.as_str(), c.status.verdict()), ("space.checks", Verdict::Failed));
        assert_eq!(c.summary, "the checks of the space stopped on an internal error");
        assert!(
            c.evidence[0]
                .found
                .as_deref()
                .unwrap()
                .contains("attempt to add with overflow")
        );
        let c = Stopped::TimedOut(Duration::from_secs(40)).check("space.checks", "the checks of the space");
        assert_eq!(c.status.verdict(), Verdict::Suspect);
        assert_eq!(c.summary, "the checks of the space did not finish in 40 s");
    }
}
