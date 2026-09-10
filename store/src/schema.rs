use crate::error::Error;
use crate::Store;
use rusqlite::Connection;
use std::path::Path;
use std::sync::Mutex;

/// Версія схеми БД, яку знає ця збірка. Пишеться в `PRAGMA user_version`.
///
/// | версія | що з'явилось |
/// |---|---|
/// | 1 | `messages` + `locks` + `idx_messages_inbox` — базова схема |
/// | 2 | широкомовна адреса `Both` перейменована на `*` |
/// | 3 | `messages.sig` і `messages.key_id` — місце під підпис |
pub const SCHEMA_VERSION: i64 = 3;

/// Один крок міграції: довести схему до версії `to`.
///
/// Кроки виконуються в порядку зростання `to` всередині однієї транзакції
/// разом із записом `user_version` — або все, або нічого.
struct Migration {
    to: i64,
    apply: fn(&Connection) -> Result<(), Error>,
}

/// Версія 1 — базова схема, яку створює `CREATE TABLE IF NOT EXISTS` у
/// [`Store::open`], тому окремого кроку `to: 1` немає.
///
/// Наступні зрізи дописують сюди рядки виду
/// `Migration { to: 3, apply: |c| { c.execute_batch("ALTER TABLE …")?; Ok(()) } }`
/// і піднімають [`SCHEMA_VERSION`].
const MIGRATIONS: &[Migration] = &[Migration {
    to: 2,
    apply: |conn| {
        // Широкомовна адреса перестала називатись числом учасників: сервер
        // має бути придатним для чужої пари ШІ, а `Both` має сенс лише поки
        // агентів рівно двоє.
        //
        // ⚠️ Зміст рядків не змінюється: у світі з двох агентів «усім» і
        // «обом» — те саме. Тому це перейменування, а не втрата адресації.
        //
        // Незворотно. Читання приймає обидва написання назавжди
        // (див. `messages::BROADCAST_LEGACY`), тож база, яку ніхто не
        // мігрував, лишається робочою.
        conn.execute(
            "UPDATE messages SET to_agent = ?1 WHERE to_agent = ?2",
            [crate::messages::BROADCAST, crate::messages::BROADCAST_LEGACY],
        )?;
        Ok(())
    },
}, Migration {
    to: 3,
    apply: |conn| {
        // Місце під підпис Ed25519. Порожнє: писати його починає наступний
        // зріз, а тут лише з'являються колонки.
        //
        // ⚠️ Обидві NULL-абельні, і це не тимчасово. Підпис вмикається
        // конфігом; без нього сервер працює як раніше, і рядки лишаються
        // без підпису назавжди. Зробити колонки обов'язковими означало б
        // вимагати ключів від кожного, хто просто хоче поштову скриньку.
        add_column_if_missing(conn, "messages", "sig")?;
        add_column_if_missing(conn, "messages", "key_id")?;
        Ok(())
    },
}];

/// Додати колонку, якщо її ще немає.
///
/// ⚠️ Перевірка обов'язкова, і не заради «про всяк випадок». Нова база
/// створюється з повної [`SCHEMA`] — тобто вже з цими колонками, — а потім
/// однаково проходить `migrate` з версії 0. Голий `ALTER TABLE ADD COLUMN`
/// на ній впав би з «duplicate column name», і **жодна нова база не
/// відкрилася б узагалі**.
///
/// Тип не задається: SQLite тримає колонки без строгого типу, а обидві
/// потрібні як текст або NULL.
fn add_column_if_missing(conn: &Connection, table: &str, column: &str) -> Result<(), Error> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let existing = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<String>, _>>()?;
    if existing.iter().any(|c| c == column) {
        return Ok(());
    }
    conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} TEXT"))?;
    Ok(())
}

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

/// Схема **поточної** версії: `messages` + `locks` + індекс.
///
/// ⚠️ Тримається в актуальному вигляді, а не на версії 1: нова база
/// створюється саме звідси. Тому кожна міграція, що додає колонку, мусить
/// бути ідемпотентною — див. [`add_column_if_missing`].
pub const SCHEMA: &str = r#"
            CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts_unix INTEGER NOT NULL,
                v INTEGER NOT NULL,
                from_agent TEXT NOT NULL,
                to_agent TEXT NOT NULL,
                topic TEXT NOT NULL,
                op TEXT NOT NULL,
                body TEXT NOT NULL,
                read_at INTEGER,
                -- Підпис Ed25519 у base64 і ім'я ключа, яким підписано.
                -- NULL — рядок без підпису: так виглядають усі записи до
                -- вмикання підпису й усі записи там, де його не вмикали.
                sig TEXT,
                key_id TEXT
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
            "#;

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
        conn.execute_batch(SCHEMA)?;
        if found != SCHEMA_VERSION {
            migrate(&mut conn)?;
        }
        Ok(Store {
            conn: Mutex::new(conn),
            key: None,
            // Перший post/ack на цьому з'єднанні зробить gc.
            last_gc_unix: std::sync::atomic::AtomicI64::new(0),
        })
    }

    /// Версія схеми, записана в базі (`PRAGMA user_version`).
    pub fn schema_version(&self) -> Result<i64, Error> {
        let conn = self.conn()?;
        read_user_version(&conn)
    }
}
