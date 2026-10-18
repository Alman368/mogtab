use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use gtk::gdk;
use gtk::glib::{self, ControlFlow, Propagation};
use gtk::pango;
use gtk::prelude::*;
use gtk4 as gtk;
use gtk4_layer_shell::{KeyboardMode, Layer, LayerShell};
use mogtab::ipc::{self, Direction, IpcCommand, OpenRequest, Profile};
use serde::Deserialize;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::Read;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

const APP_ID: &str = "dev.alman.MogTab";
const NAMESPACE: &str = "mogtab";
const IPC_READ_TIMEOUT: Duration = Duration::from_secs(1);
const IPC_MAX_PAYLOAD: u64 = 64;
const DESKTOP_INDEX_CHECK_INTERVAL: Duration = Duration::from_secs(2);
const INITIAL_TAB_SUPPRESSION_GRACE: Duration = Duration::from_millis(180);
const MODIFIER_STATE_SETTLE_GRACE: Duration = Duration::from_millis(45);
const MODIFIER_RELEASE_CONFIRM_SAMPLES: u8 = 2;
const FOCUS_AFTER_HIDE_DELAY: Duration = Duration::from_millis(30);
const PANEL_MIN_WIDTH: i32 = 260;
const PANEL_MAX_WIDTH: i32 = 1180;
const PANEL_ITEM_WIDTH: i32 = 172;
const PANEL_HORIZONTAL_PADDING: i32 = 28;
const PANEL_HEIGHT: i32 = 154;
const CURSOR_NO_WARPS_OPTION: &str = "cursor:no_warps";
const CSS: &str = r#"
window {
  background: transparent;
}

.switcher-panel {
  background: rgba(12, 13, 15, 0.98);
  border: 1px solid rgba(255, 255, 255, 0.11);
  border-radius: 15px;
  padding: 9px 12px 8px;
  box-shadow: 0 18px 44px rgba(0, 0, 0, 0.42);
}

.switcher-row {
  border-spacing: 8px;
}

.switcher-card {
  background: transparent;
  border: 1px solid transparent;
  border-radius: 10px;
  padding: 8px 9px 7px;
  min-width: 146px;
  min-height: 122px;
}

.switcher-card.selected {
  background: rgba(255, 255, 255, 0.15);
  border-color: rgba(255, 255, 255, 0.24);
}

.switcher-icon {
  margin-bottom: 8px;
}

.switcher-app {
  color: rgba(250, 252, 255, 0.98);
  font-size: 12px;
  font-weight: 700;
  margin-bottom: 5px;
}

.switcher-title {
  color: rgba(224, 230, 240, 0.92);
  font-size: 11px;
  font-weight: 500;
}

.switcher-meta {
  color: rgba(190, 200, 214, 0.82);
  font-size: 10px;
  margin-top: 5px;
}
"#;

