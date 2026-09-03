use crate::rpc::rpc_err;
use crate::{Error, Mcp};
use serde_json::Value;
use std::io::{BufRead, Read, Write};

/// Стеля на один кадр — 1 МіБ. Довший кадр не читається в памʼять цілком:
/// хвіст відкидається, клієнт дістає -32700, потік лишається живим.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// UTF-8 BOM — деякі клієнти пишуть його перед першим повідомленням.
const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// Кадрування транспорту: LSP-заголовки або newline-delimited JSON (stdio MCP).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    /// `Content-Length: N\r\n\r\n<тіло>`
    ContentLength,
    /// `<компактний JSON>\n`
    Newline,
}

pub fn serve<R: BufRead, W: Write>(
    mcp: &Mcp,
    reader: &mut R,
    writer: &mut W,
) -> Result<(), Error> {
    loop {
        // Відповідаємо тим кадруванням, яким прийшов саме цей запит.
        let (bytes, out) = match read_frame(reader)? {
            Frame::Eof => return Ok(()),
            Frame::Bad(out, why) => {
                write_framed(
                    writer,
                    &rpc_err(&Value::Null, -32700, format!("Parse error: {why}")),
                    out,
                )?;
                continue;
            }
            Frame::Message(bytes, out) => (bytes, out),
        };
        let req: Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(e) => {
                write_framed(
                    writer,
                    &rpc_err(&Value::Null, -32700, format!("Parse error: {e}")),
                    out,
                )?;
                continue;
            }
        };
        if let Some(resp) = mcp.handle_rpc(&req) {
            write_framed(writer, &resp, out)?;
        }
    }
}

/// Один прочитаний кадр.
enum Frame {
    /// Тіло та кадрування, яким слід відповідати.
    Message(Vec<u8>, Framing),
    /// Кадр зіпсовано (завеликий або не-UTF-8 там, де мусить бути текст):
    /// віддаємо -32700 тим кадруванням і читаємо потік далі.
    Bad(Framing, String),
    /// Потік скінчився.
    Eof,
}

/// Один прочитаний рядок — у байтах, не в `String`: невалідний UTF-8 не має
/// права рвати сесію.
enum Line {
    /// Байти рядка без кінцевих `\r`/`\n`.
    Bytes(Vec<u8>),
    /// Рядок перевищив `MAX_FRAME_BYTES`; хвіст уже відкинуто до `\n`.
    TooLong,
    Eof,
}

/// Прочитати рядок до `\n` включно, але не більше за стелю кадру.
fn read_line_bytes<R: BufRead>(reader: &mut R) -> Result<Line, Error> {
    let mut buf = Vec::new();
    let n = {
        let mut limited = reader.by_ref().take(MAX_FRAME_BYTES as u64 + 1);
        limited.read_until(b'\n', &mut buf)?
    };
    if n == 0 {
        return Ok(Line::Eof);
    }
    if buf.len() > MAX_FRAME_BYTES && buf.last() != Some(&b'\n') {
        // Дочитати й викинути решту кадру, щоб наступне читання почалося
        // з межі рядка, а не з середини сміття.
        discard_until_newline(reader)?;
        return Ok(Line::TooLong);
    }
    while matches!(buf.last(), Some(b'\n') | Some(b'\r')) {
        buf.pop();
    }
    Ok(Line::Bytes(buf))
}

/// Викинути байти до найближчого `\n` включно, нічого не накопичуючи.
fn discard_until_newline<R: BufRead>(reader: &mut R) -> Result<(), Error> {
    loop {
        let (found, used) = {
            let buf = reader.fill_buf()?;
            if buf.is_empty() {
                return Ok(());
            }
            match buf.iter().position(|&b| b == b'\n') {
                Some(i) => (true, i + 1),
                None => (false, buf.len()),
            }
        };
        reader.consume(used);
        if found {
            return Ok(());
        }
    }
}

/// Викинути рівно `len` байт тіла, не алокуючи їх.
fn discard_exact<R: BufRead>(reader: &mut R, len: usize) -> Result<(), Error> {
    let mut sink = std::io::sink();
    std::io::copy(&mut reader.by_ref().take(len as u64), &mut sink)?;
    Ok(())
}

