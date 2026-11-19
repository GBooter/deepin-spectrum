//! 系统托盘（`StatusNotifierItem` + `DBusMenu`）。
//!
//! ## 为什么不用现成 GUI 工具包
//!
//! 只需要**一个图标 + 一个菜单**，引入 GTK/Qt 绑定会带来大量系统依赖，
//! 而 D-Bus 读写我们已经在用了（`zbus`）。
//! 本模块直接实现两份规范：
//!
//! - `org.kde.StatusNotifierItem`：图标与状态（宿主是 `dde-shell` 里的
//!   `org.kde.StatusNotifierWatcher`）
//! - `com.canonical.dbusmenu`：右键菜单
//!
//! 图标不走文件：用 `IconPixmap` 现场生成（`icon_argb`）。
//! 这样安装包里不需要带图标资源，也不会出现"主题里找不到图标名"的空图标。
//!
//! ## 与 daemon 的关系（重要设计约束）
//!
//! **托盘退出不会关闭频谱。** 两者是独立进程：
//!
//! - 托盘只是遥控器，退出只销毁自己的界面；
//! - 停止频谱必须走菜单里的显式项（`Action::Stop`），它发的是
//!   `stop` 控制命令，daemon 会**优雅退出**并销毁叠加窗；
//! - daemon 不能因为找不到托盘就退出 —— 它要能纯 `systemd` 下独立运行。
//!
//! 这条约束很容易被"顺手实现"破坏（比如把 daemon 变成托盘的子进程，
//! 或者退出时 kill 自己的孩子），所以写在这里当护栏。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::time::Duration;

// 本模块就在 spectrum-daemon 库里，用 `crate::` 而不是包名
use crate::icon::icon_argb;
use crate::instance::{ControlCommand, default_lock_path, is_running, send_command};

/// 菜单项 id（与 `GetLayout` 里返回的一致）。
mod ids {
    pub const STATUS: i32 = 1;
    pub const START: i32 = 10;
    pub const STOP: i32 = 11;
    pub const RESTART: i32 = 12;
    pub const SEPARATOR: i32 = 13;
    pub const QUIT: i32 = 20;
}

/// 菜单点击后要执行的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Start,
    Stop,
    Restart,
    QuitTray,
}

/// 托盘 pid 文件：进程存活期间存在，正常退出时删除。
///
/// 只写自己的 pid；删除前核对内容仍是自己，避免多实例时误删别人的记录。
struct PidFile {
    path: std::path::PathBuf,
    pid: u32,
}

impl PidFile {
    /// 写入当前进程的 pid；路径不可写时静默跳过（托盘功能不依赖它）。
    fn create() -> Self {
        let dir = std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|dir| !dir.is_empty())
            .map_or_else(std::env::temp_dir, std::path::PathBuf::from);
        let path = dir.join("deepin-spectrum-tray.pid");
        let pid = std::process::id();
        let _ = std::fs::write(&path, pid.to_string());
        Self { path, pid }
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        let own = std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok());
        if own == Some(self.pid) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// 托盘的主循环。
