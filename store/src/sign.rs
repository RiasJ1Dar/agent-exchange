//! Канонічний вигляд конверта — те, що підписують.
//!
//! Криптографії тут немає навмисне: спершу треба, щоб «той самий конверт»
//! означало «той самий рядок байтів», і тільки потім є сенс щось ним
//! підписувати. Підпис над нестабільним поданням гірший за відсутність
//! підпису: він іноді сходиться, а іноді ні, і винним виглядає ключ.

use crate::messages::{Envelope, ENVELOPE_V};
use serde::{Deserialize, Serialize};
use crate::Error;

/// Позначка формату на початку канонічного рядка.
///
/// Потрібна, щоб підпис, зроблений за одними правилами, не можна було
/// зарахувати за інших: зміна правил змінює позначку, і старі підписи
/// перестають сходитись **голосно**, а не тихо.
pub const CANON_TAG: &str = "exchange-sig-v1";

/// Скласти канонічне подання конверта.
///
/// ⚠️ Поля йдуть із **префіксом довжини**, а не просто через роздільник,
/// як пропонував план (`v|from|to|topic|op|body|ts`).
///
/// Чесно про силу цього аргументу: **сьогодні** плоский формат теж був би
/// безпечний. `|` законний у темі (`check_topic` міряє лише довжину), але
/// щоб два конверти дали однаковий рядок, одне поле мусило б «з'їсти» межу
/// наступного — а всі сім полів обов'язкові, і `op` приймає лише `Q`, `A`,
/// `N`, `L`. Колізію на цьому наборі побудувати не вдається.
///
/// Префікс тут тому, що ця безпека **випадкова**: вона тримається на тому,
/// скільки полів у конверті й наскільки вузький `op`. Додати необов'язкове
/// поле або розширити `op` — і колізія з'явиться тихо, а помітять її вже
/// як «підпис іноді не сходиться». Кодування з префіксом ін'єктивне за
/// побудовою й від складу полів не залежить.
///
/// Довжина рахується в **байтах** UTF-8, а не в символах: підписують байти.
pub fn canonical(env: &Envelope, ts_unix: i64) -> Result<String, Error> {
    // ⚠️ Порядок ключів у `body` має бути сталим, інакше підпис сходився б
    // через раз. Тримається на тому, що `serde_json::Map` без фічі
    // `preserve_order` — це `BTreeMap`, тобто алфавітний порядок.
    // Закріплено тестом `body_serialises_with_sorted_keys`: якщо фічу
    // колись увімкне транзитивна залежність, тест почервоніє, а не підпис.
    let body = serde_json::to_string(&env.body)?;

    let mut out = String::with_capacity(CANON_TAG.len() + body.len() + 64);
    out.push_str(CANON_TAG);
    for field in [
        env.v.to_string().as_str(),
        env.from.as_str(),
        env.to.as_str(),
        env.topic.as_str(),
        env.op.as_str(),
        body.as_str(),
        ts_unix.to_string().as_str(),
    ] {
        push_field(&mut out, field);
    }
    Ok(out)
}

/// Дописати одне поле як `<байтів>:<значення>`.
fn push_field(out: &mut String, value: &str) {
    out.push('|');
    out.push_str(&value.len().to_string());
    out.push(':');
    out.push_str(value);
}

/// Версія конверта, яку вміє підписувати ця збірка.
///
/// Окрема від [`ENVELOPE_V`] лише за назвою: поки вони збігаються, але
/// підпис і формат конверта можуть розійтись у версіях, і мовчазне
/// ототожнення тоді коштувало б дорого.
pub const SIGNABLE_ENVELOPE_V: u32 = ENVELOPE_V;

/// Довжина приватного ключа (seed) у байтах.
pub const KEY_BYTES: usize = 32;
/// Довжина підпису Ed25519 у байтах.
pub const SIG_BYTES: usize = 64;

