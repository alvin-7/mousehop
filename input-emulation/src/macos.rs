use super::{Emulation, EmulationHandle, error::EmulationError};
use async_trait::async_trait;
use bitflags::bitflags;
use core_foundation::base::{CFRelease, kCFAllocatorDefault};
use core_foundation::string::{CFStringCreateWithCString, CFStringRef, kCFStringEncodingUTF8};
use core_foundation_sys::number::{CFNumberGetValue, CFNumberRef, kCFNumberSInt64Type};
use core_graphics::base::CGFloat;
use core_graphics::display::{CGDisplay, CGPoint};
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGKeyCode, CGMouseButton, EventField,
    ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use input_event::{
    BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, Event, KeyboardEvent, PointerEvent,
    display::DisplayLayout, scancode,
};
use keycode::{KeyMap, KeyMapping};
use std::cell::Cell;
use std::collections::HashSet;
use std::ffi::{CString, c_char, c_int, c_void};
use std::rc::Rc;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;

use super::error::MacOSEmulationCreationError;
use crate::macos_keyboard::{
    CleanupResidue, Injection, InjectionPath, KEYBOARD_LOG_TARGET, KeySink, KeyboardInjector,
    ModifierKinds, ModifierSides, describe_sides, key_category, path_name,
};

/// Fallback initial key-repeat delay used only when the host's
/// `InitialKeyRepeat` global preference can't be read (see
/// [`read_key_repeat_prefs`]).
const DEFAULT_REPEAT_DELAY: Duration = Duration::from_millis(500);
/// Fallback key-repeat interval used only when the host's `KeyRepeat`
/// global preference can't be read (see [`read_key_repeat_prefs`]).
const DEFAULT_REPEAT_INTERVAL: Duration = Duration::from_millis(32);
const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(500);

/// Why the key-repeat task stopped, named in diagnostics.
#[derive(Clone, Copy, Debug)]
enum RepeatStopReason {
    /// Another key took over the single repeat slot.
    Replaced,
    /// The repeating key was released.
    Released,
    /// The client's session is being destroyed.
    Destroyed,
    /// The backend itself is terminating.
    Terminated,
}

impl RepeatStopReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Replaced => "replaced",
            Self::Released => "released",
            Self::Destroyed => "destroyed",
            Self::Terminated => "terminated",
        }
    }
}

/// One `cleanup_begin` line shared by destroy/terminate. `handle` is the
/// client handle destroy carries; terminate has none.
fn log_cleanup_begin(
    session: u64,
    handle: Option<EmulationHandle>,
    source: &str,
    pending: CleanupResidue,
    path: InjectionPath,
) {
    let handle = match handle {
        Some(handle) => handle.to_string(),
        None => "none".to_string(),
    };
    log::debug!(
        target: KEYBOARD_LOG_TARGET,
        "event=cleanup_begin session={session} source={source} handle={handle} path={} \
         pressed_keys={} modifier_sides={}",
        path_name(path),
        pending.pressed_keys,
        describe_sides(pending.modifier_sides),
    );
}

/// One `cleanup_end` line: a clean state at debug, remaining input as a
/// warning so a stuck key survives even without debug logging enabled.
fn log_cleanup_end(
    session: u64,
    handle: Option<EmulationHandle>,
    source: &str,
    pending: CleanupResidue,
) {
    let handle = match handle {
        Some(handle) => handle.to_string(),
        None => "none".to_string(),
    };
    if pending.is_empty() {
        log::debug!(
            target: KEYBOARD_LOG_TARGET,
            "event=cleanup_end session={session} source={source} handle={handle} residue=clean"
        );
    } else {
        log::warn!(
            target: KEYBOARD_LOG_TARGET,
            "event=cleanup_end session={session} source={source} handle={handle} \
             residue=stuck_input_remains pressed_keys={} modifier_sides={} \
             note=release_failed_on_both_paths_state_kept_for_retry",
            pending.pressed_keys,
            describe_sides(pending.modifier_sides),
        );
    }
}

/// Reads this Mac's keyboard repeat settings and returns them as
/// `(initial_delay, repeat_interval)`.
///
/// Synthetic `CGEvent`s posted by mousehop do **not** auto-repeat the
/// way a physically held key does — macOS only generates auto-repeat
/// for real HID input — so this sink synthesizes the repeat stream
/// itself. Reading the host's own `InitialKeyRepeat` / `KeyRepeat`
/// preferences (System Settings → Keyboard) here makes forwarded keys
/// feel identical to typing directly on this machine, instead of being
/// locked to a hardcoded rate.
///
/// Both values live in the global preferences domain
/// (`NSGlobalDomain` / `.GlobalPreferences`) as integers expressed in
/// units of 1/60 second — the historic 60 Hz tick the keyboard layer
/// uses. When "Key Repeat" is set to "Off", macOS stores a very large
/// value, which naturally yields an effectively infinite delay (a
/// single press, no repeat). A missing key falls back to the
/// `DEFAULT_*` constants above. We re-read on every press so changing
/// the sliders takes effect without restarting mousehop.
fn read_key_repeat_prefs() -> (Duration, Duration) {
    let initial = read_global_int_pref("InitialKeyRepeat")
        .map(ticks_to_duration)
        .unwrap_or(DEFAULT_REPEAT_DELAY);
    let interval = read_global_int_pref("KeyRepeat")
        .map(ticks_to_duration)
        .unwrap_or(DEFAULT_REPEAT_INTERVAL);
    (initial, interval)
}

/// Converts a key-repeat preference value (1/60 second ticks) into a
/// [`Duration`]. Negative/garbage values clamp to zero.
fn ticks_to_duration(ticks: i64) -> Duration {
    Duration::from_millis(ticks.max(0) as u64 * 1000 / 60)
}

/// Reads an integer key from the macOS global preferences domain,
/// returning `None` when the key is absent or not a number. Mirrors the
/// `CFPreferencesCopyAppValue` pattern already used on the capture side.
fn read_global_int_pref(name: &str) -> Option<i64> {
    unsafe {
        let key_cstr = CString::new(name).ok()?;
        let key = CFStringCreateWithCString(
            kCFAllocatorDefault,
            key_cstr.as_ptr() as *const c_char,
            kCFStringEncodingUTF8,
        );
        if key.is_null() {
            return None;
        }
        let value = CFPreferencesCopyAppValue(key, kCFPreferencesAnyApplication);
        CFRelease(key as *const c_void);
        if value.is_null() {
            return None;
        }
        let mut out: i64 = 0;
        let ok = CFNumberGetValue(
            value as CFNumberRef,
            kCFNumberSInt64Type,
            &mut out as *mut i64 as *mut c_void,
        );
        CFRelease(value);
        if ok { Some(out) } else { None }
    }
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFPreferencesCopyAppValue(key: CFStringRef, application_id: CFStringRef) -> *const c_void;
    static kCFPreferencesAnyApplication: CFStringRef;
}

