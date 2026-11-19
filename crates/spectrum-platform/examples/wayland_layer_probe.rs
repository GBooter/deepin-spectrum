//! Treeland / Wayland layer-shell 探针：在任务栏区域画一块 25% 红并保持 10 秒。
//!
//! 用法（Wayland 会话）：
//!     cargo run --release --example `wayland_layer_probe` --features wayland
//!
//! 目的：验证 `zwlr_layer_shell_v1` 建 surface、anchor=bottom、overlay 层、
//! 空输入区、SHM 提交在 Treeland 上确实可见。尺寸默认按当前 3200×2000 @2x
//! 的 dock（1829×52 逻辑 → 3658×104 物理）写死；可用环境变量覆盖。

use std::time::Duration;

use spectrum_platform::overlay::OverlayGeometry;
use spectrum_platform::wayland::WaylandOverlay;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let width: u16 = std::env::var("SPECTRUM_WAYLAND_W")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3658);
    let height: u16 = std::env::var("SPECTRUM_WAYLAND_H")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(104);
    let scale: u32 = std::env::var("SPECTRUM_WAYLAND_SCALE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2);
    let position: i32 = std::env::var("SPECTRUM_WAYLAND_POSITION")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2);

    let mut overlay =
        WaylandOverlay::create(OverlayGeometry::new(0, 0, width, height), scale, position)?;
    println!("layer surface ready: {width}x{height} (scale {scale}, position {position})");

    // 25% 红，预乘 alpha。肉眼看任务栏区域应出现一条半透明红带。
    let pixels = vec![0x40FF_0000_u32; usize::from(width) * usize::from(height)];
    overlay.put_image(&pixels, width, height)?;
    println!("frame sent; holding 10s");
    std::thread::sleep(Duration::from_secs(10));
    Ok(())
}
