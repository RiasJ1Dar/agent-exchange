//! Складання HTML-сторінки стану обміну — **без бази, без мережі, без часу**.
//!
//! Ключове рішення цього модуля: він **не знає про SQLite** і не залежить від
//! крейта `store`. На вхід приходять прості власні структури ([`Snapshot`]),
//! на вихід — рядок HTML. Мапінг рядків таблиць у ці структури — робота
//! наступного зрізу; сюди він не просочується навмисне.
//!
//! Що це дає: сторінку можна перевіряти фікстурами. Не треба ні бази, ні
//! вільного порту, ні «зараз» із системного годинника — «зараз» приходить
//! полем [`Snapshot::now`], тож тести детерміновані.
//!
//! Сторінка розрахована і на браузер, і на `curl`: усе важливе позначене
//! **словом**, а не самим лише кольором.

/// Замок на темі: хто тримає, коли взяв і скільки той замок живе.
///
/// `ttl_sec` зберігається як заданий, а не як «скільки лишилось» — інакше
/// структура застаріває тієї ж миті, коли її склали. Скільки лишилось,
/// рахує [`LockView::remaining`] відносно переданого `now`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockView {
    /// Тема, на яку взято замок.
    pub topic: String,
    /// Хто тримає: `Grok`, `Claude` — але з бази може прийти будь-що.
    pub holder: String,
    /// Коли взято, unix-секунди.
    pub taken_at: i64,
    /// Скільки секунд замок дійсний від `taken_at`.
    pub ttl_sec: i64,
    /// Вільна примітка тримача. Текст із бази — екранується.
    pub note: String,
}

impl LockView {
    /// Мить, коли замок протермінується, unix-секунди.
    pub fn expires_at(&self) -> i64 {
        self.taken_at.saturating_add(self.ttl_sec)
    }

    /// Скільки секунд лишилось. Від'ємне — стільки вже протерміновано.
    pub fn remaining(&self, now: i64) -> i64 {
        self.expires_at().saturating_sub(now)
    }

    /// Чи замок уже протермінований на мить `now`.
    ///
    /// Рівність вважається протермінуванням: у мить `expires_at` замок уже
    /// не тримає — інакше два агенти можуть однаково претендувати на цю
    /// саму секунду.
    pub fn is_expired(&self, now: i64) -> bool {
        self.remaining(now) <= 0
    }
}

/// Рядок журналу повідомлень — **без тіла**.
///
/// Тіл тут немає і не буде: вони живуть у `agent_talk.md`, і саме тому в цій
/// структурі немає відповідного поля. Не «ми його не друкуємо», а «його нема
/// що друкувати» — так тіло не може просочитись на сторінку випадково.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageView {
    /// Порядковий id повідомлення.
    pub id: i64,
    /// Час повідомлення, unix-секунди.
    pub ts: i64,
    /// Відправник.
    pub from: String,
    /// Отримувач.
    pub to: String,
    /// Тема.
    pub topic: String,
    /// Операція: `post`, `ack`, `lock`, `unlock`…
    pub op: String,
    /// Чи його підтвердили `ack`.
    pub read: bool,
}

/// Значення `to`, яке означає «всім». Єдине місце, де цей рядок записаний.
///
/// ⚠️ Рядок, а не enum: у базі це TEXT, і сторінка мусить пережити будь-яке
/// значення, зокрема невідоме їй.
pub const BROADCAST: &str = "*";

/// Історичне написання [`BROADCAST`], схема v1.
///
/// ⚠️ Переглядач відкриває базу **read-only** і мігрувати не може за
/// побудовою — отже, він завжди мусить розуміти й старе написання. Це не
/// тимчасовий місток: показати нулі там, де лежить неотримане, гірше, ніж
/// потримати зайву константу.
pub const BROADCAST_LEGACY: &str = "Both";

/// Чи адресоване це повідомлення всім, у будь-якому з написань.
pub fn is_broadcast(to: &str) -> bool {
    to == BROADCAST || to == BROADCAST_LEGACY
}

