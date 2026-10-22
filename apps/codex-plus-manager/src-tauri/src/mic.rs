//! 麦克风录音（macOS 优先），供 ZCode 语音输入使用。
//!
//! ZCode 自身没有 `com.apple.security.device.audio-input` entitlement，渲染层
//! 录音会被 TCC 拒绝；所以录音改由 Codex++ 管理端（本进程）用系统默认输入设备
//! 完成，识别出的文本再经注入脚本写回 ZCode 输入框。
//!
//! 对外接口保持最小：
//! - [`start`] 开始录音；
//! - [`stop`] 停止并返回 16 kHz 单声道 PCM16 WAV 字节；
//! - [`cancel`] 丢弃当前录音；
//! - [`is_recording`] 查询状态。
//!
//! 线程模型：`cpal::Stream` 不是 `Send`，所以设备打开、回调、停止、释放全部
//! 收敛在一条专用采集线程（`codex-plus-mic`）里。本模块只通过共享的
//! `Arc<Mutex<Vec<f32>>>` 取采样，用 channel 控制生命周期。
//!
//! 采样统一在回调里降成单声道 f32，`stop` 时按设备采样率做盒式平均降采样到
//! 16 kHz 并量化为 PCM16；这是本地 SenseVoice 唯一接受的格式。盒式平均本身是
//! 一个粗糙的低通，对语音够用；若要更高保真应换 windowed-sinc，见实现笔记。
//!
//! 最长录音 5 分钟：到点后自动停止采集（保留已录内容），`stop` 仍能取回。

// 非 macOS 生产构建里没有采集实现，纯函数只有单测在用；避免 dead_code 噪音。
#![cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]

use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

/// 输出/识别用的采样率：本地 SenseVoice 只接受 16 kHz 单声道 PCM16 WAV。
pub(crate) const TARGET_SAMPLE_RATE: u32 = 16_000;
/// 单次录音最长 5 分钟；超时后自动停止采集，已录内容仍可被 `stop` 取回。
pub(crate) const MAX_RECORDING_SECONDS: u64 = 300;
/// 等待录音线程把设备打开。首次会触发系统麦克风授权弹窗，用户点「允许」之前
/// CoreAudio 可能一直阻塞，所以留足 45 秒；授权后打开是毫秒级。
const START_TIMEOUT: Duration = Duration::from_secs(45);

/// 采集回调与 `stop` 之间共享的缓冲。
type SampleBuffer = Arc<Mutex<Vec<f32>>>;

/// 一次进行中的录音。
///
/// 字段只在 holder（全局状态或测试）里存在；采集线程自己持有 `cpal::Stream`。
#[derive(Debug)]
struct ActiveCapture {
    samples: SampleBuffer,
    /// 设备原始采样率，`stop` 时据此降采样。
    sample_rate: u32,
    /// 通知采集线程停止并释放设备。
    stop_tx: mpsc::Sender<()>,
    /// 采集线程句柄；`stop`/`cancel` 等它退出后才读缓冲，保证数据完整。
    join: std::thread::JoinHandle<()>,
}

/// 录音状态机。`Starting` 覆盖「设备尚未就绪、但已经占位」的窗口，
/// 避免两个 start 请求同时打开两次设备。
#[derive(Default)]
enum MicState {
    #[default]
    Idle,
    Starting,
    Recording(ActiveCapture),
}

impl MicState {
    /// 占用录音状态。已在录音/启动中时返回中文错误。
    fn begin(&mut self) -> anyhow::Result<()> {
        match self {
            MicState::Idle => {
                *self = MicState::Starting;
                Ok(())
            }
            MicState::Starting => anyhow::bail!("麦克风正在启动，请稍候"),
            MicState::Recording(_) => anyhow::bail!("麦克风正在录音中"),
        }
    }

    /// 设备就绪后把 `Starting` 换成 `Recording`。
    ///
    /// 失败时把 capture 原样还回去：调用方需要停掉采集线程，否则设备会一直录。
    fn install(&mut self, capture: ActiveCapture) -> Result<(), ActiveCapture> {
        if !matches!(self, MicState::Starting) {
            return Err(capture);
        }
        *self = MicState::Recording(capture);
        Ok(())
    }

