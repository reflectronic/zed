use crate::handle_open_request;
use crate::restorable_workspace_locations;
use anyhow::{Context as _, Result, anyhow};
use cli::{CliRequest, CliResponse, ipc::IpcSender};
use cli::{IpcHandshake, ipc};
use client::{ZedLink, parse_zed_link};
use collections::HashMap;
use db::kvp::KEY_VALUE_STORE;
use editor::Editor;

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use futures::channel::{mpsc, oneshot};
use futures::future;

use futures::{FutureExt, SinkExt, StreamExt};
use git_ui::{file_diff_view::FileDiffView, multi_diff_view::MultiDiffView};
use gpui::{App, AsyncApp, Global, WindowHandle};
use language::Point;
use onboarding::FIRST_OPEN;
use onboarding::show_onboarding_view;
use recent_projects::{RemoteSettings, open_remote_project};
use remote::{
    Interactive, RemoteConnectionOptions, WslConnectionOptions, remote_client::CommandTemplate,
};
use settings::Settings;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use ui::SharedString;
use util::ResultExt;
use util::paths::PathWithPosition;
use workspace::OpenedItems;
use workspace::PathList;
use workspace::item::ItemHandle;
use workspace::{AppState, OpenOptions, SerializedWorkspaceLocation, Workspace};

#[derive(Default, Debug)]
pub struct OpenRequest {
    pub kind: Option<OpenRequestKind>,
    pub open_paths: Vec<String>,
    pub diff_paths: Vec<[String; 2]>,
    pub diff_all: bool,
    pub open_channel_notes: Vec<(u64, Option<String>)>,
    pub join_channel: Option<u64>,
    pub remote_connection: Option<RemoteConnectionOptions>,
}

#[derive(Debug)]
pub enum OpenRequestKind {
    CliConnection((mpsc::Receiver<CliRequest>, IpcSender<CliResponse>)),
    Extension {
        extension_id: String,
    },
    AgentPanel {
        initial_prompt: Option<String>,
    },
    SharedAgentThread {
        session_id: String,
    },
    DockMenuAction {
        index: usize,
    },
    BuiltinJsonSchema {
        schema_path: String,
    },
    Setting {
        /// `None` opens settings without navigating to a specific path.
        setting_path: Option<String>,
    },
    GitClone {
        repo_url: SharedString,
    },
    GitCommit {
        sha: String,
    },
}

impl OpenRequest {
    pub fn parse(request: RawOpenRequest, cx: &App) -> Result<Self> {
        let mut this = Self::default();

        this.diff_paths = request.diff_paths;
        this.diff_all = request.diff_all;
        if let Some(wsl) = request.wsl {
            let (user, distro_name) = if let Some((user, distro)) = wsl.split_once('@') {
                if user.is_empty() {
                    anyhow::bail!("user is empty in wsl argument");
                }
                (Some(user.to_string()), distro.to_string())
            } else {
                (None, wsl)
            };
            this.remote_connection = Some(RemoteConnectionOptions::Wsl(WslConnectionOptions {
                distro_name,
                user,
            }));
        }

        for url in request.urls {
            if let Some(server_name) = url.strip_prefix("zed-cli://") {
                this.kind = Some(OpenRequestKind::CliConnection(connect_to_cli(server_name)?));
            } else if let Some(action_index) = url.strip_prefix("zed-dock-action://") {
                this.kind = Some(OpenRequestKind::DockMenuAction {
                    index: action_index.parse()?,
                });
            } else if let Some(file) = url.strip_prefix("file://") {
                this.parse_file_path(file)
            } else if let Some(file) = url.strip_prefix("zed://file") {
                this.parse_file_path(file)
            } else if let Some(file) = url.strip_prefix("zed://ssh") {
                let ssh_url = "ssh:/".to_string() + file;
                this.parse_ssh_file_path(&ssh_url, cx)?
            } else if let Some(extension_id) = url.strip_prefix("zed://extension/") {
                this.kind = Some(OpenRequestKind::Extension {
                    extension_id: extension_id.to_string(),
                });
            } else if let Some(agent_path) = url.strip_prefix("zed://agent") {
                this.parse_agent_url(agent_path)
            } else if let Some(session_id_str) = url.strip_prefix("zed://agent/shared/") {
                if uuid::Uuid::parse_str(session_id_str).is_ok() {
                    this.kind = Some(OpenRequestKind::SharedAgentThread {
                        session_id: session_id_str.to_string(),
                    });
                } else {
                    log::error!("Invalid session ID in URL: {}", session_id_str);
                }
            } else if let Some(schema_path) = url.strip_prefix("zed://schemas/") {
                this.kind = Some(OpenRequestKind::BuiltinJsonSchema {
                    schema_path: schema_path.to_string(),
                });
            } else if url == "zed://settings" || url == "zed://settings/" {
                this.kind = Some(OpenRequestKind::Setting { setting_path: None });
            } else if let Some(setting_path) = url.strip_prefix("zed://settings/") {
                this.kind = Some(OpenRequestKind::Setting {
                    setting_path: Some(setting_path.to_string()),
                });
            } else if let Some(clone_path) = url.strip_prefix("zed://git/clone") {
                this.parse_git_clone_url(clone_path)?
            } else if let Some(commit_path) = url.strip_prefix("zed://git/commit/") {
                this.parse_git_commit_url(commit_path)?
            } else if url.starts_with("ssh://") {
                this.parse_ssh_file_path(&url, cx)?
            } else if let Some(zed_link) = parse_zed_link(&url, cx) {
                match zed_link {
                    ZedLink::Channel { channel_id } => {
                        this.join_channel = Some(channel_id);
                    }
                    ZedLink::ChannelNotes {
                        channel_id,
                        heading,
                    } => {
                        this.open_channel_notes.push((channel_id, heading));
                    }
                }
            } else {
                log::error!("unhandled url: {}", url);
            }
        }

        Ok(this)
    }

