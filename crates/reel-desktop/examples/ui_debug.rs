//! An interactive, headless control channel for the desktop UI.
//!
//! The real `App` runs under `egui_kittest` with no window, and a tiny Unix
//! socket server lets you drive it a command at a time: find a widget by label,
//! click it, type, press keys, step frames, dump the widget tree, and render a
//! PNG. That turns "try it and see" into something a script (or an agent) can
//! do.
//!
//! ```bash
//! # start the server (background)
//! cargo run -p reel-desktop --example ui_debug -- serve /tmp/opencode/ui.sock /tmp/opencode/ui
//!
//! # drive it
//! cargo run -p reel-desktop --example ui_debug -- send /tmp/opencode/ui.sock "dump"
//! cargo run -p reel-desktop --example ui_debug -- send /tmp/opencode/ui.sock 'click_contains Play'
//! cargo run -p reel-desktop --example ui_debug -- send /tmp/opencode/ui.sock "state"
//! cargo run -p reel-desktop --example ui_debug -- send /tmp/opencode/ui.sock "snapshot home"
//! ```
//!
//! Commands: `state`, `dump`, `navigate <library|add|search|settings|detail <id>>`,
//! `click <label>`, `click_contains <substr>`, `click_role <role> <n>`,
//! `type_role <role> <n> <text>`, `key <name>`, `step [n]`, `snapshot <name>`,
//! `reset <sample|season|paused|duplicates|two_seasons>`, `quit`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

use egui::accesskit::Role;
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use reel_desktop::backend::FakeBackend;
use reel_desktop::testing::{
    sample_library, sample_paused_season_pack, sample_season_pack, sample_two_season_pack,
};
use reel_desktop::{App, LibraryItem, Screen};

type Ui = Harness<'static, App>;

fn build(items: Vec<LibraryItem>) -> Ui {
    let mut harness = Harness::builder()
        .with_size((1400.0, 1000.0))
        .build_ui_state(
            |ui, app: &mut App| {
                let mut frame = eframe::Frame::_new_kittest();
                eframe::App::ui(app, ui, &mut frame);
            },
            App::new(Box::new(FakeBackend::new(items))),
        );
    harness.run_steps(3);
    harness
}

/// A second copy of a season pack, at a different quality, so merging and the
/// per-episode chooser can be exercised.
fn duplicate_season_pack() -> LibraryItem {
    let mut copy = sample_season_pack();
    copy.torrent.id = 11;
    copy.entry.torrent_id = 11;
    copy.entry.info_hash = "1111111111111111111111111111111111111111".to_string();
    copy.torrent.info_hash = copy.entry.info_hash.clone();
    copy.torrent.name = Some("Some.Show.S01.2160p.WEB-DL.x265".to_string());
    copy.entry.release.title = "Some Show".to_string();
    copy.entry.release.attributes.resolution = Some("2160p".to_string());
    copy
}

fn fixture(name: &str) -> Vec<LibraryItem> {
    let mut items = sample_library();
    match name {
        "season" => items.push(sample_season_pack()),
        "paused" => items.push(sample_paused_season_pack()),
        "two_seasons" => items.push(sample_two_season_pack()),
        "duplicates" => {
            items.push(sample_season_pack());
            items.push(duplicate_season_pack());
        }
        _ => {}
    }
    items
}

fn role(name: &str) -> Option<Role> {
    Some(match name.to_ascii_lowercase().as_str() {
        "button" => Role::Button,
        "textinput" => Role::TextInput,
        "multiline" | "multilinetextinput" => Role::MultilineTextInput,
        "checkbox" => Role::CheckBox,
        "label" => Role::Label,
        "combobox" => Role::ComboBox,
        "slider" => Role::Slider,
        "progress" | "progressindicator" => Role::ProgressIndicator,
        _ => return None,
    })
}

fn key(name: &str) -> Option<egui::Key> {
    use egui::Key;
    Some(match name.to_ascii_lowercase().as_str() {
        "enter" => Key::Enter,
        "escape" | "esc" => Key::Escape,
        "space" => Key::Space,
        "f" => Key::F,
        "tab" => Key::Tab,
        "left" | "arrowleft" => Key::ArrowLeft,
        "right" | "arrowright" => Key::ArrowRight,
        "up" | "arrowup" => Key::ArrowUp,
        "down" | "arrowdown" => Key::ArrowDown,
        _ => return None,
    })
}

fn screen(rest: &str) -> Option<Screen> {
    let mut parts = rest.split_whitespace();
    match parts.next()? {
        "library" => Some(Screen::Library),
        "add" => Some(Screen::Add),
        "search" => Some(Screen::Search),
        "settings" => Some(Screen::Settings),
        "player" => Some(Screen::Player),
        "detail" => parts.next()?.parse().ok().map(Screen::Detail),
        _ => None,
    }
}

