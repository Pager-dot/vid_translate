import sys
import os
import json
import urllib.request

SOURCE_LANG = os.environ.get("SOURCE_LANG", "es").strip() or "es"
OLLAMA_KEY = os.environ.get("OLLAMA_KEY", "").strip()
OLLAMA_MODEL = os.environ.get("OLLAMA_MODEL", "").strip() or "gemma3:27b"

BASE_URL = "https://ollama.com" if OLLAMA_KEY else "http://localhost:11434"

LANG_NAMES = {"ja": "Japanese", "es": "Spanish"}
LANG_NAME = LANG_NAMES.get(SOURCE_LANG, SOURCE_LANG)

SYSTEM_PROMPT = (
    f"You are a real-time speech translator. You receive short, possibly "
    f"imperfect {LANG_NAME} speech-to-text fragments and must translate them "
    f"into natural, fluent English. Output ONLY the English translation with "
    f"no notes, quotes, or explanations. If the fragment is just filler noise "
    f"or has nothing translatable, output nothing."
)


def translate(text):
    url = f"{BASE_URL}/api/generate"
    payload = {
        "model": OLLAMA_MODEL,
        "system": SYSTEM_PROMPT,
        "prompt": text,
        "stream": True,
    }
    headers = {"Content-Type": "application/json"}
    if OLLAMA_KEY:
        headers["Authorization"] = f"Bearer {OLLAMA_KEY}"

    req = urllib.request.Request(
        url, data=json.dumps(payload).encode("utf-8"), headers=headers
    )
    accumulated = ""
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            for raw_line in r:
                raw_line = raw_line.strip()
                if not raw_line:
                    continue
                try:
                    chunk = json.loads(raw_line.decode("utf-8"))
                except json.JSONDecodeError:
                    continue
                piece = chunk.get("response", "")
                if piece:
                    accumulated += piece
                    print(accumulated, flush=True)
                if chunk.get("done"):
                    break
    except Exception as e:
        print(f"[translation error: {e}]", flush=True)
    print("---", flush=True)


def main():
    print("READY", flush=True)
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            data = json.loads(line)
        except json.JSONDecodeError:
            continue
        text = data.get("text", "").strip()
        if not text:
            print("---", flush=True)
            continue
        translate(text)


if __name__ == "__main__":
    main()
