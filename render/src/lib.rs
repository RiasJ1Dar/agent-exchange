use exchange_store::{Agent, Lock, Message, Op, Store};
use serde::Serialize;
use std::borrow::Cow;
use std::fmt::Write as FmtWrite;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] exchange_store::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// Файл не в жодному з двох дозволених станів — правку скасовано,
    /// байти на диску лишились як були.
    #[error("маркери у {}: {reason}", path.display())]
    MalformedMarkers { path: PathBuf, reason: String },
}

pub struct RenderOpts<'a> {
    pub db: &'a Path,
    pub now_md: &'a Path,
    /// Тека дошки. `render` її **свідомо не читає**: усе, що йому треба,
    /// виводиться з шляху `now_md` — і NOW.md, і сусідній `agent_talk.md`
    /// (див. `agent_talk_path`). Так одна й та сама логіка працює і на
    /// дошці, і на `tempdir()` у тестах, де жодної «теки дошки» немає.
    ///
    /// Поле лишається в структурі навмисно: `RenderOpts` публічний, його
    /// збирають поза крейтом, і викидання поля зламало б збирачів заради
    /// нульового виграшу. Прибирати — тільки разом із ревізією публічного
    /// API, не в межах косметичної правки.
    pub board_dir: &'a Path,
}

/// Маркери вставок. Контракт узгоджений — не міняти.
const LOCK_BEGIN: &str = "<!-- exchange:lock -->";
const LOCK_END: &str = "<!-- /exchange:lock -->";
const INBOX_BEGIN: &str = "<!-- exchange:inbox -->";
const INBOX_END: &str = "<!-- /exchange:inbox -->";
const HEADING: &str = "# Зараз";

/// Імʼя машинного файла обміну — сусід `now_md` у тій самій теці.
const TALK_NAME: &str = "agent_talk.md";

/// Шапка `agent_talk.md`: два рядки для людини, далі самі JSON-рядки.
const TALK_HEADER: &str = concat!(
    "# agent_talk — дріт агентів. Файл ГЕНЕРОВАНИЙ, правки руками зникнуть.\n",
    "# Формат: один JSON на рядок. Людині сюди дивитись не треба — див. NOW.md.\n"
);

pub fn render_now(opts: RenderOpts<'_>) -> Result<(), Error> {
    if is_log_md(opts.now_md) {
        return Ok(());
    }
    // `opts.board_dir` тут не читається — див. коментар при полі.
    let store = Store::open(opts.db)?;
    render(&store, opts.now_md)
}

pub fn render(store: &Store, now_md: &Path) -> Result<(), Error> {
    if is_log_md(now_md) {
        return Ok(());
    }

    let locks = store.locks()?;
    let mut msgs = store.inbox(Agent::Grok, false)?;
    let extra = store.inbox(Agent::Claude, false)?;
    for m in extra {
        if !msgs.iter().any(|x| x.id == m.id) {
            msgs.push(m);
        }
    }
    msgs.sort_by_key(|m| m.id);

    let lock_block = build_lock_block(&locks);
    let inbox_block = build_inbox_block(&msgs);

    let existing = std::fs::read_to_string(now_md).ok();
    let out = merge_now_md(existing.as_deref(), &lock_block, &inbox_block).map_err(|reason| {
        Error::MalformedMarkers {
            path: now_md.to_path_buf(),
            reason,
        }
    })?;

    write_now_md(now_md, &out)?;

    // Повний обмін живе окремо. NOW.md людина веде руками, тож машинний
    // журнал звідти виселено: у ньому лишився один рядок-підсумок, а весь
    // дріт — тут. Файл цілком генерований, тому переписується з нуля;
    // маркерів у ньому немає, зшивати нема з чим.
    //
    // Шлях виводиться з теки `now_md`, а не з `RenderOpts.board_dir`:
    // публічна сигнатура `render` фіксована (її кличе mcp), і сусідство
    // з NOW.md — єдине, що працює однаково і на дошці, і на tempdir.
    write_now_md(&agent_talk_path(now_md), &build_agent_talk(&msgs))?;
    Ok(())
}

/// `agent_talk.md` поруч із `now_md`.
fn agent_talk_path(now_md: &Path) -> PathBuf {
    now_md.with_file_name(TALK_NAME)
}

/// Один рядок машинного журналу. Порядок полів заданий структурою, а не
/// мапою: `serde_json` без `preserve_order` сортує ключі `Map` алфавітно,
/// і формат «як домовились» розсипався б.
#[derive(Serialize)]
struct TalkLine<'a> {
    id: i64,
    ts: i64,
    from: Agent,
    to: Agent,
    topic: &'a str,
    op: Op,
    read: bool,
    body: &'a serde_json::Value,
}

/// Увесь обмін: шапка, далі по одному компактному JSON на рядок,
/// найновіше знизу (`msgs` уже відсортовані за `id`).
///
/// Санітизації тут свідомо немає: `serde_json` сам екранує все, що могло б
/// зламати рядок, а маркерів у цьому файлі не буває. Єдина вимога до
/// формату — кожен рядок після шапки лишається валідним JSON.
fn build_agent_talk(msgs: &[Message]) -> String {
    let mut s = String::from(TALK_HEADER);
    for m in msgs {
        let line = TalkLine {
            id: m.id,
            ts: m.ts_unix,
            from: m.envelope.from,
            to: m.envelope.to,
            topic: &m.envelope.topic,
            op: m.envelope.op,
            read: m.read_at.is_some(),
            body: &m.envelope.body,
        };
        match serde_json::to_string(&line) {
            Ok(json) => {
                s.push_str(&json);
                s.push('\n');
            }
            // Тіло приїхало з бази вже розібраним, тож сюди не потрапити.
            // Але навіть у цьому разі рядок мусить лишитись валідним JSON,
            // інакше формат ламається для всіх, хто читає файл машиною.
            Err(_) => {
                let _ = writeln!(&mut s, "{{\"id\":{},\"error\":\"serialize\"}}", m.id);
            }
        }
    }
    s
}

/// Санітизація вмісту, що приходить **із бази**, на межі запису у файл.
///
/// Агенти буквально пишуть одне одному про маркери, тож справжня
/// послідовність `-->` цілком може приїхати в темі, нотатці чи тілі
/// повідомлення. Потрапивши в згенерований блок дослівно, вона добудовує
/// другий закривний маркер: наступний render обірве блок на підробці, а
/// хвіст файлу лишиться сміттям. Суворий контракт маркерів тут не рятує —
/// інʼєкція робить пару формально валідною.
///
/// Рішення людини (D2) — **видима** заміна `-->` → `--&gt;`, а не
/// нуль-ширинний символ: файл читають очима й копіюють в інші місця, тому
/// невидима підміна була б пасткою. Ціна свідома: текст у файлі
/// відрізняється від надісланого, і це має бути помітно.
///
/// Одного `-->` досить, щоб знешкодити всі чотири маркери: кожен із них
/// закінчується саме цією послідовністю.
fn sanitize(s: &str) -> String {
    s.replace("-->", "--&gt;")
}

fn build_lock_block(locks: &[Lock]) -> String {
    let mut s = String::new();
    s.push_str(LOCK_BEGIN);
    s.push_str("\n## Замок\n\n");
    if locks.is_empty() {
        s.push_str("- (немає)\n");
    }
    for l in locks {
        let _ = writeln!(
            &mut s,
            "- {} {} ttl={} taken_at={} {}",
            sanitize(&l.topic),
            l.holder,
            l.ttl_sec,
            l.taken_at,
            sanitize(&l.note)
        );
    }
    s.push('\n');
    s.push_str(LOCK_END);
    s
}

/// Три числа непрочитаного замість одного.
///
/// Одне число нічого не означало. На живій дошці воно писало «Claude — 41»,
/// і з тих сорока одного питань (`op = Q`) було рівно два, а решта — статуси
/// на `Both` («w2-ok», «apk 15.74 MB»). Сорок один звучить як катастрофа,
/// два — як робота на пʼять хвилин; саме тому лічильник почали ігнорувати.
/// Розділяємо за тим, що вимагає дії:
///
/// 1. **питання без відповіді** — непрочитані `op = Q`;
/// 2. **особисті** — непрочитані з конкретним адресатом (`to != Both`),
///    крім уже порахованих питань;
/// 3. **широкомовні** — решта непрочитаних (`to = Both`, `op != Q`).
///
/// `Both` як адресат рахується обом — рівно так само, як це робить
/// `Store::inbox` із `unread_only`, тож питання на `Both` стоїть у черзі
/// в обох. Через це широкомовних одне число, а не пара: такі повідомлення
/// лежать в обох скриньках однаково, і розписувати їх «Grok — 31,
/// Claude — 31» означало б удвічі роздути ту саму купу.
///
/// Рахуємо на вже зібраному зрізі, щоб не ходити в базу вдруге й не
/// залежати від того, що там міняється паралельно.
#[derive(Default)]
struct Unread {
    q_grok: usize,
    q_claude: usize,
    personal_grok: usize,
    personal_claude: usize,
    broadcast: usize,
}

impl Unread {
    /// Черга питань порожня — рядок має сказати це спокійно, без ⚠️.
    fn no_questions(&self) -> bool {
        self.q_grok == 0 && self.q_claude == 0
    }
}

fn tally_unread(msgs: &[Message]) -> Unread {
    let mut t = Unread::default();
    for m in msgs.iter().filter(|m| m.read_at.is_none()) {
        let to = m.envelope.to;
        if m.envelope.op == Op::Q {
            // Питання перебиває адресата: `Q` на `Both` — це питання
            // обом, а не широкомовний статус.
            if to == Agent::Grok || to == Agent::Both {
                t.q_grok += 1;
            }
            if to == Agent::Claude || to == Agent::Both {
                t.q_claude += 1;
            }
            continue;
        }
        match to {
            Agent::Grok => t.personal_grok += 1,
            Agent::Claude => t.personal_claude += 1,
            Agent::Both => t.broadcast += 1,
        }
    }
    t
}

/// Позначка непрочитаного в мічених полях рядка inbox.
///
/// Раніше час прочитання йшов останнім і **без назви**, а непрочитане
/// давало голий `-`. У списку, де кожен рядок і так починається з `- `,
/// такий дефіс читався як другий списковий маркер, а не як «немає часу».
const UNREAD_MARK: &str = "непрочитане";

/// Рядок порожньої черги питань.
///
/// Окремим текстом, а не «Grok — 0, Claude — 0» зі знаком тривоги: нуль
/// питань — це нормальний стан, і виглядати він має спокійно. ⚠️ у файлі,
/// який читають щодня, працює лише поки не стоїть там завжди.
const NO_QUESTIONS_LINE: &str = "- питань без відповіді немає";

fn op_str(op: Op) -> &'static str {
    match op {
        Op::Q => "Q",
        Op::A => "A",
        Op::N => "N",
        Op::L => "L",
    }
}