pub(crate) struct MacOSEmulation {
    /// global event source for all events
    event_source: CGEventSource,
    /// task handle for key repeats
    repeat_task: Option<JoinHandle<()>>,
    /// CG key code owned by `repeat_task`. Keeping the identity next to the
    /// task prevents releasing an unrelated key from stopping the key that is
    /// actually repeating.
    repeat_key: Option<CGKeyCode>,
    /// current state of the mouse buttons (tracked by evdev button code)
    pressed_buttons: HashSet<u32>,
    /// button previously pressed (evdev button code)
    previous_button: Option<u32>,
    /// timestamp of previous click (button down)
    previous_button_click: Option<Instant>,
    /// click state, i.e. number of clicks in quick succession
    button_click_state: i64,
    /// current modifier state
    modifier_state: Rc<Cell<XMods>>,
    /// Exact physical source keys behind the aggregate modifier mask. macOS
    /// exposes left/right key codes independently, so releasing one side must
    /// not clear Shift/Control/Option/Command while its peer remains held.
    physical_modifiers: Cell<PhysicalModifiers>,
    /// Keyboard injection path plus the modifier and key state macOS has been
    /// told about. Cloned into the repeat task so a held key repeats through
    /// the same injector instead of a second one.
    key_injector: KeyboardInjector,
    /// IOPMAssertionID returned by the most recent
    /// `IOPMAssertionDeclareUserActivity` call, kept for re-use within
    /// the system's 5-second coalesce window. Without this, a CGEvent
    /// posted while the host's display is asleep wakes nothing — the
    /// kernel power-manager only treats USB/Bluetooth HID interrupts
    /// as wake-worthy, not synthesized events. Declaring user
    /// activity is Apple's documented "treat this as real user input
    /// for power purposes" signal: it wakes the display and resets
    /// the idle timer. Initialized to 0; the first call returns a
    /// real ID, subsequent calls within 5s return the same ID.
    user_activity_assertion: Cell<u32>,
    /// Cached union of every active display's rectangle, refreshed
    /// on each `display_bounds()` call — which the emulation proxy
    /// invokes at backend creation and then every 2s. `warp_cursor`
    /// reads the origin from here instead of re-walking the display
    /// list on every crossing. Beyond the saved query, this keeps
    /// the warp self-consistent: the caller scales its target
    /// against the size `display_bounds()` returned, so pairing
    /// that with a fresher origin could mix two arrangements while
    /// displays are being rearranged.
    display_union_cache: Cell<Option<(CGFloat, CGFloat, CGFloat, CGFloat)>>,
}

/// Maps an evdev button code to the CGEventType used for drag events.
fn drag_event_type(button: u32) -> CGEventType {
    match button {
        BTN_LEFT => CGEventType::LeftMouseDragged,
        BTN_RIGHT => CGEventType::RightMouseDragged,
        // middle, back, forward, and any other button all use OtherMouseDragged
        _ => CGEventType::OtherMouseDragged,
    }
}

#[derive(Clone, Copy, Debug)]
struct ButtonEventSpec {
    event_type: CGEventType,
    mouse_button: CGMouseButton,
    button_number: Option<i64>,
}

fn button_event_spec(button: u32, state: u32) -> Option<ButtonEventSpec> {
    let button_number = match button {
        BTN_BACK => Some(3),
        BTN_FORWARD => Some(4),
        _ => None,
    };
    let (event_type, mouse_button) = match (button, state) {
        (BTN_LEFT, 1) => (CGEventType::LeftMouseDown, CGMouseButton::Left),
        (BTN_LEFT, 0) => (CGEventType::LeftMouseUp, CGMouseButton::Left),
        (BTN_RIGHT, 1) => (CGEventType::RightMouseDown, CGMouseButton::Right),
        (BTN_RIGHT, 0) => (CGEventType::RightMouseUp, CGMouseButton::Right),
        (BTN_MIDDLE, 1) => (CGEventType::OtherMouseDown, CGMouseButton::Center),
        (BTN_MIDDLE, 0) => (CGEventType::OtherMouseUp, CGMouseButton::Center),
        (BTN_BACK, 1) | (BTN_FORWARD, 1) => (CGEventType::OtherMouseDown, CGMouseButton::Center),
        (BTN_BACK, 0) | (BTN_FORWARD, 0) => (CGEventType::OtherMouseUp, CGMouseButton::Center),
        _ => return None,
    };
    Some(ButtonEventSpec {
        event_type,
        mouse_button,
        button_number,
    })
}

fn commit_button_state(pressed: &mut HashSet<u32>, button: u32, state: u32) {
    if state == 1 {
        pressed.insert(button);
    } else {
        pressed.remove(&button);
    }
}

unsafe impl Send for MacOSEmulation {}

