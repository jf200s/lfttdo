// T34：JobObject（Windows）/ 进程组（Unix）的进程树回收。
//
// 为什么需要它（切片 0 的实测，`docs/findings/plan-a-risk-verification.md:559-620`）：
// 我们 spawn 的是 `npx.cmd`，Windows 会为它另起一个 `cmd.exe` 宿主，真正的 node server 是**孙进程**；
// rmcp 的 `cancel()` / `graceful_shutdown()` 只能杀直接子进程，`taskkill /T /F` 在实测里也有
// 「只杀掉 1 个、其余报此任务不支持」的情形。⇒ 交付线「无孤儿子进程」不能靠这两条。
//
// 形状：**一个进程一个 job，所有 MCP 子进程共享**。job 设了 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`，
// 所以应用退出（job 句柄被 `Drop`/进程死亡）时整棵树由内核带走 —— 不依赖我们逐条 kill 成功。
//
// 失败策略（R323）：拿不到 job ⇒ 返回 `Err`，调用方把该 server 标成 Disconnected，**不许**
// 退化成「不带 job 直接 spawn」。静默退化正是「关窗后 npx 堆积」这个桌面经典 bug 的成因。

/// 一个「关掉就杀整棵树」的 job。`Drop` 里 `CloseHandle` ⇒ 内核立刻终止 job 里所有进程。
#[cfg(windows)]
pub struct KillOnCloseJob {
    handle: windows::Win32::Foundation::HANDLE,
}

// `HANDLE` 是裸指针，默认 !Send/!Sync；Win32 的 job/进程句柄本身是线程安全的操作系统对象
// （内核对象不绑定创建线程），这里的所有方法都只做原子式的系统调用，不共享 Rust 状态。
#[cfg(windows)]
unsafe impl Send for KillOnCloseJob {}
#[cfg(windows)]
unsafe impl Sync for KillOnCloseJob {}

#[cfg(windows)]
impl Drop for KillOnCloseJob {
    fn drop(&mut self) {
        // 尽力而为：句柄已经不能用时也没别的收场方式，Drop 里不能 panic。
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

#[cfg(windows)]
impl std::fmt::Debug for KillOnCloseJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KillOnCloseJob")
            .field("handle", &self.handle.0)
            .finish()
    }
}

/// 建一个 kill-on-close 的 job（R322）。失败（权限/系统策略）⇒ `Err`，调用方**不许**静默降级。
#[cfg(windows)]
pub fn create_kill_job() -> Result<KillOnCloseJob, String> {
    use windows::Win32::System::JobObjects::{
        CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    use windows::core::PCWSTR;

    let handle = unsafe { CreateJobObjectW(None, PCWSTR::null()) }
        .map_err(|e| format!("无法建立进程回收作业对象：CreateJobObjectW 失败（{e}）"))?;

    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let rc = unsafe {
        SetInformationJobObject(
            handle,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if let Err(e) = rc {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(handle);
        }
        return Err(format!(
            "无法建立进程回收作业对象：SetInformationJobObject 失败（{e}）"
        ));
    }
    Ok(KillOnCloseJob { handle })
}

/// 把一个已 spawn 的进程放进 job（R322）。放进 job 之后它**以及它之后创建的所有后代**都归 job 管。
#[cfg(windows)]
impl KillOnCloseJob {
    pub fn assign(&self, pid: u32) -> Result<(), String> {
        use windows::Win32::Foundation::{CloseHandle, HANDLE};
        use windows::Win32::System::JobObjects::AssignProcessToJobObject;
        use windows::Win32::System::Threading::{
            OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
        };

        let process: HANDLE =
            unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, pid) }
                .map_err(|e| format!("无法建立进程回收作业对象：OpenProcess({pid}) 失败（{e}）"))?;
        let assigned = unsafe { AssignProcessToJobObject(self.handle, process) };
        // 进程句柄用完即关；job 自己持有的是「进程在 job 里」这件事，不依赖这个句柄。
        unsafe {
            let _ = CloseHandle(process);
        }
        assigned.map_err(|e| format!("无法把进程 {pid} 放进回收作业对象（{e}）"))
    }
}

