//! Linux transport for the pinned OpenCode Rune CEF engine. Wire definitions:
//! https://github.com/anomalyco/opencode-rune/blob/4e90d0ce4530d8a1e13505009558273656e4aa38/apps/browser-engine/src/wire.rs

mod audio;

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use async_process::{Child, Command};
use base64_simd::STANDARD;
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use smol::Timer;
use smol::channel::{Receiver, Sender, bounded};
use smol::future::race;
use smol::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use smol::net::unix::UnixStream;
use std::collections::HashMap;
use std::fs::{self, DirBuilder, File, OpenOptions, Permissions};
use std::io::{self, BufReader as FileReader, Read, Write};
use std::ops::Range;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::browser_automation::Reply;

const BUNDLE: &str = "OpenCode Browser Engine";
const EXECUTABLE: &str = "opencode-browser-engine";
const CACHE: &str = "opencode-browser-engine-156.0.26-28";
// Immutable entries from https://anoma.ly/release/opencode-rune/browser-engine/stable.json.
#[cfg(target_arch = "x86_64")]
const ARTIFACT: (&str, u64, &str) = (
    "linux-x64",
    133_097_195,
    "7ebdb6b516b081fe4b2488805f720ae168b50dfcb2ba46a4f803bdfe5ed41b9e",
);
#[cfg(target_arch = "aarch64")]
const ARTIFACT: (&str, u64, &str) = (
    "linux-arm64",
    129_212_872,
    "0528000159e8255741840cdc386bc005dbe314c21c1bef01e5894f33ec7d360f",
);
const MAX_EXTRACTED: u64 = 768 * 1024 * 1024;
const MAX_ENTRIES: usize = 512;
const INPUT_QUEUE: usize = 128;
const EVENT_QUEUE: usize = 4;
const MAX_COMMAND: usize = 64 * 1024;
const MAX_MESSAGE: usize = 1024 * 1024;
pub(crate) const MAX_FRAME_SIDE: u32 = 2048;
pub(crate) const MAX_FRAME_PIXELS: u32 = 1920 * 1080;
// Chromium can round each device-pixel dimension up.
const MAX_PAINT_PIXELS: usize = (MAX_FRAME_PIXELS + 2 * MAX_FRAME_SIDE + 1) as usize;
const MAX_LINE: usize = (MAX_PAINT_PIXELS * 4).div_ceil(3) * 4 + 1024;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const STARTUP_POLL: Duration = Duration::from_millis(50);

pub(crate) enum Event {
    Message(Value),
    Error(String),
}

pub(crate) enum Outbound {
    Manual(Value),
    Agent(Value, Arc<AgentGuard>),
}

pub(crate) struct AgentGuard {
    pub reply: Sender<Result<Reply, String>>,
    cancelled: AtomicBool,
}

impl AgentGuard {
    pub(crate) fn new(reply: Sender<Result<Reply, String>>) -> Self {
        Self {
            reply,
            cancelled: AtomicBool::new(false),
        }
    }

    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub(crate) fn is_live(&self) -> bool {
        !self.cancelled.load(Ordering::Acquire) && !self.reply.is_closed()
    }
}

pub(crate) struct LatestFrames {
    tabs: Mutex<FrameTabs>,
    wake: Sender<()>,
    ready: Receiver<()>,
    decode_wake: Sender<()>,
    decode_ready: Receiver<()>,
}

#[derive(Default)]
struct FrameTabs {
    generation: u64,
    slots: HashMap<u64, FrameSlot>,
}

#[derive(Default)]
struct FrameSlot {
    generation: u64,
    pending: Option<RawFrame>,
    phase: FramePhase,
    spare: Vec<u8>,
}

#[derive(Default)]
enum FramePhase {
    #[default]
    Idle,
    Scheduled,
    Requested,
    Decoding,
    Ready(Result<Arc<gpui::RenderImage>>),
}

pub(crate) struct FrameUpdate {
    pub(crate) request: FrameRequest,
    pub(crate) schedule: bool,
    pub(crate) image: Option<Result<Arc<gpui::RenderImage>>>,
}

