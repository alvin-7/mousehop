//! macOS keyboard injection planning.
//!
//! macOS only applies its own keyboard shortcuts (Control+Right Arrow moves
//! right a space, for instance) when a modifier transition arrives as real
//! HID input: an `NX_FLAGSCHANGED` event that carries the physical key code
//! of the side that changed, the general modifier bits, and the matching
//! device-dependent side bit. The backend used to post a `CGEvent`
//! `FlagsChanged` event with no key code and only general flags, which the
//! window server accepts as a modifier state but never as a physical Control
//! press, so the system shortcut never fired.
//!
//! This module owns the platform-independent half of the replacement: which
//! modifier sides are held, the flags that describe them, the transitions to
//! post, and the policy for falling back to `CGEvent` when `IOHIDPostEvent`
//! is unavailable or fails. `macos.rs` performs the call itself.
//!
//! Nothing here knows about the window server, so the whole state machine is
//! unit tested on every platform.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Stable log target for every macOS keyboard injection diagnostic.
///
/// Keeping the target fixed lets a support filter raise exactly this stream
/// (`mousehop::keyboard=debug`) without turning on global debug logging for
/// the high-frequency pointer path.
pub(crate) const KEYBOARD_LOG_TARGET: &str = "mousehop::keyboard";

/// Hands out stable per-backend identifiers for diagnostics.
///
/// One number identifies one `KeyboardInjector`, which is one emulation
/// backend instance; clones (the key-repeat task) share the number, so every
/// line of one session can be grouped.
static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

fn next_session_id() -> u64 {
    NEXT_SESSION.fetch_add(1, Ordering::Relaxed)
}

/// `kVK_CapsLock`.
const CAPS_LOCK_KEY_CODE: u16 = 0x39;
/// `kVK_LeftArrow`.
const LEFT_ARROW_KEY_CODE: u16 = 0x7B;
/// `kVK_RightArrow`.
const RIGHT_ARROW_KEY_CODE: u16 = 0x7C;
/// `kVK_DownArrow`.
const DOWN_ARROW_KEY_CODE: u16 = 0x7D;
/// `kVK_UpArrow`.
const UP_ARROW_KEY_CODE: u16 = 0x7E;

/// Coarse key classification used by diagnostics.
///
/// Ordinary keys are never identified beyond this category: diagnostics must
/// not reveal what the user typed. Arrows, Caps Lock and modifier sides are
/// what shortcut investigations correlate, so those are named.
pub(crate) fn key_category(key_code: u16) -> &'static str {
    match key_code {
        CAPS_LOCK_KEY_CODE => "caps_lock",
        LEFT_ARROW_KEY_CODE | RIGHT_ARROW_KEY_CODE | DOWN_ARROW_KEY_CODE | UP_ARROW_KEY_CODE => {
            "arrow"
        }
        code if ModifierSide::from_key_code(code).is_some() => "modifier",
        _ => "ordinary",
    }
}

/// Stable API-path names for diagnostics.
pub(crate) fn path_name(path: InjectionPath) -> &'static str {
    match path {
        InjectionPath::Nx => "iohid",
        InjectionPath::CgEvent => "cgevent",
    }
}

/// Human-readable modifier-kind names, `none` when empty.
pub(crate) fn describe_kinds(kinds: ModifierKinds) -> String {
    let mut names = Vec::new();
    if kinds.contains(ModifierKinds::SHIFT) {
        names.push("shift");
    }
    if kinds.contains(ModifierKinds::CONTROL) {
        names.push("control");
    }
    if kinds.contains(ModifierKinds::OPTION) {
        names.push("option");
    }
    if kinds.contains(ModifierKinds::COMMAND) {
        names.push("command");
    }
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join("+")
    }
}

/// Human-readable side names joined with `+`, `none` when empty.
pub(crate) fn describe_sides(sides: ModifierSides) -> String {
    let names: Vec<&str> = ModifierSide::ALL
        .iter()
        .filter(|side| sides.contains(side.bit()))
        .map(|side| side.name())
        .collect();
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join("+")
    }
}

/// Redacted injection details for diagnostics.
///
/// Ordinary keys are described only by category so failure logs never record
/// what the user typed. Modifier transitions, arrows and Caps Lock carry their
/// identity because they are the events shortcut investigations have to
/// correlate.
pub(crate) fn describe_injection(injection: Injection) -> String {
    match injection {
        Injection::Modifiers { key_code, flags } => {
            let side = ModifierSide::from_key_code(key_code)
                .map_or_else(|| "unknown".to_string(), |side| side.name().to_string());
            format!("modifiers side={side} key_code={key_code:#06x} flags={flags:#010x}")
        }
        Injection::Key { key_code, down, .. } => {
            let category = key_category(key_code);
            if category == "ordinary" {
                format!("key category={category} down={down}")
            } else {
                format!("key category={category} key_code={key_code:#06x} down={down}")
            }
        }
    }
}

/// General (device-independent) Shift bit. `IOHIDPostEvent` and `CGEvent`
/// use the same numeric values for these bits.
pub(crate) const NX_SHIFTMASK: u32 = 0x0002_0000;
/// Caps Lock's locked bit. macOS owns this bit: the backend only carries
/// whatever the local session already reports and never derives it from the
/// remote snapshot.
pub(crate) const NX_ALPHASHIFTMASK: u32 = 0x0001_0000;
/// General Control bit.
pub(crate) const NX_CONTROLMASK: u32 = 0x0004_0000;
/// General Option bit.
pub(crate) const NX_ALTERNATEMASK: u32 = 0x0008_0000;
/// General Command bit.
pub(crate) const NX_COMMANDMASK: u32 = 0x0010_0000;

/// Device-dependent left Control bit.
const NX_DEVICELCTLKEYMASK: u32 = 0x0000_0001;
/// Device-dependent left Shift bit.
const NX_DEVICELSHIFTKEYMASK: u32 = 0x0000_0002;
/// Device-dependent right Shift bit.
const NX_DEVICERSHIFTKEYMASK: u32 = 0x0000_0004;
/// Device-dependent left Command bit.
const NX_DEVICELCMDKEYMASK: u32 = 0x0000_0008;
/// Device-dependent right Command bit.
const NX_DEVICERCMDKEYMASK: u32 = 0x0000_0010;
/// Device-dependent left Option bit.
const NX_DEVICELALTKEYMASK: u32 = 0x0000_0020;
/// Device-dependent right Option bit.
const NX_DEVICERALTKEYMASK: u32 = 0x0000_0040;
/// Device-dependent right Control bit.
const NX_DEVICERCTLKEYMASK: u32 = 0x0000_2000;

/// Every flag bit this backend writes.
///
/// `IOHIDPostEvent` replaces the whole global modifier state, so the bits
/// outside this mask belong to the local user and are never sent.
pub(crate) const MANAGED_FLAG_MASKS: u32 = NX_ALPHASHIFTMASK
    | NX_SHIFTMASK
    | NX_CONTROLMASK
    | NX_ALTERNATEMASK
    | NX_COMMANDMASK
    | NX_DEVICELCTLKEYMASK
    | NX_DEVICELSHIFTKEYMASK
    | NX_DEVICERSHIFTKEYMASK
    | NX_DEVICELCMDKEYMASK
    | NX_DEVICERCMDKEYMASK
    | NX_DEVICELALTKEYMASK
    | NX_DEVICERALTKEYMASK
    | NX_DEVICERCTLKEYMASK;

/// One physical side of a modifier key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ModifierSide {
    LeftShift,
    RightShift,
    LeftControl,
    RightControl,
    LeftOption,
    RightOption,
    LeftCommand,
    RightCommand,
}

impl ModifierSide {
    /// Every side, in bit order.
    pub(crate) const ALL: [Self; 8] = [
        Self::LeftShift,
        Self::RightShift,
        Self::LeftControl,
        Self::RightControl,
        Self::LeftOption,
        Self::RightOption,
        Self::LeftCommand,
        Self::RightCommand,
    ];

