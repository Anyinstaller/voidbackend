#![cfg(windows)]
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if let Err(error) = voidbackend::run(std::env::args_os()) {
        voidbackend::log::line(format!("backend: {error:#}"));
        std::process::exit(1);
    }
}
