//! Windows 离线语音引擎：cpal 采麦克风 + ort SenseVoice 识别（自研前处理/解码）。
//!
//! 线程合同（与 SAPI 时代同形）：`start()` 起一条专用线程，cpal 流只在那条
//! 线程上创建/使用/释放；`VoiceSession` 只持 `Sender<Command>` 与 `JoinHandle`。
//! 音频回调里只做格式转换 + 降单声道 + 发送，不做识别；识别在 Stop 之后一次性进行。
//! 模型经 `asr::cached` 进程级常驻（首次加载 ~1s，不每次说话都付），
//! ready 后 `asr::prewarm` 后台加载，把首次成本掩盖在说话时间里。

use cpal::Sample;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// 给语音线程的命令。只有两条：收尾（识别并保留文本）与取消（丢弃）。
enum Command {
    Stop,
    Cancel,
}

/// `start()` 返回给命令层的句柄。**不持有任何设备/引擎对象**：它们全在语音线程里，
/// 这里只有跨线程通信用的 `Sender` 和等待收尾用的 `JoinHandle`。
pub struct VoiceSession {
    commands: Option<Sender<Command>>,
    handle: Option<JoinHandle<Result<String, String>>>,
}

impl VoiceSession {
    /// 停止录音，等语音线程识别收尾并返回最终文本（已 `trim`，允许空串）。
    pub fn stop(mut self) -> Result<String, String> {
        // 发送失败说明线程已经退出；join 才能拿到它真正的结果/错误。
        let _ = self.send(Command::Stop);
        self.finish()
    }

    /// 取消录音并丢弃文本。没有正在进行的识别时也是成功。
    pub fn cancel(mut self) -> Result<(), String> {
        let _ = self.send(Command::Cancel);
        self.finish().map(|_| ())
    }

    fn send(&self, command: Command) -> Result<(), String> {
        self.commands
            .as_ref()
            .ok_or_else(|| "语音会话已结束。".to_string())?
            .send(command)
            .map_err(|_| "语音识别线程已退出。".to_string())
    }

    fn finish(&mut self) -> Result<String, String> {
        let handle = self
            .handle
            .take()
            .ok_or_else(|| "语音会话已结束。".to_string())?;
        self.commands.take();
        handle
            .join()
            .map_err(|_| "语音识别线程异常退出。".to_string())?
    }
}