    /// `start` 失败/超时后清掉占位，不影响其它状态。
    fn reset_starting(&mut self) {
        if matches!(self, MicState::Starting) {
            *self = MicState::Idle;
        }
    }

    /// 取出当前录音；没有录音（或还在启动）时返回错误且不改状态。
    fn take_recording(&mut self) -> anyhow::Result<ActiveCapture> {
        match std::mem::replace(self, MicState::Idle) {
            MicState::Recording(capture) => Ok(capture),
            MicState::Starting => {
                *self = MicState::Starting;
                anyhow::bail!("麦克风正在启动，请稍后再试")
            }
            MicState::Idle => anyhow::bail!("当前没有正在进行的录音"),
        }
    }

    /// 丢弃当前录音（`cancel` 语义）。启动中的占位也一并清掉。
    fn discard(&mut self) -> Option<ActiveCapture> {
        match std::mem::replace(self, MicState::Idle) {
            MicState::Recording(capture) => Some(capture),
            _ => None,
        }
    }

    fn is_active(&self) -> bool {
        !matches!(self, MicState::Idle)
    }
}

static MIC_STATE: Mutex<MicState> = Mutex::new(MicState::Idle);

fn lock_state() -> std::sync::MutexGuard<'static, MicState> {
    // 录音线程 panic 不应让后续请求全部失败；中毒后继续用内部状态。
    MIC_STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// 开始录音。已在录音中时返回错误，不重入。
pub fn start() -> anyhow::Result<()> {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = &MIC_STATE;
        anyhow::bail!("ZCode 语音输入目前仅支持 macOS")
    }
    #[cfg(target_os = "macos")]
    {
        start_macos()
    }
}

/// 停止录音并返回 16 kHz 单声道 PCM16 WAV。
pub fn stop() -> anyhow::Result<Vec<u8>> {
    #[cfg(not(target_os = "macos"))]
    {
        anyhow::bail!("ZCode 语音输入目前仅支持 macOS")
    }
    #[cfg(target_os = "macos")]
    {
        let capture = lock_state().take_recording()?;
        let sample_rate = capture.sample_rate;
        let samples = finish_capture(capture);
        Ok(encode_recording_wav(&samples, sample_rate))
    }
}

/// 丢弃当前录音。没有录音时返回 Ok。
pub fn cancel() -> anyhow::Result<()> {
    let capture = lock_state().discard();
    if let Some(capture) = capture {
        let _ = finish_capture(capture);
    }
    Ok(())
}

/// 当前是否正在录音（启动中视为 `true`：设备马上就会开始采集）。
pub fn is_recording() -> bool {
    lock_state().is_active()
}

/// 停掉采集线程并取回全部采样。
fn finish_capture(capture: ActiveCapture) -> Vec<f32> {
    // 采集线程可能正阻塞在 recv（或已因 5 分钟上限暂停设备、仍在等收尾信号）。
    let _ = capture.stop_tx.send(());
    let _ = capture.join.join();
    let mut samples = capture
        .samples
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::mem::take(&mut *samples)
}

