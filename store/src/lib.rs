//! Реалізує агент store. Не чіпати з render/mcp.

mod error;
mod schema;
mod messages;
mod locks;
mod sign;

pub use error::Error;
pub use schema::{SCHEMA, SCHEMA_VERSION};
pub use messages::{
    Agent, Envelope, InboxQuery, Message, Op, Recipient, BRIEF_MARK, BROADCAST,
    BROADCAST_LEGACY, GC_INTERVAL_SEC, GC_READ_TTL_SEC, MAX_AGENT_CHARS,
    MAX_BODY_CHARS,
};
pub use sign::{canonical, CANON_TAG, SIGNABLE_ENVELOPE_V};
pub use locks::{
    normalize_topic, Evicted, Lock, LockOutcome, MAX_NOTE_CHARS, MAX_TOPIC_CHARS,
    MAX_TTL_SEC, MIN_TTL_SEC,
};

#[cfg(test)]
pub(crate) use locks::DEFAULT_TTL_SEC;

use rusqlite::Connection;
#[cfg(test)]
use rusqlite::params;
use std::sync::atomic::AtomicI64;
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Store {
    conn: Mutex<Connection>,
    /// Unix-час останнього авто-gc. `0` — ще не було: перший `post`/`ack`
    /// на цьому з'єднанні прибере прочитане старші за [`GC_READ_TTL_SEC`].
    last_gc_unix: AtomicI64,
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Store {
    pub(crate) fn conn(&self) -> Result<MutexGuard<'_, Connection>, Error> {
        self.conn.lock().map_err(|_| Error::Poisoned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    /// Ім'я агента для тесту. Паніка на негодящому — навмисно: у тестах
    /// імена задані руками, і мовчазний `Result` тут лише ховав би друкарську
    /// помилку в самому тесті.
    fn ag(name: &str) -> Agent {
        Agent::new(name).expect("ім'я агента в тесті має бути валідним")
    }

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

    /// Зсунути `ts_unix` усіх повідомлень у минуле. Заміна sleep(14 діб)
    /// у тесті gc: той самий стан, що й реальне старіння, за нуль секунд.
    fn age_messages(store: &Store, secs: i64) {
        let conn = store.conn().unwrap();
        conn.execute("UPDATE messages SET ts_unix = ts_unix - ?1", params![secs])
            .unwrap();
    }

    fn message_count(store: &Store) -> i64 {
        store
            .conn()
            .unwrap()
            .query_row("SELECT count(*) FROM messages", [], |row| row.get(0))
            .unwrap()
    }

    fn env(
        from: Agent,
        to: impl Into<Recipient>,
        op: Op,
        body: serde_json::Value,
    ) -> Envelope {
        Envelope {
            v: 1,
            from,
            to: to.into(),
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
            .post(env(ag("Grok"), ag("Claude"), Op::Q, json!({"q": "ping"})))
            .unwrap();
        assert!(id > 0);

        let unread = store.inbox(ag("Claude"), true).unwrap();
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].id, id);
        assert!(unread[0].read_at.is_none());
        assert_eq!(unread[0].envelope.v, 1);
        assert_eq!(unread[0].envelope.from, ag("Grok"));
        assert_eq!(unread[0].envelope.to, Recipient::One(ag("Claude")));
        assert_eq!(unread[0].envelope.topic, "xvid/core");
        assert_eq!(unread[0].envelope.op, Op::Q);
        assert_eq!(unread[0].envelope.body, json!({"q": "ping"}));

        assert!(store.inbox(ag("Grok"), true).unwrap().is_empty());

        let id_both = store
            .post(env(ag("Claude"), Recipient::All, Op::N, json!({"n": "note"})))
            .unwrap();
        let grok = store.inbox(ag("Grok"), true).unwrap();
        assert_eq!(grok.len(), 1);
        assert_eq!(grok[0].id, id_both);
        assert_eq!(store.inbox(ag("Claude"), true).unwrap().len(), 2);
        assert_eq!(store.inbox(Recipient::All, true).unwrap().len(), 1);

        store.ack(id).unwrap();
        let unread = store.inbox(ag("Claude"), true).unwrap();
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].id, id_both);

        let all = store.inbox(ag("Claude"), false).unwrap();
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
            .lock("xvid/core", ag("Grok"), DEFAULT_TTL_SEC, "працюю")
            .unwrap();

        let err = store
            .lock("xvid/core", ag("Claude"), DEFAULT_TTL_SEC, "теж")
            .unwrap_err();
        match err {
            Error::LockHeld { topic, holder } => {
                assert_eq!(topic, "xvid/core");
                assert_eq!(holder, ag("Grok"));
            }
            other => panic!("очікував LockHeld, отримав {other:?}"),
        }

        // ⚠️ Тут стояв тест «замок «усіма» відхиляється». Він ЗНИК не тому,
        // що правило скасували, а тому, що `lock` більше не приймає
        // широкомовну адресу за типом: `Recipient::All` туди не передати.
        // Правило лишилось живим у `locks_reject_broadcast_holder_from_db`,
        // де перевіряється єдиний шлях, яким воно ще може прийти, — база.

        store
            .lock("xvid/core", ag("Grok"), DEFAULT_TTL_SEC, "оновлено")
            .unwrap();
        let locks = store.locks().unwrap();
        assert_eq!(locks.len(), 1);
        assert_eq!(locks[0].topic, "xvid/core");
        assert_eq!(locks[0].holder, ag("Grok"));
        assert_eq!(locks[0].ttl_sec, DEFAULT_TTL_SEC);
        assert_eq!(locks[0].note, "оновлено");

        let err = store.unlock("xvid/core", ag("Claude")).unwrap_err();
        assert!(matches!(err, Error::LockNotHeld { .. }));

        store.unlock("xvid/core", ag("Grok")).unwrap();
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

        store.lock("xvid/core", ag("Grok"), 1, "короткий").unwrap();
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
        store.lock("xvid/core", ag("Grok"), 1, "знову").unwrap();
        age_lock(&store, "xvid/core", MIN_TTL_SEC + 1);
        store
            .lock("xvid/core", ag("Claude"), DEFAULT_TTL_SEC, "мій")
            .unwrap();
        let locks = store.locks().unwrap();
        assert_eq!(locks.len(), 1);
        assert_eq!(locks[0].holder, ag("Claude"));
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

        first.lock("alpha", ag("Grok"), 90, "hold").unwrap();
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
            .lock("  XVid / Core  ", ag("Grok"), DEFAULT_TTL_SEC, "працюю")
            .unwrap();

        let locks = store.locks().unwrap();
        assert_eq!(locks.len(), 1);
        assert_eq!(locks[0].topic, "xvid / core", "у БД лягла сира тема");

        // Той самий замок очима іншого агента — конфлікт, а не другий рядок.
        let err = store
            .lock("xvid / core", ag("Claude"), DEFAULT_TTL_SEC, "теж")
            .unwrap_err();
        match err {
            Error::LockHeld { topic, holder } => {
                assert_eq!(topic, "xvid / core");
                assert_eq!(holder, ag("Grok"));
            }
            other => panic!("очікував LockHeld, отримав {other:?}"),
        }

        // Той самий власник у третьому написанні — продовження, не другий рядок.
        store
            .lock("XVID  /  CORE", ag("Grok"), DEFAULT_TTL_SEC, "оновив")
            .unwrap();
        let locks = store.locks().unwrap();
        assert_eq!(locks.len(), 1);
        assert_eq!(locks[0].note, "оновив");

        // Знімається теж у будь-якому написанні.
        store.unlock("  Xvid /   Core ", ag("Grok")).unwrap();
        assert!(store.locks().unwrap().is_empty());
    }

    #[test]
    fn ttl_is_clamped() {
        let (_tmp, store) = tmp_store();

        store.lock("t", ag("Grok"), 1, "нижче межі").unwrap();
        assert_eq!(store.locks().unwrap()[0].ttl_sec, 60);

        store.lock("t", ag("Grok"), 999_999, "вище межі").unwrap();
        assert_eq!(store.locks().unwrap()[0].ttl_sec, 86_400);

        store.lock("t", ag("Grok"), MIN_TTL_SEC, "рівно нижня").unwrap();
        assert_eq!(store.locks().unwrap()[0].ttl_sec, MIN_TTL_SEC);

        store.lock("t", ag("Grok"), MAX_TTL_SEC, "рівно верхня").unwrap();
        assert_eq!(store.locks().unwrap()[0].ttl_sec, MAX_TTL_SEC);

        store.lock("t", ag("Grok"), 3600, "усередині").unwrap();
        assert_eq!(store.locks().unwrap()[0].ttl_sec, 3600);

        // 0 і від'ємне — «за замовчуванням», поведінка не змінилась.
        store.lock("t", ag("Grok"), 0, "нуль").unwrap();
        assert_eq!(store.locks().unwrap()[0].ttl_sec, DEFAULT_TTL_SEC);
        store.lock("t", ag("Grok"), -5, "мінус").unwrap();
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
            .lock("xvid/core", ag("Grok"), MIN_TTL_SEC, "працюю")
            .unwrap();
        let taken_at = store.locks().unwrap()[0].taken_at;
        age_lock(&store, "xvid/core", MIN_TTL_SEC + 1);

        let outcome = store
            .lock_ex("xvid/core", ag("Claude"), DEFAULT_TTL_SEC, "мій")
            .unwrap();

        let evicted = outcome.evicted.expect("евікшн мав бути видимим");
        assert_eq!(evicted.holder, ag("Grok"));
        assert_eq!(
            evicted.taken_at,
            taken_at - (MIN_TTL_SEC + 1),
            "taken_at має бути моментом, коли Grok узяв замок"
        );

        // Тема справді перейшла.
        let locks = store.locks().unwrap();
        assert_eq!(locks.len(), 1);
        assert_eq!(locks[0].holder, ag("Claude"));
        assert_eq!(locks[0].note, "мій");

        // І колишній тримач про це дізнався — саме в inbox, а не «десь у логах».
        let id = outcome.notified_id.expect("сповіщення не надіслано");
        let inbox = store.inbox(ag("Grok"), true).unwrap();
        assert_eq!(inbox.len(), 1, "у Grok має бути рівно одне сповіщення");
        assert_eq!(inbox[0].id, id);
        assert_eq!(inbox[0].envelope.from, ag("Claude"));
        assert_eq!(inbox[0].envelope.to, Recipient::One(ag("Grok")));
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
            .lock("xvid/core", ag("Grok"), DEFAULT_TTL_SEC, "працюю")
            .unwrap();

        let err = store
            .lock_ex("xvid/core", ag("Claude"), DEFAULT_TTL_SEC, "теж")
            .unwrap_err();
        match err {
            Error::LockHeld { topic, holder } => {
                assert_eq!(topic, "xvid/core");
                assert_eq!(holder, ag("Grok"));
            }
            other => panic!("очікував LockHeld, отримав {other:?}"),
        }

        let locks = store.locks().unwrap();
        assert_eq!(locks[0].holder, ag("Grok"), "замок віддали живим");
        assert_eq!(locks[0].note, "працюю");
        assert!(
            store.inbox(ag("Grok"), true).unwrap().is_empty(),
            "відмова не сміє нікого сповіщати"
        );

        // Вільна тема — теж без евікшна.
        let outcome = store
            .lock_ex("інша тема", ag("Claude"), DEFAULT_TTL_SEC, "нова")
            .unwrap();
        assert_eq!(outcome.evicted, None);
        assert_eq!(outcome.notified_id, None);

        // Продовження власного живого замка — теж не евікшн.
        let outcome = store
            .lock_ex("xvid/core", ag("Grok"), DEFAULT_TTL_SEC, "далі")
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
            .lock("xvid/core", ag("Grok"), MIN_TTL_SEC, "старий")
            .unwrap();
        age_lock(&store, "xvid/core", MIN_TTL_SEC + 1);

        let store = Arc::new(store);
        let worker = Arc::clone(&store);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let notified = worker
                .lock_ex("xvid/core", ag("Claude"), DEFAULT_TTL_SEC, "мій")
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
            .lock("xvid/core", ag("Grok"), DEFAULT_TTL_SEC, "працюю")
            .unwrap();

        let err = store
            .unlock_force("xvid/core", ag("Claude"), false)
            .unwrap_err();
        assert!(matches!(err, Error::LockNotHeld { .. }));
        assert_eq!(store.locks().unwrap().len(), 1, "замок зняли без force");

        store
            .unlock_force("xvid/core", ag("Claude"), true)
            .unwrap();
        assert!(store.locks().unwrap().is_empty(), "force не зняв замок");

        // Немає замка взагалі — force не вигадує успіх.
        let err = store
            .unlock_force("xvid/core", ag("Claude"), true)
            .unwrap_err();
        assert!(matches!(err, Error::LockNotHeld { .. }));

        // Власний замок знімається і без force — стара поведінка ціла.
        store
            .lock("xvid/core", ag("Claude"), DEFAULT_TTL_SEC, "мій")
            .unwrap();
        store
            .unlock_force("xvid/core", ag("Claude"), false)
            .unwrap();
        assert!(store.locks().unwrap().is_empty());
    }

    /// Стеля тіла — помилка, не тихе обрізання.
    #[test]
    fn body_over_the_limit_is_an_error() {
        let (_tmp, store) = tmp_store();

        let ok = body_of_len(MAX_BODY_CHARS);
        let id = store
            .post(env(ag("Grok"), ag("Claude"), Op::N, ok))
            .unwrap();
        assert!(id > 0, "рівно 2000 символів мали пройти");

        let too_long = body_of_len(MAX_BODY_CHARS + 1);
        let err = store
            .post(env(ag("Grok"), ag("Claude"), Op::N, too_long))
            .unwrap_err();
        match err {
            Error::BodyTooLong { chars, max } => {
                assert_eq!(chars, MAX_BODY_CHARS + 1);
                assert_eq!(max, MAX_BODY_CHARS);
            }
            other => panic!("очікував BodyTooLong, отримав {other:?}"),
        }

        // Нічого не записано й не обрізано.
        assert_eq!(store.inbox(ag("Claude"), false).unwrap().len(), 1);
    }

    /// `to` не може дорівнювати `from` — але тільки буквально.
    #[test]
    fn self_addressed_message_is_rejected() {
        let (_tmp, store) = tmp_store();

        let err = store
            .post(env(ag("Grok"), ag("Grok"), Op::N, json!({"n": "сам собі"})))
            .unwrap_err();
        assert!(matches!(&err, Error::SelfMessage(x) if *x == ag("Grok")), "{err:?}");

        let err = store
            .post(env(ag("Claude"), ag("Claude"), Op::N, json!({})))
            .unwrap_err();
        assert!(matches!(&err, Error::SelfMessage(x) if *x == ag("Claude")), "{err:?}");

        // ⚠️ Тут була пара «всі→всі». Її теж не написати: відправником
        // тепер може бути лише конкретний агент.

        // А розсилка від конкретного агента — легальна (рішення №14),
        // навіть якщо він побачить її і в себе.
        let id = store
            .post(env(ag("Grok"), Recipient::All, Op::N, json!({"n": "усім"})))
            .unwrap();
        assert!(id > 0);
        assert_eq!(store.inbox(ag("Grok"), true).unwrap().len(), 1);
        assert_eq!(store.inbox(ag("Claude"), true).unwrap().len(), 1);
    }

    /// Те саме, що [`env`], але з довільною темою: фільтр `topic` треба
    /// перевіряти на різних написаннях, а не на одному «xvid/core».
    fn env_topic(
        from: Agent,
        to: impl Into<Recipient>,
        topic: &str,
        body: serde_json::Value,
    ) -> Envelope {
        Envelope {
            v: 1,
            from,
            to: to.into(),
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
            .post(env(ag("Grok"), ag("Claude"), Op::Q, json!({"q": "ping"})))
            .unwrap();

        // Grok підтверджує чуже повідомлення й неіснуючий id.
        assert_eq!(store.ack_many(&[to_claude], ag("Grok")).unwrap(), 0);
        assert_eq!(store.ack_many(&[999_999], ag("Grok")).unwrap(), 0);
        assert_eq!(
            store.ack_many(&[to_claude, 999_999], ag("Grok")).unwrap(),
            0,
            "жоден id не мав зарахуватись"
        );
        assert_eq!(store.ack_many(&[], ag("Claude")).unwrap(), 0);

        // І чуже повідомлення лишилось непрочитаним у справжнього адресата.
        let unread = store.inbox(ag("Claude"), true).unwrap();
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].id, to_claude);
    }

    /// Два свої з трьох → 2; третє (чуже) лишається непрочитаним.
    /// Повторний виклик тих самих → 0.
    #[test]
    fn ack_many_counts_only_what_it_marked() {
        let (_tmp, store) = tmp_store();
        let a = store
            .post(env(ag("Grok"), ag("Claude"), Op::Q, json!({"n": 1})))
            .unwrap();
        let b = store
            .post(env(ag("Grok"), Recipient::All, Op::N, json!({"n": 2})))
            .unwrap();
        let c = store
            .post(env(ag("Claude"), ag("Grok"), Op::N, json!({"n": 3})))
            .unwrap();

        assert_eq!(
            store.ack_many(&[a, b, c], ag("Claude")).unwrap(),
            2,
            "Claude мав підтвердити рівно свої два"
        );

        // Третє — Grokове, воно ціле й непрочитане.
        let grok_unread = store.inbox(ag("Grok"), true).unwrap();
        assert_eq!(grok_unread.len(), 1, "у Grok мало лишитись одне непрочитане");
        assert_eq!(grok_unread[0].id, c);

        // У Claude непрочитаних не лишилось.
        assert!(store.inbox(ag("Claude"), true).unwrap().is_empty());

        // Повторне підтвердження вже прочитаних — 0, і теж без помилки.
        assert_eq!(store.ack_many(&[a, b], ag("Claude")).unwrap(), 0);
        assert_eq!(store.ack_many(&[a, b, c], ag("Claude")).unwrap(), 0);

        // Grok свій id підтверджує сам — і рівно один раз.
        assert_eq!(store.ack_many(&[c], ag("Grok")).unwrap(), 1);
        assert_eq!(store.ack_many(&[c], ag("Grok")).unwrap(), 0);
    }

    /// `limit` віддає саме найновіші, у тому ж порядку (найстаріші першими).
    #[test]
    fn inbox_ex_limit_returns_newest() {
        let (_tmp, store) = tmp_store();
        let mut ids = Vec::new();
        for n in 1..=5 {
            ids.push(
                store
                    .post(env(ag("Grok"), ag("Claude"), Op::N, json!({ "n": n })))
                    .unwrap(),
            );
        }

        let got = store
            .inbox_ex(InboxQuery {
                agent: Recipient::One(ag("Claude")),
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
                agent: Recipient::One(ag("Claude")),
                limit: Some(99),
                ..InboxQuery::default()
            })
            .unwrap();
        assert_eq!(all.len(), 5);

        // Нуль — це нуль, а не «усі».
        let none = store
            .inbox_ex(InboxQuery {
                agent: Recipient::One(ag("Claude")),
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
            .post(env(ag("Grok"), ag("Claude"), Op::N, full.clone()))
            .unwrap();

        // Без brief — тіло ціле, байт-у-байт.
        let whole = store.inbox(ag("Claude"), false).unwrap();
        assert_eq!(whole[0].envelope.body, full);

        let cut = store
            .inbox_ex(InboxQuery {
                agent: Recipient::One(ag("Claude")),
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
                agent: Recipient::One(ag("Claude")),
                brief: Some(MAX_BODY_CHARS),
                ..InboxQuery::default()
            })
            .unwrap();
        assert_eq!(short[0].envelope.body, full, "ціле тіло не мало змінитись");

        // Рядкове тіло ріжеться як рядок, не як JSON із лапками.
        store
            .post(env(
                ag("Grok"),
                ag("Claude"),
                Op::N,
                json!("б".repeat(30)),
            ))
            .unwrap();
        let cut = store
            .inbox_ex(InboxQuery {
                agent: Recipient::One(ag("Claude")),
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
                ag("Grok"),
                ag("Claude"),
                "XVid  Core",
                json!({"n": "пробіли"}),
            ))
            .unwrap();
        let plain = store
            .post(env_topic(
                ag("Grok"),
                ag("Claude"),
                "xvid core",
                json!({"n": "як є"}),
            ))
            .unwrap();
        // А це вже інша тема: слеш нормалізація не чіпає.
        store
            .post(env_topic(
                ag("Grok"),
                ag("Claude"),
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
                agent: Recipient::One(ag("Claude")),
                topic: Some("  xVid   CORE ".into()),
                ..InboxQuery::default()
            })
            .unwrap();
        assert_eq!(got.len(), 2, "мали збігтись обидва написання «xvid core»");
        assert_eq!(got[0].id, spaced);
        assert_eq!(got[1].id, plain);

        let slash = store
            .inbox_ex(InboxQuery {
                agent: Recipient::One(ag("Claude")),
                topic: Some("XVID/Core".into()),
                ..InboxQuery::default()
            })
            .unwrap();
        assert_eq!(slash.len(), 1);
        assert_eq!(slash[0].envelope.body, json!({"n": "слеш"}));

        // Теми, якої немає, — порожньо, а не «усе».
        let empty = store
            .inbox_ex(InboxQuery {
                agent: Recipient::One(ag("Claude")),
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
            .post(env(ag("Grok"), Recipient::All, Op::N, json!({"n": "усім"})))
            .unwrap();
        let foreign = store
            .post(env(ag("Claude"), ag("Grok"), Op::N, json!({"n": "тобі"})))
            .unwrap();

        let default = store
            .inbox_ex(InboxQuery {
                agent: Recipient::One(ag("Grok")),
                ..InboxQuery::default()
            })
            .unwrap();
        assert_eq!(default.len(), 2, "дефолт не сміє нічого ховати");
        assert_eq!(default[0].id, own);

        let without_own = store
            .inbox_ex(InboxQuery {
                agent: Recipient::One(ag("Grok")),
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
            .post(env(ag("Grok"), ag("Claude"), Op::Q, json!({"n": 1})))
            .unwrap();
        let both = store
            .post(env(ag("Grok"), Recipient::All, Op::N, json!({"n": 2})))
            .unwrap();
        store
            .post(env(ag("Claude"), ag("Grok"), Op::A, json!({"n": 3})))
            .unwrap();
        store.ack_many(&[both], ag("Claude")).unwrap();

        // Дефолт запиту нікому не приписує чужої особи.
        assert_eq!(Recipient::default(), Recipient::All);

        for agent in [
            Recipient::One(ag("Grok")),
            Recipient::One(ag("Claude")),
            Recipient::All,
        ] {
            let old = store.inbox(agent.clone(), false).unwrap();
            let new = store
                .inbox_ex(InboxQuery {
                    agent: agent.clone(),
                    ..InboxQuery::default()
                })
                .unwrap();
            assert_eq!(old, new, "дефолтний запит розійшовся з inbox({agent}, false)");

            let old_unread = store.inbox(agent.clone(), true).unwrap();
            let new_unread = store
                .inbox_ex(InboxQuery {
                    agent: agent.clone(),
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
        let mut e = env(ag("Grok"), ag("Claude"), Op::A, json!({}));
        e.v = 2;
        let err = store.post(e).unwrap_err();
        assert!(matches!(err, Error::BadVersion(2)));
    }

    /// Гонка за темою між **двома різними `Store`** на одному файлі.
    ///
    /// Два `Store` — це принципово: усередині одного процесу теми стеріг
    /// `Mutex<Connection>`, і гонки не було видно. Реальні чотири агенти —
    /// чотири процеси, чотири власні мютекси й одна база; єдине, що їх
    /// серіалізує, — транзакція в самій SQLite. До фіксу `lock` робив SELECT
    /// і `INSERT … ON CONFLICT` окремими autocommit-ами, і обидва потоки
    /// діставали `Ok` на ту саму тему.
    #[test]
    fn two_locks_race_for_one_topic_and_only_one_wins() {
        use std::sync::{mpsc, Barrier};

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("exchange.db");
        // Обидва Store бачать один файл, але мають окремі з'єднання й окремі
        // мютекси — як два процеси.
        let a = Store::open(&path).unwrap();
        let b = Store::open(&path).unwrap();

        let gate = std::sync::Arc::new(Barrier::new(2));
        let (tx, rx) = mpsc::channel();

        let mut handles = Vec::new();
        for (store, holder) in [(a, ag("Grok")), (b, ag("Claude"))] {
            let gate = std::sync::Arc::clone(&gate);
            let tx = tx.clone();
            handles.push(std::thread::spawn(move || {
                gate.wait();
                let r = store.lock("гонка", holder.clone(), DEFAULT_TTL_SEC, "мій");
                let _ = tx.send((holder, r));
            }));
        }
        drop(tx);

        let mut winners = Vec::new();
        let mut held = Vec::new();
        for _ in 0..2 {
            let (holder, r) = rx
                .recv_timeout(std::time::Duration::from_secs(30))
                .expect("потік не відзвітував: lock завис");
            match r {
                Ok(()) => winners.push(holder),
                Err(Error::LockHeld { .. }) => held.push(holder),
                Err(other) => panic!("очікував Ok або LockHeld, отримав {other:?}"),
            }
        }
        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(
            winners.len(),
            1,
            "тему віддали двом одразу: {winners:?} — TOCTOU у lock"
        );
        assert_eq!(held.len(), 1, "другий мав дістати LockHeld");

        let check = Store::open(&path).unwrap();
        let locks = check.locks().unwrap();
        assert_eq!(locks.len(), 1, "на одну тему два рядки");
        assert_eq!(
            locks[0].holder, winners[0],
            "у базі сидить не той, кому сказали Ok"
        );
    }

    fn str_of_len(n: usize) -> String {
        "я".repeat(n)
    }

    /// Стеля теми — помилка, не обрізання; і вона діє в кожному вході,
    /// а не лише в `post`.
    #[test]
    fn topic_over_the_limit_is_an_error() {
        let (_tmp, store) = tmp_store();

        let ok = str_of_len(MAX_TOPIC_CHARS);
        let too_long = str_of_len(MAX_TOPIC_CHARS + 1);

        // post
        store
            .post(env_topic(ag("Grok"), ag("Claude"), &ok, json!({"a": 1})))
            .unwrap();
        let err = store
            .post(env_topic(
                ag("Grok"),
                ag("Claude"),
                &too_long,
                json!({"a": 1}),
            ))
            .unwrap_err();
        match err {
            Error::TopicTooLong { chars, max } => {
                assert_eq!(chars, MAX_TOPIC_CHARS + 1);
                assert_eq!(max, MAX_TOPIC_CHARS);
            }
            other => panic!("очікував TopicTooLong, отримав {other:?}"),
        }

        // lock / lock_ex / unlock
        store.lock(&ok, ag("Grok"), DEFAULT_TTL_SEC, "ок").unwrap();
        assert!(matches!(
            store
                .lock(&too_long, ag("Grok"), DEFAULT_TTL_SEC, "ні")
                .unwrap_err(),
            Error::TopicTooLong { .. }
        ));
        assert!(matches!(
            store
                .lock_ex(&too_long, ag("Grok"), DEFAULT_TTL_SEC, "ні")
                .unwrap_err(),
            Error::TopicTooLong { .. }
        ));
        assert!(matches!(
            store.unlock(&too_long, ag("Grok")).unwrap_err(),
            Error::TopicTooLong { .. }
        ));
        store.unlock(&ok, ag("Grok")).unwrap();

        // Довга тема не осіла в базі жодним шляхом.
        assert!(store.locks().unwrap().is_empty());
        let inbox = store.inbox(ag("Claude"), false).unwrap();
        assert_eq!(inbox.len(), 1, "у базу проліз зайвий запис");
        assert_eq!(inbox[0].envelope.topic.chars().count(), MAX_TOPIC_CHARS);
    }

    /// Стеля нотатки замка — так само помилка.
    #[test]
    fn note_over_the_limit_is_an_error() {
        let (_tmp, store) = tmp_store();

        let ok = str_of_len(MAX_NOTE_CHARS);
        let too_long = str_of_len(MAX_NOTE_CHARS + 1);

        store
            .lock("xvid/core", ag("Grok"), DEFAULT_TTL_SEC, &ok)
            .unwrap();
        assert_eq!(
            store.locks().unwrap()[0].note.chars().count(),
            MAX_NOTE_CHARS
        );

        let err = store
            .lock("xvid/core", ag("Grok"), DEFAULT_TTL_SEC, &too_long)
            .unwrap_err();
        match err {
            Error::NoteTooLong { chars, max } => {
                assert_eq!(chars, MAX_NOTE_CHARS + 1);
                assert_eq!(max, MAX_NOTE_CHARS);
            }
            other => panic!("очікував NoteTooLong, отримав {other:?}"),
        }

        assert!(matches!(
            store
                .lock_ex("xvid/core", ag("Grok"), DEFAULT_TTL_SEC, &too_long)
                .unwrap_err(),
            Error::NoteTooLong { .. }
        ));

        // Відмова нічого не переписала: у базі лишилась стара нотатка.
        assert_eq!(
            store.locks().unwrap()[0].note.chars().count(),
            MAX_NOTE_CHARS
        );
    }

    /// `ack_one` гасить лише своє — і мовчить, а не падає, на чуже.
    #[test]
    fn ack_one_marks_only_own_messages() {
        let (_tmp, store) = tmp_store();

        let to_claude = store
            .post(env(ag("Grok"), ag("Claude"), Op::Q, json!({"q": 1})))
            .unwrap();
        let to_grok = store
            .post(env(ag("Claude"), ag("Grok"), Op::A, json!({"a": 1})))
            .unwrap();
        let to_both = store
            .post(env(ag("Grok"), Recipient::All, Op::N, json!({"n": 1})))
            .unwrap();

        // Чуже — false, без помилки, і чуже лишається непрочитаним.
        assert!(!store.ack_one(to_grok, ag("Claude")).unwrap());
        let grok_unread = store.inbox(ag("Grok"), true).unwrap();
        assert!(
            grok_unread.iter().any(|m| m.id == to_grok),
            "Claude погасив чуже повідомлення"
        );

        // Неіснуюче — теж false, а не NotFound.
        assert!(!store.ack_one(999_999, ag("Claude")).unwrap());

        // Своє — true; повторно — false.
        assert!(store.ack_one(to_claude, ag("Claude")).unwrap());
        assert!(!store.ack_one(to_claude, ag("Claude")).unwrap());

        // Both адресоване обом — Claude має право його погасити.
        assert!(store.ack_one(to_both, ag("Claude")).unwrap());

        let unread: Vec<i64> = store
            .inbox(ag("Claude"), true)
            .unwrap()
            .iter()
            .map(|m| m.id)
            .collect();
        assert!(unread.is_empty(), "лишились непрочитані: {unread:?}");
    }

    /// 100 прочитаних старих + 2 unread → gc лишає unread, старі зникають.
    #[test]
    fn gc_drops_old_read_keeps_unread() {
        let (_tmp, store) = tmp_store();
        let mut old_ids = Vec::with_capacity(100);
        for n in 0..100 {
            old_ids.push(
                store
                    .post(env(ag("Grok"), ag("Claude"), Op::N, json!({ "n": n })))
                    .unwrap(),
            );
        }
        assert_eq!(store.ack_many(&old_ids, ag("Claude")).unwrap(), 100);
        age_messages(&store, GC_READ_TTL_SEC + 1);

        let u1 = store
            .post(env(ag("Grok"), ag("Claude"), Op::Q, json!({"q": "live-1"})))
            .unwrap();
        let u2 = store
            .post(env(ag("Grok"), ag("Claude"), Op::Q, json!({"q": "live-2"})))
            .unwrap();
        assert_eq!(message_count(&store), 102, "до gc мають лежати всі 102");

        let dropped = store.gc_read().unwrap();
        assert_eq!(dropped, 100, "мали зникнути рівно 100 старих прочитаних");
        assert_eq!(message_count(&store), 2);

        let unread = store.inbox(ag("Claude"), true).unwrap();
        assert_eq!(unread.len(), 2);
        assert_eq!(unread[0].id, u1);
        assert_eq!(unread[1].id, u2);
        assert!(unread.iter().all(|m| m.read_at.is_none()));
        assert_eq!(unread[0].envelope.body, json!({"q": "live-1"}));
        assert_eq!(unread[1].envelope.body, json!({"q": "live-2"}));

        let all = store.inbox(ag("Claude"), false).unwrap();
        assert_eq!(all.len(), 2, "прочитані не мали лишитись у скриньці");
    }

    /// Свіжо прочитане і старе непрочитане gc не чіпає.
    #[test]
    fn gc_keeps_recent_read_and_old_unread() {
        let (_tmp, store) = tmp_store();
        let old_unread = store
            .post(env(ag("Grok"), ag("Claude"), Op::Q, json!({"q": "старе"})))
            .unwrap();
        age_messages(&store, GC_READ_TTL_SEC + 1);
        let recent = store
            .post(env(ag("Grok"), ag("Claude"), Op::N, json!({"n": "свіже"})))
            .unwrap();
        store.ack(recent).unwrap();

        assert_eq!(store.gc_read().unwrap(), 0, "не мало що прибирати");
        assert_eq!(message_count(&store), 2);
        let all = store.inbox(ag("Claude"), false).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, old_unread);
        assert!(all[0].read_at.is_none());
        assert_eq!(all[1].id, recent);
        assert!(all[1].read_at.is_some());
    }

    /// `post` сам кличе gc, коли з попереднього прогону минула година.
    #[test]
    fn post_gcs_old_read_after_interval() {
        let (_tmp, store) = tmp_store();
        let old = store
            .post(env(ag("Grok"), ag("Claude"), Op::N, json!({"n": "старе"})))
            .unwrap();
        store.ack(old).unwrap();
        age_messages(&store, GC_READ_TTL_SEC + 1);
        store
            .last_gc_unix
            .store(0, std::sync::atomic::Ordering::Relaxed);

        let live = store
            .post(env(ag("Grok"), ag("Claude"), Op::Q, json!({"q": "живе"})))
            .unwrap();
        assert_eq!(message_count(&store), 1, "post мав прибрати старе прочитане");
        let unread = store.inbox(ag("Claude"), true).unwrap();
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].id, live);
    }

    /// У межах години `post` gc не запускає — інакше кожен лист писав би DELETE.
    #[test]
    fn post_skips_gc_within_interval() {
        let (_tmp, store) = tmp_store();
        let old = store
            .post(env(ag("Grok"), ag("Claude"), Op::N, json!({"n": "старе"})))
            .unwrap();
        store.ack(old).unwrap();
        // Перший post уже виставив last_gc; у межах години наступний не чистить.
        age_messages(&store, GC_READ_TTL_SEC + 1);
        store
            .post(env(ag("Grok"), ag("Claude"), Op::Q, json!({"q": "живе"})))
            .unwrap();
        assert_eq!(
            message_count(&store),
            2,
            "post у межах години не мав чіпати старе прочитане"
        );
        assert_eq!(store.gc_read().unwrap(), 1);
        assert_eq!(message_count(&store), 1);
    }

    // ── Широкомовна адреса: `Both` -> `*`, схема v2 ─────────────────────

    /// Зробити базу **схеми v1** із заданими значеннями `to_agent`.
    ///
    /// Пишеться напряму, повз `Store`: саме так виглядає база, створена
    /// попередньою збіркою, і лише на такій має сенс перевіряти міграцію.
    fn v1_db(dir: &TempDir, to_agents: &[&str]) -> std::path::PathBuf {
        let path = dir.path().join("v1.db");
        let conn = Connection::open(&path).unwrap();
        conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get::<_, String>(0))
            .unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        for (i, to) in to_agents.iter().enumerate() {
            conn.execute(
                "INSERT INTO messages (ts_unix, v, from_agent, to_agent, topic, op, body, read_at)
                 VALUES (?1, 1, 'Grok', ?2, 't', 'N', '{}', NULL)",
                params![1000 + i as i64, to],
            )
            .unwrap();
        }
        conn.execute_batch("PRAGMA user_version = 1").unwrap();
        path
    }

    fn to_agents_of(store: &Store) -> Vec<String> {
        let conn = store.conn().unwrap();
        let mut stmt = conn
            .prepare("SELECT to_agent FROM messages ORDER BY id")
            .unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    fn insert_legacy_broadcast(store: &Store) -> i64 {
        let conn = store.conn().unwrap();
        conn.execute(
            "INSERT INTO messages (ts_unix, v, from_agent, to_agent, topic, op, body, read_at)
             VALUES (1000, 1, 'Grok', ?1, 't', 'N', '{}', NULL)",
            params![BROADCAST_LEGACY],
        )
        .unwrap();
        conn.query_row("SELECT max(id) FROM messages", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn migration_renames_broadcast_and_leaves_named_agents_alone() {
        let dir = TempDir::new().unwrap();
        let path = v1_db(&dir, &["Both", "Claude", "Both", "Grok"]);

        let store = Store::open(&path).unwrap();

        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        assert_eq!(
            to_agents_of(&store),
            vec!["*", "Claude", "*", "Grok"],
            "міграція мала перейменувати ЛИШЕ широкомовні рядки"
        );
    }

    /// ⚠️ Найдорожча гарантія цього зрізу: міграція нікого не загубила.
    ///
    /// `UPDATE` міг би зіпсувати адресацію тихо — рядки лишились би на місці,
    /// а `inbox` перестав би їх віддавати. Тому перевіряється не вміст
    /// колонки, а те, що адресат далі бачить своє.
    #[test]
    fn migrated_broadcast_is_still_delivered_to_everyone() {
        let dir = TempDir::new().unwrap();
        let path = v1_db(&dir, &["Both", "Claude"]);

        let store = Store::open(&path).unwrap();

        assert_eq!(
            store.inbox(ag("Grok"), false).unwrap().len(),
            1,
            "Grok мав побачити широкомовне, яке до міграції було «Both»"
        );
        assert_eq!(
            store.inbox(ag("Claude"), false).unwrap().len(),
            2,
            "Claude мав побачити і своє адресне, і широкомовне"
        );
    }

    /// Рядок у старому написанні читається й після міграції.
    ///
    /// Це не гіпотетичний випадок: `ui` відкриває базу read-only й мігрувати
    /// не вміє за побудовою, а копії для перевірок роблять із живої бази
    /// будь-якої версії.
    #[test]
    fn legacy_spelling_is_still_delivered() {
        let (_tmp, store) = tmp_store();
        insert_legacy_broadcast(&store);
        assert_eq!(
            store.inbox(ag("Claude"), true).unwrap().len(),
            1,
            "старе написання широкомовної адреси мусить читатись назавжди"
        );
    }

    #[test]
    fn ack_reaches_broadcast_in_the_legacy_spelling() {
        let (_tmp, store) = tmp_store();
        let id = insert_legacy_broadcast(&store);

        assert!(
            store.ack_one(id, ag("Claude")).unwrap(),
            "ack мав дістати широкомовне у старому написанні"
        );
        assert!(store.inbox(ag("Claude"), true).unwrap().is_empty());
    }

    #[test]
    fn broadcast_is_written_as_star_and_read_in_both_spellings() {
        assert_eq!(Recipient::All.as_str(), BROADCAST);
        assert_eq!(Recipient::parse(BROADCAST).unwrap(), Recipient::All);
        assert_eq!(Recipient::parse(BROADCAST_LEGACY).unwrap(), Recipient::All);
        assert!(
            Recipient::parse("Both!").is_err(),
            "толерантність не мала розповзтися на схожі рядки"
        );
    }

    // ── Схема v3: місце під підпис ──────────────────────────────────────

    fn columns_of(store: &Store, table: &str) -> Vec<String> {
        let conn = store.conn().unwrap();
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})")).unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, String>(1)).unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    /// ⚠️ Головний тест зрізу — і він про пастку, а не про фічу.
    ///
    /// Нова база створюється з повної `SCHEMA`, тобто вже з колонками
    /// підпису, а потім однаково проходить `migrate` з версії 0. Голий
    /// `ALTER TABLE ADD COLUMN` там впав би з «duplicate column name», і
    /// жодна нова база не відкрилася б узагалі.
    #[test]
    fn a_fresh_database_survives_the_column_adding_migration() {
        let (_tmp, store) = tmp_store();
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        let cols = columns_of(&store, "messages");
        assert!(cols.contains(&"sig".to_string()), "{cols:?}");
        assert!(cols.contains(&"key_id".to_string()), "{cols:?}");
    }

    /// Стара база доростає до v3, а те, що в ній лежало, лишається на місці.
    #[test]
    fn an_old_database_gains_signature_columns_without_losing_rows() {
        let dir = TempDir::new().unwrap();
        let path = v1_db(&dir, &["Both", "Claude"]);

        let store = Store::open(&path).unwrap();

        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
        let cols = columns_of(&store, "messages");
        assert!(cols.contains(&"sig".to_string()), "{cols:?}");
        assert_eq!(message_count(&store), 2, "міграція не мала загубити рядки");

        // Місце під підпис порожнє: писати його починає наступний зріз.
        let unsigned: i64 = store
            .conn()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM messages WHERE sig IS NULL AND key_id IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(unsigned, 2, "старі рядки мали лишитись без підпису");
    }

    /// Повторне відкриття не додає колонок удруге й не збиває версію.
    #[test]
    fn reopening_a_migrated_database_is_a_no_op() {
        let dir = TempDir::new().unwrap();
        let path = v1_db(&dir, &["Claude"]);

        let before = {
            let store = Store::open(&path).unwrap();
            columns_of(&store, "messages")
        };
        let store = Store::open(&path).unwrap();

        assert_eq!(columns_of(&store, "messages"), before);
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    }

    // ── Ім'я агента: довільне, але перевірене ───────────────────────────

    /// ⚠️ Головний тест цього зрізу: сервер більше не знає, як звуть агентів.
    ///
    /// Пара `Grok`+`Claude` — наш випадок, а не властивість протоколу. Тут
    /// працюють імена, яких у коді немає взагалі, і саме це має бути видно
    /// тому, хто прийде з іншим набором ШІ.
    #[test]
    fn arbitrary_agent_names_work_end_to_end() {
        let (_tmp, store) = tmp_store();
        let codex = ag("Codex");
        let gemini = ag("gemini-2.5-pro");

        let personal = store
            .post(env(codex.clone(), gemini.clone(), Op::Q, json!({"q": "?"})))
            .unwrap();
        let broadcast = store
            .post(env(gemini.clone(), Recipient::All, Op::N, json!({"n": "усім"})))
            .unwrap();

        let inbox = store.inbox(gemini.clone(), true).unwrap();
        assert_eq!(
            inbox.iter().map(|m| m.id).collect::<Vec<_>>(),
            vec![personal, broadcast],
            "адресат мав побачити і особисте, і розсилку"
        );
        assert_eq!(
            store.inbox(codex.clone(), true).unwrap().len(),
            1,
            "відправникові лишається тільки чужа розсилка"
        );

        assert!(store.ack_one(personal, gemini.clone()).unwrap());
        assert!(
            !store.ack_one(personal, codex.clone()).unwrap(),
            "чуже не підтверджується — правило не залежить від імені"
        );

        store.lock("тема", codex.clone(), MIN_TTL_SEC, "працюю").unwrap();
        assert_eq!(store.locks().unwrap()[0].holder, codex);
        assert!(store.lock("тема", gemini, MIN_TTL_SEC, "теж").is_err());
    }

    #[test]
    fn agent_names_are_checked() {
        for good in ["Grok", "Claude", "Codex", "gpt-4o", "a", "x_1.2-3"] {
            assert!(Agent::new(good).is_ok(), "«{good}» мало пройти");
        }
        for bad in ["", "*", "Both ", "два слова", "кирилиця", "a/b", "a:b"] {
            assert!(
                Agent::new(bad).is_err(),
                "«{bad}» не мало пройти"
            );
        }
        let long = "x".repeat(MAX_AGENT_CHARS);
        assert!(Agent::new(&long).is_ok(), "рівно стеля — можна");
        assert!(
            Agent::new(&format!("{long}x")).is_err(),
            "на символ довше — вже ні"
        );
    }

    /// ⚠️ `*` відхиляється з ОКРЕМОЮ помилкою, а не як «недозволений символ».
    ///
    /// Різниця не косметична: `*` — не друкарська помилка в імені, а спроба
    /// вжити адресу там, де потрібна особистість. Людина має прочитати саме
    /// це, інакше шукатиме, який символ завадив.
    #[test]
    fn broadcast_as_an_actor_says_what_is_actually_wrong() {
        assert!(matches!(Agent::parse(BROADCAST), Err(Error::BothCannotLock)));
        assert!(matches!(
            Agent::parse(BROADCAST_LEGACY),
            Err(Error::BothCannotLock)
        ));
        assert!(matches!(
            Agent::new(BROADCAST),
            Err(Error::BadAgentName { .. })
        ));
    }

    /// Десеріалізація перевіряє ім'я, а не приймає будь-який рядок.
    ///
    /// Інакше агент на ім'я `*` заходив би через `agent_talk.md` або через
    /// аргумент інструмента — повз усі перевірки конструктора.
    #[test]
    fn deserialising_an_agent_validates_the_name() {
        let ok: Agent = serde_json::from_str("\"Codex\"").unwrap();
        assert_eq!(ok.as_str(), "Codex");
        assert!(serde_json::from_str::<Agent>("\"*\"").is_err());
        assert!(serde_json::from_str::<Agent>("\"два слова\"").is_err());
        assert_eq!(serde_json::to_string(&ok).unwrap(), "\"Codex\"");
    }

    // ── Ролі: діяч і адреса — різні типи ────────────────────────────────

    /// Єдиний шлях, яким широкомовний тримач ще може прийти, — база.
    ///
    /// ⚠️ Раніше це перевірялось на вході `lock()`. Тепер туди таке значення
    /// не передати за типом, тож перевірка переїхала сюди — до читання чужих
    /// даних, де вона й потрібна. Помилка навмисне `BothCannotLock`, а не
    /// «невідомий агент»: у колонці лежить осмислене значення, просто
    /// заборонене для цієї ролі, і людина має прочитати саме це.
    #[test]
    fn locks_reject_broadcast_holder_from_db() {
        let (_tmp, store) = tmp_store();
        {
            let conn = store.conn().unwrap();
            conn.execute(
                "INSERT INTO locks (topic, holder, taken_at, ttl_sec, note)
                 VALUES ('t', ?1, ?2, 7200, '')",
                params![BROADCAST, now_unix()],
            )
            .unwrap();
        }
        assert!(matches!(
            store.locks().unwrap_err(),
            Error::BothCannotLock
        ));
    }

    /// Те саме для історичного написання: база могла лишитись на схемі v1.
    #[test]
    fn locks_reject_legacy_broadcast_holder_from_db() {
        let (_tmp, store) = tmp_store();
        {
            let conn = store.conn().unwrap();
            conn.execute(
                "INSERT INTO locks (topic, holder, taken_at, ttl_sec, note)
                 VALUES ('t', ?1, ?2, 7200, '')",
                params![BROADCAST_LEGACY, now_unix()],
            )
            .unwrap();
        }
        assert!(matches!(
            store.locks().unwrap_err(),
            Error::BothCannotLock
        ));
    }

    /// Діяч і адреса розбираються різними правилами.
    ///
    /// Це і є суть зрізу: «всі» — законна адреса й незаконний діяч, і тепер
    /// цю різницю видно вже в типах, а не лише в рантайм-перевірках.
    #[test]
    fn broadcast_is_an_address_but_never_an_actor() {
        assert_eq!(Recipient::parse(BROADCAST).unwrap(), Recipient::All);
        assert!(
            Agent::parse(BROADCAST).is_err(),
            "«всі» не може бути відправником чи тримачем замка"
        );
        assert!(Agent::parse(BROADCAST_LEGACY).is_err());
    }

    /// Адреса лягає в JSON простим рядком, а не структурою enum.
    ///
    /// ⚠️ `derive(Serialize)` для enum зі значенням дав би `{"One":"Grok"}` —
    /// це мовчки зламало б `agent_talk.md`, який читають обидва агенти.
    /// Тому serde для `Recipient` написаний вручну, і саме це тут закріплено.
    #[test]
    fn recipient_serialises_as_a_plain_string() {
        let one = serde_json::to_string(&Recipient::One(ag("Grok"))).unwrap();
        let all = serde_json::to_string(&Recipient::All).unwrap();
        assert_eq!(one, "\"Grok\"");
        assert_eq!(all, "\"*\"");

        let back: Recipient = serde_json::from_str("\"*\"").unwrap();
        assert_eq!(back, Recipient::All);
        let back: Recipient = serde_json::from_str("\"Both\"").unwrap();
        assert_eq!(back, Recipient::All, "старе написання читається з журналу");
    }

    /// Свіжа база одразу має цільову версію, а не проходить міграцію.
    #[test]
    fn a_new_database_starts_at_the_current_schema_version() {
        let (_tmp, store) = tmp_store();
        assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    }
}
