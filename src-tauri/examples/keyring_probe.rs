//! 人工核验探针（非生产代码）。只作为 `cargo run --example keyring_probe <相位>` 运行，
//! 不会被 `mod` 进 `src/`，所以**任何测试运行器都摸不到它** —— `cargo test`、
//! `cargo test -- --ignored`、`--include-ignored`、nextest、IDE 的 code lens 全都不经过这里。
//! 形态照搬本仓库已有的 `mcp_probe.rs`（它头两行写的就是这个理由）。
//!
//! 它为什么存在：brief Step 4 要求「亲眼看到 key 落进 Windows 凭据管理器」。这句话在自动化测试里
//! 测不到 —— `settings.rs` 整条 `SecretStore` 接缝存在的理由就是绕开真实库（R80/R81），
//! 注入 mock 之后测试永远只会写进 mock。2026-10-20 之前这一步只有源码里一句「应当」，没有任何证据。
//!
//! 它为什么**不**是个 `#[ignore]` 测试：`#[ignore]` 挡不住 `--ignored` / `--include-ignored`
//! （实测：两种跑法都会真的执行探针），那时唯一的闸门是「环境变量恰好等于 `arm` 或 `check`」。
//! 会写进用户真实凭据库、且写成功后留在进程之外的东西，不该靠一道门挡着。放在 examples 之后
//! R81 重新变成没有例外的绝对规则。
//!
//! 跨进程是设计的一部分，不是一次运行：`arm` 和 `check` 必须是**两条命令、两个进程**，
//! 第二个进程读得回来才证明落进了 Windows 库而不是某个实例内的缓存。
//!
//! ```text
//! cargo run --example keyring_probe -- status    # 只读，收尾自检
//! cargo run --example keyring_probe -- arm       # 写入 canary（库里已有条目则拒绝动手）
//! cargo run --example keyring_probe -- check     # 另一个进程读回 → 删除 → 再删一次
//! cargo run --example keyring_probe -- cleanup   # 只删本探针写的 canary，别的条目不碰
//! ```
//!
//! **本探针永不打印条目的值**，只在拒绝时打印长度：库里那条可能就是用户的真 key，
//! 而 stdout 会进终端滚动缓冲、也可能进 CI 日志。

use qwen_do_lib::settings::{KeyringSecretStore, SecretStore};

/// 一眼可辨的假 key：真 key 不会长这样，`cleanup` 靠它判断这条是不是本探针写的。
const CANARY: &str = "sk-QWEN-DO-SELFTEST-CANARY-DELETE-ME";

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "本探针只在 Windows 上有意义：非 Windows 的 store 每次调用都返回 UnsupportedPlatform（R81），\
         拿不到 brief Step 4 要的那个证据。"
    );
}

#[cfg(windows)]
fn main() {
    let phase = std::env::args().nth(1).unwrap_or_default();
    let store = KeyringSecretStore::new().expect("真实 Windows store 构造失败");
    // 读一次先立着：三个相位都要知道库里现在有没有东西、是谁写的。
    let present = store.get().expect("读真实条目失败");

    match phase.as_str() {
        "status" => match present.as_deref() {
            None => println!("status: 干净 —— 库里没有 `api_key.qwen-do`。"),
            Some(v) if v == CANARY => println!(
                "status: 库里有一条**本探针留下的 canary 残留**。跑 `cleanup` 收尾，别让它躺到 T13 上线。"
            ),
            Some(v) => println!(
                "status: 库里有一条**不是本探针写的**条目（值长度 {} 字符）。本探针不会碰它。",
                v.chars().count()
            ),
        },
        "arm" => {
            if let Some(v) = present.as_deref() {
                // 长度之外的信息一概不给：这条可能是用户的真 key。
                eprintln!(
                    "arm 拒绝动手：真实库里已经有一条（值长度 {} 字符）。本探针绝不覆盖别人的条目{}\n\
                     清场后再来：`cargo run --example keyring_probe -- cleanup`",
                    v.chars().count(),
                    if v == CANARY {
                        " —— 而且看着像是上一次人工跑没收尾留下的 canary"
                    } else {
                        ""
                    }
                );
                std::process::exit(1);
            }
            store
                .set(CANARY)
                .expect("arm：CredWriteW 经本模块接缝写入失败");
            let back = store.get().expect("arm：写后读回失败");
            assert!(
                back.as_deref() == Some(CANARY),
                "arm：同进程写进去读不回来 —— 接缝或真实 store 坏了，别再往下跑"
            );
            println!(
                "arm: 已写入 canary，同进程读回一致。现在用 cmdkey 应当查得到 `api_key.qwen-do`："
            );
            println!("     cmdkey //list 2>&1 | iconv -f GBK -t UTF-8 | grep -i qwen");
            println!(
                "⚠ 必须再跑第二条命令 `check`。只跑到 arm 就收工，canary 会长期躺在真实库里，"
            );
            println!(
                "  而 T13 会把它当成用户配好的 key（掩码看着正常，每次调用 401）。随时 `cleanup`。"
            );
        }
        "check" => {
            // 比较用 `==` 不用 assert_eq!：后者会把两边的值打进 panic 输出，
            // 而这里左边那条可能是用户的真 key。
            assert!(
                present.as_deref() == Some(CANARY),
                "check：这个进程读回来才叫「落进了 Windows 库」。读不到说明 arm 那次没持久化、\
                 或者已经被 cleanup —— 先跑 `status` 看库里现在到底是什么。"
            );
            store.delete().expect("check：删除失败");
            assert!(
                store.get().expect("check：删后读失败").is_none(),
                "check：删完必须读不到（NoEntry → Ok(None)）"
            );
            store
                .delete()
                .expect("check：再删一次仍要幂等成功 —— UI 的「清除」不许依赖先探测");
            println!("check: 跨进程读回一致 → 删除 → 读不到 → 再删幂等，brief Step 4 四件事全过。");
            println!("跑 `status` 应当报干净。");
        }
        "cleanup" => match present.as_deref() {
            Some(v) if v == CANARY => {
                store.delete().expect("cleanup：删除失败");
                println!("cleanup: 已删掉本探针留下的 canary。");
            }
            Some(v) => {
                eprintln!(
                    "cleanup 拒绝：库里那条不是本探针写的（值长度 {} 字符），不碰。\
                     要删请到 Windows 凭据管理器里自己确认。",
                    v.chars().count()
                );
                std::process::exit(1);
            }
            None => println!("cleanup: 库里本来就没有这一条，无事可做。"),
        },
        other => {
            eprintln!(
                "未知相位 {other:?}。可用：status | arm | check | cleanup（见本文件头部注释）。"
            );
            std::process::exit(2);
        }
    }
}
