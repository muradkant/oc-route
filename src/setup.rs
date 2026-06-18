use anyhow::{anyhow, Result};
use dialoguer::{theme::ColorfulTheme, Confirm, FuzzySelect, Input, MultiSelect};
use std::io::Write;

use crate::config::{self, Profile, Profiles, DEFAULT_ROUTER_MODEL, DEFAULT_SLIDING_WINDOW};
use crate::oc_client::{ModelInfo, OcClient, SessionInfo};

pub enum SessionChoice {
    Continue,
    Select(String),
    New,
}

/// Print a status line to stderr. The OpenCode TUI owns stdout; oc-route must not
/// write status text there or it bleeds through the TUI's rendering. dialoguer's own
/// prompts already go to stderr, so this keeps all setup chatter on one stream.
fn elog(msg: impl AsRef<str>) {
    let _ = writeln!(std::io::stderr(), "{}", msg.as_ref());
}

pub async fn run_setup(oc: &OcClient, profiles: &mut Profiles) -> Result<(Profile, SessionChoice)> {
    let theme = ColorfulTheme::default();

    let profile = select_or_create_profile(oc, profiles, &theme).await?;

    let mut sessions = oc.list_sessions().await.unwrap_or_default();
    // Drop oc-route's own throwaway router sessions so they never show up in the
    // picker and can never be picked as the "last conversation". They are created
    // and deleted on every routed message; if one lingers (e.g. a crashed run), it
    // is not a conversation the user would ever want to continue.
    sessions.retain(|s| !is_router_session(s));
    // Sort by most-recent activity (updated, falling back to created). This mirrors
    // OpenCode's own `--continue` heuristic and is what "last conversation" means
    // to a user. We resolve the continue target ourselves below (see select_session)
    // rather than handing OpenCode a bare `--continue`, because oc-route's own
    // throwaway router sessions can briefly become the newest session.
    sessions.sort_by_key(|s| std::cmp::Reverse(session_updated(s)));
    let session_choice = select_session(&sessions, &theme)?;

    Ok((profile, session_choice))
}

/// True for oc-route's internal router sessions (title "oc-route-router"). These are
/// short-lived scaffolding for routing decisions, never real conversations.
fn is_router_session(s: &SessionInfo) -> bool {
    s.title.as_deref() == Some("oc-route-router")
}

/// Effective "last activity" timestamp for a session: prefer `time.updated`, fall
/// back to `time.created`. Treats a missing timestamp as 0 (oldest).
fn session_updated(s: &SessionInfo) -> u64 {
    s.time
        .as_ref()
        .and_then(|t| t.updated.or(t.created))
        .unwrap_or(0)
}

async fn select_or_create_profile(
    oc: &OcClient,
    profiles: &mut Profiles,
    theme: &ColorfulTheme,
) -> Result<Profile> {
    if profiles.profiles.is_empty() {
        elog("No profiles found. Let's create one.");
        let profile = create_profile_interactive(oc, theme).await?;
        config::validate(&profile)?;
        config::upsert(profiles, profile.clone());
        config::save(profiles)?;
        return Ok(profile);
    }

    let mut items: Vec<String> = profiles.profiles.iter().map(|p| p.name.clone()).collect();
    items.push("Create new…".to_string());
    let selection = FuzzySelect::with_theme(theme)
        .with_prompt("Select a profile")
        .items(&items)
        .default(0)
        .interact()?;

    if selection == items.len() - 1 {
        let profile = create_profile_interactive(oc, theme).await?;
        config::validate(&profile)?;
        let save_it = Confirm::with_theme(theme)
            .with_prompt("Save this profile for future use?")
            .default(true)
            .interact()?;
        if save_it {
            config::upsert(profiles, profile.clone());
            config::save(profiles)?;
        }
        return Ok(profile);
    }

    Ok(profiles.profiles[selection].clone())
}