impl MacOSEmulation {
    pub(crate) fn new() -> Result<Self, MacOSEmulationCreationError> {
        request_macos_emulation_permissions()?;

        let event_source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState)
            .map_err(|_| MacOSEmulationCreationError::EventSourceCreation)?;
        let key_sink = Rc::new(MacKeySink::new(event_source.clone()));
        let key_injector = KeyboardInjector::new(key_sink);
        match key_injector.current_path() {
            InjectionPath::Nx => log::info!(
                target: KEYBOARD_LOG_TARGET,
                "event=injection_path_selected session={} path=iohid permissions=granted \
                 detail=real_hid_key_codes",
                key_injector.session()
            ),
            InjectionPath::CgEvent => log::warn!(
                target: KEYBOARD_LOG_TARGET,
                "event=injection_path_selected session={} path=cgevent permissions=granted \
                 reason=iohidsystem_unavailable \
                 note=cgevent_types_but_does_not_guarantee_macos_system_shortcuts",
                key_injector.session()
            ),
        }
        Ok(Self {
            event_source,
            key_injector,
            pressed_buttons: HashSet::new(),
            previous_button: None,
            previous_button_click: None,
            button_click_state: 0,
            repeat_task: None,
            repeat_key: None,
            modifier_state: Rc::new(Cell::new(XMods::empty())),
            physical_modifiers: Cell::new(PhysicalModifiers::empty()),
            user_activity_assertion: Cell::new(0),
            display_union_cache: Cell::new(None),
        })
    }

    /// Publish the current remote modifier snapshot to the window server.
    ///
    /// The event stream describes modifiers without a side most of the time,
    /// so the current sides are passed alongside the aggregate and the
    /// window server's own flags are read first so a locally held modifier is
    /// not cleared by a remote transition.
    fn sync_remote_modifiers(&self) {
        self.key_injector.sync_modifiers(
            modifier_kinds(self.modifier_state.get()),
            modifier_sides(self.physical_modifiers.get()),
            local_modifier_flags(),
        );
    }

    /// Tell the macOS power-manager that real user input is arriving
    /// from this process. Wakes the display if asleep and resets the
    /// idle timer. Cheap to call on every event — the system itself
    /// coalesces calls within a 5-second window (returns the same
    /// IOPMAssertionID), so we just stash the most recent ID in a
    /// `Cell` and pass it back in. Required because plain
    /// `CGEventPost` doesn't trigger display wake on its own.
    fn declare_user_activity(&self) {
        let cstr = match CString::new("Mousehop: remote input") {
            Ok(c) => c,
            Err(_) => return,
        };
        let reason = unsafe {
            CFStringCreateWithCString(
                kCFAllocatorDefault,
                cstr.as_ptr() as *const c_char,
                kCFStringEncodingUTF8,
            )
        };
        if reason.is_null() {
            return;
        }
        let mut id = self.user_activity_assertion.get();
        let _ret =
            unsafe { IOPMAssertionDeclareUserActivity(reason, K_IOPM_USER_ACTIVE_LOCAL, &mut id) };
        self.user_activity_assertion.set(id);
        unsafe { CFRelease(reason as *const c_void) };
    }

    fn get_mouse_location(&self) -> Option<CGPoint> {
        let event: CGEvent = CGEvent::new(self.event_source.clone()).ok()?;
        Some(event.location())
    }

    fn next_button_click_state(&self, button: u32, state: u32) -> i64 {
        if state != 1 {
            return self.button_click_state;
        }
        if self.previous_button == Some(button)
            && self
                .previous_button_click
                .is_some_and(|instant| instant.elapsed() < DOUBLE_CLICK_INTERVAL)
        {
            self.button_click_state + 1
        } else {
            1
        }
    }

    /// Create and post one button transition without changing local tracking.
    /// The caller commits `pressed_buttons` only after this succeeds, so a
    /// locked-session/event-creation failure cannot turn later motion into a
    /// phantom drag.
    fn post_button_event(&self, button: u32, state: u32, click_state: i64) -> bool {
        let Some(spec) = button_event_spec(button, state) else {
            log::warn!("invalid button event: {button},{state}");
            return false;
        };
        let Some(location) = self.get_mouse_location() else {
            log::warn!("could not get mouse location!");
            return false;
        };
        let Ok(event) = CGEvent::new_mouse_event(
            self.event_source.clone(),
            spec.event_type,
            location,
            spec.mouse_button,
        ) else {
            log::warn!("mouse event creation failed!");
            return false;
        };
        event.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, click_state);
        if let Some(button_number) = spec.button_number {
            event.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, button_number);
        }
        event.post(CGEventTapLocation::HID);
        true
    }

    fn release_tracked_buttons(&mut self) {
        let mut pressed = self.pressed_buttons.iter().copied().collect::<Vec<_>>();
        pressed.sort_unstable();
        for button in pressed {
            if !self.post_button_event(button, 0, self.button_click_state) {
                log::warn!("unable to synthesize release for stuck mouse button {button}");
            }
        }

        // Even if secure input prevented posting an up, never carry a stale
        // drag into a later authenticated peer session.
        self.pressed_buttons.clear();
        self.previous_button = None;
        self.previous_button_click = None;
        self.button_click_state = 0;
    }

    /// Starts the repeat stream for `key`, releasing the key it replaces.
    ///
    /// An error means a release could not be delivered on either injection
    /// path; it is propagated so the emulation session stops instead of
    /// typing on top of unconfirmed keyboard state.
    async fn spawn_repeat_task(&mut self, key: u16) -> Result<(), EmulationError> {
        // there can only be one repeating key and it's
        // always the last to be pressed
        self.cancel_repeat_task(RepeatStopReason::Replaced).await?;
        // initial key event
        self.key_injector.key_down(key);
        // Use the host's own keyboard repeat settings so forwarded keys
        // feel identical to typing directly on this Mac, rather than a
        // fixed hardcoded rate.
        let (repeat_delay, repeat_interval) = read_key_repeat_prefs();
        log::debug!(
            target: KEYBOARD_LOG_TARGET,
            "event=repeat_started session={} category={} repeat_delay_ms={} \
             repeat_interval_ms={}",
            self.key_injector.session(),
            key_category(key),
            repeat_delay.as_millis(),
            repeat_interval.as_millis(),
        );
        // repeat task
        let injector = self.key_injector.clone();
        let repeat_task = tokio::task::spawn_local(async move {
            tokio::time::sleep(repeat_delay).await;
            loop {
                injector.key_down(key);
                tokio::time::sleep(repeat_interval).await;
            }
        });
        self.repeat_task = Some(repeat_task);
        self.repeat_key = Some(key);
        Ok(())
    }

    /// Stops the repeat stream and releases its key through the injector's
    /// release policy (original path, then one CGEvent retry).
    async fn cancel_repeat_task(&mut self, reason: RepeatStopReason) -> Result<(), EmulationError> {
        if let Some(task) = self.repeat_task.take() {
            task.abort();
            let _ = task.await;
        }
        if let Some(key) = self.repeat_key.take() {
            log::debug!(
                target: KEYBOARD_LOG_TARGET,
                "event=repeat_stopped session={} category={} reason={}",
                self.key_injector.session(),
                key_category(key),
                reason.as_str(),
            );
            if let Err(status) = self.key_injector.key_up(key) {
                return Err(EmulationError::KeyboardInjection(status));
            }
        }
        Ok(())
    }

    async fn release_key(&mut self, key: CGKeyCode) -> Result<(), EmulationError> {
        if self.repeat_key == Some(key) {
            self.cancel_repeat_task(RepeatStopReason::Released).await
        } else {
            // Pressing a second character ends repeat for the first one and
            // emits its synthetic key-up. Its later physical key-up still
            // needs to pass through, but must not cancel the second key.
            match self.key_injector.key_up(key) {
                Ok(()) => Ok(()),
                Err(status) => Err(EmulationError::KeyboardInjection(status)),
            }
        }
    }
}

fn request_macos_emulation_permissions() -> Result<(), MacOSEmulationCreationError> {
    let result = check_macos_emulation_permissions(
        request_accessibility_permission,
        request_input_control_permission,
    );
    match &result {
        Ok(()) => log::debug!(
            target: KEYBOARD_LOG_TARGET,
            "event=permissions_granted backend=macos permissions=accessibility,input_control"
        ),
        Err(MacOSEmulationCreationError::AccessibilityPermission) => log::warn!(
            target: KEYBOARD_LOG_TARGET,
            "event=permission_denied backend=macos permission=accessibility \
             note=gui_owns_the_user_visible_prompt"
        ),
        Err(MacOSEmulationCreationError::InputControlPermission) => log::warn!(
            target: KEYBOARD_LOG_TARGET,
            "event=permission_denied backend=macos permission=input_control \
             note=gui_owns_the_user_visible_prompt"
        ),
        // The backend does not exist yet, so these lines carry the backend
        // tag instead of a session id.
        Err(_) => {}
    }
    result
}

fn check_macos_emulation_permissions<A, I>(
    accessibility_granted: A,
    input_control_granted: I,
) -> Result<(), MacOSEmulationCreationError>
where
    A: FnOnce() -> bool,
    I: FnOnce() -> bool,
{
    // The GUI owns the explicit user-visible Accessibility prompt. Checking the
    // CoreGraphics permission while Accessibility is absent can route through
    // the same authorization helper and queue an additional request.
    if !accessibility_granted() {
        return Err(MacOSEmulationCreationError::AccessibilityPermission);
    }
    if !input_control_granted() {
        return Err(MacOSEmulationCreationError::InputControlPermission);
    }
    Ok(())
}

#[cfg(test)]
mod permission_tests {
    use super::*;

    #[test]
    fn skips_input_control_check_without_accessibility() {
        let input_control_checked = Cell::new(false);

        let result = check_macos_emulation_permissions(
            || false,
            || {
                input_control_checked.set(true);
                true
            },
        );

        assert!(matches!(
            result,
            Err(MacOSEmulationCreationError::AccessibilityPermission)
        ));
        assert!(!input_control_checked.get());
    }
}

fn request_accessibility_permission() -> bool {
    // Silent check. The GUI owns the one-time user-visible prompt at
    // startup (see mousehop_gtk::macos_privacy).
    unsafe { AXIsProcessTrusted() }
}

fn request_input_control_permission() -> bool {
    unsafe { CGPreflightPostEventAccess() }
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPreflightPostEventAccess() -> bool;
    /// Modifier flags of an event-source state (`CGEventSourceFlagsState`).
    fn CGEventSourceFlagsState(state_id: c_int) -> u64;
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOPMAssertionDeclareUserActivity(
        assertion_name: CFStringRef,
        user_type: c_int,
        out_id: *mut u32,
    ) -> i32;
}

