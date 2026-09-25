//! Reassembly of H.264 access units from RTP packets (RFC 6184, packetization mode 1).
//!
//! Replaces `rtc-media`'s `SampleBuilder`, which is built around a fixed packet window: a frame
//! with more packets than the window (every 1080p IDR at 12 Mbit/s) is force-purged, its
//! packets are released one at a time and then re-emitted as a second, truncated "sample", or
//! dropped altogether. It also holds a completed frame back until the *next* frame's first
//! packet arrives, one frame of added latency.
//!
//! This assembler is marker driven, as the RFC intends for video: an access unit is complete
//! when its packets from the first (a partition head: single NAL, STAP-A or FU-A start) to the
//! one carrying the marker bit are all present, and it is emitted at that moment. Recovery:
//!
//! * a missing packet is waited for up to [`AccessUnitAssembler::max_wait`] (NACK
//!   retransmissions arrive within an RTT), then the unit is abandoned and reassembly resumes
//!   at the next partition head of a later timestamp;
//! * a lost marker is detected when the next timestamp starts; the unit is emitted if its last
//!   packet closed a NAL unit (single NAL, STAP-A or FU-A end), dropped otherwise;
//! * late duplicates and packets of abandoned units are ignored;
//! * the buffer is bounded by [`AccessUnitAssembler::max_pending`] packets.
//!
//! Sequence numbers are unwrapped to 64 bits, so wrap-around needs no special cases.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use rtc::rtp::codec::h264::H264Packet;
use rtc::rtp::packetizer::Depacketizer;
use rtc::rtp::Packet;

const NALU_TYPE_MASK: u8 = 0x1F;
const NALU_FU_A: u8 = 28;
const NALU_FU_B: u8 = 29;
const FU_END_BIT: u8 = 0x40;

/// Counters exposed for logs and tests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AssemblerStats {
    /// Complete access units handed out.
    pub access_units: u64,
    /// Units abandoned (missing packet not recovered in time, lost tail, bad payload).
    pub dropped_units: u64,
    /// Packets discarded (part of abandoned units, or arriving before the first head).
    pub dropped_packets: u64,
    /// Packets that arrived after their unit had been emitted or abandoned (duplicates,
    /// late retransmissions).
    pub late_packets: u64,
}

struct Arrival {
    packet: Packet,
    at: Instant,
}

/// Maps 16-bit RTP sequence numbers onto a monotonically increasing 64-bit space.
#[derive(Debug, Default)]
struct SeqUnwrapper {
    highest: Option<u64>,
}

impl SeqUnwrapper {
    fn extend(&mut self, seq: u16) -> u64 {
        match self.highest {
            None => {
                // Start well above zero so packets older than the first one stay representable.
                let ext = (1u64 << 32) + u64::from(seq);
                self.highest = Some(ext);
                ext
            }
            Some(highest) => {
                let delta = seq.wrapping_sub(highest as u16);
                let ext = if delta < 0x8000 {
                    highest + u64::from(delta)
                } else {
                    highest - (0x1_0000 - u64::from(delta))
                };
                if ext > highest {
                    self.highest = Some(ext);
                }
                ext
            }
        }
    }
}

/// True if this RTP payload starts a NAL unit (RFC 6184 §5.8 partition head).
pub fn is_partition_head(payload: &[u8]) -> bool {
    H264Packet::default().is_partition_head(&Bytes::copy_from_slice(payload))
}

/// True if this payload ends a NAL unit: anything but an FU-A/FU-B fragment without the E bit.
pub fn ends_nal_unit(payload: &[u8]) -> bool {
    match payload.first().map(|b| b & NALU_TYPE_MASK) {
        Some(NALU_FU_A) | Some(NALU_FU_B) => payload.get(1).is_some_and(|b| b & FU_END_BIT != 0),
        Some(_) => true,
        None => false,
    }
}

pub struct AccessUnitAssembler {
    depacketizer: H264Packet,
    pending: BTreeMap<u64, Arrival>,
    unwrapper: SeqUnwrapper,
    /// Extended sequence number of the next packet to consume; `None` while resynchronising
    /// (waiting for a partition head).
    next: Option<u64>,
    /// Everything below this was consumed or abandoned.
    floor: u64,
    /// How long a missing packet is waited for before its unit is abandoned.
    pub max_wait: Duration,
    /// Buffer bound in packets; exceeding it abandons the oldest unit.
    pub max_pending: usize,
    pub stats: AssemblerStats,
}

