//! PTY ownership independent of the UI. Only terminal I/O workers call the
//! deferred streams; setup, control operations and destruction use smol's pool.

use anyhow::{Context, Result};
use parking_lot::{Condvar, Mutex};
use portable_pty::{Child, CommandBuilder, ExitStatus, MasterPty, PtyPair, PtySize};
use smol::channel::{Receiver, Sender};
use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const CONTROL_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Debug)]
pub enum PtyEvent {
    Ready {
        pid: Option<u32>,
    },
    /// This is the actual child's status, never a status inferred from EOF.
    Exited(ExitStatus),
    Error(String),
}

/// Construct inside the closure passed to `PendingPty::spawn`. This guard also
/// cleans up a child if extracting either of its streams fails.
pub struct PreparedPty {
    resources: Option<Resources>,
}

impl PreparedPty {
    pub fn spawn(pair: PtyPair, command: CommandBuilder) -> Result<Self> {
        let PtyPair { master, slave } = pair;
        let child = slave.spawn_command(command);
        let child = match child {
            Ok(child) => child,
            Err(error) => {
                drop(slave);
                return Err(error.context("spawning PTY child"));
            }
        };
        let pid = child.process_id();
        let mut prepared = Self {
            resources: Some(Resources {
                child: Some(child),
                master,
                reader: None,
                writer: None,
                pid,
                #[cfg(unix)]
                poll_fd: None,
            }),
        };
        let resources = prepared.resources.as_mut().unwrap();
        let extraction = (|| -> Result<()> {
            resources.writer = Some(
                resources
                    .master
                    .take_writer()
                    .context("taking PTY writer")?,
            );
            resources.reader = Some(
                resources
                    .master
                    .try_clone_reader()
                    .context("cloning PTY reader")?,
            );
            #[cfg(unix)]
            {
                resources.poll_fd = PollFd::new(resources.master.as_raw_fd())?;
            }
            Ok(())
        })();
        // Retaining this parent-side slave prevents the reader observing EOF.
        drop(slave);
        extraction?;
        Ok(prepared)
    }

    pub fn pid(&self) -> Option<u32> {
        self.resources.as_ref().and_then(|resources| resources.pid)
    }
}

impl Drop for PreparedPty {
    fn drop(&mut self) {
        if let Some(resources) = self.resources.take() {
            on_worker(move || {
                let _ = resources.cleanup();
            });
        }
    }
}

pub struct PendingPty {
    pub reader: DeferredReader,
    pub writer: DeferredWriter,
    pub control: PtyControl,
    pub events: Receiver<PtyEvent>,
}

