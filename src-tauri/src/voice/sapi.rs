//! Windows SAPI 5 实现。
//!
//! 事件所有权与 20ms 轮询的思路参考 MIT 许可的 robius
//! （<https://github.com/project-robius/robius>，`crates/speech/src/windows.rs`，
//! MIT License）：`ISpRecoContext::GetEvents` 把每个事件的 `lParam` **所有权**交给调用方，
//! 必须按 `elParamType` 用 `IUnknown::from_raw` / `ISpObjectToken::from_raw` /
//! `CoTaskMemFree` 恰好释放一次，否则麦克风会话会泄漏 COM 引用。
//!
//! 线程合同：`start()` 的线程是唯一创建/使用/释放 COM 接口的地方；`VoiceSession`
//! 只持 `Sender<Command>` 与 `JoinHandle`。`probe()` 同样在自己的线程里初始化 MTA COM。

use super::{
    FailureStage, ProbeReport, RecognizerInfo, SPERR_NOT_FOUND, append_transcript,
    describe_failure, parse_language_attribute,
};
use std::ffi::c_void;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use windows::Win32::Globalization::{GetUserDefaultLocaleName, LocaleNameToLCID};
use windows::Win32::Media::Speech::{
    ISpObjectToken, ISpObjectTokenCategory, ISpRecoContext, ISpRecoGrammar, ISpRecoResult,
    ISpRecognizer, SPCAT_AUDIOIN, SPCAT_RECOGNIZERS, SPEI_END_SR_STREAM, SPEI_FALSE_RECOGNITION,
    SPEI_RECOGNITION, SPEI_RESERVED1, SPEI_RESERVED2, SPET_LPARAM_IS_OBJECT,
    SPET_LPARAM_IS_POINTER, SPET_LPARAM_IS_STRING, SPET_LPARAM_IS_TOKEN, SPEVENT, SPLO_STATIC,
    SPRS_ACTIVE, SPRST_ACTIVE, SPRST_INACTIVE, SPRST_INACTIVE_WITH_PURGE, SpInprocRecognizer,
    SpObjectToken, SpObjectTokenCategory,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize,
};
use windows::core::{IUnknown, Interface, PCWSTR, PWSTR};

/// 给语音线程的命令。只有两条：收尾（保留文本）与取消（丢弃）。
enum Command {
    Stop,
    Cancel,
}

/// `start()` 返回给命令层的句柄。**不持有任何 COM 对象**：接口全在语音线程里，
/// 这里只有跨线程通信用的 `Sender` 和等待收尾用的 `JoinHandle`。
pub struct VoiceSession {
    commands: Option<Sender<Command>>,
    handle: Option<JoinHandle<Result<String, String>>>,
}

impl VoiceSession {
    /// 停止录音，等语音线程收尾并返回累积文本（已 `trim`）。
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

/// 起一条专用语音线程并等它完成初始化。
///
/// 初始化（COM、识别器、默认麦克风、语法、`SPRST_ACTIVE`）全部在失败时通过
/// `ready` 回传，所以 `Err` 不会在用户点了麦克风之后才以事件的形式出现。
pub fn start(language: &str) -> Result<VoiceSession, String> {
    let language = language.trim().to_string();
    let (commands, command_rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let handle = thread::Builder::new()
        .name("voice-sapi".to_string())
        .spawn(move || run(language, command_rx, ready_tx))
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
            let _ = handle.join();
            Err("语音识别初始化超时（10 秒）。请确认麦克风可用后重试。".to_string())
        }
        Err(RecvTimeoutError::Disconnected) => {
            let _ = handle.join();
            Err("语音识别线程在初始化时意外退出。".to_string())
        }
    }
}

