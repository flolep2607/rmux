use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::runtime::Handle;
use tokio::sync::{watch, Notify};
use tokio::task::AbortHandle;

#[derive(Clone, Debug)]
pub(crate) struct PaneReaderRuntime {
    handle: Handle,
    gate: Arc<PaneReaderGate>,
}

impl PaneReaderRuntime {
    /// Captures the long-lived server runtime used for Unix PTY reader tasks.
    ///
    /// Attached prompt commands can execute on short-lived helper runtimes; pane
    /// readers must not be spawned there because they outlive the command that
    /// created the pane.
    pub(crate) fn current() -> Option<Self> {
        Handle::try_current().ok().map(Self::from_handle)
    }

    pub(crate) fn from_handle(handle: Handle) -> Self {
        Self {
            handle,
            gate: Arc::new(PaneReaderGate::new()),
        }
    }

    pub(crate) fn spawn<F>(&self, task: F) -> AbortHandle
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.handle.spawn(task).abort_handle()
    }

    /// The gate every reader spawned on this runtime answers to.
    pub(crate) fn gate(&self) -> &Arc<PaneReaderGate> {
        &self.gate
    }
}

/// Parks every pane reader at the top of its loop, for a live handover.
///
/// A reader's loop reads from the pty, parses, writes the parser's replies
/// back and sometimes yields. Aborting the task at any of those points could
/// drop bytes that were read and not yet parsed, or replies not yet written.
/// So a reader is never stopped: it checks this gate only where it would
/// otherwise wait for the pty to become readable, and parks there. Once every
/// live reader is parked, everything read has been parsed, and everything
/// unread is still in the kernel's pty buffer, where the next image's reader
/// finds it (docs/design/live-handover.md, §2).
#[derive(Debug)]
pub(crate) struct PaneReaderGate {
    park: watch::Sender<bool>,
    /// Readers spawned and not yet finished. Counted from the spawn, not from
    /// the task's first poll, so a reader that has not started yet still
    /// holds the gate open until it parks.
    live: AtomicUsize,
    parked: AtomicUsize,
    changed: Notify,
}

impl PaneReaderGate {
    fn new() -> Self {
        let (park, _receiver) = watch::channel(false);
        Self {
            park,
            live: AtomicUsize::new(0),
            parked: AtomicUsize::new(0),
            changed: Notify::new(),
        }
    }

    /// Registers a reader about to be spawned. The ticket goes into its task,
    /// so the count drops when the task ends, aborted or not.
    pub(crate) fn ticket(self: &Arc<Self>) -> PaneReaderTicket {
        self.live.fetch_add(1, Ordering::SeqCst);
        PaneReaderTicket {
            gate: Arc::clone(self),
            park: self.park.subscribe(),
        }
    }

    /// Asks every reader to park, and waits until they all have.
    ///
    /// Returns false if `deadline` passes first; the readers stay asked to
    /// park until [`release`](Self::release).
    pub(crate) async fn park_all(&self, deadline: tokio::time::Instant) -> bool {
        self.park.send_replace(true);
        loop {
            let changed = self.changed.notified();
            if self.parked.load(Ordering::SeqCst) >= self.live.load(Ordering::SeqCst) {
                return true;
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                return self.parked.load(Ordering::SeqCst) >= self.live.load(Ordering::SeqCst);
            }
        }
    }

    /// Lets every parked reader go back to reading.
    pub(crate) fn release(&self) {
        self.park.send_replace(false);
    }

    #[cfg(test)]
    pub(crate) fn counts(&self) -> (usize, usize) {
        (
            self.live.load(Ordering::SeqCst),
            self.parked.load(Ordering::SeqCst),
        )
    }
}

/// One reader's registration with its [`PaneReaderGate`].
#[derive(Debug)]
pub(crate) struct PaneReaderTicket {
    gate: Arc<PaneReaderGate>,
    park: watch::Receiver<bool>,
}

impl PaneReaderTicket {
    /// Resolves when the gate asks this reader to park. Cancel-safe, so a
    /// reader selects it against its pty read.
    pub(crate) async fn park_requested(&mut self) {
        if self.park.wait_for(|park| *park).await.is_err() {
            // The gate is gone with its runtime; never ask to park.
            std::future::pending::<()>().await;
        }
    }

