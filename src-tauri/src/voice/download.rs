//! SenseVoice 模型下载器（平台无关，Linux 可测）。
//!
//! 纪律与 `tools/fetch.rs` 同源：transport 经 trait 注入（生产 reqwest chunk 循环，
//! 测试 fake），**不**按远端声明的 Content-Length 预分配内存（边收边写 `.part` 文件），
//! 尺寸校验通过后才原子 rename 生效。取消用 `CancellationToken`，任何失败/取消路径
//! 都清掉 `.part`。镜像（hf-mirror）优先，官方 HuggingFace 兜底，按文件逐个换源重试。
//! v1 不做 Range 断点续传：重试 = 重下该文件。
//!
//! 本模块不 `use tauri`：进度经 `Fn(VoiceModelStatus)` 闭包发布，闭包由 `ipc.rs` 装配。

use super::VoiceModelStatus;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};
use tokio_util::sync::CancellationToken;

/// 模型目录名（契约固定）：`data_dir/models/<MODEL_DIR_NAME>/`。
pub const MODEL_DIR_NAME: &str = "sense-voice-zh-en-ja-ko-yue-2024-07-17";
/// 仓库路径只此一处；两个 host 拼同一条路径。
const MODEL_REPO: &str = "csukuangfj/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17";
/// 镜像优先，官方兜底（单文件 URL，reqwest 默认跟随 302）。
const BASE_URLS: [&str; 2] = ["https://hf-mirror.com", "https://huggingface.co"];
const USER_AGENT: &str = concat!("lfttdo/", env!("CARGO_PKG_VERSION"));

/// 一个模型文件的验收标准：名字 + 精确字节数 + sha256（下载后核对再 rename 生效）。
/// 哈希是「同尺寸损坏模型」的唯一防线：只校尺寸时，内容损坏但字节数恰好对的模型
/// 能蒙混成 ready，用户无法经 UI 重下（尺寸对 → 认为已下载）。哈希取自实测识别
/// 3/3 对齐基线的已知良好模型。
struct FileSpec {
    name: &'static str,
    bytes: u64,
    sha256: &'static str,
}

/// 先下 tokens.txt（小，验证连通），再下 model.int8.onnx。
const FILES: [FileSpec; 2] = [
    FileSpec {
        name: "tokens.txt",
        bytes: 315_894,
        sha256: "f449eb28dc567533d7fa59be34e2abca8784f771850c78a47fb731a31429a1dc",
    },
    FileSpec {
        name: "model.int8.onnx",
        bytes: 239_233_841,
        sha256: "c71f0ce00bec95b07744e116345e33d8cbbe08cef896382cf907bf4b51a2cd51",
    },
];

/// 模型目录：`data_dir/models/sense-voice-zh-en-ja-ko-yue-2024-07-17/`。
/// 契约：绝不写进 NSIS 安装目录或用户可配的 storage root。
pub fn model_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("models").join(MODEL_DIR_NAME)
}

/// 磁盘上的模型状态。ready 的充要条件：两个文件都存在、字节数与 sha256 都与验收值
/// 精确一致 —— 半个文件、手改过/内容损坏的文件（哪怕尺寸恰好对）都按 missing 处理，
/// 宁可重下也不让引擎加载损坏的模型，也让用户能经 UI 重新下载。
/// error 只存在于下载任务的内存里，不落盘，所以这里不产出 error。
pub fn status_of(dir: &Path) -> VoiceModelStatus {
    let ready = dir_ready(dir, &FILES);
    VoiceModelStatus {
        state: if ready { "ready" } else { "missing" }.into(),
        percent: 0,
        message: String::new(),
    }
}

/// 目录就绪：每个文件都过尺寸 + 哈希。与文件表分离便于用迷你规格测试。
fn dir_ready(dir: &Path, files: &[FileSpec]) -> bool {
    files.iter().all(|f| spec_matches(&dir.join(f.name), f))
}

/// 单文件验收：先查尺寸（廉价拒绝），再流式算 sha256 比对。读失败/哈希不符都算不过。
/// 哈希结果按文件身份（路径 + 尺寸 + mtime）进程内缓存：身份不变命中缓存不重哈
/// （否则每次开始录音都要全量哈希 239MB）；文件被替换/删除、尺寸或 mtime 变化都使
/// 键失配而重新校验。缓存里存的是实算哈希，损坏文件的哈希永远不等于验收值，
/// 绝不会被缓存成 ready。
fn spec_matches(path: &Path, spec: &FileSpec) -> bool {
    let Ok(m) = std::fs::metadata(path) else {
        return false;
    };
    if m.len() != spec.bytes {
        return false;
    }
    let Ok(mtime) = m.modified() else {
        // 拿不到 mtime 就无法构造身份键：退化为现算，不进缓存。
        return sha256_of_file(path)
            .map(|h| h == spec.sha256)
            .unwrap_or(false);
    };
    let mut guard = VERIFIED.lock().unwrap_or_else(|e| e.into_inner());
    let cache = guard.get_or_insert_with(HashMap::new);
    if let Some((len, mt, hash)) = cache.get(path)
        && *len == m.len()
        && *mt == mtime
    {
        return hash == spec.sha256;
    }
    let Ok(hash) = sha256_of_file(path) else {
        return false;
    };
    let ok = hash == spec.sha256;
    cache.insert(path.to_path_buf(), (m.len(), mtime, hash));
    ok
}

