//! Subagent discovery + per-subagent routing for `<cli> claude --local`.
//!
//! Scans the user's existing Claude Code session sidecars to learn which
//! subagents they actually run, classifies each as read-only or mutating, and
//! lets them map each agent to a routing target (cloud / the local model / a
//! configured custom endpoint). The chosen mapping persists to settings.json
//! under `subagent_routes` and is materialized into the managed router config
//! that `--local` feeds to the proxy + local router (see
//! [`crate::onboarding::write_default_local_routes`]).
//!
//! The data source is the per-subagent `agent-<id>.meta.json` sidecar (field
//! `agentType`) — the same value the proxy resolves into the
//! `x-rayline-claude-code-agent-type` header at runtime, so the names harvested
//! here match the routing keys byte-for-byte. Scanning the sidecars (a few MB)
//! avoids parsing the multi-GB transcript JSONL.

use std::collections::{BTreeMap, HashMap};
use std::io::{self, IsTerminal, Write as _};
use std::path::{Path, PathBuf};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::style::Print;
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, queue};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{local_model, status};

/// settings.json schema for the persisted `subagent_routes` block. Bump when the
/// stored shape changes materially.
pub const SUBAGENT_ROUTES_SCHEMA: u32 = 1;

/// Tools whose presence means an agent can mutate the working tree or run
/// arbitrary commands; an agent granted any of these is NOT read-only, so it
/// stays on cloud by default. Compared case-insensitively.
const MUTATING_TOOLS: &[&str] = &["edit", "write", "multiedit", "notebookedit", "bash"];

/// Built-in Claude Code agent types that have no on-disk definition file. Their
/// capability is hardcoded so the read-only default works offline.
const BUILTIN_CAPABILITIES: &[(&str, Capability)] = &[
    ("explore", Capability::ReadOnly),
    ("plan", Capability::ReadOnly),
    ("general-purpose", Capability::Mutating),
];

/// Backstop on meta-sidecar files scanned per run — guards against pathological
/// session trees; far above any real history (~5k typical).
const MAX_META_FILES: usize = 200_000;

/// Agents used at least this many times are shown in the picker even without a
/// definition. The long tail of one-off orchestration agents is hidden unless
/// `--all` is passed.
const MIN_USES_DISPLAYED: u64 = 2;

/// Rows shown per page in the interactive picker. 16 so every row on a page is
/// addressable by a single hex key (`0`-`9`, `a`-`f`).
const PAGE_SIZE: usize = 16;

/// What we know about an agent's mutating capability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Capability {
    ReadOnly,
    Mutating,
    Unknown,
}

impl Capability {
    fn label(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::Mutating => "has-edit",
            Self::Unknown => "unknown",
        }
    }
}

/// One discovered subagent type and how often it was used.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredAgent {
    pub agent_type: String,
    pub uses: u64,
}

/// Where a subagent's requests are routed under `--local`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubagentTarget {
    /// Stay on cloud Claude (omitted from the proxy allowlist).
    Cloud,
    /// Route to the bundled local model (the `local` router endpoint).
    Local,
    /// Route to a configured custom OpenAI-compatible endpoint with this model.
    Endpoint { base_url: String, model: String },
}

impl SubagentTarget {
    fn to_json(&self) -> Value {
        match self {
            Self::Cloud => json!({ "target": "cloud" }),
            Self::Local => json!({ "target": "local" }),
            Self::Endpoint { base_url, model } => {
                json!({ "target": "endpoint", "base_url": base_url, "model": model })
            }
        }
    }

    fn from_json(value: &Value) -> Option<Self> {
        match value.get("target").and_then(Value::as_str)? {
            "cloud" => Some(Self::Cloud),
            "local" => Some(Self::Local),
            "endpoint" => {
                let base_url = value.get("base_url").and_then(Value::as_str)?.trim();
                let model = value.get("model").and_then(Value::as_str)?.trim();
                (!base_url.is_empty() && !model.is_empty()).then(|| Self::Endpoint {
                    base_url: base_url.to_owned(),
                    model: model.to_owned(),
                })
            }
            _ => None,
        }
    }

    fn short_label(&self) -> String {
        match self {
            Self::Cloud => "cloud".to_owned(),
            Self::Local => "local".to_owned(),
            Self::Endpoint { model, .. } => format!("endpoint:{model}"),
        }
    }
}

/// The persisted per-subagent routing decisions. Stored compactly: a single
/// `default` target for every subagent, plus `routes` holding ONLY the agents
/// whose target differs from the default. With the usual `cloud` default this
/// is a handful of entries even for users with hundreds of long-tail agents.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubagentRoutes {
    pub default: SubagentTarget,
    pub routes: BTreeMap<String, SubagentTarget>,
}

impl Default for SubagentRoutes {
    fn default() -> Self {
        Self {
            default: SubagentTarget::Cloud,
            routes: BTreeMap::new(),
        }
    }
}

impl SubagentRoutes {
    /// The effective target for an agent: its explicit exception, else the default.
    fn effective(&self, agent: &str) -> SubagentTarget {
        self.routes
            .get(agent)
            .cloned()
            .unwrap_or_else(|| self.default.clone())
    }

    /// Compact a full agent→target map: keep only entries that differ from the
    /// default. Used at save time so the stored file stays small.
    fn compact(
        default: SubagentTarget,
        full: impl IntoIterator<Item = (String, SubagentTarget)>,
    ) -> Self {
        let routes = full
            .into_iter()
            .filter(|(_, target)| target != &default)
            .collect();
        Self { default, routes }
    }
}

// ---------------------------------------------------------------------------
// Discovery: scan the Claude Code session sidecars.
// ---------------------------------------------------------------------------

/// Resolve the Claude Code projects roots, mirroring the proxy: honor
/// `$CLAUDE_CONFIG_DIR/projects` first, then `$HOME/.claude/projects`.
pub fn claude_projects_roots(home: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        if !dir.is_empty() {
            roots.push(PathBuf::from(dir).join("projects"));
        }
    }
    roots.push(home.join(".claude").join("projects"));
    roots.dedup();
    roots
}

/// Scan session sidecars under the given roots for distinct subagent types,
/// ranked by descending use (ties broken by name for deterministic output).
pub fn discover_agents(roots: &[PathBuf]) -> Vec<DiscoveredAgent> {
    let mut counts: HashMap<String, u64> = HashMap::new();
    let mut scanned = 0usize;
    for root in roots {
        scan_root(root, &mut counts, &mut scanned);
    }
    rank(counts)
}

fn scan_root(root: &Path, counts: &mut HashMap<String, u64>, scanned: &mut usize) {
    let Ok(projects) = std::fs::read_dir(root) else {
        return;
    };
    for project in projects.flatten() {
        if *scanned >= MAX_META_FILES {
            return;
        }
        if !project.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let Ok(sessions) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for session in sessions.flatten() {
            if *scanned >= MAX_META_FILES {
                return;
            }
            let subagents = session.path().join("subagents");
            let Ok(metas) = std::fs::read_dir(&subagents) else {
                continue;
            };
            for meta in metas.flatten() {
                if *scanned >= MAX_META_FILES {
                    return;
                }
                let path = meta.path();
                let is_meta = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with(".meta.json"));
                if !is_meta {
                    continue;
                }
                *scanned += 1;
                if let Some(agent_type) = read_agent_type(&path) {
                    *counts.entry(agent_type).or_default() += 1;
                }
            }
        }
    }
}

fn read_agent_type(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    let agent_type = value.get("agentType")?.as_str()?.trim();
    (!agent_type.is_empty()).then(|| agent_type.to_owned())
}

