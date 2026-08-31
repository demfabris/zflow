pub const FRAME_LEN: usize = 40;

/// Cumulative motion since sender start. Losing a frame loses nothing:
/// any later frame carries the running totals. `session` is random per
/// sender run so the receiver detects restarts instead of guessing from
/// sequence numbers.
#[derive(Debug, Clone, Copy)]
pub struct MotionFrame {
    pub session: u64,
    pub seq: u64,
    pub t_capture_us: u64,
    pub total_dx: i64,
    pub total_dy: i64,
}

impl MotionFrame {
    pub fn encode(&self) -> [u8; FRAME_LEN] {
        let mut b = [0u8; FRAME_LEN];
        b[0..8].copy_from_slice(&self.session.to_le_bytes());
        b[8..16].copy_from_slice(&self.seq.to_le_bytes());
        b[16..24].copy_from_slice(&self.t_capture_us.to_le_bytes());
        b[24..32].copy_from_slice(&self.total_dx.to_le_bytes());
        b[32..40].copy_from_slice(&self.total_dy.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() < FRAME_LEN {
            return None;
        }
        Some(Self {
            session: u64::from_le_bytes(b[0..8].try_into().ok()?),
            seq: u64::from_le_bytes(b[8..16].try_into().ok()?),
            t_capture_us: u64::from_le_bytes(b[16..24].try_into().ok()?),
            total_dx: i64::from_le_bytes(b[24..32].try_into().ok()?),
            total_dy: i64::from_le_bytes(b[32..40].try_into().ok()?),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let f = MotionFrame {
            session: 0xdeadbeef,
            seq: 42,
            t_capture_us: 1_234_567,
            total_dx: -981,
            total_dy: i64::MAX,
        };
        let d = MotionFrame::decode(&f.encode()).unwrap();
        assert_eq!(d.session, f.session);
        assert_eq!(d.seq, f.seq);
        assert_eq!(d.t_capture_us, f.t_capture_us);
        assert_eq!(d.total_dx, f.total_dx);
        assert_eq!(d.total_dy, f.total_dy);
    }

    #[test]
    fn short_buffer_rejected() {
        assert!(MotionFrame::decode(&[0u8; FRAME_LEN - 1]).is_none());
    }
}
