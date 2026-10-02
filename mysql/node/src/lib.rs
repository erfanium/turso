//! Node.js addon that runs the MySQL frontend inside the Node process.
//!
//! A [`Channel`] is the server end of one client connection. The client
//! writes MySQL protocol bytes into it and receives the server's bytes
//! through a callback, so an unmodified MySQL driver can use it as its
//! transport: there is no socket and no server process. Databases live in
//! the memory of the process and disappear with it.

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, OnceLock};

use napi::bindgen_prelude::Buffer;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi::Status;
use napi_derive::napi;
use turso_mysql::session::Engine;

/// Weak, so an open connection does not keep the process alive.
type DataCallback = ThreadsafeFunction<Buffer, (), Buffer, Status, false, true>;
type CloseCallback = ThreadsafeFunction<(), (), (), Status, false, true>;

fn engine() -> Arc<Engine> {
    static ENGINE: OnceLock<Arc<Engine>> = OnceLock::new();
    ENGINE.get_or_init(|| Engine::new(None)).clone()
}

struct ChannelReader {
    rx: Receiver<Vec<u8>>,
    chunk: Vec<u8>,
    pos: usize,
}

impl Read for ChannelReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.pos == self.chunk.len() {
            match self.rx.recv() {
                Ok(chunk) => {
                    self.chunk = chunk;
                    self.pos = 0;
                }
                // The client end is gone: end of stream.
                Err(_) => return Ok(0),
            }
        }
        let n = out.len().min(self.chunk.len() - self.pos);
        out[..n].copy_from_slice(&self.chunk[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

struct CallbackWriter {
    pending: Vec<u8>,
    on_data: DataCallback,
}

impl Write for CallbackWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.pending.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let chunk = std::mem::take(&mut self.pending);
        let status = self
            .on_data
            .call(chunk.into(), ThreadsafeFunctionCallMode::NonBlocking);
        if status == Status::Ok {
            Ok(())
        } else {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "client is gone"))
        }
    }
}

/// Drop an in-memory database by name. Returns whether it existed.
#[napi]
pub fn drop_database(name: String) -> bool {
    engine().drop_database(&name)
}

#[napi]
pub struct Channel {
    tx: Option<Sender<Vec<u8>>>,
}

#[napi]
impl Channel {
    /// Open a connection. `on_data` receives the server's bytes, starting
    /// with the handshake; `on_close` fires when the server side ends.
    #[napi(constructor)]
    pub fn new(on_data: DataCallback, on_close: CloseCallback) -> Self {
        static NEXT_ID: AtomicU32 = AtomicU32::new(1);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = channel();
        let trace = std::env::var_os("TURSO_MYSQL_TRACE").is_some();
        std::thread::Builder::new()
            .name(format!("turso-mysql-{id}"))
            .spawn(move || {
                let reader = ChannelReader {
                    rx,
                    chunk: Vec::new(),
                    pos: 0,
                };
                let writer = CallbackWriter {
                    pending: Vec::new(),
                    on_data,
                };
                let _ = turso_mysql_server::serve(reader, writer, engine(), id, trace);
                on_close.call((), ThreadsafeFunctionCallMode::NonBlocking);
            })
            .expect("spawn connection thread");
        Self { tx: Some(tx) }
    }

    /// Send client bytes to the server.
    #[napi]
    pub fn write(&self, data: Buffer) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(data.to_vec());
        }
    }

    /// Close the connection; an open transaction is rolled back.
    #[napi]
    pub fn close(&mut self) {
        self.tx = None;
    }
}