impl PendingPty {
    /// Returns without running `prepare` or waiting for any PTY operation.
    /// Keep a control alive for the pane lifetime and continuously consume events.
    /// Dropping every event receiver also cancels an unfinished/live session.
    pub fn spawn<F>(prepare: F) -> Self
    where
        F: FnOnce() -> Result<PreparedPty> + Send + 'static,
    {
        let (wake, wake_rx) = smol::channel::bounded(1);
        // Ready, one exit and at most two failure diagnostics; no output here.
        let (events, event_rx) = smol::channel::bounded(4);
        let shared = Arc::new(Shared {
            closed: AtomicBool::new(false),
            resize: Mutex::new(None),
            wake,
            reader: Slot::new(),
            writer: Slot::new(),
        });
        let worker = shared.clone();
        smol::spawn(async move {
            let prepared = smol::unblock(move || {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(prepare))
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("PTY preparation panicked")))
            })
            .await;
            let mut resources = match prepared {
                Ok(mut prepared) => prepared.resources.take().unwrap(),
                Err(error) => {
                    let message = format!("{error:#}");
                    worker.reader.fail(message.clone());
                    worker.writer.fail(message.clone());
                    let _ = events.try_send(PtyEvent::Error(message));
                    worker.close();
                    return;
                }
            };
            let mut ready = false;
            while !worker.closed.load(Ordering::Acquire) && !events.is_closed() {
                // The UI uses only this tiny mailbox lock, never a lock held over
                // resize, polling, stream I/O, termination or destruction.
                let _ = wake_rx.try_recv();
                let size = worker.resize.lock().take();
                let (returned, outcome) = if resources.child.is_some() || size.is_some() {
                    smol::unblock(move || {
                        let outcome = resources.poll(size);
                        (resources, outcome)
                    })
                    .await
                } else {
                    (resources, Ok(None))
                };
                resources = returned;
                if worker.closed.load(Ordering::Acquire) || events.is_closed() {
                    break;
                }
                let status = match outcome {
                    Ok(status) => status,
                    Err(error) => {
                        let _ = events.try_send(PtyEvent::Error(error));
                        break;
                    }
                };
                if !ready {
                    worker.reader.install(Endpoint {
                        io: resources.reader.take().unwrap(),
                        #[cfg(unix)]
                        poll_fd: resources.poll_fd.clone(),
                    });
                    worker.writer.install(Endpoint {
                        io: resources.writer.take().unwrap(),
                        #[cfg(unix)]
                        poll_fd: resources.poll_fd.clone(),
                    });
                    let _ = events.try_send(PtyEvent::Ready { pid: resources.pid });
                    ready = true;
                }
                if let Some(status) = status {
                    let _ = events.try_send(PtyEvent::Exited(status));
                }
                let child_running = resources.child.is_some();
                smol::future::race(
                    async {
                        let _ = wake_rx.recv().await;
                    },
                    smol::future::race(
                        async {
                            if child_running {
                                smol::Timer::after(CONTROL_INTERVAL).await;
                            } else {
                                std::future::pending::<()>().await;
                            }
                        },
                        events.closed(),
                    ),
                )
                .await;
            }
            worker.close();
            let (status, cleanup) = smol::unblock(move || {
                // Kill/reap before dropping the writer: portable-pty's writer
                // destructor can itself write to a full input buffer.
                let result = resources.cleanup();
                worker.reader.discard();
                worker.writer.discard();
                result
            })
            .await;
            if let Some(status) = status {
                let _ = events.try_send(PtyEvent::Exited(status));
            }
            if let Err(error) = cleanup {
                let _ = events.try_send(PtyEvent::Error(error));
            }
        })
        .detach();
        Self {
            reader: DeferredReader {
                shared: shared.clone(),
            },
            writer: DeferredWriter {
                shared: shared.clone(),
            },
            control: PtyControl(Arc::new(Lifetime { shared })),
            events: event_rx,
        }
    }
}

/// The only strong lifetime token. Streams and background tasks deliberately do
/// not own this token, so a terminal's resize callback can own it without cycles.
#[derive(Clone)]
pub struct PtyControl(Arc<Lifetime>);

impl PtyControl {
    pub fn resize(&self, size: PtySize) {
        if !self.0.shared.closed.load(Ordering::Acquire) {
            *self.0.shared.resize.lock() = Some(size);
            let _ = self.0.shared.wake.try_send(());
        }
    }

    pub fn close(&self) {
        self.0.shared.close();
    }
}

struct Lifetime {
    shared: Arc<Shared>,
}

impl Drop for Lifetime {
    fn drop(&mut self) {
        self.shared.close();
    }
}

struct Shared {
    closed: AtomicBool,
    resize: Mutex<Option<PtySize>>,
    wake: Sender<()>,
    reader: Slot<Box<dyn Read + Send>>,
    writer: Slot<Box<dyn Write + Send>>,
}

impl Shared {
    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.reader.changed.notify_all();
        self.writer.changed.notify_all();
        let _ = self.wake.try_send(());
    }
}

pub struct DeferredReader {
    shared: Arc<Shared>,
}

impl Read for DeferredReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        self.shared
            .reader
            .with_ready(&self.shared.closed, |endpoint| {
                endpoint.perform(&self.shared.closed, Interest::Read, |reader| {
                    reader.read(bytes)
                })
            })
    }
}

pub struct DeferredWriter {
    shared: Arc<Shared>,
}

impl Write for DeferredWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        self.shared
            .writer
            .with_ready(&self.shared.closed, |endpoint| {
                endpoint.perform(&self.shared.closed, Interest::Write, |writer| {
                    writer.write(bytes)
                })
            })
    }

    fn flush(&mut self) -> io::Result<()> {
        self.shared
            .writer
            .with_ready(&self.shared.closed, |endpoint| {
                endpoint.perform(&self.shared.closed, Interest::Write, |writer| {
                    writer.flush()
                })
            })
    }
}

// Slots retain ownership even while terminal workers borrow the streams. Their
// final destruction is offloaded even if an unused deferred stream dies on UI.
struct Slot<T: Send + 'static> {
    state: Mutex<SlotState<T>>,
    changed: Condvar,
}