/// A short, human-readable snapshot of everything the app knows, so a command's
/// effect is visible without a screenshot.
fn state(ui: &Ui, out: &mut String) {
    let app = ui.state();
    out.push_str(&format!(
        "screen={:?} library={} works={}\n",
        app.screen(),
        app.library_len(),
        app.works().len()
    ));
    for work in app.works() {
        let detail = if work.is_series() {
            let copies = work
                .episodes()
                .iter()
                .filter(|episode| episode.variants.len() > 1)
                .count();
            format!("episodes={} with_choices={copies}", work.episodes().len())
        } else {
            format!("versions={}", work.versions().len())
        };
        out.push_str(&format!(
            "  work lead={} kind={:?} copies={} title={:?} ({detail})\n",
            work.lead_torrent_id(),
            work.kind,
            work.torrent_count(),
            work.title
        ));
    }
    for work in app.works() {
        for member in &work.members {
            let id = member.torrent_id();
            if let Some(item) = app.item(id) {
                let included: Vec<usize> = item
                    .torrent
                    .files
                    .iter()
                    .filter(|file| file.included)
                    .map(|file| file.id)
                    .collect();
                out.push_str(&format!(
                    "  item {} hash={} state={} paused={} downloading={} included={:?}\n",
                    id,
                    &item.torrent.info_hash[..8.min(item.torrent.info_hash.len())],
                    item.torrent.stats.state,
                    item.torrent.stats.is_paused(),
                    item.downloading,
                    included
                ));
            }
        }
    }
}

/// Every labelled node on screen, which is the textual view of the UI.
fn dump(ui: &Ui, out: &mut String) {
    let mut count = 0;
    for node in ui.root().children_recursive() {
        let accesskit = node.accesskit_node();
        let label = accesskit.label().unwrap_or_default();
        let value = accesskit.value().unwrap_or_default();
        let role = accesskit.role();
        if label.is_empty() && value.is_empty() && role == Role::GenericContainer {
            continue;
        }
        count += 1;
        if value.is_empty() {
            out.push_str(&format!("  {role:?}: {label:?}\n"));
        } else {
            out.push_str(&format!("  {role:?}: {label:?} = {value:?}\n"));
        }
    }
    out.push_str(&format!("({count} nodes)\n"));
}

fn run_input(ui: &mut Ui, out: &mut String, steps: usize) {
    ui.run_steps(steps.max(1));
    out.push_str(&format!("ok (stepped {})\n", steps.max(1)));
}

/// Something a click will actually do something to. A page title "Search" and
/// the "Search" button share a label; the button is the one that matters.
fn is_clickable(role: Role) -> bool {
    matches!(
        role,
        Role::Button
            | Role::ComboBox
            | Role::CheckBox
            | Role::RadioButton
            | Role::Switch
            | Role::Link
            | Role::Tab
            | Role::MenuItem
    )
}

/// The node a click should target: the first of `nodes` that is actionable,
/// else the first at all.
fn click_node(_ui: &Ui, mut nodes: Vec<egui_kittest::Node<'_>>) {
    if nodes.is_empty() {
        return;
    }
    let preferred = nodes
        .iter()
        .position(|node| is_clickable(node.accesskit_node().role()))
        .unwrap_or(0);
    nodes.swap_remove(preferred).click();
}