impl AccessUnitAssembler {
    pub fn new(max_wait: Duration, max_pending: usize) -> Self {
        Self {
            depacketizer: H264Packet::default(),
            pending: BTreeMap::new(),
            unwrapper: SeqUnwrapper::default(),
            next: None,
            floor: 0,
            max_wait,
            max_pending: max_pending.max(2),
            stats: AssemblerStats::default(),
        }
    }

    /// Buffers one RTP packet. `now` is its arrival time.
    pub fn push(&mut self, now: Instant, packet: Packet) {
        let ext = self.unwrapper.extend(packet.header.sequence_number);
        if ext < self.floor {
            self.stats.late_packets += 1;
            return;
        }
        if self
            .pending
            .insert(ext, Arrival { packet, at: now })
            .is_some()
        {
            // Same sequence number twice (retransmission of a packet we already have).
            self.stats.late_packets += 1;
        }
        if self.pending.len() > self.max_pending {
            self.abandon_oldest_unit();
        }
    }

    /// Returns the next complete access unit (Annex B), if any.
    pub fn pop(&mut self, now: Instant) -> Option<Bytes> {
        loop {
            let next = match self.next {
                Some(n) => n,
                None => self.resync()?,
            };
            let Some(head) = self.pending.get(&next) else {
                // The head packet is missing: wait for it as long as the packets behind it
                // are younger than `max_wait`, then give the unit up.
                match self.pending.iter().next() {
                    Some((_, oldest)) if now.duration_since(oldest.at) >= self.max_wait => {
                        self.abandon_oldest_unit();
                        continue;
                    }
                    _ => return None,
                }
            };
            let timestamp = head.packet.header.timestamp;
            let first_arrival = head.at;

            // Walk the consecutive packets of this unit.
            let mut i = next;
            let mut end: Option<u64> = None;
            let mut next_unit_started = false;
            loop {
                match self.pending.get(&i) {
                    None => break,
                    Some(a) if a.packet.header.timestamp != timestamp => {
                        next_unit_started = true;
                        break;
                    }
                    Some(a) => {
                        if a.packet.header.marker {
                            end = Some(i);
                            break;
                        }
                        i += 1;
                    }
                }
            }
            let end = match end {
                Some(e) => e,
                None if next_unit_started => {
                    // Marker lost. Complete if the last packet closed a NAL unit.
                    let last = i - 1;
                    let closed = self
                        .pending
                        .get(&last)
                        .is_some_and(|a| ends_nal_unit(&a.packet.payload));
                    if closed {
                        last
                    } else {
                        self.abandon_unit(timestamp);
                        continue;
                    }
                }
                None => {
                    // A gap inside the unit (or simply not everything has arrived yet).
                    let waited_since = self
                        .pending
                        .range(i..)
                        .next()
                        .map(|(_, a)| a.at)
                        .unwrap_or(first_arrival);
                    let has_gap = self.pending.range(i..).next().is_some();
                    if has_gap && now.duration_since(waited_since) >= self.max_wait {
                        self.abandon_unit(timestamp);
                        continue;
                    }
                    return None;
                }
            };

            // Depacketize `next..=end` into one Annex B access unit.
            self.depacketizer = H264Packet::default();
            let mut out = BytesMut::new();
            let mut ok = true;
            for s in next..=end {
                let a = self.pending.remove(&s).expect("walked packets are present");
                match self.depacketizer.depacketize(&a.packet.payload) {
                    Ok(b) => out.extend_from_slice(&b),
                    Err(_) => ok = false,
                }
            }
            self.next = Some(end + 1);
            self.floor = end + 1;
            if !ok || out.is_empty() {
                self.stats.dropped_units += 1;
                self.stats.dropped_packets += end - next + 1;
                continue;
            }
            self.stats.access_units += 1;
            return Some(out.freeze());
        }
    }

    /// Drops every buffered packet with `timestamp` and resynchronises.
    fn abandon_unit(&mut self, timestamp: u32) {
        let doomed: Vec<u64> = self
            .pending
            .iter()
            .filter(|(_, a)| a.packet.header.timestamp == timestamp)
            .map(|(k, _)| *k)
            .collect();
        for k in &doomed {
            self.pending.remove(k);
        }
        self.stats.dropped_units += 1;
        self.stats.dropped_packets += doomed.len() as u64;
        if let Some(max) = doomed.iter().max() {
            self.floor = self.floor.max(*max + 1);
        }
        self.next = None;
    }