enum SlotState<T> {
    Pending,
    Ready(Endpoint<T>),
    Failed(String),
}

impl<T: Send + 'static> Slot<T> {
    fn new() -> Self {
        Self {
            state: Mutex::new(SlotState::Pending),
            changed: Condvar::new(),
        }
    }

    fn install(&self, endpoint: Endpoint<T>) {
        *self.state.lock() = SlotState::Ready(endpoint);
        self.changed.notify_all();
    }

    fn fail(&self, error: String) {
        *self.state.lock() = SlotState::Failed(error);
        self.changed.notify_all();
    }

    fn with_ready<R>(
        &self,
        closed: &AtomicBool,
        operation: impl FnOnce(&mut Endpoint<T>) -> io::Result<R>,
    ) -> io::Result<R> {
        let mut state = self.state.lock();
        loop {
            if let SlotState::Failed(error) = &*state {
                return Err(io::Error::other(error.clone()));
            }
            if closed.load(Ordering::Acquire) {
                return Err(closed_error());
            }
            match &mut *state {
                SlotState::Ready(endpoint) => return operation(endpoint),
                SlotState::Pending => {
                    // close() never acquires an I/O lock. A timeout also covers
                    // cancellation between the atomic check and this wait.
                    self.changed.wait_for(&mut state, CONTROL_INTERVAL);
                }
                SlotState::Failed(_) => unreachable!(),
            }
        }
    }

    fn discard(&self) {
        let previous = std::mem::replace(&mut *self.state.lock(), SlotState::Pending);
        drop(previous);
        self.changed.notify_all();
    }
}

impl<T: Send + 'static> Drop for Slot<T> {
    fn drop(&mut self) {
        if let SlotState::Ready(endpoint) =
            std::mem::replace(self.state.get_mut(), SlotState::Pending)
        {
            on_worker(move || drop(endpoint));
        }
    }
}

struct Endpoint<T> {
    io: T,
    #[cfg(unix)]
    poll_fd: Option<Arc<PollFd>>,
}

#[derive(Clone, Copy)]
enum Interest {
    Read,
    Write,
}

impl<T> Endpoint<T> {
    fn perform<R>(
        &mut self,
        closed: &AtomicBool,
        interest: Interest,
        mut operation: impl FnMut(&mut T) -> io::Result<R>,
    ) -> io::Result<R> {
        #[cfg(not(unix))]
        let _ = interest;
        loop {
            if closed.load(Ordering::Acquire) {
                return Err(closed_error());
            }
            match operation(&mut self.io) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    #[cfg(unix)]
                    if let Some(fd) = &self.poll_fd {
                        fd.wait(interest, closed)?;
                        continue;
                    }
                    return Err(error);
                }
                result => return result,
            }
        }
    }
}

fn closed_error() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "PTY session is closed")
}

fn on_worker(work: impl FnOnce() + Send + 'static) {
    smol::spawn(async move { smol::unblock(work).await }).detach();
}

struct Resources {
    child: Option<Box<dyn Child + Send + Sync>>,
    master: Box<dyn MasterPty + Send>,
    reader: Option<Box<dyn Read + Send>>,
    writer: Option<Box<dyn Write + Send>>,
    pid: Option<u32>,
    #[cfg(unix)]
    poll_fd: Option<Arc<PollFd>>,
}

impl Resources {
    fn poll(&mut self, size: Option<PtySize>) -> std::result::Result<Option<ExitStatus>, String> {
        if let Some(size) = size {
            self.master
                .resize(size)
                .map_err(|error| format!("resizing PTY: {error:#}"))?;
        }
        if let Some(child) = self.child.as_mut() {
            if let Some(status) = child
                .try_wait()
                .map_err(|error| format!("waiting for PTY child: {error}"))?
            {
                self.child = None;
                return Ok(Some(status));
            }
        }
        Ok(None)
    }

