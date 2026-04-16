use anyhow::{Context as _, bail};
use futures::{StreamExt as _, channel::mpsc};
use language::Buffer;
use remote::RemoteClient;
use rpc::proto::{self, REMOTE_SERVER_PROJECT_ID};
use std::{collections::VecDeque, path::Path, sync::Arc};
use task::{Shell, shell_to_proto};
use terminal::terminal_settings::TerminalSettings;
use util::{ResultExt, command::new_command, rel_path::RelPath};

use collections::HashMap;
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Subscription, Task, WeakEntity};
use settings::{Settings as _, WorktreeId};

use crate::{
    project_settings::{DirenvSettings, ProjectSettings},
    trusted_worktrees::{PathTrust, TrustedWorktrees, TrustedWorktreesEvent},
    worktree_store::WorktreeStore,
};

struct EnvEntry {
    env: DirectoryEnvironment,
    worktree_id: Option<WorktreeId>,
    update: watch::Sender<Option<HashMap<String, String>>>,
    task: Task<()>,
}

#[derive(Clone)]
pub struct DirectoryEnvironment {
    state: watch::Receiver<Option<HashMap<String, String>>>,
}

impl DirectoryEnvironment {
    pub fn new(rx: watch::Receiver<Option<HashMap<String, String>>>) -> Self {
        Self { state: rx }
    }

    pub fn constant(env: HashMap<String, String>) -> Self {
        Self::new(watch::Receiver::constant(Some(env)))
    }

    pub fn empty() -> Self {
        Self::constant(HashMap::default())
    }

    pub fn get(&self) -> impl Future<Output = HashMap<String, String>> + use<> {
        let mut state = self.state.clone();
        async move {
            if let Some(state) = state.borrow().as_ref() {
                return state.clone();
            }
            state.recv().await.ok().flatten().unwrap_or_default()
        }
    }
}

pub struct ProjectEnvironment {
    cli_environment: Option<HashMap<String, String>>,
    local_environments: HashMap<(Shell, Arc<Path>), EnvEntry>,
    remote_environments: HashMap<(Shell, Arc<Path>), EnvEntry>,
    environment_error_messages: VecDeque<String>,
    environment_error_messages_tx: mpsc::UnboundedSender<String>,
    worktree_store: WeakEntity<WorktreeStore>,
    remote_client: Option<WeakEntity<RemoteClient>>,
    is_remote_project: bool,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

pub enum ProjectEnvironmentEvent {
    ErrorsUpdated,
}

impl EventEmitter<ProjectEnvironmentEvent> for ProjectEnvironment {}

impl ProjectEnvironment {
    pub fn new(
        cli_environment: Option<HashMap<String, String>>,
        worktree_store: WeakEntity<WorktreeStore>,
        remote_client: Option<WeakEntity<RemoteClient>>,
        is_remote_project: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let (tx, mut rx) = mpsc::unbounded();
        let error_messages_tracking = cx.spawn(async move |this, cx| {
            while let Some(message) = rx.next().await {
                this.update(cx, |this, cx| {
                    this.environment_error_messages.push_back(message);
                    cx.emit(ProjectEnvironmentEvent::ErrorsUpdated);
                })
                .ok();
            }
        });

        let mut _subscriptions = Vec::new();
        if let Some(trusted_store) = TrustedWorktrees::try_get_global(cx) {
            let worktree_store = worktree_store.clone();
            _subscriptions.push(cx.subscribe(
                &trusted_store,
                move |project_environment, _, e, cx| {
                    if let TrustedWorktreesEvent::Trusted(
                        trusted_worktree_store,
                        trusted_worktrees,
                    ) = e
                    {
                        if trusted_worktree_store == &worktree_store {
                            for worktree_id in project_environment
                                .local_environments
                                .iter()
                                .filter_map(|(_, env_entry)| env_entry.worktree_id)
                                .collect::<Vec<_>>()
                            {
                                if trusted_worktrees.contains(&PathTrust::Worktree(worktree_id)) {
                                    project_environment
                                        .reload_environment_for_worktree(worktree_id, cx)
                                }
                            }
                        }
                    }
                },
            ));
        };

        Self {
            cli_environment,
            local_environments: Default::default(),
            remote_environments: Default::default(),
            environment_error_messages: Default::default(),
            environment_error_messages_tx: tx,
            worktree_store,
            remote_client,
            is_remote_project,
            _tasks: vec![error_messages_tracking],
            _subscriptions,
        }
    }

