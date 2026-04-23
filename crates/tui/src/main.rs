use std::io::Stdout;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::RwLock;

mod poller;
mod state;
mod ui;

use state::{
    AppState, ConnectionStatus, CreateField, DeviceField, DevicesMode, FocusedPane,
    NotificationKind, RoutesMode, Screen,
};

fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
        original(info);
    }));
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    // The daemon must not share this tty — its stderr will bleed into the alt-screen.
    // Use `spaghetti daemon install` to run it as a LaunchAgent instead.
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) {
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();
}

#[tokio::main]
async fn main() -> Result<()> {
    install_panic_hook();

    let state = Arc::new(RwLock::new(AppState::new()));
    tokio::spawn(poller::run_poller(Arc::clone(&state)));

    let mut terminal = setup_terminal()?;
    let result = run_event_loop(&mut terminal, &state).await;
    restore_terminal(&mut terminal);
    result
}

async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &Arc<RwLock<AppState>>,
) -> Result<()> {
    let tick = Duration::from_millis(16); // ~60 fps
    loop {
        {
            let mut s = state.write().await;
            s.expire_notification();
            s.apps_screen.clear_armed_if_expired();
            s.routes_screen.clear_armed_if_expired();
            s.devices_screen.clear_armed_if_expired();
        }
        {
            let s = state.read().await;
            ui::draw(terminal, &s)?;
        }

        if event::poll(tick)?
            && let Event::Key(key) = event::read()?
        {
            if key.kind == KeyEventKind::Release {
                continue;
            }

            // Help overlay swallows all keys except those that close it.
            if state.read().await.show_help {
                match key.code {
                    KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q') => {
                        state.write().await.show_help = false;
                    }
                    _ => {}
                }
                continue;
            }

            // While a modal create form is active, route all keys to the screen
            // handler and suppress global navigation entirely. This ensures Tab
            // moves between form fields rather than switching screens, and digits
            // 1–4 don't cause a surprise context switch mid-edit.
            if state.read().await.is_modal_active() {
                let current = state.read().await.current_screen;
                match current {
                    Screen::Routes => handle_routes_key(state, key.code, key.modifiers).await,
                    Screen::Devices => handle_devices_key(state, key.code, key.modifiers).await,
                    _ => {}
                }
                continue;
            }

            // Global keys.
            match key.code {
                KeyCode::Char('?') => {
                    state.write().await.show_help = true;
                    continue;
                }
                KeyCode::Char('q') => return Ok(()),
                KeyCode::Char('1') => {
                    state.write().await.current_screen = Screen::Dashboard;
                    continue;
                }
                KeyCode::Char('2') => {
                    state.write().await.current_screen = Screen::Apps;
                    continue;
                }
                KeyCode::Char('3') => {
                    state.write().await.current_screen = Screen::Routes;
                    continue;
                }
                KeyCode::Char('4') => {
                    state.write().await.current_screen = Screen::Devices;
                    continue;
                }
                KeyCode::Tab => {
                    let mut s = state.write().await;
                    let idx = Screen::ALL
                        .iter()
                        .position(|&sc| sc == s.current_screen)
                        .unwrap_or(0);
                    s.current_screen = Screen::ALL[(idx + 1) % Screen::ALL.len()];
                    continue;
                }
                KeyCode::BackTab => {
                    let mut s = state.write().await;
                    let idx = Screen::ALL
                        .iter()
                        .position(|&sc| sc == s.current_screen)
                        .unwrap_or(0);
                    let prev = if idx == 0 { Screen::ALL.len() - 1 } else { idx - 1 };
                    s.current_screen = Screen::ALL[prev];
                    continue;
                }
                _ => {}
            }

            let current = state.read().await.current_screen;
            match current {
                Screen::Apps => handle_apps_key(state, key.code, key.modifiers).await,
                Screen::Routes => handle_routes_key(state, key.code, key.modifiers).await,
                Screen::Devices => handle_devices_key(state, key.code, key.modifiers).await,
                Screen::Dashboard => {}
            }
        }
    }
}

/// Show "Disconnected — action ignored" and return true when offline.
async fn guard_connected(state: &Arc<RwLock<AppState>>) -> bool {
    let offline = state.read().await.connection_status == ConnectionStatus::Disconnected;
    if offline {
        state.write().await.set_notification(
            "Disconnected — action ignored".to_string(),
            NotificationKind::Error,
            2,
        );
    }
    offline
}

