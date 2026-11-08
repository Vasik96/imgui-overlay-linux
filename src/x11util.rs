//! Small X11 helpers (window management only - all *input* now comes from /dev/input,
//! see `evdev.rs`):
//!  * `apply_overlay_hints`: make the window "no input" (WM never focuses it), sticky,
//!    above everything and hidden from taskbar/pager.
//!  * `InputShape`: keeps the overlay window completely click-through, so the clicks that
//!    evdev sees are also delivered to the windows underneath.

use anyhow::Result;
use x11rb::connection::Connection;
use std::time::{Duration, Instant};
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageEvent, ClipOrdering, ConnectionExt as _, EventMask, PropMode,
    Rectangle, Window,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

x11rb::atom_manager! {
    HintAtoms: HintAtomsCookie {
        _NET_WM_STATE,
        _NET_WM_STATE_ABOVE,
        _NET_WM_STATE_STICKY,
        _NET_WM_STATE_SKIP_TASKBAR,
        _NET_WM_STATE_SKIP_PAGER,
    }
}

pub fn apply_overlay_hints(xid: u32) -> Result<()> {
    let (conn, screen_num) = RustConnection::connect(None)?;
    let root = conn.setup().roots[screen_num].root;
    let a = HintAtoms::new(&conn)?.reply()?;

    // ICCCM "No Input" focus model: WM_HINTS with the Input flag set and input = False,
    // and no WM_TAKE_FOCUS => the window manager must never give this window focus.
    // WM_HINTS = [flags, input, initial_state, icon_pixmap, icon_window, icon_x, icon_y,
    //             icon_mask, window_group]; flags = 1 (InputHint).
    let wm_hints: [u32; 9] = [1, 0, 0, 0, 0, 0, 0, 0, 0];
    conn.change_property32(
        PropMode::REPLACE,
        xid,
        AtomEnum::WM_HINTS,
        AtomEnum::WM_HINTS,
        &wm_hints,
    )?;

    // EWMH state. Set the property (honoured if the window isn't mapped yet) and also ask the
    // WM via client messages (honoured if it already is).
    let states = [
        a._NET_WM_STATE_ABOVE,
        a._NET_WM_STATE_STICKY,
        a._NET_WM_STATE_SKIP_TASKBAR,
        a._NET_WM_STATE_SKIP_PAGER,
    ];
    conn.change_property32(
        PropMode::REPLACE,
        xid,
        a._NET_WM_STATE,
        AtomEnum::ATOM,
        &states,
    )?;
    for pair in [
        (a._NET_WM_STATE_ABOVE, a._NET_WM_STATE_STICKY),
        (a._NET_WM_STATE_SKIP_TASKBAR, a._NET_WM_STATE_SKIP_PAGER),
    ] {
        // data: [action (1 = add), first property, second property, source (1 = app), 0]
        let ev = ClientMessageEvent::new(32, xid, a._NET_WM_STATE, [1u32, pair.0, pair.1, 1, 0]);
        conn.send_event(
            false,
            root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            ev,
        )?;
    }
    conn.flush()?;
    Ok(())
}

/// Controls which parts of the overlay window receive mouse input, using the X Shape
/// extension's *input* shape. Everything outside the given rectangles is click-through:
/// the X server delivers those clicks to whatever window is underneath.
pub struct InputShape {
    conn: RustConnection,
    root: Window,
    client: Window,
    targets: Vec<Window>,
    last: Vec<Rectangle>,
    last_apply: Instant,
}

impl InputShape {
    pub fn new(xid: u32) -> Result<Self> {
        let (conn, screen_num) = RustConnection::connect(None)?;
        let root = conn.setup().roots[screen_num].root;
        let mut this = Self {
            conn,
            root,
            client: xid,
            targets: vec![xid],
            last: Vec::new(),
            last_apply: Instant::now(),
        };
        this.refresh_targets();
        this.apply(&[]); // fully click-through from the start
        Ok(this)
    }

    /// The client window plus any window-manager frame windows above it (below the root).
    /// A frame window would otherwise still catch clicks.
    fn refresh_targets(&mut self) {
        let mut targets = vec![self.client];
        let mut w = self.client;
        for _ in 0..8 {
            let parent = self
                .conn
                .query_tree(w)
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|t| t.parent);
            match parent {
                Some(p) if p != 0 && p != self.root => {
                    targets.push(p);
                    w = p;
                }
                _ => break,
            }
        }
        self.targets = targets;
    }

    fn apply(&mut self, rects: &[Rectangle]) {
        for w in &self.targets {
            // An empty list with SET = empty input region = click-through everywhere.
            let _ = self.conn.shape_rectangles(
                shape::SO::SET,
                shape::SK::INPUT,
                ClipOrdering::UNSORTED,
                *w,
                0,
                0,
                rects,
            );
        }
        let _ = self.conn.flush();
        // Discard async errors (e.g. a frame window that vanished).
        while let Ok(Some(_)) = self.conn.poll_for_event() {}
        self.last = rects.to_vec();
        self.last_apply = Instant::now();
    }

    /// `rects` are `[x, y, w, h]` in overlay-window pixels. Cheap to call every frame.
    pub fn set_regions(&mut self, rects: &[[f32; 4]]) {
        let rs: Vec<Rectangle> = rects
            .iter()
            .map(|r| Rectangle {
                x: r[0].floor().clamp(i16::MIN as f32, i16::MAX as f32) as i16,
                y: r[1].floor().clamp(i16::MIN as f32, i16::MAX as f32) as i16,
                width: r[2].ceil().clamp(0.0, u16::MAX as f32) as u16,
                height: r[3].ceil().clamp(0.0, u16::MAX as f32) as u16,
            })
            .collect();

        let changed = rs.len() != self.last.len()
            || rs.iter().zip(&self.last).any(|(a, b)| {
                (a.x, a.y, a.width, a.height) != (b.x, b.y, b.width, b.height)
            });
        // Re-apply about once a second even if unchanged: the WM may add/replace frame
        // windows or reset the shape after map/resize.
        if changed {
            // Hot path (e.g. while dragging a window): no blocking round-trips.
            self.apply(&rs);
        } else if self.last_apply.elapsed() >= Duration::from_secs(1) {
            self.refresh_targets();
            self.apply(&rs);
        }
    }
}

/// Reads the real pointer position (relative to the overlay window) from the X server.
///
/// Only a *position source* - no X input events are used. evdev can only dead-reckon the
/// cursor, so on a real X11 session this removes all drift. Useless under Wayland (XWayland
/// only knows the pointer while it is over an X window), so `main` doesn't use it there.
pub struct PointerQuery {
    conn: RustConnection,
    window: Window,
}

impl PointerQuery {
    pub fn new(xid: u32) -> Result<Self> {
        let (conn, _) = RustConnection::connect(None)?;
        Ok(Self { conn, window: xid })
    }

    pub fn position(&self) -> Option<[f32; 2]> {
        let r = self.conn.query_pointer(self.window).ok()?.reply().ok()?;
        r.same_screen.then(|| [r.win_x as f32, r.win_y as f32])
    }
}