/// Пропустити порожні рядки та BOM, підглянути перший значущий байт,
/// не споживаючи його.
fn peek_first_byte<R: BufRead>(reader: &mut R) -> Result<Option<u8>, Error> {
    loop {
        let skip = {
            let buf = reader.fill_buf()?;
            if buf.is_empty() {
                return Ok(None);
            }
            if buf[0] == b'\r' || buf[0] == b'\n' {
                Some(1)
            } else if buf.starts_with(&BOM) {
                Some(BOM.len())
            } else {
                None
            }
        };
        match skip {
            Some(n) => reader.consume(n),
            None => {
                let buf = reader.fill_buf()?;
                return Ok(Some(buf[0]));
            }
        }
    }
}

/// Прочитати один кадр з автовизначенням кадрування.
fn read_frame<R: BufRead>(reader: &mut R) -> Result<Frame, Error> {
    let Some(first) = peek_first_byte(reader)? else {
        return Ok(Frame::Eof);
    };

    // JSON-RPC — це обʼєкт або batch-масив; і те, й те кадрується рядком.
    if first == b'{' || first == b'[' {
        return Ok(match read_line_bytes(reader)? {
            Line::Eof => Frame::Eof,
            Line::TooLong => Frame::Bad(
                Framing::Newline,
                format!("кадр довший за {MAX_FRAME_BYTES} байт"),
            ),
            Line::Bytes(body) => Frame::Message(body, Framing::Newline),
        });
    }

    let mut content_length: Option<usize> = None;
    let mut saw_header = false;
    loop {
        let raw = match read_line_bytes(reader)? {
            Line::Eof => {
                if saw_header {
                    return Err(Error::Io(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "неповні заголовки",
                    )));
                }
                return Ok(Frame::Eof);
            }
            Line::TooLong => {
                return Ok(Frame::Bad(
                    Framing::ContentLength,
                    format!("заголовок довший за {MAX_FRAME_BYTES} байт"),
                ));
            }
            Line::Bytes(raw) => raw,
        };
        saw_header = true;
        if raw.is_empty() {
            break;
        }
        let Ok(line) = std::str::from_utf8(&raw) else {
            return Ok(Frame::Bad(
                Framing::ContentLength,
                "заголовок не в UTF-8".to_string(),
            ));
        };
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("Content-Length") {
            let len: usize = value.trim().parse().map_err(|_| {
                Error::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("Content-Length: {}", value.trim()),
                ))
            })?;
            content_length = Some(len);
        }
    }
    let len = content_length.ok_or(Error::MissingContentLength)?;
    if len > MAX_FRAME_BYTES {
        discard_exact(reader, len)?;
        return Ok(Frame::Bad(
            Framing::ContentLength,
            format!("Content-Length {len} більший за {MAX_FRAME_BYTES}"),
        ));
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    Ok(Frame::Message(buf, Framing::ContentLength))
}

/// Прочитати одне повідомлення з автовизначенням кадрування.
/// Зіпсований кадр віддається порожнім тілом — його розбір дасть -32700.
pub fn read_framed<R: BufRead>(reader: &mut R) -> Result<Option<(Vec<u8>, Framing)>, Error> {
    Ok(match read_frame(reader)? {
        Frame::Eof => None,
        Frame::Bad(framing, _) => Some((Vec::new(), framing)),
        Frame::Message(body, framing) => Some((body, framing)),
    })
}

pub fn read_message<R: BufRead>(reader: &mut R) -> Result<Option<Vec<u8>>, Error> {
    Ok(read_framed(reader)?.map(|(body, _)| body))
}

/// Записати повідомлення заданим кадруванням. Тіло — завжди компактний JSON.
pub fn write_framed<W: Write>(w: &mut W, msg: &Value, framing: Framing) -> Result<(), Error> {
    let body = serde_json::to_vec(msg)?;
    match framing {
        Framing::ContentLength => {
            write!(w, "Content-Length: {}\r\n\r\n", body.len())?;
            w.write_all(&body)?;
        }
        Framing::Newline => {
            w.write_all(&body)?;
            w.write_all(b"\n")?;
        }
    }
    w.flush()?;
    Ok(())
}

pub fn write_message<W: Write>(w: &mut W, msg: &Value) -> Result<(), Error> {
    write_framed(w, msg, Framing::ContentLength)
}
