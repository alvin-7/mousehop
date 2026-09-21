use super::error::{EmulationError, WindowsEmulationCreationError};
use input_event::{
    BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, Event, KeyboardEvent, PointerEvent,
    scancode,
};

use async_trait::async_trait;
use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::ops::BitOrAssign;
use std::time::Duration;
use tokio::task::JoinHandle;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_WHEEL, MOUSEINPUT,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT_0, KEYEVENTF_EXTENDEDKEY, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, SendInput,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    SPI_GETKEYBOARDDELAY, SPI_GETKEYBOARDSPEED, SYSTEM_PARAMETERS_INFO_ACTION,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SetCursorPos, SystemParametersInfoW, XBUTTON1, XBUTTON2,
};

use super::{Emulation, EmulationHandle};

/// Fallback initial key-repeat delay used only when the keyboard delay
/// can't be read from the OS (see [`read_key_repeat_settings`]).
const DEFAULT_REPEAT_DELAY: Duration = Duration::from_millis(500);
/// Fallback key-repeat interval used only when the keyboard speed can't
/// be read from the OS (see [`read_key_repeat_settings`]).
const DEFAULT_REPEAT_INTERVAL: Duration = Duration::from_millis(32);

/// Reads this machine's keyboard repeat settings and returns them as
/// `(initial_delay, repeat_interval)`.
///
/// `SendInput`-injected keystrokes don't auto-repeat on Windows, so this
/// sink synthesizes the repeat stream itself. Reading the host's own
/// settings (Control Panel → Keyboard) makes forwarded keys feel like
/// typing directly on this machine instead of a hardcoded rate.
///
/// * `SPI_GETKEYBOARDDELAY` returns 0..=3, where each step is ~250 ms,
///   so the delay is `(value + 1) * 250` ms.
/// * `SPI_GETKEYBOARDSPEED` returns 0..=31, mapping roughly linearly
///   from ~2.5 repeats/sec (0) to ~30 repeats/sec (31).
fn read_key_repeat_settings() -> (Duration, Duration) {
    let delay = read_spi_u32(SPI_GETKEYBOARDDELAY)
        .map(|v| Duration::from_millis(u64::from(v.min(3) + 1) * 250))
        .unwrap_or(DEFAULT_REPEAT_DELAY);
    let interval = read_spi_u32(SPI_GETKEYBOARDSPEED)
        .map(|v| {
            let cps = 2.5 + f64::from(v.min(31)) * (30.0 - 2.5) / 31.0;
            Duration::from_millis((1000.0 / cps) as u64)
        })
        .unwrap_or(DEFAULT_REPEAT_INTERVAL);
    (delay, interval)
}