/// `kIOPMUserActiveLocal` — local mouse / keyboard activity (the
/// other variant, `kIOPMUserActiveRemote = 1`, is for screen-sharing
/// servers acting on behalf of a remote user; "local" is correct
/// here since we ARE the source generating local HID-style input).
const K_IOPM_USER_ACTIVE_LOCAL: c_int = 0;

/// Opaque `IOHIDSystem` connection owned by `nx_key_bridge.c`.
#[repr(C)]
struct NxDriverHandle {
    _private: [u8; 0],
}

extern "C" {
    fn mousehop_nx_open() -> *mut NxDriverHandle;
    fn mousehop_nx_close(driver: *mut NxDriverHandle);
    fn mousehop_nx_flags_changed(driver: *mut NxDriverHandle, key_code: u16, flags: u32) -> i32;
    fn mousehop_nx_key(driver: *mut NxDriverHandle, key_code: u16, down: i32) -> i32;
}

/// `kern_return_t` success.
const KERN_SUCCESS: i32 = 0;
/// The IOHID connection is missing, so this event cannot be posted.
const NX_DRIVER_UNAVAILABLE: i32 = -2;
/// `CGEvent` allocation failed, so no platform status is involved.
const CG_EVENT_CREATION_FAILED: i32 = -1;

/// Owns the `IOHIDSystem` connection used to post real HID events.
///
/// `IOHIDPostEvent` is deprecated since macOS 11 and has no replacement. The
/// call is confined to `nx_key_bridge.c`; this wrapper only manages the
/// handle's lifetime.
struct NxDriver(*mut NxDriverHandle);

impl NxDriver {
    /// Opens the driver, or returns `None` when the window server does not
    /// expose it (the caller then uses the CGEvent fallback).
    fn open() -> Option<Self> {
        let handle = unsafe { mousehop_nx_open() };
        (!handle.is_null()).then_some(Self(handle))
    }

    fn flags_changed(&self, key_code: u16, flags: u32) -> Result<(), i32> {
        match unsafe { mousehop_nx_flags_changed(self.0, key_code, flags) } {
            KERN_SUCCESS => Ok(()),
            status => Err(status),
        }
    }

    fn key(&self, key_code: u16, down: bool) -> Result<(), i32> {
        match unsafe { mousehop_nx_key(self.0, key_code, i32::from(down)) } {
            KERN_SUCCESS => Ok(()),
            status => Err(status),
        }
    }
}

impl Drop for NxDriver {
    /// Closes the connection when the backend (and every clone of its
    /// injector) is gone. `release_all` runs first; this only frees the
    /// handle.
    fn drop(&mut self) {
        unsafe {
            mousehop_nx_close(self.0);
        }
    }
}

/// The macOS half of [`KeyboardInjector`]: real `NXEventData` events through
/// `IOHIDPostEvent`, with the legacy `CGEvent` path as a fallback.
struct MacKeySink {
    event_source: CGEventSource,
    driver: Option<NxDriver>,
}

/// Wraps raw modifier bits in the platform's flag type for the `CGEvent`
/// fallback.
///
/// `core-graphics`' `CGEventFlags` only declares the general modifier bits;
/// the device-dependent side bits are valid for the window server but unknown
/// to the crate. `CGEventFlags::from_bits_truncate` would drop them, so the
/// fallback would lose which side of a modifier is down, and releasing one
/// side could clear the peer. `from_bits_retain` keeps the raw value while
/// still typing it.
fn cg_event_flags(flags: u32) -> CGEventFlags {
    CGEventFlags::from_bits_retain(u64::from(flags))
}

impl MacKeySink {
    fn new(event_source: CGEventSource) -> Self {
        Self {
            event_source,
            driver: NxDriver::open(),
        }
    }

    /// Legacy fallback. It still types, but macOS does not treat these events
    /// as physical HID input, so the system's own keyboard shortcuts are not
    /// guaranteed on this path.
    fn inject_cg_event(&self, injection: Injection) -> Result<(), i32> {
        match injection {
            Injection::Modifiers { key_code, flags } => {
                let Ok(event) = CGEvent::new(self.event_source.clone()) else {
                    return Err(CG_EVENT_CREATION_FAILED);
                };
                event.set_type(CGEventType::FlagsChanged);
                event.set_integer_value_field(
                    EventField::KEYBOARD_EVENT_KEYCODE,
                    i64::from(key_code),
                );
                event.set_flags(cg_event_flags(flags));
                event.post(CGEventTapLocation::HID);
                Ok(())
            }
            Injection::Key {
                key_code,
                down,
                cg_flags,
            } => {
                let Ok(event) =
                    CGEvent::new_keyboard_event(self.event_source.clone(), key_code, down)
                else {
                    return Err(CG_EVENT_CREATION_FAILED);
                };
                event.set_flags(cg_event_flags(cg_flags));
                event.post(CGEventTapLocation::HID);
                Ok(())
            }
        }
    }
}

impl KeySink for MacKeySink {
    fn preferred_path(&self) -> InjectionPath {
        if self.driver.is_some() {
            InjectionPath::Nx
        } else {
            InjectionPath::CgEvent
        }
    }

    fn inject(&self, path: InjectionPath, injection: Injection) -> Result<(), i32> {
        match path {
            InjectionPath::Nx => {
                let Some(driver) = self.driver.as_ref() else {
                    return Err(NX_DRIVER_UNAVAILABLE);
                };
                match injection {
                    Injection::Modifiers { key_code, flags } => {
                        driver.flags_changed(key_code, flags)
                    }
                    // Ordinary keys are posted with no global flags so the
                    // modifier state the flags-changed events published
                    // survives them.
                    Injection::Key { key_code, down, .. } => driver.key(key_code, down),
                }
            }
            InjectionPath::CgEvent => self.inject_cg_event(injection),
        }
    }
}

/// Union of every active display's rectangle, as `(origin_x,
/// origin_y, width, height)` in global display coordinates. `None`
/// when no active display reports a usable rectangle.
fn display_union() -> Option<(CGFloat, CGFloat, CGFloat, CGFloat)> {
    let bounds = active_display_layout()?.bounds()?;
    let (x, y) = bounds.origin();
    let (width, height) = bounds.size();
    Some((
        CGFloat::from(x),
        CGFloat::from(y),
        CGFloat::from(width),
        CGFloat::from(height),
    ))
}

/// Every active Quartz display in the same integer logical-point coordinate
/// space used by cursor events. Floor/ceil keeps a rare fractional display
/// bound fully represented instead of creating an empty sliver in the
/// contour.
fn active_display_layout() -> Option<DisplayLayout> {
    let displays = CGDisplay::active_displays().ok()?;
    let layout = DisplayLayout::new(displays.into_iter().filter_map(|id| {
        let bounds = CGDisplay::new(id).bounds();
        let left = bounds.origin.x.floor();
        let top = bounds.origin.y.floor();
        let right = (bounds.origin.x + bounds.size.width).ceil();
        let bottom = (bounds.origin.y + bounds.size.height).ceil();
        if !left.is_finite()
            || !top.is_finite()
            || !right.is_finite()
            || !bottom.is_finite()
            || left < f64::from(i32::MIN)
            || top < f64::from(i32::MIN)
            || right > f64::from(i32::MAX)
            || bottom > f64::from(i32::MAX)
        {
            return None;
        }
        let x = left as i32;
        let y = top as i32;
        let width = u32::try_from((right - left) as i64).ok()?;
        let height = u32::try_from((bottom - top) as i64).ok()?;
        Some((x, y, width, height))
    }));
    (!layout.is_empty()).then_some(layout)
}

