// Task 12：设置与凭据（spec 6.6）。`settings.json` 落在调用方给的目录里，
// api_key 落 Windows 凭据管理器，两者只有名字上的关系，没有代码上的关系。
// 本模块不得 `use tauri`（Plan A 约束，同 `store/mod.rs`）：app data 目录归 `ipc.rs` 决定。
//
// **`Settings` 里没有 `api_key` 字段，这是本文件最重要的一行设计。** spec :473 要求
// key「不写 settings 表、不进导出、不写日志」，三条里最容易失守的是第一条，而靠纪律
// （「记得别写」）守的规矩一定会被下一次加字段破掉；字段不存在才是结构性防线：
// serde 写不出一个不存在的字段，`{:?}` 打不出它，导出也就无从带上它。
// `serialized_field_names_are_exactly_the_eleven_settings_fields` 那条测试钉的正是这件事，
// 它同时也是 R89 的防线：加字段必须先给出它的消费者，否则 Plan A 里多一个没人读的键。
//
// 「不写日志」那一条还有第二道闸，而且不在结构体上：`SettingsError` 的 `Debug` 是**手写**的
// （R94，见 `impl fmt::Debug`）。`keyring_core::Error` 自己 derive 了 `Debug`，而它的
// `BadEncoding(Vec<u8>)` / `BadDataFormat(Vec<u8>, _)` 两个变体装的就是那串秘密字节 ——
// 一句 `.expect(..)` 或 `tracing::error!(?e)` 就能把 key 打成十进制转储，而 spec :543 计划的
// 「全文搜索 key 字符串」查不到那种转储。Display 侧本来就安全，所以每一支都借 Display 落地。
//
// 凭据侧的接缝（`SecretStore`，3 方法）之所以存在，是因为 `keyring` facade 不可用：
// 它的 `v1::Entry` 第一次被调用就把**进程级默认 store** 定死（crate 源码里的
// `LazyLock`），于是任何测试都会往用户真实的凭据管理器里写条目、把用户真 key 覆盖成
// 测试值（R80）。这里只走注入的 `Arc<CredentialStore>` + `build(..)`，
// 生产实现持真实的 Windows store，测试持 `keyring_core::mock::Store` —— 全仓
// `cargo test` 期间不创建也不修改任何真实凭据（R81 ③，cmdkey 前后各查一次为证）。
// **R81 到这里是没有例外的**：本模块不含任何碰真实库的测试。看一眼真实凭据管理器的那件事
// 写在 `examples/keyring_probe.rs`（人工 `cargo run --example`），形态照搬 `mcp_probe.rs`。
// 它原来确实是个 `#[ignore]` 测试，再审实测指出 `#[ignore]` 挡不住 `cargo test -- --ignored`
// 和 `--include-ignored` —— 两种跑法都会真的执行它，那时唯一在挡的是环境变量匹配。
// 会写进用户真实库、且写成功后留在进程之外的东西不该靠一道门挡着，于是搬出测试二进制。
// brief Step 4 要的证据由它提供，2026-10-20 实测跑通：写入 → 另一个进程读回 → 删除，
// `cmdkey /list` 前后逐字节相同。

use crate::agent::context::ContextConfig;
use crate::model::openai_compat::{ModelConfig, normalize_base_url};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use thiserror::Error;

/// 凭据条目的 service 段。**Windows 凭据管理器里显示的不是这个名字**：
/// Windows store 把条目名拼成 `{prefix}{user}{divider}{service}{suffix}`，默认为
/// `""`/`.`/`""`（crate 源码 store.rs:35-42 + cred.rs:53-56），所以实得
/// **`api_key.qwen-do`** —— user 在前。`the_real_windows_store_names_the_entry_user_then_service`
/// 那条测试钉的就是这个实测名字（R88 写的是 `qwen-do.api_key`，与 crate 源码相反，见任务报告）。
pub const KEYRING_SERVICE: &str = "qwen-do";
/// 凭据条目的 user 段。**名字里带 api_key 不代表它会进配置文件**：它只是 store 的键。
const KEYRING_USER: &str = "api_key";

const SETTINGS_FILE: &str = "settings.json";
/// 原子写的中转文件（R85）：与目标同目录，`rename` 才可能同卷原子。
///
/// 名字**固定**是刻意的，不要改成 `…tmp-<pid>-<n>` 那种唯一名去解决并发：
/// `save_routes_through_a_temp_file_before_replacing` 防的是「原子写退化回 `std::fs::write`」，
/// 它的做法是把这个名字抢先占成目录，让 `File::create` 必然失败 —— 只有真走了 temp 这条路
/// `save` 才会报错，直接覆盖目标文件的写法会照样「成功」。名字一唯一，这条测试就再也指不到
/// 要拦的东西（review M-3 提出的唯一名方案会顺手废掉 R85 唯一的哨兵）。
/// 并发写因此改用锁：见 `SettingsStore::state` 与 `save()` 开头那一行。
const TEMP_FILE: &str = "settings.json.tmp";

/// `budget_tokens` 不足以容纳 `reserve_output` 时补的余量（R86）。
/// 取 1024 的理由是实测出来的：`context.rs` 里 `budget = budget_tokens.saturating_sub(reserve_output)`，
/// 两者相等时预算塌成 0，`build()` 只剩最后一条消息 —— 用户看到的是「它记不住上文」，
/// 而不是一条报错。宁可悄悄多给 1024，也不要一个不报错却失忆的会话。
const BUDGET_HEADROOM: u32 = 1024;

/// `reserve_output` 的上限（R97）：留出 `BUDGET_HEADROOM` 的空间，好让 normalize 的后置条件
/// `budget_tokens > reserve_output` **无条件**成立。不夹这一道，`reserve_output = u32::MAX`
/// 时 `saturating_add` 只能把预算也顶到 `u32::MAX`，两者相等，`context.rs:117` 的
/// `saturating_sub` 照样把预算塌成 0 —— 那就是 `BUDGET_HEADROOM` 的注释说这段钳位要防的事，
/// 等于钳位函数在自己最难的那条边界上没兑现承诺。
const MAX_RESERVE_OUTPUT: u32 = u32::MAX - BUDGET_HEADROOM;

/// R97 的后置条件要同时喂两处：`clamp` 在 min > max 时会 panic，而 `reserve_output + headroom`
/// 也只有在夹过之后才不溢出。两条都只靠两个 `const` 之间的算术，没人检查 —— 所以这里编译期就查。
const _: () = assert!(MAX_RESERVE_OUTPUT >= 1);

/// `font_size` 的可读区间（R123，边界由 T15 定）。`applyTheme` 会把这个数原样写成 `<html>` 的
/// inline `--font-size`，于是 0 变成「看不见字」、`u32::MAX` 变成「一行一个字」，两者都不报错 ——
/// 和 `BUDGET_HEADROOM` 防的是同一类静默故障。钳在 `normalize()` 而不是 TS 里再钳一遍：
/// 这里是设置归一的唯一真相（R86），`settings_get`、落盘、下一次读取共用同一份边界。
const MIN_FONT_SIZE: u32 = 12;
const MAX_FONT_SIZE: u32 = 24;

/// 压缩与采样三个参数的默认值与可接受区间（Plan C T39，spec 5.3 的表）。
/// `DEFAULT_*` 与 `MIN_*`/`MAX_*` 分成两组常量而不是只写边界，是因为非有限值
/// （NaN、±inf，手改 `settings.json` 或前端漏了 `isFinite`）必须**回落默认**而不是钳到边界：
/// `f32::clamp` 对 NaN 返回 NaN，而 NaN 阈值让「该不该压」的判定永远为假且静默
/// （`set_calibration` 在 `context.rs:73-77` 为同样的事立过先例）。
const DEFAULT_TEMPERATURE: f32 = 0.7;
const MIN_TEMPERATURE: f32 = 0.0;
const MAX_TEMPERATURE: f32 = 2.0;
const DEFAULT_COMPACT_THRESHOLD: f32 = 0.85;
const MIN_COMPACT_THRESHOLD: f32 = 0.05;
/// 上界 0.95 = 至少留 5% 余量：阈值 1.0 意味着「永不压缩」，而压缩是两次对话之间的
/// 存储动作（前置事实 2），卡在 100% 上等于是把 400 留给用户去撞。
const MAX_COMPACT_THRESHOLD: f32 = 0.95;
const DEFAULT_KEEP_TURNS: u32 = 10;
/// 下限 1：K=0 会把最近一轮也压掉 ⇒ 当场失忆。
const MIN_KEEP_TURNS: u32 = 1;
const MAX_KEEP_TURNS: u32 = 50;

/// 设置与凭据的失败面（R82）。仓库约定是每模块一个枚举，不用 `Result<_, String>`：
/// T13 要能区分「没有条目」（显示「未配置」）与「读不到」（显示原因），
/// 后者被降级成前者会让用户重设 key、覆盖掉其实还在库里的那一条（R83/R71 同病）。
///
/// **`Debug` 是手写的，不是 derive 的**（R94）—— 见 `impl fmt::Debug` 上面的注释。
#[derive(Error)]
pub enum SettingsError {
    /// 配置目录不可写、文件被占用、或 `settings.json` 根本不是文件（review M-2 实测：
    /// 目录会走到这里，`read_to_string` 给 `PermissionDenied` / os error 5）。
    #[error("设置文件读写失败: {0}")]
    Io(#[from] std::io::Error),
    /// 只出现在 `save` 的序列化失败上。`load` 解不开文件时**不**返回这一支，
    /// 它备份原件后回落默认值（R84 与 `SettingsStore::load` 的文档才是权威 —— 本文件不写行号
    /// 引用：1400 行的文件里行号一碰就烂，review N-4 实测这条已经错过一次）。
    /// 解得开但不认识的字段走 `serde(default)`，同样不到这里（R91）。
    #[error("设置文件内容无法解析: {0}")]
    Json(#[from] serde_json::Error),
    /// 承载 `keyring_core::Error`（装箱后保留 `source()`，Display 原样带出上游文本，
    /// 含 `TooLong` 那句自带 platform limit 的数字）。
    #[error("凭据管理器操作失败: {0}")]
    Credential(#[from] Box<dyn std::error::Error + Send + Sync>),
    /// 非 Windows 目标（或 Windows store 初始化失败）给的 store 每次调用都走这支。
    /// 刻意**不**回落成内存存储：那会让用户以为 key 存住了，重启后才发现没有（R81）。
    #[error("当前平台没有凭据管理器")]
    UnsupportedPlatform,
}

/// R94：`thiserror` 的 `Debug` 与 `Display` 是两套实现，derive 出来的 `Debug` 会一路
/// 走进 `keyring_core::Error` 自己 derive 的 `Debug`，而它的 `BadEncoding(Vec<u8>)` /
/// `BadDataFormat(Vec<u8>, _)` 两个变体**装的就是那串秘密字节**（crate error.rs:44/50 实测）。
/// 于是 `store.api_key().expect(..)`、panic hook、`tracing::error!(?e)` 都会把 key 逐字节
/// 打成十进制转储 —— spec :473 说的「不写日志」在这种形态下守不住，而且 spec :543 计划的
/// 「全文搜索 key 字符串」根本查不到十进制转储。Display 侧本来就安全（那两个变体的 Display
/// 只说 "Password data is not valid UTF-8"），所以这里每一支都借 Display 落地。
/// `serialized_field_names_are_exactly_the_eleven_settings_fields` 同族的
/// `credential_debug_never_prints_the_secret_bytes` 钉住它。
impl std::fmt::Debug for SettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "Io({e})"),
            Self::Json(e) => write!(f, "Json({e})"),
            Self::Credential(e) => write!(f, "Credential(<凭据错误·只透出 Display>{e})"),
            Self::UnsupportedPlatform => f.write_str("UnsupportedPlatform"),
        }
    }
}