#[derive(Clone, Copy)]
pub(crate) struct FrameRequest {
    pub(crate) tab_id: u64,
    generation: u64,
}

impl FrameTabs {
    fn slot(&mut self, request: FrameRequest) -> Option<&mut FrameSlot> {
        self.slots
            .get_mut(&request.tab_id)
            .filter(|slot| slot.generation == request.generation)
    }
}

impl Default for LatestFrames {
    fn default() -> Self {
        let (wake, ready) = bounded(1);
        let (decode_wake, decode_ready) = bounded(1);
        Self {
            tabs: Mutex::default(),
            wake,
            ready,
            decode_wake,
            decode_ready,
        }
    }
}

impl LatestFrames {
    pub(crate) fn show(&self, previous: Option<u64>, next: Option<u64>) -> Option<Vec<u64>> {
        if previous == next {
            return None;
        }
        let mut tabs = self.tabs.lock();
        let removed = previous.and_then(|id| tabs.slots.remove(&id));
        let inserted = next.filter(|id| !tabs.slots.contains_key(id));
        if let Some(id) = inserted {
            tabs.generation = tabs.generation.wrapping_add(1);
            let generation = tabs.generation;
            tabs.slots.insert(
                id,
                FrameSlot {
                    generation,
                    ..Default::default()
                },
            );
        }
        let visible = (removed.is_some() || inserted.is_some())
            .then(|| tabs.slots.keys().copied().collect());
        drop(tabs);
        drop(removed);
        visible
    }

    pub(crate) fn clear(&self) {
        let slots = std::mem::take(&mut self.tabs.lock().slots);
        self.wake.close();
        self.decode_wake.close();
        drop(slots);
    }

    fn replace(&self, tab_id: u64, paint: Result<Paint>, buffer: &mut Vec<u8>) -> Result<()> {
        let wake = {
            let mut tabs = self.tabs.lock();
            let Some(slot) = tabs.slots.get_mut(&tab_id) else {
                return Ok(());
            };
            let paint = paint?;
            let previous = slot.pending.replace(RawFrame {
                paint,
                line: std::mem::take(buffer),
            });
            let wake = previous.is_none() && matches!(slot.phase, FramePhase::Idle);
            *buffer = previous.map_or_else(|| std::mem::take(&mut slot.spare), |raw| raw.line);
            wake
        };
        if wake {
            // A single signal covers every ready viewport; paints never block control reads.
            let _ = self.wake.try_send(());
        }
        Ok(())
    }

    pub(crate) async fn wait(&self, updates: &mut Vec<FrameUpdate>) -> bool {
        if self.ready.recv().await.is_err() {
            return false;
        }
        for (&tab_id, slot) in &mut self.tabs.lock().slots {
            let image = match std::mem::take(&mut slot.phase) {
                FramePhase::Ready(image) => Some(image),
                phase => {
                    slot.phase = phase;
                    None
                }
            };
            let schedule = slot.pending.is_some() && matches!(slot.phase, FramePhase::Idle);
            if schedule {
                slot.phase = FramePhase::Scheduled;
            }
            if schedule || image.is_some() {
                let request = FrameRequest {
                    tab_id,
                    generation: slot.generation,
                };
                updates.push(FrameUpdate { request, schedule, image });
            }
        }
        true
    }

    pub(crate) fn is_current(&self, request: FrameRequest) -> bool {
        self.tabs.lock().slot(request).is_some()
    }

    pub(crate) fn request_decode(&self, request: FrameRequest) {
        let mut tabs = self.tabs.lock();
        let Some(slot) = tabs.slot(request) else {
            return;
        };
        if matches!(slot.phase, FramePhase::Scheduled) {
            slot.phase = FramePhase::Requested;
            drop(tabs);
            let _ = self.decode_wake.try_send(());
        }
    }

    fn take(&self) -> Option<(FrameRequest, RawFrame)> {
        let mut tabs = self.tabs.lock();
        let (&tab_id, slot) = tabs
            .slots
            .iter_mut()
            .find(|(_, slot)| matches!(slot.phase, FramePhase::Requested))?;
        let pending = slot.pending.take()?;
        slot.phase = FramePhase::Decoding;
        let request = FrameRequest {
            tab_id,
            generation: slot.generation,
        };
        Some((request, pending))
    }