    /// The fixed macOS virtual key code (`kVK_*`) of this side.
    ///
    /// These are virtual key codes, not layout translations: the window
    /// server needs the code of the key that changed alongside the flags.
    /// Some input methods read a missing code as the `a` key.
    pub(crate) const fn key_code(self) -> u16 {
        match self {
            Self::LeftShift => 0x38,
            Self::RightShift => 0x3C,
            Self::LeftControl => 0x3B,
            Self::RightControl => 0x3E,
            Self::LeftOption => 0x3A,
            Self::RightOption => 0x3D,
            Self::LeftCommand => 0x37,
            Self::RightCommand => 0x36,
        }
    }

    /// The side whose [`Self::key_code`] this is, if any.
    pub(crate) fn from_key_code(key_code: u16) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|side| side.key_code() == key_code)
    }

    /// Stable diagnostic name for this side.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::LeftShift => "left_shift",
            Self::RightShift => "right_shift",
            Self::LeftControl => "left_control",
            Self::RightControl => "right_control",
            Self::LeftOption => "left_option",
            Self::RightOption => "right_option",
            Self::LeftCommand => "left_command",
            Self::RightCommand => "right_command",
        }
    }

    /// The side-agnostic bit this side contributes to a snapshot.
    const fn kind_bits(self) -> u8 {
        match self {
            Self::LeftShift | Self::RightShift => ModifierKinds::SHIFT.bits(),
            Self::LeftControl | Self::RightControl => ModifierKinds::CONTROL.bits(),
            Self::LeftOption | Self::RightOption => ModifierKinds::OPTION.bits(),
            Self::LeftCommand | Self::RightCommand => ModifierKinds::COMMAND.bits(),
        }
    }

    /// The bit identifying this side in a [`ModifierSides`] set.
    pub(crate) const fn bit(self) -> ModifierSides {
        ModifierSides(1 << self.index())
    }

    /// The other side of the same key.
    pub(crate) const fn sibling(self) -> Self {
        match self {
            Self::LeftShift => Self::RightShift,
            Self::RightShift => Self::LeftShift,
            Self::LeftControl => Self::RightControl,
            Self::RightControl => Self::LeftControl,
            Self::LeftOption => Self::RightOption,
            Self::RightOption => Self::LeftOption,
            Self::LeftCommand => Self::RightCommand,
            Self::RightCommand => Self::LeftCommand,
        }
    }

    /// Whether this is the side used when a snapshot names no side.
    pub(crate) const fn is_default_side(self) -> bool {
        matches!(
            self,
            Self::LeftShift | Self::LeftControl | Self::LeftOption | Self::LeftCommand
        )
    }

    /// Position of this side in [`Self::ALL`], and therefore in a
    /// [`ModifierSides`] set.
    const fn index(self) -> u8 {
        match self {
            Self::LeftShift => 0,
            Self::RightShift => 1,
            Self::LeftControl => 2,
            Self::RightControl => 3,
            Self::LeftOption => 4,
            Self::RightOption => 5,
            Self::LeftCommand => 6,
            Self::RightCommand => 7,
        }
    }

    /// The general bit this side sets in the window server's flags.
    const fn general_mask(self) -> u32 {
        match self {
            Self::LeftShift | Self::RightShift => NX_SHIFTMASK,
            Self::LeftControl | Self::RightControl => NX_CONTROLMASK,
            Self::LeftOption | Self::RightOption => NX_ALTERNATEMASK,
            Self::LeftCommand | Self::RightCommand => NX_COMMANDMASK,
        }
    }

    /// The device-dependent bit that tells the window server which side is
    /// down.
    const fn device_mask(self) -> u32 {
        match self {
            Self::LeftShift => NX_DEVICELSHIFTKEYMASK,
            Self::RightShift => NX_DEVICERSHIFTKEYMASK,
            Self::LeftControl => NX_DEVICELCTLKEYMASK,
            Self::RightControl => NX_DEVICERCTLKEYMASK,
            Self::LeftOption => NX_DEVICELALTKEYMASK,
            Self::RightOption => NX_DEVICERALTKEYMASK,
            Self::LeftCommand => NX_DEVICELCMDKEYMASK,
            Self::RightCommand => NX_DEVICERCMDKEYMASK,
        }
    }
}

/// The side-agnostic modifier state the event stream reports.
///
/// X11 (and therefore Mousehop's protocol) mostly names modifiers without a
/// side; this is the aggregate half of the model.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ModifierKinds(u8);

impl ModifierKinds {
    /// Shift is depressed.
    pub(crate) const SHIFT: Self = Self(1 << 0);
    /// Control is depressed.
    pub(crate) const CONTROL: Self = Self(1 << 1);
    /// Option (X11 `Mod1`/`Mod5`) is depressed.
    pub(crate) const OPTION: Self = Self(1 << 2);
    /// Command (X11 `Mod4`) is depressed.
    pub(crate) const COMMAND: Self = Self(1 << 3);

    /// The raw bits.
    pub(crate) const fn bits(self) -> u8 {
        self.0
    }

    /// Whether every bit of `other` is present.
    pub(crate) const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether the bit field produced by [`ModifierSide::kind_bits`] is
    /// present.
    const fn contains_kind(self, bits: u8) -> bool {
        self.0 & bits == bits
    }

    /// Adds `other`.
    pub(crate) const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// The modifier sides macOS should currently consider held.
///
/// The bit order matches `PhysicalModifiers` in `macos.rs`, so the
/// conversion is a bit copy.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ModifierSides(u8);

impl ModifierSides {
    /// Builds a set from the raw bits above.
    pub(crate) const fn from_bits_truncate(bits: u8) -> Self {
        Self(bits)
    }

    /// Whether no side is held.
    pub(crate) const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Whether every bit of `other` is present.
    pub(crate) const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether any bit of `other` is present.
    pub(crate) const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    /// Adds `other`.
    pub(crate) const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Removes `other`.
    pub(crate) const fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }
}

/// The window server's flag value for a set of held sides.
pub(crate) fn flags_for(sides: ModifierSides) -> u32 {
    let mut flags = 0;
    for side in ModifierSide::ALL {
        if sides.contains(side.bit()) {
            flags |= side.general_mask() | side.device_mask();
        }
    }
    flags
}

/// The sides macOS should hold for `aggregate`, given the sides the event
/// stream has actually identified.
///
/// An aggregate snapshot cannot name a side, but the injected state must
/// still be concrete. Known sides are kept while their kind stays depressed;
/// a kind with no known side falls back to the left side, and a later
/// explicit side event migrates off that invented side rather than leaving
/// both down.
pub(crate) fn desired_sides(aggregate: ModifierKinds, known: ModifierSides) -> ModifierSides {
    let mut desired = ModifierSides::default();
    for side in ModifierSide::ALL {
        if known.contains(side.bit()) && aggregate.contains_kind(side.kind_bits()) {
            desired = desired.with(side.bit());
        }
    }
    for side in ModifierSide::ALL {
        if !side.is_default_side() || !aggregate.contains_kind(side.kind_bits()) {
            continue;
        }
        if desired.intersects(side.bit().with(side.sibling().bit())) {
            continue;
        }
        desired = desired.with(side.bit());
    }
    desired
}

/// The modifier bits the local user's keyboard still holds.
///
/// `local_flags` is the state the window server last reported and
/// `remote_flags` the bits the remote snapshot is responsible for at that
/// moment. The window server reports the combined state, so every other bit
/// inside [`MANAGED_FLAG_MASKS`] belongs to the physical keyboard and has to
/// survive: a remote snapshot may not clear a locally held modifier.
///
/// The result is a baseline for a whole transition batch, never for a single
/// event. A batch shrinks `remote_flags` as it posts each release, so
/// recomputing the baseline per step would reclassify a side this backend has
/// just released as locally held and leave that side's flag bits turned on.
///
/// Bits the remote and the local user both hold are indistinguishable through
/// this API — the window server reports one Control bit either way — so the
/// remote state wins there and the local key keeps toggling the same bit. The
/// worst case is that releasing the remote Control also drops a locally held
/// Control that the local user is still holding; releasing that physical key
/// still clears the bit.
pub(crate) fn locally_held_flags(local_flags: u32, remote_flags: u32) -> u32 {
    local_flags & !remote_flags & MANAGED_FLAG_MASKS
}

