//! Читання бази обміну **лише на читання** і складання [`Snapshot`].
//!
//! Увесь сенс переглядача в тому, що він фізично не може зіпсувати обмін:
//! з'єднання відкривається з [`OpenFlags::SQLITE_OPEN_READ_ONLY`], тож навіть
//! помилка в SQL не перетворить його на другого писача. Тому тут немає
//! жодного `INSERT`/`UPDATE`, і не буде: база — чужа, її веде `store`.
//!
//! ⚠️ Схема тут повторена вручну (`messages`, `locks`), а не взята з `store`:
//! залежність на `store` притягла б `Store::open`, який відкриває базу **на
//! запис** і робить `PRAGMA journal_mode=WAL`, тобто пише. Ціна повтору —
//! ці два `SELECT`; ціна залежності — втрата єдиної гарантії крейта.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags};

use crate::page::{LockView, MessageView, Snapshot};

/// База обміну за замовчуванням, коли `EXCHANGE_DB` не задано.
pub const DEFAULT_DB: &str = r"C:\Users\Public\agent-board\exchange.db";

/// Файл дошки, чий mtime і є «останній render».
pub const DEFAULT_NOW_MD: &str = r"C:\Users\Public\agent-board\NOW.md";

/// Скільки останніх повідомлень показувати.
///
/// Не «усі»: журнал росте без межі, а сторінка на кілька тисяч рядків
/// перестає бути оглядом і починає бути дампом.
pub const MESSAGE_LIMIT: usize = 50;

/// Значення `to_agent`, яке означає «обом».
///
/// ⚠️ Рядок, а не enum: у базі це TEXT, і саме так його порівнює
/// `Store::inbox` (`to_agent = ?1 OR to_agent = 'Both'`).
pub const BOTH: &str = "Both";

/// Агенти, для яких рахуються непрочитані.
pub const GROK: &str = "Grok";
/// Див. [`GROK`].
pub const CLAUDE: &str = "Claude";

/// Чому не вдалось прочитати базу.
///
/// Варіанти розділені не заради краси: людина, яка відкрила сторінку,
/// має з тексту зрозуміти, що робити. «Файла немає» і «база в WAL, а писача
/// нема» лікуються по-різному, тож і повідомлення різні.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// Файла бази немає за вказаним шляхом.
    #[error(
        "файла бази немає: {path}\n\
         Перевірте змінну EXCHANGE_DB або вкажіть правильний шлях. \
         Переглядач сам бази не створює — він лише читає."
    )]
    Missing {
        /// Шлях, за яким шукали.
        path: String,
    },

    /// База є, але read-only з'єднання не може її відкрити через стан WAL.
    ///
    /// ⚠️ Це **не** лікується відкриттям на запис. З'єднання на читання не
    /// вміє ні відновити `-wal` після падіння писача, ні створити `-shm`;
    /// відкрити на запис означало б зробити переглядач другим писачем — і
    /// втратити єдину гарантію, заради якої він окремий процес.
    #[error(
        "базу {path} видно, але прочитати її зараз не можна: {detail}\n\
         Найімовірніша причина: база в режимі WAL, а сервер обміну не \
         запущено — з'єднання лише на читання не може відновити файл -wal \
         після падіння писача й не може створити -shm.\n\
         Що робити: (1) запустити сервер обміну — він відновить WAL, і \
         сторінка запрацює сама; або (2) скопіювати exchange.db разом із \
         exchange.db-wal і exchange.db-shm у окрему теку й запустити \
         переглядач на копії (EXCHANGE_DB=шлях_до_копії).\n\
         Переглядач навмисне не відкриває базу на запис: інакше він став би \
         другим писачем і міг би зіпсувати обмін."
    )]
    WalUnreadable {
        /// Шлях до бази.
        path: String,
        /// Дослівне повідомлення SQLite.
        detail: String,
    },

    /// Файл є і відкривається, але це не база обміну.
    #[error(
        "у базі {path} немає таблиці {table} — це не база обміну \
         (або її ще жодного разу не створював сервер обміну)."
    )]
    NoSchema {
        /// Шлях до бази.
        path: String,
        /// Якої таблиці бракує.
        table: String,
    },

    /// Решта помилок SQLite — віддаються як є, без тлумачення.
    #[error("не вдалось прочитати базу {path}: {source}")]
    Sqlite {
        /// Шлях до бази.
        path: String,
        /// Первинна помилка.
        #[source]
        source: rusqlite::Error,
    },
}