/// 专用线程的入口。`_com` 必须第一个声明，这样它最后析构（所有接口先释放）。
fn run(
    language: String,
    commands: Receiver<Command>,
    ready: SyncSender<Result<(), String>>,
) -> Result<String, String> {
    let _com = match ComApartment::new() {
        Ok(com) => com,
        Err(e) => {
            let _ = ready.send(Err(e.clone()));
            return Err(e);
        }
    };

    let (recognizer, context, _grammar) = match setup(&language) {
        Ok(parts) => parts,
        Err(e) => {
            let _ = ready.send(Err(e.clone()));
            return Err(e);
        }
    };

    if ready.send(Ok(())).is_err() {
        // 启动方已经走了（超时等）：直接取消，别让麦克风挂在一条没人等的线程上。
        let _ = unsafe { recognizer.SetRecoState(SPRST_INACTIVE_WITH_PURGE) };
        return Ok(String::new());
    }

    event_loop(&recognizer, &context, commands)
}

/// 启动链：识别器 →（可选）按语言选 token → 默认音频输入 → 上下文 → 事件 → 语法。
fn setup(language: &str) -> Result<(ISpRecognizer, ISpRecoContext, ISpRecoGrammar), String> {
    let recognizer: ISpRecognizer =
        unsafe { CoCreateInstance(&SpInprocRecognizer, None::<&IUnknown>, CLSCTX_INPROC_SERVER) }
            .map_err(|e| map_error(FailureStage::Recognizer, e))?;

    // `"system"` 不调用 SetRecognizer：交给系统默认的识别器。
    if let Some(token) = select_recognizer(language)? {
        unsafe { recognizer.SetRecognizer(&token) }
            .map_err(|e| map_error(FailureStage::Recognizer, e))?;
    }

    let input = default_audio_input()?;
    unsafe { recognizer.SetInput(&input, true) }
        .map_err(|e| map_error(FailureStage::Microphone, e))?;

    let context = unsafe { recognizer.CreateRecoContext() }
        .map_err(|e| map_error(FailureStage::Recognizer, e))?;
    let mask = interest_mask();
    unsafe { context.SetInterest(mask, mask) }
        .map_err(|e| map_error(FailureStage::Recognizer, e))?;

    let grammar =
        unsafe { context.CreateGrammar(1) }.map_err(|e| map_error(FailureStage::Recognizer, e))?;
    unsafe { grammar.LoadDictation(PCWSTR::null(), SPLO_STATIC) }
        .map_err(|e| map_error(FailureStage::Recognizer, e))?;
    unsafe { grammar.SetDictationState(SPRS_ACTIVE) }
        .map_err(|e| map_error(FailureStage::Recognizer, e))?;
    unsafe { recognizer.SetRecoState(SPRST_ACTIVE) }
        .map_err(|e| map_error(FailureStage::Recognizer, e))?;

    Ok((recognizer, context, grammar))
}

/// 只订阅 v1 需要的三个最终事件。`SPFEI(id)` 就是 `1 << id`。
///
/// `SPEI_RESERVED1` / `SPEI_RESERVED2` 必须一起打开：SAPI 的 `SetInterest` 文档
/// 要求这两个内部事件位，缺了会在部分 Windows 版本上以 `E_INVALIDARG` 拒绝整张掩码。
fn interest_mask() -> u64 {
    let bit = |id: i32| 1u64 << (id as u32);
    bit(SPEI_RECOGNITION.0)
        | bit(SPEI_FALSE_RECOGNITION.0)
        | bit(SPEI_END_SR_STREAM.0)
        | bit(SPEI_RESERVED1.0)
        | bit(SPEI_RESERVED2.0)
}