fn rank(counts: HashMap<String, u64>) -> Vec<DiscoveredAgent> {
    let mut agents: Vec<DiscoveredAgent> = counts
        .into_iter()
        .map(|(agent_type, uses)| DiscoveredAgent { agent_type, uses })
        .collect();
    agents.sort_by(|a, b| {
        b.uses
            .cmp(&a.uses)
            .then_with(|| a.agent_type.cmp(&b.agent_type))
    });
    agents
}

// ---------------------------------------------------------------------------
// Classification: read agent definitions, decide read-only vs mutating.
// ---------------------------------------------------------------------------

/// Directories that may hold agent definitions: the global set plus the
/// project-local set under `cwd`.
pub fn agent_definition_dirs(home: &Path, cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![home.join(".claude").join("agents")];
    let project = cwd.join(".claude").join("agents");
    if !dirs.contains(&project) {
        dirs.push(project);
    }
    dirs
}

/// Build a capability index (lowercased agent name → capability) from the
/// built-ins plus any `*.md` definitions in `dirs`.
pub fn load_agent_definitions(dirs: &[PathBuf]) -> HashMap<String, Capability> {
    let mut index: HashMap<String, Capability> = BUILTIN_CAPABILITIES
        .iter()
        .map(|(name, cap)| ((*name).to_owned(), *cap))
        .collect();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Some((name, cap)) = parse_agent_definition(&text) {
                // Routing is keyed only by agent name, so when the same name has
                // several definitions (global + project, or a name shadowing a
                // built-in) the *most mutating* capability must win — never
                // under-classify a command/edit-capable agent as read-only.
                index
                    .entry(name)
                    .and_modify(|existing| *existing = more_capable(*existing, cap))
                    .or_insert(cap);
            }
        }
    }
    index
}

/// Pick the more mutating of two capabilities (Mutating > ReadOnly > Unknown).
/// Used to merge duplicate agent definitions conservatively.
fn more_capable(a: Capability, b: Capability) -> Capability {
    fn rank(capability: Capability) -> u8 {
        match capability {
            Capability::Mutating => 2,
            Capability::ReadOnly => 1,
            Capability::Unknown => 0,
        }
    }
    if rank(b) > rank(a) { b } else { a }
}

/// Parse a `*.md` agent definition's YAML frontmatter; returns the lowercased
/// `name` and the capability implied by its `tools:` line. Handles the common
/// inline `tools: A, B, C` form (not multi-line YAML lists).
fn parse_agent_definition(markdown: &str) -> Option<(String, Capability)> {
    let mut lines = markdown.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    let mut name = None;
    let mut tools = None;
    for line in lines {
        if line.trim() == "---" {
            break;
        }
        if let Some(rest) = line.strip_prefix("name:") {
            name = Some(rest.trim().trim_matches(['"', '\'']).to_ascii_lowercase());
        } else if let Some(rest) = line.strip_prefix("tools:") {
            tools = Some(rest.trim().to_owned());
        }
    }
    let name = name?;
    let capability = tools
        .map(|t| classify_tools(&t))
        .unwrap_or(Capability::Unknown);
    Some((name, capability))
}

