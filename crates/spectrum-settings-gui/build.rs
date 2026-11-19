//! 编译 Slint 界面。
//!
//! `slint_build::compile` 会把 `slint/gui.slint` 编译成 Rust 代码放进 `OUT_DIR`，
//! `slint::include_modules!()` 在 `src/lib.rs` 里把它 include 进来。
//!
//! 界面里引用的控件（`Button` / `Slider` / `CheckBox` / `LineEdit` / `ScrollView`）
//! 都来自 Slint 自带的 `std-widgets.slint`，因此**不需要**额外的图片或字体资源文件。

fn main() {
    slint_build::compile("slint/gui.slint").expect("编译 Slint 界面失败");
}
