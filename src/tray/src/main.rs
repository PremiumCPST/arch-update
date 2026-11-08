//! arch-update-tray - A systray applet for Arch-Update
//! https://github.com/Antiz96/arch-update
//! SPDX-License-Identifier: GPL-3.0-or-later

use log::error;
use std::process;
use tokio::runtime;

mod i18n;
mod icon_statefile;
mod tray;
mod tray_helpers;
mod updates_statefiles;

fn main() {
    // Initialize logger
    env_logger::init();

    // Initialize localization
    let i18n_dir = i18n::get_i18n_dir();
    i18n::init_i18n(&i18n_dir);

    // Get the icon statefile
    let icon_statefile = icon_statefile::get_icon_statefile().unwrap_or_else(|error| {
        error!("{error:?}");
        process::exit(1);
    });

    // Get the updates statefiles
    let updates_statefiles = updates_statefiles::get_updates_statefiles().unwrap_or_else(|error| {
        error!("{error:?}");
        process::exit(1);
    });

    // Create single-threaded tokio runtime and start the systray applet
    let tokio_runtime = runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|error| {
            error!("Failed to create Tokio runtime: {error}");
            process::exit(1);
        });

    // Start the systray applet
    tokio_runtime
        .block_on(tray::run(icon_statefile, updates_statefiles))
        .unwrap_or_else(|error| {
            error!("{error:?}");
            process::exit(1);
        });
}
