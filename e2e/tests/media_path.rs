//! Frame → OpenH264 → RTP packetizer → depacketizer (SampleBuilder + H264Packet) → FFmpeg
//! software decoder, all in one process and without a network. This is the CI proof that the
//! server's bitstream is something the client can decode picture for picture, and that the
//! client's recovery policy kicks in when the parameter sets get lost.

use std::time::{Duration, Instant};

use bytes::Bytes;
use client::decoder::ffmpeg::FfmpegDecoder;
use client::decoder::{AccessUnitInfo, HwPreference, RecoveryPolicy, VideoDecoder};
use client::net::new_assembler;
use e2e::nal_types;
use rtc::rtp::codec::h264::H264Payloader;
use rtc::rtp::packetizer::{new_packetizer, Packetizer};
use rtc::rtp::sequence::new_fixed_sequencer;
use server::encoder::{self, EncodedFrame, EncoderSettings};
use server::testpattern;

const W: usize = 640;
const H: usize = 360;
const FPS: u32 = 30;
const SSRC: u32 = 0x5EED_1234;
const PAYLOAD_TYPE: u8 = 102;
/// Same outbound MTU as `TrackLocalStaticSample`.
const MTU: usize = 1200;

fn encode(frames: usize, idr_at: &[usize]) -> Vec<EncodedFrame> {
    let mut enc = encoder::create(EncoderSettings {
        width: W as u32,
        height: H as u32,
        fps: FPS,
        bitrate_kbps: 2500,
        keyframe_interval: 0,
        threads: 2,
    })
    .expect("encoder");
    (0..frames)
        .map(|i| {
            let frame = testpattern::i420(W, H, i);
            enc.encode(
                &frame,
                i as u64 * 1000 / u64::from(FPS),
                idr_at.contains(&i),
            )
            .expect("encode")
            .expect("frame skipping is disabled")
        })
        .collect()
}

fn packetizer() -> Box<dyn Packetizer> {
    Box::new(new_packetizer(
        Instant::now(),
        MTU,
        PAYLOAD_TYPE,
        SSRC,
        Box::new(H264Payloader::default()),
        Box::new(new_fixed_sequencer(43300)),
        90_000,
    ))
}

fn psnr_y(a: &[u8], b: &[u8]) -> f64 {
    let mse = a
        .iter()
        .zip(b)
        .map(|(&p, &q)| {
            let d = f64::from(p) - f64::from(q);
            d * d
        })
        .sum::<f64>()
        / a.len() as f64;
    if mse == 0.0 {
        99.0
    } else {
        10.0 * (255.0f64 * 255.0 / mse).log10()
    }
}