/// Закодувати байти в hex.
///
/// ⚠️ Свій, а не крейт. Не з упертості: підпис і ключ треба покласти в
/// текстову колонку, і це чи не єдиний випадок, коли власні десять рядків
/// чесніші за залежність — тут немає ні криптографії, ні крайніх випадків,
/// які варто комусь довіряти. Саму криптографію, навпаки, свою не пишемо.
pub fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Розібрати hex назад у байти. Довжина перевіряється викликачем.
pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let raw = s.as_bytes();
    for pair in raw.chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}

/// Ключ, яким підписують. Обгортка над `ed25519_dalek::SigningKey`.
pub struct Key {
    inner: ed25519_dalek::SigningKey,
    /// Ім'я ключа для колонки `key_id` — щоб було видно, чим підписано,
    /// не звіряючи криптографію.
    pub id: String,
}

impl Key {
    /// Зробити ключ із 32 байтів seed.
    pub fn from_seed(seed: &[u8], id: &str) -> Result<Self, Error> {
        if seed.len() != KEY_BYTES {
            return Err(Error::BadKey(format!(
                "ключ має бути {KEY_BYTES} байтів, отримано {}",
                seed.len()
            )));
        }
        let mut buf = [0u8; KEY_BYTES];
        buf.copy_from_slice(seed);
        Ok(Key {
            inner: ed25519_dalek::SigningKey::from_bytes(&buf),
            id: id.to_string(),
        })
    }

    /// Прочитати ключ із файла: рівно 64 hex-символи, пробіли з країв можна.
    ///
    /// ⚠️ Прав доступу тут не перевіряємо, і це свідома межа: на Windows
    /// ACL перевіряти складно й ненадійно, а половинчаста перевірка давала б
    /// хибне відчуття захищеності. Тримати файл ключа осторонь — робота
    /// того, хто його кладе; про це сказано в документації.
    pub fn from_file(path: &std::path::Path, id: &str) -> Result<Self, Error> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| Error::BadKey(format!("{}: {e}", path.display())))?;
        let hex = raw.trim();
        let seed = from_hex(hex).ok_or_else(|| {
            Error::BadKey(format!(
                "{}: очікував {} hex-символів, а там не hex",
                path.display(),
                KEY_BYTES * 2
            ))
        })?;
        Key::from_seed(&seed, id)
    }

    /// Публічний ключ у hex — те, що кладуть у конфіг для перевірки.
    pub fn public_hex(&self) -> String {
        use ed25519_dalek::VerifyingKey;
        let vk: VerifyingKey = self.inner.verifying_key();
        to_hex(vk.as_bytes())
    }

    /// Підписати канонічний рядок. Повертає підпис у hex.
    pub fn sign(&self, canonical: &str) -> String {
        use ed25519_dalek::Signer;
        to_hex(&self.inner.sign(canonical.as_bytes()).to_bytes())
    }
}

/// Перевірити підпис публічним ключем.
///
/// Повертає `false` на будь-якій негодящості — хибний hex, не та довжина,
/// не той підпис. Розрізняти їх назовні немає сенсу: усі три означають
/// «цьому рядку вірити не можна».
pub fn verify(public_hex: &str, canonical: &str, sig_hex: &str) -> bool {
    use ed25519_dalek::{Signature, VerifyingKey};

    let Some(pk) = from_hex(public_hex) else {
        return false;
    };
    let Ok(pk): Result<[u8; KEY_BYTES], _> = pk.try_into() else {
        return false;
    };
    let Ok(vk) = VerifyingKey::from_bytes(&pk) else {
        return false;
    };
    let Some(sig) = from_hex(sig_hex) else {
        return false;
    };
    let Ok(sig): Result<[u8; SIG_BYTES], _> = sig.try_into() else {
        return false;
    };
    vk.verify_strict(canonical.as_bytes(), &Signature::from_bytes(&sig))
        .is_ok()
}

