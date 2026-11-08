//! External Dear ImGui overlay for X11, rendered with Vulkan. Input comes from /dev/input.
//!
//! - transparent, borderless, always-on-top window covering the monitor
//! - never takes focus and is fully click-through (the X server never sends it input)
//! - needs root for /dev/input: when started as a normal user (e.g. double-clicked) it asks
//!   for the password through the desktop's polkit dialog and relaunches itself (`elevate.rs`)
//! - ALL input (hotkeys, mouse, wheel) is read from `/dev/input/event*` (evdev). Mice and
//!   touchpads are grabbed and re-injected through uinput, so clicks on the overlay's own
//!   windows are swallowed and never reach the apps underneath (works on X11 and Wayland)
//! - pointer position: exact on a real X11 session (read from the X server), dead-reckoned
//!   from the evdev deltas on Wayland (see `Pointer`)
//! - shows the currently focused window (title, class, pid, geometry)
//! - Insert toggles the menu, End quits (global hotkeys, work while other apps have focus)
//!
//! Env vars:
//!   OVERLAY_SCALE=1.5            font scale (HiDPI)
//!   OVERLAY_OVERRIDE_REDIRECT=1  bypass the window manager (stays above more things)
//!   OVERLAY_POINTER_SPEED=1.0    multiplier for the dead-reckoned pointer (mouse and touchpad)
//!   OVERLAY_POINTER_SYNC=0|1     force the X server position sync off/on (default: on for X11)
//!   OVERLAY_NO_GRAB=1            don't grab mice (clicks then also reach the apps below)

mod elevate;
mod fonts;
mod theme;
mod evdev;
mod focus;
mod vulkan;
mod x11util;

use std::ffi::CString;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as _, Result};
use imgui::{sys, Condition, Context, MouseButton, StyleColor, Ui, WindowFlags};
use imgui_rs_vulkan_renderer::{Options, Renderer};
use raw_window_handle::{HasRawWindowHandle, RawWindowHandle};
use winit::{
    dpi::{PhysicalPosition, PhysicalSize},
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
    platform::x11::{EventLoopBuilderExtX11, WindowBuilderExtX11, XWindowType},
    window::{Window, WindowBuilder, WindowLevel},
};

use evdev::{Button, Input, InputEvent, KEY_END, KEY_INSERT};
use focus::{FocusInfo, FocusWatcher};
use vulkan::Vulkan;
use x11util::{InputShape, PointerQuery};

