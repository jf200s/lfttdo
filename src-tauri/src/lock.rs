//! 中毒容忍的锁访问（R109）。
//!
//! 这三条函数是整个 crate 里唯一许碰 `unwrap()` 的锁入口。理由不是「panic 不好看」，
//! 而是这些锁**活得比一次 panic 长**：`AppState`、`PermissionGate`、`ToolRegistry`
//! 都挂在进程级的 `State` 上，持锁期间别处 panic 会把 `Mutex`/`RwLock` 打成中毒态，
//! 此后每次 `.lock().unwrap()` 都 panic —— 症状是「一次意外的越界之后，软件每条命令
//! 都打不开」，而且用户看到的是空白界面，不是一条错误。
//!
//! 中毒在这里不是需要恢复的错误，而是「上一个持有者没跑完它的赋值」这个既成事实：
//! 内层值仍然在锁里，`into_inner()` 拿到的就是它。闸门继续服务比停在中毒上更接近
//! 用户要的「还能用」。真正需要 panic 的场合只有测试夹具（断言不该发生的真的发生了），
//! 所以 `cfg(test)` 里的辅助类型照旧用 `.lock().unwrap()`，那是有意为之。

use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// `Mutex` 的中毒容忍访问。
pub fn guard<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 读锁的中毒容忍访问。RwLock 的读操作同样可能拿到中毒的锁 —— 那意味着**写者**panic 了，
/// 与本篇读者无关，所以继续读是安全判断而不是掩盖。
pub fn read<T>(m: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    m.read().unwrap_or_else(|e| e.into_inner())
}

/// 写锁的中毒容忍访问。
pub fn write<T>(m: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    m.write().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn a_poisoned_mutex_still_hands_over_its_inner_value() {
        // 这条是整个模块的存在理由：中毒之后**照常服务**。用 `.lock().unwrap()` 的实现
        // 在这里就会 panic，而那正是 R109 要挡住的「每条命令都打不开」。
        // 中毒的造法：起一段持锁的闭包让它 panic，`catch_unwind` 把 panic 关在这条测试里
        // （guard 在展开时 drop，std 据此把锁标记成中毒）。
        let m = Arc::new(Mutex::new(7usize));
        let holder = m.clone();
        let _ = std::panic::catch_unwind(move || {
            let _g = holder.lock().unwrap();
            panic!("持锁期间 panic，目的就是中毒");
        });
        assert_eq!(*guard(&m), 7, "中毒不许改变内层值，也不许拦住访问");
    }

    #[test]
    fn a_poisoned_rwlock_still_serves_reads_and_writes() {
        let rw = RwLock::new(vec![1, 2]);
        {
            let _ = std::panic::catch_unwind(|| {
                let _g = rw.write().unwrap();
                panic!("写者 panic，把锁打成中毒");
            });
        }
        assert_eq!(*read(&rw), vec![1, 2]);
        write(&rw).push(3);
        assert_eq!(*read(&rw), vec![1, 2, 3], "中毒后写路径同样要能用");
    }
}