    fn finish(&self, request: FrameRequest, buffer: Vec<u8>, image: Result<Arc<gpui::RenderImage>>) {
        let mut tabs = self.tabs.lock();
        let Some(slot) = tabs.slot(request) else { return };
        slot.phase = FramePhase::Ready(image);
        slot.spare = buffer;
        drop(tabs);
        let _ = self.wake.try_send(());
    }

    async fn decode(&self) {
        while self.decode_ready.recv().await.is_ok() {
            while let Some((request, raw)) = self.take() {
                let image = raw.paint.decode(&raw.line);
                self.finish(request, raw.line, image);
            }
        }
    }
}

pub(crate) fn start(
    cx: &gpui::App,
    frames: Arc<LatestFrames>,
) -> (Sender<Outbound>, Receiver<Event>) {
    let (commands, incoming) = bounded(INPUT_QUEUE);
    let (outgoing, events) = bounded(EVENT_QUEUE);
    let decoder = frames.clone();
    cx.background_executor()
        .spawn(async move { decoder.decode().await })
        .detach();
    cx.background_executor()
        .spawn(async move {
            let result = run(&incoming, &outgoing, &frames).await;
            let canceled = incoming.is_closed();
            drop(incoming);
            if let Err(error) = result
                && !canceled
            {
                let _ = outgoing.send(Event::Error(format!("{error:#}"))).await;
            }
        })
        .detach();
    (commands, events)
}

async fn run(
    commands: &Receiver<Outbound>,
    events: &Sender<Event>,
    frames: &LatestFrames,
) -> Result<()> {
    let guard = commands.clone();
    let Some(EngineProcess {
        mut child,
        socket_dir,
        audio,
    }) = smol::unblock(move || prepare(&guard)).await?
    else {
        return Ok(());
    };
    let _playback = audio.map(|audio| smol::spawn(audio.run()));
    let socket = socket_dir.path().join("engine.sock");
    let mut connected = None;
    let result = async {
        let Some(mut stdout) = child.stdout.take() else {
            bail!("browser engine startup pipe is missing");
        };
        let Some((stream, reader, line, agent_tools)) = race(
            async {
                before(
                    STARTUP_TIMEOUT,
                    async {
                        let mut marker = [0; b"listening\n".len()];
                        stdout.read_exact(&mut marker).await.context(
                            "browser exited before opening its socket; check Bubblewrap, user namespaces and system libraries",
                        )?;
                        ensure!(&marker == b"listening\n", "invalid browser startup marker");
                        let mut stream = UnixStream::connect(&socket)
                            .await
                            .context("connecting to the sandboxed browser")?;
                        let mut reader = BufReader::with_capacity(64 * 1024, stream.clone());
                        let mut line = Vec::new();
                        let agent_tools = negotiate(&mut stream, &mut reader, &mut line).await?;
                        Ok(Some((stream, reader, line, agent_tools)))
                    },
                    "waiting for the browser engine to be ready",
                )
                .await
            },
            async {
                while !commands.is_closed() {
                    Timer::after(STARTUP_POLL).await;
                }
                Ok(None)
            },
        )
        .await? else {
            return Ok(());
        };
        let stream = connected.insert(stream);
        race(
            write_commands(stream, commands, agent_tools),
            read_events(reader, line, events, frames),
        )
        .await
    }
    .await;
    stop(&mut child, connected).await;
    result
}

struct EngineProcess {
    child: Child,
    socket_dir: tempfile::TempDir,
    audio: Option<audio::Playback>,
}

