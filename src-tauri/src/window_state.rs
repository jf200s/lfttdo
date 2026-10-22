// 记住窗口尺寸与位置（spec §6.6 的「单窗口、记住尺寸位置」，Plan A 只做了前半句）。
//
// 本模块**不 `use tauri`**：`lib.rs:4-6` 的边界规则是「`src/` 下只有 `ipc.rs` 允许依赖 Tauri」，
// 所以这里只有纯数据 + 纯函数，几何怎么从窗口上取、事件怎么映射，全部留在 `ipc.rs`。
// 这条边界顺带带来一个实际好处：下面这些测试不需要任何运行时、不需要显示器就能跑。

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;

/// 窗口几何，单位一律**物理像素**。取和放都在同一个坐标系里
/// （`outer_position()`/`outer_size()` 返回物理值，`set_position(PhysicalPosition)` /
/// `set_size(PhysicalSize)` 收物理值时不做缩放），所以这一层不需要 `scale_factor`。
/// 存 logical 值的后果是换一块缩放不同的显示器后窗口摆到错位置，而那正是本模块要防的病。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowGeom {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// 尺寸下限：被拖成 1px 宽的窗口既看不见也抓不回来，「记住」它等于记住一次自我锁死。
const MIN_WIDTH: u32 = 320;
const MIN_HEIGHT: u32 = 240;

/// 存档文件名。定义在这里而不是 `ipc.rs` 里：写和读必须共用一个答案，
/// 否则改名的那一次会变成「窗口位置神秘失忆」，而且没有任何测试会红。
pub const STATE_FILE: &str = "window.json";

/// 只有这些事件才值得落盘。抽成纯函数是为了让「一次拖拽写几百次磁盘」这种错实现
/// 能被一条测试抓住，而不是靠注释约束。
pub enum Ev {
    Resized,
    Moved,
    CloseRequested,
    Destroyed,
}

pub fn should_persist(ev: Ev) -> bool {
    matches!(ev, Ev::CloseRequested | Ev::Destroyed)
}

