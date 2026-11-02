# Host-owned logical sessions

A logical session survives replacement of its QUIC/TLS transport. The Rust
`SessionDaemon` belongs to the running host runtime. After certificate
authentication, Hello may carry a previous daemon/session UUID pair. The host
returns a required session reference, connection generation and `resumed` outcome.
The reference is public metadata, not a bearer credential: the daemon binds it to
the fingerprint obtained from the authenticated channel. Names and device UUIDs
cannot resume another peer's session.

Attach atomically supersedes the previous generation. The native directory closes
its obsolete transport, and command dispatch checks whether its lease is still
current. A delayed old detach cannot revoke the replacement. The screen arbiter
ignores an obsolete lease, so a dead transport cannot retain screen ownership.
This does not add command replay or duplicate execution handling.

A detached session may resume for 120 seconds. Expired, missing or prior-daemon
references receive a fresh session with `resumed: false`. A reference to another
known peer's session is rejected. At most eight sessions per authenticated peer
and 256 overall are retained; expired detached entries are removed at attach.
Server stop/restart and trust revocation invalidate all leases and rotate the
daemon epoch. A newly constructed runtime also has a new random epoch.

The client retains its reference only within its connection manager and binds it
to the authenticated host fingerprint. It checks the resumed reference and an
increasing generation. UI commands and ordered input capture the whole session
reference plus generation at submission. Offline submissions are ignored, and
queued work from a prior session/daemon cannot run after reconnect. Main-actor
connection callbacks are dispatched FIFO; leaving the connected state cancels
pending desktop start continuations, including an explicit Connect action that
replaces the transport. The first reconnect delay is 150 ms, followed by bounded
exponential backoff; this is not a measured real-network latency guarantee.

## Scope and qualification

The Rust daemon owns logical connection leases; native HostRuntime owns
services/processes in the bundled per-user launch agent. The menu app controls
that runtime through authenticated XPC and can quit independently. Signed
launchd lifecycle qualification remains open. A service crash/restart does not
restore terminals or capture. The native
client restores an already-open desktop subscription only after a verified logical
resume, using a fresh capture stream ID and renderer. It pauses
input during reconnect, stops restoration when the view is closed, and respects
another device's screen ownership. A new daemon/session requires an explicit
start. Capture failure outcomes are scoped by stream ID. Terminal snapshots/replay,
general resource reattachment, acknowledged action IDs, native global state and
process lifecycle work remain. TLS resumption/early data remain disabled;
each replacement transport authenticates the current certificate pin anew.

Rust tests cover peer isolation, supersession, stale detach, expiry, reset,
per-peer/global bounds and concurrent resumes. HostRuntime integration uses real
secure connections to verify resumed references, increasing generations, closing
the prior transport and independence of another logical session. Native client
tests drop real secure connections and check resume, offline-command rejection,
and stale-generation/daemon input fences. Native UI-model tests also interrupt two
pending desktop starts through host loss and manual reconnect, check cancellation,
and verify that offline starts fail immediately. Desktop model tests cover
restoration, closed views, daemon restart, screen ownership, scoped capture errors
and repeated outages with audio changes. Signed Mac/VM, network-path change,
general resource reattachment and soak results must be recorded separately.

Native screen control has a separate ownership lease and bounded input queue;
see [input ownership and release](INPUT-OWNERSHIP.md) for disconnect/takeover
cleanup, limits and the scope of native validation.