#[test]
fn encode_packetize_depacketize_decode_yields_every_picture() {
    let frames = encode(24, &[0, 12]);
    let mut packetizer = packetizer();
    let mut assembler = new_assembler();
    let mut decoder = FfmpegDecoder::new(HwPreference::Software).expect("software decoder");
    assert!(!decoder.is_hardware());

    let mut now = Instant::now();
    let mut reassembled: Vec<Bytes> = Vec::new();
    let mut packets_total = 0usize;
    for frame in &frames {
        let packets = packetizer
            .packetize(now, &Bytes::from(frame.data.clone()), 90_000 / FPS)
            .expect("packetize");
        assert!(!packets.is_empty());
        packets_total += packets.len();
        // Every packet fits the MTU; only the last one of a frame carries the marker.
        for (i, p) in packets.iter().enumerate() {
            assert!(p.payload.len() + 12 <= MTU, "packet exceeds MTU");
            assert_eq!(p.header.marker, i + 1 == packets.len());
            assert_eq!(p.header.payload_type, PAYLOAD_TYPE);
            assert_eq!(p.header.ssrc, SSRC);
        }
        if frame.keyframe {
            // A keyframe spans several packets (SPS/PPS in a STAP-A or their own packets,
            // then FU-A fragments of the IDR slices).
            assert!(
                packets.len() > 1,
                "keyframe of {} bytes in one packet",
                frame.data.len()
            );
        }
        let before = reassembled.len();
        for p in packets {
            assembler.push(now, p);
            while let Some(au) = assembler.pop(now) {
                reassembled.push(au);
            }
        }
        // Marker driven: the access unit is out as soon as its last packet is in.
        assert_eq!(
            reassembled.len(),
            before + 1,
            "frame not emitted on its marker"
        );
        now += Duration::from_millis(33);
    }
    assert!(
        packets_total > frames.len() * 2,
        "expected multi-packet frames"
    );
    assert_eq!(reassembled.len(), frames.len());
    assert_eq!(assembler.stats.dropped_units, 0);

    // Reassembly is lossless: same NAL units in the same order.
    for (i, (original, rebuilt)) in frames.iter().zip(&reassembled).enumerate() {
        let want = nal_types(&original.data);
        let got = nal_types(rebuilt);
        assert_eq!(
            want, got,
            "frame {i}: NAL types differ after RTP round trip"
        );
        if original.keyframe {
            assert!(
                got.contains(&7) && got.contains(&8) && got.contains(&5),
                "{got:?}"
            );
        }
    }

    // Every access unit decodes to exactly one picture, immediately (no reordering delay).
    let mut pictures = 0;
    let mut last_psnr = 0.0;
    for (i, au) in reassembled.iter().enumerate() {
        let mut out = Vec::new();
        decoder
            .decode(au, &mut out)
            .unwrap_or_else(|e| panic!("frame {i}: {e}"));
        assert_eq!(out.len(), 1, "frame {i} produced {} pictures", out.len());
        let img = &out[0];
        assert_eq!((img.width as usize, img.height as usize), (W, H));
        let reference = testpattern::i420(W, H, i);
        last_psnr = psnr_y(&reference.y, &img.data[..W * H]);
        pictures += 1;
    }
    assert_eq!(pictures, frames.len());
    assert_eq!(decoder.pictures_decoded(), frames.len() as u64);
    assert!(last_psnr > 30.0, "PSNR {last_psnr:.1} dB");
}

/// Losing the packet that carries SPS/PPS leaves an IDR the decoder cannot use. The client's
/// recovery policy must ask for a keyframe (rate limited), and decoding must resume as soon as
/// a complete IDR arrives.
#[test]
fn lost_parameter_sets_trigger_a_keyframe_request_and_recovery() {
    let frames = encode(6, &[0, 3]);
    let mut packetizer = packetizer();
    let mut assembler = new_assembler();
    let mut decoder = FfmpegDecoder::new(HwPreference::Software).expect("software decoder");
    let mut policy = RecoveryPolicy::new(Duration::from_secs(1));
    let mut now = Instant::now();
    let mut requests_at: Vec<usize> = Vec::new();
    let mut decoded_at: Vec<usize> = Vec::new();

    for (i, frame) in frames.iter().enumerate() {
        let mut packets = packetizer
            .packetize(now, &Bytes::from(frame.data.clone()), 90_000 / FPS)
            .unwrap();
        if i == 0 {
            // Drop the first packet of the first IDR: it carries the SPS/PPS (STAP-A).
            let first = packets.remove(0);
            let types = nal_types(&[&[0, 0, 0, 1][..], &first.payload[..]].concat());
            assert!(
                types.contains(&24) || types.contains(&7),
                "first packet should be a STAP-A or the SPS, got NAL type {types:?}"
            );
        }
        for p in packets {
            assembler.push(now, p);
        }
        while let Some(au) = assembler.pop(now) {
            let info = AccessUnitInfo::scan(&au);
            let before = decoder.pictures_decoded();
            let mut out = Vec::new();
            let result = decoder.decode(&au, &mut out);
            if policy.after_access_unit(now, info, before, out.len(), result.is_err()) {
                requests_at.push(i);
            }
            if !out.is_empty() {
                decoded_at.push(i);
            }
        }
        now += Duration::from_millis(33);
    }

    // The assembler resynchronises on the next partition head (the first IDR slice's FU-A
    // start), so the decoder sees an IDR without SPS/PPS right away and asks for a keyframe.
    assert_eq!(
        requests_at.first(),
        Some(&0),
        "keyframe request expected at frame 0, got {requests_at:?}"
    );
    // Rate limited: everything happened within one second, so a single request.
    assert_eq!(requests_at.len(), 1, "requests at {requests_at:?}");
    // Nothing decodes before the complete IDR at frame 3; everything from there on does.
    assert_eq!(decoded_at, vec![3, 4, 5], "decoded at {decoded_at:?}");
    assert_eq!(decoder.pictures_decoded(), 3);
}
