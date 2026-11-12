use streamcore_media::{
    binary::{AudioPacket, ClockPing, ClockPong, FilePacket, Frame, InputEvent, TerminalPacket},
    media::{
        ExtendedVideoPacket, FrameTiming, FrameVersion, LtrInfo, MAX_FRAME_LENGTH, VideoExtensions,
        VideoMetadata, VideoPacket,
    },
    video_control::{
        ContentRect, FramePresentation, FrameTimingReport, LayoutInfo, LossBitmap, SourceRect,
        VideoFeedback, VideoNack, is_video_control_kind, validate_video_control,
    },
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

fn v4_header(flags: u8) -> Vec<u8> {
    // stream 7, flags, generation 2, sequence 3, pts 4: the unchanged 29-byte v3 header.
    let mut b = vec![7, 0, 0, 0, flags];
    b.extend([2, 0, 0, 0, 0, 0, 0, 0]);
    b.extend([3, 0, 0, 0, 0, 0, 0, 0]);
    b.extend([4, 0, 0, 0, 0, 0, 0, 0]);
    b
}
const DELTA: VideoMetadata = VideoMetadata {
    stream_id: 7,
    generation: 2,
    sequence: 3,
    flags: 0,
    pts_micros: 4,
};

#[test]
fn frames_v4_extension_fixture_is_exact_and_every_tlv_round_trips() {
    // protocol/BINARY.md, "frames.v4": flag 0x80, ext_len u16 LE, then TLVs in type order.
    let extensions = VideoExtensions {
        timing: Some(FrameTiming {
            h2_us: 1,
            h3_us: 2,
            h4_us: 0x0102_0304,
            h1_minus_h0_us: -2,
        }),
        capture_us: Some(0x1112_1314_1516_1718),
        discardable: true,
        ltr: Some(LtrInfo {
            token: 9,
            flags: LtrInfo::MARK,
        }),
        temporal_id: 1,
        layout_epoch: Some(0xa0b0_c0d0),
    };
    let mut fixture = v4_header(0x80);
    fixture.extend([46, 0]); // ext_len 46
    fixture.extend([
        1, 16, 1, 0, 0, 0, 2, 0, 0, 0, 4, 3, 2, 1, 0xfe, 0xff, 0xff, 0xff,
    ]);
    fixture.extend([2, 8, 0x18, 0x17, 0x16, 0x15, 0x14, 0x13, 0x12, 0x11]);
    fixture.extend([3, 0]);
    fixture.extend([4, 5, 9, 0, 0, 0, 1]);
    fixture.extend([5, 1, 1]);
    fixture.extend([6, 4, 0xd0, 0xc0, 0xb0, 0xa0]);
    fixture.extend([0xaa, 0xbb]); // sample data
    let packet = ExtendedVideoPacket {
        metadata: DELTA,
        extensions,
        data: &[0xaa, 0xbb],
    };
    assert_eq!(packet.encode(FrameVersion::V4).unwrap(), fixture);
    let decoded = ExtendedVideoPacket::decode(&fixture, FrameVersion::V4).unwrap();
    assert_eq!(decoded, packet);
    assert_eq!(decoded.data.as_ptr(), fixture[fixture.len() - 2..].as_ptr());
    // Not negotiated: the flag is invalid on receipt, and the sender drops the block.
    assert!(ExtendedVideoPacket::decode(&fixture, FrameVersion::V3).is_err());
    assert!(VideoPacket::decode(&fixture).is_err());
    let v3 = packet.encode(FrameVersion::V3).unwrap();
    let mut plain = v4_header(0);
    plain.extend([0xaa, 0xbb]);
    assert_eq!(v3, plain);
    // Every truncation into the header or block fails, as does one with no sample data.
    for end in 0..fixture.len() - 2 {
        assert!(ExtendedVideoPacket::decode(&fixture[..end], FrameVersion::V4).is_err());
    }
}

#[test]
fn frames_v4_absent_tlvs_have_defined_defaults_and_a_bare_v4_frame_is_valid() {
    // A frame with no TLVs: not discardable, temporal layer 0, no timing, no LTR, no
    // epoch. The encoder never sets 0x80 for it; it is byte-identical to v3.
    let bare = ExtendedVideoPacket {
        metadata: DELTA,
        extensions: VideoExtensions::default(),
        data: &[0xaa],
    };
    let mut v3 = v4_header(0);
    v3.push(0xaa);
    assert_eq!(bare.encode(FrameVersion::V4).unwrap(), v3);
    let decoded = ExtendedVideoPacket::decode(&v3, FrameVersion::V4).unwrap();
    assert_eq!(decoded, bare);
    assert!(!decoded.extensions.discardable);
    assert_eq!(decoded.extensions.temporal_id, 0);
    // The flag with an empty block (ext_len 0) is accepted with the same meaning.
    let mut empty = v4_header(0x80);
    empty.extend([0, 0, 0xaa]);
    assert_eq!(
        ExtendedVideoPacket::decode(&empty, FrameVersion::V4).unwrap(),
        bare
    );
    // An explicit temporal_id 0 equals an absent one; the canonical form omits it.
    let mut explicit = v4_header(0x80);
    explicit.extend([3, 0, 5, 1, 0, 0xaa]);
    assert_eq!(
        ExtendedVideoPacket::decode(&explicit, FrameVersion::V4).unwrap(),
        bare
    );
}

#[test]
fn frames_v4_unknown_tlvs_are_skipped_and_malformed_known_ones_are_invalid() {
    let decode = |block: &[u8]| {
        let mut b = v4_header(0x80);
        b.extend((block.len() as u16).to_le_bytes());
        b.extend(block);
        b.push(0xaa);
        ExtendedVideoPacket::decode(&b, FrameVersion::V4).map(|p| p.extensions)
    };
    // Unknown types 0x00, 0x07 and 0xff with any length are ignored.
    let skipped = decode(&[0, 0, 7, 3, 1, 2, 3, 0xff, 1, 9, 5, 1, 2]).unwrap();
    assert_eq!(skipped.temporal_id, 2);
    assert_eq!(
        VideoExtensions {
            temporal_id: 2,
            ..Default::default()
        },
        skipped
    );
    for bad in [
        &[5, 2, 1, 1][..],      // temporal_id with a 2-byte value
        &[3, 1, 1],             // discardable with a value
        &[2, 4, 0, 0, 0, 0],    // capture_us too short
        &[4, 5, 0, 0, 0, 0, 1], // LTR token zero
        &[5, 1, 1, 5, 1, 2],    // repeated known type
        &[7, 4, 1, 2],          // unknown TLV running past the block
        &[1],                   // truncated TLV header
    ] {
        assert!(decode(bad).is_err(), "{bad:?}");
    }
    // ext_len past the payload, an empty sample after the block, and a config frame.
    let mut long = v4_header(0x80);
    long.extend([9, 0, 5, 1, 1]);
    assert!(ExtendedVideoPacket::decode(&long, FrameVersion::V4).is_err());
    let mut no_data = v4_header(0x80);
    no_data.extend([3, 0, 5, 1, 1]);
    assert!(ExtendedVideoPacket::decode(&no_data, FrameVersion::V4).is_err());
    // Unknown low flag bits stay invalid with the extension bit set.
    let mut unknown = v4_header(0x84);
    unknown.extend([0, 0, 0xaa]);
    assert!(ExtendedVideoPacket::decode(&unknown, FrameVersion::V4).is_err());
}

#[test]
fn lane_zero_video_kinds_are_accepted_by_the_envelope() {
    for kind in 0x12..=0x15 {
        assert!(Frame::valid_kind(kind) && is_video_control_kind(kind));
        let bytes = Frame {
            kind,
            payload: &[1],
        }
        .encode()
        .unwrap();
        assert_eq!(&bytes[..5], &[kind, 0, 0, 0, 1]);
        assert_eq!(Frame::read(&bytes).unwrap().unwrap().0.kind, kind);
    }
    for kind in [0x0f, 0x16, 0x21, 0x41] {
        assert!(!Frame::valid_kind(kind) && !is_video_control_kind(kind));
        assert!(Frame::read(&[kind, 0, 0, 0, 0]).is_err());
    }
}

#[test]
fn video_feedback_fixture_is_exact() {
    let feedback = VideoFeedback {
        stream_id: 7,
        generation: 2,
        highest_complete_sequence: 0x30,
        consumed_bytes: 0x0102_0304_0506,
        flushes: 1,
        present_drops: 2,
        loss: Some(LossBitmap {
            base_sequence: 0x2e,
            bits: vec![0b0000_0101],
        }),
        ltr_ack: Some(9),
        frames: vec![FramePresentation {
            sequence: 0x30,
            receive_us: 10,
            decode_us: 11,
            present_us: 0,
        }],
    };
    let mut fixture = vec![7, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0];
    fixture.extend([0x30, 0, 0, 0, 0, 0, 0, 0]);
    fixture.extend([6, 5, 4, 3, 2, 1, 0, 0]);
    fixture.extend([1, 0, 0, 0, 2, 0, 0, 0]);
    fixture.push(3); // loss bitmap and LTR ack present
    fixture.extend([0x2e, 0, 0, 0, 0, 0, 0, 0, 1, 0b0000_0101]);
    fixture.extend([9, 0, 0, 0]);
    fixture.push(1);
    fixture.extend([0x30, 0, 0, 0, 0, 0, 0, 0]);
    fixture.extend([10, 0, 0, 0, 0, 0, 0, 0]);
    fixture.extend([11, 0, 0, 0, 0, 0, 0, 0]);
    fixture.extend([0; 8]);
    assert_eq!(feedback.encode().unwrap(), fixture);
    assert_eq!(VideoFeedback::decode(&fixture).unwrap(), feedback);
    validate_video_control(0x12, &fixture).unwrap();
    for end in 0..fixture.len() {
        assert!(VideoFeedback::decode(&fixture[..end]).is_err());
    }
    let mut long = fixture.clone();
    long.push(0);
    assert!(VideoFeedback::decode(&long).is_err());
    // The smallest body: no bitmap, no LTR ack, no frames (38 bytes).
    let minimal = VideoFeedback {
        loss: None,
        ltr_ack: None,
        frames: vec![],
        ..feedback.clone()
    };
    let bytes = minimal.encode().unwrap();
    assert_eq!(bytes.len(), 38);
    assert_eq!(VideoFeedback::decode(&bytes).unwrap(), minimal);
    // Unknown flag bits, zero generation, an empty bitmap, too many frames.
    let mut flags = bytes.clone();
    flags[36] = 4;
    assert!(VideoFeedback::decode(&flags).is_err());
    for bad in [
        VideoFeedback {
            generation: 0,
            ..minimal.clone()
        },
        VideoFeedback {
            loss: Some(LossBitmap {
                base_sequence: 1,
                bits: vec![],
            }),
            ..minimal.clone()
        },
        VideoFeedback {
            ltr_ack: Some(0),
            ..minimal.clone()
        },
        VideoFeedback {
            frames: vec![feedback.frames[0]; VideoFeedback::MAX_FRAMES + 1],
            ..minimal.clone()
        },
    ] {
        assert!(bad.encode().is_err());
    }
}

#[test]
fn video_nack_frame_timing_and_layout_fixtures_are_exact() {
    let nack = VideoNack {
        stream_id: 7,
        generation: 2,
        sequence: 3,
        fragments: vec![0, 5, 0x0102],
    };
    let mut fixture = vec![7, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0];
    fixture.extend([3, 0, 0, 0, 5, 0, 2, 1]);
    assert_eq!(nack.encode().unwrap(), fixture);
    assert_eq!(VideoNack::decode(&fixture).unwrap(), nack);
    let mut unordered = fixture.clone();
    unordered[24] = 0; // fragments 0, 0, 0x0102
    assert!(VideoNack::decode(&unordered).is_err());
    assert!(VideoNack::decode(&fixture[..fixture.len() - 1]).is_err());
    assert!(
        VideoNack {
            fragments: vec![],
            ..nack.clone()
        }
        .encode()
        .is_err()
    );

    let timing = FrameTimingReport {
        stream_id: 7,
        generation: 2,
        sequence: 3,
        h5_us: 0x10,
        h6_us: 0x20,
        backpressured: true,
    };
    let mut fixture = vec![7, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0];
    fixture.extend([0x10, 0, 0, 0, 0, 0, 0, 0, 0x20, 0, 0, 0, 0, 0, 0, 0, 1]);
    assert_eq!(timing.encode().unwrap().to_vec(), fixture);
    assert_eq!(FrameTimingReport::decode(&fixture).unwrap(), timing);
    let mut flags = fixture.clone();
    flags[36] = 2;
    assert!(FrameTimingReport::decode(&flags).is_err());
    assert!(
        FrameTimingReport {
            h6_us: 0x0f,
            ..timing
        }
        .encode()
        .is_err()
    );

    let layout = LayoutInfo {
        stream_id: 7,
        epoch: 4,
        source: SourceRect {
            x: -1920,
            y: 0,
            width: 1920,
            height: 1080,
        },
        frame_width: 1280,
        frame_height: 720,
        content: ContentRect {
            x: 0,
            y: 0,
            width: 1280,
            height: 720,
        },
        rotation: 1,
    };
    let mut fixture = vec![7, 0, 0, 0, 4, 0, 0, 0];
    fixture.extend([0x80, 0xf8, 0xff, 0xff, 0, 0, 0, 0]); // x -1920, y 0
    fixture.extend([0x80, 7, 0, 0, 0x38, 4, 0, 0]); // 1920 x 1080
    fixture.extend([0, 5, 0, 0, 0xd0, 2, 0, 0]); // frame 1280 x 720
    fixture.extend([0, 0, 0, 0, 0, 0, 0, 0, 0, 5, 0, 0, 0xd0, 2, 0, 0]);
    fixture.push(1);
    assert_eq!(layout.encode().unwrap().to_vec(), fixture);
    assert_eq!(LayoutInfo::decode(&fixture).unwrap(), layout);
    validate_video_control(0x15, &fixture).unwrap();
    assert!(validate_video_control(0x14, &fixture).is_err());
    for bad in [
        LayoutInfo {
            rotation: 4,
            ..layout
        },
        LayoutInfo {
            content: ContentRect {
                x: 1,
                ..layout.content
            },
            ..layout
        },
        LayoutInfo {
            source: SourceRect {
                width: 0,
                ..layout.source
            },
            ..layout
        },
    ] {
        assert!(bad.encode().is_err());
    }
}
