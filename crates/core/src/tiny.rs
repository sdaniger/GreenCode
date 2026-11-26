//! Local tiny models for titles and memory: `tiny/<repo>` or `local/<repo>` model specs run through mlx-lm
//! (Apple silicon) or an embedded ONNX runner. Device and precision come from `tiny_model.*`, overridden by
//! `GREENCODE_TINY_DEVICE` / `GREENCODE_TINY_DTYPE`.
use crate::config::Config;
use anyhow::{bail, Context, Result};
use std::{path::PathBuf, process::Stdio};
use tokio::process::Command;

/// ONNX execution providers, plus the MLX backend.
pub const DEVICES: &[&str] = &[
    "cpu", "cuda", "gpu", "webgpu", "coreml", "directml", "openvino", "rocm", "webnn", "qnn", "azure", "mlx", "metal",
];
/// ONNX precision/quantization; empty uses the model's shipped weights.
pub const DTYPES: &[&str] = &["auto", "fp32", "fp16", "bf16", "q8", "q4", "int8", "uint8"];

const SOURCE: &str = include_str!("tiny.py");

/// An empty value is the default (CPU-only ONNX, shipped dtype).
pub fn device_allowed(device: &str) -> bool {
    device.trim().is_empty() || DEVICES.contains(&device.trim().to_ascii_lowercase().as_str())
}

pub fn dtype_allowed(dtype: &str) -> bool {
    dtype.trim().is_empty() || DTYPES.contains(&dtype.trim().to_ascii_lowercase().as_str())
}

/// Values in effect: `GREENCODE_TINY_DEVICE`/`GREENCODE_TINY_DTYPE` win over config.toml.
pub fn resolve(cfg: &Config) -> Result<(String, String)> {
    let env = |name: &str| std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let device = env("GREENCODE_TINY_DEVICE").unwrap_or_else(|| cfg.services_opts.tiny_model.device.trim().to_string());
    let dtype = env("GREENCODE_TINY_DTYPE").unwrap_or_else(|| cfg.services_opts.tiny_model.dtype.trim().to_string());
    let device = device.to_ascii_lowercase();
    let dtype = dtype.to_ascii_lowercase();
    if !device_allowed(&device) {
        bail!("tiny_model.device must be one of {} or empty, got `{device}`", DEVICES.join(", "));
    }
    if !dtype_allowed(&dtype) {
        bail!("tiny_model.dtype must be one of {} or empty, got `{dtype}`", DTYPES.join(", "));
    }
    Ok((device, dtype))
}

/// Whether `spec` names a model this module runs itself.
pub fn local_spec(spec: &str) -> Option<&str> {
    spec.strip_prefix("tiny/").or_else(|| spec.strip_prefix("local/")).map(str::trim).filter(|m| !m.is_empty())
}

fn mlx(device: &str) -> bool {
    matches!(device, "mlx" | "metal")
}

/// `(program, args)` for one completion. Kept separate from running so the flags are testable.
pub fn command(device: &str, dtype: &str, model: &str, prompt: &str, max_tokens: u32) -> (String, Vec<String>) {
    if mlx(device) {
        return (
            "mlx_lm.generate".into(),
            vec![
                "--model".into(), model.into(),
                "--prompt".into(), prompt.into(),
                "--max-tokens".into(), max_tokens.to_string(),
                "--temp".into(), "0".into(),
            ],
        );
    }
    ("python3".into(), vec![helper_path().display().to_string(), device.into(), dtype.into(), model.into(), max_tokens.to_string(), prompt.into()])
}

fn helper_path() -> PathBuf {
    crate::home().join("bin").join("tiny.py")
}

/// Writes the ONNX runner next to the other helpers; returns its path.
pub fn install_helper() -> Result<PathBuf> {
    let path = helper_path();
    if std::fs::read_to_string(&path).ok().as_deref() != Some(SOURCE) {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, SOURCE).with_context(|| format!("cannot write {}", path.display()))?;
    }
    Ok(path)
}

