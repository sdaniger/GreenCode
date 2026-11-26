#!/usr/bin/env python3
"""Local tiny-model runner: greedy ONNX generation for GreenCode titles and memory.

usage: tiny.py DEVICE DTYPE REPO MAX_TOKENS PROMPT
DEVICE is an ONNX execution provider alias (cpu, cuda, coreml, directml, ...);
DTYPE picks the ONNX precision file (fp16, q8, int8, ...); empty means the repo's shipped model.onnx.
"""
import os
import sys

try:
    import onnxruntime as ort
    from huggingface_hub import snapshot_download
    from transformers import AutoTokenizer
except ImportError as e:  # named instead of a traceback
    sys.stderr.write(
        "tiny models need onnxruntime, transformers and huggingface_hub: "
        "pip install onnxruntime transformers huggingface_hub (%s)\n" % e
    )
    raise SystemExit(2)

DEVICES = {
    "cpu": "CPUExecutionProvider",
    "cuda": "CUDAExecutionProvider",
    "gpu": "CUDAExecutionProvider",
    "webgpu": "WebGpuExecutionProvider",
    "coreml": "CoreMLExecutionProvider",
    "directml": "DmlExecutionProvider",
    "openvino": "OpenVINOExecutionProvider",
    "rocm": "ROCMExecutionProvider",
    "webnn": "WebNNExecutionProvider",
    "qnn": "QNNExecutionProvider",
    "azure": "AzureExecutionProvider",
}
# Preferred file per precision; a repo that ships none of them falls back to model.onnx.
FILES = {
    "fp32": ["model.onnx", "model_fp32.onnx", "onnx/model.onnx"],
    "fp16": ["model_fp16.onnx", "onnx/model_fp16.onnx", "model.onnx"],
    "bf16": ["model_bf16.onnx", "onnx/model_bf16.onnx", "model.onnx"],
    "q8": ["model_quantized.onnx", "model_q8.onnx", "onnx/model_q8.onnx", "model.onnx"],
    "int8": ["model_int8.onnx", "onnx/model_int8.onnx", "model_quantized.onnx", "model.onnx"],
    "uint8": ["model_uint8.onnx", "onnx/model_uint8.onnx", "model_quantized.onnx", "model.onnx"],
    "q4": ["model_q4.onnx", "onnx/model_q4.onnx", "model_quantized.onnx", "model.onnx"],
    "auto": ["model.onnx", "onnx/model.onnx"],
}


def pick(paths, dtype):
    candidates = FILES.get(dtype or "auto", FILES["auto"])
    for name in candidates:
        for path in paths:
            if path.replace(os.sep, "/").endswith("/" + name):
                return path
    raise SystemExit(
        "no ONNX weights for dtype %r in this repo (looked for %s); clear tiny_model.dtype to use the shipped one"
        % (dtype, ", ".join(candidates))
    )


def main():
    if len(sys.argv) != 6:
        raise SystemExit(__doc__)
    device, dtype, repo, max_tokens, prompt = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4]), sys.argv[5]
    provider = DEVICES.get(device.lower(), "CPUExecutionProvider")
    root = snapshot_download(repo, allow_patterns=["*.onnx", "*.json", "*.txt", "*.model", "*.safetensors", "*.bin"])
    files = [os.path.join(dirpath, name) for dirpath, _, names in os.walk(root) for name in names]
    model = pick(files, dtype)
    tokenizer = AutoTokenizer.from_pretrained(root)
    options = ort.SessionOptions()
    session = ort.InferenceSession(model, options, providers=[provider, "CPUExecutionProvider"])
    if provider not in session.get_providers():
        sys.stderr.write("warning: %s is unavailable in this onnxruntime build; using CPU\n" % provider)
    eos = tokenizer.eos_token_id
    ids = tokenizer(prompt, return_tensors="np").input_ids
    names = {i.name for i in session.get_inputs()}
    past = {}
    out_tokens = []
    for _ in range(max_tokens):
        feed = {"input_ids": ids, "attention_mask": [[1] * ids.shape[1]]} if "attention_mask" in names else {"input_ids": ids}
        feed.update(past)
        outputs = session.run(None, feed)
        logits = outputs[0][0, -1]
        nxt = int(logits.argmax())
        if nxt == eos:
            break
        out_tokens.append(nxt)
        ids = [[nxt]]
        past = {}
        for value, meta in zip(outputs[1:], session.get_outputs()[1:]):
            past[meta.name] = value
    sys.stdout.write(tokenizer.decode(out_tokens, skip_special_tokens=True))


if __name__ == "__main__":
    main()
