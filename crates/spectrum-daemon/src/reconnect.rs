//! 采集断流后的重连策略（纯逻辑、零 I/O、可用毫秒时间戳单测）。
//!
//! ## 为什么要把这段单独抽出来
//!
//! 「换输出设备 / `PulseAudio` 重启 / 休眠唤醒」都会让采集流断掉。
//! 恢复过程是**有状态、带时间**的（退避、次数、累计断流时长），
//! 塞进主循环会既难读又难测；而真去重启用户的音频服务来测一遍代价太高。
//!
//! 抽成纯逻辑后：
//! - 退避序列、放弃条件、恢复后的状态复位，全部可以离线断言；
//! - 主循环只负责「什么时候问一次」和「重建采集链」。
//!
//! ## 策略取值理由
//!
//! 首次退避 **200ms**：设备切换/服务端重启通常在百毫秒级完成，
//! 立刻重试会撞在服务端还没起来的窗口上，白刷日志。
//! 上限 **5s**：再长的话，用户插回耳机要干等，体感很差。
//! 默认**永不放弃**：能跑到这里的进程已经在正常工作了，
//! 断流来自环境变化，环境随时可能变回来（笔记本合盖再开就是典型）。
//! 放弃只适用于「启动时就连不上」，那是另一条路径（fail fast）。

use std::time::Duration;

/// 重连退避参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectPolicy {
    /// 第一次重试前等多久。
    pub first_delay_ms: u64,
    /// 退避上限。
    pub max_delay_ms: u64,
    /// 每次失败后延时增长的倍数。
    pub factor: u32,
    /// 最多尝试多少次；`None` = 永不放弃。
    pub max_attempts: Option<u32>,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            first_delay_ms: 200,
            max_delay_ms: 5_000,
            factor: 2,
            max_attempts: None,
        }
    }
}

/// 一次失败之后的决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDecision {
    /// 建议在这么多毫秒后重试。
    RetryIn(u64),
    /// 到此为止（仅在设了 `max_attempts` 时可能发生）。
    GiveUp,
}

/// 断流恢复的累计统计，供 `doctor` 与证据输出。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReconnectStats {
    /// 累计失败次数（含首次断流）。
    pub attempts: u64,
    /// 累计断流次数（一次断流算一次，重试失败不重复计数）。
    pub outages: u64,
    /// 累计处于断流状态的毫秒数。
    pub total_outage_ms: u64,
    /// 最近一次退避时长。
    pub last_delay_ms: u64,
}

/// 断流重连状态机。
///
/// 时间全部由调用方以毫秒传入，模块内部不读时钟 —— 这样单测可以任意跳跃时间，
/// 不需要真的 sleep。
#[derive(Debug)]
pub struct Reconnector {
    policy: ReconnectPolicy,
    /// 当前这一轮断流里已经失败的次数。
    attempt: u32,
    /// 下次允许重试的时刻。
    next_try_at: Option<u64>,
    /// 本轮断流开始的时刻（`None` = 当前健康）。
    outage_started_at: Option<u64>,
    stats: ReconnectStats,
}

impl Reconnector {
    /// 以给定策略建立状态机。
    #[must_use]
    pub const fn new(policy: ReconnectPolicy) -> Self {
        Self {
            policy,
            attempt: 0,
            next_try_at: None,
            outage_started_at: None,
            stats: ReconnectStats {
                attempts: 0,
                outages: 0,
                total_outage_ms: 0,
                last_delay_ms: 0,
            },
        }
    }

    /// 记录一次失败，返回下一步该怎么做。
    ///
    /// 首次断流与后续重试失败走同一个入口：它们对退避的意义是一样的。
    pub fn on_failure(&mut self, now_ms: u64) -> RetryDecision {
        self.attempt = self.attempt.saturating_add(1);
        self.stats.attempts = self.stats.attempts.saturating_add(1);
        if self.outage_started_at.is_none() {
            self.outage_started_at = Some(now_ms);
            self.stats.outages = self.stats.outages.saturating_add(1);
        }

        if self
            .policy
            .max_attempts
            .is_some_and(|max| self.attempt > max)
        {
            self.next_try_at = None;
            return RetryDecision::GiveUp;
        }

        let delay = self.delay_for(self.attempt);
        self.stats.last_delay_ms = delay;
        self.next_try_at = Some(now_ms.saturating_add(delay));
        RetryDecision::RetryIn(delay)
    }

    /// 恢复成功：清掉本轮状态并累计断流时长。
    pub fn on_recovered(&mut self, now_ms: u64) {
        if let Some(start) = self.outage_started_at.take() {
            self.stats.total_outage_ms = self
                .stats
                .total_outage_ms
                .saturating_add(now_ms.saturating_sub(start));
        }
        self.attempt = 0;
        self.next_try_at = None;
    }

    /// 现在是都到重试时刻了。
    #[must_use]
    pub fn due(&self, now_ms: u64) -> bool {
        self.next_try_at.is_some_and(|at| now_ms >= at)
    }

    /// 下次重试时刻（`None` = 没有排期的重试）。
    #[must_use]
    pub const fn next_try_at(&self) -> Option<u64> {
        self.next_try_at
    }

    /// 是否处于断流中（还没恢复）。
    #[must_use]
    pub const fn in_outage(&self) -> bool {
        self.outage_started_at.is_some()
    }

    /// 本轮已经失败了几次。
    #[must_use]
    pub const fn attempt(&self) -> u32 {
        self.attempt
    }

