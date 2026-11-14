use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    net::{SocketAddr, TcpStream},
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use extension_host::ExtensionStore;
use fs::Fs;
use futures::StreamExt as _;
use gpui::{
    App, AppContext as _, Context, Entity, EntityId, Global, IntoElement, ParentElement, Render,
    Styled, Subscription, Task, WeakEntity, Window,
};
use project::{
    Project, TaskSourceKind, WorktreeId,
    debugger::{dap_store::DapStoreEvent, session::Session},
};
use serde::Deserialize;
use task::{DebugScenario, RevealStrategy, TaskContext, TaskId, TaskTemplate, TaskVariables, VariableName};
use terminal_view::{TerminalView, terminal_panel::TerminalPanel};
use ui::{ContextMenu, PopoverMenu, Tooltip, prelude::*};
use util::{ResultExt as _, rel_path::RelPath};
use workspace::{Workspace, tasks::ScheduledTaskResult};

pub mod run_configurations_panel;

/// Debounces bursts of file system events (e.g. `git checkout`) into a single re-detection.
const DETECTION_DEBOUNCE: Duration = Duration::from_millis(500);

/// Directory inside an installed extension that holds its run configuration rules.
pub const EXTENSION_RULES_DIR: &str = "run_configurations";

const CUSTOM_GROUP: &str = "Custom";
const TASKS_FILE_NAME: &str = "tasks.json";
const SELECTED_CHOICES_KEY: &str = "run_configurations_selected_choices";
const LAUNCHERS_KEY: &str = "run_configurations_launchers";
/// Debug adapters accept this standard launch argument to run without debugging.
const NO_DEBUG_FIELD: &str = "noDebug";
const DEBUG_PORT_POLL_INTERVAL: Duration = Duration::from_millis(500);
const DEBUG_PORT_CONNECT_TIMEOUT: Duration = Duration::from_millis(200);

/// A rule shipped by an extension in `run_configurations/*.json`. WiSCode itself knows
/// nothing about build tools: every build-tool specific configuration comes from these
/// rules, so support for a toolchain is installed together with its extension.
#[derive(Clone, Debug, Deserialize)]
pub struct RunConfigurationRule {
    pub group: String,
    pub when: RuleCondition,
    pub command: CommandSpec,
    pub tasks: Vec<RuleTask>,
    /// `KEY=VALUE` lines read every time a configuration runs, so that an extension can
    /// change the environment (for example which toolchain to use) from its own settings.
    /// May use `$ROOT`, `$ROOT_KEY` and `$EXTENSION_WORK`.
    #[serde(default)]
    pub env_file: Option<String>,
    #[serde(default)]
    pub choices: Option<ChoiceSpec>,
}

/// Values the user can toggle per worktree (for example build profiles). They are read
/// from the matched file and passed to every configuration of the rules sharing `key`.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct ChoiceSpec {
    pub key: String,
    pub label: String,
    /// Each value comes from one block of the matched file between these two markers…
    pub block: (String, String),
    /// …as the text between these two markers inside that block.
    pub value: (String, String),
    /// The selected values are joined with `separator` and appended as one argument.
    pub arg_prefix: String,
    pub separator: String,
}

