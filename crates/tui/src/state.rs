use std::collections::{HashMap, HashSet};
use std::time::Instant;

use spaghetti_common::ipc_protocol::{
    ActivityInfo, AppInfo, DaemonStatus, DeviceInfo, RouteInfo, SystemDeviceInfo, TapInfo,
};

// ─── Devices screen ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceField {
    Name,
    Channels,
}

pub struct DeviceCreateForm {
    pub name: String,
    pub channels: u32,
    pub active_field: DeviceField,
}

impl DeviceCreateForm {
    pub fn new() -> Self {
        Self {
            name: String::new(),
            channels: 2,
            active_field: DeviceField::Name,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevicesMode {
    List,
    Creating,
}

pub struct DevicesScreenState {
    pub cursor: usize,
    pub mode: DevicesMode,
    /// Armed device UID + timestamp for 3-second double-press destroy.
    pub armed_destroy: Option<(String, Instant)>,
    pub create_form: DeviceCreateForm,
}

impl DevicesScreenState {
    pub fn new() -> Self {
        Self {
            cursor: 0,
            mode: DevicesMode::List,
            armed_destroy: None,
            create_form: DeviceCreateForm::new(),
        }
    }

    pub fn clear_armed_if_expired(&mut self) {
        if let Some((_, at)) = &self.armed_destroy
            && at.elapsed().as_secs() >= 3
        {
            self.armed_destroy = None;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Dashboard,
    Apps,
    Routes,
    Devices,
}

impl Screen {
    pub const ALL: &'static [Screen] =
        &[Screen::Dashboard, Screen::Apps, Screen::Routes, Screen::Devices];

    pub fn label(self) -> &'static str {
        match self {
            Screen::Dashboard => "Dashboard",
            Screen::Apps => "Apps",
            Screen::Routes => "Routes",
            Screen::Devices => "Devices",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionStatus {
    Connected,
    Disconnected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusedPane {
    Apps,
    Taps,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationKind {
    Info,
    Error,
}

pub struct Notification {
    pub message: String,
    pub kind: NotificationKind,
    pub expires_at: Instant,
}

/// Which field is active in the route create form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateField {
    Tap,
    Device,
    Volume,
}

impl CreateField {
    pub fn next(self) -> Self {
        match self {
            Self::Tap => Self::Device,
            Self::Device => Self::Volume,
            Self::Volume => Self::Tap,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Self::Tap => Self::Volume,
            Self::Device => Self::Tap,
            Self::Volume => Self::Device,
        }
    }
}

pub struct CreateForm {
    pub selected_tap_idx: usize,
    pub selected_device_idx: usize,
    pub volume: f32,
    pub active_field: CreateField,
}

impl CreateForm {
    pub fn new() -> Self {
        Self {
            selected_tap_idx: 0,
            selected_device_idx: 0,
            volume: 1.0,
            active_field: CreateField::Tap,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutesMode {
    List,
    Editing { route_id: u32 },
    Creating,
}

pub struct RoutesScreenState {
    pub cursor: usize,
    pub mode: RoutesMode,
    /// Armed route_id for double-press delete confirmation.
    pub armed_destroy: Option<(u32, Instant)>,
    /// Route IDs with an in-flight SetVolume request.
    pub inflight_set_volume: HashSet<u32>,
    pub create_form: CreateForm,
}

impl RoutesScreenState {
    pub fn new() -> Self {
        Self {
            cursor: 0,
            mode: RoutesMode::List,
            armed_destroy: None,
            inflight_set_volume: HashSet::new(),
            create_form: CreateForm::new(),
        }
    }

    pub fn clear_armed_if_expired(&mut self) {
        if let Some((_, at)) = self.armed_destroy
            && at.elapsed().as_secs() >= 3
        {
            self.armed_destroy = None;
        }
    }
}

/// Per-screen cursor and interaction state for the Apps & Taps screen.
pub struct AppsScreenState {
    pub focused: FocusedPane,
    pub app_cursor: usize,
    pub tap_cursor: usize,
    /// Armed tap_id for double-press delete confirmation.
    pub delete_armed: Option<(u32, Instant)>,
}

impl AppsScreenState {
    pub fn new() -> Self {
        Self {
            focused: FocusedPane::Apps,
            app_cursor: 0,
            tap_cursor: 0,
            delete_armed: None,
        }
    }

    /// Clear armed state if it expired or was cancelled.
    pub fn clear_armed_if_expired(&mut self) {
        if let Some((_, at)) = self.delete_armed
            && at.elapsed().as_secs() >= 3
        {
            self.delete_armed = None;
        }
    }
}

pub struct AppState {
    pub connection_status: ConnectionStatus,
    pub current_screen: Screen,
    pub show_help: bool,

    // Polled data — None until first successful fetch.
    pub daemon_status: Option<DaemonStatus>,
    /// None = not yet polled; Some(vec![]) = polled but empty.
    pub apps: Option<Vec<AppInfo>>,
    /// None = not yet polled; Some(vec![]) = polled but empty.
    pub taps: Option<Vec<TapInfo>>,
    /// None = not yet polled; Some(vec![]) = polled but empty.
    pub routes: Option<Vec<RouteInfo>>,
    /// None = not yet polled; Some(vec![]) = polled but empty.
    pub devices: Option<Vec<DeviceInfo>>,
    pub system_devices: Vec<SystemDeviceInfo>,
    pub activity: Option<ActivityInfo>,

    pub apps_screen: AppsScreenState,
    pub routes_screen: RoutesScreenState,
    pub devices_screen: DevicesScreenState,
    pub notification: Option<Notification>,
    /// Suppresses repeated identical daemon-rejection messages within 5 seconds.
    pub(crate) daemon_rejection_seen: HashMap<String, Instant>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            connection_status: ConnectionStatus::Disconnected,
            current_screen: Screen::Dashboard,
            show_help: false,
            daemon_status: None,
            apps: None,
            taps: None,
            routes: None,
            devices: None,
            system_devices: Vec::new(),
            activity: None,
            apps_screen: AppsScreenState::new(),
            routes_screen: RoutesScreenState::new(),
            devices_screen: DevicesScreenState::new(),
            notification: None,
            daemon_rejection_seen: HashMap::new(),
        }
    }

    pub fn clear_polled_data(&mut self) {
        self.daemon_status = None;
        self.apps = None;
        self.taps = None;
        self.routes = None;
        self.devices = None;
        self.system_devices.clear();
        self.activity = None;
    }

    pub fn set_notification(&mut self, message: String, kind: NotificationKind, secs: u64) {
        self.notification = Some(Notification {
            message,
            kind,
            expires_at: Instant::now() + std::time::Duration::from_secs(secs),
        });
    }

    pub fn expire_notification(&mut self) {
        if let Some(ref n) = self.notification
            && Instant::now() >= n.expires_at
        {
            self.notification = None;
        }
    }

    /// Returns true when any modal form is active (Create Route or Create Device popup).
    ///
    /// When true, all keys must be routed to the screen handler; global navigation
    /// keys (Tab, 1–4, q) are suppressed entirely so the user can edit the form
    /// without accidental screen switches. Volume-edit mode is intentionally NOT
    /// counted as modal — it is an inline overlay, not a focused form.
    pub fn is_modal_active(&self) -> bool {
        self.show_help
            || self.routes_screen.mode == RoutesMode::Creating
            || self.devices_screen.mode == DevicesMode::Creating
    }

    /// Show an error notification for a daemon rejection, suppressing repeats within 5 seconds.
    pub fn notify_daemon_rejection(&mut self, msg: String) {
        let now = Instant::now();
        let suppress = self
            .daemon_rejection_seen
            .get(&msg)
            .map(|t| now.duration_since(*t).as_secs() < 5)
            .unwrap_or(false);
        if !suppress {
            self.daemon_rejection_seen.insert(msg.clone(), now);
            self.set_notification(msg, NotificationKind::Error, 5);
        }
    }
}