///
/// # Errors
///
/// 会话总线不可达、或总线名被占用时返回错误说明。
pub fn run() -> Result<(), String> {
    // 0.2.6：托盘不再是独立 .desktop 入口，设置窗口靠这个 pid 文件判断
    // "托盘是否已显示"（含登录自启直接拉起的实例）。路径与 GUI 的
    // `spectrum-settings-gui::trayctl` 约定一致，两边改一处必须同步。
    let _pid_file = PidFile::create();
    let (tx, rx) = std::sync::mpsc::channel::<Action>();
    // 服务名按 D-Bus 惯例带 pid，避免多实例（托盘允许重复启动）互相覆盖
    let service_name = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());

    // 服务端（SNI + 菜单）在自己的线程里跑：zbus 的 Connection 与创建它的运行时
    // 绑定，不能跨线程共享（与 dock_geom 同一个约束）。
    let server_tx = tx.clone();
    let server_name = service_name.clone();
    let handle = std::thread::Builder::new()
        .name("deepin-spectrum-tray".to_string())
        .spawn(move || {
            if let Err(err) = zbus::block_on(serve(server_tx, server_name)) {
                eprintln!("托盘服务启动失败：{err}");
            }
        })
        .map_err(|e| format!("无法创建托盘线程：{e}"))?;

    // 把服务名打出来：脚本与用户排查都要用，省得去猜 pid
    println!("托盘已启动：service={service_name}（右键图标可停止/重启频谱）");
    loop {
        match rx.recv() {
            Ok(Action::Start) => match start_daemon() {
                Ok(pid) => println!("已启动频谱（pid={pid}）"),
                Err(err) => eprintln!("启动频谱失败：{err}"),
            },
            Ok(Action::Stop) => match stop_daemon() {
                Ok(()) => println!("已请求停止频谱"),
                Err(err) => eprintln!("停止频谱失败：{err}"),
            },
            Ok(Action::Restart) => match restart_daemon() {
                Ok(pid) => println!("已重启频谱（pid={pid}）"),
                Err(err) => eprintln!("重启频谱失败：{err}"),
            },
            Ok(Action::QuitTray) => {
                println!("托盘退出（频谱若在运行会继续运行）");
                break;
            }
            // 服务线程挂了（进程要退出时会发生），主线程跟着退
            Err(_) => break,
        }
    }
    // 不 join 服务线程：它阻塞在总线循环上，进程退出会一并结束。
    drop(handle);
    Ok(())
}

// ---------------------------------------------------------------- 控制动作

/// 停止频谱：走控制通道请求优雅退出。
fn stop_daemon() -> Result<(), String> {
    let path = default_lock_path();
    if !is_running(&path) {
        return Err("频谱未在运行".to_string());
    }
    let reply =
        send_command(&path, ControlCommand::Stop).map_err(|e| format!("发送停止命令失败：{e}"))?;
    if !reply.starts_with("ok") {
        return Err(format!("控制通道拒绝：{reply}"));
    }
    Ok(())
}

/// 启动频谱：把**自己**以 `daemon --live` 重新拉起。
///
/// 用 `current_exe()` 而不是去 PATH 里找 `deepin-spectrum`：
/// 托盘本身是同一个二进制的 `tray` 子命令，这样安装位置变了也不会找错。
fn start_daemon() -> Result<u32, String> {
    if is_running(default_lock_path()) {
        return Err("频谱已在运行".to_string());
    }
    let exe = std::env::current_exe().map_err(|e| format!("取可执行文件路径失败：{e}"))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon").arg("--live");
    // 脱离托盘：托盘退出不能带走正在跑的频谱（见模块文档的约束）
    spawn_detached(&mut cmd)
}

/// 重启频谱：先请求优雅退出，等控制 socket 消失，再重新拉起。
///
/// 这一步存在的理由就是用户说的场景：**服务卡住时**（比如音频服务端卡死、
/// 上游流程僵住），停止再启动比等着恢复更可靠。
fn restart_daemon() -> Result<u32, String> {
    let path = default_lock_path();
    if is_running(&path) {
        let _ = send_command(&path, ControlCommand::Stop);
        // 等它真的退出：最多 3s。日志里的"优雅退出"含销毁叠加窗，
        // 提前拉起会撞上单实例保护（那时旧实例还在占位）。
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            if !is_running(&path) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if is_running(&path) {
            return Err("旧实例 3s 内未退出，重启中止（避免两个实例抢屏）".to_string());
        }
    }
    start_daemon()
}

/// 被拉起的 daemon 的日志路径（放在运行目录，便于排查）。
fn daemon_log_path() -> Option<std::path::PathBuf> {
    default_lock_path()
        .parent()
        .map(|dir| dir.join("deepin-spectrum-tray-daemon.log"))
}

