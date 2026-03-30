use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter};

/// Shared state for the audio recorder
pub struct AudioRecorder {
    /// Collected PCM samples (mono f32)
    samples: Arc<Mutex<Vec<f32>>>,
    /// Whether we're currently recording
    is_recording: Arc<AtomicBool>,
    /// Sample rate of the capture device
    sample_rate: Arc<Mutex<u32>>,
    /// Current audio level (0.0 - 1.0) for UI
    audio_level: Arc<Mutex<f32>>,
}

impl AudioRecorder {
    pub fn new() -> Self {
        Self {
            samples: Arc::new(Mutex::new(Vec::new())),
            is_recording: Arc::new(AtomicBool::new(false)),
            sample_rate: Arc::new(Mutex::new(44100)),
            audio_level: Arc::new(Mutex::new(0.0)),
        }
    }

    pub fn is_recording(&self) -> bool {
        self.is_recording.load(Ordering::SeqCst)
    }

    /// Start recording audio. Spawns a thread that captures audio and runs VAD.
    /// When silence is detected after speech, emits "voice-recorded" with WAV base64.
    pub fn start(&self, app_handle: AppHandle) {
        if self.is_recording.load(Ordering::SeqCst) {
            return;
        }

        let samples = self.samples.clone();
        let is_recording = self.is_recording.clone();
        let sample_rate_store = self.sample_rate.clone();
        let audio_level = self.audio_level.clone();

        // Clear previous samples
        samples.lock().unwrap().clear();
        is_recording.store(true, Ordering::SeqCst);

        std::thread::spawn(move || {
            let host = cpal::default_host();
            let device = match host.default_input_device() {
                Some(d) => d,
                None => {
                    let _ = app_handle.emit("voice-error", "No audio input device found");
                    is_recording.store(false, Ordering::SeqCst);
                    return;
                }
            };

            let config = match device.default_input_config() {
                Ok(c) => c,
                Err(e) => {
                    let _ = app_handle.emit("voice-error", format!("Audio config error: {e}"));
                    is_recording.store(false, Ordering::SeqCst);
                    return;
                }
            };

            let sr = config.sample_rate();
            *sample_rate_store.lock().unwrap() = sr;
            let channels = config.channels() as usize;

            let samples_write = samples.clone();
            let is_rec = is_recording.clone();
            let level = audio_level.clone();

            // VAD state
            let mut smoothed_rms: f32 = 0.0;
            let mut speech_detected = false;
            let mut silence_frames: u32 = 0;
            let mut speech_frames: u32 = 0;
            let speech_threshold: f32 = 0.01;
            let speech_start_count: u32 = 3; // frames of speech to confirm
            let app_for_level = app_handle.clone();

            // How many callback invocations ≈ 100ms (for VAD tick rate)
            // cpal typically gives ~10ms buffers, so check every ~10 callbacks
            let mut callback_count: u32 = 0;

            let stream = match device.build_input_stream(
                &config.into(),
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    if !is_rec.load(Ordering::SeqCst) {
                        return;
                    }

                    // Downmix to mono and collect samples
                    let mut mono_samples = Vec::with_capacity(data.len() / channels);
                    for chunk in data.chunks(channels) {
                        let sum: f32 = chunk.iter().sum();
                        mono_samples.push(sum / channels as f32);
                    }

                    // Compute RMS for this buffer
                    let rms = if mono_samples.is_empty() {
                        0.0
                    } else {
                        let sum_sq: f32 = mono_samples.iter().map(|s| s * s).sum();
                        (sum_sq / mono_samples.len() as f32).sqrt()
                    };

                    smoothed_rms = smoothed_rms * 0.85 + rms * 0.15;
                    *level.lock().unwrap() = smoothed_rms.min(1.0);

                    // Store samples
                    samples_write.lock().unwrap().extend_from_slice(&mono_samples);

                    // VAD logic (check roughly every 100ms)
                    callback_count += 1;
                    if callback_count % 10 == 0 {
                        if smoothed_rms > speech_threshold {
                            speech_frames += 1;
                            silence_frames = 0;
                            if speech_frames >= speech_start_count {
                                speech_detected = true;
                            }
                        } else {
                            speech_frames = 0;
                            if speech_detected {
                                silence_frames += 1;
                            }
                        }

                        // Emit audio level for UI visualization
                        let _ = app_for_level.emit("voice-level", smoothed_rms);

                        // Adaptive silence timeout based on recording length
                        let total_samples = samples_write.lock().unwrap().len();
                        let duration_s = total_samples as f32 / sr as f32;
                        let silence_timeout_ticks = if duration_s < 3.0 {
                            20 // 2.0s
                        } else if duration_s > 10.0 {
                            35 // 3.5s
                        } else {
                            25 // 2.5s
                        };

                        if speech_detected && silence_frames >= silence_timeout_ticks {
                            // Auto-stop: silence detected after speech
                            is_rec.store(false, Ordering::SeqCst);
                        }
                    }
                },
                move |err| {
                    eprintln!("Audio capture error: {err}");
                },
                None,
            ) {
                Ok(s) => s,
                Err(e) => {
                    let _ = app_handle.emit("voice-error", format!("Failed to start audio: {e}"));
                    is_recording.store(false, Ordering::SeqCst);
                    return;
                }
            };

            if let Err(e) = stream.play() {
                let _ = app_handle.emit("voice-error", format!("Failed to play stream: {e}"));
                is_recording.store(false, Ordering::SeqCst);
                return;
            }

            // Wait until recording stops (either by VAD or manual stop)
            while is_recording.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }

            drop(stream);

            // Convert samples to 16kHz 16-bit mono WAV
            let raw = samples.lock().unwrap();
            if raw.is_empty() {
                let _ = app_handle.emit("voice-error", "No audio captured");
                return;
            }

            let wav_data = pcm_to_wav(&raw, sr, 16000);
            let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &wav_data);
            let _ = app_handle.emit("voice-recorded", b64);
        });
    }

    /// Stop recording manually (e.g., user clicks mic button again)
    pub fn stop(&self) {
        self.is_recording.store(false, Ordering::SeqCst);
    }

    pub fn audio_level(&self) -> f32 {
        *self.audio_level.lock().unwrap()
    }
}