    fn parse_file_path(&mut self, file: &str) {
        if let Some(decoded) = urlencoding::decode(file).log_err() {
            self.open_paths.push(decoded.into_owned())
        }
    }

    fn parse_agent_url(&mut self, agent_path: &str) {
        // Format: "" or "?prompt=<text>"
        let initial_prompt = agent_path.strip_prefix('?').and_then(|query| {
            url::form_urlencoded::parse(query.as_bytes())
                .find_map(|(key, value)| (key == "prompt").then_some(value))
                .filter(|s| !s.is_empty())
                .map(|s| s.into_owned())
        });
        self.kind = Some(OpenRequestKind::AgentPanel { initial_prompt });
    }

    fn parse_git_clone_url(&mut self, clone_path: &str) -> Result<()> {
        // Format: /?repo=<url> or ?repo=<url>
        let clone_path = clone_path.strip_prefix('/').unwrap_or(clone_path);

        let query = clone_path
            .strip_prefix('?')
            .context("invalid git clone url: missing query string")?;

        let repo_url = url::form_urlencoded::parse(query.as_bytes())
            .find_map(|(key, value)| (key == "repo").then_some(value))
            .filter(|s| !s.is_empty())
            .context("invalid git clone url: missing repo query parameter")?
            .to_string()
            .into();

        self.kind = Some(OpenRequestKind::GitClone { repo_url });

        Ok(())
    }

    fn parse_git_commit_url(&mut self, commit_path: &str) -> Result<()> {
        // Format: <sha>?repo=<path>
        let (sha, query) = commit_path
            .split_once('?')
            .context("invalid git commit url: missing query string")?;
        anyhow::ensure!(!sha.is_empty(), "invalid git commit url: missing sha");

        let repo = url::form_urlencoded::parse(query.as_bytes())
            .find_map(|(key, value)| (key == "repo").then_some(value))
            .filter(|s| !s.is_empty())
            .context("invalid git commit url: missing repo query parameter")?
            .to_string();

        self.open_paths.push(repo);

        self.kind = Some(OpenRequestKind::GitCommit {
            sha: sha.to_string(),
        });

        Ok(())
    }

    fn parse_ssh_file_path(&mut self, file: &str, cx: &App) -> Result<()> {
        let url = url::Url::parse(file)?;
        let host = url
            .host()
            .with_context(|| format!("missing host in ssh url: {file}"))?
            .to_string();
        let username = Some(url.username().to_string()).filter(|s| !s.is_empty());
        let port = url.port();
        anyhow::ensure!(
            self.open_paths.is_empty(),
            "cannot open both local and ssh paths"
        );
        let mut connection_options =
            RemoteSettings::get_global(cx).connection_options_for(host, port, username);
        if let Some(password) = url.password() {
            connection_options.password = Some(password.to_string());
        }

        let connection_options = RemoteConnectionOptions::Ssh(connection_options);
        if let Some(ssh_connection) = &self.remote_connection {
            anyhow::ensure!(
                *ssh_connection == connection_options,
                "cannot open multiple different remote connections"
            );
        }
        self.remote_connection = Some(connection_options);
        self.parse_file_path(url.path());
        Ok(())
    }
}

#[derive(Clone)]
pub struct OpenListener(UnboundedSender<RawOpenRequest>);

#[derive(Default)]
pub struct RawOpenRequest {
    pub urls: Vec<String>,
    pub diff_paths: Vec<[String; 2]>,
    pub diff_all: bool,
    pub wsl: Option<String>,
}

impl Global for OpenListener {}

impl OpenListener {
    pub fn new() -> (Self, UnboundedReceiver<RawOpenRequest>) {
        let (tx, rx) = mpsc::unbounded();
        (OpenListener(tx), rx)
    }

