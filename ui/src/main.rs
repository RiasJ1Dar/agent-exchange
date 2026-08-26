//! `exchange-ui` — переглядач стану обміну, який запускає **людина**.
//!
//! Що він робить: слухає `127.0.0.1`, на кожен запит читає базу обміну
//! **лише на читання**, складає `Snapshot` і віддає одну HTML-сторінку.
//!
//! ⚠️ Чого він не робить і не робитиме: не пише в базу, не кличе `render`,
//! не торкається `NOW.md` і `agent_talk.md`. `NOW.md` читається лише як
//! `metadata` (mtime), сам файл не відкривається. Уся цінність окремого
//! процесу в тому, що він фізично не може зіпсувати обмін.

use std::net::{Ipv4Addr, TcpListener};
use std::path::Path;

use exchange_ui::db::{self, DbError};
use exchange_ui::http::{self, Request};
use exchange_ui::page::{escape, render_page};

/// Порт за замовчуванням. Сусідній із 9749, на якому сидить UI індексу коду.
const DEFAULT_PORT: u16 = 9750;

/// Код відповіді, коли базу прочитати не вдалось.
///
/// 503, а не 500: сервер живий, недоступне саме джерело даних — і воно
/// цілком може стати доступним само (запустили сервер обміну), без
/// перезапуску переглядача.
const UNAVAILABLE: u16 = 503;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "-h" || a == "--help") {
        eprintln!("{}", usage());
        return;
    }

    let port = match parse_port(&args) {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("exchange-ui: {msg}\n\n{}", usage());
            std::process::exit(2);
        }
    };

    let db_path = db::db_path();
    let now_md = db::now_md_path();

    let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "exchange-ui: не вдалось зайняти 127.0.0.1:{port}: {e}\n\
                 Найімовірніше порт уже зайнятий іншим переглядачем. \
                 Спробуйте --port з іншим числом."
            );
            std::process::exit(1);
        }
    };

    // ⚠️ Тільки петля. `0.0.0.0` віддав би стан координації в мережу; сторінка
    // не має ні автентифікації, ні причини бути видимою поза цією машиною.
    eprintln!(
        "exchange-ui: слухаю http://127.0.0.1:{port}/\n\
         exchange-ui: читаю базу (лише на читання): {}\n\
         exchange-ui: NOW.md для часу останнього render: {}\n\
         exchange-ui: зупинити — Ctrl+C",
        db_path.display(),
        now_md.display()
    );

    http::serve(listener, move |req| handle(req, &db_path, &now_md));
}

/// Підказка про запуск.
fn usage() -> String {
    format!(
        "Використання: exchange-ui [--port N]\n\
         \n\
         --port N     порт на 127.0.0.1, за замовчуванням {DEFAULT_PORT}\n\
         -h, --help   ця підказка\n\
         \n\
         Змінні середовища:\n\
         EXCHANGE_DB       шлях до бази обміну, за замовчуванням {}\n\
         EXCHANGE_NOW_MD   шлях до NOW.md, за замовчуванням {}\n\
         \n\
         Переглядач відкриває базу лише на читання й нічого в неї не пише.",
        db::DEFAULT_DB,
        db::DEFAULT_NOW_MD
    )
}

/// Розібрати `--port N` або `--port=N`.
///
/// Порт 0 відхиляється: ОС видала б випадковий вільний, і людина не знала б,
/// куди йти браузером, — а сенс запуску саме в тому, щоб відкрити сторінку.
fn parse_port(args: &[String]) -> Result<u16, String> {
    let mut i = 0;
    let mut found: Option<u16> = None;
    while i < args.len() {
        let a = &args[i];
        let raw = if let Some(v) = a.strip_prefix("--port=") {
            i += 1;
            v.to_string()
        } else if a == "--port" {
            let v = args
                .get(i + 1)
                .ok_or_else(|| "після --port бракує числа".to_string())?;
            i += 2;
            v.clone()
        } else {
            return Err(format!("невідомий аргумент: {a}"));
        };
        let n: u16 = raw
            .parse()
            .map_err(|_| format!("порт має бути числом 1..65535, а не {raw:?}"))?;
        if n == 0 {
            return Err("порт 0 не годиться: адресу тоді обирає ОС".to_string());
        }
        found = Some(n);
    }
    Ok(found.unwrap_or(DEFAULT_PORT))
}

