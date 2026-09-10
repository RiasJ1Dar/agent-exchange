use crate::{Error, AGENT_NAME_ENV, DEFAULT_BRIEF_CHARS};
use exchange_store::{
    Agent, Envelope, InboxQuery, Lock, Recipient, BROADCAST, BROADCAST_LEGACY,
};
use serde_json::{json, Value};


/// Шаблон імені агента для JSON-схем інструментів — те, що бачить клієнт.
///
/// Мусить збігатися з правилами `Agent::new`; розійшовшись, вони дали б
/// найгірше з двох світів: клієнт вважав би значення дозволеним, а сервер
/// відхиляв би його вже після відправлення.
const AGENT_PATTERN: &str = r"^[A-Za-z0-9_.-]{1,64}$";

/// Те саме плюс широкомовна адреса. Окремий шаблон, бо `*` законний лише
/// там, де вказують адресата.
const RECIPIENT_PATTERN: &str = r"^(\*|[A-Za-z0-9_.-]{1,64})$";

impl crate::Mcp {
    pub(crate) fn tools_call(&self, params: &Value) -> Value {
        let name = match params.get("name").and_then(|n| n.as_str()) {
            Some(n) => n,
            None => {
                return tool_err("немає params.name");
            }
        };
        let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
        match self.call_tool(name, &args) {
            Ok(v) => v,
            Err(e) => tool_err(e),
        }
    }

    fn call_tool(&self, name: &str, args: &Value) -> Result<Value, Error> {
        // Особистість із середовища читаємо один раз на виклик; далі всюди
        // йде тим самим значенням, щоб один tools/call не бачив двох різних.
        let agent_env = env_agent_name();
        let agent_env = agent_env.as_deref();
        match name {
            "post" => {
                let env = envelope_from_args(args, agent_env)?;
                let id = self.store.post(env)?;
                Ok(self.rendered(json!({ "id": id })))
            }
            "inbox" => {
                let agent = json_field::<Recipient>(args, "agent")?;
                let unread_only = args
                    .get("unread_only")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let query = InboxQuery {
                    agent,
                    unread_only,
                    limit: opt_usize(args, "limit")?,
                    brief: resolve_brief(args, unread_only)?,
                    topic: opt_string(args, "topic")?,
                    // `exclude_own` навмисне не виставляється з MCP: агент
                    // бачить власні розсилки `to = Both` у себе ж, і тихо
                    // ховати їх означало б міняти склад скриньки без прохання.
                    ..InboxQuery::default()
                };
                let messages = self.store.inbox_ex(query)?;
                Ok(tool_ok(json!({ "messages": messages })))
            }
            "ack" => self.ack_tool(args, agent_env),
            "lock" => {
                let topic = json_str(args, "topic")?;
                let holder = resolve_agent(args, "holder", agent_env)?;
                let ttl_sec = args.get("ttl_sec").and_then(|v| v.as_i64()).unwrap_or(0);
                let note = args
                    .get("note")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                // ⚠️ Саме `lock_ex`, а не `lock`: узяття йде однією
                // `BEGIN IMMEDIATE`-транзакцією (чотири процеси пишуть у ту
                // саму базу, а перевірка й запис у `lock` — два незалежні
                // autocommit-и), а перехоплення протермінованого замка
                // лишає слід: `evicted` у відповіді й сповіщення `Op::N`
                // колишньому тримачеві. Тихий варіант мовчки забирав тему,
                // і той, у кого її забрали, продовжував вважати її своєю.
                let outcome = self.store.lock_ex(topic, holder.clone(), ttl_sec, note)?;
                Ok(self.rendered(json!({
                    "ok": true,
                    "topic": topic,
                    "holder": holder,
                    "evicted": outcome.evicted,
                    "notified_id": outcome.notified_id
                })))
            }
            "unlock" => {
                let topic = json_str(args, "topic")?;
                let holder = resolve_agent(args, "holder", agent_env)?;
                self.store.unlock(topic, holder.clone())?;
                Ok(self.rendered(json!({
                    "ok": true,
                    "topic": topic,
                    "holder": holder
                })))
            }
            "render" => {
                self.rerender()?;
                Ok(tool_ok(json!({ "ok": true })))
            }
            "lock_status" => {
                // Тільки читання: NOW.md не чіпаємо.
                let locks = self.store.locks()?;
                Ok(tool_ok(locks_payload(&locks, now_unix())?))
            }
            other => Ok(tool_err(format!("невідомий tool: {other}"))),
        }
    }