const FOCUS_POLL_INTERVAL: Duration = Duration::from_millis(100);
const HIDDEN_FRAME_INTERVAL: Duration = Duration::from_millis(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hotkey {
    ToggleMenu, // Insert
    Quit,       // End
}

/// Software mouse pointer in overlay-window pixels.
///
/// evdev reports *motion*, not where the compositor's cursor is. So we dead-reckon: start in
/// the middle of the screen and integrate the deltas. Raw evdev deltas carry no pointer
/// acceleration, so with an accelerating desktop the tracked position slowly drifts from
/// the real cursor (a flat acceleration profile makes them match). Absolute devices
/// (touchscreens, tablets, VM tablets) have no such problem.
struct Pointer {
    pos: [f32; 2],
    speed: f32,
}

impl Pointer {
    fn new(size: PhysicalSize<u32>) -> Self {
        let speed = std::env::var("OVERLAY_POINTER_SPEED")
            .ok()
            .and_then(|s| s.parse::<f32>().ok())
            .unwrap_or(1.0);
        Self {
            pos: [size.width as f32 / 2.0, size.height as f32 / 2.0],
            speed,
        }
    }

    fn clamp(&mut self, size: PhysicalSize<u32>) {
        self.pos[0] = self.pos[0].clamp(0.0, (size.width as f32 - 1.0).max(0.0));
        self.pos[1] = self.pos[1].clamp(0.0, (size.height as f32 - 1.0).max(0.0));
    }
}

/// State shared with the UI code.
struct UiState {
    fonts: fonts::Fonts,
    panel_alpha: f32,
    quit: bool,
}

struct App {
    // NOTE: `window` must outlive `vk` (the surface references it).
    window: Window,
    vk: Vulkan,
    imgui: Context,
    renderer: Option<Renderer>,
    watcher: FocusWatcher,
    input: Input,
    pointer: Pointer,
    /// Real pointer position from the X server (X11 sessions only).
    pointer_sync: Option<PointerQuery>,
    input_shape: Option<InputShape>,
    visible: bool,
    ui_state: UiState,
    focus: Option<FocusInfo>,
    last_poll: Instant,
    last_frame: Instant,
    needs_recreate: bool,
}

impl App {
    fn new(window: Window, xid: Option<u32>) -> Result<Self> {
        let vk = unsafe { Vulkan::new(&window)? };

        let mut imgui = Context::create();
        imgui.set_ini_filename(None);
        imgui.set_log_filename(None);
        theme::apply_forest_green(imgui.style_mut());
        let ui_fonts = fonts::load(&mut imgui); // before the renderer builds the atlas
        if let Some(scale) = std::env::var("OVERLAY_SCALE")
            .ok()
            .and_then(|s| s.parse::<f32>().ok())
        {
            imgui.io_mut().font_global_scale = scale;
        }

        let renderer = Renderer::with_default_allocator(
            &vk.instance,
            vk.physical_device,
            vk.device.clone(),
            vk.queue,
            vk.command_pool,
            vk.render_pass,
            &mut imgui,
            Some(Options {
                in_flight_frames: 1,
                ..Default::default()
            }),
        )
        .map_err(|e| anyhow!("failed to create imgui renderer: {e:?}"))?;

        let watcher = FocusWatcher::new().context("connecting to the X server")?;
        // All input comes from /dev/input; needs root (or the `input` group).
        let input = Input::new();
        if input.device_count() == 0 {
            eprintln!("warning: no input devices - hotkeys and mouse will not work");
        }
        let pointer = Pointer::new(window.inner_size());
        let pointer_sync = match std::env::var("OVERLAY_POINTER_SYNC").ok().as_deref() {
            Some("0") => false,
            Some(_) => true,
            None => !is_wayland_session(),
        };
        let pointer_sync = xid
            .filter(|_| pointer_sync)
            .and_then(|id| PointerQuery::new(id).ok());
        if pointer_sync.is_none() {
            eprintln!("note: pointer position is tracked from evdev deltas (may drift with pointer acceleration)");
        }

        // Click-through: the overlay window gets an EMPTY input region for good. Clicks go to
        // the windows underneath; we see them through evdev instead.
        let input_shape = xid.and_then(|id| match InputShape::new(id) {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!("warning: could not set X input shape: {e:#}");
                None
            }
        });
        if input_shape.is_none() {
            // Fallback: fully click-through via winit (no interactive windows possible).
            let _ = window.set_cursor_hittest(false);
        }

        Ok(Self {
            window,
            vk,
            imgui,
            renderer: Some(renderer),
            watcher,
            input,
            pointer,
            pointer_sync,
            input_shape,
            visible: true,
            ui_state: UiState {
                fonts: ui_fonts,
                panel_alpha: 0.88,
                quit: false,
            },
            focus: None,
            last_poll: Instant::now() - FOCUS_POLL_INTERVAL,
            last_frame: Instant::now(),
            needs_recreate: false,
        })
    }

    /// Feed one evdev event into the pointer / ImGui. Returns a hotkey if it was one.
    fn handle_input(&mut self, ev: InputEvent) -> Option<Hotkey> {
        let size = self.window.inner_size();
        let speed = self.pointer.speed;
        let mut moved = false;

        match ev {
            InputEvent::Key { code, pressed: true } => match code {
                KEY_INSERT => return Some(Hotkey::ToggleMenu),
                KEY_END => return Some(Hotkey::Quit),
                _ => {}
            },
            InputEvent::Key { .. } => {}
            InputEvent::MouseMove { dx, dy } => {
                self.pointer.pos[0] += dx * speed;
                self.pointer.pos[1] += dy * speed;
                moved = true;
            }
            InputEvent::PadMove { dx, dy } => {
                // A full swipe across the pad moves the pointer ~60% of the screen width.
                let k = size.width as f32 * 0.6 * speed;
                self.pointer.pos[0] += dx * k;
                self.pointer.pos[1] += dy * k;
                moved = true;
            }
            InputEvent::Absolute { x, y } => {
                self.pointer.pos = [x * size.width as f32, y * size.height as f32];
                moved = true;
            }
            InputEvent::Button { button, pressed } => {
                if self.visible {
                    let b = match button {
                        Button::Left => MouseButton::Left,
                        Button::Right => MouseButton::Right,
                        Button::Middle => MouseButton::Middle,
                        Button::Back => MouseButton::Extra1,
                        Button::Forward => MouseButton::Extra2,
                    };
                    self.imgui.io_mut().add_mouse_button_event(b, pressed);
                }
            }
            InputEvent::Wheel { x, y } => {
                if self.visible {
                    self.imgui.io_mut().add_mouse_wheel_event([x, y]);
                }
            }
        }

        if moved {
            self.pointer.clamp(size);
            self.send_pointer();
        }
        None
    }

    /// Tell ImGui where the pointer is (or that there is none while the menu is hidden).
    fn send_pointer(&mut self) {
        let p = if self.visible {
            self.pointer.pos
        } else {
            [-f32::MAX, -f32::MAX]
        };
        self.imgui.io_mut().add_mouse_pos_event(p);
    }

    fn toggle_menu(&mut self) {
        self.visible = !self.visible;
        if !self.visible {
            // Don't leave a button stuck down in ImGui.
            let io = self.imgui.io_mut();
            for b in [
                MouseButton::Left,
                MouseButton::Right,
                MouseButton::Middle,
                MouseButton::Extra1,
                MouseButton::Extra2,
            ] {
                io.add_mouse_button_event(b, false);
            }
        }
        self.send_pointer();
    }

    fn draw_frame(&mut self) -> Result<()> {
        let size = self.window.inner_size();
        if size.width == 0 || size.height == 0 {
            return Ok(());
        }
        if self.needs_recreate {
            unsafe { self.vk.recreate_swapchain(size)? };
            self.needs_recreate = false;
        }

        // Poll the focused window ~10x per second.
        if self.visible && self.last_poll.elapsed() >= FOCUS_POLL_INTERVAL {
            self.focus = self.watcher.query();
            self.last_poll = Instant::now();
        }

        // Exact pointer position straight from the X server (X11 sessions).
        if self.visible {
            if let Some(p) = self.pointer_sync.as_ref().and_then(|q| q.position()) {
                self.pointer.pos = p;
                self.pointer.clamp(size);
                let pos = self.pointer.pos;
                self.imgui.io_mut().add_mouse_pos_event(pos);
            }
        }

        // Per-frame ImGui IO.
        let now = Instant::now();
        let io = self.imgui.io_mut();
        io.update_delta_time((now - self.last_frame).max(Duration::from_micros(100)));
        io.display_size = [size.width as f32, size.height as f32];
        self.last_frame = now;
        let fps = io.framerate;
        // Pointer over one of our interactive windows: swallow buttons/wheel (see evdev.rs).
        let capture = self.visible && io.want_capture_mouse;
        self.input.set_capture(capture);
        // Without the exact X position our cursor may be off from the real one, so draw
        // ImGui's own cursor while hovering - it shows what a click will hit.
        io.mouse_draw_cursor = capture && self.pointer_sync.is_none();

        let imgui = &mut self.imgui;
        let renderer = self.renderer.as_mut().expect("renderer alive");
        let focus = self.focus.as_ref();
        let visible = self.visible;
        let ui_state = &mut self.ui_state;

        let out_of_date = unsafe {
            self.vk.render(|cmd| {
                let ui = imgui.new_frame();
                if visible {
                    build_ui(ui, ui_state, focus, fps);
                }
                let draw_data = imgui.render();
                renderer
                    .cmd_draw(cmd, draw_data)
                    .map_err(|e| anyhow!("imgui draw failed: {e:?}"))?;
                Ok(())
            })?
        };
        if out_of_date {
            self.needs_recreate = true;
        }

        // The whole overlay is click-through (cheap: only touches X about once a second).
        if let Some(shape) = self.input_shape.as_mut() {
            shape.set_regions(&[]);
        }
        Ok(())
    }

    unsafe fn destroy(&mut self) {
        self.vk.wait_idle();
        // The renderer owns Vulkan objects; drop it before the device goes away.
        drop(self.renderer.take());
        self.vk.destroy();
    }
}

