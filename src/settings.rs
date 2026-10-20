//! Which translation engine to use, saved in %APPDATA%\xiaobaiyun\settings.json.
//! The API key is encrypted for the current Windows user (DPAPI), never stored
//! in plain text.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB};

pub const CLAUDE_BASE: &str = "https://api.anthropic.com";
pub const CLAUDE_MODEL: &str = "claude-opus-5-5";
pub const OPENAI_BASE: &str = "https://api.openai.com/v1";
pub const LOCAL_BASE: &str = "http://localhost:11434/v1";

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    /// Free Google endpoint, raced with MyMemory.
    #[default]
    Google,
    /// An online LLM API.
    Api,
    /// A model served on this machine (Ollama, LM Studio, ...).
    Local,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, Debug)]
#[serde(rename_all = "lowercase")]
pub enum ApiKind {
    #[default]
    Claude,
    /// Any OpenAI-compatible chat completions API (OpenAI, DeepSeek, Qwen, ...).
    OpenAi,
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub engine: Engine,
    pub api_kind: ApiKind,
    pub api_base: String,
    pub api_key: String,
    pub api_model: String,
    pub local_base: String,
    pub local_model: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            engine: Engine::Google,
            api_kind: ApiKind::Claude,
            api_base: CLAUDE_BASE.into(),
            api_key: String::new(),
            api_model: CLAUDE_MODEL.into(),
            local_base: LOCAL_BASE.into(),
            local_model: String::new(),
        }
    }
}

/// On-disk form: the key is DPAPI-encrypted and hex-encoded.
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct Stored {
    engine: Engine,
    api_kind: ApiKind,
    api_base: String,
    api_key_protected: String,
    api_model: String,
    local_base: String,
    local_model: String,
}

/// A file in the app's data folder, %APPDATA%\xiaobaiyun (created if missing).
pub fn data_file(name: &str) -> Option<PathBuf> {
    let appdata = PathBuf::from(std::env::var_os("APPDATA")?);
    let dir = appdata.join("xiaobaiyun");
    // Versions before the rename kept their data in "desk-translator".
    let old = appdata.join("desk-translator");
    if !dir.exists() && old.exists() {
        let _ = std::fs::rename(&old, &dir);
    }
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join(name))
}

pub fn load() -> Settings {
    let defaults = Settings::default();
    let Some(stored) = data_file("settings.json")
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str::<Stored>(&t).ok())
    else {
        return defaults;
    };
    let or = |v: String, d: &str| if v.trim().is_empty() { d.to_string() } else { v };
    Settings {
        engine: stored.engine,
        api_kind: stored.api_kind,
        api_base: or(stored.api_base, &defaults.api_base),
        api_key: unprotect(&stored.api_key_protected).unwrap_or_default(),
        api_model: or(stored.api_model, &defaults.api_model),
        local_base: or(stored.local_base, &defaults.local_base),
        local_model: stored.local_model,
    }
}

pub fn save(s: &Settings) -> Result<(), String> {
    let stored = Stored {
        engine: s.engine,
        api_kind: s.api_kind,
        api_base: s.api_base.trim().to_string(),
        api_key_protected: if s.api_key.is_empty() { String::new() } else { protect(s.api_key.trim())? },
        api_model: s.api_model.trim().to_string(),
        local_base: s.local_base.trim().to_string(),
        local_model: s.local_model.trim().to_string(),
    };
    let path = data_file("settings.json").ok_or("no APPDATA folder")?;
    let json = serde_json::to_string_pretty(&stored).map_err(|e| e.to_string())?;
    std::fs::write(path, json).map_err(|e| e.to_string())
}

fn protect(secret: &str) -> Result<String, String> {
    let mut data = secret.as_bytes().to_vec();
    let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_mut_ptr() };
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptProtectData(&input, None, None, None, None, 0, &mut output).map_err(|e| e.to_string())?;
        let bytes = std::slice::from_raw_parts(output.pbData, output.cbData as usize);
        let hex = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let _ = LocalFree(Some(HLOCAL(output.pbData as *mut _)));
        Ok(hex)
    }
}

fn unprotect(hex: &str) -> Option<String> {
    if hex.is_empty() || !hex.len().is_multiple_of(2) {
        return None;
    }
    let mut data: Vec<u8> =
        (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16)).collect::<Result<_, _>>().ok()?;
    let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_mut_ptr() };
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptUnprotectData(&input, None, None, None, None, 0, &mut output).ok()?;
        let text =
            String::from_utf8_lossy(std::slice::from_raw_parts(output.pbData, output.cbData as usize)).into_owned();
        let _ = LocalFree(Some(HLOCAL(output.pbData as *mut _)));
        Some(text)
    }
}