/// 校验结果缓存（进程级，与 `asr::CACHE` 同款）：键 = 文件路径，值 = (尺寸, mtime,
/// 实算 sha256)。身份（尺寸 + mtime）不变 ⇒ 内容没变，直接复用哈希；下载生效走
/// 原子 rename、手改/损坏都会动 mtime 或尺寸，键失配即重校。锁在哈希期间持有，
/// 并发查询不会重复付全量哈希。
/// 缓存条目：(尺寸, mtime, 实算 sha256)。
type VerifiedEntry = (u64, SystemTime, String);

static VERIFIED: Mutex<Option<HashMap<PathBuf, VerifiedEntry>>> = Mutex::new(None);

/// 流式算一个文件的 sha256（hex 小写）。64KB 缓冲，不整文件读进内存。
fn sha256_of_file(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65_536];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// 下载的错误形状：取消与失败必须分开 —— 取消回 missing（可重试），失败回 error（带原因）。
#[derive(Debug)]
pub enum DownloadError {
    Cancelled,
    Failed(String),
}

/// 流式 HTTP 接缝（仿 `tools/fetch.rs` 的 `FetchTransport`）。
/// `on_chunk` 返回 Err 时 transport 必须中止流并原样传播该错误。
#[async_trait::async_trait]
pub trait ModelTransport: Send + Sync {
    /// GET `url`，逐块喂给 `on_chunk`；返回 Content-Length（进度分母，拿不到为 None）。
    async fn stream(
        &self,
        url: &str,
        on_chunk: &mut (dyn for<'a> FnMut(&'a [u8]) -> Result<(), DownloadError> + Send),
    ) -> Result<Option<u64>, DownloadError>;
}

/// 生产 transport：reqwest chunk 循环 + 取消令牌。
/// 取消在两个 await 点都生效（等响应头、等每一块），连接 stall 也能立即退出。
pub struct ReqwestModelTransport {
    client: reqwest::Client,
    cancel: CancellationToken,
}

impl ReqwestModelTransport {
    pub fn new(cancel: CancellationToken) -> Result<Self, String> {
        // 超时防「对端 TCP 存活但应用静默」（镜像反代源站 stall）把 resp.chunk() 永久
        // 挂起、镜像→官方兜底永不触发。read_timeout 按块重置，不影响慢速大文件；刻意
        // 不设 total timeout —— 那会误杀 239MB 模型的慢速下载。
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| format!("无法构造下载客户端：{e}"))?;
        Ok(Self { client, cancel })
    }
}

#[async_trait::async_trait]
impl ModelTransport for ReqwestModelTransport {
    async fn stream(
        &self,
        url: &str,
        on_chunk: &mut (dyn for<'a> FnMut(&'a [u8]) -> Result<(), DownloadError> + Send),
    ) -> Result<Option<u64>, DownloadError> {
        let failed = |e: reqwest::Error| DownloadError::Failed(format!("{url}：{e}"));
        let mut resp = tokio::select! {
            _ = self.cancel.cancelled() => return Err(DownloadError::Cancelled),
            r = self.client.get(url).send() => r.map_err(failed)?,
        };
        if !resp.status().is_success() {
            return Err(DownloadError::Failed(format!(
                "{url}：HTTP {}",
                resp.status().as_u16()
            )));
        }
        let total = resp.content_length();
        loop {
            let chunk = tokio::select! {
                _ = self.cancel.cancelled() => return Err(DownloadError::Cancelled),
                c = resp.chunk() => c.map_err(failed)?,
            };
            match chunk {
                Some(bytes) => on_chunk(&bytes)?,
                None => return Ok(total),
            }
        }
    }
}

/// 进度节流：距上次发布不足 500ms 就跳过（契约 ~500ms）。
/// `now` 由调用方传入，纯逻辑可直接测；首次（`last_emit == None`）必发。
struct Progress {
    total: u64,
    /// 已下完文件的字节累计（当前文件内的增量由调用方加在 `done` 上）。
    completed: u64,
    last_emit: Option<Instant>,
    /// 已发布百分比的高水位：换源重试时当前文件的 written 从 0 重计，done 回落，
    /// 发布值钳在高水位上，UI 进度条跨重试单调不减。
    high_water: u32,
}

