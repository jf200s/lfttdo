//! 平台无关的 SenseVoice-Small int8 识别管线（自研前处理/解码 + ort 推理）。
//!
//! 链路：rubato 重采样到 16k → ×32768 → kaldi-fbank(80 mel) → LFR(7/6) →
//! CMVN(模型 metadata) → ort (ONNX Runtime) 推理 → CTC 贪心解码（跳前 4 个
//! prompt token）。已对拍 sherpa-onnx 2.0.1 基线：三条中文 wav 文本逐字一致，
//! RTF≈0.03（spike 裁决 GO）。
//!
//! **加载成本与缓存**：ort 加载 ~1s（spike 实测冷 1.27s / 热 0.87s；tract 时代
//! 是 14s），仍不每次说话都付：进程级常驻内存缓存（`CACHE`）首次加载后复用，
//! `prewarm` 在录音开始时后台加载。ORT 是 pyke 预编译**静态**库，链入 exe，
//! 无随包 DLL；动态 T 原生支持（tract 钉死 N=1 的 workaround 已随引擎删除）。
//!
//! 本模块不依赖 tauri/cpal，Linux 上可编译可单测（纯函数 + 小夹具对拍）。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use ort::environment::Environment;
use ort::session::{Session, builder::GraphOptimizationLevel};
use ort::value::Tensor;

/// fbank 80 mel × LFR 窗口 7 = 560 维输入特征。
const FEAT_DIM: usize = 560;
const MEL_BINS: usize = 80;
/// 模型输出前 4 帧是内置 prompt（语言/情感/事件/ITN 标记），解码时跳过。
const PROMPT_FRAMES: usize = 4;
/// 此模型 metadata 没有 blank_id 键；tokens.txt id 0 (<unk>) 充当 CTC blank
/// （与 sherpa-onnx 的行为一致）。
const BLANK_ID: usize = 0;

/// 模型会话 + 前处理/解码所需的全部 metadata。经 `Arc` 常驻缓存供多次识别复用。
/// `Session` 引用进程级 Environment（见 `ENV`）；`Mutex` 提供内部可变性
/// （`run` 要 `&mut self`）并把并发识别串行化。
pub struct Model {
    session: Mutex<Session>,
    neg_mean: Vec<f32>,
    inv_stddev: Vec<f32>,
    vocab: Vec<String>,
    lang_auto: i32,
    with_itn: i32,
    without_itn: i32,
    lfr_window: usize,
    lfr_shift: usize,
}

/// 进程级缓存：只缓存一个模型目录（本产品只有一个模型）。锁在加载期间持有，
/// 并发的 prewarm 与 recognize 不会重复付首次加载，后者等前者完成。
static CACHE: Mutex<Option<(PathBuf, Arc<Model>)>> = Mutex::new(None);

/// 进程级 Environment：ort 每进程只允许成功 `build()` 一次，首次成功后常驻，
/// 之后所有 `Model::load` 复用它建 Session。`build()` 本身失败时 `OnceLock` 不置位，
/// 下次加载可重试 —— 否则首次加载在 env 创建之后失败（模型损坏等）会让本进程后续
/// 每次加载都永久报 "only one environment is allowed per process"，只能重启。
static ENV: OnceLock<Environment> = OnceLock::new();

/// 取进程级 Environment；未置位时现 build。生产路径的加载都在 `CACHE` 锁内串行，
/// 这把小锁只防直接并发调 `Model::load` 时重复 build（ort 只许成功一次）。
fn environment() -> Result<&'static Environment, String> {
    if let Some(env) = ENV.get() {
        return Ok(env);
    }
    static INIT: Mutex<()> = Mutex::new(());
    let _guard = INIT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(env) = ENV.get() {
        return Ok(env);
    }
    let env = ort::init().build().map_err(|e| format!("{e}"))?;
    Ok(ENV.get_or_init(|| env))
}

/// 取缓存模型；目录变化或未加载时现加载（首次 ~1s）。
pub fn cached(model_dir: &Path) -> Result<Arc<Model>, String> {
    let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((dir, model)) = guard.as_ref()
        && dir == model_dir
    {
        return Ok(model.clone());
    }
    let model = Arc::new(Model::load(model_dir)?);
    *guard = Some((model_dir.to_path_buf(), model.clone()));
    Ok(model)
}

