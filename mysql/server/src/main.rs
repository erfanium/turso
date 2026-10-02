//! `tursomysql` — an in-memory database server that speaks the MySQL wire
//! protocol over TCP.
//!
//! Every database name a client connects with is created on first use. With
//! `--template NAME`, a new database starts as a copy of database `NAME`, so
//! clients that each pick a unique database name get isolated copies of one
//! seeded schema.

use std::io::{self, BufReader, BufWriter, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicU32, Ordering};

use turso_mysql::session::Engine;
use turso_mysql_server::serve;

fn main() -> anyhow::Result<()> {
    let mut listen = "127.0.0.1:0".to_string();
    let mut template = None;
    let mut trace = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => {
                listen = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--listen ADDR"))?
            }
            "--template" => template = args.next(),
            "--trace" => trace = true,
            "--help" | "-h" => {
                println!("usage: tursomysql [--listen ADDR:PORT] [--template DATABASE] [--trace]");
                return Ok(());
            }
            other => anyhow::bail!("unknown argument `{other}`"),
        }
    }

    let listener = TcpListener::bind(&listen)?;
    // The first stdout line is machine-readable, so a parent process can
    // bind to port 0 and learn the port.
    println!("listening {}", listener.local_addr()?);
    io::stdout().flush()?;

    let engine = Engine::new(template);
    let next_id = AtomicU32::new(1);
    for stream in listener.incoming() {
        let stream = stream?;
        let engine = engine.clone();
        let id = next_id.fetch_add(1, Ordering::Relaxed);
        std::thread::spawn(move || {
            let run = || -> io::Result<()> {
                stream.set_nodelay(true)?;
                let reader = BufReader::new(stream.try_clone()?);
                serve(reader, BufWriter::new(stream), engine, id, trace)
            };
            if let Err(e) = run() {
                if trace {
                    eprintln!("[{id}] connection error: {e}");
                }
            }
        });
    }
    Ok(())
}
