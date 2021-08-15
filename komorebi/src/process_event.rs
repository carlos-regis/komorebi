use std::fs::OpenOptions;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;

use color_eyre::eyre::ContextCompat;
use color_eyre::Result;
use crossbeam_channel::select;

use komorebi_core::OperationDirection;
use komorebi_core::Rect;
use komorebi_core::Sizing;

use crate::window_manager::WindowManager;
use crate::window_manager_event::WindowManagerEvent;
use crate::windows_api::WindowsApi;
use crate::HIDDEN_HWNDS;
use crate::MULTI_WINDOW_EXES;

#[tracing::instrument]
pub fn listen_for_events(wm: Arc<Mutex<WindowManager>>) {
    let receiver = wm.lock().unwrap().incoming_events.lock().unwrap().clone();

    thread::spawn(move || {
        tracing::info!("listening");
        loop {
            select! {
                recv(receiver) -> mut maybe_event => {
                    if let Ok(event) = maybe_event.as_mut() {
                        match wm.lock().unwrap().process_event(event) {
                            Ok(()) => {},
                            Err(error) => tracing::error!("{}", error)
                        }
                    }
                }
            }
        }
    });
}

impl WindowManager {
    #[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
    #[tracing::instrument(skip(self))]
    pub fn process_event(&mut self, event: &mut WindowManagerEvent) -> Result<()> {
        if self.is_paused {
            tracing::trace!("ignoring while paused");
            return Ok(());
        }

        // Make sure we have the most recently focused monitor from any event
        match event {
            WindowManagerEvent::FocusChange(_, window)
            | WindowManagerEvent::Show(_, window)
            | WindowManagerEvent::MoveResizeEnd(_, window) => {
                let monitor_idx = self
                    .monitor_idx_from_window(*window)
                    .context("there is no monitor associated with this window, it may have already been destroyed")?;

                self.focus_monitor(monitor_idx)?;
            }
            _ => {}
        }

        for (i, monitor) in self.monitors_mut().iter_mut().enumerate() {
            let work_area = *monitor.work_area_size();
            for (j, workspace) in monitor.workspaces_mut().iter_mut().enumerate() {
                let reaped_orphans = workspace.reap_orphans()?;
                if reaped_orphans.0 > 0 || reaped_orphans.1 > 0 {
                    workspace.update(&work_area)?;
                    tracing::info!(
                        "reaped {} orphan window(s) and {} orphaned container(s) on monitor: {}, workspace: {}",
                        reaped_orphans.0,
                        reaped_orphans.1,
                        i,
                        j
                    );
                }
            }
        }

        if matches!(event, WindowManagerEvent::MouseCapture(..)) {
            tracing::trace!("only reaping orphans for mouse capture event");
            return Ok(());
        }

        match event {
            WindowManagerEvent::Minimize(_, window) | WindowManagerEvent::Destroy(_, window) => {
                self.focused_workspace_mut()?.remove_window(window.hwnd)?;
                self.update_focused_workspace(false)?;
            }

            WindowManagerEvent::Hide(_, window) => {
                // Some major applications unfortunately send the HIDE signal when they are being
                // minimized or destroyed. Will have to keep updating this list.
                let common_multi_window_exes = MULTI_WINDOW_EXES.lock().unwrap();
                let programmatically_hidden_hwnds = HIDDEN_HWNDS.lock().unwrap();
                if (!window.is_window() || common_multi_window_exes.contains(&window.exe()?))
                    && !programmatically_hidden_hwnds.contains(&window.hwnd)
                {
                    self.focused_workspace_mut()?.remove_window(window.hwnd)?;
                    self.update_focused_workspace(false)?;
                }
            }
            WindowManagerEvent::FocusChange(_, window) => {
                let workspace = self.focused_workspace_mut()?;
                if workspace
                    .floating_windows()
                    .iter()
                    .any(|w| w.hwnd == window.hwnd)
                {
                    return Ok(());
                }

                self.focused_workspace_mut()?
                    .focus_container_by_window(window.hwnd)?;
            }
            WindowManagerEvent::Show(_, window) => {
                let mut switch_to = None;
                for (i, monitors) in self.monitors().iter().enumerate() {
                    for (j, workspace) in monitors.workspaces().iter().enumerate() {
                        if workspace.contains_window(window.hwnd) {
                            switch_to = Some((i, j));
                        }
                    }
                }

                if let Some(indices) = switch_to {
                    if self.focused_monitor_idx() != indices.0
                        && self
                            .focused_monitor()
                            .context("there is no monitor")?
                            .focused_workspace_idx()
                            != indices.1
                    {
                        self.focus_monitor(indices.0)?;
                        self.focus_workspace(indices.1)?;
                        return Ok(());
                    }
                }

                let workspace = self.focused_workspace_mut()?;

                if workspace.containers().is_empty() || !workspace.contains_window(window.hwnd) {
                    workspace.new_container_for_window(*window);
                    self.update_focused_workspace(false)?;
                }
            }
            WindowManagerEvent::MoveResizeEnd(_, window) => {
                let workspace = self.focused_workspace_mut()?;
                if workspace
                    .floating_windows()
                    .iter()
                    .any(|w| w.hwnd == window.hwnd)
                {
                    return Ok(());
                }

                let focused_idx = workspace.focused_container_idx();
                let old_position = *workspace
                    .latest_layout()
                    .get(focused_idx)
                    .context("there is no latest layout")?;
                let mut new_position = WindowsApi::window_rect(window.hwnd())?;

                // See Window.set_position() in window.rs for comments
                let border = Rect {
                    left: 12,
                    top: 0,
                    right: 24,
                    bottom: 12,
                };

                // Adjust for the invisible border
                new_position.left += border.left;
                new_position.top += border.top;
                new_position.right -= border.right;
                new_position.bottom -= border.bottom;

                let resize = Rect {
                    left: new_position.left - old_position.left,
                    top: new_position.top - old_position.top,
                    right: new_position.right - old_position.right,
                    bottom: new_position.bottom - old_position.bottom,
                };

                let is_move = resize.right == 0 && resize.bottom == 0;

                if is_move {
                    tracing::info!("moving with mouse");
                    match workspace.container_idx_from_current_point() {
                        Some(target_idx) => {
                            workspace.swap_containers(focused_idx, target_idx);
                            self.update_focused_workspace(false)?;
                        }
                        None => self.update_focused_workspace(true)?,
                    }
                } else {
                    tracing::info!("resizing with mouse");
                    let mut ops = vec![];

                    macro_rules! resize_op {
                        ($coordinate:expr, $comparator:tt, $direction:expr) => {{
                            let adjusted = $coordinate * 2;
                            let sizing = if adjusted $comparator 0 {
                                Sizing::Decrease
                            } else {
                                Sizing::Increase
                            };

                            ($direction, sizing, adjusted.abs())
                        }};
                    }

                    if resize.left != 0 {
                        ops.push(resize_op!(resize.left, >, OperationDirection::Left));
                    }

                    if resize.top != 0 {
                        ops.push(resize_op!(resize.top, >, OperationDirection::Up));
                    }

                    if resize.right != 0 && resize.left == 0 {
                        ops.push(resize_op!(resize.right, <, OperationDirection::Right));
                    }

                    if resize.bottom != 0 && resize.top == 0 {
                        ops.push(resize_op!(resize.bottom, <, OperationDirection::Down));
                    }

                    for (edge, sizing, step) in ops {
                        self.resize_window(edge, sizing, Option::from(step))?;
                    }

                    self.update_focused_workspace(false)?;
                }
            }
            WindowManagerEvent::MouseCapture(..) => {}
        };

        tracing::trace!("updating list of known hwnds");
        let mut known_hwnds = vec![];
        for monitor in self.monitors() {
            for workspace in monitor.workspaces() {
                for container in workspace.containers() {
                    for window in container.windows() {
                        known_hwnds.push(window.hwnd);
                    }
                }
            }
        }

        let mut hwnd_json = dirs::home_dir().context("there is no home directory")?;
        hwnd_json.push("komorebi.hwnd.json");
        let file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .create(true)
            .open(hwnd_json)?;

        serde_json::to_writer_pretty(&file, &known_hwnds)?;
        tracing::info!("processed: {}", event.window().to_string());
        Ok(())
    }
}