/// The modifier transitions that take `held` to `desired`, in the order they
/// must be posted.
///
/// Each entry also carries the full side set in effect right after that
/// transition, because every flags-changed event has to describe the complete
/// modifier state, not just the key that changed.
pub(crate) fn transitions(
    held: ModifierSides,
    desired: ModifierSides,
) -> Vec<(ModifierSide, ModifierSides)> {
    let mut steps = Vec::new();
    let mut current = held;
    for side in ModifierSide::ALL {
        if held.contains(side.bit()) && !desired.contains(side.bit()) {
            current = current.without(side.bit());
            steps.push((side, current));
        }
    }
    for side in ModifierSide::ALL {
        if desired.contains(side.bit()) && !held.contains(side.bit()) {
            current = current.with(side.bit());
            steps.push((side, current));
        }
    }
    steps
}

/// Which API carries a keyboard event to the window server.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InjectionPath {
    /// `IOHIDPostEvent` with real `NXEventData`. This is the only path that
    /// makes macOS treat injected input like physical HID input.
    Nx,
    /// Best-effort `CGEvent` fallback. It still types, but macOS does not
    /// treat these events as physical HID input, so system shortcuts are not
    /// guaranteed on this path.
    CgEvent,
}

/// One planned keyboard injection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Injection {
    /// A modifier transition: `NX_FLAGSCHANGED` on the IOHID path, a
    /// `FlagsChanged` event with an explicit key code on the fallback path.
    Modifiers {
        /// The virtual key code of the side that changed.
        key_code: u16,
        /// The complete modifier flags to publish.
        flags: u32,
    },
    /// An ordinary key.
    Key {
        /// The virtual key code.
        key_code: u16,
        /// Whether this is a key down.
        down: bool,
        /// The complete modifier flags, used **only** by the `CGEvent`
        /// fallback, which has to restate the flags on every event. The IOHID
        /// path posts ordinary keys with no global flags so the window server
        /// keeps the state its flags-changed events established.
        cg_flags: u32,
    },
}

/// The macOS-facing half of the injector.
pub(crate) trait KeySink {
    /// The path this sink can carry keyboard events on.
    fn preferred_path(&self) -> InjectionPath;

    /// Posts one injection, returning the raw platform status on failure.
    fn inject(&self, path: InjectionPath, injection: Injection) -> Result<(), i32>;
}

/// Input this backend injected and has not confirmed released.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct CleanupResidue {
    /// How many ordinary keys are still recorded as pressed.
    pub(crate) pressed_keys: usize,
    /// Modifier sides still recorded as held.
    pub(crate) modifier_sides: ModifierSides,
}

impl CleanupResidue {
    /// Whether cleanup confirmed every injected input was released.
    pub(crate) fn is_empty(self) -> bool {
        self.pressed_keys == 0 && self.modifier_sides.is_empty()
    }
}

/// The modifier snapshot state that was last logged.
///
/// Comparing whole snapshots keeps diagnostics on "state changed" events
/// only: repeated identical snapshots (every keystroke without modifiers)
/// stay silent.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct SnapshotLog {
    /// The aggregate state the remote reported.
    pub(crate) kinds: ModifierKinds,
    /// The sides the event stream identified.
    pub(crate) known: ModifierSides,
    /// The window server's combined state; includes this backend's own
    /// injections, so it is observed state, not purely physical keys.
    pub(crate) observed_flags: u32,
    /// The sides macOS has been told are down.
    pub(crate) injected_sides: ModifierSides,
    /// The flags the last successful injection published.
    pub(crate) posted_flags: u32,
}

/// Returns whether this snapshot differs from the last logged one, recording
/// it either way so unchanged snapshots stay silent.
fn snapshot_changed(state: &mut InjectorState, entry: SnapshotLog) -> bool {
    if state.last_snapshot_log == Some(entry) {
        false
    } else {
        state.last_snapshot_log = Some(entry);
        true
    }
}

/// The `path_degraded` diagnostics line.
///
/// `dropped` is the total number of presses held back while the failed path
/// was cleaned up. The notice names the recovery condition and never claims
/// the fallback is incapable of system shortcuts: `CGEvent` is simply not
/// guaranteed to trigger them.
pub(crate) fn degradation_message(session: u64, dropped: u64) -> String {
    format!(
        "event=path_degraded session={session} reason=iohid_injection_failure new_path=cgevent \
         dropped_presses_total={dropped} \
         recovery_condition=every_injected_key_and_modifier_released \
         iohid_retry=only_in_a_new_backend_session \
         note=cgevent_types_but_does_not_guarantee_macos_system_shortcuts"
    )
}

/// Injects remote keyboard input on macOS while keeping the injected state
/// consistent.
///
/// The state machine tracks what macOS has been told, releases exactly those
/// keys, and stops injecting new ordinary keys once the IOHID path has
/// failed, so a half-applied shortcut is never replayed and no key is left
/// down.
pub(crate) struct KeyboardInjector {
    sink: Rc<dyn KeySink>,
    state: Rc<RefCell<InjectorState>>,
    /// Stable diagnostic identifier for this backend instance; clones (the
    /// key-repeat task) keep it, so all their lines group together.
    session: u64,
}

impl Clone for KeyboardInjector {
    fn clone(&self) -> Self {
        Self {
            sink: Rc::clone(&self.sink),
            state: Rc::clone(&self.state),
            session: self.session,
        }
    }
}

#[derive(Debug)]
struct InjectorState {
    /// Path currently used for new injections.
    path: InjectionPath,
    /// Sides macOS has been told are down.
    held_sides: ModifierSides,
    /// Modifier flags the remote snapshot is responsible for.
    remote_flags: u32,
    /// Flags the last successful injection published.
    post_flags: u32,
    /// Modifier flags the local session reported at the last snapshot.
    local_flags: u32,
    /// Ordinary keys injected down and not released yet.
    pressed_keys: BTreeSet<u16>,
    /// Set when the IOHID path failed while input is still recorded, which
    /// keeps new presses out until the recorded input is released.
    degraded: bool,
    /// Ordinary presses dropped while degraded, for rate limited logging.
    dropped_presses: u64,
    /// The last snapshot written to diagnostics, to suppress repeats.
    last_snapshot_log: Option<SnapshotLog>,
}

impl KeyboardInjector {
    /// Creates an injector that posts through `sink`.
    pub(crate) fn new(sink: Rc<dyn KeySink>) -> Self {
        let path = sink.preferred_path();
        let session = next_session_id();
        Self {
            sink,
            session,
            state: Rc::new(RefCell::new(InjectorState {
                path,
                held_sides: ModifierSides::default(),
                remote_flags: 0,
                post_flags: 0,
                local_flags: 0,
                pressed_keys: BTreeSet::new(),
                degraded: false,
                dropped_presses: 0,
                last_snapshot_log: None,
            })),
        }
    }

    /// The stable diagnostic identifier of this backend instance.
    pub(crate) fn session(&self) -> u64 {
        self.session
    }

    /// The path new injections currently use.
    pub(crate) fn current_path(&self) -> InjectionPath {
        self.state.borrow().path
    }

    /// Input this backend injected and has not confirmed released.
    pub(crate) fn pending(&self) -> CleanupResidue {
        let state = self.state.borrow();
        CleanupResidue {
            pressed_keys: state.pressed_keys.len(),
            modifier_sides: state.held_sides,
        }
    }

