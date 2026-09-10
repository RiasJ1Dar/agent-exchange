use crate::messages::Agent;

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
    /// Ім'я не пройшло перевірку [`crate::Agent::new`].
    ///
    /// Окремо від [`Error::UnknownAgent`] навмисне: там питання «хто це»,
    /// тут — «так називатись не можна». Причина в тексті, бо саме її людина
    /// має прочитати, а не гадати, який символ завадив.
    #[error("негодяще ім'я агента «{name}»: {why}")]
    BadAgentName { name: String, why: String },
    /// Ключ підпису непридатний: немає файла, не hex, не та довжина.
    ///
    /// Причини не розділені навмисно: усі означають «підписати нічим», і
    /// для викликача це один випадок. Деталь — у тексті.
    #[error("ключ підпису непридатний: {0}")]
    BadKey(String),
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
    #[error(
        "тема — {chars} символів, стеля {max}: скоротіть тему. \
         Мовчки обрізати я не буду"
    )]
    TopicTooLong { chars: usize, max: usize },
    #[error(
        "нотатка замка — {chars} символів, стеля {max}: скоротіть нотатку \
         або винесіть текст у повідомлення. Мовчки обрізати я не буду"
    )]
    NoteTooLong { chars: usize, max: usize },
    #[error("{0} не може писати сам собі: to має відрізнятись від from")]
    SelfMessage(Agent),
    #[error("сховище пошкоджено (mutex poison)")]
    Poisoned,
}