    fn cleanup(mut self) -> (Option<ExitStatus>, std::result::Result<(), String>) {
        let mut errors = Vec::new();
        let mut status = None;
        // If a previous poll reaped the child, do not signal a possibly reused
        // PID. The foreground group is validated against this terminal instead.
        if let Some(child) = self.child.as_mut() {
            match child.try_wait() {
                Ok(Some(exit)) => {
                    status = Some(exit);
                    self.child = None;
                }
                Ok(None) => {}
                Err(error) => errors.push(format!("polling PTY child during close: {error}")),
            }
        }
        #[cfg(unix)]
        let root_killed = self.kill_terminal_groups(&mut errors);
        #[cfg(not(unix))]
        let root_killed = false;
        if let Some(mut child) = self.child.take() {
            // Portable Unix kill() has its own SIGHUP grace. Once SIGKILL was
            // delivered to the validated root group, reap without that delay.
            if !root_killed {
                if let Err(error) = child.kill() {
                    if error.raw_os_error() != Some(libc::ESRCH) {
                        errors.push(format!("killing PTY child: {error}"));
                    }
                }
            }
            match child.wait() {
                Ok(exit) => status = Some(exit),
                Err(error) => errors.push(format!("reaping PTY child: {error}")),
            }
        }
        // In particular, the portable writer's destructor runs here, not in a
        // deferred wrapper's UI-thread destructor.
        drop(self);
        let result = if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        };
        (status, result)
    }

    #[cfg(unix)]
    fn kill_terminal_groups(&self, errors: &mut Vec<String>) -> bool {
        let Some(fd) = self.master.as_raw_fd() else {
            return false;
        };
        let Some(pid) = self.pid.and_then(|pid| i32::try_from(pid).ok()) else {
            return false;
        };
        let session = unsafe { libc::tcgetsid(fd) };
        let own_group = unsafe { libc::getpgrp() };
        if session != pid || session <= 1 {
            return false;
        }
        let mut root_killed = false;
        if let Some(group) = self.master.process_group_leader() {
            if group > 1 && group != own_group && unsafe { libc::getsid(group) } == session {
                let killed = kill_group(group, errors);
                root_killed = group == pid && killed;
            }
        }
        if !root_killed
            && self.child.is_some()
            && pid > 1
            && pid != own_group
            && unsafe { libc::getpgid(pid) } == pid
            && unsafe { libc::getsid(pid) } == session
        {
            root_killed = kill_group(pid, errors);
        }
        root_killed
    }
}

#[cfg(unix)]
fn kill_group(group: libc::pid_t, errors: &mut Vec<String>) -> bool {
    if unsafe { libc::kill(-group, libc::SIGKILL) } == 0 {
        return true;
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return true;
    }
    errors.push(format!("killing PTY process group {group}: {error}"));
    false
}

/// portable-pty duplicates the master for its reader and writer; O_NONBLOCK is
/// shared by all of those descriptors. Polling a retained duplicate lets both
/// terminal workers observe cancellation even if a descendant retains a slave.
#[cfg(unix)]
struct PollFd(std::os::fd::OwnedFd);

