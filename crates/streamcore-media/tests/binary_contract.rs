use streamcore_media::{
    binary::{AudioPacket, ClockPing, ClockPong, FilePacket, Frame, InputEvent, TerminalPacket},
    media::MAX_FRAME_LENGTH,
};

#[test]
fn valid_inputs_are_canonical_and_every_truncated_or_extended_input_is_rejected() {
    let cases = [
        InputEvent::Pointer {
            stream: 9,
            phase: 2,
            button: 1,
            x: 0.25,
            y: 0.5,
            clicks: 2,
            pressure: 1.0,
            modifiers: 63,
        },
        InputEvent::Scroll {
            stream: 9,
            x: 0.0,
            y: 1.0,
            dx: -256.5,
            dy: 512.25,
            precise: true,
            momentum: 3,
            modifiers: 4,
        },
        InputEvent::Magnify {
            stream: 9,
            x: 0.0,
            y: 1.0,
            scale: -0.5,
            phase: 2,
        },
        InputEvent::Key {
            stream: 7,
            code: u16::MAX,
            down: false,
            modifiers: 32,
        },
    ];
    for event in cases {
        let bytes = event.encode().unwrap();
        assert_eq!(InputEvent::decode(&bytes).unwrap(), event);
        for end in 0..bytes.len() {
            assert!(InputEvent::decode(&bytes[..end]).is_err());
        }
        let mut extended = bytes;
        extended.push(0);
        assert!(InputEvent::decode(&extended).is_err());
    }
}
#[test]
fn nonfinite_coordinates_unknown_values_and_file_wraparound_are_rejected() {
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0.01, 1.01] {
        assert!(
            InputEvent::Magnify {
                stream: 1,
                x: bad,
                y: 0.5,
                scale: 1.0,
                phase: 0
            }
            .encode()
            .is_err()
        );
    }
    assert!(InputEvent::decode(&[4, 0, 0, 2, 0, 0, 0, 0]).is_err());
    assert!(
        InputEvent::Key {
            stream: 7,
            code: 1,
            down: true,
            modifiers: 64
        }
        .encode()
        .is_err()
    );
    assert!(
        FilePacket {
            transfer_id: [0; 16],
            offset: u64::MAX,
            data: &[1]
        }
        .encode()
        .is_err()
    );
}
#[test]
fn large_payloads_stay_borrowed_and_all_decoders_enforce_the_frame_cap() {
    let mut bytes = vec![0; MAX_FRAME_LENGTH + 1];
    assert!(AudioPacket::decode(&bytes[..8]).is_err());
    assert!(TerminalPacket::decode(&bytes[..16]).is_err());
    assert!(FilePacket::decode(&bytes[..24]).is_err());
    assert!(AudioPacket::decode(&bytes).is_err());
    assert!(TerminalPacket::decode(&bytes).is_err());
    assert!(FilePacket::decode(&bytes).is_err());
    bytes.pop();
    let audio = AudioPacket::decode(&bytes).unwrap();
    assert_eq!(audio.data.as_ptr(), bytes[8..].as_ptr());
    assert!(TerminalPacket::decode(&bytes).is_err());
    let terminal_bytes = TerminalPacket {
        agent_id: [0; 16],
        subscription_id: [1; 16],
        kind: 0,
        sequence: u64::MAX,
        cols: 80,
        rows: 24,
        data: &bytes[..4 * 1024 * 1024],
    }
    .encode()
    .unwrap();
    let terminal = TerminalPacket::decode(&terminal_bytes).unwrap();
    assert_eq!(terminal.data.as_ptr(), terminal_bytes[45..].as_ptr());
    assert_eq!(terminal.encode().unwrap(), terminal_bytes);
    let mut oversized_terminal = terminal_bytes;
    oversized_terminal.push(0);
    assert!(TerminalPacket::decode(&oversized_terminal).is_err());
    let file = FilePacket::decode(&bytes).unwrap();
    assert_eq!(file.data.as_ptr(), bytes[24..].as_ptr());
    assert_eq!(file.encode().unwrap(), bytes);
}
#[test]
fn frame_headers_reject_oversize_before_the_body_and_leave_coalesced_tail() {
    assert!(Frame::read(&[1, 2, 0, 0, 1]).is_err()); // 32MiB + 1
    assert!(Frame::read(&[255, 0, 0, 0, 0]).is_err());
    let bytes = Frame {
        kind: 0x30,
        payload: &[0, 255, 1],
    }
    .encode()
    .unwrap();
    for end in 0..bytes.len() {
        assert!(Frame::read(&bytes[..end]).unwrap().is_none());
    }
    let mut joined = bytes.clone();
    joined.extend(&bytes);
    let (first, consumed) = Frame::read(&joined).unwrap().unwrap();
    assert_eq!(first.payload, &[0, 255, 1]);
    let (second, tail) = Frame::read(&joined[consumed..]).unwrap().unwrap();
    assert_eq!(second, first);
    assert_eq!(consumed + tail, joined.len());
}
#[test]
fn typed_ping_and_pong_fixtures_are_exact_little_endian_and_fixed_length() {
    // clock.v1 fixtures (protocol/BINARY.md, "Typed ping and pong").
    let ping = ClockPing {
        nonce: 0x0102_0304_0506_0708,
        send_us: 0x1112_1314_1516_1718,
    };
    let ping_bytes = [
        8, 7, 6, 5, 4, 3, 2, 1, 0x18, 0x17, 0x16, 0x15, 0x14, 0x13, 0x12, 0x11,
    ];
    assert_eq!(ping.encode(), ping_bytes);
    assert_eq!(ClockPing::decode(&ping_bytes).unwrap(), ping);
    let pong = ClockPong {
        nonce: 1,
        receive_us: u64::MAX - 1,
        send_us: u64::MAX,
    };
    let mut pong_bytes = vec![1, 0, 0, 0, 0, 0, 0, 0];
    pong_bytes.extend([0xfe, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
    pong_bytes.extend([0xff; 8]);
    assert_eq!(pong.encode().unwrap().to_vec(), pong_bytes);
    assert_eq!(ClockPong::decode(&pong_bytes).unwrap(), pong);
    // Wrong lengths (including the legacy empty keepalive body) are invalid.
    for end in 0..ping_bytes.len() {
        assert!(ClockPing::decode(&ping_bytes[..end]).is_err());
    }
    for end in 0..pong_bytes.len() {
        assert!(ClockPong::decode(&pong_bytes[..end]).is_err());
    }
    let mut long = ping_bytes.to_vec();
    long.push(0);
    assert!(ClockPing::decode(&long).is_err());
    pong_bytes.push(0);
    assert!(ClockPong::decode(&pong_bytes).is_err());
    // A pong cannot be sent before it was received.
    let backwards = ClockPong {
        nonce: 1,
        receive_us: 2,
        send_us: 1,
    };
    assert!(backwards.encode().is_err());
    let mut bytes = vec![1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0];
    bytes.extend([1, 0, 0, 0, 0, 0, 0, 0]);
    assert!(ClockPong::decode(&bytes).is_err());
    // Both ride the existing envelope kinds.
    let framed = Frame {
        kind: 2,
        payload: &ping.encode(),
    }
    .encode()
    .unwrap();
    assert_eq!(&framed[..5], &[2, 0, 0, 0, 16]);
}