/// Операція «питання». Рівно `Q`, без нормалізації регістру: `store` пише
/// саме ці чотири літери (`Q`, `A`, `N`, `L`) і відкидає решту ще на вході,
/// тож «а раптом прийде `q`» — вигаданий випадок, а тиха толерантність до
/// нього приховала б справжню розбіжність зі схемою.
pub const OP_QUESTION: &str = "Q";

/// Куди зарахувати одне непрочитане повідомлення.
///
/// Розряди **взаємовиключні** й перекривають усе: кожне непрочитане потрапляє
/// рівно в один. Тому три числа складаються в загальну кількість, і жодне
/// повідомлення не зникає й не рахується двічі.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnreadKind {
    /// `op = Q` — питання, на яке ще ніхто не відповів. Кому адресоване,
    /// значення не має: питання «обом» — теж питання.
    Question,
    /// Адресне (`to != Both`), але не питання: статус чи нотатка особисто
    /// тобі. Прочитати варто, відповідати не обов'язково.
    Personal,
    /// `to = Both` і не питання — широкомовне «до відома».
    Broadcast,
}

/// Розкласти непрочитане за сенсом.
///
/// ⚠️ Порядок перевірок не випадковий: `op = Q` вирішує **першим**. Питання,
/// надіслане всім, лишається питанням — саме воно чекає на відповідь,
/// і сховати його серед широкомовних означало б повернути ту саму сліпоту,
/// заради якої лічильник і розділили.
pub fn classify_unread(to: &str, op: &str) -> UnreadKind {
    if op == OP_QUESTION {
        UnreadKind::Question
    } else if !is_broadcast(to) {
        UnreadKind::Personal
    } else {
        UnreadKind::Broadcast
    }
}

/// Усе, що потрібно сторінці. Складає його наступний зріз, читаючи базу.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Snapshot {
    /// «Зараз» для цієї сторінки, unix-секунди. Передається, а не береться
    /// з годинника, щоб сторінка була відтворюваною в тестах.
    pub now: i64,
    /// Активні й протерміновані замки — розділяє їх сторінка, не виклик.
    pub locks: Vec<LockView>,
    /// Останні повідомлення, у тому порядку, у якому їх треба показати.
    pub messages: Vec<MessageView>,
    /// Непрочитані питання ([`UnreadKind::Question`]) — головне число сторінки.
    pub unread_questions: usize,
    /// Непрочитані адресні, крім питань ([`UnreadKind::Personal`]).
    pub unread_personal: usize,
    /// Непрочитані широкомовні ([`UnreadKind::Broadcast`]).
    pub unread_broadcast: usize,
    /// Скільки непрочитаних у скриньці кожного агента: `(ім'я, скільки)`.
    ///
    /// ⚠️ Це **інша** величина, ніж три лічильники вище: широкомовне
    /// рахується кожному, тож сума цих чисел більша за суму розрядів і ні з
    /// чим не збігається. Саме тому вони показуються найдрібнішим рядком, а
    /// не як підсумок.
    ///
    /// ⚠️ Список, а не два поля: імена беруться з бази. Переглядач не має
    /// знати, як звуть агентів, — інакше третій учасник просто не з'явився б
    /// на сторінці, і людина вирішила б, що його пошта десь ділась.
    pub unread_by_agent: Vec<(String, usize)>,
    /// Коли востаннє робився `render`. `None` — не робився жодного разу.
    pub last_render: Option<i64>,
}

impl Snapshot {
    /// Скільки непрочитаних усього — сума трьох розрядів, без подвійного
    /// рахунку `Both`.
    pub fn unread_total(&self) -> usize {
        self.unread_questions + self.unread_personal + self.unread_broadcast
    }
}

/// Скільки секунд без `render` вважати «дошка застигла».
///
/// Пів години: рендер робиться після кожного обміну, тож довша тиша означає
/// або що ніхто не пише, або що рендер відвалився — і те, і те варто бачити.
pub const STALE_RENDER_SEC: i64 = 30 * 60;

