//! 显示状态机（§6.7）：`Idle` / `Running` / `FadeOut`。
//!
//! 「必须淡出」是硬约束：直接销毁表面会闪烁。系统开启减弱动画时
//! `fade_ms` 强制为 0（F39），此时 `FadeOut` 会被跳过而直接进入 `Idle`。
//!
//! **预留实现**：daemon 当前的隐藏/恢复由 `main.rs` 的 `Session` 直接表达
//! （全屏、锁屏、自动隐藏是三个独立边沿），没有接入本状态机；这里的
//! `StateMachine` / `Visibility` 仍由 `core_selfcheck` 覆盖，作为后续统一
//! 淡出动画的参考模型。不要把本模块的单测当成"生产路径已验证"。

/// 显示状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayState {
    /// 无表面，0 fps。
    Idle,
    /// 正常渲染。
    Running,
    /// 淡出中（仍需要渲染帧以完成动画）。
    FadeOut {
        /// 淡出开始时间戳（毫秒）。
        started_ms: u64,
        /// 淡出总时长（毫秒）。
        duration_ms: u64,
    },
}

/// 一帧的可见性输入；由 main 线程汇总 D-Bus / 全屏 / 锁屏 / 静音检测后传入。
// 五个条件来自五个独立子系统，用位标志或枚举会掩盖各自的来源与调试信息。
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Visibility {
    /// 音频活跃（静音门限未超时）。
    pub has_audio: bool,
    /// 任务栏可见（未自动隐藏）。
    pub dock_visible: bool,
    /// 有全屏应用。
    pub fullscreen: bool,
    /// 锁屏或屏保中。
    pub locked: bool,
    /// 用户总开关。
    pub enabled: bool,
}

impl Visibility {
    /// 是否满足进入 `Running` 的全部条件。
    #[must_use]
    pub const fn should_render(self) -> bool {
        self.has_audio && self.dock_visible && !self.fullscreen && !self.locked && self.enabled
    }

    /// 全部条件为真的构造快捷方式。
    #[must_use]
    pub const fn all_visible() -> Self {
        Self {
            has_audio: true,
            dock_visible: true,
            fullscreen: false,
            locked: false,
            enabled: true,
        }
    }
}

impl Default for Visibility {
    fn default() -> Self {
        Self {
            has_audio: false,
            dock_visible: true,
            fullscreen: false,
            locked: false,
            enabled: true,
        }
    }
}

/// 状态机实例。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateMachine {
    state: DisplayState,
    fade_ms: u32,
    reduced_motion: bool,
}

impl StateMachine {
    /// `fade_ms` 为配置的淡出时长；`reduced_motion` 通常由 `DConfig` 在启动时读一次。
    #[must_use]
    pub fn new(fade_ms: u32, reduced_motion: bool) -> Self {
        Self {
            state: DisplayState::Idle,
            fade_ms,
            reduced_motion,
        }
    }

    /// 实际生效的淡出时长；减弱动画时为 0。
    #[must_use]
    pub const fn effective_fade_ms(&self) -> u32 {
        if self.reduced_motion { 0 } else { self.fade_ms }
    }

    /// 推进一帧，返回推进后的状态。
    pub fn update(&mut self, v: Visibility, now_ms: u64) -> DisplayState {
        let fade = u64::from(self.effective_fade_ms());
        match self.state {
            DisplayState::Idle => {
                if v.should_render() {
                    self.state = DisplayState::Running;
                }
            }
            DisplayState::Running => {
                if !v.should_render() {
                    self.state = if fade == 0 {
                        DisplayState::Idle
                    } else {
                        DisplayState::FadeOut {
                            started_ms: now_ms,
                            duration_ms: fade,
                        }
                    };
                }
            }
            DisplayState::FadeOut {
                started_ms,
                duration_ms,
            } => {
                if v.should_render() {
                    // 淡出途中条件恢复 → 直接淡入，不重新走一遍创建流程
                    self.state = DisplayState::Running;
                } else if now_ms.saturating_sub(started_ms) >= duration_ms {
                    self.state = DisplayState::Idle;
                }
            }
        }
        self.state
    }

    /// 当前状态。
    #[must_use]
    pub const fn state(&self) -> DisplayState {
        self.state
    }