/// 以脱离会话的方式启动子进程。
///
/// **不能**把 stdout/stderr 丢掉：如果 daemon 起不来（X11/D-Bus/声卡任一出问题），
/// 用户点完菜单只会看到"什么也没发生"，而原因全被吞了。
/// 这里把两者追加到运行目录下的日志文件，出问题时有据可查。
fn spawn_detached(cmd: &mut std::process::Command) -> Result<u32, String> {
    cmd.stdin(std::process::Stdio::null());
    match daemon_log_path() {
        Some(path) => match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(log) => {
                let log2 = log
                    .try_clone()
                    .map_err(|e| format!("复制日志句柄失败：{e}"))?;
                cmd.stdout(log).stderr(log2);
            }
            Err(_) => {
                cmd.stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null());
            }
        },
        None => {
            cmd.stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
        }
    }
    let child = cmd.spawn().map_err(|e| format!("spawn 失败：{e}"))?;
    // 注意：**不**保存 Child 句柄。保存并在托盘退出时 kill 会直接违背
    // "托盘退出不影响频谱"的约束；这里要的就是让它独立活着。
    Ok(child.id())
}

// ---------------------------------------------------------------- D-Bus 服务

const ITEM_PATH: &str = "/StatusNotifierItem";
const MENU_PATH: &str = "/MenuBar";

/// 一个菜单节点：`(id, 属性, 子节点)` —— 对应 D-Bus 签名 `(ia{sv}av)`。
type MenuNode = (
    i32,
    HashMap<String, zbus::zvariant::Variant<'static>>,
    Vec<zbus::zvariant::Variant<'static>>,
);

/// `GetLayout` 的返回：`(revision, 根节点)` —— 对应 `u(ia{sv}av)`。
type MenuLayout = (u32, MenuNode);

/// 菜单点击的入口。
struct MenuService {
    tx: Sender<Action>,
}

impl MenuService {
    fn action_for(id: i32) -> Option<Action> {
        match id {
            ids::START => Some(Action::Start),
            ids::STOP => Some(Action::Stop),
            ids::RESTART => Some(Action::Restart),
            ids::QUIT => Some(Action::QuitTray),
            _ => None,
        }
    }
}

async fn serve(tx: Sender<Action>, service_name: String) -> Result<(), Box<dyn std::error::Error>> {
    let conn = zbus::Connection::session().await?;
    conn.request_name(service_name.as_str()).await?;

    conn.object_server()
        .at(MENU_PATH, MenuService { tx })
        .await?;
    conn.object_server().at(ITEM_PATH, ItemService).await?;

    // 注册到宿主。宿主是 dde-shell 提供的 org.kde.StatusNotifierWatcher。
    let watcher = zbus::Proxy::new(
        &conn,
        "org.kde.StatusNotifierWatcher",
        "/StatusNotifierWatcher",
        "org.kde.StatusNotifierWatcher",
    )
    .await?;
    watcher
        .call_method("RegisterStatusNotifierItem", &(service_name.as_str(),))
        .await?;

    // 保持连接：总线循环需要一直有 future 在跑。
    // `pending()` 永不完成，直到进程退出。
    std::future::pending::<()>().await;
    Ok(())
}

/// `org.kde.StatusNotifierItem`。
struct ItemService;

impl ItemService {
    /// 频谱是否在运行（读控制通道，不信"文件在不在"）。
    fn running() -> bool {
        is_running(default_lock_path())
    }
}

