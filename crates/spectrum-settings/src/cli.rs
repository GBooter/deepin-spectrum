//! `deepin-spectrum settings` 的实现。
//!
//! 同一份代码被两个入口复用：`deepin-spectrum settings`（F84 的统一子命令）
//! 与 `deepin-spectrum-settings`（独立二进制，便于 .desktop 直接拉起）。
//!
//! ## 这一版没有 GUI，为什么也值得先做
//!
//! 参数模型 + 热生效是**与 UI 形态无关**的那一半：
//! 将来不管用 Slint 还是 GTK4，都是"把控件绑到这些键 + 原子写文件"。
//! 反过来先做 UI 再定模型，就会出现"控件有了、参数没生效"的返工。
//! GUI 待 §11.1 的许可决策（Slint `GPL-3.0-only` / GTK4 / 商业许可）落定后再做。

use std::path::PathBuf;

use crate::presets::{self, UserPresetStore};
use crate::schema::{KEYS, TEMPLATE, encode, encode_with_extras};
use crate::store::{self, ConfigStore};

/// 动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// 打印当前生效配置（TOML）。
    Print,
    /// 打印配置文件路径。
    Path,
    /// 写出带注释的默认模板。
    WriteDefault {
        /// 已存在时是否覆盖。
        force: bool,
    },
    /// 校验配置文件。
    Check,
    /// 列出每个键的生效范围。
    Explain,
    /// 列出内置预置与我的方案。
    ListPresets,
    /// 应用一套预置（内置 id 或 `user-` 开头的自定义方案）。
    ApplyPreset,
    /// 把当前配置命名保存为我的方案。
    SavePreset,
    /// 删除一套我的方案。
    DeletePreset,
    /// 帮助。
    Help,
}

/// 解析后的参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    /// 配置路径。
    pub config: PathBuf,
    /// 动作。
    pub action: Action,
    /// 动作参数（预置 id 或方案名）。
    pub value: Option<String>,
}

const HELP: &str = "\
deepin-spectrum settings —— 参数模型与热重载

用法：
  deepin-spectrum settings [--config <文件>] [动作]

动作（缺省为 --print）：
  --print            打印当前生效的配置（TOML）
  --path             打印配置文件路径
  --write-default    写出带注释的默认模板（已存在则拒绝，需 --force）
  --check            校验配置文件：语法/取值/未知键
  --explain          列出每个键的生效范围（热生效 / 仅启动 / 未实现）
  --list-presets     列出 20 套内置预置与我的方案
  --apply-preset <id> 应用预置（内置 id 或 user-xxx），写入后立即热生效
  --save-preset <名称> 把当前配置命名保存为我的方案（同名会更新）
  --delete-preset <id> 删除一套我的方案
  -h, --help         显示本帮助

说明：
  配置文件改动保存后 250ms 内自动生效，不需要重启 daemon。
  生效范围不确定时看 --explain；标「未实现」的键会被保存但不生效。
";