    /// Returns the inherited CLI environment, if this project was opened from the Zed CLI.
    pub(crate) fn get_cli_environment(&self) -> Option<HashMap<String, String>> {
        if cfg!(any(test, feature = "test-support")) {
            return Some(HashMap::default());
        }
        if let Some(mut env) = self.cli_environment.clone() {
            set_origin_marker(&mut env, EnvironmentOrigin::Cli);
            Some(env)
        } else {
            None
        }
    }

    pub fn buffer_environment(
        &mut self,
        buffer: &Entity<Buffer>,
        cx: &mut Context<Self>,
    ) -> DirectoryEnvironment {
        if let Some(cli_environment) = self.get_cli_environment() {
            log::debug!("using project environment variables from CLI");
            return DirectoryEnvironment::constant(cli_environment);
        }

        let Some(worktree_id) = buffer.read(cx).file().map(|f| f.worktree_id(cx)) else {
            return DirectoryEnvironment::empty();
        };
        self.worktree_environment(worktree_id, cx)
    }

    fn reload_environment_for_worktree(&mut self, worktree_id: WorktreeId, cx: &mut App) {
        for (abs_path, env) in self.local_environments.iter() {
            if env.worktree_id == Some(worktree_id) {
                cx.spawn(async move {
                    // TODO kb: load env again, copy/share code from local_directory_environment_impl
                    tx.send(reloaded_env).ok();
                })
            }
        })
            }
        }

        for (abs_path, env) in self.remote_environments.iter() {
            if env.worktree_id == Some(worktree_id) {
                cx.spawn(async move {
                    // TODO kb: load env again, copy/share code from remote_directory_environment_impl
                    tx.send(reloaded_env).ok();
                })
            }
        }
    }

    pub fn worktree_environment(
        &mut self,
        worktree_id: WorktreeId,
        cx: &mut App,
    ) -> DirectoryEnvironment {
        let Some(worktree_store) = self.worktree_store.upgrade() else {
            return DirectoryEnvironment::empty();
        };
        let Some(worktree) = worktree_store.read(cx).worktree_for_id(worktree_id, cx) else {
            return DirectoryEnvironment::empty();
        };
        if let Some(cli_environment) = self.get_cli_environment() {
            log::debug!("using project environment variables from CLI");
            return DirectoryEnvironment::constant(cli_environment);
        }

        let worktree = worktree.read(cx);
        let mut abs_path = worktree.abs_path();
        if worktree.is_single_file() {
            let Some(parent) = abs_path.parent() else {
                return DirectoryEnvironment::empty();
            };
            abs_path = parent.into();
        }

        let remote_client = self.remote_client.as_ref().and_then(|it| it.upgrade());
        match remote_client {
            Some(remote_client) => remote_client.clone().read(cx).shell().map(|shell| {
                self.remote_directory_environment_impl(
                    &Shell::Program(shell),
                    abs_path,
                    Some(worktree_id),
                    remote_client,
                    cx,
                )
            }),
            None if self.is_remote_project => Some(self.local_directory_environment_impl(
                &Shell::System,
                abs_path,
                Some(worktree_id),
                cx,
            )),
            None => Some({
                let shell = TerminalSettings::get(
                    Some(settings::SettingsLocation {
                        worktree_id: worktree.id(),
                        path: RelPath::empty(),
                    }),
                    cx,
                )
                .shell
                .clone();

                self.local_directory_environment_impl(&shell, abs_path, Some(worktree_id), cx)
            }),
        }
        .unwrap_or_else(|| DirectoryEnvironment::empty())
    }

    pub fn directory_environment(
        &mut self,
        abs_path: Arc<Path>,
        cx: &mut App,
    ) -> DirectoryEnvironment {
        let remote_client = self.remote_client.as_ref().and_then(|it| it.upgrade());
        match remote_client {
            Some(remote_client) => remote_client.clone().read(cx).shell().map(|shell| {
                self.remote_directory_environment(
                    &Shell::Program(shell),
                    abs_path,
                    remote_client,
                    cx,
                )
            }),
            None if self.is_remote_project => {
                Some(self.local_directory_environment(&Shell::System, abs_path, cx))
            }
            None => self
                .worktree_store
                .read_with(cx, |worktree_store, cx| {
                    worktree_store.find_worktree(&abs_path, cx)
                })
                .ok()
                .map(|worktree| {
                    let shell = terminal::terminal_settings::TerminalSettings::get(
                        worktree
                            .as_ref()
                            .map(|(worktree, path)| settings::SettingsLocation {
                                worktree_id: worktree.read(cx).id(),
                                path: &path,
                            }),
                        cx,
                    )
                    .shell
                    .clone();

                    self.local_directory_environment_impl(
                        &shell,
                        abs_path,
                        worktree
                            .as_ref()
                            .map(|(worktree, _)| worktree.read(cx).id()),
                        cx,
                    )
                }),
        }
        .unwrap_or_else(|| DirectoryEnvironment::empty())
    }

    /// Returns the project environment using the default worktree path.
    /// This ensures that project-specific environment variables (e.g. from `.envrc`)
    /// are loaded from the project directory rather than the home directory.
    pub fn default_environment(&mut self, cx: &mut App) -> DirectoryEnvironment {
        let abs_path = self
            .worktree_store
            .read_with(cx, |worktree_store, cx| {
                crate::Project::default_visible_worktree_paths(worktree_store, cx)
                    .into_iter()
                    .next()
            })
            .ok()
            .flatten()
            .map(|path| Arc::<Path>::from(path))
            .unwrap_or_else(|| paths::home_dir().as_path().into());
        self.local_directory_environment(&Shell::System, abs_path, cx)
    }

    /// Returns the project environment, if possible.
    /// If the project was opened from the CLI, then the inherited CLI environment is returned.
    /// If it wasn't opened from the CLI, and an absolute path is given, then a shell is spawned in
    /// that directory, to get environment variables as if the user has `cd`'d there.
    pub fn local_directory_environment(
        &mut self,
        shell: &Shell,
        abs_path: Arc<Path>,
        cx: &mut App,
    ) -> DirectoryEnvironment {
        let worktree_id = self
            .worktree_store
            .update(cx, |store, cx| {
                Some(store.find_worktree(&abs_path, cx)?.0.read(cx).id())
            })
            .ok()
            .flatten();
        self.local_directory_environment_impl(shell, abs_path, worktree_id, cx)
    }

    /// Returns the project environment, if possible.
    /// If the project was opened from the CLI, then the inherited CLI environment is returned.
    /// If it wasn't opened from the CLI, and an absolute path is given, then a shell is spawned in
    /// that directory, to get environment variables as if the user has `cd`'d there.
    fn local_directory_environment_impl(
        &mut self,
        shell: &Shell,
        abs_path: Arc<Path>,
        worktree_id: Option<WorktreeId>,
        cx: &mut App,
    ) -> DirectoryEnvironment {
        if let Some(cli_environment) = self.get_cli_environment() {
            log::debug!("using project environment variables from CLI");
            return DirectoryEnvironment::constant(cli_environment);
        }

        self.local_environments
            .entry((shell.clone(), abs_path.clone()))
            .or_insert_with(|| {
                let (tx, rx) = watch::channel(None);
                let load_direnv = ProjectSettings::get_global(cx).load_direnv.clone();
                let shell = shell.clone();
                let error_tx = self.environment_error_messages_tx.clone();
                let can_trust = match worktree_id
                    .zip(TrustedWorktrees::try_get_global(cx))
                    .zip(self.worktree_store.upgrade())
                {
                    Some(((worktree_id, trusted_worktrees), worktree_store)) => trusted_worktrees
                        .update(cx, |trusted_worktrees, cx| {
                            trusted_worktrees.can_trust(&worktree_store, worktree_id, cx)
                        }),
                    None => true,
                };
                let task = if can_trust {
                    cx.spawn({
                        let mut tx = tx.clone();
                        async move |cx| {
                            let mut shell_env = match cx
                                .background_spawn(load_directory_shell_environment(
                                    shell,
                                    abs_path.clone(),
                                    load_direnv,
                                    error_tx,
                                ))
                                .await
                            {
                                Ok(shell_env) => Some(shell_env),
                                Err(e) => {
                                    log::error!(
                                        "Failed to load shell environment for directory {abs_path:?}: {e:#}"
                                    );
                                    None
                                }
                            };

                            if let Some(shell_env) = shell_env.as_mut() {
                                let path = shell_env
                                    .get("PATH")
                                    .map(|path| path.as_str())
                                    .unwrap_or_default();
                                log::debug!(
                                    "using project environment variables shell launched in {:?}. PATH={:?}",
                                    abs_path,
                                    path
                                );

                                set_origin_marker(shell_env, EnvironmentOrigin::WorktreeShell);
                            };

                            tx.send(Some(shell_env.unwrap_or_default())).ok();
                        }
                    })
                } else {
                    cx.background_spawn({
                        let mut tx = tx.clone();
                        async move {
                            tx.send(Some(HashMap::default())).ok();
                        }
                    })
                };
                EnvEntry {
                    env: DirectoryEnvironment::new(rx),
                    worktree_id,
                    update: tx,
                    task,
                }
            })
            .env
            .clone()
    }

    pub fn remote_directory_environment(
        &mut self,
        shell: &Shell,
        abs_path: Arc<Path>,
        remote_client: Entity<RemoteClient>,
        cx: &mut App,
    ) -> DirectoryEnvironment {
        let worktree_id = self
            .worktree_store
            .update(cx, |worktree_store, cx| {
                Some(worktree_store.find_worktree(&abs_path, cx)?.0.read(cx).id())
            })
            .ok()
            .flatten();

        self.remote_directory_environment_impl(shell, abs_path, worktree_id, remote_client, cx)
    }
    fn remote_directory_environment_impl(
        &mut self,
        shell: &Shell,
        abs_path: Arc<Path>,
        worktree_id: Option<WorktreeId>,
        remote_client: Entity<RemoteClient>,
        cx: &mut App,
    ) -> DirectoryEnvironment {
        // TODO kb test it nonetheless
        if cfg!(any(test, feature = "test-support")) {
            return DirectoryEnvironment::empty();
        }

        self.remote_environments
            .entry((shell.clone(), abs_path.clone()))
            .or_insert_with(|| {
                let (tx, rx) = watch::channel(None);
                let can_trust = match worktree_id
                    .zip(TrustedWorktrees::try_get_global(cx))
                    .zip(self.worktree_store.upgrade())
                {
                    Some(((worktree_id, trusted_worktrees), worktree_store)) => trusted_worktrees
                        .update(cx, |trusted_worktrees, cx| {
                            trusted_worktrees.can_trust(&worktree_store, worktree_id, cx)
                        }),
                    None => true,
                };
                let task = if can_trust {
                    let response = remote_client.read(cx).proto_client().request(
                        proto::GetDirectoryEnvironment {
                            project_id: REMOTE_SERVER_PROJECT_ID,
                            shell: Some(shell_to_proto(shell.clone())),
                            directory: abs_path.to_string_lossy().to_string(),
                        },
                    );
                    cx.background_spawn({
                        let mut tx = tx.clone();
                        async move {
                            if let Some(env) = response.await.log_err() {
                                tx.send(Some(env.environment.into_iter().collect())).ok();
                            } else {
                                tx.send(Some(HashMap::default())).ok();
                            }
                        }
                    })
                } else {
                    cx.background_spawn({
                        let mut tx = tx.clone();
                        async move {
                            tx.send(Some(HashMap::default())).ok();
                        }
                    })
                };

                EnvEntry {
                    env: DirectoryEnvironment::new(rx),
                    worktree_id,
                    update: tx,
                    task,
                }
            })
            .env
            .clone()
    }

    pub fn peek_environment_error(&self) -> Option<&String> {
        self.environment_error_messages.front()
    }

    pub fn pop_environment_error(&mut self) -> Option<String> {
        self.environment_error_messages.pop_front()
    }
}