/// Resample f32 PCM from `from_rate` to `to_rate` using linear interpolation
fn resample(samples: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    if from_rate == to_rate {
        return samples.to_vec();
    }
    let ratio = from_rate as f64 / to_rate as f64;
    let out_len = (samples.len() as f64 / ratio) as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src_idx = i as f64 * ratio;
        let idx = src_idx as usize;
        let frac = src_idx - idx as f64;
        let s0 = samples[idx];
        let s1 = if idx + 1 < samples.len() {
            samples[idx + 1]
        } else {
            s0
        };
        out.push((s0 as f64 * (1.0 - frac) + s1 as f64 * frac) as f32);
    }
    out
}

/// Convert f32 PCM samples to a WAV file (16-bit, mono, target_rate Hz)
fn pcm_to_wav(samples: &[f32], source_rate: u32, target_rate: u32) -> Vec<u8> {
    let resampled = resample(samples, source_rate, target_rate);

    // Convert f32 (-1.0..1.0) to i16
    let pcm16: Vec<i16> = resampled
        .iter()
        .map(|&s| {
            let clamped = s.max(-1.0).min(1.0);
            (clamped * 32767.0) as i16
        })
        .collect();

    let data_len = (pcm16.len() * 2) as u32;
    let file_len = 36 + data_len;
    let byte_rate = target_rate * 2; // 16-bit mono
    let block_align: u16 = 2;

    let mut wav = Vec::with_capacity(44 + data_len as usize);

    // RIFF header
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&file_len.to_le_bytes());
    wav.extend_from_slice(b"WAVE");

    // fmt chunk
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // chunk size
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM format
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&target_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample

    // data chunk
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for sample in &pcm16 {
        wav.extend_from_slice(&sample.to_le_bytes());
    }

    wav
}