#[cfg(unix)]
impl PollFd {
    fn new(fd: Option<std::os::fd::RawFd>) -> Result<Option<Arc<Self>>> {
        use std::os::fd::FromRawFd;
        let Some(fd) = fd else { return Ok(None) };
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error()).context("making PTY I/O cancellable");
        }
        let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
        if duplicate < 0 {
            return Err(io::Error::last_os_error()).context("retaining PTY poll descriptor");
        }
        Ok(Some(Arc::new(Self(unsafe {
            std::os::fd::OwnedFd::from_raw_fd(duplicate)
        }))))
    }

    fn wait(&self, interest: Interest, closed: &AtomicBool) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        let mut fd = libc::pollfd {
            fd: self.0.as_raw_fd(),
            events: match interest {
                Interest::Read => libc::POLLIN,
                Interest::Write => libc::POLLOUT,
            },
            revents: 0,
        };
        loop {
            if closed.load(Ordering::Acquire) {
                return Err(closed_error());
            }
            let result = unsafe { libc::poll(&mut fd, 1, CONTROL_INTERVAL.as_millis() as i32) };
            if result > 0 {
                if fd.revents & libc::POLLNVAL != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "PTY poll descriptor closed",
                    ));
                }
                return Ok(());
            }
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use portable_pty::{ChildKiller, SlavePty};
    use std::sync::mpsc;
    use std::thread::{self, ThreadId};

    const DEADLINE: Duration = Duration::from_secs(10);

    #[test]
    fn cancelling_pending_io_does_not_wait_for_startup() {
        let (pair, _, actions, _) = fixture(false);
        let (entered, entered_rx) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let PendingPty {
            mut reader,
            mut writer,
            control,
            events,
        } = PendingPty::spawn(move || {
            entered.send(()).unwrap();
            gate.recv().unwrap();
            PreparedPty::spawn(pair, CommandBuilder::new("test"))
        });
        entered_rx.recv_timeout(DEADLINE).unwrap();
        let (completed, completed_rx) = mpsc::channel();
        let read_done = completed.clone();
        let read_worker = thread::spawn(move || {
            let mut bytes = [0];
            read_done
                .send(reader.read(&mut bytes).unwrap_err().kind())
                .unwrap();
        });
        let write_worker = thread::spawn(move || {
            completed
                .send(writer.write_all(b"input").unwrap_err().kind())
                .unwrap();
        });
        control.close();
        for _ in 0..2 {
            assert_eq!(
                completed_rx.recv_timeout(DEADLINE).unwrap(),
                io::ErrorKind::BrokenPipe
            );
        }
        // The startup closure is still gated: neither worker waited for it.
        release.send(()).unwrap();
        observe(&actions, "wait", thread::current().id());
        read_worker.join().unwrap();
        write_worker.join().unwrap();
        drop((control, events));
    }

    #[cfg(unix)]
    #[test]
    fn reader_eof_does_not_report_child_exit() {
        use std::os::unix::ffi::OsStrExt;
        use std::sync::atomic::AtomicU64;
        static NEXT_GATE: AtomicU64 = AtomicU64::new(0);
        struct Fifo(std::path::PathBuf);
        impl Drop for Fifo {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let fifo = Fifo(std::env::temp_dir().join(format!(
            "kathamo-pty-gate-{}-{}",
            std::process::id(),
            NEXT_GATE.fetch_add(1, Ordering::Relaxed),
        )));
        let path = std::ffi::CString::new(fifo.0.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let gate_path = fifo.0.clone();
        let PendingPty {
            mut reader,
            writer,
            control,
            events,
        } = PendingPty::spawn(move || {
            let pair = portable_pty::native_pty_system().openpty(PtySize::default())?;
            let mut command = CommandBuilder::new("/bin/sh");
            command.args([
                "-c",
                "printf 'BEFORE_EOF\\n'; exec 0<&- 1>&- 2>&-; IFS= read -r gate < \"$PTY_TEST_GATE\"; exit 19",
            ]);
            command.env("PTY_TEST_GATE", gate_path);
            PreparedPty::spawn(pair, command)
        });
        assert!(matches!(event(&events), PtyEvent::Ready { .. }));
        let (eof, eof_rx) = mpsc::channel();
        let read_worker = thread::spawn(move || {
            let mut output = Vec::new();
            let result = reader.read_to_end(&mut output);
            eof.send((result, output)).unwrap();
        });
        let (result, output) = eof_rx
            .recv_timeout(DEADLINE)
            .expect("parent retained slave");
        result.unwrap();
        assert!(String::from_utf8(output).unwrap().contains("BEFORE_EOF"));
        assert!(
            events.try_recv().is_err(),
            "reader EOF invented a child exit"
        );
        let (released, released_rx) = mpsc::channel();
        let release_worker = thread::spawn(move || {
            let mut gate = std::fs::OpenOptions::new()
                .write(true)
                .open(&fifo.0)
                .unwrap();
            gate.write_all(b"release\n").unwrap();
            released.send(fifo).unwrap();
        });
        let fifo = released_rx.recv_timeout(DEADLINE).unwrap();
        match event(&events) {
            PtyEvent::Exited(status) => assert_eq!(status.exit_code(), 19),
            other => panic!("expected real exit after releasing child, got {other:?}"),
        }
        read_worker.join().unwrap();
        release_worker.join().unwrap();
        drop((writer, control, fifo));
    }

    fn event(events: &Receiver<PtyEvent>) -> PtyEvent {
        smol::block_on(smol::future::race(
            async { events.recv().await.expect("PTY event stream closed") },
            async {
                smol::Timer::after(DEADLINE).await;
                panic!("timed out waiting for PTY event");
            },
        ))
    }

    #[derive(Debug)]
    struct Observations {
        activity: mpsc::Sender<(&'static str, ThreadId)>,
        kill_gate: Mutex<Option<mpsc::Receiver<()>>>,
        resize_gate: Mutex<Option<mpsc::Receiver<()>>>,
        sizes: mpsc::Sender<PtySize>,
    }

    impl Observations {
        fn record(&self, action: &'static str) {
            let _ = self.activity.send((action, thread::current().id()));
        }
    }

    #[derive(Debug)]
    struct TestChild(Arc<Observations>);

    impl ChildKiller for TestChild {
        fn kill(&mut self) -> io::Result<()> {
            self.0.record("kill");
            if let Some(gate) = self.0.kill_gate.lock().take() {
                let _ = gate.recv();
            }
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(Self(self.0.clone()))
        }
    }

    impl Child for TestChild {
        fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            Ok(None)
        }

        fn wait(&mut self) -> io::Result<ExitStatus> {
            self.0.record("wait");
            Ok(ExitStatus::with_exit_code(137))
        }

        fn process_id(&self) -> Option<u32> {
            None
        }

        #[cfg(windows)]
        fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
            None
        }
    }

    struct TestSlave(Arc<Observations>);

    impl SlavePty for TestSlave {
        fn spawn_command(&self, _: CommandBuilder) -> Result<Box<dyn Child + Send + Sync>> {
            self.0.record("spawn");
            Ok(Box::new(TestChild(self.0.clone())))
        }
    }

    impl Drop for TestSlave {
        fn drop(&mut self) {
            self.0.record("slave-drop");
        }
    }

    struct TestWriter(Arc<Observations>);

    impl Write for TestWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.record("write");
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Drop for TestWriter {
        fn drop(&mut self) {
            self.0.record("writer-drop");
        }
    }

    struct TestMaster {
        observations: Arc<Observations>,
        fail_reader: bool,
    }

    impl MasterPty for TestMaster {
        fn resize(&self, size: PtySize) -> Result<()> {
            self.observations.record("resize");
            let _ = self.observations.sizes.send(size);
            if let Some(gate) = self.observations.resize_gate.lock().take() {
                let _ = gate.recv();
            }
            Ok(())
        }

        fn get_size(&self) -> Result<PtySize> {
            Ok(PtySize::default())
        }

        fn try_clone_reader(&self) -> Result<Box<dyn Read + Send>> {
            self.observations.record("reader");
            anyhow::ensure!(!self.fail_reader, "injected reader extraction failure");
            Ok(Box::new(io::Cursor::new(Vec::<u8>::new())))
        }

        fn take_writer(&self) -> Result<Box<dyn Write + Send>> {
            self.observations.record("writer");
            Ok(Box::new(TestWriter(self.observations.clone())))
        }

        #[cfg(unix)]
        fn process_group_leader(&self) -> Option<libc::pid_t> {
            None
        }

        #[cfg(unix)]
        fn as_raw_fd(&self) -> Option<std::os::fd::RawFd> {
            None
        }

        #[cfg(unix)]
        fn tty_name(&self) -> Option<std::path::PathBuf> {
            None
        }
    }

    fn fixture(
        fail_reader: bool,
    ) -> (
        PtyPair,
        Arc<Observations>,
        mpsc::Receiver<(&'static str, ThreadId)>,
        mpsc::Receiver<PtySize>,
    ) {
        let (activity, activity_rx) = mpsc::channel();
        let (sizes, sizes_rx) = mpsc::channel();
        let observations = Arc::new(Observations {
            activity,
            sizes,
            kill_gate: Mutex::new(None),
            resize_gate: Mutex::new(None),
        });
        let pair = PtyPair {
            slave: Box::new(TestSlave(observations.clone())),
            master: Box::new(TestMaster {
                observations: observations.clone(),
                fail_reader,
            }),
        };
        (pair, observations, activity_rx, sizes_rx)
    }

    fn observe(
        receiver: &mpsc::Receiver<(&'static str, ThreadId)>,
        wanted: &'static str,
        not_on: ThreadId,
    ) {
        loop {
            let (action, worker) = receiver
                .recv_timeout(DEADLINE)
                .expect("missing worker action");
            assert_ne!(worker, not_on, "{action} ran on the caller");
            if action == wanted {
                return;
            }
        }
    }

    #[test]
    fn delayed_start_cancellation_cleans_child_after_factory_returns() {
        let caller = thread::current().id();
        let (pair, _, actions, _) = fixture(false);
        let (entered, entered_rx) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let pending = PendingPty::spawn(move || {
            entered.send(()).unwrap();
            gate.recv().unwrap();
            PreparedPty::spawn(pair, CommandBuilder::new("test"))
        });
        entered_rx.recv_timeout(DEADLINE).unwrap();
        // No stream has ever been used; dropping all recipients must still
        // dispose of the late startup result and its blocking writer destructor.
        drop(pending);
        release.send(()).unwrap();
        observe(&actions, "wait", caller);
        observe(&actions, "writer-drop", caller);
    }

    #[test]
    fn partial_setup_failure_reports_error_and_reaps_child() {
        let caller = thread::current().id();
        let (pair, _, actions, _) = fixture(true);
        let pending =
            PendingPty::spawn(move || PreparedPty::spawn(pair, CommandBuilder::new("test")));
        match event(&pending.events) {
            PtyEvent::Error(error) => {
                assert!(error.contains("cloning PTY reader"));
                assert!(error.contains("injected reader extraction failure"));
            }
            other => panic!("expected setup failure, got {other:?}"),
        }
        observe(&actions, "wait", caller);
        observe(&actions, "writer-drop", caller);
    }

    #[test]
    fn vanished_event_receiver_cancels_prepared_startup() {
        let caller = thread::current().id();
        let (pair, _, actions, _) = fixture(false);
        let (entered, entered_rx) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let PendingPty {
            reader,
            writer,
            control,
            events,
        } = PendingPty::spawn(move || {
            let prepared = PreparedPty::spawn(pair, CommandBuilder::new("test"))?;
            entered.send(()).unwrap();
            gate.recv().unwrap();
            Ok(prepared)
        });
        entered_rx.recv_timeout(DEADLINE).unwrap();
        drop(events);
        release.send(()).unwrap();
        observe(&actions, "wait", caller);
        observe(&actions, "writer-drop", caller);
        drop((reader, writer, control));
    }

    #[test]
    fn last_control_drop_does_not_wait_for_blocked_kill_or_live_streams() {
        let (pair, observations, actions, _) = fixture(false);
        let (release, gate) = mpsc::channel();
        *observations.kill_gate.lock() = Some(gate);
        let PendingPty {
            reader,
            writer,
            control,
            events,
        } = PendingPty::spawn(move || PreparedPty::spawn(pair, CommandBuilder::new("test")));
        assert!(matches!(event(&events), PtyEvent::Ready { .. }));
        let (dropped, dropped_rx) = mpsc::channel();
        let dropper = thread::spawn(move || {
            let caller = thread::current().id();
            drop(control);
            dropped.send(caller).unwrap();
        });
        let caller = dropped_rx
            .recv_timeout(DEADLINE)
            .expect("control drop blocked");
        observe(&actions, "kill", caller);
        release.send(()).unwrap();
        observe(&actions, "wait", caller);
        observe(&actions, "writer-drop", caller);
        assert!(matches!(event(&events), PtyEvent::Exited(_)));
        dropper.join().unwrap();
        drop((reader, writer));
    }

    #[test]
    fn resize_coalesces_without_holding_ui_mailbox_over_io() {
        let (pair, observations, _, sizes) = fixture(false);
        let (start, gate) = mpsc::channel();
        let pending = PendingPty::spawn(move || {
            gate.recv().unwrap();
            PreparedPty::spawn(pair, CommandBuilder::new("test"))
        });
        let size = |cols| PtySize {
            cols,
            ..PtySize::default()
        };
        pending.control.resize(size(10));
        pending.control.resize(size(20));
        start.send(()).unwrap();
        assert!(matches!(event(&pending.events), PtyEvent::Ready { .. }));
        assert_eq!(sizes.recv_timeout(DEADLINE).unwrap(), size(20));
        let (release, resize_gate) = mpsc::channel();
        *observations.resize_gate.lock() = Some(resize_gate);
        pending.control.resize(size(30));
        assert_eq!(sizes.recv_timeout(DEADLINE).unwrap(), size(30));
        // This call completes while the worker is still gated inside resize.
        pending.control.resize(size(40));
        pending.control.resize(size(50));
        release.send(()).unwrap();
        assert_eq!(sizes.recv_timeout(DEADLINE).unwrap(), size(50));
        assert!(sizes.try_recv().is_err());
    }

    #[cfg(unix)]
    fn shell(script: &str) -> PreparedPty {
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize::default())
            .unwrap();
        let mut command = CommandBuilder::new("/bin/sh");
        command.args(["-c", script]);
        PreparedPty::spawn(pair, command).unwrap()
    }

    #[cfg(unix)]
    fn output_lines(reader: DeferredReader) -> (mpsc::Receiver<String>, thread::JoinHandle<()>) {
        use std::io::BufRead;
        let (lines, received) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut reader = io::BufReader::new(reader);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => return,
                    Ok(_) => {
                        if lines
                            .send(line.trim_end_matches(['\r', '\n']).to_owned())
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::BrokenPipe => return,
                    Err(error) => panic!("PTY read failed: {error}"),
                }
            }
        });
        (received, worker)
    }

    #[cfg(unix)]
    #[test]
    fn real_pty_ready_input_output_resize_and_real_exit() {
        let (release, gate) = mpsc::channel();
        let PendingPty {
            reader,
            mut writer,
            control,
            events,
        } = PendingPty::spawn(move || {
            gate.recv().unwrap();
            Ok(shell(
                "stty -echo; printf 'BOOT\\n'; IFS= read -r line; printf 'GOT:%s\\n' \"$line\"; stty size; exit 7",
            ))
        });
        control.resize(PtySize {
            rows: 30,
            cols: 70,
            ..PtySize::default()
        });
        control.resize(PtySize {
            rows: 37,
            cols: 91,
            ..PtySize::default()
        });
        release.send(()).unwrap();
        assert!(matches!(event(&events), PtyEvent::Ready { pid: Some(_) }));
        let (lines, reader_worker) = output_lines(reader);
        assert_eq!(lines.recv_timeout(DEADLINE).unwrap(), "BOOT");
        let writer_worker = thread::spawn(move || writer.write_all(b"hello\n"));
        writer_worker.join().unwrap().unwrap();
        assert_eq!(lines.recv_timeout(DEADLINE).unwrap(), "GOT:hello");
        assert_eq!(lines.recv_timeout(DEADLINE).unwrap(), "37 91");
        match event(&events) {
            PtyEvent::Exited(status) => assert_eq!(status.exit_code(), 7),
            other => panic!("expected real exit, got {other:?}"),
        }
        // The parent's slave was closed; the reader finishes without closing
        // the session control or fabricating a child exit from reader EOF.
        assert!(matches!(
            lines.recv_timeout(DEADLINE),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
        reader_worker.join().unwrap();
        drop(control);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn closing_raw_foreground_group_unblocks_reader_and_reaps_real_child() {
        let PendingPty {
            reader,
            writer,
            control,
            events,
        } = PendingPty::spawn(|| {
            Ok(shell(
                "set -m; sh -c 'stty raw -echo; trap \"\" HUP TERM; printf \"RAW:%s\\n\" \"$$\"; while :; do read x; done'; :",
            ))
        });
        let pid = match event(&events) {
            PtyEvent::Ready { pid: Some(pid) } => pid,
            other => panic!("expected ready child, got {other:?}"),
        };
        let (lines, reader_worker) = output_lines(reader);
        let marker = lines.recv_timeout(DEADLINE).unwrap();
        let foreground: u32 = marker
            .strip_prefix("RAW:")
            .expect("raw child marker")
            .parse()
            .unwrap();
        assert_ne!(
            foreground, pid,
            "test requires a separate foreground process group"
        );
        use std::os::fd::{AsRawFd, FromRawFd};
        let descriptor =
            unsafe { libc::syscall(libc::SYS_pidfd_open, foreground as libc::pid_t, 0) };
        assert!(
            descriptor >= 0,
            "opening foreground pidfd: {}",
            io::Error::last_os_error()
        );
        let foreground_fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(descriptor as i32) };
        let (closed, closed_rx) = mpsc::channel();
        let dropper = thread::spawn(move || {
            drop(control);
            closed.send(()).unwrap();
        });
        closed_rx
            .recv_timeout(DEADLINE)
            .expect("close blocked caller");
        assert!(matches!(event(&events), PtyEvent::Exited(_)));
        reader_worker.join().unwrap();
        dropper.join().unwrap();
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
        let mut foreground_exit = libc::pollfd {
            fd: foreground_fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(
            unsafe { libc::poll(&mut foreground_exit, 1, DEADLINE.as_millis() as i32) },
            1,
            "foreground process survived pane close",
        );
        assert_ne!(foreground_exit.revents & libc::POLLIN, 0);
        drop(writer);
    }
}