#[cfg(target_os = "macos")]
fn start_macos() -> anyhow::Result<()> {
    // 占位和「线程是否已经起来」必须原子判断，否则并发 start 会打开两次设备。
    lock_state().begin()?;

    let samples: SampleBuffer = Arc::new(Mutex::new(Vec::new()));
    let thread_samples = Arc::clone(&samples);
    let (ready_tx, ready_rx) = mpsc::channel::<Result<u32, String>>();
    let (stop_tx, stop_rx) = mpsc::channel::<()>();

    let join = match std::thread::Builder::new()
        .name("codex-plus-mic".to_string())
        .spawn(move || capture_thread(ready_tx, stop_rx, thread_samples))
    {
        Ok(join) => join,
        Err(error) => {
            lock_state().reset_starting();
            anyhow::bail!("无法启动麦克风采集线程：{error}");
        }
    };

    match ready_rx.recv_timeout(START_TIMEOUT) {
        Ok(Ok(sample_rate)) => {
            let capture = ActiveCapture {
                samples,
                sample_rate,
                stop_tx,
                join,
            };
            match lock_state().install(capture) {
                Ok(()) => Ok(()),
                Err(capture) => {
                    // 等待期间被 cancel 清掉了占位：设备还开着，必须停掉，不能泄漏录音。
                    let _ = finish_capture(capture);
                    anyhow::bail!("录音已被取消")
                }
            }
        }
        Ok(Err(message)) => {
            let _ = join.join();
            lock_state().reset_starting();
            Err(anyhow::anyhow!(message))
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // 不能 join：首次授权框可能一直没人点，线程会卡在设备启动上。
            // 丢掉控制端后线程随后会自己退出（ready 发送失败 / stop 通道断开）。
            drop(stop_tx);
            lock_state().reset_starting();
            anyhow::bail!(
                "麦克风启动超时：若为首次使用，请在系统弹窗中允许 Codex++ 使用麦克风后重试\
                 （系统设置 → 隐私与安全性 → 麦克风）"
            )
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            let _ = join.join();
            lock_state().reset_starting();
            anyhow::bail!("麦克风采集线程异常退出")
        }
    }
}

/// 采集线程：设备打开、开始播放、等待停止信号、释放设备，全过程在同一条线程。
#[cfg(target_os = "macos")]
fn capture_thread(
    ready: mpsc::Sender<Result<u32, String>>,
    stop_rx: mpsc::Receiver<()>,
    samples: SampleBuffer,
) {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let host = cpal::default_host();
    let Some(device) = host.default_input_device() else {
        let _ = ready.send(Err(
            "没有检测到可用的麦克风：系统默认输入设备为空，请先连接或在系统设置中启用麦克风"
                .to_string(),
        ));
        return;
    };
    let supported = match device.default_input_config() {
        Ok(config) => config,
        Err(error) => {
            let _ = ready.send(Err(capture_error_message(&error.to_string())));
            return;
        }
    };
    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let sample_rate = config.sample_rate.0;
    let channels = config.channels as usize;
    let max_samples = (MAX_RECORDING_SECONDS as u128 * sample_rate as u128)
        .min(usize::MAX as u128) as usize;

    let stream = match build_stream(
        &device,
        &config,
        sample_format,
        channels,
        Arc::clone(&samples),
        max_samples,
    ) {
        Ok(stream) => stream,
        Err(message) => {
            let _ = ready.send(Err(message));
            return;
        }
    };
    if let Err(error) = stream.play() {
        let _ = ready.send(Err(capture_error_message(&format!(
            "无法开始采集：{error}"
        ))));
        return;
    }
    if ready.send(Ok(sample_rate)).is_err() {
        // start() 已经超时离开，没人要这段录音了。
        let _ = stream.pause();
        return;
    }

    match stop_rx.recv_timeout(Duration::from_secs(MAX_RECORDING_SECONDS)) {
        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {}
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // 到 5 分钟上限：先停止采集，保留已录内容，等 stop/cancel 来收尾。
            let _ = stream.pause();
            let _ = stop_rx.recv();
        }
    }
    let _ = stream.pause();
    drop(stream);
}

