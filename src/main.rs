mod config;
mod ipc;
mod state;
mod ui;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use gtk4::glib;
use gtk4::prelude::*;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};

use config::Config;
use ipc::StateUpdate;
use state::MinimapState;
use ui::{create_layer_window, MinimapWidget};

const APP_ID: &str = "com.github.nirimap";

/// Debounce duration for config reloads in milliseconds
/// Prevents excessive reloads when config file is modified multiple times rapidly
const CONFIG_RELOAD_DEBOUNCE_MS: u64 = 500;

/// Backoff bounds for reconnecting to niri's event stream.
const IPC_RECONNECT_MIN: Duration = Duration::from_millis(250);
const IPC_RECONNECT_MAX: Duration = Duration::from_secs(30);

/// How long a connection must last before it counts as healthy and the
/// reconnect backoff resets.
const IPC_CONNECTION_STABLE: Duration = Duration::from_secs(30);

/// Messages for config reload
enum ConfigMessage {
    Reload,
}

fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    tracing::info!("Starting nirimap");

    // Load configuration
    let config = Config::load()?;
    tracing::info!("Loaded configuration from {:?}", Config::config_path());

    // Create GTK application
    let app = gtk4::Application::builder().application_id(APP_ID).build();

    // Wrap config in Rc<RefCell> for hot reload support
    let config = Rc::new(RefCell::new(config));
    let config_for_activate = config.clone();

    app.connect_activate(move |app| {
        if let Err(e) = activate(app, config_for_activate.clone()) {
            tracing::error!("Failed to activate application: {}", e);
        }
    });

    // Run the application
    let empty: Vec<String> = vec![];
    app.run_with_args(&empty);

    Ok(())
}

/// Stay subscribed to niri's event stream, reconnecting when it drops.
///
/// The event stream is the only source of minimap state. Letting the thread
/// exit on a disconnect leaves the overlay running but frozen on stale
/// windows, with nothing in the log to say why -- so retry instead, and say so
/// each time. `run_event_loop` re-fetches full state on every connection, so a
/// reconnect resyncs the UI on its own.
fn run_ipc_with_reconnect(tx: mpsc::Sender<StateUpdate>) {
    let mut backoff = IPC_RECONNECT_MIN;

    loop {
        let ui_gone = Arc::new(AtomicBool::new(false));
        let started = Instant::now();

        let result = {
            let tx = tx.clone();
            let ui_gone = ui_gone.clone();
            ipc::run_event_loop(move |update| {
                if tx.send(update).is_err() {
                    ui_gone.store(true, Ordering::Relaxed);
                }
            })
        };

        // The receiver is dropped when the UI shuts down; nothing left to feed.
        if ui_gone.load(Ordering::Relaxed) {
            tracing::info!("UI is gone, stopping IPC event loop");
            return;
        }

        match result {
            Ok(()) => tracing::warn!("Niri closed the event stream"),
            Err(e) => tracing::warn!("IPC event loop error: {}", e),
        }

        // A connection that survived a while was healthy; don't punish it with
        // the backoff a rapid reconnect loop has built up.
        if started.elapsed() >= IPC_CONNECTION_STABLE {
            backoff = IPC_RECONNECT_MIN;
        }

        tracing::info!("Reconnecting to niri in {:?}", backoff);
        thread::sleep(backoff);
        backoff = (backoff * 2).min(IPC_RECONNECT_MAX);
    }
}