/// Стан підпису одного повідомлення, як його бачить читач.
///
/// ⚠️ Це поле **віддається назовні**, а не використовується для мовчазного
/// відсіювання. Сховати повідомлення з негодящим підписом було б гірше:
/// адресат не побачив би ні листа, ні причини, і вирішив би, що відправник
/// мовчить. Позначка робить проблему видимою й лишає рішення людині.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Trust {
    /// Підпису немає, і його ніхто не вимагав. Звичайний стан там, де
    /// підпис не вмикали.
    Unsigned,
    /// Підпис є і сходиться публічним ключем відправника.
    Valid {
        /// Яким ключем підписано — з колонки `key_id`.
        key_id: String,
    },
    /// Підпис є, але не сходиться; або його немає там, де він обов'язковий.
    ///
    /// Причина в тексті, бо ці випадки лікуються по-різному: «не той ключ»
    /// означає підміну або зміну ключа, а «немає підпису» — що агент пише
    /// без ключа, хоч його публічний ключ уже роздали.
    Broken {
        /// Що саме не так — людською мовою.
        why: String,
    },
}

/// Дефолт для serde: рядок без відомостей про підпис.
pub fn unsigned() -> Trust {
    Trust::Unsigned
}

/// Публічні ключі агентів: ім'я → ключ у hex.
///
/// ⚠️ Порожній перелік і **відсутній** перелік — різні речі, і плутати їх
/// не можна. Немає конфігу — підпис ніхто не вимагає, усе `Unsigned`.
/// Конфіг є, але агента в ньому немає — його підпис не перевіряється, бо
/// нема чим; це теж `Unsigned`, а не `Broken`.
#[derive(Debug, Clone, Default)]
pub struct Trusted {
    keys: std::collections::BTreeMap<String, String>,
}

