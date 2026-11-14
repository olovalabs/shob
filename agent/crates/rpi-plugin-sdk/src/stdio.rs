//! Pollable input for an exclusive stdio server. No background stdin reader survives shutdown.
use std::io;

pub enum InputLine {
    Pending,
    Line(String),
    Eof,
}

#[derive(Default)]
pub struct StdinLines {
    buffer: Vec<u8>,
    eof: bool,
    scanned: usize,
}

impl StdinLines {
    /// Poll without waiting for input. Callers can wait on their own cancellation signal.
    pub fn poll_line(&mut self) -> io::Result<InputLine> {
        if let Some(line) = self.take_line()? {
            return Ok(InputLine::Line(line));
        }
        if self.eof {
            return Ok(InputLine::Eof);
        }
        let mut bytes = [0u8; 4096];
        // Bound work between cancellation checks while allowing large JSON lines.
        for _ in 0..16 {
            let Some(count) = read_available(&mut bytes)? else {
                return Ok(InputLine::Pending);
            };
            if count == 0 {
                self.eof = true;
            } else {
                self.buffer.extend_from_slice(&bytes[..count]);
            }
            if let Some(line) = self.take_line()? {
                return Ok(InputLine::Line(line));
            }
            if self.eof {
                return Ok(InputLine::Eof);
            }
        }
        Ok(InputLine::Pending)
    }

    fn take_line(&mut self) -> io::Result<Option<String>> {
        let length = self.buffer[self.scanned..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|index| self.scanned + index + 1)
            .or_else(|| (self.eof && !self.buffer.is_empty()).then_some(self.buffer.len()));
        let Some(length) = length else {
            self.scanned = self.buffer.len();
            return Ok(None);
        };
        self.scanned = 0;
        let mut line: Vec<u8> = self.buffer.drain(..length).collect();
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        String::from_utf8(line)
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }
}

#[cfg(unix)]
fn read_available(bytes: &mut [u8]) -> io::Result<Option<usize>> {
    read_fd_available(libc::STDIN_FILENO, bytes)
}

#[cfg(unix)]
fn read_fd_available(fd: libc::c_int, bytes: &mut [u8]) -> io::Result<Option<usize>> {
    let mut descriptor = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: the descriptor and writable byte slice are valid for each syscall.
    let ready = unsafe { libc::poll(&mut descriptor, 1, 0) };
    if ready < 0 {
        return interrupted_or_error();
    }
    if ready == 0 {
        return Ok(None);
    }
    if descriptor.revents & libc::POLLNVAL != 0 {
        return Err(io::Error::from_raw_os_error(libc::EBADF));
    }
    // The caller exclusively owns stdin. After poll reports input/EOF, read cannot
    // race another consumer for those bytes. Each read is bounded to this chunk.
    let count = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
    if count < 0 {
        interrupted_or_error()
    } else {
        Ok(Some(count as usize))
    }
}

#[cfg(unix)]
fn interrupted_or_error() -> io::Result<Option<usize>> {
    let error = io::Error::last_os_error();
    if matches!(
        error.kind(),
        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
    ) {
        Ok(None)
    } else {
        Err(error)
    }
}

#[cfg(windows)]
fn read_available(bytes: &mut [u8]) -> io::Result<Option<usize>> {
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE};
    // SAFETY: GetStdHandle returns a borrowed process handle; never close it.
    read_handle_available(unsafe { GetStdHandle(STD_INPUT_HANDLE) }, bytes)
}