/// 事件循环：20ms 轮询一次命令，期间把队列里的事件全部取空。
fn event_loop(
    recognizer: &ISpRecognizer,
    context: &ISpRecoContext,
    commands: Receiver<Command>,
) -> Result<String, String> {
    let mut transcript = String::new();
    let mut stopping = false;
    let mut deadline: Option<Instant> = None;

    loop {
        // `GetEvents` 在队列为空时返回 S_FALSE；windows 的 `.ok()` 把 >= 0 都算成功。
        loop {
            let mut event = SPEVENT::default();
            let mut fetched = 0u32;
            unsafe { context.GetEvents(1, &mut event, &mut fetched) }
                .map_err(|e| map_error(FailureStage::Recognizer, e))?;
            if fetched == 0 {
                break;
            }
            let event_id = event._bitfield & 0xFFFF;
            if event_id == SPEI_END_SR_STREAM.0 {
                // END_SR_STREAM 的 lParam 是 HRESULT 本身，不是 CoTaskMem 指针：
                // 绝不能交给 `consume_event` 释放。失败时把引擎给的原因翻译出来。
                let hr = event.lParam.0 as i32;
                if hr < 0 {
                    return Err(describe_failure(FailureStage::Recognizer, hr));
                }
                // 没收到 Stop 就结束（设备被拔、引擎自停）：会话已经不可继续，
                // 把已识别的文本交回去，别在原地空转。
                return Ok(transcript.trim().to_string());
            }
            consume_event(&event, &mut transcript);
        }

        match commands.recv_timeout(Duration::from_millis(20)) {
            Ok(Command::Stop) => {
                if !stopping {
                    unsafe { recognizer.SetRecoState(SPRST_INACTIVE) }
                        .map_err(|e| map_error(FailureStage::Recognizer, e))?;
                    stopping = true;
                    deadline = Some(Instant::now() + Duration::from_secs(5));
                }
            }
            Ok(Command::Cancel) => {
                let _ = unsafe { recognizer.SetRecoState(SPRST_INACTIVE_WITH_PURGE) };
                return Ok(String::new());
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                // 句柄被丢弃但 Drop 的 Cancel 没送到（例如命令层直接丢了 Sender）：
                // 按取消处理，绝不把麦克风留在活动状态。
                let _ = unsafe { recognizer.SetRecoState(SPRST_INACTIVE_WITH_PURGE) };
                return Ok(String::new());
            }
        }

        // 停止后最多等 5 秒 SPEI_END_SR_STREAM；等不到就把已识别文本交回去。
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Ok(transcript.trim().to_string());
        }
    }
}

/// 处理一个事件并**恰好释放一次**它的 `lParam`。
fn consume_event(event: &SPEVENT, transcript: &mut String) {
    let kind = (event._bitfield >> 16) & 0xFFFF;
    let pointer = event.lParam.0 as *mut c_void;
    if pointer.is_null() {
        return;
    }
    let event_id = event._bitfield & 0xFFFF;

    if kind == SPET_LPARAM_IS_OBJECT.0 {
        // 取所有权；`object` 析构时 Release 一次。
        let object = unsafe { IUnknown::from_raw(pointer) };
        if event_id == SPEI_RECOGNITION.0
            && let Ok(result) = object.cast::<ISpRecoResult>()
            && let Some(text) = result_text(&result)
        {
            append_transcript(transcript, &text);
        }
    } else if kind == SPET_LPARAM_IS_TOKEN.0 {
        drop(unsafe { ISpObjectToken::from_raw(pointer) });
    } else if kind == SPET_LPARAM_IS_POINTER.0 || kind == SPET_LPARAM_IS_STRING.0 {
        unsafe { CoTaskMemFree(Some(pointer as *const c_void)) };
    }
    // 其余类型（含 UNDEFINED）不拥有指针，什么也不做。
}

/// 取最终识别文本。`GetText` 的返回值同样由 `CoTaskMemFree` 释放。
fn result_text(result: &ISpRecoResult) -> Option<String> {
    let mut text = PWSTR::null();
    unsafe { result.GetText(0, u32::MAX, true, &mut text, None) }.ok()?;
    if text.0.is_null() {
        return None;
    }
    Some(take_pwstr(text))
}

