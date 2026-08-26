//! Реалізує агент store. Не чіпати з render/mcp.

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

const ENVELOPE_V: u32 = 1;
const DEFAULT_TTL_SEC: i64 = 7200;

/// Версія схеми БД, яку знає ця збірка. Пишеться в `PRAGMA user_version`.
/// 1 = messages + locks + idx_messages_inbox (базова схема).
pub const SCHEMA_VERSION: i64 = 1;

/// Стеля тіла повідомлення — 2000 символів **серіалізованого JSON**
/// (саме того рядка, що лягає в колонку `body`).
///
/// Міряється серіалізація, а не «текст усередині»: у базі зберігається
/// саме вона, і тільки її довжина однозначна для будь-якого `Value`.
/// Перевищення — [`Error::BodyTooLong`], а не тихе обрізання: мовчки
/// зрізаний хвіст повідомлення виглядає як повне, і читач ніколи не
/// дізнається, що частину думки з'їли.
pub const MAX_BODY_CHARS: usize = 2000;

/// Нижня межа TTL замка. Менші значення затискаються сюди.
pub const MIN_TTL_SEC: i64 = 60;
/// Верхня межа TTL замка. Більші значення затискаються сюди.
pub const MAX_TTL_SEC: i64 = 86_400;

/// Канонічний вигляд теми: нижній регістр, без крайніх пробілів,
/// внутрішні пробільні послідовності схлопнуті в один пробіл.
///
/// Закритого словника тем немає — це чисто текстова нормалізація,
/// жодних «схожих тем» тут не вгадується.
pub fn normalize_topic(topic: &str) -> String {
    topic
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// TTL у межах [`MIN_TTL_SEC`], [`MAX_TTL_SEC`].
/// `ttl_sec <= 0` означає «за замовчуванням» і дає [`DEFAULT_TTL_SEC`].
fn clamp_ttl(ttl_sec: i64) -> i64 {
    if ttl_sec <= 0 {
        DEFAULT_TTL_SEC
    } else {
        ttl_sec.clamp(MIN_TTL_SEC, MAX_TTL_SEC)
    }
}

/// Один крок міграції: довести схему до версії `to`.
///
/// Кроки виконуються в порядку зростання `to` всередині однієї транзакції
/// разом із записом `user_version` — або все, або нічого.
struct Migration {
    to: i64,
    apply: fn(&Connection) -> Result<(), Error>,
}

/// Кроків поки немає: версія 1 — це базова схема, яку створює `CREATE TABLE
/// IF NOT EXISTS` у [`Store::open`]. Наступні зрізи додають сюди рядки виду
/// `Migration { to: 2, apply: |c| { c.execute_batch("ALTER TABLE …")?; Ok(()) } }`
/// і піднімають [`SCHEMA_VERSION`].
const MIGRATIONS: &[Migration] = &[];

fn read_user_version(conn: &Connection) -> Result<i64, Error> {
    Ok(conn.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

/// Довести базу до [`SCHEMA_VERSION`] в одній `BEGIN IMMEDIATE … COMMIT`.
///
/// `user_version` перевіряється **повторно вже під write-lock**: між читанням
/// у [`Store::open`] і цим моментом інше з'єднання на ту саму базу (реальний
/// випадок — `render` відкриває власний `Store::open`) могло змігрувати її
/// вперед. Транзакція `IMMEDIATE` бере write-lock одразу, тому два процеси
/// не мігрують одночасно; той, хто програв гонку, побачить уже нову версію.
fn migrate(conn: &mut Connection) -> Result<(), Error> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let current = read_user_version(&tx)?;
    if current > SCHEMA_VERSION {
        // Транзакція відкотиться при drop — нічого не змінено.
        return Err(Error::SchemaTooNew {
            found: current,
            known: SCHEMA_VERSION,
        });
    }
    if current == SCHEMA_VERSION {
        return Ok(());
    }
    for m in MIGRATIONS {
        if m.to > current && m.to <= SCHEMA_VERSION {
            (m.apply)(&tx)?;
        }
    }
    // PRAGMA не приймає плейсхолдерів; SCHEMA_VERSION — константа i64.
    tx.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
    tx.commit()?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Agent {
    Grok,
    Claude,
    /// Дефолт свідомо `Both`, і це не «нейтральне значення заради `derive`».
    ///
    /// `Default` тут потрібен лише [`InboxQuery`]. Якби дефолтом стояв
    /// конкретний агент, забутий `agent:` у запиті мовчки відкривав би чужу
    /// скриньку. `Both` — найменш привілейований варіант: у скриньці `Both`
    /// лежать самі розсилки, тобто те, що й так адресоване обом.
    #[default]
    Both,
}

impl Agent {
    fn as_str(self) -> &'static str {
        match self {
            Agent::Grok => "Grok",
            Agent::Claude => "Claude",
            Agent::Both => "Both",
        }
    }

    fn parse(s: &str) -> Result<Self, Error> {
        match s {
            "Grok" => Ok(Agent::Grok),
            "Claude" => Ok(Agent::Claude),
            "Both" => Ok(Agent::Both),
            other => Err(Error::UnknownAgent(other.to_string())),
        }
    }
}

impl std::fmt::Display for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    Q,
    A,
    N,
    L,
}

impl Op {
    fn as_str(self) -> &'static str {
        match self {
            Op::Q => "Q",
            Op::A => "A",
            Op::N => "N",
            Op::L => "L",
        }
    }

    fn parse(s: &str) -> Result<Self, Error> {
        match s {
            "Q" => Ok(Op::Q),
            "A" => Ok(Op::A),
            "N" => Ok(Op::N),
            "L" => Ok(Op::L),
            other => Err(Error::UnknownOp(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub v: u32,
    pub from: Agent,
    pub to: Agent,
    pub topic: String,
    pub op: Op,
    /// Без секретів; значення cookies не логірувати.
    pub body: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: i64,
    pub ts_unix: i64,
    pub envelope: Envelope,
    pub read_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lock {
    pub topic: String,
    pub holder: Agent,
    pub taken_at: i64,
    pub ttl_sec: i64,
    pub note: String,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("тема «{topic}» вже зайнята агентом {holder}")]
    LockHeld { topic: String, holder: Agent },
    #[error("Both не може тримати замок")]
    BothCannotLock,
    #[error("замок на тему «{topic}» не належить {holder}")]
    LockNotHeld { topic: String, holder: Agent },
    #[error("повідомлення {0} не знайдено")]
    NotFound(i64),
    #[error("версія конверта має бути 1, отримано {0}")]
    BadVersion(u32),
    #[error("невідомий агент у БД: {0}")]
    UnknownAgent(String),
    #[error("невідома операція у БД: {0}")]
    UnknownOp(String),
    #[error("не вдалося ввімкнути WAL, journal_mode={0}")]
    NotWal(String),
    #[error(
        "схема БД має версію {found}, а ця збірка знає лише {known}: \
         базу писала новіша версія exchange. Оновіть бінарник; \
         працювати на чужій схемі я не буду, база не змінена"
    )]
    SchemaTooNew { found: i64, known: i64 },
    #[error(
        "тіло повідомлення — {chars} символів серіалізованого JSON, стеля {max}: \
         скоротіть текст або винесіть його у файл. Мовчки обрізати я не буду"
    )]
    BodyTooLong { chars: usize, max: usize },
    #[error("{0} не може писати сам собі: to має відрізнятись від from")]
    SelfMessage(Agent),
    #[error("сховище пошкоджено (mutex poison)")]
    Poisoned,
}