/// Convert a union-relative warp target into global display
/// coordinates.
///
/// Warp targets arrive union-relative — the `ProtoEvent::CursorPos`
/// handler scales the peer's normalized fraction against
/// `display_bounds()`, which reports only the *size* of the display
/// union — while `warp_mouse_cursor_position` consumes global
/// coordinates. Those are anchored at the primary display's top-left,
/// so a monitor left of or above the primary puts the union's origin
/// in negative space and the two disagree; see the matching
/// `display_origin` on the input-capture side.
fn union_to_global(origin: (CGFloat, CGFloat), x: i32, y: i32) -> (CGFloat, CGFloat) {
    (origin.0 + x as CGFloat, origin.1 + y as CGFloat)
}

fn clamp_to_screen_space(
    current_x: CGFloat,
    current_y: CGFloat,
    dx: CGFloat,
    dy: CGFloat,
) -> (CGFloat, CGFloat) {
    let Some(layout) = active_display_layout() else {
        log::warn!("could not get active display layout");
        return (current_x, current_y);
    };
    layout
        .clamp_to_nearest_display((current_x + dx, current_y + dy))
        .unwrap_or((current_x, current_y))
}

#[async_trait]
impl Emulation for MacOSEmulation {
    async fn consume(
        &mut self,
        event: Event,
        _handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        log::trace!("{event:?}");
        // Wake the display + reset idle timer for every incoming
        // event. CGEventPost-synthesized events alone don't trigger
        // display wake on macOS — the kernel power-manager only
        // treats USB/Bluetooth HID interrupts as wake-worthy. The
        // system coalesces these calls within a 5-second window, so
        // calling on every event is essentially free.
        self.declare_user_activity();
        match event {
            Event::Pointer(pointer_event) => {
                match pointer_event {
                    PointerEvent::Motion { time: _, dx, dy } => {
                        let mut mouse_location = match self.get_mouse_location() {
                            Some(l) => l,
                            None => {
                                log::warn!("could not get mouse location!");
                                return Ok(());
                            }
                        };

                        let (new_mouse_x, new_mouse_y) =
                            clamp_to_screen_space(mouse_location.x, mouse_location.y, dx, dy);

                        mouse_location.x = new_mouse_x;
                        mouse_location.y = new_mouse_y;

                        // If any button is held, emit a drag event for it;
                        // otherwise emit a normal mouse-moved event.
                        let event_type = self
                            .pressed_buttons
                            .iter()
                            .next()
                            .map(|&btn| drag_event_type(btn))
                            .unwrap_or(CGEventType::MouseMoved);
                        let event = match CGEvent::new_mouse_event(
                            self.event_source.clone(),
                            event_type,
                            mouse_location,
                            CGMouseButton::Left,
                        ) {
                            Ok(e) => e,
                            Err(_) => {
                                log::warn!("mouse event creation failed!");
                                return Ok(());
                            }
                        };
                        event.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_X, dx as i64);
                        event.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y, dy as i64);
                        event.post(CGEventTapLocation::HID);
                    }
                    PointerEvent::Button {
                        time: _,
                        button,
                        state,
                    } => {
                        let click_state = self.next_button_click_state(button, state);
                        if !self.post_button_event(button, state, click_state) {
                            return Ok(());
                        }

                        // Commit state only after CoreGraphics accepted the
                        // event creation/post path. In particular, secure-input
                        // failures must not leave phantom drag or click state.
                        commit_button_state(&mut self.pressed_buttons, button, state);
                        if state == 1 {
                            self.button_click_state = click_state;
                            self.previous_button = Some(button);
                            self.previous_button_click = Some(Instant::now());
                        }
                        log::debug!("click_state: {}", self.button_click_state);
                    }
                    // A macOS sink replays the source's momentum coast verbatim
                    // (so a Mac->Mac session still glides) — momentum is applied,
                    // not dropped, hence the `..`.
                    PointerEvent::Axis { axis, value, .. } => {
                        let value = value as i32;
                        let (count, wheel1, wheel2, wheel3) = match axis {
                            0 => (1, value, 0, 0), // 0 = vertical => 1 scroll wheel device (y axis)
                            1 => (2, 0, value, 0), // 1 = horizontal => 2 scroll wheel devices (y, x) -> (0, x)
                            _ => {
                                log::warn!("invalid scroll event: {axis}, {value}");
                                return Ok(());
                            }
                        };
                        let event = match CGEvent::new_scroll_event(
                            self.event_source.clone(),
                            ScrollEventUnit::PIXEL,
                            count,
                            wheel1,
                            wheel2,
                            wheel3,
                        ) {
                            Ok(e) => e,
                            Err(()) => {
                                log::warn!("scroll event creation failed!");
                                return Ok(());
                            }
                        };
                        event.post(CGEventTapLocation::HID);
                    }
                    PointerEvent::AxisDiscrete120 { axis, value } => {
                        const LINES_PER_STEP: i32 = 3;
                        let (count, wheel1, wheel2, wheel3) = match axis {
                            0 => (1, value / (120 / LINES_PER_STEP), 0, 0), // 0 = vertical => 1 scroll wheel device (y axis)
                            1 => (2, 0, value / (120 / LINES_PER_STEP), 0), // 1 = horizontal => 2 scroll wheel devices (y, x) -> (0, x)
                            _ => {
                                log::warn!("invalid scroll event: {axis}, {value}");
                                return Ok(());
                            }
                        };
                        let event = match CGEvent::new_scroll_event(
                            self.event_source.clone(),
                            ScrollEventUnit::LINE,
                            count,
                            wheel1,
                            wheel2,
                            wheel3,
                        ) {
                            Ok(e) => e,
                            Err(()) => {
                                log::warn!("scroll event creation failed!");
                                return Ok(());
                            }
                        };
                        event.post(CGEventTapLocation::HID);
                    }
                }

                // reset button click state in case it's not a button event
                if !matches!(pointer_event, PointerEvent::Button { .. }) {
                    self.button_click_state = 0;
                }
            }
            Event::Keyboard(keyboard_event) => match keyboard_event {
                KeyboardEvent::Key {
                    time: _,
                    key,
                    state,
                } => {
                    let code = match KeyMap::from_key_mapping(KeyMapping::Evdev(key as u16)) {
                        Ok(k) => k.mac as CGKeyCode,
                        Err(_) => {
                            log::warn!("unable to map key event");
                            return Ok(());
                        }
                    };

                    // macOS represents Caps Lock as a toggle, while Linux
                    // sends an ordinary down/up pair (and some compositors
                    // include a locked Caps key in their boundary-entry
                    // snapshot). Turn each accepted down into one complete
                    // tap and ignore its matching up. It must never enter the
                    // repeat task: doing so floods the guest with Caps key
                    // downs until the connection closes.
                    if is_caps_lock(key) {
                        if state == 1 {
                            if let Err(status) = self.key_injector.caps_tap(code) {
                                return Err(EmulationError::KeyboardInjection(status));
                            }
                            toggle_caps_lock(&self.modifier_state);
                        }
                        return Ok(());
                    }

                    let is_modifier = update_modifiers(
                        &self.physical_modifiers,
                        &self.modifier_state,
                        key,
                        state,
                    );
                    if is_modifier {
                        // Modifier presses are state transitions, not
                        // repeatable characters: one flags-changed event that
                        // carries the real key code of the side that changed,
                        // and no duplicate key event on top of it.
                        self.sync_remote_modifiers();
                        return Ok(());
                    }
                    match state {
                        // pressed
                        1 => self.spawn_repeat_task(code).await?,
                        _ => self.release_key(code).await?,
                    }
                }
                KeyboardEvent::Modifiers {
                    depressed,
                    latched,
                    locked,
                    group,
                } => {
                    set_modifiers(
                        &self.physical_modifiers,
                        &self.modifier_state,
                        depressed,
                        latched,
                        locked,
                        group,
                    );
                    self.sync_remote_modifiers();
                }
            },
            Event::Clipboard(_) => {
                // Clipboard injection is handled by the cross-
                // platform `ClipboardEmulation` sink, not the macOS
                // emulation backend.
            }
        }
        // FIXME
        Ok(())
    }

    async fn create(&mut self, handle: EmulationHandle) {
        log::debug!(
            target: KEYBOARD_LOG_TARGET,
            "event=session_created session={} handle={} path={}",
            self.key_injector.session(),
            handle,
            path_name(self.key_injector.current_path()),
        );
    }

    async fn destroy(&mut self, handle: EmulationHandle) {
        let session = self.key_injector.session();
        log_cleanup_begin(
            session,
            Some(handle),
            "destroy",
            self.key_injector.pending(),
            self.key_injector.current_path(),
        );
        // Lifecycle cleanup cannot propagate an error to a caller; the
        // injector has already logged the failure and `release_all` below
        // retries the release through both paths.
        if let Err(status) = self.cancel_repeat_task(RepeatStopReason::Destroyed).await {
            log::warn!(
                target: KEYBOARD_LOG_TARGET,
                "event=cleanup_repeat_release_failed session={session} source=destroy \
                 status={status}"
            );
        }
        self.release_tracked_buttons();
        let residue = self.key_injector.release_all();
        self.physical_modifiers.set(PhysicalModifiers::empty());
        self.modifier_state.set(XMods::empty());
        log_cleanup_end(session, Some(handle), "destroy", residue);
    }

    async fn terminate(&mut self) {
        let session = self.key_injector.session();
        log_cleanup_begin(
            session,
            None,
            "terminate",
            self.key_injector.pending(),
            self.key_injector.current_path(),
        );
        if let Err(status) = self.cancel_repeat_task(RepeatStopReason::Terminated).await {
            log::warn!(
                target: KEYBOARD_LOG_TARGET,
                "event=cleanup_repeat_release_failed session={session} source=terminate \
                 status={status}"
            );
        }
        self.release_tracked_buttons();
        let residue = self.key_injector.release_all();
        self.physical_modifiers.set(PhysicalModifiers::empty());
        self.modifier_state.set(XMods::empty());
        log_cleanup_end(session, None, "terminate", residue);
    }

    fn display_bounds(&mut self) -> Option<(u32, u32)> {
        // Union of every active display's rectangle. Matches the
        // shape used on the input-capture side so the host's
        // wall-press model is consistent across both ends. Also the
        // sole refresh point of `display_union_cache` — see the
        // field docs.
        let union = display_union();
        self.display_union_cache.set(union);
        let (_, _, width, height) = union?;
        Some((width as u32, height as u32))
    }

    fn display_layout(&mut self) -> Option<DisplayLayout> {
        let layout = active_display_layout()?;
        let bounds = layout.bounds()?;
        let (origin_x, origin_y) = bounds.origin();
        let (width, height) = bounds.size();
        self.display_union_cache.set(Some((
            CGFloat::from(origin_x),
            CGFloat::from(origin_y),
            CGFloat::from(width),
            CGFloat::from(height),
        )));
        Some(layout)
    }

    fn supports_edge_warp(&self) -> bool {
        true
    }

    async fn warp_cursor_in_layout(
        &mut self,
        x: i32,
        y: i32,
        layout: &DisplayLayout,
    ) -> Result<(), EmulationError> {
        let (origin_x, origin_y) = layout
            .origin()
            .ok_or(EmulationError::DisplayTopologyUnavailable)?;
        let (global_x, global_y) =
            union_to_global((CGFloat::from(origin_x), CGFloat::from(origin_y)), x, y);
        CGDisplay::warp_mouse_cursor_position(CGPoint {
            x: global_x,
            y: global_y,
        })
        .map_err(EmulationError::CoreGraphics)
    }

    async fn warp_cursor(&mut self, x: i32, y: i32) -> Result<(), EmulationError> {
        // Cached at the last display_bounds() poll; the live query
        // is only a fallback for a warp that somehow arrives before
        // the proxy's creation-time bounds read.
        let union = self.display_union_cache.get().or_else(display_union);
        let origin = union.map_or((0.0, 0.0), |(ox, oy, _, _)| (ox, oy));
        let (global_x, global_y) = union_to_global(origin, x, y);
        let pt = CGPoint {
            x: global_x,
            y: global_y,
        };
        // CGDisplay::warp_mouse_cursor_position is a global Quartz
        // call; it doesn't matter which CGDisplay receiver we use.
        CGDisplay::warp_mouse_cursor_position(pt).map_err(EmulationError::CoreGraphics)
    }
}

