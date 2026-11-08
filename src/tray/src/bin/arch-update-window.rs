//! GTK4 / Libadwaita main window for Arch-Update.
//! SPDX-License-Identifier: GPL-3.0-or-later

use log::error;
use std::process;

#[path = "../i18n.rs"]
mod i18n;
#[path = "../updates_statefiles.rs"]
mod updates_statefiles;
#[path = "../window.rs"]
mod window;

fn main() {
    env_logger::init();

    let i18n_dir = i18n::get_i18n_dir();
    i18n::init_i18n(&i18n_dir);

    let updates_statefiles = updates_statefiles::get_updates_statefiles().unwrap_or_else(|error| {
        error!("{error:?}");
        process::exit(1);
    });
    window::run(updates_statefiles);
}
