// T34：JobObject 的进程树回收。
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

/// 非 Windows：没有 job 这一套 ⇒ 返回 `Err`（R323 的失败策略：不静默降级）。
/// 本项目发布目标只有 Windows，这一支是为了让 `cargo check` 在别的平台上不骗人。
#[cfg(not(windows))]
pub struct KillOnCloseJob;

#[cfg(not(windows))]
pub fn create_kill_job() -> Result<KillOnCloseJob, String> {
    Err("当前平台不支持进程回收作业对象".to_string())
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
