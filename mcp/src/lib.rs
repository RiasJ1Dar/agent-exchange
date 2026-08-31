//! stdio JSON-RPC MCP: два кадрування — `Content-Length` і newline-delimited
//! JSON. Кадрування визначається для кожного запиту окремо, відповідь іде
//! тим самим кадруванням. Прод-БД: agent-board/exchange.db.

use exchange_store::{Agent, Envelope, InboxQuery, Lock, Store};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

pub const PROD_DB: &str = r"C:\Users\Public\agent-board\exchange.db";
pub const PROD_NOW_MD: &str = r"C:\Users\Public\agent-board\NOW.md";

/// Перевизначення шляху до БД — щоб прогнати бінарник на копії, не чіпаючи
/// прод. Не виставлена — працює [`PROD_DB`].
pub const DB_ENV: &str = "EXCHANGE_DB";

/// Те саме для `NOW.md`: без цієї змінної лишається [`PROD_NOW_MD`].
pub const NOW_MD_ENV: &str = "EXCHANGE_NOW_MD";

const PROTOCOL_VERSION: &str = "2024-11-05";

/// Стеля на один кадр — 1 МіБ. Довший кадр не читається в памʼять цілком:
/// хвіст відкидається, клієнт дістає -32700, потік лишається живим.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// UTF-8 BOM — деякі клієнти пишуть його перед першим повідомленням.
const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

#[cfg(test)]
const TOOLS: &[&str] = &[
    "post",
    "inbox",
    "ack",
    "lock",
    "unlock",
    "render",
    "lock_status",
];

/// Запасне джерело особистості, коли `from`/`holder` не передані явно.
pub const AGENT_NAME_ENV: &str = "AGENT_NAME";

/// Значення, які приймає [`AGENT_NAME_ENV`]. `Both` не є особистістю:
/// це адреса розсилки, ним не підписуються і замків він не тримає.
const ENV_AGENT_NAMES: &[&str] = &["Grok", "Claude"];

/// Скільки символів тіла віддавати, коли агент розгрібає чергу
/// (`inbox` з `unread_only = true`) і сам стелі не назвав.
///
/// ⚠️ Стеля стоїть **саме тут, на межі MCP**, а не в `store`. Причина проста:
/// `render` кличе `store.inbox` напряму й малює з нього NOW.md для людини —
/// постав дефолт у `store`, і дошка мовчки почала б писати обрізані тіла,
/// виглядаючи при цьому повною. Обрізання потрібне рівно одному викликачеві —
/// агентові, що читає сорок непрочитаних, — тож тут воно і живе.
///
/// Явний `brief` перебиває дефолт; `brief = 0` або `null` — повні тіла.
pub const DEFAULT_BRIEF_CHARS: usize = 200;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] exchange_store::Error),
    #[error(transparent)]
    Render(#[from] exchange_render::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("немає Content-Length")]
    MissingContentLength,
    #[error("невірні параметри: {0}")]
    InvalidParams(String),
    /// Задана половина пари `EXCHANGE_DB` / `EXCHANGE_NOW_MD` — див. [`check_env_pair`].
    #[error("{0}")]
    EnvPaths(String),
}

pub struct Mcp {
    store: Store,
    now_md: PathBuf,
}

impl Mcp {
    pub fn open(db: &Path, now_md: &Path) -> Result<Self, Error> {
        Ok(Self {
            store: Store::open(db)?,
            now_md: now_md.to_path_buf(),
        })
    }

    pub fn prod() -> Result<Self, Error> {
        Self::open(Path::new(PROD_DB), Path::new(PROD_NOW_MD))
    }

    fn rerender(&self) -> Result<(), Error> {
        exchange_render::render(&self.store, &self.now_md)?;
        Ok(())
    }

    /// Перемалювати NOW.md **як побічний ефект**: збій не піднімається вгору,
    /// а повертається текстом.
    ///
    /// ⚠️ stdout — канал протоколу MCP, тому слід іде тільки в stderr.
    fn rerender_note(&self) -> Option<String> {
        match exchange_render::render(&self.store, &self.now_md) {
            Ok(()) => None,
            Err(e) => {
                let msg = e.to_string();
                eprintln!("exchange-mcp: render.failed detail={msg}");
                Some(msg)
            }
        }
    }

    /// Успішна відповідь на запис + перемальовування.
    ///
    /// ⚠️ Операція вже **закомічена**: `id` виданий, замок узятий, `read_at`
    /// проставлений. Якщо після цього впав `render` (людина зіпсувала маркери
    /// в NOW.md, io-помилка, delete-pending від сусіднього процесу), то це збій
    /// малювання, а не запису. Віддавати його замість результату — рівно те,
    /// що штовхає агента на повтор: `post` кладе в базу дублікат, `lock`
    /// лишається взятим при відповіді «не вийшло», а повторний `ack_many` дає
    /// `acked = 0`, не відрізнити від справжньої відмови. Тому збій рендера —
    /// окреме поле `render_error` в **успішній** відповіді, і NOW.md полагодять
    /// наступним `render`, коли маркери повернуть на місце.
    fn rendered(&self, value: Value) -> Value {
        let Some(msg) = self.rerender_note() else {
            return tool_ok(value);
        };
        match value {
            Value::Object(mut map) => {
                map.insert("render_error".to_string(), json!(msg));
                tool_ok(Value::Object(map))
            }
            // Усі корисні навантаження тут — обʼєкти; гілка лишається, щоб
            // «інше» не з'їло поле мовчки.
            other => tool_ok(json!({ "result": other, "render_error": msg })),
        }
    }

    pub fn handle_rpc(&self, req: &Value) -> Option<Value> {
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = req.get("id");
        if id.is_none() {
            return None;
        }
        let id = id.cloned().unwrap();

        let result: Result<Value, Error> = match method {
            "initialize" => Ok(initialize_result(req.get("params").unwrap_or(&Value::Null))),
            "ping" => Ok(json!({})),
            "shutdown" => Ok(Value::Null),
            "tools/list" => Ok(json!({ "tools": tool_defs() })),
            "tools/call" => Ok(self.tools_call(req.get("params").unwrap_or(&Value::Null))),
            _ => {
                return Some(rpc_err(&id, -32601, format!("метод не знайдено: {method}")));
            }
        };

        match result {
            Ok(value) => Some(rpc_ok(&id, value)),
            Err(e) => Some(rpc_err(&id, -32603, e.to_string())),
        }
    }

    fn tools_call(&self, params: &Value) -> Value {
        let name = match params.get("name").and_then(|n| n.as_str()) {
            Some(n) => n,
            None => {
                return tool_err("немає params.name");
            }
        };
        let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
        match self.call_tool(name, &args) {
            Ok(v) => v,
            Err(e) => tool_err(e),
        }
    }

    fn call_tool(&self, name: &str, args: &Value) -> Result<Value, Error> {
        // Особистість із середовища читаємо один раз на виклик; далі всюди
        // йде тим самим значенням, щоб один tools/call не бачив двох різних.
        let agent_env = env_agent_name();
        let agent_env = agent_env.as_deref();
        match name {
            "post" => {
                let env = envelope_from_args(args, agent_env)?;
                let id = self.store.post(env)?;
                Ok(self.rendered(json!({ "id": id })))
            }
            "inbox" => {
                let agent = json_field::<Agent>(args, "agent")?;
                let unread_only = args
                    .get("unread_only")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let query = InboxQuery {
                    agent,
                    unread_only,
                    limit: opt_usize(args, "limit")?,
                    brief: resolve_brief(args, unread_only)?,
                    topic: opt_string(args, "topic")?,
                    // `exclude_own` навмисне не виставляється з MCP: агент
                    // бачить власні розсилки `to = Both` у себе ж, і тихо
                    // ховати їх означало б міняти склад скриньки без прохання.
                    ..InboxQuery::default()
                };
                let messages = self.store.inbox_ex(query)?;
                Ok(tool_ok(json!({ "messages": messages })))
            }
            "ack" => self.ack_tool(args, agent_env),
            "lock" => {
                let topic = json_str(args, "topic")?;
                let holder = resolve_agent(args, "holder", agent_env)?;
                let ttl_sec = args.get("ttl_sec").and_then(|v| v.as_i64()).unwrap_or(0);
                let note = args
                    .get("note")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                // ⚠️ Саме `lock_ex`, а не `lock`: узяття йде однією
                // `BEGIN IMMEDIATE`-транзакцією (чотири процеси пишуть у ту
                // саму базу, а перевірка й запис у `lock` — два незалежні
                // autocommit-и), а перехоплення протермінованого замка
                // лишає слід: `evicted` у відповіді й сповіщення `Op::N`
                // колишньому тримачеві. Тихий варіант мовчки забирав тему,
                // і той, у кого її забрали, продовжував вважати її своєю.
                let outcome = self.store.lock_ex(topic, holder, ttl_sec, note)?;
                Ok(self.rendered(json!({
                    "ok": true,
                    "topic": topic,
                    "holder": holder,
                    "evicted": outcome.evicted,
                    "notified_id": outcome.notified_id
                })))
            }
            "unlock" => {
                let topic = json_str(args, "topic")?;
                let holder = resolve_agent(args, "holder", agent_env)?;
                self.store.unlock(topic, holder)?;
                Ok(self.rendered(json!({
                    "ok": true,
                    "topic": topic,
                    "holder": holder
                })))
            }
            "render" => {
                self.rerender()?;
                Ok(tool_ok(json!({ "ok": true })))
            }
            "lock_status" => {
                // Тільки читання: NOW.md не чіпаємо.
                let locks = self.store.locks()?;
                Ok(tool_ok(locks_payload(&locks, now_unix())?))
            }
            other => Ok(tool_err(format!("невідомий tool: {other}"))),
        }
    }