    /// Abandons the unit the oldest buffered packet belongs to.
    fn abandon_oldest_unit(&mut self) {
        if let Some((_, oldest)) = self.pending.iter().next() {
            let ts = oldest.packet.header.timestamp;
            self.abandon_unit(ts);
        }
    }

    /// After a drop: skip to the first buffered partition head and start there.
    fn resync(&mut self) -> Option<u64> {
        let head = self
            .pending
            .iter()
            .find(|(_, a)| is_partition_head(&a.packet.payload))
            .map(|(k, _)| *k);
        match head {
            Some(k) => {
                let stale: Vec<u64> = self.pending.range(..k).map(|(s, _)| *s).collect();
                self.stats.dropped_packets += stale.len() as u64;
                for s in stale {
                    self.pending.remove(&s);
                }
                self.floor = self.floor.max(k);
                self.next = Some(k);
                Some(k)
            }
            None => {
                // Nothing usable buffered; forget whatever is there (mid-unit fragments).
                self.stats.dropped_packets += self.pending.len() as u64;
                if let Some((&last, _)) = self.pending.iter().next_back() {
                    self.floor = self.floor.max(last + 1);
                }
                self.pending.clear();
                None
            }
        }
    }

    /// Packets currently buffered.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtc::rtp::codec::h264::H264Payloader;
    use rtc::rtp::packetizer::{new_packetizer, Packetizer};
    use rtc::rtp::sequence::new_fixed_sequencer;

    const MTU: usize = 1200;

    /// A NAL unit with a 4-byte start code. `fill` must be non-zero: a real bitstream never
    /// contains `00 00 0x` inside a NAL unit (emulation prevention), and the payloader relies
    /// on that to find NAL boundaries.
    fn nal(nal_type: u8, len: usize, fill: u8) -> Vec<u8> {
        assert_ne!(fill, 0);
        let mut v = vec![0, 0, 0, 1, 0x60 | nal_type];
        v.extend(std::iter::repeat_n(fill, len));
        v
    }

    /// Access unit `index`: SPS + PPS + two big IDR slices for keyframes, two P slices
    /// otherwise.
    fn access_unit(index: usize) -> Vec<u8> {
        let fill = (index % 250) as u8 + 1;
        let mut au = Vec::new();
        if index.is_multiple_of(10) {
            au.extend(nal(7, 12, 0x11));
            au.extend(nal(8, 4, 0x22));
            au.extend(nal(5, 30_000 + index * 7, fill));
            au.extend(nal(5, 20_000, 0x33));
        } else {
            au.extend(nal(1, 900 + (index * 131) % 2_500, fill));
            au.extend(nal(1, 300, 0x44));
        }
        au
    }

    fn nal_types(annexb: &[u8]) -> Vec<u8> {
        let mut types = Vec::new();
        let mut i = 0;
        while i + 4 <= annexb.len() {
            if annexb[i..i + 4] == [0, 0, 0, 1] {
                types.push(annexb[i + 4] & 0x1F);
                i += 5;
            } else {
                i += 1;
            }
        }
        types
    }

    fn packetize(units: usize, first_seq: u16) -> Vec<Vec<Packet>> {
        let mut p = new_packetizer(
            Instant::now(),
            MTU,
            102,
            7,
            Box::new(H264Payloader::default()),
            Box::new(new_fixed_sequencer(first_seq)),
            90_000,
        );
        (0..units)
            .map(|i| {
                p.packetize(Instant::now(), &Bytes::from(access_unit(i)), 3000)
                    .unwrap()
            })
            .collect()
    }

    fn assembler() -> AccessUnitAssembler {
        AccessUnitAssembler::new(Duration::from_millis(500), 4096)
    }