/// 凭据存取的本模块接缝（R81）。三件事，全是「这一条 key」的语义，
/// 故 service/user 由实现内部固定，不暴露给调用方去拼。
pub trait SecretStore: Send + Sync {
    /// 写入/覆盖条目。空串由 `SettingsStore` 先转成 `delete`，这里收到的必为非空。
    ///
    /// **这条接缝只服务「API key」这一种秘密**（N-5）：`SettingsStore` 在写入前 `trim`，
    /// 读回来也 `trim`。Windows 的 generic credential 完全可以合法地存一段首尾带空白的秘密
    /// （LM/NT 形态的 blob、以空格结尾的口令），那种秘密不许走这条接缝 —— 它会被每次保存
    /// 悄悄改写。API key 不带空白，所以这里的取舍是安全的，但它是个**取舍**，不是免费午餐。
    fn set(&self, key: &str) -> Result<(), SettingsError>;

    /// 读出条目。`Ok(None)` 是「真没配过」，`Err` 是「读不到」—— 两者绝不能合并。
    fn get(&self) -> Result<Option<String>, SettingsError>;

    /// 删除条目。条目本来就不存在算成功（`clear` 的语义是幂等的，UI 不必先探测）。
    fn delete(&self) -> Result<(), SettingsError>;
}

/// 平台不支持时用的 store：每次调用都报错，不装成能用的样子。
pub struct UnsupportedSecretStore;

impl SecretStore for UnsupportedSecretStore {
    fn set(&self, _key: &str) -> Result<(), SettingsError> {
        Err(SettingsError::UnsupportedPlatform)
    }
    fn get(&self) -> Result<Option<String>, SettingsError> {
        Err(SettingsError::UnsupportedPlatform)
    }
    fn delete(&self) -> Result<(), SettingsError> {
        Err(SettingsError::UnsupportedPlatform)
    }
}

/// 生产实现：持注入的 Windows 凭据管理器 store，用 `build()` 取条目（R80/R81）。
/// `build()` 本身**不写任何东西**，crate 源码 store.rs:108 明写「setting a password
/// does that」，所以下面 `entry()` 可以被只读测试安全调用。
#[cfg(windows)]
pub struct KeyringSecretStore {
    store: Arc<keyring_core::CredentialStore>,
}

#[cfg(windows)]
impl KeyringSecretStore {
    /// 用真实 Windows store 构造。`windows_native_keyring_store::Store::new()` 的
    /// `Err` 分支按 crate 源码不可达（`new_internal` 不校验平台可用性），**未实测**。
    pub fn new() -> Result<Self, SettingsError> {
        let store: Arc<keyring_core::CredentialStore> = windows_native_keyring_store::Store::new()
            .map_err(|e| SettingsError::Credential(Box::new(e)))?;
        Ok(Self { store })
    }

    fn entry(&self) -> Result<keyring_core::Entry, SettingsError> {
        // 只有这一条路：`Entry::new` / `set_default_store` 走的是进程级默认 store（R80）。
        self.store
            .build(KEYRING_SERVICE, KEYRING_USER, None)
            .map_err(credential_error)
    }
}

#[cfg(all(windows, test))]
impl KeyringSecretStore {
    /// 注入接缝用的构造器（测试把 `keyring_core::mock::Store` 递进来）。
    /// 只随测试构建，且刻意不 `pub`：T13 请用 `KeyringSecretStore::new()`，别把 mock 接进生产。
    fn from_store(store: Arc<keyring_core::CredentialStore>) -> Self {
        Self { store }
    }
}

#[cfg(windows)]
fn credential_error(e: keyring_core::Error) -> SettingsError {
    SettingsError::Credential(Box::new(e))
}

#[cfg(windows)]
impl SecretStore for KeyringSecretStore {
    fn set(&self, key: &str) -> Result<(), SettingsError> {
        self.entry()?.set_password(key).map_err(credential_error)
    }

    fn get(&self) -> Result<Option<String>, SettingsError> {
        match self.entry()?.get_password() {
            Ok(key) => Ok(Some(key)),
            // 唯一的「没有」信号。其余一律 Err 透出，别在这支之前加通配（R83）。
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(e) => Err(credential_error(e)),
        }
    }

    fn delete(&self) -> Result<(), SettingsError> {
        match self.entry()?.delete_credential() {
            Ok(()) => Ok(()),
            // crate 文档：没有匹配凭据时 delete 给 NoEntry。清除的幂等语义在这一层落地，
            // 免得 T13 的「清除」按钮先发一次 get 再发 delete（两次 TOCTOU）。
            Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(e) => Err(credential_error(e)),
        }
    }
}

/// 明暗档。序列化成 `"light"`/`"dark"` 是给 T14 的 TS 镜像和 `applyTheme` 用的字面量。
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Light,
    Dark,
}

/// 持久化的用户设置。**结构体级 `serde(default)` 是 R91 的读侧向前兼容**：
/// Plan C 会往同一份 `settings.json` 加字段，旧程序读新文件必须「忽略未知字段、保住其余设置」，
/// 而不是整份回落默认（那会连带抹掉 base_url/model）。不加 `deny_unknown_fields` 同理。
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// 用户粘贴的那一行端点。空串是合法值（还没配），由 T13 的守卫去拦（R90）。
    pub base_url: String,
    pub model: String,
    pub reserve_output: u32,
    pub budget_tokens: u32,
    pub window_messages: usize,
    pub theme: String,
    pub mode: Mode,
    pub font_size: u32,
    /// 采样温度，进 `ChatRequest.temperature`（`openai_compat.rs` 的 build_request 早已
    /// 把它写进请求体，缺的只是这个键）。夹 `0.0..=2.0`：兼容端点对 >2 普遍报错。
    pub temperature: f32,
    /// 压缩触发线，**占预算的比例**（消费者在 T41）。夹 `0.05..=0.95`。
    pub compact_threshold: f32,
    /// 压缩时保留原文的最近轮数 K。夹 `1..=50`；K=0 会让刚说完的那轮也被摘要掉。
    pub keep_turns: u32,
}

impl Default for Settings {
    /// 数值默认与 spec 5.3 / `ContextConfig::default()` 逐字相同（那两个 4096/32000/40
    /// 在这里再抄一遍是**有意的**：抄一份并让测试盯着，比让 settings 反向依赖 builder 内部好）。
    fn default() -> Self {
        Self {
            base_url: String::new(),
            model: "qwen-plus".into(),
            reserve_output: 4096,
            budget_tokens: 32_000,
            window_messages: 40,
            theme: "noir".into(),
            mode: Mode::Dark,
            font_size: 15,
            temperature: DEFAULT_TEMPERATURE,
            compact_threshold: DEFAULT_COMPACT_THRESHOLD,
            keep_turns: DEFAULT_KEEP_TURNS,
        }
    }
}

/// f32 设置项的钳位：**非有限值（NaN、±inf）回落默认，而不是钳到边界**。
/// `f32::clamp` 对 NaN 返回 NaN（min/max 都被跳过），于是 NaN 阈值会一路活到
/// T41 那个「该不该压缩」的判定上 —— 任何与 NaN 的比较都是 false，于是压缩静默地
/// 永不发生，正是 `compact_threshold` 最坏的失效形态。`±inf` 也一样：手改的
/// `settings.json` 里写 `1e40` 解出来就是 inf，钳到 0.95 与回落默认哪个更能用？
/// 后者 —— 一个从默认值出发的阈值至少还是个说得清的数。
/// 先例同 `ContextBuilder::set_calibration`（`agent/context.rs` 的 `is_finite` 那道闸）。
fn clamp_finite(value: f32, fallback: f32, min: f32, max: f32) -> f32 {
    if value.is_finite() {
        value.clamp(min, max)
    } else {
        fallback
    }
}

impl Settings {
    /// 钳位与归一的**唯一**定义处（R86）。`load` 之后、`save` 之前各调一次，
    /// 两个派生方法也走它 —— 于是「base_url 谁负责归一」这个问题永远只有一个答案。
    /// 全部只调不报错：越界值的正确处置是纠正到能用的最近值，静默纠正比拒绝打开设置划算。
    pub fn normalize(&mut self) {
        // R90：`normalize_base_url("")` 提前返回空串，这里因此**不会**给空 base_url 补默认端点。
        self.base_url = normalize_base_url(&self.base_url);
        if self.window_messages < 1 {
            self.window_messages = 1;
        }
        // R97：下限 1、上限 `u32::MAX - BUDGET_HEADROOM`（`clamp` 要求 max >= min，
        // 这里 max = 42 亿 − 1024，恒成立）。夹上限是为了让下面那句加法不会饱和成
        // 「budget == reserve」，而不是为了好看。
        self.reserve_output = self.reserve_output.clamp(1, MAX_RESERVE_OUTPUT);
        if self.budget_tokens <= self.reserve_output {
            self.budget_tokens = self.reserve_output + BUDGET_HEADROOM;
        }
        // R123（边界由 T15 定，这里是唯一钳位处）：`applyTheme` 原样把它写成 inline `--font-size`，
        // 0 → 看不见字、`u32::MAX` → 一行一个字，都不报错。TS 侧不再钳第二遍，免得两处边界漂移。
        self.font_size = self.font_size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
        // Plan C T39：压缩与采样的三个参数走**同一道闸**，而且和上面那些同样只调不报错。
        // `compact_threshold` 的两端都比字面上更危险：1.5 是「永不压缩」（长对话必然失忆，
        // 而这正是本计划要修的东西），0 以下是「每轮都压」。
        // `keep_turns` 的下界是 1 而不是 0：K=0 连刚说完的那轮都被摘要掉。
        // `temperature` 的上界 2.0 是替用户挡端点的 400 —— 兼容实现对 >2 普遍报错，
        // 而报错的是用户的锅、不是设置界面的。
        self.compact_threshold = clamp_finite(
            self.compact_threshold,
            DEFAULT_COMPACT_THRESHOLD,
            MIN_COMPACT_THRESHOLD,
            MAX_COMPACT_THRESHOLD,
        );
        self.keep_turns = self.keep_turns.clamp(MIN_KEEP_TURNS, MAX_KEEP_TURNS);
        self.temperature = clamp_finite(
            self.temperature,
            DEFAULT_TEMPERATURE,
            MIN_TEMPERATURE,
            MAX_TEMPERATURE,
        );
    }

