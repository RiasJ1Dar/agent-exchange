//! HTTP-шар переглядача: розбір рядка запиту, складання відповіді, цикл прийому.
//!
//! Написано на `std` без жодної залежності. Сервер слухає лише `127.0.0.1`,
//! обслуговує лише `GET` і завжди закриває з'єднання після відповіді
//! (`Connection: close`), тож ні keep-alive, ні chunked, ні тіла запиту тут
//! немає й не передбачається.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

/// Ліміт довжини рядка запиту, 8 КіБ.
///
/// Байти, а не символи: рахуємо те, що прийшло з мережі. Довший рядок ми
/// **не читаємо до кінця** — віддаємо `414` і закриваємо з'єднання, щоб
/// клієнт (чи хтось цікавий) не міг змусити процес складати рядок без межі.
pub const MAX_REQUEST_LINE: usize = 8 * 1024;

/// Скільки чекати на рядок запиту, перш ніж кинути з'єднання.
///
/// Без цього одне мовчазне з'єднання зупинило б увесь UI: цикл
/// однопотоковий, наступного клієнта ніхто б не прийняв.
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Content-Type, яким віддаються відповіді `handler`-а.
pub const HTML: &str = "text/html; charset=utf-8";
/// Content-Type для повідомлень про помилку.
pub const TEXT: &str = "text/plain; charset=utf-8";

/// Розібраний рядок запиту. Заголовків не зберігаємо — вони тут не потрібні.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// Завжди `GET`: інші методи не доходять до цієї структури.
    pub method: String,
    /// Шлях **після** percent-декодування, завжди починається з `/`.
    pub path: String,
    /// Частина після `?`, ще не декодована. `None`, якщо `?` не було.
    pub query: Option<String>,
    /// Наприклад `HTTP/1.1`.
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HttpError {
    /// Порожній рядок або самі пробіли.
    #[error("порожній рядок запиту")]
    EmptyRequestLine,
    /// Менше трьох частин: обрізаний запит або взагалі не HTTP.
    #[error("рядок запиту не схожий на HTTP: {line:?}")]
    Malformed { line: String },
    /// З'єднання закрилось, не діславши `\n`.
    #[error("з'єднання обірвалось до кінця рядка запиту")]
    Truncated,
    /// Рядок довший за [`MAX_REQUEST_LINE`].
    #[error("рядок запиту довший за {MAX_REQUEST_LINE} Б")]
    LineTooLong,
    /// Усе, крім `GET`.
    #[error("метод {method} не підтримується")]
    MethodNotAllowed { method: String },
    /// Шлях без початкового `/`, із сегментом `..`, зі зворотним слешем,
    /// із керівним символом або з битим percent-кодуванням.
    #[error("недопустимий шлях: {reason}")]
    BadPath { reason: String },
    /// `HTTP/`, але не 1.0 і не 1.1.
    #[error("версія {version} не підтримується")]
    UnsupportedVersion { version: String },
}

impl HttpError {
    /// Код відповіді, який слід віддати клієнтові.
    pub fn status(&self) -> u16 {
        match self {
            HttpError::MethodNotAllowed { .. } => 405,
            HttpError::LineTooLong => 414,
            HttpError::UnsupportedVersion { .. } => 505,
            HttpError::EmptyRequestLine
            | HttpError::Malformed { .. }
            | HttpError::Truncated
            | HttpError::BadPath { .. } => 400,
        }
    }
}