    /// `ack` двома шляхами: один `id` або пачка `ids`.
    ///
    /// Розвʼязуються вони **до** будь-якої роботи зі сховищем і за наявністю
    /// поля, а не за його значенням: обидва разом — помилка (мовчки віддати
    /// перевагу одному означало б тихо проігнорувати половину прохання),
    /// жодного — теж помилка. Далі шляхи не змішуються:
    ///
    /// * `id` іде через [`Store::ack_one`] і **потребує особистості**
    ///   так само, як пачка. ⚠️ Раніше тут стояв [`Store::ack`] — позначення
    ///   за самим лише `id`, без адресата: будь-хто гасив чуже непрочитане,
    ///   і повернути це не було чим (`read_at = NULL` в API немає, а чужі
    ///   `id` відкрито лежать в `agent_talk.md`). Тепер позначається лише
    ///   своє й лише непрочитане, а `acked = false` (чуже, неіснуюче, вже
    ///   прочитане) — відповідь, а не збій: розрізняти ці три випадки
    ///   ззовні означало б зробити `ack` оракулом чужих id;
    /// * `ids` іде через [`Store::ack_many`] і **потребує особистості**:
    ///   пачка позначає лише те, що адресоване цьому агентові. Чужі,
    ///   неіснуючі та вже прочитані просто не рахуються — це відповідь,
    ///   а не збій, тому у відповіді видно і `acked`, і `requested`.
    /// Особистість для `ack` — один ланцюг на обидва шляхи: явний `agent`
    /// → `AGENT_NAME` → помилка. `Both` не приймається жодним із них.
    ///
    /// Спільна функція, а не два однакові шматки: розійшовшись, вони й дали б
    /// ту саму дірку, що була в `id` — один шлях питає, чиє це, другий ні.
    fn ack_agent(&self, args: &Value, agent_env: Option<&str>) -> Result<Agent, Error> {
        let agent = resolve_agent(args, "agent", agent_env)?;
        if matches!(agent, Agent::Both) {
            return Err(Error::InvalidParams(
                "agent=Both не підтверджує читання: Both — адреса розсилки, \
                 а не особистість; передайте Grok або Claude"
                    .into(),
            ));
        }
        Ok(agent)
    }

    fn ack_tool(&self, args: &Value, agent_env: Option<&str>) -> Result<Value, Error> {
        let has_id = has_value(args, "id");
        let has_ids = has_value(args, "ids");
        match (has_id, has_ids) {
            (true, true) => Err(Error::InvalidParams(
                "разом `id` і `ids` не приймаються: або один id, або пачка ids"
                    .into(),
            )),
            (false, false) => Err(Error::InvalidParams(
                "немає ні `id`, ні `ids`: передайте один id або масив ids".into(),
            )),
            (true, false) => {
                let id = json_i64(args, "id")?;
                let agent = self.ack_agent(args, agent_env)?;
                let acked = self.store.ack_one(id, agent)?;
                Ok(self.rendered(json!({
                    "ok": true,
                    "id": id,
                    "agent": agent,
                    "acked": acked
                })))
            }
            (false, true) => {
                let ids = json_i64_array(args, "ids")?;
                let agent = self.ack_agent(args, agent_env)?;
                let acked = self.store.ack_many(&ids, agent)?;
                Ok(self.rendered(json!({
                    "ok": true,
                    "agent": agent,
                    "acked": acked,
                    "requested": ids.len(),
                    "ids": ids
                })))
            }
        }
    }
}

/// Куди писати: значення `EXCHANGE_DB` / `EXCHANGE_NOW_MD`, а без них —
/// прод-константи. Приймає **значення** змінних, а не читає env сама, тож
/// перевіряється тестами без `std::env::set_var`.
///
/// Порожній рядок — це «не задано», а не «шлях завдовжки нуль»: випадково
/// порожня змінна інакше тихо відправила б сервер писати повз прод. З тієї ж
/// причини значення з самих пробілів теж не рахується заданим.
pub fn resolve_paths(db_env: Option<&str>, now_env: Option<&str>) -> (PathBuf, PathBuf) {
    fn pick(value: Option<&str>, fallback: &str) -> PathBuf {
        match value.map(str::trim).filter(|v| !v.is_empty()) {
            Some(v) => PathBuf::from(v),
            None => PathBuf::from(fallback),
        }
    }
    (pick(db_env, PROD_DB), pick(now_env, PROD_NOW_MD))
}

/// Чи змінна справді щось задає. Порожня і з самих пробілів — «не задано»
/// (те саме правило, що в [`resolve_paths`]).
fn env_is_set(value: Option<&str>) -> bool {
    matches!(value.map(str::trim), Some(v) if !v.is_empty())
}

/// `EXCHANGE_DB` і `EXCHANGE_NOW_MD` працюють **тільки парою**.
///
/// ⚠️ Половина пари — це втрата даних, а не незручність. Задана сама лише
/// `EXCHANGE_DB` (рівно та «безпечна» процедура, яку рекомендував доккоментар:
/// «прогнати бінарник на копії») означає читання з копії, але запис у **прод**:
/// `render` мерджить блоки в прод-`NOW.md` людини і переписує з нуля
/// прод-`agent_talk.md` — обидва зі складом повідомлень із копії, тобто
/// затирає живу дошку. Дзеркальний випадок — сама лише `EXCHANGE_NOW_MD` —
/// малює прод-обмін у сторонній файл, а прод-дошка тихо перестає оновлюватись.
///
/// Тому відмова старту, а не мовчазний фолбек: обидві або жодної.
pub fn check_env_pair(db_env: Option<&str>, now_env: Option<&str>) -> Result<(), Error> {
    match (env_is_set(db_env), env_is_set(now_env)) {
        (true, false) => Err(Error::EnvPaths(format!(
            "{DB_ENV} задано, а {NOW_MD_ENV} — ні: сервер читав би базу з копії, \
             а писав би в прод {PROD_NOW_MD} (і переписував прод-agent_talk.md \
             складом із копії). Виставте обидві змінні або жодної — наприклад \
             {NOW_MD_ENV} з NOW.md поруч із тією самою базою."
        ))),
        (false, true) => Err(Error::EnvPaths(format!(
            "{NOW_MD_ENV} задано, а {DB_ENV} — ні: сервер малював би прод-базу \
             {PROD_DB} у сторонній файл, а прод-NOW.md перестав би оновлюватись. \
             Виставте обидві змінні або жодної."
        ))),
        _ => Ok(()),
    }
}

/// [`resolve_paths`] з перевіркою пари — вхід для всього, що **пише**.
///
/// Сам `resolve_paths` лишається без перевірки навмисно: ним користується
/// вікно перегляду, яке лише читає, і воно має показувати те саме, що бачить
/// сервер, навіть коли той відмовляється стартувати.
pub fn resolve_paths_checked(
    db_env: Option<&str>,
    now_env: Option<&str>,
) -> Result<(PathBuf, PathBuf), Error> {
    check_env_pair(db_env, now_env)?;
    Ok(resolve_paths(db_env, now_env))
}

/// Прочитати env-перевизначення шляхів. Тонка обгортка над
/// [`resolve_paths_checked`]: сам вибір лишається чистим і тестованим.
fn paths_from_env() -> Result<(PathBuf, PathBuf), Error> {
    let db = std::env::var(DB_ENV).ok();
    let now = std::env::var(NOW_MD_ENV).ok();
    resolve_paths_checked(db.as_deref(), now.as_deref())
}

pub fn run_stdio() -> Result<(), Error> {
    let (db, now_md) = paths_from_env()?;
    let mcp = Mcp::open(&db, &now_md)?;
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut stdout = std::io::stdout();
    serve(&mcp, &mut reader, &mut stdout)
}

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

fn envelope_from_args(args: &Value, agent_env: Option<&str>) -> Result<Envelope, Error> {
    let mut obj = if let Some(env) = args.get("envelope") {
        env.clone()
    } else {
        args.clone()
    };
    if obj.is_object() {
        // Явний `from` виграє; його ж і кладемо назад, тож наявні виклики
        // проходять байт-у-байт як раніше.
        let from = resolve_agent(&obj, "from", agent_env)?;
        let from = serde_json::to_value(from)?;
        let map = obj.as_object_mut().expect("щойно перевірено is_object");
        map.entry("v").or_insert(json!(1));
        map.insert("from".to_string(), from);
    }
    serde_json::from_value(obj).map_err(|e| Error::InvalidParams(e.to_string()))
}