/// Reads a `SystemParametersInfoW` action that yields a single `u32`,
/// returning `None` if the call fails.
fn read_spi_u32(action: SYSTEM_PARAMETERS_INFO_ACTION) -> Option<u32> {
    let mut value: u32 = 0;
    unsafe {
        SystemParametersInfoW(
            action,
            0,
            Some(&mut value as *mut u32 as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .ok()?;
    }
    Some(value)
}

pub(crate) struct WindowsEmulation {
    repeat_tasks: HashMap<EmulationHandle, JoinHandle<Result<(), EmulationError>>>,
    /// Cached virtual-screen origin, refreshed on each
    /// `display_bounds()` call — which the emulation proxy invokes
    /// at backend creation and then every 2s. `warp_cursor` reads
    /// the origin from here so it pairs with the size the warp
    /// target was scaled against, instead of a fresher origin that
    /// could mix two arrangements while displays are being
    /// rearranged.
    virtual_screen_origin: Cell<Option<(i32, i32)>>,
}

impl WindowsEmulation {
    pub(crate) fn new() -> Result<Self, WindowsEmulationCreationError> {
        Ok(Self {
            repeat_tasks: HashMap::new(),
            virtual_screen_origin: Cell::new(None),
        })
    }
}

#[async_trait]
impl Emulation for WindowsEmulation {
    async fn consume(
        &mut self,
        event: Event,
        handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        match event {
            Event::Pointer(pointer_event) => match pointer_event {
                PointerEvent::Motion { time: _, dx, dy } => {
                    rel_mouse(dx as i32, dy as i32)?;
                }
                PointerEvent::Button {
                    time: _,
                    button,
                    state,
                } => mouse_button(button, state)?,
                PointerEvent::Axis { axis, value, .. } => scroll(axis, value as i32)?,
                PointerEvent::AxisDiscrete120 { axis, value } => scroll(axis, value)?,
            },
            Event::Keyboard(keyboard_event) => match keyboard_event {
                KeyboardEvent::Key {
                    time: _,
                    key,
                    state,
                } => {
                    let stopped = match state {
                        // pressed
                        0 | 1 => self.kill_repeat_task(handle).await,
                        _ => Ok(()),
                    };
                    // Even a failed repeat must not prevent the key-up attempt.
                    let injected = key_event(key, state);
                    stopped?;
                    injected?;
                    if state == 1 {
                        self.spawn_repeat_task(handle, key);
                    }
                }
                KeyboardEvent::Modifiers { .. } => {}
            },
            Event::Clipboard(_) => {
                // Clipboard injection is handled by the cross-
                // platform `ClipboardEmulation` sink.
            }
        }
        Ok(())
    }

    async fn create(&mut self, _handle: EmulationHandle) {}

    async fn destroy(&mut self, handle: EmulationHandle) {
        if let Err(error) = self.kill_repeat_task(handle).await {
            log::warn!("repeat cleanup: {error}");
        }
    }

    async fn terminate(&mut self) {
        for handle in self.repeat_tasks.keys().copied().collect::<Vec<_>>() {
            self.destroy(handle).await;
        }
    }

    async fn quiesce(&mut self, handle: EmulationHandle) -> Result<(), EmulationError> {
        self.kill_repeat_task(handle).await
    }

    fn recovery_baseline(&mut self) -> Result<super::RecoveryBaseline, EmulationError> {
        use windows::Win32::{Foundation::POINT, UI::WindowsAndMessaging::GetCursorPos};
        let mut point = POINT::default();
        let (w, h) = self
            .display_bounds()
            .ok_or(EmulationError::DisplayTopologyUnavailable)?;
        let (x, y) = self
            .virtual_screen_origin
            .get()
            .ok_or(EmulationError::DisplayTopologyUnavailable)?;
        let layout = input_event::display::DisplayLayout::new([(x, y, w, h)]);
        unsafe { GetCursorPos(&mut point) }
            .map_err(|e| EmulationError::Io(std::io::Error::other(e.to_string())))?;
        Ok(super::RecoveryBaseline {
            cursor: (point.x, point.y),
            layout,
        })
    }

    fn display_bounds(&mut self) -> Option<(u32, u32)> {
        // Virtual-screen metrics cover the union of every monitor
        // attached to the system, matching the host-side capture
        // model that uses the union of all displays. Also the sole
        // refresh point of `virtual_screen_origin` — see the field
        // docs.
        let (w, h) = unsafe {
            (
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        if w <= 0 || h <= 0 {
            return None;
        }
        self.virtual_screen_origin
            .set(Some(virtual_screen_origin()));
        Some((w as u32, h as u32))
    }

    fn supports_edge_warp(&self) -> bool {
        true
    }

    async fn warp_cursor(&mut self, x: i32, y: i32) -> Result<(), EmulationError> {
        // Cached at the last display_bounds() poll; the live query
        // is only a fallback for a warp that somehow arrives before
        // the proxy's creation-time bounds read.
        let origin = self
            .virtual_screen_origin
            .get()
            .unwrap_or_else(virtual_screen_origin);
        let (screen_x, screen_y) = union_to_screen(origin, x, y);
        unsafe {
            let _ = SetCursorPos(screen_x, screen_y);
        }
        Ok(())
    }
}

/// Top-left corner of the virtual screen in absolute screen
/// coordinates. Zero on the common layout, but negative on any setup
/// with a display left of or above the primary — the primary always
/// starts at (0, 0), so the union extends into negative space.
fn virtual_screen_origin() -> (i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
        )
    }
}

/// Convert a union-relative warp target into absolute screen
/// coordinates.
///
/// Warp targets arrive union-relative — the `ProtoEvent::CursorPos`
/// handler scales the peer's normalized fraction against
/// `display_bounds()`, which reports only the *size* of the display
/// union — while `SetCursorPos` consumes absolute screen coordinates.
/// Reapplying the origin is a no-op whenever the primary display is
/// the top-left one, and is the difference between landing on the
/// intended monitor and the wrong one when it isn't.
fn union_to_screen(origin: (i32, i32), x: i32, y: i32) -> (i32, i32) {
    (origin.0.saturating_add(x), origin.1.saturating_add(y))
}

impl WindowsEmulation {
    fn spawn_repeat_task(&mut self, handle: EmulationHandle, key: u32) {
        // there can only be one repeating key and it's
        // always the last to be pressed
        // Use the host's own keyboard repeat settings so forwarded keys
        // feel identical to typing directly on this machine.
        let (repeat_delay, repeat_interval) = read_key_repeat_settings();
        let repeat_task = tokio::task::spawn_local(async move {
            tokio::time::sleep(repeat_delay).await;
            loop {
                key_event(key, 1)?;
                tokio::time::sleep(repeat_interval).await;
            }
        });
        self.repeat_tasks.insert(handle, repeat_task);
    }
    async fn kill_repeat_task(&mut self, handle: EmulationHandle) -> Result<(), EmulationError> {
        if let Some(task) = self.repeat_tasks.remove(&handle) {
            task.abort();
            match task.await {
                Ok(result) => result?,
                Err(error) if error.is_cancelled() => {}
                Err(error) => return Err(EmulationError::BackgroundTask(error.to_string())),
            }
        }
        Ok(())
    }
}

fn send_input_safe(input: INPUT) -> Result<(), EmulationError> {
    submit_input(|| unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) })
}