impl ChoiceSpec {
    fn extract_values(&self, contents: &str) -> Vec<String> {
        let (block_start, block_end) = &self.block;
        let (value_start, value_end) = &self.value;
        let mut values = Vec::new();
        let mut rest = contents;
        while let Some(start) = rest.find(block_start.as_str()) {
            let after_start = &rest[start + block_start.len()..];
            let Some(end) = after_start.find(block_end.as_str()) else {
                break;
            };
            let block = &after_start[..end];
            if let Some(value) = block
                .split_once(value_start.as_str())
                .and_then(|(_, after)| after.split_once(value_end.as_str()))
                .map(|(value, _)| value.trim().to_string())
                && !value.is_empty()
                && !values.contains(&value)
            {
                values.push(value);
            }
            rest = &after_start[end + block_end.len()..];
        }
        values
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Choices {
    spec: ChoiceSpec,
    values: Vec<String>,
}

impl RunConfiguration {
    /// Identifies a set of choices within one worktree; selections are stored under it.
    fn choices_key(&self) -> Option<String> {
        let choices = self.choices.as_ref()?;
        Some(format!(
            "{}::{}",
            self.worktree_root.to_string_lossy(),
            choices.spec.key
        ))
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct RuleCondition {
    /// Files relative to the worktree root; the first one that exists is used.
    pub files: Vec<String>,
    /// When non-empty, the matched file must contain at least one of these strings.
    #[serde(default)]
    pub contains_any: Vec<String>,
    /// The matched file must contain none of these strings.
    #[serde(default)]
    pub contains_none: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CommandSpec {
    /// Tried in order when the configuration is run; the first available one wins.
    #[serde(default)]
    pub candidates: Vec<CommandCandidate>,
    /// Used as-is when no candidate is available.
    pub fallback: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CommandCandidate {
    /// A file path that may use `$ROOT`, `$EXTENSION_WORK` and `*` path segments.
    #[serde(default)]
    pub path: Option<String>,
    /// A program looked up on `PATH`.
    #[serde(default)]
    pub program: Option<String>,
    #[serde(default)]
    pub os: Option<CandidateOs>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CandidateOs {
    Windows,
    Unix,
}

impl CandidateOs {
    fn matches_current(self) -> bool {
        match self {
            Self::Windows => cfg!(windows),
            Self::Unix => !cfg!(windows),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct RuleTask {
    pub label: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub debug: Option<DebugSpec>,
}

/// How to debug a rule task: the task runs with `args` appended, and once it listens on
/// `port` a debug session of `adapter` starts with `config`.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct DebugSpec {
    #[serde(default)]
    pub args: Vec<String>,
    pub adapter: String,
    pub port: u16,
    pub config: serde_json::Value,
}

#[derive(Clone, Debug)]
pub struct ExtensionRule {
    pub rule: RunConfigurationRule,
    pub extension_work_dir: PathBuf,
}

#[derive(Clone, Debug)]
struct CommandResolution {
    spec: CommandSpec,
    env_file: Option<String>,
    extension_work_dir: PathBuf,
}

/// A file-name-safe identifier of a worktree root. Only lowercase ASCII letters and digits
/// are kept so that extensions computing it from their own view of the path (which may
/// differ in separators, drive letter case or a `\\?\` prefix) get the same value.
pub fn root_key(root: &Path) -> String {
    root.to_string_lossy()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|character| character.to_ascii_lowercase())
        .collect()
}

fn expand_placeholders(text: &str, root: &Path, extension_work_dir: &Path) -> String {
    // `$ROOT_KEY` first, because `$ROOT` is a prefix of it.
    text.replace("$ROOT_KEY", &root_key(root))
        .replace("$ROOT", &root.to_string_lossy())
        .replace("$EXTENSION_WORK", &extension_work_dir.to_string_lossy())
}

async fn load_env_file(
    fs: &dyn Fs,
    root: &Path,
    resolution: &CommandResolution,
) -> Vec<(String, String)> {
    let Some(env_file) = &resolution.env_file else {
        return Vec::new();
    };
    let path = expand_placeholders(env_file, root, &resolution.extension_work_dir);
    let Ok(contents) = fs.load(Path::new(&path)).await else {
        return Vec::new();
    };
    contents
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            let key = key.trim();
            (!key.is_empty() && !key.starts_with('#'))
                .then(|| (key.to_string(), value.trim().to_string()))
        })
        .collect()
}

#[derive(Clone, Debug)]
pub struct RunConfiguration {
    pub group: String,
    pub name: String,
    pub worktree_id: WorktreeId,
    pub worktree_root: PathBuf,
    pub template: TaskTemplate,
    command: Option<CommandResolution>,
    choices: Option<Choices>,
    debug: Option<DebugSpec>,
    /// Set for launchers remembered from a debug session started elsewhere (for example the
    /// gutter); running them starts that debug scenario again.
    scenario: Option<DebugScenario>,
}

impl RunConfiguration {
    fn key(&self) -> String {
        format!(
            "{}::{}::{}",
            self.worktree_id.to_usize(),
            self.group,
            self.name
        )
    }

    fn is_custom(&self) -> bool {
        self.command.is_none() && self.scenario.is_none()
    }

    fn can_debug(&self) -> bool {
        self.debug.is_some() || self.scenario.is_some()
    }

    fn launcher(scenario: DebugScenario, worktree_id: WorktreeId, worktree_root: PathBuf) -> Self {
        RunConfiguration {
            group: scenario.adapter.to_string(),
            name: scenario.label.to_string(),
            worktree_id,
            worktree_root,
            template: TaskTemplate {
                label: scenario.label.to_string(),
                ..TaskTemplate::default()
            },
            command: None,
            choices: None,
            debug: None,
            scenario: Some(scenario),
        }
    }

    fn display_name(&self, show_worktree: bool) -> String {
        let name = if self.command.is_none() {
            self.name.clone()
        } else {
            format!("{}: {}", self.group, self.name)
        };
        match self.worktree_root.file_name().filter(|_| show_worktree) {
            Some(root_name) => format!("{name} ({})", root_name.to_string_lossy()),
            None => name,
        }
    }
}

fn substitute_variables_in_config(config: &mut serde_json::Value, context: &TaskContext) {
    match config {
        serde_json::Value::Object(object) => object
            .values_mut()
            .for_each(|value| substitute_variables_in_config(value, context)),
        serde_json::Value::Array(array) => array
            .iter_mut()
            .for_each(|value| substitute_variables_in_config(value, context)),
        serde_json::Value::String(text) => {
            if let Some(substituted) = task::substitute_variables_in_str(text, context) {
                *text = substituted;
            }
        }
        _ => {}
    }
}

/// Loads the run configuration rules of every installed extension.
pub async fn load_extension_rules(
    fs: &dyn Fs,
    extensions: &[(PathBuf, PathBuf)],
) -> Vec<ExtensionRule> {
    let mut rules = Vec::new();
    for (installed_dir, work_dir) in extensions {
        let rules_dir = installed_dir.join(EXTENSION_RULES_DIR);
        let Ok(mut entries) = fs.read_dir(&rules_dir).await else {
            continue;
        };
        let mut rule_files = Vec::new();
        while let Some(entry) = entries.next().await {
            if let Some(path) = entry.log_err()
                && path
                    .extension()
                    .is_some_and(|extension| extension == "json")
            {
                rule_files.push(path);
            }
        }
        rule_files.sort();
        for rule_file in rule_files {
            let Some(contents) = fs.load(&rule_file).await.log_err() else {
                continue;
            };
            match serde_json::from_str::<Vec<RunConfigurationRule>>(&contents) {
                Ok(file_rules) => rules.extend(file_rules.into_iter().map(|rule| ExtensionRule {
                    rule,
                    extension_work_dir: work_dir.clone(),
                })),
                Err(error) => {
                    log::warn!("invalid run configuration rules in {rule_file:?}: {error}")
                }
            }
        }
    }
    rules
}

/// Evaluates extension rules against the files at the root of a worktree.
pub async fn detect_run_configurations(
    fs: Arc<dyn Fs>,
    worktree_id: WorktreeId,
    root: &Path,
    rules: &[ExtensionRule],
) -> Vec<RunConfiguration> {
    let mut configurations = Vec::new();
    for extension_rule in rules {
        let rule = &extension_rule.rule;
        let mut matched_contents = None;
        for file_name in &rule.when.files {
            if let Ok(contents) = fs.load(&root.join(file_name)).await {
                matched_contents = Some(contents);
                break;
            }
        }
        let Some(contents) = matched_contents else {
            continue;
        };
        if !rule.when.contains_any.is_empty()
            && !rule
                .when
                .contains_any
                .iter()
                .any(|needle| contents.contains(needle.as_str()))
        {
            continue;
        }
        if rule
            .when
            .contains_none
            .iter()
            .any(|needle| contents.contains(needle.as_str()))
        {
            continue;
        }
        let choices = rule.choices.as_ref().and_then(|spec| {
            let values = spec.extract_values(&contents);
            (!values.is_empty()).then(|| Choices {
                spec: spec.clone(),
                values,
            })
        });
        for rule_task in &rule.tasks {
            configurations.push(RunConfiguration {
                choices: choices.clone(),
                debug: rule_task.debug.clone(),
                scenario: None,
                group: rule.group.clone(),
                name: rule_task.label.clone(),
                worktree_id,
                worktree_root: root.to_path_buf(),
                template: TaskTemplate {
                    label: format!("{}: {}", rule.group, rule_task.label),
                    command: rule.command.fallback.clone(),
                    args: rule_task.args.clone(),
                    env: rule_task.env.clone().into_iter().collect(),
                    cwd: Some(root.to_string_lossy().into_owned()),
                    reveal: RevealStrategy::Always,
                    ..TaskTemplate::default()
                },
                command: Some(CommandResolution {
                    spec: rule.command.clone(),
                    env_file: rule.env_file.clone(),
                    extension_work_dir: extension_rule.extension_work_dir.clone(),
                }),
            });
        }
    }
    configurations
}

/// Resolved at run time rather than detection time, because extensions may install
/// a tool (e.g. a build tool distribution) after the configurations were detected.
async fn resolve_command(fs: &dyn Fs, root: &Path, resolution: &CommandResolution) -> String {
    for candidate in &resolution.spec.candidates {
        if candidate
            .os
            .is_some_and(|candidate_os| !candidate_os.matches_current())
        {
            continue;
        }
        if let Some(path) = &candidate.path {
            let pattern = expand_placeholders(path, root, &resolution.extension_work_dir);
            if let Some(found) = first_matching_file(fs, Path::new(&pattern)).await {
                return found.to_string_lossy().into_owned();
            }
        }
        if let Some(program) = &candidate.program
            && which::which(program).is_ok()
        {
            return program.clone();
        }
    }
    resolution.spec.fallback.clone()
}

/// Expands `*` path segments, preferring the lexicographically greatest entry so that
/// the newest of several installed versions is chosen.
async fn first_matching_file(fs: &dyn Fs, pattern: &Path) -> Option<PathBuf> {
    let mut prefixes = vec![PathBuf::new()];
    for component in pattern.components() {
        let mut next_prefixes = Vec::new();
        for prefix in &prefixes {
            if matches!(component, Component::Normal(name) if name == "*") {
                let Ok(mut entries) = fs.read_dir(prefix).await else {
                    continue;
                };
                let mut children = Vec::new();
                while let Some(entry) = entries.next().await {
                    if let Ok(child) = entry {
                        children.push(child);
                    }
                }
                children.sort();
                children.reverse();
                next_prefixes.extend(children);
            } else {
                next_prefixes.push(prefix.join(component));
            }
        }
        prefixes = next_prefixes;
    }
    for path in prefixes {
        if fs.is_file(&path).await {
            return Some(path);
        }
    }
    None
}

struct RunningConfiguration {
    key: String,
    task_id: TaskId,
}

pub struct RunConfigurations {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    configurations: Vec<RunConfiguration>,
    selected: Option<String>,
    running: Vec<RunningConfiguration>,
    watched_file_names: HashSet<String>,
    selected_choices: HashMap<String, BTreeSet<String>>,
    /// Remembered launchers by worktree root path.
    launchers: HashMap<String, Vec<DebugScenario>>,
    worktree_roots: Vec<(WorktreeId, PathBuf)>,
    /// Sessions this view starts for its own configurations must not become launchers.
    own_session_labels: HashSet<SharedString>,
    /// Re-renders when a debug session starts or terminates, since that is what decides whether a
    /// launcher shows as running.
    session_observers: HashMap<EntityId, Subscription>,
    detection_task: Task<()>,
    run_task: Task<()>,
    save_choices_task: Task<()>,
    save_launchers_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

/// One `RunConfigurations` per workspace, shared by the title bar selector and the panel
/// so that both show the same selection and running state.
#[derive(Default)]
struct WorkspaceRunConfigurations(HashMap<EntityId, WeakEntity<RunConfigurations>>);

impl Global for WorkspaceRunConfigurations {}

impl RunConfigurations {
    pub fn for_workspace(workspace: &Workspace, cx: &mut App) -> Entity<Self> {
        let workspace_id = workspace.weak_handle().entity_id();
        let registry = cx.default_global::<WorkspaceRunConfigurations>();
        registry.0.retain(|_, entity| entity.upgrade().is_some());
        if let Some(existing) = registry.0.get(&workspace_id).and_then(WeakEntity::upgrade) {
            return existing;
        }
        let entity = cx.new(|cx| Self::new(workspace, cx));
        cx.default_global::<WorkspaceRunConfigurations>()
            .0
            .insert(workspace_id, entity.downgrade());
        entity
    }

    fn run(&mut self, key: String, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(key);
        cx.notify();
        self.run_selected(window, cx);
    }

    fn debug(&mut self, key: String, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(key);
        cx.notify();
        self.debug_selected(window, cx);
    }

    fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().clone();
        let mut subscriptions = vec![cx.subscribe(&project, |this, _, event, cx| match event {
            project::Event::WorktreeAdded(_) | project::Event::WorktreeRemoved(_) => {
                this.schedule_detection(cx);
            }
            project::Event::WorktreeUpdatedEntries(_, entries) => {
                let touches_watched_file = entries.iter().any(|(path, _, _)| {
                    path.file_name().is_some_and(|file_name| {
                        file_name == TASKS_FILE_NAME || this.watched_file_names.contains(file_name)
                    })
                });
                if touches_watched_file {
                    this.schedule_detection(cx);
                }
            }
            _ => {}
        })];
        if let Some(extension_store) = ExtensionStore::try_global(cx) {
            subscriptions.push(cx.subscribe(&extension_store, |this, _, event, cx| {
                if matches!(event, extension_host::Event::ExtensionsUpdated) {
                    this.schedule_detection(cx);
                }
            }));
        }
        let dap_store = project.read(cx).dap_store();
        subscriptions.push(cx.subscribe(&dap_store, |this, dap_store, event, cx| {
            match event {
                DapStoreEvent::DebugClientStarted(session_id) => {
                    if let Some(session) = dap_store.read(cx).session_by_id(session_id) {
                        this.session_observers.insert(
                            session.entity_id(),
                            cx.observe(&session, |_, _, cx| cx.notify()),
                        );
                    }
                    cx.notify();
                }
                DapStoreEvent::DebugClientShutdown(_) => {
                    let live_sessions = dap_store
                        .read(cx)
                        .sessions()
                        .map(|session| session.entity_id())
                        .collect::<HashSet<_>>();
                    this.session_observers
                        .retain(|session, _| live_sessions.contains(session));
                    cx.notify();
                }
                _ => {}
            }
        }));
        if let Some(workspace_entity) = workspace.weak_handle().upgrade() {
            subscriptions.push(cx.subscribe(&workspace_entity, |this, _, event, cx| {
                if let workspace::Event::DebugSessionRequested {
                    scenario,
                    task_context,
                    worktree_id,
                } = event
                {
                    this.remember_launcher(scenario, task_context, *worktree_id, cx);
                }
            }));
        }
        let mut this = Self {
            workspace: workspace.weak_handle(),
            project,
            configurations: Vec::new(),
            selected: None,
            running: Vec::new(),
            watched_file_names: HashSet::default(),
            selected_choices: HashMap::default(),
            launchers: HashMap::default(),
            worktree_roots: Vec::new(),
            own_session_labels: HashSet::default(),
            session_observers: HashMap::default(),
            detection_task: Task::ready(()),
            run_task: Task::ready(()),
            save_choices_task: Task::ready(()),
            save_launchers_task: Task::ready(()),
            _subscriptions: subscriptions,
        };
        this.load_selected_choices(cx);
        this.load_launchers(cx);
        this.schedule_detection(cx);
        this
    }

    fn add_launcher_configurations(&mut self) {
        self.configurations
            .retain(|configuration| configuration.scenario.is_none());
        for (worktree_id, root) in &self.worktree_roots {
            let Some(scenarios) = self.launchers.get(root.to_string_lossy().as_ref()) else {
                continue;
            };
            for scenario in scenarios {
                self.configurations.push(RunConfiguration::launcher(
                    scenario.clone(),
                    *worktree_id,
                    root.clone(),
                ));
            }
        }
    }

    /// Like IntelliJ's temporary run configurations: whatever was started from the gutter
    /// becomes a launcher in the title bar that can be run or debugged again.
    fn remember_launcher(
        &mut self,
        scenario: &DebugScenario,
        task_context: &TaskContext,
        worktree_id: Option<WorktreeId>,
        cx: &mut Context<Self>,
    ) {
        if self.own_session_labels.remove(&scenario.label) {
            return;
        }
        let Some((worktree_id, root)) = self
            .worktree_roots
            .iter()
            .find(|(id, _)| worktree_id.is_none_or(|worktree_id| *id == worktree_id))
            .cloned()
        else {
            return;
        };
        let mut scenario = scenario.clone();
        substitute_variables_in_config(&mut scenario.config, task_context);
        if let Some(config) = scenario.config.as_object_mut() {
            config.remove(NO_DEBUG_FIELD);
        }
        let scenarios = self
            .launchers
            .entry(root.to_string_lossy().into_owned())
            .or_default();
        scenarios.retain(|existing| {
            existing.label != scenario.label || existing.adapter != scenario.adapter
        });
        scenarios.push(scenario.clone());
        self.add_launcher_configurations();
        self.selected = Some(RunConfiguration::launcher(scenario, worktree_id, root).key());
        self.save_launchers(cx);
        cx.notify();
    }

    fn load_launchers(&mut self, cx: &mut Context<Self>) {
        let kvp = db::kvp::KeyValueStore::global(cx);
        cx.spawn(async move |this, cx| {
            let stored = cx
                .background_spawn(async move { kvp.read_kvp(LAUNCHERS_KEY) })
                .await
                .log_err()
                .flatten()
                .and_then(|json| {
                    serde_json::from_str::<HashMap<String, Vec<DebugScenario>>>(&json).log_err()
                });
            if let Some(stored) = stored {
                this.update(cx, |this, cx| {
                    for (root, scenarios) in stored {
                        this.launchers.entry(root).or_insert(scenarios);
                    }
                    this.add_launcher_configurations();
                    cx.notify();
                })
                .log_err();
            }
        })
        .detach();
    }

    fn save_launchers(&mut self, cx: &mut Context<Self>) {
        let json = match serde_json::to_string(&self.launchers) {
            Ok(json) => json,
            Err(error) => {
                log::error!("failed to serialize run configuration launchers: {error}");
                return;
            }
        };
        let kvp = db::kvp::KeyValueStore::global(cx);
        self.save_launchers_task = cx.background_spawn(async move {
            kvp.write_kvp(LAUNCHERS_KEY.to_string(), json).await.log_err();
        });
    }

    fn start_launcher(
        &mut self,
        configuration: &RunConfiguration,
        debug: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(mut scenario) = configuration.scenario.clone() else {
            return;
        };
        if !debug && let Some(config) = scenario.config.as_object_mut() {
            config.insert(NO_DEBUG_FIELD.to_string(), serde_json::Value::Bool(true));
        }
        let task_context = TaskContext {
            cwd: Some(configuration.worktree_root.clone()),
            ..TaskContext::default()
        };
        let worktree_id = configuration.worktree_id;
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.start_debug_session(
                    scenario,
                    task_context.into(),
                    None,
                    Some(worktree_id),
                    window,
                    cx,
                );
            })
            .log_err();
    }

    fn load_selected_choices(&mut self, cx: &mut Context<Self>) {
        let kvp = db::kvp::KeyValueStore::global(cx);
        cx.spawn(async move |this, cx| {
            let stored = cx
                .background_spawn(async move { kvp.read_kvp(SELECTED_CHOICES_KEY) })
                .await
                .log_err()
                .flatten()
                .and_then(|json| {
                    serde_json::from_str::<HashMap<String, BTreeSet<String>>>(&json).log_err()
                });
            if let Some(stored) = stored {
                this.update(cx, |this, cx| {
                    this.selected_choices = stored;
                    cx.notify();
                })
                .log_err();
            }
        })
        .detach();
    }

    fn is_choice_selected(&self, choices_key: &str, value: &str) -> bool {
        self.selected_choices
            .get(choices_key)
            .is_some_and(|selected| selected.contains(value))
    }

    fn toggle_choice(&mut self, choices_key: String, value: String, cx: &mut Context<Self>) {
        let selected = self.selected_choices.entry(choices_key).or_default();
        if !selected.remove(&value) {
            selected.insert(value);
        }
        self.selected_choices
            .retain(|_, selected| !selected.is_empty());
        let json = serde_json::to_string(&self.selected_choices).log_err();
        let kvp = db::kvp::KeyValueStore::global(cx);
        self.save_choices_task = cx.background_spawn(async move {
            if let Some(json) = json {
                kvp.write_kvp(SELECTED_CHOICES_KEY.to_string(), json)
                    .await
                    .log_err();
            }
        });
        cx.notify();
    }

    /// Only values still present in the build file are passed, so a removed profile that
    /// was selected earlier does not break the build.
    fn choices_argument(&self, configuration: &RunConfiguration) -> Option<String> {
        let choices = configuration.choices.as_ref()?;
        let selected = self.selected_choices.get(&configuration.choices_key()?)?;
        let values = choices
            .values
            .iter()
            .filter(|value| selected.contains(*value))
            .map(String::as_str)
            .collect::<Vec<_>>();
        (!values.is_empty()).then(|| {
            format!(
                "{}{}",
                choices.spec.arg_prefix,
                values.join(&choices.spec.separator)
            )
        })
    }

    fn schedule_detection(&mut self, cx: &mut Context<Self>) {
        let project = self.project.read(cx);
        if !project.is_local() {
            return;
        }
        let fs = project.fs().clone();
        let worktrees = project
            .visible_worktrees(cx)
            .filter_map(|worktree| {
                let worktree = worktree.read(cx);
                worktree
                    .root_dir()
                    .map(|root| (worktree.id(), root.to_path_buf()))
            })
            .collect::<Vec<_>>();
        let custom_tasks = self.custom_task_templates(&worktrees, cx);
        let extensions = ExtensionStore::try_global(cx)
            .map(|extension_store| {
                let extension_store = extension_store.read(cx);
                extension_store
                    .installed_extensions()
                    .keys()
                    .map(|extension_id| {
                        (
                            extension_store.installed_dir.join(extension_id.as_ref()),
                            extension_store
                                .wasm_host
                                .work_dir
                                .join(extension_id.as_ref()),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        self.detection_task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DETECTION_DEBOUNCE).await;
            let rules = load_extension_rules(fs.as_ref(), &extensions).await;
            let watched_file_names = rules
                .iter()
                .flat_map(|extension_rule| extension_rule.rule.when.files.iter())
                .filter_map(|file| {
                    Path::new(file)
                        .file_name()
                        .map(|file_name| file_name.to_string_lossy().into_owned())
                })
                .collect::<HashSet<_>>();
            let mut configurations = Vec::new();
            for (worktree_id, root) in &worktrees {
                configurations.extend(
                    detect_run_configurations(fs.clone(), *worktree_id, root, &rules).await,
                );
            }
            for task in custom_tasks {
                for (worktree_id, root, template) in task.await {
                    configurations.push(RunConfiguration {
                        group: CUSTOM_GROUP.to_string(),
                        name: template.label.clone(),
                        worktree_id,
                        worktree_root: root,
                        template,
                        command: None,
                        choices: None,
                        debug: None,
                        scenario: None,
                    });
                }
            }
            this.update(cx, |this, cx| {
                this.watched_file_names = watched_file_names;
                this.configurations = configurations;
                this.worktree_roots = worktrees;
                this.add_launcher_configurations();
                let selection_still_exists = this.selected.as_ref().is_some_and(|selected| {
                    this.configurations
                        .iter()
                        .any(|configuration| &configuration.key() == selected)
                });
                if !selection_still_exists {
                    this.selected = this.configurations.first().map(RunConfiguration::key);
                }
                cx.notify();
            })
            .log_err();
        });
    }

    fn custom_task_templates(
        &self,
        worktrees: &[(WorktreeId, PathBuf)],
        cx: &App,
    ) -> Vec<Task<Vec<(WorktreeId, PathBuf, TaskTemplate)>>> {
        let Some(inventory) = self.project.read(cx).task_store().read(cx).task_inventory() else {
            return Vec::new();
        };
        worktrees
            .iter()
            .map(|(worktree_id, root)| {
                let worktree_id = *worktree_id;
                let root = root.clone();
                let tasks = inventory
                    .read(cx)
                    .list_tasks(None, None, Some(worktree_id), cx);
                cx.background_spawn(async move {
                    tasks
                        .await
                        .into_iter()
                        .filter(|(kind, _)| matches!(kind, TaskSourceKind::Worktree { .. }))
                        .map(|(_, template)| (worktree_id, root.clone(), template))
                        .collect()
                })
            })
            .collect()
    }

    fn selected_configuration(&self) -> Option<&RunConfiguration> {
        let selected = self.selected.as_ref()?;
        self.configurations
            .iter()
            .find(|configuration| &configuration.key() == selected)
    }

    pub(crate) fn is_running(&self, key: &str, cx: &App) -> bool {
        self.running.iter().any(|running| running.key == key)
            || self
                .configurations
                .iter()
                .find(|configuration| configuration.key() == key)
                .is_some_and(|configuration| !self.debug_sessions(configuration, cx).is_empty())
    }

    /// Launchers and debug configurations start their debug session under the configuration's
    /// label, so that is how a running session is found again to stop it.
    fn debug_sessions(&self, configuration: &RunConfiguration, cx: &App) -> Vec<Entity<Session>> {
        let label = configuration.template.label.as_str();
        let dap_store = self.project.read(cx).dap_store();
        dap_store
            .read(cx)
            .sessions()
            .filter(|session| {
                let session = session.read(cx);
                !session.is_terminated()
                    && session.parent_id(cx).is_none()
                    && session
                        .label()
                        .is_some_and(|session_label| session_label.as_ref() == label)
            })
            .cloned()
            .collect()
    }

    fn run_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.launch_selected(false, window, cx);
    }

    fn debug_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.launch_selected(true, window, cx);
    }

    fn launch_selected(&mut self, debug: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut configuration) = self.selected_configuration().cloned() else {
            return;
        };
        if configuration.scenario.is_some() {
            // The previous run still holds its ports until it has exited, so a rerun waits for it.
            let stopping = self.stop(&configuration.key(), cx);
            self.run_task = cx.spawn_in(window, async move |this, cx| {
                stopping.await;
                this.update_in(cx, |this, window, cx| {
                    this.start_launcher(&configuration, debug, window, cx)
                })
                .log_err();
            });
            return;
        }
        if !debug {
            configuration.debug = None;
        } else if let Some(debug_spec) = &configuration.debug {
            configuration
                .template
                .args
                .extend(debug_spec.args.iter().cloned());
        } else {
            return;
        }
        let stopping = self.stop(&configuration.key(), cx);

        let Some(resolution) = configuration.command.clone() else {
            self.run_task = cx.spawn_in(window, async move |this, cx| {
                stopping.await;
                this.update_in(cx, |this, window, cx| {
                    this.schedule_run(configuration, window, cx)
                })
                .log_err();
            });
            return;
        };
        let mut configuration = configuration;
        if let Some(argument) = self.choices_argument(&configuration) {
            configuration.template.args.push(argument);
        }
        let fs = self.project.read(cx).fs().clone();
        self.run_task = cx.spawn_in(window, async move |this, cx| {
            stopping.await;
            let (command, file_env) = cx
                .background_spawn({
                    let root = configuration.worktree_root.clone();
                    async move {
                        let command = resolve_command(fs.as_ref(), &root, &resolution).await;
                        let file_env = load_env_file(fs.as_ref(), &root, &resolution).await;
                        (command, file_env)
                    }
                })
                .await;
            let mut configuration = configuration;
            configuration.template.command = command;
            // Variables a rule task sets explicitly take precedence over the env file.
            for (key, value) in file_env {
                configuration.template.env.entry(key).or_insert(value);
            }
            this.update_in(cx, |this, window, cx| {
                this.schedule_run(configuration, window, cx)
            })
            .log_err();
        });
    }

    fn schedule_run(
        &mut self,
        configuration: RunConfiguration,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = configuration.key();
        let mut task_variables = TaskVariables::default();
        task_variables.insert(
            VariableName::WorktreeRoot,
            configuration.worktree_root.to_string_lossy().into_owned(),
        );
        let task_context = TaskContext {
            cwd: Some(configuration.worktree_root.clone()),
            task_variables,
            project_env: Default::default(),
        };
        let source_kind = TaskSourceKind::Worktree {
            id: configuration.worktree_id,
            directory_in_worktree: RelPath::empty().into(),
            id_base: "run configuration".into(),
        };
        let debug_task_context = task_context.clone();
        let Some(resolved_task) = configuration
            .template
            .resolve_task(&source_kind.to_id_base(), &task_context)
        else {
            log::warn!(
                "could not resolve run configuration {:?}",
                configuration.template.label
            );
            return;
        };

        let task_id = resolved_task.id.clone();
        self.running.push(RunningConfiguration {
            key,
            task_id: task_id.clone(),
        });
        cx.notify();

        let this = cx.weak_entity();
        let running_task_id = task_id.clone();
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.schedule_resolved_task_with_completion(
                    source_kind,
                    resolved_task,
                    false,
                    move |_: ScheduledTaskResult, cx| {
                        this.update(cx, |this, cx| {
                            this.running.retain(|running| running.task_id != task_id);
                            cx.notify();
                        })
                        .ok();
                    },
                    window,
                    cx,
                );
            })
            .log_err();

        if let Some(debug_spec) = configuration.debug {
            self.attach_when_listening(
                debug_spec,
                configuration.template.label,
                configuration.worktree_id,
                debug_task_context,
                running_task_id,
                window,
                cx,
            );
        }
    }

    /// Debug configurations start the program with a debug agent listening on a port; the
    /// debug session can only attach once that port accepts connections.
    fn attach_when_listening(
        &mut self,
        debug_spec: DebugSpec,
        label: String,
        worktree_id: WorktreeId,
        task_context: TaskContext,
        task_id: TaskId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let address = SocketAddr::from(([127, 0, 0, 1], debug_spec.port));
        cx.spawn_in(window, async move |this, cx| {
            loop {
                let still_running = this
                    .read_with(cx, |this, _| {
                        this.running
                            .iter()
                            .any(|running| running.task_id == task_id)
                    })
                    .unwrap_or(false);
                if !still_running {
                    return;
                }
                let listening = cx
                    .background_spawn(async move {
                        TcpStream::connect_timeout(&address, DEBUG_PORT_CONNECT_TIMEOUT).is_ok()
                    })
                    .await;
                if listening {
                    break;
                }
                cx.background_executor()
                    .timer(DEBUG_PORT_POLL_INTERVAL)
                    .await;
            }
            this.update_in(cx, |this, window, cx| {
                let label = SharedString::from(label);
                this.own_session_labels.insert(label.clone());
                let scenario = DebugScenario {
                    adapter: debug_spec.adapter.into(),
                    label,
                    build: None,
                    config: debug_spec.config,
                    tcp_connection: None,
                };
                this.workspace
                    .update(cx, |workspace, cx| {
                        workspace.start_debug_session(
                            scenario,
                            task_context.into(),
                            None,
                            Some(worktree_id),
                            window,
                            cx,
                        );
                    })
                    .log_err();
            })
            .log_err();
        })
        .detach();
    }

    /// Resolves once the configuration's processes and debug sessions have ended.
    pub(crate) fn stop(&mut self, key: &str, cx: &mut Context<Self>) -> Task<()> {
        let mut stopping = Vec::<Task<()>>::new();
        let sessions = self
            .configurations
            .iter()
            .find(|configuration| configuration.key() == key)
            .map(|configuration| self.debug_sessions(configuration, cx))
            .unwrap_or_default();
        let dap_store = self.project.read(cx).dap_store();
        for session in sessions {
            let session_id = session.read(cx).session_id();
            let shutdown =
                dap_store.update(cx, |dap_store, cx| dap_store.shutdown_session(session_id, cx));
            stopping.push(cx.background_spawn(async move {
                shutdown.await.log_err();
            }));
        }

        let task_ids = self
            .running
            .iter()
            .filter(|running| running.key == key)
            .map(|running| running.task_id.clone())
            .collect::<Vec<_>>();
        if !task_ids.is_empty()
            && let Some(workspace) = self.workspace.upgrade()
        {
            let workspace = workspace.read(cx);
            let mut terminal_views = workspace
                .items_of_type::<TerminalView>(cx)
                .collect::<Vec<_>>();
            if let Some(terminal_panel) = workspace.panel::<TerminalPanel>(cx) {
                for pane in terminal_panel.read(cx).panes() {
                    terminal_views.extend(pane.read(cx).items_of_type::<TerminalView>());
                }
            }
            for terminal_view in terminal_views {
                let terminal = terminal_view.read(cx).terminal().clone();
                let is_ours = terminal
                    .read(cx)
                    .task()
                    .is_some_and(|task| task_ids.contains(&task.spawned_task.id));
                if is_ours {
                    let completed = terminal.read(cx).wait_for_completed_task(cx);
                    terminal.update(cx, |terminal, _| terminal.kill_active_task());
                    stopping.push(cx.background_spawn(async move {
                        completed.await;
                    }));
                }
            }
        }
        self.running.retain(|running| running.key != key);
        cx.notify();
        cx.background_spawn(async move {
            futures::future::join_all(stopping).await;
        })
    }

    fn render_menu(&self, window: &mut Window, cx: &mut Context<Self>) -> Entity<ContextMenu> {
        let this = cx.weak_entity();
        let configurations = self.configurations.clone();
        let selected = self.selected.clone();
        let show_worktree = {
            let mut roots = configurations
                .iter()
                .map(|configuration| configuration.worktree_id)
                .collect::<Vec<_>>();
            roots.dedup();
            roots.len() > 1
        };
        ContextMenu::build(window, cx, move |mut menu, _, _| {
            let mut current_group = None;
            for configuration in &configurations {
                let group = configuration.group.as_str();
                if current_group != Some(group) {
                    if current_group.is_some() {
                        menu = menu.separator();
                    }
                    menu = menu.header(group.to_string());
                    current_group = Some(group);
                }
                let key = configuration.key();
                let is_selected = selected.as_ref() == Some(&key);
                let this = this.clone();
                menu = menu.toggleable_entry(
                    configuration.display_name(show_worktree),
                    is_selected,
                    IconPosition::Start,
                    None,
                    move |_, cx| {
                        this.update(cx, |this, cx| {
                            this.selected = Some(key.clone());
                            cx.notify();
                        })
                        .ok();
                    },
                );
            }
            if !configurations.is_empty() {
                menu = menu.separator();
            }
            menu.action(
                "Edit Configurations…",
                Box::new(wu_actions::OpenProjectTasks),
            )
        })
    }
}

impl Render for RunConfigurations {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.selected_configuration().cloned();
        let is_running = selected
            .as_ref()
            .is_some_and(|configuration| self.is_running(&configuration.key(), cx));
        let label: SharedString = match &selected {
            Some(configuration) => configuration.display_name(false).into(),
            None => "Add Configuration…".into(),
        };
        let selector_icon = selected
            .as_ref()
            .map(|configuration| {
                if configuration.is_custom() {
                    IconName::Settings
                } else {
                    IconName::PlayOutlined
                }
            })
            .unwrap_or(IconName::Settings);
        let menu_entity = cx.entity().downgrade();