/// Прочитати `AGENT_NAME`. Порожнє значення — те саме, що не виставлене.
fn env_agent_name() -> Option<String> {
    std::env::var(AGENT_NAME_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
}

/// Розпізнати значення `AGENT_NAME` без урахування регістру.
fn agent_from_env(raw: &str) -> Result<Agent, Error> {
    let name = raw.trim();
    for allowed in ENV_AGENT_NAMES {
        if name.eq_ignore_ascii_case(allowed) {
            return serde_json::from_value(json!(allowed))
                .map_err(|e| Error::InvalidParams(e.to_string()));
        }
    }
    Err(Error::InvalidParams(format!(
        "{AGENT_NAME_ENV}=«{name}» не розпізнано; дозволені значення: {}",
        ENV_AGENT_NAMES.join(", ")
    )))
}

/// Особистість для `from`/`holder`: явний аргумент виграє; якщо його немає —
/// `AGENT_NAME`; якщо немає й того — помилка, а не мовчазний дефолт.
fn resolve_agent(args: &Value, field: &str, agent_env: Option<&str>) -> Result<Agent, Error> {
    if args.get(field).is_some() {
        return json_field::<Agent>(args, field);
    }
    match agent_env {
        Some(raw) => agent_from_env(raw),
        None => Err(Error::InvalidParams(format!(
            "немає {field}: передайте його явно або виставте змінну середовища \
             {AGENT_NAME_ENV} ({})",
            ENV_AGENT_NAMES.join(" або ")
        ))),
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Замки + похідні поля: коли спливає TTL і чи вже сплив.
///
/// Увага: `Store::locks` сама вимітає протерміновані рядки, тож крізь
/// прод-шлях `expired` практично завжди `false`. Позначка лишається
/// чесною для будь-якого замка, що дожив до відповіді.
fn locks_payload(locks: &[Lock], now: i64) -> Result<Value, Error> {
    let mut out = Vec::with_capacity(locks.len());
    for lock in locks {
        let mut v = serde_json::to_value(lock)?;
        let expires_at = lock.taken_at.saturating_add(lock.ttl_sec);
        let map = v
            .as_object_mut()
            .ok_or_else(|| Error::InvalidParams("Lock серіалізувався не в обʼєкт".into()))?;
        map.insert("expires_at".to_string(), json!(expires_at));
        map.insert("expired".to_string(), json!(expires_at <= now));
        out.push(Value::Object(map.clone()));
    }
    Ok(json!({ "now": now, "count": out.len(), "locks": out }))
}

fn json_field<T: serde::de::DeserializeOwned>(args: &Value, field: &str) -> Result<T, Error> {
    let v = args
        .get(field)
        .ok_or_else(|| Error::InvalidParams(format!("немає {field}")))?;
    serde_json::from_value(v.clone()).map_err(|e| Error::InvalidParams(format!("{field}: {e}")))
}

fn json_str<'a>(args: &'a Value, field: &str) -> Result<&'a str, Error> {
    args.get(field)
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::InvalidParams(format!("немає {field}")))
}

fn json_i64(args: &Value, field: &str) -> Result<i64, Error> {
    let v = args
        .get(field)
        .ok_or_else(|| Error::InvalidParams(format!("немає {field}")))?;
    v.as_i64()
        .ok_or_else(|| Error::InvalidParams(format!("{field} має бути цілим")))
}

/// Чи поле справді щось несе. `null` — те саме, що відсутнє: клієнти
/// охоче підставляють його замість пропуску, і рахувати таке за «поле задано»
/// означало б падати на порожньому місці.
fn has_value(args: &Value, field: &str) -> bool {
    matches!(args.get(field), Some(v) if !v.is_null())
}

fn json_i64_array(args: &Value, field: &str) -> Result<Vec<i64>, Error> {
    let arr = args
        .get(field)
        .and_then(|v| v.as_array())
        .ok_or_else(|| Error::InvalidParams(format!("{field} має бути масивом цілих")))?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, v) in arr.iter().enumerate() {
        out.push(
            v.as_i64()
                .ok_or_else(|| Error::InvalidParams(format!("{field}[{i}] має бути цілим")))?,
        );
    }
    Ok(out)
}

/// Необовʼязкове невідʼємне число. Відсутнє або `null` — `None`;
/// відʼємне — помилка, а не мовчазний нуль.
fn opt_usize(args: &Value, field: &str) -> Result<Option<usize>, Error> {
    match args.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let n = v
                .as_i64()
                .ok_or_else(|| Error::InvalidParams(format!("{field} має бути цілим")))?;
            if n < 0 {
                return Err(Error::InvalidParams(format!(
                    "{field} не може бути відʼємним, отримано {n}"
                )));
            }
            Ok(Some(n as usize))
        }
    }
}

fn opt_string(args: &Value, field: &str) -> Result<Option<String>, Error> {
    match args.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_str()
            .map(|s| Some(s.to_string()))
            .ok_or_else(|| Error::InvalidParams(format!("{field} має бути рядком"))),
    }
}

/// Стеля тіла для `inbox` — див. [`DEFAULT_BRIEF_CHARS`].
///
/// Поле відсутнє — дефолт вмикається **тільки** при `unread_only`: агент
/// розгрібає чергу і йому потрібні заголовки, а не сорок повних тіл.
/// Явний `brief` завжди виграє, `brief = 0` і `brief = null` означають
/// «повне тіло» (у `store` `Some(0)` різало б усе до самої позначки —
/// відповіді з одних «…» ніхто не просив).
fn resolve_brief(args: &Value, unread_only: bool) -> Result<Option<usize>, Error> {
    match args.get("brief") {
        None => Ok(unread_only.then_some(DEFAULT_BRIEF_CHARS)),
        Some(Value::Null) => Ok(None),
        Some(_) => Ok(opt_usize(args, "brief")?.filter(|n| *n > 0)),
    }
}

fn tool_ok(value: Value) -> Value {
    json!({
        "content": [{ "type": "text", "text": value.to_string() }],
        "isError": false
    })
}

fn tool_err(msg: impl std::fmt::Display) -> Value {
    json!({
        "content": [{ "type": "text", "text": msg.to_string() }],
        "isError": true
    })
}