    pub fn open(&self, request: RawOpenRequest) {
        self.0
            .unbounded_send(request)
            .context("no listener for open requests")
            .log_err();
    }
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub fn listen_for_cli_connections(opener: OpenListener) -> Result<()> {
    use release_channel::RELEASE_CHANNEL_NAME;
    use std::os::unix::net::UnixDatagram;

    let sock_path = paths::data_dir().join(format!("zed-{}.sock", *RELEASE_CHANNEL_NAME));
    // remove the socket if the process listening on it has died
    if let Err(e) = UnixDatagram::unbound()?.connect(&sock_path)
        && e.kind() == std::io::ErrorKind::ConnectionRefused
    {
        std::fs::remove_file(&sock_path)?;
    }
    let listener = UnixDatagram::bind(&sock_path)?;
    thread::spawn(move || {
        let mut buf = [0u8; 1024];
        while let Ok(len) = listener.recv(&mut buf) {
            opener.open(RawOpenRequest {
                urls: vec![String::from_utf8_lossy(&buf[..len]).to_string()],
                ..Default::default()
            });
        }
    });
    Ok(())
}

fn connect_to_cli(
    server_name: &str,
) -> Result<(mpsc::Receiver<CliRequest>, IpcSender<CliResponse>)> {
    let handshake_tx = cli::ipc::IpcSender::<IpcHandshake>::connect(server_name.to_string())
        .context("error connecting to cli")?;
    let (request_tx, request_rx) = ipc::channel::<CliRequest>()?;
    let (response_tx, response_rx) = ipc::channel::<CliResponse>()?;

    handshake_tx
        .send(IpcHandshake {
            requests: request_tx,
            responses: response_rx,
        })
        .context("error sending ipc handshake")?;

    let (mut async_request_tx, async_request_rx) =
        futures::channel::mpsc::channel::<CliRequest>(16);
    thread::spawn(move || {
        while let Ok(cli_request) = request_rx.recv() {
            if smol::block_on(async_request_tx.send(cli_request)).is_err() {
                break;
            }
        }
        anyhow::Ok(())
    });

    Ok((async_request_rx, response_tx))
}

pub async fn open_paths_for_location(
    paths: Vec<PathBuf>,
    diff_paths: &[[String; 2]],
    diff_all: bool,
    location: &SerializedWorkspaceLocation,
    app_state: &Arc<AppState>,
    open_options: &OpenOptions,
    cx: &mut AsyncApp,
) -> Result<OpenedItems> {
    match location {
        SerializedWorkspaceLocation::Local => {
            let fs = app_state.fs.clone();
            let is_dir = |path: PathBuf| {
                let fs = fs.clone();
                async move { fs.is_dir(&path).await }
            };
            open_paths_with_positions(
                paths,
                diff_paths,
                diff_all,
                location,
                open_options,
                is_dir,
                |paths, cx| {
                    let task = Workspace::new_local(
                        paths,
                        app_state.clone(),
                        open_options.replace_window.clone(),
                        open_options.env.clone(),
                        None,
                        cx,
                    );
                    cx.spawn(async move |_| task.await.map(Some))
                },
                cx,
            )
            .await
        }
        SerializedWorkspaceLocation::Remote(connection) => {
            let mut connection = connection.clone();
            if let RemoteConnectionOptions::Ssh(options) = &mut connection {
                cx.update(|cx| {
                    RemoteSettings::get_global(cx).fill_connection_options_from_settings(options)
                });
            }
            let remote_connection = cx.update(|cx| {
                workspace::workspace_windows_for_location(location, cx)
                    .into_iter()
                    .next()
                    .and_then(|window| {
                        let workspace = window.read(cx).ok()?;
                        let remote_client = workspace.project().read(cx).remote_client()?;
                        remote_client.read(cx).connection()
                    })
            });
            let is_dir = move |path: PathBuf| {
                let remote_connection = remote_connection.clone();
                async move {
                    if let Some(conn) = remote_connection {
                        remote_path_is_dir(&conn, &path).await
                    } else {
                        true
                    }
                }
            };
            open_paths_with_positions(
                paths,
                diff_paths,
                diff_all,
                location,
                open_options,
                is_dir,
                |paths, cx| {
                    let connection = connection.clone();
                    let app_state = app_state.clone();
                    let open_options = open_options.clone();
                    cx.spawn(async move |cx| {
                        open_remote_project(connection, paths, app_state, open_options, cx).await
                    })
                },
                cx,
            )
            .await
        }
    }
}

async fn remote_path_is_dir(connection: &Arc<dyn remote::RemoteConnection>, path: &Path) -> bool {
    let command = match connection.build_command(
        Some("test".to_string()),
        &["-d".to_owned(), path.to_string_lossy().to_string()],
        &Default::default(),
        None,
        None,
        Interactive::No,
    ) {
        Ok(CommandTemplate { program, args, env }) => util::command::new_smol_command(program)
            .args(args)
            .envs(env)
            .spawn(),
        Err(_) => return false,
    };
    match command {
        Ok(mut child) => child.status().await.is_ok_and(|status| status.success()),
        Err(_) => false,
    }
}

pub async fn open_paths_with_positions<IsDirFut: Future<Output = bool> + Send>(
    paths: Vec<PathBuf>,
    diff_paths: &[[String; 2]],
    diff_all: bool,
    location: &SerializedWorkspaceLocation,
    open_options: &OpenOptions,
    is_dir: impl Fn(PathBuf) -> IsDirFut + Send + Sync,
    workspace_factory: impl FnOnce(
        Vec<PathBuf>,
        &mut gpui::App,
    ) -> gpui::Task<anyhow::Result<Option<OpenedItems>>>,
    cx: &mut AsyncApp,
) -> Result<OpenedItems> {
    let OpenedItems {
        workspace,
        mut items,
        resolved_paths,
    } = workspace::open_paths_with_workspace_factory(
        paths,
        location,
        open_options,
        is_dir,
        workspace_factory,
        cx,
    )
    .await?;

    let mut caret_positions = HashMap::default();
    for resolved in &resolved_paths {
        if let Some(row) = resolved.row {
            let row = row.saturating_sub(1);
            let col = resolved.column.unwrap_or(0).saturating_sub(1);
            caret_positions.insert(resolved.path.clone(), Point::new(row, col));
        }
    }

    let paths: Vec<PathBuf> = resolved_paths.iter().map(|p| p.path.clone()).collect();

    if diff_all && !diff_paths.is_empty() {
        if let Ok(diff_view) = workspace.update(cx, |workspace, window, cx| {
            MultiDiffView::open(diff_paths.to_vec(), workspace, window, cx)
        }) {
            if let Some(diff_view) = diff_view.await.log_err() {
                items.push(Some(Ok(Box::new(diff_view))));
            }
        }
    } else {
        for diff_pair in diff_paths {
            let old_path = Path::new(&diff_pair[0]).canonicalize()?;
            let new_path = Path::new(&diff_pair[1]).canonicalize()?;
            if let Ok(diff_view) = workspace.update(cx, |workspace, window, cx| {
                FileDiffView::open(old_path, new_path, workspace, window, cx)
            }) {
                if let Some(diff_view) = diff_view.await.log_err() {
                    items.push(Some(Ok(Box::new(diff_view))))
                }
            }
        }
    }

    for (item, path) in items.iter_mut().zip(&paths) {
        if let Some(Err(error)) = item {
            *error = anyhow!("error opening {path:?}: {error}");
            continue;
        }
        let Some(Ok(item)) = item else {
            continue;
        };
        let Some(point) = caret_positions.remove(path) else {
            continue;
        };
        if let Some(active_editor) = item.downcast::<Editor>() {
            workspace
                .update(cx, |_, window, cx| {
                    active_editor.update(cx, |editor, cx| {
                        editor.go_to_singleton_buffer_point(point, window, cx);
                    });
                })
                .log_err();
        }
    }

    Ok(OpenedItems {
        workspace,
        items,
        resolved_paths,
    })
}

pub async fn handle_cli_connection(
    (mut requests, responses): (mpsc::Receiver<CliRequest>, IpcSender<CliResponse>),
    app_state: Arc<AppState>,
    cx: &mut AsyncApp,
) {
    if let Some(request) = requests.next().await {
        match request {
            CliRequest::Open {
                urls,
                paths,
                diff_paths,
                diff_all,
                wait,
                wsl,
                open_new_workspace,
                reuse,
                env,
                user_data_dir: _,
            } => {
                if !urls.is_empty() {
                    cx.update(|cx| {
                        match OpenRequest::parse(
                            RawOpenRequest {
                                urls,
                                diff_paths,
                                diff_all,
                                wsl,
                            },
                            cx,
                        ) {
                            Ok(open_request) => {
                                handle_open_request(open_request, app_state.clone(), cx);
                                responses.send(CliResponse::Exit { status: 0 }).log_err();
                            }
                            Err(e) => {
                                responses
                                    .send(CliResponse::Stderr {
                                        message: format!("{e}"),
                                    })
                                    .log_err();
                                responses.send(CliResponse::Exit { status: 1 }).log_err();
                            }
                        };
                    });
                    return;
                }

                let open_workspace_result = open_workspaces(
                    paths,
                    diff_paths,
                    diff_all,
                    open_new_workspace,
                    reuse,
                    &responses,
                    wait,
                    app_state.clone(),
                    env,
                    cx,
                )
                .await;

                let status = if open_workspace_result.is_err() { 1 } else { 0 };
                responses.send(CliResponse::Exit { status }).log_err();
            }
        }
    }
}

async fn open_workspaces(
    paths: Vec<String>,
    diff_paths: Vec<[String; 2]>,
    diff_all: bool,
    open_new_workspace: Option<bool>,
    reuse: bool,
    responses: &IpcSender<CliResponse>,
    wait: bool,
    app_state: Arc<AppState>,
    env: Option<collections::HashMap<String, String>>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let grouped_locations: Vec<(SerializedWorkspaceLocation, PathList)> =
        if paths.is_empty() && diff_paths.is_empty() {
            if open_new_workspace == Some(true) {
                Vec::new()
            } else {
                // The workspace_id from the database is not used;
                // open_paths will assign a new WorkspaceId when opening the workspace.
                restorable_workspace_locations(cx, &app_state)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(_workspace_id, location, paths)| (location, paths))
                    .collect()
            }
        } else {
            vec![(
                SerializedWorkspaceLocation::Local,
                PathList::new(&paths.into_iter().map(PathBuf::from).collect::<Vec<_>>()),
            )]
        };