/// Spawn a task to create a tap for the currently selected app with the given mode.
///
/// No-ops if the Apps pane is not focused, there are no apps, or the app is already tapped.
async fn create_tap_for_selected(
    state: &Arc<RwLock<AppState>>,
    mode: spaghetti_common::ipc_protocol::TapMode,
) {
    let (app_info, already_tapped) = {
        let s = state.read().await;
        if s.apps_screen.focused != FocusedPane::Apps {
            return;
        }
        let Some(ref apps) = s.apps else { return };
        if apps.is_empty() {
            return;
        }
        let mut sorted: Vec<_> = apps.iter().collect();
        sorted.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        let cursor = s.apps_screen.app_cursor.min(sorted.len().saturating_sub(1));
        let app = sorted[cursor];

        let tapped = s
            .taps
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .any(|t| t.pid == app.pid);

        ((app.pid, app.name.clone()), tapped)
    };

    if already_tapped {
        state.write().await.set_notification(
            format!("Tap already exists for {}", app_info.1),
            NotificationKind::Info,
            2,
        );
        return;
    }

    let state_clone = Arc::clone(state);
    tokio::spawn(async move {
        let pid = app_info.0;
        let name = app_info.1;
        match spaghetti_common::async_client::IpcClient::connect().await {
            Ok(mut client) => {
                match client
                    .send(spaghetti_common::ipc_protocol::Request::CreateTap { pid, mode })
                    .await
                {
                    Ok(spaghetti_common::ipc_protocol::Response::TapCreated { tap_id }) => {
                        state_clone.write().await.set_notification(
                            format!("Created tap #{tap_id} for {name}"),
                            NotificationKind::Info,
                            2,
                        );
                    }
                    Ok(spaghetti_common::ipc_protocol::Response::Error { message }) => {
                        state_clone.write().await.set_notification(
                            format!("Error: {message}"),
                            NotificationKind::Error,
                            3,
                        );
                    }
                    Ok(_) | Err(_) => {
                        state_clone.write().await.set_notification(
                            "Failed to create tap".to_string(),
                            NotificationKind::Error,
                            3,
                        );
                    }
                }
            }
            Err(_) => {
                state_clone.write().await.set_notification(
                    "Cannot connect to daemon".to_string(),
                    NotificationKind::Error,
                    3,
                );
            }
        }
    });
}