/// `SQLITE_READONLY_RECOVERY` — WAL потребує відновлення, а з'єднання лише
/// на читання цього не вміє.
const SQLITE_READONLY_RECOVERY: i32 = 8 | (1 << 8);
/// `SQLITE_READONLY_CANTINIT` — не вдалось створити `-shm`.
const SQLITE_READONLY_CANTINIT: i32 = 8 | (5 << 8);
/// `SQLITE_READONLY_DIRECTORY` — тека бази недоступна на запис, а WAL цього
/// вимагає навіть від читача.
const SQLITE_READONLY_DIRECTORY: i32 = 8 | (6 << 8);

/// Розкласти помилку SQLite на зрозумілу людині.
///
/// `path_exists` передається окремо: `SQLITE_CANTOPEN` однаково прилітає і
/// коли файла немає, і коли він є, але поруч непридатний `-wal`.
fn classify(err: rusqlite::Error, path: &Path, path_exists: bool) -> DbError {
    let shown = path.display().to_string();
    if let rusqlite::Error::SqliteFailure(inner, ref msg) = err {
        let detail = msg.clone().unwrap_or_else(|| inner.to_string());
        let wal = matches!(
            inner.extended_code,
            SQLITE_READONLY_RECOVERY | SQLITE_READONLY_CANTINIT | SQLITE_READONLY_DIRECTORY
        ) || (inner.code == rusqlite::ErrorCode::CannotOpen && path_exists);
        if wal {
            return DbError::WalUnreadable {
                path: shown,
                detail,
            };
        }
        if inner.code == rusqlite::ErrorCode::CannotOpen && !path_exists {
            return DbError::Missing { path: shown };
        }
    }
    DbError::Sqlite {
        path: shown,
        source: err,
    }
}

/// Відкрити базу **строго на читання**.
///
/// Прапорів рівно два: [`OpenFlags::SQLITE_OPEN_READ_ONLY`] і
/// `SQLITE_OPEN_NO_MUTEX`. Ні `CREATE`, ні `READ_WRITE` — тому неіснуючий
/// файл дає помилку, а не порожню нову базу, і жодна гілка коду не може
/// перетворити переглядач на писача.
///
/// Після відкриття робиться пробний `SELECT` по `sqlite_master`: сам
/// `sqlite3_open_v2` лінивий і на зламаному WAL мовчить до першого читання.
pub fn open_read_only(path: &Path) -> Result<Connection, DbError> {
    let exists = path.exists();
    if !exists {
        return Err(DbError::Missing {
            path: path.display().to_string(),
        });
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(path, flags).map_err(|e| classify(e, path, exists))?;
    // Перше справжнє читання: саме тут вилазить стан WAL.
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
        r.get::<_, i64>(0)
    })
    .map_err(|e| classify(e, path, exists))?;
    Ok(conn)
}