fn prepare(commands: &Receiver<Outbound>) -> Result<Option<EngineProcess>> {
    if commands.is_closed() {
        return Ok(None);
    }
    let bwrap = ["/usr/bin/bwrap", "/bin/bwrap", "/usr/local/bin/bwrap"]
        .into_iter()
        .map(Path::new)
        .find(|path| fs::metadata(path).is_ok_and(|m| m.is_file() && executable(&m)))
        .context("Browser requires Bubblewrap (bwrap) with Linux user namespaces enabled")?;
    let data = std::path::absolute(crate::paths::data_dir())?;
    let bundle = install(&data.join("tools"), commands).context("installing browser engine")?;
    if commands.is_closed() {
        return Ok(None);
    }
    let profile = data.join("browser-profile");
    match DirBuilder::new().mode(0o700).create(&profile) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).context("creating browser profile"),
    }
    let metadata = fs::symlink_metadata(&profile)?;
    ensure!(
        metadata.is_dir() && metadata.permissions().mode() & 0o077 == 0,
        "browser profile must be a private directory: {}",
        profile.display()
    );
    // AF_UNIX paths have a tight length limit. This directory is random and private.
    let socket_dir = tempfile::Builder::new()
        .prefix("shob-cef-")
        .permissions(Permissions::from_mode(0o700))
        .tempdir_in("/tmp")
        .context("creating private browser socket directory")?;
    let audio = audio::Playback::bind(socket_dir.path()).ok().flatten();
    let mut cmd = Command::new(bwrap);
    // This Linux pin disables CEF's own sandbox. Never execute it outside containment.
    cmd.env_clear().args([
        "--unshare-all",
        "--unshare-user",
        // Browser networking, including localhost development servers, stays on the host.
        "--share-net",
        "--die-with-parent",
        "--new-session",
        "--disable-userns",
        "--cap-drop",
        "ALL",
        "--clearenv",
        "--proc",
        "/proc",
        "--dev",
        "/dev",
        "--perms",
        "1777",
        "--tmpfs",
        "/tmp",
        "--dir",
        "/tmp/runtime",
        "--chmod",
        "0700",
        "/tmp/runtime",
    ]);
    for path in [
        "/lib",
        "/lib64",
        "/usr/lib",
        "/usr/lib64",
        "/etc/ld.so.cache",
        "/etc/hosts",
        "/etc/host.conf",
        "/etc/resolv.conf",
        "/etc/nsswitch.conf",
        "/etc/localtime",
        "/etc/fonts",
        "/usr/share/fonts",
        "/usr/share/fontconfig",
        "/usr/share/X11/xkb",
        "/etc/ssl/certs",
        "/usr/share/ca-certificates",
        "/etc/pki/tls/certs",
        "/etc/pki/ca-trust/extracted",
        "/usr/share/glib-2.0/schemas",
        "/usr/share/vulkan/icd.d",
    ] {
        cmd.args(["--ro-bind-try", path, path]);
    }
    if audio.is_some() {
        cmd.args(["--setenv", "PULSE_SERVER", audio::SANDBOX_SERVER]);
    }
    let child = cmd
        .arg("--ro-bind")
        .arg(bundle)
        .arg("/engine")
        .arg("--bind")
        .arg(profile)
        .arg("/profile")
        .arg("--bind")
        .arg(socket_dir.path())
        .arg("/ipc")
        .args(["--remount-ro", "/"])
        .args(["--setenv", "HOME", "/profile"])
        .args(["--setenv", "LANG", "C.UTF-8"])
        .args(["--setenv", "TMPDIR", "/tmp"])
        .args(["--setenv", "XDG_RUNTIME_DIR", "/tmp/runtime"])
        .args(["--setenv", "OPENCODE_BROWSER_PROFILE", "/profile/chromium"])
        .args([
            "--chdir",
            "/engine",
            "--",
            "/engine/opencode-browser-engine",
        ])
        .args(["--socket", "/ipc/engine.sock", "--lang", "en-US"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .context("starting the browser engine inside Bubblewrap")?;
    Ok(Some(EngineProcess {
        child,
        socket_dir,
        audio,
    }))
}

async fn negotiate(
    stream: &mut UnixStream,
    reader: &mut BufReader<UnixStream>,
    line: &mut Vec<u8>,
) -> Result<bool> {
    let hello = json!({
        "type": "hello",
        "mach_service": null,
        "pid": std::process::id(),
        "prompts": false,
        "extensions": false,
        "download_save": false,
        "tab_lifecycle": true,
    });
    let mut hello = serde_json::to_vec(&hello)?;
    hello.push(b'\n');
    write_line(stream, &hello).await?;
    let message: Value = serde_json::from_slice(read_line(reader, line, MAX_MESSAGE).await?)
        .context("reading the browser engine handshake")?;
    ensure!(
        message["type"] == "ready"
            && message["chromium_major"] == 156
            && message["pid"].as_u64().is_some_and(|pid| pid != 0)
            && message["tab_lifecycle"] == true,
        "incompatible browser engine handshake"
    );
    Ok(["policy", "partitions", "agent_tools", "frame_authority"]
        .into_iter()
        .all(|capability| message[capability] == true))
}

async fn stop(child: &mut Child, mut stream: Option<UnixStream>) {
    if let Some(stream) = stream.as_mut() {
        let _ = write_line(stream, b"{\"type\":\"shutdown\"}\n").await;
    }
    drop(stream);
    if before(
        SHUTDOWN_TIMEOUT,
        async { Ok(child.status().await?) },
        "stopping the browser sandbox",
    )
    .await
    .is_err()
    {
        // Only signal our own Bubblewrap supervisor. Its PID namespace and die-with-parent
        // policy tear down the engine and its helpers; the Child guard reaps on drop too.
        let _ = child.kill();
        let _ = before(
            SHUTDOWN_TIMEOUT,
            async { Ok(child.status().await?) },
            "reaping the browser sandbox",
        )
        .await;
    }
}

async fn before<T>(time: Duration, work: impl Future<Output = Result<T>>, what: &str) -> Result<T> {
    race(work, async {
        Timer::after(time).await;
        Err(anyhow!("timed out {what}"))
    })
    .await
}

async fn write_line(stream: &mut UnixStream, line: &[u8]) -> Result<()> {
    before(
        WRITE_TIMEOUT,
        async {
            stream.write_all(line).await?;
            Ok(())
        },
        "sending browser input",
    )
    .await
    .context("writing to the browser engine")
}

async fn write_commands(
    stream: &mut UnixStream,
    commands: &Receiver<Outbound>,
    agent_tools: bool,
) -> Result<()> {
    let mut buffer = CommandBuffer(Vec::new());
    while !commands.is_closed() {
        let Ok(command) = commands.recv().await else {
            break;
        };
        let value = match &command {
            Outbound::Manual(value) | Outbound::Agent(value, _) => value,
        };
        if let Outbound::Agent(_, guard) = &command
            && !agent_tools
        {
            if guard.is_live() {
                let _ = guard.reply.try_send(Err(
                    "The browser engine does not support isolated agent tabs.".into(),
                ));
                guard.cancel();
            }
            continue;
        }
        buffer.0.clear();
        serde_json::to_writer(&mut buffer, value).context("encoding browser input")?;
        buffer.0.push(b'\n');
        if let Outbound::Agent(_, guard) = &command
            && !guard.is_live()
        {
            continue;
        }
        write_line(stream, &buffer.0).await?;
    }
    Ok(())
}

struct CommandBuffer(Vec<u8>);

impl Write for CommandBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() >= MAX_COMMAND - self.0.len() {
            return Err(io::Error::other("browser command is too large"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

async fn read_line<'a>(
    reader: &mut BufReader<UnixStream>,
    line: &'a mut Vec<u8>,
    limit: usize,
) -> Result<&'a [u8]> {
    line.clear();
    let bytes = reader
        .take((limit + 1) as u64)
        .read_until(b'\n', line)
        .await
        .context("reading from the browser engine")?;
    ensure!(bytes != 0, "browser engine disconnected");
    ensure!(
        bytes <= limit,
        "browser engine message exceeds its size limit"
    );
    line.strip_suffix(b"\n")
        .context("browser engine sent an incomplete message")
}

#[derive(Deserialize)]
struct Incoming<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    tab_id: Option<u64>,
    method: Option<&'a str>,
    width: Option<u32>,
    height: Option<u32>,
    bgra_b64: Option<&'a str>,
}

struct Paint {
    width: u32,
    height: u32,
    bgra: Range<usize>,
}

struct RawFrame {
    paint: Paint,
    line: Vec<u8>,
}

impl Incoming<'_> {
    fn paint(self, line: &[u8]) -> Result<Paint> {
        let (Some(width), Some(height), Some(bgra)) = (self.width, self.height, self.bgra_b64)
        else {
            bail!("browser frame is missing a required field");
        };
        ensure!(
            (1..=MAX_FRAME_SIDE).contains(&width) && (1..=MAX_FRAME_SIDE).contains(&height),
            "browser frame dimensions exceed the allowed size ({width}×{height})"
        );
        let pixels = width as usize * height as usize;
        ensure!(
            pixels <= MAX_PAINT_PIXELS,
            "browser frame exceeds the pixel budget"
        );
        let bytes = pixels * 4;
        ensure!(
            bgra.len() == bytes.div_ceil(3) * 4,
            "incorrect browser frame length"
        );
        // `bgra` is borrowed from `line` by serde_json, so retain the offset, not a copy.
        let start = bgra.as_ptr() as usize - line.as_ptr() as usize;
        Ok(Paint {
            width,
            height,
            bgra: start..start + bgra.len(),
        })
    }
}