/// Замок, який зняли з попереднього тримача через протермінування.
///
/// Повертається з [`Store::lock_ex`], щоб перехоплення було **видимим**:
/// мовчазне видалення протермінованого рядка не лишало жодного сліду —
/// ані в результаті виклику, ані в inbox колишнього тримача.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evicted {
    /// Хто тримав замок до перехоплення.
    pub holder: Agent,
    /// Коли він його взяв (unix-час).
    pub taken_at: i64,
}

/// Результат [`Store::lock_ex`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockOutcome {
    /// `Some`, якщо тему звільнили з-під протермінованого замка.
    pub evicted: Option<Evicted>,
    /// `id` сповіщення, що лягло колишньому тримачеві. `None`, якщо
    /// перехоплення не було або протермінований замок належав тому самому
    /// агентові (повідомлення самому собі заборонене — див. [`Error::SelfMessage`]).
    pub notified_id: Option<i64>,
}

/// Позначка обрізаного тіла — саме вона робить обрізання видимим.
///
/// Правило те саме, що й у [`MAX_BODY_CHARS`]: мовчки зрізаний хвіст виглядає
/// як повне повідомлення, і читач ніколи не дізнається, що частину з'їли.
/// Тому `brief` не просто ріже, а лишає слід.
pub const BRIEF_MARK: &str = "…";

/// Запит до скриньки. Структура, а не десяток позиційних аргументів:
/// наступне поле фільтра має додаватись, не ламаючи виклики.
///
/// [`InboxQuery::default()`] дає рівно те саме, що `inbox(agent, false)`:
/// усі повідомлення агента, найстарші першими, тіла цілі.
///
/// ⚠️ У `brief` **немає дефолтної стелі** і не має бути. Стеля виставляється
/// на межі MCP, свідомим викликачем: постав її тут — і `render` почав би тихо
/// писати обрізані тіла в дошку людини, а дошка виглядала б повною.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InboxQuery {
    /// Чия скринька: беруться повідомлення з `to == agent` або `to == Both`.
    pub agent: Agent,
    /// Тільки непрочитані.
    pub unread_only: bool,
    /// Скільки віддати — **найновіші**. `None` — усі.
    /// `Some(0)` — порожній список, а не «усі».
    pub limit: Option<usize>,
    /// Обрізати тіло до N символів, лишивши [`BRIEF_MARK`]. `None` — не чіпати.
    pub brief: Option<usize>,
    /// Фільтр за темою. Звіряється **після** [`normalize_topic`] з обох боків,
    /// тож «XVid  Core» і «xvid core» — та сама тема. Слеш не чіпається:
    /// «xvid/core» і «xvid / core» лишаються різними.
    pub topic: Option<String>,
    /// Не показувати власні повідомлення агента (він бачить свої розсилки
    /// `to = Both` у себе ж у скриньці). За замовчуванням `false` —
    /// стара поведінка недоторкана.
    pub exclude_own: bool,
}

/// Обрізати тіло до `n` символів і лишити видиму позначку.
///
/// Рядок ріжеться як рядок; будь-що інше спершу серіалізується — інакше
/// «половину об'єкта» довелось би віддавати поламаним JSON. Тіло, що вміщується
/// в `n`, повертається недоторканим, без позначки: позначка означає рівно
/// «тут щось відрізано».
fn brief_body(body: serde_json::Value, n: usize) -> serde_json::Value {
    fn cut(s: &str, n: usize) -> String {
        let mut out: String = s.chars().take(n).collect();
        out.push_str(BRIEF_MARK);
        out
    }
    match body {
        serde_json::Value::String(s) => {
            if s.chars().count() > n {
                serde_json::Value::String(cut(&s, n))
            } else {
                serde_json::Value::String(s)
            }
        }
        other => {
            // `to_string` на Value не падає (у ньому немає не-JSON-типів),
            // але помилку все одно не ковтаємо мовчки — лишаємо тіло цілим.
            match serde_json::to_string(&other) {
                Ok(s) if s.chars().count() > n => serde_json::Value::String(cut(&s, n)),
                _ => other,
            }
        }
    }
}