/// Блок inbox у NOW.md — **підсумок, а не журнал**.
///
/// Заміряно на живій дошці: повний список з'їдав 176 рядків із 262, тобто
/// дві третини файла, який людина читає очима, і ліміту в нього не було —
/// друкувались усі повідомлення, і прочитані теж. Рішення людини: журнал
/// переїхав у `agent_talk.md`, а тут лишаються числа й вказівник.
///
/// Маркери на місці — контракт вставок не змінився, змінився лише вміст
/// між ними. Тема останнього повідомлення все ще проходить `sanitize`:
/// вона потрапляє у файл із маркерами, отже інʼєкція `-->` тут так само
/// небезпечна, як була.
fn build_inbox_block(msgs: &[Message]) -> String {
    let mut s = String::new();
    s.push_str(INBOX_BEGIN);
    s.push_str("\n## Inbox\n\n");

    // Порядок рядків = порядок важливості: питання першими й помітно,
    // широкомовні останніми й тихо, одним числом у спільному рядку.
    let t = tally_unread(msgs);
    if t.no_questions() {
        s.push_str(NO_QUESTIONS_LINE);
        s.push('\n');
    } else {
        let _ = writeln!(
            &mut s,
            "- ⚠️ питань без відповіді: Grok — {}, Claude — {}",
            t.q_grok, t.q_claude
        );
    }
    let _ = writeln!(
        &mut s,
        "- особистих непрочитаних: Grok — {}, Claude — {}",
        t.personal_grok, t.personal_claude
    );
    let _ = writeln!(
        &mut s,
        "- широкомовних: {} · усього повідомлень {}",
        t.broadcast,
        msgs.len()
    );

    match msgs.last() {
        None => s.push_str("- (немає)\n"),
        Some(m) => {
            // Кожне поле з назвою: `read_at` тепер теж, а замість голого
            // дефіса для непрочитаного стоїть слово.
            let read_at = match m.read_at {
                Some(t) => t.to_string(),
                None => UNREAD_MARK.to_string(),
            };
            let _ = writeln!(
                &mut s,
                "- останнє: #{} {}→{} {} {} ts_unix={} read_at={}",
                m.id,
                m.envelope.from,
                m.envelope.to,
                sanitize(&m.envelope.topic),
                op_str(m.envelope.op),
                m.ts_unix,
                read_at
            );
        }
    }

    let _ = writeln!(
        &mut s,
        "- повний обмін — {TALK_NAME} (машинний, один JSON на рядок)"
    );

    s.push('\n');
    s.push_str(INBOX_END);
    s
}

const LF: &str = "\n";
const CRLF: &str = "\r\n";

/// Який перенос рядка переважає в наявному файлі.
///
/// Рахуємо `\r\n` проти **всіх** `\n`: файл вважається CRLF-овим лише коли
/// таких переносів більшість (`2 * crlf > усі`). Мішанина з перевагою LF —
/// це LF, і тоді генерація йде колишнім шляхом, без жодного перетворення,
/// тож LF-файл лишається байт у байт таким, як був.
///
/// Навіщо взагалі: блоки будуються з `\n`, і в CRLF-файлі вставка давала
/// змішаний EOL. Файл після цього ламався в інструментах, що ріжуть текст
/// саме по `\r\n`, а людина бачила зайвий розрив перед наступним розділом.
fn dominant_eol(text: &str) -> &'static str {
    let crlf = text.matches(CRLF).count();
    let all = text.matches('\n').count();
    if crlf * 2 > all {
        CRLF
    } else {
        LF
    }
}

/// Переводить згенерований текст (він завжди `\n`-термінований) у EOL файла.
///
/// Для LF це тотожність без жодної алокації — саме тому LF-файл не може
/// змінитись навіть на байт. Для CRLF спершу згортаємо вже наявні `\r\n`
/// назад у `\n`: у блок потрапляє текст із бази (тема, нотатка), і там
/// цілком може лежати справжній `\r\n`. Без згортання вийшло б `\r\r\n`.
fn to_eol<'a>(s: &'a str, eol: &'static str) -> Cow<'a, str> {
    if eol == LF {
        Cow::Borrowed(s)
    } else {
        Cow::Owned(s.replace(CRLF, LF).replace('\n', CRLF))
    }
}

fn strip_bom(s: &str) -> (bool, &str) {
    match s.strip_prefix('\u{feff}') {
        Some(rest) => (true, rest),
        None => (false, s),
    }
}

/// Півінтервал байтів `[begin_pos, end_pos_включно_з_end)` однієї пари маркерів.
type Span = (usize, usize);

/// Класифікує один вид маркера в уже знятому з BOM тексті.
///
/// * `Ok(None)` — маркерів цього виду немає зовсім;
/// * `Ok(Some(span))` — рівно одна коректна пара;
/// * `Err(reason)` — будь-що інше: дублі, непарний, закривний перед відкривним.
fn classify_marker(text: &str, begin: &str, end: &str) -> Result<Option<Span>, String> {
    let nb = text.matches(begin).count();
    let ne = text.matches(end).count();

    match (nb, ne) {
        (0, 0) => Ok(None),
        (1, 1) => {
            let b = text.find(begin).expect("щойно порахований відкривний");
            let e = text.find(end).expect("щойно порахований закривний");
            if b < e {
                Ok(Some((b, e + end.len())))
            } else {
                Err(format!(
                    "закривний маркер {end} стоїть раніше за відкривний {begin}"
                ))
            }
        }
        (1, 0) => Err(format!(
            "непарний відкривний маркер {begin}: закривного {end} немає"
        )),
        (0, 1) => Err(format!(
            "непарний закривний маркер {end}: відкривного {begin} немає"
        )),
        _ => Err(format!(
            "маркер має траплятись рівно раз: {begin} — {nb}, {end} — {ne}"
        )),
    }
}

/// Зшиває нові вставки з наявним файлом. Перед будь-якою правкою файл
/// класифікується, і працюємо лише у двох станах:
///
/// * **рівно одна коректна пара** кожного виду, діапазони не перетинаються
///   → міняється лише вміст між маркерами;
/// * **жодного маркера** (обидва види відсутні) → блоки вставляються після
///   першого рядка `# Зараз` (а якщо заголовка немає — на початок).
///
/// Файла немає або він порожній → мінімальний каркас `# Зараз` + два блоки.
/// Будь-який інший стан — `Err(reason)`, файл не чіпається.
fn merge_now_md(
    existing: Option<&str>,
    lock_block: &str,
    inbox_block: &str,
) -> Result<String, String> {
    let skeleton = |eol: &'static str| {
        let lock = to_eol(lock_block, eol);
        let inbox = to_eol(inbox_block, eol);
        format!("{HEADING}{eol}{eol}{lock}{eol}{eol}{inbox}{eol}")
    };

    let Some(raw) = existing else {
        // Файла ще немає — переймати EOL нема в кого, пишемо LF.
        return Ok(skeleton(LF));
    };
    let (bom, text) = strip_bom(raw);
    let eol = dominant_eol(text);

    // Порожній наявний файл рівносильний відсутньому: інакше він назавжди
    // лишався б без `# Зараз`.
    if text.trim().is_empty() {
        return Ok(with_bom(bom, skeleton(eol)));
    }

    // Далі блоки йдуть уже в переносі файла. Для LF `to_eol` — тотожність,
    // тож і байти лишаються ті самі.
    let lock_block = to_eol(lock_block, eol);
    let inbox_block = to_eol(inbox_block, eol);

    let lock = classify_marker(text, LOCK_BEGIN, LOCK_END)?;
    let inbox = classify_marker(text, INBOX_BEGIN, INBOX_END)?;

    let out = match (lock, inbox) {
        (Some(l), Some(i)) => {
            if l.1 > i.0 && i.1 > l.0 {
                return Err("діапазони exchange:lock та exchange:inbox перетинаються".to_string());
            }
            replace_two_spans(text, l, &lock_block, i, &inbox_block)
        }
        (None, None) => {
            let insert = format!("{lock_block}{eol}{eol}{inbox_block}{eol}{eol}");
            insert_after_heading(text, &insert, eol)
        }
        (Some(_), None) => {
            return Err(
                "змішаний стан: пара exchange:lock є, а exchange:inbox немає зовсім".to_string(),
            )
        }
        (None, Some(_)) => {
            return Err(
                "змішаний стан: пара exchange:inbox є, а exchange:lock немає зовсім".to_string(),
            )
        }
    };

    Ok(with_bom(bom, out))
}

fn with_bom(bom: bool, out: String) -> String {
    if bom {
        format!("\u{feff}{out}")
    } else {
        out
    }
}

/// Міняє вміст двох діапазонів, що не перетинаються, на готові блоки.
/// Усе поза ними — байт у байт як було.
fn replace_two_spans(
    text: &str,
    lock: Span,
    lock_block: &str,
    inbox: Span,
    inbox_block: &str,
) -> String {
    let (first, first_block, second, second_block) = if lock.0 < inbox.0 {
        (lock, lock_block, inbox, inbox_block)
    } else {
        (inbox, inbox_block, lock, lock_block)
    };

    let mut out = String::with_capacity(text.len() + lock_block.len() + inbox_block.len());
    out.push_str(&text[..first.0]);
    out.push_str(first_block);
    out.push_str(&text[first.1..second.0]);
    out.push_str(second_block);
    out.push_str(&text[second.1..]);
    out
}

/// Індекс байта одразу після рядка `# Зараз`; 0, якщо такого рядка немає.
fn heading_end(text: &str) -> usize {
    let mut idx = 0usize;
    for line in text.split_inclusive('\n') {
        if line.trim() == HEADING {
            return idx + line.len();
        }
        idx += line.len();
    }
    0
}

/// Вставляє блоки після заголовка. Порожній рядок-роздільник ставиться
/// переносом самого файла (`eol`), інакше в CRLF-файлі з'являвся самотній
/// `\n` — той самий змішаний EOL, що й ламав вигляд наступного розділу.
fn insert_after_heading(text: &str, insert: &str, eol: &str) -> String {
    let pos = heading_end(text);
    let (head, tail) = text.split_at(pos);

    let mut out = String::with_capacity(text.len() + insert.len() + 2 * eol.len());
    out.push_str(head);
    if !head.is_empty() {
        if !head.ends_with('\n') {
            out.push_str(eol);
        }
        out.push_str(eol);
    }
    out.push_str(insert);
    out.push_str(tail);
    out
}

fn is_log_md(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.ends_with("-log.md"))
        .unwrap_or(false)
}

/// Імʼя тимчасового файла — унікальне на кожен виклик.
///
/// Спільне `.NOW.md.tmp` було пасткою: два процеси (або два потоки) беруть
/// той самий шлях, `File::create` другого обрізає щойно записаний tmp
/// першого, і `rename` кладе на місце NOW.md обрізок. pid розводить процеси,
/// наносекунди й лічильник — виклики всередині одного процесу.
fn tmp_path_for(now_md: &Path) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);

    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let suffix = format!("{pid}.{nanos}.{seq}.tmp");

    match now_md.file_name().and_then(|n| n.to_str()) {
        Some(name) => now_md.with_file_name(format!(".{name}.{suffix}")),
        None => now_md.with_extension(suffix),
    }
}