/// 后台预热：engine 在录音开始（ready 之后）调用，把首次加载成本掩盖在说话
/// 时间里。错误静默丢弃——模型未下载/损坏会在 stop 后的 recognize 里以同样
/// 的可行动文案再报一次，这里不需要第二个错误出口。
pub fn prewarm(model_dir: &Path) {
    // 缓存已热时线程立即返回，代价可忽略，不做去重。
    let dir = model_dir.to_path_buf();
    let _ = std::thread::Builder::new()
        .name("voice-prewarm".to_string())
        .spawn(move || {
            let _ = cached(&dir);
        });
}

fn prop_str(meta: &ort::session::ModelMetadata, key: &str) -> Result<String, String> {
    meta.custom(key)
        .ok_or_else(|| format!("模型 metadata 缺少键 {key}"))
}

fn prop_floats(meta: &ort::session::ModelMetadata, key: &str) -> Result<Vec<f32>, String> {
    prop_str(meta, key)?
        .split(',')
        .map(|s| {
            s.trim()
                .parse::<f32>()
                .map_err(|e| format!("metadata {key} 含非法浮点数：{e}"))
        })
        .collect()
}

fn prop_parse<T: std::str::FromStr>(
    meta: &ort::session::ModelMetadata,
    key: &str,
) -> Result<T, String>
where
    T::Err: std::fmt::Display,
{
    prop_str(meta, key)?
        .trim()
        .parse::<T>()
        .map_err(|e| format!("metadata {key} 解析失败：{e}"))
}

impl Model {
    /// 从模型目录加载 `model.int8.onnx` + `tokens.txt` 并建 ort 会话（首次 ~1s）。
    pub fn load(model_dir: &Path) -> Result<Self, String> {
        let model_path = model_dir.join("model.int8.onnx");
        let tokens_path = model_dir.join("tokens.txt");
        if !model_path.is_file() || !tokens_path.is_file() {
            return Err("语音模型尚未下载，请先在 设置 → 语音输入 中下载。".into());
        }
        let corrupted = |e: String| {
            format!("语音模型加载失败：{e}。文件可能已损坏，请在 设置 → 语音输入 中重新下载。")
        };

        let env = environment().map_err(corrupted)?;
        let session = Session::builder(env)
            .map_err(|e| corrupted(format!("{e}")))?
            .with_intra_threads(2)
            .map_err(|e| corrupted(format!("{e}")))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| corrupted(format!("{e}")))?
            .commit_from_file(&model_path)
            .map_err(|e| corrupted(format!("{e}")))?;

        // ModelMetadata 借用 session 且带 Drop：块限定生命周期，读完即还。
        let (
            neg_mean,
            inv_stddev,
            lang_auto,
            with_itn,
            without_itn,
            lfr_window,
            lfr_shift,
            vocab_size,
        ) = {
            let meta = session.metadata().map_err(|e| corrupted(format!("{e}")))?;
            let neg_mean = prop_floats(&meta, "neg_mean").map_err(corrupted)?;
            let inv_stddev = prop_floats(&meta, "inv_stddev").map_err(corrupted)?;
            if neg_mean.len() != FEAT_DIM || inv_stddev.len() != FEAT_DIM {
                return Err(corrupted(format!(
                    "CMVN 维度应为 {FEAT_DIM}，实际 {}/{}",
                    neg_mean.len(),
                    inv_stddev.len()
                )));
            }
            let lang_auto: i32 = prop_parse(&meta, "lang_auto").map_err(corrupted)?;
            let with_itn: i32 = prop_parse(&meta, "with_itn").map_err(corrupted)?;
            let without_itn: i32 = prop_parse(&meta, "without_itn").map_err(corrupted)?;
            let lfr_window: usize = prop_parse(&meta, "lfr_window_size").map_err(corrupted)?;
            let lfr_shift: usize = prop_parse(&meta, "lfr_window_shift").map_err(corrupted)?;
            let vocab_size: usize = prop_parse(&meta, "vocab_size").map_err(corrupted)?;
            if lfr_window * MEL_BINS != FEAT_DIM || lfr_shift == 0 {
                return Err(corrupted(format!(
                    "LFR 参数异常：window={lfr_window} shift={lfr_shift}"
                )));
            }
            (
                neg_mean,
                inv_stddev,
                lang_auto,
                with_itn,
                without_itn,
                lfr_window,
                lfr_shift,
                vocab_size,
            )
        };

