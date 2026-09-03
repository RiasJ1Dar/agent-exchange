use crate::error::Error;
use crate::messages::{post_with_conn, Agent, Envelope, Op, ENVELOPE_V};
use crate::now_unix;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

pub(crate) const DEFAULT_TTL_SEC: i64 = 7200;
/// Стеля теми — 200 символів **вхідного** рядка (до [`normalize_topic`]).
///
/// Міряється саме вхід, а не нормалізована форма: нормалізація лише
/// скорочує, і мегабайт пробілів не має доїжджати навіть до неї.
/// Стеля тіла нічого не важила, поки той самий виклик міг пронести
/// мегабайт сусіднім полем: тема лягає в `messages.topic` і в `locks.topic`,
/// і звідти — у `NOW.md`, який фсинкається на кожен запис.
pub const MAX_TOPIC_CHARS: usize = 200;

/// Стеля нотатки замка — 1000 символів вхідного рядка.
/// Причина та сама, що й у [`MAX_TOPIC_CHARS`]: `locks.note` теж
/// потрапляє в `NOW.md` при кожному рендері.
pub const MAX_NOTE_CHARS: usize = 1000;

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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lock {
    pub topic: String,
    pub holder: Agent,
    pub taken_at: i64,
    pub ttl_sec: i64,
    pub note: String,
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

/// Тема в межах [`MAX_TOPIC_CHARS`]. Перевищення — помилка, не обрізання.
///
/// Викликається **до** [`normalize_topic`]: нормалізація тільки скорочує,
/// тож міряти після неї означало б пропускати мегабайтний вхід усередину.
pub(crate) fn check_topic(topic: &str) -> Result<(), Error> {
    let chars = topic.chars().count();
    if chars > MAX_TOPIC_CHARS {
        return Err(Error::TopicTooLong {
            chars,
            max: MAX_TOPIC_CHARS,
        });
    }
    Ok(())
}

/// Нотатка замка в межах [`MAX_NOTE_CHARS`]. Перевищення — помилка.
pub(crate) fn check_note(note: &str) -> Result<(), Error> {
    let chars = note.chars().count();
    if chars > MAX_NOTE_CHARS {
        return Err(Error::NoteTooLong {
            chars,
            max: MAX_NOTE_CHARS,
        });
    }
    Ok(())
}

fn expire_locks(conn: &Connection, now: i64) -> Result<(), Error> {
    conn.execute(
        "DELETE FROM locks WHERE taken_at + ttl_sec <= ?1",
        params![now],
    )?;
    Ok(())
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

impl crate::Store {
    /// Тихе взяття замка: протермінований чужий замок зникає без сліду
    /// й без сповіщення (на відміну від [`Store::lock_ex`]).
    ///
    /// ⚠️ Читання стану теми і запис рядка йдуть **однією `IMMEDIATE`-
    /// транзакцією**, тією самою гілкою, що й у [`Store::lock_ex`]. Двома
    /// окремими autocommit-ами (SELECT, потім `INSERT … ON CONFLICT`) це було
    /// TOCTOU: на чотирьох живих процесах обидва встигали прочитати «тема
    /// вільна», обидва діставали `Ok` — і обидва вважали тему своєю.
    /// `busy_timeout` тут не рятує: `SQLITE_BUSY` не виникає взагалі,
    /// конфлікт логічний, а не блокувальний.
    ///
    /// Сигнатура навмисне лишається `Result<(), Error>` — її кличе `mcp`.
    /// Кому саме віддали тему після протермінування, показує [`Store::lock_ex`].
    pub fn lock(&self, topic: &str, holder: Agent, ttl_sec: i64, note: &str) -> Result<(), Error> {
        if matches!(holder, Agent::Both) {
            return Err(Error::BothCannotLock);
        }
        check_topic(topic)?;
        check_note(note)?;
        let topic = normalize_topic(topic);
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        lock_inner(&tx, &topic, holder, ttl_sec, note, false)?;
        tx.commit()?;
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
        check_topic(topic)?;
        check_note(note)?;
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
        check_topic(topic)?;
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
