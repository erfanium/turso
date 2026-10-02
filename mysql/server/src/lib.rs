//! The MySQL wire protocol (text protocol only) over any byte stream.
//!
//! [`serve`] runs one client connection to completion. The transport is the
//! caller's: a TCP socket for the standalone server, or an in-memory pipe when
//! the engine is embedded in the client's own process.

use std::io::{self, Read, Write};
use std::sync::Arc;

use turso_core::Value;
use turso_mysql::catalog::MyType;
use turso_mysql::session::{ColumnMeta, Engine, MyError, Outcome, ResultSet, Session};

const SERVER_VERSION: &str = "8.0.33-turso";

// Capability flags.
const CLIENT_LONG_PASSWORD: u32 = 0x1;
const CLIENT_FOUND_ROWS: u32 = 0x2;
const CLIENT_LONG_FLAG: u32 = 0x4;
const CLIENT_CONNECT_WITH_DB: u32 = 0x8;
const CLIENT_PROTOCOL_41: u32 = 0x200;
const CLIENT_TRANSACTIONS: u32 = 0x2000;
const CLIENT_SECURE_CONNECTION: u32 = 0x8000;
const CLIENT_PLUGIN_AUTH: u32 = 0x8_0000;
const CLIENT_CONNECT_ATTRS: u32 = 0x10_0000;
const CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA: u32 = 0x20_0000;

const SERVER_CAPABILITIES: u32 = CLIENT_LONG_PASSWORD
    | CLIENT_FOUND_ROWS
    | CLIENT_LONG_FLAG
    | CLIENT_CONNECT_WITH_DB
    | CLIENT_PROTOCOL_41
    | CLIENT_TRANSACTIONS
    | CLIENT_SECURE_CONNECTION
    | CLIENT_PLUGIN_AUTH
    | CLIENT_CONNECT_ATTRS
    | CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA;

const STATUS_IN_TRANS: u16 = 0x1;
const STATUS_AUTOCOMMIT: u16 = 0x2;

const CHARSET_UTF8MB4: u16 = 45;
const CHARSET_BINARY: u16 = 63;

// Column types.
const TYPE_TINY: u8 = 1;
const TYPE_SHORT: u8 = 2;
const TYPE_LONG: u8 = 3;
const TYPE_FLOAT: u8 = 4;
const TYPE_DOUBLE: u8 = 5;
const TYPE_TIMESTAMP: u8 = 7;
const TYPE_LONGLONG: u8 = 8;
const TYPE_INT24: u8 = 9;
const TYPE_DATE: u8 = 10;
const TYPE_TIME: u8 = 11;
const TYPE_DATETIME: u8 = 12;
const TYPE_YEAR: u8 = 13;
const TYPE_JSON: u8 = 245;
const TYPE_NEWDECIMAL: u8 = 246;
const TYPE_BLOB: u8 = 252;
const TYPE_VAR_STRING: u8 = 253;
const TYPE_STRING: u8 = 254;

const FLAG_UNSIGNED: u16 = 0x20;
const FLAG_BINARY: u16 = 0x80;

const MAX_PACKET: usize = 0xFF_FFFF;

struct Wire<R, W> {
    reader: R,
    writer: W,
    seq: u8,
}

impl<R: Read, W: Write> Wire<R, W> {
    fn read_packet(&mut self) -> io::Result<Vec<u8>> {
        let mut payload = Vec::new();
        loop {
            let mut header = [0u8; 4];
            self.reader.read_exact(&mut header)?;
            let len = u32::from_le_bytes([header[0], header[1], header[2], 0]) as usize;
            self.seq = header[3].wrapping_add(1);
            let start = payload.len();
            payload.resize(start + len, 0);
            self.reader.read_exact(&mut payload[start..])?;
            if len < MAX_PACKET {
                return Ok(payload);
            }
        }
    }