fn submit_input(mut send: impl FnMut() -> u32) -> Result<(), EmulationError> {
    // Exactly one event: no partially executed batch and no blind retry.
    if send() == 1 {
        Ok(())
    } else {
        Err(EmulationError::Io(std::io::Error::other(format!(
            "SendInput rejected input (possibly UIPI): {}",
            std::io::Error::last_os_error()
        ))))
    }
}

fn send_mouse_input(mi: MOUSEINPUT) -> Result<(), EmulationError> {
    send_input_safe(INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi },
    })
}

fn send_keyboard_input(ki: KEYBDINPUT) -> Result<(), EmulationError> {
    send_input_safe(INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki },
    })
}
fn rel_mouse(dx: i32, dy: i32) -> Result<(), EmulationError> {
    let mi = MOUSEINPUT {
        dx,
        dy,
        mouseData: 0,
        dwFlags: MOUSEEVENTF_MOVE,
        time: 0,
        dwExtraInfo: 0,
    };
    send_mouse_input(mi)
}

fn mouse_button(button: u32, state: u32) -> Result<(), EmulationError> {
    let dw_flags = match state {
        0 => match button {
            BTN_LEFT => MOUSEEVENTF_LEFTUP,
            BTN_RIGHT => MOUSEEVENTF_RIGHTUP,
            BTN_MIDDLE => MOUSEEVENTF_MIDDLEUP,
            BTN_BACK => MOUSEEVENTF_XUP,
            BTN_FORWARD => MOUSEEVENTF_XUP,
            _ => return Ok(()),
        },
        1 => match button {
            BTN_LEFT => MOUSEEVENTF_LEFTDOWN,
            BTN_RIGHT => MOUSEEVENTF_RIGHTDOWN,
            BTN_MIDDLE => MOUSEEVENTF_MIDDLEDOWN,
            BTN_BACK => MOUSEEVENTF_XDOWN,
            BTN_FORWARD => MOUSEEVENTF_XDOWN,
            _ => return Ok(()),
        },
        _ => return Ok(()),
    };
    let mouse_data = match button {
        BTN_BACK => XBUTTON1 as u32,
        BTN_FORWARD => XBUTTON2 as u32,
        _ => 0,
    };
    let mi = MOUSEINPUT {
        dx: 0,
        dy: 0, // no movement
        mouseData: mouse_data,
        dwFlags: dw_flags,
        time: 0,
        dwExtraInfo: 0,
    };
    send_mouse_input(mi)
}

fn scroll(axis: u8, value: i32) -> Result<(), EmulationError> {
    if let Some(input) = scroll_input(axis, value) {
        send_mouse_input(input)?;
    }
    Ok(())
}

fn scroll_input(axis: u8, value: i32) -> Option<MOUSEINPUT> {
    // Wire axes are positive down/right. Win32 wheel axes are positive
    // up/right, so only vertical scrolling needs a sign conversion here.
    // The per-peer natural-scroll preference has already been applied.
    let (event_type, delta) = match axis {
        0 => (MOUSEEVENTF_WHEEL, value.wrapping_neg()),
        1 => (MOUSEEVENTF_HWHEEL, value),
        _ => return None,
    };
    Some(MOUSEINPUT {
        dx: 0,
        dy: 0,
        mouseData: delta as u32,
        dwFlags: event_type,
        time: 0,
        dwExtraInfo: 0,
    })
}

fn key_event(key: u32, state: u8) -> Result<(), EmulationError> {
    let scancode = match linux_keycode_to_windows_scancode(key) {
        Some(code) => code,
        None => return Ok(()),
    };
    let extended = scancode > 0xff;
    let scancode = scancode & 0xff;
    let mut flags = KEYEVENTF_SCANCODE;
    if extended {
        flags.bitor_assign(KEYEVENTF_EXTENDEDKEY);
    }
    if state == 0 {
        flags.bitor_assign(KEYEVENTF_KEYUP);
    }
    let ki = KEYBDINPUT {
        wVk: Default::default(),
        wScan: scancode,
        dwFlags: flags,
        time: 0,
        dwExtraInfo: 0,
    };
    send_keyboard_input(ki)
}

