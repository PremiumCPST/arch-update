//! GTK4 / Libadwaita main window for the systray applet.

use adw::prelude::*;
use gettextrs::gettext;
use gtk::{Align, Orientation, glib};
use std::cell::{Cell, RefCell};
use std::fs;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::Duration;

use crate::updates_statefiles::UpdatesStateFiles;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UpdateSource {
    Packages,
    Aur,
    Flatpak,
}

impl UpdateSource {
    fn label(self) -> String {
        match self {
            Self::Packages => gettext("Packages"),
            Self::Aur => gettext("AUR"),
            Self::Flatpak => gettext("Flatpak"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PackageUpdate {
    source: UpdateSource,
    name: String,
    installed_version: Option<String>,
    available_version: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct UpdateSection {
    source: UpdateSource,
    entries: Vec<PackageUpdate>,
}

const SOURCES: [(UpdateSource, fn(&UpdatesStateFiles) -> &std::path::Path); 3] = [
    (UpdateSource::Packages, |files| files.packages.as_path()),
    (UpdateSource::Aur, |files| files.aur.as_path()),
    (UpdateSource::Flatpak, |files| files.flatpak.as_path()),
];

fn parse_update(source: UpdateSource, line: &str) -> PackageUpdate {
    let (name, installed_version, available_version) =
        if let Some((installed, available)) = line.split_once(" -> ") {
            let mut installed = installed.split_whitespace();
            (
                installed.next().unwrap_or(line).to_owned(),
                installed.next().map(str::to_owned),
                available.split_whitespace().next().map(str::to_owned),
            )
        } else if source == UpdateSource::Flatpak {
            match line.rsplit_once(' ') {
                Some((name, version)) if !name.is_empty() && !version.contains(' ') => {
                    (name.to_owned(), None, Some(version.to_owned()))
                }
                _ => (line.to_owned(), None, None),
            }
        } else {
            (
                line.split_whitespace().next().unwrap_or(line).to_owned(),
                None,
                None,
            )
        };

    PackageUpdate {
        source,
        name,
        installed_version,
        available_version,
    }
}

fn read_updates(files: &UpdatesStateFiles) -> Vec<UpdateSection> {
    SOURCES
        .iter()
        .filter_map(|(source, path)| {
            let entries = fs::read_to_string(path(files))
                .unwrap_or_default()
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(|line| parse_update(*source, line))
                .collect::<Vec<_>>();
            (!entries.is_empty()).then(|| UpdateSection {
                source: *source,
                entries,
            })
        })
        .collect()
}

fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program)
        .env("LC_ALL", "C")
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

enum UpdateEvent {
    Output(String),
    Finished(i32),
    Failed(String),
}

fn forward_output<R: Read + Send + 'static>(
    mut stream: R,
    tx: Sender<UpdateEvent>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(length) => {
                    let output = String::from_utf8_lossy(&buffer[..length]).into_owned();
                    if tx.send(UpdateEvent::Output(output)).is_err() {
                        break;
                    }
                }
            }
        }
    })
}

fn start_update(system_only: bool) -> anyhow::Result<(Receiver<UpdateEvent>, Sender<String>)> {
    let (event_tx, event_rx) = mpsc::channel();
    let (input_tx, input_rx) = mpsc::channel::<String>();
    let command = if system_only {
        "arch-update --system-only"
    } else {
        "arch-update"
    };
    let mut child = Command::new("script")
        .args(["-qefc", command, "/dev/null"])
        .env("TERM", "dumb")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let mut child_stdin = child.stdin.take().expect("piped child stdin");
    thread::spawn(move || {
        while let Ok(input) = input_rx.recv() {
            if child_stdin.write_all(input.as_bytes()).is_err() || child_stdin.flush().is_err() {
                break;
            }
        }
    });

    let mut readers = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        readers.push(forward_output(stdout, event_tx.clone()));
    }
    if let Some(stderr) = child.stderr.take() {
        readers.push(forward_output(stderr, event_tx.clone()));
    }

    thread::spawn(move || match child.wait() {
        Ok(status) => {
            for reader in readers {
                let _ = reader.join();
            }
            let _ = event_tx.send(UpdateEvent::Finished(status.code().unwrap_or(1)));
        }
        Err(error) => {
            let _ = event_tx.send(UpdateEvent::Failed(error.to_string()));
        }
    });

    Ok((event_rx, input_tx))
}