// zbus 生成的 property 方法签名固定带 `&self`，而多数属性是常量，
// 于是 clippy 会报 `unused_self` / `unnecessary_literal_bound`。
// 这是宏签名的约束，不是代码问题，故就地 allow 并说明。
#[allow(clippy::unused_self, clippy::unnecessary_literal_bound)]
#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl ItemService {
    /// 分类：`ApplicationStatus` 表示"常驻应用状态"（不是硬件指示器）。
    #[zbus(property)]
    fn category(&self) -> &str {
        "ApplicationStatus"
    }

    #[zbus(property)]
    fn id(&self) -> &str {
        "deepin-spectrum"
    }

    #[zbus(property)]
    fn title(&self) -> String {
        "任务栏音频频谱".to_string()
    }

    /// `Active` / `Passive`：宿主据此决定是否显示（本应用总是显示）。
    #[zbus(property)]
    fn status(&self) -> &str {
        "Active"
    }

    /// 图标：现场生成的 ARGB32 像素图（见 [`icon_argb`]）。
    ///
    /// 返回 `a(iiay)`：`(宽, 高, 像素)` 的数组。元素必须**从大到小**排列，
    /// 宿主通常挑第一个能放下的。
    #[zbus(property)]
    fn icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
        let (w, h, data) = icon_argb(32);
        vec![(w, h, data)]
    }

    /// 右键菜单的对象路径。
    #[zbus(property)]
    fn menu(&self) -> zbus::OwnedObjectPath {
        zbus::OwnedObjectPath::try_from(MENU_PATH).expect("常量路径合法")
    }

    /// 左键单击是否弹菜单（false = 左键激活、右键菜单）。
    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
    }

    /// 左键单击：没有主窗口，点一下就把当前状态打出来（便于排查）。
    fn activate(&self, x: i32, y: i32) {
        println!(
            "托盘被点击（{x},{y}）：频谱当前{}",
            if ItemService::running() {
                "运行中"
            } else {
                "未运行"
            }
        );
    }

    fn secondary_activate(&self, x: i32, y: i32) {
        let _ = (x, y);
    }

    fn context_menu(&self, x: i32, y: i32) {
        let _ = (x, y);
    }

    fn scroll(&self, delta: i32, orientation: &str) {
        let _ = (delta, orientation);
    }
}

/// 菜单文本的一行描述。
#[derive(Clone)]
struct MenuEntry {
    id: i32,
    label: String,
    enabled: bool,
    separator: bool,
}

fn menu_entries(running: bool) -> Vec<MenuEntry> {
    vec![
        MenuEntry {
            id: ids::STATUS,
            // 纯信息行：不可点，用 label 显示当前状态
            label: if running {
                "状态：运行中".to_string()
            } else {
                "状态：已停止".to_string()
            },
            enabled: false,
            separator: false,
        },
        MenuEntry {
            id: ids::START,
            label: "启动频谱".to_string(),
            enabled: !running,
            separator: false,
        },
        MenuEntry {
            id: ids::STOP,
            label: "停止频谱".to_string(),
            enabled: running,
            separator: false,
        },
        MenuEntry {
            id: ids::RESTART,
            label: "重启频谱".to_string(),
            enabled: true,
            separator: false,
        },
        MenuEntry {
            id: ids::SEPARATOR,
            label: String::new(),
            enabled: true,
            separator: true,
        },
        MenuEntry {
            id: ids::QUIT,
            label: "退出托盘（频谱继续运行）".to_string(),
            enabled: true,
            separator: false,
        },
    ]
}

/// 菜单布局版本：运行状态变化时递增，宿主据此丢弃缓存的旧布局。
static MENU_REVISION: AtomicU32 = AtomicU32::new(1);
/// 上一次生成布局时的运行状态。
static MENU_LAST_RUNNING: AtomicBool = AtomicBool::new(false);
/// 是否已经有基准状态（首个 `GetLayout` 只记录，不计为变化）。
static MENU_STATE_KNOWN: AtomicBool = AtomicBool::new(false);

