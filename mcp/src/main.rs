//! Запуск stdio-сервера обміну — і, поруч із ним, вікна перегляду для людини.
//!
//! ⚠️ **stdout тут — канал протоколу MCP.** Один зайвий байт у ньому ламає
//! сесію цілком, тож у цьому файлі немає жодного `println!`: усе службове
//! йде в stderr. З тієї ж причини UI ніколи не пише в stdout — його обробник
//! віддає HTML у сокет, а свої скарги в stderr.
//!
//! Чому UI живе всередині сервера, а не окремим процесом: людина хоче бачити
//! стан обміну **постійно**, а не запускати переглядач руками. Сервер живе
//! рівно стільки, скільки сесія агента, — це і є потрібний час життя вікна.
//! Окремий бінарник `exchange-ui` при цьому лишається: він потрібен, коли
//! треба подивитись на обмін, не піднімаючи сервера.

use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};

use exchange_ui::db::{self, DbError};
use exchange_ui::http::{self, Request};
use exchange_ui::page::{escape, render_page};

/// Порт вікна за замовчуванням. Сусідній із 9749 — там UI індексу коду.
const DEFAULT_UI_PORT: u16 = 9750;

/// Env-перевизначення порту. Аргумент `--ui-port` має вищий пріоритет.
const UI_PORT_ENV: &str = "EXCHANGE_UI_PORT";

/// Код відповіді, коли базу прочитати не вдалось.
///
/// 503, а не 500: сервер живий, недоступне саме джерело даних, і воно може
/// стати доступним само — сторінка перезавантажиться й запрацює.
const UNAVAILABLE: u16 = 503;

/// Налаштування вікна: чи піднімати і на якому порті.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UiConfig {
    enabled: bool,
    port: u16,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "-h" || a == "--help") {
        eprintln!("{}", usage());
        return;
    }

    let env_port = std::env::var(UI_PORT_ENV).ok();
    let ui = match parse_args(&args, env_port.as_deref()) {
        Ok(cfg) => cfg,
        Err(msg) => {
            eprintln!("exchange-mcp: {msg}\n\n{}", usage());
            std::process::exit(2);
        }
    };

    if ui.enabled {
        start_ui(ui.port);
    } else {
        eprintln!("exchange-mcp: ui.disabled reason=--no-ui");
    }

    if let Err(e) = exchange_mcp::run_stdio() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

/// Підказка про запуск.
fn usage() -> String {
    format!(
        "Використання: exchange-mcp [--no-ui] [--ui-port N]\n\
         \n\
         Сервер обміну говорить MCP через stdin/stdout. Додатково піднімає\n\
         сторінку перегляду на 127.0.0.1 — за замовчуванням увімкнено.\n\
         \n\
         --no-ui        не піднімати сторінку\n\
         --ui-port N    порт сторінки, за замовчуванням {DEFAULT_UI_PORT}\n\
         -h, --help     ця підказка\n\
         \n\
         Змінні середовища:\n\
         {UI_PORT_ENV}   порт сторінки; аргумент --ui-port важливіший\n\
         {db_env}        шлях до бази обміну, за замовчуванням {db}\n\
         {now_env}    шлях до NOW.md, за замовчуванням {now}\n\
         \n\
         Порт зайнятий — сервер працює далі без сторінки: обмін важливіший.",
        db_env = exchange_mcp::DB_ENV,
        now_env = exchange_mcp::NOW_MD_ENV,
        db = exchange_mcp::PROD_DB,
        now = exchange_mcp::PROD_NOW_MD,
    )
}

/// Розібрати аргументи вікна.
///
/// За замовчуванням UI **увімкнено**: пряма вимога людини — щоб вікно було
/// без правки конфігів. `env_port` передається значенням, а не читається
/// звідси, щоб пріоритет «аргумент перебиває env» перевірявся тестами без
/// `std::env::set_var` — той небезпечний і ламає паралельні тести.
fn parse_args(args: &[String], env_port: Option<&str>) -> Result<UiConfig, String> {
    let mut enabled = true;
    let mut arg_port: Option<u16> = None;

    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--no-ui" {
            enabled = false;
            i += 1;
            continue;
        }
        let raw = if let Some(v) = a.strip_prefix("--ui-port=") {
            i += 1;
            v.to_string()
        } else if a == "--ui-port" {
            let v = args
                .get(i + 1)
                .ok_or_else(|| "після --ui-port бракує числа".to_string())?;
            i += 2;
            v.clone()
        } else {
            return Err(format!("невідомий аргумент: {a}"));
        };
        arg_port = Some(parse_port(&raw)?);
    }

    // Аргумент перебиває env. Порожня або зіпсована змінна не має валити
    // сервер: обмін важливіший за вікно, тож сміття в env — це порт за
    // замовчуванням і рядок у stderr, а не вихід із кодом 2.
    let port = match arg_port {
        Some(p) => p,
        None => match env_port.map(str::trim).filter(|v| !v.is_empty()) {
            Some(v) => match parse_port(v) {
                Ok(p) => p,
                Err(msg) => {
                    eprintln!("exchange-mcp: {UI_PORT_ENV}: {msg}; беру {DEFAULT_UI_PORT}");
                    DEFAULT_UI_PORT
                }
            },
            None => DEFAULT_UI_PORT,
        },
    };

    Ok(UiConfig { enabled, port })
}

