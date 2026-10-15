//! Input, platform independent.
//!
//! - Keyboard and mouse go to the engine's input block, which the browser page
//!   used to fill (layout from index.html / browser_input_wasm.cpp).
//! - Gamepads (physical or virtual, e.g. touch controls) feed the engine's own
//!   pad code: the `pad_update` hook writes [`Gamepad`] into `rage::ioPad`
//!   after the original update, exactly where XInput would on PC.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use glue5_game::{Env, orig};

use crate::memory::Mem;

// ---- gamepads ----------------------------------------------------------------

/// Pad buttons, with RAGE's bit values.
pub mod button {
    pub const L2: u16 = 0x0001;
    pub const R2: u16 = 0x0002;
    pub const L1: u16 = 0x0004;
    pub const R1: u16 = 0x0008;
    /// Y / Triangle
    pub const NORTH: u16 = 0x0010;
    /// B / Circle
    pub const EAST: u16 = 0x0020;
    /// A / Cross
    pub const SOUTH: u16 = 0x0040;
    /// X / Square
    pub const WEST: u16 = 0x0080;
    /// Back / Select
    pub const SELECT: u16 = 0x0100;
    pub const L3: u16 = 0x0200;
    pub const R3: u16 = 0x0400;
    pub const START: u16 = 0x0800;
    pub const UP: u16 = 0x1000;
    pub const RIGHT: u16 = 0x2000;
    pub const DOWN: u16 = 0x4000;
    pub const LEFT: u16 = 0x8000;
}

/// State of one pad. Sticks are -1..1 with +y down (screen convention);
/// triggers 0..1.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Gamepad {
    pub connected: bool,
    pub buttons: u16,
    pub left: [f32; 2],
    pub right: [f32; 2],
    pub l2: f32,
    pub r2: f32,
}

pub const MAX_PADS: usize = 4;

/// Rumble motor strengths the engine asked for, per pad: (low, high frequency).
static RUMBLE: Mutex<[(u16, u16); MAX_PADS]> = Mutex::new([(0, 0); MAX_PADS]);

/// The engine's current rumble request for pad `slot` (low, high frequency).
pub fn rumble(slot: usize) -> (u16, u16) {
    RUMBLE.lock().unwrap().get(slot).copied().unwrap_or_default()
}

static PADS: Mutex<[Gamepad; MAX_PADS]> =
    Mutex::new([Gamepad { connected: false, buttons: 0, left: [0.0; 2], right: [0.0; 2], l2: 0.0, r2: 0.0 }; MAX_PADS]);

/// Buttons pressed since the engine last read each pad. The engine reads
/// the pads once a frame; a press that begins and ends in between (a quick
/// tap on a touch button: START, a face button) would never reach it, so
/// it counts as held until the engine's next read.
static PRESSED: Mutex<[u16; MAX_PADS]> = Mutex::new([0; MAX_PADS]);

/// Sets what pad `slot` reports to the engine from its next update on.
pub fn set_gamepad(slot: usize, pad: Gamepad) {
    if slot < MAX_PADS {
        let mut pads = PADS.lock().unwrap();
        PRESSED.lock().unwrap()[slot] |= pad.buttons & !pads[slot].buttons;
        pads[slot] = pad;
    }
}

// rage::ioPad (136 bytes per pad, 4 pads).
const PAD_INDEX: u64 = 8;
const PAD_BUTTONS: u64 = 12;
const PAD_AXES: u64 = 20; // LX, LY, RX, RY: one byte each, 0x80 = centre
const PAD_ANALOG: u64 = 24; // analog button values; [0] = L2, [1] = R2
const PAD_HAS_ANALOG: u64 = 55; // 1: CPad reads the analog trigger values
const PAD_MOTORS: u64 = 66; // u16 low, u16 high (ioPad::SetActuatorValue)

