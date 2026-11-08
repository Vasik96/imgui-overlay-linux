//! Raw input straight from the kernel: `/dev/input/event*` (evdev). No X11, no Wayland.
//!
//! Needs read access to the event nodes and /dev/uinput, i.e. root (see `elevate.rs`).
//!
//! What it does:
//!  * scans `/dev/input` for keyboards (anything with Insert/End), mice, touchpads and
//!    absolute pointers (touchscreens, graphics tablets, VM "USB tablet" devices),
//!  * rescans every couple of seconds, so hot-plugged devices are picked up and unplugged
//!    ones are dropped,
//!  * keyboards are only listened to (never grabbed),
//!  * pointer devices are GRABBED (`EVIOCGRAB`, so the compositor no longer sees them) and
//!    everything is re-injected through a `uinput` clone of the device. While the overlay
//!    says "the pointer is over my UI" (`set_capture(true)`), button presses and wheel
//!    events are swallowed instead of forwarded, so they never reach the apps underneath.
//!    Motion is always forwarded, which keeps the real cursor in sync with ours.
//!    Works identically under X11 and Wayland, since it happens below both.
//!    The kernel releases the grab and destroys the clone when the process dies for any
//!    reason, so a crash can't leave the mouse dead.
//!  * turns the raw kernel events into a small, device-independent `InputEvent` stream.
//!
//! Only `libc` is used (open/read/write/ioctl); no extra evdev crate.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::mem::size_of;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// ---- kernel constants (linux/input-event-codes.h) -------------------------------------
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;
const EV_MSC: u16 = 0x04;

const SYN_REPORT: u16 = 0;
const SYN_DROPPED: u16 = 3;

const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;
const REL_WHEEL_HI_RES: u16 = 0x0b;
const REL_HWHEEL_HI_RES: u16 = 0x0c;

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;

pub const KEY_END: u16 = 107;
pub const KEY_INSERT: u16 = 110;

const BTN_JOYSTICK: u16 = 0x120;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const BTN_SIDE: u16 = 0x113;
const BTN_EXTRA: u16 = 0x114;
const BTN_FORWARD: u16 = 0x115;
const BTN_BACK: u16 = 0x116;
const BTN_GAMEPAD: u16 = 0x130; // = BTN_SOUTH
const BTN_TOOL_FINGER: u16 = 0x145;
const BTN_TOOL_QUINTTAP: u16 = 0x148;
const BTN_TOUCH: u16 = 0x14a;
const BTN_TOOL_DOUBLETAP: u16 = 0x14d;
const BTN_TOOL_TRIPLETAP: u16 = 0x14e;
const BTN_TOOL_QUADTAP: u16 = 0x14f;

const RESCAN_INTERVAL: Duration = Duration::from_secs(2);

/// Name prefix of our own uinput clones. Devices with this name are never grabbed again
/// (otherwise we would clone our own clone forever).
const CLONE_PREFIX: &str = "imgui-overlay passthrough";

// uinput / evdev ioctl numbers (linux/uinput.h, linux/input.h)
const UI_SET_EVBIT: u32 = 100;
const UI_SET_KEYBIT: u32 = 101;
const UI_SET_RELBIT: u32 = 102;
const UI_SET_ABSBIT: u32 = 103;
const UI_SET_MSCBIT: u32 = 104;
const UI_SET_PROPBIT: u32 = 110;

/// Event types copied to the clone: (type, size of its capability bitmap in bytes, UI_SET_*).
/// LED / sound / switch / force-feedback are deliberately NOT cloned: their state would end
/// up on the clone instead of the real hardware. EV_REP is skipped too, the real device's
/// own repeat events are forwarded and a second kernel repeat timer would double them.
const CLONE_TYPES: [(u16, usize, u32); 4] = [
    (EV_KEY, 96, UI_SET_KEYBIT),
    (EV_REL, 2, UI_SET_RELBIT),
    (EV_ABS, 8, UI_SET_ABSBIT),
    (EV_MSC, 1, UI_SET_MSCBIT),
];

