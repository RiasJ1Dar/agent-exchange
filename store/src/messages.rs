use crate::error::Error;
use crate::locks::{check_topic, normalize_topic};
use crate::now_unix;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

pub(crate) const ENVELOPE_V: u32 = 1;
/// Стеля тіла повідомлення — 2000 символів **серіалізованого JSON**
/// (саме того рядка, що лягає в колонку `body`).
///
/// Міряється серіалізація, а не «текст усередині»: у базі зберігається
/// саме вона, і тільки її довжина однозначна для будь-якого `Value`.
/// Перевищення — [`Error::BodyTooLong`], а не тихе обрізання: мовчки
/// зрізаний хвіст повідомлення виглядає як повне, і читач ніколи не
/// дізнається, що частину думки з'їли.
pub const MAX_BODY_CHARS: usize = 2000;

/// Вік прочитаного повідомлення, після якого його прибирає gc.
/// Міряється за `ts_unix` (час запису), не за `read_at`: щойно підтверджений
/// старий лист теж зникає — історія живе в `agent_talk.md`, не в таблиці.
pub const GC_READ_TTL_SEC: i64 = 14 * 24 * 60 * 60;

/// Автоматичний gc на `post`/`ack` — не частіше ніж раз на стільки секунд
/// на одному [`crate::Store`]. Окремого MCP-інструмента немає.
pub const GC_INTERVAL_SEC: i64 = 60 * 60;

/// Адреса «всім», як вона лежить у базі.
///
/// ⚠️ Історично тут стояв рядок `Both` — назва, що мала сенс лише поки
/// агентів рівно двоє. Сервер має бути придатним і для чужої пари ШІ, тож
/// широкомовна адреса більше не називається числом учасників.
///
/// Символ `*` навмисно не може бути іменем агента: інакше агент із таким
/// іменем читав би всю чужу пошту.
pub const BROADCAST: &str = "*";

/// Історичне написання [`BROADCAST`]. Приймається **на читанні назавжди**,
/// а не лише під час міграції: `ui` відкриває базу read-only і мігрувати не
/// може за побудовою, тож він завжди може побачити схему v1.
pub const BROADCAST_LEGACY: &str = "Both";

/// Стеля довжини імені агента.
///
/// 64 — з запасом для будь-якого розумного імені й водночас достатньо мало,
/// щоб ім'я лишалось читабельним у рядку дошки й у журналі.
pub const MAX_AGENT_CHARS: usize = 64;

/// Конкретний агент — той, хто **діє**: пише, підтверджує, тримає замок.
///
/// ⚠️ Тут навмисно **немає** широкомовного варіанта. Раніше «всі» був третім
/// значенням цього ж типу, тож підписати повідомлення від імені всіх або
/// взяти замок «усіма» було синтаксично можливо — і від цього рятували
/// рантайм-перевірки, розкидані по `locks`. Тепер це гарантує тип.
///
/// Адреса, за якою повідомлення **отримують**, — окремий тип [`Recipient`].
///
/// ⚠️ Ім'я — довільний рядок, а не перелік. Сервер не має знати, як звуть
/// агентів: пара `Grok`+`Claude` — це наш випадок, а не властивість
/// протоколу. Чужа людина з іншим набором ШІ має **налаштувати**, а не
/// форкати.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Agent(String);

