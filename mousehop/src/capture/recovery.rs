//! Physical release gate, before Command/Ctrl mapping or transport queueing.
use input_event::{Event, KeyboardEvent, PointerEvent};
use std::collections::HashSet;

#[derive(Default)]
pub(super) struct PhysicalGate {
    held: HashSet<(bool, u32)>,
    blocked: HashSet<(bool, u32)>,
    modifiers: [u32; 3],
    blocked_modifiers: [u32; 3],
    paused: bool,
    stale_momentum: bool,
}
impl PhysicalGate {
    pub(super) fn freeze(&mut self) {
        self.blocked.extend(self.held.iter().copied());
        self.blocked_modifiers = self.modifiers;
        self.paused = true;
        self.stale_momentum = true;
    }
    pub(super) fn resume(&mut self) {
        self.paused = false;
    }
    pub(super) fn filter(&mut self, mut event: Event) -> Option<Event> {
        let key = match event {
            Event::Keyboard(KeyboardEvent::Key { key, state, .. }) => {
                Some(((false, key), u32::from(state)))
            }
            Event::Pointer(PointerEvent::Button { button, state, .. }) => {
                Some(((true, button), state))
            }
            _ => None,
        };
        if let Some((key, state)) = key {
            let blocked = self.blocked.contains(&key);
            if state == 0 {
                self.held.remove(&key);
                self.blocked.remove(&key);
            } else {
                self.held.insert(key);
                if self.paused {
                    self.blocked.insert(key);
                }
            }
            if blocked || self.paused {
                return None;
            }
        }
        if let Event::Keyboard(KeyboardEvent::Modifiers {
            depressed,
            latched,
            locked,
            ..
        }) = &mut event
        {
            self.modifiers = [*depressed, *latched, *locked];
            for (blocked, current) in self.blocked_modifiers.iter_mut().zip(self.modifiers) {
                *blocked &= current;
                if self.paused {
                    *blocked |= current;
                }
            }
            *depressed &= !self.blocked_modifiers[0];
            *latched &= !self.blocked_modifiers[1];
            *locked &= !self.blocked_modifiers[2];
        }
        if self.paused {
            return None;
        }
        if let Event::Pointer(PointerEvent::Axis { momentum, .. }) = event {
            if momentum && self.stale_momentum {
                return None;
            }
            if !momentum {
                self.stale_momentum = false;
            }
        }
        Some(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(state: u8) -> Event {
        Event::Keyboard(KeyboardEvent::Key {
            time: 0,
            key: 30,
            state,
        })
    }
    fn button(state: u32) -> Event {
        Event::Pointer(PointerEvent::Button {
            time: 0,
            button: input_event::BTN_LEFT,
            state,
        })
    }
    #[test]
    fn r13_held_keys_buttons_and_repeat_require_physical_release() {
        let mut gate = PhysicalGate::default();
        assert!(gate.filter(key(1)).is_some());
        gate.freeze();
        assert!(gate.filter(button(1)).is_none());
        gate.resume();
        for event in [key(2), key(1), button(1), key(0), button(0)] {
            assert!(gate.filter(event).is_none());
        }
        assert!(gate.filter(key(1)).is_some());
        assert!(gate.filter(button(1)).is_some());
    }
    #[test]
    fn r13_pause_discards_motion_scroll_and_filters_modifier_snapshots() {
        let mut gate = PhysicalGate::default();
        let mods = |depressed| {
            Event::Keyboard(KeyboardEvent::Modifiers {
                depressed,
                latched: 0,
                locked: 0,
                group: 0,
            })
        };
        gate.filter(mods(3));
        gate.freeze();
        assert!(
            gate.filter(Event::Pointer(PointerEvent::Motion {
                time: 0,
                dx: 42.0,
                dy: -7.0
            }))
            .is_none()
        );
        assert!(
            gate.filter(Event::Pointer(PointerEvent::AxisDiscrete120 {
                axis: 0,
                value: 120
            }))
            .is_none()
        );
        gate.resume();
        assert_eq!(gate.filter(mods(3)), Some(mods(0)));
        assert_eq!(gate.filter(mods(0)), Some(mods(0)));
        assert_eq!(gate.filter(mods(1)), Some(mods(1)));
    }
}