fn clean_terminal_output(output: &str) -> String {
    let mut cleaned = String::with_capacity(output.len());
    let mut chars = output.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\u{1b}' {
            if chars.next() == Some('[') {
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
        } else if character == '\r' {
            cleaned.push('\n');
        } else if character == '\u{8}' {
            cleaned.pop();
        } else {
            cleaned.push(character);
        }
    }
    cleaned
}

fn pacman_metadata(name: &str) -> Vec<(String, String)> {
    let Some(output) = command_output("pacman", &["-Si", name]) else {
        return Vec::new();
    };

    let mut details = Vec::new();
    for line in output.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        let label = match key {
            "Repository" => gettext("Repository"),
            "Version" => gettext("Available version"),
            "Description" => gettext("Description"),
            "URL" => gettext("Project website"),
            "Download Size" => gettext("Download size"),
            "Installed Size" => gettext("Installed size"),
            "Depends On" => gettext("Dependencies"),
            _ => continue,
        };
        if !value.is_empty() && value != "None" {
            details.push((label, value.to_owned()));
        }
    }
    details
}

fn aur_metadata(name: &str) -> Vec<(String, String)> {
    let configured = std::env::var("ARCH_UPDATE_AUR_HELPER").ok();
    let helpers = configured
        .filter(|helper| !helper.is_empty())
        .into_iter()
        .chain(["paru".to_owned(), "yay".to_owned(), "pikaur".to_owned()]);

    for helper in helpers {
        let output = Command::new("timeout")
            .env("LC_ALL", "C")
            .args(["8", &helper, "-Si", name])
            .output()
            .ok();
        if let Some(output) = output.filter(|output| output.status.success()) {
            let text = String::from_utf8_lossy(&output.stdout);
            let mut details = Vec::new();
            for line in text.lines() {
                let Some((key, value)) = line.split_once(':') else {
                    continue;
                };
                let label = match key.trim() {
                    "Repository" => gettext("Repository"),
                    "Version" => gettext("Available version"),
                    "Description" => gettext("Description"),
                    "URL" => gettext("Project website"),
                    "Download Size" => gettext("Download size"),
                    "Installed Size" => gettext("Installed size"),
                    "Depends On" => gettext("Dependencies"),
                    _ => continue,
                };
                let value = value.trim();
                if !value.is_empty() && value != "None" {
                    details.push((label, value.to_owned()));
                }
            }
            if !details.is_empty() {
                return details;
            }
        }
    }
    Vec::new()
}

fn flatpak_metadata(update: &PackageUpdate) -> Vec<(String, String)> {
    let Some(installed_apps) = command_output(
        "flatpak",
        &["list", "--columns=application,name,version,origin"],
    ) else {
        return Vec::new();
    };
    let app = installed_apps.lines().find_map(|line| {
        let columns = line.split('\t').collect::<Vec<_>>();
        (columns.len() >= 2
            && (columns[0].trim() == update.name || columns[1].trim() == update.name))
            .then(|| {
                (
                    columns[0].trim().to_owned(),
                    columns
                        .get(2)
                        .copied()
                        .unwrap_or_default()
                        .trim()
                        .to_owned(),
                )
            })
    });
    let Some((app_id, installed_version)) = app else {
        return Vec::new();
    };

    let mut details = vec![(gettext("Application ID"), app_id.clone())];
    if !installed_version.is_empty() {
        details.push((gettext("Installed version"), installed_version));
    }
    let available = command_output(
        "flatpak",
        &[
            "remote-ls",
            "--updates",
            "--cached",
            "--columns=application,name,version,origin",
        ],
    )
    .and_then(|updates| {
        updates.lines().find_map(|line| {
            let columns = line.split('\t').collect::<Vec<_>>();
            (columns.len() >= 3 && columns[0].trim() == app_id)
                .then(|| columns[2].trim().to_owned())
        })
    });
    if let Some(version) = available.filter(|version| !version.is_empty()) {
        details.push((gettext("Available version"), version));
    }
    for (flag, label) in [
        ("--show-origin", gettext("Repository")),
        ("--show-description", gettext("Description")),
    ] {
        if let Some(value) =
            command_output("flatpak", &["info", flag, &app_id]).filter(|value| !value.is_empty())
        {
            details.push((label, value));
        }
    }
    details
}