fn linux_keycode_to_windows_scancode(linux_keycode: u32) -> Option<u16> {
    let linux_scancode = match scancode::Linux::try_from(linux_keycode) {
        Ok(s) => s,
        Err(_) => {
            log::warn!("unknown keycode: {linux_keycode}");
            return None;
        }
    };
    log::trace!("linux code: {linux_scancode:?}");
    let windows_scancode = match scancode::Windows::try_from(linux_scancode) {
        Ok(s) => s,
        Err(_) => {
            log::warn!("failed to translate linux code into windows scancode: {linux_scancode:?}");
            return None;
        }
    };
    log::trace!("windows code: {windows_scancode:?}");
    Some(windows_scancode as u16)
}

#[cfg(test)]
mod tests {
    #[test]
    fn recovery_r1_2_send_input_failure_is_bounded_and_propagated() {
        let mut calls = 0;
        assert!(
            super::submit_input(|| {
                calls += 1;
                0
            })
            .is_err()
        );
        assert_eq!(calls, 1);
        assert!(super::submit_input(|| 1).is_ok());
    }

    #[tokio::test]
    async fn recovery_r1_2_repeat_stop_joins_and_preserves_other_handle() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        struct Stopped(Arc<AtomicBool>);
        impl Drop for Stopped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let mut backend = super::WindowsEmulation::new().unwrap();
        let stopped = Arc::new(AtomicBool::new(false));
        let flag = stopped.clone();
        let (ready, started) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _guard = Stopped(flag);
            ready.send(()).unwrap();
            std::future::pending::<()>().await;
            Ok(())
        });
        backend.repeat_tasks.insert(1, task);
        backend
            .repeat_tasks
            .insert(2, tokio::spawn(std::future::pending()));
        started.await.unwrap();
        backend.kill_repeat_task(1).await.unwrap();
        assert!(stopped.load(Ordering::SeqCst));
        assert!(!backend.repeat_tasks[&2].is_finished());
        backend.kill_repeat_task(2).await.unwrap();
    }

    #[tokio::test]
    async fn recovery_r1_2_repeat_failure_reaches_barrier() {
        let mut backend = super::WindowsEmulation::new().unwrap();
        let task = tokio::spawn(async { Err(super::EmulationError::EndOfStream) });
        while !task.is_finished() {
            tokio::task::yield_now().await;
        }
        backend.repeat_tasks.insert(1, task);
        assert!(backend.kill_repeat_task(1).await.is_err());
    }
    use super::{MOUSEEVENTF_HWHEEL, MOUSEEVENTF_WHEEL, scroll_input, union_to_screen};

    #[test]
    fn scroll_axes_map_to_win32_up_and_right_conventions() {
        for (value, vertical, horizontal) in [(120, -120, 120), (-120, 120, -120)] {
            let y = scroll_input(0, value).unwrap();
            let x = scroll_input(1, value).unwrap();
            assert_eq!(y.dwFlags, MOUSEEVENTF_WHEEL);
            assert_eq!(x.dwFlags, MOUSEEVENTF_HWHEEL);
            assert_eq!(y.mouseData as i32, vertical);
            assert_eq!(x.mouseData as i32, horizontal);
        }
        assert!(scroll_input(2, 120).is_none());
    }

    #[test]
    fn union_to_screen_is_identity_when_the_primary_is_top_left() {
        assert_eq!(union_to_screen((0, 0), 0, 0), (0, 0));
        assert_eq!(union_to_screen((0, 0), 1919, 1079), (1919, 1079));
    }

    #[test]
    fn union_to_screen_reapplies_a_negative_origin() {
        // A 1920x1200 display left of and slightly above a 1920x1080
        // primary: the union's top-left sits at (-1920, -120), so a
        // union-relative left-edge entry belongs there, not at x = 0.
        let origin = (-1920, -120);
        assert_eq!(union_to_screen(origin, 0, 0), (-1920, -120));
        // The primary's own top-left, reached from union coordinates.
        assert_eq!(union_to_screen(origin, 1920, 120), (0, 0));
        // The far corner of the union: the primary's bottom-right.
        assert_eq!(union_to_screen(origin, 3839, 1199), (1919, 1079));
    }

    #[test]
    fn union_to_screen_saturates_rather_than_overflowing() {
        assert_eq!(
            union_to_screen((i32::MAX, i32::MIN), 1, -1),
            (i32::MAX, i32::MIN)
        );
    }
}
