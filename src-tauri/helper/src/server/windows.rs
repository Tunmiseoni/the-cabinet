use super::{Config, Shared};
use crate::win;
use std::fs::File;
use std::io::BufReader;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::PathBuf;
use std::sync::Arc;
use windows_sys::Win32::Foundation::{GetLastError, ERROR_PIPE_CONNECTED, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_GENERIC_READ, FILE_WRITE_DATA, PIPE_ACCESS_DUPLEX,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};

const BUFFER: u32 = 64 * 1024;
pub const CLIENT_ACCESS: u32 = FILE_GENERIC_READ | FILE_WRITE_DATA;

pub struct Server {
    endpoint: PathBuf,
    pipe: Vec<u16>,
    descriptor: win::Descriptor,
    first: Option<OwnedHandle>,
    shared: Arc<Shared>,
}

pub fn access_list(server: &str, client: &str) -> String {
    let mut sddl = format!("D:P(A;;FA;;;{})", win::SYSTEM);
    if server != win::SYSTEM {
        sddl.push_str(&format!("(A;;FA;;;{server})"));
    }
    sddl.push_str(&format!("(A;;{CLIENT_ACCESS:#x};;;{client})"));
    sddl
}

fn instance(
    pipe: &[u16],
    descriptor: &win::Descriptor,
    first: bool,
) -> std::io::Result<OwnedHandle> {
    let attributes = descriptor.attributes();
    let flags = if first {
        FILE_FLAG_FIRST_PIPE_INSTANCE
    } else {
        0
    };
    let handle = unsafe {
        CreateNamedPipeW(
            pipe.as_ptr(),
            PIPE_ACCESS_DUPLEX | flags,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            BUFFER,
            BUFFER,
            0,
            &attributes,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

impl Shared {
    fn serve(&self, stream: File) -> std::io::Result<()> {
        let writer = stream.try_clone()?;
        self.converse(BufReader::new(stream), writer)
    }
}

impl Server {
    pub fn bind(config: Config) -> Result<Self, String> {
        let me = win::current_user()?;
        let client = config.user.clone().unwrap_or_else(|| me.clone());
        win::check_sid(&client)?;
        let descriptor = win::Descriptor::parse(&access_list(&me, &client))?;
        let endpoint = config.endpoint.clone();
        let pipe = win::wide(&endpoint);
        let shared = Shared::open(config)?;
        let first = instance(&pipe, &descriptor, true)
            .map_err(|err| format!("cannot listen on {}: {err}", endpoint.display()))?;
        Ok(Self {
            endpoint,
            pipe,
            descriptor,
            first: Some(first),
            shared: Arc::new(shared),
        })
    }

    pub fn run(mut self) {
        let mut next = self.first.take();
        loop {
            let handle = match next.take() {
                Some(handle) => handle,
                None => match instance(&self.pipe, &self.descriptor, false) {
                    Ok(handle) => handle,
                    Err(err) => {
                        log::warn!(
                            "cannot open another instance of {}: {err}",
                            self.endpoint.display()
                        );
                        std::thread::sleep(std::time::Duration::from_secs(1));
                        continue;
                    }
                },
            };
            let connected =
                unsafe { ConnectNamedPipe(handle.as_raw_handle(), std::ptr::null_mut()) } != 0
                    || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
            if !connected {
                log::warn!(
                    "cannot accept a connection: {}",
                    std::io::Error::last_os_error()
                );
                continue;
            }
            let Some(admitted) = self.shared.admit() else {
                continue;
            };
            let stream = File::from(handle);
            std::thread::spawn(move || {
                if let Err(err) = admitted.0.serve(stream) {
                    log::warn!("a connection failed: {err}");
                }
            });
        }
    }
}