/// Hook for `rage::ioPad::Update(bool)`: the original runs first (it resets the
/// pad and merges the engine's own injected input), then the host's state is
/// merged on top.
///
/// (START and Back as any button: the engine's debug pad, which took them,
/// is off; see `Config::engine_args`.)
pub(crate) unsafe extern "C" fn pad_update(env: *mut Env, pad: u64) {
    unsafe { orig::pad_update(env, pad) };
    let mem = unsafe { Mem::of(env) };
    let slot = mem.u32(pad + PAD_INDEX) as usize;
    if let Some(r) = RUMBLE.lock().unwrap().get_mut(slot) {
        *r = (mem.u16(pad + PAD_MOTORS), mem.u16(pad + PAD_MOTORS + 2));
    }
    let Some(mut state) = PADS.lock().unwrap().get(slot).copied() else { return };
    state.buttons |= std::mem::take(&mut PRESSED.lock().unwrap()[slot]);
    if !state.connected {
        return;
    }
    mem.set_u32(pad + PAD_BUTTONS, mem.u32(pad + PAD_BUTTONS) | state.buttons as u32);
    let axis = |v: f32| ((v.clamp(-1.0, 1.0) + 1.0) * 127.5).round() as u8;
    for (i, v) in [state.left[0], state.left[1], state.right[0], state.right[1]].into_iter().enumerate() {
        // Keep the original value when the host reports a centred stick.
        if v.abs() > 1.0 / 128.0 {
            mem.set_u8(pad + PAD_AXES + i as u64, axis(v));
        }
    }
    let trigger = |v: f32, bit: u16| {
        if state.buttons & bit != 0 { 255 } else { (v.clamp(0.0, 1.0) * 255.0).round() as u8 }
    };
    mem.set_u8(pad + PAD_ANALOG, trigger(state.l2, button::L2));
    mem.set_u8(pad + PAD_ANALOG + 1, trigger(state.r2, button::R2));
    mem.set_u8(pad + PAD_HAS_ANALOG, 1);
}

// ---- radio wheel ---------------------------------------------------------------

/// Hook for `CRadioWheel::GetInputAxis`. The engine reads the radio wheel's
/// own inputs (`INPUT_RADIO_WHEEL_LR`/`UD`), which the control files of this
/// game data, older than the code, map to no device: the selector never
/// moved, with the mouse or a pad. It reads the selection wheel's, as the
/// weapon wheel does: the mouse (`fixed.meta`), the right stick.
pub(crate) unsafe extern "C" fn radio_wheel_axes(env: *mut Env, this: u64, x: u64, y: u64) {
    unsafe { orig::selection_wheel_axes(env, this, x, y) }
}

// ---- text entry ----------------------------------------------------------------

static TEXT_BOX: AtomicBool = AtomicBool::new(false);

/// Whether the game's text box is open (it wants typed text): touch devices
/// show the system keyboard meanwhile.
pub fn text_box_open() -> bool {
    TEXT_BOX.load(Ordering::Relaxed)
}

/// Hook for `CTextInputBox::Open`.
pub(crate) unsafe extern "C" fn text_box_open_hook(env: *mut Env, this: u64, params: u64) {
    unsafe { orig::text_box_open(env, this, params) };
    TEXT_BOX.store(true, Ordering::Relaxed);
}

/// Hook for `CTextInputBox::RemoveTextInputBoxMovie`: the text box closes
/// (accepted, cancelled or closed by the game).
pub(crate) unsafe extern "C" fn text_box_close_hook(env: *mut Env, this: u64) {
    unsafe { orig::text_box_close(env, this) };
    TEXT_BOX.store(false, Ordering::Relaxed);
}

// ---- keyboard & mouse --------------------------------------------------------

// The input block.
const KEYS: u64 = 0; // u8[256] by Windows virtual-key code, 0x80 = down
const MOUSE_X: u64 = 256;
const MOUSE_Y: u64 = 260;
const MOUSE_DX: u64 = 264; // accumulated; the engine swaps them with 0
const MOUSE_DY: u64 = 268;
const MOUSE_DZ: u64 = 272; // wheel notches, + = up
const MOUSE_BUTTONS: u64 = 276;
const FOCUS: u64 = 280;
const LOCKED: u64 = 284;
const TEXT_HEAD: u64 = 288;
const TEXT_TAIL: u64 = 292;
const TEXT: u64 = 296; // u16[64] ring of UTF-16 units
const TEXT_LEN: u32 = 64;
const WIN_W: u64 = 432;
const WIN_H: u64 = 436;

