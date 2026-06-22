//! WebDriver server binary. Usage: `cargo run -p webdriver -- --port 4444`.

fn main() {
    // Automation must never serve stale resources: WPT (and any test runner) regenerates pages and
    // endpoints per run, so the engine's opt-in on-disk HTTP cache would poison results by replaying
    // a previous run's body for a URL. Disable it before any fetch can initialize the net layer
    // (unless the user explicitly set NET_CACHE_DIR).
    if std::env::var_os("NET_CACHE_DIR").is_none() {
        std::env::set_var("NET_CACHE_DIR", "off");
    }
    // `.https` WPT tests are served by `wpt serve` with a self-signed cert. As a test driver we
    // accept it (matching the `acceptInsecureCerts` capability we advertise) so secure-context tests
    // load over TLS without threading the WPT CA path through.
    if std::env::var_os("WPT_INSECURE_TLS").is_none() {
        std::env::set_var("WPT_INSECURE_TLS", "1");
    }

    let mut port: u16 = 4444;
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--port" | "-p" => {
                if let Some(v) = args.get(i + 1).and_then(|s| s.parse().ok()) {
                    port = v;
                }
                i += 2;
            }
            "--help" | "-h" => {
                eprintln!("usage: webdriver [--port <port>]   (default 4444)");
                return;
            }
            _ => i += 1,
        }
    }

    if let Err(e) = webdriver::server::run(port) {
        eprintln!("webdriver server error: {e}");
        std::process::exit(1);
    }
}