/// Порт із рядка.
///
/// Нуль відхиляється: ОС видала б випадковий вільний порт, і людина не знала
/// б, куди йти браузером, — а сенс вікна саме в тому, щоб його відкрити.
fn parse_port(raw: &str) -> Result<u16, String> {
    let n: u16 = raw
        .parse()
        .map_err(|_| format!("порт має бути числом 1..65535, а не {raw:?}"))?;
    if n == 0 {
        return Err("порт 0 не годиться: адресу тоді обирає ОС".to_string());
    }
    Ok(n)
}

/// Шляхи ті самі, що бере сам сервер: [`exchange_mcp::resolve_paths`].
///
/// Свідомо **не** `exchange_ui::db::db_path()`: та функція трактує порожню
/// змінну як заданий шлях, і вікно тоді дивилось би не в ту базу, що сервер.
/// Розбіжність показувала б людині чужий стан — гірше за відсутнє вікно.
fn paths_from_env() -> (PathBuf, PathBuf) {
    let db = std::env::var(exchange_mcp::DB_ENV).ok();
    let now = std::env::var(exchange_mcp::NOW_MD_ENV).ok();
    exchange_mcp::resolve_paths(db.as_deref(), now.as_deref())
}

/// Спробувати підняти вікно. Невдача — це рядок у stderr, а не зупинка.
///
/// ⚠️ Порт зайнятий означає, що вікно вже тримає інший агент. Другий інстанс
/// **не падає** і не шукає інший порт: два однакові вікна на різних портах
/// плутали б людину сильніше, ніж одне. Хто перший стартував — той тримає.
fn start_ui(port: u16) {
    let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("exchange-mcp: ui.unavailable port={port} reason=in_use detail={e}");
            return;
        }
    };

    let (db_path, now_md) = paths_from_env();

    // ⚠️ Окремий потік, і паніка в ньому не має валити сервер. `catch_unwind`
    // тут не для краси: `serve` викликає обробник на кожен запит, а обмін
    // мусить пережити будь-який зіпсований запит. Помер потік — лишається
    // сервер без вікна, тобто рівно те саме, що при зайнятому порті.
    let spawned = std::thread::Builder::new()
        .name("exchange-ui".to_string())
        .spawn(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                http::serve(listener, move |req| handle(req, &db_path, &now_md));
            }));
            if outcome.is_err() {
                eprintln!("exchange-mcp: ui.stopped reason=panic; обмін працює далі");
            }
        });

    match spawned {
        Ok(_) => {
            eprintln!("exchange-mcp: ui.listening http://127.0.0.1:{port}/ (база лише на читання)")
        }
        Err(e) => eprintln!("exchange-mcp: ui.unavailable port={port} reason=no_thread detail={e}"),
    }
}

