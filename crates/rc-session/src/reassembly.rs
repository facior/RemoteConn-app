//! Reassembles video frames from datagram fragments.
//!
//! H.264 P-frames depend on every previous frame, so once a frame is lost the
//! stream is unusable until the next keyframe: we drop frames and ask the host
//! for a keyframe instead of showing corrupted video.

use std::collections::BTreeMap;

use rc_proto::VideoPacket;

const MAX_FRAGMENTS: u32 = 8192;
const MAX_PENDING: usize = 32;
/// Bytes buffered across incomplete frames; beyond this the oldest go.
const MAX_BUFFERED: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompleteFrame {
    pub monitor_id: u32,
    pub frame_id: u64,
    pub keyframe: bool,
    pub capture_ts_us: u64,
    pub data: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Output {
    Frame(CompleteFrame),
    /// A frame was lost; the decoder needs a keyframe to recover.
    NeedKeyframe,
}

struct Partial {
    parts: Vec<Option<Vec<u8>>>,
    received: usize,
    bytes: usize,
    keyframe: bool,
    monitor_id: u32,
    capture_ts_us: u64,
}

#[derive(Default)]
pub struct Reassembler {
    pending: BTreeMap<u64, Partial>,
    last_frame: Option<u64>,
    /// True until a keyframe arrives (at start and after any loss).
    broken: bool,
    started: bool,
    /// Bytes held in `pending`.
    buffered: usize,
}

impl Reassembler {
    pub fn push(&mut self, pkt: VideoPacket) -> Option<Output> {
        let count = pkt.fragment_count;
        if count == 0 || count > MAX_FRAGMENTS || pkt.fragment_index >= count {
            return None;
        }
        if self.last_frame.is_some_and(|last| pkt.frame_id <= last) {
            return None; // late fragment of a finished or abandoned frame
        }

        let partial = self.pending.entry(pkt.frame_id).or_insert_with(|| Partial {
            parts: vec![None; count as usize],
            received: 0,
            bytes: 0,
            keyframe: pkt.keyframe,
            monitor_id: pkt.monitor_id,
            capture_ts_us: pkt.capture_ts_us,
        });
        if partial.parts.len() != count as usize {
            return None; // inconsistent fragment_count
        }
        let slot = &mut partial.parts[pkt.fragment_index as usize];
        if slot.is_none() {
            partial.bytes += pkt.data.len();
            self.buffered += pkt.data.len();
            *slot = Some(pkt.data);
            partial.received += 1;
        }
        if partial.received < partial.parts.len() {
            while self.pending.len() > MAX_PENDING || self.buffered > MAX_BUFFERED {
                match self.pending.pop_first() {
                    Some((_, dropped)) => self.buffered -= dropped.bytes,
                    None => break,
                }
            }
            return None;
        }

        let frame_id = pkt.frame_id;
        let partial = self.pending.remove(&frame_id).expect("present");
        self.buffered -= partial.bytes;
        // Anything older that is still incomplete is lost for good.
        let lost_older = self.pending.range(..frame_id).next().is_some();
        let newer = self.pending.split_off(&frame_id);
        for (_, old) in std::mem::replace(&mut self.pending, newer) {
            self.buffered -= old.bytes;
        }
        let gap = match self.last_frame {
            Some(last) => frame_id != last + 1 || lost_older,
            None => !partial.keyframe,
        };
        self.last_frame = Some(frame_id);

        if partial.keyframe {
            self.broken = false;
            self.started = true;
        } else if gap || !self.started {
            self.broken = true;
        }
        if self.broken {
            return Some(Output::NeedKeyframe);
        }
        let data = partial.parts.into_iter().flatten().flatten().collect();
        Some(Output::Frame(CompleteFrame {
            monitor_id: partial.monitor_id,
            frame_id,
            keyframe: partial.keyframe,
            capture_ts_us: partial.capture_ts_us,
            data,
        }))
    }

    /// True while waiting for a keyframe.
    pub fn needs_keyframe(&self) -> bool {
        self.broken || !self.started
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frags(frame_id: u64, keyframe: bool, data: &[u8], size: usize) -> Vec<VideoPacket> {
        let chunks: Vec<_> = data.chunks(size).collect();
        chunks
            .iter()
            .enumerate()
            .map(|(i, c)| VideoPacket {
                frame_id,
                keyframe,
                fragment_index: i as u32,
                fragment_count: chunks.len() as u32,
                data: c.to_vec(),
                ..Default::default()
            })
            .collect()
    }

    fn feed(r: &mut Reassembler, pkts: Vec<VideoPacket>) -> Vec<Output> {
        pkts.into_iter().filter_map(|p| r.push(p)).collect()
    }

    fn frame_ids(out: &[Output]) -> Vec<u64> {
        out.iter()
            .filter_map(|o| match o {
                Output::Frame(f) => Some(f.frame_id),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn reorders_fragments_within_frame() {
        let mut r = Reassembler::default();
        let mut pkts = frags(0, true, b"abcdefghij", 3);
        pkts.reverse();
        let out = feed(&mut r, pkts);
        assert_eq!(
            out,
            vec![Output::Frame(CompleteFrame {
                monitor_id: 0,
                frame_id: 0,
                keyframe: true,
                capture_ts_us: 0,
                data: b"abcdefghij".to_vec(),
            })]
        );
        assert!(!r.needs_keyframe());
    }

    #[test]
    fn waits_for_first_keyframe() {
        let mut r = Reassembler::default();
        assert_eq!(
            feed(&mut r, frags(5, false, b"p", 4)),
            vec![Output::NeedKeyframe]
        );
        let out = feed(&mut r, frags(6, true, b"idr", 4));
        assert_eq!(frame_ids(&out), vec![6]);
    }

    #[test]
    fn loss_drops_until_keyframe() {
        let mut r = Reassembler::default();
        let mut out = feed(&mut r, frags(0, true, b"k0", 4));
        out.extend(feed(&mut r, frags(1, false, b"p1", 4)));
        // Frame 2 loses a fragment, frame 3 arrives complete.
        let mut f2 = frags(2, false, b"p2p2p2", 2);
        f2.pop();
        out.extend(feed(&mut r, f2));
        let after_loss = feed(&mut r, frags(3, false, b"p3", 4));
        assert_eq!(after_loss, vec![Output::NeedKeyframe]);
        assert!(r.needs_keyframe());
        out.extend(feed(&mut r, frags(4, true, b"k4", 4)));
        assert_eq!(frame_ids(&out), vec![0, 1, 4]);
        assert!(!r.needs_keyframe());
    }

    #[test]
    fn ignores_late_and_bogus_fragments() {
        let mut r = Reassembler::default();
        feed(&mut r, frags(3, true, b"k", 4));
        assert!(feed(&mut r, frags(2, true, b"old", 4)).is_empty());
        let bogus = VideoPacket {
            frame_id: 9,
            fragment_index: 5,
            fragment_count: 2,
            ..Default::default()
        };
        assert!(r.push(bogus).is_none());
    }
}
