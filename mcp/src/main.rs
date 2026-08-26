fn main() {
    if let Err(e) = exchange_mcp::run_stdio() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