impl Paint {
    fn decode(self, line: &[u8]) -> Result<Arc<gpui::RenderImage>> {
        let Self { width, height, bgra } = self;
        let pixels = STANDARD
            .decode_to_vec(&line[bgra])
            .context("decoding browser frame")?;
        ensure!(
            pixels.len() == width as usize * height as usize * 4,
            "incorrect browser frame length"
        );
        // RenderImage takes BGRA already. RgbaImage is just the owning four-byte pixel buffer.
        let pixels =
            image::RgbaImage::from_raw(width, height, pixels).context("building browser frame")?;
        Ok(Arc::new(gpui::RenderImage::new([image::Frame::new(
            pixels,
        )])))
    }
}

async fn read_events(
    mut reader: BufReader<UnixStream>,
    mut buffer: Vec<u8>,
    events: &Sender<Event>,
    frames: &LatestFrames,
) -> Result<()> {
    loop {
        let line = read_line(&mut reader, &mut buffer, MAX_LINE).await?;
        let incoming: Incoming<'_> =
            serde_json::from_slice(line).context("invalid browser engine message")?;
        if matches!(incoming.kind, "cdp_event" | "cdp_response") && line.len() > MAX_MESSAGE {
            continue;
        }
        if incoming.kind == "cdp_event"
            && !matches!(
                incoming.method,
                Some(
                    "Runtime.executionContextCreated"
                        | "Runtime.executionContextDestroyed"
                        | "Runtime.executionContextsCleared"
                )
            )
        {
            continue;
        }
        if incoming.kind == "cpu_frame" {
            let id = incoming.tab_id.context("browser frame is missing a tab ID")?;
            let paint = incoming.paint(line);
            frames.replace(id, paint, &mut buffer)?;
            continue;
        }
        ensure!(
            line.len() <= MAX_MESSAGE,
            "browser control message is too large"
        );
        let event = Event::Message(
            serde_json::from_slice(line).context("invalid browser control message")?,
        );
        if events.send(event).await.is_err() {
            return Ok(());
        }
    }
}