/// Mouse buttons in the input block.
pub mod mouse {
    pub const LEFT: u32 = 1;
    pub const RIGHT: u32 = 2;
    pub const MIDDLE: u32 = 4;
    pub const X1: u32 = 8;
    pub const X2: u32 = 16;
}

/// Writes keyboard and mouse input into the engine's input block.
#[derive(Clone, Copy)]
pub struct InputBlock {
    mem: Mem,
    at: u64,
}

impl InputBlock {
    pub(crate) fn new(mem: Mem, at: u64) -> Self {
        InputBlock { mem, at }
    }

    /// A key by Windows virtual-key code.
    pub fn key(&self, vk: u8, down: bool) {
        self.mem.set_u8(self.at + KEYS + vk as u64, if down { 0x80 } else { 0 });
    }

    /// Releases every key and button (e.g. on focus loss).
    pub fn release_all(&self) {
        self.mem.bytes_mut(self.at + KEYS, 256).fill(0);
        self.mem.atomic_u32(self.at + MOUSE_BUTTONS).store(0, Ordering::Relaxed);
    }

    /// Absolute position in back-buffer pixels.
    pub fn mouse_position(&self, x: i32, y: i32) {
        self.mem.set_i32(self.at + MOUSE_X, x);
        self.mem.set_i32(self.at + MOUSE_Y, y);
    }

    /// Relative motion (accumulates until the engine reads it).
    pub fn mouse_motion(&self, dx: i32, dy: i32) {
        self.mem.atomic_u32(self.at + MOUSE_DX).fetch_add(dx as u32, Ordering::AcqRel);
        self.mem.atomic_u32(self.at + MOUSE_DY).fetch_add(dy as u32, Ordering::AcqRel);
    }

    pub fn mouse_wheel(&self, notches: i32) {
        self.mem.atomic_u32(self.at + MOUSE_DZ).fetch_add(notches as u32, Ordering::AcqRel);
    }

    pub fn mouse_button(&self, button: u32, down: bool) {
        let word = self.mem.atomic_u32(self.at + MOUSE_BUTTONS);
        if down {
            word.fetch_or(button, Ordering::AcqRel);
        } else {
            word.fetch_and(!button, Ordering::AcqRel);
        }
    }

    pub fn focus(&self, focused: bool) {
        if !focused {
            self.release_all();
        }
        self.mem.set_u32(self.at + FOCUS, focused as u32);
    }

    pub fn pointer_locked(&self, locked: bool) {
        self.mem.set_u32(self.at + LOCKED, locked as u32);
    }

    /// Typed text (Enter = 13, Backspace = 8 also go through here).
    pub fn text(&self, s: &str) {
        let head = self.mem.atomic_u32(self.at + TEXT_HEAD);
        let tail = self.mem.atomic_u32(self.at + TEXT_TAIL);
        for unit in s.encode_utf16() {
            let h = head.load(Ordering::Acquire);
            if h.wrapping_sub(tail.load(Ordering::Acquire)) >= TEXT_LEN {
                break; // full
            }
            self.mem.set_u16(self.at + TEXT + (h % TEXT_LEN) as u64 * 2, unit);
            head.store(h.wrapping_add(1), Ordering::Release);
        }
    }

    /// Asks the engine to resize its back buffer.
    pub fn resize(&self, width: u32, height: u32) {
        self.mem.set_u32(self.at + WIN_W, width);
        self.mem.set_u32(self.at + WIN_H, height);
    }
}

/// This module's hooks.
pub(crate) fn hooks(hooks: &mut glue5_game::Hooks) {
    hooks.pad_update = Some(pad_update);
    hooks.radio_wheel_axes = Some(radio_wheel_axes);
    hooks.text_box_open = Some(text_box_open_hook);
    hooks.text_box_close = Some(text_box_close_hook);
}