/// Розібрати рядок запиту: `GET /шлях HTTP/1.1`.
///
/// Кінцеві `\r\n` можна лишати — вони зрізаються. Жоден вхід не призводить
/// до паніки: усе, що не розібралось, повертається як [`HttpError`].
pub fn parse_request_line(line: &str) -> Result<Request, HttpError> {
    if line.len() > MAX_REQUEST_LINE {
        return Err(HttpError::LineTooLong);
    }
    let line = line.trim_end_matches('\n').trim_end_matches('\r');
    if line.trim().is_empty() {
        return Err(HttpError::EmptyRequestLine);
    }

    let mut parts = line.split(' ').filter(|p| !p.is_empty());
    let (method, target, version) = match (parts.next(), parts.next(), parts.next()) {
        (Some(m), Some(t), Some(v)) => (m, t, v),
        _ => {
            return Err(HttpError::Malformed {
                line: line.to_string(),
            })
        }
    };
    if parts.next().is_some() {
        return Err(HttpError::Malformed {
            line: line.to_string(),
        });
    }

    if method != "GET" {
        return Err(HttpError::MethodNotAllowed {
            method: method.to_string(),
        });
    }

    if !version.starts_with("HTTP/") {
        return Err(HttpError::Malformed {
            line: line.to_string(),
        });
    }
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(HttpError::UnsupportedVersion {
            version: version.to_string(),
        });
    }

    let (raw_path, query) = match target.split_once('?') {
        Some((p, q)) => (p, Some(q.to_string())),
        None => (target, None),
    };
    let path = check_path(raw_path)?;

    Ok(Request {
        method: method.to_string(),
        path,
        query,
        version: version.to_string(),
    })
}

/// Перевірити й декодувати шлях.
///
/// Перевірка йде **після** percent-декодування: інакше `/%2e%2e/` пройшло б
/// повз заборону `..`. Зворотний слеш заборонений окремо — на Windows це
/// теж роздільник тек.
fn check_path(raw: &str) -> Result<String, HttpError> {
    if !raw.starts_with('/') {
        return Err(HttpError::BadPath {
            reason: format!("не починається з '/': {raw:?}"),
        });
    }
    let path = percent_decode(raw)?;
    if path.contains('\\') {
        return Err(HttpError::BadPath {
            reason: format!("зворотний слеш: {path:?}"),
        });
    }
    if path.chars().any(|c| c.is_control()) {
        return Err(HttpError::BadPath {
            reason: "керівний символ у шляху".to_string(),
        });
    }
    if path.split('/').any(|seg| seg == "..") {
        return Err(HttpError::BadPath {
            reason: format!("сегмент '..': {path:?}"),
        });
    }
    Ok(path)
}

/// Percent-декодування. Результат мусить бути валідним UTF-8.
fn percent_decode(raw: &str) -> Result<String, HttpError> {
    let bytes = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3).ok_or_else(|| HttpError::BadPath {
                reason: format!("обірване percent-кодування: {raw:?}"),
            })?;
            let hi = hex_digit(hex[0]).ok_or_else(|| HttpError::BadPath {
                reason: format!("не-шістнадцяткова цифра після '%': {raw:?}"),
            })?;
            let lo = hex_digit(hex[1]).ok_or_else(|| HttpError::BadPath {
                reason: format!("не-шістнадцяткова цифра після '%': {raw:?}"),
            })?;
            out.push(hi * 16 + lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| HttpError::BadPath {
        reason: format!("шлях не UTF-8 після декодування: {raw:?}"),
    })
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Стандартне пояснення коду. Невідомі коди подаються як `Status`, аби
/// довільне число не валило відповідь.
pub fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        414 => "URI Too Long",
        500 => "Internal Server Error",
        505 => "HTTP Version Not Supported",
        _ => "Status",
    }
}

/// Скласти відповідь цілком, разом із тілом.
///
/// `Content-Length` — довжина тіла **в байтах** (`str::len()`), а не в
/// символах: для кирилиці це вдвічі різні числа, і саме тут найлегше
/// помилитись.
pub fn response(status: u16, content_type: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        reason = reason_phrase(status),
        len = body.len(),
    )
}