fn executable(metadata: &fs::Metadata) -> bool {
    metadata.permissions().mode() & 0o111 != 0
}

fn check_bundle(directory: &Path) -> Result<PathBuf> {
    let bundle = directory.join(BUNDLE);
    for dir in [directory, bundle.as_path()] {
        ensure!(
            fs::symlink_metadata(dir)?.is_dir(),
            "invalid browser engine directory: {}",
            dir.display()
        );
    }
    for filename in [EXECUTABLE, "opencode-browser-engine-helper"] {
        let path = bundle.join(filename);
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("missing browser engine file {}", path.display()))?;
        ensure!(
            metadata.is_file() && executable(&metadata),
            "invalid browser executable: {}",
            path.display()
        );
    }
    Ok(bundle)
}

fn install(tools: &Path, commands: &Receiver<Outbound>) -> Result<PathBuf> {
    let destination = tools.join(CACHE);
    match fs::symlink_metadata(&destination) {
        Ok(_) => return check_bundle(&destination),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("accessing the browser engine cache"),
    }
    fs::create_dir_all(tools)?;
    let temporary = tempfile::Builder::new()
        .prefix(".browser-engine-")
        .tempdir_in(tools)?;
    let archive = temporary.path().join("engine.tar.zst");
    let stage = temporary.path().join("stage");
    download(&archive, commands).context("downloading the pinned OpenCode browser engine")?;
    let reader = FileReader::new(File::open(archive)?);
    let mut decoder = zstd::stream::read::Decoder::with_buffer(reader)?;
    decoder.window_log_max(27)?;
    fs::create_dir(&stage)?;
    unpack(decoder, &stage, commands).context("extracting browser engine bundle")?;
    check_bundle(&stage)?;
    ensure!(!commands.is_closed(), "browser closed during installation");
    match fs::rename(&stage, &destination) {
        Ok(()) => Ok(destination.join(BUNDLE)),
        // Another process may have installed the same pin while we were downloading.
        Err(error) => check_bundle(&destination)
            .with_context(|| format!("publishing browser engine bundle: {error}")),
    }
}