/// Classify a `tools:` frontmatter value. `*` or "All tools" grants everything
/// (mutating); an explicit list is read-only only if it names no mutating tool.
/// YAML quoting/brackets are stripped both around the whole value and per token,
/// so `tools: "Bash"`, `tools: "Read, Edit"`, and `tools: [Read, Edit]` are
/// classified correctly rather than slipping past the mutating-tool check.
fn classify_tools(tools: &str) -> Capability {
    let trimmed = tools
        .trim()
        .trim_matches(['"', '\''])
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim();
    if trimmed.is_empty() {
        return Capability::Unknown;
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower == "*" || lower.contains("all tools") {
        return Capability::Mutating;
    }
    let grants_mutation = trimmed
        .split(',')
        .map(|tool| {
            tool.trim()
                .trim_matches(['"', '\''])
                .trim()
                .to_ascii_lowercase()
        })
        .any(|tool| MUTATING_TOOLS.contains(&tool.as_str()));
    if grants_mutation {
        Capability::Mutating
    } else {
        Capability::ReadOnly
    }
}

fn capability_of(agent_type: &str, index: &HashMap<String, Capability>) -> Capability {
    index
        .get(&agent_type.to_ascii_lowercase())
        .copied()
        .unwrap_or(Capability::Unknown)
}

/// Default routing target for an agent with no prior decision: read-only and
/// anchor agents go local (a bad local read is recoverable), everything else
/// stays on cloud.
fn default_target(agent_type: &str, capability: Capability) -> SubagentTarget {
    let anchored = crate::onboarding::LOCAL_DEFAULT_SUBAGENTS
        .iter()
        .any(|anchor| anchor.eq_ignore_ascii_case(agent_type));
    if anchored || capability == Capability::ReadOnly {
        SubagentTarget::Local
    } else {
        SubagentTarget::Cloud
    }
}

// ---------------------------------------------------------------------------
// Persistence + router-config materialization.
// ---------------------------------------------------------------------------

pub fn read_subagent_routes(home: &Path) -> Option<SubagentRoutes> {
    let entry = status::read_settings(home)?.get("subagent_routes")?.clone();
    // `default` is the fallback for any unlisted agent; absent (legacy files)
    // means cloud. `routes` then holds only the exceptions.
    let default = entry
        .get("default")
        .and_then(SubagentTarget::from_json)
        .unwrap_or(SubagentTarget::Cloud);
    let mut routes = BTreeMap::new();
    if let Some(routes_obj) = entry.get("routes").and_then(Value::as_object) {
        for (agent, target) in routes_obj {
            if let Some(target) = SubagentTarget::from_json(target) {
                // Drop any entry that merely restates the default (keeps legacy
                // verbose files compact once re-read).
                if target != default {
                    routes.insert(agent.clone(), target);
                }
            }
        }
    }
    Some(SubagentRoutes { default, routes })
}

pub fn write_subagent_routes_in_home(home: &Path, routes: &SubagentRoutes) -> io::Result<()> {
    let mut settings = status::read_settings(home)
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    let object = settings
        .as_object_mut()
        .expect("settings is an object by construction");
    let mut routes_json = serde_json::Map::new();
    for (agent, target) in &routes.routes {
        // Never persist an entry equal to the default — that's what keeps the
        // file compact for users with a long tail of agents.
        if target != &routes.default {
            routes_json.insert(agent.clone(), target.to_json());
        }
    }
    object.insert(
        "subagent_routes".to_owned(),
        json!({
            "schema": SUBAGENT_ROUTES_SCHEMA,
            "default": routes.default.to_json(),
            "routes": Value::Object(routes_json),
        }),
    );
    status::write_settings(home, &settings)
}

/// Synthesize a stable router-endpoint id from a base URL (base URLs are not
/// valid endpoint ids; this maps each distinct URL to one deterministic token).
/// The truncated slug is for readability; a short digest of the full URL is
/// appended so two URLs sharing a 40-char slug prefix never collide onto one id.
fn endpoint_id(base_url: &str) -> String {
    let slug: String = base_url
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-');
    let slug = &slug[..slug.len().min(40)];
    let digest = Sha256::digest(base_url.as_bytes());
    let suffix: String = digest
        .iter()
        .take(4)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("custom-{slug}-{suffix}")
}

/// Sentinel allowlist entry for an all-cloud mapping. The proxy treats an EMPTY
/// selective-subagent list as the legacy "route every subagent to local", so to
/// honor an explicit all-cloud choice we keep the list non-empty with a key that
/// can never match a real agent id or type — nothing gets intercepted.
const ROUTE_NOTHING_SENTINEL: &str = "__rayline_no_local_subagents__";

/// Build the managed router config (proxy allowlist + router endpoints + routes)
/// from a persisted mapping. Cloud agents are omitted so the proxy leaves them
/// on cloud; local agents use the bundled `local` endpoint (no model — the
/// router fills it from the served model); endpoint agents get a synthesized
/// custom endpoint reached via the router's remote-forward path.
pub fn routes_config_json(routes: &SubagentRoutes) -> Value {
    let mut subagents = serde_json::Map::new();
    // endpoint id -> (base_url, models seen)
    let mut endpoints: BTreeMap<String, (String, Vec<String>)> = BTreeMap::new();
    for (agent, target) in &routes.routes {
        match target {
            SubagentTarget::Cloud => {} // omit → stays on cloud Claude
            SubagentTarget::Local => {
                subagents.insert(agent.clone(), json!({ "endpoint": "local" }));
            }
            SubagentTarget::Endpoint { base_url, model } => {
                let id = endpoint_id(base_url);
                subagents.insert(agent.clone(), json!({ "endpoint": id, "model": model }));
                let entry = endpoints
                    .entry(id)
                    .or_insert_with(|| (base_url.clone(), Vec::new()));
                if !entry.1.contains(model) {
                    entry.1.push(model.clone());
                }
            }
        }
    }
    // All-cloud mapping → empty allowlist would invert to "route everything
    // local"; emit the sentinel so the proxy intercepts nothing instead.
    if subagents.is_empty() {
        subagents.insert(
            ROUTE_NOTHING_SENTINEL.to_owned(),
            json!({ "endpoint": "local" }),
        );
    }
    let mut config = serde_json::Map::new();
    config.insert(
        "routes".to_owned(),
        json!({ "subagents": Value::Object(subagents) }),
    );
    if !endpoints.is_empty() {
        let endpoints_json: Vec<Value> = endpoints
            .into_iter()
            .map(|(id, (base_url, models))| {
                json!({
                    "id": id,
                    "kind": "custom",
                    // Custom endpoints saved via `local custom` are Anthropic
                    // Messages servers (validated at `{base_url}/v1/messages`),
                    // so the router must speak that protocol, not openai_chat.
                    "protocol": "anthropic_messages",
                    "base_url": base_url,
                    "models": models,
                })
            })
            .collect();
        config.insert("endpoints".to_owned(), Value::Array(endpoints_json));
    }
    Value::Object(config)
}

// ---------------------------------------------------------------------------
// Interactive picker (pure core + thin IO shell).
// ---------------------------------------------------------------------------

/// The targets a user can cycle an agent through: cloud, the bundled local
/// model, then each configured custom endpoint.
#[derive(Clone, Debug, Default)]
pub struct TargetMenu {
    /// Extra (base_url, model) endpoints beyond cloud + the bundled local model.
    pub endpoints: Vec<(String, String)>,
}

/// Read the configured custom endpoints (beyond the bundled `local` model) so
/// the picker can offer them as per-subagent targets.
pub fn target_menu(home: &Path) -> TargetMenu {
    let Some(cfg) = local_model::read_from_home(home) else {
        return TargetMenu::default();
    };
    // Only in Custom mode is the active `base_url`/`model` the bundled "local"
    // target — there it must not also appear as a separate endpoint. In
    // Recommended mode "local" is the bundled GGUF, so any retained custom pair
    // is a real alternative endpoint the user can still route subagents to.
    let active_local = if matches!(cfg.mode, local_model::LocalModelMode::Custom) {
        cfg.base_url
            .as_ref()
            .zip(cfg.model.as_ref())
            .map(|(base_url, model)| (base_url.clone(), model.clone()))
    } else {
        None
    };
    let mut candidates: Vec<(String, String)> = cfg
        .custom_endpoints
        .iter()
        .map(|endpoint| (endpoint.base_url.clone(), endpoint.model.clone()))
        .collect();
    if matches!(cfg.mode, local_model::LocalModelMode::Recommended) {
        if let (Some(base_url), Some(model)) = (cfg.base_url.as_ref(), cfg.model.as_ref()) {
            candidates.push((base_url.clone(), model.clone()));
        }
    }
    let mut endpoints = Vec::new();
    for pair in candidates {
        if Some(&pair) != active_local.as_ref() && !endpoints.contains(&pair) {
            endpoints.push(pair);
        }
    }
    TargetMenu { endpoints }
}

impl TargetMenu {
    /// The full cycle order: cloud, local, then each configured endpoint.
    fn sequence(&self) -> Vec<SubagentTarget> {
        let mut seq = vec![SubagentTarget::Cloud, SubagentTarget::Local];
        for (base_url, model) in &self.endpoints {
            seq.push(SubagentTarget::Endpoint {
                base_url: base_url.clone(),
                model: model.clone(),
            });
        }
        seq
    }

    /// Advance a target one step around the cycle.
    fn cycle(&self, target: &SubagentTarget) -> SubagentTarget {
        let seq = self.sequence();
        match seq.iter().position(|candidate| candidate == target) {
            Some(index) => seq[(index + 1) % seq.len()].clone(),
            // Unknown/foreign target (e.g. an endpoint no longer configured):
            // restart the cycle at local.
            None => SubagentTarget::Local,
        }
    }
}

/// One editable row in the picker.
#[derive(Clone, Debug)]
pub struct Row {
    pub agent_type: String,
    pub uses: u64,
    pub capability: Capability,
    pub target: SubagentTarget,
}

/// Assemble the editable rows: discovered agents (filtered to the meaningful set
/// unless `include_all`), plus any anchor or previously-decided agent that
/// wasn't discovered, so prior choices stay visible.
///
/// `configured` is the saved mapping, or `None` on the very first run. On first
/// run each agent gets the read-only heuristic (read-only/anchor → local) to
/// seed sensible exceptions; once configured, an agent's target is its stored
/// exception, else the configured default (so the long tail follows the default
/// predictably rather than re-applying the heuristic).
pub fn build_rows(
    discovered: &[DiscoveredAgent],
    index: &HashMap<String, Capability>,
    configured: Option<&SubagentRoutes>,
    include_all: bool,
) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    let target_for = |agent: &str, capability: Capability| match configured {
        Some(config) => config.effective(agent),
        None => default_target(agent, capability),
    };

    for agent in discovered {
        let capability = capability_of(&agent.agent_type, index);
        let decided = configured.is_some_and(|c| c.routes.contains_key(&agent.agent_type));
        let anchored = crate::onboarding::LOCAL_DEFAULT_SUBAGENTS
            .iter()
            .any(|anchor| anchor.eq_ignore_ascii_case(&agent.agent_type));
        let meaningful = include_all
            || decided
            || anchored
            || capability != Capability::Unknown
            || agent.uses >= MIN_USES_DISPLAYED;
        if !meaningful {
            continue;
        }
        seen.insert(agent.agent_type.clone());
        rows.push(Row {
            agent_type: agent.agent_type.clone(),
            uses: agent.uses,
            capability,
            target: target_for(&agent.agent_type, capability),
        });
    }

    // Anchors that never appeared in history still deserve a row (the default
    // read-only allowlist), as does any previously-decided agent.
    let decided_keys = configured
        .into_iter()
        .flat_map(|c| c.routes.keys().cloned());
    let extras = crate::onboarding::LOCAL_DEFAULT_SUBAGENTS
        .iter()
        .map(|anchor| (*anchor).to_owned())
        .chain(decided_keys);
    for agent in extras {
        if seen.contains(&agent) {
            continue;
        }
        seen.insert(agent.clone());
        let capability = capability_of(&agent, index);
        rows.push(Row {
            agent_type: agent.clone(),
            uses: 0,
            capability,
            target: target_for(&agent, capability),
        });
    }
    rows
}

/// Which capability the picker currently shows. Cycled with `f`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KindFilter {
    All,
    ReadOnly,
    Mutating,
    Unknown,
}