#[cfg(target_os = "macos")]
fn build_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    sample_format: cpal::SampleFormat,
    channels: usize,
    samples: SampleBuffer,
    max_samples: usize,
) -> Result<cpal::Stream, String> {
    match sample_format {
        cpal::SampleFormat::I8 => build_typed_stream::<i8>(device, config, channels, samples, max_samples),
        cpal::SampleFormat::I16 => build_typed_stream::<i16>(device, config, channels, samples, max_samples),
        cpal::SampleFormat::I32 => build_typed_stream::<i32>(device, config, channels, samples, max_samples),
        cpal::SampleFormat::I64 => build_typed_stream::<i64>(device, config, channels, samples, max_samples),
        cpal::SampleFormat::U8 => build_typed_stream::<u8>(device, config, channels, samples, max_samples),
        cpal::SampleFormat::U16 => build_typed_stream::<u16>(device, config, channels, samples, max_samples),
        cpal::SampleFormat::U32 => build_typed_stream::<u32>(device, config, channels, samples, max_samples),
        cpal::SampleFormat::U64 => build_typed_stream::<u64>(device, config, channels, samples, max_samples),
        cpal::SampleFormat::F32 => build_typed_stream::<f32>(device, config, channels, samples, max_samples),
        cpal::SampleFormat::F64 => build_typed_stream::<f64>(device, config, channels, samples, max_samples),
        other => Err(format!(
            "默认麦克风使用了不支持的采样格式（{other:?}），请换一个输入设备"
        )),
    }
}

#[cfg(target_os = "macos")]
fn build_typed_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    samples: SampleBuffer,
    max_samples: usize,
) -> Result<cpal::Stream, String>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    use cpal::traits::DeviceTrait;
    use cpal::Sample;

    let channels = channels.max(1);
    device
        .build_input_stream(
            config,
            move |data: &[T], _info| {
                let Ok(mut buffer) = samples.lock() else {
                    return;
                };
                let frames = data.len() / channels;
                let limit = max_samples.saturating_sub(buffer.len()).min(frames);
                buffer.reserve(limit);
                for frame in 0..limit {
                    let start = frame * channels;
                    let mut sum = 0.0f32;
                    for channel in 0..channels {
                        sum += f32::from_sample(data[start + channel]);
                    }
                    buffer.push(sum / channels as f32);
                }
            },
            move |error| {
                // 采集线程的错误回调只影响本次录音；写诊断日志方便排查设备被拔掉等场景。
                let _ = codex_plus_core::diagnostic_log::append_diagnostic_log(
                    "zcode_voice.mic_stream_error",
                    serde_json::json!({ "message": error.to_string() }),
                );
            },
            None,
        )
        .map_err(|error| capture_error_message(&error.to_string()))
}

/// 把底层英文错误包装成用户能看懂的中文提示（含 TCC 权限指引）。
#[cfg(target_os = "macos")]
fn capture_error_message(detail: &str) -> String {
    format!(
        "无法打开默认麦克风：{detail}。请确认系统已连接麦克风，并在「系统设置 → \
         隐私与安全性 → 麦克风」中允许 Codex++ 使用麦克风"
    )
}

// ---------------------------------------------------------------------------
// 纯函数：降采样 / 量化 / WAV 封装。与设备无关，可在任意平台单测。
// ---------------------------------------------------------------------------

/// 把设备原始采样率的单声道 f32 降采样到 [`TARGET_SAMPLE_RATE`]。
pub(crate) fn downsample_to_target(samples: &[f32], source_rate: u32) -> Vec<f32> {
    downsample_mono(samples, source_rate, TARGET_SAMPLE_RATE)
}

/// 盒式平均降采样：每个输出样本覆盖输入时间轴上等长的一小段并取均值。
///
/// 输出长度 `floor(len * target / source)`；上采样（目标更高）时窗口可能为空，
/// 退化为取最近样本，保证长度正确、不 panic（正常麦克风不会遇到）。
pub(crate) fn downsample_mono(samples: &[f32], source_rate: u32, target_rate: u32) -> Vec<f32> {
    if samples.is_empty() || source_rate == 0 || target_rate == 0 {
        return Vec::new();
    }
    if source_rate == target_rate {
        return samples.to_vec();
    }
    let output_len =
        (samples.len() as u128 * target_rate as u128 / source_rate as u128) as usize;
    let mut output = Vec::with_capacity(output_len);
    for index in 0..output_len {
        let start = (index as u128 * source_rate as u128 / target_rate as u128) as usize;
        let end = (((index as u128 + 1) * source_rate as u128 + target_rate as u128 - 1)
            / target_rate as u128) as usize;
        let end = end.min(samples.len());
        if end <= start {
            output.push(samples[start.min(samples.len() - 1)]);
            continue;
        }
        let sum: f32 = samples[start..end].iter().sum();
        output.push(sum / (end - start) as f32);
    }
    output
}