    /// Publishes the remote modifier snapshot.
    ///
    /// `aggregate` is the remote's side-agnostic state, `known` the sides the
    /// event stream identified, and `local_flags` the window server's current
    /// modifier flags.
    pub(crate) fn sync_modifiers(
        &self,
        aggregate: ModifierKinds,
        known: ModifierSides,
        local_flags: u32,
    ) {
        let desired = desired_sides(aggregate, known);
        let mut state = self.state.borrow_mut();
        state.local_flags = local_flags;
        // One baseline for the whole batch: `remote_flags` moves as the steps
        // below are posted, and a per-step baseline would turn the sides this
        // backend has just released into locally held bits.
        let locally_held = locally_held_flags(local_flags, state.remote_flags);
        for (side, next) in transitions(state.held_sides, desired) {
            let flags = locally_held | flags_for(next);
            let injection = Injection::Modifiers {
                key_code: side.key_code(),
                flags,
            };
            if self.post(&mut state, injection).is_err() {
                // Leave the recorded state at the last side macOS accepted;
                // cleanup releases exactly that.
                break;
            }
            state.held_sides = next;
            state.remote_flags = flags_for(next);
            state.post_flags = flags;
        }
        self.settle(&mut state);
        self.log_snapshot(&mut state, aggregate, known, desired);
    }

    /// Writes one `modifier_snapshot` line per distinct state.
    ///
    /// `observed_flags` is the window server's combined session state, which
    /// already contains this backend's injections: the field is deliberately
    /// named "observed", never "physical".
    fn log_snapshot(
        &self,
        state: &mut InjectorState,
        aggregate: ModifierKinds,
        known: ModifierSides,
        expected: ModifierSides,
    ) {
        let entry = SnapshotLog {
            kinds: aggregate,
            known,
            observed_flags: state.local_flags,
            injected_sides: state.held_sides,
            posted_flags: state.post_flags,
        };
        if snapshot_changed(state, entry) {
            log::debug!(
                target: KEYBOARD_LOG_TARGET,
                "event=modifier_snapshot session={} observed_flags={:#010x} \
                 observed_source=combined_session_state kinds={} known_sides={} \
                 expected_sides={} injected_sides={} posted_flags={:#010x}",
                self.session,
                state.local_flags,
                describe_kinds(aggregate),
                describe_sides(known),
                describe_sides(expected),
                describe_sides(state.held_sides),
                state.post_flags,
            );
        }
    }

    /// Injects a key down.
    pub(crate) fn key_down(&self, key_code: u16) {
        let mut state = self.state.borrow_mut();
        if state.degraded {
            drop_press(&mut state, key_code, self.session);
            return;
        }
        let injection = Injection::Key {
            key_code,
            down: true,
            cg_flags: state.post_flags,
        };
        if self.post(&mut state, injection).is_ok() {
            state.pressed_keys.insert(key_code);
        }
    }

    /// Injects a key up.
    ///
    /// A key that was never pressed is still released, matching the previous
    /// behaviour: the peer's key-up is authoritative, and a release can never
    /// create a stuck key.
    ///
    /// A failed release is the one failure this state machine cannot absorb
    /// by dropping input: the user already let the key go, so it is retried
    /// once through the other path and the last failure is propagated to the
    /// caller instead of leaving a phantom press that blocks the session
    /// forever.
    pub(crate) fn key_up(&self, key_code: u16) -> Result<(), i32> {
        let mut state = self.state.borrow_mut();
        let path = state.path;
        let injection = Injection::Key {
            key_code,
            down: false,
            cg_flags: state.post_flags,
        };
        let released = self.release(&mut state, path, injection);
        if released.is_ok() {
            state.pressed_keys.remove(&key_code);
        }
        self.settle(&mut state);
        released
    }

    /// Turns Caps Lock over once.
    ///
    /// macOS owns the locked state, so this is a real HID tap of the Caps
    /// Lock key; it is not a modifier transition and must never be repeated.
    pub(crate) fn caps_tap(&self, key_code: u16) -> Result<(), i32> {
        let mut state = self.state.borrow_mut();
        if state.degraded {
            drop_press(&mut state, key_code, self.session);
            return Ok(());
        }
        let path = state.path;
        let down = Injection::Key {
            key_code,
            down: true,
            cg_flags: state.post_flags,
        };
        if self.post(&mut state, down).is_err() {
            // The tap never landed. A press is never replayed after a
            // failure, so this tap is simply dropped like any other press.
            return Ok(());
        }
        let up = Injection::Key {
            key_code,
            down: false,
            cg_flags: state.post_flags,
        };
        let released = self.release(&mut state, path, up);
        if released.is_err() {
            // The tap was posted but not released; keep it so cleanup
            // retries the key-up instead of leaving Caps Lock down.
            state.pressed_keys.insert(key_code);
        }
        self.settle(&mut state);
        released
    }

    /// Releases everything this backend injected.
    pub(crate) fn release_all(&self) -> CleanupResidue {
        let mut state = self.state.borrow_mut();
        let path = state.path;
        let keys = state.pressed_keys.iter().copied().collect::<Vec<_>>();
        for key_code in keys {
            let injection = Injection::Key {
                key_code,
                down: false,
                cg_flags: state.post_flags,
            };
            if self.release(&mut state, path, injection).is_ok() {
                state.pressed_keys.remove(&key_code);
            }
        }

        let mut still_held = ModifierSides::default();
        // Same single baseline as `sync_modifiers`, for the same reason.
        let locally_held = locally_held_flags(state.local_flags, state.remote_flags);
        for (side, next) in transitions(state.held_sides, ModifierSides::default()) {
            let flags = locally_held | flags_for(next);
            let injection = Injection::Modifiers {
                key_code: side.key_code(),
                flags,
            };
            if self.release(&mut state, path, injection).is_ok() {
                state.remote_flags = flags_for(next);
                state.post_flags = flags;
            } else {
                still_held = still_held.with(side.bit());
            }
        }
        state.held_sides = still_held;
        self.settle(&mut state);
        CleanupResidue {
            pressed_keys: state.pressed_keys.len(),
            modifier_sides: state.held_sides,
        }
    }

    /// Posts one injection on the current path.
    fn post(&self, state: &mut InjectorState, injection: Injection) -> Result<(), i32> {
        match self.sink.inject(state.path, injection) {
            Ok(()) => {
                self.log_submission(state.path, injection);
                Ok(())
            }
            Err(status) => {
                log::warn!(
                    target: KEYBOARD_LOG_TARGET,
                    "event=injection_failed session={} path={} status={status} detail={} \
                     pressed_keys={} held_sides={} note=input_retained_for_controlled_release",
                    self.session,
                    path_name(state.path),
                    describe_injection(injection),
                    state.pressed_keys.len(),
                    describe_sides(state.held_sides),
                );
                if state.path == InjectionPath::Nx {
                    state.degraded = true;
                }
                Err(status)
            }
        }
    }

    /// Logs one submitted shortcut-relevant event.
    ///
    /// Ordinary keys stay silent so normal typing never floods the log;
    /// `submitted` names delivery to the API, never shortcut effectiveness.
    fn log_submission(&self, path: InjectionPath, injection: Injection) {
        let relevant = match injection {
            Injection::Modifiers { .. } => true,
            Injection::Key { key_code, .. } => key_category(key_code) != "ordinary",
        };
        if !relevant {
            return;
        }
        log::debug!(
            target: KEYBOARD_LOG_TARGET,
            "event=injection_submitted session={} path={} result=submitted detail={}",
            self.session,
            path_name(path),
            describe_injection(injection),
        );
    }