/// 计算当前菜单布局版本（运行状态变化时 +1）。
fn menu_revision(running: bool) -> u32 {
    if MENU_STATE_KNOWN.swap(true, Ordering::SeqCst) {
        if MENU_LAST_RUNNING.swap(running, Ordering::SeqCst) != running {
            MENU_REVISION.fetch_add(1, Ordering::SeqCst);
        }
    } else {
        MENU_LAST_RUNNING.store(running, Ordering::SeqCst);
    }
    MENU_REVISION.load(Ordering::SeqCst)
}

// D-Bus 方法的参数由宏按信号签名**按值**接收（`Vec<String>`、`Variant` 等），
// 不能改成借用 —— 否则签名对不上。故就地 allow。
#[allow(
    clippy::unused_self,
    clippy::unnecessary_literal_bound,
    clippy::needless_pass_by_value
)]
#[zbus::interface(name = "com.canonical.dbusmenu")]
impl MenuService {
    /// 布局版本；每次布局变化应递增。这里是按需生成，用时间无关的常量即可。
    #[zbus(property)]
    fn version(&self) -> u32 {
        3
    }

    #[zbus(property)]
    fn text_direction(&self) -> &str {
        "ltr"
    }

    #[zbus(property)]
    fn status(&self) -> &str {
        "normal"
    }

    /// `GetLayout(parentId, recursionDepth, propertyNames) -> (revision, layout)`
    ///
    /// 返回值签名是 `u(ia{sv}av)`：`layout` 是 `(id, props, children)`。
    /// 我们只做**一层**菜单（children 恒为空），所以无需递归。
    fn get_layout(
        &self,
        parent_id: i32,
        recursion_depth: i32,
        property_names: Vec<String>,
    ) -> MenuLayout {
        // 只做一层菜单：忽略父节点与递归深度（宿主问一次就要全量）
        let _ = (parent_id, recursion_depth, property_names);
        let running = is_running(default_lock_path());
        let children = menu_entries(running)
            .into_iter()
            .map(|entry| {
                let mut props: HashMap<String, zbus::zvariant::Variant<'static>> = HashMap::new();
                if entry.separator {
                    props.insert(
                        "type".to_string(),
                        zbus::zvariant::Variant::new("separator"),
                    );
                } else {
                    props.insert(
                        "label".to_string(),
                        zbus::zvariant::Variant::new(entry.label),
                    );
                }
                props.insert(
                    "enabled".to_string(),
                    zbus::zvariant::Variant::new(entry.enabled),
                );
                props.insert("visible".to_string(), zbus::zvariant::Variant::new(true));
                let node = zbus::Structure::builder()
                    .add_field(entry.id)
                    .add_field(props)
                    .add_field(Vec::<zbus::zvariant::Variant<'static>>::new())
                    .build()
                    .expect("菜单节点结构合法");
                zbus::zvariant::Variant::new(node)
            })
            .collect::<Vec<_>>();

        let root = (
            0_i32,
            HashMap::<String, zbus::zvariant::Variant<'static>>::new(),
            children,
        );
        (menu_revision(running), root)
    }

    /// `GetGroupProperties(ids, propertyNames) -> a(ia{sv})`
    fn get_group_properties(
        &self,
        ids: Vec<i32>,
        property_names: Vec<String>,
    ) -> Vec<(i32, HashMap<String, zbus::zvariant::Variant<'static>>)> {
        let _ = property_names;
        let running = is_running(default_lock_path());
        menu_entries(running)
            .into_iter()
            .filter(|e| ids.is_empty() || ids.contains(&e.id))
            .map(|entry| {
                let mut props: HashMap<String, zbus::zvariant::Variant<'static>> = HashMap::new();
                props.insert(
                    "label".to_string(),
                    zbus::zvariant::Variant::new(entry.label),
                );
                props.insert(
                    "enabled".to_string(),
                    zbus::zvariant::Variant::new(entry.enabled),
                );
                (entry.id, props)
            })
            .collect()
    }

    /// 菜单项被点击。`event_id == "clicked"` 才算激活。
    fn event(&self, id: i32, event_id: String, data: zbus::zvariant::Variant<'_>, timestamp: u32) {
        let _ = (data, timestamp);
        if event_id != "clicked" {
            return;
        }
        if let Some(action) = MenuService::action_for(id) {
            // 只投递、不在这里执行：动作可能阻塞（spawn、等旧实例退出），
            // 在总线线程里阻塞会让整个菜单卡住。
            let _ = self.tx.send(action);
        }
    }

    /// 菜单即将展开：返回是否需要重建。
    ///
    /// 菜单状态行（运行中/已停止）是按需生成的，展开前必须让宿主重新取一次，
    /// 否则运行状态变化后宿主会继续显示缓存的旧布局。
    fn about_to_show(&self, id: i32) -> bool {
        let _ = id;
        true
    }

    fn about_to_show_group(&self, ids: Vec<i32>) -> (Vec<i32>, Vec<i32>) {
        (ids, Vec::new())
    }
}

// ---------------------------------------------------------------- 图标

/// 生成 `size × size` 的 ARGB32 像素图（D-Bus 要求 **网络字节序 A,R,G,B**）。
///
/// 画法：底部对齐的 5 根柱 + 青→品红渐变，与频谱本身同一套配色，
/// 让用户在托盘里一眼认出是同一个东西。
// 尺寸上限 32×32，浮点只用于柱高与颜色插值，
// 截断/精度/符号在此域内都不可能出问题，故就地 allow。
#[cfg(test)]
mod tests {
    use super::{Action, icon_argb, ids, menu_entries};