#[derive(Debug, Parser)]
#[command(name = "mogtab")]
#[command(about = "Visual MRU Alt-Tab switcher for Hyprland")]
struct Cli {
    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    /// Run the resident GTK/layer-shell switcher daemon.
    Run,
    /// Open the switcher or move selection while it is open.
    Open {
        #[arg(long, value_enum, default_value_t = Profile::Alt)]
        profile: Profile,
        #[arg(value_enum, default_value_t = Direction::Next)]
        direction: Direction,
    },
    /// Close the switcher and focus the selected window.
    Close,
    /// Close the switcher without switching.
    Cancel,
    /// Print the current MRU window list.
    List {
        #[arg(long, value_enum, default_value_t = Profile::Alt)]
        profile: Profile,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiUpdate {
    None,
    RenderAndPresent,
    Selection,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum Target {
    #[default]
    Windows,
    Workspaces,
    Monitors,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum Scope {
    #[default]
    All,
    CurrentWorkspace,
    CurrentMonitor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProfileSettings {
    target: Target,
    scope: Scope,
    same_class: bool,
    include_special_workspaces: bool,
    include_empty_workspaces: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(from = "RawAppConfig")]
struct AppConfig {
    alt: ProfileSettings,
    super_profile: ProfileSettings,
}

/// Config as written, with every field optional, so a partial section keeps
/// the defaults of its own profile instead of generic ones.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawAppConfig {
    alt: RawProfileSettings,
    #[serde(rename = "super")]
    super_profile: RawProfileSettings,
}

#[derive(Debug, Default, Deserialize)]
struct RawProfileSettings {
    target: Option<Target>,
    scope: Option<Scope>,
    same_class: Option<bool>,
    include_special_workspaces: Option<bool>,
    include_empty_workspaces: Option<bool>,
}

impl RawProfileSettings {
    fn or_defaults(self, defaults: ProfileSettings) -> ProfileSettings {
        ProfileSettings {
            target: self.target.unwrap_or(defaults.target),
            scope: self.scope.unwrap_or(defaults.scope),
            same_class: self.same_class.unwrap_or(defaults.same_class),
            include_special_workspaces: self
                .include_special_workspaces
                .unwrap_or(defaults.include_special_workspaces),
            include_empty_workspaces: self
                .include_empty_workspaces
                .unwrap_or(defaults.include_empty_workspaces),
        }
    }
}

impl From<RawAppConfig> for AppConfig {
    fn from(raw: RawAppConfig) -> Self {
        let defaults = Self::default();
        Self {
            alt: raw.alt.or_defaults(defaults.alt),
            super_profile: raw.super_profile.or_defaults(defaults.super_profile),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct HyprWorkspace {
    #[serde(default)]
    id: i64,
    #[serde(default)]
    name: String,
}

#[derive(Debug, Clone, Deserialize)]
struct HyprClient {
    #[serde(default)]
    address: String,
    #[serde(default = "default_true")]
    mapped: bool,
    #[serde(default)]
    hidden: bool,
    #[serde(default = "default_true", rename = "acceptsInput")]
    accepts_input: bool,
    #[serde(default)]
    workspace: HyprWorkspace,
    #[serde(default)]
    monitor: i64,
    #[serde(default, rename = "class")]
    wm_class: String,
    #[serde(default, rename = "initialClass")]
    initial_class: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    pid: i64,
    #[serde(default, rename = "focusHistoryID")]
    focus_history_id: Option<i64>,
    #[serde(default)]
    pinned: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct HyprMonitorInfo {
    #[serde(default)]
    id: i64,
    #[serde(default)]
    name: String,
    #[serde(default, rename = "activeWorkspace")]
    active_workspace: HyprWorkspace,
    #[serde(default)]
    focused: bool,
    #[serde(default)]
    disabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct HyprIntOption {
    #[serde(default)]
    int: i64,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
struct HyprCursorPosition {
    #[serde(default)]
    x: i64,
    #[serde(default)]
    y: i64,
}

#[derive(Debug, Clone, Deserialize)]
struct HyprActiveWindow {
    #[serde(default)]
    address: String,
    #[serde(default)]
    monitor: Option<i64>,
    #[serde(default)]
    workspace: HyprWorkspace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WindowInfo {
    address: String,
    title: String,
    wm_class: String,
    exe: String,
    app_name: String,
    desktop_icon: String,
    workspace_id: i64,
    workspace_name: String,
    monitor: i64,
    focus_history_id: i64,
    pinned: bool,
    active: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct HyprActiveWorkspace {
    #[serde(default)]
    id: i64,
    #[serde(default, rename = "monitorID")]
    monitor_id: i64,
}

#[derive(Debug, Clone, Deserialize)]
struct HyprWorkspaceInfo {
    #[serde(default)]
    id: i64,
    #[serde(default)]
    name: String,
    #[serde(default, rename = "monitorID")]
    monitor_id: i64,
    #[serde(default)]
    windows: i64,
    #[serde(default)]
    lastwindowtitle: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkspaceInfo {
    id: i64,
    name: String,
    monitor: i64,
    windows: i64,
    last_title: String,
    best_focus_history_id: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MonitorInfo {
    id: i64,
    name: String,
    active_workspace_name: String,
    focused: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SwitchItem {
    Window(WindowInfo),
    Workspace(WorkspaceInfo),
    Monitor(MonitorInfo),
}

struct SwitcherState {
    window: gtk::ApplicationWindow,
    panel: gtk::Box,
    row: gtk::Box,
    items: Vec<SwitchItem>,
    selected: usize,
    open: bool,
    ignore_initial_tab_until: Option<Instant>,
    modifier_watch_started_at: Option<Instant>,
    modifier_release_misses: u8,
    pointer_selection_armed: bool,
    pointer_open_position: Option<HyprCursorPosition>,
    active_profile: Option<Profile>,
    config: AppConfig,
}

fn default_true() -> bool {
    true
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            alt: ProfileSettings {
                target: Target::Windows,
                scope: Scope::CurrentMonitor,
                same_class: false,
                include_special_workspaces: false,
                include_empty_workspaces: false,
            },
            super_profile: ProfileSettings {
                target: Target::Workspaces,
                scope: Scope::CurrentMonitor,
                same_class: false,
                include_special_workspaces: false,
                include_empty_workspaces: false,
            },
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        CliCommand::Run => run_daemon(),
        CliCommand::Open { profile, direction } => {
            let daemon = env::current_exe().context("failed to resolve current executable")?;
            ipc::send_or_start(
                IpcCommand::Open(OpenRequest { profile, direction }),
                daemon.as_os_str(),
            )
            .map_err(anyhow::Error::msg)
        }
        CliCommand::Close => {
            let _ = ipc::send(IpcCommand::Close);
            Ok(())
        }
        CliCommand::Cancel => {
            let _ = ipc::send(IpcCommand::Cancel);
            Ok(())
        }
        CliCommand::List { profile } => {
            let config = load_config();
            // Initialize GTK when possible so icons are checked against the theme.
            if gtk::init().is_ok() {
                add_icon_search_paths();
            }
            for item in collect_items(config.profile(profile))? {
                let icon = match item.icon() {
                    AppIcon::Named(name) => name,
                    AppIcon::File(path) => path.display().to_string(),
                };
                println!(
                    "{}  [{}]  icon: {}",
                    item.display_name(),
                    item.meta_text(),
                    icon
                );
            }
            Ok(())
        }
    }
}

fn run_daemon() -> Result<()> {
    // NON_UNIQUE: with D-Bus uniqueness a second `mogtab run` would re-run
    // `activate` inside the running daemon, whose socket bind then fails and
    // quits it. The IPC socket already guarantees a single daemon.
    let app = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();

    app.connect_activate(|app| {
        // This is a resident hidden-window app; keep it alive between switcher opens.
        std::mem::forget(app.hold());
        install_css();
        add_icon_search_paths();
        // Warm the desktop entry index so the first switcher open stays fast.
        thread::spawn(desktop_index);

        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("MogTab")
            .decorated(false)
            .resizable(false)
            .default_width(PANEL_MIN_WIDTH)
            .default_height(PANEL_HEIGHT)
            .build();
        window.init_layer_shell();
        window.set_namespace(Some(NAMESPACE));
        window.set_layer(Layer::Overlay);
        window.set_keyboard_mode(KeyboardMode::Exclusive);

        let panel = gtk::Box::new(gtk::Orientation::Vertical, 0);
        panel.add_css_class("switcher-panel");
        panel.set_halign(gtk::Align::Center);
        panel.set_valign(gtk::Align::Center);

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.add_css_class("switcher-row");
        row.set_halign(gtk::Align::Center);
        row.set_valign(gtk::Align::Center);
        panel.append(&row);

        window.set_child(Some(&panel));
        window.set_visible(false);

        let (tx, rx) = mpsc::channel::<IpcCommand>();
        if let Err(err) = spawn_ipc_listener(tx) {
            eprintln!("mogtab: failed to start IPC listener: {err:#}");
            app.quit();
            return;
        }

        let state = Rc::new(RefCell::new(SwitcherState {
            window: window.clone(),
            panel: panel.clone(),
            row: row.clone(),
            items: Vec::new(),
            selected: 0,
            open: false,
            ignore_initial_tab_until: None,
            modifier_watch_started_at: None,
            modifier_release_misses: 0,
            pointer_selection_armed: false,
            pointer_open_position: None,
            active_profile: None,
            config: load_config(),
        }));

        setup_keyboard_controller(&window, Rc::clone(&state));

        glib::timeout_add_local(Duration::from_millis(12), move || {
            while let Ok(command) = rx.try_recv() {
                handle_command(&state, command);
            }
            state.borrow_mut().close_if_active_modifier_is_up();
            ControlFlow::Continue
        });
    });

    app.run_with_args::<&str>(&[]);
    Ok(())
}

fn setup_keyboard_controller(window: &gtk::ApplicationWindow, state: Rc<RefCell<SwitcherState>>) {
    let controller = gtk::EventControllerKey::new();
    let state_pressed = Rc::clone(&state);
    controller.connect_key_pressed(move |_, key, _, _| match key {
        gdk::Key::Tab => {
            if state_pressed
                .borrow_mut()
                .consume_initial_keyboard_tab(Instant::now())
            {
                return Propagation::Stop;
            }

            let profile = state_pressed
                .borrow()
                .active_profile
                .unwrap_or(Profile::Alt);
            handle_command(
                &state_pressed,
                IpcCommand::Open(OpenRequest {
                    profile,
                    direction: Direction::Next,
                }),
            );
            Propagation::Stop
        }
        gdk::Key::Right | gdk::Key::l => {
            let profile = state_pressed
                .borrow()
                .active_profile
                .unwrap_or(Profile::Alt);
            handle_command(
                &state_pressed,
                IpcCommand::Open(OpenRequest {
                    profile,
                    direction: Direction::Next,
                }),
            );
            Propagation::Stop
        }
        gdk::Key::ISO_Left_Tab => {
            if state_pressed
                .borrow_mut()
                .consume_initial_keyboard_tab(Instant::now())
            {
                return Propagation::Stop;
            }

            let profile = state_pressed
                .borrow()
                .active_profile
                .unwrap_or(Profile::Alt);
            handle_command(
                &state_pressed,
                IpcCommand::Open(OpenRequest {
                    profile,
                    direction: Direction::Prev,
                }),
            );
            Propagation::Stop
        }
        gdk::Key::Left | gdk::Key::h | gdk::Key::grave => {
            let profile = state_pressed
                .borrow()
                .active_profile
                .unwrap_or(Profile::Alt);
            handle_command(
                &state_pressed,
                IpcCommand::Open(OpenRequest {
                    profile,
                    direction: Direction::Prev,
                }),
            );
            Propagation::Stop
        }
        gdk::Key::Escape => {
            handle_command(&state_pressed, IpcCommand::Cancel);
            Propagation::Stop
        }
        gdk::Key::Return | gdk::Key::KP_Enter => {
            handle_command(&state_pressed, IpcCommand::Close);
            Propagation::Stop
        }
        _ => Propagation::Proceed,
    });

    let state_released = Rc::clone(&state);
    controller.connect_key_released(move |_, key, _, _| {
        if matches!(key, gdk::Key::Tab | gdk::Key::ISO_Left_Tab) {
            state_released.borrow_mut().clear_initial_keyboard_tab();
        }

        if matches!(
            key,
            gdk::Key::Alt_L | gdk::Key::Alt_R | gdk::Key::Super_L | gdk::Key::Super_R
        ) {
            state_released.borrow_mut().close(true);
        }
    });

    window.add_controller(controller);
}

fn handle_command(state: &Rc<RefCell<SwitcherState>>, command: IpcCommand) {
    let update = state.borrow_mut().handle(command);

    match update {
        UiUpdate::None => {}
        UiUpdate::RenderAndPresent => {
            render_state(state);
            let switcher = state.borrow();
            switcher.window.set_keyboard_mode(KeyboardMode::Exclusive);
            switcher.window.present();
            switcher.window.grab_focus();
        }
        UiUpdate::Selection => refresh_selected_styles(state),
    }
}

fn render_state(state: &Rc<RefCell<SwitcherState>>) {
    let (window, panel, row, items, selected) = {
        let switcher = state.borrow();
        (
            switcher.window.clone(),
            switcher.panel.clone(),
            switcher.row.clone(),
            switcher.items.clone(),
            switcher.selected,
        )
    };

    let width = panel_width(items.len());
    window.set_default_size(width, PANEL_HEIGHT);
    window.set_size_request(width, PANEL_HEIGHT);
    panel.set_size_request(width, PANEL_HEIGHT);
    clear_row(&row);

    for (idx, item) in items.iter().enumerate() {
        row.append(&item_card(item, idx == selected, idx, Rc::clone(state)));
    }
}

fn activate_item(state: &Rc<RefCell<SwitcherState>>, idx: usize) {
    let mut switcher = state.borrow_mut();
    if !switcher.open || idx >= switcher.items.len() {
        return;
    }

    switcher.selected = idx;
    switcher.close(true);
}

fn select_item(state: &Rc<RefCell<SwitcherState>>, idx: usize) {
    let changed = {
        let mut switcher = state.borrow_mut();
        if !switcher.open || idx >= switcher.items.len() || switcher.selected == idx {
            false
        } else {
            switcher.selected = idx;
            true
        }
    };

    if changed {
        refresh_selected_styles(state);
    }
}

fn select_item_after_pointer_enter(state: &Rc<RefCell<SwitcherState>>, idx: usize) {
    let (pointer_selection_armed, pointer_open_position) = {
        let switcher = state.borrow();
        (
            switcher.pointer_selection_armed,
            switcher.pointer_open_position,
        )
    };

    if pointer_selection_armed || pointer_moved_since_open(pointer_open_position) {
        state.borrow_mut().pointer_selection_armed = true;
        select_item(state, idx);
    }
}

fn pointer_moved_since_open(open_position: Option<HyprCursorPosition>) -> bool {
    open_position.is_none_or(|open_position| {
        cursor_position()
            .map(|position| position != open_position)
            .unwrap_or(true)
    })
}

fn refresh_selected_styles(state: &Rc<RefCell<SwitcherState>>) {
    let (row, selected) = {
        let switcher = state.borrow();
        (switcher.row.clone(), switcher.selected)
    };
    refresh_selected_card_styles(&row, selected);
}

fn refresh_selected_card_styles(row: &gtk::Box, selected: usize) {
    let mut idx = 0;
    let mut child = row.first_child();

    while let Some(widget) = child {
        let next = widget.next_sibling();
        if idx == selected {
            widget.add_css_class("selected");
        } else {
            widget.remove_css_class("selected");
        }
        child = next;
        idx += 1;
    }
}

fn clear_row(row: &gtk::Box) {
    while let Some(child) = row.first_child() {
        row.remove(&child);
    }
}

impl SwitcherState {
    fn handle(&mut self, command: IpcCommand) -> UiUpdate {
        match command {
            IpcCommand::Open(request) => self.open_or_advance(request),
            IpcCommand::Close => {
                self.close(true);
                UiUpdate::None
            }
            IpcCommand::Cancel => {
                self.close(false);
                UiUpdate::None
            }
        }
    }

    fn open_or_advance(&mut self, request: OpenRequest) -> UiUpdate {
        if self.open {
            self.advance(request.direction);
            return UiUpdate::Selection;
        }

        self.config = load_config();
        let settings = self.config.profile(request.profile);
        match collect_items(settings) {
            Ok(items)
                if should_open_switcher(settings, items.len(), only_window_is_active(&items)) =>
            {
                let now = Instant::now();
                self.items = items;
                self.selected = initial_selection(self.items.len(), request.direction);
                self.open = true;
                self.ignore_initial_tab_until = Some(now + INITIAL_TAB_SUPPRESSION_GRACE);
                self.modifier_watch_started_at = Some(now);
                self.modifier_release_misses = 0;
                self.pointer_selection_armed = false;
                self.pointer_open_position = cursor_position().ok();
                self.active_profile = Some(request.profile);
                UiUpdate::RenderAndPresent
            }
            Ok(_) => {
                self.ignore_initial_tab_until = None;
                UiUpdate::None
            }
            Err(err) => {
                eprintln!("mogtab: failed to collect items: {err:#}");
                UiUpdate::None
            }
        }
    }

    fn consume_initial_keyboard_tab(&mut self, now: Instant) -> bool {
        let Some(deadline) = self.ignore_initial_tab_until else {
            return false;
        };

        self.ignore_initial_tab_until = None;
        now <= deadline
    }

    fn clear_initial_keyboard_tab(&mut self) {
        self.ignore_initial_tab_until = None;
    }

    fn advance(&mut self, direction: Direction) {
        if self.items.is_empty() {
            self.selected = 0;
            return;
        }

        self.selected = match direction {
            Direction::Next => (self.selected + 1) % self.items.len(),
            Direction::Prev => {
                if self.selected == 0 {
                    self.items.len() - 1
                } else {
                    self.selected - 1
                }
            }
        };
    }

    fn close(&mut self, should_focus: bool) {
        if !self.open {
            return;
        }

        self.ignore_initial_tab_until = None;
        self.modifier_watch_started_at = None;
        self.modifier_release_misses = 0;
        self.pointer_selection_armed = false;
        self.pointer_open_position = None;
        self.active_profile = None;
        self.open = false;
        self.window.set_keyboard_mode(KeyboardMode::None);
        self.window.set_visible(false);

        let selected = self.items.get(self.selected).cloned();
        clear_row(&self.row);
        self.items.clear();
        self.selected = 0;

        if should_focus && let Some(item) = selected {
            schedule_focus_item(item);
        }
    }

    fn close_if_active_modifier_is_up(&mut self) {
        if !self.open {
            return;
        }

        let Some(started_at) = self.modifier_watch_started_at else {
            return;
        };

        if Instant::now().duration_since(started_at) < MODIFIER_STATE_SETTLE_GRACE {
            return;
        }

        let Some(profile) = self.active_profile else {
            return;
        };

        let Some(modifier_is_down) = active_modifier_is_down(profile) else {
            return;
        };

        if should_close_from_modifier_poll(&mut self.modifier_release_misses, modifier_is_down) {
            self.close(true);
        }
    }
}

fn initial_selection(len: usize, direction: Direction) -> usize {
    if len <= 1 {
        0
    } else if direction == Direction::Prev {
        len - 1
    } else {
        1
    }
}

fn should_open_switcher(
    settings: ProfileSettings,
    item_count: usize,
    only_window_is_active: bool,
) -> bool {
    item_count > 0
        && (settings.target != Target::Windows || item_count > 1 || !only_window_is_active)
}

fn only_window_is_active(items: &[SwitchItem]) -> bool {
    matches!(items, [SwitchItem::Window(window)] if window.active)
}

fn should_close_from_modifier_poll(missing_samples: &mut u8, modifier_is_down: bool) -> bool {
    if modifier_is_down {
        *missing_samples = 0;
        return false;
    }

    *missing_samples = missing_samples.saturating_add(1);
    *missing_samples >= MODIFIER_RELEASE_CONFIRM_SAMPLES
}

fn active_modifier_is_down(profile: Profile) -> Option<bool> {
    let state = gdk::Display::default()?
        .default_seat()?
        .keyboard()?
        .modifier_state();
    Some(modifier_is_down(profile, state))
}

fn modifier_is_down(profile: Profile, state: gdk::ModifierType) -> bool {
    state.contains(modifier_mask(profile))
}

fn modifier_mask(profile: Profile) -> gdk::ModifierType {
    match profile {
        Profile::Alt => gdk::ModifierType::ALT_MASK,
        Profile::Super => gdk::ModifierType::SUPER_MASK,
    }
}

fn panel_width(item_count: usize) -> i32 {
    let width = PANEL_HORIZONTAL_PADDING + PANEL_ITEM_WIDTH * item_count as i32;
    width.clamp(PANEL_MIN_WIDTH, PANEL_MAX_WIDTH)
}

fn item_card(
    item: &SwitchItem,
    selected: bool,
    idx: usize,
    state: Rc<RefCell<SwitcherState>>,
) -> gtk::Box {
    let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
    card.add_css_class("switcher-card");
    if selected {
        card.add_css_class("selected");
    }
    card.set_cursor_from_name(Some("pointer"));

    let motion = gtk::EventControllerMotion::new();
    let state_enter = Rc::clone(&state);
    motion.connect_enter(move |_, _, _| {
        select_item_after_pointer_enter(&state_enter, idx);
    });
    let state_motion = Rc::clone(&state);
    motion.connect_motion(move |_, _, _| {
        state_motion.borrow_mut().pointer_selection_armed = true;
        select_item(&state_motion, idx);
    });
    card.add_controller(motion);

    let click = gtk::GestureClick::new();
    click.set_button(1);
    let state_click = Rc::clone(&state);
    click.connect_pressed(move |_, _, _, _| {
        activate_item(&state_click, idx);
    });
    card.add_controller(click);

    let icon = match item.icon() {
        AppIcon::Named(name) => gtk::Image::from_icon_name(&name),
        AppIcon::File(path) => gtk::Image::from_file(path),
    };
    icon.add_css_class("switcher-icon");
    icon.set_pixel_size(52);
    icon.set_halign(gtk::Align::Center);
    card.append(&icon);

    let app = gtk::Label::new(Some(&item.app_label()));
    app.add_css_class("switcher-app");
    app.set_ellipsize(pango::EllipsizeMode::End);
    app.set_max_width_chars(20);
    app.set_halign(gtk::Align::Center);
    card.append(&app);

    let title = gtk::Label::new(Some(&item.short_title()));
    title.add_css_class("switcher-title");
    title.set_ellipsize(pango::EllipsizeMode::End);
    title.set_max_width_chars(20);
    title.set_lines(1);
    title.set_halign(gtk::Align::Center);
    title.set_justify(gtk::Justification::Center);
    card.append(&title);

    let meta_text = item.meta_text();
    let meta = gtk::Label::new(Some(&meta_text));
    meta.add_css_class("switcher-meta");
    meta.set_ellipsize(pango::EllipsizeMode::End);
    meta.set_max_width_chars(16);
    meta.set_halign(gtk::Align::Center);
    card.append(&meta);

    card
}

impl AppConfig {
    fn profile(&self, profile: Profile) -> ProfileSettings {
        match profile {
            Profile::Alt => self.alt,
            Profile::Super => self.super_profile,
        }
    }
}

impl SwitchItem {
    fn display_name(&self) -> String {
        match self {
            Self::Window(window) => window.display_name(),
            Self::Workspace(workspace) => workspace.display_name(),
            Self::Monitor(monitor) => monitor.display_name(),
        }
    }

    fn short_title(&self) -> String {
        match self {
            Self::Window(window) => window.short_title(),
            Self::Workspace(workspace) => workspace.short_title(),
            Self::Monitor(monitor) => monitor.short_title(),
        }
    }

    fn app_label(&self) -> String {
        match self {
            Self::Window(window) => window.app_name.clone(),
            Self::Workspace(workspace) => format!("Workspace {}", workspace.name),
            Self::Monitor(_) => "Monitor".to_string(),
        }
    }

    fn meta_text(&self) -> String {
        match self {
            Self::Window(window) => {
                if window.pinned {
                    format!("ws {} - pinned", window.workspace_name)
                } else {
                    format!("ws {}", window.workspace_name)
                }
            }
            Self::Workspace(workspace) => {
                let count = if workspace.windows == 1 {
                    "window"
                } else {
                    "windows"
                };
                format!("ws {} - {} {}", workspace.name, workspace.windows, count)
            }
            Self::Monitor(monitor) => format!("ws {}", monitor.active_workspace_name),
        }
    }

    fn icon(&self) -> AppIcon {
        match self {
            Self::Window(window) => window_icon(window),
            Self::Workspace(_) => AppIcon::Named("view-grid-symbolic".to_string()),
            Self::Monitor(_) => AppIcon::Named("video-display-symbolic".to_string()),
        }
    }
}

impl WindowInfo {
    fn display_name(&self) -> String {
        if !self.title.trim().is_empty() && !self.wm_class.trim().is_empty() {
            format!(
                "{}: {}",
                self.app_name.clone(),
                clean_display_title(&self.title)
            )
        } else if !self.title.trim().is_empty() {
            clean_display_title(&self.title)
        } else if !self.wm_class.trim().is_empty() {
            self.app_name.clone()
        } else {
            self.address.clone()
        }
    }

    fn short_title(&self) -> String {
        if !self.title.trim().is_empty() {
            clean_display_title(&self.title)
        } else if !self.wm_class.trim().is_empty() {
            self.app_name.clone()
        } else {
            self.address.clone()
        }
    }
}

impl WorkspaceInfo {
    fn display_name(&self) -> String {
        format!(
            "Workspace {}: {}",
            self.name,
            clean_display_title(&self.last_title)
        )
    }

    fn short_title(&self) -> String {
        if self.last_title.trim().is_empty() {
            format!("Workspace {}", self.name)
        } else {
            clean_display_title(&self.last_title)
        }
    }
}

impl MonitorInfo {
    fn display_name(&self) -> String {
        format!(
            "Monitor {}: workspace {}",
            self.name, self.active_workspace_name
        )
    }

    fn short_title(&self) -> String {
        self.name.clone()
    }
}

enum AppIcon {
    Named(String),
    File(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DesktopEntry {
    id: String,
    name: String,
    icon: String,
    wm_class: String,
    exec: String,
    no_display: bool,
}

impl DesktopEntry {
    /// Ranks entries competing for the same fuzzy key, so the result does not
    /// depend on directory iteration order: an entry whose id is the key wins,
    /// then entries shown in menus, then the shortest id.
    fn rank(
        &self,
        key: &str,
    ) -> (
        bool,
        bool,
        std::cmp::Reverse<usize>,
        std::cmp::Reverse<&str>,
    ) {
        (
            self.id == key,
            !self.no_display,
            std::cmp::Reverse(self.id.len()),
            std::cmp::Reverse(self.id.as_str()),
        )
    }
}

#[derive(Default)]
struct DesktopIndex {
    seen_ids: HashSet<String>,
    by_wm_class: HashMap<String, DesktopEntry>,
    by_id: HashMap<String, DesktopEntry>,
    by_id_tail: HashMap<String, DesktopEntry>,
    by_exec: HashMap<String, DesktopEntry>,
    by_name: HashMap<String, DesktopEntry>,
}

impl DesktopIndex {
    fn load(dirs: &[PathBuf]) -> Self {
        let locale = locale_names();
        let mut index = Self::default();
        for dir in dirs {
            index.scan_dir(dir, "", &locale);
        }
        index
    }

    fn scan_dir(&mut self, dir: &Path, id_prefix: &str, locale: &[String]) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let file_name = entry.file_name().to_string_lossy().into_owned();
            if path.is_dir() {
                self.scan_dir(&path, &format!("{id_prefix}{file_name}-"), locale);
                continue;
            }

            let Some(stem) = file_name.strip_suffix(".desktop") else {
                continue;
            };
            // The first data dir that has a desktop id owns it, even when that
            // file hides the entry, so user overrides mask system files.
            let id = format!("{id_prefix}{stem}").to_ascii_lowercase();
            if !self.seen_ids.insert(id.clone()) {
                continue;
            }
            if let Some(desktop_entry) = fs::read_to_string(&path)
                .ok()
                .and_then(|contents| parse_desktop_entry(&id, &contents, locale))
            {
                self.insert(desktop_entry);
            }
        }
    }

    fn insert(&mut self, entry: DesktopEntry) {
        let keyed = [
            (&mut self.by_wm_class, entry.wm_class.to_ascii_lowercase()),
            (&mut self.by_id_tail, class_tail(&entry.id)),
            (&mut self.by_exec, entry.exec.to_ascii_lowercase()),
            (&mut self.by_name, entry.name.to_ascii_lowercase()),
        ];
        for (map, key) in keyed {
            if key.is_empty() {
                continue;
            }
            match map.get(&key) {
                Some(current) if current.rank(&key) >= entry.rank(&key) => {}
                _ => {
                    map.insert(key, entry.clone());
                }
            }
        }
        self.by_id.insert(entry.id.clone(), entry);
    }

    fn lookup(&self, class_name: &str) -> Option<&DesktopEntry> {
        let normalized = class_name.trim().to_ascii_lowercase();
        if normalized.is_empty() {
            return None;
        }
        let tail = class_tail(&normalized);

        self.by_wm_class
            .get(&normalized)
            .or_else(|| self.by_id.get(&normalized))
            .or_else(|| self.by_id_tail.get(&normalized))
            .or_else(|| self.by_exec.get(&normalized))
            .or_else(|| self.by_name.get(&normalized))
            .or_else(|| self.by_wm_class.get(&tail))
            .or_else(|| self.by_id.get(&tail))
            .or_else(|| self.by_exec.get(&tail))
            .or_else(|| self.by_name.get(&tail))
    }
}

/// Last segment of a reverse-DNS name: `dev.zed.Zed` -> `zed`.
fn class_tail(name: &str) -> String {
    name.rsplit('.').next().unwrap_or(name).to_ascii_lowercase()
}

/// Modification times of the applications dirs, used to detect changes.
type DirsSignature = Vec<Option<SystemTime>>;

struct IndexCache {
    signature: DirsSignature,
    index: Arc<DesktopIndex>,
    checked_at: Instant,
    reloading: bool,
}

static DESKTOP_INDEX: Mutex<Option<IndexCache>> = Mutex::new(None);

fn dirs_signature(dirs: &[PathBuf]) -> DirsSignature {
    dirs.iter()
        .map(|dir| fs::metadata(dir).and_then(|meta| meta.modified()).ok())
        .collect()
}

/// Shared desktop index. Only the first call loads it synchronously; after that
/// the applications dirs are checked at most once per interval and, when they
/// changed (an app was installed or removed), the index is rebuilt on a
/// background thread while callers keep using the previous one.
fn desktop_index() -> Arc<DesktopIndex> {
    let mut cache = DESKTOP_INDEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let Some(current) = cache.as_mut() else {
        let dirs = application_dirs();
        let signature = dirs_signature(&dirs);
        let index = Arc::new(DesktopIndex::load(&dirs));
        *cache = Some(IndexCache {
            signature,
            index: Arc::clone(&index),
            checked_at: Instant::now(),
            reloading: false,
        });
        return index;
    };

    if !current.reloading && current.checked_at.elapsed() >= DESKTOP_INDEX_CHECK_INTERVAL {
        current.checked_at = Instant::now();
        let dirs = application_dirs();
        let signature = dirs_signature(&dirs);
        if signature != current.signature {
            current.reloading = true;
            thread::spawn(move || {
                let index = Arc::new(DesktopIndex::load(&dirs));
                let mut cache = DESKTOP_INDEX
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(current) = cache.as_mut() {
                    current.signature = signature;
                    current.index = index;
                    current.reloading = false;
                }
            });
        }
    }

    Arc::clone(&current.index)
}

fn application_dirs() -> Vec<PathBuf> {
    data_dirs()
        .into_iter()
        .map(|dir| dir.join("applications"))
        .collect()
}

fn data_dirs() -> Vec<PathBuf> {
    let home = env::var_os("HOME").map(PathBuf::from);
    let data_home = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|home| home.join(".local/share")));
    let data_dirs = env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|dirs| !dirs.trim().is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());

    // Flatpak and Snap export desktop files here; sessions started without
    // their profile scripts miss them in XDG_DATA_DIRS.
    let package_dirs = [
        home.map(|home| home.join(".local/share/flatpak/exports/share")),
        Some(PathBuf::from("/var/lib/flatpak/exports/share")),
        Some(PathBuf::from("/var/lib/snapd/desktop")),
    ];

    let mut dirs: Vec<PathBuf> = Vec::new();
    for dir in data_home
        .into_iter()
        .chain(
            data_dirs
                .split(':')
                .filter(|dir| !dir.is_empty())
                .map(PathBuf::from),
        )
        .chain(package_dirs.into_iter().flatten())
    {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

/// Locale names to try for `Name[...]`, most specific first: `es_ES`, `es`.
fn locale_names() -> Vec<String> {
    let locale = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|key| env::var(key).ok())
        .find(|value| !value.is_empty())
        .unwrap_or_default();
    let base = locale
        .split(['.', '@'])
        .next()
        .unwrap_or_default()
        .to_string();
    if base.is_empty() || base == "C" || base == "POSIX" {
        return Vec::new();
    }

    let mut names = vec![base.clone()];
    if let Some((language, _)) = base.split_once('_') {
        names.push(language.to_string());
    }
    names
}

fn parse_desktop_entry(id: &str, contents: &str, locale: &[String]) -> Option<DesktopEntry> {
    let mut in_main_section = false;
    let mut name = String::new();
    let mut localized_name: Option<(usize, String)> = None;
    let mut icon = String::new();
    let mut wm_class = String::new();
    let mut exec = String::new();
    let mut no_display = false;

    for line in contents.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_main_section = line == "[Desktop Entry]";
            continue;
        }
        if !in_main_section {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "Name" => name = value.to_string(),
            "Icon" => icon = value.to_string(),
            "StartupWMClass" => wm_class = value.to_string(),
            "Exec" => exec = exec_program(value),
            "NoDisplay" => no_display = value == "true",
            "Hidden" if value == "true" => return None,
            "Type" if value != "Application" => return None,
            _ => {
                let Some(lang) = key
                    .strip_prefix("Name[")
                    .and_then(|rest| rest.strip_suffix(']'))
                else {
                    continue;
                };
                if let Some(priority) = locale.iter().position(|candidate| candidate == lang)
                    && localized_name
                        .as_ref()
                        .is_none_or(|(best, _)| priority < *best)
                {
                    localized_name = Some((priority, value.to_string()));
                }
            }
        }
    }

    if let Some((_, localized)) = localized_name {
        name = localized;
    }

    (!name.is_empty() || !icon.is_empty()).then(|| DesktopEntry {
        id: id.to_ascii_lowercase(),
        name,
        icon,
        wm_class,
        exec,
        no_display,
    })
}

/// Program basename from an `Exec=` line, skipping `env` and `VAR=value` prefixes.
fn exec_program(exec: &str) -> String {
    exec_arguments(exec)
        .into_iter()
        .find(|token| token != "env" && !token.contains('='))
        .and_then(|program| {
            Path::new(&program)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_default()
}

/// Splits an `Exec=` value into arguments, honoring double quotes and the
/// backslash escapes allowed inside them by the desktop entry spec.
fn exec_arguments(exec: &str) -> Vec<String> {
    let mut arguments = Vec::new();
    let mut current = String::new();
    let mut in_argument = false;
    let mut in_quotes = false;
    let mut chars = exec.chars();

    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                in_argument = true;
            }
            '\\' if in_quotes => {
                if let Some(escaped) = chars.next() {
                    current.push(escaped);
                }
            }
            ch if ch.is_whitespace() && !in_quotes => {
                if in_argument {
                    arguments.push(std::mem::take(&mut current));
                    in_argument = false;
                }
            }
            ch => {
                current.push(ch);
                in_argument = true;
            }
        }
    }
    if in_argument {
        arguments.push(current);
    }
    arguments
}

/// Desktop entry for a window: by class first, then by the process executable,
/// which covers windows with a custom class (e.g. `alacritty --class foo`).
fn window_desktop_entry<'a>(
    index: &'a DesktopIndex,
    class_name: &str,
    exe: &str,
) -> Option<&'a DesktopEntry> {
    index
        .lookup(class_name)
        .or_else(|| is_specific_exe(exe).then(|| index.lookup(exe)).flatten())
}

/// Runtimes and wrappers say nothing about which app owns the window.
fn is_specific_exe(exe: &str) -> bool {
    const GENERIC: [&str; 8] = [
        "electron", "python", "java", "node", "bwrap", "flatpak", "wine", "bash",
    ];
    !exe.is_empty() && !GENERIC.iter().any(|prefix| exe.starts_with(prefix))
}

fn process_exe_name(pid: i64) -> String {
    if pid <= 0 {
        return String::new();
    }
    fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_default()
}

/// Icon names to try for a window, best guess first.
fn icon_candidates(desktop_icon: &str, class_name: &str, exe: &str) -> Vec<String> {
    let trimmed = class_name.trim();
    let mut candidates = vec![desktop_icon.to_string()];
    // Steam games: class `steam_app_<id>`, icon `steam_icon_<id>`.
    if let Some(app_id) = trimmed.strip_prefix("steam_app_") {
        candidates.push(format!("steam_icon_{app_id}"));
    }
    candidates.push(trimmed.to_string());
    candidates.push(trimmed.to_ascii_lowercase());
    candidates.push(class_tail(trimmed));
    if is_specific_exe(exe) {
        candidates.push(exe.to_string());
    }
    candidates.retain(|candidate| !candidate.is_empty());
    candidates
}

fn window_icon(window: &WindowInfo) -> AppIcon {
    let theme = gdk::Display::default().map(|display| gtk::IconTheme::for_display(&display));
    icon_candidates(&window.desktop_icon, &window.wm_class, &window.exe)
        .iter()
        .find_map(|candidate| resolve_icon(candidate, theme.as_ref()))
        .unwrap_or_else(|| AppIcon::Named("application-x-executable".to_string()))
}

/// Makes icons of every data dir visible to GTK, including Flatpak and Snap
/// exports that the session's XDG_DATA_DIRS may not list.
fn add_icon_search_paths() {
    let Some(display) = gdk::Display::default() else {
        return;
    };
    let theme = gtk::IconTheme::for_display(&display);
    let known = theme.search_path();
    for dir in data_dirs() {
        let icons = dir.join("icons");
        if icons.is_dir() && !known.contains(&icons) {
            theme.add_search_path(&icons);
        }
    }
}

fn resolve_icon(icon: &str, theme: Option<&gtk::IconTheme>) -> Option<AppIcon> {
    let path = Path::new(icon);
    if path.is_absolute() {
        return path.is_file().then(|| AppIcon::File(path.to_path_buf()));
    }

    // Some desktop files wrongly include an extension in a themed icon name.
    let name = ["png", "svg", "xpm"]
        .iter()
        .find_map(|ext| icon.strip_suffix(&format!(".{ext}")))
        .unwrap_or(icon);

    let Some(theme) = theme else {
        return Some(AppIcon::Named(name.to_string()));
    };
    if theme.has_icon(name) {
        return Some(AppIcon::Named(name.to_string()));
    }

    data_dirs()
        .into_iter()
        .map(|dir| dir.join("pixmaps"))
        .flat_map(|dir| ["png", "svg", "xpm"].map(|ext| dir.join(format!("{name}.{ext}"))))
        .find(|candidate| candidate.is_file())
        .map(AppIcon::File)
}

fn app_label(class_name: &str, entry: Option<&DesktopEntry>) -> String {
    entry
        .map(|entry| entry.name.clone())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| {
            let class_name = class_name.trim();
            if class_name.is_empty() {
                "Application".to_string()
            } else {
                class_name.to_string()
            }
        })
}

fn clean_display_title(title: &str) -> String {
    title
        .replace(" --- ", " | ")
        .replace(" -- ", " | ")
        .replace(" — ", " | ")
        .replace(" – ", " | ")
}

fn load_config() -> AppConfig {
    let path = config_path();
    let Ok(contents) = fs::read_to_string(&path) else {
        return AppConfig::default();
    };

    match toml::from_str::<AppConfig>(&contents) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("mogtab: failed to parse {}: {err}", path.display());
            AppConfig::default()
        }
    }
}

fn config_path() -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(env::temp_dir)
        .join("mogtab")
        .join("config.toml")
}

fn collect_items(settings: ProfileSettings) -> Result<Vec<SwitchItem>> {
    match settings.target {
        Target::Windows => collect_windows(settings)
            .map(|windows| windows.into_iter().map(SwitchItem::Window).collect()),
        Target::Workspaces => collect_workspaces(settings)
            .map(|workspaces| workspaces.into_iter().map(SwitchItem::Workspace).collect()),
        Target::Monitors => collect_monitors()
            .map(|monitors| monitors.into_iter().map(SwitchItem::Monitor).collect()),
    }
}

fn collect_windows(settings: ProfileSettings) -> Result<Vec<WindowInfo>> {
    let clients: Vec<HyprClient> = hyprctl_json(&["clients", "-j"])?;
    let active: Option<HyprActiveWindow> = hyprctl_json(&["activewindow", "-j"]).ok();
    let active_workspace: Option<HyprActiveWorkspace> =
        hyprctl_json(&["activeworkspace", "-j"]).ok();
    let monitors: Option<Vec<HyprMonitorInfo>> = hyprctl_json(&["monitors", "-j"]).ok();
    let active_address = active.as_ref().map(|a| a.address.clone());
    let active_workspace_id = active_workspace.as_ref().map(|workspace| workspace.id);
    let active_monitor_id = current_monitor_id(
        monitors.as_deref(),
        active_workspace.as_ref(),
        active.as_ref(),
    );
    let active_class = active_address.as_ref().and_then(|active_address| {
        clients
            .iter()
            .find(|client| &client.address == active_address)
            .map(client_class)
    });

    let desktop = desktop_index();

    let mut windows = clients
        .into_iter()
        .filter(|client| {
            !client.address.is_empty()
                && client.mapped
                && !client.hidden
                && client.accepts_input
                && (settings.include_special_workspaces || client.workspace.id >= 0)
                && (!settings.same_class
                    || active_class
                        .as_ref()
                        .is_some_and(|active_class| client_class(client) == *active_class))
                && settings.scope.includes(
                    client.workspace.id,
                    client.monitor,
                    active_workspace_id,
                    active_monitor_id,
                )
        })
        .map(|client| {
            let active = client_is_active(
                active_address.as_deref(),
                active_workspace_id,
                active.as_ref(),
                &client,
            );

            let wm_class = resolved_class(client.wm_class, client.initial_class);
            let exe = process_exe_name(client.pid);
            let entry = window_desktop_entry(&desktop, &wm_class, &exe);

            WindowInfo {
                address: client.address,
                title: strip_markup(&client.title),
                app_name: app_label(&wm_class, entry),
                desktop_icon: entry.map(|entry| entry.icon.clone()).unwrap_or_default(),
                wm_class,
                exe,
                workspace_id: client.workspace.id,
                workspace_name: if client.workspace.name.is_empty() {
                    client.workspace.id.to_string()
                } else {
                    client.workspace.name
                },
                monitor: client.monitor,
                focus_history_id: client.focus_history_id.unwrap_or(999_999),
                pinned: client.pinned,
                active,
            }
        })
        .collect::<Vec<_>>();

    windows.sort_by(|a, b| {
        a.focus_history_id
            .cmp(&b.focus_history_id)
            .then_with(|| a.workspace_id.cmp(&b.workspace_id))
            .then_with(|| a.display_name().cmp(&b.display_name()))
    });

    if let Some(active_index) = windows.iter().position(|window| window.active) {
        let active = windows.remove(active_index);
        windows.insert(0, active);
    }

    Ok(windows)
}

fn collect_workspaces(settings: ProfileSettings) -> Result<Vec<WorkspaceInfo>> {
    let clients = collect_windows(ProfileSettings {
        target: Target::Windows,
        scope: Scope::All,
        same_class: false,
        include_special_workspaces: settings.include_special_workspaces,
        include_empty_workspaces: false,
    })?;
    let workspaces: Vec<HyprWorkspaceInfo> = hyprctl_json(&["workspaces", "-j"])?;
    let active_workspace: HyprActiveWorkspace = hyprctl_json(&["activeworkspace", "-j"])?;
    let active_workspace_id = Some(active_workspace.id);
    let active_monitor_id = Some(active_workspace.monitor_id);

    let mut best_focus_by_workspace = HashMap::<i64, i64>::new();
    for client in clients {
        best_focus_by_workspace
            .entry(client.workspace_id)
            .and_modify(|best| *best = (*best).min(client.focus_history_id))
            .or_insert(client.focus_history_id);
    }

    let mut items = workspaces
        .into_iter()
        .filter(|workspace| {
            (settings.include_special_workspaces || workspace.id >= 0)
                && (settings.include_empty_workspaces || workspace.windows > 0)
                && settings.scope.includes(
                    workspace.id,
                    workspace.monitor_id,
                    active_workspace_id,
                    active_monitor_id,
                )
        })
        .map(|workspace| WorkspaceInfo {
            id: workspace.id,
            name: if workspace.name.is_empty() {
                workspace.id.to_string()
            } else {
                workspace.name
            },
            monitor: workspace.monitor_id,
            windows: workspace.windows,
            last_title: strip_markup(&workspace.lastwindowtitle),
            best_focus_history_id: best_focus_by_workspace
                .get(&workspace.id)
                .copied()
                .unwrap_or(999_999),
        })
        .collect::<Vec<_>>();

    items.sort_by(|a, b| {
        a.best_focus_history_id
            .cmp(&b.best_focus_history_id)
            .then_with(|| a.id.cmp(&b.id))
    });

    if let Some(active_index) = items
        .iter()
        .position(|workspace| workspace.id == active_workspace.id)
    {
        let active = items.remove(active_index);
        items.insert(0, active);
    }

    Ok(items)
}

fn collect_monitors() -> Result<Vec<MonitorInfo>> {
    let mut monitors = hyprctl_json::<Vec<HyprMonitorInfo>>(&["monitors", "-j"])?
        .into_iter()
        .filter(|monitor| !monitor.disabled && !monitor.name.is_empty())
        .map(|monitor| MonitorInfo {
            id: monitor.id,
            name: monitor.name,
            active_workspace_name: if monitor.active_workspace.name.is_empty() {
                monitor.active_workspace.id.to_string()
            } else {
                monitor.active_workspace.name
            },
            focused: monitor.focused,
        })
        .collect::<Vec<_>>();

    monitors.sort_by(|a, b| {
        b.focused
            .cmp(&a.focused)
            .then_with(|| a.id.cmp(&b.id))
            .then_with(|| a.name.cmp(&b.name))
    });

    Ok(monitors)
}

fn resolved_class(wm_class: String, initial_class: String) -> String {
    if wm_class.trim().is_empty() {
        initial_class
    } else {
        wm_class
    }
}

fn client_class(client: &HyprClient) -> String {
    if client.wm_class.trim().is_empty() {
        client.initial_class.clone()
    } else {
        client.wm_class.clone()
    }
}

fn client_is_active(
    active_address: Option<&str>,
    active_workspace_id: Option<i64>,
    active: Option<&HyprActiveWindow>,
    client: &HyprClient,
) -> bool {
    if active_address != Some(client.address.as_str()) {
        return false;
    }

    let active_workspace_id = active_workspace_id.or_else(|| {
        active
            .map(|window| window.workspace.id)
            .filter(|workspace_id| *workspace_id >= 0)
    });

    client.pinned
        || active_workspace_id.is_none_or(|workspace_id| workspace_id == client.workspace.id)
}

fn current_monitor_id(
    monitors: Option<&[HyprMonitorInfo]>,
    active_workspace: Option<&HyprActiveWorkspace>,
    active: Option<&HyprActiveWindow>,
) -> Option<i64> {
    monitors
        .and_then(focused_monitor_id)
        .or_else(|| {
            active_workspace
                .map(|workspace| workspace.monitor_id)
                .filter(|monitor| *monitor >= 0)
        })
        .or_else(|| active.and_then(|window| window.monitor.filter(|monitor| *monitor >= 0)))
}

fn focused_monitor_id(monitors: &[HyprMonitorInfo]) -> Option<i64> {
    monitors
        .iter()
        .find(|monitor| monitor.focused && !monitor.disabled && monitor.id >= 0)
        .map(|monitor| monitor.id)
}

impl Scope {
    fn includes(
        self,
        workspace_id: i64,
        monitor_id: i64,
        active_workspace_id: Option<i64>,
        active_monitor_id: Option<i64>,
    ) -> bool {
        match self {
            Self::All => true,
            Self::CurrentWorkspace => active_workspace_id == Some(workspace_id),
            Self::CurrentMonitor => active_monitor_id == Some(monitor_id),
        }
    }
}

fn focus_item(item: &SwitchItem) -> Result<()> {
    let cursor_no_warps_enabled = cursor_no_warps_enabled().unwrap_or(false);
    hyprctl_batch(&focus_item_commands(item, cursor_no_warps_enabled))
}

fn focus_item_commands(item: &SwitchItem, cursor_no_warps_enabled: bool) -> Vec<String> {
    let mut commands = Vec::new();
    let should_restore_cursor_warps = !cursor_no_warps_enabled;

    if should_restore_cursor_warps {
        commands.push(format!("keyword {CURSOR_NO_WARPS_OPTION} 1"));
    }

    match item {
        SwitchItem::Window(window) => commands.extend(focus_window_commands(window)),
        SwitchItem::Workspace(workspace) => commands.extend(focus_workspace_commands(workspace.id)),
        SwitchItem::Monitor(monitor) => commands.extend(focus_monitor_commands(&monitor.name)),
    }

    if should_restore_cursor_warps {
        commands.push(format!("keyword {CURSOR_NO_WARPS_OPTION} 0"));
    }

    commands
}

fn schedule_focus_item(item: SwitchItem) {
    glib::timeout_add_local_once(FOCUS_AFTER_HIDE_DELAY, move || {
        if let Err(err) = focus_item(&item) {
            eprintln!("mogtab: failed to focus item: {err:#}");
        }
    });
}

fn focus_window_commands(window: &WindowInfo) -> Vec<String> {
    let selector = format!("address:{}", window.address);
    let mut commands = Vec::with_capacity(3);

    if window.workspace_id > 0 {
        commands.push(format!("dispatch workspace {}", window.workspace_id));
    }

    commands.push(format!("dispatch focuswindow {selector}"));
    commands.push(format!("dispatch alterzorder top,{selector}"));
    commands
}

fn focus_workspace_commands(workspace_id: i64) -> Vec<String> {
    vec![format!("dispatch workspace {workspace_id}")]
}

fn focus_monitor_commands(monitor_name: &str) -> Vec<String> {
    vec![format!("dispatch focusmonitor {monitor_name}")]
}

fn cursor_no_warps_enabled() -> Result<bool> {
    let option: HyprIntOption = hyprctl_json(&["getoption", CURSOR_NO_WARPS_OPTION, "-j"])?;
    Ok(option.int != 0)
}

fn cursor_position() -> Result<HyprCursorPosition> {
    hyprctl_json(&["cursorpos", "-j"])
}

fn hyprctl_batch(commands: &[String]) -> Result<()> {
    let batch = commands.join(";");
    let output = Command::new("hyprctl")
        .args(["--batch", &batch])
        .output()
        .with_context(|| format!("failed to run hyprctl --batch {batch}"))?;

    if !output.status.success() {
        bail!(
            "hyprctl --batch {} failed: {}",
            batch,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(())
}

fn hyprctl_json<T>(args: &[&str]) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    let output = Command::new("hyprctl")
        .args(args)
        .output()
        .with_context(|| format!("failed to run hyprctl {}", args.join(" ")))?;

    if !output.status.success() {
        bail!(
            "hyprctl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    serde_json::from_slice(&output.stdout)
        .with_context(|| format!("hyprctl {} returned invalid JSON", args.join(" ")))
}

fn spawn_ipc_listener(tx: mpsc::Sender<IpcCommand>) -> Result<()> {
    let path = ipc::socket_path().context("failed to resolve IPC socket path")?;
    if path.exists() {
        if UnixStream::connect(&path).is_ok() {
            bail!("another daemon is already listening on {}", path.display());
        }
        fs::remove_file(&path)
            .with_context(|| format!("failed to remove stale {}", path.display()))?;
    }

    let listener =
        UnixListener::bind(&path).with_context(|| format!("failed to bind {}", path.display()))?;

    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                continue;
            };
            // Read each client on its own thread, bounded in time and size, so a
            // client that never closes its end cannot stall later commands.
            let tx = tx.clone();
            thread::spawn(move || {
                let _ = stream.set_read_timeout(Some(IPC_READ_TIMEOUT));
                let mut payload = String::new();
                if stream
                    .take(IPC_MAX_PAYLOAD)
                    .read_to_string(&mut payload)
                    .is_ok()
                    && let Some(command) = IpcCommand::parse(&payload)
                {
                    let _ = tx.send(command);
                }
            });
        }
    });

    Ok(())
}

fn install_css() {
    let Some(display) = gdk::Display::default() else {
        return;
    };

    let provider = gtk::CssProvider::new();
    provider.load_from_data(CSS);
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

fn strip_markup(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut inside_tag = false;

    for ch in input.chars() {
        match ch {
            '<' => inside_tag = true,
            '>' => inside_tag = false,
            _ if !inside_tag => output.push(ch),
            _ => {}
        }
    }

    output.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window_profile_settings() -> ProfileSettings {
        ProfileSettings {
            target: Target::Windows,
            scope: Scope::CurrentMonitor,
            same_class: false,
            include_special_workspaces: false,
            include_empty_workspaces: false,
        }
    }

    fn active_window(monitor: Option<i64>, workspace_id: i64) -> HyprActiveWindow {
        HyprActiveWindow {
            address: "0x1".to_string(),
            monitor,
            workspace: HyprWorkspace {
                id: workspace_id,
                name: workspace_id.to_string(),
            },
        }
    }

    fn active_workspace(monitor_id: i64) -> HyprActiveWorkspace {
        HyprActiveWorkspace { id: 8, monitor_id }
    }

    fn monitor(id: i64, focused: bool, disabled: bool) -> HyprMonitorInfo {
        HyprMonitorInfo {
            id,
            name: format!("monitor-{id}"),
            active_workspace: HyprWorkspace {
                id: id + 1,
                name: (id + 1).to_string(),
            },
            focused,
            disabled,
        }
    }

    fn hypr_client(address: &str, workspace_id: i64, pinned: bool) -> HyprClient {
        HyprClient {
            address: address.to_string(),
            mapped: true,
            hidden: false,
            accepts_input: true,
            workspace: HyprWorkspace {
                id: workspace_id,
                name: workspace_id.to_string(),
            },
            monitor: 1,
            wm_class: "code".to_string(),
            initial_class: String::new(),
            title: "Code".to_string(),
            pid: 0,
            focus_history_id: Some(0),
            pinned,
        }
    }

    fn window_info(address: &str, workspace_id: i64) -> WindowInfo {
        WindowInfo {
            address: address.to_string(),
            title: "Code".to_string(),
            wm_class: "code".to_string(),
            exe: String::new(),
            app_name: "Visual Studio Code".to_string(),
            desktop_icon: String::new(),
            workspace_id,
            workspace_name: workspace_id.to_string(),
            monitor: 1,
            focus_history_id: 0,
            pinned: false,
            active: false,
        }
    }

    #[test]
    fn initial_next_selects_previous_mru_window() {
        assert_eq!(initial_selection(0, Direction::Next), 0);
        assert_eq!(initial_selection(1, Direction::Next), 0);
        assert_eq!(initial_selection(4, Direction::Next), 1);
    }

    #[test]
    fn initial_prev_wraps_to_oldest_window() {
        assert_eq!(initial_selection(1, Direction::Prev), 0);
        assert_eq!(initial_selection(4, Direction::Prev), 3);
    }

    #[test]
    fn active_single_window_does_not_open_switcher() {
        let settings = window_profile_settings();

        assert!(!should_open_switcher(settings, 0, false));
        assert!(!should_open_switcher(settings, 1, true));
        assert!(should_open_switcher(settings, 2, true));
    }

    #[test]
    fn inactive_single_window_can_open_switcher() {
        assert!(should_open_switcher(window_profile_settings(), 1, false));
    }

    #[test]
    fn non_window_targets_can_open_with_one_candidate() {
        let settings = ProfileSettings {
            target: Target::Workspaces,
            scope: Scope::CurrentMonitor,
            same_class: false,
            include_special_workspaces: false,
            include_empty_workspaces: false,
        };

        assert!(!should_open_switcher(settings, 0, false));
        assert!(should_open_switcher(settings, 1, false));
    }

    #[test]
    fn modifier_poll_waits_for_confirmed_release() {
        let mut missing_samples = 0;

        assert!(!should_close_from_modifier_poll(&mut missing_samples, true));
        assert_eq!(missing_samples, 0);

        assert!(!should_close_from_modifier_poll(
            &mut missing_samples,
            false
        ));
        assert_eq!(missing_samples, 1);

        assert!(should_close_from_modifier_poll(&mut missing_samples, false));
        assert_eq!(missing_samples, MODIFIER_RELEASE_CONFIRM_SAMPLES);
    }

    #[test]
    fn modifier_poll_resets_when_modifier_returns() {
        let mut missing_samples = 1;

        assert!(!should_close_from_modifier_poll(&mut missing_samples, true));
        assert_eq!(missing_samples, 0);
    }

    #[test]
    fn modifier_state_matches_profile_mask() {
        assert!(modifier_is_down(
            Profile::Alt,
            gdk::ModifierType::ALT_MASK | gdk::ModifierType::SHIFT_MASK
        ));
        assert!(!modifier_is_down(
            Profile::Super,
            gdk::ModifierType::ALT_MASK | gdk::ModifierType::SHIFT_MASK
        ));
        assert!(modifier_is_down(
            Profile::Super,
            gdk::ModifierType::SUPER_MASK
        ));
    }

    #[test]
    fn panel_width_tracks_item_count_without_using_a_fixed_full_width() {
        assert_eq!(panel_width(0), PANEL_MIN_WIDTH);
        assert_eq!(
            panel_width(3),
            PANEL_HORIZONTAL_PADDING + PANEL_ITEM_WIDTH * 3
        );
        assert_eq!(panel_width(50), PANEL_MAX_WIDTH);
    }

    #[test]
    fn window_focus_temporarily_disables_cursor_warps() {
        let item = SwitchItem::Window(window_info("0xabc", 7));

        assert_eq!(
            focus_item_commands(&item, false),
            vec![
                "keyword cursor:no_warps 1",
                "dispatch workspace 7",
                "dispatch focuswindow address:0xabc",
                "dispatch alterzorder top,address:0xabc",
                "keyword cursor:no_warps 0",
            ]
        );
    }

    #[test]
    fn focus_keeps_existing_no_warps_setting_enabled() {
        let item = SwitchItem::Window(window_info("0xabc", 7));

        assert_eq!(
            focus_item_commands(&item, true),
            vec![
                "dispatch workspace 7",
                "dispatch focuswindow address:0xabc",
                "dispatch alterzorder top,address:0xabc",
            ]
        );
    }

    #[test]
    fn workspace_focus_also_preserves_cursor_position() {
        let item = SwitchItem::Workspace(WorkspaceInfo {
            id: 7,
            name: "7".to_string(),
            monitor: 1,
            windows: 1,
            last_title: "Code".to_string(),
            best_focus_history_id: 0,
        });

        assert_eq!(
            focus_item_commands(&item, false),
            vec![
                "keyword cursor:no_warps 1",
                "dispatch workspace 7",
                "keyword cursor:no_warps 0",
            ]
        );
    }

    #[test]
    fn default_config_scopes_alt_and_super_to_current_monitor() {
        let config = AppConfig::default();

        assert_eq!(
            config.profile(Profile::Alt),
            ProfileSettings {
                target: Target::Windows,
                scope: Scope::CurrentMonitor,
                same_class: false,
                include_special_workspaces: false,
                include_empty_workspaces: false,
            }
        );
        assert_eq!(
            config.profile(Profile::Super),
            ProfileSettings {
                target: Target::Workspaces,
                scope: Scope::CurrentMonitor,
                same_class: false,
                include_special_workspaces: false,
                include_empty_workspaces: false,
            }
        );
    }

    #[test]
    fn partial_config_preserves_profile_defaults() {
        let config = toml::from_str::<AppConfig>(
            r#"
            [alt]
            scope = "current-workspace"
            "#,
        )
        .expect("config should parse");

        assert_eq!(
            config.profile(Profile::Alt),
            ProfileSettings {
                target: Target::Windows,
                scope: Scope::CurrentWorkspace,
                same_class: false,
                include_special_workspaces: false,
                include_empty_workspaces: false,
            }
        );
        assert_eq!(
            config.profile(Profile::Super),
            ProfileSettings {
                target: Target::Workspaces,
                scope: Scope::CurrentMonitor,
                same_class: false,
                include_special_workspaces: false,
                include_empty_workspaces: false,
            }
        );
    }

    #[test]
    fn partial_super_config_keeps_workspace_target() {
        let config = toml::from_str::<AppConfig>(
            r#"
            [super]
            scope = "all"
            "#,
        )
        .expect("config should parse");

        assert_eq!(
            config.profile(Profile::Super),
            ProfileSettings {
                target: Target::Workspaces,
                scope: Scope::All,
                same_class: false,
                include_special_workspaces: false,
                include_empty_workspaces: false,
            }
        );
    }

    #[test]
    fn partial_alt_config_defaults_to_current_monitor() {
        let config = toml::from_str::<AppConfig>(
            r#"
            [alt]
            same_class = true
            "#,
        )
        .expect("config should parse");

        assert_eq!(
            config.profile(Profile::Alt),
            ProfileSettings {
                target: Target::Windows,
                scope: Scope::CurrentMonitor,
                same_class: true,
                include_special_workspaces: false,
                include_empty_workspaces: false,
            }
        );
    }

    #[test]
    fn extended_profile_config_parses() {
        let config = toml::from_str::<AppConfig>(
            r#"
            [super]
            target = "windows"
            scope = "current-workspace"
            same_class = true
            include_special_workspaces = true
            include_empty_workspaces = true
            "#,
        )
        .expect("config should parse");

        assert_eq!(
            config.profile(Profile::Super),
            ProfileSettings {
                target: Target::Windows,
                scope: Scope::CurrentWorkspace,
                same_class: true,
                include_special_workspaces: true,
                include_empty_workspaces: true,
            }
        );
    }

    #[test]
    fn scope_current_monitor_matches_only_active_monitor() {
        assert!(Scope::CurrentMonitor.includes(6, 1, Some(6), Some(1)));
        assert!(!Scope::CurrentMonitor.includes(2, 0, Some(6), Some(1)));
    }

    #[test]
    fn current_monitor_prefers_focused_monitor() {
        let monitors = [monitor(2, true, false)];
        let active = active_window(Some(0), 1);
        let active_workspace = active_workspace(1);

        assert_eq!(
            current_monitor_id(Some(&monitors), Some(&active_workspace), Some(&active)),
            Some(2)
        );
    }

    #[test]
    fn current_monitor_falls_back_to_active_workspace_monitor() {
        let active = active_window(None, 8);
        let active_workspace = active_workspace(1);

        assert_eq!(
            current_monitor_id(None, Some(&active_workspace), Some(&active)),
            Some(1)
        );
    }

    #[test]
    fn current_monitor_falls_back_to_active_window_monitor() {
        let active = active_window(Some(2), 8);

        assert_eq!(current_monitor_id(None, None, Some(&active)), Some(2));
    }

    #[test]
    fn current_monitor_ignores_disabled_focused_monitor() {
        let monitors = [monitor(2, true, true)];
        let active_workspace = active_workspace(1);

        assert_eq!(
            current_monitor_id(Some(&monitors), Some(&active_workspace), None),
            Some(1)
        );
    }

    #[test]
    fn active_window_must_be_on_active_workspace() {
        let active = active_window(Some(1), 7);
        let client = hypr_client("0x1", 7, false);

        assert!(client_is_active(
            Some("0x1"),
            Some(7),
            Some(&active),
            &client
        ));
        assert!(!client_is_active(
            Some("0x1"),
            Some(9),
            Some(&active),
            &client
        ));
    }

    #[test]
    fn markup_is_stripped_from_titles() {
        assert_eq!(strip_markup("one <b>two</b> three"), "one two three");
    }

    #[test]
    fn display_titles_use_pipe_separators() {
        assert_eq!(clean_display_title("Desktop — rnn.py"), "Desktop | rnn.py");
        assert_eq!(
            clean_display_title("one --- two -- three – four"),
            "one | two | three | four"
        );
    }

    fn desktop_entry(id: &str, name: &str, wm_class: &str, exec: &str) -> DesktopEntry {
        DesktopEntry {
            id: id.to_ascii_lowercase(),
            name: name.to_string(),
            icon: format!("{}-icon", id.to_ascii_lowercase()),
            wm_class: wm_class.to_string(),
            exec: exec.to_string(),
            no_display: false,
        }
    }

    #[test]
    fn desktop_entry_reads_main_section_only() {
        let contents = "\
[Desktop Entry]
Type=Application
Name=Claude
Exec=claude-desktop %U
Icon=claude-desktop
StartupWMClass=com.anthropic.Claude

[Desktop Action new-chat]
Name=New chat
Icon=other
";
        assert_eq!(
            parse_desktop_entry("com.anthropic.Claude", contents, &[]),
            Some(DesktopEntry {
                id: "com.anthropic.claude".to_string(),
                name: "Claude".to_string(),
                icon: "claude-desktop".to_string(),
                wm_class: "com.anthropic.Claude".to_string(),
                exec: "claude-desktop".to_string(),
                no_display: false,
            })
        );
    }

    #[test]
    fn desktop_entry_prefers_most_specific_localized_name() {
        let contents = "\
[Desktop Entry]
Name=Files
Name[es]=Archivos
Name[es_MX]=Archivos MX
Name[fr]=Fichiers
";
        let locale = |names: &[&str]| {
            names
                .iter()
                .map(|name| name.to_string())
                .collect::<Vec<_>>()
        };
        let name_for = |names: &[&str]| {
            parse_desktop_entry("files", contents, &locale(names)).map(|entry| entry.name)
        };
        assert_eq!(name_for(&[]), Some("Files".to_string()));
        assert_eq!(name_for(&["es_ES", "es"]), Some("Archivos".to_string()));
        assert_eq!(name_for(&["es_MX", "es"]), Some("Archivos MX".to_string()));
        assert_eq!(name_for(&["de_DE", "de"]), Some("Files".to_string()));
    }

    #[test]
    fn desktop_entry_skips_hidden_and_non_application_entries() {
        let hidden = "[Desktop Entry]\nName=Gone\nIcon=gone\nHidden=true\n";
        assert_eq!(parse_desktop_entry("gone", hidden, &[]), None);
        let link = "[Desktop Entry]\nType=Link\nName=Docs\nIcon=docs\n";
        assert_eq!(parse_desktop_entry("docs", link, &[]), None);
    }

    #[test]
    fn desktop_index_matches_by_class_id_tail_exec_and_name() {
        let mut index = DesktopIndex::default();
        index.insert(desktop_entry(
            "com.anthropic.Claude",
            "Claude",
            "com.anthropic.Claude",
            "claude-desktop",
        ));
        index.insert(desktop_entry("dev.zed.Zed", "Zed", "", "zed"));
        index.insert(desktop_entry(
            "spotify-launcher",
            "Spotify",
            "",
            "spotify-launcher",
        ));
        index.insert(desktop_entry("org.example.Tool", "Tool", "", "tool-bin"));

        let name_of = |class: &str| index.lookup(class).map(|entry| entry.name.as_str());
        assert_eq!(name_of("com.anthropic.Claude"), Some("Claude"));
        assert_eq!(name_of("dev.zed.Zed"), Some("Zed"));
        assert_eq!(name_of("zed"), Some("Zed"));
        assert_eq!(name_of("spotify-launcher"), Some("Spotify"));
        assert_eq!(name_of("Spotify"), Some("Spotify"));
        assert_eq!(name_of("tool-bin"), Some("Tool"));
        assert_eq!(name_of("unknown"), None);
        assert_eq!(name_of(""), None);
    }

    #[test]
    fn desktop_index_fuzzy_keys_do_not_depend_on_insert_order() {
        // Every Steam game desktop file has `Exec=steam steam://rungameid/...`.
        let steam = desktop_entry("steam", "Steam", "", "steam");
        let game = desktop_entry("Counter-Strike 2", "Counter-Strike 2", "", "steam");

        for entries in [[steam.clone(), game.clone()], [game, steam]] {
            let mut index = DesktopIndex::default();
            for entry in entries {
                index.insert(entry);
            }
            assert_eq!(index.by_exec["steam"].name, "Steam");
        }
    }

    #[test]
    fn user_desktop_files_override_and_hide_system_files() {
        let root = env::temp_dir().join(format!("mogtab-test-{}", std::process::id()));
        let user = root.join("user");
        let system = root.join("system");
        fs::create_dir_all(&user).unwrap();
        fs::create_dir_all(&system).unwrap();

        fs::write(
            system.join("editor.desktop"),
            "[Desktop Entry]\nName=Editor\nIcon=system-editor\n",
        )
        .unwrap();
        fs::write(
            user.join("editor.desktop"),
            "[Desktop Entry]\nName=Editor\nIcon=user-editor\n",
        )
        .unwrap();
        fs::write(
            system.join("old.desktop"),
            "[Desktop Entry]\nName=Old\nIcon=old\nStartupWMClass=old-app\n",
        )
        .unwrap();
        fs::write(user.join("old.desktop"), "[Desktop Entry]\nHidden=true\n").unwrap();

        let index = DesktopIndex::load(&[user, system]);
        fs::remove_dir_all(&root).unwrap();

        assert_eq!(index.lookup("editor").unwrap().icon, "user-editor");
        assert_eq!(index.lookup("old"), None);
        assert_eq!(index.lookup("old-app"), None);
    }

    #[test]
    fn steam_games_try_their_steam_icon() {
        assert!(icon_candidates("", "steam_app_730", "").contains(&"steam_icon_730".to_string()));
    }

    #[test]
    fn generic_runtimes_are_not_used_to_identify_apps() {
        assert!(!is_specific_exe(""));
        assert!(!is_specific_exe("electron36"));
        assert!(!is_specific_exe("python3.13"));
        assert!(is_specific_exe("alacritty"));
    }

    #[test]
    fn exec_program_skips_env_and_arguments() {
        assert_eq!(exec_program("claude-desktop %U"), "claude-desktop");
        assert_eq!(exec_program("env FOO=1 /opt/app/bin/app --flag"), "app");
        assert_eq!(exec_program("/usr/bin/zed %F"), "zed");
        assert_eq!(exec_program("\"/opt/My App/bin/app\" %U"), "app");
        assert_eq!(
            exec_arguments(r#"sh -c "echo \"hi\" there" arg"#),
            ["sh", "-c", "echo \"hi\" there", "arg"]
        );
    }
}