    /// `ack` двома шляхами: один `id` або пачка `ids`.
    ///
    /// Розвʼязуються вони **до** будь-якої роботи зі сховищем і за наявністю
    /// поля, а не за його значенням: обидва разом — помилка (мовчки віддати
    /// перевагу одному означало б тихо проігнорувати половину прохання),
    /// жодного — теж помилка. Далі шляхи не змішуються:
    ///
    /// * `id` іде через [`Store::ack_one`] і **потребує особистості**
    ///   так само, як пачка. ⚠️ Раніше тут стояв [`Store::ack`] — позначення
    ///   за самим лише `id`, без адресата: будь-хто гасив чуже непрочитане,
    ///   і повернути це не було чим (`read_at = NULL` в API немає, а чужі
    ///   `id` відкрито лежать в `agent_talk.md`). Тепер позначається лише
    ///   своє й лише непрочитане, а `acked = false` (чуже, неіснуюче, вже
    ///   прочитане) — відповідь, а не збій: розрізняти ці три випадки
    ///   ззовні означало б зробити `ack` оракулом чужих id;
    /// * `ids` іде через [`Store::ack_many`] і **потребує особистості**:
    ///   пачка позначає лише те, що адресоване цьому агентові. Чужі,
    ///   неіснуючі та вже прочитані просто не рахуються — це відповідь,
    ///   а не збій, тому у відповіді видно і `acked`, і `requested`.
    ///
    /// Особистість для `ack` — один ланцюг на обидва шляхи: явний `agent`
    /// → `AGENT_NAME` → помилка. Широкомовна адреса не приймається жодним.
    ///
    /// Спільна функція, а не два однакові шматки: розійшовшись, вони й дали б
    /// ту саму дірку, що була в `id` — один шлях питає, чиє це, другий ні.
    fn ack_agent(&self, args: &Value, agent_env: Option<&str>) -> Result<Agent, Error> {
        // ⚠️ Перевіряється СИРИЙ аргумент, до розбору в `Agent`.
        //
        // Тип більше не має широкомовного варіанта, тож `Agent::parse` сказав
        // би просто «невідомий агент» — і людина шукала б друкарську помилку
        // в імені замість справжньої причини. Повідомлення важливіше за
        // економію рядка.
        if let Some(raw) = args.get("agent").and_then(|v| v.as_str()) {
            if raw == BROADCAST || raw == BROADCAST_LEGACY {
                return Err(Error::InvalidParams(format!(
                    "agent={raw} не підтверджує читання: це адреса розсилки, \
                     а не особистість; передайте конкретного агента"
                )));
            }
        }
        resolve_agent(args, "agent", agent_env)
    }

    fn ack_tool(&self, args: &Value, agent_env: Option<&str>) -> Result<Value, Error> {
        let has_id = has_value(args, "id");
        let has_ids = has_value(args, "ids");
        match (has_id, has_ids) {
            (true, true) => Err(Error::InvalidParams(
                "разом `id` і `ids` не приймаються: або один id, або пачка ids"
                    .into(),
            )),
            (false, false) => Err(Error::InvalidParams(
                "немає ні `id`, ні `ids`: передайте один id або масив ids".into(),
            )),
            (true, false) => {
                let id = json_i64(args, "id")?;
                let agent = self.ack_agent(args, agent_env)?;
                let acked = self.store.ack_one(id, agent.clone())?;
                Ok(self.rendered(json!({
                    "ok": true,
                    "id": id,
                    "agent": agent,
                    "acked": acked
                })))
            }
            (false, true) => {
                let ids = json_i64_array(args, "ids")?;
                let agent = self.ack_agent(args, agent_env)?;
                let acked = self.store.ack_many(&ids, agent.clone())?;
                Ok(self.rendered(json!({
                    "ok": true,
                    "agent": agent,
                    "acked": acked,
                    "requested": ids.len(),
                    "ids": ids
                })))
            }
        }
    }
}

fn envelope_from_args(args: &Value, agent_env: Option<&str>) -> Result<Envelope, Error> {
    let mut obj = if let Some(env) = args.get("envelope") {
        env.clone()
    } else {
        args.clone()
    };
    if obj.is_object() {
        // Явний `from` виграє; його ж і кладемо назад, тож наявні виклики
        // проходять байт-у-байт як раніше.
        let from = resolve_agent(&obj, "from", agent_env)?;
        let from = serde_json::to_value(from)?;
        let map = obj.as_object_mut().expect("щойно перевірено is_object");
        map.entry("v").or_insert(json!(1));
        map.insert("from".to_string(), from);
    }
    serde_json::from_value(obj).map_err(|e| Error::InvalidParams(e.to_string()))
}

