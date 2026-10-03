//! The natively-streaming backend. Still the recognizer for Spanish and English.

use vosk::{DecodingState, Model, Recognizer};

use super::RecognitionResult;

/// Run the Vosk streaming recognizer on audio chunks from `rx`.
/// Calls `on_result` synchronously for every chunk — Vosk is fast enough
/// that this runs in near-real-time (<50ms per 250ms chunk).
pub fn run<F>(
    model_path: &str,
    rx: std::sync::mpsc::Receiver<Vec<i16>>,
    on_ready: impl FnOnce(),
    mut on_result: F,
) -> Result<(), String>
where
    F: FnMut(RecognitionResult),
{
    let model = Model::new(model_path)
        .ok_or_else(|| "Failed to load Vosk model — check the model path".to_string())?;

    let mut rec = Recognizer::new(&model, 16000.0)
        .ok_or_else(|| "Failed to create Vosk recognizer".to_string())?;

    rec.set_max_alternatives(0);
    rec.set_words(false);
    rec.set_partial_words(false);

    on_ready();

    for chunk in rx {
        let result = match rec.accept_waveform(&chunk) {
            Ok(DecodingState::Running) => {
                let text = rec.partial_result().partial.trim().to_string();
                if text.is_empty() {
                    RecognitionResult::Silent
                } else {
                    RecognitionResult::Partial(text)
                }
            }
            Ok(DecodingState::Finalized) => {
                let text = rec
                    .final_result()
                    .single()
                    .map(|r| r.text.trim().to_string())
                    .unwrap_or_default();
                if text.is_empty() {
                    RecognitionResult::Silent
                } else {
                    RecognitionResult::Final(text)
                }
            }
            Ok(DecodingState::Failed) | Err(_) => RecognitionResult::Silent,
        };

        on_result(result);
    }

    Ok(())
}
