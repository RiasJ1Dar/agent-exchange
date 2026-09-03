use exchange_store::{Agent, Message, Op};
use serde::Serialize;
use std::fmt::Write as FmtWrite;
use std::path::{Path, PathBuf};

/// Імʼя машинного файла обміну — сусід `now_md` у тій самій теці.
pub(crate) const TALK_NAME: &str = "agent_talk.md";

/// Шапка `agent_talk.md`: два рядки для людини, далі самі JSON-рядки.
pub(crate) const TALK_HEADER: &str = concat!(
    "# agent_talk — дріт агентів. Файл ГЕНЕРОВАНИЙ, правки руками зникнуть.\n",
    "# Формат: один JSON на рядок. Людині сюди дивитись не треба — див. NOW.md.\n"
);

/// `agent_talk.md` поруч із `now_md`.
pub(crate) fn agent_talk_path(now_md: &Path) -> PathBuf {
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
pub(crate) fn build_agent_talk(msgs: &[Message]) -> String {
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