/// Прочитати `AGENT_NAME`. Порожнє значення — те саме, що не виставлене.
fn env_agent_name() -> Option<String> {
    std::env::var(AGENT_NAME_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
}

/// Розібрати значення `AGENT_NAME`.
///
/// ⚠️ **Регістр значущий**, і це зміна поведінки. Раніше тут стояло
/// звіряння з переліком `Grok`/`Claude` без урахування регістру, тож
/// `AGENT_NAME=claude` давало `Claude`. Тепер переліку немає — сервер не
/// знає, як звуть агентів, — а отже, немає й канонічної форми, до якої
/// можна було б привести: `Codex` і `codex` це просто різні імена.
///
/// Обрізаються лише пробіли з країв: вони майже завжди друкарська помилка
/// в конфігу, а іменем бути не можуть за правилами [`Agent::new`].
fn agent_from_env(raw: &str) -> Result<Agent, Error> {
    Agent::new(raw.trim()).map_err(|e| {
        Error::InvalidParams(format!("{AGENT_NAME_ENV}=«{}»: {e}", raw.trim()))
    })
}

/// Особистість для `from`/`holder`: явний аргумент виграє; якщо його немає —
/// `AGENT_NAME`; якщо немає й того — помилка, а не мовчазний дефолт.
fn resolve_agent(args: &Value, field: &str, agent_env: Option<&str>) -> Result<Agent, Error> {
    if args.get(field).is_some() {
        return json_field::<Agent>(args, field);
    }
    match agent_env {
        Some(raw) => agent_from_env(raw),
        None => Err(Error::InvalidParams(format!(
            "немає {field}: передайте його явно або виставте змінну середовища \
             {AGENT_NAME_ENV}",
        ))),
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Замки + похідні поля: коли спливає TTL і чи вже сплив.
///
/// Увага: `Store::locks` сама вимітає протерміновані рядки, тож крізь
/// прод-шлях `expired` практично завжди `false`. Позначка лишається
/// чесною для будь-якого замка, що дожив до відповіді.
pub(crate) fn locks_payload(locks: &[Lock], now: i64) -> Result<Value, Error> {
    let mut out = Vec::with_capacity(locks.len());
    for lock in locks {
        let mut v = serde_json::to_value(lock)?;
        let expires_at = lock.taken_at.saturating_add(lock.ttl_sec);
        let map = v
            .as_object_mut()
            .ok_or_else(|| Error::InvalidParams("Lock серіалізувався не в обʼєкт".into()))?;
        map.insert("expires_at".to_string(), json!(expires_at));
        map.insert("expired".to_string(), json!(expires_at <= now));
        out.push(Value::Object(map.clone()));
    }
    Ok(json!({ "now": now, "count": out.len(), "locks": out }))
}

fn json_field<T: serde::de::DeserializeOwned>(args: &Value, field: &str) -> Result<T, Error> {
    let v = args
        .get(field)
        .ok_or_else(|| Error::InvalidParams(format!("немає {field}")))?;
    serde_json::from_value(v.clone()).map_err(|e| Error::InvalidParams(format!("{field}: {e}")))
}

fn json_str<'a>(args: &'a Value, field: &str) -> Result<&'a str, Error> {
    args.get(field)
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::InvalidParams(format!("немає {field}")))
}

fn json_i64(args: &Value, field: &str) -> Result<i64, Error> {
    let v = args
        .get(field)
        .ok_or_else(|| Error::InvalidParams(format!("немає {field}")))?;
    v.as_i64()
        .ok_or_else(|| Error::InvalidParams(format!("{field} має бути цілим")))
}

/// Чи поле справді щось несе. `null` — те саме, що відсутнє: клієнти
/// охоче підставляють його замість пропуску, і рахувати таке за «поле задано»
/// означало б падати на порожньому місці.
fn has_value(args: &Value, field: &str) -> bool {
    matches!(args.get(field), Some(v) if !v.is_null())
}

fn json_i64_array(args: &Value, field: &str) -> Result<Vec<i64>, Error> {
    let arr = args
        .get(field)
        .and_then(|v| v.as_array())
        .ok_or_else(|| Error::InvalidParams(format!("{field} має бути масивом цілих")))?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, v) in arr.iter().enumerate() {
        out.push(
            v.as_i64()
                .ok_or_else(|| Error::InvalidParams(format!("{field}[{i}] має бути цілим")))?,
        );
    }
    Ok(out)
}

/// Необовʼязкове невідʼємне число. Відсутнє або `null` — `None`;
/// відʼємне — помилка, а не мовчазний нуль.
fn opt_usize(args: &Value, field: &str) -> Result<Option<usize>, Error> {
    match args.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let n = v
                .as_i64()
                .ok_or_else(|| Error::InvalidParams(format!("{field} має бути цілим")))?;
            if n < 0 {
                return Err(Error::InvalidParams(format!(
                    "{field} не може бути відʼємним, отримано {n}"
                )));
            }
            Ok(Some(n as usize))
        }
    }
}

