// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::Path;

fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    if args
        .get(1)
        .is_some_and(|argument| argument == "--relay-gateway-token")
    {
        let Some(reference) = args.get(2).filter(|reference| {
            reference.starts_with("client-key:") || reference.starts_with("profile:")
        }) else {
            std::process::exit(2);
        };
        let Some(data_dir) = args
            .windows(2)
            .find_map(|pair| (pair[0] == "--relay-data-dir").then_some(pair[1].as_str()))
        else {
            std::process::exit(2);
        };
        match codex_relay_lib::read_relay_gateway_token(reference, Path::new(data_dir)) {
            Ok(token) => print!("{token}"),
            Err(_) => std::process::exit(1),
        }
        return;
    }
    codex_relay_lib::run()
}