    /// 累计统计。
    #[must_use]
    pub const fn stats(&self) -> ReconnectStats {
        self.stats
    }

    /// 第 `attempt` 次失败后应等多久。
    ///
    /// 用整数幂而不是 `f64`：浮点会让退避序列出现 199.999… 这类值，
    /// 断言只能写容差，而这里本该是精确可比的东西。
    fn delay_for(&self, attempt: u32) -> u64 {
        let mut delay = self.policy.first_delay_ms;
        // attempt 从 1 开始；1 次失败就用 first_delay，故指数是 attempt-1
        for _ in 1..attempt {
            delay = delay.saturating_mul(u64::from(self.policy.factor.max(1)));
            if delay >= self.policy.max_delay_ms {
                break;
            }
        }
        delay.min(self.policy.max_delay_ms)
    }
}

/// 把退避时长转成 `Duration`（`max_delay_ms` 已限制在合理范围）。
#[must_use]
pub fn as_duration(ms: u64) -> Duration {
    Duration::from_millis(ms)
}

#[cfg(test)]
mod tests {
    use super::{ReconnectPolicy, ReconnectStats, Reconnector, RetryDecision};

    fn policy() -> ReconnectPolicy {
        ReconnectPolicy::default()
    }

    #[test]
    fn backoff_grows_then_caps() {
        let mut rc = Reconnector::new(policy());
        let mut delays = Vec::new();
        for _ in 0..7 {
            if let RetryDecision::RetryIn(d) = rc.on_failure(0) {
                delays.push(d);
            }
        }
        // 200 → 400 → 800 → 1600 → 3200 → 5000（封顶）→ 5000
        assert_eq!(delays, vec![200, 400, 800, 1_600, 3_200, 5_000, 5_000]);
    }

    #[test]
    fn retry_is_not_due_before_deadline() {
        let mut rc = Reconnector::new(policy());
        assert_eq!(rc.on_failure(1_000), RetryDecision::RetryIn(200));
        assert_eq!(rc.next_try_at(), Some(1_200));
        assert!(!rc.due(1_199), "未到点不应重试");
        assert!(rc.due(1_200), "到点应重试");
        assert!(rc.due(5_000), "过点更应重试");
    }

    #[test]
    fn gives_up_only_when_max_attempts_is_set() {
        let mut rc = Reconnector::new(ReconnectPolicy {
            max_attempts: Some(2),
            ..policy()
        });
        assert_eq!(rc.on_failure(0), RetryDecision::RetryIn(200));
        assert_eq!(rc.on_failure(0), RetryDecision::RetryIn(400));
        // 第 3 次失败超过上限
        assert_eq!(rc.on_failure(0), RetryDecision::GiveUp);
        assert_eq!(rc.next_try_at(), None);
    }

    #[test]
    fn default_never_gives_up() {
        // 笔记本合盖几十分钟再开、或用户临时拔掉声卡，都可能需要很久才回来。
        let mut rc = Reconnector::new(policy());
        for _ in 0..500 {
            assert!(matches!(rc.on_failure(0), RetryDecision::RetryIn(_)));
        }
        assert_eq!(rc.attempt(), 500);
    }

    #[test]
    fn recovery_resets_attempts_and_accumulates_outage() {
        let mut rc = Reconnector::new(policy());
        rc.on_failure(1_000); // 断流开始
        rc.on_failure(1_200);
        rc.on_recovered(3_000);
        assert_eq!(rc.attempt(), 0, "恢复后重试计数必须归零");
        assert!(!rc.in_outage());
        assert_eq!(rc.next_try_at(), None);
        assert_eq!(rc.stats().outages, 1, "一次断流只计一次");
        assert_eq!(rc.stats().total_outage_ms, 2_000);
        assert_eq!(rc.stats().attempts, 2);

        // 第二次断流：次数累加、时长累加，退避从 first_delay 重新开始
        assert_eq!(rc.on_failure(10_000), RetryDecision::RetryIn(200));
        rc.on_recovered(10_500);
        assert_eq!(rc.stats().outages, 2);
        assert_eq!(rc.stats().total_outage_ms, 2_500);
    }

    #[test]
    fn retries_within_one_outage_count_as_one_outage() {
        // 重试失败不是"又一次断流"，否则 outages 会等于 attempts，失去意义
        let mut rc = Reconnector::new(policy());
        rc.on_failure(0);
        rc.on_failure(200);
        rc.on_failure(600);
        assert_eq!(rc.stats().outages, 1);
        assert_eq!(rc.stats().attempts, 3);
        assert_eq!(rc.stats().total_outage_ms, 0, "未恢复时不应结算时长");
    }

    #[test]
    fn stats_default_is_zero() {
        let rc = Reconnector::new(policy());
        assert_eq!(rc.stats(), ReconnectStats::default());
        assert!(!rc.in_outage());
        assert!(!rc.due(0));
    }

    #[test]
    fn huge_attempt_count_does_not_overflow() {
        // delay 用 saturating_mul，attempt 用 saturating_add：
        // 即便被某个 bug 灌进 u32::MAX 次失败，也不能 panic
        let mut rc = Reconnector::new(policy());
        for _ in 0..64 {
            rc.on_failure(u64::MAX - 10);
        }
        assert_eq!(rc.stats().last_delay_ms, 5_000);
        // 时刻已溢出边界时，next_try_at 饱和到 u64::MAX，不会 panic
        assert_eq!(rc.next_try_at(), Some(u64::MAX));
    }
}
