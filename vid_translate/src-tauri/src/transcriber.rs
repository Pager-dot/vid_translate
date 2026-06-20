use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

pub struct Transcriber {
    ctx: WhisperContext,
}

impl Transcriber {
    pub fn new(model_path: &str) -> Result<Self, String> {
        let ctx = WhisperContext::new_with_params(
            model_path,
            WhisperContextParameters::default(),
        )
        .map_err(|e| format!("Failed to load Whisper model: {:?}", e))?;
        Ok(Self { ctx })
    }

    /// Transcribe 16kHz mono f32 audio and translate it to English.
    /// `language` should be an ISO-639-1 code (e.g. "ja") or "auto".
    /// Returns transcribed English text, empty string if nothing detected.
    pub fn transcribe(&self, audio: &[f32], language: &str) -> Result<String, String> {
        let mut state = self
            .ctx
            .create_state()
            .map_err(|e| format!("State error: {:?}", e))?;

        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_translate(true);
        params.set_language(Some(language));
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        params.set_suppress_blank(true);
        // Lower the no-speech threshold so vocals in music aren't silently skipped.
        // Default is 0.6; 0.3 keeps Whisper attentive to singing.
        params.set_no_speech_thold(0.3);
        // Hint to the model that the audio is likely song lyrics.
        params.set_initial_prompt("Song lyrics:");

        state
            .full(params, audio)
            .map_err(|e| format!("Inference error: {:?}", e))?;

        let n = state
            .full_n_segments()
            .map_err(|e| format!("Segment count error: {:?}", e))?;

        let mut text = String::new();
        for i in 0..n {
            if let Ok(seg) = state.full_get_segment_text(i) {
                let trimmed = seg.trim().to_string();
                if !trimmed.is_empty() {
                    if !text.is_empty() {
                        text.push(' ');
                    }
                    text.push_str(&trimmed);
                }
            }
        }

        Ok(text)
    }
}