fn activate(app: &gtk4::Application, config: Rc<RefCell<Config>>) -> Result<()> {
    let state = Rc::new(RefCell::new(MinimapState::new()));
    let minimaps = create_minimaps(app, config.clone(), state.clone())?;
    if minimaps.is_empty() {
        anyhow::bail!("No monitors with niri output names were found");
    }
    let minimaps = MinimapSet::new(app.clone(), config.clone(), state.clone(), minimaps);
    minimaps.watch_monitors()?;

    // Set up channel for state updates from IPC thread
    let (tx, rx) = mpsc::channel::<StateUpdate>();

    // Start IPC event loop in a background thread
    thread::spawn(move || run_ipc_with_reconnect(tx));

    // Set up channel for config reload messages
    let (config_tx, config_rx) = mpsc::channel::<ConfigMessage>();

    // Start file watcher in a background thread
    let config_path = Config::config_path();
    thread::spawn(move || {
        if let Err(e) = watch_config_file(config_path, config_tx) {
            tracing::error!("Config watcher error: {}", e);
        }
    });

    // Set up glib idle handler to process state updates and config reloads
    let minimaps_clone = minimaps.clone();
    let last_config_reload = Rc::new(RefCell::new(Instant::now()));
    let config_reload_debounce = Duration::from_millis(CONFIG_RELOAD_DEBOUNCE_MS);

    glib::timeout_add_local(Duration::from_millis(50), move || {
        // Process a batch of state updates
        for _ in 0..10 {
            if let Ok(update) = rx.try_recv() {
                let list = minimaps_clone.widgets();
                apply_state_update(&minimaps_clone.state, &list, update);
            } else {
                break;
            }
        }

        // Process config reload messages with debouncing
        while let Ok(ConfigMessage::Reload) = config_rx.try_recv() {
            let now = Instant::now();
            let mut last_reload = last_config_reload.borrow_mut();

            // Only reload if enough time has passed since the last reload
            if now.duration_since(*last_reload) >= config_reload_debounce {
                for minimap in minimaps_clone.widgets().iter() {
                    minimap.reload_config();
                }
                *last_reload = now;
            } else {
                tracing::debug!("Config reload debounced (too soon after last reload)");
            }
        }

        glib::ControlFlow::Continue
    });

    // Keep the GTK/layer-shell process warm, but hide its surfaces until
    // Overview opens when overview-only mode is enabled.
    if config.borrow().behavior.overview_only || !config.borrow().behavior.always_visible {
        for minimap in minimaps.widgets().iter() {
            minimap.hide();
        }
    }

    tracing::info!(
        "Created {} nirimap monitor windows",
        minimaps.widgets().len()
    );

    Ok(())
}

/// Every monitor GDK currently knows about, paired with its connector name.
///
/// The name is `None` for a monitor GDK has added but not yet named -- the
/// normal state for the first moment after an output is plugged in or
/// re-enabled. Callers decide whether to wait for it or skip it.
fn live_monitors() -> Result<Vec<(Option<String>, gtk4::gdk::Monitor)>> {
    let display = gtk4::gdk::Display::default().context("Could not get default display")?;
    let monitors = display.monitors();
    let mut live = Vec::new();

    for index in 0..monitors.n_items() {
        let Some(object) = monitors.item(index) else {
            continue;
        };
        let Ok(monitor) = object.downcast::<gtk4::gdk::Monitor>() else {
            tracing::warn!("Display monitor {} is not a GDK monitor, skipping", index);
            continue;
        };
        let output = monitor.connector().map(|connector| connector.to_string());
        live.push((output, monitor));
    }

    Ok(live)
}

/// Build one minimap layer window bound to a single monitor.
fn create_minimap_for_monitor(
    app: &gtk4::Application,
    config: &Rc<RefCell<Config>>,
    state: &Rc<RefCell<MinimapState>>,
    output: String,
    monitor: gtk4::gdk::Monitor,
) -> MinimapWidget {
    let window = create_layer_window(app, &config.borrow(), &monitor);
    let minimap = MinimapWidget::new(config.clone(), state.clone(), output, monitor);
    minimap.set_window(window.clone());
    window.set_child(Some(minimap.widget()));
    window.present();
    minimap
}

/// Create one minimap layer window for each monitor known to GDK.
fn create_minimaps(
    app: &gtk4::Application,
    config: Rc<RefCell<Config>>,
    state: Rc<RefCell<MinimapState>>,
) -> Result<Vec<MinimapWidget>> {
    let mut minimaps = Vec::new();

    for (index, (output, monitor)) in live_monitors()?.into_iter().enumerate() {
        let Some(output) = output else {
            tracing::warn!(
                "Skipping monitor {} because it has no connector name",
                index
            );
            continue;
        };
        tracing::info!("Created nirimap for output {}", output);
        minimaps.push(create_minimap_for_monitor(
            app, &config, &state, output, monitor,
        ));
    }

    Ok(minimaps)
}

/// The live set of per-monitor minimaps, kept in sync with GDK's monitor list.
///
/// Each minimap is pinned to one `gdk::Monitor`, and layer-shell binds a
/// surface to its output at creation time. Disabling an output invalidates its
/// monitor, so a surface can never be moved back onto it -- the minimap has to
/// be dropped and rebuilt against the new monitor when the output returns.
#[derive(Clone)]
struct MinimapSet {
    app: gtk4::Application,
    config: Rc<RefCell<Config>>,
    state: Rc<RefCell<MinimapState>>,
    list: Rc<RefCell<Vec<MinimapWidget>>>,
    /// Monitors that arrived without a connector name yet. GDK adds a monitor
    /// to the list before the Wayland output's name event lands, so the first
    /// reconcile after a hotplug usually cannot identify it.
    pending: Rc<RefCell<Vec<gtk4::gdk::Monitor>>>,
}

