mod config;
mod ipc;
mod state;
mod ui;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;
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

fn activate(app: &gtk4::Application, config: Rc<RefCell<Config>>) -> Result<()> {
    let state = Rc::new(RefCell::new(MinimapState::new()));
    let minimaps = create_minimaps(app, config.clone(), state)?;
    if minimaps.is_empty() {
        anyhow::bail!("No monitors with niri output names were found");
    }
    let minimaps = Rc::new(minimaps);

    // Set up channel for state updates from IPC thread
    let (tx, rx) = mpsc::channel::<StateUpdate>();

    // Start IPC event loop in a background thread
    thread::spawn(move || {
        if let Err(e) = ipc::run_event_loop(move |update| {
            if tx.send(update).is_err() {
                tracing::warn!("Failed to send state update, receiver dropped");
            }
        }) {
            tracing::error!("IPC event loop error: {}", e);
        }
    });

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
                apply_state_update(&minimaps_clone, update);
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
                for minimap in minimaps_clone.iter() {
                    minimap.reload_config();
                }
                *last_reload = now;
            } else {
                tracing::debug!("Config reload debounced (too soon after last reload)");
            }
        }

        glib::ControlFlow::Continue
    });

    // Hide immediately if not always visible
    if !config.borrow().behavior.always_visible {
        for minimap in minimaps.iter() {
            minimap.hide();
        }
    }

    tracing::info!("Created {} nirimap monitor windows", minimaps.len());

    Ok(())
}

/// Create one minimap layer window for each monitor known to GDK.
fn create_minimaps(
    app: &gtk4::Application,
    config: Rc<RefCell<Config>>,
    state: Rc<RefCell<MinimapState>>,
) -> Result<Vec<MinimapWidget>> {
    let display = gtk4::gdk::Display::default().context("Could not get default display")?;
    let monitors = display.monitors();
    let mut minimaps = Vec::new();

    for index in 0..monitors.n_items() {
        let Some(object) = monitors.item(index) else {
            continue;
        };
        let monitor = object
            .downcast::<gtk4::gdk::Monitor>()
            .map_err(|_| anyhow::anyhow!("Display monitor is not a GDK monitor"))?;
        let Some(output) = monitor.connector().map(|connector| connector.to_string()) else {
            tracing::warn!(
                "Skipping monitor {} because it has no connector name",
                index
            );
            continue;
        };

        let window = create_layer_window(app, &config.borrow(), &monitor);
        let minimap = MinimapWidget::new(config.clone(), state.clone(), output.clone(), monitor);
        minimap.set_window(window.clone());
        window.set_child(Some(minimap.widget()));
        window.present();

        tracing::info!("Created nirimap for output {}", output);
        minimaps.push(minimap);
    }

    Ok(minimaps)
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

fn update_shared_state<F>(minimaps: &[MinimapWidget], update: F)
where
    F: FnOnce(&mut MinimapState),
{
    if let Some(first) = minimaps.first() {
        first.update_state(update);
        for minimap in &minimaps[1..] {
            minimap.refresh();
        }
    }
}

fn show_all(minimaps: &[MinimapWidget]) {
    for minimap in minimaps {
        minimap.show();
    }
}

/// Apply a state update to every per-monitor minimap.
fn apply_state_update(minimaps: &[MinimapWidget], update: StateUpdate) {
    match update {
        StateUpdate::FullState(new_state) => {
            update_shared_state(minimaps, |state| {
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

            update_shared_state(minimaps, |state| {
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
                        .or_insert_with(Default::default);
                    is_new_window = !workspace.windows.contains_key(&window_id);
                    workspace.windows.insert(window_id, window);
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
            update_shared_state(minimaps, |state| {
                state.remove_window(window_id);
            });
            tracing::debug!("Window {} closed", window_id);
        }

        StateUpdate::FocusChanged(window_id) => {
            if let Some(window_id) = window_id {
                // Niri reports no focused window while Overview owns focus.
                // Keep the last focused tile highlighted so the minimap still
                // shows where focus will return when Overview closes.
                update_shared_state(minimaps, |state| {
                    state.set_focused_window(Some(window_id));
                });

                // Show the minimap only if focus changed to a different window.
                for minimap in minimaps {
                    minimap.show_on_focus_change(Some(window_id));
                }
            } else {
                // Do not clear the last focused tile when Niri's Overview is
                // active; its focus is not a normal window focus.
                for minimap in minimaps {
                    minimap.refresh();
                }
            }
            tracing::debug!("Focus changed to {:?}", window_id);
        }

        StateUpdate::WorkspaceActivated { id, focused } => {
            if focused {
                update_shared_state(minimaps, |state| {
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
            update_shared_state(minimaps, |state| {
                state.replace_workspace_metadata(&workspaces);
            });
            tracing::debug!("Workspaces changed ({} total)", workspaces.len());
        }

        StateUpdate::WorkspaceActiveWindowChanged {
            workspace_id,
            active_window_id,
        } => {
            update_shared_state(minimaps, |state| {
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
            update_shared_state(minimaps, |state| {
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
}