// ─── Unix：进程组等价物 ───
//
// 语义与 JobObject 等价，但建立时机不同：Windows 是「先建 job，spawn 后 assign」；
// Unix 的进程组**必须在 spawn 时建立**（`process_group(0)`，在 bridge.rs 的
// `SpawnPlan::into_command` —— 真 spawn 的唯一出口 —— 里做），spawn 之后再补 `setpgid`
// 会撞上 exec 竞态（父进程无法保证子进程还没 exec）。所以职责切分是：
//   * spawn 侧：`process_group(0)` 让子进程自成一个进程组（pgid = pid）。setpgid 失败
//     会让 spawn 本身报错 ⇒ `connect_stdio` 把这条标成 Disconnected —— 与 R323 同一
//     失败策略：**不静默降级**。spawn 成功 ⇒ 组必然存在，没有「没建成组却 spawn 成功」
//     的中间态。
//   * assign 侧：把 pgid 记下来「上膛」。不验证组存在（验证会与 pre_exec 的 setpgid
//     竞态），也不需要：存在性由 spawn 侧的结构保证。
//   * Drop 侧：`kill(-pgid, SIGKILL)` 带走整组 —— 组内之后 fork/spawn 出来的孙进程
//     同属该组，一并带走，与 `KILL_ON_JOB_CLOSE` 等价。
//
// 与 JobObject 的两处已知差异（同为 best-effort 语义下的可接受残留）：
//   * job 持内核句柄，pgid 只是个数字：若直接子进程先被收尸、pid 被复用、且复用者恰好
//     又是组领袖，kill 会打偏。收尸发生在 transport 释放之后、本结构 Drop 之前
//     （`Connection` 里相邻的两个字段），窗口极小。
//   * daemon 化（setsid 脱组）的孙进程两边都会逃逸 —— Windows 上同一句话成立。

/// 「上膛」标记：0 = 未 assign（Drop 时不动干戈）。真实 pgid = 直接子进程 pid，恒 > 0。
#[cfg(not(windows))]
pub struct KillOnCloseJob {
    pgid: std::sync::atomic::AtomicI32,
}

// AtomicI32 自带 Send/Sync，无需像 Windows 的裸 HANDLE 那样 unsafe 声明。
#[cfg(not(windows))]
impl std::fmt::Debug for KillOnCloseJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KillOnCloseJob")
            .field("pgid", &self.pgid)
            .finish()
    }
}

#[cfg(not(windows))]
impl Drop for KillOnCloseJob {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        let pgid = self.pgid.load(Ordering::Acquire);
        if pgid > 0 {
            // 尽力而为：组已全灭（ESRCH）或权限不足（EPERM）时没有别的收场方式，Drop 里不 panic。
            let _ = unsafe { libc::kill(-pgid, libc::SIGKILL) };
        }
    }
}

/// Unix 支的「建 job」无事可做（组在 spawn 时由 `into_command` 建立）⇒ 恒 `Ok`。
/// 真正可能失败的环节是 spawn 本身（setpgid 失败 ⇒ spawn 报错 ⇒ 这条不连，R323）。
#[cfg(not(windows))]
pub fn create_kill_job() -> Result<KillOnCloseJob, String> {
    Ok(KillOnCloseJob {
        pgid: std::sync::atomic::AtomicI32::new(0),
    })
}

#[cfg(not(windows))]
impl KillOnCloseJob {
    /// spawn 成功后记录 pgid（= 子进程 pid，`process_group(0)` 的定义）。
    /// 恒成功：pid 超过 `i32::MAX` 这种形态现实里不存在，`try_from` 只是类型上绕不开。
    pub fn assign(&self, pid: u32) -> Result<(), String> {
        use std::sync::atomic::Ordering;
        let pgid = i32::try_from(pid).map_err(|e| format!("pid {pid} 越界：{e}"))?;
        self.pgid.store(pgid, Ordering::Release);
        Ok(())
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;
    use std::time::{Duration, Instant};

    /// `tasklist` 里有这个 pid 吗（输出 GBK，但 PID 是 ASCII）。
    fn pid_alive(pid: u32) -> bool {
        let out = StdCommand::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output();
        match out {
            Ok(o) => String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()),
            // 取不到就当成「还活着」：宁可让测试红，也不许把「没法确认」当成「已回收」。
            Err(_) => true,
        }
    }