    /// Posts a release, retrying once through the other path so a failure on
    /// the IOHID path cannot leave input stuck.
    ///
    /// Returns the status of the last failed attempt so the caller can
    /// propagate an unrecoverable release instead of pretending the input
    /// was released.
    fn release(
        &self,
        state: &mut InjectorState,
        path: InjectionPath,
        injection: Injection,
    ) -> Result<(), i32> {
        let status = match self.sink.inject(path, injection) {
            Ok(()) => {
                self.log_submission(path, injection);
                return Ok(());
            }
            Err(status) => {
                log::warn!(
                    target: KEYBOARD_LOG_TARGET,
                    "event=release_failed session={} path={} status={status} detail={} \
                     retrying_once_through=cgevent pressed_keys={} held_sides={}",
                    self.session,
                    path_name(path),
                    describe_injection(injection),
                    state.pressed_keys.len(),
                    describe_sides(state.held_sides),
                );
                if path == InjectionPath::Nx {
                    state.degraded = true;
                }
                status
            }
        };
        if path != InjectionPath::Nx {
            return Err(status);
        }
        match self.sink.inject(InjectionPath::CgEvent, injection) {
            Ok(()) => {
                log::debug!(
                    target: KEYBOARD_LOG_TARGET,
                    "event=release_recovered session={} path=cgevent result=submitted detail={}",
                    self.session,
                    describe_injection(injection),
                );
                Ok(())
            }
            Err(status) => {
                log::warn!(
                    target: KEYBOARD_LOG_TARGET,
                    "event=release_retry_failed session={} path=cgevent status={status} \
                     detail={} pressed_keys={} held_sides={} \
                     note=state_kept_for_retry_and_error_propagation",
                    self.session,
                    describe_injection(injection),
                    state.pressed_keys.len(),
                    describe_sides(state.held_sides),
                );
                Err(status)
            }
        }
    }

    /// Replaces a failed IOHID path with the fallback once nothing injected is
    /// still held.
    ///
    /// The failed path is not retried mid-session: a half-applied shortcut
    /// must not be replayed, so the switch happens only from a clean state.
    fn settle(&self, state: &mut InjectorState) {
        if !state.degraded || state.path != InjectionPath::Nx {
            return;
        }
        if !state.pressed_keys.is_empty() || !state.held_sides.is_empty() {
            return;
        }
        state.path = InjectionPath::CgEvent;
        state.degraded = false;
        let dropped = state.dropped_presses;
        state.dropped_presses = 0;
        log::warn!(target: KEYBOARD_LOG_TARGET, "{}", degradation_message(self.session, dropped));
    }
}