async fn create_profile_interactive(oc: &OcClient, theme: &ColorfulTheme) -> Result<Profile> {
    let name: String = Input::with_theme(theme)
        .with_prompt("Profile name")
        .interact_text()?;

    let models = oc.list_models().await.unwrap_or_default();
    let model_ids: Vec<String> = models.iter().map(model_display).collect();
    if model_ids.is_empty() {
        elog("Warning: could not fetch models from OpenCode. You can type model IDs manually.");
    }

    let pool_selections: Vec<usize> = if model_ids.is_empty() {
        Vec::new()
    } else {
        MultiSelect::with_theme(theme)
            .with_prompt("Select models for the pool (toggle with space)")
            .items(&model_ids)
            .interact()?
    };

    let mut model_pool: Vec<String> = pool_selections
        .iter()
        .map(|&i| full_model_id(&models[i]))
        .collect();

    if model_pool.is_empty() {
        elog("No models selected from the list. Enter model IDs as providerID/modelID (comma-separated):");
        let raw: String = Input::with_theme(theme)
            .with_prompt("Model pool")
            .interact_text()?;
        model_pool = raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }

    if model_pool.is_empty() {
        return Err(anyhow!("model pool cannot be empty"));
    }

    let router_items: Vec<String> = model_pool
        .iter()
        .cloned()
        .chain(std::iter::once(DEFAULT_ROUTER_MODEL.to_string()))
        .collect();
    let router_default = router_items
        .iter()
        .position(|m| m == DEFAULT_ROUTER_MODEL)
        .unwrap_or(0);
    let router_idx = FuzzySelect::with_theme(theme)
        .with_prompt("Router model (chooses which model handles each message)")
        .items(&router_items)
        .default(router_default)
        .interact()?;
    let router_model = router_items[router_idx].clone();
    if !model_pool.contains(&router_model) {
        model_pool.push(router_model.clone());
    }

    let sliding_window: usize = Input::with_theme(theme)
        .with_prompt("Sliding window (number of recent messages sent to the router)")
        .default(DEFAULT_SLIDING_WINDOW)
        .interact_text()?;

    elog("Enter your routing prompt. This is plain English describing when to use each model.");
    elog("Press Enter on a blank line to finish.");
    let routing_prompt = read_multiline(theme)?;

    if routing_prompt.trim().is_empty() {
        return Err(anyhow!("routing prompt cannot be empty"));
    }

    Ok(Profile {
        name,
        router_model,
        sliding_window,
        router_timeout_secs: config::DEFAULT_ROUTER_TIMEOUT_SECS,
        model_pool,
        routing_prompt,
    })
}

fn read_multiline(theme: &ColorfulTheme) -> Result<String> {
    let mut lines = Vec::new();
    loop {
        let line: String = Input::with_theme(theme)
            .with_prompt(if lines.is_empty() {
                ">>".to_string()
            } else {
                "..".to_string()
            })
            .allow_empty(true)
            .interact_text()?;
        if line.trim().is_empty() && !lines.is_empty() {
            break;
        }
        lines.push(line);
    }
    Ok(lines.join("\n"))
}

fn select_session(sessions: &[SessionInfo], theme: &ColorfulTheme) -> Result<SessionChoice> {
    let mut labels: Vec<String> = Vec::new();
    // When sessions exist, the first option resolves to the most-recently-active one
    // (sessions arrive pre-sorted by updated time). We pass the concrete id via
    // --session instead of OpenCode's --continue, so oc-route fully controls which
    // conversation opens — robust against any throwaway sessions in the list.
    let continue_label = match sessions.first() {
        Some(latest) => {
            let title = latest
                .title
                .clone()
                .filter(|t| !t.trim().is_empty())
                .unwrap_or_else(|| "untitled".to_string());
            format!("Continue last conversation — {} ({})", title, latest.id)
        }
        None => "Continue last conversation".to_string(),
    };
    labels.push(continue_label);
    let start_new_idx = 1 + sessions.len();
    for (i, s) in sessions.iter().enumerate() {
        let title = s
            .title
            .clone()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| format!("(untitled session {})", i));
        labels.push(format!("{} — {}", title, s.id));
    }
    labels.push("Start new conversation".to_string());

    let selection = FuzzySelect::with_theme(theme)
        .with_prompt("Select a session")
        .items(&labels)
        .default(0)
        .interact()?;

    if selection == 0 {
        // Resolve to the most-recently-active session by id. If there are no
        // sessions at all, fall back to OpenCode's --continue (a no-op that starts
        // fresh) rather than inventing an id.
        return Ok(match sessions.first() {
            Some(latest) => SessionChoice::Select(latest.id.clone()),
            None => SessionChoice::Continue,
        });
    }
    if selection == start_new_idx {
        return Ok(SessionChoice::New);
    }
    let session_idx = selection - 1;
    Ok(SessionChoice::Select(sessions[session_idx].id.clone()))
}

fn model_display(m: &ModelInfo) -> String {
    match &m.name {
        Some(n) => format!("{} ({})", n, full_model_id(m)),
        None => full_model_id(m),
    }
}

fn full_model_id(m: &ModelInfo) -> String {
    format!("{}/{}", m.provider_id, m.id)
}

pub fn flush_stdout() {
    let _ = std::io::stdout().flush();
}