/// `VoiceSession` 被丢弃时（前端没来得及 stop/cancel、命令层早退等）必须取消：
/// 否则专用线程会一直握着麦克风。这里只尽力发送，join 会等线程真的释放设备。
impl Drop for VoiceSession {
    fn drop(&mut self) {
        if let Some(commands) = self.commands.take() {
            let _ = commands.send(Command::Cancel);
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// 起一条专用语音线程并等它完成初始化（默认输入设备 + 开流）。
/// 初始化失败通过 `ready` 回传，所以 `Err` 不会在用户点了麦克风之后才以事件形式出现。
/// 模型未就绪由命令层先拦；ready 后后台预热模型，不阻塞采集。
pub fn start(model_dir: &Path) -> Result<VoiceSession, String> {
    let model_dir = model_dir.to_path_buf();
    let (commands, command_rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let handle = thread::Builder::new()
        .name("voice-engine".to_string())
        .spawn(move || run(model_dir, command_rx, ready_tx))
        .map_err(|e| format!("无法启动语音识别线程：{e}"))?;

    match ready_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(())) => Ok(VoiceSession {
            commands: Some(commands),
            handle: Some(handle),
        }),
        Ok(Err(e)) => {
            let _ = handle.join();
            Err(e)
        }
        Err(RecvTimeoutError::Timeout) => {
            let _ = commands.send(Command::Cancel);
            // 不 join：Cancel 只在 run() 进入命令循环后才被消费，卡在 open_capture()
            // （正是超时想防御的场景）时 join 会无限期阻塞。JoinHandle drop 即 detach；
            // 线程开流成功后 ready.send 失败会自行 drop(stream) 退出（见 run()）。
            Err("语音识别初始化超时（10 秒）。请确认麦克风可用后重试。".to_string())
        }
        Err(RecvTimeoutError::Disconnected) => {
            let _ = handle.join();
            Err("语音识别线程在初始化时意外退出。".to_string())
        }
    }
}

/// 后端录音兜底上限：前端 120s 计时（Composer 的 VOICE_MAX_SECONDS）正常会先停，
/// 此闸只在前端失联/异常时兜底，防 samples 缓冲无界增长。到点走与 Command::Stop
/// 相同的识别收尾路径，前端迟到的 voice_stop 仍能 join 拿到结果。
const MAX_RECORDING_DURATION: Duration = Duration::from_secs(150);

/// 专用线程入口：开流 → 报 ready → 攒音频 → Stop 时识别。
fn run(
    model_dir: PathBuf,
    commands: Receiver<Command>,
    ready: SyncSender<Result<(), String>>,
) -> Result<String, String> {
    let (stream, sample_rate, audio_rx, error_rx) = match open_capture() {
        Ok(parts) => parts,
        Err(e) => {
            let _ = ready.send(Err(e.clone()));
            return Err(e);
        }
    };
    if ready.send(Ok(())).is_err() {
        // 启动方已经走了（超时等）：直接退出，别让麦克风挂在一条没人等的线程上。
        drop(stream);
        return Ok(String::new());
    }
    // 后台预热模型（首次加载 ~1s），不阻塞采集；错误在 stop 识别时会再报。
    super::asr::prewarm(&model_dir);

    let mut samples: Vec<f32> = Vec::new();
    let deadline = Instant::now() + MAX_RECORDING_DURATION;
    loop {
        // 回调只发送，这里攒成一条连续的 mono f32 缓冲。
        while let Ok(chunk) = audio_rx.try_recv() {
            samples.extend_from_slice(&chunk);
        }
        if let Ok(message) = error_rx.try_recv() {
            drop(stream);
            return Err(message);
        }
        let stop = match commands.recv_timeout(Duration::from_millis(20)) {
            Ok(Command::Stop) => true,
            Ok(Command::Cancel) => {
                drop(stream);
                return Ok(String::new());
            }
            // 兜底上限到点与 Stop 同路（见 MAX_RECORDING_DURATION），不丢已录下的音频。
            Err(RecvTimeoutError::Timeout) => Instant::now() >= deadline,
            Err(RecvTimeoutError::Disconnected) => {
                // 句柄被丢弃但 Drop 的 Cancel 没送到：按取消处理，绝不把麦克风留在活动状态。
                drop(stream);
                return Ok(String::new());
            }
        };
        if stop {
            drop(stream); // 停采：Stream 析构即释放设备。
            while let Ok(chunk) = audio_rx.try_recv() {
                samples.extend_from_slice(&chunk);
            }
            return recognize(&model_dir, sample_rate, &samples);
        }
    }
}

/// 采集流打开后的四件套：流本体、设备原始采样率、音频接收端、错误接收端。
type CaptureParts = (cpal::Stream, u32, Receiver<Vec<f32>>, Receiver<String>);

/// 打开默认输入设备的采集流。返回 (流, 设备原始采样率, 音频接收端, 错误接收端)。
fn open_capture() -> Result<CaptureParts, String> {
    let host = cpal::default_host();
    let device = host.default_input_device().ok_or_else(no_microphone)?;
    let config = device
        .default_input_config()
        .map_err(|e| format!("无法读取麦克风配置：{e}"))?;
    // cpal 0.18 起 SampleRate 就是 u32 别名（不再是带 .0 的新类型）。
    let sample_rate = config.sample_rate();
    let channels = config.channels() as usize;
    let (audio_tx, audio_rx) = mpsc::channel::<Vec<f32>>();
    let (error_tx, error_rx) = mpsc::channel::<String>();

    let err_tx = error_tx.clone();
    let stream = build_stream(&device, config, channels, audio_tx, move |e| {
        // 设备中途被拔/权限被收回：把可行动文案送回语音线程，由它结束会话。
        let _ = err_tx.send(capture_error(&e));
    })?;
    stream.play().map_err(|e| capture_error(&e))?;
    Ok((stream, sample_rate, audio_rx, error_rx))
}

/// 按设备默认格式建流。WASAPI 默认通常是 f32；其余整数/浮点格式逐样本转 f32。
/// 不支持的格式（I24/U24/DSD 等）明确报错，不静默按错格式解读字节。
fn build_stream<E>(
    device: &cpal::Device,
    config: cpal::SupportedStreamConfig,
    channels: usize,
    audio_tx: Sender<Vec<f32>>,
    on_error: E,
) -> Result<cpal::Stream, String>
where
    E: FnMut(cpal::Error) + Send + 'static,
{
    fn build<T, E>(
        device: &cpal::Device,
        config: cpal::SupportedStreamConfig,
        channels: usize,
        audio_tx: Sender<Vec<f32>>,
        on_error: E,
    ) -> Result<cpal::Stream, String>
    where
        T: cpal::SizedSample,
        f32: cpal::FromSample<T>,
        E: FnMut(cpal::Error) + Send + 'static,
    {
        device
            .build_input_stream(
                config.config(),
                move |data: &[T], _| {
                    // 回调纪律：只转换 + 降单声道 + 发送，不做识别、不分配大对象。
                    let mono = to_mono_f32(data, channels);
                    let _ = audio_tx.send(mono);
                },
                on_error,
                None,
            )
            .map_err(|e| capture_error(&e))
    }
    let format = config.sample_format();
    match format {
        cpal::SampleFormat::F32 => build::<f32, _>(device, config, channels, audio_tx, on_error),
        cpal::SampleFormat::F64 => build::<f64, _>(device, config, channels, audio_tx, on_error),
        cpal::SampleFormat::I8 => build::<i8, _>(device, config, channels, audio_tx, on_error),
        cpal::SampleFormat::I16 => build::<i16, _>(device, config, channels, audio_tx, on_error),
        cpal::SampleFormat::I32 => build::<i32, _>(device, config, channels, audio_tx, on_error),
        cpal::SampleFormat::I64 => build::<i64, _>(device, config, channels, audio_tx, on_error),
        cpal::SampleFormat::U8 => build::<u8, _>(device, config, channels, audio_tx, on_error),
        cpal::SampleFormat::U16 => build::<u16, _>(device, config, channels, audio_tx, on_error),
        cpal::SampleFormat::U32 => build::<u32, _>(device, config, channels, audio_tx, on_error),
        cpal::SampleFormat::U64 => build::<u64, _>(device, config, channels, audio_tx, on_error),
        _ => Err(format!(
            "不支持的麦克风采样格式：{format}。请在系统声音设置中更换默认输入设备后重试"
        )),
    }
}

/// 交错多声道 → 单声道 f32（逐帧平均）。单声道时就是逐样本转换。
fn to_mono_f32<T>(data: &[T], channels: usize) -> Vec<f32>
where
    T: cpal::Sample,
    f32: cpal::FromSample<T>,
{
    if channels <= 1 {
        return data.iter().map(|&s| f32::from_sample(s)).collect();
    }
    data.chunks_exact(channels)
        .map(|frame| {
            let sum: f32 = frame.iter().map(|&s| f32::from_sample(s)).sum();
            sum / channels as f32
        })
        .collect()
}

/// 停采后的一次性识别：取缓存模型（首次现加载，管线内部重采样到 16k）→
/// 推理 → CTC 解码 → trim。空录音不碰模型，直接返回空串。
fn recognize(model_dir: &Path, sample_rate: u32, samples: &[f32]) -> Result<String, String> {
    if samples.is_empty() {
        return Ok(String::new());
    }
    let model = create_recognizer(model_dir)?;
    // SenseVoice 自动识别中/英/日/韩/粤；use_itn 开模型自带标点/ITN。
    model.recognize(samples, sample_rate, true)
}

/// 加载 SenseVoice 模型（进程级缓存）。探针（examples/speech_probe.rs）也走这里。
pub fn probe_recognizer(model_dir: &Path) -> Result<(), String> {
    create_recognizer(model_dir).map(|_| ())
}

fn create_recognizer(model_dir: &Path) -> Result<std::sync::Arc<super::asr::Model>, String> {
    super::asr::cached(model_dir)
}

/// 列出当前可用的输入设备名（探针用；空表 = 没有麦克风）。
pub fn input_devices() -> Result<Vec<String>, String> {
    let host = cpal::default_host();
    let devices = host
        .input_devices()
        .map_err(|e| format!("无法枚举音频输入设备：{e}"))?;
    let mut names = Vec::new();
    for device in devices {
        names.push(
            device
                .description()
                .map(|d| d.name().to_string())
                .unwrap_or_else(|_| "（无法读取设备名）".to_string()),
        );
    }
    Ok(names)
}

fn no_microphone() -> String {
    "没有检测到可用的麦克风（无默认录音设备）。请连接麦克风，并在系统声音设置中把它设为默认输入设备"
        .into()
}

/// cpal 错误 → 可行动中文。未知错误带原文，方便定位。
fn capture_error(e: &cpal::Error) -> String {
    match e.kind() {
        cpal::ErrorKind::DeviceNotAvailable | cpal::ErrorKind::HostUnavailable => no_microphone(),
        cpal::ErrorKind::PermissionDenied => {
            "麦克风权限被拒绝：请在 Windows 设置 → 隐私和安全性 → 麦克风中，允许桌面应用访问麦克风"
                .into()
        }
        cpal::ErrorKind::DeviceBusy => {
            "麦克风被其他应用占用。请关闭正在使用麦克风的应用后重试".into()
        }
        _ => format!("语音采集失败：{e}"),
    }
}
