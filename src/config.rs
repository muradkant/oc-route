use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

pub const DEFAULT_ROUTER_MODEL: &str = "opencode/mimo-v2.5-free";
pub const DEFAULT_SLIDING_WINDOW: usize = 10;
pub const DEFAULT_ROUTER_TIMEOUT_SECS: u64 = 90;
pub const MAX_SLIDING_WINDOW: usize = 50;
pub const MAX_ROUTER_TIMEOUT_SECS: u64 = 600;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
    if let Some(path) = std::env::var_os("OC_ROUTE_CONFIG_DIR") {
        return Ok(PathBuf::from(path));
    }
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
    let raw =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let profiles: Profiles =
        toml::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(profiles)
}

pub fn save(profiles: &Profiles) -> Result<()> {
    let dir = config_dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let path = profiles_path()?;
    let raw = toml::to_string_pretty(profiles).context("failed to serialize profiles")?;
    let with_header = format!(
        "# oc-route profiles\n# Edit ~/.config/oc-route/profiles.toml\n\n{}",
        raw
    );
    let temporary = path.with_extension("toml.tmp");
    fs::write(&temporary, with_header)
        .with_context(|| format!("failed to write {}", temporary.display()))?;
    fs::rename(&temporary, &path)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

pub fn upsert(profiles: &mut Profiles, profile: Profile) {
    if let Some(existing) = profiles
        .profiles
        .iter_mut()
        .find(|p| p.name == profile.name)
    {
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
    if profile.routing_prompt.trim().is_empty() {
        anyhow::bail!("profile '{}' has an empty routing prompt", profile.name);
    }
    if split_model_id(&profile.router_model).is_none() {
        anyhow::bail!(
            "profile '{}': invalid router_model '{}'; expected providerID/modelID",
            profile.name,
            profile.router_model
        );
    }
    let mut unique = HashSet::new();
    for model in &profile.model_pool {
        if split_model_id(model).is_none() {
            anyhow::bail!(
                "profile '{}': invalid model '{}'; expected providerID/modelID",
                profile.name,
                model
            );
        }
        if !unique.insert(model.trim()) {
            anyhow::bail!("profile '{}': duplicate model '{}'", profile.name, model);
        }
    }
    if profile.sliding_window == 0 {
        anyhow::bail!(
            "profile '{}': sliding_window must be at least 1",
            profile.name
        );
    }
    if profile.sliding_window > MAX_SLIDING_WINDOW {
        anyhow::bail!(
            "profile '{}': sliding_window must not exceed {}",
            profile.name,
            MAX_SLIDING_WINDOW
        );
    }
    if profile.router_timeout_secs == 0 || profile.router_timeout_secs > MAX_ROUTER_TIMEOUT_SECS {
        anyhow::bail!(
            "profile '{}': router_timeout_secs must be between 1 and {}",
            profile.name,
            MAX_ROUTER_TIMEOUT_SECS
        );
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

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_profile() -> Profile {
        Profile {
            name: "default".into(),
            router_model: "router/small".into(),
            sliding_window: 10,
            router_timeout_secs: 90,
            model_pool: vec!["provider/fast".into(), "provider/deep".into()],
            routing_prompt: "Use deep for hard tasks.".into(),
        }
    }

    #[test]
    fn accepts_router_model_outside_destination_pool() {
        assert!(validate(&valid_profile()).is_ok());
    }

    #[test]
    fn rejects_ambiguous_or_unbounded_profiles() {
        let mut profile = valid_profile();
        profile.model_pool.push(" provider/fast ".into());
        assert!(validate(&profile)
            .unwrap_err()
            .to_string()
            .contains("duplicate"));

        let mut profile = valid_profile();
        profile.sliding_window = MAX_SLIDING_WINDOW + 1;
        assert!(validate(&profile).is_err());

        let mut profile = valid_profile();
        profile.router_timeout_secs = 0;
        assert!(validate(&profile).is_err());

        let mut profile = valid_profile();
        profile.routing_prompt = "  ".into();
        assert!(validate(&profile).is_err());
    }

    #[test]
    fn rejects_unknown_fields_instead_of_silently_ignoring_typos() {
        let raw = r#"
            [[profile]]
            name = "typo"
            router_model = "router/small"
            sliding_windows = 10
            model_pool = ["provider/fast"]
            routing_prompt = "rules"
        "#;
        let error = toml::from_str::<Profiles>(raw).unwrap_err().to_string();
        assert!(error.contains("unknown field `sliding_windows`"));
    }

    #[test]
    fn model_ids_require_both_components_but_allow_slashes_in_model_id() {
        assert_eq!(
            split_model_id("provider/family/model"),
            Some(("provider".into(), "family/model".into()))
        );
        assert!(split_model_id("provider/").is_none());
        assert!(split_model_id("/model").is_none());
        assert!(split_model_id("model").is_none());
    }
}