#[cfg(windows)]
fn read_handle_available(
    input: windows_sys::Win32::Foundation::HANDLE,
    bytes: &mut [u8],
) -> io::Result<Option<usize>> {
    use windows_sys::Win32::Foundation::{
        GetLastError, ERROR_BROKEN_PIPE, ERROR_HANDLE_EOF, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileType, ReadFile, FILE_TYPE_DISK, FILE_TYPE_PIPE,
    };
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;
    if input.is_null() || input == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let kind = unsafe { GetFileType(input) };
    let capacity = if kind == FILE_TYPE_PIPE {
        let mut available = 0;
        // Peek works on anonymous and named pipes and never consumes input.
        let ok = unsafe {
            PeekNamedPipe(
                input,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            let error = unsafe { GetLastError() };
            return if matches!(error, ERROR_BROKEN_PIPE | ERROR_HANDLE_EOF) {
                Ok(Some(0))
            } else {
                Err(io::Error::from_raw_os_error(error as i32))
            };
        }
        if available == 0 {
            return Ok(None);
        }
        available.min(bytes.len() as u32)
    } else if kind == FILE_TYPE_DISK {
        bytes.len() as u32
    } else {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "stdio server requires piped or file stdin",
        ));
    };
    let mut count = 0;
    // SAFETY: capacity is within the valid buffer and available pipe bytes.
    let ok = unsafe {
        ReadFile(
            input,
            bytes.as_mut_ptr(),
            capacity,
            &mut count,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        let error = unsafe { GetLastError() };
        return if matches!(error, ERROR_BROKEN_PIPE | ERROR_HANDLE_EOF) {
            Ok(Some(0))
        } else {
            Err(io::Error::from_raw_os_error(error as i32))
        };
    }
    Ok(Some(count as usize))
}

#[cfg(not(any(unix, windows)))]
fn read_available(_: &mut [u8]) -> io::Result<Option<usize>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "stdio polling is unsupported on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    #[test]
    fn pipe_poll_is_idle_then_readable_then_eof_without_a_reader_thread() {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::Storage::FileSystem::WriteFile;
        use windows_sys::Win32::System::Pipes::CreatePipe;
        let mut input = std::ptr::null_mut();
        let mut output = std::ptr::null_mut();
        assert_ne!(
            unsafe { CreatePipe(&mut input, &mut output, std::ptr::null(), 0) },
            0
        );
        let input = unsafe { OwnedHandle::from_raw_handle(input) };
        let output = unsafe { OwnedHandle::from_raw_handle(output) };
        let mut bytes = [0u8; 16];
        assert_eq!(
            read_handle_available(input.as_raw_handle(), &mut bytes).unwrap(),
            None
        );
        let mut written = 0;
        assert_ne!(
            unsafe {
                WriteFile(
                    output.as_raw_handle(),
                    b"abc".as_ptr(),
                    3,
                    &mut written,
                    std::ptr::null_mut(),
                )
            },
            0
        );
        assert_eq!(
            read_handle_available(input.as_raw_handle(), &mut bytes).unwrap(),
            Some(3)
        );
        assert_eq!(&bytes[..3], b"abc");
        drop(output);
        assert_eq!(
            read_handle_available(input.as_raw_handle(), &mut bytes).unwrap(),
            Some(0)
        );
    }

    #[cfg(unix)]
    #[test]
    fn pipe_poll_is_idle_then_readable_then_eof_without_a_reader_thread() {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        let mut descriptors = [0; 2];
        assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
        let input = unsafe { OwnedFd::from_raw_fd(descriptors[0]) };
        let output = unsafe { OwnedFd::from_raw_fd(descriptors[1]) };
        let mut bytes = [0u8; 16];
        assert_eq!(
            read_fd_available(input.as_raw_fd(), &mut bytes).unwrap(),
            None
        );
        assert_eq!(
            unsafe { libc::write(output.as_raw_fd(), b"abc".as_ptr().cast(), 3) },
            3
        );
        assert_eq!(
            read_fd_available(input.as_raw_fd(), &mut bytes).unwrap(),
            Some(3)
        );
        assert_eq!(&bytes[..3], b"abc");
        drop(output);
        assert_eq!(
            read_fd_available(input.as_raw_fd(), &mut bytes).unwrap(),
            Some(0)
        );
    }

    #[test]
    fn retains_partial_utf8_and_final_line_without_newline() {
        let mut reader = StdinLines::default();
        reader
            .buffer
            .extend_from_slice(&"中文\r\nnext".as_bytes()[..2]);
        assert!(reader.take_line().unwrap().is_none());
        reader
            .buffer
            .extend_from_slice(&"中文\r\nnext".as_bytes()[2..]);
        assert_eq!(reader.take_line().unwrap().as_deref(), Some("中文"));
        assert!(reader.take_line().unwrap().is_none());
        reader.eof = true;
        assert_eq!(reader.take_line().unwrap().as_deref(), Some("next"));
        assert!(reader.take_line().unwrap().is_none());
    }
}