    if grouped_locations.is_empty() {
        // If we have no paths to open, show the welcome screen if this is the first launch
        if matches!(KEY_VALUE_STORE.read_kvp(FIRST_OPEN), Ok(None)) {
            cx.update(|cx| show_onboarding_view(app_state, cx).detach());
        }
        // If not the first launch, show an empty window with empty editor
        else {
            cx.update(|cx| {
                let open_options = OpenOptions {
                    env,
                    ..Default::default()
                };
                workspace::open_new(open_options, app_state, cx, |workspace, window, cx| {
                    Editor::new_file(workspace, &Default::default(), window, cx)
                })
                .detach();
            });
        }
        return Ok(());
    }
    // If there are paths to open, open a workspace for each grouping of paths
    let mut errored = false;

    for (location, workspace_paths) in grouped_locations {
        // If reuse flag is passed, open a new workspace in an existing window.
        let (open_new_workspace, replace_window) = if reuse {
            (
                Some(true),
                cx.update(|cx| {
                    workspace::workspace_windows_for_location(&location, cx)
                        .into_iter()
                        .next()
                }),
            )
        } else {
            (open_new_workspace, None)
        };
        let open_options = workspace::OpenOptions {
            open_new_workspace,
            replace_window,
            wait,
            env: env.clone(),
            ..Default::default()
        };

        let paths: Vec<PathBuf> = workspace_paths
            .paths()
            .iter()
            .map(|path| path.to_path_buf())
            .collect();

        let result = open_paths_for_location(
            paths,
            &diff_paths,
            diff_all,
            &location,
            &app_state,
            &open_options,
            cx,
        )
        .await;

        match result {
            Ok(OpenedItems {
                workspace,
                items,
                resolved_paths,
            }) => {
                // Handle CLI wait/responses for both local and remote
                let workspace_failed = handle_cli_items(
                    workspace,
                    items,
                    &resolved_paths,
                    &diff_paths,
                    open_options.wait,
                    responses,
                    &app_state,
                    cx,
                )
                .await;
                if workspace_failed {
                    errored = true;
                }
            }
            Err(error) => {
                responses
                    .send(CliResponse::Stderr {
                        message: format!("error opening: {error}"),
                    })
                    .log_err();
                errored = true;
            }
        }
    }

