//! 端末 ID とハイブリッド論理時計 (HLC)。docs/sync.md §4 / D160。
//!
//! 時刻は**すべて呼び出し側が注入する** (`now_ms`)。このモジュールは
//! `SystemTime` を読まないので、テストは決定的で、実装が壁時計の巻き戻りに
//! 強いことも単体で検査できる。

use std::fmt;

use serde::{Deserialize, Serialize};

/// [`DeviceId`] の最大長 (バイト)。
pub const MAX_DEVICE_ID_LEN: usize = 64;

/// リモートの時刻がローカルの現在時刻よりこれ以上先なら [`HlcClock::observe`]
/// は拒否する (壊れた/悪意ある端末が時計を未来へ引き上げ続けるのを防ぐ)。
pub const MAX_CLOCK_DRIFT_MS: u64 = 24 * 60 * 60 * 1000;

/// 端末を一意に識別する ID。`[A-Za-z0-9_-]{1,64}`。
///
/// 生成 (ランダム値の採番) は呼び出し側の責務で、ここでは形だけを検査する。
/// 認証には使わない (名乗りにすぎない、docs/sync.md §3)。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct DeviceId(String);

/// [`DeviceId`] の形が不正。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidDeviceId;

impl fmt::Display for InvalidDeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid device id (expected [A-Za-z0-9_-]{{1,64}})")
    }
}

impl std::error::Error for InvalidDeviceId {}

impl DeviceId {
    pub fn new(s: &str) -> Result<Self, InvalidDeviceId> {
        let ok = !s.is_empty()
            && s.len() <= MAX_DEVICE_ID_LEN
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if ok {
            Ok(Self(s.to_owned()))
        } else {
            Err(InvalidDeviceId)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for DeviceId {
    type Error = InvalidDeviceId;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::new(&s)
    }
}

impl From<DeviceId> for String {
    fn from(d: DeviceId) -> String {
        d.0
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// ハイブリッド論理時計の値。全順序は `(wall, counter, device)` の辞書順で、
/// 端末 ID が最後の決定的タイブレークになる。**同じ書き込みは全端末で同じ
/// 値になり、別の書き込みが同じ値になることはない** (端末ごとに単調増加)。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Hlc {
    /// ミリ秒 (Unix epoch)。論理的には「これ以前に起きた」を含む上界。
    pub wall: u64,
    pub counter: u32,
    pub device: DeviceId,
}

/// [`HlcClock::observe`] の失敗。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClockError {
    /// リモートの `wall` が [`MAX_CLOCK_DRIFT_MS`] を超えて未来。
    DriftTooLarge { remote_wall: u64, now: u64 },
}

impl fmt::Display for ClockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClockError::DriftTooLarge { remote_wall, now } => {
                write!(f, "remote clock too far ahead ({remote_wall} vs {now})")
            }
        }
    }
}

impl std::error::Error for ClockError {}

/// 1 端末ぶんの HLC。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlcClock {
    device: DeviceId,
    last_wall: u64,
    last_counter: u32,
}

impl HlcClock {
    pub fn new(device: DeviceId) -> Self {
        Self {
            device,
            last_wall: 0,
            last_counter: 0,
        }
    }

    /// 永続化しておいた最後の値から復元する (再起動後も単調性を保つ)。
    pub fn restore(device: DeviceId, last: Option<&Hlc>) -> Self {
        let mut c = Self::new(device);
        if let Some(h) = last {
            c.last_wall = h.wall;
            c.last_counter = h.counter;
        }
        c
    }

    pub fn device(&self) -> &DeviceId {
        &self.device
    }

    /// 最後に発行した値 (未発行なら `None`)。永続化して [`Self::restore`] に渡す。
    pub fn last(&self) -> Option<Hlc> {
        if self.last_wall == 0 && self.last_counter == 0 {
            None
        } else {
            Some(Hlc {
                wall: self.last_wall,
                counter: self.last_counter,
                device: self.device.clone(),
            })
        }
    }

    /// ローカルの書き込み用に次の値を発行する。壁時計が巻き戻っても
    /// 単調増加する。
    pub fn tick(&mut self, now_ms: u64) -> Hlc {
        self.advance(now_ms, None)
    }

    /// リモートの値を観測して時計を進め、それより後の値を発行する
    /// (因果順序: 受信した書き込みの後の書き込みは必ず大きくなる)。
    pub fn observe(&mut self, remote: &Hlc, now_ms: u64) -> Result<Hlc, ClockError> {
        if remote.wall > now_ms.saturating_add(MAX_CLOCK_DRIFT_MS) {
            return Err(ClockError::DriftTooLarge {
                remote_wall: remote.wall,
                now: now_ms,
            });
        }
        Ok(self.advance(now_ms, Some(remote)))
    }

