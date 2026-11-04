# voice/testdata 夹具来源

- `fbank_input.f32` / `fbank_expected.f32`：little-endian f32。输入是
  sherpa-onnx 官方测试音频 `zh.wav`（16k 单声道 int16，归一化到 [-1,1]）的
  前 8000 样本（0.5s）；期望值是它经 kaldi-fbank 0.1.0 算出的 [48, 80] 特征，
  参数与 `asr.rs::fbank_features` 完全一致（80 mel、hamming、snip_edges、
  dither=0、去直流、preemph 0.97、20–8000Hz、FFT512、帧长 400/帧移 160，
  输入先 ×32768）。该参数链已在 spike 中对拍 sherpa-onnx 2.0.1 基线
  （三条 wav 识别文本逐字一致）。生成方式：按上述参数对输入跑
  `OnlineFbank::accept_waveform + input_finished`，逐帧 `get_frame` 拼接。
- `baseline_texts.json`：三条测试 wav 的 sherpa-onnx 2.0.1 基线识别文本
  （language=auto、use_itn=true、cpu、num_threads=2），供 `#[ignore]`
  端到端测试对拍。wav 本体不入库（手动测试时经 `ASR_WAV_DIR` 指定）。