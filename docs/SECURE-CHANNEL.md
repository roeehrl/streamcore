# Secure channel foundation

The working-tree native host and client use this Phase 1 transport. It is not yet
a qualified release: signed macOS credentials, full app/VM validation and the
remaining daemon, media and performance gates are open.

## Identity and trust

`PeerIdentity` owns an ECDSA P-256 private key and a self-signed certificate with
client/server authentication usage. Its SHA-256 certificate fingerprint is the
cryptographic peer identity. A display name, discovery address or claimed UUID
never grants trust. The entire certificate is pinned, with no intermediate CA,
public CA fallback, or trust-on-first-use path. Rustls verifies certificate validity,
name/usage and handshake signatures in addition to matching the pin.

Exported identity bytes contain private material. Native code must persist them
only in the OS credential store, release temporary buffers and never log them.
The core does not write keys to disk. The Swift `CredentialStore` uses the
data-protection Keychain with synchronization disabled and device-only accessibility.
Corrupt or inaccessible stored data fails; it never silently rotates the identity.
Concurrent first use uses insert-only storage, then reads the winning identity.

The macOS API must explicitly select the data-protection implementation; the
legacy implementation has different access semantics. See Apple's
[kSecUseDataProtectionKeychain documentation](https://developer.apple.com/documentation/security/ksecusedataprotectionkeychain)
and [keychain implementation note](https://developer.apple.com/documentation/technotes/tn3137-on-mac-keychains).
Real app identifiers/profiles are required for native macOS Keychain validation.
The test-only Simulator access group is never a shipping app entitlement.

`SecureListener.replaceTrustedPeers` updates both transports on the same bound port.
Adding a pin preserves established channels. Removing a pin closes every retained
native channel for that peer; replacing with an empty set revokes all peers and
keeps a deny-all listener ready for the next explicit pairing. Client certificates
remain mandatory even with no trusted peers. Invalid replacements leave the prior
configuration untouched. Queued handshakes carry a trust generation and cannot be
delivered after revocation, including when that certificate is later reauthorized.
Listener close also closes retained TCP and QUIC channels, with at most 256 live
native channels tracked. The native client owner must still close its outbound
channel when revoking its stored host pin.

## Pairing

Opening a `PairingWindow` locally generates a six-digit cryptographically random
PIN. It lasts 120 seconds, permits at most five started attempts across all clients,
and is consumed on success or cancel. Malformed messages and started PAKE attempts
consume the budget. A socket that closes after the public Hello, before submitting
a request, spends no PIN attempt. Reopening a window requires a local user action.

OPAQUE uses ristretto255, TripleDH/SHA-512 and Argon2. Local registration happens
only while opening the host's pairing window. Its authentication context binds the
host and client certificate hashes in a fixed role order. Changing either public
certificate invalidates the proof. An observed exchange does not expose a reusable
PIN verifier. This integration has not received an independent cryptographic audit.

1. Native host displays the PIN locally or in a user-controlled invitation.
2. Client constructs its OPAQUE request with both public certificates and the PIN.
3. Host reserves an attempt and returns a challenge.
4. Client authenticates the host and returns its proof.
5. Host verifies, consumes the window, and returns the client pin and a confirmation
   to its local owner. The owner persists that pin **before sending confirmation**.
6. Client checks confirmation, persists the authenticated host pin, then reports
   pairing success. Any persistence failure must remain visible.

No PIN is sent in discovery or network plaintext. `listen_pairing` now opens a
separate bootstrap TCP service that accepts only public certificates and OPAQUE
messages. It closes after 120 seconds; each exchange has a 20-second deadline,
eight connections may be pending, and bootstrap frames are limited to 16 KiB before
allocation. Its explicitly tagged JSON messages are Hello, Request, Challenge,
Finish and Persisted, or a generic failure. A socket can finish only its own attempt.
It accepts no commands, media, files or session traffic.

`next_verified` returns a `PendingPairing` to the native host. The host saves its
certificate before awaiting `confirm_persisted`; that future returns only after
writing the confirmation, so closing the service immediately afterward does not
lose it. Dropping or rejecting the pending result releases no success proof.
`pair_with_host` accepts up to 16 numeric addresses, deduplicates them, and races
TCP/public-Hello paths with 100-ms staggering and three-second path timeouts.
The optional invitation fingerprint filters paths before PAKE. Only the winning
path receives a PIN proof; authentication or persistence failure never replays
the code to another address. It returns a host pin only after verifying the
host's persistence confirmation.

Swift `CredentialStore.persistPairing` saves the peer and updates the supplied
secure listener before sending confirmation. `pairHost` saves the host pin after
verifying that confirmation. These helpers enforce the ordering with
the real Keychain. Cancellation, revocation, or a superseding pairing operation
prevents a late response from restoring the client pin. A host storage success
followed by a lost network confirmation is reported as an error; its saved pin
remains visible for explicit recovery. QR/headless encoding and native host/client
wiring are implemented; signed app credential and live VM qualification remain open.

## Session transport

Both transports require pinned mutual TLS 1.3 and ALPN `mycnc/2`. TLS tickets,
resumption and early data are disabled so every new channel proves the current pin.
QUIC and TCP share a port. Callers may race up to 16 numeric address/transport paths
with bounded stagger delays. Only a fully authenticated, negotiated path wins;
losers are canceled before application input is sent. QUIC can rebind its client
socket while retaining authenticated identity. TCP needs a fresh connection.

The negotiation frame is a four-byte big-endian length followed by
1–1,048,576 bytes. Empty, oversized and truncated frames fail. Hello is UTF-8 JSON:

```json
{"major":2,"capabilities":["inventory.v1"],"required":["inventory.v1"]}
```

Both required sets must be advertised locally and supported by the peer. Capability
names and counts are bounded; duplicates are invalid. The channel returns the
intersection. After Hello, native secure channels use five bounded, ordered
[traffic lanes](../protocol/LANES.md). QUIC opens one bidirectional stream per lane;
TLS/TCP interleaves 16-KiB fragments. StreamCore's own contracts are Hello major 2,
the five lanes and `frames.v3` binary payloads; video continuity uses the
[binary contract](../protocol/BINARY.md). Each product carries and versions its own
application control protocol on top of these and negotiates it as a Hello
capability of its own.
Interactive, video, audio, terminal and bulk lanes have independent receive
credits and bounded queues. Scheduling does not establish a latency or throughput
guarantee; native [capture admission](CAPTURE-PIPELINE.md) is bounded separately.
Full display-pipeline qualification and session replay remain open.

The listener bounds concurrent handshakes and queued results to 16 each, with a
five-second handshake/negotiation timeout. A stalled unauthenticated connection
cannot block all later clients. Excess completed sessions are closed when the
native consumer falls behind. An invalid connection returns an error to `accept`;
the native accept loop should continue unless explicitly closed.

Native async calls run on an owned Tokio runtime. Lane readers and writers run
independently; synchronous enqueue preserves native admission order within a lane.
`OperationCancellation` explicitly cancels outbound dialing and pairing. The Swift
package wrappers map `Task` cancellation to that handle and close a connection
that finishes concurrently with cancellation. Generated UniFFI async bindings alone
do not do this; see [UniFFI cancellation guidance](https://mozilla.github.io/uniffi-rs/next/futures.html#cancelling-async-code).
Other native session/listener owners must call `close` when cancelling pending I/O.
`ByteChannel.receive` also closes the channel when its Swift reader is cancelled.
An interrupted Rust frame closes the channel; partial frames cannot be reused.
`close` releases both TCP halves even if native UI retains a cached reference.

The client resolves candidates independently and starts pinned handshakes as each
address arrives. It bounds the active race to eight endpoint pairs (16 QUIC/TCP
handshakes). Numeric addresses bypass DNS. Blocking OS DNS work uses four workers,
at most 64 admitted operations and a three-second caller timeout; cancellation
releases callers even if the OS lookup returns later. Bonjour resolution is stopped
on cancellation. Generation guards reject stale dial status and winning channels.

## Evidence and remaining gates

Local checks cover Rust state isolation, framing, PIN replay/expiry/attempt budgets,
certificate substitution, missing credentials, ALPN mismatch, QUIC/TCP mutual trust,
UDP black-hole fallback, QUIC rebind, slow handshakes and two loopback hosts where
one stops. Generated Swift also performs pairing and real encrypted exchanges.
The Apple harness links the static library and exercises Keychain persistence.

This is not evidence of two real hosts streaming capture/terminals, resume after a
daemon restart, 120 fps scheduling, sub-second network handoff, USB behavior,
locked-phone delivery, battery usage or the two-hour soak. Those remain explicit
Phase 1 gates. The apps roadmap and execution checklist track their status.

## Native trust activation evidence

The live-trust batch passes 53 Rust tests, including deny-all startup, first pairing
on the same port, live additions preserving both transports, per-peer revocation,
invalid replacement rollback, queued-handshake revocation and listener shutdown.
The Apple package passes its macOS transport test and nine entitled iOS Simulator
tests. The pairing test opens an authenticated channel immediately after its
storage acknowledgement, then revokes it. Other cases show that corrupt stored
trust closes the listener and a failed listener activation sends no pairing success.

These are historical live-trust-batch results. The current native cutover and
additional path/cancellation tests are recorded in MyCNC's execution ledger.
Signed macOS credential lifecycle remains open.