/// 解析命令行。
///
/// # Errors
///
/// 未知参数、缺少参数值。
pub fn parse(args: &[String]) -> Result<Args, String> {
    let mut config = store::default_path();
    let mut action: Option<Action> = None;
    let mut force = false;
    let mut value: Option<String> = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--config" => {
                let value = args.get(index + 1).ok_or("--config 需要文件路径")?;
                config = PathBuf::from(value);
                index += 2;
            }
            "--print" => {
                set_once(&mut action, Action::Print)?;
                index += 1;
            }
            "--path" => {
                set_once(&mut action, Action::Path)?;
                index += 1;
            }
            "--write-default" => {
                set_once(&mut action, Action::WriteDefault { force: false })?;
                index += 1;
            }
            "--check" => {
                set_once(&mut action, Action::Check)?;
                index += 1;
            }
            "--explain" => {
                set_once(&mut action, Action::Explain)?;
                index += 1;
            }
            "--list-presets" => {
                set_once(&mut action, Action::ListPresets)?;
                index += 1;
            }
            "--apply-preset" | "--save-preset" | "--delete-preset" => {
                let flag = args[index].clone();
                let arg = args
                    .get(index + 1)
                    .ok_or_else(|| format!("{flag} 需要一个值"))?
                    .clone();
                let next = match flag.as_str() {
                    "--apply-preset" => Action::ApplyPreset,
                    "--save-preset" => Action::SavePreset,
                    _ => Action::DeletePreset,
                };
                set_once(&mut action, next)?;
                value = Some(arg);
                index += 2;
            }
            "--force" => {
                force = true;
                index += 1;
            }
            "-h" | "--help" => {
                set_once(&mut action, Action::Help)?;
                index += 1;
            }
            other => return Err(format!("未知参数 {other}")),
        }
    }
    let action = match (action, force) {
        (Some(Action::WriteDefault { .. }), true) => Action::WriteDefault { force: true },
        (Some(a), _) => a,
        (None, false) => Action::Print,
        (None, true) => return Err("--force 只能与 --write-default 一起用".to_owned()),
    };
    Ok(Args {
        config,
        action,
        value,
    })
}

fn set_once(slot: &mut Option<Action>, action: Action) -> Result<(), String> {
    if slot.is_some() {
        return Err("一次只能指定一个动作".to_owned());
    }
    *slot = Some(action);
    Ok(())
}

/// 执行。返回 `Err` 表示应当以非零码退出。
///
/// # Errors
///
/// 参数非法、配置校验失败、写文件失败。
pub fn run(args: &[String]) -> Result<(), String> {
    let Args {
        config,
        action,
        value,
    } = parse(args)?;
    match action {
        Action::Help => {
            print!("{HELP}");
            Ok(())
        }
        Action::Path => {
            println!("{}", config.display());
            Ok(())
        }
        Action::WriteDefault { force } => {
            if config.exists() && !force {
                return Err(format!(
                    "{} 已存在；要覆盖请加 --force（现有配置不会被自动丢弃）",
                    config.display()
                ));
            }
            store::write_atomic_text(&config, TEMPLATE)
                .map_err(|e| format!("写入 {} 失败：{e}", config.display()))?;
            println!("已写出默认配置：{}", config.display());
            println!("改完保存即热生效（最迟 250ms）。");
            Ok(())
        }
        Action::Print => {
            let s = ConfigStore::open(&config);
            report_header(&s);
            print!("{}", encode(s.config()));
            Ok(())
        }
        Action::Check => check(&config),
        Action::Explain => {
            explain();
            Ok(())
        }
        Action::ListPresets => {
            list_presets();
            Ok(())
        }
        Action::ApplyPreset => {
            let id = value.as_deref().ok_or("--apply-preset 缺少 id")?;
            apply_preset_cmd(&config, id)
        }
        Action::SavePreset => {
            let name = value.as_deref().ok_or("--save-preset 缺少名称")?;
            let store = ConfigStore::open(&config);
            let saved = UserPresetStore::open().save(name, store.config())?;
            println!("已保存「{}」({})", saved.name, saved.id);
            Ok(())
        }
        Action::DeletePreset => {
            let id = value.as_deref().ok_or("--delete-preset 缺少 id")?;
            UserPresetStore::open().delete(id)?;
            println!("已删除方案 {id}");
            Ok(())
        }
    }
}

/// 内置预置 + 我的方案清单。
/// 情绪分类的中文名（与 `presets.json` 的 families 一致）。
fn family_name(id: &str) -> &'static str {
    match id {
        "cheer" => "欢快明快",
        "calm" => "平静专注",
        "warm" => "温暖怀旧",
        "melancholy" => "忧伤低回",
        "serious" => "严肃仪表",
        "rad" => "炫酷实验",
        _ => "其他",
    }
}