/// Чи є в базі таблиця з такою назвою.
fn has_table(conn: &Connection, table: &str) -> Result<bool, rusqlite::Error> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Усі замки, впорядковані за темою.
///
/// Впорядкування за `topic`, а не за часом: сторінка сама відрізняє
/// протерміновані від живих, а стабільний порядок означає, що рядки не
/// стрибають між оновленнями кожні 5 с.
fn read_locks(conn: &Connection, path: &Path) -> Result<Vec<LockView>, DbError> {
    let mut stmt = conn
        .prepare(
            "SELECT topic, holder, taken_at, ttl_sec, note
             FROM locks
             ORDER BY topic ASC",
        )
        .map_err(|e| classify(e, path, true))?;
    let rows = stmt
        .query_map([], |r| {
            Ok(LockView {
                topic: r.get(0)?,
                holder: r.get(1)?,
                // ⚠️ unix-СЕКУНДИ, як і все часове у схемі `store`.
                taken_at: r.get(2)?,
                ttl_sec: r.get(3)?,
                note: r.get(4)?,
            })
        })
        .map_err(|e| classify(e, path, true))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| classify(e, path, true))?);
    }
    Ok(out)
}

/// Останні [`MESSAGE_LIMIT`] повідомлень, найновіші першими.
///
/// `ORDER BY id DESC LIMIT n` — саме так, а не «прочитати все й обрізати»:
/// журнал росте без межі, і вибирати з бази мільйон рядків, щоб показати 50,
/// означало б платити за це кожні 5 с автооновлення.
fn read_messages(conn: &Connection, path: &Path) -> Result<Vec<MessageView>, DbError> {
    let mut stmt = conn
        .prepare(
            "SELECT id, ts_unix, from_agent, to_agent, topic, op, read_at
             FROM messages
             ORDER BY id DESC
             LIMIT ?1",
        )
        .map_err(|e| classify(e, path, true))?;
    let rows = stmt
        .query_map([MESSAGE_LIMIT as i64], |r| {
            let read_at: Option<i64> = r.get(6)?;
            Ok(MessageView {
                id: r.get(0)?,
                ts: r.get(1)?,
                from: r.get(2)?,
                to: r.get(3)?,
                topic: r.get(4)?,
                op: r.get(5)?,
                // `read_at` — час підтвердження; сторінці треба лише «чи є».
                read: read_at.is_some(),
            })
        })
        .map_err(|e| classify(e, path, true))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| classify(e, path, true))?);
    }
    Ok(out)
}

/// Скільки непрочитаних лежить у скриньці агента.
///
/// ⚠️ `Both` рахується **обом** — так само, як у `Store::inbox`
/// (`to_agent = ?1 OR to_agent = 'Both'`). Інакше сторінка показувала б
/// нулі там, де насправді лежить неотримане.
fn count_unread(conn: &Connection, path: &Path, agent: &str) -> Result<usize, DbError> {
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM messages
             WHERE (to_agent = ?1 OR to_agent = ?2) AND read_at IS NULL",
            [agent, BOTH],
            |r| r.get(0),
        )
        .map_err(|e| classify(e, path, true))?;
    Ok(n.max(0) as usize)
}

/// mtime файла в unix-секундах. `None`, якщо файла немає або час недоступний.
///
/// ⚠️ Читається лише `metadata` — сам файл не відкривається. `NOW.md` пише
/// `render`, і переглядачу нема чого тримати на ньому дескриптор.
pub fn file_mtime_unix(path: &Path) -> Option<i64> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    match modified.duration_since(UNIX_EPOCH) {
        Ok(d) => Some(d.as_secs() as i64),
        // Файл із часом до 1970 — екзотика, але не привід падати.
        Err(e) => Some(-(e.duration().as_secs() as i64)),
    }
}

/// Шлях до `NOW.md`: `EXCHANGE_NOW_MD`, інакше [`DEFAULT_NOW_MD`].
pub fn now_md_path() -> PathBuf {
    std::env::var_os("EXCHANGE_NOW_MD")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_NOW_MD))
}