    /// 归一后的副本。派生方法用它，省掉「调用者忘了先 normalize」这条静默错误路径。
    /// `pub`（review minor）：T13 的设置面板要把「表单当前的值」显示出来，那需要一个归一后的
    /// 视图而不能就地把表单状态改掉 —— 没有它就只能走一次磁盘往返。
    pub fn normalized(&self) -> Settings {
        let mut c = self.clone();
        c.normalize();
        c
    }

    /// R86 补的派生入口。brief 只给了 `to_model_config`，漏了它，T13（plan:2946）
    /// 就会手抄一遍 reserve/budget/window 三行映射 —— 那是第二份真相。
    pub fn to_context_config(&self) -> ContextConfig {
        let c = self.normalized();
        ContextConfig {
            reserve_output: c.reserve_output,
            budget_tokens: c.budget_tokens,
            window_messages: c.window_messages,
            // T39：压缩的两个参数也必须经这里，否则 `settings.json` 里的键就成了
            // 只有界面能看见的装饰品（T41 读的是 `ContextConfig`，不是 `Settings`）。
            compact_threshold: c.compact_threshold,
            keep_turns: c.keep_turns as usize,
        }
    }

    /// key 由调用方显式传进来（不从磁盘猜），这样「谁拿到了明文 key」在签名上一目了然。
    pub fn to_model_config(&self, api_key: &str) -> ModelConfig {
        let c = self.normalized();
        ModelConfig {
            base_url: c.base_url,
            model: c.model,
            api_key: api_key.to_string(),
            max_output_tokens: c.reserve_output,
            // `Settings.temperature` 是 f32（有默认值、已被钳进 0..=2），`ModelConfig` 里
            // 却是 `Option<f32>`：`None` 在协议上意味着「这个字段不上线，由端点自己定」，
            // 而设置面板给出的永远是一个具体的数。见 `ChatRequest.temperature` 那一条线。
            temperature: Some(c.temperature),
        }
    }
}

/// 只用于界面显示的前 4 + 后 4（R83 的三条规则按 brief 原样）。
/// 按**字符**取而不是字节：key 里出现非 ASCII（用户手抖粘进全角字符）时按字节切会踩边界 panic。
pub fn mask_key(key: &str) -> String {
    if key.is_empty() {
        return "未配置".into();
    }
    let n = key.chars().count();
    if n <= 8 {
        return "***".into();
    }
    let head: String = key.chars().take(4).collect();
    let tail: String = key.chars().skip(n - 4).collect();
    format!("{head}…{tail}")
}

/// 备份名后缀用毫秒而不是秒：用户在**一分钟内连续改坏两次**（反复手改文件、改完没存盘又改）
/// 是常规形态，秒级粒度会让第二份证据盖掉第一份。
///
/// 注意这里**只有**「同名覆盖」这一条路要防：`preserve_corrupt_file` 用 `rename` 而不是 copy，
/// 所以真有两个进程同时撞上坏文件时，输的那一次 `rename` 会因为源已经不在了而失败（走 `warn`
/// 分支），并不会互相覆盖 —— 早期注释把它算成需要毫秒的理由，是**错的**（review 实测更正）。
fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// `SettingsStore` 里需要跨调用共享、又必须串行访问的那一点状态（R92/R93）。
#[derive(Default)]
struct Shared {
    /// 最近一次 `load()` 因「解不开」回落默认值时保留下来的备份路径。
    /// 只有一份：第二次损坏会盖掉第一次的**指针**（文件本身还在磁盘上，名字带毫秒不撞）。
    corrupt_backup: Option<PathBuf>,
}

/// `Settings` 的落盘位置 + 凭据接缝。目录由调用方给（Plan A：核心层不碰 Tauri path API）。
pub struct SettingsStore {
    dir: PathBuf,
    secrets: Arc<dyn SecretStore>,
    /// 一把锁两个用途（R93 补的字段，review M-3/M-4）：
    /// ① 把 `save()` 整个串行化。temp 名是**固定**的（`TEMP_FILE` 的注释解释为什么不改成唯一名），
    ///   而 T13 计划用 `spawn_blocking` 调本方法 —— 两个线程在同一路径上会互相踩：A `create`→写→
    ///   `rename` 之后，B 的句柄已经指向被改名的 `settings.json`，B 再 `rename` 时报 `NotFound`，
    ///   UI 于是收到一个平白无故的失败。共享的 state 对象挡不住这件事：`Send + Sync` 类型的
    ///   `&self` 方法本来就可以被两个 blocking 线程同时调。
    /// ② 记下最近一次损坏回落留下的备份名，让 T13 有机会告诉用户「你的设置文件坏了、已备份为 X」，
    ///   而不是静默地看到一份默认值（R83 反对的正是这种静默）。
    /// 中毒不致命：这里没有跨调用的不变量，拿不到就 `into_inner`（同 `db.rs:100` 的处置）。
    ///
    /// **一个目录只许存在一个 `SettingsStore`**（N-3）：这把锁是**实例内**的，两个各自 `new` 在
    /// 同一个目录上的实例会同时去写同一个固定名的 temp —— 审阅者实测 6 线程 × 8 次、两个实例，
    /// 48 次 save 里 20~30 次报 `Io(os error 2)`，三次运行都能复现。
    /// T13 必须用 `app.manage()` 装一个实例再到处共享，绝不许在每条命令里现构一个。
    state: Mutex<Shared>,
}

/// Tauri 的 `manage()` 只收 `Send + Sync`，而 T13 会在 `spawn_blocking` 里用它（同 db.rs）。
/// 闭包永不执行，只做类型检查：谁把接缝换成 `RefCell` 之类，这里立刻编译失败。
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SettingsStore>();
};

impl SettingsStore {
    /// 生产构造：Windows 上接真实的凭据管理器，其余平台接 `UnsupportedSecretStore`。
    /// 这里不返回 `Result` 是因为文件侧对任何平台都可用；凭据侧的「不能用」由每次调用
    /// 自己报（R81），而不是让构造期就把设置页整个堵死。
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
            secrets: default_secret_store(),
            state: Mutex::new(Shared::default()),
        }
    }

    /// 接缝（R81）：测试注入 mock/假 store 的唯一入口。
    pub fn with_secrets(dir: &Path, secrets: Arc<dyn SecretStore>) -> Self {
        Self {
            dir: dir.to_path_buf(),
            secrets,
            state: Mutex::new(Shared::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        crate::lock::guard(&self.state)
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(SETTINGS_FILE)
    }

    /// 读设置。四条分支各有各的道理：
    /// 1. 文件不存在 = 还没设置过，给默认值（这不是错误，`NotFound` 一支把「目录都没建」
    ///    也算进来，首次启动就是这个形态）。`first_run_reads_defaults_without_a_file` 钉它。
    /// 2. 读都读不到（权限、`settings.json` 是个目录、被别的进程独占）= **报 `Io`，不回落**。
    ///    回落默认值等于「一遇到读不动就替用户把设置清掉」，正是 R84 反对的静默，只是下沉了一层。
    ///    `an_unreadable_settings_file_reports_io_instead_of_defaults` 钉它。
    /// 3. 解不开 = 损坏。回落默认值让应用起得来（spec 要它起得来），但**先把原件留成
    ///    `settings.json.corrupt-<ms>`**（R84）—— 否则紧接着那次 `save()` 会把用户全部
    ///    设置静默抹掉，连查都没得查。备份本身失败也只是 `warn`，不 panic。
    ///    备份名会存进 `Shared::corrupt_backup`，由 `take_corrupt_backup()` 交给 T13（R93）。
    /// 4. 解得开 = 归一后再返回（R86），于是钳位规则在读写两侧各只有一处定义。
    ///
    /// `Json` 变体因此**不会**从 `load` 返回（损坏走回落），它只出现在 `save` 的序列化失败上。
    pub fn load(&self) -> Result<Settings, SettingsError> {
        let path = self.path();
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Settings::default()),
            Err(e) => return Err(e.into()),
        };
        let mut settings: Settings = match serde_json::from_str(&raw) {
            Ok(s) => s,
            Err(e) => {
                if let Some(backup) = preserve_corrupt_file(&path, &e.to_string()) {
                    self.lock().corrupt_backup = Some(backup);
                }
                return Ok(Settings::default());
            }
        };
        settings.normalize();
        // N-2：**只有真的读通一份好文件才作废通知**。反过来说不能在这一支之外清它 ——
        // 损坏回落刚刚把原件 `rename` 走了，紧接着的第二次 `load()` 命中的是 `NotFound`，
        // 那是同一次事故的延续而不是新事故；在那里清空会让 T13 只要多读一次就永远看不到提示。
        // `Err(Io)` 那一支同样不清：读都读不到，没有资格宣布问题已经过去了。
        // 三条分支各有测试钉住（`a_healthy_load_expires_the_pending_corrupt_notice` /
        // `a_missing_file_keeps_the_notice_alive` / `an_unreadable_file_keeps_the_notice_alive_too`）：
        // 后两条是再审补的 —— 那两句当时只是文档，变异在全绿下存活。
        self.lock().corrupt_backup = None;
        Ok(settings)
    }

    /// 取走「最近一次成功的 `load()` 是否发生了损坏回落」留下的备份路径，取一次就清（R93）。
    /// `T13` 需要在设置页第一屏告诉用户这件事；没有这个入口，损坏回落对外和「本来就没配置」
    /// 长得一模一样 —— 用户会以为设置被谁改了，而证据只躺在一条当前没有 subscriber 的 `warn!` 里。
    ///
    /// 生命周期（N-2）：这份通知描述的是**最近一次读盘的结果**。下一次 `load()` 如果读通了一份
    /// 好文件，通知自动作废（文件已经修好了，继续报就是骗人）；如果那次是 `NotFound` 或 `Io` 错误，
    /// 通知留着 —— 前者通常就是「坏文件刚被 rename 走」的同一现场。
    pub fn take_corrupt_backup(&self) -> Option<PathBuf> {
        self.lock().corrupt_backup.take()
    }

    /// 写设置（R85 的原子写）：同目录 temp → `write_all` → `sync_all` → `rename`。
    /// `std::fs::write` 是「截断 + 写」，进程在写的中途被杀（或断电）就留下一份半文件，
    /// 下一次 `load()` 只能按损坏处理 —— R84 让损坏可查，防不住它，所以这里防。
    /// Windows 上 `std::fs::rename` 带 `REPLACE_EXISTING`，同卷（temp 就在目标目录里）是原子的。
    ///
    /// 整个方法在锁里（R92，review M-3）：temp 名字固定，两个线程同时写就会互相踩。
    /// T13 用 `spawn_blocking`，所以「同时写」是设计形态而不是罕见形态。
    pub fn save(&self, settings: &Settings) -> Result<(), SettingsError> {
        let _serial = self.lock();
        std::fs::create_dir_all(&self.dir)?;
        let json = serde_json::to_string_pretty(&settings.normalized())?;
        let temp = self.dir.join(TEMP_FILE);
        {
            let mut file = std::fs::File::create(&temp)?;
            file.write_all(json.as_bytes())?;
            file.sync_all()?;
        }
        std::fs::rename(&temp, self.path())?;
        Ok(())
    }

    /// 空串按「清除」处理：UI 的保存按钮把空输入原样送过来是常规路径，
    /// 而往凭据管理器里存一条空 key 只会让掩码显示「未配置」、库里却真有个条目（比没有更糟）。
    ///
    /// 先 `trim`（R95，review minor）：从聊天窗口/邮件里粘 key 常带尾部 `\r\n`，
    /// 原样存进去换来的是 API 侧一个 401，而设置页上一切看起来都「已配置」—— 没有任何线索
    /// 指向真正的原因。`normalize_base_url` 已经在钳位时 trim 了，key 这边不 trim 是不一致。
    /// 纯空白 trim 完就是空串，于是自然走清除那一支，与「空串按清除处理」原本的意图一致。
    pub fn set_api_key(&self, key: &str) -> Result<(), SettingsError> {
        let key = key.trim();
        if key.is_empty() {
            return self.clear_api_key();
        }
        self.secrets.set(key)
    }

    /// 明文 key。只给 T13 组装 `ModelConfig` 用，别存进任何结构体字段或日志（spec :473）。
    ///
    /// 读侧也 `trim`（N-5 的另一半）：`set_api_key` 写进去的已经是干净的，但库里那条可能是
    /// **别的程序**、或 `ed0fc2e` 之前那次构建写的（那时还没有 trim）。那种条目会带着尾部
    /// `\r\n` 一路走到 HTTP 头，换来一个 401，而设置页上掩码看着完全正常。
    /// 读写两侧不对称才是问题，所以这一支也夹。
    pub fn api_key(&self) -> Result<Option<String>, SettingsError> {
        Ok(self.secrets.get()?.map(|k| k.trim().to_string()))
    }

    /// R83 覆盖 brief 的 `-> String`：`Ok(None)` 才是「未配置」，`Err` 是「读不到」，
    /// 两者在界面上必须长得不一样（brief 的 `.ok().flatten()` 会把后者显示成「未配置」，
    /// 用户于是重设 key、覆盖掉其实还在库里的那一条）。
    ///
    /// 库里躺着一条空白条目时也给 `Ok(None)`（R95 的另一半）：`mask_key("")` 返回的是
    /// 「未配置」这三个字，于是 `Ok(Some("未配置"))` 和 `Ok(None)` 在界面上又会长得一样，
    /// 把 R83 好不容易分开的两件事重新合起来。空白条目不是本模块写进去的（`set_api_key`
    /// 已经把它转成清除），但别的程序可以。
    pub fn api_key_masked(&self) -> Result<Option<String>, SettingsError> {
        Ok(self
            .api_key()?
            .filter(|k| !k.trim().is_empty())
            .as_deref()
            .map(mask_key))
    }

    pub fn clear_api_key(&self) -> Result<(), SettingsError> {
        self.secrets.delete()
    }
}

