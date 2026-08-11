use std::{
    fs::File,
    io::{self, Read},
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
};

const MAX_CAPTURED_BYTES: usize = 1024 * 1024;

pub fn ioctl_request(request: impl Into<nix::libc::c_ulong>) -> nix::libc::c_ulong {
    request.into()
}

pub struct PtyDrain {
    output: Arc<Mutex<Vec<u8>>>,
    handle: Option<JoinHandle<io::Result<()>>>,
}

impl PtyDrain {
    pub fn start(mut master: File) -> Self {
        let output = Arc::new(Mutex::new(Vec::new()));
        let thread_output = Arc::clone(&output);
        let handle = thread::spawn(move || {
            let mut buffer = [0_u8; 4096];
            loop {
                match master.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        let mut output = thread_output.lock().unwrap();
                        let remaining = MAX_CAPTURED_BYTES.saturating_sub(output.len());
                        output.extend_from_slice(&buffer[..count.min(remaining)]);
                    }
                    Err(error) if error.raw_os_error() == Some(nix::libc::EIO) => break,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                }
            }
            Ok(())
        });
        Self {
            output,
            handle: Some(handle),
        }
    }

    pub fn join(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle
                .join()
                .expect("PTY drain thread panicked")
                .expect("failed to drain PTY output");
        }
    }

    pub fn output(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }
}