// ---- public event type ----------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputEvent {
    /// Keyboard key (Linux key code, e.g. `KEY_INSERT`). Auto-repeat is filtered out.
    Key { code: u16, pressed: bool },
    /// Relative mouse motion in device units (mickeys). No acceleration applied.
    MouseMove { dx: f32, dy: f32 },
    /// Touchpad finger motion, as a fraction of the pad's width (1.0 = full pad width).
    PadMove { dx: f32, dy: f32 },
    /// Absolute pointer (touchscreen / tablet), normalized to 0..=1 on both axes.
    Absolute { x: f32, y: f32 },
    Button { button: Button, pressed: bool },
    /// Scroll wheel, in notches. +y = up, +x = right.
    Wheel { x: f32, y: f32 },
}

// ---- raw ioctl plumbing ---------------------------------------------------------------

/// `struct input_event` (64-bit layout: timeval + type + code + value).
#[repr(C)]
#[derive(Clone, Copy)]
struct RawEvent {
    time: libc::timeval,
    type_: u16,
    code: u16,
    value: i32,
}

const RAW_ZERO: RawEvent = RawEvent {
    time: libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    },
    type_: 0,
    code: 0,
    value: 0,
};

/// `_IOC(_IOC_READ, ty, nr, size)` for the common architectures (x86, arm, riscv).
const fn ioc_read(ty: u8, nr: u32, size: usize) -> libc::c_ulong {
    ((2u32 << 30) | ((size as u32) << 16) | ((ty as u32) << 8) | nr) as libc::c_ulong
}

/// `_IOC(_IOC_WRITE, ty, nr, size)`
const fn ioc_write(ty: u8, nr: u32, size: usize) -> libc::c_ulong {
    ((1u32 << 30) | ((size as u32) << 16) | ((ty as u32) << 8) | nr) as libc::c_ulong
}

/// `_IO(ty, nr)`
const fn ioc_none(ty: u8, nr: u32) -> libc::c_ulong {
    (((ty as u32) << 8) | nr) as libc::c_ulong
}