impl Agent {
    /// Перевірити ім'я і зробити з нього агента.
    ///
    /// Правила навмисне вузькі: ім'я лягає в SQL, у JSON журналу, у рядок
    /// дошки й у назву файла ключа (R5). Розширити перелік символів колись
    /// можна; звузити після того, як хтось назветься — уже ні.
    pub fn new(name: &str) -> Result<Self, Error> {
        if name.is_empty() {
            return Err(Error::BadAgentName {
                name: name.to_string(),
                why: "порожнє ім'я".into(),
            });
        }
        let chars = name.chars().count();
        if chars > MAX_AGENT_CHARS {
            return Err(Error::BadAgentName {
                name: name.to_string(),
                why: format!("{chars} символів, стеля {MAX_AGENT_CHARS}"),
            });
        }
        // ⚠️ Історична широкомовна адреса заборонена ОКРЕМО, бо вона
        // складається з дозволених літер і крізь перевірку символів
        // пройшла б.
        //
        // Ціна помилки конкретна: у схемі v1 колонка `to_agent` тримає саме
        // рядок `Both`, і SQL звіряє з ним досі (`IN ('*','Both')`), бо `ui`
        // мігрувати не вміє. Агент, що назвався б так, читав би всі чужі
        // розсилки в кожній недомігрованій базі.
        if name == BROADCAST_LEGACY {
            return Err(Error::BadAgentName {
                name: name.to_string(),
                why: format!(
                    "«{BROADCAST_LEGACY}» — історична адреса розсилки (схема v1),                      а не ім'я: агент із таким іменем читав би чужу пошту"
                ),
            });
        }
        // ⚠️ `*` не проходить сюди саме через перелік символів — окремої
        // перевірки на нього немає навмисне, бо окрему легше загубити при
        // правці. Агент на ім'я `*` читав би всю чужу пошту.
        if let Some(bad) = name
            .chars()
            .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')))
        {
            return Err(Error::BadAgentName {
                name: name.to_string(),
                why: format!("недозволений символ «{bad}»; можна A-Z a-z 0-9 _ - ."),
            });
        }
        Ok(Agent(name.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Розбір імені діяча з бази або з аргументів інструмента.
    ///
    /// Широкомовні написання тут — **помилка**, а не значення: відправником
    /// чи тримачем замка «всі» бути не можуть. Помилка при цьому окрема від
    /// «недозволений символ»: `*` — не друкарська помилка в імені, а спроба
    /// вжити адресу там, де потрібна особистість.
    pub(crate) fn parse(s: &str) -> Result<Self, Error> {
        if s == BROADCAST || s == BROADCAST_LEGACY {
            return Err(Error::BothCannotLock);
        }
        Agent::new(s)
    }
}

impl std::fmt::Display for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// ⚠️ Serde вручну з тієї ж причини, що й у [`Recipient`]: ім'я має лишатись
/// простим рядком у JSON. Плюс десеріалізація **перевіряє** — інакше через
/// `agent_talk.md` чи аргумент інструмента міг би зайти агент на ім'я `*`.
impl Serialize for Agent {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Agent {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Agent::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// Кому адресовано: конкретному агентові або всім.
///
/// Окремий тип від [`Agent`], бо це інша роль. Адресатом може бути «всі»;
/// відправником — ніколи.
// ⚠️ Не `Copy`: ім'я агента тепер рядок, а не варіант переліку.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Recipient {
    /// Особисто цьому агентові.
    One(Agent),
    /// Усім. У базі лежить як [`BROADCAST`].
    ///
    /// Дефолт свідомо тут, і це не «нейтральне значення заради `derive`».
    /// `Default` потрібен лише [`InboxQuery`]: якби дефолтом стояв конкретний
    /// агент, забутий `agent:` у запиті мовчки відкривав би чужу скриньку.
    /// «Всі» — найменш привілейований варіант: у цій скриньці лежать самі
    /// розсилки, тобто те, що й так адресоване кожному.
    #[default]
    All,
}

impl Recipient {
    pub fn as_str(&self) -> &str {
        match self {
            Recipient::One(a) => a.as_str(),
            Recipient::All => BROADCAST,
        }
    }

    /// Розбір адреси з бази або з аргументів інструмента.
    ///
    /// Обидва написання широкомовної адреси приймаються назавжди: `*` — нове,
    /// `Both` — те, що лежить у вже наявних базах і в чужих копіях, яких ніхто
    /// не мігрував.
    pub(crate) fn parse(s: &str) -> Result<Self, Error> {
        match s {
            BROADCAST | BROADCAST_LEGACY => Ok(Recipient::All),
            other => Ok(Recipient::One(Agent::parse(other)?)),
        }
    }
}

impl std::fmt::Display for Recipient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// ⚠️ Serde вручну, і це не стилістика.
///
/// `derive` для enum зі значенням дав би `{"One":"Grok"}` замість `"Grok"` —
/// тобто мовчки зламав би формат `agent_talk.md`, який читають обидва агенти,
/// і зробив би несумісними всі вже записані рядки. Адреса має лишатись
/// простим рядком, тим самим, що лежить у колонці `to_agent`.
impl Serialize for Recipient {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Recipient {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Recipient::parse(&raw).map_err(serde::de::Error::custom)
    }
}

impl From<Agent> for Recipient {
    fn from(a: Agent) -> Self {
        Recipient::One(a)
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
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Op::Q => "Q",
            Op::A => "A",
            Op::N => "N",
            Op::L => "L",
        }
    }

    pub(crate) fn parse(s: &str) -> Result<Self, Error> {
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
    /// Хто пише. Завжди конкретний — «всі» відправником бути не можуть.
    pub from: Agent,
    /// Кому: особисто чи всім.
    pub to: Recipient,
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
    /// Чия скринька: беруться повідомлення з `to == agent` або широкомовні.
    ///
    /// [`Recipient::All`] означає «лише розсилки» — саме тому воно й дефолт:
    /// найменше, що можна побачити, помилившись.
    pub agent: Recipient,
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
    /// у себе ж у скриньці). За замовчуванням `false` — стара поведінка
    /// недоторкана.
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
pub(crate) fn post_with_conn(conn: &Connection, env: Envelope) -> Result<i64, Error> {
    if env.v != ENVELOPE_V {
        return Err(Error::BadVersion(env.v));
    }
    // Заборонена лише **буквальна** рівність: from=Grok, to=All — легальна
    // розсилка, хоч Grok і побачить її у власному inbox.
    // Порівняння через посилання: ім'я більше не `Copy`, і конверт має
    // доїхати до `INSERT` цілим.
    if matches!(&env.to, Recipient::One(to) if *to == env.from) {
        return Err(Error::SelfMessage(env.from));
    }
    // Тема міряється нарівні з тілом: без цього стеля тіла обходилась
    // сусіднім полем того самого виклику.
    check_topic(&env.topic)?;
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
            // ⚠️ Рядки беруться ДО того, як `env` частинами переїде далі:
            // ім'я більше не `Copy`, і позичити його після переміщення
            // компілятор не дасть.
            env.from.as_str(),
            env.to.as_str(),
            env.topic.as_str(),
            env.op.as_str(),
            body
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Видалити прочитані повідомлення з `ts_unix` старшим за `now - TTL`.
/// Непрочитані (`read_at IS NULL`) не чіпає. Повертає кількість рядків.
pub(crate) fn gc_read_with_conn(conn: &Connection, now: i64) -> Result<usize, Error> {
    let cutoff = now.saturating_sub(GC_READ_TTL_SEC);
    let n = conn.execute(
        "DELETE FROM messages WHERE read_at IS NOT NULL AND ts_unix < ?1",
        params![cutoff],
    )?;
    Ok(n)
}

/// Gc, якщо з попереднього на цьому з'єднанні минула година.
///
/// Вільна функція, не метод `Store`: гард `Mutex<Connection>` уже позичає
/// `self`, і другий `self.maybe_gc` не зібрався б.
fn maybe_gc(
    last_gc_unix: &std::sync::atomic::AtomicI64,
    conn: &Connection,
) -> Result<(), Error> {
    let now = now_unix();
    let last = last_gc_unix.load(std::sync::atomic::Ordering::Relaxed);
    if now.saturating_sub(last) < GC_INTERVAL_SEC {
        return Ok(());
    }
    gc_read_with_conn(conn, now)?;
    last_gc_unix.store(now, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

impl crate::Store {
    /// Прибрати прочитані повідомлення старші за [`GC_READ_TTL_SEC`].
    /// Непрочитані не чіпає. Повертає кількість видалених рядків.
    pub fn gc_read(&self) -> Result<usize, Error> {
        let conn = self.conn.lock().map_err(|_| Error::Poisoned)?;
        let now = now_unix();
        let n = gc_read_with_conn(&conn, now)?;
        self.last_gc_unix
            .store(now, std::sync::atomic::Ordering::Relaxed);
        Ok(n)
    }

    /// Записати повідомлення. Тонка обгортка над [`post_with_conn`]:
    /// бере гард рівно один раз і більше жодного методу `Store` не кличе.
    pub fn post(&self, env: Envelope) -> Result<i64, Error> {
        let conn = self.conn.lock().map_err(|_| Error::Poisoned)?;
        maybe_gc(&self.last_gc_unix, &conn)?;
        post_with_conn(&conn, env)
    }

    /// Скринька агента: усе, що адресоване йому або всім, найстарші першими.
    ///
    /// Делегат до [`Store::inbox_ex`] з дефолтним запитом — поведінка та сама,
    /// що й була. Для фільтрів беріть [`Store::inbox_ex`].
    /// `impl Into<Recipient>` навмисне: `inbox(Agent::Claude, ..)` читається
    /// краще за `inbox(Recipient::One(Agent::Claude), ..)`, а «лише розсилки»
    /// лишається доступним через `Recipient::All`.
    pub fn inbox(
        &self,
        agent: impl Into<Recipient>,
        unread_only: bool,
    ) -> Result<Vec<Message>, Error> {
        self.inbox_ex(InboxQuery {
            agent: agent.into(),
            unread_only,
            ..InboxQuery::default()
        })
    }

    /// Усі повідомлення, найстаріші першими — незалежно від адресата.
    ///
    /// ⚠️ Це не те саме, що об'єднати скриньки всіх агентів, і саме тому
    /// метод окремий. `render` раніше читав скриньки `Grok` і `Claude` й
    /// зшивав результати — тобто **знав імена** й загубив би повідомлення
    /// третього агента, щойно той з'явився б. Журнал має показувати обмін
    /// цілком, а не суму відомих скриньок.
    pub fn all_messages(&self) -> Result<Vec<Message>, Error> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, ts_unix, v, from_agent, to_agent, topic, op, body, read_at
             FROM messages
             ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
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
                    v: v as u32,
                    from: Agent::parse(&from)?,
                    to: Recipient::parse(&to)?,
                    topic,
                    op: Op::parse(&op)?,
                    body: serde_json::from_str(&body)?,
                },
                read_at,
            });
        }
        Ok(out)
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
            Self::read_inbox(&conn, &q.agent, q.unread_only)?
        };

        if q.exclude_own {
            // При `q.agent = All` не виключається ніхто: своєю скринька
            // «лише розсилки» не буває.
            out.retain(|m| Recipient::One(m.envelope.from.clone()) != q.agent);
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
        agent: &Recipient,
        unread_only: bool,
    ) -> Result<Vec<Message>, Error> {
        // ⚠️ Широкомовна адреса перевіряється в ОБОХ написаннях, і це не
        // тимчасовий місток на час міграції. База може бути на схемі v1 —
        // чужа копія, копія для перевірки, або та, яку відкрив read-only
        // процес, що мігрувати не вміє за побудовою.
        let sql = if unread_only {
            "SELECT id, ts_unix, v, from_agent, to_agent, topic, op, body, read_at
             FROM messages
             WHERE (to_agent = ?1 OR to_agent IN ('*','Both')) AND read_at IS NULL
             ORDER BY id ASC"
        } else {
            "SELECT id, ts_unix, v, from_agent, to_agent, topic, op, body, read_at
             FROM messages
             WHERE (to_agent = ?1 OR to_agent IN ('*','Both'))
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
                    to: Recipient::parse(&to)?,
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
        let conn = self.conn.lock().map_err(|_| Error::Poisoned)?;
        maybe_gc(&self.last_gc_unix, &conn)?;
        let n = conn.execute(
            "UPDATE messages SET read_at = ?1 WHERE id = ?2",
            params![now_unix(), id],
        )?;
        if n == 0 {
            return Err(Error::NotFound(id));
        }
        Ok(())
    }

    /// Підтвердити **одне** повідомлення від імені `agent`.
    ///
    /// ⚠️ Це заміна [`Store::ack`] для всього нового коду. Той позначає рядок
    /// **за самим лише `id`**, без перевірки адресата: будь-хто може погасити
    /// чуже повідомлення, і повернути це нічим — `read_at = NULL` в API немає,
    /// а `id` усіх чужих листів лежать відкрито в `agent_talk.md`.
    ///
    /// Тут позначається лише те, що адресоване цьому агентові (`to == agent`
    /// або `to == Both`) **і ще не прочитане**.
    ///
    /// * `true` — позначено;
    /// * `false` — не адресоване цьому агентові, не існує або вже прочитане.
    ///
    /// Жоден із трьох випадків не помилка — так само, як в [`Store::ack_many`]:
    /// «нічого не позначив» — це відповідь, а не збій. Розрізняти їх ззовні
    /// свідомо не можна: інакше `ack_one` став би оракулом, що каже, які чужі
    /// `id` існують.
    pub fn ack_one(&self, id: i64, agent: Agent) -> Result<bool, Error> {
        let conn = self.conn.lock().map_err(|_| Error::Poisoned)?;
        maybe_gc(&self.last_gc_unix, &conn)?;
        let n = conn.execute(
            "UPDATE messages SET read_at = ?1
             WHERE id = ?2
               AND (to_agent = ?3 OR to_agent IN ('*','Both'))
               AND read_at IS NULL",
            params![now_unix(), id, agent.as_str()],
        )?;
        Ok(n > 0)
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
        let mut conn = self.conn.lock().map_err(|_| Error::Poisoned)?;
        maybe_gc(&self.last_gc_unix, &conn)?;
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
                   AND (to_agent = ?3 OR to_agent IN ('*','Both'))
                   AND read_at IS NULL",
            )?;
            for id in ids {
                acked += stmt.execute(params![now, id, agent.as_str()])?;
            }
        }
        tx.commit()?;
        Ok(acked)
    }
}
