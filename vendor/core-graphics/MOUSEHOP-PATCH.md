# Local patch to core-graphics 0.25.0

Source: crates.io core-graphics 0.25.0; original MIT/Apache licenses included.

Mousehop needs to subscribe to macOS `CGEventTypeGesture` (29) so the capture
backend can observe IOHID zoom gestures. Upstream 0.25.0 omits this public enum
variant even though CGEventTap accepts its event-mask bit. No binding behavior
is changed.
