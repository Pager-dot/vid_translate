use ct2rs::{Config, Translator};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};

type Ct2Translator = Translator<ct2rs::tokenizers::auto::Tokenizer>;

/// Caches loaded local translation models across Start/Stop toggles within one running
/// app instance, so weights are only loaded from disk once per language per session.
#[derive(Default)]
pub struct MarianState {
    ja: Mutex<Option<Ct2Translator>>,
    es: Mutex<Option<Ct2Translator>>,
}

/// Where the offline-converted CTranslate2 model directory lives for a given language.
///
/// Unlike the Vosk models, these can't be downloaded and converted at runtime: producing
/// them requires Python + `transformers` + `ctranslate2` and the `ct2-transformers-converter`
/// CLI, which isn't something we can ship inside (or invoke from) the Tauri app. Each
/// directory is produced once, offline, via:
///
///   ct2-transformers-converter --model staka/fugumt-ja-en \
///       --output_dir ct2-model-ja --quantization int8 --copy_files source.spm target.spm
///
/// (fugumt-ja-en beat Helsinki-NLP/opus-mt-ja-en head-to-head on real fragments — clearly
/// better on some, e.g. greetings, worse on a few, net improvement. For Spanish, swap `ja`
/// for `es` / the model for `Helsinki-NLP/opus-mt-es-en`), then copied into place here.
fn model_path(source_lang: &str) -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("vid_translate")
        .join(format!("ct2-model-{source_lang}"))
}

fn build_translator(source_lang: &str) -> Result<Ct2Translator, String> {
    let path = model_path(source_lang);
    if !path.exists() {
        return Err(format!(
            "no local model found at {} — see marian.rs for the one-time offline conversion command",
            path.display()
        ));
    }
    eprintln!("[marian] loading local model for '{source_lang}' from {}...", path.display());
    let start = std::time::Instant::now();
    let result = Translator::new(&path, &Config::default()).map_err(|e| format!("{e}"));
    let elapsed = start.elapsed();
    match &result {
        Ok(_) => eprintln!("[marian] model for '{source_lang}' loaded in {elapsed:.1?}"),
        Err(e) => eprintln!("[marian] model load FAILED for '{source_lang}' after {elapsed:.1?}: {e}"),
    }
    result
}

/// ES only: source text is split into chunks of at most this many words before translating,
/// so one long recognized sentence can't turn into one long, slow `translate_batch` call.
/// Each chunk is translated separately and the results are joined, which also restores a
/// real incremental "streaming" feel (`on_update` fires after every chunk) and gives natural
/// points to check `stop_flag` mid-sentence. Tradeoff: translating in isolated chunks can
/// occasionally read less fluently than translating the whole sentence at once, since each
/// chunk loses full-sentence context — acceptable given the latency this fixes.
///
/// Japanese deliberately skips this: Vosk's JA "words" are morphemes, not words, so an
/// 8-token chunk is a tiny context-free fragment the model can only mistranslate. JA only
/// translates on Vosk `Final` (whole stable sentences), so there's no latency problem to
/// chunk away in the first place.
const MAX_CHUNK_WORDS: usize = 8;