fn ioctl_val(fd: RawFd, req: libc::c_ulong, val: libc::c_ulong) -> io::Result<()> {
    let r = unsafe { libc::ioctl(fd, req as _, val) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn ioctl_ptr<T>(fd: RawFd, req: libc::c_ulong, arg: &T) -> io::Result<()> {
    let r = unsafe { libc::ioctl(fd, req as _, arg as *const T) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// `EVIOCGBIT` with a runtime size.
fn get_bits_vec(fd: RawFd, ev: u16, n: usize) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; n];
    let req = ioc_read(b'E', 0x20 + ev as u32, n);
    let r = unsafe { libc::ioctl(fd, req as _, buf.as_mut_ptr()) };
    (r >= 0).then_some(buf)
}

/// Full `struct input_absinfo` of one axis.
fn get_abs_info(fd: RawFd, axis: u16) -> Option<[i32; 6]> {
    let mut info = [0i32; 6];
    let req = ioc_read(b'E', 0x40 + axis as u32, size_of::<[i32; 6]>());
    let r = unsafe { libc::ioctl(fd, req as _, info.as_mut_ptr()) };
    (r >= 0).then_some(info)
}

/// `EVIOCGBIT(ev, N)`: capability bitmap for event type `ev` (0 = the event types themselves).
fn get_bits<const N: usize>(fd: RawFd, ev: u16) -> Option<[u8; N]> {
    let mut buf = [0u8; N];
    let req = ioc_read(b'E', 0x20 + ev as u32, N);
    let r = unsafe { libc::ioctl(fd, req as _, buf.as_mut_ptr()) };
    (r >= 0).then_some(buf)
}

fn test_bit(bits: &[u8], n: u16) -> bool {
    bits.get(n as usize / 8)
        .map_or(false, |b| b & (1 << (n % 8)) != 0)
}

/// `EVIOCGABS(axis)`: returns (minimum, maximum).
fn get_abs_range(fd: RawFd, axis: u16) -> Option<(i32, i32)> {
    // struct input_absinfo { value, minimum, maximum, fuzz, flat, resolution } (6 x 32 bit)
    let mut info = [0i32; 6];
    let req = ioc_read(b'E', 0x40 + axis as u32, size_of::<[i32; 6]>());
    let r = unsafe { libc::ioctl(fd, req as _, info.as_mut_ptr()) };
    (r >= 0 && info[2] > info[1]).then_some((info[1], info[2]))
}

/// `EVIOCGNAME(256)`.
fn get_name(fd: RawFd) -> String {
    let mut buf = [0u8; 256];
    let req = ioc_read(b'E', 0x06, buf.len());
    let r = unsafe { libc::ioctl(fd, req as _, buf.as_mut_ptr()) };
    if r < 0 {
        return "unknown".into();
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

// ---- grab + uinput passthrough ---------------------------------------------------------

#[repr(C)]
struct UinputSetup {
    id: [u16; 4], // bustype, vendor, product, version
    name: [u8; 80],
    ff_effects_max: u32,
}

#[repr(C)]
struct UinputAbsSetup {
    code: u16,
    _pad: u16,
    absinfo: [i32; 6],
}

/// A grabbed real device plus the virtual clone that stands in for it.
struct Forwarder {
    uinput: File,
    /// Button codes whose press we swallowed; their release must be swallowed too.
    swallowed: Vec<u16>,
}

impl Forwarder {
    /// Clone `src` (capabilities, abs ranges, properties, id) into a uinput device, then grab
    /// `src` exclusively. Order matters: the clone exists before the real device disappears.
    fn create(src: RawFd, name: &str) -> io::Result<Self> {
        let open = |p: &str| {
            OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
                .open(p)
        };
        let uinput = open("/dev/uinput").or_else(|_| open("/dev/input/uinput"))?;
        let ufd = uinput.as_raw_fd();
        let other = |m: &str| io::Error::new(io::ErrorKind::Other, m.to_string());

        let ev_bits = get_bits_vec(src, 0, 4).ok_or_else(|| other("EVIOCGBIT failed"))?;
        for (ty, nbytes, set_nr) in CLONE_TYPES {
            if !test_bit(&ev_bits, ty) {
                continue;
            }
            ioctl_val(ufd, ioc_write(b'U', UI_SET_EVBIT, 4), ty as libc::c_ulong)?;
            let bits = get_bits_vec(src, ty, nbytes).unwrap_or_default();
            for code in 0..(nbytes * 8) as u16 {
                if !test_bit(&bits, code) {
                    continue;
                }
                ioctl_val(ufd, ioc_write(b'U', set_nr, 4), code as libc::c_ulong)?;
                if ty == EV_ABS {
                    let absinfo = get_abs_info(src, code).ok_or_else(|| other("EVIOCGABS"))?;
                    let setup = UinputAbsSetup {
                        code,
                        _pad: 0,
                        absinfo,
                    };
                    ioctl_ptr(
                        ufd,
                        ioc_write(b'U', 4, size_of::<UinputAbsSetup>()),
                        &setup,
                    )?;
                }
            }
        }

        // Input properties (POINTER, BUTTONPAD, ...): libinput needs them to classify
        // touchpads correctly.
        let mut props = [0u8; 4];
        let req = ioc_read(b'E', 0x09, props.len());
        if unsafe { libc::ioctl(src, req as _, props.as_mut_ptr()) } >= 0 {
            for bit in 0..32u16 {
                if test_bit(&props, bit) {
                    ioctl_val(ufd, ioc_write(b'U', UI_SET_PROPBIT, 4), bit as libc::c_ulong)?;
                }
            }
        }

        // Same bus/vendor/product as the original so libinput's per-device quirks still apply.
        let mut id = [0u16; 4];
        let req = ioc_read(b'E', 0x02, size_of::<[u16; 4]>());
        let _ = unsafe { libc::ioctl(src, req as _, id.as_mut_ptr()) };
        let mut setup = UinputSetup {
            id,
            name: [0; 80],
            ff_effects_max: 0,
        };
        let full = format!("{CLONE_PREFIX}: {name}");
        let n = full.len().min(79);
        setup.name[..n].copy_from_slice(&full.as_bytes()[..n]);
        ioctl_ptr(ufd, ioc_write(b'U', 3, size_of::<UinputSetup>()), &setup)?;
        ioctl_val(ufd, ioc_none(b'U', 1), 0)?; // UI_DEV_CREATE

        // Now take the real device away from the compositor (EVIOCGRAB).
        ioctl_val(src, ioc_write(b'E', 0x90, 4), 1)?;

        Ok(Self {
            uinput,
            swallowed: Vec::new(),
        })
    }

    fn write(&self, events: &[RawEvent]) {
        if events.is_empty() {
            return;
        }
        // Whole `input_event`s only; a failed/short write just loses a few events.
        unsafe {
            libc::write(
                self.uinput.as_raw_fd(),
                events.as_ptr() as *const libc::c_void,
                std::mem::size_of_val(events),
            );
        }
    }
}

fn is_wheel(code: u16) -> bool {
    matches!(
        code,
        REL_WHEEL | REL_HWHEEL | REL_WHEEL_HI_RES | REL_HWHEEL_HI_RES
    )
}

// ---- one device -----------------------------------------------------------------------

struct Device {
    file: File,
    name: String,
    /// Has Insert/End: report key events.
    keys: bool,
    /// Relative pointer (mouse, trackball, trackpoint).
    rel: bool,
    /// Touchpad: absolute finger coordinates, converted to relative motion.
    touchpad: bool,
    /// Absolute pointer (touchscreen, tablet, VM tablet).
    abs_ptr: bool,

    abs_x: (i32, i32),
    abs_y: (i32, i32),

    /// Set when the device is grabbed and cloned (pointer devices only).
    fwd: Option<Forwarder>,

    // per-report state
    dx: i32,
    dy: i32,
    cur_x: i32,
    cur_y: i32,
    abs_dirty: bool,
    // touchpad state
    touching: bool,
    multi: [bool; 4], // 2, 3, 4, 5 fingers down => scrolling/gestures, don't move the pointer
    last: Option<(i32, i32)>,
}

impl Device {
    fn probe(file: File, grab: bool) -> Option<Self> {
        let fd = file.as_raw_fd();
        let name = get_name(fd);
        if name.starts_with(CLONE_PREFIX) {
            return None; // our own clone
        }
        let ev = get_bits::<4>(fd, 0)?;
        let key_bits = if test_bit(&ev, EV_KEY) {
            get_bits::<96>(fd, EV_KEY).unwrap_or([0; 96]) // KEY_MAX = 0x2ff
        } else {
            [0; 96]
        };
        let rel_bits = if test_bit(&ev, EV_REL) {
            get_bits::<2>(fd, EV_REL).unwrap_or([0; 2])
        } else {
            [0; 2]
        };
        let abs_bits = if test_bit(&ev, EV_ABS) {
            get_bits::<8>(fd, EV_ABS).unwrap_or([0; 8])
        } else {
            [0; 8]
        };

        let keys = test_bit(&key_bits, KEY_INSERT) || test_bit(&key_bits, KEY_END);
        let rel = test_bit(&rel_bits, REL_X) && test_bit(&rel_bits, REL_Y);
        let has_xy = test_bit(&abs_bits, ABS_X) && test_bit(&abs_bits, ABS_Y);
        let touchpad = has_xy && test_bit(&key_bits, BTN_TOOL_FINGER);
        let joystick = test_bit(&key_bits, BTN_JOYSTICK) || test_bit(&key_bits, BTN_GAMEPAD);
        let abs_ptr = has_xy
            && !touchpad
            && !joystick
            && !rel
            && (test_bit(&key_bits, BTN_LEFT) || test_bit(&key_bits, BTN_TOUCH));

        if !(keys || rel || touchpad || abs_ptr) {
            return None;
        }
        let (abs_x, abs_y) = if touchpad || abs_ptr {
            (get_abs_range(fd, ABS_X)?, get_abs_range(fd, ABS_Y)?)
        } else {
            ((0, 1), (0, 1))
        };

        let fwd = if grab && (rel || touchpad || abs_ptr) {
            match Forwarder::create(fd, &name) {
                Ok(f) => Some(f),
                Err(e) => {
                    eprintln!(
                        "input: cannot grab '{name}': {e} - clicks will ALSO reach other apps \
                         (is the `uinput` kernel module loaded? try `modprobe uinput`)"
                    );
                    None
                }
            }
        } else {
            None
        };

        Some(Self {
            name,
            file,
            fwd,
            keys,
            rel,
            touchpad,
            abs_ptr,
            abs_x,
            abs_y,
            dx: 0,
            dy: 0,
            cur_x: 0,
            cur_y: 0,
            abs_dirty: false,
            touching: false,
            multi: [false; 4],
            last: None,
        })
    }

    fn describe(&self) -> String {
        let mut kinds = Vec::new();
        if self.keys {
            kinds.push("keyboard");
        }
        if self.rel {
            kinds.push("mouse");
        }
        if self.touchpad {
            kinds.push("touchpad");
        }
        if self.abs_ptr {
            kinds.push("absolute pointer");
        }
        let grab = if self.fwd.is_some() { ", grabbed" } else { "" };
        format!("{} [{}{}]", self.name, kinds.join("+"), grab)
    }

    /// Drain everything the kernel has queued. `Err` means the device is gone.
    fn read_events(&mut self, capture: bool, out: &mut Vec<InputEvent>) -> io::Result<()> {
        let mut buf = [RAW_ZERO; 64];
        let mut fwd_buf: Vec<RawEvent> = Vec::with_capacity(64);
        let fd = self.file.as_raw_fd();
        loop {
            let n = unsafe {
                libc::read(
                    fd,
                    buf.as_mut_ptr() as *mut libc::c_void,
                    size_of::<[RawEvent; 64]>(),
                )
            };
            if n < 0 {
                let e = io::Error::last_os_error();
                return match e.kind() {
                    io::ErrorKind::WouldBlock => Ok(()),
                    io::ErrorKind::Interrupted => continue,
                    _ => Err(e), // e.g. ENODEV after unplugging
                };
            }
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            let count = n as usize / size_of::<RawEvent>();
            fwd_buf.clear();
            for i in 0..count {
                let ev = buf[i];
                if self.should_forward(&ev, capture) {
                    fwd_buf.push(ev);
                }
                self.handle(ev.type_, ev.code, ev.value, out);
            }
            if let Some(f) = &self.fwd {
                f.write(&fwd_buf);
            }
        }
    }

    /// Decide whether a raw event goes on to the compositor (through the uinput clone).
    /// Everything is forwarded, except buttons and wheel while `capture` is on.
    fn should_forward(&mut self, ev: &RawEvent, capture: bool) -> bool {
        let abs_ptr = self.abs_ptr;
        let Some(fwd) = self.fwd.as_mut() else {
            return false;
        };
        match ev.type_ {
            EV_KEY => {
                let swallowable =
                    matches!(ev.code, BTN_LEFT..=BTN_BACK) || (ev.code == BTN_TOUCH && abs_ptr);
                if !swallowable {
                    return true;
                }
                let idx = fwd.swallowed.iter().position(|c| *c == ev.code);
                match ev.value {
                    // press: swallow if the pointer is over our UI
                    1 => {
                        if capture {
                            fwd.swallowed.push(ev.code);
                            false
                        } else {
                            true
                        }
                    }
                    // release: swallow iff its press was swallowed (even if we left the UI since)
                    0 => match idx {
                        Some(i) => {
                            fwd.swallowed.swap_remove(i);
                            false
                        }
                        None => true,
                    },
                    _ => idx.is_none(),
                }
            }
            EV_REL => !(capture && is_wheel(ev.code)),
            _ => true,
        }
    }

    fn handle(&mut self, ty: u16, code: u16, value: i32, out: &mut Vec<InputEvent>) {
        match ty {
            EV_KEY => {
                if value == 2 {
                    return; // auto-repeat
                }
                let pressed = value == 1;
                let button = match code {
                    BTN_LEFT => Some(Button::Left),
                    BTN_RIGHT => Some(Button::Right),
                    BTN_MIDDLE => Some(Button::Middle),
                    BTN_SIDE | BTN_BACK => Some(Button::Back),
                    BTN_EXTRA | BTN_FORWARD => Some(Button::Forward),
                    _ => None,
                };
                if let Some(button) = button {
                    out.push(InputEvent::Button { button, pressed });
                    return;
                }
                match code {
                    BTN_TOUCH => {
                        if self.abs_ptr {
                            // touchscreen / pen contact == left click
                            out.push(InputEvent::Button {
                                button: Button::Left,
                                pressed,
                            });
                        }
                        if self.touchpad {
                            // touching a pad is not a click; just start/stop tracking
                            self.touching = pressed;
                            self.last = None;
                        }
                    }
                    BTN_TOOL_FINGER if self.touchpad => self.last = None,
                    BTN_TOOL_DOUBLETAP | BTN_TOOL_TRIPLETAP | BTN_TOOL_QUADTAP
                    | BTN_TOOL_QUINTTAP
                        if self.touchpad =>
                    {
                        let idx = match code {
                            BTN_TOOL_DOUBLETAP => 0,
                            BTN_TOOL_TRIPLETAP => 1,
                            BTN_TOOL_QUADTAP => 2,
                            _ => 3,
                        };
                        self.multi[idx] = pressed;
                        self.last = None;
                    }
                    _ => {
                        if self.keys {
                            out.push(InputEvent::Key { code, pressed });
                        }
                    }
                }
            }
            EV_REL if self.rel => match code {
                REL_X => self.dx += value,
                REL_Y => self.dy += value,
                REL_WHEEL => out.push(InputEvent::Wheel {
                    x: 0.0,
                    y: value as f32,
                }),
                REL_HWHEEL => out.push(InputEvent::Wheel {
                    x: value as f32,
                    y: 0.0,
                }),
                _ => {}
            },
            EV_ABS if self.abs_ptr || self.touchpad => match code {
                ABS_X => {
                    self.cur_x = value;
                    self.abs_dirty = true;
                }
                ABS_Y => {
                    self.cur_y = value;
                    self.abs_dirty = true;
                }
                _ => {}
            },
            EV_SYN => match code {
                SYN_REPORT => self.flush(out),
                SYN_DROPPED => {
                    // We lost events; forget partial state until the next clean report.
                    self.dx = 0;
                    self.dy = 0;
                    self.abs_dirty = false;
                    self.last = None;
                }
                _ => {}
            },
            _ => {}
        }
    }

    /// End of one atomic report: emit the accumulated motion.
    fn flush(&mut self, out: &mut Vec<InputEvent>) {
        if self.dx != 0 || self.dy != 0 {
            out.push(InputEvent::MouseMove {
                dx: self.dx as f32,
                dy: self.dy as f32,
            });
            self.dx = 0;
            self.dy = 0;
        }
        if !self.abs_dirty {
            return;
        }
        self.abs_dirty = false;

        let (x0, x1) = self.abs_x;
        let (y0, y1) = self.abs_y;
        if self.abs_ptr {
            out.push(InputEvent::Absolute {
                x: ((self.cur_x - x0) as f32 / (x1 - x0) as f32).clamp(0.0, 1.0),
                y: ((self.cur_y - y0) as f32 / (y1 - y0) as f32).clamp(0.0, 1.0),
            });
        } else if self.touchpad {
            if !self.touching || self.multi.iter().any(|m| *m) {
                self.last = None;
                return;
            }
            if let Some((lx, ly)) = self.last {
                // Same scale on both axes (relative to pad width) keeps motion isotropic.
                let w = (x1 - x0) as f32;
                out.push(InputEvent::PadMove {
                    dx: (self.cur_x - lx) as f32 / w,
                    dy: (self.cur_y - ly) as f32 / w,
                });
            }
            self.last = Some((self.cur_x, self.cur_y));
        }
    }
}

// ---- device set -----------------------------------------------------------------------

pub struct Input {
    devices: HashMap<PathBuf, Device>,
    /// Event nodes we looked at and don't care about (power button, gamepad, ...).
    ignored: HashSet<PathBuf>,
    last_scan: Option<Instant>,
    warned: bool,
    /// Grab pointer devices and forward them through uinput (OVERLAY_NO_GRAB=1 disables).
    grab: bool,
    /// The overlay's UI is under the pointer: swallow buttons and wheel.
    capture: bool,
}

impl Input {
    pub fn new() -> Self {
        let mut this = Self {
            devices: HashMap::new(),
            ignored: HashSet::new(),
            last_scan: None,
            warned: false,
            grab: std::env::var_os("OVERLAY_NO_GRAB").is_none(),
            capture: false,
        };
        this.rescan();
        this
    }

    /// While `true`, mouse buttons and wheel are kept away from the compositor/apps.
    pub fn set_capture(&mut self, capture: bool) {
        self.capture = capture;
    }

    pub fn device_count(&self) -> usize {
        self.devices.len()
    }

    /// Non-blocking. Returns everything that happened since the last call.
    pub fn poll(&mut self) -> Vec<InputEvent> {
        if self
            .last_scan
            .map_or(true, |t| t.elapsed() >= RESCAN_INTERVAL)
        {
            self.rescan();
        }

        let mut out = Vec::new();
        let mut gone = Vec::new();
        for (path, dev) in self.devices.iter_mut() {
            if dev.read_events(self.capture, &mut out).is_err() {
                gone.push(path.clone());
            }
        }
        for path in gone {
            if let Some(dev) = self.devices.remove(&path) {
                eprintln!("input: lost {} ({})", path.display(), dev.describe());
            }
        }
        out
    }

    fn rescan(&mut self) {
        self.last_scan = Some(Instant::now());
        self.ignored.retain(|p| p.exists());

        let dir = match fs::read_dir("/dev/input") {
            Ok(d) => d,
            Err(e) => {
                if !self.warned {
                    eprintln!("input: cannot read /dev/input: {e}");
                    self.warned = true;
                }
                return;
            }
        };

        let mut denied = false;
        for entry in dir.flatten() {
            let path = entry.path();
            let is_event = path
                .file_name()
                .and_then(|n| n.to_str())
                .map_or(false, |n| n.starts_with("event"));
            if !is_event || self.devices.contains_key(&path) || self.ignored.contains(&path) {
                continue;
            }
            match open_nonblocking(&path) {
                Ok(file) => match Device::probe(file, self.grab) {
                    Some(dev) => {
                        eprintln!("input: using {} ({})", path.display(), dev.describe());
                        self.devices.insert(path, dev);
                    }
                    None => {
                        self.ignored.insert(path);
                    }
                },
                Err(e) if e.kind() == io::ErrorKind::PermissionDenied => denied = true,
                Err(_) => {}
            }
        }

        if self.devices.is_empty() && !self.warned {
            if denied {
                eprintln!(
                    "input: permission denied on /dev/input/event* - run the overlay with sudo \
                     (or add your user to the `input` group)"
                );
            } else {
                eprintln!("input: no usable keyboard/mouse found in /dev/input");
            }
            self.warned = true;
        }
    }
}

fn open_nonblocking(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
}