/// f32 采样量化为 PCM16。非有限值按静音处理，超范围截断。
pub(crate) fn mono_f32_to_pcm16(samples: &[f32]) -> Vec<i16> {
    samples
        .iter()
        .map(|sample| {
            let value = if sample.is_finite() {
                sample.clamp(-1.0, 1.0)
            } else {
                0.0
            };
            (value * i16::MAX as f32).round() as i16
        })
        .collect()
}

/// 生成 16 kHz 单声道 PCM16 的 WAV 文件字节（44 字节标准头）。
pub(crate) fn pcm16_wav(samples: &[i16]) -> Vec<u8> {
    let data_len = samples.len() * 2;
    let mut wav = Vec::with_capacity(44 + data_len);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk 大小
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // 单声道
    wav.extend_from_slice(&TARGET_SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&(TARGET_SAMPLE_RATE * 2).to_le_bytes()); // byte rate
    wav.extend_from_slice(&2u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data_len as u32).to_le_bytes());
    for sample in samples {
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    wav
}

/// 一步到位：单声道 f32（设备采样率）→ 16 kHz PCM16 WAV。
pub(crate) fn encode_recording_wav(samples: &[f32], source_rate: u32) -> Vec<u8> {
    let resampled = downsample_to_target(samples, source_rate);
    let pcm = mono_f32_to_pcm16(&resampled);
    pcm16_wav(&pcm)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_u16(wav: &[u8], offset: usize) -> u16 {
        u16::from_le_bytes([wav[offset], wav[offset + 1]])
    }

    fn read_u32(wav: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes([
            wav[offset],
            wav[offset + 1],
            wav[offset + 2],
            wav[offset + 3],
        ])
    }

    #[test]
    fn wav_header_is_16k_mono_pcm16() {
        let wav = pcm16_wav(&[1, -1, 256, -256]);
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(read_u32(&wav, 16), 16, "fmt chunk 大小");
        assert_eq!(read_u16(&wav, 20), 1, "PCM 格式");
        assert_eq!(read_u16(&wav, 22), 1, "单声道");
        assert_eq!(read_u32(&wav, 24), 16_000, "采样率");
        assert_eq!(read_u32(&wav, 28), 32_000, "byte rate");
        assert_eq!(read_u16(&wav, 32), 2, "block align");
        assert_eq!(read_u16(&wav, 34), 16, "PCM16");
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(read_u32(&wav, 40), 8, "data chunk 大小");
        assert_eq!(wav.len(), 44 + 8);
        assert_eq!(read_u32(&wav, 4), (36 + 8) as u32, "RIFF chunk 大小");
        assert_eq!(read_u16(&wav, 44), 1);
        assert_eq!(read_u16(&wav, 46), u16::MAX, "负样本按补码存");
    }

    #[test]
    fn empty_recording_still_yields_valid_wav_header() {
        let wav = pcm16_wav(&[]);
        assert_eq!(wav.len(), 44);
        assert_eq!(read_u32(&wav, 40), 0);
        assert_eq!(read_u32(&wav, 4), 36);
    }

    #[test]
    fn downsample_48k_one_second_is_16k_samples() {
        let input = vec![0.25f32; 48_000];
        let output = downsample_mono(&input, 48_000, 16_000);
        assert_eq!(output.len(), 16_000);
    }

    #[test]
    fn downsample_44k1_one_second_is_16k_samples() {
        let input = vec![0.5f32; 44_100];
        let output = downsample_mono(&input, 44_100, 16_000);
        // 44100 * 16000 / 44100 = 16000，正好一秒。
        assert_eq!(output.len(), 16_000);
    }

    #[test]
    fn downsample_preserves_constant_level() {
        let input = vec![0.3f32; 32_000];
        let output = downsample_mono(&input, 32_000, 16_000);
        assert_eq!(output.len(), 16_000);
        assert!(output.iter().all(|value| (value - 0.3).abs() < 1e-5));
    }

    #[test]
    fn downsample_averages_within_window() {
        // 4 -> 2：每个输出样本取相邻两个输入样本的均值。
        let output = downsample_mono(&[1.0, 3.0, -1.0, 1.0], 4, 2);
        assert_eq!(output, vec![2.0, 0.0]);
    }

    #[test]
    fn encode_recording_pipes_through_downsample_and_quantization() {
        let input = vec![0.5f32; 48_000];
        let wav = encode_recording_wav(&input, 48_000);
        assert_eq!(wav.len(), 44 + 16_000 * 2);
        assert_eq!(read_u32(&wav, 40) as usize, 16_000 * 2);
        // 0.5 * 32767 ≈ 16383.5，四舍五入到 16384。
        assert_eq!(i16::from_le_bytes([wav[44], wav[45]]), 16_384);
    }

    #[test]
    fn pcm16_clamps_out_of_range_and_flattens_non_finite() {
        let pcm = mono_f32_to_pcm16(&[2.0, -2.0, f32::NAN, f32::INFINITY, 0.0]);
        assert_eq!(pcm, vec![i16::MAX, -i16::MAX, 0, 0, 0]);
    }

    fn fake_capture(samples: Vec<f32>) -> ActiveCapture {
        let buffer = Arc::new(Mutex::new(samples));
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let join = std::thread::spawn(move || {
            let _ = stop_rx.recv();
        });
        ActiveCapture {
            samples: buffer,
            sample_rate: 48_000,
            stop_tx,
            join,
        }
    }

    #[test]
    fn state_machine_rejects_concurrent_start() {
        let mut state = MicState::default();
        assert!(!state.is_active());
        state.begin().expect("第一次 start 应成功");
        assert!(state.is_active(), "启动中也算占用，避免二次打开设备");
        let error = state.begin().expect_err("重复 start 必须失败");
        assert!(error.to_string().contains("正在启动"), "{error}");
    }

    #[test]
    fn state_machine_stop_before_device_ready_is_error() {
        let mut state = MicState::default();
        state.begin().unwrap();
        let error = state
            .take_recording()
            .expect_err("设备未就绪时 stop 应返回错误");
        assert!(error.to_string().contains("正在启动"), "{error}");
        // 错误不能破坏占位，否则设备就绪后 install 会失败。
        assert!(state.is_active());
    }

    #[test]
    fn state_machine_install_and_take_recording() {
        let mut state = MicState::default();
        state.begin().unwrap();
        let capture = fake_capture(vec![0.1, 0.2, 0.3]);
        state.install(capture).expect("Starting 状态可以安装 capture");
        assert!(state.is_active());
        assert!(state.begin().is_err());
        let capture = state.take_recording().expect("录音中应能取出");
        assert!(!state.is_active(), "取出后回到 Idle");
        let samples = finish_capture(capture);
        assert_eq!(samples, vec![0.1, 0.2, 0.3]);
        assert!(
            state.take_recording().is_err(),
            "停止后再次 stop 必须报错而不是返回空 WAV"
        );
    }

    #[test]
    fn state_machine_cancel_discards_and_reports_idle() {
        let mut state = MicState::default();
        state.begin().unwrap();
        let capture = fake_capture(vec![0.9]);
        state.install(capture).unwrap();
        let capture = state.discard().expect("cancel 应取回 capture");
        assert!(!state.is_active());
        let _ = finish_capture(capture); // cancel 丢弃采样，只要求线程能正常收尾
        assert!(state.discard().is_none(), "空闲时 cancel 直接 Ok");
    }

    #[test]
    fn install_rejects_capture_when_state_was_cleared() {
        let mut state = MicState::default();
        state.begin().unwrap();
        let _ = state.discard(); // 模拟 start 等待期间被 cancel
        let capture = fake_capture(vec![]);
        let capture = state
            .install(capture)
            .expect_err("占位被清掉后不能安装 capture");
        // 还回来的 capture 必须能被正常停掉，不能泄漏设备。
        let _ = finish_capture(capture);
        assert!(!state.is_active());
    }
}