fn rpc_ok(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_err(id: &Value, code: i64, message: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

fn initialize_result(params: &Value) -> Value {
    let pv = params
        .get("protocolVersion")
        .and_then(|v| v.as_str())
        .unwrap_or(PROTOCOL_VERSION);
    json!({
        "protocolVersion": pv,
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "exchange-mcp",
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

fn tool_defs() -> Value {
    json!([
        {
            "name": "post",
            "description": "Покласти Envelope у store і перемалювати NOW.md; без `from` підпис береться зі змінної середовища AGENT_NAME",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "from": { "type": "string", "enum": ["Grok", "Claude", "Both"] },
                    "to": { "type": "string", "enum": ["Grok", "Claude", "Both"] },
                    "topic": { "type": "string" },
                    "op": { "type": "string", "enum": ["Q", "A", "N", "L"] },
                    "body": { "type": "object" },
                    "v": { "type": "integer" },
                    "envelope": { "type": "object" }
                }
            }
        },
        {
            "name": "inbox",
            "description": "Прочитати inbox агента: unread_only (за замовчуванням false), limit (стільки найновіших), topic (фільтр за темою), brief (стеля символів тіла; при unread_only за замовчуванням 200, brief=0 — повні тіла)",
            "inputSchema": {
                "type": "object",
                "required": ["agent"],
                "properties": {
                    "agent": { "type": "string", "enum": ["Grok", "Claude", "Both"] },
                    "unread_only": { "type": "boolean" },
                    "limit": { "type": "integer", "minimum": 0 },
                    "brief": { "type": "integer", "minimum": 0 },
                    "topic": { "type": "string" }
                }
            }
        },
        {
            "name": "ack",
            "description": "Позначити прочитаним і перемалювати NOW.md: або один `id`, або пачка `ids` — обидва шляхи від імені `agent` (без `agent` він береться зі змінної середовища AGENT_NAME) і позначають лише адресоване йому; у відповіді acked (на пачку — проти requested): чуже, неіснуюче та вже прочитане не рахується й помилкою не є",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "integer" },
                    "ids": { "type": "array", "items": { "type": "integer" } },
                    "agent": { "type": "string", "enum": ["Grok", "Claude"] }
                }
            }
        },
        {
            "name": "lock",
            "description": "Взяти замок на тему і перемалювати NOW.md; без `holder` власник береться зі змінної середовища AGENT_NAME; перехоплення протермінованого чужого замка видно в полі evicted, а колишньому тримачеві лягає сповіщення (notified_id)",
            "inputSchema": {
                "type": "object",
                "required": ["topic"],
                "properties": {
                    "topic": { "type": "string" },
                    "holder": { "type": "string", "enum": ["Grok", "Claude"] },
                    "ttl_sec": { "type": "integer" },
                    "note": { "type": "string" }
                }
            }
        },
        {
            "name": "unlock",
            "description": "Зняти свій замок і перемалювати NOW.md; без `holder` власник береться зі змінної середовища AGENT_NAME",
            "inputSchema": {
                "type": "object",
                "required": ["topic"],
                "properties": {
                    "topic": { "type": "string" },
                    "holder": { "type": "string", "enum": ["Grok", "Claude"] }
                }
            }
        },
        {
            "name": "render",
            "description": "Перемалювати NOW.md зі store",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "lock_status",
            "description": "Показати всі замки з полем expired (taken_at + ttl_sec проти поточного часу)",
            "inputSchema": { "type": "object", "properties": {} }
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::io::Cursor;
    use tempfile::tempdir;

    fn tmp_mcp() -> (tempfile::TempDir, Mcp) {
        let dir = tempdir().unwrap();
        let db = dir.path().join("exchange.db");
        let now = dir.path().join("NOW.md");
        let mcp = Mcp::open(&db, &now).unwrap();
        (dir, mcp)
    }

    fn call(mcp: &Mcp, id: i64, name: &str, arguments: Value) -> Value {
        mcp.handle_rpc(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        }))
        .unwrap()
    }

    fn tool_text(resp: &Value) -> String {
        resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn tool_json(resp: &Value) -> Value {
        serde_json::from_str(&tool_text(resp)).unwrap()
    }

    fn is_error(resp: &Value) -> bool {
        resp["result"]["isError"].as_bool().unwrap_or(false)
    }

    #[test]
    fn content_length_roundtrip_crlf_and_lf() {
        let msg = json!({"jsonrpc":"2.0","id":1,"method":"ping"});
        let body = serde_json::to_vec(&msg).unwrap();

        let mut crlf = Vec::new();
        write_message(&mut crlf, &msg).unwrap();
        assert!(crlf.starts_with(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes()));
        let got = read_message(&mut Cursor::new(crlf)).unwrap().unwrap();
        assert_eq!(got, body);

        let mut lf = Vec::new();
        write!(&mut lf, "Content-Length: {}\n\n", body.len()).unwrap();
        lf.extend_from_slice(&body);
        let got = read_message(&mut Cursor::new(lf)).unwrap().unwrap();
        assert_eq!(got, body);
    }

    #[test]
    fn initialize_and_tools_list() {
        let (_dir, mcp) = tmp_mcp();
        let init = mcp
            .handle_rpc(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "test", "version": "0" }
                }
            }))
            .unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], "exchange-mcp");
        assert_eq!(init["result"]["protocolVersion"], "2024-11-05");
        assert!(init["result"]["capabilities"]["tools"].is_object());

        let list = mcp
            .handle_rpc(&json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list"
            }))
            .unwrap();
        let names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        for t in TOOLS {
            assert!(names.contains(t), "немає {t} у {names:?}");
        }
        assert_eq!(names.len(), TOOLS.len());
    }

    #[test]
    fn notification_has_no_response() {
        let (_dir, mcp) = tmp_mcp();
        assert!(mcp
            .handle_rpc(&json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            }))
            .is_none());
    }

    #[test]
    fn post_inbox_ack_and_render_now_md() {
        let (dir, mcp) = tmp_mcp();
        let now = dir.path().join("NOW.md");

        let posted = call(
            &mcp,
            1,
            "post",
            json!({
                "from": "Grok",
                "to": "Claude",
                "topic": "xvid/core",
                "op": "Q",
                "body": { "q": "ping" }
            }),
        );
        assert!(!is_error(&posted), "{posted}");
        let id = tool_json(&posted)["id"].as_i64().unwrap();
        assert!(id > 0);

        let now_text = fs::read_to_string(&now).unwrap();
        assert!(now_text.starts_with("# Зараз\n"));
        assert!(now_text.contains(&format!("#{id}")));
        assert!(now_text.contains("Q"));

        let inbox = call(
            &mcp,
            2,
            "inbox",
            json!({ "agent": "Claude", "unread_only": true }),
        );
        let msgs = tool_json(&inbox)["messages"].as_array().unwrap().clone();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["id"], id);
        assert!(msgs[0]["read_at"].is_null());

        // `id` теж підтверджується від імені агента: адресат — Claude, він і
        // гасить. Без особистості `ack` більше не працює взагалі.
        let acked = call(&mcp, 3, "ack", json!({ "id": id, "agent": "Claude" }));
        assert_eq!(tool_json(&acked)["ok"], true);
        assert_eq!(tool_json(&acked)["acked"], true);

        let unread = call(
            &mcp,
            4,
            "inbox",
            json!({ "agent": "Claude", "unread_only": true }),
        );
        assert!(tool_json(&unread)["messages"]
            .as_array()
            .unwrap()
            .is_empty());

        let now_text = fs::read_to_string(&now).unwrap();
        assert!(now_text.contains(&format!("#{id}")));
    }

    #[test]
    fn lock_unlock_and_conflict() {
        let (dir, mcp) = tmp_mcp();
        let now = dir.path().join("NOW.md");

        let locked = call(
            &mcp,
            1,
            "lock",
            json!({
                "topic": "xvid/core",
                "holder": "Grok",
                "ttl_sec": 90,
                "note": "працюю"
            }),
        );
        assert!(!is_error(&locked), "{locked}");
        let now_text = fs::read_to_string(&now).unwrap();
        assert!(now_text.contains("xvid/core"));
        assert!(now_text.contains("ttl=90"));

        let conflict = call(
            &mcp,
            2,
            "lock",
            json!({
                "topic": "xvid/core",
                "holder": "Claude",
                "ttl_sec": 30,
                "note": "ні"
            }),
        );
        assert!(is_error(&conflict));
        assert!(tool_text(&conflict).contains("зайнята"));

        let unlocked = call(
            &mcp,
            3,
            "unlock",
            json!({ "topic": "xvid/core", "holder": "Grok" }),
        );
        assert!(!is_error(&unlocked), "{unlocked}");
        let now_text = fs::read_to_string(&now).unwrap();
        assert!(now_text.contains("- (немає)"));
    }

    #[test]
    fn render_tool_writes_now_md() {
        let (dir, mcp) = tmp_mcp();
        let now = dir.path().join("NOW.md");
        mcp.store
            .post(Envelope {
                v: 1,
                from: Agent::Claude,
                to: Agent::Grok,
                topic: "beta".into(),
                op: exchange_store::Op::N,
                body: json!({"n": "note"}),
            })
            .unwrap();

        let resp = call(&mcp, 1, "render", json!({}));
        assert_eq!(tool_json(&resp)["ok"], true);
        let text = fs::read_to_string(&now).unwrap();
        assert!(text.contains("beta"));
        assert!(text.contains("## Inbox"));
    }

    #[test]
    fn unknown_tool_is_error() {
        let (_dir, mcp) = tmp_mcp();
        let resp = call(&mcp, 1, "explode", json!({}));
        assert!(is_error(&resp));
        assert!(tool_text(&resp).contains("невідомий tool"));
    }

    #[test]
    fn serve_loop_content_length() {
        let (_dir, mcp) = tmp_mcp();
        let req = json!({"jsonrpc":"2.0","id":7,"method":"tools/list"});
        let mut input = Vec::new();
        write_message(&mut input, &req).unwrap();
        let mut output = Vec::new();
        serve(&mcp, &mut Cursor::new(input), &mut output).unwrap();

        let body = read_message(&mut Cursor::new(output)).unwrap().unwrap();
        let resp: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(resp["id"], 7);
        let names: Vec<&str> = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, TOOLS);
    }

    #[test]
    fn missing_ack_id_is_error() {
        let (_dir, mcp) = tmp_mcp();
        let resp = call(&mcp, 1, "ack", json!({}));
        assert!(is_error(&resp));
    }

    fn serve_newline(mcp: &Mcp, input: &str) -> String {
        let mut output = Vec::new();
        serve(mcp, &mut Cursor::new(input.as_bytes().to_vec()), &mut output).unwrap();
        String::from_utf8(output).unwrap()
    }

    #[test]
    fn newline_framed_initialize_gets_response() {
        let (_dir, mcp) = tmp_mcp();
        let out = serve_newline(
            &mcp,
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
        );

        assert!(
            !out.contains("Content-Length"),
            "у newline-режимі не має бути заголовка: {out:?}"
        );
        assert!(out.ends_with('\n'), "{out:?}");
        let lines: Vec<&str> = out.trim_end_matches('\n').split('\n').collect();
        assert_eq!(lines.len(), 1, "має бути рівно один рядок: {out:?}");

        let resp: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["serverInfo"]["name"], "exchange-mcp");
    }

    #[test]
    fn newline_framed_tools_list() {
        let (_dir, mcp) = tmp_mcp();
        let out = serve_newline(
            &mcp,
            concat!(
                "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
                "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
                "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n",
            ),
        );

        assert!(!out.contains("Content-Length"), "{out:?}");
        let lines: Vec<&str> = out.trim_end_matches('\n').split('\n').collect();
        assert_eq!(lines.len(), 2, "initialize + tools/list: {out:?}");

        let list: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(list["id"], 2);
        let names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, TOOLS);
    }

    #[test]
    fn newline_notification_has_no_response() {
        let (_dir, mcp) = tmp_mcp();
        let out = serve_newline(
            &mcp,
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
        );
        assert!(out.is_empty(), "нотифікація не має відповіді: {out:?}");
    }

    #[test]
    fn newline_body_is_compact_single_line() {
        let (_dir, mcp) = tmp_mcp();
        let out = serve_newline(&mcp, "{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"tools/list\"}\n");
        // tools/list — велика відповідь, у ній не має бути жодного внутрішнього \n
        assert_eq!(out.matches('\n').count(), 1, "{out:?}");
        let resp: Value = serde_json::from_str(out.trim_end_matches('\n')).unwrap();
        assert_eq!(resp["id"], 9);
    }

    #[test]
    fn content_length_still_works() {
        let (_dir, mcp) = tmp_mcp();
        let req = json!({"jsonrpc":"2.0","id":5,"method":"initialize","params":{}});
        let mut input = Vec::new();
        write_message(&mut input, &req).unwrap();

        let mut output = Vec::new();
        serve(&mcp, &mut Cursor::new(input), &mut output).unwrap();

        let text = String::from_utf8(output.clone()).unwrap();
        assert!(text.starts_with("Content-Length: "), "{text:?}");

        let (body, framing) = read_framed(&mut Cursor::new(output)).unwrap().unwrap();
        assert_eq!(framing, Framing::ContentLength);
        let resp: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(resp["id"], 5);
        assert_eq!(resp["result"]["serverInfo"]["name"], "exchange-mcp");
    }

    #[test]
    fn mixed_framing_each_reply_matches_its_request() {
        let (_dir, mcp) = tmp_mcp();

        // 1) Content-Length, 2) newline, 3) Content-Length — в одному потоці.
        let mut input = Vec::new();
        write_message(
            &mut input,
            &json!({"jsonrpc":"2.0","id":1,"method":"ping"}),
        )
        .unwrap();
        input.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n");
        write_message(
            &mut input,
            &json!({"jsonrpc":"2.0","id":3,"method":"ping"}),
        )
        .unwrap();

        let mut output = Vec::new();
        serve(&mcp, &mut Cursor::new(input), &mut output).unwrap();

        let mut cur = Cursor::new(output.clone());

        let (body, framing) = read_framed(&mut cur).unwrap().unwrap();
        assert_eq!(framing, Framing::ContentLength, "перша відповідь");
        let first: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(first["id"], 1);

        let (body, framing) = read_framed(&mut cur).unwrap().unwrap();
        assert_eq!(framing, Framing::Newline, "друга відповідь");
        let second: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(second["id"], 2);

        let (body, framing) = read_framed(&mut cur).unwrap().unwrap();
        assert_eq!(framing, Framing::ContentLength, "третя відповідь");
        let third: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(third["id"], 3);

        assert!(read_framed(&mut cur).unwrap().is_none(), "більше нічого");

        // Байт у байт: заголовок лише навколо 1-ї та 3-ї, друга — голий рядок.
        let mut expected = Vec::new();
        write_framed(&mut expected, &first, Framing::ContentLength).unwrap();
        write_framed(&mut expected, &second, Framing::Newline).unwrap();
        write_framed(&mut expected, &third, Framing::ContentLength).unwrap();

        assert_eq!(
            String::from_utf8(output).unwrap(),
            String::from_utf8(expected).unwrap()
        );
    }

    #[test]
    fn blank_lines_between_newline_messages_are_skipped() {
        let (_dir, mcp) = tmp_mcp();
        let out = serve_newline(
            &mcp,
            "\n\n{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"ping\"}\n\n\n",
        );
        let lines: Vec<&str> = out.trim_end_matches('\n').split('\n').collect();
        assert_eq!(lines.len(), 1, "{out:?}");
        let resp: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(resp["id"], 3);
        assert!(resp["result"].is_object());
    }

    fn serve_bytes(mcp: &Mcp, input: Vec<u8>) -> String {
        let mut output = Vec::new();
        serve(mcp, &mut Cursor::new(input), &mut output).unwrap();
        String::from_utf8(output).unwrap()
    }

    #[test]
    fn invalid_utf8_in_newline_body_is_parse_error() {
        let (_dir, mcp) = tmp_mcp();
        let mut input = Vec::new();
        input.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\",\"x\":\"");
        input.push(0xFF);
        input.extend_from_slice(b"\"}\n");
        input.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n");

        let out = serve_bytes(&mcp, input);
        let lines: Vec<&str> = out.trim_end_matches('\n').split('\n').collect();
        assert_eq!(lines.len(), 2, "помилка + жива відповідь: {out:?}");
        let bad: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(bad["error"]["code"], -32700);
        let alive: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(alive["id"], 2);
        assert!(alive["result"].is_object());
    }

    #[test]
    fn invalid_utf8_in_header_is_parse_error() {
        let (_dir, mcp) = tmp_mcp();
        let mut input = Vec::new();
        input.push(0xFF);
        input.extend_from_slice(b"-Length: 10\r\n");
        input.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n");

        let out = serve_bytes(&mcp, input);
        assert!(out.contains("-32700"), "{out:?}");
        assert!(out.contains("\"id\":2"), "сесія має пережити кадр: {out:?}");
    }

    #[test]
    fn oversized_newline_frame_is_parse_error() {
        let (_dir, mcp) = tmp_mcp();
        let mut input = Vec::new();
        input.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\",\"pad\":\"");
        input.resize(input.len() + MAX_FRAME_BYTES + 16, b'x');
        input.extend_from_slice(b"\"}\n");
        input.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n");

        let out = serve_bytes(&mcp, input);
        let lines: Vec<&str> = out.trim_end_matches('\n').split('\n').collect();
        assert_eq!(lines.len(), 2, "помилка + жива відповідь: {out:?}");
        let bad: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(bad["error"]["code"], -32700);
        let alive: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(alive["id"], 2);
    }

    #[test]
    fn oversized_content_length_is_parse_error() {
        let (_dir, mcp) = tmp_mcp();
        let mut input = Vec::new();
        input.extend_from_slice(b"Content-Length: 99999999\r\n\r\n");

        let out = serve_bytes(&mcp, input);
        assert!(out.contains("-32700"), "{out:?}");
    }

    #[test]
    fn batch_array_does_not_kill_server() {
        let (_dir, mcp) = tmp_mcp();
        let mut input = Vec::new();
        input.extend_from_slice(b"[{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}]\n");
        input.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n");

        let out = serve_bytes(&mcp, input);
        assert!(!out.contains("неповні заголовки"), "{out:?}");
        assert!(out.contains("\"id\":2"), "сесія має пережити batch: {out:?}");
    }

    #[test]
    fn bom_before_first_message_is_skipped() {
        let (_dir, mcp) = tmp_mcp();
        let mut input = vec![0xEF, 0xBB, 0xBF];
        input.extend_from_slice(b"{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"ping\"}\n");

        let out = serve_bytes(&mcp, input);
        assert!(!out.contains("Content-Length"), "{out:?}");
        let resp: Value = serde_json::from_str(out.trim_end_matches('\n')).unwrap();
        assert_eq!(resp["id"], 4);
        assert!(resp["result"].is_object());
    }

    // ── lock_status ──────────────────────────────────────────────────────

    #[test]
    fn tools_list_has_seven_tools_including_lock_status() {
        let (_dir, mcp) = tmp_mcp();
        let list = mcp
            .handle_rpc(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }))
            .unwrap();
        let tools = list["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 7, "{tools:?}");
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"lock_status"), "{names:?}");
    }

    #[test]
    fn lock_status_on_empty_store_is_empty_list() {
        let (_dir, mcp) = tmp_mcp();
        let resp = call(&mcp, 1, "lock_status", json!({}));
        assert!(!is_error(&resp), "{resp}");
        let out = tool_json(&resp);
        assert_eq!(out["count"], 0);
        assert!(out["locks"].as_array().unwrap().is_empty(), "{out}");
    }

    #[test]
    fn lock_status_lists_live_locks_as_not_expired() {
        let (_dir, mcp) = tmp_mcp();
        for (topic, holder) in [("xvid/core", "Grok"), ("xvid/ui", "Claude")] {
            let locked = call(
                &mcp,
                1,
                "lock",
                json!({ "topic": topic, "holder": holder, "ttl_sec": 300, "note": "працюю" }),
            );
            assert!(!is_error(&locked), "{locked}");
        }

        let out = tool_json(&call(&mcp, 2, "lock_status", json!({})));
        assert_eq!(out["count"], 2, "{out}");
        let locks = out["locks"].as_array().unwrap();
        for lock in locks {
            assert_eq!(lock["ttl_sec"], 300);
            assert_eq!(lock["note"], "працюю");
            assert_eq!(
                lock["expires_at"].as_i64().unwrap(),
                lock["taken_at"].as_i64().unwrap() + 300
            );
            assert_eq!(lock["expired"], false, "живий замок не протермінований");
        }
        let holders: Vec<&str> = locks.iter().map(|l| l["holder"].as_str().unwrap()).collect();
        assert!(holders.contains(&"Grok") && holders.contains(&"Claude"), "{out}");
    }

    /// Два замки, один протермінований. Через `lock_status` таке не зібрати:
    /// `Store::locks` вимітає протерміновані рядки ще до SELECT, а `MIN_TTL_SEC`
    /// — 60 с. Тому позначку перевіряємо на самому обчисленні.
    #[test]
    fn locks_payload_marks_the_expired_one() {
        let now = 1_700_000_000;
        let locks = vec![
            Lock {
                topic: "xvid/core".to_string(),
                holder: Agent::Grok,
                taken_at: now - 30,
                ttl_sec: 300,
                note: "живий".to_string(),
            },
            Lock {
                topic: "xvid/ui".to_string(),
                holder: Agent::Claude,
                taken_at: now - 600,
                ttl_sec: 60,
                note: "прострочений".to_string(),
            },
        ];

        let out = locks_payload(&locks, now).unwrap();
        assert_eq!(out["now"], now);
        assert_eq!(out["count"], 2);
        let locks = out["locks"].as_array().unwrap();

        assert_eq!(locks[0]["topic"], "xvid/core");
        assert_eq!(locks[0]["holder"], "Grok");
        assert_eq!(locks[0]["taken_at"], now - 30);
        assert_eq!(locks[0]["expires_at"], now + 270);
        assert_eq!(locks[0]["expired"], false);

        assert_eq!(locks[1]["topic"], "xvid/ui");
        assert_eq!(locks[1]["holder"], "Claude");
        assert_eq!(locks[1]["expires_at"], now - 540);
        assert_eq!(locks[1]["expired"], true);
    }

    /// Мить рівності — теж протермінований: `Store` саме за цією межею й вимітає.
    #[test]
    fn lock_expiring_exactly_now_is_expired() {
        let now = 1_700_000_000;
        let locks = vec![Lock {
            topic: "t".to_string(),
            holder: Agent::Grok,
            taken_at: now - 60,
            ttl_sec: 60,
            note: String::new(),
        }];
        let out = locks_payload(&locks, now).unwrap();
        assert_eq!(out["locks"][0]["expired"], true);
    }

    // ── особистість: явний аргумент → AGENT_NAME → помилка ───────────────

    /// `set_var` глобальний на процес, тож env-тести ходять по черзі.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct EnvGuard {
        prev: Option<String>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var(AGENT_NAME_ENV, v),
                None => std::env::remove_var(AGENT_NAME_ENV),
            }
        }
    }

    fn with_agent_name(value: Option<&str>) -> EnvGuard {
        // Отруєний mutex тут нешкідливий: захищаємо не дані, а порядок.
        let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var(AGENT_NAME_ENV).ok();
        match value {
            Some(v) => std::env::set_var(AGENT_NAME_ENV, v),
            None => std::env::remove_var(AGENT_NAME_ENV),
        }
        EnvGuard { prev, _lock: lock }
    }

    #[test]
    fn post_without_from_takes_identity_from_env() {
        let _guard = with_agent_name(Some("claude"));
        let (_dir, mcp) = tmp_mcp();

        let posted = call(
            &mcp,
            1,
            "post",
            json!({ "to": "Grok", "topic": "xvid/core", "op": "N", "body": {} }),
        );
        assert!(!is_error(&posted), "{posted}");

        let inbox = call(&mcp, 2, "inbox", json!({ "agent": "Grok" }));
        let msgs = tool_json(&inbox)["messages"].as_array().unwrap().clone();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["envelope"]["from"], "Claude", "регістр нормалізовано");
    }

    #[test]
    fn post_without_from_and_without_env_is_a_clear_error() {
        let _guard = with_agent_name(None);
        let (_dir, mcp) = tmp_mcp();

        let posted = call(
            &mcp,
            1,
            "post",
            json!({ "to": "Grok", "topic": "xvid/core", "op": "N", "body": {} }),
        );
        assert!(is_error(&posted), "{posted}");
        let text = tool_text(&posted);
        assert!(text.contains("немає from"), "{text}");
        assert!(text.contains(AGENT_NAME_ENV), "{text}");
    }

    #[test]
    fn alien_agent_name_is_rejected_with_the_allowed_list() {
        let _guard = with_agent_name(Some("hacker"));
        let (_dir, mcp) = tmp_mcp();

        let posted = call(
            &mcp,
            1,
            "post",
            json!({ "to": "Grok", "topic": "xvid/core", "op": "N", "body": {} }),
        );
        assert!(is_error(&posted), "{posted}");
        let text = tool_text(&posted);
        assert!(text.contains("hacker"), "{text}");
        assert!(text.contains("Grok") && text.contains("Claude"), "{text}");
    }

    #[test]
    fn both_is_not_a_valid_agent_name() {
        let _guard = with_agent_name(Some("Both"));
        let (_dir, mcp) = tmp_mcp();

        let locked = call(&mcp, 1, "lock", json!({ "topic": "xvid/core" }));
        assert!(is_error(&locked), "{locked}");
        assert!(tool_text(&locked).contains("Both"), "{locked}");
    }

    #[test]
    fn explicit_argument_beats_env() {
        let _guard = with_agent_name(Some("Claude"));
        let (_dir, mcp) = tmp_mcp();

        let posted = call(
            &mcp,
            1,
            "post",
            json!({ "from": "Grok", "to": "Claude", "topic": "xvid/core", "op": "N", "body": {} }),
        );
        assert!(!is_error(&posted), "{posted}");

        let inbox = call(&mcp, 2, "inbox", json!({ "agent": "Claude" }));
        let msgs = tool_json(&inbox)["messages"].as_array().unwrap().clone();
        assert_eq!(msgs[0]["envelope"]["from"], "Grok");

        let locked = call(
            &mcp,
            3,
            "lock",
            json!({ "topic": "xvid/core", "holder": "Grok", "ttl_sec": 300 }),
        );
        assert_eq!(tool_json(&locked)["holder"], "Grok");
    }

    #[test]
    fn lock_and_unlock_without_holder_take_identity_from_env() {
        let _guard = with_agent_name(Some("GROK"));
        let (_dir, mcp) = tmp_mcp();

        let locked = call(&mcp, 1, "lock", json!({ "topic": "xvid/core", "ttl_sec": 300 }));
        assert!(!is_error(&locked), "{locked}");
        assert_eq!(tool_json(&locked)["holder"], "Grok");

        let status = tool_json(&call(&mcp, 2, "lock_status", json!({})));
        assert_eq!(status["locks"][0]["holder"], "Grok");

        let unlocked = call(&mcp, 3, "unlock", json!({ "topic": "xvid/core" }));
        assert!(!is_error(&unlocked), "{unlocked}");
        assert_eq!(tool_json(&call(&mcp, 4, "lock_status", json!({})))["count"], 0);
    }

    #[test]
    fn unlock_without_holder_and_without_env_is_a_clear_error() {
        let _guard = with_agent_name(None);
        let (_dir, mcp) = tmp_mcp();

        let unlocked = call(&mcp, 1, "unlock", json!({ "topic": "xvid/core" }));
        assert!(is_error(&unlocked), "{unlocked}");
        let text = tool_text(&unlocked);
        assert!(text.contains("немає holder"), "{text}");
        assert!(text.contains(AGENT_NAME_ENV), "{text}");
    }

    #[test]
    fn without_env_paths_stay_exactly_prod() {
        let (db, now) = resolve_paths(None, None);
        assert_eq!(db, PathBuf::from(PROD_DB));
        assert_eq!(now, PathBuf::from(PROD_NOW_MD));
    }

    #[test]
    fn env_paths_win_over_prod() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("copy.db");
        let now_path = dir.path().join("NOW.copy.md");

        let (db, now) = resolve_paths(
            Some(db_path.to_str().unwrap()),
            Some(now_path.to_str().unwrap()),
        );
        assert_eq!(db, db_path);
        assert_eq!(now, now_path);
    }

    #[test]
    fn empty_env_values_mean_not_set() {
        // Порожня змінна не має відправити сервер писати повз прод.
        for blank in ["", "   ", "\t"] {
            let (db, now) = resolve_paths(Some(blank), Some(blank));
            assert_eq!(db, PathBuf::from(PROD_DB), "db на «{blank}»");
            assert_eq!(now, PathBuf::from(PROD_NOW_MD), "now на «{blank}»");
        }
    }

    /// ⚠️ Раніше цей тест закріплював пастку: задана сама лише `EXCHANGE_DB`
    /// лишала NOW.md на проді, тобто «прогонка на копії» читала копію, а писала
    /// в живу дошку. Тепер половина пари — відмова старту.
    #[test]
    fn one_env_set_alone_is_refused_with_an_explanation() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("copy.db");
        let now_path = dir.path().join("NOW.copy.md");

        let err = resolve_paths_checked(Some(db_path.to_str().unwrap()), None)
            .expect_err("сама лише база має бути відмовою");
        let text = err.to_string();
        assert!(text.contains(DB_ENV), "{text}");
        assert!(text.contains(NOW_MD_ENV), "{text}");
        // Текст мусить називати саме те, що постраждало б.
        assert!(text.contains(PROD_NOW_MD), "{text}");

        let err = resolve_paths_checked(None, Some(now_path.to_str().unwrap()))
            .expect_err("сам лише NOW.md має бути відмовою");
        let text = err.to_string();
        assert!(text.contains(DB_ENV), "{text}");
        assert!(text.contains(NOW_MD_ENV), "{text}");
        assert!(text.contains(PROD_DB), "{text}");
    }

    /// Порожня змінна — «не задано», тож порожня половина пари відмови не дає:
    /// це рівно той самий прод, а не мішанина.
    #[test]
    fn blank_half_counts_as_unset_not_as_half_a_pair() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("copy.db");

        for blank in ["", "   ", "\t"] {
            let (db, now) = resolve_paths_checked(Some(blank), Some(blank))
                .unwrap_or_else(|e| panic!("обидві порожні мали пройти: {e}"));
            assert_eq!(db, PathBuf::from(PROD_DB), "db на «{blank}»");
            assert_eq!(now, PathBuf::from(PROD_NOW_MD), "now на «{blank}»");

            // А задана база з порожньою парою — та сама пастка, та сама відмова.
            assert!(
                resolve_paths_checked(Some(db_path.to_str().unwrap()), Some(blank)).is_err(),
                "порожній {NOW_MD_ENV} на «{blank}» мав лишитись відмовою"
            );
        }
    }

    /// Обидві задані — працює, і жодного натяку на прод у шляхах.
    #[test]
    fn both_env_paths_together_are_accepted() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("copy.db");
        let now_path = dir.path().join("NOW.copy.md");

        let (db, now) = resolve_paths_checked(
            Some(db_path.to_str().unwrap()),
            Some(now_path.to_str().unwrap()),
        )
        .unwrap();
        assert_eq!(db, db_path);
        assert_eq!(now, now_path);
    }

    /// Пара змінних у **процесі**, а не в аргументах: `paths_from_env` читає
    /// глобальний стан, тож ходить під тим самим `ENV_LOCK`.
    struct PathEnvGuard {
        prev_db: Option<String>,
        prev_now: Option<String>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    fn set_or_clear(key: &str, value: &Option<String>) {
        match value {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }

    impl Drop for PathEnvGuard {
        fn drop(&mut self) {
            set_or_clear(DB_ENV, &self.prev_db);
            set_or_clear(NOW_MD_ENV, &self.prev_now);
        }
    }

    fn with_path_env(db: Option<&str>, now: Option<&str>) -> PathEnvGuard {
        let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let guard = PathEnvGuard {
            prev_db: std::env::var(DB_ENV).ok(),
            prev_now: std::env::var(NOW_MD_ENV).ok(),
            _lock: lock,
        };
        set_or_clear(DB_ENV, &db.map(str::to_string));
        set_or_clear(NOW_MD_ENV, &now.map(str::to_string));
        guard
    }

    /// Перевірку пари має робити **прод-вхід**, а не лише чиста функція поруч.
    ///
    /// ⚠️ Тест навмисне ходить через `paths_from_env` — те саме, що кличе
    /// `run_stdio`. Перевіряти самий `resolve_paths_checked` означало б
    /// пересвідчитись, що безпечний варіант написаний, і не помітити, що
    /// сервер стартує повз нього.
    #[test]
    fn env_pair_is_checked_on_the_startup_path() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("copy.db");
        let now_path = dir.path().join("NOW.copy.md");
        let db = db_path.to_str().unwrap();
        let now = now_path.to_str().unwrap();

        {
            let _g = with_path_env(Some(db), None);
            let err = paths_from_env().expect_err("сама лише база мала відмовити старт");
            let text = err.to_string();
            assert!(text.contains(NOW_MD_ENV), "{text}");
            assert!(text.contains(PROD_NOW_MD), "{text}");
        }
        {
            let _g = with_path_env(None, Some(now));
            let err = paths_from_env().expect_err("сам лише NOW.md мав відмовити старт");
            let text = err.to_string();
            assert!(text.contains(DB_ENV), "{text}");
            assert!(text.contains(PROD_DB), "{text}");
        }
        {
            let _g = with_path_env(Some(db), Some(now));
            let (got_db, got_now) = paths_from_env().expect("пара цілком — старт дозволено");
            assert_eq!(got_db, db_path);
            assert_eq!(got_now, now_path);
        }
        {
            let _g = with_path_env(None, None);
            let (got_db, got_now) = paths_from_env().expect("жодної змінної — прод");
            assert_eq!(got_db, PathBuf::from(PROD_DB));
            assert_eq!(got_now, PathBuf::from(PROD_NOW_MD));
        }
    }

    /// Перевизначені шляхи справді відкриваються — і прод при цьому не
    /// створюється й не чіпається.
    #[test]
    fn resolved_paths_open_a_working_store() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("copy.db");
        let now_path = dir.path().join("NOW.copy.md");

        let (db, now) = resolve_paths(
            Some(db_path.to_str().unwrap()),
            Some(now_path.to_str().unwrap()),
        );
        let mcp = Mcp::open(&db, &now).unwrap();

        let posted = call(
            &mcp,
            1,
            "post",
            json!({
                "from": "Claude",
                "to": "Grok",
                "topic": "суха/прогонка",
                "op": "Q",
                "body": { "q": "ping" }
            }),
        );
        assert!(!is_error(&posted), "{posted}");
        let id = tool_json(&posted)["id"].as_i64().unwrap();

        assert!(db_path.exists(), "БД мала лягти за перевизначеним шляхом");
        let now_text = fs::read_to_string(&now_path).unwrap();
        assert!(now_text.starts_with("# Зараз\n"), "{now_text}");
        assert!(now_text.contains(&format!("#{id}")), "{now_text}");
    }

    // ——— пачковий ack і фільтри inbox ———

    /// Покласти повідомлення й віддати його id. `rpc_id` — щоб виклики
    /// у тесті не зливались в один.
    fn post_msg(mcp: &Mcp, rpc_id: i64, from: &str, to: &str, topic: &str, body: Value) -> i64 {
        let posted = call(
            mcp,
            rpc_id,
            "post",
            json!({ "from": from, "to": to, "topic": topic, "op": "N", "body": body }),
        );
        assert!(!is_error(&posted), "{posted}");
        tool_json(&posted)["id"].as_i64().unwrap()
    }

    fn inbox_msgs(mcp: &Mcp, rpc_id: i64, args: Value) -> Vec<Value> {
        let resp = call(mcp, rpc_id, "inbox", args);
        assert!(!is_error(&resp), "{resp}");
        tool_json(&resp)["messages"].as_array().unwrap().clone()
    }

    /// Пачка не падає через сторонні id: рахується тільки те, що агент
    /// справді мав право підтвердити, і в відповіді видно обидва числа.
    #[test]
    fn ack_ids_counts_only_own_and_reports_requested() {
        let (_dir, mcp) = tmp_mcp();

        let mine = post_msg(&mcp, 1, "Grok", "Claude", "своє", json!({ "q": "ping" }));
        let foreign = post_msg(&mcp, 2, "Claude", "Grok", "чуже", json!({ "q": "ping" }));

        let resp = call(
            &mcp,
            3,
            "ack",
            json!({ "ids": [mine, foreign, 999_999], "agent": "Claude" }),
        );
        assert!(!is_error(&resp), "{resp}");
        let out = tool_json(&resp);
        assert_eq!(out["acked"], 1, "{out}");
        assert_eq!(out["requested"], 3, "{out}");
        assert_eq!(out["agent"], "Claude");

        // Своє прочитане, чуже — ні.
        assert!(
            inbox_msgs(&mcp, 4, json!({ "agent": "Claude", "unread_only": true })).is_empty()
        );
        let grok = inbox_msgs(&mcp, 5, json!({ "agent": "Grok", "unread_only": true }));
        assert_eq!(grok.len(), 1);
        assert_eq!(grok[0]["id"], foreign);

        // Повторна пачка тим самим складом — нуль, і знову не помилка.
        let again = call(
            &mcp,
            6,
            "ack",
            json!({ "ids": [mine, foreign, 999_999], "agent": "Claude" }),
        );
        assert!(!is_error(&again), "{again}");
        assert_eq!(tool_json(&again)["acked"], 0);
    }

    #[test]
    fn ack_id_and_ids_together_is_error() {
        let (_dir, mcp) = tmp_mcp();
        let id = post_msg(&mcp, 1, "Grok", "Claude", "тема", json!({ "q": "ping" }));

        let resp = call(
            &mcp,
            2,
            "ack",
            json!({ "id": id, "ids": [id], "agent": "Claude" }),
        );
        assert!(is_error(&resp), "{resp}");
        let text = tool_text(&resp);
        assert!(text.contains("id") && text.contains("ids"), "{text}");

        // Помилка розбору — сховище не чіпалось.
        let unread = inbox_msgs(&mcp, 3, json!({ "agent": "Claude", "unread_only": true }));
        assert_eq!(unread.len(), 1, "повідомлення мало лишитись непрочитаним");
    }

    #[test]
    fn ack_ids_rejects_both_as_agent() {
        let (_dir, mcp) = tmp_mcp();
        let id = post_msg(&mcp, 1, "Grok", "Both", "тема", json!({ "q": "ping" }));

        let resp = call(&mcp, 2, "ack", json!({ "ids": [id], "agent": "Both" }));
        assert!(is_error(&resp), "{resp}");
        assert!(tool_text(&resp).contains("Both"), "{}", tool_text(&resp));
    }

    #[test]
    fn ack_empty_ids_is_zero_not_error() {
        let (_dir, mcp) = tmp_mcp();
        let resp = call(&mcp, 1, "ack", json!({ "ids": [], "agent": "Claude" }));
        assert!(!is_error(&resp), "{resp}");
        assert_eq!(tool_json(&resp)["acked"], 0);
        assert_eq!(tool_json(&resp)["requested"], 0);
    }

    /// Один `id` іде через [`Store::ack_one`], тобто **питає, чиє це**.
    ///
    /// ⚠️ Тест сторожить саме той шлях, що раніше стояв на `Store::ack`:
    /// той гасив непрочитане за самим лише `id`, без адресата, і чужі id
    /// відкрито лежать в `agent_talk.md`. Тепер чуже лишається чужим, а
    /// відмова позначати — `acked = false`, а не збій: чуже, неіснуюче й
    /// уже прочитане ззовні не розрізняються навмисно.
    #[test]
    fn ack_single_id_marks_only_own_unread_and_never_errors() {
        let (_dir, mcp) = tmp_mcp();
        let foreign = post_msg(&mcp, 1, "Claude", "Grok", "чуже", json!({ "q": "ping" }));

        let resp = call(&mcp, 2, "ack", json!({ "id": foreign, "agent": "Claude" }));
        assert!(!is_error(&resp), "чужий id — відповідь, а не збій: {resp}");
        let out = tool_json(&resp);
        assert_eq!(out["acked"], false, "{out}");
        assert_eq!(out["id"], foreign, "{out}");
        assert_eq!(out["agent"], "Claude", "{out}");

        // Головне: адресат досі бачить його непрочитаним.
        let grok = inbox_msgs(&mcp, 3, json!({ "agent": "Grok", "unread_only": true }));
        assert_eq!(grok.len(), 1, "чуже мало лишитись непрочитаним: {grok:?}");
        assert_eq!(grok[0]["id"], foreign);

        // Своє — позначається; повторно вже ні, і теж без помилки.
        let mine = post_msg(&mcp, 4, "Claude", "Grok", "своє", json!({ "q": "ping" }));
        let first = call(&mcp, 5, "ack", json!({ "id": mine, "agent": "Grok" }));
        assert!(!is_error(&first), "{first}");
        assert_eq!(tool_json(&first)["acked"], true, "{first}");
        let again = call(&mcp, 6, "ack", json!({ "id": mine, "agent": "Grok" }));
        assert!(!is_error(&again), "{again}");
        assert_eq!(tool_json(&again)["acked"], false, "{again}");

        // Неіснуючий id — та сама відповідь, а не оракул чужих id.
        let missing = call(&mcp, 7, "ack", json!({ "id": 999_999, "agent": "Grok" }));
        assert!(!is_error(&missing), "{missing}");
        assert_eq!(tool_json(&missing)["acked"], false, "{missing}");
    }

    /// Один `id` без особистості — помилка, а не позначення «кимось».
    #[test]
    fn ack_single_id_without_agent_is_refused() {
        let (_dir, mcp) = tmp_mcp();
        let _guard = with_agent_name(None);
        let id = post_msg(&mcp, 1, "Grok", "Claude", "тема", json!({ "q": "ping" }));

        let resp = call(&mcp, 2, "ack", json!({ "id": id }));
        assert!(is_error(&resp), "{resp}");
        assert!(tool_text(&resp).contains("agent"), "{}", tool_text(&resp));

        let unread = inbox_msgs(&mcp, 3, json!({ "agent": "Claude", "unread_only": true }));
        assert_eq!(unread.len(), 1, "нічого не мало позначитись");
    }

    /// `lock` іде через [`Store::lock_ex`] — гучний варіант.
    ///
    /// ⚠️ Тихий `Store::lock` повертає `()`, тож `evicted` і `notified_id`
    /// у відповіді взятись нізвідки: наявність обох полів і є доказом, що
    /// прод стоїть на `lock_ex`. Сам евікшн (перехоплення протермінованого
    /// зі сповіщенням колишньому тримачеві) перевіряється в `store` —
    /// звідси його не поставити: `MIN_TTL_SEC` = 60 с, а зістарити замок
    /// ззовні `store` нічим, зʼєднання приватне.
    #[test]
    fn lock_answer_carries_eviction_fields() {
        let (_dir, mcp) = tmp_mcp();

        let locked = call(
            &mcp,
            1,
            "lock",
            json!({ "topic": "xvid/core", "holder": "Grok", "ttl_sec": 90, "note": "працюю" }),
        );
        assert!(!is_error(&locked), "{locked}");
        let out = tool_json(&locked);
        let map = out.as_object().expect("відповідь lock — обʼєкт");
        assert!(
            map.contains_key("evicted"),
            "у відповіді немає `evicted` — прод, схоже, знову на тихому lock: {out}"
        );
        assert!(
            map.contains_key("notified_id"),
            "у відповіді немає `notified_id`: {out}"
        );
        // Вільна тема — перехоплювати нікого.
        assert_eq!(out["evicted"], Value::Null, "{out}");
        assert_eq!(out["notified_id"], Value::Null, "{out}");
        assert_eq!(out["holder"], "Grok", "{out}");
    }

    /// `NOW.md`, який насправді тека: `render` на ній падає гарантовано —
    /// і на читанні, і на записі.
    fn mcp_with_unrenderable_now_md() -> (tempfile::TempDir, Mcp) {
        let dir = tempdir().unwrap();
        let db = dir.path().join("exchange.db");
        let now = dir.path().join("NOW.md");
        fs::create_dir(&now).unwrap();
        let mcp = Mcp::open(&db, &now).unwrap();
        (dir, mcp)
    }

    /// Збій перемальовування не перетворює виконану операцію на помилку.
    ///
    /// ⚠️ Це не про красу відповіді. `post` уже закомічений; віддай агентові
    /// помилку — він повторить, і в базі ляже дублікат. Те саме з `lock`:
    /// замок узятий, а відповідь каже «не вийшло».
    #[test]
    fn a_failing_rerender_does_not_undo_a_committed_write() {
        let (_dir, mcp) = mcp_with_unrenderable_now_md();

        let posted = call(
            &mcp,
            1,
            "post",
            json!({ "from": "Grok", "to": "Claude", "topic": "тема", "op": "N",
                    "body": { "q": "ping" } }),
        );
        assert!(
            !is_error(&posted),
            "успішний post не має ставати помилкою через рендер: {posted}"
        );
        let out = tool_json(&posted);
        assert!(out["id"].as_i64().unwrap_or(0) > 0, "{out}");
        let note = out["render_error"]
            .as_str()
            .expect("збій рендера мав лишити слід окремим полем");
        assert!(!note.is_empty(), "порожній render_error нічого не каже");

        // Повідомлення справді в базі — рівно одне, без повтору.
        let unread = inbox_msgs(&mcp, 2, json!({ "agent": "Claude", "unread_only": true }));
        assert_eq!(unread.len(), 1, "{unread:?}");

        // Те саме для замка: узятий і видимий, попри збій малювання.
        let locked = call(
            &mcp,
            3,
            "lock",
            json!({ "topic": "xvid/core", "holder": "Grok", "ttl_sec": 90, "note": "працюю" }),
        );
        assert!(!is_error(&locked), "{locked}");
        assert!(
            tool_json(&locked)["render_error"].is_string(),
            "{locked}"
        );
        let status = call(&mcp, 4, "lock_status", json!({}));
        assert!(!is_error(&status), "{status}");
        assert_eq!(tool_json(&status)["count"], 1, "{status}");

        // А сам `render` як інструмент лишається чесним: його робота — саме
        // малювати, тож його збій — таки збій.
        let rendered = call(&mcp, 5, "render", json!({}));
        assert!(is_error(&rendered), "{rendered}");
    }

    /// Довге тіло — щоб стеля `brief` було на чому побачити.
    fn long_body() -> Value {
        json!({ "text": "я".repeat(600) })
    }

    #[test]
    fn inbox_unread_only_briefs_bodies_by_default() {
        let (_dir, mcp) = tmp_mcp();
        post_msg(&mcp, 1, "Grok", "Claude", "тема", long_body());

        let msgs = inbox_msgs(&mcp, 2, json!({ "agent": "Claude", "unread_only": true }));
        assert_eq!(msgs.len(), 1);
        let body = msgs[0]["envelope"]["body"].as_str().unwrap_or_else(|| {
            panic!("обрізане тіло має бути рядком: {}", msgs[0]["envelope"]["body"])
        });
        assert!(body.ends_with('…'), "позначка обрізання має бути видимою: {body}");
        assert_eq!(
            body.chars().count(),
            DEFAULT_BRIEF_CHARS + 1,
            "стеля {DEFAULT_BRIEF_CHARS} плюс позначка"
        );
    }

    /// Дефолт вмикається **тільки** на розгрібанні черги: звичайний перегляд
    /// скриньки віддає тіла цілими, як і `render` для дошки.
    #[test]
    fn inbox_without_unread_only_is_not_briefed() {
        let (_dir, mcp) = tmp_mcp();
        post_msg(&mcp, 1, "Grok", "Claude", "тема", long_body());

        let msgs = inbox_msgs(&mcp, 2, json!({ "agent": "Claude" }));
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["envelope"]["body"], long_body());
    }

    #[test]
    fn inbox_brief_zero_gives_full_bodies() {
        let (_dir, mcp) = tmp_mcp();
        post_msg(&mcp, 1, "Grok", "Claude", "тема", long_body());

        let msgs = inbox_msgs(
            &mcp,
            2,
            json!({ "agent": "Claude", "unread_only": true, "brief": 0 }),
        );
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["envelope"]["body"], long_body());

        // Явний null — те саме «повне тіло».
        let msgs = inbox_msgs(
            &mcp,
            3,
            json!({ "agent": "Claude", "unread_only": true, "brief": null }),
        );
        assert_eq!(msgs[0]["envelope"]["body"], long_body());
    }

    #[test]
    fn inbox_explicit_brief_wins_over_default() {
        let (_dir, mcp) = tmp_mcp();
        post_msg(&mcp, 1, "Grok", "Claude", "тема", long_body());

        let msgs = inbox_msgs(
            &mcp,
            2,
            json!({ "agent": "Claude", "unread_only": true, "brief": 10 }),
        );
        let body = msgs[0]["envelope"]["body"].as_str().unwrap();
        assert_eq!(body.chars().count(), 11, "10 символів плюс позначка: {body}");
    }

    #[test]
    fn inbox_limit_returns_newest() {
        let (_dir, mcp) = tmp_mcp();
        let a = post_msg(&mcp, 1, "Grok", "Claude", "тема", json!({ "n": 1 }));
        let b = post_msg(&mcp, 2, "Grok", "Claude", "тема", json!({ "n": 2 }));
        let c = post_msg(&mcp, 3, "Grok", "Claude", "тема", json!({ "n": 3 }));

        let msgs = inbox_msgs(&mcp, 4, json!({ "agent": "Claude", "limit": 2 }));
        let ids: Vec<i64> = msgs.iter().map(|m| m["id"].as_i64().unwrap()).collect();
        assert_eq!(ids, vec![b, c], "мали прийти два найновіші, а не {a}…");
    }

    #[test]
    fn inbox_filters_by_topic() {
        let (_dir, mcp) = tmp_mcp();
        let core = post_msg(&mcp, 1, "Grok", "Claude", "xvid/core", json!({ "n": 1 }));
        post_msg(&mcp, 2, "Grok", "Claude", "xvid/ui", json!({ "n": 2 }));

        let msgs = inbox_msgs(&mcp, 3, json!({ "agent": "Claude", "topic": "xvid/core" }));
        let ids: Vec<i64> = msgs.iter().map(|m| m["id"].as_i64().unwrap()).collect();
        assert_eq!(ids, vec![core]);

        // Нормалізація теми — робота store; тут перевіряємо, що фільтр
        // доходить до неї, а не з'їдається на межі MCP.
        let msgs = inbox_msgs(&mcp, 4, json!({ "agent": "Claude", "topic": "XVID/CORE" }));
        assert_eq!(msgs.len(), 1, "{msgs:?}");

        let msgs = inbox_msgs(&mcp, 5, json!({ "agent": "Claude", "topic": "нема" }));
        assert!(msgs.is_empty(), "{msgs:?}");
    }

    #[test]
    fn inbox_rejects_negative_limit() {
        let (_dir, mcp) = tmp_mcp();
        let resp = call(&mcp, 1, "inbox", json!({ "agent": "Claude", "limit": -1 }));
        assert!(is_error(&resp), "{resp}");
    }

    /// Схеми розширились, кількість інструментів — ні: кожен зайвий tool
    /// висить у контексті агента щосесії.
    #[test]
    fn tool_defs_stay_seven_and_single_line() {
        let defs = tool_defs();
        let arr = defs.as_array().unwrap();
        assert_eq!(arr.len(), 7, "інструментів має лишатись сім");
        for t in arr {
            let name = t["name"].as_str().unwrap();
            let desc = t["description"].as_str().unwrap();
            assert!(!desc.contains('\n'), "опис {name} має бути однорядковим");
        }
        let ack = arr.iter().find(|t| t["name"] == "ack").unwrap();
        let props = &ack["inputSchema"]["properties"];
        assert!(props["ids"].is_object() && props["agent"].is_object(), "{ack}");
        assert!(
            ack["inputSchema"].get("required").is_none(),
            "ack більше не вимагає id: приймається або id, або ids"
        );
        let inbox = arr.iter().find(|t| t["name"] == "inbox").unwrap();
        let props = &inbox["inputSchema"]["properties"];
        for f in ["unread_only", "limit", "brief", "topic"] {
            assert!(props[f].is_object(), "немає {f} у схемі inbox: {inbox}");
        }
    }
}