/// One greedy completion from a local model. Errors name the missing runtime instead of returning text.
pub async fn generate(cfg: &Config, spec: &str, system: &str, prompt: &str, max_tokens: u32) -> Result<String> {
    let model = local_spec(spec).ok_or_else(|| anyhow::anyhow!("`{spec}` is not a tiny/ or local/ model"))?;
    let (device, dtype) = resolve(cfg)?;
    let full = if system.trim().is_empty() { prompt.to_string() } else { format!("{system}\n\n{prompt}") };
    let (program, args) = if mlx(&device) {
        let (program, args) = command(&device, &dtype, model, &full, max_tokens);
        (program, args)
    } else {
        install_helper()?;
        command(&device, &dtype, model, &full, max_tokens)
    };
    let output = Command::new(&program)
        .args(&args)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| start_error(&program, &device, e))?;
    if !output.status.success() {
        bail!("{program} failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() { bail!("{program} returned no text"); }
    Ok(text)
}

/// Names the missing runtime when the runner cannot start.
fn start_error(program: &str, device: &str, e: std::io::Error) -> anyhow::Error {
    match (program, e.kind()) {
        ("mlx_lm.generate", std::io::ErrorKind::NotFound) => anyhow::anyhow!(
            "tiny_model.device = `{device}` runs mlx-lm; install it with `pip install mlx-lm` (Apple silicon), or clear tiny_model.device"
        ),
        (_, std::io::ErrorKind::NotFound) => anyhow::anyhow!("tiny models need `python3` and onnxruntime: pip install onnxruntime transformers huggingface_hub"),
        _ => anyhow::anyhow!("cannot start {program}: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_and_dtype_validation_accepts_the_documented_set() {
        for d in DEVICES { assert!(device_allowed(d)); }
        for d in DTYPES { assert!(dtype_allowed(d)); }
        assert!(device_allowed("") && dtype_allowed(""));
        assert!(device_allowed("CUDA"));
        assert!(!device_allowed("tpu") && !dtype_allowed("q2"));
    }

    #[test]
    fn env_overrides_the_configured_backend() {
        let mut cfg = Config::default();
        cfg.services_opts.tiny_model = crate::config::services::TinyModelCfg { device: "cpu".into(), dtype: "q4".into() };
        assert_eq!(resolve(&cfg).unwrap(), ("cpu".into(), "q4".into()));
        // Set and read through the process environment of this test only.
        std::env::set_var("GREENCODE_TINY_DEVICE", "mlx");
        std::env::set_var("GREENCODE_TINY_DTYPE", "fp16");
        assert_eq!(resolve(&cfg).unwrap(), ("mlx".into(), "fp16".into()));
        std::env::remove_var("GREENCODE_TINY_DEVICE");
        std::env::remove_var("GREENCODE_TINY_DTYPE");
        cfg.services_opts.tiny_model.device = "coreml".into();
        cfg.services_opts.tiny_model.dtype.clear();
        assert_eq!(resolve(&cfg).unwrap(), ("coreml".into(), "".into()));
    }

    #[test]
    fn specs_and_commands() {
        assert_eq!(local_spec("tiny/HuggingFaceTB/SmolLM2-135M-Instruct"), Some("HuggingFaceTB/SmolLM2-135M-Instruct"));
        assert_eq!(local_spec("local/org/model"), Some("org/model"));
        assert_eq!(local_spec("tiny/"), None);
        assert_eq!(local_spec("ollama/qwen"), None);
        let (program, args) = command("cpu", "q4", "org/m", "hello", 40);
        assert_eq!(program, "python3");
        assert_eq!(&args[1..4], ["cpu", "q4", "org/m"]);
        assert_eq!((args[4].as_str(), args[5].as_str()), ("40", "hello"));
        assert!(args[0].ends_with("tiny.py"));
        let (program, args) = command("mlx", "q4", "org/m", "hello", 40);
        assert_eq!(program, "mlx_lm.generate");
        assert_eq!(args, ["--model", "org/m", "--prompt", "hello", "--max-tokens", "40", "--temp", "0"]);
    }

    #[test]
    fn a_missing_runtime_is_a_clear_error() {
        // Mapped directly: clearing PATH here would break every test spawning a process in parallel.
        let missing = || std::io::Error::from(std::io::ErrorKind::NotFound);
        assert!(start_error("mlx_lm.generate", "mlx", missing()).to_string().contains("pip install mlx-lm"));
        assert!(start_error("python3", "cpu", missing()).to_string().contains("pip install onnxruntime"));
    }
}
