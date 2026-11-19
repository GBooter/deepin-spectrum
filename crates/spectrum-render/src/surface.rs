//! ARGB 像素缓冲。
//!
//! 像素格式为**预乘 alpha 的 ARGB**（`0xAARRGGBB`），与 X11 depth-32 窗口
//! 的 `ZPixmap` 在小端机器上的字节序（`B,G,R,A`）一一对应：
//! `u32::from_le_bytes([b, g, r, a])` 得到的就是本类型的值。
//! 因此上屏（`put_image`）与回读（`get_image`）都不需要额外转换，
//! 像素比对可以直接逐 u32 相等判断 —— 这让"渲染是否正确"可机器断言。

/// 一块行主序的像素缓冲。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Surface {
    width: u16,
    height: u16,
    pixels: Vec<u32>,
}

impl Surface {
    /// 新建全透明缓冲。
    ///
    /// `width`/`height` 为 0 时得到空缓冲，后续 [`Surface::set`] 一律无效，
    /// 调用方不需要额外判空。
    #[must_use]
    pub fn new(width: u16, height: u16) -> Self {
        let len = usize::from(width) * usize::from(height);
        Self {
            width,
            height,
            pixels: vec![0; len],
        }
    }

    /// 宽度（像素）。
    #[must_use]
    pub const fn width(&self) -> u16 {
        self.width
    }

    /// 高度（像素）。
    #[must_use]
    pub const fn height(&self) -> u16 {
        self.height
    }

    /// 只读像素视图（行主序）。
    #[must_use]
    pub fn pixels(&self) -> &[u32] {
        &self.pixels
    }

    /// 是否需要重新分配（尺寸变化时上屏端要跟着重建）。
    #[must_use]
    pub fn needs_resize(&self, width: u16, height: u16) -> bool {
        self.width != width || self.height != height
    }

    /// 调整尺寸；尺寸变化时内容全部清空。
    ///
    /// 内容不复用是有意的：任务栏高度改变后旧内容本来就要整帧重画，
    /// 保留旧像素只会在新尺寸里留下错位的残影。
    pub fn resize(&mut self, width: u16, height: u16) {
        if self.needs_resize(width, height) {
            self.width = width;
            self.height = height;
            self.pixels = vec![0; usize::from(width) * usize::from(height)];
        }
    }

    /// 全部置为透明（预乘 alpha 下 `0` 就是"完全透明黑"）。
    pub fn clear(&mut self) {
        self.pixels.fill(0);
    }

    /// 读一个像素；越界返回 `None`（而不是 panic）。
    ///
    /// 坐标用 `u32`：渲染器的布局计算是 `u32` 整数运算，让它直接传进来
    /// 可以避免在热循环里做窄化转换；边界检查仍按 `u16` 的真实尺寸做。
    #[must_use]
    pub fn get(&self, x: u32, y: u32) -> Option<u32> {
        let index = self.index(x, y)?;
        self.pixels.get(index).copied()
    }

    /// 写一个像素；越界静默忽略。
    ///
    /// 越界忽略而不是 panic，是因为渲染期间任务栏尺寸可能刚被缩小
    /// （信号与渲染不同步），此时丢掉几个越界像素比崩掉整个进程合理。
    pub fn set(&mut self, x: u32, y: u32, argb: u32) {
        if let Some(index) = self.index(x, y)
            && let Some(slot) = self.pixels.get_mut(index)
        {
            *slot = argb;
        }
    }

    /// 用同一颜色填充一行中的 `[x0, x1)`；越界部分自动裁剪。
    ///
    /// 返回实际写入的像素数。渲染热路径按行填色，比逐像素 [`Surface::set`]
    /// 少一次边界检查与分支。
    pub fn fill_row(&mut self, y: u32, x0: u32, x1: u32, argb: u32) -> u64 {
        let width = u32::from(self.width);
        if y >= u32::from(self.height) || width == 0 {
            return 0;
        }
        let start = x0.min(width);
        let end = x1.min(width);
        if end <= start {
            return 0;
        }
        let stride = usize::from(self.width);
        let row_start = y as usize * stride + start as usize;
        let row_end = y as usize * stride + end as usize;
        if let Some(row) = self.pixels.get_mut(row_start..row_end) {
            row.fill(argb);
            u64::from(end - start)
        } else {
            0
        }
    }

    /// 行主序下标；越界返回 `None`。
    fn index(&self, x: u32, y: u32) -> Option<usize> {
        if x >= u32::from(self.width) || y >= u32::from(self.height) {
            return None;
        }
        let stride = usize::from(self.width);
        Some(y as usize * stride + x as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::Surface;

    #[test]
    fn new_is_fully_transparent() {
        let surface = Surface::new(4, 3);
        assert_eq!(surface.pixels().len(), 12);
        assert!(surface.pixels().iter().all(|&p| p == 0));
    }

    #[test]
    fn set_and_get_roundtrip() {
        let mut surface = Surface::new(3, 2);
        surface.set(2, 1, 0x1122_3344);
        assert_eq!(surface.get(2, 1), Some(0x1122_3344));
        assert_eq!(surface.get(1, 1), Some(0));
    }

    #[test]
    fn out_of_bounds_is_ignored() {
        let mut surface = Surface::new(3, 2);
        surface.set(3, 0, 0xFFFF_FFFF);
        surface.set(0, 2, 0xFFFF_FFFF);
        assert_eq!(surface.get(3, 0), None);
        assert!(surface.pixels().iter().all(|&p| p == 0));
    }

    #[test]
    fn zero_size_surface_is_inert() {
        let mut surface = Surface::new(0, 0);
        surface.set(0, 0, 0xFFFF_FFFF);
        assert!(surface.pixels().is_empty());
        assert_eq!(surface.get(0, 0), None);
    }

    #[test]
    fn resize_clears_previous_content() {
        let mut surface = Surface::new(2, 2);
        surface.set(0, 0, 0xFFFF_FFFF);
        surface.resize(4, 4);
        assert_eq!(surface.pixels().len(), 16);
        assert!(surface.pixels().iter().all(|&p| p == 0));
    }

    #[test]
    fn fill_row_clamps_and_writes_only_requested_range() {
        let mut surface = Surface::new(4, 2);
        assert_eq!(surface.fill_row(0, 1, 3, 0x1122_3344), 2);
        assert_eq!(
            surface.pixels(),
            &[0, 0x1122_3344, 0x1122_3344, 0, 0, 0, 0, 0]
        );
        // 越界自动裁剪
        assert_eq!(surface.fill_row(1, 2, 99, 0xAABB_CCDD), 2);
        assert_eq!(surface.get(3, 1), Some(0xAABB_CCDD));
        // 空区间 / 越界行不写入
        assert_eq!(surface.fill_row(1, 3, 3, 0xFFFF_FFFF), 0);
        assert_eq!(surface.fill_row(9, 0, 4, 0xFFFF_FFFF), 0);
    }
}