    fn write_packet(&mut self, payload: &[u8]) -> io::Result<()> {
        let mut rest = payload;
        loop {
            let chunk = rest.len().min(MAX_PACKET);
            let len = (chunk as u32).to_le_bytes();
            self.writer.write_all(&[len[0], len[1], len[2], self.seq])?;
            self.writer.write_all(&rest[..chunk])?;
            self.seq = self.seq.wrapping_add(1);
            rest = &rest[chunk..];
            // A payload that is an exact multiple of the maximum is followed
            // by an empty packet.
            if chunk < MAX_PACKET {
                return Ok(());
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }

    fn write_ok(
        &mut self,
        affected: u64,
        last_insert_id: u64,
        status: u16,
        info: &str,
    ) -> io::Result<()> {
        let mut p = vec![0x00];
        put_lenenc_int(&mut p, affected);
        put_lenenc_int(&mut p, last_insert_id);
        p.extend_from_slice(&status.to_le_bytes());
        p.extend_from_slice(&0u16.to_le_bytes());
        // Without CLIENT_SESSION_TRACK the info string runs to the end.
        p.extend_from_slice(info.as_bytes());
        self.write_packet(&p)
    }

    fn write_eof(&mut self, status: u16) -> io::Result<()> {
        let mut p = vec![0xfe];
        p.extend_from_slice(&0u16.to_le_bytes());
        p.extend_from_slice(&status.to_le_bytes());
        self.write_packet(&p)
    }

    fn write_err(&mut self, e: &MyError) -> io::Result<()> {
        let mut p = vec![0xff];
        p.extend_from_slice(&e.code.to_le_bytes());
        p.push(b'#');
        p.extend_from_slice(e.sqlstate.as_bytes());
        p.extend_from_slice(e.message.as_bytes());
        self.write_packet(&p)
    }
}

fn put_lenenc_int(buf: &mut Vec<u8>, n: u64) {
    if n < 251 {
        buf.push(n as u8);
    } else if n < 1 << 16 {
        buf.push(0xfc);
        buf.extend_from_slice(&(n as u16).to_le_bytes());
    } else if n < 1 << 24 {
        buf.push(0xfd);
        buf.extend_from_slice(&(n as u32).to_le_bytes()[..3]);
    } else {
        buf.push(0xfe);
        buf.extend_from_slice(&n.to_le_bytes());
    }
}

fn put_lenenc_bytes(buf: &mut Vec<u8>, bytes: &[u8]) {
    put_lenenc_int(buf, bytes.len() as u64);
    buf.extend_from_slice(bytes);
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let slice = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(slice)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }

    fn u32(&mut self) -> Option<u32> {
        self.take(4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn lenenc_int(&mut self) -> Option<u64> {
        Some(match self.u8()? {
            0xfc => {
                let b = self.take(2)?;
                u16::from_le_bytes([b[0], b[1]]) as u64
            }
            0xfd => {
                let b = self.take(3)?;
                u32::from_le_bytes([b[0], b[1], b[2], 0]) as u64
            }
            0xfe => {
                let b = self.take(8)?;
                u64::from_le_bytes(b.try_into().ok()?)
            }
            n => n as u64,
        })
    }

    fn cstr(&mut self) -> Option<&'a [u8]> {
        let rest = self.buf.get(self.pos..)?;
        let end = rest.iter().position(|&b| b == 0)?;
        self.pos += end + 1;
        Some(&rest[..end])
    }
}

/// Parse `HandshakeResponse41`, returning the database the client asked for.
fn parse_handshake_response(payload: &[u8]) -> Option<Option<String>> {
    let mut c = Cursor {
        buf: payload,
        pos: 0,
    };
    let capabilities = c.u32()?;
    c.u32()?; // max packet size
    c.u8()?; // charset
    c.take(23)?;
    c.cstr()?; // user
    if capabilities & CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA != 0 {
        let n = c.lenenc_int()? as usize;
        c.take(n)?;
    } else if capabilities & CLIENT_SECURE_CONNECTION != 0 {
        let n = c.u8()? as usize;
        c.take(n)?;
    } else {
        c.cstr()?;
    }
    if capabilities & CLIENT_CONNECT_WITH_DB != 0 {
        let db = c.cstr()?;
        if !db.is_empty() {
            return Some(Some(String::from_utf8_lossy(db).into_owned()));
        }
    }
    Some(None)
}

fn write_handshake<R: Read, W: Write>(wire: &mut Wire<R, W>, connection_id: u32) -> io::Result<()> {
    let salt = *b"tursomysqlsalt012345";
    let mut p = vec![10];
    p.extend_from_slice(SERVER_VERSION.as_bytes());
    p.push(0);
    p.extend_from_slice(&connection_id.to_le_bytes());
    p.extend_from_slice(&salt[..8]);
    p.push(0);
    p.extend_from_slice(&(SERVER_CAPABILITIES as u16).to_le_bytes());
    p.push(CHARSET_UTF8MB4 as u8);
    p.extend_from_slice(&STATUS_AUTOCOMMIT.to_le_bytes());
    p.extend_from_slice(&((SERVER_CAPABILITIES >> 16) as u16).to_le_bytes());
    p.push(21);
    p.extend_from_slice(&[0; 10]);
    p.extend_from_slice(&salt[8..]);
    p.push(0);
    p.extend_from_slice(b"mysql_native_password\0");
    wire.write_packet(&p)?;
    wire.flush()
}

struct WireColumn {
    ty: u8,
    charset: u16,
    flags: u16,
    decimals: u8,
    my_type: Option<MyType>,
}

/// Wire metadata for a result column: from the MySQL type when the column
/// traces back to a table column, otherwise from the values it holds.
fn wire_column(meta: &ColumnMeta, rows: &[Vec<Value>], index: usize) -> WireColumn {
    let numeric = |ty: u8, unsigned: bool| WireColumn {
        ty,
        charset: CHARSET_BINARY,
        flags: FLAG_BINARY | if unsigned { FLAG_UNSIGNED } else { 0 },
        decimals: 0,
        my_type: meta.ty,
    };
    let text = |ty: u8| WireColumn {
        ty,
        charset: CHARSET_UTF8MB4,
        flags: 0,
        decimals: 0,
        my_type: meta.ty,
    };
    let binary = |ty: u8| WireColumn {
        ty,
        charset: CHARSET_BINARY,
        flags: FLAG_BINARY,
        decimals: 0,
        my_type: meta.ty,
    };
    match meta.ty {
        Some(MyType::Int { bytes, unsigned }) => numeric(
            match bytes {
                1 => TYPE_TINY,
                2 => TYPE_SHORT,
                3 => TYPE_INT24,
                8 => TYPE_LONGLONG,
                _ => TYPE_LONG,
            },
            unsigned,
        ),
        Some(MyType::Decimal { scale, .. }) => WireColumn {
            decimals: scale as u8,
            ..numeric(TYPE_NEWDECIMAL, false)
        },
        Some(MyType::Float) => numeric(TYPE_FLOAT, false),
        Some(MyType::Double) => numeric(TYPE_DOUBLE, false),
        Some(MyType::Char | MyType::Enum) => text(TYPE_STRING),
        Some(MyType::Varchar) => text(TYPE_VAR_STRING),
        Some(MyType::Text) => text(TYPE_BLOB),
        Some(MyType::Binary) => binary(TYPE_STRING),
        Some(MyType::Blob) => binary(TYPE_BLOB),
        Some(MyType::Datetime { fsp }) => WireColumn {
            decimals: fsp as u8,
            ..binary(TYPE_DATETIME)
        },
        Some(MyType::Timestamp { fsp }) => WireColumn {
            decimals: fsp as u8,
            ..binary(TYPE_TIMESTAMP)
        },
        Some(MyType::Date) => binary(TYPE_DATE),
        Some(MyType::Time { fsp }) => WireColumn {
            decimals: fsp as u8,
            ..binary(TYPE_TIME)
        },
        Some(MyType::Year) => numeric(TYPE_YEAR, true),
        Some(MyType::Json) => binary(TYPE_JSON),
        None => {
            let sample = rows
                .iter()
                .map(|r| &r[index])
                .find(|v| !matches!(v, Value::Null));
            match sample {
                Some(Value::Blob(_)) => binary(TYPE_VAR_STRING),
                Some(Value::Text(_)) | None => text(TYPE_VAR_STRING),
                Some(v) if v.as_int().is_some() => numeric(TYPE_LONGLONG, false),
                Some(_) => numeric(TYPE_DOUBLE, false),
            }
        }
    }
}

/// Text-protocol encoding of one value.
fn encode_value(buf: &mut Vec<u8>, value: &Value, column: &WireColumn) {
    match value {
        Value::Null => buf.push(0xfb),
        Value::Blob(b) => put_lenenc_bytes(buf, b),
        Value::Text(t) => {
            let s = t.as_str();
            match column.my_type {
                // Stored without trailing fraction zeros; MySQL pads to the
                // column's precision.
                Some(MyType::Datetime { fsp } | MyType::Timestamp { fsp }) if fsp > 0 => {
                    put_lenenc_bytes(buf, pad_fraction(s, fsp as usize).as_bytes())
                }
                Some(MyType::Decimal { scale, .. }) => match s.parse::<f64>() {
                    Ok(x) => put_lenenc_bytes(buf, format!("{x:.*}", scale as usize).as_bytes()),
                    Err(_) => put_lenenc_bytes(buf, s.as_bytes()),
                },
                _ => put_lenenc_bytes(buf, s.as_bytes()),
            }
        }
        numeric => {
            let s = match (column.my_type, numeric.as_int()) {
                (Some(MyType::Decimal { scale, .. }), _) => {
                    format!("{:.*}", scale as usize, numeric.as_float())
                }
                (_, Some(i)) => i.to_string(),
                (_, None) => numeric.as_float().to_string(),
            };
            put_lenenc_bytes(buf, s.as_bytes());
        }
    }
}

fn pad_fraction(s: &str, fsp: usize) -> String {
    let (head, frac) = match s.split_once('.') {
        Some((h, f)) => (h, f),
        None => (s, ""),
    };
    format!("{head}.{frac:0<fsp$}")
}

fn write_result_set<R: Read, W: Write>(
    wire: &mut Wire<R, W>,
    rs: &ResultSet,
    status: u16,
) -> io::Result<()> {
    let columns: Vec<WireColumn> = rs
        .columns
        .iter()
        .enumerate()
        .map(|(i, meta)| wire_column(meta, &rs.rows, i))
        .collect();

    let mut p = Vec::new();
    put_lenenc_int(&mut p, columns.len() as u64);
    wire.write_packet(&p)?;

    for (meta, column) in rs.columns.iter().zip(&columns) {
        p.clear();
        put_lenenc_bytes(&mut p, b"def");
        put_lenenc_bytes(&mut p, b"");
        put_lenenc_bytes(&mut p, meta.table.as_bytes());
        put_lenenc_bytes(&mut p, meta.table.as_bytes());
        put_lenenc_bytes(&mut p, meta.name.as_bytes());
        put_lenenc_bytes(&mut p, meta.name.as_bytes());
        p.push(0x0c);
        p.extend_from_slice(&column.charset.to_le_bytes());
        p.extend_from_slice(&1024u32.to_le_bytes());
        p.push(column.ty);
        p.extend_from_slice(&column.flags.to_le_bytes());
        p.push(column.decimals);
        p.extend_from_slice(&[0, 0]);
        wire.write_packet(&p)?;
    }
    wire.write_eof(status)?;

    for row in &rs.rows {
        p.clear();
        for (value, column) in row.iter().zip(&columns) {
            encode_value(&mut p, value, column);
        }
        wire.write_packet(&p)?;
    }
    wire.write_eof(status)
}

fn status(session: &Session) -> u16 {
    if session.in_transaction() {
        STATUS_IN_TRANS
    } else {
        STATUS_AUTOCOMMIT
    }
}

/// Serve one client connection until it disconnects.
///
/// `writer` is flushed once per response, so it should buffer.
pub fn serve<R: Read, W: Write>(
    reader: R,
    writer: W,
    engine: Arc<Engine>,
    connection_id: u32,
    trace: bool,
) -> io::Result<()> {
    let mut wire = Wire {
        reader,
        writer,
        seq: 0,
    };
    let mut session = Session::new(engine);

    write_handshake(&mut wire, connection_id)?;
    let response = wire.read_packet()?;
    let database = parse_handshake_response(&response).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "malformed handshake response")
    })?;
    match database.map(|db| session.use_database(&db)) {
        Some(Err(e)) => {
            wire.write_err(&e)?;
            return wire.flush();
        }
        _ => wire.write_ok(0, 0, STATUS_AUTOCOMMIT, "")?,
    }
    wire.flush()?;

    loop {
        wire.seq = 0;
        let packet = match wire.read_packet() {
            Ok(p) => p,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };
        let Some((&command, body)) = packet.split_first() else {
            continue;
        };
        match command {
            // COM_QUIT
            0x01 => return Ok(()),
            // COM_INIT_DB
            0x02 => match session.use_database(&String::from_utf8_lossy(body)) {
                Ok(()) => wire.write_ok(0, 0, status(&session), "")?,
                Err(e) => wire.write_err(&e)?,
            },
            // COM_QUERY
            0x03 => {
                let sql = String::from_utf8_lossy(body);
                let outcome = session.execute(&sql);
                if trace {
                    match &outcome {
                        Ok(_) => eprintln!("[{connection_id}] {sql}"),
                        Err(e) => eprintln!("[{connection_id}] {sql}\n    !! {e}"),
                    }
                }
                match outcome {
                    Ok(Outcome::Ok {
                        affected_rows,
                        last_insert_id,
                        info,
                    }) => wire.write_ok(affected_rows, last_insert_id, status(&session), &info)?,
                    Ok(Outcome::Rows(rs)) => write_result_set(&mut wire, &rs, status(&session))?,
                    Err(e) => wire.write_err(&e)?,
                }
            }
            // COM_PING
            0x0e => wire.write_ok(0, 0, status(&session), "")?,
            // COM_STMT_CLOSE has no response.
            0x19 => continue,
            // COM_RESET_CONNECTION
            0x1f => {
                session.reset();
                wire.write_ok(0, 0, status(&session), "")?
            }
            other => wire.write_err(&MyError {
                code: 1047,
                sqlstate: "08S01",
                message: format!("unsupported command 0x{other:02x}"),
            })?,
        }
        wire.flush()?;
    }
}