fn set_origin_marker(env: &mut HashMap<String, String>, origin: EnvironmentOrigin) {
    env.insert(ZED_ENVIRONMENT_ORIGIN_MARKER.to_string(), origin.into());
}

const ZED_ENVIRONMENT_ORIGIN_MARKER: &str = "ZED_ENVIRONMENT";

enum EnvironmentOrigin {
    Cli,
    WorktreeShell,
}

impl From<EnvironmentOrigin> for String {
    fn from(val: EnvironmentOrigin) -> Self {
        match val {
            EnvironmentOrigin::Cli => "cli".into(),
            EnvironmentOrigin::WorktreeShell => "worktree-shell".into(),
        }
    }
}

async fn load_directory_shell_environment(
    shell: Shell,
    abs_path: Arc<Path>,
    load_direnv: DirenvSettings,
    tx: mpsc::UnboundedSender<String>,
) -> anyhow::Result<HashMap<String, String>> {
    if let DirenvSettings::Disabled = load_direnv {
        return Ok(HashMap::default());
    }

    let meta = smol::fs::metadata(&abs_path).await.with_context(|| {
        tx.unbounded_send(format!("Failed to open {}", abs_path.display()))
            .ok();
        format!("stat {abs_path:?}")
    })?;

    let dir = if meta.is_dir() {
        abs_path.clone()
    } else {
        abs_path
            .parent()
            .with_context(|| {
                tx.unbounded_send(format!("Failed to open {}", abs_path.display()))
                    .ok();
                format!("getting parent of {abs_path:?}")
            })?
            .into()
    };

    let (shell, args) = shell.program_and_args();
    let mut envs = util::shell_env::capture(shell.clone(), args, abs_path)
        .await
        .with_context(|| {
            tx.unbounded_send("Failed to load environment variables".into())
                .ok();
            format!("capturing shell environment with {shell:?}")
        })?;

    if cfg!(target_os = "windows")
        && let Some(path) = envs.remove("Path")
    {
        // windows env vars are case-insensitive, so normalize the path var
        // so we can just assume `PATH` in other places
        envs.insert("PATH".into(), path);
    }
    // If the user selects `Direct` for direnv, it would set an environment
    // variable that later uses to know that it should not run the hook.
    // We would include in `.envs` call so it is okay to run the hook
    // even if direnv direct mode is enabled.
    let direnv_environment = match load_direnv {
        DirenvSettings::ShellHook => None,
        DirenvSettings::Disabled => bail!("direnv integration is disabled"),
        // Note: direnv is not available on Windows, so we skip direnv processing
        // and just return the shell environment
        DirenvSettings::Direct if cfg!(target_os = "windows") => None,
        DirenvSettings::Direct => load_direnv_environment(&envs, &dir)
            .await
            .with_context(|| {
                tx.unbounded_send("Failed to load direnv environment".into())
                    .ok();
                "load direnv environment"
            })
            .log_err(),
    };
    if let Some(direnv_environment) = direnv_environment {
        for (key, value) in direnv_environment {
            if let Some(value) = value {
                envs.insert(key, value);
            } else {
                envs.remove(&key);
            }
        }
    }

    Ok(envs)
}

async fn load_direnv_environment(
    env: &HashMap<String, String>,
    dir: &Path,
) -> anyhow::Result<HashMap<String, Option<String>>> {
    let Some(direnv_path) = which::which("direnv").ok() else {
        return Ok(HashMap::default());
    };

    let args = &["export", "json"];
    let direnv_output = new_command(&direnv_path)
        .args(args)
        .envs(env)
        .env("TERM", "dumb")
        .current_dir(dir)
        .output()
        .await
        .context("running direnv")?;

    if !direnv_output.status.success() {
        bail!(
            "Loading direnv environment failed ({}), stderr: {}",
            direnv_output.status,
            String::from_utf8_lossy(&direnv_output.stderr)
        );
    }

    let output = String::from_utf8_lossy(&direnv_output.stdout);
    if output.is_empty() {
        // direnv outputs nothing when it has no changes to apply to environment variables
        return Ok(HashMap::default());
    }

    serde_json::from_str(&output).context("parsing direnv json")
}