impl Progress {
    fn new(total: u64) -> Self {
        Self {
            total,
            completed: 0,
            last_emit: None,
            high_water: 0,
        }
    }

    /// 该发进度时返回 Some(percent)，否则 None。
    fn tick(&mut self, now: Instant, done: u64) -> Option<u32> {
        if let Some(last) = self.last_emit
            && now.duration_since(last) < Duration::from_millis(500)
        {
            return None;
        }
        self.last_emit = Some(now);
        self.high_water = self.high_water.max(percent(done, self.total));
        Some(self.high_water)
    }
}

/// 总体百分比（两个文件合并计）。纯函数。
fn percent(done: u64, total: u64) -> u32 {
    if total == 0 {
        return 100;
    }
    ((done.min(total) as u128 * 100) / total as u128) as u32
}

fn downloading(percent: u32) -> VoiceModelStatus {
    VoiceModelStatus {
        state: "downloading".into(),
        percent,
        message: String::new(),
    }
}

/// 下载全部模型文件到 `dir`。调用方（ipc）负责先查 ready/正在下载并装好取消槽位。
/// 成功/失败/取消都会经 `publish` 发布终态；取消与失败都清掉 `.part`。
pub async fn download_model(
    dir: &Path,
    transport: &dyn ModelTransport,
    cancel: &CancellationToken,
    publish: &(dyn Fn(VoiceModelStatus) + Send + Sync),
) -> Result<(), DownloadError> {
    download_with_files(dir, &FILES, transport, cancel, publish).await
}

/// 终态发布与文件表分离：测试用迷你规格走同一条终态逻辑。
async fn download_with_files(
    dir: &Path,
    files: &[FileSpec],
    transport: &dyn ModelTransport,
    cancel: &CancellationToken,
    publish: &(dyn Fn(VoiceModelStatus) + Send + Sync),
) -> Result<(), DownloadError> {
    let result = download_files(dir, files, transport, cancel, publish).await;
    match &result {
        Ok(()) => publish(VoiceModelStatus {
            state: "ready".into(),
            percent: 0,
            message: String::new(),
        }),
        // 取消不是错误：回到磁盘真相（missing），用户可立即重试。
        Err(DownloadError::Cancelled) => publish(status_of(dir)),
        Err(DownloadError::Failed(message)) => publish(VoiceModelStatus {
            state: "error".into(),
            percent: 0,
            message: message.clone(),
        }),
    }
    result
}

/// `download_model` 的可测内核：文件表注入，测试用迷你规格，不搬 239MB。
async fn download_files(
    dir: &Path,
    files: &[FileSpec],
    transport: &dyn ModelTransport,
    cancel: &CancellationToken,
    publish: &(dyn Fn(VoiceModelStatus) + Send + Sync),
) -> Result<(), DownloadError> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| DownloadError::Failed(format!("无法创建模型目录：{e}")))?;
    publish(downloading(0));
    let total: u64 = files.iter().map(|f| f.bytes).sum();
    let mut progress = Progress::new(total);
    for spec in files {
        fetch_file(dir, spec, transport, cancel, &mut progress, publish).await?;
        progress.completed += spec.bytes;
        // 文件边界必发一次（不受节流约束）：小文件下完时用户能看到动静。
        progress.last_emit = None;
        if let Some(p) = progress.tick(Instant::now(), progress.completed) {
            publish(downloading(p));
        }
    }
    Ok(())
}

/// 下一个文件：镜像 → 官方逐源尝试；取消立即终止，其余错误换源重试该文件。
async fn fetch_file(
    dir: &Path,
    spec: &FileSpec,
    transport: &dyn ModelTransport,
    cancel: &CancellationToken,
    progress: &mut Progress,
    publish: &(dyn Fn(VoiceModelStatus) + Send + Sync),
) -> Result<(), DownloadError> {
    let mut last = String::new();
    for base in BASE_URLS {
        let url = format!("{base}/{MODEL_REPO}/resolve/main/{}", spec.name);
        match fetch_one(dir, spec, &url, transport, cancel, progress, publish).await {
            Ok(()) => return Ok(()),
            Err(DownloadError::Cancelled) => return Err(DownloadError::Cancelled),
            Err(DownloadError::Failed(e)) => last = e,
        }
    }
    Err(DownloadError::Failed(format!(
        "{} 下载失败（两个源都试过）：{last}",
        spec.name
    )))
}