/// Window helper: like `ui.window(name).build(body)` with raw flags (needed for NO_INPUTS).
fn overlay_window(
    ui: &Ui,
    name: &str,
    pos: [f32; 2],
    bg_alpha: f32,
    flags: WindowFlags,
    cond: Condition,
    body: impl FnOnce(),
) {
    let _ = ui;
    let name = CString::new(name).expect("window name must not contain NUL");
    unsafe {
        sys::igSetNextWindowPos(
            sys::ImVec2 { x: pos[0], y: pos[1] },
            cond as sys::ImGuiCond,
            sys::ImVec2 { x: 0.0, y: 0.0 },
        );
        sys::igSetNextWindowBgAlpha(bg_alpha);
        let visible = sys::igBegin(
            name.as_ptr(),
            std::ptr::null_mut(),
            flags.bits() as sys::ImGuiWindowFlags,
        );
        if visible {
            body();
        }
        sys::igEnd();
    }
}

fn is_wayland_session() -> bool {
    std::env::var("XDG_SESSION_TYPE").map_or(false, |t| t == "wayland")
        || std::env::var_os("WAYLAND_DISPLAY").is_some()
}

fn build_ui(ui: &Ui, state: &mut UiState, focus: Option<&FocusInfo>, fps: f32) {
    // --- Info panel: display only ---------------------------------------------------------
    // NO_INPUTS: ImGui ignores the mouse here, so clicks pass through to the apps below.
    overlay_window(
        ui,
        "##overlay",
        [20.0, 20.0],
        state.panel_alpha,
        WindowFlags::NO_INPUTS
            | WindowFlags::NO_TITLE_BAR
            | WindowFlags::NO_RESIZE
            | WindowFlags::NO_MOVE
            | WindowFlags::NO_COLLAPSE
            | WindowFlags::ALWAYS_AUTO_RESIZE
            | WindowFlags::NO_SAVED_SETTINGS
            | WindowFlags::NO_FOCUS_ON_APPEARING,
        Condition::Always,
        || {
            {
                let _bold = ui.push_font(state.fonts.bold);
                ui.text_colored(theme::ACCENT, "X11 ImGui Overlay (Vulkan)");
            }
            ui.text_disabled(format!("{fps:.0} FPS"));
            ui.separator();

            match focus {
                Some(f) => {
                    {
                        let _bold = ui.push_font(state.fonts.bold);
                        ui.text_colored(theme::ACCENT_BRIGHT, "Focused window");
                    }
                    let _mono = ui.push_font(state.fonts.mono);
                    ui.text(format!("Title:    {}", truncate(&f.title, 80)));
                    ui.text(format!("Class:    {} / {}", f.wm_instance, f.wm_class));
                    ui.text(format!("XID:      0x{:08x}", f.window_id));
                    match (f.pid, f.process.as_deref()) {
                        (Some(pid), Some(name)) => ui.text(format!("Process:  {name} ({pid})")),
                        (Some(pid), None) => ui.text(format!("PID:      {pid}")),
                        _ => ui.text("Process:  unknown"),
                    }
                    ui.text(format!(
                        "Geometry: {}x{} @ {},{}",
                        f.width, f.height, f.x, f.y
                    ));
                }
                None => ui.text("No focused window"),
            }
            ui.separator();
            ui.text_disabled("Insert: hide menu    End: quit");
        },
    );

    // --- Example interactive window: draggable, collapsible, driven by the evdev pointer --
    // Clicks on this window are swallowed (evdev grab), they don't reach the apps below.
    ui.window("Controls")
        .position([20.0, 280.0], Condition::FirstUseEver)
        .bg_alpha(0.90)
        .always_auto_resize(true)
        .save_settings(false)
        .focus_on_appearing(false)
        .build(|| {
            ui.slider("Panel opacity", 0.1, 1.0, &mut state.panel_alpha);
            // red "danger" button, like the delete buttons in the reference theme
            let _b = ui.push_style_color(StyleColor::Button, [0.45, 0.14, 0.14, 1.0]);
            let _bh = ui.push_style_color(StyleColor::ButtonHovered, [0.60, 0.20, 0.20, 1.0]);
            let _ba = ui.push_style_color(StyleColor::ButtonActive, [0.72, 0.26, 0.26, 1.0]);
            if ui.button("Quit overlay") {
                state.quit = true;
            }
        });
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

fn main() -> Result<()> {
    // No terminal (double-clicked)? Keep messages in /tmp/imgui-x11-overlay-<uid>.log.
    elevate::redirect_output_if_no_tty();
    // Not root (e.g. double-clicked)? Ask for authentication and relaunch as root.
    elevate::ensure_root()?;

    // Force the X11 backend (works under XWayland too). Under sudo, DISPLAY and XAUTHORITY
    // must survive: use `sudo -E` (or `sudo env DISPLAY=$DISPLAY XAUTHORITY=$XAUTHORITY ...`).
    let event_loop = EventLoopBuilder::new()
        .with_x11()
        .build()
        .context("creating X11 event loop (is DISPLAY set?)")?;

    // Cover the primary monitor.
    let (pos, size) = event_loop
        .primary_monitor()
        .map(|m| (m.position(), m.size()))
        .unwrap_or((PhysicalPosition::new(0, 0), PhysicalSize::new(1920, 1080)));

    let mut builder = WindowBuilder::new()
        .with_title("imgui-overlay")
        .with_transparent(true) // picks an ARGB visual on X11
        .with_decorations(false)
        .with_resizable(false)
        .with_active(false) // don't steal focus
        .with_window_level(WindowLevel::AlwaysOnTop)
        .with_position(pos)
        .with_inner_size(size)
        .with_x11_window_type(vec![XWindowType::Notification])
        .with_name("imgui-overlay", "imgui-overlay");
    if std::env::var_os("OVERLAY_OVERRIDE_REDIRECT").is_some() {
        builder = builder.with_override_redirect(true);
    }
    let window = builder.build(&event_loop)?;

    // Never take focus / keep above everything / hide from taskbar.
    let xid = match window.raw_window_handle() {
        RawWindowHandle::Xlib(h) => Some(h.window as u32),
        RawWindowHandle::Xcb(h) => Some(h.window),
        _ => None,
    };
    match xid {
        Some(id) => {
            if let Err(e) = x11util::apply_overlay_hints(id) {
                eprintln!("warning: could not apply X11 window hints: {e:#}");
            }
        }
        None => eprintln!("warning: not an X11 window, hints not applied"),
    }

    let mut app = App::new(window, xid)?;

    event_loop.run(move |event, elwt| {
        elwt.set_control_flow(ControlFlow::Poll);
        match event {
            Event::WindowEvent { event, .. } => {
                match event {
                    WindowEvent::CloseRequested => elwt.exit(),
                    WindowEvent::Resized(_) => app.needs_recreate = true,
                    WindowEvent::RedrawRequested => {
                        if let Err(e) = app.draw_frame() {
                            eprintln!("render error: {e:?}");
                            elwt.exit();
                        }
                    }
                    _ => {}
                }
            }
            Event::AboutToWait => {
                for ev in app.input.poll() {
                    match app.handle_input(ev) {
                        Some(Hotkey::ToggleMenu) => app.toggle_menu(),
                        Some(Hotkey::Quit) => elwt.exit(),
                        None => {}
                    }
                }
                if app.ui_state.quit {
                    elwt.exit();
                }
                if !app.visible {
                    // Nothing to show: idle at ~30 fps (still polling the hotkeys).
                    elwt.set_control_flow(ControlFlow::WaitUntil(
                        Instant::now() + HIDDEN_FRAME_INTERVAL,
                    ));
                }
                app.window.request_redraw();
            }
            Event::LoopExiting => unsafe { app.destroy() },
            _ => {}
        }
    })?;

    // Vulkan objects were released in LoopExiting; make sure nothing keeps the process alive.
    std::process::exit(0);
}