/// Екранує все, що прийшло ззовні.
///
/// ⚠️ Проганяти **обов'язково** через кожне поле з бази. `topic` і `note`
/// пише інший агент; неекранований `<` перетворює сторінку на щось інше.
/// `&` замінюється першим — інакше він зіпсував би вже вставлені сутності.
pub fn escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// Календарна дата з кількості днів від епохи (UTC).
///
/// Алгоритм Гінанта: зсуваємо початок року на березень, тоді високосний день
/// опиняється в кінці й не розриває формулу. Працює і для від'ємних днів.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Абсолютний час, `YYYY-MM-DD ГГ:ХХ:СС UTC`.
///
/// UTC, а не місцевий: без залежностей зона недоступна, а мовчки вдавати
/// місцевий час гірше, ніж написати чесний суфікс.
pub fn fmt_abs(ts: i64) -> String {
    let days = ts.div_euclid(86_400);
    let secs = ts.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Тривалість словами: `45 с`, `12 хв`, `3 год`, `2 д`.
///
/// Скорочення, а не повні слова, свідомо: вони не змінюються за числом, тож
/// не треба тягнути правила відмінювання заради підпису в таблиці.
pub fn fmt_dur(secs: i64) -> String {
    let s = secs.abs();
    if s < 60 {
        format!("{s} с")
    } else if s < 3600 {
        format!("{} хв", s / 60)
    } else if s < 86_400 {
        format!("{} год", s / 3600)
    } else {
        format!("{} д", s / 86_400)
    }
}

/// Відносний час щодо `now`: `12 хв тому`, `через 3 год`, `щойно`.
pub fn fmt_rel(ts: i64, now: i64) -> String {
    let delta = now.saturating_sub(ts);
    if delta.abs() < 5 {
        return "щойно".to_string();
    }
    if delta < 0 {
        format!("через {}", fmt_dur(delta))
    } else {
        format!("{} тому", fmt_dur(delta))
    }
}

/// Абсолютний **і** відносний час разом.
///
/// ⚠️ Саме разом. «2 хв тому» без абсолютного марне, коли сторінку відкрили
/// через годину; абсолютний без відносного змушує рахувати в голові.
fn fmt_both(ts: i64, now: i64) -> String {
    format!("{} ({})", fmt_abs(ts), fmt_rel(ts, now))
}

/// Стилі. Вбудовані рядком: зовнішній файл означав би другий маршрут у
/// `http`, а сторінка тут рівно одна.
const STYLE: &str = "\
body{font:14px/1.5 system-ui,sans-serif;margin:2rem auto;max-width:60rem;padding:0 1rem}
h1{font-size:1.4rem;margin:0 0 .2rem}
h2{font-size:1.1rem;margin:1.6rem 0 .4rem;border-bottom:1px solid #ccc}
table{border-collapse:collapse;width:100%}
th,td{border:1px solid #ccc;padding:.3rem .5rem;text-align:left;vertical-align:top}
th{background:#f2f2f2}
.expired{background:#fde8e8;font-weight:bold}
.unread{font-weight:bold}
.questions{font-size:1.2rem;margin:.4rem 0 0}
.qnum{font-size:2.6rem;font-weight:bold;line-height:1.1}
.calm .qnum{font-weight:normal;color:#555}
.personal{font-size:1rem;margin:.9rem 0 0}
.broadcast{font-size:.85rem;color:#555;margin:.5rem 0 0}
.empty{color:#555;font-style:italic}
.sub{color:#555;font-size:.85rem}
";

/// Повна HTML-сторінка стану обміну.
///
/// Оновлення — через `<meta http-equiv="refresh">`, без JS: сторінку мають
/// однаково читати браузер і `curl`, а автооновлення на JS у `curl` не видно.
pub fn render_page(s: &Snapshot) -> String {
    let mut h = String::with_capacity(4096);

    h.push_str("<!DOCTYPE html>\n<html lang=\"uk\">\n<head>\n");
    h.push_str("<meta charset=\"utf-8\">\n");
    h.push_str("<meta http-equiv=\"refresh\" content=\"5\">\n");
    h.push_str("<title>Стан обміну</title>\n<style>\n");
    h.push_str(STYLE);
    h.push_str("</style>\n</head>\n<body>\n");

    h.push_str("<h1>Стан обміну Grok ↔ Claude</h1>\n");
    h.push_str(&format!(
        "<p class=\"sub\">Сторінка складена: {}. Оновлюється сама кожні 5 с.</p>\n",
        escape(&fmt_abs(s.now))
    ));

    push_unread(&mut h, s);
    push_locks(&mut h, s);
    push_messages(&mut h, s);
    push_render(&mut h, s);

    h.push_str("</body>\n</html>\n");
    h
}

/// Непрочитані — трьома числами за спаданням важливості.
///
/// ⚠️ Одне число тут було брехнею. «Непрочитаних 41» серед яких питань два —
/// це не сорок одна хвилина роботи, а дві: решта — широкомовні статуси на
/// `Both`, які ніхто не чекає прочитаними. Тому головне число сторінки —
/// **питання**, а широкомовні стоять останніми й дрібним шрифтом.
///
/// ⚠️ Нуль питань — добра новина, і оформлюється як добра: те саме велике
/// число, але спокійним сірим (`calm`) і без жодного знака оклику. Порожня
/// черга не має виглядати як аварія, інакше сторінку перестануть читати.
fn push_unread(h: &mut String, s: &Snapshot) {
    h.push_str("<h2>Непрочитані</h2>\n");

    // 1. Питання без відповіді.
    let calm = if s.unread_questions == 0 { " calm" } else { "" };
    h.push_str(&format!(
        "<p class=\"questions{calm}\">Питань без відповіді: \
         <span class=\"qnum\">{}</span></p>\n",
        s.unread_questions
    ));
    h.push_str(if s.unread_questions == 0 {
        "<p class=\"sub\">Черга питань порожня: усе, що питали, уже має відповідь.</p>\n"
    } else {
        "<p class=\"sub\">Це повідомлення з op = Q без ack — саме вони чекають \
         на відповідь.</p>\n"
    });

    // 2. Адресні, крім питань.
    h.push_str(&format!(
        "<p class=\"personal\">Особистих без ack: <span class=\"unread\">{}</span> \
         <span class=\"sub\">(адресних, не Both, і не питання)</span></p>\n",
        s.unread_personal
    ));

    // 3. Широкомовні — дрібно й тихо.
    h.push_str(&format!(
        "<p class=\"broadcast\">Широкомовних без ack: {} (Both, не питання — \
         до відома, відповіді не потребують).</p>\n",
        s.unread_broadcast
    ));

    // Довідково: по скриньках — щоб було видно, хто саме не робить ack.
    //
    // ⚠️ Спільного підсумку тут немає навмисне. Одне велике число і є те, від
    // чого пішов цей зріз: воно однакове і для сорока статусів, і для сорока
    // питань, тобто не каже нічого. Три числа вище вже його заміняють.
    let by_agent = if s.unread_by_agent.is_empty() {
        "агентів ще немає".to_string()
    } else {
        s.unread_by_agent
            .iter()
            // ⚠️ Ім'я з бази — через `escape`: його пише інший агент.
            .map(|(name, n)| format!("{} {n}", escape(name)))
            .collect::<Vec<_>>()
            .join(", ")
    };
    h.push_str(&format!(
        "<p class=\"sub\">По скриньках, широкомовне рахується кожному: {by_agent}.</p>\n"
    ));

    if s.unread_total() == 0 {
        h.push_str("<p class=\"empty\">Непрочитаних немає — ack робиться.</p>\n");
    }
}

fn push_locks(h: &mut String, s: &Snapshot) {
    h.push_str("<h2>Замки</h2>\n");
    if s.locks.is_empty() {
        h.push_str(
            "<p class=\"empty\">Замків немає: жодної теми зараз ніхто не тримає.</p>\n",
        );
        return;
    }
    let expired = s.locks.iter().filter(|l| l.is_expired(s.now)).count();
    h.push_str(&format!(
        "<p>Усього замків: {}, із них протерміновано: {}.</p>\n",
        s.locks.len(),
        expired
    ));
    h.push_str(
        "<table>\n<tr><th>Тема</th><th>Тримач</th><th>Взято</th>\
         <th>Стан</th><th>Примітка</th></tr>\n",
    );
    for l in &s.locks {
        let expired = l.is_expired(s.now);
        // ⚠️ Слово, а не самий лише клас: сторінку читають і в ч/б, і через curl.
        let state = if expired {
            format!("ПРОТЕРМІНОВАНО {} тому", fmt_dur(l.remaining(s.now)))
        } else {
            format!("діє, лишилось {}", fmt_dur(l.remaining(s.now)))
        };
        h.push_str(&format!(
            "<tr class=\"{}\"><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>\n",
            if expired { "expired" } else { "active" },
            escape(&l.topic),
            escape(&l.holder),
            escape(&fmt_both(l.taken_at, s.now)),
            escape(&state),
            escape(&l.note),
        ));
    }
    h.push_str("</table>\n");
}

fn push_messages(h: &mut String, s: &Snapshot) {
    h.push_str("<h2>Останні повідомлення</h2>\n");
    if s.messages.is_empty() {
        h.push_str("<p class=\"empty\">Повідомлень немає: обміну ще не було.</p>\n");
        return;
    }
    h.push_str("<p class=\"sub\">Тіл повідомлень тут немає — вони в agent_talk.md.</p>\n");
    h.push_str(
        "<table>\n<tr><th>id</th><th>Час</th><th>Від</th><th>Кому</th>\
         <th>Тема</th><th>op</th><th>Стан</th></tr>\n",
    );
    for m in &s.messages {
        let state = if m.read { "прочитано" } else { "НЕ ПРОЧИТАНО" };
        h.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td>\
             <td>{}</td><td>{}</td><td class=\"{}\">{}</td></tr>\n",
            m.id,
            escape(&fmt_both(m.ts, s.now)),
            escape(&m.from),
            escape(&m.to),
            escape(&m.topic),
            escape(&m.op),
            if m.read { "read" } else { "unread" },
            state,
        ));
    }
    h.push_str("</table>\n");
}

/// Час останнього `render` — щоб було видно, чи дошка не застигла.
fn push_render(h: &mut String, s: &Snapshot) {
    h.push_str("<h2>Останній render</h2>\n");
    match s.last_render {
        None => h.push_str(
            "<p class=\"empty\">render не робився жодного разу: дошка порожня.</p>\n",
        ),
        Some(ts) => {
            let age = s.now.saturating_sub(ts);
            let mark = if age > STALE_RENDER_SEC {
                " — ЗАСТИГЛА: рендер давно не робився"
            } else {
                ""
            };
            h.push_str(&format!(
                "<p>{}{}</p>\n",
                escape(&fmt_both(ts, s.now)),
                mark
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Фіксоване «зараз»: 2026-08-26 12:00:00 UTC.
    const NOW: i64 = 1_787_745_600;

    fn lock(topic: &str, taken_at: i64, ttl: i64) -> LockView {
        LockView {
            topic: topic.to_string(),
            holder: "Grok".to_string(),
            taken_at,
            ttl_sec: ttl,
            note: "робота".to_string(),
        }
    }

    fn msg(id: i64, read: bool) -> MessageView {
        MessageView {
            id,
            ts: NOW - 120,
            from: "Grok".to_string(),
            to: "Claude".to_string(),
            topic: "ui".to_string(),
            op: "post".to_string(),
            read,
        }
    }

    fn filled() -> Snapshot {
        Snapshot {
            now: NOW,
            locks: vec![lock("ui", NOW - 60, 600), lock("store", NOW - 9000, 600)],
            messages: vec![msg(1, true), msg(2, false)],
            // Живий випадок: 41 «непрочитане» в Claude, а питань — два.
            unread_questions: 2,
            unread_personal: 9,
            unread_broadcast: 30,
            unread_by_agent: vec![("Grok".into(), 31), ("Claude".into(), 41)],
            last_render: Some(NOW - 300),
        }
    }

    /// Лише блок «Непрочитані»: решта сторінки має власні числа й слова, і
    /// перевірки лічильників не повинні на них ловитись.
    fn unread_block(html: &str) -> String {
        let start = html.find("<h2>Непрочитані</h2>").expect("блок непрочитаних");
        let rest = &html[start..];
        let end = rest[1..].find("<h2>").map(|i| i + 1).unwrap_or(rest.len());
        rest[..end].to_string()
    }

    #[test]
    fn empty_snapshot_gives_a_valid_page() {
        let html = render_page(&Snapshot::default());
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("<html"));
        assert!(html.trim_end().ends_with("</html>"));
    }

    #[test]
    fn empty_snapshot_explains_that_there_are_no_locks() {
        // ⚠️ Порожня таблиця без пояснення виглядає як зламана сторінка.
        let html = render_page(&Snapshot::default());
        assert!(html.contains("Замків немає"), "{html}");
        assert!(html.contains("Повідомлень немає"), "{html}");
        assert!(html.contains("render не робився"), "{html}");
    }

    #[test]
    fn expired_lock_is_marked_with_a_word_not_only_a_style() {
        let s = Snapshot {
            now: NOW,
            locks: vec![lock("store", NOW - 9000, 600)],
            ..Snapshot::default()
        };
        let html = render_page(&s);
        assert!(html.contains("ПРОТЕРМІНОВАНО"), "{html}");
        // Слово мусить лишитись і після зняття всіх стилів.
        let no_style = html.replace("expired", "");
        assert!(no_style.contains("ПРОТЕРМІНОВАНО"));
    }

    #[test]
    fn live_lock_is_not_marked_as_expired() {
        let s = Snapshot {
            now: NOW,
            locks: vec![lock("ui", NOW - 60, 600)],
            ..Snapshot::default()
        };
        let html = render_page(&s);
        assert!(!html.contains("ПРОТЕРМІНОВАНО"), "{html}");
        assert!(html.contains("діє, лишилось 9 хв"), "{html}");
    }

    #[test]
    fn lock_expiry_boundary_counts_as_expired() {
        let l = lock("ui", NOW - 600, 600);
        assert!(l.is_expired(NOW));
        assert!(!l.is_expired(NOW - 1));
    }

    #[test]
    fn script_in_topic_is_escaped_and_no_tag_survives() {
        let s = Snapshot {
            now: NOW,
            locks: vec![LockView {
                topic: "<script>alert('x')</script>".to_string(),
                holder: "Grok\"".to_string(),
                taken_at: NOW - 10,
                ttl_sec: 60,
                note: "a & b".to_string(),
            }],
            ..Snapshot::default()
        };
        let html = render_page(&s);
        assert!(!html.contains("<script>"), "{html}");
        assert!(!html.contains("</script>"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
        assert!(html.contains("&#39;"), "{html}");
        assert!(html.contains("&quot;"), "{html}");
        assert!(html.contains("a &amp; b"), "{html}");
    }

    #[test]
    fn script_in_message_topic_is_escaped_too() {
        let mut m = msg(7, false);
        m.topic = "<img src=x onerror=1>".to_string();
        m.from = "<b>".to_string();
        let s = Snapshot { now: NOW, messages: vec![m], ..Snapshot::default() };
        let html = render_page(&s);
        assert!(!html.contains("<img"), "{html}");
        assert!(!html.contains("<b>"), "{html}");
        assert!(html.contains("&lt;img"), "{html}");
    }

    #[test]
    fn escape_replaces_ampersand_first() {
        assert_eq!(escape("&lt;"), "&amp;lt;");
        assert_eq!(escape("<>&\"'"), "&lt;&gt;&amp;&quot;&#39;");
        assert_eq!(escape("звичайний текст"), "звичайний текст");
    }

    /// ⚠️ Головна перевірка зрізу: три числа окремо, а не одне велике.
    #[test]
    fn three_counters_are_shown_separately_not_as_one_lump() {
        let s = Snapshot {
            now: NOW,
            unread_questions: 3,
            unread_personal: 8,
            unread_broadcast: 30,
            // По скриньках навмисне інші числа: вони рахують Both двічі.
            unread_by_agent: vec![("Grok".into(), 33), ("Claude".into(), 38)],
            ..Snapshot::default()
        };
        let block = unread_block(&render_page(&s));
        assert!(
            block.contains("Питань без відповіді: <span class=\"qnum\">3</span>"),
            "{block}"
        );
        assert!(
            block.contains("Особистих без ack: <span class=\"unread\">8</span>"),
            "{block}"
        );
        assert!(block.contains("Широкомовних без ack: 30"), "{block}");
        // Того самого 41 одним числом бути не має — заради цього все й робилось.
        // ⚠️ Ловить і спробу «на всяк випадок» дописати спільний підсумок.
        assert!(!block.contains("41"), "{block}");
        assert!(!block.contains("Разом"), "{block}");
    }

    /// Порядок на сторінці = порядок важливості, і він видимий у HTML.
    #[test]
    fn counters_go_in_order_of_importance() {
        let block = unread_block(&render_page(&filled()));
        let q = block.find("Питань без відповіді").expect("питання");
        let p = block.find("Особистих без ack").expect("особисті");
        let b = block.find("Широкомовних без ack").expect("широкомовні");
        assert!(q < p && p < b, "{block}");
        // Питання — єдине, що набране великим кеглем.
        assert_eq!(block.matches("class=\"qnum\"").count(), 1, "{block}");
        assert!(STYLE.contains(".qnum{font-size:2.6rem"), "{STYLE}");
        assert!(STYLE.contains(".broadcast{font-size:.85rem"), "{STYLE}");
    }

    /// ⚠️ Нуль питань — добра новина. Жодної тривоги в оформленні.
    #[test]
    fn zero_questions_looks_calm() {
        let s = Snapshot {
            now: NOW,
            unread_questions: 0,
            unread_personal: 4,
            unread_broadcast: 37,
            unread_by_agent: vec![("Grok".into(), 20), ("Claude".into(), 21)],
            ..Snapshot::default()
        };
        let block = unread_block(&render_page(&s));
        assert!(
            block.contains("Питань без відповіді: <span class=\"qnum\">0</span>"),
            "{block}"
        );
        assert!(block.contains("Черга питань порожня"), "{block}");
        // Ні знаків оклику, ні великих слів-тривог, ні червоного класу.
        assert!(!block.contains('!'), "{block}");
        assert!(!block.contains("expired"), "{block}");
        assert!(!block.contains("УВАГА"), "{block}");
        // Спокій задається класом, а не самим лише кольором у стилях.
        assert!(block.contains("class=\"questions calm\""), "{block}");
        assert!(STYLE.contains(".calm .qnum{font-weight:normal;color:#555}"), "{STYLE}");
        // Червоне в стилях лишається тільки для протермінованих замків.
        assert_eq!(STYLE.matches("#fde8e8").count(), 1, "{STYLE}");
    }

    /// Ненульові питання спокійного класу не отримують — інакше різниці
    /// між «нуль» і «є що робити» на сторінці не видно.
    #[test]
    fn some_questions_are_not_marked_calm() {
        let block = unread_block(&render_page(&filled()));
        assert!(block.contains("class=\"questions\""), "{block}");
        assert!(!block.contains("calm"), "{block}");
        assert!(block.contains("op = Q"), "{block}");
    }

    #[test]
    fn question_to_both_is_still_a_question() {
        assert_eq!(classify_unread(BROADCAST_LEGACY, OP_QUESTION), UnreadKind::Question);
        assert_eq!(classify_unread("Claude", OP_QUESTION), UnreadKind::Question);
        assert_eq!(classify_unread("Grok", "Q"), UnreadKind::Question);
    }

    #[test]
    fn both_without_question_is_broadcast_not_personal() {
        assert_eq!(classify_unread(BROADCAST_LEGACY, "N"), UnreadKind::Broadcast);
        assert_eq!(classify_unread(BROADCAST_LEGACY, "A"), UnreadKind::Broadcast);
        assert_eq!(classify_unread(BROADCAST_LEGACY, "L"), UnreadKind::Broadcast);
    }

    #[test]
    fn addressed_non_question_is_personal() {
        assert_eq!(classify_unread("Claude", "N"), UnreadKind::Personal);
        assert_eq!(classify_unread("Grok", "A"), UnreadKind::Personal);
        // Невідомий адресат — теж адресний: він точно не «обом».
        assert_eq!(classify_unread("Хтось", "N"), UnreadKind::Personal);
    }

    #[test]
    fn unread_total_sums_the_three_kinds_without_double_counting() {
        // ⚠️ Не `unread_grok + unread_claude`: там Both рахується двічі.
        assert_eq!(filled().unread_total(), 2 + 9 + 30);
    }

    #[test]
    fn mailbox_counts_stay_but_quietly() {
        let block = unread_block(&render_page(&filled()));
        assert!(block.contains("Grok 31, Claude 41"), "{block}");
        // Дрібним шрифтом і останнім рядком, а не головним числом сторінки.
        assert!(block.contains("<p class=\"sub\">По скриньках"), "{block}");
        let q = block.find("Питань без відповіді").expect("питання");
        assert!(block.find("По скриньках").expect("скриньки") > q, "{block}");
    }

    #[test]
    fn zero_unread_is_stated_explicitly() {
        let html = render_page(&Snapshot::default());
        assert!(html.contains("Непрочитаних немає"), "{html}");
        assert!(html.contains("Черга питань порожня"), "{html}");
    }

    #[test]
    fn message_bodies_are_not_on_the_page() {
        // Тіла не може бути структурно: у `MessageView` немає такого поля.
        // Перевірка ловить майбутню спробу його додати найімовірнішим шляхом.
        let html = render_page(&filled());
        assert!(!html.contains("<pre"), "{html}");
        assert!(!html.contains("<textarea"), "{html}");
        assert!(html.contains("Тіл повідомлень тут немає"), "{html}");
    }

    #[test]
    fn read_state_is_a_word() {
        let html = render_page(&filled());
        assert!(html.contains("НЕ ПРОЧИТАНО"), "{html}");
        assert!(html.contains(">прочитано<"), "{html}");
    }

    #[test]
    fn page_has_refresh_and_html_tags() {
        let html = render_page(&filled());
        assert!(html.contains("<meta http-equiv=\"refresh\" content=\"5\">"), "{html}");
        assert!(html.contains("<html"));
        assert!(html.contains("</html>"));
        assert!(html.contains("charset=\"utf-8\""));
    }

    #[test]
    fn times_are_printed_absolute_and_relative() {
        let html = render_page(&filled());
        assert!(html.contains("2026-08-26"), "{html}");
        assert!(html.contains("тому"), "{html}");
    }

    #[test]
    fn last_render_is_shown_and_stale_is_marked() {
        let fresh = render_page(&filled());
        assert!(!fresh.contains("ЗАСТИГЛА"), "{fresh}");

        let stale = Snapshot { last_render: Some(NOW - STALE_RENDER_SEC - 1), ..filled() };
        assert!(render_page(&stale).contains("ЗАСТИГЛА"));
    }

    #[test]
    fn fmt_abs_matches_known_moments() {
        assert_eq!(fmt_abs(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(fmt_abs(NOW), "2026-08-26 12:00:00 UTC");
        // Високосний день не має зсувати дату.
        assert_eq!(fmt_abs(1_709_164_800), "2024-02-29 00:00:00 UTC");
        // Від'ємні секунди не мають панікувати.
        assert_eq!(fmt_abs(-1), "1969-12-31 23:59:59 UTC");
    }

    #[test]
    fn fmt_dur_picks_a_readable_unit() {
        assert_eq!(fmt_dur(45), "45 с");
        assert_eq!(fmt_dur(120), "2 хв");
        assert_eq!(fmt_dur(7200), "2 год");
        assert_eq!(fmt_dur(200_000), "2 д");
        assert_eq!(fmt_dur(-120), "2 хв");
    }

    #[test]
    fn fmt_rel_handles_past_present_and_future() {
        assert_eq!(fmt_rel(NOW, NOW), "щойно");
        assert_eq!(fmt_rel(NOW - 120, NOW), "2 хв тому");
        assert_eq!(fmt_rel(NOW + 120, NOW), "через 2 хв");
    }

    #[test]
    fn extreme_timestamps_do_not_panic() {
        let s = Snapshot {
            now: 0,
            locks: vec![lock("край", i64::MIN, i64::MAX)],
            messages: vec![MessageView { ts: i64::MAX, ..msg(1, false) }],
            last_render: Some(i64::MIN),
            ..Snapshot::default()
        };
        let html = render_page(&s);
        assert!(html.contains("</html>"));
    }
}
