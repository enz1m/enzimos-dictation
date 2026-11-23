//! Where the daemon listens and which speech service it uses.

use std::path::{Path, PathBuf};

/// Dictation settings: the runtime paths, and the optional
/// `$XDG_CONFIG_HOME/enzimos/dictation.json` (`model`, `gateway_socket`,
/// `inference_socket`, `max_seconds`).
#[derive(Clone, Debug)]
pub(crate) struct Config {
    /// The daemon's private directory (control socket, recordings).
    pub(crate) runtime: PathBuf,
    pub(crate) control: PathBuf,
    /// The AI gateway, which routes the transcription to its provider.
    pub(crate) gateway: PathBuf,
    /// The local inference service, asked which speech model is installed.
    pub(crate) inference: PathBuf,
    /// A speech model chosen by the user; otherwise the installed one.
    pub(crate) model: Option<String>,
    /// The longest recording; the speech service takes at most 60 seconds.
    pub(crate) max_seconds: u32,
    /// Present once the user allowed dictation to use the microphone.
    pub(crate) consent: Option<PathBuf>,
    pub(crate) disabled: Option<PathBuf>,
}

pub(crate) const SERVICE_LIMIT_SECONDS: u32 = 60;

impl Config {
    pub(crate) fn load() -> Result<Self, String> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or("XDG_RUNTIME_DIR is not set")?;
        let config_home = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
        let file = config_home
            .as_ref()
            .map(|home| home.join("enzimos/dictation.json"));
        let mut config = Self::from_parts(&runtime, file.as_deref())?;
        if let Some(home) = config_home {
            config.disabled = Some(home.join("enzimos/local-ai-disabled"));
            config.consent = Some(home.join("enzimos/dictation-consent"));
        }
        Ok(config)
    }

    fn from_parts(runtime: &Path, file: Option<&Path>) -> Result<Self, String> {
        let own = runtime.join("enzimos-dictation");
        let mut config = Self {
            control: own.join("control.sock"),
            runtime: own,
            gateway: runtime.join("enzimos-ai-gateway/service.sock"),
            inference: runtime.join("enzimos-inference/service.sock"),
            model: None,
            max_seconds: SERVICE_LIMIT_SECONDS,
            consent: None,
            disabled: None,
        };
        let Some(text) = file.and_then(|path| std::fs::read_to_string(path).ok()) else {
            return Ok(config);
        };
        let value: serde_json::Value =
            serde_json::from_str(&text).map_err(|error| format!("dictation.json: {error}"))?;
        let path = |key: &str| {
            value[key]
                .as_str()
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
        };
        if let Some(gateway) = path("gateway_socket") {
            config.gateway = gateway;
        }
        if let Some(inference) = path("inference_socket") {
            config.inference = inference;
        }
        config.model = value["model"]
            .as_str()
            .filter(|model| !model.is_empty())
            .map(str::to_owned);
        if let Some(seconds) = value["max_seconds"].as_u64() {
            config.max_seconds = u32::try_from(seconds)
                .unwrap_or(SERVICE_LIMIT_SECONDS)
                .clamp(1, SERVICE_LIMIT_SECONDS);
        }
        Ok(config)
    }
}