impl KindFilter {
    fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::ReadOnly => "read-only",
            Self::Mutating => "has-edit",
            Self::Unknown => "unknown",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::All => Self::ReadOnly,
            Self::ReadOnly => Self::Mutating,
            Self::Mutating => Self::Unknown,
            Self::Unknown => Self::All,
        }
    }

    fn matches(self, capability: Capability) -> bool {
        match self {
            Self::All => true,
            Self::ReadOnly => capability == Capability::ReadOnly,
            Self::Mutating => capability == Capability::Mutating,
            Self::Unknown => capability == Capability::Unknown,
        }
    }
}

/// A single keypress translated into a picker command. Rows on the current page
/// are addressed by a hex slot (`0`-`9`, `a`-`f`); commands use non-hex keys.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PickerKey {
    /// Cycle the target of the row in this 0-based slot on the current page.
    Row(usize),
    NextPage,
    PrevPage,
    CycleFilter,
    AllLocal,
    AllCloud,
    Search,
    Save,
    Quit,
    /// An unrecognized key — ignored.
    Ignore,
}

/// Map a character to a picker command. Hex digits address page rows; the
/// remaining keys are commands chosen to never collide with `0`-`9a`-`f`.
pub fn map_key(c: char) -> PickerKey {
    match c {
        '0'..='9' => PickerKey::Row((c as u8 - b'0') as usize),
        'a'..='f' => PickerKey::Row((c as u8 - b'a' + 10) as usize),
        'n' => PickerKey::NextPage,
        'p' => PickerKey::PrevPage,
        'k' => PickerKey::CycleFilter,
        'L' => PickerKey::AllLocal,
        'C' => PickerKey::AllCloud,
        '/' => PickerKey::Search,
        's' => PickerKey::Save,
        'q' => PickerKey::Quit,
        _ => PickerKey::Ignore,
    }
}

/// What an applied key means for the edit loop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PickerAction {
    Continue,
    /// The caller should enter name-search input mode.
    Search,
    Save,
    Quit,
}

/// The editable picker: the full row set plus the current view (kind filter +
/// name query) and page. The view is a window over `rows`; saving uses every
/// row regardless of the active filter.
pub struct PickerState {
    pub rows: Vec<Row>,
    /// The fallback target for unlisted agents; only rows differing from it are
    /// persisted (keeps the saved file compact).
    pub default: SubagentTarget,
    pub filter: KindFilter,
    /// Lowercased substring filter on the agent name; empty = no name filter.
    pub query: String,
    pub page: usize,
}

impl PickerState {
    pub fn new(rows: Vec<Row>, default: SubagentTarget) -> Self {
        Self {
            rows,
            default,
            filter: KindFilter::All,
            query: String::new(),
            page: 0,
        }
    }

    /// Indices into `rows` matching the current kind filter AND name query.
    fn view(&self) -> Vec<usize> {
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, row)| self.filter.matches(row.capability))
            .filter(|(_, row)| {
                self.query.is_empty() || row.agent_type.to_ascii_lowercase().contains(&self.query)
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn page_count(&self, view_len: usize) -> usize {
        view_len.div_ceil(PAGE_SIZE).max(1)
    }

    /// `rows` indices shown on the current page (≤ PAGE_SIZE).
    fn page_rows(&self) -> Vec<usize> {
        self.view()
            .into_iter()
            .skip(self.page * PAGE_SIZE)
            .take(PAGE_SIZE)
            .collect()
    }

    /// Set the name query (lowercased) and reset to the first page.
    pub fn set_query(&mut self, query: &str) {
        self.query = query.trim().to_ascii_lowercase();
        self.page = 0;
    }

    /// Apply a single keypress. Returns the resulting edit-loop action.
    pub fn apply_key(&mut self, menu: &TargetMenu, key: PickerKey) -> PickerAction {
        match key {
            PickerKey::Row(slot) => {
                if let Some(&row) = self.page_rows().get(slot) {
                    self.rows[row].target = menu.cycle(&self.rows[row].target);
                }
                PickerAction::Continue
            }
            PickerKey::NextPage => {
                if self.page + 1 < self.page_count(self.view().len()) {
                    self.page += 1;
                }
                PickerAction::Continue
            }
            PickerKey::PrevPage => {
                self.page = self.page.saturating_sub(1);
                PickerAction::Continue
            }
            PickerKey::CycleFilter => {
                self.filter = self.filter.next();
                self.page = 0;
                PickerAction::Continue
            }
            PickerKey::AllLocal => {
                for index in self.view() {
                    self.rows[index].target = SubagentTarget::Local;
                }
                PickerAction::Continue
            }
            PickerKey::AllCloud => {
                for index in self.view() {
                    self.rows[index].target = SubagentTarget::Cloud;
                }
                PickerAction::Continue
            }
            PickerKey::Search => PickerAction::Search,
            PickerKey::Save => PickerAction::Save,
            PickerKey::Quit => PickerAction::Quit,
            PickerKey::Ignore => PickerAction::Continue,
        }
    }
}

fn rows_to_routes(rows: &[Row], default: SubagentTarget) -> SubagentRoutes {
    SubagentRoutes::compact(
        default,
        rows.iter()
            .map(|row| (row.agent_type.clone(), row.target.clone())),
    )
}

// ---------------------------------------------------------------------------
// Commands: `<cli> local subagents [--json] [--all]` and the onboard hook.
// ---------------------------------------------------------------------------

/// `<cli> local subagents` — scan history, then either print the inventory as
/// JSON (`--json`, non-interactive) or run the interactive mapping picker.
pub async fn run_subagents_command(json: bool, include_all: bool) -> Result<(), String> {
    let home = dirs::home_dir().ok_or_else(|| "home directory not found".to_owned())?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| home.clone());

    eprintln!("Scanning your Claude Code history for subagents…");
    let discovered = discover_agents(&claude_projects_roots(&home));
    let index = load_agent_definitions(&agent_definition_dirs(&home, &cwd));
    let existing = read_subagent_routes(&home);
    let default = existing
        .as_ref()
        .map(|e| e.default.clone())
        .unwrap_or(SubagentTarget::Cloud);

    if json {
        print_inventory_json(&discovered, &index, existing.as_ref());
        return Ok(());
    }

    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        eprintln!(
            "`{cli} local subagents` needs an interactive terminal. \
             Use `{cli} local subagents --json` to inspect discovered agents non-interactively.",
            cli = crate::CLI_BIN,
        );
        return Ok(());
    }

    let menu = target_menu(&home);
    let rows = build_rows(&discovered, &index, existing.as_ref(), include_all);
    if rows.is_empty() {
        eprintln!(
            "No subagents discovered in your history. The read-only default ({} agents) applies under `--local`.",
            crate::onboarding::LOCAL_DEFAULT_SUBAGENTS.len(),
        );
        return Ok(());
    }

    run_picker(
        &home,
        &menu,
        PickerState::new(rows, default),
        discovered.len(),
    )
}

/// RAII guard: enter raw mode + alternate screen, restore on drop so a panic or
/// early return never leaves the user's terminal wedged. Mirrors `router.rs`.
struct PickerTerminalGuard;

impl PickerTerminalGuard {
    fn enter(stdout: &mut io::Stdout) -> io::Result<Self> {
        terminal::enable_raw_mode().map_err(io::Error::other)?;
        if let Err(error) = execute!(stdout, EnterAlternateScreen, Hide) {
            let _ = terminal::disable_raw_mode();
            return Err(error);
        }
        Ok(Self)
    }
}