    anyhow::ensure!(!errored, "failed to open a workspace");

    Ok(())
}

async fn handle_cli_items(
    workspace: WindowHandle<Workspace>,
    items: Vec<Option<Result<Box<dyn ItemHandle>>>>,
    paths_with_position: &[PathWithPosition],
    diff_paths: &[[String; 2]],
    wait: bool,
    responses: &IpcSender<CliResponse>,
    app_state: &Arc<AppState>,
    cx: &mut AsyncApp,
) -> bool {
    let mut errored = false;
    let mut item_release_futures = Vec::new();
    let mut subscriptions = Vec::new();

    // If --wait flag is used with no paths, or a directory, then wait until
    // the entire workspace is closed.
    if wait {
        let mut wait_for_window_close = paths_with_position.is_empty() && diff_paths.is_empty();
        for path_with_position in paths_with_position.iter() {
            if app_state.fs.is_dir(&path_with_position.path).await {
                wait_for_window_close = true;
                break;
            }
        }

        if wait_for_window_close {
            let (release_tx, release_rx) = oneshot::channel();
            item_release_futures.push(release_rx);
            subscriptions.push(workspace.update(cx, |_, _, cx| {
                cx.on_release(move |_, _| {
                    let _ = release_tx.send(());
                })
            }));
        }
    }

    for item in items {
        match item {
            Some(Ok(item)) => {
                if wait {
                    let (release_tx, release_rx) = oneshot::channel();
                    item_release_futures.push(release_rx);
                    subscriptions.push(Ok(cx.update(|cx| {
                        item.on_release(
                            cx,
                            Box::new(move |_| {
                                release_tx.send(()).ok();
                            }),
                        )
                    })));
                }
            }
            Some(Err(err)) => {
                responses
                    .send(CliResponse::Stderr {
                        message: err.to_string(),
                    })
                    .log_err();
                errored = true;
            }
            None => {}
        }
    }

    if wait {
        let wait = async move {
            let _subscriptions = subscriptions;
            let _ = future::try_join_all(item_release_futures).await;
        }
        .fuse();
        futures::pin_mut!(wait);

        let background = cx.background_executor().clone();
        loop {
            // Repeatedly check if CLI is still open to avoid wasting resources
            // waiting for files or workspaces to close.
            let mut timer = background.timer(Duration::from_secs(1)).fuse();
            futures::select_biased! {
                _ = wait => break,
                _ = timer => {
                    if responses.send(CliResponse::Ping).is_err() {
                        break;
                    }
                }
            }
        }
    }

    errored
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zed::tests::init_test;
    use cli::{
        CliResponse,
        ipc::{self},
    };
    use editor::Editor;
    use futures::poll;
    use gpui::{AppContext as _, TestAppContext};
    use language::LineEnding;
    use remote::SshConnectionOptions;
    use rope::Rope;
    use serde_json::json;
    use std::{sync::Arc, task::Poll};
    use util::path;
    use workspace::{AppState, Workspace, find_existing_workspace};

    #[gpui::test]
    fn test_parse_ssh_url(cx: &mut TestAppContext) {
        let _app_state = init_test(cx);
        let request = cx.update(|cx| {
            OpenRequest::parse(
                RawOpenRequest {
                    urls: vec!["ssh://me@localhost:/".into()],
                    ..Default::default()
                },
                cx,
            )
            .unwrap()
        });
        assert_eq!(
            request.remote_connection.unwrap(),
            RemoteConnectionOptions::Ssh(SshConnectionOptions {
                host: "localhost".into(),
                username: Some("me".into()),
                port: None,
                password: None,
                args: None,
                port_forwards: None,
                nickname: None,
                upload_binary_over_ssh: false,
                connection_timeout: None,
            })
        );
        assert_eq!(request.open_paths, vec!["/"]);
    }

    #[gpui::test]
    fn test_parse_git_commit_url(cx: &mut TestAppContext) {
        let _app_state = init_test(cx);

        // Test basic git commit URL
        let request = cx.update(|cx| {
            OpenRequest::parse(
                RawOpenRequest {
                    urls: vec!["zed://git/commit/abc123?repo=path/to/repo".into()],
                    ..Default::default()
                },
                cx,
            )
            .unwrap()
        });

        match request.kind.unwrap() {
            OpenRequestKind::GitCommit { sha } => {
                assert_eq!(sha, "abc123");
            }
            _ => panic!("expected GitCommit variant"),
        }
        // Verify path was added to open_paths for workspace routing
        assert_eq!(request.open_paths, vec!["path/to/repo"]);

        // Test with URL encoded path
        let request = cx.update(|cx| {
            OpenRequest::parse(
                RawOpenRequest {
                    urls: vec!["zed://git/commit/def456?repo=path%20with%20spaces".into()],
                    ..Default::default()
                },
                cx,
            )
            .unwrap()
        });

        match request.kind.unwrap() {
            OpenRequestKind::GitCommit { sha } => {
                assert_eq!(sha, "def456");
            }
            _ => panic!("expected GitCommit variant"),
        }
        assert_eq!(request.open_paths, vec!["path with spaces"]);

        // Test with empty path
        cx.update(|cx| {
            assert!(
                OpenRequest::parse(
                    RawOpenRequest {
                        urls: vec!["zed://git/commit/abc123?repo=".into()],
                        ..Default::default()
                    },
                    cx,
                )
                .unwrap_err()
                .to_string()
                .contains("missing repo")
            );
        });

        // Test error case: missing SHA
        let result = cx.update(|cx| {
            OpenRequest::parse(
                RawOpenRequest {
                    urls: vec!["zed://git/commit/abc123?foo=bar".into()],
                    ..Default::default()
                },
                cx,
            )
        });
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("missing repo query parameter")
        );
    }

    #[gpui::test]
    async fn test_open_workspace_with_directory(cx: &mut TestAppContext) {
        let app_state = init_test(cx);

        app_state
            .fs
            .as_fake()
            .insert_tree(
                path!("/root"),
                json!({
                    "dir1": {
                        "file1.txt": "content1",
                        "file2.txt": "content2",
                    },
                }),
            )
            .await;

        assert_eq!(cx.windows().len(), 0);

        // First open the workspace directory
        open_workspace_file(path!("/root/dir1"), None, app_state.clone(), cx).await;

        assert_eq!(cx.windows().len(), 1);
        let workspace = cx.windows()[0].downcast::<Workspace>().unwrap();
        workspace
            .update(cx, |workspace, _, cx| {
                assert!(workspace.active_item_as::<Editor>(cx).is_none())
            })
            .unwrap();

        // Now open a file inside that workspace
        open_workspace_file(path!("/root/dir1/file1.txt"), None, app_state.clone(), cx).await;

        assert_eq!(cx.windows().len(), 1);
        workspace
            .update(cx, |workspace, _, cx| {
                assert!(workspace.active_item_as::<Editor>(cx).is_some());
            })
            .unwrap();

        // Now open a file inside that workspace, but tell Zed to open a new window
        open_workspace_file(
            path!("/root/dir1/file1.txt"),
            Some(true),
            app_state.clone(),
            cx,
        )
        .await;

        assert_eq!(cx.windows().len(), 2);

        let workspace_2 = cx.windows()[1].downcast::<Workspace>().unwrap();
        workspace_2
            .update(cx, |workspace, _, cx| {
                assert!(workspace.active_item_as::<Editor>(cx).is_some());
                let items = workspace.items(cx).collect::<Vec<_>>();
                assert_eq!(items.len(), 1, "Workspace should have two items");
            })
            .unwrap();
    }

    #[gpui::test]
    async fn test_wait_with_directory_waits_for_window_close(cx: &mut TestAppContext) {
        let app_state = init_test(cx);

        app_state
            .fs
            .as_fake()
            .insert_tree(
                path!("/root"),
                json!({
                    "dir1": {
                        "file1.txt": "content1",
                    },
                }),
            )
            .await;

        let workspace_paths = vec![PathBuf::from(path!("/root/dir1"))];

        let (done_tx, mut done_rx) = futures::channel::oneshot::channel();
        cx.spawn({
            let app_state = app_state.clone();
            move |mut cx| async move {
                let result = open_paths_for_location(
                    workspace_paths,
                    &[],
                    false,
                    &workspace::SerializedWorkspaceLocation::Local,
                    &app_state,
                    &workspace::OpenOptions {
                        wait: true,
                        ..Default::default()
                    },
                    &mut cx,
                )
                .await;
                let _ = done_tx.send(result.is_err());
            }
        })
        .detach();

        cx.background_executor.run_until_parked();
        assert_eq!(cx.windows().len(), 1);
        assert!(matches!(poll!(&mut done_rx), Poll::Pending));

        let window = cx.windows()[0];
        cx.update_window(window, |_, window, _| window.remove_window())
            .unwrap();
        cx.background_executor.run_until_parked();

        let errored = done_rx.await.unwrap();
        assert!(!errored);
    }

    #[gpui::test]
    async fn test_open_workspace_with_nonexistent_files(cx: &mut TestAppContext) {
        let app_state = init_test(cx);

        app_state
            .fs
            .as_fake()
            .insert_tree(path!("/root"), json!({}))
            .await;

        assert_eq!(cx.windows().len(), 0);

        // Test case 1: Open a single file that does not exist yet
        open_workspace_file(path!("/root/file5.txt"), None, app_state.clone(), cx).await;

        assert_eq!(cx.windows().len(), 1);
        let workspace_1 = cx.windows()[0].downcast::<Workspace>().unwrap();
        workspace_1
            .update(cx, |workspace, _, cx| {
                assert!(workspace.active_item_as::<Editor>(cx).is_some())
            })
            .unwrap();

        // Test case 2: Open a single file that does not exist yet,
        // but tell Zed to add it to the current workspace
        open_workspace_file(path!("/root/file6.txt"), Some(false), app_state.clone(), cx).await;

        assert_eq!(cx.windows().len(), 1);
        workspace_1
            .update(cx, |workspace, _, cx| {
                let items = workspace.items(cx).collect::<Vec<_>>();
                assert_eq!(items.len(), 2, "Workspace should have two items");
            })
            .unwrap();

        // Test case 3: Open a single file that does not exist yet,
        // but tell Zed to NOT add it to the current workspace
        open_workspace_file(path!("/root/file7.txt"), Some(true), app_state.clone(), cx).await;

        assert_eq!(cx.windows().len(), 2);
        let workspace_2 = cx.windows()[1].downcast::<Workspace>().unwrap();
        workspace_2
            .update(cx, |workspace, _, cx| {
                let items = workspace.items(cx).collect::<Vec<_>>();
                assert_eq!(items.len(), 1, "Workspace should have two items");
            })
            .unwrap();
    }

    async fn open_workspace_file(
        path: &str,
        open_new_workspace: Option<bool>,
        app_state: Arc<AppState>,
        cx: &TestAppContext,
    ) {
        let workspace_paths = vec![PathBuf::from(path)];

        let result = cx
            .spawn(|mut cx| async move {
                open_paths_for_location(
                    workspace_paths,
                    &[],
                    false,
                    &workspace::SerializedWorkspaceLocation::Local,
                    &app_state,
                    &OpenOptions {
                        open_new_workspace,
                        ..Default::default()
                    },
                    &mut cx,
                )
                .await
            })
            .await;

        assert!(result.is_ok());
    }

    #[gpui::test]
    async fn test_reuse_flag_functionality(cx: &mut TestAppContext) {
        let app_state = init_test(cx);

        let root_dir = if cfg!(windows) { "C:\\root" } else { "/root" };
        let file1_path = if cfg!(windows) {
            "C:\\root\\file1.txt"
        } else {
            "/root/file1.txt"
        };
        let file2_path = if cfg!(windows) {
            "C:\\root\\file2.txt"
        } else {
            "/root/file2.txt"
        };

        app_state.fs.create_dir(Path::new(root_dir)).await.unwrap();
        app_state
            .fs
            .create_file(Path::new(file1_path), Default::default())
            .await
            .unwrap();
        app_state
            .fs
            .save(
                Path::new(file1_path),
                &Rope::from("content1"),
                LineEnding::Unix,
            )
            .await
            .unwrap();
        app_state
            .fs
            .create_file(Path::new(file2_path), Default::default())
            .await
            .unwrap();
        app_state
            .fs
            .save(
                Path::new(file2_path),
                &Rope::from("content2"),
                LineEnding::Unix,
            )
            .await
            .unwrap();

        // First, open a workspace normally
        let workspace_paths = vec![PathBuf::from(file1_path)];

        let _result = cx
            .spawn({
                let app_state = app_state.clone();
                |mut cx| async move {
                    open_paths_for_location(
                        workspace_paths,
                        &[],
                        false,
                        &workspace::SerializedWorkspaceLocation::Local,
                        &app_state,
                        &workspace::OpenOptions::default(),
                        &mut cx,
                    )
                    .await
                }
            })
            .await;

        // Now test the reuse functionality - should replace the existing workspace
        let workspace_paths_reuse = vec![PathBuf::from(file1_path)];
        let fs = app_state.fs.clone();
        let is_dir = |path: PathBuf| {
            let fs = fs.clone();
            async move { fs.is_dir(&path).await }
        };
        let window_to_replace = find_existing_workspace(
            &workspace_paths_reuse,
            &workspace::OpenOptions::default(),
            &workspace::SerializedWorkspaceLocation::Local,
            is_dir,
            &mut cx.to_async(),
        )
        .await
        .0
        .unwrap();

        let result_reuse = cx
            .spawn({
                let app_state = app_state.clone();
                |mut cx| async move {
                    open_paths_for_location(
                        workspace_paths_reuse,
                        &[],
                        false,
                        &workspace::SerializedWorkspaceLocation::Local,
                        &app_state,
                        &workspace::OpenOptions {
                            replace_window: Some(window_to_replace),
                            ..Default::default()
                        },
                        &mut cx,
                    )
                    .await
                }
            })
            .await;

        assert!(result_reuse.is_ok());
    }

    #[gpui::test]
    fn test_parse_git_clone_url(cx: &mut TestAppContext) {
        let _app_state = init_test(cx);

        let request = cx.update(|cx| {
            OpenRequest::parse(
                RawOpenRequest {
                    urls: vec![
                        "zed://git/clone/?repo=https://github.com/zed-industries/zed.git".into(),
                    ],
                    ..Default::default()
                },
                cx,
            )
            .unwrap()
        });

        match request.kind {
            Some(OpenRequestKind::GitClone { repo_url }) => {
                assert_eq!(repo_url, "https://github.com/zed-industries/zed.git");
            }
            _ => panic!("Expected GitClone kind"),
        }
    }

    #[gpui::test]
    fn test_parse_git_clone_url_without_slash(cx: &mut TestAppContext) {
        let _app_state = init_test(cx);

        let request = cx.update(|cx| {
            OpenRequest::parse(
                RawOpenRequest {
                    urls: vec![
                        "zed://git/clone?repo=https://github.com/zed-industries/zed.git".into(),
                    ],
                    ..Default::default()
                },
                cx,
            )
            .unwrap()
        });

        match request.kind {
            Some(OpenRequestKind::GitClone { repo_url }) => {
                assert_eq!(repo_url, "https://github.com/zed-industries/zed.git");
            }
            _ => panic!("Expected GitClone kind"),
        }
    }

    #[gpui::test]
    fn test_parse_git_clone_url_with_encoding(cx: &mut TestAppContext) {
        let _app_state = init_test(cx);

        let request = cx.update(|cx| {
            OpenRequest::parse(
                RawOpenRequest {
                    urls: vec![
                        "zed://git/clone/?repo=https%3A%2F%2Fgithub.com%2Fzed-industries%2Fzed.git"
                            .into(),
                    ],
                    ..Default::default()
                },
                cx,
            )
            .unwrap()
        });

        match request.kind {
            Some(OpenRequestKind::GitClone { repo_url }) => {
                assert_eq!(repo_url, "https://github.com/zed-industries/zed.git");
            }
            _ => panic!("Expected GitClone kind"),
        }
    }
}
