//! Per-peer jitter buffer: reorders packets and decides, every 20 ms,
//! whether we have the next frame, lost it, or are waiting for more.

use std::collections::BTreeMap;

pub enum Pop {
    Packet(Vec<u8>),
    /// The expected frame is missing; `next` is the following packet if we
    /// have it (for FEC recovery).
    Lost {
        next: Option<Vec<u8>>,
    },
    /// Not playing (buffering or the peer is silent).
    Empty,
}

pub struct JitterBuffer {
    packets: BTreeMap<u32, Vec<u8>>,
    next: u32,
    playing: bool,
    /// Frames to buffer before (re)starting playback.
    target: usize,
    /// Above this many frames we drop the oldest to cut latency.
    max: usize,
    misses: u32,
}

impl Default for JitterBuffer {
    fn default() -> Self {
        Self::new(3, 12)
    }
}

impl JitterBuffer {
    pub fn new(target: usize, max: usize) -> Self {
        Self {
            packets: BTreeMap::new(),
            next: 0,
            playing: false,
            target,
            max,
            misses: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.packets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.packets.is_empty()
    }

    pub fn insert(&mut self, seq: u32, data: Vec<u8>) {
        if self.playing && seq < self.next {
            return; // too late
        }
        self.packets.insert(seq, data);
        while self.packets.len() > self.max {
            if let Some((&oldest, _)) = self.packets.iter().next() {
                self.packets.remove(&oldest);
                if self.playing && self.next <= oldest {
                    self.next = oldest + 1;
                }
            }
        }
    }

    pub fn pop(&mut self) -> Pop {
        if !self.playing {
            if self.packets.len() < self.target {
                return Pop::Empty;
            }
            self.playing = true;
            self.misses = 0;
            self.next = *self.packets.keys().next().expect("non-empty");
        }
        let seq = self.next;
        if let Some(data) = self.packets.remove(&seq) {
            self.next = seq.wrapping_add(1);
            self.misses = 0;
            return Pop::Packet(data);
        }
        if self.packets.is_empty() {
            // Ran dry: the peer stopped sending (muted/DTX) or a burst was
            // lost. Rebuffer instead of concealing silence forever.
            self.misses += 1;
            if self.misses > 2 {
                self.playing = false;
                return Pop::Empty;
            }
            self.next = seq.wrapping_add(1);
            return Pop::Lost { next: None };
        }
        self.next = seq.wrapping_add(1);
        Pop::Lost {
            next: self.packets.get(&self.next).cloned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(p: Pop) -> String {
        match p {
            Pop::Packet(d) => format!("p{}", d[0]),
            Pop::Lost { next: Some(d) } => format!("lost(next p{})", d[0]),
            Pop::Lost { next: None } => "lost".into(),
            Pop::Empty => "empty".into(),
        }
    }

    #[test]
    fn buffers_then_plays_in_order() {
        let mut jb = JitterBuffer::new(3, 12);
        jb.insert(11, vec![11]);
        jb.insert(10, vec![10]);
        assert_eq!(kind(jb.pop()), "empty");
        jb.insert(12, vec![12]);
        assert_eq!(kind(jb.pop()), "p10");
        assert_eq!(kind(jb.pop()), "p11");
        assert_eq!(kind(jb.pop()), "p12");
    }

    #[test]
    fn reports_loss_with_fec_source_and_drops_late_packets() {
        let mut jb = JitterBuffer::new(2, 12);
        for s in [1, 2, 4, 5] {
            jb.insert(s, vec![s as u8]);
        }
        assert_eq!(kind(jb.pop()), "p1");
        assert_eq!(kind(jb.pop()), "p2");
        assert_eq!(kind(jb.pop()), "lost(next p4)");
        jb.insert(3, vec![3]); // arrived too late
        assert_eq!(kind(jb.pop()), "p4");
        assert_eq!(kind(jb.pop()), "p5");
    }

    #[test]
    fn rebuffers_after_running_dry() {
        let mut jb = JitterBuffer::new(2, 12);
        jb.insert(1, vec![1]);
        jb.insert(2, vec![2]);
        assert_eq!(kind(jb.pop()), "p1");
        assert_eq!(kind(jb.pop()), "p2");
        assert_eq!(kind(jb.pop()), "lost");
        assert_eq!(kind(jb.pop()), "lost");
        assert_eq!(kind(jb.pop()), "empty");
        jb.insert(40, vec![40]);
        assert_eq!(kind(jb.pop()), "empty");
        jb.insert(41, vec![41]);
        assert_eq!(kind(jb.pop()), "p40");
    }

    #[test]
    fn caps_latency_by_dropping_oldest() {
        let mut jb = JitterBuffer::new(2, 4);
        for s in 0..10 {
            jb.insert(s, vec![s as u8]);
        }
        assert_eq!(jb.len(), 4);
        assert_eq!(kind(jb.pop()), "p6");
    }
}