fn modifier_source(key: scancode::Linux) -> Option<(PhysicalModifiers, PhysicalModifiers, XMods)> {
    let (source, group, mask) = match key {
        scancode::Linux::KeyLeftShift => (
            PhysicalModifiers::LEFT_SHIFT,
            PhysicalModifiers::SHIFT,
            XMods::ShiftMask,
        ),
        scancode::Linux::KeyRightShift => (
            PhysicalModifiers::RIGHT_SHIFT,
            PhysicalModifiers::SHIFT,
            XMods::ShiftMask,
        ),
        scancode::Linux::KeyLeftCtrl => (
            PhysicalModifiers::LEFT_CONTROL,
            PhysicalModifiers::CONTROL,
            XMods::ControlMask,
        ),
        scancode::Linux::KeyRightCtrl => (
            PhysicalModifiers::RIGHT_CONTROL,
            PhysicalModifiers::CONTROL,
            XMods::ControlMask,
        ),
        scancode::Linux::KeyLeftAlt => (
            PhysicalModifiers::LEFT_OPTION,
            PhysicalModifiers::OPTION,
            XMods::Mod1Mask,
        ),
        scancode::Linux::KeyRightalt => (
            PhysicalModifiers::RIGHT_OPTION,
            PhysicalModifiers::OPTION,
            XMods::Mod1Mask,
        ),
        scancode::Linux::KeyLeftMeta => (
            PhysicalModifiers::LEFT_COMMAND,
            PhysicalModifiers::COMMAND,
            XMods::Mod4Mask,
        ),
        scancode::Linux::KeyRightmeta => (
            PhysicalModifiers::RIGHT_COMMAND,
            PhysicalModifiers::COMMAND,
            XMods::Mod4Mask,
        ),
        _ => return None,
    };
    Some((source, group, mask))
}

fn update_modifiers(
    physical: &Cell<PhysicalModifiers>,
    modifiers: &Cell<XMods>,
    key: u32,
    state: u8,
) -> bool {
    let Ok(key) = scancode::Linux::try_from(key) else {
        return false;
    };
    let Some((source, group, mask)) = modifier_source(key) else {
        return false;
    };

    let mut physical_state = physical.get();
    physical_state.set(source, state == 1);
    physical.set(physical_state);

    let mut aggregate = modifiers.get();
    aggregate.set(mask, physical_state.intersects(group));
    modifiers.set(aggregate);
    true
}