fn opt_string(args: &Value, field: &str) -> Result<Option<String>, Error> {
    match args.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_str()
            .map(|s| Some(s.to_string()))
            .ok_or_else(|| Error::InvalidParams(format!("{field} має бути рядком"))),
    }
}

/// Стеля тіла для `inbox` — див. [`DEFAULT_BRIEF_CHARS`].
///
/// Поле відсутнє — дефолт вмикається **тільки** при `unread_only`: агент
/// розгрібає чергу і йому потрібні заголовки, а не сорок повних тіл.
/// Явний `brief` завжди виграє, `brief = 0` і `brief = null` означають
/// «повне тіло» (у `store` `Some(0)` різало б усе до самої позначки —
/// відповіді з одних «…» ніхто не просив).
fn resolve_brief(args: &Value, unread_only: bool) -> Result<Option<usize>, Error> {
    match args.get("brief") {
        None => Ok(unread_only.then_some(DEFAULT_BRIEF_CHARS)),
        Some(Value::Null) => Ok(None),
        Some(_) => Ok(opt_usize(args, "brief")?.filter(|n| *n > 0)),
    }
}

pub(crate) fn tool_ok(value: Value) -> Value {
    json!({
        "content": [{ "type": "text", "text": value.to_string() }],
        "isError": false
    })
}

pub(crate) fn tool_err(msg: impl std::fmt::Display) -> Value {
    json!({
        "content": [{ "type": "text", "text": msg.to_string() }],
        "isError": true
    })
}

pub(crate) fn tool_defs() -> Value {
    json!([
        {
            "name": "post",
            "description": "Покласти Envelope у store і перемалювати NOW.md; без `from` підпис береться зі змінної середовища AGENT_NAME",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "from": { "type": "string", "pattern": AGENT_PATTERN },
                    "to": { "type": "string", "pattern": RECIPIENT_PATTERN },
                    "topic": { "type": "string" },
                    "op": { "type": "string", "enum": ["Q", "A", "N", "L"] },
                    "body": { "type": "object" },
                    "v": { "type": "integer" },
                    "envelope": { "type": "object" }
                }
            }
        },
        {
            "name": "inbox",
            "description": "Прочитати inbox агента: unread_only (за замовчуванням false), limit (стільки найновіших), topic (фільтр за темою), brief (стеля символів тіла; при unread_only за замовчуванням 200, brief=0 — повні тіла)",
            "inputSchema": {
                "type": "object",
                "required": ["agent"],
                "properties": {
                    "agent": { "type": "string", "pattern": RECIPIENT_PATTERN },
                    "unread_only": { "type": "boolean" },
                    "limit": { "type": "integer", "minimum": 0 },
                    "brief": { "type": "integer", "minimum": 0 },
                    "topic": { "type": "string" }
                }
            }
        },
        {
            "name": "ack",
            "description": "Позначити прочитаним і перемалювати NOW.md: або один `id`, або пачка `ids` — обидва шляхи від імені `agent` (без `agent` він береться зі змінної середовища AGENT_NAME) і позначають лише адресоване йому; у відповіді acked (на пачку — проти requested): чуже, неіснуюче та вже прочитане не рахується й помилкою не є",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "integer" },
                    "ids": { "type": "array", "items": { "type": "integer" } },
                    "agent": { "type": "string", "pattern": AGENT_PATTERN }
                }
            }
        },
        {
            "name": "lock",
            "description": "Взяти замок на тему і перемалювати NOW.md; без `holder` власник береться зі змінної середовища AGENT_NAME; перехоплення протермінованого чужого замка видно в полі evicted, а колишньому тримачеві лягає сповіщення (notified_id)",
            "inputSchema": {
                "type": "object",
                "required": ["topic"],
                "properties": {
                    "topic": { "type": "string" },
                    "holder": { "type": "string", "pattern": AGENT_PATTERN },
                    "ttl_sec": { "type": "integer" },
                    "note": { "type": "string" }
                }
            }
        },
        {
            "name": "unlock",
            "description": "Зняти свій замок і перемалювати NOW.md; без `holder` власник береться зі змінної середовища AGENT_NAME",
            "inputSchema": {
                "type": "object",
                "required": ["topic"],
                "properties": {
                    "topic": { "type": "string" },
                    "holder": { "type": "string", "pattern": AGENT_PATTERN }
                }
            }
        },
        {
            "name": "render",
            "description": "Перемалювати NOW.md зі store",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "lock_status",
            "description": "Показати всі замки з полем expired (taken_at + ttl_sec проти поточного часу)",
            "inputSchema": { "type": "object", "properties": {} }
        }
    ])
}