impl Drop for PickerTerminalGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = execute!(stdout, Show, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

/// The interactive edit loop: draw the current page → read ONE keypress → apply
/// → repeat until the user saves or quits. No Enter required to act on a key.
fn run_picker(
    home: &Path,
    menu: &TargetMenu,
    mut state: PickerState,
    discovered_total: usize,
) -> Result<(), String> {
    let saved = {
        let mut stdout = io::stdout();
        let _guard = PickerTerminalGuard::enter(&mut stdout).map_err(|e| e.to_string())?;
        let mut searching: Option<String> = None;
        loop {
            draw_picker(
                &mut stdout,
                &state,
                menu,
                discovered_total,
                searching.as_deref(),
            )
            .map_err(|e| e.to_string())?;

            let Event::Key(key) = event::read().map_err(|e| e.to_string())? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            // Ctrl-C always aborts without saving (raw mode swallows the signal).
            if matches!(key.code, KeyCode::Char('c'))
                && key.modifiers.contains(KeyModifiers::CONTROL)
            {
                break false;
            }

            // Name-search input mode captures text until Enter (commit) / Esc.
            if let Some(buffer) = searching.as_mut() {
                match key.code {
                    KeyCode::Enter => {
                        state.set_query(buffer);
                        searching = None;
                    }
                    KeyCode::Esc => searching = None,
                    KeyCode::Backspace => {
                        buffer.pop();
                    }
                    KeyCode::Char(c) => buffer.push(c),
                    _ => {}
                }
                continue;
            }

            let action = match key.code {
                KeyCode::Enter => PickerAction::Save,
                KeyCode::Esc => PickerAction::Quit,
                KeyCode::Char(c) => state.apply_key(menu, map_key(c)),
                _ => PickerAction::Continue,
            };
            match action {
                PickerAction::Continue => continue,
                PickerAction::Search => searching = Some(state.query.clone()),
                PickerAction::Save => break true,
                PickerAction::Quit => break false,
            }
        }
    };

    if !saved {
        eprintln!("No changes saved.");
        return Ok(());
    }

    let routes = rows_to_routes(&state.rows, state.default.clone());
    let stored = routes.routes.len();
    write_subagent_routes_in_home(home, &routes).map_err(|e| e.to_string())?;
    // Refresh the managed router config so the change takes effect next launch.
    crate::onboarding::write_default_local_routes(home).map_err(|e| e.to_string())?;

    let local = state
        .rows
        .iter()
        .filter(|r| !matches!(r.target, SubagentTarget::Cloud))
        .count();
    eprintln!(
        "\nSaved {stored} exception(s) (default: {default}); {local} subagent(s) route to local under `{cli} claude --local`.",
        default = state.default.short_label(),
        cli = crate::CLI_BIN,
    );
    Ok(())
}

/// Draw the current page into the alternate screen. Row slots are labeled with
/// a single hex digit (`0`-`f`); pressing it cycles that row.
fn draw_picker(
    stdout: &mut io::Stdout,
    state: &PickerState,
    menu: &TargetMenu,
    discovered_total: usize,
    searching: Option<&str>,
) -> io::Result<()> {
    let view = state.view();
    let total = view.len();
    let pages = state.page_count(total);
    let start = state.page * PAGE_SIZE;
    let end = (start + PAGE_SIZE).min(total);

    let query_note = if state.query.is_empty() {
        String::new()
    } else {
        format!("  search:\"{}\"", state.query)
    };
    let mut lines: Vec<String> = vec![
        format!(
            "Map {discovered_total} subagent(s) for `{} claude --local`.  Unlisted agents → {default} (only changes are saved).",
            crate::CLI_BIN,
            default = state.default.short_label(),
        ),
        format!(
            "Filter: {filter}{query_note}   Page {page}/{pages}   ({shown} of {total} shown)",
            filter = state.filter.label(),
            page = state.page + 1,
            shown = if total == 0 {
                "0".to_owned()
            } else {
                format!("{}-{}", start + 1, end)
            },
        ),
        format!(
            "  key  {:<34} {:>6}  {:<10} target",
            "agent", "uses", "kind"
        ),
    ];
    if total == 0 {
        lines.push("  (no agents match — press k or / to change)".to_owned());
    }
    for (slot, &row_index) in view[start..end].iter().enumerate() {
        let agent = &state.rows[row_index];
        let uses = if agent.uses == 0 {
            "-".to_owned()
        } else {
            agent.uses.to_string()
        };
        lines.push(format!(
            "   {:1x}   {:<34} {:>6}  {:<10} {}",
            slot,
            truncate(&agent.agent_type, 34),
            uses,
            agent.capability.label(),
            agent.target.short_label(),
        ));
    }

    // Pin the footer below a full page so it doesn't jump on short last pages.
    while lines.len() < PAGE_SIZE + 4 {
        lines.push(String::new());
    }
    lines.push(match searching {
        Some(buffer) => format!("search name: {buffer}_   (Enter apply · Esc cancel)"),
        None => {
            let cycle_hint = if menu.endpoints.is_empty() {
                "local/cloud"
            } else {
                "cloud->local->endpoint"
            };
            format!(
                "0-f cycle {cycle_hint} · n/p page · k kind(->{}) · / search · L/C all local/cloud · s save · q quit",
                state.filter.next().label()
            )
        }
    });

    queue!(stdout, Clear(ClearType::All))?;
    for (row, text) in lines.into_iter().enumerate() {
        queue!(stdout, MoveTo(0, row as u16), Print(text))?;
    }
    stdout.flush()
}

fn truncate(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        value.to_owned()
    } else {
        let kept: String = value.chars().take(width.saturating_sub(1)).collect();
        format!("{kept}…")
    }
}