/// Один запит: свіже читання бази, далі сторінка.
///
/// База читається на **кожен** запит, без кешу: сторінка автооновлюється
/// кожні 5 с саме для того, щоб показувати поточний стан, а кеш робив би її
/// показником того, що було на момент старту процесу.
fn handle(req: &Request, db_path: &Path, now_md: &Path) -> (u16, String) {
    if req.path != "/" && req.path != "/index.html" {
        return (404, not_found_page(&req.path));
    }
    match db::read_snapshot_now(db_path, now_md) {
        Ok(snap) => (200, render_page(&snap)),
        Err(err) => {
            // Пишемо і в stderr: людина, яка запустила процес, бачить причину
            // у вікні терміналу, навіть якщо браузер уже закрила.
            eprintln!("exchange-ui: {err}");
            (UNAVAILABLE, error_page(db_path, &err))
        }
    }
}

/// Сторінка на невідомий шлях. Маршрут тут рівно один — так і кажемо.
fn not_found_page(path: &str) -> String {
    simple_page(
        "Такої сторінки немає",
        &format!(
            "<p>Шлях <code>{}</code> не обслуговується.</p>\
             <p>У переглядача рівно одна сторінка: <a href=\"/\">/</a>.</p>",
            escape(path)
        ),
    )
}

/// Сторінка, коли базу прочитати не вдалось.
///
/// ⚠️ Порожня сторінка або голий 500 змусили б людину лізти в лог. Тут вона
/// бачить дослівну причину й що з нею робити — текст помилки для того й
/// написаний повними реченнями.
fn error_page(db_path: &Path, err: &DbError) -> String {
    simple_page(
        "Базу обміну прочитати не вдалось",
        &format!(
            "<p>Переглядач живий, але даних показати не може.</p>\
             <p><b>База:</b> <code>{}</code></p>\
             <pre>{}</pre>\
             <p>Сторінка сама оновиться через 10 с — якщо причину усунути \
             (наприклад, запустити сервер обміну), вона запрацює без \
             перезапуску переглядача.</p>",
            escape(&db_path.display().to_string()),
            escape(&err.to_string())
        ),
    )
}

/// Мінімальна сторінка для службових повідомлень.
///
/// Свій каркас, а не `render_page`: та функція описує **стан обміну**, і
/// підсовувати їй порожній `Snapshot` означало б показати нулі там, де
/// насправді нічого не прочитано. Нуль і «невідомо» — різні речі.
fn simple_page(title: &str, body_html: &str) -> String {
    format!(
        "<!DOCTYPE html>\n<html lang=\"uk\">\n<head>\n\
         <meta charset=\"utf-8\">\n\
         <meta http-equiv=\"refresh\" content=\"10\">\n\
         <title>{t}</title>\n\
         <style>\n\
         body{{font:14px/1.5 system-ui,sans-serif;margin:2rem auto;max-width:60rem;padding:0 1rem}}\n\
         h1{{font-size:1.4rem}}\n\
         pre{{white-space:pre-wrap;background:#f6f6f6;border:1px solid #ccc;padding:.6rem}}\n\
         </style>\n</head>\n<body>\n\
         <h1>{t}</h1>\n{body}\n</body>\n</html>\n",
        t = escape(title),
        body = body_html
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn default_port_when_no_args() {
        assert_eq!(parse_port(&[]).unwrap(), DEFAULT_PORT);
    }

    #[test]
    fn port_in_both_forms() {
        assert_eq!(parse_port(&args(&["--port", "8080"])).unwrap(), 8080);
        assert_eq!(parse_port(&args(&["--port=8081"])).unwrap(), 8081);
    }

    #[test]
    fn bad_port_is_reported_not_panicked() {
        assert!(parse_port(&args(&["--port", "ні"])).is_err());
        assert!(parse_port(&args(&["--port"])).is_err());
        assert!(parse_port(&args(&["--port", "0"])).is_err());
        assert!(parse_port(&args(&["--host", "0.0.0.0"])).is_err());
    }

    #[test]
    fn error_page_names_cause_and_escapes_it() {
        let err = DbError::Missing {
            path: "C:\\<немає>.db".to_string(),
        };
        let html = error_page(Path::new("C:\\<немає>.db"), &err);
        assert!(html.contains("Базу обміну прочитати не вдалось"));
        assert!(html.contains("EXCHANGE_DB"));
        // Шлях із кутовими дужками не має стати розміткою.
        assert!(!html.contains("<немає>"));
        assert!(html.contains("&lt;немає&gt;"));
    }

    #[test]
    fn not_found_page_escapes_path() {
        let html = not_found_page("/<script>x</script>");
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn usage_names_defaults() {
        let u = usage();
        assert!(u.contains("9750"));
        assert!(u.contains("EXCHANGE_DB"));
        assert!(u.contains("EXCHANGE_NOW_MD"));
    }
}
