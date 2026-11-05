//! 请求序号：只有最新一次请求的结果有效。

#[derive(Debug, Default)]
pub struct RequestSeq {
    latest: u64,
}

impl RequestSeq {
    /// 发起新请求，之前的请求全部过期。
    pub fn next(&mut self) -> u64 {
        self.latest += 1;
        self.latest
    }

    pub fn is_current(&self, seq: u64) -> bool {
        seq == self.latest
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_latest_request_is_current() {
        let mut s = RequestSeq::default();
        let a = s.next();
        assert!(s.is_current(a));
        let b = s.next();
        assert!(!s.is_current(a));
        assert!(s.is_current(b));
    }
}