fn print_inventory_json(
    discovered: &[DiscoveredAgent],
    index: &HashMap<String, Capability>,
    configured: Option<&SubagentRoutes>,
) {
    let agents: Vec<Value> = discovered
        .iter()
        .map(|agent| {
            let capability = capability_of(&agent.agent_type, index);
            let target = match configured {
                Some(config) => config.effective(&agent.agent_type),
                None => default_target(&agent.agent_type, capability),
            };
            json!({
                "agent_type": agent.agent_type,
                "uses": agent.uses,
                "capability": capability.label(),
                "target": target.short_label(),
            })
        })
        .collect();
    let payload = json!({
        "default": configured
            .map(|c| c.default.short_label())
            .unwrap_or_else(|| SubagentTarget::Cloud.short_label()),
        "agents": agents,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".to_owned())
    );
}

/// Offered at the end of `local onboard`: ask whether to map subagents now, and
/// run the picker if the user agrees. Best-effort — never fails the onboard.
pub fn offer_subagent_mapping(home: &Path) {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return;
    }
    eprint!("\nMap your subagents to local/cloud now? [Y/n] ");
    io::stderr().flush().ok();
    let mut answer = String::new();
    if io::stdin().read_line(&mut answer).is_err() {
        return;
    }
    if matches!(answer.trim().to_ascii_lowercase().as_str(), "n" | "no") {
        eprintln!(
            "Skipped. The read-only default applies; re-run anytime with `{} local subagents`.",
            crate::CLI_BIN,
        );
        return;
    }

    eprintln!("Scanning your Claude Code history for subagents…");
    let cwd = std::env::current_dir().unwrap_or_else(|_| home.to_path_buf());
    let discovered = discover_agents(&claude_projects_roots(home));
    let index = load_agent_definitions(&agent_definition_dirs(home, &cwd));
    let existing = read_subagent_routes(home);
    let default = existing
        .as_ref()
        .map(|e| e.default.clone())
        .unwrap_or(SubagentTarget::Cloud);
    let menu = target_menu(home);
    let rows = build_rows(&discovered, &index, existing.as_ref(), false);
    if rows.is_empty() {
        eprintln!("No subagents discovered yet; the read-only default applies.");
        return;
    }
    if let Err(error) = run_picker(
        home,
        &menu,
        PickerState::new(rows, default),
        discovered.len(),
    ) {
        eprintln!("Subagent mapping skipped: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rl-discover-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".config").join("rayline")).unwrap();
        dir
    }

    fn write_meta(root: &Path, project: &str, session: &str, id: &str, agent_type: &str) {
        let dir = root.join(project).join(session).join("subagents");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("agent-{id}.meta.json")),
            json!({ "agentType": agent_type, "toolUseId": id }).to_string(),
        )
        .unwrap();
    }

    #[test]
    fn discover_ranks_by_use_then_name() {
        let root = std::env::temp_dir().join(format!("rl-disc-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        write_meta(&root, "proj-a", "sess1", "1", "Explore");
        write_meta(&root, "proj-a", "sess1", "2", "Explore");
        write_meta(&root, "proj-a", "sess1", "3", "general-purpose");
        write_meta(&root, "proj-b", "sess2", "4", "Explore");
        write_meta(&root, "proj-b", "sess2", "5", "codebase-analyzer");
        // A malformed file must be skipped, not crash the scan.
        let bad = root.join("proj-b").join("sess2").join("subagents");
        std::fs::write(bad.join("agent-bad.meta.json"), "{ not json").unwrap();

        let agents = discover_agents(&[root]);
        assert_eq!(
            agents[0],
            DiscoveredAgent {
                agent_type: "Explore".into(),
                uses: 3
            }
        );
        // codebase-analyzer and general-purpose both have 1 use → name order.
        assert_eq!(agents[1].agent_type, "codebase-analyzer");
        assert_eq!(agents[2].agent_type, "general-purpose");
    }

    #[test]
    fn discover_missing_root_is_empty() {
        let root = std::env::temp_dir().join("rl-disc-nope-does-not-exist");
        assert!(discover_agents(&[root]).is_empty());
    }

    #[test]
    fn classify_tools_detects_mutation() {
        assert_eq!(classify_tools("Read, Grep, Glob, LS"), Capability::ReadOnly);
        assert_eq!(classify_tools("Read, Edit, Bash"), Capability::Mutating);
        assert_eq!(classify_tools("All tools"), Capability::Mutating);
        assert_eq!(classify_tools("*"), Capability::Mutating);
        assert_eq!(classify_tools("WebSearch, WebFetch"), Capability::ReadOnly);
        assert_eq!(classify_tools(""), Capability::Unknown);
        // P1: YAML quoting/brackets must not hide a mutating tool.
        assert_eq!(classify_tools("\"Bash\""), Capability::Mutating);
        assert_eq!(classify_tools("'Bash'"), Capability::Mutating);
        assert_eq!(classify_tools("\"Read, Edit\""), Capability::Mutating);
        assert_eq!(classify_tools("[Read, Edit]"), Capability::Mutating);
        assert_eq!(classify_tools("[\"Read\", \"Glob\"]"), Capability::ReadOnly);
    }

    #[test]
    fn parse_definition_extracts_name_and_capability() {
        let md = "---\nname: codebase-locator\ndescription: finds files\ntools: Grep, Glob, LS\ncolor: blue\n---\nbody";
        assert_eq!(
            parse_agent_definition(md),
            Some(("codebase-locator".to_owned(), Capability::ReadOnly))
        );
        let editor = "---\nname: My-Refactorer\ntools: Read, Edit, Write\n---\n";
        assert_eq!(
            parse_agent_definition(editor),
            Some(("my-refactorer".to_owned(), Capability::Mutating))
        );
        assert_eq!(parse_agent_definition("no frontmatter"), None);
    }

    #[test]
    fn builtins_classified_without_files() {
        let index = load_agent_definitions(&[]);
        assert_eq!(capability_of("Explore", &index), Capability::ReadOnly);
        assert_eq!(
            capability_of("general-purpose", &index),
            Capability::Mutating
        );
        assert_eq!(capability_of("never-seen", &index), Capability::Unknown);
    }

    #[test]
    fn duplicate_definitions_let_mutating_win() {
        // P2: a read-only `reviewer` globally and a mutating one project-local
        // must classify as mutating (routing is keyed only by name).
        let base = std::env::temp_dir().join(format!("rl-disc-dup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let global = base.join("global");
        let project = base.join("project");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            global.join("reviewer.md"),
            "---\nname: reviewer\ntools: Read, Grep\n---\n",
        )
        .unwrap();
        std::fs::write(
            project.join("reviewer.md"),
            "---\nname: reviewer\ntools: Read, Edit\n---\n",
        )
        .unwrap();

        let index = load_agent_definitions(&[global.clone(), project.clone()]);
        assert_eq!(capability_of("reviewer", &index), Capability::Mutating);
        // Order-independent: project listed first still resolves to mutating.
        let index = load_agent_definitions(&[project, global]);
        assert_eq!(capability_of("reviewer", &index), Capability::Mutating);
    }

    #[test]
    fn default_target_routes_readonly_and_anchors_local() {
        assert_eq!(
            default_target("Explore", Capability::ReadOnly),
            SubagentTarget::Local
        );
        // codebase-locator is an anchor → local even if its capability is unknown.
        assert_eq!(
            default_target("codebase-locator", Capability::Unknown),
            SubagentTarget::Local
        );
        assert_eq!(
            default_target("general-purpose", Capability::Mutating),
            SubagentTarget::Cloud
        );
        assert_eq!(
            default_target("mystery", Capability::Unknown),
            SubagentTarget::Cloud
        );
    }

    #[test]
    fn subagent_routes_compact_round_trip_drops_default_entries() {
        let home = tmp_home("rt");
        status::write_settings(&home, &json!({ "local_model": { "mode": "recommended" } }))
            .unwrap();

        // A full mapping including a cloud entry (== the cloud default).
        let mut full = SubagentRoutes::default();
        full.routes.insert("Explore".into(), SubagentTarget::Local);
        full.routes
            .insert("general-purpose".into(), SubagentTarget::Cloud);
        full.routes.insert(
            "my-agent".into(),
            SubagentTarget::Endpoint {
                base_url: "http://127.0.0.1:11434/v1".into(),
                model: "llama3.3".into(),
            },
        );
        write_subagent_routes_in_home(&home, &full).unwrap();

        // Read back: the cloud entry (== default) was dropped; non-default kept.
        let read = read_subagent_routes(&home).unwrap();
        assert_eq!(read.default, SubagentTarget::Cloud);
        assert_eq!(read.routes.len(), 2);
        assert!(!read.routes.contains_key("general-purpose"));
        assert_eq!(read.effective("general-purpose"), SubagentTarget::Cloud);
        assert_eq!(read.effective("Explore"), SubagentTarget::Local);

        // The on-disk file is compact, and the sibling key survived.
        let settings = status::read_settings(&home).unwrap();
        assert_eq!(settings["local_model"]["mode"], "recommended");
        assert_eq!(settings["subagent_routes"]["default"]["target"], "cloud");
        assert_eq!(
            settings["subagent_routes"]["routes"]
                .as_object()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn subagent_routes_back_compat_no_default_field_is_cloud() {
        // Legacy files (pre-compaction) have no `default` and list every agent.
        let home = tmp_home("legacy");
        status::write_settings(
            &home,
            &json!({ "subagent_routes": { "schema": 1, "routes": {
                "Explore": { "target": "local" },
                "general-purpose": { "target": "cloud" }
            }}}),
        )
        .unwrap();
        let read = read_subagent_routes(&home).unwrap();
        assert_eq!(read.default, SubagentTarget::Cloud);
        // The redundant cloud entry is dropped on read, leaving only the exception.
        assert_eq!(read.routes.len(), 1);
        assert_eq!(read.effective("Explore"), SubagentTarget::Local);
        assert_eq!(read.effective("general-purpose"), SubagentTarget::Cloud);
    }

    #[test]
    fn subagent_routes_non_cloud_default_keeps_cloud_exceptions() {
        let home = tmp_home("noncloud");
        let mut full = SubagentRoutes {
            default: SubagentTarget::Local,
            routes: BTreeMap::new(),
        };
        full.routes.insert("reviewer".into(), SubagentTarget::Cloud);
        full.routes.insert("Explore".into(), SubagentTarget::Local); // == default
        write_subagent_routes_in_home(&home, &full).unwrap();

        let read = read_subagent_routes(&home).unwrap();
        assert_eq!(read.default, SubagentTarget::Local);
        // Only the cloud exception is stored; the local entry (== default) dropped.
        assert_eq!(read.routes.len(), 1);
        assert_eq!(read.effective("reviewer"), SubagentTarget::Cloud);
        assert_eq!(read.effective("anything-else"), SubagentTarget::Local);
    }

    #[test]
    fn routes_config_omits_cloud_and_emits_endpoints() {
        let mut routes = SubagentRoutes::default();
        routes
            .routes
            .insert("Explore".into(), SubagentTarget::Local);
        routes
            .routes
            .insert("reviewer".into(), SubagentTarget::Cloud);
        routes.routes.insert(
            "analyzer".into(),
            SubagentTarget::Endpoint {
                base_url: "http://localhost:11434/v1".into(),
                model: "llama3.3".into(),
            },
        );
        let config = routes_config_json(&routes);
        let subagents = config["routes"]["subagents"].as_object().unwrap();

        // Cloud agent omitted; local agent uses the bundled endpoint (no model);
        // endpoint agent points at the synthesized endpoint with its model.
        assert!(!subagents.contains_key("reviewer"));
        assert_eq!(subagents["Explore"], json!({ "endpoint": "local" }));
        let analyzer_endpoint = subagents["analyzer"]["endpoint"].as_str().unwrap();
        assert_eq!(subagents["analyzer"]["model"], "llama3.3");

        let endpoints = config["endpoints"].as_array().unwrap();
        assert_eq!(endpoints.len(), 1);
        assert_eq!(endpoints[0]["id"], analyzer_endpoint);
        // Custom endpoints are Anthropic Messages servers (see P2).
        assert_eq!(endpoints[0]["protocol"], "anthropic_messages");
        assert_eq!(endpoints[0]["base_url"], "http://localhost:11434/v1");
    }

    #[test]
    fn generated_config_deserializes_as_local_router_config() {
        // The contract that matters at runtime: the file we materialize must
        // load as the local router's own RouterConfig (field names, the
        // `anthropic_messages` protocol tag, local routes without a model).
        let mut routes = SubagentRoutes::default();
        routes
            .routes
            .insert("Explore".into(), SubagentTarget::Local);
        routes
            .routes
            .insert("reviewer".into(), SubagentTarget::Cloud);
        routes.routes.insert(
            "analyzer".into(),
            SubagentTarget::Endpoint {
                base_url: "http://localhost:11434/v1".into(),
                model: "llama3.3".into(),
            },
        );
        let json = serde_json::to_string(&routes_config_json(&routes)).unwrap();
        let config: rayline_local_router::RouterConfig =
            serde_json::from_str(&json).expect("router must parse the materialized config");

        // Allowlist keys reached the router; cloud agent stayed out.
        assert!(config.routes.subagents.contains_key("Explore"));
        assert!(config.routes.subagents.contains_key("analyzer"));
        assert!(!config.routes.subagents.contains_key("reviewer"));
        // The custom endpoint is defined and reachable by id.
        let endpoint_id = &config.routes.subagents["analyzer"].endpoint;
        assert!(config.endpoints.iter().any(|e| &e.id == endpoint_id));
    }

    #[test]
    fn routes_config_all_cloud_emits_sentinel_not_empty_allowlist() {
        // An empty allowlist would invert to "route everything local" at the
        // proxy (P1). An all-cloud mapping must instead emit the non-matching
        // sentinel so nothing is intercepted, and define no endpoints.
        let mut routes = SubagentRoutes::default();
        routes.routes.insert("a".into(), SubagentTarget::Cloud);
        routes.routes.insert("b".into(), SubagentTarget::Cloud);
        let config = routes_config_json(&routes);
        let subagents = config["routes"]["subagents"].as_object().unwrap();

        assert!(!subagents.is_empty(), "allowlist must not be empty");
        assert!(!subagents.contains_key("a"));
        assert!(!subagents.contains_key("b"));
        assert!(subagents.contains_key(ROUTE_NOTHING_SENTINEL));
        assert!(config.get("endpoints").is_none());
    }

    #[test]
    fn target_menu_offers_retained_endpoint_in_recommended_mode_only() {
        // P3: in Recommended mode a retained custom base_url/model is a real
        // alternative endpoint and must be offered; in Custom mode that same
        // pair IS the bundled `local` target and must be hidden.
        let recommended = tmp_home("menu-rec");
        status::write_settings(
            &recommended,
            &json!({ "local_model": {
                "mode": "recommended",
                "base_url": "http://127.0.0.1:11434",
                "model": "llama3.3",
            }}),
        )
        .unwrap();
        let menu = target_menu(&recommended);
        assert_eq!(
            menu.endpoints,
            vec![("http://127.0.0.1:11434".to_owned(), "llama3.3".to_owned())],
        );

        let custom = tmp_home("menu-custom");
        status::write_settings(
            &custom,
            &json!({ "local_model": {
                "mode": "custom",
                "base_url": "http://127.0.0.1:11434",
                "model": "llama3.3",
            }}),
        )
        .unwrap();
        // The active custom pair is "local", so it is not also a menu endpoint.
        assert!(target_menu(&custom).endpoints.is_empty());
    }

    #[test]
    fn endpoint_id_is_deterministic_and_safe() {
        let a = endpoint_id("http://127.0.0.1:11434/v1");
        let b = endpoint_id("http://127.0.0.1:11434/v1");
        assert_eq!(a, b);
        assert!(a.starts_with("custom-"));
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }

    #[test]
    fn endpoint_id_distinguishes_urls_sharing_a_slug_prefix() {
        // P2: two URLs whose first 40 slug chars are identical must still get
        // distinct ids (the digest suffix), or routes_config_json would merge
        // them onto one base URL and misroute the second.
        let prefix = "a".repeat(60);
        let one = endpoint_id(&format!("http://{prefix}.example.com/v1"));
        let two = endpoint_id(&format!("http://{prefix}.other.com/v1"));
        assert_ne!(one, two);

        // And those distinct ids yield two endpoints (not a silent merge).
        let mut routes = SubagentRoutes::default();
        routes.routes.insert(
            "a".into(),
            SubagentTarget::Endpoint {
                base_url: format!("http://{prefix}.example.com/v1"),
                model: "m1".into(),
            },
        );
        routes.routes.insert(
            "b".into(),
            SubagentTarget::Endpoint {
                base_url: format!("http://{prefix}.other.com/v1"),
                model: "m2".into(),
            },
        );
        let config = routes_config_json(&routes);
        assert_eq!(config["endpoints"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn cycle_toggles_cloud_local_without_endpoints() {
        let menu = TargetMenu::default();
        assert_eq!(menu.cycle(&SubagentTarget::Cloud), SubagentTarget::Local);
        assert_eq!(menu.cycle(&SubagentTarget::Local), SubagentTarget::Cloud);
    }

    #[test]
    fn cycle_includes_endpoints_when_present() {
        let menu = TargetMenu {
            endpoints: vec![("http://x/v1".into(), "m1".into())],
        };
        let endpoint = SubagentTarget::Endpoint {
            base_url: "http://x/v1".into(),
            model: "m1".into(),
        };
        assert_eq!(menu.cycle(&SubagentTarget::Cloud), SubagentTarget::Local);
        assert_eq!(menu.cycle(&SubagentTarget::Local), endpoint.clone());
        assert_eq!(menu.cycle(&endpoint), SubagentTarget::Cloud);
    }

    fn row(agent: &str, target: SubagentTarget) -> Row {
        row_cap(agent, target, Capability::Unknown)
    }

    fn row_cap(agent: &str, target: SubagentTarget, capability: Capability) -> Row {
        Row {
            agent_type: agent.to_owned(),
            uses: 1,
            capability,
            target,
        }
    }

    #[test]
    fn map_key_addresses_hex_rows_and_commands() {
        assert_eq!(map_key('0'), PickerKey::Row(0));
        assert_eq!(map_key('9'), PickerKey::Row(9));
        assert_eq!(map_key('a'), PickerKey::Row(10));
        assert_eq!(map_key('f'), PickerKey::Row(15));
        assert_eq!(map_key('n'), PickerKey::NextPage);
        assert_eq!(map_key('p'), PickerKey::PrevPage);
        assert_eq!(map_key('k'), PickerKey::CycleFilter);
        assert_eq!(map_key('L'), PickerKey::AllLocal);
        assert_eq!(map_key('C'), PickerKey::AllCloud);
        assert_eq!(map_key('/'), PickerKey::Search);
        assert_eq!(map_key('s'), PickerKey::Save);
        assert_eq!(map_key('q'), PickerKey::Quit);
        assert_eq!(map_key('z'), PickerKey::Ignore);
    }

    #[test]
    fn picker_cycles_rows_bulk_and_signals() {
        let menu = TargetMenu::default();
        let mut state = PickerState::new(
            vec![
                row("a", SubagentTarget::Cloud),
                row("b", SubagentTarget::Local),
            ],
            SubagentTarget::Cloud,
        );
        // Save / Quit / Search signals.
        assert_eq!(state.apply_key(&menu, PickerKey::Save), PickerAction::Save);
        assert_eq!(state.apply_key(&menu, PickerKey::Quit), PickerAction::Quit);
        assert_eq!(
            state.apply_key(&menu, PickerKey::Search),
            PickerAction::Search
        );
        // Hex slot cycles that row (cloud⇄local without endpoints).
        assert_eq!(
            state.apply_key(&menu, PickerKey::Row(0)),
            PickerAction::Continue
        );
        assert_eq!(state.rows[0].target, SubagentTarget::Local);
        // A slot past the page is a no-op, not a panic.
        assert_eq!(
            state.apply_key(&menu, PickerKey::Row(9)),
            PickerAction::Continue
        );
        // Bulk over the (unfiltered) view.
        state.apply_key(&menu, PickerKey::AllCloud);
        assert!(state.rows.iter().all(|r| r.target == SubagentTarget::Cloud));
        state.apply_key(&menu, PickerKey::AllLocal);
        assert!(state.rows.iter().all(|r| r.target == SubagentTarget::Local));
    }

    #[test]
    fn picker_filter_and_search_scope_the_view() {
        let menu = TargetMenu::default();
        let mut state = PickerState::new(
            vec![
                row_cap(
                    "codebase-locator",
                    SubagentTarget::Local,
                    Capability::ReadOnly,
                ),
                row_cap("refactorer", SubagentTarget::Cloud, Capability::Mutating),
                row_cap(
                    "codebase-analyzer",
                    SubagentTarget::Local,
                    Capability::ReadOnly,
                ),
            ],
            SubagentTarget::Cloud,
        );
        // Kind filter → read-only view is rows 0 and 2; hex slot is page-relative.
        state.apply_key(&menu, PickerKey::CycleFilter);
        assert_eq!(state.filter, KindFilter::ReadOnly);
        assert_eq!(state.view(), vec![0, 2]);
        state.apply_key(&menu, PickerKey::Row(1)); // slot 1 → rows[2]
        assert_eq!(state.rows[2].target, SubagentTarget::Cloud);
        assert_eq!(state.rows[1].target, SubagentTarget::Cloud); // mutating untouched

        // Bulk over the read-only filter spares the mutating row.
        state.apply_key(&menu, PickerKey::AllLocal);
        assert_eq!(state.rows[0].target, SubagentTarget::Local);
        assert_eq!(state.rows[2].target, SubagentTarget::Local);
        assert_eq!(state.rows[1].target, SubagentTarget::Cloud);

        // Name search composes with the kind filter (case-insensitive substring).
        state.set_query("ANALYZER");
        assert_eq!(state.view(), vec![2]);
        // Clearing the query restores the kind-filtered view.
        state.set_query("");
        assert_eq!(state.view(), vec![0, 2]);
    }

    #[test]
    fn picker_pages_at_sixteen_and_clamps() {
        let menu = TargetMenu::default();
        let rows: Vec<Row> = (0..40)
            .map(|i| row(&format!("agent-{i}"), SubagentTarget::Cloud))
            .collect();
        let mut state = PickerState::new(rows, SubagentTarget::Cloud);
        assert_eq!(state.page_count(state.view().len()), 3); // 40 / 16 → 3 pages
        state.apply_key(&menu, PickerKey::NextPage);
        assert_eq!(state.page, 1);
        // Slot 0 on page 1 (PAGE_SIZE=16) addresses rows[16].
        state.apply_key(&menu, PickerKey::Row(0));
        assert_eq!(state.rows[16].target, SubagentTarget::Local);
        state.apply_key(&menu, PickerKey::NextPage);
        state.apply_key(&menu, PickerKey::NextPage); // clamp at last page
        assert_eq!(state.page, 2);
        state.apply_key(&menu, PickerKey::PrevPage);
        assert_eq!(state.page, 1);
        // Changing the filter resets to page 0.
        state.apply_key(&menu, PickerKey::CycleFilter);
        assert_eq!(state.page, 0);
    }

    #[test]
    fn build_rows_filters_tail_but_keeps_anchors_and_decided() {
        let discovered = vec![
            DiscoveredAgent {
                agent_type: "Explore".into(),
                uses: 100,
            },
            DiscoveredAgent {
                agent_type: "general-purpose".into(),
                uses: 50,
            },
            DiscoveredAgent {
                agent_type: "one-off-agent".into(),
                uses: 1,
            },
            DiscoveredAgent {
                agent_type: "decided-rare".into(),
                uses: 1,
            },
        ];
        let index = load_agent_definitions(&[]);
        let mut existing = SubagentRoutes::default();
        existing
            .routes
            .insert("decided-rare".into(), SubagentTarget::Local);

        let rows = build_rows(&discovered, &index, Some(&existing), false);
        let names: Vec<&str> = rows.iter().map(|r| r.agent_type.as_str()).collect();
        // High-use + built-in capability shown; decided agent shown.
        assert!(names.contains(&"Explore"));
        assert!(names.contains(&"general-purpose"));
        assert!(names.contains(&"decided-rare"));
        // The unknown one-off is filtered out.
        assert!(!names.contains(&"one-off-agent"));
        // Anchors not in history still appear (e.g. codebase-locator).
        assert!(names.contains(&"codebase-locator"));

        // include_all keeps the tail too.
        let all = build_rows(&discovered, &index, Some(&existing), true);
        assert!(all.iter().any(|r| r.agent_type == "one-off-agent"));
    }

    #[test]
    fn build_rows_first_run_heuristic_then_follows_configured_default() {
        let discovered = vec![DiscoveredAgent {
            agent_type: "codebase-analyzer".into(), // an anchor → heuristic local
            uses: 5,
        }];
        let index = load_agent_definitions(&[]);
        let target_of = |rows: &[Row]| {
            rows.iter()
                .find(|r| r.agent_type == "codebase-analyzer")
                .unwrap()
                .target
                .clone()
        };

        // First run (no config): the read-only/anchor heuristic seeds local.
        let first = build_rows(&discovered, &index, None, false);
        assert_eq!(target_of(&first), SubagentTarget::Local);

        // Configured with a cloud default and no exception for it: the agent now
        // follows the default (cloud), not the heuristic — predictable + compact.
        let configured = SubagentRoutes::default();
        let after = build_rows(&discovered, &index, Some(&configured), false);
        assert_eq!(target_of(&after), SubagentTarget::Cloud);
    }
}