impl MinimapSet {
    fn new(
        app: gtk4::Application,
        config: Rc<RefCell<Config>>,
        state: Rc<RefCell<MinimapState>>,
        minimaps: Vec<MinimapWidget>,
    ) -> Self {
        Self {
            app,
            config,
            state,
            list: Rc::new(RefCell::new(minimaps)),
            pending: Rc::new(RefCell::new(Vec::new())),
        }
    }

    fn widgets(&self) -> std::cell::Ref<'_, Vec<MinimapWidget>> {
        self.list.borrow()
    }

    /// Reconcile whenever outputs are enabled or disabled.
    fn watch_monitors(&self) -> Result<()> {
        let display = gtk4::gdk::Display::default().context("Could not get default display")?;
        let this = self.clone();
        display
            .monitors()
            .connect_items_changed(move |_, position, removed, added| {
                tracing::info!(
                    "Monitor list changed at {} (-{} +{}), reconciling minimaps",
                    position,
                    removed,
                    added
                );
                this.reconcile();
            });
        Ok(())
    }

    /// Drop minimaps whose output went away, and build one for every output
    /// that doesn't have a live minimap yet.
    fn reconcile(&self) {
        let live = match live_monitors() {
            Ok(live) => live,
            Err(e) => {
                tracing::error!("Could not enumerate monitors: {}", e);
                return;
            }
        };

        // Collected while the list is borrowed, torn down after it is released
        // so GTK signals from destroy() can't re-enter a borrowed list.
        let mut stale = Vec::new();

        {
            let mut current = self.list.borrow_mut();

            current.retain(|minimap| {
                let still_present = live
                    .iter()
                    .any(|(output, _)| output.as_deref() == Some(minimap.output()));
                if !still_present {
                    stale.push(minimap.clone());
                }
                still_present
            });

            // A minimap created while Overview is open must come up visible.
            let overview_open = self.state.borrow().overview_open;

            for (output, monitor) in &live {
                let Some(output) = output else {
                    self.watch_for_connector(monitor);
                    continue;
                };
                if current.iter().any(|m| m.output() == output) {
                    continue;
                }

                let minimap = create_minimap_for_monitor(
                    &self.app,
                    &self.config,
                    &self.state,
                    output.clone(),
                    monitor.clone(),
                );
                minimap.set_overview_open(overview_open);
                minimap.refresh();

                tracing::info!("Created nirimap for output {}", output);
                current.push(minimap);
            }
        }

        for minimap in stale {
            tracing::info!("Removed nirimap for output {}", minimap.output());
            minimap.close();
        }
    }

    /// Reconcile again once a freshly-added monitor learns its connector name.
    ///
    /// `items-changed` fires as soon as GDK appends the monitor, which is
    /// before the compositor has sent the output's name. Reconciling only on
    /// that signal would silently skip every re-enabled output.
    fn watch_for_connector(&self, monitor: &gtk4::gdk::Monitor) {
        let mut pending = self.pending.borrow_mut();
        if pending.iter().any(|m| m == monitor) {
            return;
        }
        pending.push(monitor.clone());
        drop(pending);

        tracing::debug!("Monitor has no connector name yet, waiting for it");

        let this = self.clone();
        monitor.connect_connector_notify(move |monitor| {
            if monitor.connector().is_none() {
                return;
            }
            this.pending.borrow_mut().retain(|m| m != monitor);
            this.reconcile();
        });
    }
}

