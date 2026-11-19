//! 应用图标：现场生成的 ARGB32 频谱柱。
//!
//! ## 为什么"现场生成"而不是带一张 PNG
//!
//! - 托盘图标要 32×32，桌面图标要 256×256 —— 同一段代码按尺寸生成，
//!   深浅色任务栏上都清晰（渐变两端分别是青与品红）；
//! - 不带二进制资源 → 打包时不用处理图标安装路径，也不会出现"图标找不到"；
//! - 同一份像素还能直接导出成 PNG 供打包用（`deepin-spectrum icon`）。
//!
//! 像素格式：**预乘 alpha 的 ARGB32**（`0xAARRGGBB`）。`icon_argb` 按
//! SNI `IconPixmap` 的网络字节序输出 `A,R,G,B`；`icon_rgba` 再转换成
//! 标准 PNG 需要的 `R,G,B,A`。

// 图标尺寸 ≤1024，柱高换算是 `像素数 × 比例`（比例恒在 0~1），
// 这些窄化/精度换算不会越界。
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]

/// 生成 `size × size` 的图标，返回 `(宽, 高, 预乘 ARGB 字节)`。
///
/// 字节布局为 SNI 网络序：每像素 4 字节，顺序 `A, R, G, B`。
#[must_use]
pub fn icon_argb(size: i32) -> (i32, i32, Vec<u8>) {
    // 变量名写全：图标里同时有 x/y/size/颜色三个维度，单字母很容易串（clippy 也会报）
    let px = usize::try_from(size.max(8)).unwrap_or(32);
    let mut data = vec![0_u8; px * px * 4];
    let bar_count = 5_usize;
    let gap = px / (bar_count * 4);
    let bar_w = (px - gap * (bar_count + 1)) / bar_count;
    // 柱高比例（同一套固定值，保证图标稳定不闪）
    let heights = [0.45_f32, 0.75, 1.0, 0.65, 0.35];

    for (index, ratio) in heights.iter().enumerate() {
        let left = gap + index * (bar_w + gap);
        let bar_h = ((px as f32) * ratio * 0.8) as usize;
        let top = px.saturating_sub(bar_h + gap);
        for py in top..(px - gap).min(px) {
            for px_x in left..(left + bar_w).min(px) {
                // 0 = 柱顶，1 = 柱底；颜色从顶部品红渐变到底部青。
                let step = if bar_h <= 1 {
                    1.0
                } else {
                    (py - top) as f32 / (bar_h - 1) as f32
                };
                // 顶部品红 (255,43,214) → 底部青 (0,229,255)
                let red = ((1.0 - step) * 255.0) as u8;
                let green = ((1.0 - step) * 43.0 + step * 229.0) as u8;
                let blue = ((1.0 - step) * 214.0 + step * 255.0) as u8;
                let offset = (py * px + px_x) * 4;
                data[offset] = 255; // A
                data[offset + 1] = red;
                data[offset + 2] = green;
                data[offset + 3] = blue;
            }
        }
    }
    (
        i32::try_from(px).unwrap_or(32),
        i32::try_from(px).unwrap_or(32),
        data,
    )
}

/// 生成图标并返回原始 RGBA 字节（**非**预乘，便于直接写 PNG）。
#[must_use]
pub fn icon_rgba(size: i32) -> (i32, i32, Vec<u8>) {
    let (w, h, argb) = icon_argb(size);
    let mut rgba = Vec::with_capacity(argb.len());
    // 源字节序是 A,R,G,B；输出标准 R,G,B,A
    for chunk in argb.chunks_exact(4) {
        rgba.extend_from_slice(&[chunk[1], chunk[2], chunk[3], chunk[0]]);
    }
    (w, h, rgba)
}

#[cfg(test)]
mod tests {
    use super::{icon_argb, icon_rgba};

    #[test]
    fn icon_has_expected_size_and_content() {
        let (w, h, data) = icon_argb(32);
        assert_eq!((w, h), (32, 32));
        assert_eq!(data.len(), 32 * 32 * 4);
        // 不能是一张全透明的空图
        assert!(data.chunks_exact(4).any(|p| p[0] > 0), "图标应当有可见像素");

        // 字节序 A,R,G,B：找到最上方与最下方的可见像素，验证渐变方向。
        let mut top: Option<[u8; 4]> = None;
        let mut bottom: Option<[u8; 4]> = None;
        for pixel in data.chunks_exact(4) {
            if pixel[0] == 0 {
                continue;
            }
            let argb = [pixel[0], pixel[1], pixel[2], pixel[3]];
            if top.is_none() {
                top = Some(argb);
            }
            bottom = Some(argb);
        }
        let top = top.expect("应有可见像素");
        let bottom = bottom.expect("应有可见像素");
        // 顶部品红：R 高、G 低、B 高
        assert!(
            top[1] > 180 && top[2] < 100 && top[3] > 180,
            "顶部应为品红，实际 {top:?}"
        );
        // 底部青：R 低、G 高、B 高
        assert!(
            bottom[1] < 100 && bottom[2] > 180 && bottom[3] > 180,
            "底部应为青色，实际 {bottom:?}"
        );
    }

    #[test]
    fn rgba_export_swaps_channels() {
        let (_, _, argb) = icon_argb(16);
        let (_, _, rgba) = icon_rgba(16);
        assert_eq!(rgba.len(), argb.len());
        // 源 A,R,G,B → 输出 R,G,B,A
        assert_eq!(rgba[0], argb[1]);
        assert_eq!(rgba[1], argb[2]);
        assert_eq!(rgba[2], argb[3]);
        assert_eq!(rgba[3], argb[0]);
    }

    #[test]
    fn tiny_sizes_do_not_panic() {
        for size in [0, 1, 4, 8] {
            let (w, h, data) = icon_argb(size);
            assert!(w >= 8 && h >= 8);
            assert_eq!(data.len(), (w * h * 4) as usize);
        }
    }
}