fn load_package_details(update: &PackageUpdate) -> Vec<(String, String)> {
    let mut details = vec![(gettext("Source"), update.source.label())];
    let mut metadata = match update.source {
        UpdateSource::Packages => pacman_metadata(&update.name),
        UpdateSource::Aur => aur_metadata(&update.name),
        UpdateSource::Flatpak => flatpak_metadata(update),
    };
    let installed_version = update.installed_version.clone().or_else(|| {
        (update.source != UpdateSource::Flatpak)
            .then(|| command_output("pacman", &["-Q", &update.name]))
            .flatten()
            .and_then(|output| output.split_whitespace().nth(1).map(str::to_owned))
    });
    if let Some(version) = installed_version {
        details.push((gettext("Installed version"), version.clone()));
    }
    if let Some(version) = &update.available_version {
        details.push((gettext("Available version"), version.clone()));
    }
    let installed_in_state = update.installed_version.is_some();
    let available_in_state = update.available_version.is_some();
    metadata.retain(|(label, _)| {
        !(installed_in_state && label == &gettext("Installed version"))
            && !(available_in_state && label == &gettext("Available version"))
    });
    if metadata.is_empty() {
        metadata.push((
            gettext("Package details"),
            gettext("Additional package information is unavailable."),
        ));
    }
    details.extend(metadata);
    details
}