        h_flex()
            .gap_0p5()
            .child(
                PopoverMenu::new("run-configurations-menu")
                    .trigger_with_tooltip(
                        Button::new("run-configuration-selector", label)
                            .label_size(LabelSize::Small)
                            .start_icon(
                                Icon::new(selector_icon)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .end_icon(
                                Icon::new(IconName::ChevronDown)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            ),
                        Tooltip::text("Run Configurations"),
                    )
                    .menu(move |window, cx| {
                        menu_entity
                            .update(cx, |this, cx| this.render_menu(window, cx))
                            .ok()
                    }),
            )
            .child(
                IconButton::new(
                    "run-configuration-run",
                    if is_running {
                        IconName::RotateCw
                    } else {
                        IconName::PlayFilled
                    },
                )
                .icon_size(IconSize::Small)
                .icon_color(if selected.is_some() {
                    Color::Success
                } else {
                    Color::Disabled
                })
                .disabled(selected.is_none())
                .tooltip(Tooltip::text(if is_running { "Rerun" } else { "Run" }))
                .on_click(cx.listener(|this, _, window, cx| this.run_selected(window, cx))),
            )
            .when(
                selected
                    .as_ref()
                    .is_some_and(RunConfiguration::can_debug),
                |this| {
                    this.child(
                        IconButton::new("run-configuration-debug", IconName::Debug)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Success)
                            .tooltip(Tooltip::text("Debug"))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.debug_selected(window, cx)),
                            ),
                    )
                },
            )
            .when(is_running, |this| {
                this.child(
                    IconButton::new("run-configuration-stop", IconName::Stop)
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Error)
                        .tooltip(Tooltip::text("Stop"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(key) =
                                this.selected_configuration().map(RunConfiguration::key)
                            {
                                this.stop(&key, cx).detach();
                            }
                        })),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui::TestAppContext;
    use serde_json::json;

    const RULES: &str = r#"[
        {
            "group": "Quarkus",
            "when": { "files": ["pom.xml"], "contains_any": ["quarkus-maven-plugin"] },
            "command": {
                "candidates": [
                    { "path": "$ROOT/mvnw" },
                    { "path": "$EXTENSION_WORK/maven/*/bin/mvn" }
                ],
                "fallback": "mvn"
            },
            "tasks": [{ "label": "dev", "args": ["quarkus:dev"] }],
            "env_file": "$EXTENSION_WORK/env/$ROOT_KEY.env"
        },
        {
            "group": "Gradle",
            "when": { "files": ["build.gradle.kts", "build.gradle"] },
            "command": { "fallback": "gradle" },
            "tasks": [{ "label": "build", "args": ["build"] }]
        }
    ]"#;

    async fn rules(fs: &FakeFs) -> Vec<ExtensionRule> {
        fs.insert_tree(
            "/extensions/installed/java/run_configurations",
            json!({ "java.json": RULES }),
        )
        .await;
        load_extension_rules(
            fs,
            &[(
                PathBuf::from("/extensions/installed/java"),
                PathBuf::from("/extensions/work/java"),
            )],
        )
        .await
    }

    #[gpui::test]
    async fn test_detects_configurations_from_extension_rules(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        let rules = rules(&fs).await;
        fs.insert_tree(
            "/project",
            json!({
                "pom.xml": "<plugin><artifactId>quarkus-maven-plugin</artifactId></plugin>",
            }),
        )
        .await;

        let configurations =
            detect_run_configurations(fs, WorktreeId::from_usize(1), Path::new("/project"), &rules)
                .await;
        let names = configurations
            .iter()
            .map(|configuration| configuration.display_name(false))
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["Quarkus: dev".to_string()]);
    }

    #[gpui::test]
    async fn test_skips_file_containing_excluded_text(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        let mut rules = rules(&fs).await;
        rules[0].rule.when.contains_none = vec!["<packaging>pom</packaging>".to_string()];
        fs.insert_tree(
            "/project",
            json!({
                "pom.xml": "<packaging>pom</packaging><artifactId>quarkus-maven-plugin</artifactId>",
            }),
        )
        .await;

        let configurations =
            detect_run_configurations(fs, WorktreeId::from_usize(1), Path::new("/project"), &rules)
                .await;
        assert!(configurations.is_empty());
    }

    #[test]
    fn test_launcher_config_has_task_variables_substituted() {
        let mut task_variables = TaskVariables::default();
        task_variables.insert(
            VariableName::Custom("java_class_name".into()),
            "Main".to_string(),
        );
        let context = TaskContext {
            task_variables,
            ..TaskContext::default()
        };
        let mut config = json!({
            "request": "launch",
            "mainClass": "com.example.${ZED_CUSTOM_java_class_name}",
            "args": ["${ZED_CUSTOM_java_class_name}"],
        });
        substitute_variables_in_config(&mut config, &context);
        assert_eq!(
            config,
            json!({ "request": "launch", "mainClass": "com.example.Main", "args": ["Main"] })
        );
    }

    #[gpui::test]
    async fn test_no_configurations_without_extension_rules(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", json!({ "pom.xml": "quarkus-maven-plugin" }))
            .await;

        let configurations =
            detect_run_configurations(fs, WorktreeId::from_usize(1), Path::new("/project"), &[])
                .await;
        assert!(configurations.is_empty());
    }

    #[gpui::test]
    async fn test_resolves_command_installed_by_extension(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        let rules = rules(&fs).await;
        fs.insert_tree("/project", json!({ "pom.xml": "quarkus-maven-plugin" }))
            .await;
        fs.insert_tree(
            "/extensions/work/java/maven",
            json!({
                "3.8.1": { "bin": { "mvn": "" } },
                "3.9.9": { "bin": { "mvn": "" } },
            }),
        )
        .await;

        let configurations = detect_run_configurations(
            fs.clone(),
            WorktreeId::from_usize(1),
            Path::new("/project"),
            &rules,
        )
        .await;
        let resolution = configurations
            .first()
            .and_then(|configuration| configuration.command.clone())
            .expect("extension configuration");
        let command = resolve_command(fs.as_ref(), Path::new("/project"), &resolution).await;
        assert_eq!(
            PathBuf::from(command),
            PathBuf::from("/extensions/work/java/maven/3.9.9/bin/mvn")
        );

        fs.insert_file("/project/mvnw", Vec::new()).await;
        let command = resolve_command(fs.as_ref(), Path::new("/project"), &resolution).await;
        assert_eq!(PathBuf::from(command), PathBuf::from("/project/mvnw"));
    }

    #[gpui::test]
    async fn test_reads_env_file_written_by_extension(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        let rules = rules(&fs).await;
        fs.insert_tree("/project", json!({ "pom.xml": "quarkus-maven-plugin" }))
            .await;
        let configurations = detect_run_configurations(
            fs.clone(),
            WorktreeId::from_usize(1),
            Path::new("/project"),
            &rules,
        )
        .await;
        let resolution = configurations
            .first()
            .and_then(|configuration| configuration.command.clone())
            .expect("extension configuration");

        let env = load_env_file(fs.as_ref(), Path::new("/project"), &resolution).await;
        assert!(env.is_empty());

        fs.insert_tree(
            "/extensions/work/java/env",
            serde_json::Value::Object(serde_json::Map::from_iter([(
                format!("{}.env", root_key(Path::new("/project"))),
                json!("# comment\nJAVA_HOME=/jdks/17\n"),
            )])),
        )
        .await;
        let env = load_env_file(fs.as_ref(), Path::new("/project"), &resolution).await;
        assert_eq!(
            env,
            vec![("JAVA_HOME".to_string(), "/jdks/17".to_string())]
        );
    }

    #[test]
    fn test_extracts_choice_values_from_blocks() {
        let spec = ChoiceSpec {
            key: "maven-profiles".to_string(),
            label: "Profiles".to_string(),
            block: ("<profile>".to_string(), "</profile>".to_string()),
            value: ("<id>".to_string(), "</id>".to_string()),
            arg_prefix: "-P".to_string(),
            separator: ",".to_string(),
        };
        let pom = "<profiles>\
            <profile><id>with-user-service</id><modules><module>Common</module></modules></profile>\
            <profile> <id> no-user-service </id></profile>\
            <profile><id>with-user-service</id></profile>\
            </profiles>";
        assert_eq!(
            spec.extract_values(pom),
            vec!["with-user-service".to_string(), "no-user-service".to_string()]
        );
        assert!(spec.extract_values("<project></project>").is_empty());
    }

    #[test]
    fn test_root_key_ignores_path_spelling() {
        assert_eq!(
            root_key(Path::new(r"\\?\D:\Code\Core-Parent")),
            root_key(Path::new("d:/Code/Core-Parent"))
        );
    }
}