/// Прочитати рядок запиту, не даючи йому перевищити [`MAX_REQUEST_LINE`].
///
/// Читаємо не більше ніж ліміт плюс один байт: якщо на цьому місці ще немає
/// `\n`, рядок задовгий — далі не читаємо взагалі.
pub fn read_request_line<R: Read>(reader: &mut BufReader<R>) -> Result<String, HttpError> {
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    let mut limited = reader.take((MAX_REQUEST_LINE + 1) as u64);
    match limited.read_until(b'\n', &mut buf) {
        Ok(0) => return Err(HttpError::Truncated),
        Ok(_) => {}
        Err(_) => return Err(HttpError::Truncated),
    }
    if !buf.ends_with(b"\n") {
        // Або рядок довший за ліміт, або з'єднання обірвалось посеред нього.
        if buf.len() > MAX_REQUEST_LINE {
            return Err(HttpError::LineTooLong);
        }
        return Err(HttpError::Truncated);
    }
    String::from_utf8(buf).map_err(|_| HttpError::Malformed {
        line: "<не UTF-8>".to_string(),
    })
}

/// Обробити рівно одне з'єднання: прийняти, розібрати, відповісти, закрити.
///
/// Помилка розбору — це теж відповідь (`400`/`405`/`414`/`505`), а не збій.
/// `Err` повертається лише на збої самого сокета.
pub fn serve_one<F>(listener: &TcpListener, handler: &F) -> std::io::Result<()>
where
    F: Fn(&Request) -> (u16, String),
{
    let (stream, _peer) = listener.accept()?;
    handle_connection(stream, handler);
    Ok(())
}

fn handle_connection<F>(stream: TcpStream, handler: &F)
where
    F: Fn(&Request) -> (u16, String),
{
    // Таймаути не критичні: якщо ОС їх не прийняла, гірше не стане.
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(READ_TIMEOUT));

    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut out = stream;

    let text = match read_request_line(&mut reader) {
        Ok(t) => t,
        Err(err) => {
            let _ = write_error(&mut out, &err);
            return;
        }
    };

    let reply = match parse_request_line(&text) {
        Ok(req) => {
            let (status, body) = handler(&req);
            response(status, HTML, &body)
        }
        Err(err) => response(err.status(), TEXT, &err.to_string()),
    };
    let _ = out.write_all(reply.as_bytes());
    let _ = out.flush();
}

fn write_error(out: &mut TcpStream, err: &HttpError) -> std::io::Result<()> {
    let reply = response(err.status(), TEXT, &err.to_string());
    out.write_all(reply.as_bytes())?;
    out.flush()
}