fn remove_children(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

fn version_summary(update: &PackageUpdate) -> String {
    match (&update.installed_version, &update.available_version) {
        (Some(installed), Some(available)) => format!("{installed} → {available}"),
        (_, Some(available)) => {
            gettext("Available version {version}").replace("{version}", available)
        }
        _ => gettext("Package update"),
    }
}

fn package_row(update: &PackageUpdate) -> adw::ExpanderRow {
    let row = adw::ExpanderRow::builder()
        .title(&update.name)
        .subtitle(version_summary(update))
        .expanded(false)
        .build();
    row.add_prefix(&gtk::Image::from_icon_name(
        "software-update-available-symbolic",
    ));

    let loading_row = adw::ActionRow::builder()
        .title(gettext("Expand to load package details"))
        .build();
    row.add_row(&loading_row);

    let loaded = Rc::new(Cell::new(false));
    let loaded_in_callback = Rc::clone(&loaded);
    let update = update.clone();
    row.connect_expanded_notify(move |row| {
        if !row.is_expanded() || loaded_in_callback.replace(true) {
            return;
        }

        loading_row.set_title(&gettext("Loading package details…"));
        let (sender, receiver) = mpsc::channel();
        let update = update.clone();
        thread::spawn(move || {
            let _ = sender.send(load_package_details(&update));
        });

        let row = row.clone();
        let loading_row = loading_row.clone();
        glib::timeout_add_local(Duration::from_millis(50), move || {
            match receiver.try_recv() {
                Ok(details) => {
                    row.remove(&loading_row);
                    for (label, value) in details {
                        if label == gettext("Project website") {
                            let detail = adw::ActionRow::builder().title(&label).build();
                            let link =
                                gtk::LinkButton::with_label(&value, &gettext("Open project page"));
                            detail.add_suffix(&link);
                            row.add_row(&detail);
                        } else {
                            row.add_row(
                                &adw::ActionRow::builder()
                                    .title(&label)
                                    .subtitle(&value)
                                    .subtitle_lines(3)
                                    .build(),
                            );
                        }
                    }
                    glib::ControlFlow::Break
                }
                Err(TryRecvError::Empty) => glib::ControlFlow::Continue,
                Err(TryRecvError::Disconnected) => {
                    loading_row.set_title(&gettext("Package details could not be loaded."));
                    glib::ControlFlow::Break
                }
            }
        });
    });

    row
}

fn render(
    list: &gtk::Box,
    stack: &gtk::Stack,
    status: &adw::StatusPage,
    count_label: &gtk::Label,
    updates: &[UpdateSection],
) {
    remove_children(list);
    let count = updates
        .iter()
        .map(|section| section.entries.len())
        .sum::<usize>();

    if count == 0 {
        status.set_title(&gettext("System is up to date"));
        status.set_description(Some(&gettext("There are no pending updates.")));
        status.set_icon_name(Some("emblem-ok-symbolic"));
        stack.set_visible_child_name("empty");
        list.set_visible(false);
    } else {
        stack.set_visible_child_name("updates");
        list.set_visible(true);
        for section in updates {
            let group = adw::PreferencesGroup::builder()
                .title(section.source.label())
                .build();
            for update in &section.entries {
                group.add(&package_row(update));
            }
            let revealer = gtk::Revealer::builder()
                .transition_type(gtk::RevealerTransitionType::SlideDown)
                .transition_duration(180)
                .child(&group)
                .build();
            list.append(&revealer);
            glib::timeout_add_local_once(Duration::from_millis(40), move || {
                revealer.set_reveal_child(true);
            });
        }
    }

    let label = if count == 1 {
        gettext("1 update available")
    } else {
        gettext("{count} updates available").replace("{count}", &count.to_string())
    };
    count_label.set_text(&label);
    count_label.set_visible(count > 0);
}

pub fn run(files: UpdatesStateFiles) {
    let application = adw::Application::builder()
        .application_id("io.github.antiz96.ArchUpdate")
        .build();
    let files = Rc::new(files);

    application.connect_startup(|_| {
        let provider = gtk::CssProvider::new();
        provider.load_from_data(
            ".system-only-update-button { background-color: @card_bg_color; border: 1px solid @borders; border-radius: 9px; }",
        );
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
    });

    application.connect_activate(move |application| {
        let window = adw::ApplicationWindow::builder()
            .application(application)
            .title("Arch-Update")
            .default_width(760)
            .default_height(620)
            .build();

        let toolbar = adw::ToolbarView::new();
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(
            &gtk::Label::builder()
                .label(gettext("Available updates"))
                .css_classes(["title"])
                .build(),
        ));
        toolbar.add_top_bar(&header);

        let content = gtk::Box::new(Orientation::Vertical, 18);
        content.set_margin_top(24);
        content.set_margin_bottom(18);
        content.set_margin_start(24);
        content.set_margin_end(24);
        content.set_vexpand(true);

        let status = adw::StatusPage::builder()
            .title(gettext("System is up to date"))
            .description(gettext("There are no pending updates."))
            .icon_name("emblem-ok-symbolic")
            .vexpand(true)
            .build();
        let list = gtk::Box::new(Orientation::Vertical, 18);
        list.set_valign(Align::Center);
        let scroll = gtk::ScrolledWindow::builder()
            .child(&list)
            .vexpand(true)
            .min_content_height(380)
            .build();
        let page = adw::Clamp::builder()
            .maximum_size(720)
            .child(&scroll)
            .build();
        let status_page = adw::Clamp::builder()
            .maximum_size(720)
            .child(&status)
            .build();
        let stack = gtk::Stack::builder()
            .vexpand(true)
            .transition_type(gtk::StackTransitionType::Crossfade)
            .transition_duration(180)
            .build();
        stack.add_named(&status_page, Some("empty"));
        stack.add_named(&page, Some("updates"));

        let run_page = gtk::Box::new(Orientation::Vertical, 12);
        let run_status = gtk::Label::builder()
            .label(gettext("Starting update…"))
            .xalign(0.0)
            .build();
        let run_output = gtk::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .monospace(true)
            .wrap_mode(gtk::WrapMode::WordChar)
            .build();
        let run_scroll = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .child(&run_output)
            .build();
        let input_row = gtk::Box::new(Orientation::Horizontal, 8);
        let run_input = gtk::Entry::builder()
            .hexpand(true)
            .visibility(false)
            .placeholder_text(gettext("Enter a response to continue…"))
            .tooltip_text(gettext(
                "Responses are hidden because this field may receive a password.",
            ))
            .build();
        let send_input = gtk::Button::with_label(&gettext("Send"));
        input_row.append(&run_input);
        input_row.append(&send_input);
        let return_button = gtk::Button::with_label(&gettext("Back to updates"));
        return_button.set_halign(Align::End);
        return_button.set_visible(false);
        run_page.append(&run_status);
        run_page.append(&run_scroll);
        run_page.append(&input_row);
        run_page.append(&return_button);
        stack.add_named(&run_page, Some("running"));
        content.append(&stack);

        let footer = gtk::Box::new(Orientation::Vertical, 12);
        footer.add_css_class("toolbar");
        footer.set_halign(Align::Center);
        let full_update_content = adw::ButtonContent::builder()
            .icon_name("software-update-available-symbolic")
            .label(gettext("Update system + AUR"))
            .build();
        let full_update_button = gtk::Button::builder()
            .child(&full_update_content)
            .css_classes(["suggested-action"])
            .tooltip_text(gettext(
                "Update system packages, AUR and Flatpak when enabled.",
            ))
            .build();
        footer.append(&full_update_button);

        let system_only_content = adw::ButtonContent::builder()
            .icon_name("software-update-available-symbolic")
            .label(gettext("Update system packages only"))
            .build();
        let system_only_button = gtk::Button::builder()
            .child(&system_only_content)
            .css_classes(["system-only-update-button"])
            .tooltip_text(gettext("Skip AUR and Flatpak packages."))
            .build();
        footer.append(&system_only_button);

        let count_label = gtk::Label::new(None);
        count_label.add_css_class("dim-label");
        footer.append(&count_label);

        content.append(&footer);

        let input_sender = Rc::new(RefCell::new(None::<Sender<String>>));
        let send_response: Rc<dyn Fn()> = Rc::new({
            let input_sender = Rc::clone(&input_sender);
            let run_input = run_input.clone();
            move || {
                let response = run_input.text();
                if let Some(sender) = input_sender.borrow().as_ref() {
                    let _ = sender.send(format!("{response}\n"));
                }
                run_input.set_text("");
            }
        });
        send_input.connect_clicked({
            let send_response = Rc::clone(&send_response);
            move |_| send_response()
        });
        run_input.connect_activate(move |_| send_response());

        let update_running = Rc::new(Cell::new(false));
        let begin_update: Rc<dyn Fn(bool)> = Rc::new({
            let stack = stack.clone();
            let run_status = run_status.clone();
            let run_output = run_output.clone();
            let update_running = Rc::clone(&update_running);
            let input_sender = Rc::clone(&input_sender);
            let full_update_button = full_update_button.clone();
            let system_only_button = system_only_button.clone();
            let footer = footer.clone();
            let return_button = return_button.clone();
            move |system_only| {
                update_running.set(true);
                run_output.buffer().set_text("");
                run_status.set_text(&gettext("Starting update…"));
                return_button.set_visible(false);
                footer.set_visible(false);
                full_update_button.set_sensitive(false);
                system_only_button.set_sensitive(false);
                stack.set_visible_child_name("running");

                match start_update(system_only) {
                    Ok((events, sender)) => {
                        *input_sender.borrow_mut() = Some(sender);
                        let events = Rc::new(RefCell::new(events));
                        let events_for_poll = Rc::clone(&events);
                        let update_running_for_poll = Rc::clone(&update_running);
                        let input_sender_for_poll = Rc::clone(&input_sender);
                        let run_status = run_status.clone();
                        let run_output = run_output.clone();
                        let footer = footer.clone();
                        let full_update_button = full_update_button.clone();
                        let system_only_button = system_only_button.clone();
                        let return_button = return_button.clone();
                        glib::timeout_add_local(Duration::from_millis(50), move || {
                            loop {
                                match events_for_poll.borrow().try_recv() {
                                    Ok(UpdateEvent::Output(output)) => {
                                        let output = clean_terminal_output(&output);
                                        if !output.is_empty() {
                                            let buffer = run_output.buffer();
                                            let mut end = buffer.end_iter();
                                            buffer.insert(&mut end, &output);
                                            let mut end = buffer.end_iter();
                                            run_output
                                                .scroll_to_iter(&mut end, 0.0, false, 0.0, 1.0);
                                        }
                                    }
                                    Ok(UpdateEvent::Finished(0)) => {
                                        update_running_for_poll.set(false);
                                        run_status.set_text(&gettext("Update completed."));
                                        return_button.set_visible(true);
                                        full_update_button.set_sensitive(true);
                                        system_only_button.set_sensitive(true);
                                        *input_sender_for_poll.borrow_mut() = None;
                                        return glib::ControlFlow::Break;
                                    }
                                    Ok(UpdateEvent::Finished(code)) => {
                                        update_running_for_poll.set(false);
                                        run_status.set_text(
                                            &gettext(
                                                "Update ended with an error (exit code {code}).",
                                            )
                                            .replace("{code}", &code.to_string()),
                                        );
                                        return_button.set_visible(true);
                                        full_update_button.set_sensitive(true);
                                        system_only_button.set_sensitive(true);
                                        *input_sender_for_poll.borrow_mut() = None;
                                        return glib::ControlFlow::Break;
                                    }
                                    Ok(UpdateEvent::Failed(error)) => {
                                        update_running_for_poll.set(false);
                                        run_status.set_text(
                                            &gettext("Could not start the update: {error}.")
                                                .replace("{error}", &error),
                                        );
                                        return_button.set_visible(true);
                                        full_update_button.set_sensitive(true);
                                        system_only_button.set_sensitive(true);
                                        *input_sender_for_poll.borrow_mut() = None;
                                        return glib::ControlFlow::Break;
                                    }
                                    Err(TryRecvError::Empty) => break,
                                    Err(TryRecvError::Disconnected) => {
                                        update_running_for_poll.set(false);
                                        run_status.set_text(&gettext(
                                            "The update process stopped unexpectedly.",
                                        ));
                                        return_button.set_visible(true);
                                        footer.set_visible(true);
                                        full_update_button.set_sensitive(true);
                                        system_only_button.set_sensitive(true);
                                        *input_sender_for_poll.borrow_mut() = None;
                                        return glib::ControlFlow::Break;
                                    }
                                }
                            }
                            glib::ControlFlow::Continue
                        });
                    }
                    Err(error) => {
                        update_running.set(false);
                        run_status.set_text(
                            &gettext("Could not start the update: {error}.")
                                .replace("{error}", &error.to_string()),
                        );
                        return_button.set_visible(true);
                        full_update_button.set_sensitive(true);
                        system_only_button.set_sensitive(true);
                    }
                }
            }
        });
        full_update_button.connect_clicked({
            let begin_update = Rc::clone(&begin_update);
            move |_| begin_update(false)
        });
        system_only_button.connect_clicked({
            let begin_update = Rc::clone(&begin_update);
            move |_| begin_update(true)
        });
        return_button.connect_clicked({
            let stack = stack.clone();
            let footer = footer.clone();
            move |_| {
                stack.set_visible_child_name("updates");
                footer.set_visible(true);
            }
        });

        toolbar.set_content(Some(&content));
        window.set_content(Some(&toolbar));

        let last_updates = Rc::new(RefCell::new(Vec::<UpdateSection>::new()));
        let candidate_updates = Rc::new(RefCell::new(None::<(Vec<UpdateSection>, u8)>));
        let initial = read_updates(&files);
        render(&list, &stack, &status, &count_label, &initial);
        *last_updates.borrow_mut() = initial;

        // Checks can update these files while the window is open; reread them periodically.
        let files = Rc::clone(&files);
        let last_updates = Rc::clone(&last_updates);
        let candidate_updates = Rc::clone(&candidate_updates);
        let update_running = Rc::clone(&update_running);
        let list = list.clone();
        let stack = stack.clone();
        let status = status.clone();
        let count_label = count_label.clone();
        glib::timeout_add_seconds_local(3, move || {
            if update_running.get() {
                return glib::ControlFlow::Continue;
            }
            let updates = read_updates(&files);
            if *last_updates.borrow() == updates {
                *candidate_updates.borrow_mut() = None;
            } else {
                let mut candidate = candidate_updates.borrow_mut();
                let observations = match candidate.as_mut() {
                    Some((candidate_updates, observations)) if *candidate_updates == updates => {
                        *observations += 1;
                        *observations
                    }
                    _ => {
                        *candidate = Some((updates.clone(), 1));
                        1
                    }
                };
                if observations >= 2 {
                    render(&list, &stack, &status, &count_label, &updates);
                    *last_updates.borrow_mut() = updates;
                    *candidate = None;
                }
            }
            glib::ControlFlow::Continue
        });

        window.present();
    });

    application.run();
}