fn list_presets() {
    println!("内置预置（{} 套 / 6 个情绪分类）：", presets::PRESETS.len());
    let mut last = "";
    for (i, p) in presets::PRESETS.iter().enumerate() {
        if p.family != last {
            println!("\n  【{}】", family_name(p.family));
            last = p.family;
        }
        let multi = if p.palette.is_empty() {
            ""
        } else {
            "  ★多色"
        };
        println!(
            "  {:02}  {}  {}  {} → {}  {} 柱{}\n      {}",
            i + 1,
            p.id,
            p.name,
            p.color_start,
            p.color_end,
            p.bands,
            multi,
            p.tagline
        );
    }
    let mine = UserPresetStore::open().list();
    if mine.is_empty() {
        println!("\n我的方案：暂无（用 --save-preset <名称> 保存当前配置）");
    } else {
        println!("\n我的方案（{} 套）：", mine.len());
        for u in mine {
            println!("  {}  {}", u.id, u.name);
        }
    }
}

/// 应用预置并原子写回配置文件；保留文件里本版本不认识的键。
fn apply_preset_cmd(config: &std::path::Path, id: &str) -> Result<(), String> {
    let store = ConfigStore::open(config);
    let mut cfg = store.config().clone();
    if id.starts_with("user-") {
        let mine = UserPresetStore::open();
        let preset = mine
            .get(id)
            .ok_or_else(|| format!("我的方案 {id} 不存在"))?;
        preset.apply_to(&mut cfg);
    } else {
        presets::apply_preset(&mut cfg, id)?;
    }
    let text = encode_with_extras(&cfg, store.extras());
    store::write_atomic_text(config, &text)
        .map_err(|e| format!("写入 {} 失败：{e}", config.display()))?;
    println!("已应用「{id}」· 保存成功，最迟 250ms 后画面变化（无需重启）");
    Ok(())
}

fn report_header(store: &ConfigStore) {
    eprintln!("# 路径：{}", store.path().display());
    eprintln!("# 来源：{}", store.source());
    if let Some(err) = store.last_error() {
        eprintln!("# 当前文件有错，正在用上一次的好配置：{err}");
    }
    for w in store.warnings() {
        eprintln!("# 告警：{w}");
    }
}

fn check(config: &std::path::Path) -> Result<(), String> {
    match store::load(config) {
        Ok(load) => {
            println!(
                "语法：OK（{}）",
                if load.existed {
                    "文件存在"
                } else {
                    "文件不存在，等价全默认"
                }
            );
            if load.warnings.is_empty() {
                println!("告警：无");
            } else {
                println!("告警：{} 条", load.warnings.len());
                for w in &load.warnings {
                    println!("  - {w}");
                }
            }
            // 未知键/未实现键不影响启动，只提示
            println!(
                "生效范围：{} 个键热生效，{} 个仅启动，{} 个未实现",
                count_scope(crate::schema::Scope::Live)
                    + count_scope(crate::schema::Scope::LiveRebuild),
                count_scope(crate::schema::Scope::Startup),
                count_scope(crate::schema::Scope::Unimplemented)
            );
            Ok(())
        }
        Err(e) => Err(format!("校验失败：{e}")),
    }
}

fn count_scope(scope: crate::schema::Scope) -> usize {
    KEYS.iter().filter(|k| k.scope == scope).count()
}

/// 显示宽度：中文按 2 列算，ASCII 按 1 列。
///
/// 用 `{:width$}` 补齐会因为中文算 1 个字符而错位，表格会歪。
fn display_width(s: &str) -> usize {
    s.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum()
}

/// 按显示宽度左对齐补齐。
fn pad(s: &str, width: usize) -> String {
    let used = display_width(s);
    format!("{s}{}", " ".repeat(width.saturating_sub(used)))
}