/// 从单个 URL 下一个文件：写 `.part` → 尺寸校验 → 原子 rename。
/// 任何失败路径都删掉 `.part`，绝不留半个文件冒充成品。
async fn fetch_one(
    dir: &Path,
    spec: &FileSpec,
    url: &str,
    transport: &dyn ModelTransport,
    cancel: &CancellationToken,
    progress: &mut Progress,
    publish: &(dyn Fn(VoiceModelStatus) + Send + Sync),
) -> Result<(), DownloadError> {
    let part = dir.join(format!("{}.part", spec.name));
    let result = async {
        if cancel.is_cancelled() {
            return Err(DownloadError::Cancelled);
        }
        // 同步写盘是刻意的：chunk 回调是同步接缝，每块只有几十~几百 KB，
        // 且本任务独占一条下载任务，不占命令路径的 worker。
        let mut file = std::fs::File::create(&part)
            .map_err(|e| DownloadError::Failed(format!("无法创建临时文件：{e}")))?;
        let mut written = 0u64;
        let mut hasher = Sha256::new();
        let on_chunk = &mut |bytes: &[u8]| -> Result<(), DownloadError> {
            if cancel.is_cancelled() {
                return Err(DownloadError::Cancelled);
            }
            file.write_all(bytes)
                .map_err(|e| DownloadError::Failed(format!("写入临时文件失败：{e}")))?;
            hasher.update(bytes);
            written += bytes.len() as u64;
            if let Some(p) = progress.tick(Instant::now(), progress.completed + written) {
                publish(downloading(p));
            }
            Ok(())
        };
        transport.stream(url, on_chunk).await?;
        if written != spec.bytes {
            return Err(DownloadError::Failed(format!(
                "{} 尺寸校验失败：期望 {} 字节，实收 {written} 字节",
                spec.name, spec.bytes
            )));
        }
        let digest = format!("{:x}", hasher.finalize());
        if digest != spec.sha256 {
            return Err(DownloadError::Failed(format!(
                "{} 哈希校验失败：内容损坏（期望 sha256 {}…，实得 {}…）",
                spec.name,
                &spec.sha256[..12],
                &digest[..12]
            )));
        }
        file.flush()
            .map_err(|e| DownloadError::Failed(format!("写入临时文件失败：{e}")))?;
        drop(file);
        tokio::fs::rename(&part, dir.join(spec.name))
            .await
            .map_err(|e| DownloadError::Failed(format!("模型文件生效失败：{e}")))?;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&part).await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 迷你文件表：测试不搬 239MB。哈希分别是 "xxxx" 与 "xxxxxxxx" 的 sha256。
    const TINY: [FileSpec; 2] = [
        FileSpec {
            name: "tokens.txt",
            bytes: 4,
            sha256: "2481a63c85a62cf889d2b149f1a52e985a9341750173fe01eff50cc27b5941b5",
        },
        FileSpec {
            name: "model.int8.onnx",
            bytes: 8,
            sha256: "4cf0c1012276f46af31e44d2fbb03ae7af56f03c9996eb9452b99b3e6273698e",
        },
    ];

    /// fake transport：按 URL 决定行为。记录试过的 URL 顺序（镜像兜底测试用）。
    struct FakeTransport {
        /// URL 包含该子串时返回 Err（模拟源挂掉）。
        fail_host: Option<&'static str>,
        /// 每个文件实收的字节数（None = 按 URL 末段给足字节）。
        short_bytes: Option<u64>,
        /// 填充字节（默认 b'x' 匹配 TINY 哈希；改 b'y' 模拟同尺寸损坏）。
        content_byte: u8,
        urls: Mutex<Vec<String>>,
        /// 每块字节数。
        chunk: usize,
    }

    impl FakeTransport {
        fn new() -> Self {
            Self {
                fail_host: None,
                short_bytes: None,
                content_byte: b'x',
                urls: Mutex::new(Vec::new()),
                chunk: 3,
            }
        }
    }

    #[async_trait::async_trait]
    impl ModelTransport for FakeTransport {
        async fn stream(
            &self,
            url: &str,
            on_chunk: &mut (dyn for<'a> FnMut(&'a [u8]) -> Result<(), DownloadError> + Send),
        ) -> Result<Option<u64>, DownloadError> {
            self.urls.lock().unwrap().push(url.to_string());
            if let Some(host) = self.fail_host
                && url.contains(host)
            {
                return Err(DownloadError::Failed(format!("{url}：模拟源故障")));
            }
            let file = url.rsplit('/').next().unwrap();
            let want = TINY.iter().find(|f| f.name == file).unwrap().bytes;
            let want = self.short_bytes.unwrap_or(want);
            let mut sent = 0u64;
            while sent < want {
                let n = ((want - sent).min(self.chunk as u64)) as usize;
                on_chunk(&vec![self.content_byte; n])?;
                sent += n as u64;
            }
            Ok(Some(want))
        }
    }

    /// 收集发布出的状态序列。
    fn collector() -> (
        impl Fn(VoiceModelStatus) + Send + Sync,
        std::sync::Arc<Mutex<Vec<VoiceModelStatus>>>,
    ) {
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        (move |s| sink.lock().unwrap().push(s), seen)
    }

    async fn run(
        dir: &Path,
        transport: &dyn ModelTransport,
        cancel: &CancellationToken,
    ) -> (
        Result<(), DownloadError>,
        std::sync::Arc<Mutex<Vec<VoiceModelStatus>>>,
    ) {
        let (publish, seen) = collector();
        let result = download_files(dir, &TINY, transport, cancel, &publish).await;
        (result, seen)
    }

    #[test]
    fn percent_math_is_exact_and_clamped() {
        assert_eq!(percent(0, 12), 0);
        assert_eq!(percent(6, 12), 50);
        assert_eq!(percent(12, 12), 100);
        assert_eq!(percent(13, 12), 100, "超出总量钳在 100");
        assert_eq!(percent(0, 0), 100, "空表视为完成");
        assert_eq!(percent(1, 3), 33);
    }

    #[test]
    fn progress_throttles_to_500ms_but_first_tick_always_fires() {
        let mut p = Progress::new(100);
        let t0 = Instant::now();
        assert_eq!(p.tick(t0, 1), Some(1));
        assert_eq!(p.tick(t0 + Duration::from_millis(499), 50), None);
        assert_eq!(p.tick(t0 + Duration::from_millis(500), 50), Some(50));
        assert_eq!(p.tick(t0 + Duration::from_millis(900), 90), None);
    }

    #[test]
    fn status_of_missing_for_empty_wrong_size_or_corrupt() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        assert_eq!(status_of(dir).state, "missing");
        // 只有 tokens 不够。
        std::fs::write(dir.join("tokens.txt"), vec![0; 315_894]).unwrap();
        assert_eq!(status_of(dir).state, "missing");
        // 尺寸不对的 model 不算 ready（sparse 文件，不真写 239MB）。
        let f = std::fs::File::create(dir.join("model.int8.onnx")).unwrap();
        f.set_len(239_233_840).unwrap();
        assert_eq!(status_of(dir).state, "missing");
        // 尺寸恰好对、但内容是零（哈希不符）⇒ 仍 missing。这正是「同尺寸损坏模型」
        // 的回归：只校尺寸时这里会误判 ready，用户无法经 UI 重下。
        f.set_len(239_233_841).unwrap();
        assert_eq!(status_of(dir).state, "missing");
    }

    #[test]
    fn dir_ready_requires_exact_size_and_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        assert!(!dir_ready(dir, &TINY));
        // 内容正确（"xxxx"/"xxxxxxxx"）⇒ ready。
        std::fs::write(dir.join("tokens.txt"), b"xxxx").unwrap();
        std::fs::write(dir.join("model.int8.onnx"), b"xxxxxxxx").unwrap();
        assert!(dir_ready(dir, &TINY));
        // 同尺寸但内容损坏 ⇒ 不 ready。
        std::fs::write(dir.join("model.int8.onnx"), b"xxxxxxxy").unwrap();
        assert!(!dir_ready(dir, &TINY));
        // 尺寸不对 ⇒ 不 ready。
        std::fs::write(dir.join("model.int8.onnx"), b"xxx").unwrap();
        assert!(!dir_ready(dir, &TINY));
    }

    #[test]
    fn sha256_of_file_matches_known_vector() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("v");
        std::fs::write(&p, b"xxxx").unwrap();
        assert_eq!(
            sha256_of_file(&p).unwrap(),
            "2481a63c85a62cf889d2b149f1a52e985a9341750173fe01eff50cc27b5941b5"
        );
    }

    /// 缓存键 = 尺寸 + mtime：身份不变命中缓存（改坏内容也不重哈），身份变化必重校，
    /// 损坏判定绝不会被当成 ready。用独立 tempdir 路径，不与其他测试共享缓存键。
    #[test]
    fn hash_cache_hits_on_unchanged_identity_and_rehashes_on_change() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("model.int8.onnx");
        let spec = &TINY[1];
        std::fs::write(&path, b"xxxxxxxx").unwrap();
        assert!(spec_matches(&path, spec), "首次现算并入缓存");
        let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        // 内容改坏、尺寸不变、mtime 拨回原值 ⇒ 身份不变命中缓存，不重新哈希
        // （若重哈必发现损坏；这里仍返回 true 证明走的是缓存）。
        std::fs::write(&path, b"xxxxxxxy").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        assert!(spec_matches(&path, spec), "身份不变应当命中缓存");
        // mtime 变化 ⇒ 重校，损坏现形，且反复查都不许变成 ready。
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(mtime + Duration::from_secs(1))
            .unwrap();
        assert!(!spec_matches(&path, spec), "身份变化应当重哈希并发现损坏");
        assert!(!spec_matches(&path, spec), "损坏结果不许被缓存成 ready");
        // 换回正确内容（模拟重下的原子 rename 覆盖）⇒ 身份再变，重校通过。
        std::fs::write(&path, b"xxxxxxxx").unwrap();
        assert!(spec_matches(&path, spec), "覆盖后应当重校并通过");
        // 尺寸变化 ⇒ 直接不过。
        std::fs::write(&path, b"xxx").unwrap();
        assert!(!spec_matches(&path, spec));
        // 删除 ⇒ 不过；缓存里的旧判定不许借尸还魂。
        std::fs::remove_file(&path).unwrap();
        assert!(!spec_matches(&path, spec));
    }

    /// 换源重试时 done 回落（当前文件 written 从 0 重计），发布值钳在高水位。
    #[test]
    fn progress_clamps_to_high_water_across_retry() {
        let mut p = Progress::new(100);
        let t0 = Instant::now();
        assert_eq!(p.tick(t0, 80), Some(80));
        assert_eq!(
            p.tick(t0 + Duration::from_millis(500), 10),
            Some(80),
            "重试回落不许拉低已发布的百分比"
        );
        assert_eq!(p.tick(t0 + Duration::from_millis(1000), 90), Some(90));
    }

    #[tokio::test]
    async fn download_writes_part_then_renames_atomically_and_reports_progress() {
        let tmp = tempfile::tempdir().unwrap();
        let transport = FakeTransport::new();
        let cancel = CancellationToken::new();
        let (result, seen) = run(tmp.path(), &transport, &cancel).await;
        result.unwrap();
        // 成品在，.part 不在。
        assert_eq!(
            std::fs::read(tmp.path().join("tokens.txt")).unwrap(),
            b"xxxx"
        );
        assert_eq!(
            std::fs::read(tmp.path().join("model.int8.onnx")).unwrap(),
            b"xxxxxxxx"
        );
        assert!(!tmp.path().join("tokens.txt.part").exists());
        assert!(!tmp.path().join("model.int8.onnx.part").exists());
        // 状态序列：downloading(0) 开头，进度单调不减，全部 downloading。
        let seen = seen.lock().unwrap();
        assert_eq!(seen.first().unwrap().state, "downloading");
        assert_eq!(seen.first().unwrap().percent, 0);
        assert!(seen.iter().all(|s| s.state == "downloading"));
        assert!(seen.windows(2).all(|w| w[0].percent <= w[1].percent));
        assert_eq!(seen.last().unwrap().percent, 100);
        // tokens 先下（URL 顺序）。
        let urls = transport.urls.lock().unwrap();
        assert!(urls[0].ends_with("tokens.txt"));
        assert!(urls[1].ends_with("model.int8.onnx"));
        assert!(urls[0].starts_with("https://hf-mirror.com/"));
    }

    #[tokio::test]
    async fn a_failed_mirror_falls_back_to_the_official_host_per_file() {
        let tmp = tempfile::tempdir().unwrap();
        let transport = FakeTransport {
            fail_host: Some("hf-mirror.com"),
            ..FakeTransport::new()
        };
        let cancel = CancellationToken::new();
        let (result, _seen) = run(tmp.path(), &transport, &cancel).await;
        result.unwrap();
        let urls = transport.urls.lock().unwrap();
        assert_eq!(urls.len(), 4, "每个文件都是镜像失败 → 官方成功");
        assert!(urls[0].contains("hf-mirror.com") && urls[0].ends_with("tokens.txt"));
        assert!(urls[1].contains("huggingface.co") && urls[1].ends_with("tokens.txt"));
        assert!(urls[2].contains("hf-mirror.com") && urls[2].ends_with("model.int8.onnx"));
        assert!(urls[3].contains("huggingface.co") && urls[3].ends_with("model.int8.onnx"));
        assert!(tmp.path().join("model.int8.onnx").exists());
    }

    /// 回归（换源失败路径）：镜像把 model.int8.onnx 下到一半才断流，官方源重下成功。
    /// 块间刻意停 >500ms 节流窗，保证重试后的首个 tick 真的会发布 —— 不钳高水位时
    /// 它会从 83% 回落到 58%，进度条倒退。
    #[tokio::test]
    async fn retry_after_mid_stream_failure_never_regresses_progress() {
        struct MidStreamFailTransport;
        #[async_trait::async_trait]
        impl ModelTransport for MidStreamFailTransport {
            async fn stream(
                &self,
                url: &str,
                on_chunk: &mut (dyn for<'a> FnMut(&'a [u8]) -> Result<(), DownloadError> + Send),
            ) -> Result<Option<u64>, DownloadError> {
                let file = url.rsplit('/').next().unwrap();
                let want = TINY.iter().find(|f| f.name == file).unwrap().bytes;
                if url.contains("hf-mirror.com") && file == "model.int8.onnx" {
                    on_chunk(b"xxx")?;
                    tokio::time::sleep(Duration::from_millis(550)).await;
                    on_chunk(b"xxx")?;
                    tokio::time::sleep(Duration::from_millis(550)).await;
                    return Err(DownloadError::Failed(format!("{url}：模拟中途断流")));
                }
                let mut sent = 0u64;
                while sent < want {
                    let n = ((want - sent).min(3)) as usize;
                    on_chunk(&vec![b'x'; n])?;
                    sent += n as u64;
                }
                Ok(Some(want))
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let (publish, seen) = collector();
        download_files(
            tmp.path(),
            &TINY,
            &MidStreamFailTransport,
            &cancel,
            &publish,
        )
        .await
        .unwrap();
        let seen = seen.lock().unwrap();
        assert!(
            seen.windows(2).all(|w| w[0].percent <= w[1].percent),
            "换源重试后进度不许倒退：{seen:?}"
        );
        assert_eq!(seen.last().unwrap().percent, 100);
        assert!(tmp.path().join("model.int8.onnx").exists());
    }

    /// 回归（同尺寸损坏模型经 UI 重下）：盘上已有尺寸恰好对但内容损坏的文件时，
    /// dir_ready 判非 ready（UI 据此显示「下载模型」按钮），重下原子 rename 覆盖
    /// 坏文件，终态回 ready。
    #[tokio::test]
    async fn redownload_overwrites_corrupt_same_size_files_and_ends_ready() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("tokens.txt"), b"yyyy").unwrap();
        std::fs::write(tmp.path().join("model.int8.onnx"), b"yyyyyyyy").unwrap();
        assert!(!dir_ready(tmp.path(), &TINY), "同尺寸损坏必须判非 ready");
        let cancel = CancellationToken::new();
        let (publish, seen) = collector();
        download_with_files(tmp.path(), &TINY, &FakeTransport::new(), &cancel, &publish)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(tmp.path().join("tokens.txt")).unwrap(),
            b"xxxx"
        );
        assert_eq!(
            std::fs::read(tmp.path().join("model.int8.onnx")).unwrap(),
            b"xxxxxxxx"
        );
        assert!(dir_ready(tmp.path(), &TINY));
        assert_eq!(seen.lock().unwrap().last().unwrap().state, "ready");
    }

    #[tokio::test]
    async fn both_sources_failing_is_an_error_and_leaves_no_part() {
        let tmp = tempfile::tempdir().unwrap();
        let transport = FakeTransport {
            fail_host: Some("http"),
            ..FakeTransport::new()
        };
        let cancel = CancellationToken::new();
        let (result, _seen) = run(tmp.path(), &transport, &cancel).await;
        match result {
            Err(DownloadError::Failed(msg)) => assert!(msg.contains("两个源"), "{msg}"),
            other => panic!("应当失败：{other:?}"),
        }
        assert!(!tmp.path().join("tokens.txt.part").exists());
        assert!(!tmp.path().join("tokens.txt").exists());
    }

    #[tokio::test]
    async fn wrong_size_fails_verification_and_cleans_the_part_file() {
        let tmp = tempfile::tempdir().unwrap();
        let transport = FakeTransport {
            short_bytes: Some(2),
            ..FakeTransport::new()
        };
        let cancel = CancellationToken::new();
        let (result, _seen) = run(tmp.path(), &transport, &cancel).await;
        match result {
            Err(DownloadError::Failed(msg)) => assert!(msg.contains("尺寸校验"), "{msg}"),
            other => panic!("应当尺寸校验失败：{other:?}"),
        }
        assert!(!tmp.path().join("tokens.txt.part").exists());
        assert!(!tmp.path().join("tokens.txt").exists());
    }

    #[tokio::test]
    async fn wrong_content_right_size_fails_hash_and_cleans_the_part_file() {
        let tmp = tempfile::tempdir().unwrap();
        // 字节数给足（尺寸校验过），但内容是 'y' 不是 'x' ⇒ 哈希不符。
        let transport = FakeTransport {
            content_byte: b'y',
            ..FakeTransport::new()
        };
        let cancel = CancellationToken::new();
        let (result, _seen) = run(tmp.path(), &transport, &cancel).await;
        match result {
            Err(DownloadError::Failed(msg)) => assert!(msg.contains("哈希校验"), "{msg}"),
            other => panic!("应当哈希校验失败：{other:?}"),
        }
        // 第一个文件 tokens.txt 两个源都哈希不符即终止（不会轮到 model），不留 .part/成品。
        assert_eq!(transport.urls.lock().unwrap().len(), 2);
        assert!(!tmp.path().join("tokens.txt.part").exists());
        assert!(!tmp.path().join("tokens.txt").exists());
    }

    #[tokio::test]
    async fn cancel_before_start_leaves_nothing_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let transport = FakeTransport::new();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let (result, _seen) = run(tmp.path(), &transport, &cancel).await;
        assert!(matches!(result, Err(DownloadError::Cancelled)));
        assert!(!tmp.path().join("tokens.txt.part").exists());
        assert!(!tmp.path().join("tokens.txt").exists());
        assert!(
            transport.urls.lock().unwrap().is_empty(),
            "取消后不该发请求"
        );
    }

    #[tokio::test]
    async fn cancel_mid_stream_cleans_the_part_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        // 第一块写入后就取消：on_chunk 的取消闸在第二块触发。
        let seen_part = std::sync::Arc::new(Mutex::new(false));
        let part_flag = seen_part.clone();
        let dir = tmp.path().to_path_buf();
        let cancel2 = cancel.clone();
        let transport = CancellingTransport {
            on_first_chunk: Box::new(move || {
                *part_flag.lock().unwrap() = dir.join("tokens.txt.part").exists();
                cancel2.cancel();
            }),
        };
        let (result, _seen) = run(tmp.path(), &transport, &cancel).await;
        assert!(matches!(result, Err(DownloadError::Cancelled)));
        assert!(*seen_part.lock().unwrap(), "下载中应当存在 .part");
        assert!(
            !tmp.path().join("tokens.txt.part").exists(),
            "取消后 .part 必须清掉"
        );
        assert!(!tmp.path().join("tokens.txt").exists());
    }

    /// 第一块回调时触发取消的 transport。
    struct CancellingTransport {
        on_first_chunk: Box<dyn Fn() + Send + Sync>,
    }

    #[async_trait::async_trait]
    impl ModelTransport for CancellingTransport {
        async fn stream(
            &self,
            _url: &str,
            on_chunk: &mut (dyn for<'a> FnMut(&'a [u8]) -> Result<(), DownloadError> + Send),
        ) -> Result<Option<u64>, DownloadError> {
            on_chunk(b"xx")?;
            (self.on_first_chunk)();
            on_chunk(b"xx")?;
            Ok(Some(4))
        }
    }

    #[tokio::test]
    async fn terminal_states_are_published_for_ready_error_and_cancel() {
        let tmp = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let (publish, seen) = collector();
        // ready：迷你文件表全下完。
        download_with_files(tmp.path(), &TINY, &FakeTransport::new(), &cancel, &publish)
            .await
            .unwrap();
        assert_eq!(seen.lock().unwrap().last().unwrap().state, "ready");
        // error：两个源都挂。
        let transport = FakeTransport {
            fail_host: Some("http"),
            ..FakeTransport::new()
        };
        let result = download_with_files(tmp.path(), &TINY, &transport, &cancel, &publish).await;
        assert!(matches!(result, Err(DownloadError::Failed(_))));
        let last = seen.lock().unwrap().last().unwrap().clone();
        assert_eq!(last.state, "error");
        assert!(!last.message.is_empty());
        assert_eq!(last.percent, 0);
        // cancel：回到磁盘真相。此时目录里已有上一次 ready 留下的成品 ⇒ ready；
        // 换一个空目录验证 missing。
        let empty = tempfile::tempdir().unwrap();
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let result = download_with_files(
            empty.path(),
            &TINY,
            &FakeTransport::new(),
            &cancelled,
            &publish,
        )
        .await;
        assert!(matches!(result, Err(DownloadError::Cancelled)));
        assert_eq!(seen.lock().unwrap().last().unwrap().state, "missing");
    }

    /// 回归：对端 TCP 存活但应用静默（镜像反代源站 stall）⇒ 必须在 read_timeout 内
    /// 返回 Err 触发换源，而不是永久挂起。修复前客户端无任何超时，resp.chunk()（乃至
    /// 等响应头）会一直挂着。需要真实本地静默对端、耗时约 30s（read_timeout），手动跑：
    /// ```text
    /// cargo test --manifest-path src-tauri/Cargo.toml voice::download -- --ignored
    /// ```
    #[tokio::test]
    #[ignore = "需要真实静默对端，耗时约 30s（read_timeout），手动运行"]
    async fn stalled_peer_times_out_instead_of_hanging() {
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // 接受连接但一个字节都不发、也不关闭：模拟「TCP 存活、应用静默」。
        let server = tokio::spawn(async move {
            let (_sock, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        let transport = ReqwestModelTransport::new(CancellationToken::new()).unwrap();
        let start = Instant::now();
        let result = transport
            .stream(
                &format!("http://{addr}/model.int8.onnx"),
                &mut |_bytes: &[u8]| Ok(()),
            )
            .await;
        assert!(
            matches!(result, Err(DownloadError::Failed(_))),
            "静默对端应当超时失败：{result:?}"
        );
        assert!(
            start.elapsed() < Duration::from_secs(45),
            "应当在 read_timeout 内返回，而不是挂起：{:?}",
            start.elapsed()
        );
        server.abort();
    }
}