/// Повний запис у tmp: байти дійшли до диска, не лише до кешу.
///
/// `flush()` на `File` — no-op (буфера в нього немає), тож без `sync_all`
/// після втрати живлення `rename` міг «приземлити» на місце NOW.md порожній
/// або обрізаний файл: метадані каталогу переживають дані.
fn write_tmp_file(tmp: &Path, contents: &str) -> std::io::Result<()> {
    let mut f = std::fs::File::create(tmp)?;
    f.write_all(contents.as_bytes())?;
    f.flush()?;
    f.sync_all()?;
    Ok(())
}

/// Атомарна заміна файла дошки — і NOW.md, і `agent_talk.md` ідуть цим
/// самим шляхом: другої реалізації запису в крейті бути не має.
///
/// Ключове: **жодного `remove_file` перед `rename`**. Раніше файл спершу
/// видаляли, і між видаленням та переймаванням NOW.md не існувало —
/// падіння, вимкнення живлення чи помилка `rename` у цьому вікні лишали
/// людину без рукопису. На Windows `fs::rename` — це `MoveFileEx` з
/// `MOVEFILE_REPLACE_EXISTING`, він і так заміняє наявний файл атомарно,
/// тож видалення нічого не давало й лише відкривало вікно.
fn write_now_md(now_md: &Path, contents: &str) -> Result<(), Error> {
    if is_log_md(now_md) {
        return Ok(());
    }
    if let Some(parent) = now_md.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let tmp = tmp_path_for(now_md);

    // Помилка запису — прибираємо за собою: інакше недописаний tmp
    // лишається в теці назавжди (імʼя тепер унікальне, тож ніхто його
    // вже не перевикористає).
    if let Err(e) = write_tmp_file(&tmp, contents) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }

    match std::fs::rename(&tmp, now_md) {
        Ok(()) => Ok(()),
        // Єдиний випадок, де неатомарний шлях кращий за помилку: ціль
        // тримає відкритою хтось сторонній — антивірус, індексатор,
        // редактор. Атомарно вже не вийде, але лишити людину без NOW.md
        // гірше, ніж переписати файл на місці.
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            let res = std::fs::write(now_md, contents.as_bytes());
            let _ = std::fs::remove_file(&tmp);
            res?;
            Ok(())
        }
        // Решта помилок — це не «спробуймо інакше», це справжня біда
        // (немає теки, немає місця, зник диск). Мовчки переходити на
        // неатомарний запис означало б втратити діагностику й ризикнути
        // наявним файлом.
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exchange_store::{Agent, Envelope, Op, Store};
    use std::fs;
    use tempfile::tempdir;

    fn sample_envelope(from: Agent, to: Agent, topic: &str, op: Op) -> Envelope {
        Envelope {
            v: 1,
            from,
            to,
            topic: topic.to_string(),
            op,
            body: serde_json::json!({"k": "v"}),
        }
    }

    /// Рукописний файл із живими розділами — саме те, що render не сміє з'їсти.
    fn handwritten() -> &'static str {
        "# Зараз\n\n\
         ## Головне\n\n\
         тримаємо курс на маркери\n\n\
         ## Прочитай\n\n\
         - контракт вставок\n\n\
         ## Зроби\n\n\
         - не чіпати чуже\n\n\
         ## Людина\n\n\
         Віктор\n"
    }

    /// Усе, що поза парою блоків: до `lock` і після `inbox`, плюс шов між ними.
    fn outside_blocks(text: &str) -> String {
        let a = text.find(LOCK_BEGIN).expect("маркер lock");
        let mid_a = text.find(LOCK_END).expect("кінець lock") + LOCK_END.len();
        let mid_b = text.find(INBOX_BEGIN).expect("маркер inbox");
        let b = text.find(INBOX_END).expect("кінець inbox") + INBOX_END.len();
        format!(
            "{}\u{1}{}\u{1}{}",
            &text[..a],
            &text[mid_a..mid_b],
            &text[b..]
        )
    }

    #[test]
    fn render_writes_locks_and_inbox_into_temp_now_md() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("exchange.db");
        let now = dir.path().join("NOW.md");
        let store = Store::open(&db).unwrap();

        store.lock("alpha", Agent::Grok, 90, "hold").unwrap();
        let id = store
            .post(sample_envelope(Agent::Claude, Agent::Grok, "alpha", Op::Q))
            .unwrap();
        store.ack(id).unwrap();

        render(&store, &now).unwrap();

        let text = fs::read_to_string(&now).unwrap();
        assert!(text.starts_with("# Зараз\n"));
        assert!(text.contains("## Замок"));
        assert!(text.contains("alpha"));
        assert!(text.contains("ttl=90"));
        assert!(text.contains("## Inbox"));
        assert!(text.contains(&format!("#{id}")));
        assert!(text.contains("Q"));
        // R5: тіл повідомлень у NOW.md більше немає — лише підсумок
        // і вказівник на машинний файл.
        assert!(!text.contains("```json"), "тіло лишилось у NOW.md: {text}");
        assert!(text.contains("особистих непрочитаних:"));
        // Єдине повідомлення ack-нуте, тож черга питань порожня — і рядок
        // про це має бути спокійний.
        assert!(
            text.contains(NO_QUESTIONS_LINE),
            "немає тихого рядка: {text}"
        );
        assert!(!text.contains('⚠'), "тривога на порожній черзі: {text}");
        // P0-R4: час прочитання йде з назвою. Повідомлення тут ack-нуте,
        // тож у рядку має стояти число, а не позначка непрочитаного.
        assert!(text.contains("read_at="), "немає мітки read_at: {text}");
        assert!(
            !text.contains(UNREAD_MARK),
            "ack-нуте показане як непрочитане: {text}"
        );
        assert!(text.contains(TALK_NAME));
        assert!(!now.to_string_lossy().contains("agent-board"));
    }

    #[test]
    fn render_now_uses_store_and_skips_log_files() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("exchange.db");
        let now = dir.path().join("NOW.md");
        let log = dir.path().join("session-log.md");
        fs::write(&log, "KEEP").unwrap();

        let store = Store::open(&db).unwrap();
        store
            .post(sample_envelope(Agent::Grok, Agent::Claude, "beta", Op::N))
            .unwrap();
        store.lock("beta", Agent::Claude, 15, "note").unwrap();
        drop(store);

        render_now(RenderOpts {
            db: &db,
            now_md: &now,
            board_dir: dir.path(),
        })
        .unwrap();

        let text = fs::read_to_string(&now).unwrap();
        assert!(text.contains("# Зараз"));
        assert!(text.contains("beta"));

        render_now(RenderOpts {
            db: &db,
            now_md: &log,
            board_dir: dir.path(),
        })
        .unwrap();
        assert_eq!(fs::read_to_string(&log).unwrap(), "KEEP");
    }

    #[test]
    fn empty_store_renders_placeholders() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("exchange.db");
        let now = dir.path().join("NOW.md");
        let store = Store::open(&db).unwrap();
        render(&store, &now).unwrap();
        let text = fs::read_to_string(&now).unwrap();
        assert!(text.contains("- (немає)"));
        assert!(text.contains("## Inbox"));
    }

    #[test]
    fn keeps_handwritten_sections_when_markers_absent() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("exchange.db");
        let now = dir.path().join("NOW.md");
        fs::write(&now, handwritten()).unwrap();

        let store = Store::open(&db).unwrap();
        store.lock("alpha", Agent::Grok, 90, "hold").unwrap();
        store
            .post(sample_envelope(Agent::Claude, Agent::Grok, "alpha", Op::Q))
            .unwrap();

        render(&store, &now).unwrap();
        let text = fs::read_to_string(&now).unwrap();

        for kept in [
            "## Головне",
            "тримаємо курс на маркери",
            "## Прочитай",
            "контракт вставок",
            "## Зроби",
            "не чіпати чуже",
            "## Людина",
            "Віктор",
        ] {
            assert!(text.contains(kept), "зник рукописний фрагмент: {kept}");
        }

        for marker in [LOCK_BEGIN, LOCK_END, INBOX_BEGIN, INBOX_END] {
            assert!(text.contains(marker), "немає маркера: {marker}");
        }

        // Блоки стали одразу після заголовка, перед рукописним «Головне».
        let heading = text.find(HEADING).unwrap();
        let lock = text.find(LOCK_BEGIN).unwrap();
        let inbox = text.find(INBOX_BEGIN).unwrap();
        let main = text.find("## Головне").unwrap();
        assert!(heading < lock && lock < inbox && inbox < main);
        assert!(text.contains("taken_at="));
        assert!(text.contains("ts_unix="));
    }

    #[test]
    fn second_render_touches_only_the_two_blocks() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("exchange.db");
        let now = dir.path().join("NOW.md");
        fs::write(&now, handwritten()).unwrap();

        let store = Store::open(&db).unwrap();
        store.lock("alpha", Agent::Grok, 90, "hold").unwrap();
        render(&store, &now).unwrap();
        let first = fs::read_to_string(&now).unwrap();

        // Стан бази змінився — вміст блоків має оновитись.
        store.unlock("alpha", Agent::Grok).unwrap();
        // TTL береться в межах [MIN_TTL_SEC, MAX_TTL_SEC] крейта store,
        // інакше значення клампиться і в блок потрапляє не воно.
        store.lock("gamma", Agent::Claude, 4200, "новий").unwrap();
        let id = store
            .post(sample_envelope(Agent::Grok, Agent::Claude, "gamma", Op::N))
            .unwrap();

        render(&store, &now).unwrap();
        let second = fs::read_to_string(&now).unwrap();

        assert_eq!(
            outside_blocks(&first),
            outside_blocks(&second),
            "змінилось щось поза блоками"
        );
        assert!(second.contains("тримаємо курс на маркери"));
        assert!(second.contains("gamma"));
        assert!(second.contains("ttl=4200"));
        assert!(second.contains(&format!("#{id}")));
        assert!(!second.contains("ttl=90"));
        assert_eq!(second.matches(LOCK_BEGIN).count(), 1);
        assert_eq!(second.matches(INBOX_BEGIN).count(), 1);
    }

    #[test]
    fn neighbouring_log_md_is_left_byte_identical() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("exchange.db");
        let now = dir.path().join("NOW.md");
        let log = dir.path().join("xvid-log.md");
        let log_bytes = "# xvid\n\n2026-08-24 рукописний журнал\n".as_bytes();
        fs::write(&log, log_bytes).unwrap();
        fs::write(&now, handwritten()).unwrap();

        let store = Store::open(&db).unwrap();
        store.lock("alpha", Agent::Grok, 90, "hold").unwrap();

        render(&store, &now).unwrap();
        render(&store, &log).unwrap();
        render_now(RenderOpts {
            db: &db,
            now_md: &log,
            board_dir: dir.path(),
        })
        .unwrap();

        assert_eq!(fs::read(&log).unwrap(), log_bytes.to_vec());
    }

    #[test]
    fn empty_store_keeps_file_and_fills_placeholders() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("exchange.db");
        let now = dir.path().join("NOW.md");
        fs::write(&now, handwritten()).unwrap();

        let store = Store::open(&db).unwrap();
        render(&store, &now).unwrap();
        let text = fs::read_to_string(&now).unwrap();

        let lock = &text[text.find(LOCK_BEGIN).unwrap()..text.find(LOCK_END).unwrap()];
        let inbox = &text[text.find(INBOX_BEGIN).unwrap()..text.find(INBOX_END).unwrap()];
        assert!(lock.contains("- (немає)"), "замок без заглушки: {lock}");
        assert!(inbox.contains("- (немає)"), "inbox без заглушки: {inbox}");

        assert!(text.contains("## Головне"));
        assert!(text.contains("## Людина"));
        assert!(text.contains("Віктор"));
    }

    #[test]
    fn bom_is_kept_and_handwriting_survives() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("exchange.db");
        let now = dir.path().join("NOW.md");
        fs::write(&now, format!("\u{feff}{}", handwritten())).unwrap();
        let store = Store::open(&db).unwrap();
        store.lock("alpha", Agent::Grok, 90, "hold").unwrap();
        render(&store, &now).unwrap();
        let raw = fs::read(&now).unwrap();
        assert_eq!(&raw[..3], &[0xEF, 0xBB, 0xBF]);
        let text = String::from_utf8(raw).unwrap();
        assert!(text.contains("## Головне"));
        assert!(text.contains("alpha"));
        assert_eq!(text.matches(LOCK_BEGIN).count(), 1);
    }

    /// Помічник: render мусив відмовитись саме через маркери, і файл
    /// лишитись байт у байт таким, яким був.
    fn refuses_and_keeps_bytes(now: &Path, store: &Store) -> String {
        let before = fs::read(now).unwrap();
        let err = render(store, now).expect_err("render мав відмовитись");
        let reason = match &err {
            Error::MalformedMarkers { path, reason } => {
                assert_eq!(path.as_path(), now, "помилка вказує не на той файл");
                reason.clone()
            }
            other => panic!("очікували MalformedMarkers, отримали {other:?}"),
        };
        assert_eq!(fs::read(now).unwrap(), before, "файл усе-таки змінився");
        reason
    }

    #[test]
    fn unclosed_lock_marker_does_not_eat_the_file() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("exchange.db");
        let now = dir.path().join("NOW.md");
        fs::write(
            &now,
            format!("{HEADING}\n\n{LOCK_BEGIN}\nзламано\n## Головне\nтримати\n"),
        )
        .unwrap();
        let store = Store::open(&db).unwrap();

        refuses_and_keeps_bytes(&now, &store);

        // Вставка заборонена: другий відкривний маркер не з'явився.
        let text = fs::read_to_string(&now).unwrap();
        assert!(text.contains("## Головне"));
        assert!(text.contains("тримати"));
        assert_eq!(text.matches(LOCK_BEGIN).count(), 1);
        assert!(!text.contains(INBOX_BEGIN));
    }

    #[test]
    fn orphan_open_marker_above_heading_is_refused() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("exchange.db");
        let now = dir.path().join("NOW.md");
        fs::write(&now, format!("{LOCK_BEGIN}\nсирота\n\n{}", handwritten())).unwrap();

        let store = Store::open(&db).unwrap();
        store.lock("alpha", Agent::Grok, 90, "hold").unwrap();

        refuses_and_keeps_bytes(&now, &store);

        let text = fs::read_to_string(&now).unwrap();
        for kept in [
            "сирота",
            "## Головне",
            "тримаємо курс на маркери",
            "## Прочитай",
            "контракт вставок",
            "## Зроби",
            "не чіпати чуже",
            "## Людина",
            "Віктор",
        ] {
            assert!(text.contains(kept), "зник рукописний фрагмент: {kept}");
        }
        assert_eq!(text.matches(LOCK_BEGIN).count(), 1);
    }

    #[test]
    fn marker_mentioned_in_prose_twice_is_refused() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("exchange.db");
        let now = dir.path().join("NOW.md");
        let prose = format!(
            "{HEADING}\n\n## Про контракт\n\n\
             Блок починається з {LOCK_BEGIN}, і саме {LOCK_BEGIN} ми шукаємо.\n"
        );
        fs::write(&now, &prose).unwrap();

        let store = Store::open(&db).unwrap();
        refuses_and_keeps_bytes(&now, &store);

        let text = fs::read_to_string(&now).unwrap();
        assert_eq!(text, prose, "абзац змінився");
        assert_eq!(text.matches(LOCK_BEGIN).count(), 2);
    }

    #[test]
    fn two_full_lock_pairs_are_refused() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("exchange.db");
        let now = dir.path().join("NOW.md");
        let text = format!(
            "{HEADING}\n\n\
             {LOCK_BEGIN}\nперший\n{LOCK_END}\n\n\
             {LOCK_BEGIN}\nдругий\n{LOCK_END}\n\n\
             {INBOX_BEGIN}\nпошта\n{INBOX_END}\n\n## Людина\nВіктор\n"
        );
        fs::write(&now, &text).unwrap();

        let store = Store::open(&db).unwrap();
        store.lock("alpha", Agent::Grok, 90, "hold").unwrap();

        refuses_and_keeps_bytes(&now, &store);

        // Ані тихого оновлення першої пари, ані втрати другої.
        let after = fs::read_to_string(&now).unwrap();
        assert_eq!(after, text);
        assert!(after.contains("перший") && after.contains("другий"));
        assert!(!after.contains("ttl=90"));
    }

    #[test]
    fn lock_pair_nested_in_inbox_pair_is_refused() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("exchange.db");
        let now = dir.path().join("NOW.md");
        let text = format!(
            "{HEADING}\n\n\
             {INBOX_BEGIN}\nпошта\n\
             {LOCK_BEGIN}\nзамок усередині\n{LOCK_END}\n\
             ще пошта\n{INBOX_END}\n\n## Людина\nВіктор\n"
        );
        fs::write(&now, &text).unwrap();

        let store = Store::open(&db).unwrap();
        refuses_and_keeps_bytes(&now, &store);

        let after = fs::read_to_string(&now).unwrap();
        assert_eq!(after, text);
        assert!(after.contains("замок усередині") && after.contains("ще пошта"));
    }

    #[test]
    fn closing_marker_before_opening_is_refused() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("exchange.db");
        let now = dir.path().join("NOW.md");
        let text = format!(
            "{HEADING}\n\n\
             {LOCK_END}\nперевернуто\n{LOCK_BEGIN}\n\n\
             {INBOX_BEGIN}\nпошта\n{INBOX_END}\n"
        );
        fs::write(&now, &text).unwrap();

        let store = Store::open(&db).unwrap();
        refuses_and_keeps_bytes(&now, &store);
        assert_eq!(fs::read_to_string(&now).unwrap(), text);
    }

    #[test]
    fn empty_existing_file_gets_heading_and_blocks() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("exchange.db");
        let now = dir.path().join("NOW.md");
        fs::write(&now, "").unwrap();

        let store = Store::open(&db).unwrap();
        store.lock("alpha", Agent::Grok, 90, "hold").unwrap();
        render(&store, &now).unwrap();

        let text = fs::read_to_string(&now).unwrap();
        assert!(text.starts_with(HEADING), "немає заголовка: {text}");
        for marker in [LOCK_BEGIN, LOCK_END, INBOX_BEGIN, INBOX_END] {
            assert_eq!(text.matches(marker).count(), 1, "маркер {marker}");
        }
        assert!(text.contains("ttl=90"));

        // Файл із самих пробілів — те саме.
        let blank = dir.path().join("BLANK.md");
        fs::write(&blank, "   \n\n").unwrap();
        render(&store, &blank).unwrap();
        let text = fs::read_to_string(&blank).unwrap();
        assert!(text.starts_with(HEADING));
        assert!(text.contains(INBOX_END));
    }

    // ==================================================================
    // P0-R4: перенос рядка наявного файла й мітка read_at.
    // ==================================================================

    /// У тексті немає ані самотнього `\n`, ані самотнього `\r`.
    fn assert_pure_crlf(text: &str, ctx: &str) {
        let lf = text.matches('\n').count();
        let cr = text.matches('\r').count();
        let crlf = text.matches(CRLF).count();
        assert!(crlf > 0, "[{ctx}] у файлі взагалі немає CRLF: {text:?}");
        assert_eq!(lf, crlf, "[{ctx}] лишився самотній \\n: {text:?}");
        assert_eq!(cr, crlf, "[{ctx}] лишився самотній \\r: {text:?}");
    }

    /// Порожній store дає детерміновані блоки, тож розкладку вставки можна
    /// зафіксувати побайтово. LF-файл — еталон (він **не сміє** змінитись
    /// від правки EOL), CRLF-файл — той самий текст із `\r\n`.
    #[test]
    fn lf_layout_is_byte_exact_and_crlf_is_its_mirror() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("exchange.db");
        let store = Store::open(&db).unwrap();

        let base = format!("{HEADING}\n\n## Людина\nВіктор\n");
        let lf = dir.path().join("LF.md");
        let crlf = dir.path().join("CRLF.md");
        fs::write(&lf, &base).unwrap();
        fs::write(&crlf, base.replace('\n', CRLF)).unwrap();

        render(&store, &lf).unwrap();
        render(&store, &crlf).unwrap();

        let expected = format!(
            "{HEADING}\n\n{}\n\n{}\n\n\n## Людина\nВіктор\n",
            build_lock_block(&[]),
            build_inbox_block(&[])
        );
        assert_eq!(
            fs::read_to_string(&lf).unwrap(),
            expected,
            "LF-розкладка змінилась"
        );

        let crlf_text = fs::read_to_string(&crlf).unwrap();
        assert_eq!(
            crlf_text,
            expected.replace('\n', CRLF),
            "CRLF-файл не повторює LF-розкладку"
        );
        assert_pure_crlf(&crlf_text, "вставка");

        // Жодного зайвого розриву перед наступним розділом: рівно те, що
        // й у LF-еталоні, і без домішків `\n`.
        assert!(
            crlf_text.contains(&format!("{INBOX_END}\r\n\r\n\r\n## Людина")),
            "шов перед наступним розділом не той: {crlf_text:?}"
        );

        // Повторний render іде вже шляхом заміни — і нічого не зсуває.
        render(&store, &crlf).unwrap();
        assert_eq!(fs::read_to_string(&crlf).unwrap(), crlf_text);
    }

    #[test]
    fn crlf_survives_insert_and_replace_with_live_content() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("exchange.db");
        let now = dir.path().join("NOW.md");
        fs::write(&now, handwritten().replace('\n', CRLF)).unwrap();

        let store = Store::open(&db).unwrap();
        store.lock("alpha", Agent::Grok, 90, "hold").unwrap();
        store
            .post(sample_envelope(Agent::Claude, Agent::Grok, "alpha", Op::Q))
            .unwrap();

        render(&store, &now).unwrap();
        let first = fs::read_to_string(&now).unwrap();
        assert_pure_crlf(&first, "вставка з живим вмістом");
        handwriting_survives(&first, "CRLF");
        exactly_four_real_markers(&first, "CRLF");

        // Друга ітерація: заміна вмісту блоків у вже CRLF-файлі.
        store.unlock("alpha", Agent::Grok).unwrap();
        store.lock("gamma", Agent::Claude, 4200, "новий").unwrap();
        store
            .post(sample_envelope(Agent::Grok, Agent::Claude, "gamma", Op::N))
            .unwrap();

        render(&store, &now).unwrap();
        let second = fs::read_to_string(&now).unwrap();
        assert_pure_crlf(&second, "заміна");
        assert_eq!(
            outside_blocks(&first),
            outside_blocks(&second),
            "заміна зачепила текст поза блоками"
        );
        assert!(second.contains("ttl=4200") && !second.contains("ttl=90"));

        // Усередині блоків теж CRLF — заголовок «## Замок» стоїть на
        // своєму рядку, а не приклеєним до маркера.
        assert!(
            second.contains(&format!("{LOCK_BEGIN}\r\n## Замок\r\n")),
            "блок замка лишився LF: {second:?}"
        );
        assert!(
            second.contains(&format!("{INBOX_BEGIN}\r\n## Inbox\r\n")),
            "блок inbox лишився LF: {second:?}"
        );
    }

    /// Файл із перевагою LF (кілька випадкових `\r\n` усередині рукопису)
    /// лишається LF: правило — саме більшість, а не «є хоч один CRLF».
    #[test]
    fn mostly_lf_file_stays_lf() {
        assert_eq!(dominant_eol("a\nb\nc\r\nd\n"), LF);
        assert_eq!(dominant_eol("a\r\nb\r\nc\nd\r\n"), CRLF);
        assert_eq!(dominant_eol(""), LF);
        assert_eq!(dominant_eol("без переносів"), LF);
        // Рівність не робить файл CRLF-овим: сумнів тлумачиться на користь
        // старої поведінки.
        assert_eq!(dominant_eol("a\r\nb\n"), LF);
    }

    #[test]
    fn to_eol_is_identity_for_lf_and_idempotent_for_crlf() {
        let s = "перший\nдругий\n";
        assert!(matches!(to_eol(s, LF), Cow::Borrowed(_)), "LF щось алокує");
        assert_eq!(to_eol(s, LF), s);
        assert_eq!(to_eol(s, CRLF), "перший\r\nдругий\r\n");
        // Уже CRLF-ний шматок (таке приїжджає з бази в темі чи нотатці)
        // не перетворюється на `\r\r\n`.
        assert_eq!(to_eol("з бази\r\nдалі\n", CRLF), "з бази\r\nдалі\r\n");
        assert!(!to_eol("з бази\r\nдалі\n", CRLF).contains("\r\r"));
    }

    /// Час прочитання має назву, а непрочитане — слово, а не голий дефіс.
    #[test]
    fn inbox_last_line_labels_read_at() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("exchange.db");
        let now = dir.path().join("NOW.md");
        let store = Store::open(&db).unwrap();

        let id = store
            .post(sample_envelope(Agent::Claude, Agent::Grok, "alpha", Op::Q))
            .unwrap();
        render(&store, &now).unwrap();

        let text = fs::read_to_string(&now).unwrap();
        let block = &text[text.find(INBOX_BEGIN).unwrap()..text.find(INBOX_END).unwrap()];
        assert!(
            block.contains(&format!("read_at={UNREAD_MARK}")),
            "непрочитане без однозначної позначки: {block}"
        );
        // Саме те, що плуталось зі списковим маркером.
        assert!(
            !block.contains(" -\n") && !block.contains(" -\r\n"),
            "рядок усе ще закінчується голим дефісом: {block}"
        );

        store.ack(id).unwrap();
        render(&store, &now).unwrap();

        let read_at = store
            .inbox(Agent::Grok, false)
            .unwrap()
            .into_iter()
            .find(|m| m.id == id)
            .expect("повідомлення на місці")
            .read_at
            .expect("ack мав проставити час");

        let text = fs::read_to_string(&now).unwrap();
        let block = &text[text.find(INBOX_BEGIN).unwrap()..text.find(INBOX_END).unwrap()];
        assert!(
            block.contains(&format!("read_at={read_at}")),
            "немає часу прочитання з назвою: {block}"
        );
        assert!(
            !block.contains(UNREAD_MARK),
            "прочитане показане як непрочитане: {block}"
        );
        // Порядок мічених полів не змінився: read_at іде після ts_unix.
        let ts = block.find("ts_unix=").expect("ts_unix");
        let ra = block.find("read_at=").expect("read_at");
        assert!(ts < ra, "read_at виїхав уперед: {block}");
    }

    // ==================================================================
    // R5: журнал агентів виселено з NOW.md у машинний agent_talk.md.
    // ==================================================================

    /// Шапка плюс рядки повідомлень. Повертає (шапка, рядки-JSON).
    fn split_talk(text: &str) -> (Vec<&str>, Vec<&str>) {
        let head: Vec<&str> = text.lines().take_while(|l| l.starts_with('#')).collect();
        let rest: Vec<&str> = text
            .lines()
            .skip(head.len())
            .filter(|l| !l.is_empty())
            .collect();
        (head, rest)
    }

    /// Кожен рядок після шапки розбирається `serde_json`-ом; повертає їх.
    fn parsed_talk(now: &Path) -> Vec<serde_json::Value> {
        let text = fs::read_to_string(agent_talk_path(now)).expect("agent_talk.md");
        let (head, lines) = split_talk(&text);
        assert_eq!(head.len(), 2, "шапка має бути з двох рядків: {text}");
        assert!(head[0].contains("agent_talk"), "не та шапка: {text}");
        lines
            .iter()
            .map(|l| {
                serde_json::from_str::<serde_json::Value>(l)
                    .unwrap_or_else(|e| panic!("рядок не JSON ({e}): {l}"))
            })
            .collect()
    }

    #[test]
    fn agent_talk_lands_next_to_now_md_with_one_line_per_message() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("exchange.db");
        let now = dir.path().join("NOW.md");
        fs::write(&now, handwritten()).unwrap();

        let store = Store::open(&db).unwrap();
        let a = store
            .post(sample_envelope(Agent::Claude, Agent::Grok, "alpha", Op::Q))
            .unwrap();
        let b = store
            .post(sample_envelope(Agent::Grok, Agent::Claude, "beta", Op::A))
            .unwrap();
        let c = store
            .post(sample_envelope(Agent::Grok, Agent::Both, "gamma", Op::N))
            .unwrap();
        store.ack(a).unwrap();

        render(&store, &now).unwrap();

        let talk = agent_talk_path(&now);
        assert_eq!(talk, dir.path().join("agent_talk.md"), "не поруч із NOW.md");

        let rows = parsed_talk(&now);
        assert_eq!(rows.len(), 3, "рядків не стільки, скільки повідомлень");

        // Найновіше знизу, порядок за id.
        let ids: Vec<i64> = rows.iter().map(|r| r["id"].as_i64().unwrap()).collect();
        assert_eq!(ids, vec![a, b, c]);

        // Поля контракту на місці й із правильними значеннями.
        assert_eq!(rows[0]["from"], "Claude");
        assert_eq!(rows[0]["to"], "Grok");
        assert_eq!(rows[0]["topic"], "alpha");
        assert_eq!(rows[0]["op"], "Q");
        assert_eq!(
            rows[0]["read"],
            serde_json::json!(true),
            "ack не врахований"
        );
        assert_eq!(rows[1]["read"], serde_json::json!(false));
        assert_eq!(rows[2]["to"], "Both");
        assert_eq!(rows[2]["body"], serde_json::json!({"k": "v"}));
        assert!(rows[0]["ts"].as_i64().unwrap() > 0);
    }

    #[test]
    fn agent_talk_is_not_written_for_project_logs() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("exchange.db");
        let log = dir.path().join("xvid-log.md");

        let store = Store::open(&db).unwrap();
        store
            .post(sample_envelope(Agent::Grok, Agent::Claude, "alpha", Op::N))
            .unwrap();

        render(&store, &log).unwrap();

        assert!(!log.exists(), "журнал проєкту створено");
        assert!(
            !agent_talk_path(&log).exists(),
            "agent_talk.md з'явився поруч із журналом проєкту"
        );
    }

    #[test]
    fn second_render_rewrites_agent_talk_whole() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("exchange.db");
        let now = dir.path().join("NOW.md");
        fs::write(&now, handwritten()).unwrap();

        let store = Store::open(&db).unwrap();
        store
            .post(sample_envelope(Agent::Claude, Agent::Grok, "alpha", Op::Q))
            .unwrap();
        render(&store, &now).unwrap();
        assert_eq!(parsed_talk(&now).len(), 1);

        store
            .post(sample_envelope(Agent::Grok, Agent::Claude, "beta", Op::A))
            .unwrap();
        render(&store, &now).unwrap();

        let rows = parsed_talk(&now);
        assert_eq!(rows.len(), 2, "переписування дало не той обсяг");
        assert_eq!(rows[0]["topic"], "alpha");
        assert_eq!(rows[1]["topic"], "beta");

        // Переписано з нуля: шапка рівно одна, дублів рядків немає.
        let text = fs::read_to_string(agent_talk_path(&now)).unwrap();
        assert_eq!(text.matches("# agent_talk").count(), 1, "шапка подвоїлась");
        assert_eq!(text.matches("\"topic\":\"alpha\"").count(), 1);
    }

    #[test]
    fn inbox_block_is_a_summary_without_message_bodies() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("exchange.db");
        let now = dir.path().join("NOW.md");
        fs::write(&now, handwritten()).unwrap();

        let store = Store::open(&db).unwrap();
        let read_one = store
            .post(sample_envelope(Agent::Claude, Agent::Grok, "alpha", Op::Q))
            .unwrap();
        store.ack(read_one).unwrap();
        store
            .post(sample_envelope(Agent::Claude, Agent::Grok, "beta", Op::N))
            .unwrap();
        let last = store
            .post(sample_envelope(Agent::Grok, Agent::Both, "gamma", Op::A))
            .unwrap();

        render(&store, &now).unwrap();

        let text = fs::read_to_string(&now).unwrap();
        let block = &text[text.find(INBOX_BEGIN).unwrap()..text.find(INBOX_END).unwrap()];

        // Прочитане не рахується. Єдине питання (`alpha`, Op::Q) ack-нуте,
        // тож черга питань порожня; `beta` — особисте Grokові; `gamma` на
        // `Both` — широкомовне, а не особисте.
        assert!(block.contains(NO_QUESTIONS_LINE), "не тиха черга: {block}");
        assert!(
            block.contains("особистих непрочитаних: Grok — 1, Claude — 0"),
            "не ті числа особистих: {block}"
        );
        assert!(
            block.contains("широкомовних: 1 · усього повідомлень 3"),
            "не той підсумок: {block}"
        );
        assert!(block.contains(&format!("- останнє: #{last}")), "{block}");
        assert!(block.contains("ts_unix="), "{block}");
        // P0-R4: останнє повідомлення тут непрочитане — і це видно словом,
        // а не голим дефісом у кінці рядка.
        assert!(
            block.contains(&format!("read_at={UNREAD_MARK}")),
            "немає позначки непрочитаного: {block}"
        );
        assert!(block.contains(TALK_NAME), "немає вказівника: {block}");

        // Тіл немає, і блок лишився коротким.
        assert!(!block.contains("```json"), "{block}");
        assert!(!block.contains("\"k\""), "тіло просочилось: {block}");
        // Пʼять рядків підсумку (три лічильники, останнє, вказівник) —
        // і жодного «на повідомлення».
        assert_eq!(
            block.lines().filter(|l| l.starts_with("- ")).count(),
            5,
            "блок inbox знову розрісся: {block}"
        );
    }

    /// Головний випадок, заради якого лічильник розділено: три питання
    /// й десять статусів — це «3 і 10», а не одне лячне «13».
    #[test]
    fn questions_and_broadcasts_are_counted_apart() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("exchange.db");
        let now = dir.path().join("NOW.md");
        fs::write(&now, handwritten()).unwrap();

        let store = Store::open(&db).unwrap();
        // Знімок із порожньою чергою — з ним і звірятимемо рукопис.
        render(&store, &now).unwrap();
        let first = fs::read_to_string(&now).unwrap();

        // Питання йдуть на `Both` — саме так їх і ставлять на дошці.
        for i in 0..3 {
            store
                .post(sample_envelope(
                    Agent::Claude,
                    Agent::Both,
                    &format!("q{i}"),
                    Op::Q,
                ))
                .unwrap();
        }
        for i in 0..10 {
            store
                .post(sample_envelope(
                    Agent::Claude,
                    Agent::Both,
                    &format!("w{i}-ok"),
                    Op::N,
                ))
                .unwrap();
        }

        render(&store, &now).unwrap();
        let text = fs::read_to_string(&now).unwrap();
        let block = &text[text.find(INBOX_BEGIN).unwrap()..text.find(INBOX_END).unwrap()];

        // `Q` на `Both` — питання обом, а не широкомовне.
        assert!(
            block.contains("⚠️ питань без відповіді: Grok — 3, Claude — 3"),
            "питання загубились: {block}"
        );
        assert!(
            block.contains("широкомовних: 10 · усього повідомлень 13"),
            "13 замість 3 і 10: {block}"
        );
        // `Both` ніколи не особисте.
        assert!(
            block.contains("особистих непрочитаних: Grok — 0, Claude — 0"),
            "Both порахувалось особистим: {block}"
        );
        assert!(!block.contains(NO_QUESTIONS_LINE), "{block}");

        // Питання стоїть вище за широкомовні.
        let q = block.find("питань без відповіді").unwrap();
        let b = block.find("широкомовних:").unwrap();
        assert!(q < b, "широкомовні перебили питання: {block}");

        // Рукопис поза блоками — байт у байт.
        assert_eq!(
            outside_blocks(&first),
            outside_blocks(&text),
            "лічильник зачепив текст поза парою"
        );
        assert!(text.contains("тримаємо курс на маркери"), "{text}");
    }

    /// Особисте — це конкретний адресат; `Both` без `Q` — широкомовне.
    #[test]
    fn personal_counts_only_named_recipient() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("exchange.db");
        let now = dir.path().join("NOW.md");
        fs::write(&now, handwritten()).unwrap();

        let store = Store::open(&db).unwrap();
        store
            .post(sample_envelope(
                Agent::Claude,
                Agent::Grok,
                "особисте",
                Op::N,
            ))
            .unwrap();
        store
            .post(sample_envelope(Agent::Grok, Agent::Claude, "теж", Op::A))
            .unwrap();
        store
            .post(sample_envelope(
                Agent::Grok,
                Agent::Both,
                "apk 15.74 MB",
                Op::N,
            ))
            .unwrap();

        render(&store, &now).unwrap();
        let text = fs::read_to_string(&now).unwrap();
        let block = &text[text.find(INBOX_BEGIN).unwrap()..text.find(INBOX_END).unwrap()];

        assert!(
            block.contains("особистих непрочитаних: Grok — 1, Claude — 1"),
            "не ті числа особистих: {block}"
        );
        assert!(
            block.contains("широкомовних: 1 · усього повідомлень 3"),
            "Both не порахувалось широкомовним: {block}"
        );
    }

    /// Порожня черга питань не сміє виглядати тривогою.
    #[test]
    fn zero_questions_reads_calm() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("exchange.db");
        let now = dir.path().join("NOW.md");
        fs::write(&now, handwritten()).unwrap();

        let store = Store::open(&db).unwrap();
        for i in 0..5 {
            store
                .post(sample_envelope(
                    Agent::Grok,
                    Agent::Both,
                    &format!("uniffi-ok {i}"),
                    Op::N,
                ))
                .unwrap();
        }
        // Ack-нуте питання теж не має піднімати тривогу.
        let answered = store
            .post(sample_envelope(
                Agent::Claude,
                Agent::Grok,
                "закрите",
                Op::Q,
            ))
            .unwrap();
        store.ack(answered).unwrap();

        render(&store, &now).unwrap();
        let text = fs::read_to_string(&now).unwrap();
        let block = &text[text.find(INBOX_BEGIN).unwrap()..text.find(INBOX_END).unwrap()];

        assert!(block.contains(NO_QUESTIONS_LINE), "не тихий рядок: {block}");
        assert!(!block.contains('⚠'), "тривога на нулі питань: {block}");
        assert!(
            !block.contains("питань без відповіді: Grok"),
            "нулі розписані числами: {block}"
        );
    }

    #[test]
    fn empty_store_gives_bare_header_and_zeroes() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("exchange.db");
        let now = dir.path().join("NOW.md");
        fs::write(&now, handwritten()).unwrap();

        let store = Store::open(&db).unwrap();
        render(&store, &now).unwrap();

        assert!(parsed_talk(&now).is_empty(), "у файлі не лише шапка");
        assert_eq!(
            fs::read_to_string(agent_talk_path(&now)).unwrap(),
            TALK_HEADER,
            "порожній store дав не саму шапку"
        );

        let text = fs::read_to_string(&now).unwrap();
        let block = &text[text.find(INBOX_BEGIN).unwrap()..text.find(INBOX_END).unwrap()];
        assert!(
            block.contains("особистих непрочитаних: Grok — 0, Claude — 0"),
            "немає нулів: {block}"
        );
        assert!(block.contains(NO_QUESTIONS_LINE), "{block}");
        assert!(
            block.contains("широкомовних: 0 · усього повідомлень 0"),
            "{block}"
        );
        assert!(block.contains("- (немає)"), "{block}");
    }

    /// Тіло з переносами, лапками й самими маркерами не сміє розірвати
    /// рядок: один JSON — один рядок, і він валідний.
    #[test]
    fn awkward_body_stays_one_valid_json_line() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("exchange.db");
        let now = dir.path().join("NOW.md");

        let store = Store::open(&db).unwrap();
        let mut env = sample_envelope(Agent::Claude, Agent::Grok, "тема\nз переносом", Op::Q);
        env.body = serde_json::json!({
            "рядки": "перший\nдругий\r\nтретій",
            "лапки": "він сказав \"так\"",
            "маркер": INBOX_END,
            "число": 42,
        });
        store.post(env).unwrap();

        render(&store, &now).unwrap();

        let rows = parsed_talk(&now);
        assert_eq!(rows.len(), 1, "перенос у тілі розірвав рядок");
        assert_eq!(rows[0]["body"]["рядки"], "перший\nдругий\r\nтретій");
        assert_eq!(rows[0]["body"]["лапки"], "він сказав \"так\"");
        assert_eq!(rows[0]["body"]["маркер"], INBOX_END);
        assert_eq!(rows[0]["body"]["число"], 42);
        assert_eq!(rows[0]["topic"], "тема\nз переносом");
    }

    // ==================================================================
    // BUG 3: інʼєкція маркера через вміст із бази.
    //
    // Агент цілком законно пише в темі/нотатці/тілі про самі маркери.
    // Без санітизації такий рядок їде у блок дослівно, наступний render
    // обриває блок на підробці — і хвіст файлу лишається сміттям.
    // ==================================================================

    /// Кожен із чотирьох маркерів трапляється рівно раз — тобто справжніх
    /// маркерів у файлі рівно чотири, дві коректні пари.
    fn exactly_four_real_markers(text: &str, ctx: &str) {
        for m in [LOCK_BEGIN, LOCK_END, INBOX_BEGIN, INBOX_END] {
            assert_eq!(
                text.matches(m).count(),
                1,
                "[{ctx}] маркер {m} трапляється не раз"
            );
        }
        let total: usize = [LOCK_BEGIN, LOCK_END, INBOX_BEGIN, INBOX_END]
            .iter()
            .map(|m| text.matches(m).count())
            .sum();
        assert_eq!(total, 4, "[{ctx}] справжніх маркерів має бути рівно чотири");
    }

    /// Увесь рукопис на місці — інʼєкція не з'їла хвіст файлу.
    fn handwriting_survives(text: &str, ctx: &str) {
        for kept in [
            "## Головне",
            "тримаємо курс на маркери",
            "## Прочитай",
            "контракт вставок",
            "## Зроби",
            "не чіпати чуже",
            "## Людина",
            "Віктор",
        ] {
            assert!(
                text.contains(kept),
                "[{ctx}] зник рукописний фрагмент: {kept}"
            );
        }
    }

    #[test]
    fn sanitize_replaces_only_the_marker_tail() {
        assert_eq!(sanitize("нема стрілки"), "нема стрілки");
        assert_eq!(sanitize("a -> b"), "a -> b");
        assert_eq!(sanitize("<!-- відкрито"), "<!-- відкрито");
        assert_eq!(sanitize("--"), "--");
        assert_eq!(sanitize("x --> y"), "x --&gt; y");
        assert_eq!(sanitize("--> -->"), "--&gt; --&gt;");
        // Жоден із чотирьох маркерів не переживає санітизацію дослівно.
        for m in [LOCK_BEGIN, LOCK_END, INBOX_BEGIN, INBOX_END] {
            let s = sanitize(m);
            assert!(!s.contains(m), "маркер {m} пережив санітизацію: {s}");
            assert!(s.ends_with("--&gt;"), "не той хвіст: {s}");
        }
    }

    #[test]
    fn marker_in_message_body_does_not_break_the_file() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("exchange.db");
        let now = dir.path().join("NOW.md");
        fs::write(&now, handwritten()).unwrap();

        let store = Store::open(&db).unwrap();
        let mut env = sample_envelope(Agent::Claude, Agent::Grok, "alpha", Op::Q);
        env.body = serde_json::json!({
            "про": format!("блок inbox закривається так: {INBOX_END}"),
            "ще": format!("а замок так: {LOCK_END}"),
        });
        store.post(env).unwrap();

        // Другий render — саме той, що падав або обривав блок на підробці.
        render(&store, &now).unwrap();
        render(&store, &now).unwrap();

        let text = fs::read_to_string(&now).unwrap();
        exactly_four_real_markers(&text, "тіло");
        handwriting_survives(&text, "тіло");

        // R5: тіло переїхало в agent_talk.md, тож інʼєкція маркера більше
        // не має де спрацювати — у NOW.md тіла просто немає.
        assert!(
            !text.contains("блок inbox закривається так"),
            "тіло лишилось у NOW.md: {text}"
        );

        // У машинному файлі маркерів немає, отже й санітизації: там тіло
        // мусить лежати дослівно й у валідному JSON.
        let talk = fs::read_to_string(agent_talk_path(&now)).unwrap();
        let line: serde_json::Value =
            serde_json::from_str(talk.lines().nth(2).expect("рядок повідомлення")).unwrap();
        assert_eq!(
            line["body"]["про"],
            serde_json::json!(format!("блок inbox закривається так: {INBOX_END}")),
            "тіло в agent_talk.md спотворене: {talk}"
        );
    }

    #[test]
    fn marker_in_message_topic_does_not_break_the_file() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("exchange.db");
        let now = dir.path().join("NOW.md");
        fs::write(&now, handwritten()).unwrap();

        let store = Store::open(&db).unwrap();
        let topic = format!("про {INBOX_END} і {INBOX_BEGIN}");
        store
            .post(sample_envelope(Agent::Claude, Agent::Grok, &topic, Op::N))
            .unwrap();

        render(&store, &now).unwrap();
        render(&store, &now).unwrap();

        let text = fs::read_to_string(&now).unwrap();
        exactly_four_real_markers(&text, "topic повідомлення");
        handwriting_survives(&text, "topic повідомлення");
        assert!(
            text.contains("про <!-- /exchange:inbox --&gt; і <!-- exchange:inbox --&gt;"),
            "topic не санітизований: {text}"
        );
    }

    #[test]
    fn marker_in_lock_note_and_topic_does_not_break_the_file() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("exchange.db");
        let now = dir.path().join("NOW.md");
        fs::write(&now, handwritten()).unwrap();

        let store = Store::open(&db).unwrap();
        // note з маркером...
        store
            .lock(
                "alpha",
                Agent::Grok,
                90,
                &format!("тримаю, поки не поясню {LOCK_END}"),
            )
            .unwrap();
        // ...і окремий замок із маркером у самій темі.
        store
            .lock(&format!("beta {LOCK_BEGIN}"), Agent::Claude, 120, "друга")
            .unwrap();

        render(&store, &now).unwrap();
        render(&store, &now).unwrap();

        let text = fs::read_to_string(&now).unwrap();
        exactly_four_real_markers(&text, "замок");
        handwriting_survives(&text, "замок");
        assert!(
            text.contains("тримаю, поки не поясню <!-- /exchange:lock --&gt;"),
            "note не санітизована: {text}"
        );
        assert!(
            text.contains("beta <!-- exchange:lock --&gt;"),
            "topic замка не санітизований: {text}"
        );
        assert!(text.contains("ttl=90") && text.contains("ttl=120"));
    }

    #[test]
    fn ordinary_content_is_left_alone() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("exchange.db");
        let now = dir.path().join("NOW.md");
        fs::write(&now, handwritten()).unwrap();

        let store = Store::open(&db).unwrap();
        store
            .lock("alpha", Agent::Grok, 90, "стрілка -> і тег <b>, мінус --")
            .unwrap();
        let mut env = sample_envelope(Agent::Claude, Agent::Grok, "тема без стрілок", Op::Q);
        env.body = serde_json::json!({"текст": "a -> b, <!-- відкрито, 5 - 3 = 2"});
        store.post(env).unwrap();

        render(&store, &now).unwrap();
        let text = fs::read_to_string(&now).unwrap();

        exactly_four_real_markers(&text, "невинний текст");
        handwriting_survives(&text, "невинний текст");
        assert!(
            text.contains("стрілка -> і тег <b>, мінус --"),
            "нотатку зачепило: {text}"
        );
        assert!(text.contains("тема без стрілок"), "тему зачепило: {text}");
        assert!(
            !text.contains("&gt;"),
            "заміна вдарила по невинному тексту: {text}"
        );

        // R5: тіло тепер в agent_talk.md — і воно теж має лишитись цілим.
        let talk = fs::read_to_string(agent_talk_path(&now)).unwrap();
        let line: serde_json::Value =
            serde_json::from_str(talk.lines().nth(2).expect("рядок повідомлення")).unwrap();
        assert_eq!(
            line["body"]["текст"],
            serde_json::json!("a -> b, <!-- відкрито, 5 - 3 = 2"),
            "тіло в agent_talk.md зачепило: {talk}"
        );
    }

    // ==================================================================
    // Property-корпус: детермінований декартів добуток, без залежностей
    // і без випадкового сіду. Два інваріанти:
    //
    //   1. збереження — де маркери не утворюють рівно однієї коректної
    //      неперетинної пари, кожен непорожній рядок входу лишається у
    //      виході (або файл не змінено взагалі);
    //   2. локальність — для добре сформованого входу все поза парою
    //      збігається побайтово (`outside_blocks`).
    // ==================================================================

    /// Рукописна основа: чи є заголовок і чи є взагалі текст.
    #[derive(Clone, Copy, PartialEq)]
    enum Base {
        Plain,
        Empty,
        NoHeading,
    }

    /// Куди кладеться цікавий (для двох відкривних — зайвий) lock-маркер.
    #[derive(Clone, Copy, PartialEq)]
    enum Placement {
        AboveHeading,
        InProse,
        InsideInbox,
    }

    /// Форма lock-маркерів: скільки відкривних і що із закривним.
    #[derive(Clone, Copy, PartialEq)]
    enum Shape {
        /// 0 відкривних, 0 закривних, inbox теж немає.
        NoMarkers,
        /// 0 відкривних lock, але коректна пара inbox — змішаний стан.
        InboxOnly,
        /// 1 відкривний + 1 закривний у правильному порядку.
        PairOk,
        /// 1 відкривний, закривного немає.
        OpenOnly,
        /// 1 + 1, але закривний стоїть раніше за відкривний.
        ClosedFirst,
        /// 0 відкривних, 1 закривний-сирота.
        OrphanClose,
        /// 2 відкривних + 1 закривний.
        TwoOpensOneClose,
        /// 2 відкривних, закривного немає.
        TwoOpensNoClose,
    }

    /// Що render зобов'язаний зробити з цим входом.
    #[derive(Clone, Copy, PartialEq)]
    enum Expect {
        /// Маркерів немає — блоки вставляються, рукопис лишається цілим.
        InsertsBlocks,
        /// Рівно одна коректна неперетинна пара кожного виду — міняється
        /// лише вміст між маркерами.
        ReplacesBlocks,
        /// Будь-який інший стан — відмова, байти на диску не рухаються.
        Refused,
    }

    struct Case {
        name: String,
        input: String,
        expect: Expect,
    }

    fn base_name(b: Base) -> &'static str {
        match b {
            Base::Plain => "звичайний",
            Base::Empty => "порожній",
            Base::NoHeading => "без-заголовка",
        }
    }

    fn placement_name(p: Option<Placement>) -> &'static str {
        match p {
            None => "—",
            Some(Placement::AboveHeading) => "вище-Зараз",
            Some(Placement::InProse) => "у-прозі",
            Some(Placement::InsideInbox) => "усередині-inbox",
        }
    }

    fn shape_name(s: Shape) -> &'static str {
        match s {
            Shape::NoMarkers => "0-відкривних/без-inbox",
            Shape::InboxOnly => "0-відкривних/inbox-пара",
            Shape::PairOk => "1-відкривний/закривний-є",
            Shape::OpenOnly => "1-відкривний/закривного-нема",
            Shape::ClosedFirst => "1-відкривний/закривний-раніше",
            Shape::TwoOpensOneClose => "2-відкривних/закривний-є",
            Shape::TwoOpensNoClose => "2-відкривних/закривного-нема",
            Shape::OrphanClose => "0-відкривних/закривний-сирота",
        }
    }

    fn base_lines(b: Base) -> Vec<String> {
        let s = |x: &str| x.to_string();
        match b {
            Base::Plain => vec![
                s(HEADING),
                s(""),
                s("## Головне"),
                s("тримаємо курс на маркери"),
                s(""),
                s("## Людина"),
                s("Віктор"),
            ],
            Base::NoHeading => vec![
                s("## Головне"),
                s("тримаємо курс на маркери"),
                s(""),
                s("## Людина"),
                s("Віктор"),
            ],
            Base::Empty => vec![],
        }
    }

    fn lock_lines(shape: Shape) -> Vec<String> {
        let s = |x: &str| x.to_string();
        match shape {
            Shape::NoMarkers | Shape::InboxOnly => vec![],
            Shape::PairOk => vec![s(LOCK_BEGIN), s("замок"), s(LOCK_END)],
            Shape::OpenOnly => vec![s(LOCK_BEGIN), s("замок без кінця")],
            Shape::ClosedFirst => vec![s(LOCK_END), s("перевернуто"), s(LOCK_BEGIN)],
            Shape::OrphanClose => vec![s("сирота"), s(LOCK_END)],
            Shape::TwoOpensOneClose => vec![
                s(LOCK_BEGIN),
                s("перший"),
                s(LOCK_END),
                s(LOCK_BEGIN),
                s("другий"),
            ],
            Shape::TwoOpensNoClose => {
                vec![s(LOCK_BEGIN), s("перший"), s(LOCK_BEGIN), s("другий")]
            }
        }
    }

    fn inbox_pair() -> Vec<String> {
        vec![
            INBOX_BEGIN.to_string(),
            "пошта".to_string(),
            INBOX_END.to_string(),
        ]
    }

    /// Рядок, після якого починається «проза посеред тексту».
    fn prose_pos(lines: &[String]) -> usize {
        lines
            .iter()
            .position(|l| l == "тримаємо курс на маркери")
            .map(|i| i + 1)
            .unwrap_or(lines.len())
    }

    fn push_blank(lines: &mut Vec<String>) {
        if !lines.is_empty() {
            lines.push(String::new());
        }
    }

    fn make_case(
        bom: bool,
        crlf: bool,
        base: Base,
        placement: Option<Placement>,
        shape: Shape,
    ) -> Case {
        let mut lines = base_lines(base);
        let lock = lock_lines(shape);

        match shape {
            Shape::NoMarkers => {}
            Shape::InboxOnly => {
                push_blank(&mut lines);
                lines.extend(inbox_pair());
            }
            _ => match placement.expect("формі з маркерами потрібне розташування")
            {
                // Зайвий (чи єдиний) lock — вище `# Зараз`; inbox тут немає
                // зовсім, тож коректна пара lock дає змішаний стан.
                Placement::AboveHeading => {
                    let mut head = lock.clone();
                    head.push(String::new());
                    let _: Vec<String> = lines.splice(0..0, head).collect();
                }
                // lock у прозі, одразу за ним коректна пара inbox — єдина
                // комбінація, де добре сформований вхід реально можливий.
                Placement::InProse => {
                    let at = prose_pos(&lines);
                    let mut chunk = vec![String::new()];
                    chunk.extend(lock.clone());
                    chunk.push(String::new());
                    chunk.extend(inbox_pair());
                    chunk.push(String::new());
                    let _: Vec<String> = lines.splice(at..at, chunk).collect();
                }
                // lock усередині пари inbox — діапазони перетинаються.
                Placement::InsideInbox => {
                    push_blank(&mut lines);
                    lines.push(INBOX_BEGIN.to_string());
                    lines.push("пошта".to_string());
                    lines.extend(lock.clone());
                    lines.push("ще пошта".to_string());
                    lines.push(INBOX_END.to_string());
                }
            },
        }

        let mut text = lines.join("\n");
        if !text.is_empty() {
            text.push('\n');
        }
        if crlf {
            text = text.replace('\n', "\r\n");
        }
        if bom {
            text.insert(0, '\u{feff}');
        }

        let expect = match (shape, placement) {
            (Shape::NoMarkers, _) => Expect::InsertsBlocks,
            (Shape::PairOk, Some(Placement::InProse)) => Expect::ReplacesBlocks,
            _ => Expect::Refused,
        };

        let name = format!(
            "bom={} eol={} рукопис={} lock={} місце={}",
            if bom { "є" } else { "нема" },
            if crlf { "CRLF" } else { "LF" },
            base_name(base),
            shape_name(shape),
            placement_name(placement),
        );

        Case {
            name,
            input: text,
            expect,
        }
    }

    /// Повний корпус: 2 (BOM) × 2 (EOL) × 3 (рукопис) × 20 форм маркерів.
    fn corpus() -> Vec<Case> {
        let mut out = Vec::new();
        for &bom in &[false, true] {
            for &crlf in &[false, true] {
                for &base in &[Base::Plain, Base::Empty, Base::NoHeading] {
                    for &shape in &[Shape::NoMarkers, Shape::InboxOnly] {
                        out.push(make_case(bom, crlf, base, None, shape));
                    }
                    for &shape in &[
                        Shape::PairOk,
                        Shape::OpenOnly,
                        Shape::ClosedFirst,
                        Shape::OrphanClose,
                        Shape::TwoOpensOneClose,
                        Shape::TwoOpensNoClose,
                    ] {
                        for &placement in &[
                            Placement::AboveHeading,
                            Placement::InProse,
                            Placement::InsideInbox,
                        ] {
                            out.push(make_case(bom, crlf, base, Some(placement), shape));
                        }
                    }
                }
            }
        }
        out
    }

    fn corpus_store(dir: &Path) -> Store {
        let store = Store::open(&dir.join("exchange.db")).unwrap();
        store.lock("alpha", Agent::Grok, 90, "hold").unwrap();
        store
            .post(sample_envelope(Agent::Claude, Agent::Grok, "alpha", Op::Q))
            .unwrap();
        store
    }

    #[test]
    fn corpus_is_the_declared_product() {
        let cases = corpus();
        assert_eq!(cases.len(), 240, "корпус має бути 2×2×3×20");

        let mut names: Vec<&str> = cases.iter().map(|c| c.name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), cases.len(), "назви випадків не унікальні");

        let count = |e: Expect| cases.iter().filter(|c| c.expect == e).count();
        assert_eq!(count(Expect::Refused), 216);
        assert_eq!(count(Expect::InsertsBlocks), 12);
        assert_eq!(count(Expect::ReplacesBlocks), 12);
    }

    /// Інваріант 1: там, де маркери не утворюють рівно однієї коректної
    /// неперетинної пари, рукопис не зникає — або файл узагалі не змінено.
    #[test]
    fn corpus_never_loses_a_handwritten_line() {
        let dir = tempdir().unwrap();
        let store = corpus_store(dir.path());

        for (i, case) in corpus().iter().enumerate() {
            if case.expect == Expect::ReplacesBlocks {
                continue; // добре сформований вхід — інша перевірка
            }
            let now = dir.path().join(format!("case_{i}.md"));
            fs::write(&now, case.input.as_bytes()).unwrap();
            let res = render(&store, &now);
            let after = fs::read(&now).unwrap();

            match case.expect {
                Expect::Refused => {
                    let err = res
                        .err()
                        .unwrap_or_else(|| panic!("[{}] render мав відмовитись", case.name));
                    assert!(
                        matches!(err, Error::MalformedMarkers { .. }),
                        "[{}] не той тип помилки: {err:?}",
                        case.name
                    );
                    assert_eq!(
                        after,
                        case.input.as_bytes(),
                        "[{}] файл змінено попри відмову",
                        case.name
                    );
                }
                Expect::InsertsBlocks => {
                    if let Err(e) = res {
                        panic!("[{}] render мав спрацювати: {e}", case.name);
                    }
                    let out = String::from_utf8(after).unwrap();
                    // BOM зіставляємо окремо: у рядках він лише заважає.
                    for line in case.input.trim_start_matches('\u{feff}').lines() {
                        if line.trim().is_empty() {
                            continue;
                        }
                        assert!(out.contains(line), "[{}] зник рядок {line:?}", case.name);
                    }
                    assert_eq!(
                        out.starts_with('\u{feff}'),
                        case.input.starts_with('\u{feff}'),
                        "[{}] BOM не збережено як було",
                        case.name
                    );
                    for m in [LOCK_BEGIN, LOCK_END, INBOX_BEGIN, INBOX_END] {
                        assert_eq!(
                            out.matches(m).count(),
                            1,
                            "[{}] маркер {m} трапляється не раз",
                            case.name
                        );
                    }
                    assert!(
                        out.contains("ttl=90"),
                        "[{}] блок замка порожній",
                        case.name
                    );
                }
                Expect::ReplacesBlocks => unreachable!(),
            }
        }
    }

    /// Інваріант 2: для добре сформованого входу все поза парою збігається
    /// побайтово — і після першого render, і після повторного зі зміненою
    /// базою.
    #[test]
    fn corpus_wellformed_changes_nothing_outside_the_pair() {
        let dir = tempdir().unwrap();
        let store = corpus_store(dir.path());

        let cases: Vec<Case> = corpus()
            .into_iter()
            .filter(|c| c.expect != Expect::Refused)
            .collect();
        assert_eq!(cases.len(), 24, "щасливих випадків має бути 24");

        let mut paths = Vec::new();
        let mut first = Vec::new();
        for (i, case) in cases.iter().enumerate() {
            let now = dir.path().join(format!("well_{i}.md"));
            fs::write(&now, case.input.as_bytes()).unwrap();
            if let Err(e) = render(&store, &now) {
                panic!("[{}] render мав спрацювати: {e}", case.name);
            }
            let text = fs::read_to_string(&now).unwrap();
            if case.expect == Expect::ReplacesBlocks {
                assert_eq!(
                    outside_blocks(&case.input),
                    outside_blocks(&text),
                    "[{}] перший render зачепив текст поза парою",
                    case.name
                );
            }
            first.push(text);
            paths.push(now);
        }

        // База змінилась — оновитись має рівно вміст блоків.
        store.unlock("alpha", Agent::Grok).unwrap();
        store.lock("gamma", Agent::Claude, 4200, "новий").unwrap();
        let id = store
            .post(sample_envelope(Agent::Grok, Agent::Claude, "gamma", Op::N))
            .unwrap();

        for (i, case) in cases.iter().enumerate() {
            if let Err(e) = render(&store, &paths[i]) {
                panic!("[{}] повторний render мав спрацювати: {e}", case.name);
            }
            let second = fs::read_to_string(&paths[i]).unwrap();
            assert_eq!(
                outside_blocks(&first[i]),
                outside_blocks(&second),
                "[{}] повторний render зачепив текст поза парою",
                case.name
            );
            assert!(
                second.contains("ttl=4200"),
                "[{}] блок замка не оновився",
                case.name
            );
            assert!(
                second.contains(&format!("#{id}")),
                "[{}] блок inbox не оновився",
                case.name
            );
            assert!(
                !second.contains("ttl=90"),
                "[{}] знятий замок лишився у файлі",
                case.name
            );
            assert_eq!(second.matches(LOCK_BEGIN).count(), 1, "[{}]", case.name);
            assert_eq!(second.matches(INBOX_BEGIN).count(), 1, "[{}]", case.name);
            assert_eq!(
                second.starts_with('\u{feff}'),
                case.input.starts_with('\u{feff}'),
                "[{}] BOM не збережено як було",
                case.name
            );
        }
    }
    /// Скільки `.tmp` лишилось у теці після запису.
    fn tmp_leftovers(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        v.sort();
        v
    }

    #[test]
    fn successful_write_leaves_no_tmp_and_keeps_contents() {
        let dir = tempdir().unwrap();
        let now = dir.path().join("NOW.md");

        write_now_md(&now, "# Зараз\n\nрукопис\n").unwrap();

        assert_eq!(
            fs::read_to_string(&now).unwrap(),
            "# Зараз\n\nрукопис\n",
            "вміст не доїхав до NOW.md"
        );
        assert_eq!(
            tmp_leftovers(dir.path()),
            Vec::<String>::new(),
            "після успішного запису tmp лишився на диску"
        );
    }

    #[test]
    fn two_writes_leave_no_tmp_files() {
        let dir = tempdir().unwrap();
        let now = dir.path().join("NOW.md");

        write_now_md(&now, "перший\n").unwrap();
        write_now_md(&now, "другий\n").unwrap();

        assert_eq!(
            fs::read_to_string(&now).unwrap(),
            "другий\n",
            "другий запис не замінив перший"
        );
        assert_eq!(
            tmp_leftovers(dir.path()),
            Vec::<String>::new(),
            "два послідовних записи лишили сміття"
        );
    }

    #[test]
    fn tmp_names_differ_between_calls() {
        let dir = tempdir().unwrap();
        let now = dir.path().join("NOW.md");

        let a = tmp_path_for(&now);
        let b = tmp_path_for(&now);

        assert_ne!(a, b, "два виклики взяли те саме імʼя tmp");
        for p in [&a, &b] {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            assert!(name.ends_with(".tmp"), "не .tmp: {name}");
            assert!(name.starts_with(".NOW.md."), "не при своєму файлі: {name}");
            assert!(
                name.contains(&std::process::id().to_string()),
                "у tmp немає pid: {name}"
            );
        }
    }

    /// Запис лишає NOW.md на місці **весь час**: у теці немає моменту,
    /// коли файл зник. Перевіряємо те, що можна перевірити чесно —
    /// що видалення перед переймаванням у коді більше немає й другий
    /// запис не проходить через стан «файлу нема».
    #[test]
    fn rewrite_never_removes_the_target_first() {
        let dir = tempdir().unwrap();
        let now = dir.path().join("NOW.md");
        write_now_md(&now, "рукопис людини\n").unwrap();

        let before = fs::metadata(&now).unwrap().len();
        write_now_md(&now, "рукопис людини, доповнений\n").unwrap();

        assert!(now.exists(), "NOW.md зник після перезапису");
        assert!(fs::metadata(&now).unwrap().len() > before);
        assert_eq!(
            fs::read_to_string(&now).unwrap(),
            "рукопис людини, доповнений\n"
        );
    }

    #[test]
    fn log_md_is_still_never_written() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("project-log.md");

        write_now_md(&log, "не сміє зʼявитись\n").unwrap();

        assert!(!log.exists(), "журнал проєкту створено записом");
        assert_eq!(tmp_leftovers(dir.path()), Vec::<String>::new());
    }
}
