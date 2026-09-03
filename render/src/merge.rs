use crate::talk::TALK_NAME;
use exchange_store::{Agent, Lock, Message, Op};
use std::borrow::Cow;
use std::fmt::Write as FmtWrite;

/// Маркери вставок. Контракт узгоджений — не міняти.
pub(crate) const LOCK_BEGIN: &str = "<!-- exchange:lock -->";
pub(crate) const LOCK_END: &str = "<!-- /exchange:lock -->";
pub(crate) const INBOX_BEGIN: &str = "<!-- exchange:inbox -->";
pub(crate) const INBOX_END: &str = "<!-- /exchange:inbox -->";
pub(crate) const HEADING: &str = "# Зараз";

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
pub(crate) fn sanitize(s: &str) -> String {
    s.replace("-->", "--&gt;")
}

pub(crate) fn build_lock_block(locks: &[Lock]) -> String {
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
pub(crate) const UNREAD_MARK: &str = "непрочитане";

/// Рядок порожньої черги питань.
///
/// Окремим текстом, а не «Grok — 0, Claude — 0» зі знаком тривоги: нуль
/// питань — це нормальний стан, і виглядати він має спокійно. ⚠️ у файлі,
/// який читають щодня, працює лише поки не стоїть там завжди.
pub(crate) const NO_QUESTIONS_LINE: &str = "- питань без відповіді немає";

fn op_str(op: Op) -> &'static str {
    match op {
        Op::Q => "Q",
        Op::A => "A",
        Op::N => "N",
        Op::L => "L",
    }
}

/// Стеля непрочитаних у маркері `exchange:inbox`.
///
/// Повні тіла на дошку не потрапляють — їх читають через MCP `inbox`.
/// Стеля бере **найновіші**, як [`exchange_store::InboxQuery::limit`].
pub(crate) const INBOX_UNREAD_LIMIT: usize = 15;

/// Рядок лічильника прочитаних, яких у маркері немає.
pub(crate) fn read_in_db_line(n: usize) -> String {
    format!("- ще {n} прочитаних у базі")
}

/// Блок inbox у NOW.md — **непрочитане для людини, не журнал**.
///
/// Журнал лежить у `agent_talk.md`; тут лише непрочитане (стеля
/// [`INBOX_UNREAD_LIMIT`]) і, якщо в зрізі є прочитані, один рядок
/// [`read_in_db_line`]. Лічильники питань / особистих / широкомовних
/// лишаються повними: стеля ріже список, а не числа.
///
/// Тема кожного рядка проходить `sanitize`: вона потрапляє у файл із
/// маркерами, отже інʼєкція `-->` тут так само небезпечна, як була.
pub(crate) fn build_inbox_block(msgs: &[Message]) -> String {
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
    let _ = writeln!(&mut s, "- широкомовних: {}", t.broadcast);

    let unread: Vec<&Message> = msgs.iter().filter(|m| m.read_at.is_none()).collect();
    let read_count = msgs.len().saturating_sub(unread.len());
    let start = unread.len().saturating_sub(INBOX_UNREAD_LIMIT);
    let shown = &unread[start..];

    if shown.is_empty() {
        s.push_str("- (немає)\n");
    } else {
        for m in shown {
            let _ = writeln!(
                &mut s,
                "- #{} {}→{} {} {} ts_unix={} read_at={UNREAD_MARK}",
                m.id,
                m.envelope.from,
                m.envelope.to,
                sanitize(&m.envelope.topic),
                op_str(m.envelope.op),
                m.ts_unix,
            );
        }
    }

    if read_count > 0 {
        s.push_str(&read_in_db_line(read_count));
        s.push('\n');
    }

    let _ = writeln!(
        &mut s,
        "- повний обмін — {TALK_NAME} (машинний, один JSON на рядок)"
    );

    s.push('\n');
    s.push_str(INBOX_END);
    s
}

pub(crate) const LF: &str = "\n";
pub(crate) const CRLF: &str = "\r\n";

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
pub(crate) fn dominant_eol(text: &str) -> &'static str {
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
pub(crate) fn to_eol<'a>(s: &'a str, eol: &'static str) -> Cow<'a, str> {
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
pub(crate) fn merge_now_md(
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