/// 按语言选识别器 token。找不到时给「缺语言包」那条中文错误。
fn select_recognizer(language: &str) -> Result<Option<ISpObjectToken>, String> {
    let language = language.trim();
    if language == "system" {
        return Ok(None);
    }
    let target = locale_lcid(language)?;

    let category = token_category(SPCAT_RECOGNIZERS, FailureStage::Recognizer)?;
    let tokens = unsafe { category.EnumTokens(PCWSTR::null(), PCWSTR::null()) }
        .map_err(|e| map_error(FailureStage::Recognizer, e))?;
    let mut count = 0u32;
    unsafe { tokens.GetCount(&mut count) }.map_err(|e| map_error(FailureStage::Recognizer, e))?;

    for index in 0..count {
        let token =
            unsafe { tokens.Item(index) }.map_err(|e| map_error(FailureStage::Recognizer, e))?;
        let Some(value) = read_token_value(&token, "Language") else {
            continue;
        };
        if parse_language_attribute(&value) == Some(target) {
            return Ok(Some(token));
        }
    }
    Err(describe_failure(FailureStage::Recognizer, SPERR_NOT_FOUND))
}

/// `LocaleNameToLCID` 是语言名 → LCID 的唯一转换；0 表示系统不认识这个语言。
fn locale_lcid(language: &str) -> Result<u32, String> {
    let name = wide(language);
    let lcid = unsafe { LocaleNameToLCID(PCWSTR(name.as_ptr()), 0) };
    if lcid == 0 {
        Err(describe_failure(FailureStage::Recognizer, SPERR_NOT_FOUND))
    } else {
        Ok(lcid)
    }
}

/// `SPCAT_AUDIOIN` 的默认 token —— 系统当前的默认录音设备。
fn default_audio_input() -> Result<IUnknown, String> {
    let category = token_category(SPCAT_AUDIOIN, FailureStage::Microphone)?;
    let token_id = unsafe { category.GetDefaultTokenId() }
        .map_err(|e| map_error(FailureStage::Microphone, e))?;
    if token_id.0.is_null() {
        return Err(describe_failure(FailureStage::Microphone, SPERR_NOT_FOUND));
    }

    let token: ISpObjectToken =
        unsafe { CoCreateInstance(&SpObjectToken, None::<&IUnknown>, CLSCTX_INPROC_SERVER) }
            .map_err(|e| map_error(FailureStage::Microphone, e))?;
    let set_id = unsafe { token.SetId(SPCAT_AUDIOIN, PCWSTR(token_id.0), false) };
    // `GetDefaultTokenId` 的字符串是 CoTaskMem 分配的，无论 SetId 成败都要还回去。
    unsafe { CoTaskMemFree(Some(token_id.0 as *const c_void)) };
    set_id.map_err(|e| map_error(FailureStage::Microphone, e))?;

    token
        .cast::<IUnknown>()
        .map_err(|e| map_error(FailureStage::Microphone, e))
}

fn token_category(id: PCWSTR, stage: FailureStage) -> Result<ISpObjectTokenCategory, String> {
    let category: ISpObjectTokenCategory = unsafe {
        CoCreateInstance(
            &SpObjectTokenCategory,
            None::<&IUnknown>,
            CLSCTX_INPROC_SERVER,
        )
    }
    .map_err(|e| map_error(stage, e))?;
    unsafe { category.SetId(id, false) }.map_err(|e| map_error(stage, e))?;
    Ok(category)
}

/// 读 token 的字符串属性（`Description` / `Language`），并把 COM 分配的缓冲区还掉。
fn read_token_value(token: &ISpObjectToken, name: &str) -> Option<String> {
    let name = wide(name);
    let value = unsafe { token.GetStringValue(PCWSTR(name.as_ptr())) }.ok()?;
    Some(take_pwstr(value))
}

/// 枚举 `SPCAT_RECOGNIZERS` 与 zh-CN 试探。**不启动麦克风采集。**
pub fn probe() -> Result<ProbeReport, String> {
    thread::Builder::new()
        .name("voice-probe".to_string())
        .spawn(probe_on_thread)
        .map_err(|e| format!("无法启动语音探测线程：{e}"))?
        .join()
        .map_err(|_| "语音探测线程异常退出。".to_string())?
}