fn download(archive: &Path, commands: &Receiver<Outbound>) -> Result<()> {
    let (target, size, sha) = ARTIFACT;
    let url = format!(
        "https://anoma.ly/release/opencode-rune/files/{sha}/OpenCode-Browser-Engine-156.0.26.28-{target}.tar.zst"
    );
    let client = reqwest::blocking::Client::builder()
        .https_only(true)
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(120))
        .build()?;
    let start = Instant::now();
    let mut response = client.get(url).send()?.error_for_status()?;
    if let Some(length) = response.content_length() {
        ensure!(length == size, "incorrect browser engine download length");
    }
    let mut output = File::create_new(archive)?;
    let mut hash = Sha256::new();
    let mut total = 0;
    let mut buffer = [0; 64 * 1024];
    loop {
        ensure!(!commands.is_closed(), "browser closed during installation");
        ensure!(
            start.elapsed() < Duration::from_secs(180),
            "browser download timed out"
        );
        let count = response.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        ensure!(total <= size, "browser engine download is too large");
        hash.update(&buffer[..count]);
        output.write_all(&buffer[..count])?;
    }
    ensure!(total == size, "incorrect browser engine download length");
    ensure!(
        format!("{:x}", hash.finalize()) == sha,
        "incorrect browser engine download SHA-256"
    );
    Ok(())
}

fn bundle_path(path: &Path) -> Result<&Path> {
    let mut components = path.components();
    ensure!(
        components.next() == Some(Component::Normal(BUNDLE.as_ref()))
            && components.all(|component| matches!(component, Component::Normal(_))),
        "unsafe browser archive path: {}",
        path.display()
    );
    Ok(path)
}

