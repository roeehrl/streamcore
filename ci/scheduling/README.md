# Five-lane transport scheduling probe

`crates/streamcore-transport/examples/scheduling_probe.rs` measures the production
secure channel and scheduler under a declared five-lane workload. It sends
deterministic payloads to an authenticated echo peer over QUIC, then TLS/TCP. Both
directions use the same lane. The probe verifies every payload byte after its
header, per-lane sequence, and accepted/received totals. Generated private
identities stay in RAM; only public certificates cross temporary files.

| Lane | Messages/s | Bytes/message |
| --- | ---: | ---: |
| Interactive | 60 | 64 |
| Video | 120 | 98,304 |
| Audio | 100 | 1,024 |
| Terminal | 20 | 4,096 |
| Bulk | 32 | 1,048,576 |

The aggregate offered load is about 364.3 Mbit/s in each direction before
framing/encryption overhead. This is a declared workload, not a maximum-throughput
test. Those are the default values; a bulk rate of 0 disables bulk, and 128
messages/s of 256 KiB keeps the same 32 MiB/s aggregate bulk rate with smaller
chunks. All active producers run concurrently. The echo reader preserves its
pending receive while collecting write completions, so another lane's write does
not cancel or serialize the read.

Producers use a monotonic clock and skip expired slots rather than burst to catch
up. `missed_deadlines` counts whole elapsed slots; `send_lateness` reports actual
enqueue time relative to the selected slot, including message preparation.
Backpressure is counted separately, so `scheduled = missed_deadlines +
backpressured + accepted`. Round-trip time runs from just before enqueue to
receipt of the complete echoed message. `write_completion` runs from just before
enqueue to `PendingSend.written()` (handed to the transport, not delivered).
`receive_gap` measures spacing between consecutive complete replies on each lane.
Percentiles use the nearest-rank method over every received message.

The probe does not run capture, an encoder or decoder, a display or input; synthetic
video-sized messages are not rendered frames.

## Running it

```bash
cargo run --locked --example scheduling_probe -- self-test
cargo build --locked --example scheduling_probe && python3 ci/scheduling/test-probe.py
```

`self-test` runs both transports on loopback for two seconds per profile (no bulk,
32 one-MiB messages/s, 128 256-KiB messages/s), first sending a malformed lane
message that must close only the offending peer. It checks delivery, receipt
counts, the selected workload and complete accounting, not latency thresholds.
`test-probe.py` starts real client and server processes on loopback and checks
argument forwarding, verified delivery and percentile sample counts on both
transports. `cargo test --workspace` does not run either; local CI does
(`local/scheduling`).

Against a second machine:

```bash
scheduling_probe client FRESH_DIRECTORY SERVER_IP SECONDS_PER_TRANSPORT BULK_MESSAGES_PER_SECOND BULK_BYTES
scheduling_probe server FRESH_DIRECTORY   # on the other machine, sharing that directory
```

The client publishes its public certificate in the directory and waits; the server
publishes its own and its port, serves for at most 180 seconds, and the client
writes `report.json` there.

`network_probe` (`cargo build --locked --example network_probe`) is the two-machine
reachability check: `network_probe client DIRECTORY SERVER_IP` here, then
`network_probe server SHARED_DIRECTORY` on the other machine. It keeps QUIC and
TLS/TCP channels to a local endpoint and the remote one, checks 100 16-KiB round
trips on each of the five lanes per channel, stops the local endpoint, then checks
100 more per lane on both remote channels and a QUIC socket rebind.