/// R84 的取证动作。只 `warn`，永不 `Err`：备份失败也不能让应用起不来。
/// 成功时把备份路径交回调用方（R93），失败时给 `None`。
/// （`tracing` 在本仓没有 subscriber，而本模块是 src 里第一个也是唯一一个使用者
/// —— 这几条 `warn!` 目前只在测试里可见，subscriber 与「日志落 app data 文件」归 T13，见 R87。）
fn preserve_corrupt_file(path: &Path, reason: &str) -> Option<PathBuf> {
    let backup = format!("{}.corrupt-{}", path.display(), now_ms());
    // rename 而不是 copy：证据挪走、原路径腾干净，下一次 `save()` 才不会把坏文件当成
    // 「已修复的旧配置」盖掉；而且同目录 rename 是原子的，不会留下两份坏文件并存。
    match std::fs::rename(path, &backup) {
        Ok(()) => {
            tracing::warn!(
                "settings.json 无法解析（{reason}），原件已保留为 {backup}，本次回落默认值"
            );
            Some(PathBuf::from(backup))
        }
        Err(e) => {
            // 早期这条写的是「原件仍在 {backup} 该处未生成」：把备份名当成了原件的位置，
            // 读日志的人会去翻一个根本不存在的文件（review minor，实测措辞已纠正）。
            tracing::warn!(
                "settings.json 无法解析（{reason}），备份也失败了（{e}）；原件仍在 {}，备份 {backup} 未生成",
                path.display()
            );
            None
        }
    }
}