/// Translates one sentence through a local, offline, int8-quantized CTranslate2 model
/// (dedicated staka/fugumt-ja-en / Helsinki-NLP opus-mt-es-en models — small and fast,
/// unlike the earlier rust-bert/M2M100 attempt).
pub fn translate_local_blocking(
    source_lang: &str,
    text: &str,
    stop_flag: &AtomicBool,
    marian_state: &MarianState,
    mut on_update: impl FnMut(&str),
) -> String {
    if stop_flag.load(Ordering::Relaxed) {
        return String::new();
    }

    let slot = match source_lang {
        "ja" => &marian_state.ja,
        "es" => &marian_state.es,
        other => return format!("[translation error: unsupported local language {other}]"),
    };

    let mut guard = slot.lock().unwrap();
    if guard.is_none() {
        match build_translator(source_lang) {
            Ok(t) => *guard = Some(t),
            Err(e) => return format!("[translation error: {e}]"),
        }
    }
    let translator = guard.as_ref().unwrap();

    if source_lang == "ja" {
        // Whole sentence, one call — and with Vosk's artificial inter-morpheme spaces
        // stripped: the model was trained on natural unspaced Japanese, and spaced input
        // tokenizes out-of-distribution ("▁を ▁食べ" instead of "を 食べ"), degrading
        // quality even on otherwise correct transcriptions.
        let despaced: String = text.split_whitespace().collect();
        eprintln!("[marian] translating 'ja' sentence: {despaced:?}");
        let start = std::time::Instant::now();
        return match translator.translate_batch(&[despaced], &Default::default(), None) {
            Ok(mut out) => {
                let (translated, _score) = out.pop().unwrap_or_default();
                eprintln!("[marian] sentence translated in {:.1?}: {translated:?}", start.elapsed());
                if !translated.is_empty() {
                    on_update(&translated);
                }
                translated
            }
            Err(e) => {
                eprintln!("[marian] translate() error after {:.1?}: {e}", start.elapsed());
                format!("[translation error: {e}]")
            }
        };
    }

    let words: Vec<&str> = text.split_whitespace().collect();
    let mut accumulated = String::new();

    for chunk_words in words.chunks(MAX_CHUNK_WORDS) {
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }
        let chunk = chunk_words.join(" ");
        eprintln!("[marian] translating '{source_lang}' chunk ({} words): {chunk:?}", chunk_words.len());
        let start = std::time::Instant::now();
        let translated = translator.translate_batch(&[chunk], &Default::default(), None);
        let elapsed = start.elapsed();

        match translated {
            Ok(mut out) => {
                let (piece, _score) = out.pop().unwrap_or_default();
                eprintln!("[marian] chunk translated in {elapsed:.1?}: {piece:?}");
                if !piece.is_empty() {
                    // Pass just this chunk's translation, not the running total — the
                    // frontend displays each chunk on its own for a fixed minimum duration
                    // (a readable, paced reveal) rather than an ever-growing concatenation.
                    on_update(&piece);
                    if !accumulated.is_empty() {
                        accumulated.push(' ');
                    }
                    accumulated.push_str(&piece);
                }
            }
            Err(e) => {
                eprintln!("[marian] translate() error after {elapsed:.1?}: {e}");
                return format!("[translation error: {e}]");
            }
        }
    }

    accumulated
}

#[cfg(test)]
mod smoke_test {
    use super::*;

    #[test]
    #[ignore] // requires the offline-converted model to already be in place; run explicitly
    fn translates_spanish() {
        let state = MarianState::default();
        let stop_flag = AtomicBool::new(false);
        let mut updates = Vec::new();
        let result = translate_local_blocking(
            "es",
            "Hola, ¿cómo estás hoy?",
            &stop_flag,
            &state,
            |partial| updates.push(partial.to_string()),
        );
        println!("RESULT: {result}");
        println!("UPDATES: {updates:?}");
        assert!(!result.is_empty());
        assert!(!result.starts_with("[translation error"));
    }

    #[test]
    #[ignore] // requires the offline-converted model to already be in place; run explicitly
    fn translates_japanese() {
        let state = MarianState::default();
        let stop_flag = AtomicBool::new(false);
        // Space-separated morphemes, exactly the shape Vosk's JA recognizer emits — the
        // de-space + whole-sentence path must handle these, not just natural written JA.
        let vosk_style_inputs = [
            "こんにちは 、 元気 です か ？",
            "小 学校 です",
            "そんな 残念 だ なぁ と",
            "健康 で 元気 に 仲良く 過ごし ましょう",
            "今年 の 抱負 は 何 です か",
        ];
        for input in vosk_style_inputs {
            let mut updates = Vec::new();
            let result = translate_local_blocking(
                "ja",
                input,
                &stop_flag,
                &state,
                |partial| updates.push(partial.to_string()),
            );
            println!("INPUT:  {input}");
            println!("RESULT: {result}");
            assert!(!result.is_empty());
            assert!(!result.starts_with("[translation error"));
            assert_eq!(updates.len(), 1, "JA should translate as one whole sentence");
        }
    }
}

#[cfg(test)]
mod long_sentence_test {
    use super::*;

    #[test]
    #[ignore] // requires the offline-converted model to already be in place; run explicitly
    fn translates_long_spanish_in_chunks() {
        let state = MarianState::default();
        let stop_flag = AtomicBool::new(false);
        let mut updates = Vec::new();
        // 23 words — should split into 3 chunks of MAX_CHUNK_WORDS (8) plus a remainder.
        let text = "también aleros de streamer de esa conversación aunque eso es tango which will it son sentences will meet perfect sense bath veces al río frases de este río";
        let result = translate_local_blocking(
            "es",
            text,
            &stop_flag,
            &state,
            |partial| updates.push(partial.to_string()),
        );
        assert!(!result.is_empty());
        assert!(!result.starts_with("[translation error"));
        assert_eq!(updates.len(), 4, "expected one on_update call per chunk");
    }
}