fn explain() {
    println!("生效范围说明：");
    println!("  热生效        改完保存，最迟 250ms 后画面变化（不重启、不闪窗）");
    println!("  热生效（重建） 同上，但会重建分析器（一帧内完成，不停采集）");
    println!("  仅启动        只在 daemon 启动时读一次，改完需重启 daemon");
    println!("  未实现        值会被保存，但本版本没有对应功能，不会生效");
    println!();
    println!("{}  {}  说明", pad("键", 32), pad("生效范围", 16));
    println!("{}", "-".repeat(96));
    let mut section = "";
    for spec in KEYS {
        if spec.section != section {
            section = spec.section;
            println!("[{section}]");
        }
        println!(
            "  {}  {}  {}",
            pad(spec.key, 32),
            pad(&spec.scope.to_string(), 16),
            spec.note
        );
    }
    println!();
    let live =
        count_scope(crate::schema::Scope::Live) + count_scope(crate::schema::Scope::LiveRebuild);
    println!(
        "合计 {} 个键：{} 热生效 / {} 仅启动 / {} 未实现",
        KEYS.len(),
        live,
        count_scope(crate::schema::Scope::Startup),
        count_scope(crate::schema::Scope::Unimplemented)
    );
}

#[cfg(test)]
mod tests {
    use super::{Action, parse};
    use std::path::PathBuf;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn default_action_is_print() {
        let a = parse(&args(&[])).expect("解析失败");
        assert_eq!(a.action, Action::Print);
        assert!(a.config.to_string_lossy().ends_with("config.toml"));
    }

    #[test]
    fn config_path_is_honored() {
        let a = parse(&args(&["--config", "/tmp/x.toml", "--check"])).expect("解析失败");
        assert_eq!(a.config, PathBuf::from("/tmp/x.toml"));
        assert_eq!(a.action, Action::Check);
    }

    #[test]
    fn force_upgrades_write_default() {
        let a = parse(&args(&["--write-default", "--force"])).expect("解析失败");
        assert_eq!(a.action, Action::WriteDefault { force: true });
    }

    #[test]
    fn force_alone_is_rejected() {
        assert!(parse(&args(&["--force"])).is_err());
    }

    #[test]
    fn two_actions_are_rejected() {
        assert!(parse(&args(&["--print", "--check"])).is_err());
    }

    #[test]
    fn preset_actions_carry_their_value() {
        let a = parse(&args(&["--apply-preset", "neon-run"])).expect("解析失败");
        assert_eq!(a.action, Action::ApplyPreset);
        assert_eq!(a.value.as_deref(), Some("neon-run"));

        let b = parse(&args(&["--save-preset", "深夜写代码"])).expect("解析失败");
        assert_eq!(b.action, Action::SavePreset);
        assert_eq!(b.value.as_deref(), Some("深夜写代码"));

        let c = parse(&args(&["--list-presets"])).expect("解析失败");
        assert_eq!(c.action, Action::ListPresets);
        assert_eq!(c.value, None);
    }

    #[test]
    fn preset_action_without_value_is_rejected() {
        assert!(parse(&args(&["--apply-preset"])).is_err());
        assert!(parse(&args(&["--save-preset"])).is_err());
        assert!(parse(&args(&["--delete-preset"])).is_err());
    }

    #[test]
    fn unknown_flag_is_rejected() {
        assert!(parse(&args(&["--nope"])).is_err());
        assert!(parse(&args(&["--config"])).is_err());
    }

    #[test]
    fn pad_counts_cjk_as_two_columns() {
        assert_eq!(super::display_width("abc"), 3);
        assert_eq!(super::display_width("热生效"), 6);
        assert_eq!(super::pad("abc", 5), "abc  ");
        assert_eq!(super::pad("热生效", 6), "热生效");
        // "热生效" 占 6 列，补到 8 列 = 2 个空格（字符数是 5，不是 8）
        let padded = super::pad("热生效", 8);
        assert_eq!(padded.chars().count(), 5);
        assert_eq!(super::display_width(&padded), 8);
    }
}
