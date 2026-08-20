// minimax-quota-tray — Rust port. Currently exposes the pure-logic modules
// (burn, config, parse). The UI/tray/fetch/keyring modules will be filled
// in incrementally in subsequent commits; the gjs file remains in production.
mod burn;
mod config;
mod parse;

fn main() {
    println!("minimax-quota-tray (Rust) — bootstrap only; UI not yet implemented");
}