pub struct Store {
    conn: Mutex<Connection>,
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn expire_locks(conn: &Connection, now: i64) -> Result<(), Error> {
    conn.execute(
        "DELETE FROM locks WHERE taken_at + ttl_sec <= ?1",
        params![now],
    )?;
    Ok(())
}

/// Запис повідомлення **на вже взятому** з'єднанні.
///
/// ⚠️ Це не стилістика, а обхід реальної пастки. `Store` тримає
/// `conn: Mutex<Connection>`, а `std::sync::Mutex` **не реентрантний**.
/// Композиція виду «`lock_ex` бере гард → всередині кличе `self.post(...)`»
/// дала б тихий deadlock: другий `self.conn()` чекав би на гард, який тримає
/// той самий потік. Це не паніка й не помилка — процес просто зависає, а
/// зовні виглядає як «агент не відповідає».
///
/// Тому вся логіка запису живе тут і приймає `&Connection` (годиться і
/// `&Transaction` — він дереференсується у `Connection`), а публічний
/// [`Store::post`] — тонка обгортка, що бере гард рівно один раз.
/// Будь-яка майбутня композиція методів `Store` має йти цим же шляхом.
fn post_with_conn(conn: &Connection, env: Envelope) -> Result<i64, Error> {
    if env.v != ENVELOPE_V {
        return Err(Error::BadVersion(env.v));
    }
    // Заборонена лише **буквальна** рівність: from=Grok, to=Both — легальна
    // розсилка, хоч Grok і побачить її у власному inbox.
    if env.from == env.to {
        return Err(Error::SelfMessage(env.from));
    }
    let body = serde_json::to_string(&env.body)?;
    let chars = body.chars().count();
    if chars > MAX_BODY_CHARS {
        return Err(Error::BodyTooLong {
            chars,
            max: MAX_BODY_CHARS,
        });
    }
    let now = now_unix();
    conn.execute(
        "INSERT INTO messages (ts_unix, v, from_agent, to_agent, topic, op, body, read_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL)",
        params![
            now,
            env.v,
            env.from.as_str(),
            env.to.as_str(),
            env.topic,
            env.op.as_str(),
            body
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Тіло сповіщення про перехоплення. Свідомо коротке — воно має
/// поміститись у [`MAX_BODY_CHARS`] за будь-якої довжини теми.
fn eviction_body(topic: &str, evicted: &Evicted, new_holder: Agent, now: i64) -> serde_json::Value {
    serde_json::json!({
        "подія": "замок перехоплено після протермінування",
        "тема": topic,
        "попередній_тримач": evicted.holder.as_str(),
        "взято_о": evicted.taken_at,
        "новий_тримач": new_holder.as_str(),
        "перехоплено_о": now,
    })
}

/// Спільне тіло [`Store::lock`] і [`Store::lock_ex`] на вже взятому з'єднанні.
///
/// `notify` розводить дві поведінки:
/// * `false` — стара, як була: протермінований замок мовчки зникає;
/// * `true` — гучна: колишньому тримачеві лягає сповіщення (через
///   [`post_with_conn`], не через `Store::post` — див. застереження там).
fn lock_inner(
    conn: &Connection,
    topic: &str,
    holder: Agent,
    ttl_sec: i64,
    note: &str,
    notify: bool,
) -> Result<LockOutcome, Error> {
    let ttl_sec = clamp_ttl(ttl_sec);
    let now = now_unix();

    let existing = match conn.query_row(
        "SELECT holder, taken_at, ttl_sec FROM locks WHERE topic = ?1",
        params![topic],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        },
    ) {
        Ok(v) => Some(v),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) => return Err(e.into()),
    };

    // Стан теми читається **до** чистки: після `expire_locks` протермінований
    // рядок уже не існує, і сказати, хто саме тримав тему, стає нічим.
    let mut evicted = None;
    if let Some((existing_holder, taken_at, existing_ttl)) = existing {
        let existing_holder = Agent::parse(&existing_holder)?;
        if taken_at + existing_ttl <= now {
            evicted = Some(Evicted {
                holder: existing_holder,
                taken_at,
            });
        } else if existing_holder != holder {
            // Живий чужий замок — відмова, як і раніше.
            return Err(Error::LockHeld {
                topic: topic.to_string(),
                holder: existing_holder,
            });
        }
    }

    expire_locks(conn, now)?;

    conn.execute(
        "INSERT INTO locks (topic, holder, taken_at, ttl_sec, note)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(topic) DO UPDATE SET
             holder   = excluded.holder,
             taken_at = excluded.taken_at,
             ttl_sec  = excluded.ttl_sec,
             note     = excluded.note",
        params![topic, holder.as_str(), now, ttl_sec, note],
    )?;

    let mut notified_id = None;
    if notify {
        if let Some(ev) = evicted {
            // Свій же протермінований замок — перехоплення формально є, але
            // писати самому собі не можна (і нема кого сповіщати).
            if ev.holder != holder {
                notified_id = Some(post_with_conn(
                    conn,
                    Envelope {
                        v: ENVELOPE_V,
                        from: holder,
                        to: ev.holder,
                        topic: topic.to_string(),
                        op: Op::N,
                        body: eviction_body(topic, &ev, holder, now),
                    },
                )?);
            }
        }
    }

    Ok(LockOutcome {
        evicted,
        notified_id,
    })
}

impl Store {
    pub fn open(path: &Path) -> Result<Self, Error> {
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_millis(5000))?;
        let mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(Error::NotWal(mode));
        }
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        // Спершу версія, потім будь-який запис: на чужій, новішій схемі
        // не можна ні створювати таблиці, ні читати.
        let found = read_user_version(&conn)?;
        if found > SCHEMA_VERSION {
            return Err(Error::SchemaTooNew {
                found,
                known: SCHEMA_VERSION,
            });
        }
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts_unix INTEGER NOT NULL,
                v INTEGER NOT NULL,
                from_agent TEXT NOT NULL,
                to_agent TEXT NOT NULL,
                topic TEXT NOT NULL,
                op TEXT NOT NULL,
                body TEXT NOT NULL,
                read_at INTEGER
            );
            CREATE TABLE IF NOT EXISTS locks (
                topic TEXT PRIMARY KEY,
                holder TEXT NOT NULL,
                taken_at INTEGER NOT NULL,
                ttl_sec INTEGER NOT NULL,
                note TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_messages_inbox
                ON messages (to_agent, read_at, id);
            "#,
        )?;
        if found != SCHEMA_VERSION {
            migrate(&mut conn)?;
        }
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> Result<MutexGuard<'_, Connection>, Error> {
        self.conn.lock().map_err(|_| Error::Poisoned)
    }

    /// Версія схеми, записана в базі (`PRAGMA user_version`).
    pub fn schema_version(&self) -> Result<i64, Error> {
        let conn = self.conn()?;
        read_user_version(&conn)
    }

    /// Записати повідомлення. Тонка обгортка над [`post_with_conn`]:
    /// бере гард рівно один раз і більше жодного методу `Store` не кличе.
    pub fn post(&self, env: Envelope) -> Result<i64, Error> {
        let conn = self.conn()?;
        post_with_conn(&conn, env)
    }

    /// Скринька агента: усе, що адресоване йому або `Both`, найстарші першими.
    ///
    /// Делегат до [`Store::inbox_ex`] з дефолтним запитом — поведінка та сама,
    /// що й була. Для фільтрів беріть [`Store::inbox_ex`].
    pub fn inbox(&self, agent: Agent, unread_only: bool) -> Result<Vec<Message>, Error> {
        self.inbox_ex(InboxQuery {
            agent,
            unread_only,
            ..InboxQuery::default()
        })
    }

    /// Скринька з фільтрами — див. [`InboxQuery`].
    ///
    /// Порядок фільтрів важливий і саме такий: спершу відсіюється зайве
    /// (`exclude_own`, `topic`), і лише потім працює `limit`. Інакше «дай
    /// 5 останніх по темі» віддавало б 5 останніх узагалі, з яких по темі
    /// лишився б один, — і викликач вирішив би, що більше нічого немає.
    /// `brief` — останнім: він міняє тіла, а не склад вибірки.
    pub fn inbox_ex(&self, q: InboxQuery) -> Result<Vec<Message>, Error> {
        let mut out = {
            let conn = self.conn()?;
            Self::read_inbox(&conn, q.agent, q.unread_only)?
        };

        if q.exclude_own {
            out.retain(|m| m.envelope.from != q.agent);
        }
        if let Some(topic) = q.topic.as_deref() {
            let want = normalize_topic(topic);
            // Теми повідомлень лягають у БД сирими (на відміну від замків),
            // тому нормалізуються обидва боки — тут, а не в SQL.
            out.retain(|m| normalize_topic(&m.envelope.topic) == want);
        }
        if let Some(limit) = q.limit {
            if out.len() > limit {
                // Найновіші — це хвіст: вибірка йде за зростанням id.
                out.drain(..out.len() - limit);
            }
        }
        if let Some(brief) = q.brief {
            for m in &mut out {
                let body = std::mem::replace(&mut m.envelope.body, serde_json::Value::Null);
                m.envelope.body = brief_body(body, brief);
            }
        }
        Ok(out)
    }

    /// Сира вибірка скриньки **на вже взятому** з'єднанні.
    ///
    /// Приймає `&Connection`, а не `&self`, з тієї ж причини, що й
    /// [`post_with_conn`]: `conn: Mutex<Connection>` не реентрантний, і метод,
    /// який брав би гард удруге, завис би тихо.
    fn read_inbox(
        conn: &Connection,
        agent: Agent,
        unread_only: bool,
    ) -> Result<Vec<Message>, Error> {
        let sql = if unread_only {
            "SELECT id, ts_unix, v, from_agent, to_agent, topic, op, body, read_at
             FROM messages
             WHERE (to_agent = ?1 OR to_agent = 'Both') AND read_at IS NULL
             ORDER BY id ASC"
        } else {
            "SELECT id, ts_unix, v, from_agent, to_agent, topic, op, body, read_at
             FROM messages
             WHERE (to_agent = ?1 OR to_agent = 'Both')
             ORDER BY id ASC"
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params![agent.as_str()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, u32>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, Option<i64>>(8)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, ts_unix, v, from, to, topic, op, body, read_at) = row?;
            out.push(Message {
                id,
                ts_unix,
                envelope: Envelope {
                    v,
                    from: Agent::parse(&from)?,
                    to: Agent::parse(&to)?,
                    topic,
                    op: Op::parse(&op)?,
                    body: serde_json::from_str(&body)?,
                },
                read_at,
            });
        }
        Ok(out)
    }

    pub fn ack(&self, id: i64) -> Result<(), Error> {
        let conn = self.conn()?;
        let n = conn.execute(
            "UPDATE messages SET read_at = ?1 WHERE id = ?2",
            params![now_unix(), id],
        )?;
        if n == 0 {
            return Err(Error::NotFound(id));
        }
        Ok(())
    }

    /// Підтвердити пачку повідомлень від імені `agent`.
    ///
    /// Позначаються лише ті, що адресовані цьому агентові (`to == agent` або
    /// `to == Both`) **і ще не прочитані**. Чужі, неіснуючі та вже прочитані
    /// просто не рахуються — жодної помилки: пачка не мусить падати цілком
    /// через один сторонній id, а «підтверджено 0» — це відповідь, а не збій.
    ///
    /// Повертає, **скільки насправді позначено**. Розбіжність із `ids.len()`
    /// — сигнал викликачеві: він просив більше, ніж мав право підтвердити.
    /// Саме тому тут число, а не `()`.
    ///
    /// [`Store::ack`] лишається як був: один id, без агента, `NotFound` на
    /// невідомий.
    pub fn ack_many(&self, ids: &[i64], agent: Agent) -> Result<usize, Error> {
        if ids.is_empty() {
            return Ok(0);
        }
        let mut conn = self.conn()?;
        let now = now_unix();
        // Одна транзакція: пачка або лягає цілком, або не лягає взагалі —
        // напівпідтверджена пачка після збою давала б число, якому не можна
        // вірити.
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut acked = 0usize;
        {
            let mut stmt = tx.prepare(
                "UPDATE messages SET read_at = ?1
                 WHERE id = ?2
                   AND (to_agent = ?3 OR to_agent = 'Both')
                   AND read_at IS NULL",
            )?;
            for id in ids {
                acked += stmt.execute(params![now, id, agent.as_str()])?;
            }
        }
        tx.commit()?;
        Ok(acked)
    }

    /// Тихе взяття замка — поведінка збережена байт-у-байт: протермінований
    /// чужий замок зникає без сліду й без сповіщення.
    ///
    /// Делегат до [`lock_inner`] з `notify = false`. Для нового коду беріть
    /// [`Store::lock_ex`]: він показує, кого саме витіснили.
    pub fn lock(&self, topic: &str, holder: Agent, ttl_sec: i64, note: &str) -> Result<(), Error> {
        if matches!(holder, Agent::Both) {
            return Err(Error::BothCannotLock);
        }
        let topic = normalize_topic(topic);
        let conn = self.conn()?;
        lock_inner(&conn, &topic, holder, ttl_sec, note, false)?;
        Ok(())
    }

    /// Гучне взяття замка.
    ///
    /// * тема вільна → `LockOutcome { evicted: None, notified_id: None }`;
    /// * тема під **живим** чужим замком → [`Error::LockHeld`], нічого не змінено;
    /// * тема під **протермінованим** замком → замок забирається,
    ///   `evicted` каже, хто його тримав і коли взяв, а колишньому тримачеві
    ///   лягає сповіщення (його `id` — у `notified_id`).
    ///
    /// Евікшн, взяття й сповіщення — одна `IMMEDIATE`-транзакція: або читач
    /// побачить нового тримача **разом** із поясненням у себе в inbox, або
    /// не побачить нічого.
    pub fn lock_ex(
        &self,
        topic: &str,
        holder: Agent,
        ttl_sec: i64,
        note: &str,
    ) -> Result<LockOutcome, Error> {
        if matches!(holder, Agent::Both) {
            return Err(Error::BothCannotLock);
        }
        let topic = normalize_topic(topic);
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let outcome = lock_inner(&tx, &topic, holder, ttl_sec, note, true)?;
        tx.commit()?;
        Ok(outcome)
    }

    /// Зняти власний замок. Делегат до [`Store::unlock_force`] без `force`.
    pub fn unlock(&self, topic: &str, holder: Agent) -> Result<(), Error> {
        self.unlock_force(topic, holder, false)
    }

    /// Зняти замок; чужий — лише за явного `force`.
    ///
    /// Без `force` знімається тільки замок, що належить `holder`; чужий дає
    /// [`Error::LockNotHeld`] (та сама відмова, що й була). З `force` знімається
    /// замок на тему незалежно від тримача — але це має бути свідома,
    /// написана руками дія, а не побічний ефект звичайного `unlock`.
    pub fn unlock_force(&self, topic: &str, holder: Agent, force: bool) -> Result<(), Error> {
        if matches!(holder, Agent::Both) {
            return Err(Error::BothCannotLock);
        }
        let topic = normalize_topic(topic);
        let topic = topic.as_str();
        let conn = self.conn()?;
        expire_locks(&conn, now_unix())?;
        let n = if force {
            conn.execute("DELETE FROM locks WHERE topic = ?1", params![topic])?
        } else {
            conn.execute(
                "DELETE FROM locks WHERE topic = ?1 AND holder = ?2",
                params![topic, holder.as_str()],
            )?
        };
        if n == 0 {
            return Err(Error::LockNotHeld {
                topic: topic.to_string(),
                holder,
            });
        }
        Ok(())
    }

    pub fn locks(&self) -> Result<Vec<Lock>, Error> {
        let conn = self.conn()?;
        expire_locks(&conn, now_unix())?;
        let mut stmt = conn.prepare(
            "SELECT topic, holder, taken_at, ttl_sec, note FROM locks ORDER BY topic ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (topic, holder, taken_at, ttl_sec, note) = row?;
            let holder = Agent::parse(&holder)?;
            if matches!(holder, Agent::Both) {
                return Err(Error::BothCannotLock);
            }
            out.push(Lock {
                topic,
                holder,
                taken_at,
                ttl_sec,
                note,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn tmp_store() -> (TempDir, Store) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("exchange.db");
        let store = Store::open(&path).unwrap();
        (dir, store)
    }

    /// Зістарити замок: зсунути `taken_at` на `secs` у минуле прямим SQL.
    ///
    /// Це заміна `thread::sleep` у тестах TTL: після клапу мінімальний TTL —
    /// 60 с, і чекати стільки в тесті неприпустимо. Зсув годинника бази дає
    /// той самий стан, що й реальне протермінування, за нуль секунд.
    fn age_lock(store: &Store, topic: &str, secs: i64) {
        let conn = store.conn().unwrap();
        let n = conn
            .execute(
                "UPDATE locks SET taken_at = taken_at - ?1 WHERE topic = ?2",
                params![secs, topic],
            )
            .unwrap();
        assert_eq!(n, 1, "немає замка «{topic}», щоб його зістарити");
    }

    fn env(from: Agent, to: Agent, op: Op, body: serde_json::Value) -> Envelope {
        Envelope {
            v: 1,
            from,
            to,
            topic: "xvid/core".into(),
            op,
            body,
        }
    }

    #[test]
    fn wal_mode() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("exchange.db");
        let store = Store::open(&path).unwrap();
        let conn = store.conn().unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert!(mode.eq_ignore_ascii_case("wal"), "journal_mode={mode}");
    }

    #[test]
    fn post_inbox_ack() {
        let (_tmp, store) = tmp_store();
        let id = store
            .post(env(Agent::Grok, Agent::Claude, Op::Q, json!({"q": "ping"})))
            .unwrap();
        assert!(id > 0);

        let unread = store.inbox(Agent::Claude, true).unwrap();
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].id, id);
        assert!(unread[0].read_at.is_none());
        assert_eq!(unread[0].envelope.v, 1);
        assert_eq!(unread[0].envelope.from, Agent::Grok);
        assert_eq!(unread[0].envelope.to, Agent::Claude);
        assert_eq!(unread[0].envelope.topic, "xvid/core");
        assert_eq!(unread[0].envelope.op, Op::Q);
        assert_eq!(unread[0].envelope.body, json!({"q": "ping"}));

        assert!(store.inbox(Agent::Grok, true).unwrap().is_empty());

        let id_both = store
            .post(env(Agent::Claude, Agent::Both, Op::N, json!({"n": "note"})))
            .unwrap();
        let grok = store.inbox(Agent::Grok, true).unwrap();
        assert_eq!(grok.len(), 1);
        assert_eq!(grok[0].id, id_both);
        assert_eq!(store.inbox(Agent::Claude, true).unwrap().len(), 2);
        assert_eq!(store.inbox(Agent::Both, true).unwrap().len(), 1);

        store.ack(id).unwrap();
        let unread = store.inbox(Agent::Claude, true).unwrap();
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].id, id_both);

        let all = store.inbox(Agent::Claude, false).unwrap();
        assert_eq!(all.len(), 2);
        assert!(all[0].read_at.is_some());
        assert!(all[1].read_at.is_none());

        let err = store.ack(999_999).unwrap_err();
        assert!(matches!(err, Error::NotFound(999_999)));
    }

    #[test]
    fn foreign_lock() {
        let (_tmp, store) = tmp_store();
        store
            .lock("xvid/core", Agent::Grok, DEFAULT_TTL_SEC, "працюю")
            .unwrap();

        let err = store
            .lock("xvid/core", Agent::Claude, DEFAULT_TTL_SEC, "теж")
            .unwrap_err();
        match err {
            Error::LockHeld { topic, holder } => {
                assert_eq!(topic, "xvid/core");
                assert_eq!(holder, Agent::Grok);
            }
            other => panic!("очікував LockHeld, отримав {other:?}"),
        }

        let err = store
            .lock("other", Agent::Both, DEFAULT_TTL_SEC, "ні")
            .unwrap_err();
        assert!(matches!(err, Error::BothCannotLock));

        store
            .lock("xvid/core", Agent::Grok, DEFAULT_TTL_SEC, "оновлено")
            .unwrap();
        let locks = store.locks().unwrap();
        assert_eq!(locks.len(), 1);
        assert_eq!(locks[0].topic, "xvid/core");
        assert_eq!(locks[0].holder, Agent::Grok);
        assert_eq!(locks[0].ttl_sec, DEFAULT_TTL_SEC);
        assert_eq!(locks[0].note, "оновлено");
        assert!(!matches!(locks[0].holder, Agent::Both));

        let err = store.unlock("xvid/core", Agent::Claude).unwrap_err();
        assert!(matches!(err, Error::LockNotHeld { .. }));

        store.unlock("xvid/core", Agent::Grok).unwrap();
        assert!(store.locks().unwrap().is_empty());
    }

    /// Раніше тест спирався на `ttl_sec = 1` + `sleep(2)`. Після клапу TTL
    /// такий замок живе щонайменше 60 с, тож старий тест був би вічно
    /// зеленим не по суті: замок просто не встигав протермінуватись, а
    /// `assert` на порожній список ловив би вже не TTL, а нічого.
    /// Тому годинник зсувається прямим SQL (`age_lock`), а не sleep(61).
    #[test]
    fn lock_ttl_expires() {
        let (_tmp, store) = tmp_store();

        store.lock("xvid/core", Agent::Grok, 1, "короткий").unwrap();
        let locks = store.locks().unwrap();
        assert_eq!(locks.len(), 1, "замок мав з'явитись");
        assert_eq!(locks[0].ttl_sec, MIN_TTL_SEC, "ttl=1 мав затиснутись до 60");

        // Ще живий: до межі лишилась секунда.
        age_lock(&store, "xvid/core", MIN_TTL_SEC - 1);
        assert_eq!(store.locks().unwrap().len(), 1, "замок помер зарано");

        // Перетнули межу — `locks()` має його прибрати.
        age_lock(&store, "xvid/core", 2);
        assert!(store.locks().unwrap().is_empty(), "замок не протермінувався");

        // Протермінований чужий замок не блокує нового власника.
        store.lock("xvid/core", Agent::Grok, 1, "знову").unwrap();
        age_lock(&store, "xvid/core", MIN_TTL_SEC + 1);
        store
            .lock("xvid/core", Agent::Claude, DEFAULT_TTL_SEC, "мій")
            .unwrap();
        let locks = store.locks().unwrap();
        assert_eq!(locks.len(), 1);
        assert_eq!(locks[0].holder, Agent::Claude);
        assert_eq!(locks[0].note, "мій");
        assert_eq!(locks[0].ttl_sec, DEFAULT_TTL_SEC);
    }

    #[test]
    fn schema_version_stamped_on_new_db() {
        let (_tmp, store) = tmp_store();
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    }

    /// Повторне відкриття не має ні падати, ні збивати версію; і два
    /// одночасні `Store::open` на ту саму базу (реальний випадок: `render`)
    /// теж мають ужитись — це перевірка каркаса міграцій під write-lock.
    #[test]
    fn reopen_and_second_connection_keep_version() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("exchange.db");

        let first = Store::open(&path).unwrap();
        assert_eq!(first.schema_version().unwrap(), SCHEMA_VERSION);

        let second = Store::open(&path).unwrap();
        assert_eq!(second.schema_version().unwrap(), SCHEMA_VERSION);

        first.lock("alpha", Agent::Grok, 90, "hold").unwrap();
        assert_eq!(second.locks().unwrap().len(), 1);
    }

    #[test]
    fn refuses_newer_schema() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("exchange.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 7))
                .unwrap();
        }

        // `Store` не реалізує Debug, тож `unwrap_err()` тут недоступний.
        let open_err = || match Store::open(&path) {
            Ok(_) => panic!("відкрив базу з новішою схемою"),
            Err(e) => e,
        };

        match open_err() {
            Error::SchemaTooNew { found, known } => {
                assert_eq!(found, SCHEMA_VERSION + 7);
                assert_eq!(known, SCHEMA_VERSION);
            }
            other => panic!("очікував SchemaTooNew, отримав {other:?}"),
        }
        // Текст має пояснювати, що робити, а не бути кодом помилки.
        let text = open_err().to_string();
        assert!(text.contains("схема БД"), "{text}");
        assert!(text.contains("Оновіть бінарник"), "{text}");

        // Відмова не мала нічого створити на чужій схемі.
        let conn = Connection::open(&path).unwrap();
        let tables: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 0, "на новішій схемі створено {tables} таблиць");
    }

    #[test]
    fn topic_is_normalized() {
        assert_eq!(normalize_topic("  XVid / Core  "), "xvid / core");
        assert_eq!(normalize_topic("xvid / core"), "xvid / core");
        assert_eq!(normalize_topic("XVid\t/\n\nCore"), "xvid / core");
        assert_eq!(normalize_topic("xvid/core"), "xvid/core");
        assert_eq!(normalize_topic("  Ядро   ГРАФА "), "ядро графа");
    }

    /// «  XVid / Core  » і «xvid / core» — той самий замок.
    #[test]
    fn lock_matches_regardless_of_case_and_spaces() {
        let (_tmp, store) = tmp_store();
        store
            .lock("  XVid / Core  ", Agent::Grok, DEFAULT_TTL_SEC, "працюю")
            .unwrap();

        let locks = store.locks().unwrap();
        assert_eq!(locks.len(), 1);
        assert_eq!(locks[0].topic, "xvid / core", "у БД лягла сира тема");

        // Той самий замок очима іншого агента — конфлікт, а не другий рядок.
        let err = store
            .lock("xvid / core", Agent::Claude, DEFAULT_TTL_SEC, "теж")
            .unwrap_err();
        match err {
            Error::LockHeld { topic, holder } => {
                assert_eq!(topic, "xvid / core");
                assert_eq!(holder, Agent::Grok);
            }
            other => panic!("очікував LockHeld, отримав {other:?}"),
        }

        // Той самий власник у третьому написанні — продовження, не другий рядок.
        store
            .lock("XVID  /  CORE", Agent::Grok, DEFAULT_TTL_SEC, "оновив")
            .unwrap();
        let locks = store.locks().unwrap();
        assert_eq!(locks.len(), 1);
        assert_eq!(locks[0].note, "оновив");

        // Знімається теж у будь-якому написанні.
        store.unlock("  Xvid /   Core ", Agent::Grok).unwrap();
        assert!(store.locks().unwrap().is_empty());
    }

    #[test]
    fn ttl_is_clamped() {
        let (_tmp, store) = tmp_store();

        store.lock("t", Agent::Grok, 1, "нижче межі").unwrap();
        assert_eq!(store.locks().unwrap()[0].ttl_sec, 60);

        store.lock("t", Agent::Grok, 999_999, "вище межі").unwrap();
        assert_eq!(store.locks().unwrap()[0].ttl_sec, 86_400);

        store.lock("t", Agent::Grok, MIN_TTL_SEC, "рівно нижня").unwrap();
        assert_eq!(store.locks().unwrap()[0].ttl_sec, MIN_TTL_SEC);

        store.lock("t", Agent::Grok, MAX_TTL_SEC, "рівно верхня").unwrap();
        assert_eq!(store.locks().unwrap()[0].ttl_sec, MAX_TTL_SEC);

        store.lock("t", Agent::Grok, 3600, "усередині").unwrap();
        assert_eq!(store.locks().unwrap()[0].ttl_sec, 3600);

        // 0 і від'ємне — «за замовчуванням», поведінка не змінилась.
        store.lock("t", Agent::Grok, 0, "нуль").unwrap();
        assert_eq!(store.locks().unwrap()[0].ttl_sec, DEFAULT_TTL_SEC);
        store.lock("t", Agent::Grok, -5, "мінус").unwrap();
        assert_eq!(store.locks().unwrap()[0].ttl_sec, DEFAULT_TTL_SEC);
    }

    /// Тіло рівно `n` символів серіалізованого JSON.
    /// `{"t":""}` — 8 символів каркаса, решта йде в рядок.
    fn body_of_len(n: usize) -> serde_json::Value {
        let overhead = serde_json::to_string(&json!({"t": ""})).unwrap().len();
        let body = json!({ "t": "a".repeat(n - overhead) });
        assert_eq!(
            serde_json::to_string(&body).unwrap().chars().count(),
            n,
            "тест сам собі збрехав про довжину тіла"
        );
        body
    }

    /// Протермінований замок перехоплюється **гучно**: видно, кого витіснили,
    /// і той дізнається про це зі свого inbox.
    #[test]
    fn expired_lock_is_evicted_loudly() {
        let (_tmp, store) = tmp_store();
        store
            .lock("xvid/core", Agent::Grok, MIN_TTL_SEC, "працюю")
            .unwrap();
        let taken_at = store.locks().unwrap()[0].taken_at;
        age_lock(&store, "xvid/core", MIN_TTL_SEC + 1);

        let outcome = store
            .lock_ex("xvid/core", Agent::Claude, DEFAULT_TTL_SEC, "мій")
            .unwrap();

        let evicted = outcome.evicted.expect("евікшн мав бути видимим");
        assert_eq!(evicted.holder, Agent::Grok);
        assert_eq!(
            evicted.taken_at,
            taken_at - (MIN_TTL_SEC + 1),
            "taken_at має бути моментом, коли Grok узяв замок"
        );

        // Тема справді перейшла.
        let locks = store.locks().unwrap();
        assert_eq!(locks.len(), 1);
        assert_eq!(locks[0].holder, Agent::Claude);
        assert_eq!(locks[0].note, "мій");

        // І колишній тримач про це дізнався — саме в inbox, а не «десь у логах».
        let id = outcome.notified_id.expect("сповіщення не надіслано");
        let inbox = store.inbox(Agent::Grok, true).unwrap();
        assert_eq!(inbox.len(), 1, "у Grok має бути рівно одне сповіщення");
        assert_eq!(inbox[0].id, id);
        assert_eq!(inbox[0].envelope.from, Agent::Claude);
        assert_eq!(inbox[0].envelope.to, Agent::Grok);
        assert_eq!(inbox[0].envelope.topic, "xvid/core");
        assert_eq!(inbox[0].envelope.op, Op::N);
        assert_eq!(inbox[0].envelope.body["попередній_тримач"], "Grok");
        assert_eq!(inbox[0].envelope.body["новий_тримач"], "Claude");
        assert_eq!(inbox[0].envelope.body["взято_о"], evicted.taken_at);
    }

    /// Живий чужий замок не віддається — і жодного сповіщення при цьому.
    #[test]
    fn live_foreign_lock_is_not_taken_by_lock_ex() {
        let (_tmp, store) = tmp_store();
        store
            .lock("xvid/core", Agent::Grok, DEFAULT_TTL_SEC, "працюю")
            .unwrap();

        let err = store
            .lock_ex("xvid/core", Agent::Claude, DEFAULT_TTL_SEC, "теж")
            .unwrap_err();
        match err {
            Error::LockHeld { topic, holder } => {
                assert_eq!(topic, "xvid/core");
                assert_eq!(holder, Agent::Grok);
            }
            other => panic!("очікував LockHeld, отримав {other:?}"),
        }

        let locks = store.locks().unwrap();
        assert_eq!(locks[0].holder, Agent::Grok, "замок віддали живим");
        assert_eq!(locks[0].note, "працюю");
        assert!(
            store.inbox(Agent::Grok, true).unwrap().is_empty(),
            "відмова не сміє нікого сповіщати"
        );

        // Вільна тема — теж без евікшна.
        let outcome = store
            .lock_ex("інша тема", Agent::Claude, DEFAULT_TTL_SEC, "нова")
            .unwrap();
        assert_eq!(outcome.evicted, None);
        assert_eq!(outcome.notified_id, None);

        // Продовження власного живого замка — теж не евікшн.
        let outcome = store
            .lock_ex("xvid/core", Agent::Grok, DEFAULT_TTL_SEC, "далі")
            .unwrap();
        assert_eq!(outcome.evicted, None);
        assert_eq!(outcome.notified_id, None);
    }

    /// Найгостріший ризик зрізу: `lock_ex` тримає гард `Mutex<Connection>` і
    /// всередині пише повідомлення. Якби воно йшло через `Store::post`, той
    /// спробував би взяти той самий нереентрантний мютекс — і виклик завис би
    /// назавжди. Тест ловить саме зависання, не помилку.
    #[test]
    fn evicting_lock_does_not_deadlock() {
        use std::sync::mpsc;
        use std::sync::Arc;

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("exchange.db");
        let store = Store::open(&path).unwrap();
        store
            .lock("xvid/core", Agent::Grok, MIN_TTL_SEC, "старий")
            .unwrap();
        age_lock(&store, "xvid/core", MIN_TTL_SEC + 1);

        let store = Arc::new(store);
        let worker = Arc::clone(&store);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let notified = worker
                .lock_ex("xvid/core", Agent::Claude, DEFAULT_TTL_SEC, "мій")
                .map(|o| o.notified_id);
            let _ = tx.send(notified);
        });

        let notified = rx
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("lock_ex завис: авто-post узяв той самий нереентрантний мютекс")
            .expect("lock_ex повернув помилку");
        assert!(notified.is_some(), "перехоплення пройшло без сповіщення");
    }

    /// Чужий замок знімається лише явним `force`.
    #[test]
    fn foreign_unlock_needs_force() {
        let (_tmp, store) = tmp_store();
        store
            .lock("xvid/core", Agent::Grok, DEFAULT_TTL_SEC, "працюю")
            .unwrap();

        let err = store
            .unlock_force("xvid/core", Agent::Claude, false)
            .unwrap_err();
        assert!(matches!(err, Error::LockNotHeld { .. }));
        assert_eq!(store.locks().unwrap().len(), 1, "замок зняли без force");

        store
            .unlock_force("xvid/core", Agent::Claude, true)
            .unwrap();
        assert!(store.locks().unwrap().is_empty(), "force не зняв замок");

        // Немає замка взагалі — force не вигадує успіх.
        let err = store
            .unlock_force("xvid/core", Agent::Claude, true)
            .unwrap_err();
        assert!(matches!(err, Error::LockNotHeld { .. }));

        // Власний замок знімається і без force — стара поведінка ціла.
        store
            .lock("xvid/core", Agent::Claude, DEFAULT_TTL_SEC, "мій")
            .unwrap();
        store
            .unlock_force("xvid/core", Agent::Claude, false)
            .unwrap();
        assert!(store.locks().unwrap().is_empty());
    }

    /// Стеля тіла — помилка, не тихе обрізання.
    #[test]
    fn body_over_the_limit_is_an_error() {
        let (_tmp, store) = tmp_store();

        let ok = body_of_len(MAX_BODY_CHARS);
        let id = store
            .post(env(Agent::Grok, Agent::Claude, Op::N, ok))
            .unwrap();
        assert!(id > 0, "рівно 2000 символів мали пройти");

        let too_long = body_of_len(MAX_BODY_CHARS + 1);
        let err = store
            .post(env(Agent::Grok, Agent::Claude, Op::N, too_long))
            .unwrap_err();
        match err {
            Error::BodyTooLong { chars, max } => {
                assert_eq!(chars, MAX_BODY_CHARS + 1);
                assert_eq!(max, MAX_BODY_CHARS);
            }
            other => panic!("очікував BodyTooLong, отримав {other:?}"),
        }

        // Нічого не записано й не обрізано.
        assert_eq!(store.inbox(Agent::Claude, false).unwrap().len(), 1);
    }

    /// `to` не може дорівнювати `from` — але тільки буквально.
    #[test]
    fn self_addressed_message_is_rejected() {
        let (_tmp, store) = tmp_store();

        let err = store
            .post(env(Agent::Grok, Agent::Grok, Op::N, json!({"n": "сам собі"})))
            .unwrap_err();
        assert!(matches!(err, Error::SelfMessage(Agent::Grok)), "{err:?}");

        let err = store
            .post(env(Agent::Claude, Agent::Claude, Op::N, json!({})))
            .unwrap_err();
        assert!(matches!(err, Error::SelfMessage(Agent::Claude)), "{err:?}");

        // Both→Both — теж буквальна рівність.
        let err = store
            .post(env(Agent::Both, Agent::Both, Op::N, json!({})))
            .unwrap_err();
        assert!(matches!(err, Error::SelfMessage(Agent::Both)), "{err:?}");

        // А розсилка від конкретного агента — легальна (рішення №14),
        // навіть якщо він побачить її і в себе.
        let id = store
            .post(env(Agent::Grok, Agent::Both, Op::N, json!({"n": "усім"})))
            .unwrap();
        assert!(id > 0);
        assert_eq!(store.inbox(Agent::Grok, true).unwrap().len(), 1);
        assert_eq!(store.inbox(Agent::Claude, true).unwrap().len(), 1);
    }

    /// Те саме, що [`env`], але з довільною темою: фільтр `topic` треба
    /// перевіряти на різних написаннях, а не на одному «xvid/core».
    fn env_topic(from: Agent, to: Agent, topic: &str, body: serde_json::Value) -> Envelope {
        Envelope {
            v: 1,
            from,
            to,
            topic: topic.into(),
            op: Op::N,
            body,
        }
    }

    /// Чужий і неіснуючий id — «підтверджено 0», без винятку.
    #[test]
    fn ack_many_ignores_foreign_and_missing() {
        let (_tmp, store) = tmp_store();
        let to_claude = store
            .post(env(Agent::Grok, Agent::Claude, Op::Q, json!({"q": "ping"})))
            .unwrap();

        // Grok підтверджує чуже повідомлення й неіснуючий id.
        assert_eq!(store.ack_many(&[to_claude], Agent::Grok).unwrap(), 0);
        assert_eq!(store.ack_many(&[999_999], Agent::Grok).unwrap(), 0);
        assert_eq!(
            store.ack_many(&[to_claude, 999_999], Agent::Grok).unwrap(),
            0,
            "жоден id не мав зарахуватись"
        );
        assert_eq!(store.ack_many(&[], Agent::Claude).unwrap(), 0);

        // І чуже повідомлення лишилось непрочитаним у справжнього адресата.
        let unread = store.inbox(Agent::Claude, true).unwrap();
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].id, to_claude);
    }

    /// Два свої з трьох → 2; третє (чуже) лишається непрочитаним.
    /// Повторний виклик тих самих → 0.
    #[test]
    fn ack_many_counts_only_what_it_marked() {
        let (_tmp, store) = tmp_store();
        let a = store
            .post(env(Agent::Grok, Agent::Claude, Op::Q, json!({"n": 1})))
            .unwrap();
        let b = store
            .post(env(Agent::Grok, Agent::Both, Op::N, json!({"n": 2})))
            .unwrap();
        let c = store
            .post(env(Agent::Claude, Agent::Grok, Op::N, json!({"n": 3})))
            .unwrap();

        assert_eq!(
            store.ack_many(&[a, b, c], Agent::Claude).unwrap(),
            2,
            "Claude мав підтвердити рівно свої два"
        );

        // Третє — Grokове, воно ціле й непрочитане.
        let grok_unread = store.inbox(Agent::Grok, true).unwrap();
        assert_eq!(grok_unread.len(), 1, "у Grok мало лишитись одне непрочитане");
        assert_eq!(grok_unread[0].id, c);

        // У Claude непрочитаних не лишилось.
        assert!(store.inbox(Agent::Claude, true).unwrap().is_empty());

        // Повторне підтвердження вже прочитаних — 0, і теж без помилки.
        assert_eq!(store.ack_many(&[a, b], Agent::Claude).unwrap(), 0);
        assert_eq!(store.ack_many(&[a, b, c], Agent::Claude).unwrap(), 0);

        // Grok свій id підтверджує сам — і рівно один раз.
        assert_eq!(store.ack_many(&[c], Agent::Grok).unwrap(), 1);
        assert_eq!(store.ack_many(&[c], Agent::Grok).unwrap(), 0);
    }

    /// `limit` віддає саме найновіші, у тому ж порядку (найстаріші першими).
    #[test]
    fn inbox_ex_limit_returns_newest() {
        let (_tmp, store) = tmp_store();
        let mut ids = Vec::new();
        for n in 1..=5 {
            ids.push(
                store
                    .post(env(Agent::Grok, Agent::Claude, Op::N, json!({ "n": n })))
                    .unwrap(),
            );
        }

        let got = store
            .inbox_ex(InboxQuery {
                agent: Agent::Claude,
                limit: Some(2),
                ..InboxQuery::default()
            })
            .unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, ids[3]);
        assert_eq!(got[1].id, ids[4]);
        assert_eq!(got[0].envelope.body, json!({"n": 4}));
        assert_eq!(got[1].envelope.body, json!({"n": 5}));

        // Ліміт більший за наявне — усі п'ять, нічого не вигадано.
        let all = store
            .inbox_ex(InboxQuery {
                agent: Agent::Claude,
                limit: Some(99),
                ..InboxQuery::default()
            })
            .unwrap();
        assert_eq!(all.len(), 5);

        // Нуль — це нуль, а не «усі».
        let none = store
            .inbox_ex(InboxQuery {
                agent: Agent::Claude,
                limit: Some(0),
                ..InboxQuery::default()
            })
            .unwrap();
        assert!(none.is_empty());
    }

    /// `brief` ріже й лишає видиму позначку; без нього тіло ціле.
    #[test]
    fn inbox_ex_brief_is_visible() {
        let (_tmp, store) = tmp_store();
        let full = json!({ "t": "a".repeat(50) });
        store
            .post(env(Agent::Grok, Agent::Claude, Op::N, full.clone()))
            .unwrap();

        // Без brief — тіло ціле, байт-у-байт.
        let whole = store.inbox(Agent::Claude, false).unwrap();
        assert_eq!(whole[0].envelope.body, full);

        let cut = store
            .inbox_ex(InboxQuery {
                agent: Agent::Claude,
                brief: Some(10),
                ..InboxQuery::default()
            })
            .unwrap();
        let text = cut[0].envelope.body.as_str().expect("обрізане тіло — рядок");
        assert!(
            text.ends_with(BRIEF_MARK),
            "обрізання не лишило позначки: {text}"
        );
        assert_eq!(
            text.chars().count(),
            10 + BRIEF_MARK.chars().count(),
            "мало лишитись рівно 10 символів плюс позначка: {text}"
        );
        assert_ne!(cut[0].envelope.body, full);

        // Тіло, що вміщується у стелю, позначки не отримує.
        let short = store
            .inbox_ex(InboxQuery {
                agent: Agent::Claude,
                brief: Some(MAX_BODY_CHARS),
                ..InboxQuery::default()
            })
            .unwrap();
        assert_eq!(short[0].envelope.body, full, "ціле тіло не мало змінитись");

        // Рядкове тіло ріжеться як рядок, не як JSON із лапками.
        store
            .post(env(
                Agent::Grok,
                Agent::Claude,
                Op::N,
                json!("б".repeat(30)),
            ))
            .unwrap();
        let cut = store
            .inbox_ex(InboxQuery {
                agent: Agent::Claude,
                limit: Some(1),
                brief: Some(5),
                ..InboxQuery::default()
            })
            .unwrap();
        assert_eq!(cut.len(), 1);
        assert_eq!(cut[0].envelope.body, json!(format!("ббббб{BRIEF_MARK}")));
    }

    /// Фільтр `topic` звіряється після [`normalize_topic`]: регістр і зайві
    /// пробіли не мають значення, слеш — має.
    #[test]
    fn inbox_ex_filters_by_normalized_topic() {
        let (_tmp, store) = tmp_store();
        // Нормалізується в «xvid core».
        let spaced = store
            .post(env_topic(
                Agent::Grok,
                Agent::Claude,
                "XVid  Core",
                json!({"n": "пробіли"}),
            ))
            .unwrap();
        let plain = store
            .post(env_topic(
                Agent::Grok,
                Agent::Claude,
                "xvid core",
                json!({"n": "як є"}),
            ))
            .unwrap();
        // А це вже інша тема: слеш нормалізація не чіпає.
        store
            .post(env_topic(
                Agent::Grok,
                Agent::Claude,
                "xvid/core",
                json!({"n": "слеш"}),
            ))
            .unwrap();

        // Перевірка припущення тесту про саму нормалізацію.
        assert_eq!(normalize_topic("XVid  Core"), "xvid core");
        assert_eq!(normalize_topic("xvid core"), "xvid core");
        assert_ne!(normalize_topic("xvid/core"), "xvid core");

        let got = store
            .inbox_ex(InboxQuery {
                agent: Agent::Claude,
                topic: Some("  xVid   CORE ".into()),
                ..InboxQuery::default()
            })
            .unwrap();
        assert_eq!(got.len(), 2, "мали збігтись обидва написання «xvid core»");
        assert_eq!(got[0].id, spaced);
        assert_eq!(got[1].id, plain);

        let slash = store
            .inbox_ex(InboxQuery {
                agent: Agent::Claude,
                topic: Some("XVID/Core".into()),
                ..InboxQuery::default()
            })
            .unwrap();
        assert_eq!(slash.len(), 1);
        assert_eq!(slash[0].envelope.body, json!({"n": "слеш"}));

        // Теми, якої немає, — порожньо, а не «усе».
        let empty = store
            .inbox_ex(InboxQuery {
                agent: Agent::Claude,
                topic: Some("нема такої".into()),
                ..InboxQuery::default()
            })
            .unwrap();
        assert!(empty.is_empty());
    }

    /// `exclude_own` вимикається за замовчуванням: власна розсилка `to = Both`
    /// видно у своїй же скриньці, як і було.
    #[test]
    fn inbox_ex_exclude_own_is_opt_in() {
        let (_tmp, store) = tmp_store();
        let own = store
            .post(env(Agent::Grok, Agent::Both, Op::N, json!({"n": "усім"})))
            .unwrap();
        let foreign = store
            .post(env(Agent::Claude, Agent::Grok, Op::N, json!({"n": "тобі"})))
            .unwrap();

        let default = store
            .inbox_ex(InboxQuery {
                agent: Agent::Grok,
                ..InboxQuery::default()
            })
            .unwrap();
        assert_eq!(default.len(), 2, "дефолт не сміє нічого ховати");
        assert_eq!(default[0].id, own);

        let without_own = store
            .inbox_ex(InboxQuery {
                agent: Agent::Grok,
                exclude_own: true,
                ..InboxQuery::default()
            })
            .unwrap();
        assert_eq!(without_own.len(), 1);
        assert_eq!(without_own[0].id, foreign);
    }

    /// `InboxQuery::default()` має бути тотожним `inbox(agent, false)` —
    /// інакше `render` і `mcp`, які кличуть старий `inbox`, тихо змінили б
    /// поведінку разом із появою структури.
    #[test]
    fn inbox_ex_default_equals_inbox() {
        let (_tmp, store) = tmp_store();
        store
            .post(env(Agent::Grok, Agent::Claude, Op::Q, json!({"n": 1})))
            .unwrap();
        let both = store
            .post(env(Agent::Grok, Agent::Both, Op::N, json!({"n": 2})))
            .unwrap();
        store
            .post(env(Agent::Claude, Agent::Grok, Op::A, json!({"n": 3})))
            .unwrap();
        store.ack_many(&[both], Agent::Claude).unwrap();

        // Дефолт запиту нікому не приписує чужої особи.
        assert_eq!(Agent::default(), Agent::Both);

        for agent in [Agent::Grok, Agent::Claude, Agent::Both] {
            let old = store.inbox(agent, false).unwrap();
            let new = store
                .inbox_ex(InboxQuery {
                    agent,
                    ..InboxQuery::default()
                })
                .unwrap();
            assert_eq!(old, new, "дефолтний запит розійшовся з inbox({agent}, false)");

            let old_unread = store.inbox(agent, true).unwrap();
            let new_unread = store
                .inbox_ex(InboxQuery {
                    agent,
                    unread_only: true,
                    ..InboxQuery::default()
                })
                .unwrap();
            assert_eq!(old_unread, new_unread, "розбіжність на unread_only");
        }
    }

    #[test]
    fn post_rejects_bad_version() {
        let (_tmp, store) = tmp_store();
        let mut e = env(Agent::Grok, Agent::Claude, Op::A, json!({}));
        e.v = 2;
        let err = store.post(e).unwrap_err();
        assert!(matches!(err, Error::BadVersion(2)));
    }
}