        let tokens =
            std::fs::read_to_string(&tokens_path).map_err(|e| corrupted(format!("{e}")))?;
        let vocab = parse_tokens(&tokens);
        if vocab.len() != vocab_size {
            return Err(corrupted(format!(
                "tokens.txt 词条数 {} 与模型 vocab_size {vocab_size} 不符",
                vocab.len()
            )));
        }

        Ok(Self {
            session: Mutex::new(session),
            neg_mean,
            inv_stddev,
            vocab,
            lang_auto,
            with_itn,
            without_itn,
            lfr_window,
            lfr_shift,
        })
    }

    /// 识别一段单声道 f32（[-1,1]，采样率 `sample_rate`）。语言自动检测，
    /// `use_itn` 开启模型自带的标点/逆文本正则化。返回 trim 后的文本（可为空串）。
    pub fn recognize(
        &self,
        samples: &[f32],
        sample_rate: u32,
        use_itn: bool,
    ) -> Result<String, String> {
        if samples.is_empty() {
            return Ok(String::new());
        }
        let samples16k = resample_to_16k(samples, sample_rate)?;
        let fbank = fbank_features(&samples16k);
        let fbank_frames = fbank.len() / MEL_BINS;
        if fbank_frames == 0 {
            // 不足一帧（<25ms）的录音没有可识别内容。
            return Ok(String::new());
        }
        let feats = lfr_cmvn(
            &fbank,
            fbank_frames,
            &self.neg_mean,
            &self.inv_stddev,
            self.lfr_window,
            self.lfr_shift,
        );
        // 模型吃的是 LFR 之后的帧数（ceil(fbank帧数/6)），不是 fbank 帧数。
        let n_frames = feats.len() / FEAT_DIM;

        // 输入顺序实测为 x, x_length, language, text_norm；输出仅 logits [1,T+4,V]。
        let inputs = ort::inputs![
            "x" => Tensor::from_array(([1usize, n_frames, FEAT_DIM], feats))
                .map_err(|e| format!("语音识别输入构造失败：{e}"))?,
            "x_length" => Tensor::from_array(([1usize], vec![n_frames as i32]))
                .map_err(|e| format!("语音识别输入构造失败：{e}"))?,
            "language" => Tensor::from_array(([1usize], vec![self.lang_auto]))
                .map_err(|e| format!("语音识别输入构造失败：{e}"))?,
            "text_norm" => Tensor::from_array(([1usize], vec![if use_itn {
                self.with_itn
            } else {
                self.without_itn
            }]))
            .map_err(|e| format!("语音识别输入构造失败：{e}"))?,
        ];
        let mut session = self.session.lock().unwrap_or_else(|e| e.into_inner());
        let outputs = session
            .run(inputs)
            .map_err(|e| format!("语音识别推理失败：{e}"))?;

        let (shape, logits) = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|e| format!("语音识别输出解读失败：{e}"))?;
        if shape.len() != 3
            || shape[0] != 1
            || shape[1] != (n_frames + PROMPT_FRAMES) as i64
            || shape[2] != self.vocab.len() as i64
        {
            return Err(format!(
                "语音识别输出形状异常：{shape:?}，期望 [1, {}, {}]",
                n_frames + PROMPT_FRAMES,
                self.vocab.len()
            ));
        }
        let ids = ctc_greedy_decode(
            logits,
            shape[1] as usize,
            shape[2] as usize,
            PROMPT_FRAMES,
            BLANK_ID,
        );
        Ok(ids_to_text(&ids, &self.vocab))
    }
}

/// tokens.txt 每行 `<token> <id>`；取空格前的 token 部分，按行序即 id 序。
pub fn parse_tokens(content: &str) -> Vec<String> {
    content
        .lines()
        .map(|l| {
            l.rsplit_once(' ')
                .map(|(token, _)| token.to_string())
                .unwrap_or_default()
        })
        .collect()
}

/// 重采样到 16kHz（rubato FFT，单声道）。已是 16k 时原样返回。
pub fn resample_to_16k(samples: &[f32], sample_rate: u32) -> Result<Vec<f32>, String> {
    if sample_rate == 16000 {
        return Ok(samples.to_vec());
    }
    use rubato::{Fft, FixedSync, Resampler};
    let mut resampler = Fft::<f32>::new(sample_rate as usize, 16000, 1024, 1, FixedSync::Input)
        .map_err(|e| format!("语音重采样初始化失败（采样率 {sample_rate}）：{e:?}"))?;
    let input = rubato::audioadapter_buffers::owned::InterleavedOwned::new_from(
        samples.to_vec(),
        1,
        samples.len(),
    )
    .map_err(|e| format!("语音重采样缓冲构造失败：{e:?}"))?;
    let out = resampler
        .process_all(&input, samples.len(), None)
        .map_err(|e| format!("语音重采样失败：{e:?}"))?;
    Ok(out.take_data())
}