/// Watch the config file for changes and send reload messages
fn watch_config_file(
    config_path: std::path::PathBuf,
    tx: mpsc::Sender<ConfigMessage>,
) -> Result<()> {
    let (watcher_tx, watcher_rx) = mpsc::channel::<Result<Event, notify::Error>>();

    let mut watcher = RecommendedWatcher::new(
        move |res| {
            let _ = watcher_tx.send(res);
        },
        notify::Config::default(),
    )?;

    // Watch the config file's parent directory (to catch file replacements)
    if let Some(parent) = config_path.parent() {
        watcher.watch(parent, RecursiveMode::NonRecursive)?;
        tracing::info!("Watching config directory: {}", parent.display());
    }

    for event in watcher_rx {
        match event {
            Ok(event) => {
                // Check if the event is for our config file
                let is_config_event = event.paths.iter().any(|p| p == &config_path);

                if is_config_event {
                    use notify::EventKind;
                    match event.kind {
                        EventKind::Create(_) | EventKind::Modify(_) => {
                            tracing::debug!("Config file changed, triggering reload");
                            if tx.send(ConfigMessage::Reload).is_err() {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                tracing::warn!("File watcher error: {}", e);
            }
        }
    }

    Ok(())
}

/// Apply an update to the shared state, then redraw every minimap.
///
/// The state is updated directly rather than through a widget: the minimap
/// list is empty whenever every output is disabled, and dropping updates in
/// that window would leave the state stale for whichever monitor comes back.
fn update_shared_state<F>(state: &Rc<RefCell<MinimapState>>, minimaps: &[MinimapWidget], update: F)
where
    F: FnOnce(&mut MinimapState),
{
    update(&mut state.borrow_mut());
    for minimap in minimaps {
        minimap.refresh();
    }
}

fn show_all(minimaps: &[MinimapWidget]) {
    for minimap in minimaps {
        minimap.show();
    }
}

/// Apply a state update to every per-monitor minimap.
fn apply_state_update(
    state: &Rc<RefCell<MinimapState>>,
    minimaps: &[MinimapWidget],
    update: StateUpdate,
) {
    match update {
        StateUpdate::FullState(new_state) => {
            update_shared_state(state, minimaps, |state| {
                *state = new_state;
            });
            tracing::debug!("Applied full state update");
        }

        StateUpdate::WindowChanged {
            window,
            workspace_id,
        } => {
            let window_id = window.id;
            let is_focused = window.is_focused;
            let is_floating = window.is_floating;
            let mut is_new_window = false;
            let mut is_on_active_workspace = false;

            update_shared_state(state, minimaps, |state| {
                // If this window is focused, clear focus from all other windows first
                if is_focused {
                    state.set_focused_window(Some(window_id));
                }

                if let Some(ws_id) = workspace_id {
                    is_on_active_workspace = state.active_workspace_id == Some(ws_id);

                    // Remove from any other workspace (handles workspace moves)
                    for (&id, workspace) in state.workspaces.iter_mut() {
                        if id != ws_id {
                            workspace.windows.remove(&window_id);
                        }
                    }

                    // Insert into the correct workspace
                    let workspace = state
                        .workspaces
                        .entry(ws_id)
                        .or_insert_with(|| state::Workspace {
                            id: ws_id,
                            ..Default::default()
                        });
                    is_new_window = !workspace.windows.contains_key(&window_id);
                    workspace.windows.insert(window_id, window);
                } else {
                    for workspace in state.workspaces.values_mut() {
                        workspace.windows.remove(&window_id);
                    }
                }
            });

            // Only show the minimap for new windows on the active workspace.
            // Floating spawns are filtered by show_for_new_window when the
            // show_for_floating_windows opt-out is in effect.
            if is_on_active_workspace && is_new_window {
                for minimap in minimaps {
                    minimap.show_for_new_window(is_floating);
                }
                tracing::debug!(
                    "New window {} opened (focused: {}, floating: {})",
                    window_id,
                    is_focused,
                    is_floating
                );
            } else {
                tracing::debug!("Window {} updated (focused: {})", window_id, is_focused);
            }
        }

        StateUpdate::WindowClosed(window_id) => {
            update_shared_state(state, minimaps, |state| {
                state.remove_window(window_id);
            });
            tracing::debug!("Window {} closed", window_id);
        }

        StateUpdate::FocusChanged(window_id) => {
            update_shared_state(state, minimaps, |state| {
                state.set_focused_window(window_id);
            });
            // Show the minimap only if focus changed to a different window
            for minimap in minimaps {
                minimap.show_on_focus_change(window_id);
            }
            tracing::debug!("Focus changed to {:?}", window_id);
        }

        StateUpdate::OverviewChanged(is_open) => {
            update_shared_state(state, minimaps, |state| {
                state.overview_open = is_open;
            });
            for minimap in minimaps {
                minimap.set_overview_open(is_open);
            }
            tracing::debug!("Overview {}", if is_open { "opened" } else { "closed" });
        }

        StateUpdate::WorkspaceActivated { id, focused } => {
            if focused {
                update_shared_state(state, minimaps, |state| {
                    state.set_active_workspace(id);
                });
            } else {
                for minimap in minimaps {
                    minimap.refresh();
                }
            }
            // Show the minimap when workspace changes (will auto-hide if configured)
            show_all(minimaps);
            if focused {
                tracing::debug!("Workspace {} activated", id);
            }
        }

        StateUpdate::WorkspacesChanged(workspaces) => {
            update_shared_state(state, minimaps, |state| {
                state.replace_workspace_metadata(&workspaces);
            });
            tracing::debug!("Workspaces changed ({} total)", workspaces.len());
        }

        StateUpdate::WorkspaceActiveWindowChanged {
            workspace_id,
            active_window_id,
        } => {
            update_shared_state(state, minimaps, |state| {
                if let Some(ws) = state.workspaces.get_mut(&workspace_id) {
                    ws.active_window_id = active_window_id;
                }
            });
            tracing::debug!(
                "Workspace {} active window -> {:?}",
                workspace_id,
                active_window_id
            );
        }

        StateUpdate::LayoutsChanged(layouts) => {
            update_shared_state(state, minimaps, |state| {
                for (window_id, layout) in layouts {
                    // Find and update the window's layout
                    for workspace in state.workspaces.values_mut() {
                        if let Some(window) = workspace.windows.get_mut(&window_id) {
                            window.pos = layout.tile_pos_in_workspace_view;
                            window.size = layout.tile_size;
                            // Update floating status
                            window.is_floating = layout.pos_in_scrolling_layout.is_none();
                            if let Some((col, win_idx)) = layout.pos_in_scrolling_layout {
                                let (column_index, window_index) =
                                    ipc::validate_and_convert_indices(col, win_idx, window_id);
                                window.column_index = column_index;
                                window.window_index = window_index;
                            }
                        }
                    }
                }
            });
            // Show the minimap when layouts change (window resize, move, etc.)
            show_all(minimaps);
            tracing::debug!("Window layouts changed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_reload_debounce_constant() {
        // Verify the debounce constant is set to a reasonable value
        assert_eq!(CONFIG_RELOAD_DEBOUNCE_MS, 500);
    }

    #[test]
    fn test_debounce_logic_simulation() {
        // Simulate debouncing logic similar to what happens in activate()
        let debounce_duration = Duration::from_millis(CONFIG_RELOAD_DEBOUNCE_MS);
        let mut last_reload = Instant::now();

        // Wait a bit less than the debounce duration
        std::thread::sleep(Duration::from_millis(100));
        let now = Instant::now();

        // Should be debounced (too soon)
        assert!(now.duration_since(last_reload) < debounce_duration);

        // Wait past the debounce duration
        std::thread::sleep(Duration::from_millis(450)); // Total: 550ms > 500ms
        let now = Instant::now();

        // Should not be debounced (enough time has passed)
        assert!(now.duration_since(last_reload) >= debounce_duration);

        // Update last_reload
        last_reload = now;

        // Immediate reload attempt should be debounced
        let now = Instant::now();
        assert!(now.duration_since(last_reload) < debounce_duration);
    }

    #[test]
    fn test_debounce_edge_case_exact_boundary() {
        let debounce_duration = Duration::from_millis(CONFIG_RELOAD_DEBOUNCE_MS);
        let last_reload = Instant::now();

        // Sleep for exactly the debounce duration
        std::thread::sleep(debounce_duration);
        let now = Instant::now();

        // Should be >= debounce duration (edge case: exactly at boundary)
        assert!(now.duration_since(last_reload) >= debounce_duration);
    }

    #[test]
    fn test_debounce_multiple_rapid_events() {
        let debounce_duration = Duration::from_millis(CONFIG_RELOAD_DEBOUNCE_MS);
        let mut last_reload = Instant::now();
        let mut reload_count = 0;

        // Simulate 10 rapid events over 200ms (all within debounce window)
        for _ in 0..10 {
            std::thread::sleep(Duration::from_millis(20));
            let now = Instant::now();

            if now.duration_since(last_reload) >= debounce_duration {
                reload_count += 1;
                last_reload = now;
            }
        }

        // Only the first event should trigger a reload (200ms total < 500ms)
        assert_eq!(reload_count, 0);

        // Now wait long enough for the debounce to expire
        std::thread::sleep(Duration::from_millis(350)); // Total: 550ms > 500ms
        let now = Instant::now();

        if now.duration_since(last_reload) >= debounce_duration {
            reload_count += 1;
        }

        // Now we should get a reload
        assert_eq!(reload_count, 1);
    }

    fn test_ipc_window_layout(col: usize, win_idx: usize, w: f64, h: f64) -> niri_ipc::WindowLayout {
        niri_ipc::WindowLayout {
            pos_in_scrolling_layout: Some((col, win_idx)),
            tile_size: (w, h),
            window_size: (w as i32, h as i32),
            tile_pos_in_workspace_view: None,
            window_offset_in_tile: (0.0, 0.0),
        }
    }

    #[test]
    fn test_layouts_changed_applied_during_overview() {
        let state = Rc::new(RefCell::new(MinimapState::new()));
        let minimaps: Vec<MinimapWidget> = Vec::new();

        // Add two windows in workspace 1, column 1 (1-based from niri)
        let win1 = state::Window {
            id: 1,
            pos: None,
            size: (100.0, 50.0),
            column_index: 0,
            window_index: 0,
            is_focused: false,
            is_floating: false,
            title: None,
            app_id: None,
        };
        let win2 = state::Window {
            id: 2,
            pos: None,
            size: (100.0, 50.0),
            column_index: 0,
            window_index: 1,
            is_focused: false,
            is_floating: false,
            title: None,
            app_id: None,
        };
        apply_state_update(
            &state,
            &minimaps,
            StateUpdate::WindowChanged {
                window: win1,
                workspace_id: Some(1),
            },
        );
        apply_state_update(
            &state,
            &minimaps,
            StateUpdate::WindowChanged {
                window: win2,
                workspace_id: Some(1),
            },
        );

        // Enter Overview mode
        apply_state_update(&state, &minimaps, StateUpdate::OverviewChanged(true));
        assert!(state.borrow().overview_open);

        // Move win2 UP and win1 DOWN (swap vertical positions in column) while in Overview
        let layout_win1 = test_ipc_window_layout(1, 2, 100.0, 60.0);
        let layout_win2 = test_ipc_window_layout(1, 1, 100.0, 40.0);
        apply_state_update(
            &state,
            &minimaps,
            StateUpdate::LayoutsChanged(vec![(1, layout_win1), (2, layout_win2)]),
        );

        let ws = state.borrow().workspaces.get(&1).cloned().unwrap();
        let w1 = ws.windows.get(&1).unwrap();
        let w2 = ws.windows.get(&2).unwrap();

        // win1 should now be at window_index 1, win2 at window_index 0
        assert_eq!(w1.column_index, 0);
        assert_eq!(w1.window_index, 1);
        assert_eq!(w1.size, (100.0, 60.0));

        assert_eq!(w2.column_index, 0);
        assert_eq!(w2.window_index, 0);
        assert_eq!(w2.size, (100.0, 40.0));
    }

    #[test]
    fn test_window_changed_workspace_move_and_none() {
        let state = Rc::new(RefCell::new(MinimapState::new()));
        let minimaps: Vec<MinimapWidget> = Vec::new();

        let win = state::Window {
            id: 10,
            pos: None,
            size: (100.0, 100.0),
            column_index: 0,
            window_index: 0,
            is_focused: false,
            is_floating: false,
            title: None,
            app_id: None,
        };

        // Window appears in workspace 1
        apply_state_update(
            &state,
            &minimaps,
            StateUpdate::WindowChanged {
                window: win.clone(),
                workspace_id: Some(1),
            },
        );
        assert!(state.borrow().workspaces.get(&1).unwrap().windows.contains_key(&10));

        // Window moves to workspace 2
        apply_state_update(
            &state,
            &minimaps,
            StateUpdate::WindowChanged {
                window: win.clone(),
                workspace_id: Some(2),
            },
        );
        assert!(!state.borrow().workspaces.get(&1).unwrap().windows.contains_key(&10));
        assert!(state.borrow().workspaces.get(&2).unwrap().windows.contains_key(&10));

        // Window workspace becomes None (unmapped/removed)
        apply_state_update(
            &state,
            &minimaps,
            StateUpdate::WindowChanged {
                window: win,
                workspace_id: None,
            },
        );
        assert!(!state.borrow().workspaces.get(&2).unwrap().windows.contains_key(&10));
    }
}
