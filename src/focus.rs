//! Queries the currently focused top-level window through EWMH (`_NET_ACTIVE_WINDOW`).
//! Uses its own X connection, independent from the one winit owns.

use anyhow::Result;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{Atom, AtomEnum, ConnectionExt, Window};
use x11rb::rust_connection::RustConnection;

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        _NET_ACTIVE_WINDOW,
        _NET_WM_NAME,
        _NET_WM_PID,
        UTF8_STRING,
    }
}

#[derive(Debug, Clone)]
pub struct FocusInfo {
    pub window_id: u32,
    pub title: String,
    pub wm_instance: String,
    pub wm_class: String,
    pub pid: Option<u32>,
    pub process: Option<String>,
    /// Position in root-window (screen) coordinates.
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

pub struct FocusWatcher {
    conn: RustConnection,
    root: Window,
    atoms: Atoms,
}

impl FocusWatcher {
    pub fn new() -> Result<Self> {
        let (conn, screen_num) = RustConnection::connect(None)?;
        let root = conn.setup().roots[screen_num].root;
        let atoms = Atoms::new(&conn)?.reply()?;
        Ok(Self { conn, root, atoms })
    }

    /// Returns `None` if nothing is focused or the window vanished mid-query.
    pub fn query(&self) -> Option<FocusInfo> {
        self.try_query().ok().flatten()
    }

    fn prop(&self, win: Window, prop: Atom, ty: impl Into<Atom>) -> Result<Vec<u8>> {
        Ok(self
            .conn
            .get_property(false, win, prop, ty, 0, 1024)?
            .reply()?
            .value)
    }

    fn try_query(&self) -> Result<Option<FocusInfo>> {
        let reply = self
            .conn
            .get_property(
                false,
                self.root,
                self.atoms._NET_ACTIVE_WINDOW,
                AtomEnum::WINDOW,
                0,
                1,
            )?
            .reply()?;

        let win = match reply.value32().and_then(|mut it| it.next()) {
            Some(w) if w != 0 => w,
            _ => return Ok(None),
        };

        // Title: prefer UTF-8 _NET_WM_NAME, fall back to legacy WM_NAME.
        let mut title = String::from_utf8_lossy(&self.prop(
            win,
            self.atoms._NET_WM_NAME,
            self.atoms.UTF8_STRING,
        )?)
        .into_owned();
        if title.is_empty() {
            title = String::from_utf8_lossy(&self.prop(
                win,
                AtomEnum::WM_NAME.into(),
                AtomEnum::STRING,
            )?)
            .into_owned();
        }

        // WM_CLASS = "instance\0class\0"
        let class_raw = self.prop(win, AtomEnum::WM_CLASS.into(), AtomEnum::STRING)?;
        let mut parts = class_raw
            .split(|b| *b == 0)
            .map(|s| String::from_utf8_lossy(s).into_owned());
        let wm_instance = parts.next().unwrap_or_default();
        let wm_class = parts.next().unwrap_or_default();

        // PID (+ process name from /proc)
        let pid = self
            .conn
            .get_property(
                false,
                win,
                self.atoms._NET_WM_PID,
                AtomEnum::CARDINAL,
                0,
                1,
            )?
            .reply()?
            .value32()
            .and_then(|mut it| it.next());
        let process = pid.and_then(|p| {
            std::fs::read_to_string(format!("/proc/{p}/comm"))
                .ok()
                .map(|s| s.trim().to_string())
        });

        // Geometry in screen coordinates (get_geometry x/y is parent-relative
        // under reparenting window managers, so translate to the root).
        let geo = self.conn.get_geometry(win)?.reply()?;
        let pos = self
            .conn
            .translate_coordinates(win, self.root, 0, 0)?
            .reply()?;

        Ok(Some(FocusInfo {
            window_id: win,
            title,
            wm_instance,
            wm_class,
            pid,
            process,
            x: pos.dst_x as i32,
            y: pos.dst_y as i32,
            width: geo.width as u32,
            height: geo.height as u32,
        }))
    }
}