    fn advance(&mut self, now_ms: u64, remote: Option<&Hlc>) -> Hlc {
        let mut wall = self.last_wall.max(now_ms);
        if let Some(r) = remote {
            wall = wall.max(r.wall);
        }
        let mut base: Option<u32> = None;
        if self.last_wall == wall {
            base = Some(self.last_counter);
        }
        if let Some(r) = remote {
            if r.wall == wall {
                base = Some(base.map_or(r.counter, |b| b.max(r.counter)));
            }
        }
        let (wall, counter) = match base {
            None => (wall, 0),
            Some(b) => match b.checked_add(1) {
                Some(c) => (wall, c),
                // カウンタが尽きたら wall を 1ms 進める (実質到達しない)。
                None => (wall.saturating_add(1), 0),
            },
        };
        self.last_wall = wall;
        self.last_counter = counter;
        Hlc {
            wall,
            counter,
            device: self.device.clone(),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn dev(s: &str) -> DeviceId {
        DeviceId::new(s).unwrap()
    }

    #[test]
    fn device_id_validation() {
        assert!(DeviceId::new("laptop-1_A").is_ok());
        assert!(DeviceId::new("").is_err());
        assert!(DeviceId::new("a b").is_err());
        assert!(DeviceId::new("日本語").is_err());
        assert!(DeviceId::new(&"x".repeat(MAX_DEVICE_ID_LEN)).is_ok());
        assert!(DeviceId::new(&"x".repeat(MAX_DEVICE_ID_LEN + 1)).is_err());
    }

    #[test]
    fn device_id_serde_rejects_invalid() {
        let ok: DeviceId = serde_json::from_str("\"abc\"").unwrap();
        assert_eq!(ok.as_str(), "abc");
        assert!(serde_json::from_str::<DeviceId>("\"a/b\"").is_err());
    }

    #[test]
    fn tick_is_strictly_monotonic_even_if_wall_clock_goes_back() {
        let mut c = HlcClock::new(dev("a"));
        let h1 = c.tick(1000);
        let h2 = c.tick(1000);
        let h3 = c.tick(500); // 巻き戻り
        assert!(h1 < h2 && h2 < h3);
        assert_eq!(h3.wall, 1000);
        let h4 = c.tick(2000);
        assert_eq!((h4.wall, h4.counter), (2000, 0));
    }

    #[test]
    fn observe_orders_after_remote() {
        let mut a = HlcClock::new(dev("a"));
        let mut b = HlcClock::new(dev("b"));
        let ha = a.tick(5000);
        // b の壁時計は遅れている。
        let hb = b.observe(&ha, 1000).unwrap();
        assert!(hb > ha);
        assert_eq!(hb.wall, 5000);
    }

    #[test]
    fn observe_takes_max_counter_at_same_wall() {
        let mut a = HlcClock::new(dev("a"));
        a.tick(100);
        a.tick(100); // (100, 1)
        let remote = Hlc {
            wall: 100,
            counter: 7,
            device: dev("b"),
        };
        let h = a.observe(&remote, 100).unwrap();
        assert_eq!((h.wall, h.counter), (100, 8));
    }

    #[test]
    fn observe_rejects_far_future() {
        let mut a = HlcClock::new(dev("a"));
        let remote = Hlc {
            wall: 1_000 + MAX_CLOCK_DRIFT_MS + 1,
            counter: 0,
            device: dev("b"),
        };
        assert!(matches!(
            a.observe(&remote, 1_000),
            Err(ClockError::DriftTooLarge { .. })
        ));
        // 拒否しても状態は汚れない。
        assert_eq!(a.last(), None);
    }

    #[test]
    fn device_id_is_final_tiebreak() {
        let x = Hlc {
            wall: 1,
            counter: 0,
            device: dev("a"),
        };
        let y = Hlc {
            wall: 1,
            counter: 0,
            device: dev("b"),
        };
        assert!(x < y);
    }

    #[test]
    fn restore_keeps_monotonicity() {
        let mut c = HlcClock::new(dev("a"));
        let last = c.tick(9000);
        let mut r = HlcClock::restore(dev("a"), Some(&last));
        assert!(r.tick(10) > last);
        assert_eq!(HlcClock::new(dev("a")).last(), None);
    }

    #[test]
    fn counter_overflow_bumps_wall() {
        let mut c = HlcClock::restore(
            dev("a"),
            Some(&Hlc {
                wall: 10,
                counter: u32::MAX,
                device: dev("a"),
            }),
        );
        let h = c.tick(10);
        assert_eq!((h.wall, h.counter), (11, 0));
    }
}