/// ×32768 → kaldi fbank（80 mel、hamming、snip_edges、dither=0、去直流、
/// preemph 0.97、20–8000Hz、FFT512、帧长 400/帧移 160）。
/// 输入为 16k 单声道 [-1,1]；输出行主序 [T, 80]。
pub fn fbank_features(samples16k: &[f32]) -> Vec<f32> {
    use kaldi_fbank::{
        FbankOptions, FrameExtractionOptions, MelBanksOptions, OnlineFbank, WindowType,
    };
    let scaled: Vec<f32> = samples16k.iter().map(|&s| s * 32768.0).collect();
    let opts = FbankOptions {
        frame_opts: FrameExtractionOptions {
            samp_freq: 16000.0,
            frame_shift_ms: 10.0,
            frame_length_ms: 25.0,
            dither: 0.0,
            preemph_coeff: 0.97,
            remove_dc_offset: true,
            window_type: WindowType::Hamming,
            round_to_power_of_two: true, // 400 → 512 FFT
            snip_edges: true,
            ..Default::default()
        },
        mel_opts: MelBanksOptions {
            num_bins: MEL_BINS as i32,
            low_freq: 20.0,
            high_freq: 8000.0, // 16k 的 Nyquist
            ..Default::default()
        },
        use_energy: false,
        ..Default::default()
    };
    let mut fbank = OnlineFbank::new(opts);
    fbank.accept_waveform(16000.0, &scaled);
    fbank.input_finished();
    let t = fbank.num_frames_ready() as usize;
    let mut out = Vec::with_capacity(t * MEL_BINS);
    for i in 0..t {
        out.extend_from_slice(fbank.get_frame(i as i32));
    }
    out
}

/// LFR（窗口 w / 步长 s，输出 ceil(T/s) 帧，首尾复制边缘帧）+ CMVN
/// `(x + neg_mean) * inv_stddev`。输入行主序 [T, 80]，输出 [ceil(T/s), w*80]。
pub fn lfr_cmvn(
    fbank: &[f32],
    n_frames: usize,
    neg_mean: &[f32],
    inv_stddev: &[f32],
    window: usize,
    shift: usize,
) -> Vec<f32> {
    if n_frames == 0 {
        return Vec::new();
    }
    let left = (window - 1) / 2;
    let n_out = n_frames.div_ceil(shift);
    let mut out = Vec::with_capacity(n_out * window * MEL_BINS);
    for i in 0..n_out {
        for j in 0..window {
            let idx = (i * shift + j).saturating_sub(left).min(n_frames - 1);
            let frame = &fbank[idx * MEL_BINS..(idx + 1) * MEL_BINS];
            for (d, &v) in frame.iter().enumerate() {
                let k = j * MEL_BINS + d;
                out.push((v + neg_mean[k]) * inv_stddev[k]);
            }
        }
    }
    out
}

/// CTC 贪心：逐帧 argmax，跳过前 `skip` 帧（prompt），去 blank、去连续重复
/// （blank 分隔的相同 id 保留）。logits 行主序 [n_frames_out, vocab_size]。
pub fn ctc_greedy_decode(
    logits: &[f32],
    n_frames_out: usize,
    vocab_size: usize,
    skip: usize,
    blank: usize,
) -> Vec<usize> {
    let mut ids = Vec::new();
    let mut prev = blank;
    for frame in skip..n_frames_out {
        let row = &logits[frame * vocab_size..(frame + 1) * vocab_size];
        let mut best = 0usize;
        let mut best_val = f32::NEG_INFINITY;
        for (i, &val) in row.iter().enumerate() {
            if val > best_val {
                best_val = val;
                best = i;
            }
        }
        if best != prev && best != blank {
            ids.push(best);
        }
        prev = best;
    }
    ids
}