async fn handle_apps_key(
    state: &Arc<RwLock<AppState>>,
    code: KeyCode,
    _modifiers: KeyModifiers,
) {
    match code {
        // Pane switching — always allowed.
        KeyCode::Char('h') | KeyCode::Left => {
            let mut s = state.write().await;
            s.apps_screen.focused = FocusedPane::Apps;
            s.apps_screen.delete_armed = None;
        }
        KeyCode::Char('l') | KeyCode::Right => {
            let mut s = state.write().await;
            s.apps_screen.focused = FocusedPane::Taps;
        }

        // Navigation — always allowed.
        KeyCode::Char('j') | KeyCode::Down => {
            let mut s = state.write().await;
            match s.apps_screen.focused {
                FocusedPane::Apps => {
                    let len = s.apps.as_deref().map(|a| a.len()).unwrap_or(0);
                    if len > 0 {
                        s.apps_screen.app_cursor =
                            (s.apps_screen.app_cursor + 1).min(len.saturating_sub(1));
                    }
                    s.apps_screen.delete_armed = None;
                }
                FocusedPane::Taps => {
                    let len = s.taps.as_deref().map(|t| t.len()).unwrap_or(0);
                    if len > 0 {
                        s.apps_screen.tap_cursor =
                            (s.apps_screen.tap_cursor + 1).min(len.saturating_sub(1));
                    }
                    s.apps_screen.delete_armed = None;
                }
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            let mut s = state.write().await;
            match s.apps_screen.focused {
                FocusedPane::Apps => {
                    s.apps_screen.app_cursor = s.apps_screen.app_cursor.saturating_sub(1);
                    s.apps_screen.delete_armed = None;
                }
                FocusedPane::Taps => {
                    s.apps_screen.tap_cursor = s.apps_screen.tap_cursor.saturating_sub(1);
                    s.apps_screen.delete_armed = None;
                }
            }
        }

        // Mutation keys — require connection.
        KeyCode::Enter => {
            if guard_connected(state).await {
                return;
            }
            create_tap_for_selected(state, spaghetti_common::ipc_protocol::TapMode::Mirror).await;
        }

        KeyCode::Char('m') => {
            if state.read().await.apps_screen.focused != FocusedPane::Apps {
                state.write().await.apps_screen.delete_armed = None;
                return;
            }
            if guard_connected(state).await {
                return;
            }
            create_tap_for_selected(
                state,
                spaghetti_common::ipc_protocol::TapMode::MuteSourceWhenTapped,
            )
            .await;
        }

        KeyCode::Char('d') => {
            let (focused, tap_info, armed) = {
                let s = state.read().await;
                if s.apps_screen.focused != FocusedPane::Taps {
                    drop(s);
                    state.write().await.apps_screen.delete_armed = None;
                    return;
                }
                let Some(ref taps) = s.taps else { return };
                if taps.is_empty() {
                    return;
                }
                let cursor = s.apps_screen.tap_cursor.min(taps.len().saturating_sub(1));
                let tap = &taps[cursor];

                let armed_id = s.apps_screen.delete_armed.map(|(id, _)| id);
                let armed_matches = armed_id == Some(tap.tap_id);

                (FocusedPane::Taps, (tap.tap_id, tap.app_name.clone()), armed_matches)
            };

            let _ = focused; // always Taps here

            if armed {
                if guard_connected(state).await {
                    state.write().await.apps_screen.delete_armed = None;
                    return;
                }
                {
                    state.write().await.apps_screen.delete_armed = None;
                }
                let state_clone = Arc::clone(state);
                tokio::spawn(async move {
                    let tap_id = tap_info.0;
                    let name = tap_info.1;
                    match spaghetti_common::async_client::IpcClient::connect().await {
                        Ok(mut client) => {
                            match client
                                .send(
                                    spaghetti_common::ipc_protocol::Request::DestroyTap { tap_id },
                                )
                                .await
                            {
                                Ok(spaghetti_common::ipc_protocol::Response::Ok) => {
                                    state_clone.write().await.set_notification(
                                        format!("Destroyed tap #{tap_id} for {name}"),
                                        NotificationKind::Info,
                                        2,
                                    );
                                }
                                Ok(spaghetti_common::ipc_protocol::Response::Error {
                                    message,
                                }) => {
                                    state_clone.write().await.set_notification(
                                        format!("Error: {message}"),
                                        NotificationKind::Error,
                                        3,
                                    );
                                }
                                Ok(_) | Err(_) => {
                                    state_clone.write().await.set_notification(
                                        "Failed to destroy tap".to_string(),
                                        NotificationKind::Error,
                                        3,
                                    );
                                }
                            }
                        }
                        Err(_) => {
                            state_clone.write().await.set_notification(
                                "Cannot connect to daemon".to_string(),
                                NotificationKind::Error,
                                3,
                            );
                        }
                    }
                });
            } else {
                // First press — arm.
                let tap_id = tap_info.0;
                state.write().await.apps_screen.delete_armed =
                    Some((tap_id, std::time::Instant::now()));
            }
        }

        _ => {
            state.write().await.apps_screen.delete_armed = None;
        }
    }
}

async fn handle_routes_key(
    state: &Arc<RwLock<AppState>>,
    code: KeyCode,
    modifiers: KeyModifiers,
) {
    let mode = state.read().await.routes_screen.mode.clone();

    match mode {
        RoutesMode::Creating => handle_create_form_key(state, code, modifiers).await,
        RoutesMode::Editing { route_id } => {
            handle_volume_edit_key(state, code, modifiers, route_id).await;
        }
        RoutesMode::List => handle_list_key(state, code).await,
    }
}

async fn handle_list_key(state: &Arc<RwLock<AppState>>, code: KeyCode) {
    match code {
        // Navigation — always allowed.
        KeyCode::Char('j') | KeyCode::Down => {
            let mut s = state.write().await;
            let len = s.routes.as_deref().map(|r| r.len()).unwrap_or(0);
            if len > 0 {
                s.routes_screen.cursor =
                    (s.routes_screen.cursor + 1).min(len.saturating_sub(1));
            }
            s.routes_screen.armed_destroy = None;
        }
        KeyCode::Char('k') | KeyCode::Up => {
            let mut s = state.write().await;
            s.routes_screen.cursor = s.routes_screen.cursor.saturating_sub(1);
            s.routes_screen.armed_destroy = None;
        }

        KeyCode::Char('c') => {
            if guard_connected(state).await {
                return;
            }
            let mut s = state.write().await;
            let output_count = s.system_devices.iter().filter(|d| d.is_output).count();
            if output_count == 0 {
                s.set_notification(
                    "No output devices available".to_string(),
                    NotificationKind::Error,
                    3,
                );
                return;
            }
            // selected_device_idx lives in filtered (output-only) index space,
            // matching the renderer and submit handler which both filter by is_output.
            s.routes_screen.create_form.selected_tap_idx = 0;
            s.routes_screen.create_form.selected_device_idx = 0;
            s.routes_screen.create_form.volume = 1.0;
            s.routes_screen.create_form.active_field = CreateField::Tap;
            s.routes_screen.mode = RoutesMode::Creating;
            s.routes_screen.armed_destroy = None;
        }

        KeyCode::Enter => {
            let route_id = {
                let s = state.read().await;
                let routes = s.routes.as_deref().unwrap_or(&[]);
                if routes.is_empty() {
                    return;
                }
                let cursor = s.routes_screen.cursor.min(routes.len().saturating_sub(1));
                routes[cursor].route_id
            };
            state.write().await.routes_screen.mode = RoutesMode::Editing { route_id };
        }

        KeyCode::Char('d') => {
            let (route_id, armed) = {
                let s = state.read().await;
                let routes = s.routes.as_deref().unwrap_or(&[]);
                if routes.is_empty() {
                    return;
                }
                let cursor = s.routes_screen.cursor.min(routes.len().saturating_sub(1));
                let id = routes[cursor].route_id;
                let armed = s
                    .routes_screen
                    .armed_destroy
                    .map(|(aid, _)| aid == id)
                    .unwrap_or(false);
                (id, armed)
            };

            if armed {
                if guard_connected(state).await {
                    state.write().await.routes_screen.armed_destroy = None;
                    return;
                }
                state.write().await.routes_screen.armed_destroy = None;
                let state_clone = Arc::clone(state);
                tokio::spawn(async move {
                    match spaghetti_common::async_client::IpcClient::connect().await {
                        Ok(mut client) => {
                            match client
                                .send(spaghetti_common::ipc_protocol::Request::DestroyRoute {
                                    route_id,
                                })
                                .await
                            {
                                Ok(spaghetti_common::ipc_protocol::Response::Ok) => {
                                    state_clone.write().await.set_notification(
                                        format!("Destroyed route #{route_id}"),
                                        NotificationKind::Info,
                                        2,
                                    );
                                }
                                Ok(spaghetti_common::ipc_protocol::Response::Error {
                                    message,
                                }) => {
                                    state_clone.write().await.set_notification(
                                        format!("Error: {message}"),
                                        NotificationKind::Error,
                                        3,
                                    );
                                }
                                Ok(_) | Err(_) => {
                                    state_clone.write().await.set_notification(
                                        "Failed to destroy route".to_string(),
                                        NotificationKind::Error,
                                        3,
                                    );
                                }
                            }
                        }
                        Err(_) => {
                            state_clone.write().await.set_notification(
                                "Cannot connect to daemon".to_string(),
                                NotificationKind::Error,
                                3,
                            );
                        }
                    }
                });
            } else {
                state.write().await.routes_screen.armed_destroy =
                    Some((route_id, std::time::Instant::now()));
            }
        }

        _ => {
            state.write().await.routes_screen.armed_destroy = None;
        }
    }
}

async fn handle_volume_edit_key(
    state: &Arc<RwLock<AppState>>,
    code: KeyCode,
    modifiers: KeyModifiers,
    route_id: u32,
) {
    let shift = modifiers.contains(KeyModifiers::SHIFT);

    let delta: Option<f32> = match code {
        KeyCode::Char('h') | KeyCode::Left if !shift => Some(-0.05),
        KeyCode::Char('l') | KeyCode::Right if !shift => Some(0.05),
        KeyCode::Char('H') | KeyCode::Left if shift => Some(-0.25),
        KeyCode::Char('L') | KeyCode::Right if shift => Some(0.25),
        _ => None,
    };

    let reset_to: Option<f32> = match code {
        KeyCode::Char('0') => Some(0.0),
        KeyCode::Char('1') => Some(1.0),
        _ => None,
    };

    if matches!(code, KeyCode::Enter | KeyCode::Esc) {
        state.write().await.routes_screen.mode = RoutesMode::List;
        return;
    }

    let new_volume = {
        let s = state.read().await;
        let routes = s.routes.as_deref().unwrap_or(&[]);
        let Some(route) = routes.iter().find(|r| r.route_id == route_id) else {
            return;
        };

        if let Some(to) = reset_to {
            to
        } else if let Some(d) = delta {
            (route.volume + d).clamp(0.0, 2.0)
        } else {
            return;
        }
    };

    // Check in-flight guard. If a SetVolume is already in flight for this route,
    // coalesce by updating the optimistic value locally and dropping this keypress.
    // The read guard MUST be released before taking a write lock on the same RwLock —
    // tokio's RwLock is write-preferring, and holding a read while awaiting a write
    // from the same task deadlocks.
    let inflight = {
        let s = state.read().await;
        s.routes_screen.inflight_set_volume.contains(&route_id)
    };
    if inflight {
        let mut s = state.write().await;
        if let Some(ref mut routes) = s.routes
            && let Some(route) = routes.iter_mut().find(|r| r.route_id == route_id)
        {
            route.volume = new_volume;
        }
        return;
    }

    // Guard connection before firing IPC.
    if guard_connected(state).await {
        // Still apply optimistic update so the UI doesn't feel frozen.
        let mut s = state.write().await;
        if let Some(ref mut routes) = s.routes
            && let Some(route) = routes.iter_mut().find(|r| r.route_id == route_id)
        {
            route.volume = new_volume;
        }
        return;
    }

    // Optimistic update + mark in-flight.
    {
        let mut s = state.write().await;
        if let Some(ref mut routes) = s.routes
            && let Some(route) = routes.iter_mut().find(|r| r.route_id == route_id)
        {
            route.volume = new_volume;
        }
        s.routes_screen.inflight_set_volume.insert(route_id);
    }

    let state_clone = Arc::clone(state);
    tokio::spawn(async move {
        let result = async {
            let mut client = spaghetti_common::async_client::IpcClient::connect().await?;
            client
                .send(spaghetti_common::ipc_protocol::Request::SetVolume {
                    route_id,
                    volume: new_volume,
                })
                .await
        }
        .await;

        let mut s = state_clone.write().await;
        s.routes_screen.inflight_set_volume.remove(&route_id);

        match result {
            Ok(spaghetti_common::ipc_protocol::Response::Ok) => {}
            Ok(spaghetti_common::ipc_protocol::Response::Error { message }) => {
                s.set_notification(format!("Volume error: {message}"), NotificationKind::Error, 3);
            }
            Ok(_) | Err(_) => {
                s.set_notification(
                    "Failed to set volume".to_string(),
                    NotificationKind::Error,
                    3,
                );
            }
        }
    });
}

async fn handle_create_form_key(
    state: &Arc<RwLock<AppState>>,
    code: KeyCode,
    modifiers: KeyModifiers,
) {
    if code == KeyCode::Esc {
        state.write().await.routes_screen.mode = RoutesMode::List;
        return;
    }

    {
        let s = state.read().await;
        if s.taps.as_deref().unwrap_or(&[]).is_empty() {
            return;
        }
    }

    let shift = modifiers.contains(KeyModifiers::SHIFT);

    match code {
        KeyCode::Tab => {
            let mut s = state.write().await;
            s.routes_screen.create_form.active_field =
                s.routes_screen.create_form.active_field.next();
        }
        KeyCode::BackTab => {
            let mut s = state.write().await;
            s.routes_screen.create_form.active_field =
                s.routes_screen.create_form.active_field.prev();
        }

        KeyCode::Up | KeyCode::Char('k') => {
            let mut s = state.write().await;
            match s.routes_screen.create_form.active_field {
                CreateField::Tap => {
                    s.routes_screen.create_form.selected_tap_idx = s
                        .routes_screen
                        .create_form
                        .selected_tap_idx
                        .saturating_sub(1);
                }
                CreateField::Device => {
                    s.routes_screen.create_form.selected_device_idx = s
                        .routes_screen
                        .create_form
                        .selected_device_idx
                        .saturating_sub(1);
                }
                CreateField::Volume => {
                    let vol = s.routes_screen.create_form.volume;
                    s.routes_screen.create_form.volume = (vol - 0.05).clamp(0.0, 2.0);
                }
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let (tap_len, dev_len) = {
                let s = state.read().await;
                let tl = s.taps.as_deref().unwrap_or(&[]).len();
                let dl = s.system_devices.iter().filter(|d| d.is_output).count();
                (tl, dl)
            };
            let mut s = state.write().await;
            match s.routes_screen.create_form.active_field {
                CreateField::Tap => {
                    let idx = s.routes_screen.create_form.selected_tap_idx;
                    s.routes_screen.create_form.selected_tap_idx =
                        (idx + 1).min(tap_len.saturating_sub(1));
                }
                CreateField::Device => {
                    let idx = s.routes_screen.create_form.selected_device_idx;
                    s.routes_screen.create_form.selected_device_idx =
                        (idx + 1).min(dev_len.saturating_sub(1));
                }
                CreateField::Volume => {
                    let vol = s.routes_screen.create_form.volume;
                    s.routes_screen.create_form.volume = (vol + 0.05).clamp(0.0, 2.0);
                }
            }
        }

        KeyCode::Left | KeyCode::Char('h') if !shift => {
            let mut s = state.write().await;
            if s.routes_screen.create_form.active_field == CreateField::Volume {
                let vol = s.routes_screen.create_form.volume;
                s.routes_screen.create_form.volume = (vol - 0.05).clamp(0.0, 2.0);
            }
        }
        KeyCode::Right | KeyCode::Char('l') if !shift => {
            let mut s = state.write().await;
            if s.routes_screen.create_form.active_field == CreateField::Volume {
                let vol = s.routes_screen.create_form.volume;
                s.routes_screen.create_form.volume = (vol + 0.05).clamp(0.0, 2.0);
            }
        }
        KeyCode::Left | KeyCode::Char('H') if shift => {
            let mut s = state.write().await;
            if s.routes_screen.create_form.active_field == CreateField::Volume {
                let vol = s.routes_screen.create_form.volume;
                s.routes_screen.create_form.volume = (vol - 0.25).clamp(0.0, 2.0);
            }
        }
        KeyCode::Right | KeyCode::Char('L') if shift => {
            let mut s = state.write().await;
            if s.routes_screen.create_form.active_field == CreateField::Volume {
                let vol = s.routes_screen.create_form.volume;
                s.routes_screen.create_form.volume = (vol + 0.25).clamp(0.0, 2.0);
            }
        }
        KeyCode::Char('0') => {
            let mut s = state.write().await;
            if s.routes_screen.create_form.active_field == CreateField::Volume {
                s.routes_screen.create_form.volume = 0.0;
            }
        }
        KeyCode::Char('1') => {
            let mut s = state.write().await;
            if s.routes_screen.create_form.active_field == CreateField::Volume {
                s.routes_screen.create_form.volume = 1.0;
            }
        }

        KeyCode::Enter => {
            if guard_connected(state).await {
                return;
            }
            let (tap_id, device_uid, volume) = {
                let s = state.read().await;
                let taps = s.taps.as_deref().unwrap_or(&[]);
                let output_devs: Vec<_> = s
                    .system_devices
                    .iter()
                    .filter(|d| d.is_output)
                    .collect();

                if taps.is_empty() || output_devs.is_empty() {
                    return;
                }

                let tap_idx = s
                    .routes_screen
                    .create_form
                    .selected_tap_idx
                    .min(taps.len().saturating_sub(1));
                let dev_idx = s
                    .routes_screen
                    .create_form
                    .selected_device_idx
                    .min(output_devs.len().saturating_sub(1));

                (
                    taps[tap_idx].tap_id,
                    output_devs[dev_idx].uid.clone(),
                    s.routes_screen.create_form.volume,
                )
            };

            state.write().await.routes_screen.mode = RoutesMode::List;

            let state_clone = Arc::clone(state);
            tokio::spawn(async move {
                match spaghetti_common::async_client::IpcClient::connect().await {
                    Ok(mut client) => {
                        match client
                            .send(spaghetti_common::ipc_protocol::Request::CreateRoute {
                                tap_id,
                                device_uid,
                                volume,
                            })
                            .await
                        {
                            Ok(spaghetti_common::ipc_protocol::Response::RouteCreated {
                                route_id,
                            }) => {
                                state_clone.write().await.set_notification(
                                    format!("Created route #{route_id}"),
                                    NotificationKind::Info,
                                    2,
                                );
                            }
                            Ok(spaghetti_common::ipc_protocol::Response::Error { message }) => {
                                state_clone.write().await.set_notification(
                                    format!("Error: {message}"),
                                    NotificationKind::Error,
                                    3,
                                );
                            }
                            Ok(_) | Err(_) => {
                                state_clone.write().await.set_notification(
                                    "Failed to create route".to_string(),
                                    NotificationKind::Error,
                                    3,
                                );
                            }
                        }
                    }
                    Err(_) => {
                        state_clone.write().await.set_notification(
                            "Cannot connect to daemon".to_string(),
                            NotificationKind::Error,
                            3,
                        );
                    }
                }
            });
        }

        _ => {}
    }
}

// ─── Devices screen ───────────────────────────────────────────────────────────

async fn handle_devices_key(
    state: &Arc<RwLock<AppState>>,
    code: KeyCode,
    modifiers: KeyModifiers,
) {
    let mode = state.read().await.devices_screen.mode.clone();
    match mode {
        DevicesMode::Creating => handle_device_create_form_key(state, code, modifiers).await,
        DevicesMode::List => handle_devices_list_key(state, code).await,
    }
}

async fn handle_devices_list_key(state: &Arc<RwLock<AppState>>, code: KeyCode) {
    match code {
        // Navigation — always allowed.
        KeyCode::Char('j') | KeyCode::Down => {
            let mut s = state.write().await;
            let len = s.devices.as_deref().map(|d| d.len()).unwrap_or(0);
            if len > 0 {
                s.devices_screen.cursor =
                    (s.devices_screen.cursor + 1).min(len.saturating_sub(1));
            }
            s.devices_screen.armed_destroy = None;
        }
        KeyCode::Char('k') | KeyCode::Up => {
            let mut s = state.write().await;
            s.devices_screen.cursor = s.devices_screen.cursor.saturating_sub(1);
            s.devices_screen.armed_destroy = None;
        }

        KeyCode::Char('c') => {
            if guard_connected(state).await {
                return;
            }
            let mut s = state.write().await;
            s.devices_screen.create_form = state::DeviceCreateForm::new();
            s.devices_screen.mode = DevicesMode::Creating;
            s.devices_screen.armed_destroy = None;
        }

        KeyCode::Char('d') => {
            let (device_uid, armed) = {
                let s = state.read().await;
                let devices = s.devices.as_deref().unwrap_or(&[]);
                if devices.is_empty() {
                    return;
                }
                let cursor = s.devices_screen.cursor.min(devices.len().saturating_sub(1));
                let uid = devices[cursor].uid.clone();
                let armed = s
                    .devices_screen
                    .armed_destroy
                    .as_ref()
                    .map(|(auid, _)| auid == &uid)
                    .unwrap_or(false);
                (uid, armed)
            };

            if armed {
                if guard_connected(state).await {
                    state.write().await.devices_screen.armed_destroy = None;
                    return;
                }
                state.write().await.devices_screen.armed_destroy = None;
                let state_clone = Arc::clone(state);
                tokio::spawn(async move {
                    match spaghetti_common::async_client::IpcClient::connect().await {
                        Ok(mut client) => {
                            match client
                                .send(spaghetti_common::ipc_protocol::Request::DestroyDevice {
                                    device_uid: device_uid.clone(),
                                })
                                .await
                            {
                                Ok(spaghetti_common::ipc_protocol::Response::Ok) => {
                                    state_clone.write().await.set_notification(
                                        format!("Destroyed device {device_uid}"),
                                        NotificationKind::Info,
                                        2,
                                    );
                                }
                                Ok(spaghetti_common::ipc_protocol::Response::Error {
                                    message,
                                }) => {
                                    state_clone.write().await.set_notification(
                                        format!("Error: {message}"),
                                        NotificationKind::Error,
                                        3,
                                    );
                                }
                                Ok(_) | Err(_) => {
                                    state_clone.write().await.set_notification(
                                        "Failed to destroy device".to_string(),
                                        NotificationKind::Error,
                                        3,
                                    );
                                }
                            }
                        }
                        Err(_) => {
                            state_clone.write().await.set_notification(
                                "Cannot connect to daemon".to_string(),
                                NotificationKind::Error,
                                3,
                            );
                        }
                    }
                });
            } else {
                state.write().await.devices_screen.armed_destroy =
                    Some((device_uid, std::time::Instant::now()));
            }
        }

        _ => {
            state.write().await.devices_screen.armed_destroy = None;
        }
    }
}

async fn handle_device_create_form_key(
    state: &Arc<RwLock<AppState>>,
    code: KeyCode,
    _modifiers: KeyModifiers,
) {
    if code == KeyCode::Esc {
        state.write().await.devices_screen.mode = DevicesMode::List;
        return;
    }

    match code {
        KeyCode::Tab => {
            let mut s = state.write().await;
            s.devices_screen.create_form.active_field = match s.devices_screen.create_form.active_field {
                DeviceField::Name => DeviceField::Channels,
                DeviceField::Channels => DeviceField::Name,
            };
        }
        KeyCode::BackTab => {
            let mut s = state.write().await;
            s.devices_screen.create_form.active_field = match s.devices_screen.create_form.active_field {
                DeviceField::Name => DeviceField::Channels,
                DeviceField::Channels => DeviceField::Name,
            };
        }

        // Text input for Name field; +/- adjust channels when on that field.
        KeyCode::Char(ch) if ch != '\t' => {
            let mut s = state.write().await;
            match s.devices_screen.create_form.active_field {
                DeviceField::Name => {
                    s.devices_screen.create_form.name.push(ch);
                }
                DeviceField::Channels => {
                    if ch == '+' {
                        let v = s.devices_screen.create_form.channels;
                        s.devices_screen.create_form.channels = (v + 1).min(64);
                    } else if ch == '-' {
                        let v = s.devices_screen.create_form.channels;
                        s.devices_screen.create_form.channels = v.saturating_sub(1).max(1);
                    }
                }
            }
        }

        KeyCode::Backspace => {
            let mut s = state.write().await;
            if s.devices_screen.create_form.active_field == DeviceField::Name {
                s.devices_screen.create_form.name.pop();
            }
        }

        KeyCode::Up => {
            let mut s = state.write().await;
            if s.devices_screen.create_form.active_field == DeviceField::Channels {
                let v = s.devices_screen.create_form.channels;
                s.devices_screen.create_form.channels = (v + 1).min(64);
            }
        }
        KeyCode::Down => {
            let mut s = state.write().await;
            if s.devices_screen.create_form.active_field == DeviceField::Channels {
                let v = s.devices_screen.create_form.channels;
                s.devices_screen.create_form.channels = v.saturating_sub(1).max(1);
            }
        }

        KeyCode::Enter => {
            let (name, channels) = {
                let s = state.read().await;
                (
                    s.devices_screen.create_form.name.clone(),
                    s.devices_screen.create_form.channels,
                )
            };

            if name.trim().is_empty() {
                state.write().await.set_notification(
                    "Name cannot be empty".to_string(),
                    NotificationKind::Error,
                    2,
                );
                return;
            }

            if guard_connected(state).await {
                return;
            }

            // Close the form immediately.
            state.write().await.devices_screen.mode = DevicesMode::List;

            let state_clone = Arc::clone(state);
            tokio::spawn(async move {
                match spaghetti_common::async_client::IpcClient::connect().await {
                    Ok(mut client) => {
                        match client
                            .send(spaghetti_common::ipc_protocol::Request::CreateDevice {
                                name: name.clone(),
                                channels: Some(channels),
                            })
                            .await
                        {
                            Ok(spaghetti_common::ipc_protocol::Response::DeviceCreated {
                                device_uid,
                            }) => {
                                state_clone.write().await.set_notification(
                                    format!("Created device {device_uid}"),
                                    NotificationKind::Info,
                                    3,
                                );
                            }
                            Ok(spaghetti_common::ipc_protocol::Response::Error { message }) => {
                                // Re-open form on error so user can retry.
                                let mut s = state_clone.write().await;
                                s.devices_screen.mode = DevicesMode::Creating;
                                s.devices_screen.create_form.name = name;
                                s.devices_screen.create_form.channels = channels;
                                s.set_notification(
                                    format!("Error: {message}"),
                                    NotificationKind::Error,
                                    4,
                                );
                            }
                            Ok(_) | Err(_) => {
                                state_clone.write().await.set_notification(
                                    "Failed to create device".to_string(),
                                    NotificationKind::Error,
                                    3,
                                );
                            }
                        }
                    }
                    Err(_) => {
                        state_clone.write().await.set_notification(
                            "Cannot connect to daemon".to_string(),
                            NotificationKind::Error,
                            3,
                        );
                    }
                }
            });
        }

        _ => {}
    }
}
