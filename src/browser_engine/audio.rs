use smol::future::race;
use smol::io::{AsyncReadExt as _, AsyncWriteExt as _};
use smol::lock::Mutex;
use smol::net::unix::{UnixListener, UnixStream};
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(super) const SANDBOX_SERVER: &str = "unix:/ipc/audio.sock";
const MAX_CLIENTS: usize = 8;
const MAX_PAYLOAD: usize = 256 * 1024;
const HEADER: usize = 20;
const CONTROL: u32 = u32::MAX;
const U32_TAG: u8 = b'L';

// PulseAudio src/pulsecore/native-common.h and pstream.c. No descriptor passing or shared memory.
const AUTH: u32 = 8;
const SUBSCRIBE: u32 = 35;
const SINK_AND_SERVER: u32 = (1 << 0) | (1 << 7);

pub(super) struct Playback {
    host: PathBuf,
    listener: UnixListener,
}

impl Playback {
    pub(super) fn bind(ipc: &Path) -> io::Result<Option<Self>> {
        let Some(host) = host_socket() else {
            return Ok(None);
        };
        Ok(Some(Self {
            host,
            listener: UnixListener::bind(ipc.join("audio.sock"))?,
        }))
    }

    pub(super) async fn run(self) {
        let host = Arc::new(self.host);
        let mut clients: Vec<smol::Task<()>> = Vec::with_capacity(MAX_CLIENTS);
        while let Ok((browser, _)) = self.listener.accept().await {
            clients.retain(|client| !client.is_finished());
            if clients.len() == MAX_CLIENTS {
                continue;
            }
            let host = Arc::clone(&host);
            clients.push(smol::spawn(async move {
                if let Ok(host) = UnixStream::connect(host.as_path()).await {
                    let _ = connection(browser, host).await;
                }
            }));
        }
    }
}

fn host_socket() -> Option<PathBuf> {
    if let Some(server) = std::env::var_os("PULSE_SERVER") {
        let server = server.as_bytes();
        let path = Path::new(OsStr::from_bytes(server.strip_prefix(b"unix:").unwrap_or(server)));
        return path.is_absolute().then(|| path.to_owned());
    }
    let runtime = std::env::var_os("PULSE_RUNTIME_PATH")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("XDG_RUNTIME_DIR").map(|path| PathBuf::from(path).join("pulse"))
        })?;
    runtime.is_absolute().then(|| runtime.join("native"))
}

async fn connection(browser: UnixStream, host: UnixStream) -> io::Result<()> {
    let browser_write = Mutex::new(browser.clone());
    race(
        browser_to_host(browser, host.clone(), &browser_write),
        host_to_browser(host, &browser_write),
    )
    .await
}

struct Packet {
    bytes: Vec<u8>,
    end: usize,
}

impl Packet {
    fn new() -> Self {
        Self {
            bytes: vec![0; HEADER],
            end: HEADER,
        }
    }

    async fn read(&mut self, socket: &mut UnixStream) -> io::Result<()> {
        socket.read_exact(&mut self.bytes[..HEADER]).await?;
        let len = word(&self.bytes[..4]) as usize;
        if len == 0 || len > MAX_PAYLOAD {
            return Err(invalid());
        }
        self.end = HEADER + len;
        if self.bytes.len() < self.end {
            self.bytes.resize(self.end, 0);
        }
        socket.read_exact(&mut self.bytes[HEADER..self.end]).await
    }

    fn channel(&self) -> u32 {
        word(&self.bytes[4..8])
    }

    fn control(&self) -> bool {
        self.channel() == CONTROL && self.bytes[8..HEADER] == [0; 12]
    }

    fn bytes(&self) -> &[u8] {
        &self.bytes[..self.end]
    }
}

fn word(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes.try_into().unwrap())
}

fn native_u32(bytes: &[u8]) -> io::Result<u32> {
    match bytes {
        [U32_TAG, number @ ..] if number.len() == 4 => Ok(word(number)),
        _ => Err(invalid()),
    }
}

fn invalid() -> io::Error {
    io::ErrorKind::InvalidData.into()
}

fn authorize(command: u32, args: &mut [u8]) -> io::Result<bool> {
    match command {
        AUTH => {
            let version = args.get_mut(..5).ok_or_else(invalid)?;
            let socket_only = native_u32(version)? & 0xffff;
            version[1..].copy_from_slice(&socket_only.to_be_bytes());
        }
        SUBSCRIBE => {
            let mask = native_u32(args)? & SINK_AND_SERVER;
            args[1..].copy_from_slice(&mask.to_be_bytes());
        }
        3 | 4 | 12 | 14 | 41..=43 | 46 | 60 | 72 | 74 | 81 | 84 => {} // Own playback streams
        10 | 20..=22 => {} // Playback device discovery
        9 | 82 | 85 => {} // Own client metadata
        _ => return Ok(false),
    }
    Ok(true)
}