/// 读不到、解不开，一律按「没有存档」处理，**不**返回错误。
/// 理由见测试 `a_corrupt_or_missing_state_file_is_absent_not_an_error`：能让应用起不来
/// 的最坏原因，恰恰是一个用户既看不到也删不掉的配置文件。
pub fn load(path: &Path) -> Option<WindowGeom> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// 原子写，照 `settings.rs:478-490` 的写法：同目录 temp → `write_all` → `sync_all` → `rename`。
/// 这里不需要 `settings.rs` 那把实例锁：调用点只有窗口的关闭事件，而窗口事件是在
/// 同一根事件循环线程上串行派发的，进程内不存在两个并发写者。
pub fn save(path: &Path, g: WindowGeom) -> Result<(), String> {
    let json = serde_json::to_string_pretty(&g).map_err(|e| e.to_string())?;
    let temp = path.with_file_name(format!(
        "{}.tmp",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    {
        let mut file = std::fs::File::create(&temp).map_err(|e| e.to_string())?;
        file.write_all(json.as_bytes()).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
    }
    std::fs::rename(&temp, path).map_err(|e| e.to_string())
}

/// 把一份存档几何改成「当前这块屏幕上真的看得见」的形状；改不了就返回 `None`（= 用默认值）。
/// `work` 是 `Monitor::work_area()` 摊平后的 `(x, y, width, height)`，绝对物理像素。
pub fn sane(g: WindowGeom, work: (i32, i32, u32, u32)) -> Option<WindowGeom> {
    let (work_x, work_y, work_w, work_h) = work;
    let width = g.width.max(MIN_WIDTH).min(work_w.max(1));
    let height = g.height.max(MIN_HEIGHT).min(work_h.max(1));
    let g = WindowGeom { width, height, ..g };
    // 右下角用 `- 1`：窗口占据的是 `[x, x+width)` 这一半开区间，最后一个**有像素的**
    // 列是 `x+width-1`。用 `x+width` 判定的话，贴右边缘那条会被误判成越界。
    let right = g.x.saturating_add(width.saturating_sub(1) as i32);
    let bottom = g.y.saturating_add(height.saturating_sub(1) as i32);
    let inside = |x: i32, y: i32| {
        x >= work_x
            && x < work_x.saturating_add(work_w as i32)
            && y >= work_y
            && y < work_y.saturating_add(work_h as i32)
    };
    // 四个角里**有一个**还在工作区内就保留。判据写成「左上角在内」的话，
    // 「拖到右边缘只露一条」这种合法摆法会被丢掉，用户看到的症状是「我摆的位置没记住」。
    if inside(g.x, g.y) || inside(right, g.y) || inside(g.x, bottom) || inside(right, bottom) {
        Some(g)
    } else {
        None
    }
}

/// 在多块显示器里挑出**真能装下这个窗口**的那一块的工作区来钳位；一块都装不下才返回 `None`。
/// 为什么不是「只问当前显示器」：窗口刚创建时 `current_monitor()` 给的是系统把它摆在的那块
/// （通常是主屏），于是摆在副屏上的存档会被判成越界并丢掉。症状不是报错，而是
/// 「每次都回默认位置」—— 而这恰恰是双屏用户每天的摆法。
pub fn pick(g: WindowGeom, works: &[(i32, i32, u32, u32)]) -> Option<WindowGeom> {
    works.iter().find_map(|&work| sane(g, work))
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORK: (i32, i32, u32, u32) = (0, 0, 1920, 1040);

    /// 四元组构造器：这四条判据的区别只在几何本身，写成字面量的话每条要占 7 行，
    /// 而「哪个角还在屏内」正是需要一眼看清的那四个数。
    fn geom(x: i32, y: i32, width: u32, height: u32) -> WindowGeom {
        WindowGeom {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn a_geometry_fully_off_the_current_work_area_is_discarded() {
        // 症状：拔掉副屏后重启，窗口落在 x=3000 上——用户看到的是「程序打不开」，
        // 唯一的线索是任务栏图标点不出来也关不掉。这不是边角，是双屏用户的日常。
        assert_eq!(sane(geom(3000, 0, 800, 600), WORK), None);
        // 只要**有一个角**还在工作区内就保留（拖到边缘只露一条是合法用法）：
        assert!(sane(geom(1919, 0, 800, 600), WORK).is_some());
        assert!(sane(geom(-799, 0, 800, 600), WORK).is_some());
        assert!(sane(geom(-800, 0, 800, 600), WORK).is_none());
    }

    #[test]
    fn tiny_or_absurd_geometry_is_clamped_not_obeyed() {
        let g = sane(geom(0, 0, 1, 1), WORK).unwrap();
        assert!(
            g.width >= 320 && g.height >= 240,
            "被拖成 0 宽就再也抓不回来"
        );
        // 副屏比主屏大、或最大化写回的尺寸：必须缩回工作区内，而不是钳到某个上限后照样跑出屏幕。
        let big = sane(geom(0, 0, 99999, 99999), WORK).unwrap();
        assert!(big.width <= 1920 && big.height <= 1040);
    }

    #[test]
    fn a_corrupt_or_missing_state_file_is_absent_not_an_error() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("window.json");
        assert_eq!(load(&p), None, "第一次运行没有这个文件，是常态");
        std::fs::write(&p, "{ not json").unwrap();
        assert_eq!(
            load(&p),
            None,
            "解不开就按『没有』处理——启动失败的用户看不到任何解释，只会以为程序坏了"
        );
    }

    #[test]
    fn a_geometry_on_any_attached_monitor_is_kept() {
        // 副屏在右（工作区从 x=1920 起），窗口摆在副屏上是双屏用户的日常。
        // 只按「当前显示器」判定的话，每次重启都会把它从副屏拽回主屏 ——
        // 症状不是崩溃，是「明明记住了却每次都回默认位置」，也就是这条需求白做。
        let saved = geom(2400, 200, 900, 700);
        let works = [(0, 0, 1920, 1040), (1920, 0, 2560, 1440)];
        assert_eq!(pick(saved, &works), Some(saved));
        // 拔掉副屏之后同一份存档：一块屏都装不下 ⇒ 丢掉，落回 conf 的默认尺寸。
        assert_eq!(pick(saved, &[(0, 0, 1920, 1040)]), None);
        // 一块显示器都枚举不到：不恢复，也不许 panic。
        assert_eq!(pick(saved, &[]), None);
    }

    #[test]
    fn only_the_close_events_persist() {
        // 判据是**可观测的写入次数**：一次拖拽会触发几百个 Resized/Moved，
        // 每个都落盘等于把「记住位置」做成「每次移动都写一次磁盘」。
        assert!(!should_persist(Ev::Resized));
        assert!(!should_persist(Ev::Moved));
        assert!(should_persist(Ev::CloseRequested));
        assert!(should_persist(Ev::Destroyed));
    }

    #[test]
    fn a_saved_geometry_reads_back_and_leaves_no_temp_behind() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("window.json");
        let g = geom(-1200, 80, 1024, 700);
        save(&p, g).unwrap();
        assert_eq!(load(&p), Some(g));
        // temp→rename 的写法若漏了 rename，界面上「记住了」，磁盘上留着的是一个没人读的 .tmp。
        let leftovers: Vec<String> = std::fs::read_dir(d.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "window.json")
            .collect();
        assert_eq!(
            leftovers,
            Vec::<String>::new(),
            "落盘后目录里只许有状态文件本身"
        );
    }
}