/// Один запит: свіже читання бази, далі сторінка.
///
/// ⚠️ Жодного `println!`: усе, що тут може захотіти сказати, йде в stderr.
fn handle(req: &Request, db_path: &Path, now_md: &Path) -> (u16, String) {
    if req.path != "/" && req.path != "/index.html" {
        return (404, not_found_page(&req.path));
    }
    match db::read_snapshot_now(db_path, now_md) {
        Ok(snap) => (200, render_page(&snap)),
        Err(err) => {
            eprintln!("exchange-mcp: ui.read_failed {err}");
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
/// Дослівна причина просто на сторінці: інакше людина мусила б лізти в лог
/// сесії агента, куди вона зазвичай не дивиться.
fn error_page(db_path: &Path, err: &DbError) -> String {
    simple_page(
        "Базу обміну прочитати не вдалось",
        &format!(
            "<p>Сервер обміну живий, але вікно даних показати не може.</p>\
             <p><b>База:</b> <code>{}</code></p>\
             <pre>{}</pre>\
             <p>Сторінка сама оновиться через 10 с — якщо причину усунути, \
             вона запрацює без перезапуску сервера.</p>",
            escape(&db_path.display().to_string()),
            escape(&err.to_string())
        ),
    )
}

/// Мінімальна сторінка для службових повідомлень.
///
/// Свій каркас, а не `render_page`: та описує **стан обміну**, і порожній
/// `Snapshot` показав би нулі там, де насправді нічого не прочитано.
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

    /// Пряма вимога людини: нічого не налаштовувати — вікно вже є.
    #[test]
    fn ui_is_on_by_default_on_9750() {
        let cfg = parse_args(&[], None).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.port, 9750);
        assert_eq!(cfg.port, DEFAULT_UI_PORT);
    }

    #[test]
    fn no_ui_disables_window_but_keeps_port_value() {
        let cfg = parse_args(&args(&["--no-ui"]), None).unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.port, DEFAULT_UI_PORT);
    }

    #[test]
    fn ui_port_in_both_forms() {
        assert_eq!(
            parse_args(&args(&["--ui-port", "9999"]), None).unwrap().port,
            9999
        );
        assert_eq!(
            parse_args(&args(&["--ui-port=9998"]), None).unwrap().port,
            9998
        );
    }

    #[test]
    fn env_sets_port_when_no_arg() {
        let cfg = parse_args(&[], Some("9123")).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.port, 9123);
    }

    /// Аргумент перебиває env — інакше змінна середовища, забута в профілі,
    /// мовчки ігнорувала б те, що людина щойно набрала руками.
    #[test]
    fn arg_beats_env() {
        let cfg = parse_args(&args(&["--ui-port", "9999"]), Some("9123")).unwrap();
        assert_eq!(cfg.port, 9999);
    }

    /// Зіпсована env не валить сервер: обмін важливіший за вікно.
    #[test]
    fn broken_env_falls_back_to_default() {
        assert_eq!(parse_args(&[], Some("ні")).unwrap().port, DEFAULT_UI_PORT);
        assert_eq!(parse_args(&[], Some("0")).unwrap().port, DEFAULT_UI_PORT);
        assert_eq!(parse_args(&[], Some("   ")).unwrap().port, DEFAULT_UI_PORT);
        assert_eq!(parse_args(&[], Some("")).unwrap().port, DEFAULT_UI_PORT);
    }

    /// Сміття в аргументах — зрозуміла помилка, а не паніка.
    #[test]
    fn garbage_args_are_reported_not_panicked() {
        for bad in [
            args(&["--ui-port", "ні"]),
            args(&["--ui-port"]),
            args(&["--ui-port", "0"]),
            args(&["--ui-port", "70000"]),
            args(&["--noui"]),
            args(&["--port", "9750"]),
            args(&["сміття"]),
        ] {
            let err = parse_args(&bad, None).unwrap_err();
            assert!(!err.is_empty(), "порожнє повідомлення для {bad:?}");
        }
    }

    #[test]
    fn no_ui_together_with_port_parses() {
        let cfg = parse_args(&args(&["--no-ui", "--ui-port", "9001"]), None).unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.port, 9001);
    }

    #[test]
    fn usage_names_switches_and_defaults() {
        let u = usage();
        assert!(u.contains("--no-ui"));
        assert!(u.contains("--ui-port"));
        assert!(u.contains("9750"));
        assert!(u.contains(UI_PORT_ENV));
        assert!(u.contains(exchange_mcp::DB_ENV));
        assert!(u.contains(exchange_mcp::NOW_MD_ENV));
    }

    /// Вікно мусить дивитись у ту саму базу, що й сервер.
    #[test]
    fn paths_match_the_servers_own_resolution() {
        let (db, now) = exchange_mcp::resolve_paths(None, None);
        assert_eq!(db, PathBuf::from(exchange_mcp::PROD_DB));
        assert_eq!(now, PathBuf::from(exchange_mcp::PROD_NOW_MD));
        let (db, now) = exchange_mcp::resolve_paths(Some("  "), Some(""));
        assert_eq!(db, PathBuf::from(exchange_mcp::PROD_DB));
        assert_eq!(now, PathBuf::from(exchange_mcp::PROD_NOW_MD));
    }

    #[test]
    fn error_page_names_cause_and_escapes_it() {
        let err = DbError::Missing {
            path: "C:\\<немає>.db".to_string(),
        };
        let html = error_page(Path::new("C:\\<немає>.db"), &err);
        assert!(html.contains("Базу обміну прочитати не вдалось"));
        assert!(!html.contains("<немає>"));
        assert!(html.contains("&lt;немає&gt;"));
    }

    #[test]
    fn not_found_page_escapes_path() {
        let html = not_found_page("/<script>x</script>");
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
    }
}