    #[test]
    fn icon_has_expected_layout_and_bytes() {
        let (w, h, data) = icon_argb(32);
        assert_eq!((w, h), (32, 32));
        assert_eq!(data.len(), 32 * 32 * 4, "ARGB32 每像素 4 字节");
        // 至少有一部分像素是不透明的（画了柱子），且 alpha 只会是 0 或 255
        let opaque = data.chunks_exact(4).filter(|px| px[0] == 255).count();
        assert!(opaque > 0, "图标不应全透明");
        assert!(
            data.chunks_exact(4).all(|px| px[0] == 0 || px[0] == 255),
            "alpha 只允许 0/255"
        );
    }

    #[test]
    fn menu_entries_reflect_running_state() {
        let running = menu_entries(true);
        let stopped = menu_entries(false);
        // 状态行必须反映真实状态
        assert!(running[0].label.contains("运行中"));
        assert!(stopped[0].label.contains("已停止"));
        // 运行中：停止可用、启动禁用；未运行反之
        let find = |v: &Vec<super::MenuEntry>, id: i32| {
            v.iter().find(|e| e.id == id).cloned().map(|e| e.enabled)
        };
        assert_eq!(find(&running, ids::STOP), Some(true));
        assert_eq!(find(&running, ids::START), Some(false));
        assert_eq!(find(&stopped, ids::STOP), Some(false));
        assert_eq!(find(&stopped, ids::START), Some(true));
        // 重启任何时候都应该可用（服务卡住时的救命入口）
        assert_eq!(find(&running, ids::RESTART), Some(true));
        assert_eq!(find(&stopped, ids::RESTART), Some(true));
    }

    #[test]
    fn menu_ids_map_to_actions() {
        assert_eq!(
            super::MenuService::action_for(ids::STOP),
            Some(Action::Stop)
        );
        assert_eq!(
            super::MenuService::action_for(ids::RESTART),
            Some(Action::Restart)
        );
        assert_eq!(
            super::MenuService::action_for(ids::START),
            Some(Action::Start)
        );
        assert_eq!(
            super::MenuService::action_for(ids::QUIT),
            Some(Action::QuitTray)
        );
        // 状态行与分隔符不可点
        assert_eq!(super::MenuService::action_for(ids::STATUS), None);
        assert_eq!(super::MenuService::action_for(ids::SEPARATOR), None);
    }
}
