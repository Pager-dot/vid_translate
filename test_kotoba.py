"""
Quick test of kotoba-whisper-bilingual-v1.0 on system audio.
Records 10 seconds from the default monitor source (whatever is playing),
then transcribes and attempts English translation.

Run: python3 test_kotoba.py
"""

import subprocess
import sys
import numpy as np

def record_loopback(seconds=10, rate=16000):
    """Capture `seconds` of system audio via parec (PipeWire/PulseAudio)."""
    sink = subprocess.check_output(["pactl", "get-default-sink"]).decode().strip()
    device = f"{sink}.monitor"
    print(f"Recording {seconds}s from: {device}")

    proc = subprocess.run(
        ["parec", f"--device={device}", "--format=s16le",
         f"--rate={rate}", "--channels=1"],
        capture_output=True,
        timeout=seconds + 2,
    )
    raw = proc.stdout[: seconds * rate * 2]  # trim to exact length
    samples = np.frombuffer(raw, dtype=np.int16).astype(np.float32) / 32768.0
    return samples, rate


def main():
    try:
        from transformers import pipeline as hf_pipeline
        import torch
    except ImportError:
        print("Install deps first:  pip install transformers torch accelerate")
        sys.exit(1)

    device = "cuda" if torch.cuda.is_available() else "cpu"
    print(f"Using device: {device}")
    print("Loading kotoba-whisper-bilingual-v1.0 (first run downloads ~3 GB)…")

    pipe = hf_pipeline(
        "automatic-speech-recognition",
        model="kotoba-tech/kotoba-whisper-bilingual-v1.0",
        torch_dtype=torch.float16 if device == "cuda" else torch.float32,
        device=device,
    )

    audio, rate = record_loopback(seconds=10)

    print("\n--- Transcription (no translate) ---")
    result = pipe({"array": audio, "sampling_rate": rate})
    print(result["text"])

    print("\n--- Translation to English ---")
    result_en = pipe(
        {"array": audio, "sampling_rate": rate},
        generate_kwargs={"task": "translate", "language": "japanese"},
    )
    print(result_en["text"])


if __name__ == "__main__":
    main()