impl Trusted {
    /// Прочитати конфіг: JSON-обʼєкт `{"ім'я": "публічний ключ у hex"}`.
    pub fn from_file(path: &std::path::Path) -> Result<Self, Error> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| Error::BadKey(format!("{}: {e}", path.display())))?;
        let keys: std::collections::BTreeMap<String, String> =
            serde_json::from_str(&raw).map_err(|e| {
                Error::BadKey(format!("{}: очікував JSON-обʼєкт: {e}", path.display()))
            })?;
        for (name, hex) in &keys {
            let ok = from_hex(hex).map(|b| b.len() == KEY_BYTES).unwrap_or(false);
            if !ok {
                return Err(Error::BadKey(format!(
                    "{}: ключ «{name}» має бути {} hex-символів",
                    path.display(),
                    KEY_BYTES * 2
                )));
            }
        }
        Ok(Trusted { keys })
    }

    /// Скільки агентів у переліку.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Чи перелік порожній.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Оцінити підпис одного повідомлення.
    ///
    /// `sig` і `key_id` — те, що лежить у рядку; `canonical` — подання, яке
    /// підписували.
    pub fn judge(
        &self,
        from: &str,
        canonical: &str,
        sig: Option<&str>,
        key_id: Option<&str>,
    ) -> Trust {
        let Some(public) = self.keys.get(from) else {
            // Ключа цього агента нам не давали — перевіряти нічим.
            // Це не привід не вірити: підпис може бути й правильним.
            return Trust::Unsigned;
        };
        let Some(sig) = sig else {
            return Trust::Broken {
                why: format!(
                    "від «{from}» очікується підпис (його ключ є в переліку), \
                     а рядок не підписаний"
                ),
            };
        };
        if verify(public, canonical, sig) {
            Trust::Valid {
                key_id: key_id.unwrap_or("").to_string(),
            }
        } else {
            Trust::Broken {
                why: format!("підпис від «{from}» не сходиться його публічним ключем"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::{Agent, Op, Recipient};
    use serde_json::json;

    fn env(topic: &str, body: serde_json::Value) -> Envelope {
        Envelope {
            v: 1,
            from: Agent::new("Grok").unwrap(),
            to: Recipient::One(Agent::new("Claude").unwrap()),
            topic: topic.to_string(),
            op: Op::N,
            body,
        }
    }

    #[test]
    fn the_same_envelope_always_gives_the_same_string() {
        let e = env("t", json!({"b": 2, "a": 1, "c": [3, {"z": 1, "y": 2}]}));
        let first = canonical(&e, 1000).unwrap();
        for _ in 0..1000 {
            assert_eq!(canonical(&e, 1000).unwrap(), first);
        }
    }

    /// ⚠️ Сторожовий тест: підпис мовчки поламається, якщо порядок ключів
    /// у `body` перестане бути сталим.
    ///
    /// Тримається це на тому, що `serde_json::Map` без фічі `preserve_order`
    /// — `BTreeMap`. Фічу може ввімкнути **транзитивна** залежність, і тоді
    /// підпис почне сходитись через раз. Хай краще почервоніє тест.
    #[test]
    fn body_serialises_with_sorted_keys() {
        let one = serde_json::to_string(&json!({"b": 1, "a": 2})).unwrap();
        let two = serde_json::to_string(&json!({"a": 2, "b": 1})).unwrap();
        assert_eq!(one, two, "порядок ключів у body перестав бути сталим");
        assert_eq!(one, r#"{"a":2,"b":1}"#);
    }

    #[test]
    fn changing_any_field_changes_the_string() {
        let base = env("t", json!({"n": 1}));
        let s = canonical(&base, 1000).unwrap();

        let mut other = base.clone();
        other.topic = "t2".into();
        assert_ne!(canonical(&other, 1000).unwrap(), s, "тема");

        let mut other = base.clone();
        other.op = Op::Q;
        assert_ne!(canonical(&other, 1000).unwrap(), s, "операція");

        let mut other = base.clone();
        other.from = Agent::new("Claude").unwrap();
        assert_ne!(canonical(&other, 1000).unwrap(), s, "відправник");

        let mut other = base.clone();
        other.to = Recipient::All;
        assert_ne!(canonical(&other, 1000).unwrap(), s, "адресат");

        let mut other = base.clone();
        other.body = json!({"n": 2});
        assert_ne!(canonical(&other, 1000).unwrap(), s, "тіло");

        assert_ne!(canonical(&base, 1001).unwrap(), s, "час");
    }

    /// Кодування ін'єктивне: різні набори полів — різні рядки, і вміст
    /// поля не може зімітувати межу.
    ///
    /// ⚠️ Тест перевіряє **властивість кодування**, а не вигаданий сценарій
    /// атаки. Мутаційна перевірка показала, що плоский формат тут теж
    /// зелений: із поточними сімома обов'язковими полями й `op` із чотирьох
    /// значень колізію не побудувати. Префікс потрібен, щоб ця безпека не
    /// залежала від складу полів — і тест закріплює саме ін'єктивність.
    #[test]
    fn the_encoding_is_injective_even_when_a_field_mimics_the_layout() {
        let cases = [
            env("xvid", json!({})),
            env("xvid|1:N", json!({})),
            env("xvid|", json!({})),
            env("", json!({})),
            env("1:xvid", json!({})),
            env("xvid", json!({"k": "|1:N|"})),
        ];
        let mut seen: Vec<String> = Vec::new();
        for e in &cases {
            let s = canonical(e, 1000).unwrap();
            assert!(
                !seen.contains(&s),
                "два різні конверти дали однаковий рядок: {s}"
            );
            seen.push(s);
        }
    }

    /// Довжина поля відновлюється однозначно навіть тоді, коли вміст
    /// виглядає як префікс іншого поля.
    #[test]
    fn a_field_that_looks_like_a_prefix_is_still_measured() {
        let s = canonical(&env("1:N", json!({})), 1000).unwrap();
        assert!(s.contains("|3:1:N|"), "довжина мала стояти перед вмістом: {s}");
    }

    /// Довжина в байтах, а не в символах: підписують байти.
    #[test]
    fn length_prefix_counts_bytes_not_chars() {
        let e = env("їжа", json!({}));
        let s = canonical(&e, 1000).unwrap();
        // «їжа» — три символи, шість байтів UTF-8.
        assert!(s.contains("|6:їжа|"), "{s}");
    }

    // ── Підпис ──────────────────────────────────────────────────────────

    fn key(id: &str, seed_byte: u8) -> Key {
        Key::from_seed(&[seed_byte; KEY_BYTES], id).unwrap()
    }

    #[test]
    fn a_signature_verifies_with_its_own_public_key() {
        let k = key("claude-1", 7);
        let msg = canonical(&env("t", json!({"n": 1})), 1000).unwrap();
        let sig = k.sign(&msg);

        assert!(verify(&k.public_hex(), &msg, &sig));
    }

    /// ⚠️ Суть усього R5: **чужим ключем підписатись не можна**.
    ///
    /// Саме це не закриває HMAC зі спільним секретом — там хто може
    /// перевірити, той може й підробити. Тут здатності розведені.
    #[test]
    fn another_key_cannot_produce_a_signature_that_verifies() {
        let mine = key("claude-1", 7);
        let theirs = key("grok-1", 9);
        let msg = canonical(&env("t", json!({})), 1000).unwrap();

        let forged = theirs.sign(&msg);
        assert!(
            !verify(&mine.public_hex(), &msg, &forged),
            "підпис чужим ключем зарахувався як мій"
        );
    }

    #[test]
    fn a_changed_message_breaks_the_signature() {
        let k = key("claude-1", 7);
        let msg = canonical(&env("t", json!({"n": 1})), 1000).unwrap();
        let sig = k.sign(&msg);

        let other = canonical(&env("t", json!({"n": 2})), 1000).unwrap();
        assert!(!verify(&k.public_hex(), &other, &sig), "тіло");

        let later = canonical(&env("t", json!({"n": 1})), 1001).unwrap();
        assert!(!verify(&k.public_hex(), &later, &sig), "час");
    }

    /// Негодящий вхід — це «не вірити», а не паніка.
    #[test]
    fn garbage_input_is_rejected_quietly() {
        let k = key("claude-1", 7);
        let msg = canonical(&env("t", json!({})), 1000).unwrap();
        let sig = k.sign(&msg);

        assert!(!verify("не hex", &msg, &sig), "ключ не hex");
        assert!(!verify(&k.public_hex(), &msg, "не hex"), "підпис не hex");
        assert!(!verify("aabb", &msg, &sig), "ключ не тієї довжини");
        assert!(!verify(&k.public_hex(), &msg, "aabb"), "підпис не тієї довжини");
        assert!(!verify("", &msg, &sig), "порожній ключ");
    }

    #[test]
    fn hex_round_trips() {
        for bytes in [vec![], vec![0], vec![255, 0, 16], vec![7; KEY_BYTES]] {
            let hex = to_hex(&bytes);
            assert_eq!(from_hex(&hex).unwrap(), bytes, "{hex}");
        }
        assert_eq!(to_hex(&[0, 15, 16, 255]), "000f10ff");
        assert!(from_hex("abc").is_none(), "непарна довжина");
        assert!(from_hex("zz").is_none(), "не hex-цифри");
    }

    #[test]
    fn a_key_file_is_read_and_checked() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("k.hex");

        std::fs::write(&path, format!("  {}  
", to_hex(&[3u8; KEY_BYTES]))).unwrap();
        let k = Key::from_file(&path, "id").unwrap();
        assert_eq!(k.id, "id");

        std::fs::write(&path, "не hex").unwrap();
        assert!(matches!(Key::from_file(&path, "id"), Err(Error::BadKey(_))));

        std::fs::write(&path, to_hex(&[1u8; 8])).unwrap();
        assert!(
            matches!(Key::from_file(&path, "id"), Err(Error::BadKey(_))),
            "коротка довжина мала бути помилкою"
        );

        assert!(matches!(
            Key::from_file(&dir.path().join("немає"), "id"),
            Err(Error::BadKey(_))
        ));
    }

    #[test]
    fn the_format_tag_opens_the_string() {
        let s = canonical(&env("t", json!({})), 1000).unwrap();
        assert!(s.starts_with(CANON_TAG), "{s}");
    }
}
