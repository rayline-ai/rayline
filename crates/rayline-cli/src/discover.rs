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

/// Rows shown per page in the interactive picker.
const PAGE_SIZE: usize = 25;

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

/// The persisted per-subagent routing decisions (agent type → target).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SubagentRoutes {
    pub routes: BTreeMap<String, SubagentTarget>,
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
    let routes_obj = entry.get("routes")?.as_object()?;
    let mut routes = BTreeMap::new();
    for (agent, target) in routes_obj {
        if let Some(target) = SubagentTarget::from_json(target) {
            routes.insert(agent.clone(), target);
        }
    }
    Some(SubagentRoutes { routes })
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
        routes_json.insert(agent.clone(), target.to_json());
    }
    object.insert(
        "subagent_routes".to_owned(),
        json!({
            "schema": SUBAGENT_ROUTES_SCHEMA,
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
/// wasn't discovered, so prior choices stay visible. Existing decisions win over
/// computed defaults.
pub fn build_rows(
    discovered: &[DiscoveredAgent],
    index: &HashMap<String, Capability>,
    existing: &SubagentRoutes,
    include_all: bool,
) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for agent in discovered {
        let capability = capability_of(&agent.agent_type, index);
        let decided = existing.routes.contains_key(&agent.agent_type);
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
        let target = existing
            .routes
            .get(&agent.agent_type)
            .cloned()
            .unwrap_or_else(|| default_target(&agent.agent_type, capability));
        seen.insert(agent.agent_type.clone());
        rows.push(Row {
            agent_type: agent.agent_type.clone(),
            uses: agent.uses,
            capability,
            target,
        });
    }

    // Anchors that never appeared in history still deserve a row (the default
    // read-only allowlist), as does any previously-decided agent.
    let extras = crate::onboarding::LOCAL_DEFAULT_SUBAGENTS
        .iter()
        .map(|anchor| (*anchor).to_owned())
        .chain(existing.routes.keys().cloned());
    for agent in extras {
        if seen.contains(&agent) {
            continue;
        }
        seen.insert(agent.clone());
        let capability = capability_of(&agent, index);
        let target = existing
            .routes
            .get(&agent)
            .cloned()
            .unwrap_or_else(|| default_target(&agent, capability));
        rows.push(Row {
            agent_type: agent,
            uses: 0,
            capability,
            target,
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

/// What an applied input line means for the edit loop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PickerAction {
    Continue,
    Save,
    Quit,
}

/// The editable picker: the full row set plus the current kind filter and page.
/// The filter/page are a *view* over `rows`; saving always uses every row.
pub struct PickerState {
    pub rows: Vec<Row>,
    pub filter: KindFilter,
    pub page: usize,
}

impl PickerState {
    pub fn new(rows: Vec<Row>) -> Self {
        Self {
            rows,
            filter: KindFilter::All,
            page: 0,
        }
    }

    /// Indices into `rows` matching the current filter (the ordered view).
    fn view(&self) -> Vec<usize> {
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, row)| self.filter.matches(row.capability))
            .map(|(index, _)| index)
            .collect()
    }

    fn page_count(&self, view_len: usize) -> usize {
        view_len.div_ceil(PAGE_SIZE).max(1)
    }

    /// Apply one line of picker input. `<numbers>` are 1-based positions within
    /// the current filtered view; `n`/`p` page; `f` cycles the filter; `a`/`c`
    /// bulk-set the *filtered* view to local/cloud.
    pub fn apply(&mut self, menu: &TargetMenu, input: &str) -> Result<PickerAction, String> {
        let trimmed = input.trim();
        let view = self.view();
        match trimmed {
            "" => return Ok(PickerAction::Save),
            "q" | "quit" => return Ok(PickerAction::Quit),
            "n" | "next" => {
                if self.page + 1 < self.page_count(view.len()) {
                    self.page += 1;
                }
                return Ok(PickerAction::Continue);
            }
            "p" | "prev" => {
                self.page = self.page.saturating_sub(1);
                return Ok(PickerAction::Continue);
            }
            "f" | "filter" => {
                self.filter = self.filter.next();
                self.page = 0;
                return Ok(PickerAction::Continue);
            }
            "a" | "all" => {
                for &index in &view {
                    self.rows[index].target = SubagentTarget::Local;
                }
                return Ok(PickerAction::Continue);
            }
            "c" | "cloud" => {
                for &index in &view {
                    self.rows[index].target = SubagentTarget::Cloud;
                }
                return Ok(PickerAction::Continue);
            }
            _ => {}
        }
        let mut positions = Vec::new();
        for token in trimmed.split(|c: char| c.is_whitespace() || c == ',') {
            if token.is_empty() {
                continue;
            }
            let position: usize = token
                .parse()
                .map_err(|_| format!("unknown command or row: {token:?}"))?;
            if position < 1 || position > view.len() {
                return Err(format!("row {position} is out of range (1-{})", view.len()));
            }
            positions.push(position - 1);
        }
        if positions.is_empty() {
            return Err(
                "enter row numbers, `n`/`p` to page, `f` to filter, `a`/`c` for bulk, or Enter to save".to_owned(),
            );
        }
        for position in positions {
            let row = view[position];
            self.rows[row].target = menu.cycle(&self.rows[row].target);
        }
        Ok(PickerAction::Continue)
    }
}

fn rows_to_routes(rows: &[Row]) -> SubagentRoutes {
    SubagentRoutes {
        routes: rows
            .iter()
            .map(|row| (row.agent_type.clone(), row.target.clone()))
            .collect(),
    }
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
    let existing = read_subagent_routes(&home).unwrap_or_default();

    if json {
        print_inventory_json(&discovered, &index, &existing);
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
    let rows = build_rows(&discovered, &index, &existing, include_all);
    if rows.is_empty() {
        eprintln!(
            "No subagents discovered in your history. The read-only default ({} agents) applies under `--local`.",
            crate::onboarding::LOCAL_DEFAULT_SUBAGENTS.len(),
        );
        return Ok(());
    }

    run_picker(&home, &menu, PickerState::new(rows), discovered.len())
}

/// The interactive edit loop: render the current page → read a line → apply →
/// repeat until the user saves or quits.
fn run_picker(
    home: &Path,
    menu: &TargetMenu,
    mut state: PickerState,
    discovered_total: usize,
) -> Result<(), String> {
    let cli = crate::CLI_BIN;
    eprintln!(
        "\nDiscovered {discovered_total} subagent type(s); editing {shown}.\n\
         Read-only agents are pre-selected for local — a bad local read is recoverable, a bad edit isn't.",
        shown = state.rows.len(),
    );
    loop {
        render_state(&state);
        let cycle_hint = if menu.endpoints.is_empty() {
            "local/cloud"
        } else {
            "cloud→local→endpoint"
        };
        eprintln!(
            "Commands: <#> cycle {cycle_hint} · n/p page · f filter (→{next}) · a/c all local/cloud (filtered) · Enter save · q quit",
            next = state.filter.next().label(),
        );
        eprint!("> ");
        io::stderr().flush().ok();

        let mut input = String::new();
        if io::stdin()
            .read_line(&mut input)
            .map_err(|e| e.to_string())?
            == 0
        {
            // EOF — treat as quit without saving.
            eprintln!("\nNo changes saved.");
            return Ok(());
        }
        match state.apply(menu, &input) {
            Ok(PickerAction::Save) => break,
            Ok(PickerAction::Quit) => {
                eprintln!("No changes saved.");
                return Ok(());
            }
            Ok(PickerAction::Continue) => continue,
            Err(message) => eprintln!("  {message}"),
        }
    }

    let routes = rows_to_routes(&state.rows);
    write_subagent_routes_in_home(home, &routes).map_err(|e| e.to_string())?;
    // Refresh the managed router config so the change takes effect next launch.
    crate::onboarding::write_default_local_routes(home).map_err(|e| e.to_string())?;

    let local = state
        .rows
        .iter()
        .filter(|r| !matches!(r.target, SubagentTarget::Cloud))
        .count();
    eprintln!(
        "\nSaved. {local} subagent(s) route to local under `{cli} claude --local`; the rest stay on cloud."
    );
    Ok(())
}

/// Render the current page of the filtered view. Row numbers are 1-based
/// positions within the filtered view, so `<#>` is stable until the filter
/// changes.
fn render_state(state: &PickerState) {
    let view = state.view();
    let total = view.len();
    let pages = state.page_count(total);
    let start = state.page * PAGE_SIZE;
    let end = (start + PAGE_SIZE).min(total);
    eprintln!(
        "\nFilter: {filter}   Page {page}/{pages}   ({shown} of {total} shown)",
        filter = state.filter.label(),
        page = state.page + 1,
        shown = if total == 0 {
            "0".to_owned()
        } else {
            format!("{}–{}", start + 1, end)
        },
    );
    eprintln!(
        "    #  {:<34} {:>7}  {:<10} → target",
        "agent", "uses", "kind"
    );
    if total == 0 {
        eprintln!("  (no agents match this filter — press `f` to change it)");
        return;
    }
    for (offset, &row_index) in view[start..end].iter().enumerate() {
        let row = &state.rows[row_index];
        let uses = if row.uses == 0 {
            "—".to_owned()
        } else {
            row.uses.to_string()
        };
        eprintln!(
            "  {:>3}  {:<34} {:>7}  {:<10} {}",
            start + offset + 1,
            truncate(&row.agent_type, 34),
            uses,
            row.capability.label(),
            row.target.short_label(),
        );
    }
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
    existing: &SubagentRoutes,
) {
    let agents: Vec<Value> = discovered
        .iter()
        .map(|agent| {
            let capability = capability_of(&agent.agent_type, index);
            let target = existing
                .routes
                .get(&agent.agent_type)
                .cloned()
                .unwrap_or_else(|| default_target(&agent.agent_type, capability));
            json!({
                "agent_type": agent.agent_type,
                "uses": agent.uses,
                "capability": capability.label(),
                "target": target.short_label(),
            })
        })
        .collect();
    let payload = json!({ "agents": agents });
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
    let existing = read_subagent_routes(home).unwrap_or_default();
    let menu = target_menu(home);
    let rows = build_rows(&discovered, &index, &existing, false);
    if rows.is_empty() {
        eprintln!("No subagents discovered yet; the read-only default applies.");
        return;
    }
    if let Err(error) = run_picker(home, &menu, PickerState::new(rows), discovered.len()) {
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
    fn subagent_routes_round_trip_preserves_siblings() {
        let home = tmp_home("rt");
        status::write_settings(&home, &json!({ "local_model": { "mode": "recommended" } }))
            .unwrap();

        let mut routes = SubagentRoutes::default();
        routes
            .routes
            .insert("Explore".into(), SubagentTarget::Local);
        routes
            .routes
            .insert("general-purpose".into(), SubagentTarget::Cloud);
        routes.routes.insert(
            "my-agent".into(),
            SubagentTarget::Endpoint {
                base_url: "http://127.0.0.1:11434/v1".into(),
                model: "llama3.3".into(),
            },
        );
        write_subagent_routes_in_home(&home, &routes).unwrap();

        let read = read_subagent_routes(&home).unwrap();
        assert_eq!(read, routes);
        // Sibling key survived the merge-preserving write.
        let settings = status::read_settings(&home).unwrap();
        assert_eq!(settings["local_model"]["mode"], "recommended");
        assert_eq!(
            settings["subagent_routes"]["schema"],
            SUBAGENT_ROUTES_SCHEMA
        );
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
    fn picker_cycles_bulk_and_validates() {
        let menu = TargetMenu::default();
        let mut state = PickerState::new(vec![
            row("a", SubagentTarget::Cloud),
            row("b", SubagentTarget::Local),
        ]);
        // Empty → Save; `q` → Quit.
        assert_eq!(state.apply(&menu, "  "), Ok(PickerAction::Save));
        assert_eq!(state.apply(&menu, "q"), Ok(PickerAction::Quit));
        // Cycle rows 1 and 2 (cloud⇄local without endpoints).
        assert_eq!(state.apply(&menu, "1 2"), Ok(PickerAction::Continue));
        assert_eq!(state.rows[0].target, SubagentTarget::Local);
        assert_eq!(state.rows[1].target, SubagentTarget::Cloud);
        // `c` → all cloud, `a` → all local (over the filtered view).
        assert_eq!(state.apply(&menu, "c"), Ok(PickerAction::Continue));
        assert!(state.rows.iter().all(|r| r.target == SubagentTarget::Cloud));
        assert_eq!(state.apply(&menu, "a"), Ok(PickerAction::Continue));
        assert!(state.rows.iter().all(|r| r.target == SubagentTarget::Local));
        // Out of range / unknown command → Err (re-prompt).
        assert!(state.apply(&menu, "9").is_err());
        assert!(state.apply(&menu, "z").is_err());
    }

    #[test]
    fn picker_filter_scopes_view_numbering_and_bulk() {
        let menu = TargetMenu::default();
        let mut state = PickerState::new(vec![
            row_cap("ro1", SubagentTarget::Local, Capability::ReadOnly),
            row_cap("edit1", SubagentTarget::Cloud, Capability::Mutating),
            row_cap("ro2", SubagentTarget::Local, Capability::ReadOnly),
        ]);
        // `f` cycles All → ReadOnly. The view now holds only ro1, ro2.
        assert_eq!(state.apply(&menu, "f"), Ok(PickerAction::Continue));
        assert_eq!(state.filter, KindFilter::ReadOnly);
        assert_eq!(state.view(), vec![0, 2]);
        // View-relative numbering: "2" is ro2 (rows[2]), not the mutating row.
        assert_eq!(state.apply(&menu, "2"), Ok(PickerAction::Continue));
        assert_eq!(state.rows[2].target, SubagentTarget::Cloud);
        assert_eq!(state.rows[1].target, SubagentTarget::Cloud); // untouched mutating
        // Bulk `a` over the read-only filter leaves the mutating row alone.
        state.rows[2].target = SubagentTarget::Local;
        assert_eq!(state.apply(&menu, "a"), Ok(PickerAction::Continue));
        assert_eq!(state.rows[0].target, SubagentTarget::Local);
        assert_eq!(state.rows[2].target, SubagentTarget::Local);
        assert_eq!(state.rows[1].target, SubagentTarget::Cloud); // still cloud
        // A number beyond the filtered view is rejected.
        assert!(state.apply(&menu, "3").is_err());
    }

    #[test]
    fn picker_paging_advances_and_clamps() {
        let menu = TargetMenu::default();
        let rows: Vec<Row> = (0..60)
            .map(|i| row(&format!("agent-{i}"), SubagentTarget::Cloud))
            .collect();
        let mut state = PickerState::new(rows);
        assert_eq!(state.page_count(state.view().len()), 3); // 60 / 25 → 3 pages
        assert_eq!(state.apply(&menu, "n"), Ok(PickerAction::Continue));
        assert_eq!(state.page, 1);
        assert_eq!(state.apply(&menu, "n"), Ok(PickerAction::Continue));
        assert_eq!(state.page, 2);
        // Clamp at the last page.
        assert_eq!(state.apply(&menu, "n"), Ok(PickerAction::Continue));
        assert_eq!(state.page, 2);
        // `p` walks back and clamps at 0.
        assert_eq!(state.apply(&menu, "p"), Ok(PickerAction::Continue));
        assert_eq!(state.page, 1);
        // Changing the filter resets to page 0.
        state.apply(&menu, "f").unwrap();
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

        let rows = build_rows(&discovered, &index, &existing, false);
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
        let all = build_rows(&discovered, &index, &existing, true);
        assert!(all.iter().any(|r| r.agent_type == "one-off-agent"));
    }
}