fn probe_on_thread() -> Result<ProbeReport, String> {
    let _com = ComApartment::new()?;
    let default_language = system_default_language();

    let category = token_category(SPCAT_RECOGNIZERS, FailureStage::Recognizer)?;
    let tokens = unsafe { category.EnumTokens(PCWSTR::null(), PCWSTR::null()) }
        .map_err(|e| map_error(FailureStage::Recognizer, e))?;
    let mut count = 0u32;
    unsafe { tokens.GetCount(&mut count) }.map_err(|e| map_error(FailureStage::Recognizer, e))?;

    let mut recognizers = Vec::with_capacity(count as usize);
    for index in 0..count {
        let token =
            unsafe { tokens.Item(index) }.map_err(|e| map_error(FailureStage::Recognizer, e))?;
        let token_id = unsafe { token.GetId() }.map(take_pwstr).unwrap_or_default();
        let description = read_token_value(&token, "Description").unwrap_or_default();
        let language = read_token_value(&token, "Language").unwrap_or_default();
        recognizers.push(RecognizerInfo {
            index,
            token_id,
            description,
            language,
        });
    }

    Ok(ProbeReport {
        default_language,
        recognizers,
        zh_cn_dictation: probe_zh_cn(),
    })
}

/// 用 zh-CN（LCID 0x804）创建 `SpInprocRecognizer` 并 `LoadDictation`，不碰麦克风。
fn probe_zh_cn() -> Result<(), String> {
    let recognizer: ISpRecognizer =
        unsafe { CoCreateInstance(&SpInprocRecognizer, None::<&IUnknown>, CLSCTX_INPROC_SERVER) }
            .map_err(|e| map_error(FailureStage::Recognizer, e))?;
    let token = select_recognizer("zh-CN")?
        .ok_or_else(|| describe_failure(FailureStage::Recognizer, SPERR_NOT_FOUND))?;
    unsafe { recognizer.SetRecognizer(&token) }
        .map_err(|e| map_error(FailureStage::Recognizer, e))?;
    let context = unsafe { recognizer.CreateRecoContext() }
        .map_err(|e| map_error(FailureStage::Recognizer, e))?;
    let grammar =
        unsafe { context.CreateGrammar(1) }.map_err(|e| map_error(FailureStage::Recognizer, e))?;
    unsafe { grammar.LoadDictation(PCWSTR::null(), SPLO_STATIC) }
        .map_err(|e| map_error(FailureStage::Recognizer, e))?;
    Ok(())
}

/// `GetUserDefaultLocaleName` 返回字符数（含结尾 NUL）；0 表示取不到。
fn system_default_language() -> Option<String> {
    const LOCALE_NAME_MAX_LENGTH: usize = 85;
    let mut buffer = [0u16; LOCALE_NAME_MAX_LENGTH];
    let len = unsafe { GetUserDefaultLocaleName(&mut buffer) };
    if len <= 0 || len as usize > buffer.len() {
        return None;
    }
    let len = len as usize;
    let end = buffer[..len].iter().position(|&c| c == 0).unwrap_or(len);
    Some(String::from_utf16_lossy(&buffer[..end]))
}

fn map_error(stage: FailureStage, error: windows::core::Error) -> String {
    describe_failure(stage, error.code().0)
}

/// MTA COM 的 RAII。构造成功才允许 `CoUninitialize`，与 COM 的配对规则一致。
struct ComApartment;

impl ComApartment {
    fn new() -> Result<Self, String> {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_ok() {
            Ok(Self)
        } else {
            Err(describe_failure(FailureStage::Recognizer, hr.0))
        }
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

/// UTF-16 + 结尾 NUL。
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 取走 `CoTaskMemAlloc` 分配的宽字符串：转成 `String` 后释放缓冲区。
fn take_pwstr(value: PWSTR) -> String {
    if value.0.is_null() {
        return String::new();
    }
    let text = unsafe {
        let mut len = 0usize;
        while *value.0.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(value.0, len))
    };
    unsafe { CoTaskMemFree(Some(value.0 as *const c_void)) };
    text
}
