# Native desktop input ownership

The screen arbiter grants an opaque input lease to exactly one authenticated
controller. Key and text input require that controller to own a desktop stream;
other authenticated clients cannot inject into the Mac merely by sending keys.
Positional input additionally requires the referenced stream's content rectangle.

The native injector checks the ownership lease and the Rust logical-session
validity at submission and again immediately before posting. Ownership changes
invalidate queued commands synchronously. The same serial native queue releases
held keys/buttons and clears modifier flags before posting a successor's presses.
A late disconnect/release from the former owner cannot release the new owner's
keys. Deferred window-raise callbacks also recheck their input authorization.

The queue admits at most 128 pending input operations and coalesces ownership
cleanup into one pending job. A text insertion admits at most 4096 UTF-16 units.
Overflow invalidates the input lease, discards queued work and releases held input;
the connection stops affected streams and sends stream-scoped input.limit errors.
The user must reopen the desktop. Text is not silently truncated or queued for a
future controller. Unicode events contain at most 20 UTF-16 units and split only
between Unicode scalars, preserving surrogate pairs even for a grapheme longer
than one event. Application interpretation of separate Unicode events still needs
native app coverage.

Input uses a private CoreGraphics event source, matching Apple's guidance for
remote-control programs:
https://developer.apple.com/documentation/coregraphics/cgeventsourcestateid
This keeps the source's state table separate from physical hardware state; actual
WindowServer/application behavior must be qualified in the signed VM.

Tests use the production screen arbiter, admission queue and CGEvent construction,
with controlled controller validity and interception at the final OS-post boundary.
They cover screen ownership, takeover release order, stale disconnect, expired
logical sessions, queued input after invalidation, repeated handoffs, overflow,
right-button dragging, text bounds and Unicode reconstruction. They do not inject
into the developer desktop and do not establish real TCC/input-device behavior.

This is the native macOS input adapter. The client input channel and general
resource acknowledgements retain their separately tracked work; this bound must
not be presented as a bound for every queue in the product.
