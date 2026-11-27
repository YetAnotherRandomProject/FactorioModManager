#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod check;
mod config;
mod factorio;
mod instance;
mod modpack;
mod portal;
mod store;
mod sync;
mod transfer;
mod updater;
mod version;

slint::include_modules!();

fn main() -> anyhow::Result<()> {
    app::run()
}
