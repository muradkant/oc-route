use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

pub const DEFAULT_ROUTER_MODEL: &str = "opencode/mimo-v2.5-free";
pub const DEFAULT_SLIDING_WINDOW: usize = 10;
pub const DEFAULT_ROUTER_TIMEOUT_SECS: u64 = 90;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    #[serde(default = "default_router_model")]
    pub router_model: String,
    #[serde(default = "default_sliding_window")]
    pub sliding_window: usize,
    #[serde(default = "default_router_timeout_secs")]
    pub router_timeout_secs: u64,
    pub model_pool: Vec<String>,
    pub routing_prompt: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Profiles {
    #[serde(rename = "profile")]
    pub profiles: Vec<Profile>,
}

fn default_router_model() -> String {
    DEFAULT_ROUTER_MODEL.to_string()
}
fn default_sliding_window() -> usize {
    DEFAULT_SLIDING_WINDOW
}
fn default_router_timeout_secs() -> u64 {
    DEFAULT_ROUTER_TIMEOUT_SECS
}

pub fn config_dir() -> Result<PathBuf> {
    let base = dirs::config_dir().context("could not determine user config directory")?;
    Ok(base.join("oc-route"))
}

pub fn profiles_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("profiles.toml"))
}

pub fn load() -> Result<Profiles> {
    let path = profiles_path()?;
    if !path.exists() {
        return Ok(Profiles::default());
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let profiles: Profiles = toml::from_str(&raw)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(profiles)
}

pub fn save(profiles: &Profiles) -> Result<()> {
    let dir = config_dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let path = profiles_path()?;
    let raw = toml::to_string_pretty(profiles).context("failed to serialize profiles")?;
    let with_header = format!("# oc-route profiles\n# Edit ~/.config/oc-route/profiles.toml\n\n{}", raw);
    fs::write(&path, with_header)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

pub fn upsert(profiles: &mut Profiles, profile: Profile) {
    if let Some(existing) = profiles.profiles.iter_mut().find(|p| p.name == profile.name) {
        *existing = profile;
    } else {
        profiles.profiles.push(profile);
    }
}

pub fn validate(profile: &Profile) -> Result<()> {
    if profile.name.trim().is_empty() {
        anyhow::bail!("profile name cannot be empty");
    }
    if profile.model_pool.is_empty() {
        anyhow::bail!("profile '{}' has an empty model pool", profile.name);
    }
    if !profile.model_pool.iter().any(|m| m == &profile.router_model) {
        anyhow::bail!(
            "profile '{}': router_model '{}' must be present in model_pool",
            profile.name,
            profile.router_model
        );
    }
    if profile.sliding_window == 0 {
        anyhow::bail!("profile '{}': sliding_window must be at least 1", profile.name);
    }
    Ok(())
}

pub fn split_model_id(model_id: &str) -> Option<(String, String)> {
    let mut iter = model_id.splitn(2, '/');
    let provider = iter.next()?.trim();
    let model = iter.next()?.trim();
    if provider.is_empty() || model.is_empty() {
        return None;
    }
    Some((provider.to_string(), model.to_string()))
}

pub fn ensure_config_dir_exists() -> Result<PathBuf> {
    let dir = config_dir()?;
    fs::create_dir_all(&dir).ok();
    Ok(dir)
}

pub fn profile_dir() -> Result<PathBuf> {
    ensure_config_dir_exists()
}
