//! Speech, live-voice and local tiny-model settings (OMP's `Services` / `Tiny Model` rows); flattened into `Config`.
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct ServicesOpts {
    pub tts: TtsCfg,
    pub speech: SpeechCfg,
    pub live: LiveCfg,
    pub tiny_model: TinyModelCfg,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct TtsCfg {
    /// Voice for local synthesis: a Phonon id (`harper`, `hadley`, ...), mapped to a macOS voice.
    pub local_voice: String,
    /// Speaking rate; clamped to 0.8–1.3 where it is used.
    pub local_speed: f64,
}

impl Default for TtsCfg {
    fn default() -> Self {
        Self { local_voice: "harper".into(), local_speed: 1.0 }
    }
}

/// What the assistant says aloud.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SpeechMode {
    /// Messages plus thinking.
    All,
    /// Assistant messages only.
    #[default]
    Assistant,
    /// Only the final message, at turn end.
    Yield,
}

impl SpeechMode {
    pub fn as_str(self) -> &'static str {
        match self {
            SpeechMode::All => "all",
            SpeechMode::Assistant => "assistant",
            SpeechMode::Yield => "yield",
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct SpeechCfg {
    /// Speak the assistant's output aloud through the speakers as it streams.
    pub enabled: bool,
    pub mode: SpeechMode,
    /// Rewrite output into spoken prose with the tiny/smol model before synthesis.
    pub enhanced: bool,
    pub voice: String,
    pub speed: f64,
}

impl Default for SpeechCfg {
    fn default() -> Self {
        Self { enabled: false, mode: SpeechMode::Assistant, enhanced: false, voice: "harper".into(), speed: 1.0 }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct LiveCfg {
    /// OpenAI Realtime-compatible server base URL, e.g. `http://127.0.0.1:8080/v1`; empty uses the Codex endpoint.
    pub endpoint: String,
    pub voice: String,
    /// Bearer key for `endpoint` only: a literal, an environment variable name, or `!command`.
    pub api_key: String,
}

impl Default for LiveCfg {
    fn default() -> Self {
        Self { endpoint: String::new(), voice: "sol".into(), api_key: String::new() }
    }
}

/// Backend for local tiny models: an ONNX execution provider, or `mlx`/`metal` through mlx-lm.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct TinyModelCfg {
    /// Empty means CPU-only ONNX.
    pub device: String,
    /// ONNX precision/quantization; empty uses the model's shipped file.
    pub dtype: String,
}

impl Default for TinyModelCfg {
    fn default() -> Self {
        Self { device: String::new(), dtype: String::new() }
    }
}

impl ServicesOpts {
    /// Rejects values `config set` would otherwise store unreadably; the speech rates are clamped where used.
    pub fn validate(&self) -> Result<()> {
        for (key, speed) in [("tts.local_speed", self.tts.local_speed), ("speech.speed", self.speech.speed)] {
            if !speed.is_finite() {
                bail!("{key} must be a finite number");
            }
        }
        check_endpoint(&self.live.endpoint)?;
        if !crate::tiny::device_allowed(&self.tiny_model.device) {
            bail!("tiny_model.device must be one of {}, got `{}`", crate::tiny::DEVICES.join(", "), self.tiny_model.device);
        }
        if !crate::tiny::dtype_allowed(&self.tiny_model.dtype) {
            bail!("tiny_model.dtype must be one of {}, got `{}`", crate::tiny::DTYPES.join(", "), self.tiny_model.dtype);
        }
        Ok(())
    }
}

fn check_endpoint(endpoint: &str) -> Result<()> {
    let endpoint = endpoint.trim();
    if endpoint.is_empty() || endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        return Ok(());
    }
    bail!("live.endpoint must be an http(s) URL, got `{endpoint}`")
}

/// OMP key → native dotted key; `config import-omp` copies the value verbatim unless `convert` handles it.
pub(super) const OMP: &[(&str, &str)] = &[
    ("tts.localVoice", "tts.local_voice"),
    ("tts.localSpeed", "tts.local_speed"),
    ("speech.enabled", "speech.enabled"),
    ("speech.mode", "speech.mode"),
    ("speech.enhanced", "speech.enhanced"),
    ("speech.voice", "speech.voice"),
    ("speech.speed", "speech.speed"),
    ("live.endpoint", "live.endpoint"),
    ("live.voice", "live.voice"),
    ("live.apiKey", "live.api_key"),
    ("providers.tinyModelDevice", "tiny_model.device"),
    ("providers.tinyModelDtype", "tiny_model.dtype"),
];

/// Translates an OMP value whose shape or units differ from the native field; `None` copies it verbatim.
pub(super) fn convert(from: &str, value: &Value) -> Option<Result<toml::Value>> {
    match from {
        "tts.localSpeed" | "speech.speed" => Some(
            value
                .as_f64()
                .filter(|v| v.is_finite())
                .map(toml::Value::Float)
                .ok_or_else(|| anyhow!("{from} must be a number")),
        ),
        "live.endpoint" => Some(
            value
                .as_str()
                .ok_or_else(|| anyhow!("{from} must be a URL string"))
                .and_then(|endpoint| check_endpoint(endpoint).map(|()| toml::Value::String(endpoint.trim().into()))),
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn omp_rows_import_under_native_keys() {
        let yaml = "\
tts:
  localVoice: hadley
  localSpeed: 1.2
speech:
  enabled: true
  mode: yield
  enhanced: true
  voice: maeve
  speed: 0.9
live:
  endpoint: http://127.0.0.1:8080/v1
  voice: eve
  apiKey: XAI_KEY
providers:
  tinyModelDevice: mlx
  tinyModelDtype: q8
";
        let table = crate::config::settings::omp::translate(yaml).unwrap();
        let cfg = crate::config::settings::decode(toml::Value::Table(table)).unwrap();
        let o = &cfg.services_opts;
        assert_eq!((o.tts.local_voice.as_str(), o.tts.local_speed), ("hadley", 1.2));
        assert!(o.speech.enabled && o.speech.enhanced);
        assert_eq!(o.speech.mode, SpeechMode::Yield);
        assert_eq!((o.speech.voice.as_str(), o.speech.speed), ("maeve", 0.9));
        assert_eq!((o.live.endpoint.as_str(), o.live.voice.as_str(), o.live.api_key.as_str()), ("http://127.0.0.1:8080/v1", "eve", "XAI_KEY"));
        assert_eq!((o.tiny_model.device.as_str(), o.tiny_model.dtype.as_str()), ("mlx", "q8"));
    }

    #[test]
    fn defaults_and_validation_match_omp() {
        let d = Config::default().services_opts;
        assert_eq!((d.tts.local_voice.as_str(), d.tts.local_speed), ("harper", 1.0));
        assert!(!d.speech.enabled && !d.speech.enhanced && d.speech.mode == SpeechMode::Assistant);
        assert_eq!((d.speech.voice.as_str(), d.speech.speed), ("harper", 1.0));
        assert_eq!((d.live.voice.as_str(), d.live.endpoint.as_str()), ("sol", ""));
        let mut o = ServicesOpts::default();
        o.live.endpoint = "ws://127.0.0.1:1".into();
        assert!(o.validate().is_err());
        o.live.endpoint = "https://x.test/v1".into();
        o.tiny_model.device = "tpu".into();
        assert!(o.validate().is_err());
        o.tiny_model.device = "coreml".into();
        o.tiny_model.dtype = "fp16".into();
        o.tts.local_speed = f64::INFINITY;
        assert!(o.validate().is_err());
        o.tts.local_speed = 2.0;
        assert!(o.validate().is_ok(), "out-of-band rates are clamped where used, not rejected");
        // A bad value is refused at import (`translate`) or at decode; either way it never lands.
        let decode = |text: &str| crate::config::settings::omp::translate(text).and_then(|t| crate::config::settings::decode(toml::Value::Table(t)));
        assert!(decode("speech:\n  mode: sometimes\n").is_err());
        assert!(decode("live:\n  endpoint: ftp://host\n").is_err());
        assert!(decode("speech:\n  speed: fast\n").is_err());
        assert_eq!(decode("speech:\n  mode: all\n").unwrap().services_opts.speech.mode, SpeechMode::All);
    }
}