    /// 当前帧应使用的目标帧率；`Idle` 为 0（省电核心）。
    #[must_use]
    pub fn target_fps(&self, base_fps: u32) -> u32 {
        match self.state {
            DisplayState::Idle => 0,
            DisplayState::Running | DisplayState::FadeOut { .. } => base_fps,
        }
    }

    /// 淡出进度 `[0,1]`；`1` 表示应销毁表面。
    #[must_use]
    pub fn fade_progress(&self, now_ms: u64) -> f32 {
        match self.state {
            DisplayState::FadeOut {
                started_ms,
                duration_ms,
            } => {
                if duration_ms == 0 {
                    1.0
                } else {
                    (now_ms.saturating_sub(started_ms) as f32 / duration_ms as f32).clamp(0.0, 1.0)
                }
            }
            DisplayState::Running => 0.0,
            DisplayState::Idle => 1.0,
        }
    }
}

impl Default for StateMachine {
    fn default() -> Self {
        Self::new(150, false)
    }
}

#[cfg(test)]
mod tests {
    use super::{DisplayState, StateMachine, Visibility};

    #[test]
    fn full_transition_coverage() {
        let mut sm = StateMachine::new(150, false);
        assert_eq!(sm.state(), DisplayState::Idle);
        assert_eq!(sm.target_fps(90), 0, "Idle 必须 0 fps");

        assert_eq!(
            sm.update(Visibility::all_visible(), 0),
            DisplayState::Running
        );
        assert_eq!(sm.target_fps(90), 90);

        let fading = sm.update(
            Visibility {
                locked: true,
                ..Visibility::all_visible()
            },
            1_000,
        );
        assert_eq!(
            fading,
            DisplayState::FadeOut {
                started_ms: 1_000,
                duration_ms: 150
            }
        );
        assert!(sm.fade_progress(1_075) > 0.49);
        assert_eq!(sm.target_fps(90), 90, "淡出期间仍需渲染帧");

        // 淡出途中恢复
        assert_eq!(
            sm.update(Visibility::all_visible(), 1_080),
            DisplayState::Running
        );
        assert_eq!(sm.fade_progress(1_080), 0.0);

        // 再次淡出直到完成
        sm.update(
            Visibility {
                fullscreen: true,
                ..Visibility::all_visible()
            },
            2_000,
        );
        assert_eq!(
            sm.update(Visibility::default(), 2_100),
            DisplayState::FadeOut {
                started_ms: 2_000,
                duration_ms: 150
            }
        );
        assert_eq!(sm.update(Visibility::default(), 2_150), DisplayState::Idle);
        assert_eq!(sm.target_fps(90), 0);
    }

    #[test]
    fn reduced_motion_skips_fade() {
        let mut sm = StateMachine::new(150, true);
        assert_eq!(sm.effective_fade_ms(), 0);
        sm.update(Visibility::all_visible(), 0);
        assert_eq!(sm.update(Visibility::default(), 10), DisplayState::Idle);
        assert_eq!(sm.fade_progress(10), 1.0);
    }

    /// 五种退出条件逐一验证（静音 / 自动隐藏 / 全屏 / 锁屏 / 用户关闭）。
    #[test]
    fn every_exit_condition_leaves_running() {
        let cases = [
            Visibility {
                has_audio: false,
                ..Visibility::all_visible()
            },
            Visibility {
                dock_visible: false,
                ..Visibility::all_visible()
            },
            Visibility {
                fullscreen: true,
                ..Visibility::all_visible()
            },
            Visibility {
                locked: true,
                ..Visibility::all_visible()
            },
            Visibility {
                enabled: false,
                ..Visibility::all_visible()
            },
        ];
        for v in cases {
            let mut sm = StateMachine::default();
            sm.update(Visibility::all_visible(), 0);
            let s = sm.update(v, 100);
            assert!(
                matches!(s, DisplayState::FadeOut { .. }),
                "条件 {v:?} 应触发淡出"
            );
        }
    }

    #[test]
    fn fade_progress_is_clamped() {
        let mut sm = StateMachine::default();
        sm.update(Visibility::all_visible(), 0);
        sm.update(Visibility::default(), 500);
        assert_eq!(sm.fade_progress(400), 0.0, "时间倒退不应产生负进度");
        assert_eq!(sm.fade_progress(100_000), 1.0);
    }
}