    #[test]
    fn units_come_out_on_the_marker_without_waiting_for_the_next_frame() {
        let mut a = assembler();
        let now = Instant::now();
        for (i, packets) in packetize(12, 100).into_iter().enumerate() {
            let expected = access_unit(i);
            assert!(packets.len() > 1 || i % 10 != 0, "IDR must span packets");
            let n = packets.len();
            for (k, p) in packets.into_iter().enumerate() {
                a.push(now, p);
                let popped = a.pop(now);
                if k + 1 < n {
                    assert!(popped.is_none(), "unit {i} emitted before its marker");
                } else {
                    let au = popped.unwrap_or_else(|| panic!("unit {i} not emitted on marker"));
                    assert_eq!(au.as_ref(), &expected[..], "unit {i} differs");
                }
            }
        }
        assert_eq!(a.stats.access_units, 12);
        assert_eq!(a.stats.dropped_units, 0);
        assert_eq!(a.pending(), 0);
    }

    #[test]
    fn large_units_are_not_truncated_or_duplicated() {
        // 50 KB IDR = ~43 packets, far beyond the old 64-packet window when several queue up.
        let mut a = AccessUnitAssembler::new(Duration::from_millis(500), 4096);
        let now = Instant::now();
        let frames = packetize(21, 60_000); // crosses the u16 wrap-around too
        let mut out = Vec::new();
        for packets in frames {
            for p in packets {
                a.push(now, p);
                while let Some(au) = a.pop(now) {
                    out.push(au);
                }
            }
        }
        assert_eq!(out.len(), 21);
        for (i, au) in out.iter().enumerate() {
            assert_eq!(au.as_ref(), &access_unit(i)[..], "unit {i}");
        }
        assert_eq!(nal_types(&out[0]), vec![7, 8, 5, 5]);
        assert_eq!(nal_types(&out[10]), vec![7, 8, 5, 5]);
    }

    #[test]
    fn reordered_packets_within_a_unit_are_fine() {
        let mut a = assembler();
        let now = Instant::now();
        let mut frames = packetize(2, 500);
        let idr = frames.remove(0);
        let n = idr.len();
        assert!(n > 4);
        // Deliver the IDR with two packets swapped and the marker packet early.
        let mut order: Vec<usize> = (0..n).collect();
        order.swap(1, 2);
        let marker = order.remove(n - 1);
        order.insert(3, marker);
        let mut emitted = None;
        for idx in order {
            a.push(now, idr[idx].clone());
            if let Some(au) = a.pop(now) {
                emitted = Some(au);
            }
        }
        assert_eq!(emitted.unwrap().as_ref(), &access_unit(0)[..]);
        assert_eq!(a.stats.late_packets, 0);
    }

    #[test]
    fn a_lost_packet_is_waited_for_then_the_unit_is_abandoned() {
        let mut a = assembler();
        let t0 = Instant::now();
        let frames = packetize(3, 10);
        // Unit 0 (IDR) loses its third packet.
        for (k, p) in frames[0].iter().enumerate() {
            if k != 2 {
                a.push(t0, p.clone());
            }
        }
        assert!(a.pop(t0).is_none());
        // Unit 1 arrives 33 ms later: still waiting for the retransmission.
        let t1 = t0 + Duration::from_millis(33);
        for p in &frames[1] {
            a.push(t1, p.clone());
        }
        assert!(a.pop(t1).is_none(), "must not skip ahead before max_wait");
        // The retransmission arrives in time: both units come out, in order.
        let t2 = t0 + Duration::from_millis(60);
        a.push(t2, frames[0][2].clone());
        assert_eq!(a.pop(t2).unwrap().as_ref(), &access_unit(0)[..]);
        assert_eq!(a.pop(t2).unwrap().as_ref(), &access_unit(1)[..]);
        assert!(a.pop(t2).is_none());
        assert_eq!(a.stats.dropped_units, 0);

        // Same scenario, but the retransmission never comes.
        let mut a = assembler();
        let frames = packetize(3, 10);
        for (k, p) in frames[0].iter().enumerate() {
            if k != 2 {
                a.push(t0, p.clone());
            }
        }
        for p in &frames[1] {
            a.push(t1, p.clone());
        }
        assert!(a.pop(t1).is_none());
        let late = t1 + Duration::from_millis(600);
        for p in &frames[2] {
            a.push(late, p.clone());
        }
        // Unit 0 is abandoned, units 1 and 2 come out.
        assert_eq!(a.pop(late).unwrap().as_ref(), &access_unit(1)[..]);
        assert_eq!(a.pop(late).unwrap().as_ref(), &access_unit(2)[..]);
        assert_eq!(a.stats.dropped_units, 1);
        assert_eq!(a.stats.dropped_packets as usize, frames[0].len() - 1);
        // The retransmission arriving now is ignored.
        a.push(late, frames[0][2].clone());
        assert!(a.pop(late).is_none());
        assert_eq!(a.stats.late_packets, 1);
    }