    /// Stays parked until the gate releases this reader.
    pub(crate) async fn stay_parked(&mut self) {
        let _parked = ParkedReader::new(&self.gate);
        let _ = self.park.wait_for(|park| !*park).await;
    }
}

impl Drop for PaneReaderTicket {
    fn drop(&mut self) {
        self.gate.live.fetch_sub(1, Ordering::SeqCst);
        self.gate.changed.notify_waiters();
    }
}

/// Counts a reader as parked for exactly as long as it is, including when its
/// task is aborted while parked.
struct ParkedReader<'a> {
    gate: &'a PaneReaderGate,
}

impl<'a> ParkedReader<'a> {
    fn new(gate: &'a PaneReaderGate) -> Self {
        gate.parked.fetch_add(1, Ordering::SeqCst);
        gate.changed.notify_waiters();
        Self { gate }
    }
}

impl Drop for ParkedReader<'_> {
    fn drop(&mut self) {
        self.gate.parked.fetch_sub(1, Ordering::SeqCst);
        self.gate.changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::mpsc;

    use super::PaneReaderRuntime;

    /// A stand-in reader: "reads" from a channel, and parks at the top of its
    /// loop as the real one does.
    fn spawn_reader(
        runtime: &PaneReaderRuntime,
    ) -> (mpsc::UnboundedSender<u8>, mpsc::UnboundedReceiver<u8>) {
        let (input, mut source) = mpsc::unbounded_channel::<u8>();
        let (sink, output) = mpsc::unbounded_channel::<u8>();
        let mut ticket = runtime.gate().ticket();
        runtime.spawn(async move {
            loop {
                let byte = tokio::select! {
                    biased;
                    () = ticket.park_requested() => {
                        ticket.stay_parked().await;
                        continue;
                    }
                    byte = source.recv() => byte,
                };
                let Some(byte) = byte else { return };
                let _ = sink.send(byte);
            }
        });
        (input, output)
    }

    fn soon() -> tokio::time::Instant {
        tokio::time::Instant::now() + Duration::from_secs(10)
    }

    #[tokio::test]
    async fn parked_readers_hold_their_input_until_released() {
        let runtime = PaneReaderRuntime::current().expect("runtime");
        let (first_in, mut first_out) = spawn_reader(&runtime);
        let (second_in, mut second_out) = spawn_reader(&runtime);

        first_in.send(1).expect("send");
        assert_eq!(first_out.recv().await, Some(1));

        assert!(runtime.gate().park_all(soon()).await);
        assert_eq!(runtime.gate().counts(), (2, 2));
        first_in.send(2).expect("send");
        second_in.send(3).expect("send");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            first_out.try_recv().is_err(),
            "a parked reader reads nothing"
        );
        assert!(
            second_out.try_recv().is_err(),
            "a parked reader reads nothing"
        );

        runtime.gate().release();
        assert_eq!(first_out.recv().await, Some(2), "nothing was lost");
        assert_eq!(second_out.recv().await, Some(3), "nothing was lost");
    }

    #[tokio::test]
    async fn a_reader_that_ends_or_is_aborted_stops_counting() {
        let runtime = PaneReaderRuntime::current().expect("runtime");
        let (ending, mut ending_out) = spawn_reader(&runtime);
        drop(ending);
        assert_eq!(ending_out.recv().await, None, "the reader has ended");
        let ticket = runtime.gate().ticket();
        let aborted = runtime.spawn(async move {
            let _ticket = ticket;
            std::future::pending::<()>().await;
        });
        aborted.abort();
        assert!(runtime.gate().park_all(soon()).await);
        assert_eq!(runtime.gate().counts(), (0, 0));
        runtime.gate().release();
    }

    #[tokio::test]
    async fn a_reader_that_never_parks_runs_the_gate_to_its_deadline() {
        let runtime = PaneReaderRuntime::current().expect("runtime");
        // Registered, but never polls its ticket: a reader stuck mid-loop.
        let _stuck = runtime.gate().ticket();
        let (_input, _output) = spawn_reader(&runtime);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(50);
        assert!(!runtime.gate().park_all(deadline).await);
        assert_eq!(runtime.gate().counts(), (2, 1));
        runtime.gate().release();
    }
}
