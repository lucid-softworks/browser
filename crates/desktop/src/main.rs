//! Native Rune shell for the shared Rust browser engine.
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]
mod platform;
mod ui;
mod worker;

fn main() {
    rune::set_app_id("com.lucidsoftworks.browser");
    if let Err(error) = ui::app().try_run() {
        eprintln!("Could not start Browser: {error}");
        std::process::exit(1);
    }
}
