//! Канонічний вигляд конверта — те, що підписують.
//!
//! Криптографії тут немає навмисне: спершу треба, щоб «той самий конверт»
//! означало «той самий рядок байтів», і тільки потім є сенс щось ним
//! підписувати. Підпис над нестабільним поданням гірший за відсутність
//! підпису: він іноді сходиться, а іноді ні, і винним виглядає ключ.

use crate::messages::{Envelope, ENVELOPE_V};
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

    #[test]
    fn the_format_tag_opens_the_string() {
        let s = canonical(&env("t", json!({})), 1000).unwrap();
        assert!(s.starts_with(CANON_TAG), "{s}");
    }
}