/// Records a press that was dropped because the path failed, logging the first
/// drop and then every 64th one.
fn drop_press(state: &mut InjectorState, key_code: u16, session: u64) {
    state.dropped_presses += 1;
    if state.dropped_presses == 1 || state.dropped_presses.is_multiple_of(64) {
        log::warn!(
            target: KEYBOARD_LOG_TARGET,
            "event=press_dropped session={session} category={} dropped_total={} \
             reason=waiting_for_failed_path_cleanup",
            key_category(key_code),
            state.dropped_presses,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Injection, InjectionPath, InjectorState, KeySink, KeyboardInjector, ModifierKinds,
        ModifierSide, ModifierSides, NX_ALTERNATEMASK, NX_COMMANDMASK, NX_CONTROLMASK,
        NX_DEVICELALTKEYMASK, NX_DEVICELCMDKEYMASK, NX_DEVICELCTLKEYMASK, NX_DEVICELSHIFTKEYMASK,
        NX_DEVICERALTKEYMASK, NX_DEVICERCMDKEYMASK, NX_DEVICERCTLKEYMASK, NX_DEVICERSHIFTKEYMASK,
        NX_SHIFTMASK, desired_sides, flags_for, locally_held_flags,
    };
    use std::cell::RefCell;
    use std::collections::BTreeSet;
    use std::rc::Rc;

    /// `kVK_Control`, the left Control virtual key code. The comparison app
    /// that proved the IOHID path also used `kVK_Control`.
    const LEFT_CONTROL: u16 = 0x3B;
    /// `kVK_RightControl`.
    const RIGHT_CONTROL: u16 = 0x3E;
    /// `kVK_RightArrow`, the key in the reported shortcut.
    const RIGHT_ARROW: u16 = 0x7C;
    /// `kVK_CapsLock`.
    const CAPS_LOCK: u16 = 0x39;

    #[derive(Debug)]
    struct FakeSink {
        preferred: InjectionPath,
        /// Paths to fail once, in order, before succeeding again.
        failures: RefCell<Vec<InjectionPath>>,
        /// One-based call numbers to fail once.
        call_failures: RefCell<Vec<usize>>,
        calls: RefCell<Vec<(InjectionPath, Injection)>>,
    }

    impl FakeSink {
        fn new(preferred: InjectionPath) -> Rc<Self> {
            Rc::new(Self {
                preferred,
                failures: RefCell::new(Vec::new()),
                call_failures: RefCell::new(Vec::new()),
                calls: RefCell::new(Vec::new()),
            })
        }

        fn fail_once(&self, path: InjectionPath) {
            self.failures.borrow_mut().push(path);
        }

        /// Fails the `call`-th (one-based) injection posted from now on.
        fn fail_call(&self, call: usize) {
            self.call_failures.borrow_mut().push(call);
        }

        fn injections(&self) -> Vec<Injection> {
            self.calls
                .borrow()
                .iter()
                .map(|(_, injection)| *injection)
                .collect()
        }

        fn calls(&self) -> Vec<(InjectionPath, Injection)> {
            self.calls.borrow().clone()
        }
    }

    impl KeySink for FakeSink {
        fn preferred_path(&self) -> InjectionPath {
            self.preferred
        }

        fn inject(&self, path: InjectionPath, injection: Injection) -> Result<(), i32> {
            let call = {
                let mut calls = self.calls.borrow_mut();
                calls.push((path, injection));
                calls.len()
            };
            let mut call_failures = self.call_failures.borrow_mut();
            if let Some(index) = call_failures.iter().position(|failed| *failed == call) {
                call_failures.remove(index);
                return Err(-536_870_911);
            }
            drop(call_failures);
            let mut failures = self.failures.borrow_mut();
            if let Some(index) = failures.iter().position(|failed| *failed == path) {
                failures.remove(index);
                return Err(-536_870_911);
            }
            Ok(())
        }
    }

    fn sides(list: &[ModifierSide]) -> ModifierSides {
        list.iter()
            .fold(ModifierSides::default(), |set, side| set.with(side.bit()))
    }

    fn modifier(key_code: u16, flags: u32) -> Injection {
        Injection::Modifiers { key_code, flags }
    }

    fn key(key_code: u16, down: bool, cg_flags: u32) -> Injection {
        Injection::Key {
            key_code,
            down,
            cg_flags,
        }
    }

    #[test]
    fn a_transient_key_up_failure_recovers_through_the_fallback_path() {
        let sink = FakeSink::new(InjectionPath::Nx);
        let injector = KeyboardInjector::new(sink.clone());

        injector.key_down(RIGHT_ARROW);
        sink.fail_once(InjectionPath::Nx);
        assert!(
            injector.key_up(RIGHT_ARROW).is_ok(),
            "the fallback retry delivered the release"
        );

        assert_eq!(
            sink.calls(),
            vec![
                (InjectionPath::Nx, key(RIGHT_ARROW, true, 0)),
                (InjectionPath::Nx, key(RIGHT_ARROW, false, 0)),
                (InjectionPath::CgEvent, key(RIGHT_ARROW, false, 0)),
            ],
            "a failed ordinary release must be retried through the fallback"
        );
        injector.key_down(0x0B);
        assert_eq!(
            sink.calls().last().copied(),
            Some((InjectionPath::CgEvent, key(0x0B, true, 0))),
            "a transient release failure must not drop later input forever"
        );
    }

    #[test]
    fn a_caps_tap_release_failure_recovers_through_the_fallback_path() {
        let sink = FakeSink::new(InjectionPath::Nx);
        let injector = KeyboardInjector::new(sink.clone());

        sink.fail_call(2);
        assert!(
            injector.caps_tap(CAPS_LOCK).is_ok(),
            "the fallback retry delivered the tap's release"
        );

        assert_eq!(
            sink.calls(),
            vec![
                (InjectionPath::Nx, key(CAPS_LOCK, true, 0)),
                (InjectionPath::Nx, key(CAPS_LOCK, false, 0)),
                (InjectionPath::CgEvent, key(CAPS_LOCK, false, 0)),
            ],
            "the tap's key-up must be retried instead of leaving Caps Lock down"
        );
        injector.key_down(RIGHT_ARROW);
        assert_eq!(
            sink.calls().last().copied(),
            Some((InjectionPath::CgEvent, key(RIGHT_ARROW, true, 0)))
        );
    }

    #[test]
    fn modifier_side_key_codes_are_the_macos_virtual_keys() {
        assert_eq!(ModifierSide::LeftShift.key_code(), 0x38);
        assert_eq!(ModifierSide::RightShift.key_code(), 0x3C);
        assert_eq!(ModifierSide::LeftControl.key_code(), 0x3B);
        assert_eq!(ModifierSide::RightControl.key_code(), 0x3E);
        assert_eq!(ModifierSide::LeftOption.key_code(), 0x3A);
        assert_eq!(ModifierSide::RightOption.key_code(), 0x3D);
        assert_eq!(ModifierSide::LeftCommand.key_code(), 0x37);
        assert_eq!(ModifierSide::RightCommand.key_code(), 0x36);
    }

    #[test]
    fn every_side_maps_to_its_general_and_device_flags() {
        assert_eq!(
            flags_for(sides(&[ModifierSide::LeftControl])),
            NX_CONTROLMASK | NX_DEVICELCTLKEYMASK
        );
        assert_eq!(
            flags_for(sides(&[ModifierSide::RightControl])),
            NX_CONTROLMASK | NX_DEVICERCTLKEYMASK
        );
        assert_eq!(
            flags_for(sides(&[ModifierSide::LeftShift])),
            NX_SHIFTMASK | NX_DEVICELSHIFTKEYMASK
        );
        assert_eq!(
            flags_for(sides(&[ModifierSide::LeftShift, ModifierSide::RightShift])),
            NX_SHIFTMASK | NX_DEVICELSHIFTKEYMASK | NX_DEVICERSHIFTKEYMASK
        );
        assert_eq!(
            flags_for(sides(&[ModifierSide::LeftOption])),
            NX_ALTERNATEMASK | NX_DEVICELALTKEYMASK
        );
        assert_eq!(
            flags_for(sides(&[ModifierSide::RightOption])),
            NX_ALTERNATEMASK | NX_DEVICERALTKEYMASK
        );
        assert_eq!(
            flags_for(sides(&[ModifierSide::LeftCommand])),
            NX_COMMANDMASK | NX_DEVICELCMDKEYMASK
        );
        assert_eq!(
            flags_for(sides(&[ModifierSide::RightCommand])),
            NX_COMMANDMASK | NX_DEVICERCMDKEYMASK
        );
        assert_eq!(
            flags_for(sides(&[
                ModifierSide::LeftControl,
                ModifierSide::RightCommand
            ])),
            NX_CONTROLMASK | NX_DEVICELCTLKEYMASK | NX_COMMANDMASK | NX_DEVICERCMDKEYMASK
        );
        assert_eq!(flags_for(ModifierSides::default()), 0);
    }

    #[test]
    fn a_remote_control_press_posts_a_flags_changed_with_the_left_side_key_code() {
        let sink = FakeSink::new(InjectionPath::Nx);
        let injector = KeyboardInjector::new(sink.clone());

        injector.sync_modifiers(ModifierKinds::CONTROL, ModifierSides::default(), 0);

        assert_eq!(
            sink.injections(),
            vec![modifier(
                LEFT_CONTROL,
                NX_CONTROLMASK | NX_DEVICELCTLKEYMASK
            )]
        );
    }

    #[test]
    fn a_remote_right_control_press_reports_the_right_device_flag() {
        let sink = FakeSink::new(InjectionPath::Nx);
        let injector = KeyboardInjector::new(sink.clone());

        injector.sync_modifiers(
            ModifierKinds::CONTROL,
            sides(&[ModifierSide::RightControl]),
            0,
        );

        assert_eq!(
            sink.injections(),
            vec![modifier(
                RIGHT_CONTROL,
                NX_CONTROLMASK | NX_DEVICERCTLKEYMASK
            )]
        );
    }

    #[test]
    fn an_unchanged_snapshot_does_not_repeat_the_transition() {
        let sink = FakeSink::new(InjectionPath::Nx);
        let injector = KeyboardInjector::new(sink.clone());
        let both = sides(&[ModifierSide::LeftControl, ModifierSide::RightControl]);

        injector.sync_modifiers(ModifierKinds::CONTROL, both, 0);
        injector.sync_modifiers(ModifierKinds::CONTROL, both, 0);

        assert_eq!(sink.injections().len(), 2);
    }

    #[test]
    fn releasing_one_control_side_keeps_the_other_side_held() {
        let sink = FakeSink::new(InjectionPath::Nx);
        let injector = KeyboardInjector::new(sink.clone());

        injector.sync_modifiers(
            ModifierKinds::CONTROL,
            sides(&[ModifierSide::LeftControl, ModifierSide::RightControl]),
            0,
        );
        injector.sync_modifiers(
            ModifierKinds::CONTROL,
            sides(&[ModifierSide::RightControl]),
            0,
        );

        assert_eq!(
            sink.injections().last().copied(),
            Some(modifier(
                LEFT_CONTROL,
                NX_CONTROLMASK | NX_DEVICERCTLKEYMASK
            ))
        );
    }

    #[test]
    fn a_snapshot_without_side_information_uses_the_left_side() {
        assert_eq!(
            desired_sides(ModifierKinds::CONTROL, ModifierSides::default()),
            sides(&[ModifierSide::LeftControl])
        );
        assert_eq!(
            desired_sides(
                ModifierKinds::CONTROL.with(ModifierKinds::COMMAND),
                sides(&[ModifierSide::RightCommand])
            ),
            sides(&[ModifierSide::LeftControl, ModifierSide::RightCommand])
        );
    }

    #[test]
    fn a_snapshot_migrates_off_the_invented_side_when_a_real_side_arrives() {
        let sink = FakeSink::new(InjectionPath::Nx);
        let injector = KeyboardInjector::new(sink.clone());

        injector.sync_modifiers(ModifierKinds::CONTROL, ModifierSides::default(), 0);
        injector.sync_modifiers(
            ModifierKinds::CONTROL,
            sides(&[ModifierSide::RightControl]),
            0,
        );

        assert_eq!(
            sink.injections(),
            vec![
                modifier(LEFT_CONTROL, NX_CONTROLMASK | NX_DEVICELCTLKEYMASK),
                modifier(LEFT_CONTROL, 0),
                modifier(RIGHT_CONTROL, NX_CONTROLMASK | NX_DEVICERCTLKEYMASK),
            ]
        );
    }

    #[test]
    fn a_snapshot_that_drops_a_kind_releases_the_invented_side() {
        let sink = FakeSink::new(InjectionPath::Nx);
        let injector = KeyboardInjector::new(sink.clone());

        injector.sync_modifiers(ModifierKinds::CONTROL, ModifierSides::default(), 0);
        injector.sync_modifiers(ModifierKinds::default(), ModifierSides::default(), 0);

        assert_eq!(
            sink.injections(),
            vec![
                modifier(LEFT_CONTROL, NX_CONTROLMASK | NX_DEVICELCTLKEYMASK),
                modifier(LEFT_CONTROL, 0),
            ]
        );
    }

    #[test]
    fn the_remote_snapshot_never_clears_a_locally_held_modifier() {
        let local_shift = NX_SHIFTMASK | NX_DEVICELSHIFTKEYMASK;
        let sink = FakeSink::new(InjectionPath::Nx);
        let injector = KeyboardInjector::new(sink.clone());

        injector.sync_modifiers(
            ModifierKinds::CONTROL,
            ModifierSides::default(),
            local_shift,
        );
        assert_eq!(
            sink.injections().last().copied(),
            Some(modifier(
                LEFT_CONTROL,
                local_shift | NX_CONTROLMASK | NX_DEVICELCTLKEYMASK
            ))
        );

        injector.sync_modifiers(
            ModifierKinds::default(),
            ModifierSides::default(),
            local_shift,
        );
        assert_eq!(
            sink.injections().last().copied(),
            Some(modifier(LEFT_CONTROL, local_shift))
        );
        // Bits the remote is responsible for are released; the bits it never
        // claimed belong to the local keyboard and are kept.
        assert_eq!(locally_held_flags(local_shift, 0), local_shift);
        assert_eq!(locally_held_flags(local_shift, local_shift), 0);
        assert_eq!(
            locally_held_flags(0, local_shift | NX_CONTROLMASK | NX_DEVICELCTLKEYMASK),
            0
        );
    }

    #[test]
    fn releasing_both_injected_control_sides_leaves_the_flags_clear() {
        let sink = FakeSink::new(InjectionPath::Nx);
        let injector = KeyboardInjector::new(sink.clone());
        let both = sides(&[ModifierSide::LeftControl, ModifierSide::RightControl]);

        injector.sync_modifiers(ModifierKinds::CONTROL, both, 0);
        // A later snapshot observes the combined state the window server
        // reports, which is this backend's own injection.
        injector.sync_modifiers(ModifierKinds::CONTROL, both, flags_for(both));
        injector.release_all();

        assert_eq!(
            sink.injections().last().copied(),
            Some(modifier(RIGHT_CONTROL, 0)),
            "a released remote side must not come back as a locally held flag"
        );
    }

    #[test]
    fn a_snapshot_that_dropped_every_side_leaves_no_injected_flag_behind() {
        let sink = FakeSink::new(InjectionPath::Nx);
        let injector = KeyboardInjector::new(sink.clone());
        let both = sides(&[ModifierSide::LeftControl, ModifierSide::RightControl]);

        injector.sync_modifiers(ModifierKinds::CONTROL, both, 0);
        injector.sync_modifiers(ModifierKinds::CONTROL, both, flags_for(both));
        injector.sync_modifiers(
            ModifierKinds::default(),
            ModifierSides::default(),
            flags_for(both),
        );

        assert_eq!(
            sink.injections().last().copied(),
            Some(modifier(RIGHT_CONTROL, 0))
        );
    }

    #[test]
    fn a_locally_held_modifier_survives_a_multi_step_release_batch() {
        let local_shift = NX_SHIFTMASK | NX_DEVICELSHIFTKEYMASK;
        let sink = FakeSink::new(InjectionPath::Nx);
        let injector = KeyboardInjector::new(sink.clone());
        let both = sides(&[ModifierSide::LeftControl, ModifierSide::RightControl]);

        injector.sync_modifiers(ModifierKinds::CONTROL, both, local_shift);
        injector.sync_modifiers(ModifierKinds::CONTROL, both, local_shift | flags_for(both));
        injector.sync_modifiers(
            ModifierKinds::default(),
            ModifierSides::default(),
            local_shift | flags_for(both),
        );

        assert_eq!(
            sink.injections().last().copied(),
            Some(modifier(RIGHT_CONTROL, local_shift))
        );
    }

    #[test]
    fn ordinary_keys_carry_modifier_state_only_for_the_fallback_path() {
        let sink = FakeSink::new(InjectionPath::Nx);
        let injector = KeyboardInjector::new(sink.clone());

        injector.sync_modifiers(ModifierKinds::CONTROL, ModifierSides::default(), 0);
        injector.key_down(RIGHT_ARROW);
        assert!(injector.key_up(RIGHT_ARROW).is_ok());

        assert_eq!(
            sink.injections(),
            vec![
                modifier(LEFT_CONTROL, NX_CONTROLMASK | NX_DEVICELCTLKEYMASK),
                key(RIGHT_ARROW, true, NX_CONTROLMASK | NX_DEVICELCTLKEYMASK),
                key(RIGHT_ARROW, false, NX_CONTROLMASK | NX_DEVICELCTLKEYMASK),
            ]
        );
    }

    #[test]
    fn caps_lock_is_one_tap_and_leaves_nothing_to_release() {
        let sink = FakeSink::new(InjectionPath::Nx);
        let injector = KeyboardInjector::new(sink.clone());

        assert!(injector.caps_tap(CAPS_LOCK).is_ok());
        injector.release_all();

        assert_eq!(
            sink.injections(),
            vec![key(CAPS_LOCK, true, 0), key(CAPS_LOCK, false, 0)]
        );
    }

    #[test]
    fn a_caps_tap_whose_release_fails_is_retried_by_cleanup() {
        let sink = FakeSink::new(InjectionPath::Nx);
        let injector = KeyboardInjector::new(sink.clone());

        // The tap's key-up is the second injection it posts.
        sink.fail_call(2);
        assert!(injector.caps_tap(CAPS_LOCK).is_ok());
        injector.release_all();

        assert_eq!(
            sink.calls(),
            vec![
                (InjectionPath::Nx, key(CAPS_LOCK, true, 0)),
                (InjectionPath::Nx, key(CAPS_LOCK, false, 0)),
                (InjectionPath::CgEvent, key(CAPS_LOCK, false, 0)),
            ]
        );
    }

    #[test]
    fn release_all_releases_keys_before_modifiers() {
        let sink = FakeSink::new(InjectionPath::Nx);
        let injector = KeyboardInjector::new(sink.clone());

        injector.sync_modifiers(ModifierKinds::CONTROL, ModifierSides::default(), 0);
        injector.key_down(RIGHT_ARROW);
        injector.release_all();

        assert_eq!(
            sink.injections(),
            vec![
                modifier(LEFT_CONTROL, NX_CONTROLMASK | NX_DEVICELCTLKEYMASK),
                key(RIGHT_ARROW, true, NX_CONTROLMASK | NX_DEVICELCTLKEYMASK),
                key(RIGHT_ARROW, false, NX_CONTROLMASK | NX_DEVICELCTLKEYMASK),
                modifier(LEFT_CONTROL, 0),
            ]
        );
    }

    #[test]
    fn an_iohid_failure_holds_new_presses_until_the_recorded_input_is_released() {
        let sink = FakeSink::new(InjectionPath::Nx);
        let injector = KeyboardInjector::new(sink.clone());

        injector.key_down(0x00);
        sink.fail_once(InjectionPath::Nx);
        injector.key_down(0x0B);
        injector.key_down(0x08);
        assert_eq!(sink.calls().len(), 2, "the dropped press is never posted");

        assert!(injector.key_up(0x00).is_ok());
        injector.key_down(0x08);
        assert_eq!(
            sink.calls().last().copied(),
            Some((InjectionPath::CgEvent, key(0x08, true, 0)))
        );
    }

    #[test]
    fn a_failed_iohid_release_is_retried_once_through_the_fallback_path() {
        let sink = FakeSink::new(InjectionPath::Nx);
        let injector = KeyboardInjector::new(sink.clone());

        injector.key_down(RIGHT_ARROW);
        sink.fail_once(InjectionPath::Nx);
        injector.release_all();

        assert_eq!(
            sink.calls(),
            vec![
                (InjectionPath::Nx, key(RIGHT_ARROW, true, 0)),
                (InjectionPath::Nx, key(RIGHT_ARROW, false, 0)),
                (InjectionPath::CgEvent, key(RIGHT_ARROW, false, 0)),
            ]
        );
        injector.key_down(RIGHT_ARROW);
        assert_eq!(
            sink.calls().last().copied(),
            Some((InjectionPath::CgEvent, key(RIGHT_ARROW, true, 0)))
        );
    }

    #[test]
    fn a_key_up_failure_on_both_paths_is_propagated_and_recovers_on_retry() {
        let sink = FakeSink::new(InjectionPath::Nx);
        let injector = KeyboardInjector::new(sink.clone());

        injector.key_down(RIGHT_ARROW);
        sink.fail_once(InjectionPath::Nx);
        sink.fail_once(InjectionPath::CgEvent);
        assert!(
            injector.key_up(RIGHT_ARROW).is_err(),
            "an unreleasable key must be reported, not silently recorded"
        );

        // The failed release keeps the key recorded, so a new press is held
        // back instead of interleaving with unconfirmed state. It must not
        // be swallowed forever: the key's next release retries both paths
        // and, once clean, the session degrades for new input.
        injector.key_down(0x0B);
        assert_eq!(sink.calls().len(), 3, "the press is held back");
        assert!(injector.key_up(RIGHT_ARROW).is_ok());
        injector.key_down(0x0B);
        assert_eq!(
            sink.calls().last().copied(),
            Some((InjectionPath::CgEvent, key(0x0B, true, 0)))
        );
    }

    #[test]
    fn a_release_that_fails_on_both_paths_keeps_the_side_recorded() {
        let sink = FakeSink::new(InjectionPath::Nx);
        let injector = KeyboardInjector::new(sink.clone());

        injector.sync_modifiers(ModifierKinds::CONTROL, ModifierSides::default(), 0);
        sink.fail_once(InjectionPath::Nx);
        sink.fail_once(InjectionPath::CgEvent);
        injector.release_all();
        assert_eq!(
            sink.calls().last().copied(),
            Some((InjectionPath::CgEvent, modifier(LEFT_CONTROL, 0)))
        );

        // The next snapshot still knows Control was never released, so it
        // releases it again instead of inventing a second press, and only then
        // is the session clean enough for the fallback path.
        injector.sync_modifiers(ModifierKinds::default(), ModifierSides::default(), 0);
        assert_eq!(
            sink.calls().last().copied(),
            Some((InjectionPath::Nx, modifier(LEFT_CONTROL, 0)))
        );

        // Only once that release is confirmed does the session switch to the
        // fallback for new presses.
        injector.key_down(0x00);
        assert_eq!(
            sink.calls().last().copied(),
            Some((InjectionPath::CgEvent, key(0x00, true, 0)))
        );
    }

    #[test]
    fn an_unavailable_path_starts_on_the_cg_event_fallback() {
        let sink = FakeSink::new(InjectionPath::CgEvent);
        let injector = KeyboardInjector::new(sink.clone());

        injector.key_down(RIGHT_ARROW);
        assert_eq!(
            sink.calls().last().copied(),
            Some((InjectionPath::CgEvent, key(RIGHT_ARROW, true, 0)))
        );
    }

    #[test]
    fn a_release_that_fails_on_both_paths_is_reported() {
        let sink = FakeSink::new(InjectionPath::Nx);
        let injector = KeyboardInjector::new(sink.clone());
        let mut state = InjectorState {
            path: InjectionPath::Nx,
            held_sides: sides(&[ModifierSide::LeftControl]),
            remote_flags: NX_CONTROLMASK | NX_DEVICELCTLKEYMASK,
            post_flags: NX_CONTROLMASK | NX_DEVICELCTLKEYMASK,
            local_flags: 0,
            pressed_keys: BTreeSet::new(),
            degraded: false,
            dropped_presses: 0,
            last_snapshot_log: None,
        };

        sink.fail_once(InjectionPath::Nx);
        sink.fail_once(InjectionPath::CgEvent);

        assert!(
            injector
                .release(&mut state, InjectionPath::Nx, modifier(LEFT_CONTROL, 0))
                .is_err()
        );
        assert_eq!(
            sink.calls(),
            vec![
                (InjectionPath::Nx, modifier(LEFT_CONTROL, 0)),
                (InjectionPath::CgEvent, modifier(LEFT_CONTROL, 0)),
            ]
        );
        assert!(state.degraded);
    }

    mod diagnostics {
        use super::super::{
            CleanupResidue, InjectionPath, InjectorState, KeyboardInjector, ModifierKinds,
            ModifierSide, ModifierSides, SnapshotLog, degradation_message, describe_injection,
            describe_sides, key_category, path_name, snapshot_changed,
        };
        use super::{FakeSink, LEFT_CONTROL, NX_CONTROLMASK, RIGHT_ARROW, key, sides};
        use std::collections::BTreeSet;

        fn fresh_state() -> InjectorState {
            InjectorState {
                path: InjectionPath::Nx,
                held_sides: ModifierSides::default(),
                remote_flags: 0,
                post_flags: 0,
                local_flags: 0,
                pressed_keys: BTreeSet::new(),
                degraded: false,
                dropped_presses: 0,
                last_snapshot_log: None,
            }
        }

        #[test]
        fn categories_and_redaction_hide_ordinary_key_codes() {
            // kVK_ANSI_A / kVK_ANSI_S: identities of ordinary typing.
            assert_eq!(key_category(0x00), "ordinary");
            assert_eq!(key_category(0x01), "ordinary");
            assert_eq!(key_category(RIGHT_ARROW), "arrow");
            assert_eq!(key_category(0x39), "caps_lock");
            assert_eq!(key_category(LEFT_CONTROL), "modifier");

            let ordinary = describe_injection(key(0x01, true, 0));
            assert!(ordinary.contains("category=ordinary"));
            assert!(
                !ordinary.contains("key_code"),
                "ordinary key identity must never reach diagnostics: {ordinary}"
            );
            let shortcut = describe_injection(key(RIGHT_ARROW, true, 0));
            assert!(shortcut.contains("category=arrow"));
        }

        #[test]
        fn paths_and_modifier_sides_are_named_stably() {
            assert_eq!(path_name(InjectionPath::Nx), "iohid");
            assert_eq!(path_name(InjectionPath::CgEvent), "cgevent");
            let both = ModifierSide::LeftControl
                .bit()
                .with(ModifierSide::RightControl.bit());
            assert_eq!(describe_sides(both), "left_control+right_control");
            assert_eq!(describe_sides(ModifierSides::default()), "none");
        }

        #[test]
        fn snapshot_diagnostics_report_each_distinct_state_once() {
            let mut state = fresh_state();
            let entry = SnapshotLog {
                kinds: ModifierKinds::CONTROL,
                known: ModifierSide::LeftControl.bit(),
                observed_flags: NX_CONTROLMASK,
                injected_sides: ModifierSide::LeftControl.bit(),
                posted_flags: NX_CONTROLMASK,
            };
            assert!(snapshot_changed(&mut state, entry));
            assert!(
                !snapshot_changed(&mut state, entry),
                "an unchanged snapshot must stay silent"
            );
            let moved = SnapshotLog {
                injected_sides: ModifierSide::RightControl.bit(),
                ..entry
            };
            assert!(
                snapshot_changed(&mut state, moved),
                "a new injected side must be reported"
            );
        }

        #[test]
        fn degradation_diagnostics_carry_totals_and_avoid_absolute_claims() {
            let message = degradation_message(7, 12);
            assert!(message.contains("session=7"));
            assert!(message.contains("dropped_presses_total=12"));
            assert!(message.contains("new_path=cgevent"));
            assert!(message.contains("does_not_guarantee_macos_system_shortcuts"));
            assert!(
                !message.to_lowercase().contains("cannot"),
                "the fallback is best effort, not proven impossible: {message}"
            );
        }

        #[test]
        fn cleanup_residue_reports_keys_that_survived_both_paths() {
            let sink = FakeSink::new(InjectionPath::Nx);
            let injector = KeyboardInjector::new(sink.clone());

            injector.key_down(RIGHT_ARROW);
            assert_eq!(
                injector.pending(),
                CleanupResidue {
                    pressed_keys: 1,
                    modifier_sides: ModifierSides::default(),
                }
            );

            sink.fail_once(InjectionPath::Nx);
            sink.fail_once(InjectionPath::CgEvent);
            let residue = injector.release_all();
            assert_eq!(residue.pressed_keys, 1);
            assert!(residue.modifier_sides.is_empty());

            // Once both paths accept the release, cleanup reports a clean
            // state for the destroy/terminate diagnostics.
            assert!(injector.release_all().is_empty());
            assert!(injector.pending().is_empty());
        }

        #[test]
        fn cleanup_residue_reports_a_stuck_modifier_side() {
            let sink = FakeSink::new(InjectionPath::Nx);
            let injector = KeyboardInjector::new(sink.clone());

            injector.sync_modifiers(
                ModifierKinds::CONTROL,
                sides(&[ModifierSide::LeftControl]),
                0,
            );
            sink.fail_once(InjectionPath::Nx);
            sink.fail_once(InjectionPath::CgEvent);

            let residue = injector.release_all();
            assert_eq!(residue.pressed_keys, 0);
            assert_eq!(residue.modifier_sides, sides(&[ModifierSide::LeftControl]));
        }
    }
}
