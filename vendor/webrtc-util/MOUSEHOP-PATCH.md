# Local patch to webrtc-util 0.11.0

Source: crates.io webrtc-util 0.11.0; original MIT/Apache licenses included.

The Windows shared UDP listener continues receiving after WSAECONNRESET (10054).
Windows reports a peer's ICMP Port Unreachable through recv_from; terminating the
shared receive task leaves the port bound but all future DTLS handshakes unanswered.
Other errors retain upstream behavior. This patch applies to the dependency used
by both Mousehop and webrtc-dtls through the workspace crates.io patch.