fn unpack(source: impl Read, destination: &Path, commands: &Receiver<Outbound>) -> Result<()> {
    let mut archive = tar::Archive::new(source);
    let mut remaining = MAX_EXTRACTED;
    for (index, entry) in archive.entries()?.raw(true).enumerate() {
        ensure!(!commands.is_closed(), "browser closed during installation");
        ensure!(
            index < MAX_ENTRIES,
            "browser engine archive has too many entries"
        );
        let mut entry = entry?;
        let path = destination.join(bundle_path(&entry.path()?)?);
        let kind = entry.header().entry_type();
        let size = entry.size();
        if kind.is_dir() {
            ensure!(size == 0, "invalid browser archive directory entry");
            fs::create_dir_all(path)?;
        } else if kind.is_file() {
            remaining = remaining
                .checked_sub(size)
                .context("browser engine archive is too large")?;
            let mode = 0o644 | (entry.header().mode()? & 0o111);
            fs::create_dir_all(path.parent().context("missing browser archive parent")?)?;
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)?;
            ensure!(
                io::copy(&mut entry, &mut output)? == size,
                "truncated browser archive entry"
            );
            output.set_permissions(Permissions::from_mode(mode))?;
        } else {
            bail!("unsupported browser archive entry type");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_archive_paths_outside_the_bundle() {
        assert!(bundle_path(Path::new("OpenCode Browser Engine/locales/en-US.pak")).is_ok());
        for path in [
            "/OpenCode Browser Engine/libcef.so",
            "OpenCode Browser Engine/../escape",
            "other/file",
        ] {
            assert!(bundle_path(Path::new(path)).is_err(), "{path}");
        }
    }

    #[test]
    fn decodes_bgra_without_swapping_channels() {
        let line = br#"{"type":"cpu_frame","tab_id":5,"frame_id":7,"width":1,"height":1,"bgra_b64":"AQID/w=="}"#;
        let msg: Incoming<'_> = serde_json::from_slice(line).unwrap();
        assert_eq!(msg.tab_id, Some(5));
        let image = msg.paint(line).unwrap().decode(line).unwrap();
        assert_eq!(image.as_bytes(0), Some([1, 2, 3, 255].as_slice()));
    }

    #[test]
    fn rejects_oversized_or_invalid_frames() {
        for (width, height, bgra_b64) in [
            (0, 1, ""),
            (MAX_FRAME_SIDE + 1, 1, ""),
            (MAX_FRAME_SIDE, MAX_FRAME_SIDE, ""),
            (1, 1, ""),
            (1, 1, "AQID////"),
            (1, 1, "AQID//=="),
        ] {
            let message = Incoming {
                kind: "cpu_frame",
                tab_id: Some(1),
                method: None,
                width: Some(width),
                height: Some(height),
                bgra_b64: Some(bgra_b64),
            };
            let line = bgra_b64.as_bytes();
            assert!(message.paint(line).and_then(|paint| paint.decode(line)).is_err());
        }
    }

    #[test]
    fn coalesces_raw_paints_per_subscription() {
        let frames = LatestFrames::default();
        let publish = |id, payload: &str| {
            let mut buffer = payload.as_bytes().to_vec();
            let paint = Paint { width: 1, height: 1, bgra: 0..buffer.len() };
            frames.replace(id, Ok(paint), &mut buffer).unwrap();
        };
        let queued = || {
            let mut updates = Vec::new();
            assert_eq!(frames.ready.len(), 1);
            assert!(smol::block_on(frames.wait(&mut updates)));
            updates
        };
        publish(1, "AQID/w==");
        assert!(frames.ready.is_empty());
        frames.show(None, Some(1));
        frames.show(None, Some(2));
        publish(1, "invalid!");
        publish(2, "AQID/w==");
        let mut updates = queued();
        assert_eq!(updates.len(), 2);
        updates.sort_by_key(|update| update.request.tab_id);
        let first = updates[0].request;
        let other = updates[1].request;
        publish(1, "AQID/w==");
        assert!(frames.ready.is_empty());
        frames.request_decode(first);
        let (request, raw) = frames.take().unwrap();
        assert_eq!(request.tab_id, first.tab_id);
        let image = raw.paint.decode(&raw.line).unwrap();
        publish(1, "AAAA/w==");
        assert!(frames.ready.is_empty());
        frames.finish(first, raw.line, Ok(image));
        let update = queued().pop().unwrap();
        assert!(update.schedule);
        assert_eq!(
            update.image.unwrap().unwrap().as_bytes(0),
            Some([1, 2, 3, 255].as_slice())
        );

        frames.request_decode(first);
        let (_, old) = frames.take().unwrap();
        frames.show(Some(1), None);
        frames.show(None, Some(1));
        publish(1, "AAAA/w==");
        let new = queued()[0].request;
        frames.request_decode(first);
        assert!(frames.take().is_none());
        let old_image = old.paint.decode(&old.line);
        frames.finish(first, old.line, old_image);
        assert!(frames.ready.is_empty());
        frames.request_decode(new);
        assert_eq!(frames.take().unwrap().1.line.as_slice(), b"AAAA/w==");
        frames.request_decode(other);
        assert_eq!(frames.take().unwrap().0.tab_id, 2);
    }
}