/// Шлях до бази: `EXCHANGE_DB`, інакше [`DEFAULT_DB`].
pub fn db_path() -> PathBuf {
    std::env::var_os("EXCHANGE_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_DB))
}

/// Поточний час у unix-секундах.
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Скласти [`Snapshot`] із бази — свіже з'єднання на кожен виклик.
///
/// З'єднання не кешується навмисне: сторінка автооновлюється кожні 5 с, і
/// довгоживучий читач у WAL тримав би снапшот бази, не даючи писачеві
/// чекпоінтити `-wal`. Відкриття SQLite дешеве; заважати обміну — ні.
///
/// `now` передається, а не береться з годинника, з тієї ж причини, що і в
/// `page`: інакше жоден тест не був би відтворюваним.
pub fn read_snapshot(db: &Path, now: i64, now_md: &Path) -> Result<Snapshot, DbError> {
    let conn = open_read_only(db)?;
    let shown = db.display().to_string();

    for table in ["messages", "locks"] {
        let present = has_table(&conn, table).map_err(|e| classify(e, db, true))?;
        if !present {
            return Err(DbError::NoSchema {
                path: shown,
                table: table.to_string(),
            });
        }
    }

    Ok(Snapshot {
        now,
        locks: read_locks(&conn, db)?,
        messages: read_messages(&conn, db)?,
        unread_grok: count_unread(&conn, db, GROK)?,
        unread_claude: count_unread(&conn, db, CLAUDE)?,
        last_render: file_mtime_unix(now_md),
    })
}

/// Те саме, що [`read_snapshot`], але «зараз» береться з системного годинника.
///
/// Окремою функцією, щоб системний час був рівно в одному місці й не
/// просочився в те, що перевіряють тести.
pub fn read_snapshot_now(db: &Path, now_md: &Path) -> Result<Snapshot, DbError> {
    read_snapshot(db, now_unix(), now_md)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Лічильник для унікальних імен тек: тести йдуть паралельно в одному
    /// процесі, і `process::id()` сам по собі їх не розрізнить.
    static SEQ: AtomicUsize = AtomicUsize::new(0);

    /// Тимчасова тека, яка прибирає себе сама.
    ///
    /// Своя, а не `tempfile`: цей крейт свідомо тримає мінімум залежностей,
    /// а потрібні тут рівно `create_dir_all` і `remove_dir_all`.
    struct TmpDir {
        path: PathBuf,
    }

    impl TmpDir {
        fn new(tag: &str) -> TmpDir {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let mut path = std::env::temp_dir();
            path.push(format!(
                "exchange-ui-{tag}-{}-{n}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("тимчасова тека");
            TmpDir { path }
        }

        fn db(&self) -> PathBuf {
            self.path.join("exchange.db")
        }

        fn join(&self, name: &str) -> PathBuf {
            self.path.join(name)
        }
    }

    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// Схема повторена з `store/src/lib.rs` дослівно — це та база, яку
    /// переглядач читатиме насправді.
    const SCHEMA: &str = r#"
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
    "#;

    /// Створити тимчасову базу зі схемою обміну. Писач одразу закривається,
    /// тож read-only читач далі працює з чистим файлом.
    fn make_db(dir: &TmpDir) -> PathBuf {
        let path = dir.db();
        let conn = Connection::open(&path).expect("створення бази");
        conn.execute_batch(SCHEMA).expect("схема");
        drop(conn);
        path
    }

    fn writer(path: &Path) -> Connection {
        Connection::open(path).expect("писач")
    }

    fn insert_msg(
        conn: &Connection,
        ts: i64,
        from: &str,
        to: &str,
        topic: &str,
        op: &str,
        read_at: Option<i64>,
    ) {
        conn.execute(
            "INSERT INTO messages (ts_unix, v, from_agent, to_agent, topic, op, body, read_at)
             VALUES (?1, 1, ?2, ?3, ?4, ?5, '{}', ?6)",
            rusqlite::params![ts, from, to, topic, op, read_at],
        )
        .expect("вставка повідомлення");
    }

    fn insert_lock(conn: &Connection, topic: &str, holder: &str, taken_at: i64, ttl: i64) {
        conn.execute(
            "INSERT INTO locks (topic, holder, taken_at, ttl_sec, note)
             VALUES (?1, ?2, ?3, ?4, 'нотатка')",
            rusqlite::params![topic, holder, taken_at, ttl],
        )
        .expect("вставка замка");
    }

    #[test]
    fn empty_db_gives_zero_snapshot() {
        let dir = TmpDir::new("empty");
        let db = make_db(&dir);
        let snap = read_snapshot(&db, 1_000, &dir.join("NOW.md")).expect("снапшот");
        assert_eq!(snap.now, 1_000);
        assert!(snap.locks.is_empty());
        assert!(snap.messages.is_empty());
        assert_eq!(snap.unread_grok, 0);
        assert_eq!(snap.unread_claude, 0);
        assert_eq!(snap.last_render, None);
    }

    #[test]
    fn locks_map_with_expiry() {
        let dir = TmpDir::new("locks");
        let db = make_db(&dir);
        {
            let w = writer(&db);
            // Живий: узятий 100 с тому, живе 600 с.
            insert_lock(&w, "alpha", "Grok", 900, 600);
            // Протермінований: узятий 5000 с тому, жив 60 с.
            insert_lock(&w, "beta", "Claude", 5_000, 60);
        }
        let now = 10_000;
        let snap = read_snapshot(&db, now, &dir.join("NOW.md")).expect("снапшот");
        assert_eq!(snap.locks.len(), 2);

        let alpha = &snap.locks[0];
        assert_eq!(alpha.topic, "alpha");
        assert_eq!(alpha.holder, "Grok");
        assert_eq!(alpha.taken_at, 900);
        assert_eq!(alpha.ttl_sec, 600);
        assert_eq!(alpha.note, "нотатка");
        assert_eq!(alpha.expires_at(), 1_500);
        // На `now` він уже давно протермінований — перевіряємо арифметику.
        assert!(alpha.is_expired(now));
        assert!(!alpha.is_expired(1_400));
        assert_eq!(alpha.remaining(1_400), 100);

        let beta = &snap.locks[1];
        assert_eq!(beta.topic, "beta");
        assert_eq!(beta.holder, "Claude");
        assert_eq!(beta.expires_at(), 5_060);
        assert!(beta.is_expired(now));
        assert_eq!(beta.remaining(now), 5_060 - now);
    }

    #[test]
    fn both_message_counts_for_each_agent() {
        let dir = TmpDir::new("both");
        let db = make_db(&dir);
        {
            let w = writer(&db);
            insert_msg(&w, 10, "Grok", "Claude", "t", "n", None);
            insert_msg(&w, 20, "Claude", "Grok", "t", "n", None);
            // ⚠️ Це має потрапити в обидва лічильники.
            insert_msg(&w, 30, "Grok", BOTH, "t", "n", None);
            // Прочитане не рахується жодному.
            insert_msg(&w, 40, "Grok", BOTH, "t", "n", Some(41));
            insert_msg(&w, 50, "Claude", "Grok", "t", "n", Some(51));
        }
        let snap = read_snapshot(&db, 100, &dir.join("NOW.md")).expect("снапшот");
        // Claude: одне персональне + одне Both.
        assert_eq!(snap.unread_claude, 2);
        // Grok: одне персональне + те саме Both.
        assert_eq!(snap.unread_grok, 2);
        assert_eq!(snap.messages.len(), 5);
    }

    #[test]
    fn limit_returns_newest() {
        let dir = TmpDir::new("limit");
        let db = make_db(&dir);
        {
            let w = writer(&db);
            for n in 1..=(MESSAGE_LIMIT as i64 + 20) {
                insert_msg(&w, 1_000 + n, "Grok", "Claude", &format!("t{n}"), "n", None);
            }
        }
        let snap = read_snapshot(&db, 9_999, &dir.join("NOW.md")).expect("снапшот");
        assert_eq!(snap.messages.len(), MESSAGE_LIMIT);
        let total = MESSAGE_LIMIT as i64 + 20;
        // Найновіші — першими; найстаріший у вибірці має бути рівно
        // `total - LIMIT + 1`, тобто перші 20 відрізані.
        assert_eq!(snap.messages[0].id, total);
        assert_eq!(snap.messages[0].topic, format!("t{total}"));
        let oldest = snap.messages.last().unwrap();
        assert_eq!(oldest.id, total - MESSAGE_LIMIT as i64 + 1);
        assert!(snap.messages.iter().all(|m| m.id > 20));
    }

    #[test]
    fn missing_db_file_gives_clear_error() {
        let dir = TmpDir::new("missing");
        let db = dir.join("немає.db");
        let err = read_snapshot(&db, 1, &dir.join("NOW.md")).expect_err("має бути помилка");
        assert!(matches!(err, DbError::Missing { .. }), "{err:?}");
        let text = err.to_string();
        assert!(text.contains("файла бази немає"), "{text}");
        assert!(text.contains("EXCHANGE_DB"), "{text}");
        // Файл не мав з'явитись: відкриття без SQLITE_OPEN_CREATE.
        assert!(!db.exists());
    }

    #[test]
    fn file_without_schema_is_named_not_panicked() {
        let dir = TmpDir::new("noschema");
        let db = dir.db();
        {
            let conn = Connection::open(&db).expect("база");
            conn.execute_batch("CREATE TABLE stranger (x INTEGER)")
                .expect("чужа таблиця");
        }
        let err = read_snapshot(&db, 1, &dir.join("NOW.md")).expect_err("має бути помилка");
        assert!(matches!(err, DbError::NoSchema { .. }), "{err:?}");
        assert!(err.to_string().contains("messages"), "{err}");
    }

    #[test]
    fn connection_is_really_read_only() {
        let dir = TmpDir::new("ro");
        let db = make_db(&dir);
        let conn = open_read_only(&db).expect("відкриття");
        let err = conn
            .execute("INSERT INTO locks VALUES ('x','Grok',1,1,'')", [])
            .expect_err("запис має бути відхилений");
        // Саме це й є гарантія крейта: писати він не може технічно.
        match err {
            rusqlite::Error::SqliteFailure(inner, _) => {
                assert_eq!(inner.code, rusqlite::ErrorCode::ReadOnly, "{inner:?}");
            }
            other => panic!("очікували ReadOnly, отримали {other:?}"),
        }
    }

    #[test]
    fn last_render_reads_mtime_without_opening_file() {
        let dir = TmpDir::new("nowmd");
        let db = make_db(&dir);
        let now_md = dir.join("NOW.md");

        // Файла ще немає.
        let before = read_snapshot(&db, 1, &now_md).expect("снапшот");
        assert_eq!(before.last_render, None);

        std::fs::write(&now_md, "# дошка\n").expect("запис NOW.md");
        let after = read_snapshot(&db, 1, &now_md).expect("снапшот");
        let mtime = after.last_render.expect("mtime має бути");
        let wall = now_unix();
        // Щойно записаний файл: mtime не може лежати далеко від «зараз».
        assert!((wall - mtime).abs() < 120, "mtime={mtime}, зараз={wall}");
        assert_eq!(mtime, file_mtime_unix(&now_md).unwrap());
    }

    #[test]
    fn wal_database_reads_when_writer_is_gone() {
        let dir = TmpDir::new("wal");
        let db = dir.db();
        {
            let conn = Connection::open(&db).expect("база");
            let mode: String = conn
                .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
                .expect("WAL");
            assert!(mode.eq_ignore_ascii_case("wal"), "режим={mode}");
            conn.execute_batch(SCHEMA).expect("схема");
            insert_msg(&conn, 5, "Grok", BOTH, "t", "n", None);
        }
        // Писач закрився чисто — read-only читач мусить дати снапшот.
        let snap = read_snapshot(&db, 7, &dir.join("NOW.md")).expect("снапшот на WAL-базі");
        assert_eq!(snap.messages.len(), 1);
        assert_eq!(snap.unread_grok, 1);
        assert_eq!(snap.unread_claude, 1);
    }
}