/// id 序列 → 文本：查词表拼接，`▁`(U+2581) 还原为空格，trim。
pub fn ids_to_text(ids: &[usize], vocab: &[String]) -> String {
    let text: String = ids
        .iter()
        .filter_map(|&i| vocab.get(i).map(|s| s.as_str()))
        .collect::<String>()
        .replace('\u{2581}', " ");
    text.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f32s(bytes: &[u8]) -> Vec<f32> {
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect()
    }

    #[test]
    fn tokens_parse_takes_token_before_last_space() {
        let vocab = parse_tokens("你 0\n好 1\n▁hello 2\n\n");
        assert_eq!(vocab, vec!["你", "好", "▁hello", ""]);
    }

    #[test]
    fn lfr_single_frame_replicates_both_edges() {
        // 1 帧输入 → ceil(1/6)=1 帧输出，窗口 7 个槽位全部复制唯一的那帧。
        let fbank = vec![3.0; MEL_BINS];
        let out = lfr_cmvn(&fbank, 1, &vec![0.0; FEAT_DIM], &vec![1.0; FEAT_DIM], 7, 6);
        assert_eq!(out.len(), FEAT_DIM);
        assert!(out.iter().all(|&v| v == 3.0));
    }

    #[test]
    fn lfr_ceil_count_and_tail_replication() {
        // 7 帧 → ceil(7/6)=2 输出。每帧 80 维全填帧号，便于核对索引。
        let n = 7;
        let fbank: Vec<f32> = (0..n).flat_map(|i| vec![i as f32; MEL_BINS]).collect();
        let out = lfr_cmvn(&fbank, n, &vec![0.0; FEAT_DIM], &vec![1.0; FEAT_DIM], 7, 6);
        assert_eq!(out.len(), 2 * FEAT_DIM);
        // 输出 0：idx = j-3 → 首三槽复制帧 0，然后 0,1,2,3。
        let head: Vec<f32> = (0..7).map(|j| out[j * MEL_BINS]).collect();
        assert_eq!(head, vec![0.0, 0.0, 0.0, 0.0, 1.0, 2.0, 3.0]);
        // 输出 1：idx = 6+j-3 → 3,4,5,6，尾三槽复制帧 6。
        let tail: Vec<f32> = (0..7).map(|j| out[FEAT_DIM + j * MEL_BINS]).collect();
        assert_eq!(tail, vec![3.0, 4.0, 5.0, 6.0, 6.0, 6.0, 6.0]);
    }

    #[test]
    fn cmvn_applies_neg_mean_and_inv_stddev_per_slot() {
        let fbank = vec![2.0; MEL_BINS];
        let mut neg_mean = vec![0.0; FEAT_DIM];
        let mut inv_stddev = vec![1.0; FEAT_DIM];
        neg_mean[0] = 1.0; // 槽位 0 第 0 维：(2+1)*0.5 = 1.5
        inv_stddev[0] = 0.5;
        let out = lfr_cmvn(&fbank, 1, &neg_mean, &inv_stddev, 7, 6);
        assert_eq!(out[0], 1.5);
        assert_eq!(out[1], 2.0);
        // 同一维在不同 LFR 槽位用各自的 mean/std（槽位 1 未改 → 2.0）。
        assert_eq!(out[MEL_BINS], 2.0);
    }

    #[test]
    fn lfr_empty_input_is_empty_output() {
        assert!(lfr_cmvn(&[], 0, &[], &[], 7, 6).is_empty());
    }

    /// 造一行 logits：指定 id 处为 1，其余 0。
    fn row(vocab: usize, id: usize) -> Vec<f32> {
        let mut r = vec![0.0; vocab];
        r[id] = 1.0;
        r
    }

    #[test]
    fn ctc_skips_prompt_frames_dedups_and_drops_blank() {
        let v = 5;
        // 前 4 帧 prompt（argmax=1，必须被跳过）；随后 blank,2,2,blank,2,3,3,blank。
        let mut logits = Vec::new();
        for id in [1, 1, 1, 1, 0, 2, 2, 0, 2, 3, 3, 0] {
            logits.extend(row(v, id));
        }
        let ids = ctc_greedy_decode(&logits, 12, v, 4, 0);
        // 连续重复去掉，但 blank 分隔的 2,2 保留。
        assert_eq!(ids, vec![2, 2, 3]);
    }

    #[test]
    fn ctc_all_blank_is_empty() {
        let logits: Vec<f32> = (0..6).flat_map(|_| row(3, 0)).collect();
        assert!(ctc_greedy_decode(&logits, 6, 3, 4, 0).is_empty());
    }

    #[test]
    fn text_maps_sentencepiece_space_and_trims() {
        let vocab: Vec<String> = ["<unk>", "▁hello", "▁", "▁world", "你好"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(ids_to_text(&[1, 3], &vocab), "hello world");
        assert_eq!(ids_to_text(&[2, 4, 2], &vocab), "你好");
        assert_eq!(ids_to_text(&[], &vocab), "");
    }

    #[test]
    fn resample_16k_is_passthrough() {
        let samples = vec![0.1, -0.2, 0.3];
        assert_eq!(resample_to_16k(&samples, 16000).unwrap(), samples);
    }

    /// 回归：首次加载在 env 创建之后失败（模型损坏），第二次加载不得报
    /// "only one environment is allowed per process"。修复前 `Model::load` 每次都
    /// `ort::init().build()`，而 ort 每进程只许成功 build 一次 ⇒ 第二次必撞这条
    /// 错误，文案还引导用户重新下载（实际修不好，只能重启）。修复后 env 进程级
    /// 常驻，重试复用同一 env，落到真正的模型错误上。
    #[test]
    fn reload_after_corrupt_model_does_not_hit_single_environment_limit() {
        let tmp = tempfile::tempdir().unwrap();
        // 两个文件都「在」以越过 is_file 闸，但 onnx 内容是垃圾 ⇒ env 创建之后
        // commit_from_file 才失败，正是触发缺陷的那条路径。
        std::fs::write(
            tmp.path().join("model.int8.onnx"),
            b"not a valid onnx model",
        )
        .unwrap();
        std::fs::write(tmp.path().join("tokens.txt"), "<unk> 0\n").unwrap();

        assert!(Model::load(tmp.path()).is_err(), "损坏模型应当加载失败");
        let err = Model::load(tmp.path()).err().expect("第二次加载仍应失败");
        assert!(
            !err.contains("only one environment is allowed per process"),
            "第二次加载撞上了进程级 env 限制：{err}"
        );
    }

    /// fbank 数值对拍：夹具是 zh.wav 前 0.5s（8000 样本）经 spike 验证过的
    /// kaldi-fbank 参数链算出的 [48, 80] 特征（生成器与参数见 testdata/README）。
    #[test]
    fn fbank_matches_spike_fixture() {
        let input = f32s(include_bytes!("testdata/fbank_input.f32"));
        let expected = f32s(include_bytes!("testdata/fbank_expected.f32"));
        assert_eq!(input.len(), 8000);
        let actual = fbank_features(&input);
        assert_eq!(actual.len(), expected.len());
        assert_eq!(actual.len(), 48 * MEL_BINS);
        let max_diff = actual
            .iter()
            .zip(&expected)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(max_diff < 1e-4, "fbank 与 spike 夹具最大偏差 {max_diff}");
    }

    /// 端到端文本对拍（需 239MB 模型 + 测试 wav，手动跑）：
    /// ```text
    /// ASR_MODEL_DIR=<模型目录> ASR_WAV_DIR=<wav目录> \
    ///   cargo test --manifest-path src-tauri/Cargo.toml voice::asr -- --ignored
    /// ```
    /// 模型下载见 设置 → 语音输入；wav 与期望文本来自 sherpa-onnx 2.0.1 基线
    /// （期望文本存 testdata/baseline_texts.json，wav 不入库）。
    #[test]
    #[ignore = "需要模型与测试 wav，手动运行"]
    fn end_to_end_matches_sherpa_baseline() {
        let model_dir = std::env::var("ASR_MODEL_DIR").expect("设置 ASR_MODEL_DIR");
        let wav_dir = std::env::var("ASR_WAV_DIR").expect("设置 ASR_WAV_DIR");
        let model = Model::load(Path::new(&model_dir)).unwrap();
        let baseline: serde_json::Value =
            serde_json::from_str(include_str!("testdata/baseline_texts.json")).unwrap();
        for case in baseline.as_array().unwrap() {
            let wav = Path::new(&wav_dir).join(case["wav"].as_str().unwrap());
            let mut reader = hound::WavReader::open(&wav).unwrap();
            let spec = reader.spec();
            assert_eq!(spec.channels, 1);
            let max = 2i64.pow(spec.bits_per_sample as u32 - 1) as f32;
            let samples: Vec<f32> = reader
                .samples::<i32>()
                .map(|s| s.unwrap() as f32 / max)
                .collect();
            let text = model.recognize(&samples, spec.sample_rate, true).unwrap();
            assert_eq!(text, case["text"].as_str().unwrap(), "{}", wav.display());
        }
    }
}