/// Нескінченний цикл прийому.
///
/// Збій на одному з'єднанні (обірваний клієнт, сміття замість запиту,
/// відмова `accept`) цикл **не** завершує: UI має пережити будь-якого
/// клієнта. Повертається лише тоді, коли слухач сам віддав `None`.
pub fn serve<F>(listener: TcpListener, handler: F)
where
    F: Fn(&Request) -> (u16, String),
{
    for stream in listener.incoming() {
        match stream {
            Ok(s) => handle_connection(s, &handler),
            Err(_) => continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn valid_get_parses() {
        let req = parse_request_line("GET / HTTP/1.1\r\n").unwrap();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path, "/");
        assert_eq!(req.query, None);
        assert_eq!(req.version, "HTTP/1.1");
    }

    #[test]
    fn query_is_split_off_the_path() {
        let req = parse_request_line("GET /state?topic=ui HTTP/1.1").unwrap();
        assert_eq!(req.path, "/state");
        assert_eq!(req.query.as_deref(), Some("topic=ui"));
    }

    #[test]
    fn http_1_0_is_accepted() {
        assert_eq!(parse_request_line("GET / HTTP/1.0").unwrap().version, "HTTP/1.0");
    }

    #[test]
    fn other_versions_are_505() {
        let err = parse_request_line("GET / HTTP/2.0").unwrap_err();
        assert_eq!(err.status(), 505);
    }

    #[test]
    fn post_is_405() {
        let err = parse_request_line("POST / HTTP/1.1").unwrap_err();
        assert!(matches!(err, HttpError::MethodNotAllowed { .. }));
        assert_eq!(err.status(), 405);
    }

    #[test]
    fn head_and_delete_are_also_405() {
        for line in ["HEAD / HTTP/1.1", "DELETE /x HTTP/1.1", "PUT / HTTP/1.1"] {
            assert_eq!(parse_request_line(line).unwrap_err().status(), 405, "{line}");
        }
    }

    #[test]
    fn dot_dot_path_is_400() {
        let err = parse_request_line("GET /../secret HTTP/1.1").unwrap_err();
        assert!(matches!(err, HttpError::BadPath { .. }));
        assert_eq!(err.status(), 400);
    }

    #[test]
    fn dot_dot_in_the_middle_is_400() {
        assert_eq!(
            parse_request_line("GET /a/../../b HTTP/1.1").unwrap_err().status(),
            400
        );
    }

    #[test]
    fn percent_encoded_dot_dot_is_400() {
        // Найтихіший спосіб вийти з теки: перевірка мусить бути після декодування.
        assert_eq!(
            parse_request_line("GET /%2e%2e/etc HTTP/1.1").unwrap_err().status(),
            400
        );
    }

    #[test]
    fn backslash_path_is_400() {
        assert_eq!(
            parse_request_line("GET /a\\b HTTP/1.1").unwrap_err().status(),
            400
        );
    }

    #[test]
    fn broken_percent_encoding_is_400() {
        for line in ["GET /%zz HTTP/1.1", "GET /%4 HTTP/1.1", "GET /% HTTP/1.1"] {
            assert_eq!(parse_request_line(line).unwrap_err().status(), 400, "{line}");
        }
    }

    #[test]
    fn percent_encoded_cyrillic_path_decodes() {
        // /привіт у percent-кодуванні
        let req =
            parse_request_line("GET /%D0%BF%D1%80%D0%B8%D0%B2%D1%96%D1%82 HTTP/1.1").unwrap();
        assert_eq!(req.path, "/привіт");
    }

    #[test]
    fn path_without_leading_slash_is_400() {
        assert_eq!(
            parse_request_line("GET state HTTP/1.1").unwrap_err().status(),
            400
        );
    }

    #[test]
    fn over_limit_line_is_414_without_panic() {
        let line = format!("GET /{} HTTP/1.1\r\n", "a".repeat(MAX_REQUEST_LINE));
        assert!(line.len() > MAX_REQUEST_LINE);
        let err = parse_request_line(&line).unwrap_err();
        assert!(matches!(err, HttpError::LineTooLong));
        assert_eq!(err.status(), 414);
    }

    #[test]
    fn long_but_within_limit_line_parses() {
        let path_len = MAX_REQUEST_LINE - 32;
        let line = format!("GET /{} HTTP/1.1", "a".repeat(path_len));
        assert!(line.len() <= MAX_REQUEST_LINE);
        assert!(parse_request_line(&line).is_ok());
    }

    #[test]
    fn empty_input_is_an_error_not_a_panic() {
        for line in ["", "\r\n", "   ", "\n"] {
            let err = parse_request_line(line).unwrap_err();
            assert!(matches!(err, HttpError::EmptyRequestLine), "{line:?}");
            assert_eq!(err.status(), 400);
        }
    }

    #[test]
    fn truncated_input_is_an_error_not_a_panic() {
        for line in ["GET", "GET /", "GET /\r\n"] {
            let err = parse_request_line(line).unwrap_err();
            assert!(matches!(err, HttpError::Malformed { .. }), "{line:?}");
            assert_eq!(err.status(), 400);
        }
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        for line in ["мотлох", "\0\0\0", "GET / HTTP/1.1 extra", "{\"jsonrpc\":\"2.0\"}"] {
            assert!(parse_request_line(line).is_err(), "{line:?}");
        }
    }

    #[test]
    fn control_characters_in_path_are_400() {
        assert_eq!(
            parse_request_line("GET /%00 HTTP/1.1").unwrap_err().status(),
            400
        );
    }

    #[test]
    fn response_content_length_counts_bytes_not_chars() {
        let body = "Привіт";
        assert_eq!(body.chars().count(), 6);
        assert_eq!(body.len(), 12); // кирилиця — по два байти
        let reply = response(200, HTML, body);
        assert!(reply.contains("Content-Length: 12"), "{reply}");
        assert!(!reply.contains("Content-Length: 6"), "{reply}");
    }

    #[test]
    fn response_head_and_body_are_separated_by_a_blank_line() {
        let reply = response(200, HTML, "<h1>Обмін</h1>");
        let (head, body) = reply.split_once("\r\n\r\n").expect("немає порожнього рядка");
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(head.contains("Content-Type: text/html; charset=utf-8"));
        assert!(head.contains("Connection: close"));
        assert_eq!(body, "<h1>Обмін</h1>");
        // Тіло має рівно ту довжину, яку обіцяє заголовок.
        assert!(head.contains(&format!("Content-Length: {}", body.len())));
    }

    #[test]
    fn response_of_empty_body_is_zero_length() {
        let reply = response(404, TEXT, "");
        assert!(reply.contains("Content-Length: 0"));
        assert!(reply.ends_with("\r\n\r\n"));
    }

    #[test]
    fn unknown_status_still_builds_a_response() {
        assert!(response(599, TEXT, "x").starts_with("HTTP/1.1 599 Status\r\n"));
    }

    #[test]
    fn read_request_line_stops_at_newline() {
        let mut r = BufReader::new(Cursor::new(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n".to_vec()));
        assert_eq!(read_request_line(&mut r).unwrap(), "GET / HTTP/1.1\r\n");
    }

    #[test]
    fn read_request_line_rejects_an_endless_line() {
        let flood = vec![b'a'; MAX_REQUEST_LINE * 2];
        let mut r = BufReader::new(Cursor::new(flood));
        assert!(matches!(
            read_request_line(&mut r).unwrap_err(),
            HttpError::LineTooLong
        ));
    }

    #[test]
    fn read_request_line_on_closed_connection_is_truncated() {
        let mut r = BufReader::new(Cursor::new(Vec::new()));
        assert!(matches!(
            read_request_line(&mut r).unwrap_err(),
            HttpError::Truncated
        ));
        let mut r = BufReader::new(Cursor::new(b"GET / HTTP/1.1".to_vec()));
        assert!(matches!(
            read_request_line(&mut r).unwrap_err(),
            HttpError::Truncated
        ));
    }

    // --- живий сокет ---------------------------------------------------
    // Порт 0: вільний порт добирає ОС. Порт 9750 у тестах не чіпаємо ніколи —
    // на ньому може стояти справжній UI людини.

    use std::net::TcpStream;
    use std::thread;

    fn round_trip(request: &[u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind на 127.0.0.1:0");
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            serve_one(&listener, &|req: &Request| {
                (200, format!("шлях: {}", req.path))
            })
        });

        let mut client = TcpStream::connect(addr).expect("connect");
        client.write_all(request).unwrap();
        client.flush().unwrap();
        let mut got = String::new();
        client.read_to_string(&mut got).unwrap();
        server.join().unwrap().unwrap();
        got
    }

    #[test]
    fn serve_one_answers_a_real_get() {
        let got = round_trip(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
        assert!(got.starts_with("HTTP/1.1 200 OK\r\n"), "{got}");
        assert!(got.ends_with("шлях: /"), "{got}");
        // Кирилиця в тілі — довжина в БАЙТАХ, не в символах.
        // "шлях: /" = 7 символів, але 4 кириличні по 2 байти + ": /" = 11.
        assert!(got.contains("Content-Length: 11"), "{got}");
    }

    #[test]
    fn serve_one_answers_405_to_post() {
        let got = round_trip(b"POST / HTTP/1.1\r\n\r\n");
        assert!(got.starts_with("HTTP/1.1 405 Method Not Allowed\r\n"), "{got}");
    }

    #[test]
    fn serve_one_survives_garbage() {
        let got = round_trip(b"\x01\x02 not http at all\r\n");
        assert!(got.starts_with("HTTP/1.1 400 Bad Request\r\n"), "{got}");
    }
}
