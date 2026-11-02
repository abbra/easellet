use candle_core::{Device, IndexOp, Tensor};
use candle_nn::ops::softmax;
use candle_transformers::models::whisper::{self as m, audio, Config};
use flate2::{write::ZlibEncoder, Compression};
use futures_channel::mpsc::UnboundedSender;
use kalosm_common::{accelerated_device_if_available, CacheError, TensorCache};
use rand::{distributions::Distribution, SeedableRng};
use std::{
    io::Write,
    num::NonZeroUsize,
    ops::RangeInclusive,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokenizers::Tokenizer;

use super::{DecodingResult, Segment};
use crate::{
    quantized::TextDecoderCache, Task, TaskType, TokenChunk, WhisperBuilder, WhisperLanguage,
};

enum ModelType {
    Quantized(crate::quantized::Whisper),
    Unquantized(m::model::Whisper),
}

impl ModelType {
    fn load(
        weights_filename: &PathBuf,
        device: &Device,
        config: Config,
        quantized: bool,
    ) -> candle_core::Result<Self> {
        if quantized {
            let vb = crate::m::quantized_model::VarBuilder::from_gguf(weights_filename, device)?;
            Ok(Self::Quantized(crate::quantized::Whisper::load(
                &vb, config,
            )?))
        } else {
            let vb = unsafe {
                candle_nn::VarBuilder::from_mmaped_safetensors(
                    &[weights_filename],
                    m::DTYPE,
                    device,
                )?
            };
            Ok(Self::Unquantized(m::model::Whisper::load(&vb, config)?))
        }
    }

    fn config(&self) -> &Config {
        match self {
            Self::Quantized(model) => &model.config,
            Self::Unquantized(model) => &model.config,
        }
    }
}

/// An error that can occur when loading a [`Whisper`](crate::Whisper) model.
#[derive(Debug, thiserror::Error)]
pub enum WhisperLoadingError {
    /// An error that can occur when trying to load a [`Whisper`](crate::Whisper) model from huggingface or a local file.
    #[error("Failed to load model from huggingface or local file: {0}")]
    DownloadingError(#[from] CacheError),
    /// An error that can occur when trying to load a [`Whisper`](crate::Whisper) model.
    #[error("Failed to load model into device: {0}")]
    LoadModel(#[from] candle_core::Error),
    /// An error that can occur when trying to load the whisper tokenizer.
    #[error("Failed to load tokenizer: {0}")]
    LoadTokenizer(tokenizers::Error),
    /// An error that can occur when trying to load the whisper config.
    #[error("Failed to load config: {0}")]
    LoadConfig(serde_json::Error),
    /// Unsupported mel filter length
    #[error("Unsupported mel filter length: {0}; only 80 and 128 are supported")]
    UnsupportedMelFilterLength(usize),
    /// Language not supported
    #[error("Language not supported: {0}")]
    UnsupportedLanguage(WhisperLanguage),
}

/// An error that can occur when running a [`Whisper`] model.
#[derive(Debug, thiserror::Error)]
pub enum WhisperError {
    /// An error that can occur when trying to run a [`Whisper`] model.
    #[error("Candle error: {0}")]
    Candle(#[from] candle_core::Error),
    /// An error that can occur when encoding or decoding for a [`Whisper`] model.
    #[error("Tokenizer error: {0}")]
    Tokenizer(tokenizers::Error),
    /// An error that can occur when compressing the text the model generates to determine the compression ratio.
    #[error("Compression error: {0}")]
    Compression(std::io::Error),
}

/// What the caller steers decoding with, shared with the worker thread.
#[derive(Debug, Default)]
pub(crate) struct Steering {
    /// Text the model treats as just heard (vocabulary such as rare names).
    pub(crate) prompt: String,
    /// Two-letter code of the language the caller expects.
    pub(crate) language_hint: Option<String>,
}

/// Logit head start of the hinted language in detection (e^2.5 ~ 12x odds).
const HINT_BONUS: f32 = 2.5;

pub(crate) struct WhisperInner {
    mel_filters: Vec<f32>,
    device: Device,
    decoder: Decoder,
    config: Config,
    /// Set by the worker thread so the bridge can choose a TTS voice.
    language_slot: Arc<Mutex<Option<String>>>,
}

impl WhisperInner {
    pub(crate) fn new(
        settings: WhisperBuilder,
        weights_filename: PathBuf,
        tokenizer_filename: PathBuf,
        config_filename: PathBuf,
        language_slot: Arc<Mutex<Option<String>>>,
        steering: Arc<Mutex<Steering>>,
    ) -> Result<Self, WhisperLoadingError> {
        let device = accelerated_device_if_available()?;
        let tokenizer =
            Tokenizer::from_file(tokenizer_filename).map_err(WhisperLoadingError::LoadTokenizer)?;
        let config: Config =
            serde_json::from_str(&std::fs::read_to_string(config_filename).unwrap())
                .map_err(WhisperLoadingError::LoadConfig)?;

        let mel_bytes = match config.num_mel_bins {
            80 => include_bytes!("melfilters.bytes").as_slice(),
            128 => include_bytes!("melfilters128.bytes").as_slice(),
            nmel => return Err(WhisperLoadingError::UnsupportedMelFilterLength(nmel)),
        };
        let mut mel_filters = vec![0f32; mel_bytes.len() / 4];
        <byteorder::LittleEndian as byteorder::ByteOrder>::read_f32_into(
            mel_bytes,
            &mut mel_filters,
        );
        let attention_heads = settings.model.timestamp_attention_heads();

        let model = ModelType::load(
            &weights_filename,
            &device,
            config.clone(),
            settings.model.is_quantized(),
        )?;
        let auto_language = settings.model.is_multilingual() && settings.language.is_none();
        let language_token = if settings.model.is_multilingual() {
            match settings.language {
                Some(language) => match token_id(&tokenizer, &format!("<|{language}|>")) {
                    Ok(token_id) => Some(token_id),
                    Err(_) => return Err(WhisperLoadingError::UnsupportedLanguage(language)),
                },
                // No language was requested: detect it from the first chunk below.
                None => None,
            }
        } else {
            None
        };
        let decoder = Decoder::new(
            model,
            tokenizer,
            0,
            &device,
            language_token,
            auto_language,
            settings.language_filter,
            attention_heads,
            steering,
        )?;

        Ok(Self {
            mel_filters,
            device,
            decoder,
            config,
            language_slot,
        })
    }

    pub(crate) fn transcribe(
        &mut self,
        pcm_data: Vec<f32>,
        word_level_time_stamps: bool,
        result: UnboundedSender<Segment>,
    ) {
        let mel = audio::pcm_to_mel(&self.config, &pcm_data, &self.mel_filters);
        let mel_len = mel.len();
        let mel = Tensor::from_vec(
            mel,
            (self.config.num_mel_bins, mel_len / self.config.num_mel_bins),
            &self.device,
        )
        .unwrap();

        if let Err(err) = self.decoder.run(
            &mel,
            pcm_data.len(),
            Task {
                task_type: TaskType::Transcribe,
                word_level_time_stamps,
                without_timestamps: true,
            },
            result,
        ) {
            tracing::error!("Error transcribing audio: {err}");
        }
        if let Some(code) = self.decoder.detected_language() {
            if let Ok(mut slot) = self.language_slot.lock() {
                *slot = Some(code.to_owned());
            }
        }
    }
}

struct Decoder {
    model: ModelType,
    rng: rand::rngs::StdRng,
    tokenizer: Tokenizer,
    suppress_tokens: Tensor,
    sot_token: u32,
    transcribe_token: u32,
    translate_token: u32,
    eot_token: u32,
    no_speech_token: u32,
    no_timestamps_token: u32,
    language_token: Option<u32>,
    /// Detect the language from the audio when `language_token` is `None`.
    auto_language: bool,
    /// The language chosen for the current utterance, including a detected one.
    active_language: Option<u32>,
    /// Two-letter code of a detected language, for the caller's voice choice.
    detected_language: Option<String>,
    /// Every `<|xx|>` token in the tokenizer, with its code, for detection.
    language_tokens: Vec<(u32, String)>,
    /// Codes [`Decoder::detect_language`] may return; empty for all of them.
    language_filter: Vec<String>,
    timestamp_token_range: RangeInclusive<u32>,
    attention_heads: Option<&'static [[usize; 2]]>,
    /// `<|startofprev|>`, which introduces a text prompt before the transcript.
    sot_prev_token: Option<u32>,
    /// The caller's prompt and language hint (`Whisper::set_prompt`,
    /// `Whisper::set_language_hint`), read at every utterance.
    steering: Arc<Mutex<Steering>>,
}

impl Decoder {
    #[allow(clippy::too_many_arguments)]
    fn new(
        model: ModelType,
        tokenizer: Tokenizer,
        seed: u64,
        device: &Device,
        language_token: Option<u32>,
        auto_language: bool,
        language_filter: Vec<String>,
        attention_heads: Option<&'static [[usize; 2]]>,
        steering: Arc<Mutex<Steering>>,
    ) -> candle_core::Result<Self> {
        let no_timestamps_token = token_id(&tokenizer, m::NO_TIMESTAMPS_TOKEN)?;
        // Suppress the notimestamps token when in timestamps mode.
        // https://github.com/openai/whisper/blob/e8622f9afc4eba139bf796c210f5c01081000472/whisper/decoding.py#L452
        let suppress_tokens: Vec<f32> = (0..model.config().vocab_size as u32)
            .map(|i| {
                if model.config().suppress_tokens.contains(&i) {
                    f32::NEG_INFINITY
                } else {
                    0f32
                }
            })
            .collect();
        let suppress_tokens = Tensor::new(suppress_tokens.as_slice(), device)?;
        let sot_token = token_id(&tokenizer, m::SOT_TOKEN)?;
        let sot_prev_token = token_id(&tokenizer, "<|startofprev|>").ok();
        let transcribe_token = token_id(&tokenizer, m::TRANSCRIBE_TOKEN)?;
        let translate_token = token_id(&tokenizer, m::TRANSLATE_TOKEN)?;
        let eot_token = token_id(&tokenizer, m::EOT_TOKEN)?;
        let no_speech_token = m::NO_SPEECH_TOKENS
            .iter()
            .find_map(|token| token_id(&tokenizer, token).ok())
            .ok_or_else(|| candle_core::Error::Msg("no_speech_token not found".to_string()))?;
        let timestamp_tokens = (0..=1500)
            .map(|i| token_id(&tokenizer, &format!("<|{:.2}|>", i as f32 * 0.02)))
            .collect::<candle_core::Result<Vec<_>>>()?;
        let timestamp_token_range =
            *timestamp_tokens.first().unwrap()..=*timestamp_tokens.last().unwrap();
        // Whisper's language tokens are the two-letter `<|xx|>` entries. Picking the
        // largest logit among them is the reference language-detection step, done
        // here because the tokenizer is already open.
        let language_tokens: Vec<(u32, String)> = tokenizer
            .get_vocab(true)
            .into_iter()
            .filter_map(|(token, id)| {
                let code = token.strip_prefix("<|")?.strip_suffix("|>")?;
                (code.len() == 2 && code.bytes().all(|byte| byte.is_ascii_lowercase()))
                    .then(|| (id, code.to_owned()))
            })
            .collect();
        debug_assert!(timestamp_tokens
            .iter()
            .all(|t| timestamp_token_range.contains(t)));

        Ok(Self {
            model,
            rng: rand::rngs::StdRng::seed_from_u64(seed),
            tokenizer,
            suppress_tokens,
            sot_token,
            transcribe_token,
            translate_token,
            eot_token,
            no_speech_token,
            language_token,
            auto_language,
            active_language: language_token,
            detected_language: None,
            language_tokens,
            language_filter,
            no_timestamps_token,
            timestamp_token_range,
            attention_heads,
            sot_prev_token,
            steering,
        })
    }

    /// `<|startofprev|>` and the prompt's tokens, the last 223 of them as the
    /// reference implementation keeps; empty when there is no prompt.
    fn prompt_prefix(&self) -> Vec<u32> {
        let Some(sot_prev) = self.sot_prev_token else {
            return Vec::new();
        };
        let text = self
            .steering
            .lock()
            .map(|steering| steering.prompt.trim().to_owned())
            .unwrap_or_default();
        if text.is_empty() {
            return Vec::new();
        }
        let Ok(encoding) = self.tokenizer.encode(format!(" {text}"), false) else {
            return Vec::new();
        };
        let ids = encoding.get_ids();
        let keep = &ids[ids.len().saturating_sub(223)..];
        let mut prefix = Vec::with_capacity(keep.len() + 1);
        prefix.push(sot_prev);
        prefix.extend_from_slice(keep);
        prefix
    }

    pub(crate) fn is_special(&self, token: u32) -> bool {
        self.timestamp_token_range.contains(&token)
            || token == self.sot_token
            || token == self.transcribe_token
            || token == self.translate_token
            || token == self.no_timestamps_token
            || token == self.eot_token
    }

    fn is_timestamp_or_eot(&self, token: u32) -> bool {
        self.timestamp_token_range.contains(&token) || token == self.eot_token
    }

    fn special_tokens(&self) -> impl Iterator<Item = u32> + '_ {
        self.timestamp_token_range
            .clone()
            .chain(std::iter::once(self.eot_token))
    }

    fn encode(&mut self, mel: &Tensor) -> candle_core::Result<Tensor> {
        let tensor = match &mut self.model {
            ModelType::Quantized(model) => model.encoder.forward(mel)?,
            ModelType::Unquantized(model) => model.encoder.forward(mel, true)?,
        };

        Ok(tensor)
    }

    /// The language detected for the utterance just transcribed, if auto mode ran.
    pub(crate) fn detected_language(&self) -> Option<&str> {
        self.detected_language.as_deref()
    }

    /// Return the most likely `<|xx|>` language token for this chunk.
    ///
    /// This is one decoder step with `<|startoftranscript|>` alone, which is what
    /// Whisper's own language detection does. It is only reached when the model was
    /// built without a fixed language, so a multilingual bridge pays one extra
    /// token of work per utterance and can then answer each speaker in their own
    /// language.
    fn detect_language(&mut self, audio_features: &Tensor) -> candle_core::Result<u32> {
        let tokens = [self.sot_token];
        let ys = match &mut self.model {
            ModelType::Quantized(model) => {
                let mut cache = TextDecoderCache::new();
                model
                    .decoder
                    .forward(&tokens, audio_features, &mut cache, None)?
            }
            ModelType::Unquantized(model) => {
                let tokens = Tensor::new(tokens.as_slice(), audio_features.device())?.unsqueeze(0)?;
                model.decoder.forward(&tokens, audio_features, true)?
            }
        };
        let logits = match &mut self.model {
            ModelType::Quantized(model) => model.decoder.final_linear(&ys.i(..1)?)?,
            ModelType::Unquantized(model) => model.decoder.final_linear(&ys.i(..1)?)?,
        }
        .i(0)?
        .i(0)?;
        let hint = self
            .steering
            .lock()
            .ok()
            .and_then(|steering| steering.language_hint.clone());
        let mut best: Option<(f32, u32)> = None;
        for (id, code) in &self.language_tokens {
            // An empty filter means every language, which is Whisper's own
            // behaviour; a filter is how a caller says which ones can occur here.
            if !self.language_filter.is_empty() && !self.language_filter.contains(code) {
                continue;
            }
            let mut score = logits.i(*id as usize)?.to_scalar::<f32>()?;
            // The caller's expected language starts ahead: a clear sentence in
            // another language still wins, a lone short word («Рамка», heard
            // as English "Ram") does not.
            if hint.as_deref() == Some(code.as_str()) {
                score += HINT_BONUS;
            }
            if best.is_none_or(|(best_score, _)| score > best_score) {
                best = Some((score, *id));
            }
        }
        best.map(|(_, id)| id)
            .ok_or_else(|| candle_core::Error::Msg("no language tokens in tokenizer".to_owned()))
    }

    fn decode(
        &mut self,
        audio_features: &Tensor,
        temperature: f64,
        task: Task,
        previous_tokens: &[u32],
        n_frames: usize,
        deadline: Instant,
    ) -> Result<DecodingResult, WhisperError> {
        let sample_len = self.model.config().max_target_positions / 2;
        let mut sum_logprob = 0f64;
        let mut no_speech_prob = f64::NAN;
        let mut tokens = vec![self.sot_token];
        if let Some(language_token) = self.active_language.or(self.language_token) {
            tokens.push(language_token);
        }
        match task.task_type {
            TaskType::Transcribe => tokens.push(self.transcribe_token),
            TaskType::Translate => tokens.push(self.translate_token),
            TaskType::Unset => {}
        }
        if task.without_timestamps {
            tokens.push(self.no_timestamps_token);
        } else {
            tokens.push(*self.timestamp_token_range.start());
        }
        tokens.extend(previous_tokens);
        // The tokens that are queued for decoding
        let n_start_tokens = tokens.len();
        // A prompt is model input only: it goes before `<|startoftranscript|>`
        // in what the decoder sees, never into `tokens`, whose text is the
        // result. Word timestamps align attention by position, so they go
        // without it.
        let mut queued_tokens = if task.word_level_time_stamps {
            Vec::new()
        } else {
            self.prompt_prefix()
        };
        // Where `<|startoftranscript|>` sits in what the decoder sees: the
        // no-speech probability is read at that position, not at the prompt's.
        let sot_position = queued_tokens.len();
        queued_tokens.extend_from_slice(&tokens);
        let mut cache = TextDecoderCache::new();
        let mut attention_output = None;
        for i in 0..sample_len {
            let ys = match &mut self.model {
                ModelType::Quantized(model) => {
                    if task.word_level_time_stamps {
                        attention_output.get_or_insert_with(|| {
                            let mut outputs = Vec::new();
                            for _ in 0..model.decoder.block_count() {
                                outputs.push(TensorCache::new(2, usize::MAX));
                            }
                            outputs
                        });
                    }
                    if let Some(last_mut) = queued_tokens.last_mut() {
                        if last_mut == &self.eot_token {
                            // When configured to output word-level timestamps, the OpenAI inference
                            // implementation passes a timestamp token with the nearest second in the
                            // last pass. While the predicted token from this pass is not included in the
                            // output transcript, it impacts the word/token-level timestamps.
                            let nearest_second =
                                n_frames as f32 * m::HOP_LENGTH as f32 / m::SAMPLE_RATE as f32;
                            let nearest_second_02 = nearest_second / 0.02;
                            let nearest_second_02 = nearest_second_02 as usize;
                            let timestamp_token =
                                *self.timestamp_token_range.start() + nearest_second_02 as u32;

                            *last_mut = timestamp_token;
                        }
                    }
                    let result = model.decoder.forward(
                        &queued_tokens,
                        audio_features,
                        &mut cache,
                        attention_output.as_deref_mut(),
                    )?;

                    // The quantized model caches tokens so we can remove any old tokens
                    queued_tokens.clear();
                    result
                }
                ModelType::Unquantized(model) => {
                    let tokens_t = Tensor::new(queued_tokens.as_slice(), audio_features.device())?;
                    // The model expects a batch dim but this inference loop does not handle
                    // it so we add it at this point.
                    let tokens_t = tokens_t.unsqueeze(0)?;
                    model.decoder.forward(&tokens_t, audio_features, i == 0)?
                }
            };

            // Extract the no speech probability on the first iteration by looking at the first
            // token logits and the probability for the according token.
            if i == 0 {
                let logits = match &mut self.model {
                    ModelType::Quantized(model) => model.decoder.final_linear(&ys.i(..1)?)?,
                    ModelType::Unquantized(model) => model.decoder.final_linear(&ys.i(..1)?)?,
                }
                .i(0)?
                .i(sot_position)?;
                no_speech_prob = softmax(&logits, 0)?
                    .i(self.no_speech_token as usize)?
                    .to_scalar::<f32>()? as f64;
                eprintln!("PROBE no_speech={no_speech_prob:.3} temp={temperature}");
            }

            let (_, seq_len, _) = ys.dims3()?;
            let logits = match &mut self.model {
                ModelType::Quantized(model) => {
                    model.decoder.final_linear(&ys.i((..1, seq_len - 1..))?)?
                }
                ModelType::Unquantized(model) => {
                    model.decoder.final_linear(&ys.i((..1, seq_len - 1..))?)?
                }
            }
            .i(0)?
            .i(0)?;
            // TODO: Besides suppress tokens, we should apply the heuristics from
            // ApplyTimestampRules, i.e.:
            // - Timestamps come in pairs, except before EOT.
            // - Timestamps should be non-decreasing.
            // - If the sum of the probabilities of timestamps is higher than any other tokens,
            //   only consider timestamps when sampling.
            // https://github.com/openai/whisper/blob/e8622f9afc4eba139bf796c210f5c01081000472/whisper/decoding.py#L439
            let logits = logits.broadcast_add(&self.suppress_tokens)?;
            let next_token = if temperature > 0f64 {
                let prs = softmax(&(&logits / temperature)?, 0)?;
                let logits_v: Vec<f32> =
                    self.apply_timestamp_rules(prs, &tokens, task.without_timestamps)?;
                // Weights may be NaN or all zero when decoding has lost its way
                // (seen on room noise behind a prompt); end the segment there
                // rather than fail the whole utterance after seconds of work.
                match rand::distributions::WeightedIndex::new(&logits_v) {
                    Ok(distr) => distr.sample(&mut self.rng) as u32,
                    Err(_) => self.eot_token,
                }
            } else {
                let logits = softmax(&logits, 0)?;
                let logits_v: Vec<f32> =
                    self.apply_timestamp_rules(logits, &tokens, task.without_timestamps)?;
                logits_v
                    .iter()
                    .enumerate()
                    .max_by(|(_, u), (_, v)| u.total_cmp(v))
                    .map(|(i, _)| i as u32)
                    .unwrap()
            };
            // After the final pass if word level timestamps are requested, we stop decoding
            if task.word_level_time_stamps && tokens.last() == Some(&self.eot_token) {
                break;
            }
            tokens.push(next_token);
            queued_tokens.push(next_token);
            let prob = softmax(&logits, candle_core::D::Minus1)?
                .i(next_token as usize)?
                .to_scalar::<f32>()? as f64;
            // If we have read the maximum number of tokens, stop regardless of the eot token
            // Or if word level timestamps are disabled, stop as soon was we reach the eot token
            // A deadline is the last resort: noise can make the model loop for minutes.
            if tokens.len() > 128
                || tokens.len() > self.model.config().max_target_positions
                || Instant::now() >= deadline
                || (!task.word_level_time_stamps && next_token == self.eot_token)
            {
                break;
            }
            sum_logprob += prob.ln();
        }
        let mut token_timestamps = None;
        if let Some(attention_output) = attention_output.as_mut() {
            let result = crate::quantized::Whisper::dtw_timestamps(
                self.attention_heads,
                const { NonZeroUsize::new(7).unwrap() },
                n_frames,
                n_start_tokens,
                attention_output,
            )
            .unwrap();
            let [timestamps] = result.as_slice() else {
                panic!("The timestamps should be a single vector when the batch size is 1");
            };
            token_timestamps = Some(timestamps.clone());
        }

        let (text, chunks) = {
            let mut remaining_tokens: Vec<_> = tokens
                .iter()
                .copied()
                .filter(|t| !self.is_special(*t))
                .enumerate()
                .collect();
            remaining_tokens.reverse();
            let mut queued_tokens = Vec::new();
            let mut timestamp_start = None;
            let mut prev_text_len = 0;
            let mut chunks = Vec::new();
            let mut current_text = String::new();
            while let Some((index, token)) = remaining_tokens.pop() {
                queued_tokens.push(token);
                if let Some(timestamps) = &token_timestamps {
                    if timestamp_start.is_none() {
                        timestamp_start = Some(timestamps[index]);
                    }
                }
                let detokenized = self
                    .tokenizer
                    .decode(&queued_tokens, true)
                    .map_err(WhisperError::Tokenizer)?;
                if detokenized.len() > prev_text_len
                    && detokenized.chars().last().unwrap().is_ascii()
                {
                    let timestamp = token_timestamps.as_ref().map(|timestamps| {
                        let start = timestamp_start.unwrap();
                        let end = timestamps[index];
                        timestamp_start = Some(end);
                        start..end
                    });
                    let text_range = current_text.len()..current_text.len() + detokenized.len();
                    current_text += &detokenized;
                    queued_tokens.clear();
                    prev_text_len = 0;
                    let token = TokenChunk {
                        text_range,
                        timestamp,
                    };
                    chunks.push(token);
                } else {
                    prev_text_len = detokenized.len();
                }
            }

            if !queued_tokens.is_empty() {
                let detokenized = self
                    .tokenizer
                    .decode(&queued_tokens, true)
                    .map_err(WhisperError::Tokenizer)?;
                let timestamp = token_timestamps.as_ref().map(|timestamps| {
                    let start = timestamp_start.unwrap();
                    let end = *timestamps.last().unwrap();
                    start..end
                });
                let text_range = current_text.len()..current_text.len() + detokenized.len();
                current_text += &detokenized;
                let token = TokenChunk {
                    text_range,
                    timestamp,
                };
                chunks.push(token);
            }

            (current_text, chunks)
        };
        let avg_logprob = sum_logprob / tokens.len() as f64;

        let compression_ratio = {
            let mut e = ZlibEncoder::new(Vec::new(), Compression::default());
            e.write_all(text.as_bytes())
                .map_err(WhisperError::Compression)?;
            let compressed_bytes = e.finish();

            compressed_bytes
                .map(|buf| text.len() as f64 / buf.len() as f64)
                .unwrap_or_default()
        };

        Ok(DecodingResult {
            text,
            avg_logprob,
            no_speech_prob,
            compression_ratio,
            chunks,
        })
    }

    fn decode_with_fallback(
        &mut self,
        audio_features: &Tensor,
        task: Task,
        previous_tokens: &[u32],
        n_frames: usize,
    ) -> Result<DecodingResult, WhisperError> {
        // The full ladder is six passes and the bridge has one request slot. Two
        // greedy/slightly-sampled passes are enough for clean speech; more than
        // that just multiplies the time a noisy capture can block the server.
        let temperatures = &m::TEMPERATURES[..m::TEMPERATURES.len().min(2)];
        let deadline = Instant::now() + Duration::from_secs(20);
        for (i, &t) in temperatures.iter().enumerate() {
            let dr: Result<DecodingResult, WhisperError> = self.decode(
                audio_features,
                t,
                task,
                previous_tokens,
                n_frames,
                deadline,
            );
            if i == temperatures.len() - 1 {
                return dr;
            }
            // On errors, we try again with a different temperature.
            match dr {
                Ok(dr) => {
                    let needs_fallback = dr.compression_ratio > m::COMPRESSION_RATIO_THRESHOLD
                        || dr.avg_logprob < m::LOGPROB_THRESHOLD;
                    if !needs_fallback && dr.no_speech_prob < m::NO_SPEECH_THRESHOLD {
                        return Ok(dr);
                    }
                }
                Err(err) => {
                    tracing::trace!("Error running at {t}: {err}")
                }
            }
        }
        unreachable!()
    }

    fn run(
        &mut self,
        mel: &Tensor,
        audio_frames: usize,
        task: Task,
        mut result: UnboundedSender<Segment>,
    ) -> Result<(), WhisperError> {
        // TODO: This should be dynamic based on how much memory the model uses and how much memory is available
        const MAX_CHUNKS: usize = 1;

        // A fixed language survives from load time; auto mode starts fresh for every
        // utterance, so a Russian question cannot set the language for the next one.
        self.active_language = self.language_token;
        self.detected_language = None;
        let (_, content_frames) = mel.dims2()?;
        let mut seek = 0;
        let start_time = Instant::now();
        let mut chunk_indices = Vec::new();
        let mut chunked = Vec::new();
        // Keep looping until we have all the chunks we need
        while seek < content_frames {
            // Take a chunk up to the maximum size
            chunk_indices.clear();
            chunked.clear();
            while chunk_indices.len() < MAX_CHUNKS && seek < content_frames {
                let remaining_frames = content_frames - seek;
                let segment_size = usize::min(remaining_frames, m::N_FRAMES);
                // If the new frame doesn't fit into a perfect chunk, just include it in the next chunk
                if remaining_frames < m::N_FRAMES && !chunk_indices.is_empty() {
                    break;
                }
                chunk_indices.push(seek..seek + segment_size);
                let mel_segment = mel.narrow(1, seek, segment_size)?;
                chunked.push(mel_segment);
                seek += segment_size;
            }

            // Encode all of the chunks
            let batched_mel_segment = Tensor::stack(&chunked, 0)?;
            let batched_audio_features = self.encode(&batched_mel_segment)?;
            let split = batched_audio_features.chunk(chunk_indices.len(), 0)?;
            if self.auto_language && self.active_language.is_none() {
                let token = self.detect_language(&split[0])?;
                self.active_language = Some(token);
                self.detected_language = self
                    .language_tokens
                    .iter()
                    .find(|(id, _)| *id == token)
                    .map(|(_, code)| code.clone());
            }

            // Tokens that are remaining in the last chunk's sentence fragment
            let mut tokens_in_sentence_fragment = Vec::new();

            for (audio_features, range) in split.iter().zip(chunk_indices.iter()) {
                let segment_size = range.end - range.start;
                let end = range.end;
                let time_offset = (end * m::HOP_LENGTH) as f64 / m::SAMPLE_RATE as f64;

                let segment_duration =
                    (segment_size * m::HOP_LENGTH) as f64 / m::SAMPLE_RATE as f64;
                let total_frames = (audio_frames as f64 / m::HOP_LENGTH as f64).round() as usize;
                let n_frames = segment_size.min(
                    total_frames
                        .checked_sub(seek)
                        .or_else(|| {
                            seek.checked_sub(m::N_FRAMES)
                                .and_then(|seek| total_frames.checked_sub(seek))
                        })
                        .unwrap_or_default(),
                );
                let dr = self.decode_with_fallback(
                    audio_features,
                    task,
                    &tokens_in_sentence_fragment,
                    n_frames,
                )?;
                tokens_in_sentence_fragment.clear();
                if dr.no_speech_prob > m::NO_SPEECH_THRESHOLD
                    && dr.avg_logprob < m::LOGPROB_THRESHOLD
                {
                    tracing::trace!("no speech detected, skipping {end} {dr:?}");
                    continue;
                }

                // Grab any text that was in the previous sentence fragment
                if let Some(index) = dr.text.char_indices().rev().find_map(|(idx, c)| {
                    if c == '.' || c == '?' || c == '!' {
                        Some(idx)
                    } else {
                        None
                    }
                }) {
                    let text_after_last_sentence = &dr.text[index + 1..];
                    let tokens = self
                        .tokenizer
                        .encode(text_after_last_sentence, false)
                        .map_err(WhisperError::Tokenizer)?;
                    tokens_in_sentence_fragment.extend(tokens.get_ids());
                };

                let elapsed = start_time.elapsed();
                let remaining = Duration::from_millis(
                    ((elapsed.as_millis() as usize / seek) * (content_frames - seek)) as u64,
                );
                let progress = end as f32 / content_frames as f32;
                let segment = Segment {
                    sample_range: (range.start * m::HOP_LENGTH)
                        ..audio_frames.min(range.end * m::HOP_LENGTH),
                    start: time_offset,
                    duration: segment_duration,
                    remaining_time: remaining,
                    elapsed_time: elapsed,
                    progress,
                    result: dr,
                };

                if let Err(err) = result.start_send(segment) {
                    tracing::error!("Error sending segment: {err}");
                    break;
                }
            }
        }

        Ok(())
    }
}

pub fn token_id(tokenizer: &Tokenizer, token: &str) -> candle_core::Result<u32> {
    match tokenizer.token_to_id(token) {
        None => candle_core::bail!("no token-id for {token}"),
        Some(id) => Ok(id),
    }
}

impl Decoder {
    fn apply_timestamp_rules(
        &self,
        logits: Tensor,
        tokens: &[u32],
        no_timestamps: bool,
    ) -> candle_core::Result<Vec<f32>> {
        let mut logits = logits.to_vec1()?;

        logits[self.no_timestamps_token as usize] = 0.;
        logits[self.sot_token as usize] = 0.;
        logits[self.transcribe_token as usize] = 0.;
        logits[self.translate_token as usize] = 0.;

        if no_timestamps {
            for i in self.timestamp_token_range.clone() {
                logits[i as usize] = 0.;
            }
            return Ok(logits);
        }

        let mut iter_rev = tokens.iter().rev();
        let last_was_timestamp = iter_rev
            .next()
            .map(|t| self.is_special(*t))
            .unwrap_or(false);
        let penultimate_was_timestamp =
            iter_rev.next().map(|t| self.is_special(*t)).unwrap_or(true);

        match (penultimate_was_timestamp, last_was_timestamp) {
            // If the last two tokens were timestamps, then the new token cannot be a timestamp
            (true, true) => {
                for i in self.special_tokens() {
                    logits[i as usize] = 0.;
                }
            }
            // If the last token was a timestamp and the penultimate token was not, then the new token must be a timestamp
            (false, true) => {
                for (i, logit) in logits.iter_mut().enumerate() {
                    if !self.is_timestamp_or_eot(i as u32) {
                        *logit = 0.;
                    }
                }
            }
            _ => {}
        }

        // Make sure timestamps don't decrease
        let timestamp_last = tokens
            .iter()
            .rev()
            .find(|t| self.timestamp_token_range.contains(t))
            .copied()
            .unwrap_or(0);
        let timestamp_last = if last_was_timestamp && !penultimate_was_timestamp {
            timestamp_last
        } else {
            timestamp_last + 1
        };

        for (i, logit) in logits.iter_mut().enumerate() {
            if self.timestamp_token_range.contains(&(i as u32)) && i < timestamp_last as usize {
                *logit = 0.;
            }
        }

        // If the sum of the probability over timestamps is more than any other individual token, sample a timestamp
        let mut timestamp_sum_prob = 0.;
        let mut max_text_token_prob = 0.;
        for (i, logit) in logits.iter().enumerate() {
            if self.is_timestamp_or_eot(i as u32) {
                timestamp_sum_prob += logit;
            } else if *logit > max_text_token_prob {
                max_text_token_prob = *logit;
            }
        }

        if timestamp_sum_prob > max_text_token_prob {
            for (i, logit) in logits.iter_mut().enumerate() {
                if !self.is_timestamp_or_eot(i as u32) {
                    *logit = 0.;
                }
            }
        }

        Ok(logits)
    }
}