fn is_caps_lock(key: u32) -> bool {
    scancode::Linux::try_from(key).is_ok_and(|key| key == scancode::Linux::KeyCapsLock)
}

/// The side-agnostic modifier state a snapshot carries.
fn modifier_kinds(mods: XMods) -> ModifierKinds {
    let mut kinds = ModifierKinds::default();
    if mods.contains(XMods::ShiftMask) {
        kinds = kinds.with(ModifierKinds::SHIFT);
    }
    if mods.contains(XMods::ControlMask) {
        kinds = kinds.with(ModifierKinds::CONTROL);
    }
    // Mod5 is ISO_Level3_Shift (AltGr on Linux); treat it as macOS Option,
    // which is how the backend has always mapped it.
    if mods.contains(XMods::Mod1Mask) || mods.contains(XMods::Mod5Mask) {
        kinds = kinds.with(ModifierKinds::OPTION);
    }
    if mods.contains(XMods::Mod4Mask) {
        kinds = kinds.with(ModifierKinds::COMMAND);
    }
    kinds
}

/// The sides the event stream identified. [`ModifierSides`] mirrors
/// [`PhysicalModifiers`]' bit order, so this is a bit copy.
fn modifier_sides(physical: PhysicalModifiers) -> ModifierSides {
    ModifierSides::from_bits_truncate(physical.bits() as u8)
}

/// The window server's current modifier flags.
///
/// `IOHIDPostEvent` replaces the whole global modifier state, so the remote
/// snapshot is merged with whatever the local keyboard already holds instead
/// of clearing it.
fn local_modifier_flags() -> u32 {
    let state_id = CGEventSourceStateID::CombinedSessionState as c_int;
    unsafe { CGEventSourceFlagsState(state_id) as u32 }
}

fn toggle_caps_lock(modifiers: &Cell<XMods>) {
    let mut state = modifiers.get();
    state.toggle(XMods::LockMask);
    modifiers.set(state);
}

fn set_modifiers(
    physical: &Cell<PhysicalModifiers>,
    active_modifiers: &Cell<XMods>,
    depressed: u32,
    latched: u32,
    locked: u32,
    group: u32,
) {
    // Modifier indices beyond the conventional low eight bits are valid with
    // custom XKB keymaps. Preserve every known bit instead of dropping the
    // entire field when an unknown bit accompanies it.
    let depressed = XMods::from_bits_truncate(depressed);
    let latched = XMods::from_bits_truncate(latched);
    let locked = XMods::from_bits_truncate(locked);
    let _group = XMods::from_bits_truncate(group);

    let snapshot = depressed | latched | locked;
    active_modifiers.replace(snapshot);

    // A modifier snapshot is authoritative for aggregate state, but cannot
    // identify a physical side. Retain a known side only while its aggregate
    // bit is still present so a later key-up cannot revive stale state.
    let mut physical_state = physical.get();
    for (group, mask) in [
        (PhysicalModifiers::SHIFT, XMods::ShiftMask),
        (PhysicalModifiers::CONTROL, XMods::ControlMask),
        (PhysicalModifiers::OPTION, XMods::Mod1Mask),
        (PhysicalModifiers::COMMAND, XMods::Mod4Mask),
    ] {
        if !snapshot.contains(mask) {
            physical_state.remove(group);
        }
    }
    physical.set(physical_state);
}

bitflags! {
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    struct PhysicalModifiers: u16 {
        const LEFT_SHIFT = 1 << 0;
        const RIGHT_SHIFT = 1 << 1;
        const LEFT_CONTROL = 1 << 2;
        const RIGHT_CONTROL = 1 << 3;
        const LEFT_OPTION = 1 << 4;
        const RIGHT_OPTION = 1 << 5;
        const LEFT_COMMAND = 1 << 6;
        const RIGHT_COMMAND = 1 << 7;

        const SHIFT = Self::LEFT_SHIFT.bits() | Self::RIGHT_SHIFT.bits();
        const CONTROL = Self::LEFT_CONTROL.bits() | Self::RIGHT_CONTROL.bits();
        const OPTION = Self::LEFT_OPTION.bits() | Self::RIGHT_OPTION.bits();
        const COMMAND = Self::LEFT_COMMAND.bits() | Self::RIGHT_COMMAND.bits();
    }
}