/// 平台默认的凭据 store。
fn default_secret_store() -> Arc<dyn SecretStore> {
    #[cfg(windows)]
    {
        match KeyringSecretStore::new() {
            Ok(store) => Arc::new(store),
            Err(e) => {
                // 理论不可达（`Store::new()` 就是 `Ok(Self::new_internal(..))`，crate store.rs:35-40
                // 实测；review 也确认了），真到达了也只报「用不了」，绝不换成内存 store。
                // 已知瑕疵（**本轮不改**，理由见任务报告）：这条支给用户的话是
                // 「当前平台没有凭据管理器」，而在 Windows 上初始化失败时说「平台不支持」并不准确 ——
                // 原始原因只进了上面那条 `warn!`。因为这支不可达，为它新增一个错误变体
                // 属于给不会发生的情形加防线（仓库纪律：边界之外不防御）。等它真 reachable 再说。
                tracing::warn!("Windows 凭据管理器初始化失败，凭据操作将一律报错: {e}");
                Arc::new(UnsupportedSecretStore)
            }
        }
    }
    #[cfg(not(windows))]
    {
        Arc::new(UnsupportedSecretStore)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Arc;

    #[cfg(windows)]
    use keyring_core::api::CredentialStoreApi;
    #[cfg(windows)]
    use keyring_core::{CredentialStore, mock};

    /// 目录里的文件名（排序后）。R84/R85 三条测试都靠它盯「该留的留、不该留的别留」。
    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .collect();
        v.sort();
        v
    }

    fn corrupt_backups(dir: &Path) -> Vec<String> {
        names(dir)
            .into_iter()
            .filter(|n| n.starts_with("settings.json.corrupt-"))
            .collect()
    }

    /// spec :543 的测法：把配置目录里**每个文件**的全文拼起来，搜 key 应当 0 命中。
    fn dir_text(dir: &Path) -> String {
        let mut out = String::new();
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            if let Ok(s) = std::fs::read_to_string(e.path()) {
                out.push_str(&s);
            }
        }
        out
    }

    /// R81 ②：一条永远失败的 store。它存在的唯一目的就是把「错误被吞掉」写死成失败。
    fn broken_err() -> SettingsError {
        SettingsError::Credential(Box::new(std::io::Error::other(
            "凭据管理器读不到（测试注入）",
        )))
    }

    struct BrokenStore;

    impl SecretStore for BrokenStore {
        fn set(&self, _key: &str) -> Result<(), SettingsError> {
            Err(broken_err())
        }
        fn get(&self) -> Result<Option<String>, SettingsError> {
            Err(broken_err())
        }
        fn delete(&self) -> Result<(), SettingsError> {
            Err(broken_err())
        }
    }

    /// R81 ①：生产实现（`KeyringSecretStore`，走 `store.build(..)` 那条注入路径）
    /// 底下换成 keyring-core 自带的 mock store —— 全程不落真实凭据管理器。
    #[cfg(windows)]
    fn mock_backed(dir: &Path) -> (SettingsStore, Arc<mock::Store>) {
        let m = mock::Store::new().unwrap();
        let injected: Arc<CredentialStore> = m.clone();
        let store =
            SettingsStore::with_secrets(dir, Arc::new(KeyringSecretStore::from_store(injected)));
        (store, m)
    }

    // ---- brief Step 1 ----

    #[test]
    fn defaults_match_spec_numbers() {
        let s = Settings::default();
        assert_eq!(s.reserve_output, 4096);
        assert_eq!(s.budget_tokens, 32_000);
        assert_eq!(s.window_messages, 40);
        assert_eq!(s.mode, Mode::Dark);
        // T39 的三个新键同样得在这里钉住：它们没有别的默认值锚点，
        // 而漏进 `Default` 的字段会被所有 `..Default::default()` 夹具静默读成 0。
        assert_eq!(s.temperature, 0.7);
        assert_eq!(s.compact_threshold, 0.85);
        assert_eq!(s.keep_turns, 10);
    }

    #[test]
    fn masked_key_never_contains_more_than_prefix_and_suffix() {
        assert_eq!(mask_key("sk-abcdefghijklmnop"), "sk-a…mnop");
        assert_eq!(mask_key("short"), "***");
        assert_eq!(mask_key(""), "未配置");
    }

    #[test]
    fn saved_settings_survive_reload_and_do_not_contain_key() {
        let d = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(d.path());
        let s = Settings {
            base_url: "https://x.com/v1".into(),
            model: "qwen-plus".into(),
            ..Default::default()
        };
        store.save(&s).unwrap();
        let raw = std::fs::read_to_string(store.path()).unwrap();
        assert!(
            !raw.contains("api_key"),
            "配置文件里绝不能出现 api_key 字段"
        );
        let got = store.load().unwrap();
        assert_eq!(got.base_url, "https://x.com/v1");
        assert_eq!(got.model, "qwen-plus");
    }

    #[test]
    fn corrupt_file_falls_back_to_defaults_without_crash() {
        let d = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(d.path());
        std::fs::write(store.path(), "{ not json").unwrap();
        let got = store.load().unwrap();
        assert_eq!(got, Settings::default());
    }

    #[test]
    fn base_url_is_normalized_on_save() {
        let d = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(d.path());
        let s = Settings {
            base_url: "https://x.com/v1/".into(),
            ..Default::default()
        };
        store.save(&s).unwrap();
        assert_eq!(store.load().unwrap().base_url, "https://x.com/v1");
    }

    #[test]
    fn model_config_carries_normalized_base_and_explicit_key() {
        let s = Settings {
            base_url: "https://x.com".into(),
            ..Default::default()
        };
        let mc = s.to_model_config("k");
        assert_eq!(mc.base_url, "https://x.com/v1");
        assert_eq!(mc.api_key, "k");
    }

    // ---- R84：损坏必须留证据 ----

    #[test]
    fn corrupt_load_backs_the_original_file_up_before_falling_back() {
        let d = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(d.path());
        std::fs::write(store.path(), "{ not json").unwrap();
        let got = store.load().unwrap();
        assert_eq!(got, Settings::default());
        let backups = corrupt_backups(d.path());
        assert_eq!(
            backups.len(),
            1,
            "损坏文件必须被保留成一份备份，实测目录：{:?}",
            names(d.path())
        );
        // 名字带毫秒，免得同一次会话里第二次损坏覆盖掉第一份证据
        let rest = backups[0].strip_prefix("settings.json.corrupt-").unwrap();
        assert!(
            rest.chars().all(|c| c.is_ascii_digit()) && !rest.is_empty(),
            "备份名后缀应是毫秒：{rest}"
        );
        assert_eq!(
            std::fs::read_to_string(d.path().join(&backups[0])).unwrap(),
            "{ not json"
        );
    }

    #[test]
    fn a_blocked_backup_still_returns_defaults_without_losing_the_bytes() {
        let d = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(d.path());
        std::fs::write(store.path(), "{ not json").unwrap();
        // 先记录一件实测事实：**空着句柄挡不住这次 rename** —— 实测在拿着 `File::open`
        // 句柄的情况下备份照样成功（std 打开文件时带 FILE_SHARE_DELETE），所以「备份失败」
        // 那一支不能靠拿句柄构造出来。这里改成把备份的目标名抢先占成一个目录：
        // `MoveFileExW` 落到目录上必然失败，于是走到 warn 分支。
        // 毫秒数字对不上时备份会照样成功，所以断言只兜住两支都必然成立的三件事：
        // 不 panic、给默认值、坏字节在目录里仍然找得着。
        // 实测命中率：连跑 10 次，5 次落到「备份失败」分支（settings.json 仍原样在），
        // 5 次毫秒错开、备份成功 —— 也就是说 warn 那一支确实被执行过，但不是每次。
        let blocker = d.path().join(format!("settings.json.corrupt-{}", now_ms()));
        std::fs::create_dir(&blocker).unwrap();
        let got = store.load().unwrap();
        assert_eq!(got, Settings::default());
        assert!(
            dir_text(d.path()).contains("{ not json"),
            "备份失败也不许把用户的证据吃掉，实测目录：{:?}",
            names(d.path())
        );
    }

    // ---- review M-2：load 的另外两条 IO 分支 ----
    //
    // 这两条在 Task 12 第一轮交付里**缺席**：审阅者的变异 MA（把 `NotFound` 改成报错）和
    // MB（把所有 IO 错误吞成默认值）都在 34/34 全绿下存活。缺的不是「覆盖率数字」，
    // 是两条真实存在的语义 —— 首次启动给默认值、读不动时报错 —— 现在没有任何东西钉住它们。

    #[test]
    fn first_run_reads_defaults_without_a_file() {
        let d = tempfile::tempdir().unwrap();
        // 连文件都没有（目录是空的）：这是每个用户第一次打开应用的形态，不是异常。
        assert_eq!(names(d.path()), Vec::<String>::new());
        let s = d.path().join("settings.json");
        assert!(!s.exists(), "前提：文件不存在");
        assert_eq!(
            SettingsStore::new(d.path()).load().unwrap(),
            Settings::default(),
            "`NotFound` 必须走默认值，不是报错"
        );
        // 目录本身也不存在 —— Tauri 首次启动时 app data 目录还没建，同一个形态。
        let absent = d.path().join("app data").join("v1");
        assert_eq!(
            SettingsStore::new(&absent).load().unwrap(),
            Settings::default()
        );
    }

    #[test]
    fn an_unreadable_settings_file_reports_io_instead_of_defaults() {
        let d = tempfile::tempdir().unwrap();
        // `settings.json` 位置上是个**目录**。Windows 实测（控制器与审阅者各跑过一次）：
        // `read_to_string` 给 `PermissionDenied` / os error 5，既不是 `NotFound` 也不是解码失败。
        // 这一支必须报错：把它吞成默认值 = 「一遇到读不动就替用户把设置清掉」，
        // 正是 R84 反对的那种静默，只是从「解不开」下沉到了「读不到」。
        std::fs::create_dir(d.path().join("settings.json")).unwrap();
        let store = SettingsStore::new(d.path());
        let err = store
            .load()
            .expect_err("读不到必须报 Err，不许降级成默认值");
        assert!(
            matches!(err, SettingsError::Io(_)),
            "应当是 Io，实测：{err}"
        );
        assert!(err.to_string().contains("设置文件读写失败"), "{err}");
        assert!(
            corrupt_backups(d.path()).is_empty(),
            "读不到 ≠ 损坏，不该产生 .corrupt- 备份"
        );
    }

    // ---- review M-4：损坏回落要让调用方看得见 ----

    #[test]
    fn take_corrupt_backup_reports_the_fallback_and_is_consumed_once() {
        let d = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(d.path());
        assert_eq!(
            store.take_corrupt_backup(),
            None,
            "没坏过就没有备份可报，别给 T13 一个假的"
        );
        std::fs::write(store.path(), "{ not json").unwrap();
        assert_eq!(store.load().unwrap(), Settings::default());
        let reported = store
            .take_corrupt_backup()
            .expect("损坏回落必须留下可出示的证据路径（R93）");
        assert!(
            reported.exists(),
            "报出来的路径得真能打开，实测：{}",
            reported.display()
        );
        assert_eq!(
            reported.file_name().unwrap().to_str().unwrap(),
            corrupt_backups(d.path())[0],
            "报出来的名字要和磁盘上那个对得上"
        );
        assert_eq!(
            store.take_corrupt_backup(),
            None,
            "同一次回落只许提示一次，否则每次重进设置页都会重复报错"
        );
    }

    #[test]
    fn a_healthy_load_expires_the_pending_corrupt_notice() {
        // N-2 的现场：通知**立着没人取**，然后用户把文件修好了。
        // 这时候再把它报出去，就是指着一个月前的备份说「你的设置坏了」。
        // （修复轮第一版把这段写成「再写坏、再删掉、再 load」，测的其实是 NotFound 分支，
        //   和它自己注释承诺的「坏→修好」不是一件事 —— review N-2 实测戳穿的正是那个。）
        let d = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(d.path());
        std::fs::write(store.path(), "{ not json").unwrap();
        assert_eq!(store.load().unwrap(), Settings::default());
        std::fs::write(store.path(), r#"{"model":"qwen-max"}"#).unwrap();
        assert_eq!(
            store.load().unwrap().model,
            "qwen-max",
            "前提：这次读通了好文件"
        );
        assert_eq!(
            store.take_corrupt_backup(),
            None,
            "读通好文件必须作废那份还没取走的通知"
        );
    }

    #[test]
    fn a_missing_file_keeps_the_notice_alive() {
        // 损坏回落刚刚把原件 `rename` 走，于是紧接着的第二次 `load()` 命中的是 `NotFound`。
        // 那是**同一次事故的延续**，不是新事故：在这里也清空的话，T13 只要 startup 时多读一次
        // 就永远看不到提示（`load` 与渲染之间隔一次刷新是常规形态）。
        let d = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(d.path());
        std::fs::write(store.path(), "{ not json").unwrap();
        store.load().unwrap();
        assert!(
            !store.path().exists(),
            "前提：原件已被 rename 走，下一次读必然 NotFound"
        );
        assert_eq!(store.load().unwrap(), Settings::default());
        assert!(
            store.take_corrupt_backup().is_some(),
            "`NotFound` 一支不许清通知"
        );
    }

    #[test]
    fn an_unreadable_file_keeps_the_notice_alive_too() {
        // 通知生命周期第三条腿。`SettingsStore::load` 的文档明写「`Err(Io)` 那一支同样不清」，
        // 但这句话此前只是文档 —— 变异（在 Io 支也清一次通知）能在 44 条全绿下存活。
        // 库里已经立着一次回落的证据时，一次读不动不该把证据抹掉：文件坏过、现在又读不到，
        // 是两个各自要报的问题，不是后一个把前一个抵消了。
        let d = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(d.path());
        std::fs::write(store.path(), "{ not json").unwrap();
        store.load().unwrap();
        // 回落已把原件 rename 走，于是这个位置现在是空的 —— 换成一个目录，
        // 下一次读就会走 `Err(Io)`（`read_to_string` 读目录给 PermissionDenied / os error 5）。
        std::fs::create_dir(store.path()).unwrap();
        assert!(
            matches!(store.load(), Err(SettingsError::Io(_))),
            "前提：这里必须真的走到 `Io` 那支，否则本条什么都没测"
        );
        assert!(
            store.take_corrupt_backup().is_some(),
            "`Io` 错误没有资格宣布问题已经过去，通知得留着"
        );
    }

    // ---- R85：save 原子 ----

    #[test]
    fn save_leaves_no_temp_file_behind_and_reads_back_exactly() {
        let d = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(d.path());
        let s = Settings {
            model: "qwen-max".into(),
            theme: "ivory".into(),
            ..Default::default()
        };
        store.save(&s).unwrap();
        assert_eq!(
            names(d.path()),
            vec!["settings.json".to_string()],
            "R85：不得留下 .tmp 残留"
        );
        let got = store.load().unwrap();
        assert_eq!(got, s);
        store.save(&s).unwrap();
        assert_eq!(names(d.path()), vec!["settings.json".to_string()]);
    }

    #[test]
    fn save_routes_through_a_temp_file_before_replacing() {
        // 这条专防「原子写退化回 std::fs::write」：那种写法不留任何文件痕迹，
        // 上面「无 .tmp 残留」那条对它照样绿（review 变异 MH 实测：整段 temp+sync+rename
        // 换成 `fs::write` 后只有**这一条**转红）。
        // 把 temp 的名字抢先占成一个目录，`File::create` 必然失败 ——
        // 只有真的走了 temp 这条路，save 才会报错，而直接覆盖目标文件的写法会照样「成功」。
        let d = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(d.path());
        store
            .save(&Settings {
                model: "qwen-plus".into(),
                ..Default::default()
            })
            .unwrap();
        let before = std::fs::read_to_string(store.path()).unwrap();
        std::fs::create_dir(d.path().join("settings.json.tmp")).unwrap();
        let err = store
            .save(&Settings {
                model: "qwen-max".into(),
                ..Default::default()
            })
            .expect_err("temp 不可用时必须报错，不许退化成直接覆盖目标文件");
        assert!(matches!(err, SettingsError::Io(_)), "{err}");
        assert_eq!(std::fs::read_to_string(store.path()).unwrap(), before);
        assert_eq!(store.load().unwrap().model, "qwen-plus", "旧内容必须还在");
    }

    #[test]
    fn save_creates_a_missing_directory() {
        let d = tempfile::tempdir().unwrap();
        let nested = d.path().join("app data").join("v1");
        let store = SettingsStore::new(&nested);
        store.save(&Settings::default()).unwrap();
        assert!(store.path().exists());
    }

    #[test]
    fn concurrent_saves_never_error_and_never_leave_a_temp_behind() {
        // review M-3：temp 名字是固定的，而 T13 打算在 `spawn_blocking` 里用这个 store，
        // 于是「两个线程同时 save」是设计形态。没有 `SettingsStore::state` 那把锁时，
        // 第二个 `rename` 会因为自己的 temp 已被第一个 rename 搬走而报 `NotFound`。
        let d = tempfile::tempdir().unwrap();
        let store = Arc::new(SettingsStore::new(d.path()));
        let models = ["qwen-a", "qwen-b", "qwen-c", "qwen-d", "qwen-e", "qwen-f"];
        std::thread::scope(|s| {
            for m in models {
                let store = Arc::clone(&store);
                s.spawn(move || {
                    for _ in 0..8 {
                        let st = Settings {
                            model: m.to_string(),
                            ..Default::default()
                        };
                        let e = store.save(&st);
                        assert!(e.is_ok(), "{m} 的 save 不该失败，实测：{e:?}");
                    }
                });
            }
        });
        assert_eq!(
            names(d.path()),
            vec!["settings.json".to_string()],
            "并发下也不许留下 .tmp 或半文件"
        );
        let got = store.load().unwrap();
        assert!(
            models.contains(&got.model.as_str()),
            "最终内容必须是某一次完整写入，实测：{}",
            got.model
        );
    }

    // ---- R86：钳位与派生只留一处 ----

    #[test]
    fn normalize_raises_zero_window_messages_to_one() {
        let mut s = Settings {
            window_messages: 0,
            ..Default::default()
        };
        s.normalize();
        assert_eq!(s.window_messages, 1);
    }

    #[test]
    fn normalize_raises_zero_reserve_output_to_one() {
        let mut s = Settings {
            reserve_output: 0,
            budget_tokens: 32_000,
            ..Default::default()
        };
        s.normalize();
        assert_eq!(s.reserve_output, 1);
        assert_eq!(
            s.budget_tokens, 32_000,
            "只补 reserve，别顺手改动本来合法的预算"
        );
    }

    #[test]
    fn normalize_rebuilds_a_budget_that_cannot_hold_the_reservation() {
        // budget == reserve 也必须触发：context.rs 的 `budget - reserve` 在这种组合下正好塌成 0
        for budget in [0u32, 1, 4_096] {
            let mut s = Settings {
                reserve_output: 4_096,
                budget_tokens: budget,
                ..Default::default()
            };
            s.normalize();
            assert_eq!(
                s.budget_tokens, 5_120,
                "reserve=4096, budget={budget} 应重算成 reserve+1024"
            );
        }
    }

    #[test]
    fn normalize_keeps_valid_numbers_untouched() {
        let mut s = Settings {
            base_url: "https://x.com/v1".into(),
            reserve_output: 2_000,
            budget_tokens: 2_001,
            window_messages: 2,
            ..Default::default()
        };
        s.normalize();
        assert_eq!(
            s,
            Settings {
                base_url: "https://x.com/v1".into(),
                reserve_output: 2_000,
                budget_tokens: 2_001,
                window_messages: 2,
                ..Default::default()
            }
        );
    }

    #[test]
    fn normalize_is_idempotent_and_survives_the_u32_ceiling() {
        let mut s = Settings {
            reserve_output: u32::MAX,
            budget_tokens: 1,
            ..Default::default()
        };
        s.normalize();
        let once = s.clone();
        s.normalize();
        assert_eq!(s, once, "normalize 必须幂等：load 与 save 各调一次才安全");
        // R97：reserve 被夹到 `u32::MAX - 1024`，预算因此能真的比它大 1024。
        // 旧写法（不夹 reserve、只 `saturating_add`）在这里得到的是 reserve == budget == u32::MAX，
        // `context.rs:117` 的 `saturating_sub` 照样把预算塌成 0 —— 钳位在最难的那条边界上失守。
        assert_eq!(
            s.reserve_output,
            u32::MAX - BUDGET_HEADROOM,
            "reserve 必须留出 headroom 的空间，否则后置条件守不住"
        );
        assert_eq!(s.budget_tokens, u32::MAX, "不许溢出 panic");
        assert!(
            s.budget_tokens > s.reserve_output,
            "normalize 的后置条件必须无条件成立：预算塌成 0 的形态就是「它记不住上文」"
        );
        // 后置条件也要在 `to_context_config` 那一侧兑现（同一个 bug 的下游形状）。
        let ctx = s.to_context_config();
        assert!(ctx.budget_tokens > ctx.reserve_output, "{ctx:?}");
    }

    #[test]
    fn normalize_clamps_font_size_into_the_readable_range() {
        // R123：`applyTheme` 把这个数原样写成 `<html>` 的 inline `--font-size`，
        // 于是 0 是「字看不见」、`u32::MAX` 是「一行一个字」，两者都不报错。
        for (input, want) in [
            (0u32, MIN_FONT_SIZE),
            (11, MIN_FONT_SIZE),
            (12, MIN_FONT_SIZE),
            (15, 15),
            (24, MAX_FONT_SIZE),
            (25, MAX_FONT_SIZE),
            (999, MAX_FONT_SIZE),
            (u32::MAX, MAX_FONT_SIZE),
        ] {
            let mut s = Settings {
                font_size: input,
                ..Default::default()
            };
            s.normalize();
            assert_eq!(s.font_size, want, "font_size={input} 应钳到 {want}");
        }
    }

    #[test]
    fn absurd_compaction_settings_are_clamped_not_obeyed() {
        // 1.5 的阈值意味着「永不压缩」，0 的阈值意味着「每轮都压」。两者都不该照原样进内核，
        // 也不该报错 —— 钳位是 normalize 的职责（`font_size` 12..=24 是同一道闸的兄弟字段）。
        let s = Settings {
            compact_threshold: 1.5,
            keep_turns: 0,
            temperature: 9.0,
            ..Default::default()
        };
        let n = s.normalized();
        assert!(n.compact_threshold <= 0.95, "阈值上界：留至少 5% 余量");
        assert!(n.keep_turns >= 1, "K=0 会让压缩把最近一轮也压掉 ⇒ 当场失忆");
        assert!(
            (0.0..=2.0).contains(&n.temperature),
            "兼容端点对 >2 普遍报错，别把用户的锅变成 400"
        );
    }

    #[test]
    fn non_finite_numbers_fall_back_to_the_default_instead_of_to_nan() {
        // `f32::clamp` 对 NaN 返回 NaN（min/max 都被跳过），而 NaN 与任何数的比较都是 false：
        // 一个 NaN 的 `compact_threshold` 会让「该不该压缩」永远判假，且一句日志都不留。
        // 所以这里回落默认而不是钳位 —— 先例是 `set_calibration`（`agent/context.rs`）。
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let n = Settings {
                compact_threshold: bad,
                temperature: bad,
                ..Default::default()
            }
            .normalized();
            assert_eq!(
                n.compact_threshold, DEFAULT_COMPACT_THRESHOLD,
                "非有限阈值必须回落默认，实际 {n:?}"
            );
            assert_eq!(n.temperature, DEFAULT_TEMPERATURE, "非有限温度必须回落默认");
            assert!(n.compact_threshold.is_finite() && n.temperature.is_finite());
        }
    }

    #[test]
    fn compaction_bounds_are_the_clamped_values_on_both_ends() {
        // 上面那条只管「不许照原样进内核」，这一条把**两个方向**的边界都钉成精确数字，
        // 否则把 `0.05` 写成 `0.0`（阈值 0 = 每轮都压）不会有任何测试变红。
        for (input, want) in [
            (-1.0f32, MIN_COMPACT_THRESHOLD),
            (0.0, MIN_COMPACT_THRESHOLD),
            (0.05, MIN_COMPACT_THRESHOLD),
            (0.85, 0.85),
            (0.95, MAX_COMPACT_THRESHOLD),
            (1.0, MAX_COMPACT_THRESHOLD),
            (1.5, MAX_COMPACT_THRESHOLD),
        ] {
            let mut s = Settings {
                compact_threshold: input,
                ..Default::default()
            };
            s.normalize();
            assert_eq!(
                s.compact_threshold, want,
                "compact_threshold={input} 应钳到 {want}"
            );
        }
        for (input, want) in [
            (-1.0f32, MIN_TEMPERATURE),
            (0.0, MIN_TEMPERATURE),
            (0.7, 0.7),
            (2.0, MAX_TEMPERATURE),
            (2.5, MAX_TEMPERATURE),
            (9.0, MAX_TEMPERATURE),
        ] {
            let mut s = Settings {
                temperature: input,
                ..Default::default()
            };
            s.normalize();
            assert_eq!(s.temperature, want, "temperature={input} 应钳到 {want}");
        }
        // `keep_turns` 是 u32，非有限值压根造不出来，只需要边界。
        for (input, want) in [
            (0u32, MIN_KEEP_TURNS),
            (1, MIN_KEEP_TURNS),
            (10, 10),
            (50, MAX_KEEP_TURNS),
            (51, MAX_KEEP_TURNS),
            (u32::MAX, MAX_KEEP_TURNS),
        ] {
            let mut s = Settings {
                keep_turns: input,
                ..Default::default()
            };
            s.normalize();
            assert_eq!(s.keep_turns, want, "keep_turns={input} 应钳到 {want}");
        }
    }

    #[test]
    fn font_size_is_the_clamped_value_after_a_save_and_load_round_trip() {
        // 边界只在 `normalize()` 里定义一次，所以「读回来」「落盘的字节」两处都得是钳过的值，
        // 而不是靠前端再钳一遍（那会造出第二份真相，T16 就无法确定自己拿到的是哪个数）。
        let d = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(d.path());
        store
            .save(&Settings {
                font_size: 999,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(store.load().unwrap().font_size, MAX_FONT_SIZE);
        store
            .save(&Settings {
                font_size: 0,
                ..Default::default()
            })
            .unwrap();
        let raw = std::fs::read_to_string(store.path()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            v["font_size"], MIN_FONT_SIZE,
            "落盘的字节也得是钳过的值，别让下一次读取先看到 0：{raw}"
        );
        assert_eq!(store.load().unwrap().font_size, MIN_FONT_SIZE);
    }

    #[test]
    fn loaded_settings_are_clamped_even_when_the_file_asks_for_zero() {
        let d = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(d.path());
        std::fs::write(
            store.path(),
            r#"{"reserve_output":4096,"budget_tokens":10,"window_messages":0}"#,
        )
        .unwrap();
        let got = store.load().unwrap();
        assert_eq!(got.budget_tokens, 5_120);
        assert_eq!(got.window_messages, 1);
    }

    #[test]
    fn saved_settings_are_clamped_in_the_file_itself() {
        let d = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(d.path());
        let s = Settings {
            reserve_output: 0,
            budget_tokens: 1,
            window_messages: 0,
            ..Default::default()
        };
        store.save(&s).unwrap();
        let raw = std::fs::read_to_string(store.path()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["reserve_output"], 1);
        // 1025 而不是 1024+4096：预算是**跟钳位之后的 reserve** 比的，
        // 顺序反过来（先比预算再补 reserve）会让这条落进「budget > 0」的假合法区。
        assert_eq!(v["budget_tokens"], 1_025);
        assert_eq!(v["window_messages"], 1);
    }

    #[test]
    fn to_context_config_maps_every_context_field_and_clamps_them() {
        let cc = Settings {
            reserve_output: 2_000,
            budget_tokens: 1_000,
            window_messages: 0,
            ..Default::default()
        }
        .to_context_config();
        assert_eq!(cc.reserve_output, 2_000);
        assert_eq!(cc.budget_tokens, 3_024);
        assert_eq!(cc.window_messages, 1);
        let d = Settings::default().to_context_config();
        assert_eq!(
            (d.reserve_output, d.budget_tokens, d.window_messages),
            (4096, 32_000, 40),
            "派生出来的默认值必须和 context.rs 的默认值逐字相同（spec 5.3）"
        );
        assert_eq!(
            (d.compact_threshold, d.keep_turns),
            (0.85, 10),
            "压缩那两个键也一样：两边默认值一旦分叉，T41 读到的就不是设置里的那个"
        );
    }

    #[test]
    fn settings_reach_the_context_builder_end_to_end() {
        // 这条钉的是「映射」而不是「钳位」：钳位由上面那条管。谁把 to_context_config
        // 里这两行漏掉，T41 的压缩就会永远看到 0.85/10 —— 用户改了设置、什么都没发生。
        let s = Settings {
            compact_threshold: 0.5,
            keep_turns: 3,
            ..Default::default()
        };
        let c = s.to_context_config();
        assert_eq!(c.compact_threshold, 0.5);
        assert_eq!(c.keep_turns, 3);
    }

    #[test]
    fn settings_temperature_reaches_the_request_body() {
        // 前置事实 3：管线早就通了（openai_compat.rs 的 build_request 且 :1025 那条钉着），
        // 缺的只有 `Settings` 这个键和 session.rs 那句 `temperature: None`。
        // 这条测试钉的是**最后一段线**，不是重测 openai_compat。
        let s = Settings {
            temperature: 0.3,
            ..Default::default()
        };
        let mc = s.to_model_config("k");
        assert_eq!(mc.temperature, Some(0.3));
    }

    #[test]
    fn model_config_uses_the_same_normalization_as_the_saved_file() {
        let s = Settings {
            base_url: "https://x.com/v1/chat/completions".into(),
            reserve_output: 777,
            ..Default::default()
        };
        let mc = s.to_model_config("k");
        assert_eq!(
            mc.base_url, "https://x.com/v1",
            "端点段必须摘掉，否则拼出双端点 404（R27）"
        );
        assert_eq!(mc.max_output_tokens, 777);
        assert_eq!(mc.model, "qwen-plus");
    }

    // ---- R90：空 base_url 是合法值 ----

    #[test]
    fn an_empty_base_url_stays_empty_instead_of_getting_an_invented_default() {
        let mut s = Settings::default();
        s.normalize();
        assert_eq!(s.base_url, "", "不许替用户决定接哪家服务（R90）");
        let d = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(d.path());
        store.save(&Settings::default()).unwrap();
        assert_eq!(store.load().unwrap().base_url, "");
        assert_eq!(Settings::default().to_model_config("k").base_url, "");
    }

    // ---- R89：不加没有消费者的字段 ----

    #[test]
    fn serialized_field_names_are_exactly_the_eleven_settings_fields() {
        let json = serde_json::to_value(Settings::default()).unwrap();
        let mut keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(|s| s.as_str())
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "base_url",
                "budget_tokens",
                "compact_threshold",
                "font_size",
                "keep_turns",
                "mode",
                "model",
                "reserve_output",
                "temperature",
                "theme",
                "window_messages"
            ]
        );
        // T39 起 `temperature`/`compact_threshold`/`keep_turns` 都是**认识的**键，
        // 再放进这个名单就会自我矛盾（它测的是「这个键压根不存在」，不是「它没被序列化」）。
        // 剩下唯一永远不该出现的键只有 `api_key`：它住凭据管理器，不住 settings.json（R89/spec :473）。
        // T43 新加的 `accent`/`mcp`/`roots` 落进 `Settings` 后同样要从这里挪走 ——
        // 这条名单只该收缩，谁加字段谁负责，别把它当成「五个永远不会出现的键」的原样清单。
        let raw = serde_json::to_string(&Settings::default()).unwrap();
        for banned in ["api_key"] {
            assert!(
                !raw.contains(banned),
                "{banned} 不该出现在 settings.json 里"
            );
        }
    }

    #[test]
    fn mode_serializes_as_lowercase_words_for_the_frontend_mirror() {
        let s = Settings {
            mode: Mode::Light,
            ..Default::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"mode\":\"light\""), "{json}");
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.mode, Mode::Light);
    }

    // ---- R91：读写向前兼容 ----

    #[test]
    fn missing_fields_fall_back_to_defaults_without_a_backup() {
        let d = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(d.path());
        std::fs::write(store.path(), r#"{"model":"qwen-turbo"}"#).unwrap();
        let got = store.load().unwrap();
        assert_eq!(got.model, "qwen-turbo");
        assert_eq!(got.reserve_output, 4096);
        assert_eq!(got.theme, "noir");
        assert!(
            corrupt_backups(d.path()).is_empty(),
            "缺字段是可读的，不该按损坏处理（R84 与 R91 的边界）"
        );
    }

    #[test]
    fn unknown_fields_are_ignored_and_the_file_is_not_backed_up() {
        let d = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(d.path());
        // 夹具必须用**永远不会被认识**的键。原来这里写的是 `"temperature":0.7,"accent":"#ffffff"`，
        // 而 T39 把 `temperature` 变成了 `Settings` 的真字段 —— 测试照绿，但它声称的
        // 「不认识的字段被忽略」从此没人测了（前置事实 4 的第二半，那才是真正的坑）。
        std::fs::write(
            store.path(),
            r##"{"model":"qwen-turbo","quantum_mode":true,"legacy_engine":"x"}"##,
        )
        .unwrap();
        let got = store.load().unwrap();
        assert_eq!(got.model, "qwen-turbo");
        assert_eq!(
            got.window_messages, 40,
            "不认识字段时其余设置必须照原样保住（R91）"
        );
        assert!(corrupt_backups(d.path()).is_empty());
    }

    // ---- R81/R83：凭据接缝 ----

    #[test]
    fn api_key_masked_reports_a_read_failure_instead_of_unconfigured() {
        let d = tempfile::tempdir().unwrap();
        let store = SettingsStore::with_secrets(d.path(), Arc::new(BrokenStore));
        // brief 的 `.ok().flatten()` 会把「读不到」显示成「未配置」，用户于是会重设 key、
        // 覆盖掉其实还在库里的那条（R83）。这条断言就是钉住那个区别的。
        let err = store
            .api_key_masked()
            .expect_err("读不到必须报 Err，不许降级成 Ok(None)");
        assert!(
            err.to_string().contains("凭据管理器读不到（测试注入）"),
            "{err}"
        );
        assert!(store.api_key().is_err());
        assert!(store.set_api_key("k").is_err());
        assert!(store.clear_api_key().is_err());
    }

    #[test]
    fn the_unsupported_platform_store_refuses_every_operation() {
        // R81：非 Windows 目标上 `SettingsStore::new` 给出的就是它。不许静默换成内存存储。
        let s = UnsupportedSecretStore;
        assert!(matches!(
            s.set("k"),
            Err(SettingsError::UnsupportedPlatform)
        ));
        assert!(matches!(s.get(), Err(SettingsError::UnsupportedPlatform)));
        assert!(matches!(
            s.delete(),
            Err(SettingsError::UnsupportedPlatform)
        ));
        assert!(
            s.get()
                .unwrap_err()
                .to_string()
                .contains("当前平台没有凭据管理器")
        );
    }

    #[cfg(windows)]
    #[test]
    fn mock_backed_round_trip_sets_gets_and_deletes_the_key() {
        let d = tempfile::tempdir().unwrap();
        let (store, _m) = mock_backed(d.path());
        assert_eq!(
            store.api_key().unwrap(),
            None,
            "没设过的条目是 None，不是 Err"
        );
        store.set_api_key("sk-abcdefghijklmnop").unwrap();
        assert_eq!(
            store.api_key().unwrap().as_deref(),
            Some("sk-abcdefghijklmnop")
        );
        assert_eq!(
            store.api_key_masked().unwrap().as_deref(),
            Some("sk-a…mnop")
        );
        store.clear_api_key().unwrap();
        assert_eq!(store.api_key().unwrap(), None);
        assert_eq!(store.api_key_masked().unwrap(), None);
        // R88：再删一次仍然是 Ok —— 「清除」按幂等语义设计，UI 不必先探测有没有
        store.clear_api_key().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn an_empty_key_is_stored_as_a_clear_instead_of_a_blank_credential() {
        let d = tempfile::tempdir().unwrap();
        let (store, _m) = mock_backed(d.path());
        store.set_api_key("sk-abcdefghijklmnop").unwrap();
        store.set_api_key("").unwrap();
        assert_eq!(
            store.api_key().unwrap(),
            None,
            "空串必须走清除，否则掩码会说「未配置」而库里真有条目"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_platform_length_limit_surfaces_with_its_own_number() {
        let d = tempfile::tempdir().unwrap();
        let (store, m) = mock_backed(d.path());
        let entry = m.build(KEYRING_SERVICE, KEYRING_USER, None).unwrap();
        let cred = entry
            .as_any()
            .downcast_ref::<mock::Cred>()
            .expect("mock entry downcasts to mock::Cred");
        cred.set_error(keyring_core::Error::TooLong(
            "password encoded as UTF-16".to_string(),
            2560,
        ));
        let err = store
            .set_api_key("x")
            .expect_err("TooLong 必须透到调用方（R82）");
        let msg = err.to_string();
        assert!(msg.contains("2560"), "上限数字必须原样带出来，实测：{msg}");
        assert!(msg.contains("password encoded as UTF-16"), "{msg}");
        store.set_api_key("sk-abcdefghijklmnop").unwrap();
        assert_eq!(
            store.api_key().unwrap().as_deref(),
            Some("sk-abcdefghijklmnop"),
            "错误是一次性的，不该把 store 弄坏"
        );
    }

    #[cfg(windows)]
    #[test]
    fn the_real_windows_store_names_the_entry_user_then_service() {
        // 只 build、不 set_password：crate 自己写着「Building a credential does not create a
        // key in the store」（store.rs:108），所以这条不碰真实凭据管理器，
        // 跑完 `cmdkey /list` 仍然应当查不到 qwen 条目。
        let real: Arc<CredentialStore> = windows_native_keyring_store::Store::new().unwrap();
        let entry = real.build(KEYRING_SERVICE, KEYRING_USER, None).unwrap();
        assert_eq!(
            entry.get_specifiers(),
            Some((KEYRING_SERVICE.to_string(), KEYRING_USER.to_string()))
        );
        let dbg = format!("{entry:?}");
        assert!(
            dbg.contains("api_key.qwen-do"),
            "实测条目名取自 crate 的 `{{prefix}}{{user}}{{divider}}{{service}}{{suffix}}`，实得：{dbg}"
        );
    }

    // ---- review M-1：`{:?}` 不许把秘密字节打出来 ----

    #[cfg(windows)]
    #[test]
    fn credential_debug_never_prints_the_secret_bytes() {
        // 前提先自证：`keyring_core::Error` 自己 derive 的 Debug 确实把 Vec<u8> 逐字节打印。
        // 不写成断言的话，这条测试会在「上游哪天改了 Debug」之后悄悄失去意义。
        let secret: Vec<u8> = b"sk-CANARY-0123456789".to_vec();
        let upstream = format!("{:?}", keyring_core::Error::BadEncoding(secret.clone()));
        assert!(
            upstream.contains("115"),
            "前提不成立（上游 Debug 变了，本条要重读）：{upstream}"
        );

        // 我们的 Debug 必须只走 Display。Display 侧本来就安全：那两个变体只会说
        // "Password data is not valid UTF-8"，一个字节都不带。
        // 每一对是「装箱后的错误」和「它的 Display 文本」。断言用的是**内层** Display：
        // Debug 里透出的是上游那句原因，不带 `SettingsError` 自己的「凭据管理器操作失败: 」
        // 前缀（前缀属于 Display，属于另一套实现），拿 `err.to_string()` 去比会假红。
        for (err, reason) in [
            (
                credential_error(keyring_core::Error::BadEncoding(secret.clone())),
                "Password data is not valid UTF-8",
            ),
            (
                SettingsError::Credential(Box::new(std::io::Error::other(
                    "凭据管理器读不到（测试注入）",
                ))),
                "凭据管理器读不到（测试注入）",
            ),
        ] {
            let dbg = format!("{err:?}");
            assert!(
                !dbg.contains("115") && !dbg.contains("sk-CANARY"),
                "`{err:?}` 把秘密带出来了：{dbg}"
            );
            // N-1：只查「不带出字节」是半个守卫 —— 变异 `Self::Credential(_) =>
            // f.write_str("Credential")` 能在 41/41 全绿下存活，因为「安全但什么都不说」
            // 同样满足前半句。后半句必须单独钉：原因文本要真的透出来。
            assert!(
                dbg.contains(reason),
                "Debug 应当透出上游 Display 的那句原因，实测：{dbg}"
            );
        }
        // `TooLong` 自带平台上限数字（R82 要的就是它能透到调用方）：走 Display 之后仍在。
        let too_long = credential_error(keyring_core::Error::TooLong(
            "password encoded as UTF-16".to_string(),
            2560,
        ));
        let dbg = format!("{too_long:?}");
        assert!(dbg.contains("2560"), "上限数字必须留在 Debug 里：{dbg}");
        // 手写 Debug 的另一面：`UnsupportedPlatform` / `Io` / `Json` 三支仍然打得出来，
        // 别把「不泄露」做成「什么都打不出」。
        let p = format!("{:?}", SettingsError::UnsupportedPlatform);
        assert!(p.contains("UnsupportedPlatform"), "{p}");
    }

    // ---- review R95：粘贴进来的 key 要 trim ----

    #[cfg(windows)]
    #[test]
    fn a_pasted_key_is_trimmed_and_a_whitespace_only_one_clears() {
        // 从浏览器/邮件里复制 key 常带尾部 `\r\n`。原样入库换来的是 API 侧一个 401，
        // 而设置页显示 `sk-a…mnop` 一切正常 —— 没有任何线索指向真正的原因。
        let d = tempfile::tempdir().unwrap();
        let (store, _m) = mock_backed(d.path());
        store
            .set_api_key("  sk-abcdefghijklmnop\r\n")
            .expect("trim 之后是非空，正常写入");
        assert_eq!(
            store.api_key().unwrap().as_deref(),
            Some("sk-abcdefghijklmnop"),
            "存进去的必须是 trim 后的那一串，空格与 \\r\\n 都不许跟着进凭据管理器"
        );
        // 纯空白 trim 完就是空串 → 走清除，而不是存一条空白条目（比没有更难查）。
        store.set_api_key("sk-abcdefghijklmnop").unwrap();
        store.set_api_key("  \r\n ").unwrap();
        assert_eq!(store.api_key().unwrap(), None, "空白必须等价于「清除」");
    }

    #[cfg(windows)]
    #[test]
    fn a_blank_stored_entry_reads_as_unconfigured_not_masked() {
        // `mask_key("")` 返回的是「未配置」这三个字。若库里躺着一条空白条目（本模块写不进去，
        // 但别的程序可以），`api_key_masked()` 就会给出 `Ok(Some("未配置"))` —— 和 `Ok(None)`
        // 在界面上长得一模一样，把 R83 特意分开的两件事重新合起来。这里提前把它掰开。
        let d = tempfile::tempdir().unwrap();
        let (store, m) = mock_backed(d.path());
        m.build(KEYRING_SERVICE, KEYRING_USER, None)
            .unwrap()
            .set_password("")
            .expect("绕过 SettingsStore，直接往 mock 里塞一条空白条目");
        assert_eq!(
            store.api_key().unwrap().as_deref(),
            Some(""),
            "明文侧只夹空白，不把条目变成 None（库里确实有一条）"
        );
        assert_eq!(
            store.api_key_masked().unwrap(),
            None,
            "掩码侧空白必须收敛成 None，否则 UI 无从区分"
        );
    }

    #[cfg(windows)]
    #[test]
    fn an_entry_written_by_another_program_reads_back_trimmed() {
        // N-5 的另一半。`set_api_key` 写进去的必然干净，但库里那条可能是**别的程序**、或
        // R95 之前那次构建写的，带着尾部 `\r\n`。那种值一路走到 HTTP 头就是一个 401，
        // 而设置页上的掩码看着完全正常 —— 读侧不夹，这个 bug 在本机没有任何线索。
        let d = tempfile::tempdir().unwrap();
        let (store, m) = mock_backed(d.path());
        let put = |v: &str| {
            m.build(KEYRING_SERVICE, KEYRING_USER, None)
                .unwrap()
                .set_password(v)
                .expect("绕过 SettingsStore，直接往 mock 里塞条目")
        };
        put("sk-abcdefghijklmnop\r\n");
        assert_eq!(
            store.api_key().unwrap().as_deref(),
            Some("sk-abcdefghijklmnop"),
            "尾部的 \\r\\n 要在读侧就消失"
        );
        assert_eq!(
            store.api_key_masked().unwrap().as_deref(),
            Some("sk-a…mnop"),
            "掩码不许带着 ␍␊ 的尾巴（那会让用户以为 key 是对的、只是显示问题）"
        );
        put("\r\n");
        assert_eq!(
            store.api_key().unwrap().as_deref(),
            Some(""),
            "纯空白条目夹完就是空串"
        );
        assert_eq!(
            store.api_key_masked().unwrap(),
            None,
            "纯空白条目走「未配置」，与上一条同源"
        );
    }

    // ---- spec 6.6 / :473：key 永远不落盘 ----

    #[cfg(windows)]
    #[test]
    fn the_key_never_appears_anywhere_in_the_settings_directory() {
        let d = tempfile::tempdir().unwrap();
        let (store, _m) = mock_backed(d.path());
        let key = "sk-abcdefghijklmnop";
        store.set_api_key(key).unwrap();
        let s = Settings {
            base_url: "https://x.com/v1".into(),
            ..Default::default()
        };
        store.save(&s).unwrap();
        store.load().unwrap();
        let all = dir_text(d.path());
        assert!(
            !all.contains(key),
            "配置目录全文搜索 key，0 命中才是 spec :543 要的测法"
        );
        assert!(!all.contains("api_key"), "{all}");
        assert_eq!(names(d.path()), vec!["settings.json".to_string()]);
        assert!(format!("{:?}", store.api_key_masked().unwrap()).contains("sk-a…mnop"));
    }

    #[test]
    fn mask_key_counts_chars_not_bytes_and_only_shows_four_of_each_end() {
        // 4 字节一组的非 ASCII：按字节切会踩到字符边界 panic，这里必须是按字符
        assert_eq!(mask_key("sk-测试密钥abcdefgh"), "sk-测…efgh");
        assert_eq!(mask_key("12345678"), "***", "8 个字符及以内一律全遮");
        assert_eq!(mask_key("123456789"), "1234…6789");
        assert!(!mask_key("sk-abcdefghijklmnop").contains("bcdefghijklmno"));
    }

    #[test]
    fn settings_store_is_send_and_sync_for_tauri_managed_state() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SettingsStore>();
        assert_send_sync::<Settings>();
        assert_send_sync::<SettingsError>();
    }
}