    #[test]
    fn lost_marker_completes_on_the_next_timestamp_when_the_nal_is_closed() {
        let mut a = assembler();
        let now = Instant::now();
        let frames = packetize(3, 1000);
        // Unit 1 has two single-NAL packets; drop its second (the marker) entirely.
        for p in &frames[0] {
            a.push(now, p.clone());
        }
        assert!(a.pop(now).is_some());
        a.push(now, frames[1][0].clone());
        assert!(a.pop(now).is_none());
        // Unit 2 starts: unit 1 is still incomplete because packet frames[1][1] is missing,
        // not just its marker -> wait, then abandon.
        for p in &frames[2] {
            a.push(now + Duration::from_millis(1), p.clone());
        }
        assert!(a.pop(now + Duration::from_millis(1)).is_none());
        let later = now + Duration::from_millis(700);
        assert_eq!(a.pop(later).unwrap().as_ref(), &access_unit(2)[..]);
        assert_eq!(a.stats.dropped_units, 1);

        // A unit whose *marker bit* was cleared (not lost) still completes on the next start.
        let mut a = assembler();
        let mut frames = packetize(2, 2000);
        let last = frames[0].len() - 1;
        frames[0][last].header.marker = false;
        for p in &frames[0] {
            a.push(now, p.clone());
        }
        assert!(a.pop(now).is_none());
        a.push(now, frames[1][0].clone());
        assert_eq!(a.pop(now).unwrap().as_ref(), &access_unit(0)[..]);
    }

    #[test]
    fn stream_joined_mid_unit_starts_at_the_next_partition_head() {
        let mut a = assembler();
        let now = Instant::now();
        let frames = packetize(2, 77);
        // Skip the first 3 packets of the IDR (STAP-A and two fragments).
        for p in frames[0].iter().skip(3) {
            a.push(now, p.clone());
        }
        // The remaining fragments are not a head... except the second slice's FU-A start,
        // which is a partition head: that slice comes out alone (the decoder will reject it
        // and ask for a keyframe, which is the right outcome).
        let mut got = Vec::new();
        while let Some(au) = a.pop(now) {
            got.push(au);
        }
        assert!(got.len() <= 1);
        for p in &frames[1] {
            a.push(now, p.clone());
        }
        assert_eq!(a.pop(now).unwrap().as_ref(), &access_unit(1)[..]);
        assert!(a.stats.dropped_packets >= 1);
    }

    #[test]
    fn duplicates_and_buffer_bound() {
        let mut a = AccessUnitAssembler::new(Duration::from_secs(10), 8);
        let now = Instant::now();
        let frames = packetize(2, 5);
        let p = frames[1][0].clone();
        a.push(now, p.clone());
        a.push(now, p);
        assert_eq!(a.stats.late_packets, 1);
        // Fill beyond the bound with fragments of a unit whose head never arrives.
        for p in frames[0].iter().skip(1).take(12) {
            a.push(now, p.clone());
        }
        assert!(a.pending() <= 9, "pending {}", a.pending());
        assert!(a.stats.dropped_units >= 1);
    }

    #[test]
    fn helpers_classify_payloads() {
        assert!(is_partition_head(&[0x65, 0x88])); // single NAL
        assert!(is_partition_head(&[0x78, 0x05, 0x00])); // STAP-A
        assert!(is_partition_head(&[0x7C, 0x85, 0x00])); // FU-A with S bit
        assert!(!is_partition_head(&[0x7C, 0x05, 0x00])); // FU-A middle
        assert!(ends_nal_unit(&[0x65, 0x88]));
        assert!(ends_nal_unit(&[0x7C, 0x45, 0x00])); // FU-A with E bit
        assert!(!ends_nal_unit(&[0x7C, 0x05, 0x00]));
        assert!(!ends_nal_unit(&[]));
        let mut u = SeqUnwrapper::default();
        let a = u.extend(65_534);
        assert_eq!(u.extend(65_535), a + 1);
        assert_eq!(u.extend(0), a + 2);
        assert_eq!(u.extend(1), a + 3);
        assert_eq!(u.extend(65_535), a + 1); // late packet keeps its old position
        assert_eq!(u.extend(2), a + 4);
    }
}