// From X11/X.h
bitflags! {
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
    struct XMods: u32 {
        const ShiftMask = (1<<0);
        const LockMask = (1<<1);
        const ControlMask = (1<<2);
        const Mod1Mask = (1<<3);
        const Mod2Mask = (1<<4);
        const Mod3Mask = (1<<5);
        const Mod4Mask = (1<<6);
        const Mod5Mask = (1<<7);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ModifierKinds, ModifierSides, PhysicalModifiers, XMods, button_event_spec, cg_event_flags,
        commit_button_state, is_caps_lock, modifier_kinds, modifier_sides, set_modifiers,
        toggle_caps_lock, union_to_global, update_modifiers,
    };
    use crate::macos_keyboard::ModifierSide;
    use core_graphics::event::CGEventFlags;
    use input_event::{
        BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT,
        scancode::Linux::{
            self, KeyCapsLock, KeyLeftAlt, KeyLeftCtrl, KeyLeftMeta, KeyLeftShift, KeyRightCtrl,
            KeyRightShift, KeyRightalt, KeyRightmeta,
        },
    };
    use std::cell::Cell;
    use std::collections::HashSet;

    /// `NX_DEVICERCTLKEYMASK`, the device-dependent right Control bit.
    const NX_DEVICERCTLKEYMASK: u32 = 0x0000_2000;

    #[test]
    fn cg_event_fallback_keeps_the_device_dependent_modifier_bits() {
        // `core-graphics` declares only the general modifier bits, so a
        // truncating conversion would drop the side information the fallback
        // needs to release one Control without clearing its peer.
        let flags = crate::macos_keyboard::NX_CONTROLMASK | NX_DEVICERCTLKEYMASK;
        assert_eq!(cg_event_flags(flags).bits(), u64::from(flags));
        assert_ne!(
            CGEventFlags::from_bits_truncate(u64::from(flags)).bits(),
            u64::from(flags),
            "core-graphics now declares the device bits; the comment above is stale"
        );
    }

    #[test]
    fn modifier_sides_mirror_the_physical_modifier_bits() {
        for (physical, side) in [
            (PhysicalModifiers::LEFT_SHIFT, ModifierSide::LeftShift),
            (PhysicalModifiers::RIGHT_SHIFT, ModifierSide::RightShift),
            (PhysicalModifiers::LEFT_CONTROL, ModifierSide::LeftControl),
            (PhysicalModifiers::RIGHT_CONTROL, ModifierSide::RightControl),
            (PhysicalModifiers::LEFT_OPTION, ModifierSide::LeftOption),
            (PhysicalModifiers::RIGHT_OPTION, ModifierSide::RightOption),
            (PhysicalModifiers::LEFT_COMMAND, ModifierSide::LeftCommand),
            (PhysicalModifiers::RIGHT_COMMAND, ModifierSide::RightCommand),
        ] {
            assert_eq!(modifier_sides(physical), side.bit(), "{physical:?}");
        }
        assert_eq!(
            modifier_sides(PhysicalModifiers::empty()),
            ModifierSides::default()
        );
    }

    #[test]
    fn modifier_kinds_follow_the_aggregate_snapshot() {
        assert_eq!(modifier_kinds(XMods::empty()).bits(), 0);
        assert_eq!(
            modifier_kinds(XMods::ShiftMask).bits(),
            ModifierKinds::SHIFT.bits()
        );
        assert_eq!(
            modifier_kinds(XMods::ControlMask).bits(),
            ModifierKinds::CONTROL.bits()
        );
        assert_eq!(
            modifier_kinds(XMods::Mod1Mask).bits(),
            ModifierKinds::OPTION.bits()
        );
        // Mod5 is AltGr; the backend has always treated it as Option.
        assert_eq!(
            modifier_kinds(XMods::Mod5Mask).bits(),
            ModifierKinds::OPTION.bits()
        );
        assert_eq!(
            modifier_kinds(XMods::Mod4Mask).bits(),
            ModifierKinds::COMMAND.bits()
        );
        // Caps Lock is not a modifier transition: macOS owns the locked bit,
        // so it never reaches the injected flags.
        assert_eq!(modifier_kinds(XMods::LockMask).bits(), 0);
        assert_eq!(
            modifier_kinds(XMods::ShiftMask | XMods::ControlMask | XMods::Mod4Mask).bits(),
            ModifierKinds::SHIFT.bits()
                | ModifierKinds::CONTROL.bits()
                | ModifierKinds::COMMAND.bits()
        );
    }

    #[test]
    fn union_to_global_is_identity_when_the_primary_is_top_left() {
        assert_eq!(union_to_global((0.0, 0.0), 0, 0), (0.0, 0.0));
        assert_eq!(union_to_global((0.0, 0.0), 1919, 1079), (1919.0, 1079.0));
    }

    #[test]
    fn union_to_global_reapplies_a_negative_origin() {
        // A 1920x1200 display left of and slightly above a 1920x1080
        // primary: global coordinates are anchored at the primary's
        // top-left, so the union's own origin sits at (-1920, -120).
        let origin = (-1920.0, -120.0);
        assert_eq!(union_to_global(origin, 0, 0), (-1920.0, -120.0));
        // The primary's top-left, reached from union coordinates.
        assert_eq!(union_to_global(origin, 1920, 120), (0.0, 0.0));
        // The far corner of the union: the primary's bottom-right.
        assert_eq!(union_to_global(origin, 3839, 1199), (1919.0, 1079.0));
    }

    #[test]
    fn caps_lock_is_a_tap_not_a_repeatable_modifier() {
        let physical = Cell::new(PhysicalModifiers::empty());
        let modifiers = Cell::new(XMods::empty());

        assert!(is_caps_lock(KeyCapsLock as u32));
        assert!(!update_modifiers(
            &physical,
            &modifiers,
            KeyCapsLock as u32,
            1
        ));
        assert!(physical.get().is_empty());
        assert!(modifiers.get().is_empty());

        toggle_caps_lock(&modifiers);
        assert_eq!(modifiers.get(), XMods::LockMask);
        toggle_caps_lock(&modifiers);
        assert!(modifiers.get().is_empty());
    }

    #[test]
    fn ordinary_modifiers_still_track_key_state() {
        let physical = Cell::new(PhysicalModifiers::empty());
        let modifiers = Cell::new(XMods::empty());

        assert!(update_modifiers(
            &physical,
            &modifiers,
            KeyLeftShift as u32,
            1
        ));
        assert_eq!(physical.get(), PhysicalModifiers::LEFT_SHIFT);
        assert_eq!(modifiers.get(), XMods::ShiftMask);
        assert!(update_modifiers(
            &physical,
            &modifiers,
            KeyLeftShift as u32,
            0
        ));
        assert!(physical.get().is_empty());
        assert!(modifiers.get().is_empty());
    }

    #[test]
    fn releasing_one_modifier_side_preserves_the_other_side() {
        for (left, right, mask) in [
            (KeyLeftShift, KeyRightShift, XMods::ShiftMask),
            (KeyLeftCtrl, KeyRightCtrl, XMods::ControlMask),
            (KeyLeftAlt, KeyRightalt, XMods::Mod1Mask),
            (KeyLeftMeta, KeyRightmeta, XMods::Mod4Mask),
        ] {
            let physical = Cell::new(PhysicalModifiers::empty());
            let modifiers = Cell::new(XMods::empty());

            for (key, state) in [(left, 1), (right, 1), (left, 0)] {
                assert!(update_modifiers(&physical, &modifiers, key as u32, state));
            }
            assert_eq!(modifiers.get() & mask, mask, "{left:?}/{right:?}");

            assert!(update_modifiers(&physical, &modifiers, right as u32, 0));
            assert!(modifiers.get().is_empty(), "{left:?}/{right:?}");
        }
    }

    #[test]
    fn explicit_modifier_snapshot_discards_stale_physical_sides() {
        let physical = Cell::new(PhysicalModifiers::empty());
        let modifiers = Cell::new(XMods::empty());
        assert!(update_modifiers(
            &physical,
            &modifiers,
            Linux::KeyLeftShift as u32,
            1
        ));

        set_modifiers(&physical, &modifiers, 0, 0, 0, 0);
        assert!(physical.get().is_empty());
        assert!(modifiers.get().is_empty());

        assert!(update_modifiers(
            &physical,
            &modifiers,
            Linux::KeyLeftShift as u32,
            0
        ));
        assert!(modifiers.get().is_empty());
    }

    #[test]
    fn button_specs_cover_every_tracked_button_and_reject_bad_states() {
        for button in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_BACK, BTN_FORWARD] {
            assert!(button_event_spec(button, 1).is_some());
            assert!(button_event_spec(button, 0).is_some());
            assert!(button_event_spec(button, 2).is_none());
        }
        assert_eq!(
            button_event_spec(BTN_BACK, 1).unwrap().button_number,
            Some(3)
        );
        assert_eq!(
            button_event_spec(BTN_FORWARD, 1).unwrap().button_number,
            Some(4)
        );
        assert_eq!(
            button_event_spec(BTN_MIDDLE, 1).unwrap().button_number,
            None
        );
    }

    #[test]
    fn button_tracking_commits_down_and_up_independently() {
        let mut pressed = HashSet::new();
        commit_button_state(&mut pressed, BTN_LEFT, 1);
        commit_button_state(&mut pressed, BTN_RIGHT, 1);
        commit_button_state(&mut pressed, BTN_LEFT, 0);

        assert_eq!(pressed, HashSet::from([BTN_RIGHT]));
    }

    #[test]
    fn modifier_snapshot_uses_depressed_latched_and_locked_masks() {
        let physical = Cell::new(PhysicalModifiers::empty());
        let modifiers = Cell::new(XMods::empty());

        set_modifiers(
            &physical,
            &modifiers,
            XMods::ShiftMask.bits(),
            XMods::ControlMask.bits(),
            XMods::LockMask.bits(),
            0,
        );

        assert_eq!(
            modifiers.get(),
            XMods::ShiftMask | XMods::ControlMask | XMods::LockMask
        );
    }

    #[test]
    fn modifier_snapshot_keeps_known_bits_alongside_unknown_xkb_bits() {
        let physical = Cell::new(PhysicalModifiers::empty());
        let modifiers = Cell::new(XMods::empty());

        set_modifiers(
            &physical,
            &modifiers,
            XMods::ShiftMask.bits() | (1 << 31),
            0,
            0,
            0,
        );

        assert_eq!(modifiers.get(), XMods::ShiftMask);
    }
}