async fn browser_to_host(
    mut browser: UnixStream,
    mut host: UnixStream,
    browser_write: &Mutex<UnixStream>,
) -> io::Result<()> {
    let mut packet = Packet::new();
    loop {
        packet.read(&mut browser).await?;
        if packet.channel() == CONTROL {
            if !packet.control() {
                return Err(invalid());
            }
            let payload = &mut packet.bytes[HEADER..packet.end];
            let (fields, args) = payload.split_at_mut_checked(10).ok_or_else(invalid)?;
            let command = native_u32(&fields[..5])?;
            let tag = native_u32(&fields[5..])?;
            if !authorize(command, args)? {
                browser_write.lock().await.write_all(&access_denied(tag)).await?;
                continue;
            }
        } else if word(&packet.bytes[16..HEADER]) > 3 {
            // Raw PCM allows only the four seek modes, not any SHM flags.
            return Err(invalid());
        }
        host.write_all(packet.bytes()).await?;
    }
}

async fn host_to_browser(mut host: UnixStream, browser: &Mutex<UnixStream>) -> io::Result<()> {
    let mut packet = Packet::new();
    loop {
        packet.read(&mut host).await?;
        if !packet.control() {
            return Err(invalid());
        }
        // Only replies and playback notifications; never PCM or descriptor negotiation.
        let command = native_u32(packet.bytes().get(HEADER..HEADER + 5).ok_or_else(invalid)?)?;
        if !matches!(command, 0 | 2 | 61..=64 | 66 | 76 | 78 | 86 | 91 | 92 | 94) {
            return Err(invalid());
        }
        browser.lock().await.write_all(packet.bytes()).await?;
    }
}

fn access_denied(tag: u32) -> [u8; HEADER + 15] {
    let mut reply = [0; HEADER + 15];
    reply[3] = 15;
    reply[4..8].copy_from_slice(&CONTROL.to_be_bytes());
    reply[HEADER] = U32_TAG; // PA_COMMAND_ERROR = 0
    reply[HEADER + 5] = U32_TAG;
    reply[HEADER + 6..HEADER + 10].copy_from_slice(&tag.to_be_bytes());
    reply[HEADER + 10] = U32_TAG;
    reply[HEADER + 14] = 1; // PA_ERR_ACCESS
    reply
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(code: u32, tag: u32, args: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0; HEADER];
        bytes[..4].copy_from_slice(&(10 + args.len() as u32).to_be_bytes());
        bytes[4..8].copy_from_slice(&CONTROL.to_be_bytes());
        for value in [code, tag] {
            bytes.push(U32_TAG);
            bytes.extend_from_slice(&value.to_be_bytes());
        }
        bytes.extend_from_slice(args);
        bytes
    }

    #[test]
    fn browser_audio_denies_capture_and_host_commands_without_interrupting_playback() {
        smol::block_on(async {
            let (mut browser, relay_in) = UnixStream::pair().unwrap();
            let (relay_out, mut host) = UnixStream::pair().unwrap();
            let _relay = smol::spawn(connection(relay_in, relay_out));
            for code in [5, 23, 36, 48, 51, 87, 103] {
                browser.write_all(&command(code, code, &[])).await.unwrap();
                let mut denied = [0; HEADER + 15];
                browser.read_exact(&mut denied).await.unwrap();
                assert_eq!(denied, access_denied(code));
            }
            browser
                .write_all(&command(AUTH, 1, &[U32_TAG, 0xc0, 0, 0, 35]))
                .await
                .unwrap();
            browser.write_all(&command(3, 2, &[])).await.unwrap();
            let mut packet = Packet::new();
            packet.read(&mut host).await.unwrap();
            assert_eq!(packet.bytes(), command(AUTH, 1, &[U32_TAG, 0, 0, 0, 35]));
            packet.read(&mut host).await.unwrap();
            assert_eq!(packet.bytes(), command(3, 2, &[]));
        });
    }

    #[test]
    fn browser_audio_never_forwards_host_pcm_to_the_browser() {
        smol::block_on(async {
            let (mut browser, relay_in) = UnixStream::pair().unwrap();
            let (relay_out, mut host) = UnixStream::pair().unwrap();
            let relay = smol::spawn(connection(relay_in, relay_out));
            let mut frame = [0; HEADER + 4];
            frame[3] = 4;
            frame[HEADER..].copy_from_slice(&[1, 2, 3, 4]);
            host.write_all(&frame).await.unwrap();
            assert_eq!(relay.await.unwrap_err().kind(), io::ErrorKind::InvalidData);
            assert_eq!(browser.read(&mut frame).await.unwrap(), 0);
        });
    }
}