/// Execute one command. Returns true to shut the server down.
fn execute(ui: &mut Ui, line: &str, dir: &PathBuf, out: &mut String) -> bool {
    let mut parts = line.splitn(2, ' ');
    let command = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("").trim();

    match command {
        "state" => state(ui, out),
        "dump" => dump(ui, out),
        "navigate" => match screen(rest) {
            Some(target) => {
                ui.state_mut().navigate(target);
                run_input(ui, out, 3);
            }
            None => out.push_str("error: navigate needs library|add|search|settings|detail <id>\n"),
        },
        "click" => {
            let found = {
                let nodes: Vec<_> = ui.get_all_by_label(rest).collect();
                let count = nodes.len();
                click_node(ui, nodes);
                count
            };
            if found == 0 {
                out.push_str(&format!("error: no node labelled {rest:?}\n"));
            } else {
                run_input(ui, out, 3);
                out.push_str(&format!("(clicked one of {found})\n"));
            }
        }
        "click_contains" => {
            let found = {
                let nodes: Vec<_> = ui.get_all_by_label_contains(rest).collect();
                let count = nodes.len();
                click_node(ui, nodes);
                count
            };
            if found == 0 {
                out.push_str(&format!("error: no node containing {rest:?}\n"));
            } else {
                run_input(ui, out, 3);
                out.push_str(&format!("(clicked one of {found})\n"));
            }
        }
        "click_role" => {
            let mut args = rest.split_whitespace();
            let role_name = args.next().unwrap_or("");
            let index: usize = args.next().and_then(|n| n.parse().ok()).unwrap_or(0);
            match role(role_name) {
                Some(target) => {
                    let mut clicked = false;
                    {
                        let nodes: Vec<_> = ui.get_all_by_role(target).collect();
                        if let Some(node) = nodes.get(index) {
                            node.click();
                            clicked = true;
                        }
                    }
                    if clicked {
                        run_input(ui, out, 3);
                    } else {
                        out.push_str(&format!("error: no {role_name} at index {index}\n"));
                    }
                }
                None => out.push_str(&format!("error: unknown role {role_name:?}\n")),
            }
        }
        "type_role" => {
            let mut args = rest.splitn(3, ' ');
            let role_name = args.next().unwrap_or("");
            let index: usize = args.next().and_then(|n| n.parse().ok()).unwrap_or(0);
            let text = args.next().unwrap_or("");
            match role(role_name) {
                Some(target) => {
                    let mut typed = false;
                    {
                        let nodes: Vec<_> = ui.get_all_by_role(target).collect();
                        if let Some(node) = nodes.get(index) {
                            node.focus();
                            node.type_text(text);
                            typed = true;
                        }
                    }
                    if typed {
                        run_input(ui, out, 2);
                    } else {
                        out.push_str(&format!("error: no {role_name} at index {index}\n"));
                    }
                }
                None => out.push_str(&format!("error: unknown role {role_name:?}\n")),
            }
        }
        "key" => match key(rest) {
            Some(k) => {
                ui.key_press(k);
                run_input(ui, out, 3);
            }
            None => out.push_str(&format!("error: unknown key {rest:?}\n")),
        },
        "step" => {
            let steps: usize = rest.parse().unwrap_or(1);
            run_input(ui, out, steps);
        }
        "snapshot" => {
            let name = if rest.is_empty() { "frame" } else { rest };
            match ui.render() {
                Ok(image) => {
                    let path = dir.join(format!("{name}.png"));
                    match image.save(&path) {
                        Ok(()) => out.push_str(&format!("wrote {}\n", path.display())),
                        Err(e) => out.push_str(&format!("error: saving: {e}\n")),
                    }
                }
                Err(e) => out.push_str(&format!("error: rendering: {e}\n")),
            }
        }
        "reset" => {
            *ui = build(fixture(rest));
            out.push_str(&format!("reset to {rest:?}\n"));
        }
        "quit" => {
            out.push_str("bye\n");
            return true;
        }
        other => out.push_str(&format!("error: unknown command {other:?}\n")),
    }
    false
}

fn serve(sock: PathBuf, dir: PathBuf) {
    if let Some(parent) = sock.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::create_dir_all(&dir);
    let listener = UnixListener::bind(&sock).expect("bind the control socket");
    let mut ui = build(fixture("sample"));
    println!("reel ui_debug listening on {}", sock.display());

    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let Ok(reader_stream) = stream.try_clone() else {
            continue;
        };
        let reader = BufReader::new(reader_stream);
        let mut out = String::new();
        let mut quit = false;
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if execute(&mut ui, line, &dir, &mut out) {
                quit = true;
                break;
            }
        }
        let _ = (&stream).write_all(out.as_bytes());
        let _ = stream.flush();
        if quit {
            let _ = std::fs::remove_file(&sock);
            return;
        }
    }
}

fn send(sock: PathBuf, command: String) {
    let mut stream = UnixStream::connect(&sock)
        .unwrap_or_else(|e| panic!("connect to {}: {e}", sock.display()));
    stream.write_all(command.as_bytes()).unwrap();
    stream.write_all(b"\n").unwrap();
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut response = String::new();
    let _ = std::io::Read::read_to_string(&mut stream, &mut response);
    print!("{response}");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("serve") => {
            let sock = args.get(2).cloned().unwrap_or("/tmp/opencode/reel-ui.sock".into());
            let dir = args.get(3).cloned().unwrap_or("/tmp/opencode/reel-ui".into());
            serve(PathBuf::from(sock), PathBuf::from(dir));
        }
        Some("send") => {
            let sock = args.get(2).cloned().unwrap_or("/tmp/opencode/reel-ui.sock".into());
            let command = args.get(3).cloned().unwrap_or_else(|| {
                let mut line = String::new();
                let _ = std::io::stdin().read_line(&mut line);
                line
            });
            send(PathBuf::from(sock), command);
        }
        _ => {
            eprintln!("usage: ui_debug serve <socket> <dir>");
            eprintln!("       ui_debug send <socket> \"<command>\"");
            std::process::exit(2);
        }
    }
}