    #[test]
    fn create_kill_job_works_and_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<KillOnCloseJob>();
        let job =
            create_kill_job().expect("本机必须能建 job（失败就是 R323 的 Disconnected 分支）");
        drop(job);
    }

    #[test]
    fn dropping_the_job_takes_the_whole_tree_with_it() {
        let job = create_kill_job().expect("建 job");
        // 三层：cmd.exe → ping.exe（`ping -n 120` 约两分钟，足够测量）。
        let mut cmd = StdCommand::new("cmd");
        cmd.args(["/c", "ping", "-n", "120", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut child = cmd.spawn().expect("spawn cmd");
        let pid = child.id();
        job.assign(pid).expect("把直接子进程放进 job");
        assert!(
            pid_alive(pid),
            "刚 spawn 的进程必须在场，否则这条测试什么都没测到"
        );

        drop(job);
        // `KILL_ON_JOB_CLOSE` 是内核行为，几毫秒内生效；给 3 秒的余量。
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(3) {
            if !pid_alive(pid) {
                // 回收子进程句柄（它已经被 job 杀掉，`wait` 立即返回）。
                let _ = child.wait();
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        // 请神容易送神难：这里失败说明 JobObject 在本机也救不了，要如实上报（R324）。
        let _ = StdCommand::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .output();
        let _ = child.wait();
        panic!("drop(job) 之后 {pid} 仍在世：JobObject 没有带走整棵树");
    }
}

#[cfg(all(test, not(windows)))]
mod unix_tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::Command as StdCommand;
    use std::time::{Duration, Instant};

    fn assert_send_sync<T: Send + Sync>() {}

    /// 组里还有活口吗。`kill(-pgid, 0)` 只探测不投递；返回 -1 且 errno = ESRCH 才算全灭，
    /// 其余（含拿不到 errno 的形态）一律当「还在」：宁可让测试红，也不把「没法确认」当成「已回收」。
    fn group_alive(pgid: i32) -> bool {
        let rc = unsafe { libc::kill(-pgid, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }

    #[test]
    fn create_kill_job_works_and_is_send_and_sync() {
        assert_send_sync::<KillOnCloseJob>();
        let job = create_kill_job().expect("unix 支建 job 恒成功");
        drop(job);
    }

    /// 与 Windows 侧 `dropping_the_job_takes_the_whole_tree_with_it` 同一条验收：
    /// drop 之后整棵树（含孙进程）必须被内核带走。
    #[test]
    fn dropping_the_job_takes_the_whole_group_with_it() {
        let job = create_kill_job().expect("建 job");
        // 与 bridge.rs `into_command` 同一动作：spawn 时自成一组（这是 unix 支的全部前提）。
        // `sh -c 'sleep 120 & wait'`：sh 守着 wait，后台的 sleep 是「后代进程」的最小替身。
        let mut cmd = StdCommand::new("sh");
        cmd.args(["-c", "sleep 120 & wait"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0);
        let mut child = cmd.spawn().expect("spawn sh");
        let pid = child.id();
        job.assign(pid).expect("unix 支 assign 恒成功");
        let pgid = pid as i32;
        assert!(
            group_alive(pgid),
            "刚 spawn 的进程组必须在场，否则这条测试什么都没测到"
        );

        drop(job);
        // 先收尸 sh：SIGKILL 之后它是我们名下的**僵尸**，而僵尸仍占着组员名额 —— 不收尸，
        // `kill(-pgid, 0)` 会永远探到「组还在」（Windows 没有僵尸，`tasklist` 那条探针没有这层）。
        // sh 3 秒内没死 = SIGKILL 根本没投进组，同样按失败处理，绝不挂住测试。
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(3) {
            if matches!(child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if !matches!(child.try_wait(), Ok(Some(_))) {
            // 清扫：SIGKILL 没到组里，用测试自己的 kill 收场再如实上报。
            let _ = StdCommand::new("kill")
                .args(["-9", &format!("-{pgid}")])
                .output();
            let _ = child.wait();
            panic!("drop(job) 之后 sh 未死：SIGKILL 没有投到组 -{pgid} 里");
        }
        // sh 已收尸出组，组里只剩孙进程（被 init 收尸前短暂占名额）。SIGKILL 是内核行为，
        // 几毫秒内生效；给 3 秒的余量。
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(3) {
            if !group_alive(pgid) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        // 请神容易送神难：这里失败说明组回收没带走孙进程，要如实上报（R324）。
        // 测试自己的清扫：别把 sleep 120 留在机器上。
        let _ = StdCommand::new("kill")
            .args(["-9", &format!("-{pgid}")])
            .output();
        panic!("drop(job) 之后组 -{pgid} 仍有活口：进程组回收没有带走整棵树");
    }
}
