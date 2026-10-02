//! Windows 原生窗口标题的展示投影、跨窗口重名处理和生命周期同步。

use crate::project_window_registry::ProjectWindowRegistry;
use crate::project_windows::ProjectWindows;
use same_file::Handle;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Manager, WebviewWindow};

const APPLICATION_NAME: &str = "Lithe";
const TITLE_SEPARATOR: &str = " – ";
const TITLE_APPLY_TIMEOUT: Duration = Duration::from_secs(5);
const INITIAL_WORKSPACE_ID: &str = "initial-project";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowTitleProject {
    workspace_id: String,
    display_name: String,
    path: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowTitleContext {
    projects: Vec<WindowTitleProject>,
    active_workspace_id: Option<String>,
    file_name: Option<String>,
}

#[derive(Debug, Eq, Hash, PartialEq)]
enum ProjectIdentity {
    Local(Arc<Handle>),
    Other(String),
}

type NativeIdentities = HashMap<(String, String), Arc<Handle>>;

struct ResolvedProject {
    workspace_id: String,
    display_name: String,
    path: String,
    identity: ProjectIdentity,
}

#[derive(Default)]
struct ResolvedContext {
    projects: Vec<ResolvedProject>,
    active_workspace_id: Option<String>,
    file_name: Option<String>,
}

struct WindowProjection {
    context: ResolvedContext,
    provisional: bool,
    applied_title: Option<String>,
}

#[derive(Default)]
struct TitleRegistry {
    windows: BTreeMap<String, WindowProjection>,
}

impl TitleRegistry {
    fn native_identities(&self, label: &str) -> NativeIdentities {
        self.windows
            .get(label)
            .into_iter()
            .flat_map(|window| &window.context.projects)
            .filter_map(|project| match &project.identity {
                ProjectIdentity::Local(identity) => Some((
                    (project.workspace_id.clone(), project.path.clone()),
                    Arc::clone(identity),
                )),
                ProjectIdentity::Other(_) => None,
            })
            .collect()
    }

    fn update(&mut self, label: &str, context: ResolvedContext, provisional: bool) {
        let applied_title = self
            .windows
            .get_mut(label)
            .and_then(|window| window.applied_title.take());
        self.windows.insert(
            label.to_owned(),
            WindowProjection {
                context,
                provisional,
                applied_title,
            },
        );
    }

    fn remove(&mut self, label: &str) {
        self.windows.remove(label);
    }

    fn update_frontend(&mut self, label: &str, context: ResolvedContext) {
        let initial_welcome = context.projects.is_empty()
            && context.active_workspace_id.is_none()
            && context.file_name.is_none();
        if initial_welcome
            && self
                .windows
                .get(label)
                .is_some_and(|window| window.provisional)
        {
            // 前端挂载时先产生欢迎态，打开目标尚未恢复；显式失败清理仍会撤销临时标题。
            return;
        }
        self.update(label, context, false);
    }

    fn update_open_window(
        &mut self,
        label: &str,
        context: ResolvedContext,
        exists: impl FnOnce(&str) -> bool,
    ) -> Result<(), String> {
        if !exists(label) {
            return Err("Window is no longer available".into());
        }
        self.update_frontend(label, context);
        Ok(())
    }

    fn release_pending(&mut self, label: &str) {
        if self
            .windows
            .get(label)
            .is_some_and(|window| window.provisional)
        {
            self.update(label, ResolvedContext::default(), false);
        }
    }

    fn titles(&self) -> BTreeMap<String, String> {
        let mut identities_by_name: HashMap<&str, HashSet<&ProjectIdentity>> = HashMap::new();
        for window in self.windows.values() {
            for project in &window.context.projects {
                identities_by_name
                    .entry(&project.display_name)
                    .or_default()
                    .insert(&project.identity);
            }
        }
        self.windows
            .iter()
            .map(|(label, window)| {
                let project = window.context.active_workspace_id.as_ref().and_then(|id| {
                    window
                        .context
                        .projects
                        .iter()
                        .find(|project| &project.workspace_id == id)
                });
                let mut parts = Vec::new();
                if let Some(project) = project {
                    let duplicated = identities_by_name
                        .get(project.display_name.as_str())
                        .is_some_and(|identities| identities.len() > 1);
                    parts.push(if duplicated {
                        format!("{} [{}]", project.display_name, project.path)
                    } else {
                        project.display_name.clone()
                    });
                }
                if let Some(file_name) = &window.context.file_name {
                    parts.push(file_name.clone());
                }
                parts.push(APPLICATION_NAME.to_owned());
                (label.clone(), parts.join(TITLE_SEPARATOR))
            })
            .collect()
    }

    fn apply(
        &mut self,
        mut set_title: impl FnMut(&str, &str) -> Result<bool, String>,
    ) -> Result<(), String> {
        let mut failures = Vec::new();
        for (label, title) in self.titles() {
            let window = self
                .windows
                .get_mut(&label)
                .expect("Window title projection exists");
            if window.applied_title.as_ref() == Some(&title) {
                continue;
            }
            match set_title(&label, &title) {
                Ok(true) => window.applied_title = Some(title),
                Ok(false) => {}
                Err(error) => failures.push(format!("{label}: {error}")),
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "Could not update window title: {}",
                failures.join("; ")
            ))
        }
    }
}

#[derive(Default)]
pub struct WindowTitles(Mutex<TitleRegistry>);

fn report_error(app: &AppHandle, message: &str) {
    eprintln!("[window-title] {message}");
    if let Some(manager) = app.try_state::<Arc<crate::logging::LogManager>>() {
        manager.emit_json("warn", "window-title".into(), message.into(), None);
    }
}

fn single_line(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() || matches!(character, '\u{2028}' | '\u{2029}') {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .trim()
        .to_owned()
}

fn optional_name(value: Option<String>) -> Option<String> {
    value
        .map(|value| single_line(&value))
        .filter(|value| !value.is_empty())
}

fn resolve_context(context: WindowTitleContext) -> ResolvedContext {
    resolve_context_with_previous(context, &NativeIdentities::default())
}

fn resolve_context_with_previous(
    context: WindowTitleContext,
    previous: &NativeIdentities,
) -> ResolvedContext {
    let projects = context
        .projects
        .into_iter()
        .filter_map(|project| {
            let display_name = single_line(&project.display_name);
            if display_name.is_empty() || project.workspace_id.is_empty() || project.path.is_empty()
            {
                return None;
            }
            let identity = if project.path.contains("://") {
                ProjectIdentity::Other(project.path.clone())
            } else {
                // 暂时失效的目录沿用已验证句柄；首次恢复则仅用展示路径区分，保留仍打开的项目。
                match ProjectWindowRegistry::identity(Path::new(&project.path)) {
                    Ok(identity) => ProjectIdentity::Local(Arc::new(identity)),
                    Err(error) => {
                        eprintln!("[window-title] Preserving title while project identity is unavailable: {error}");
                        previous
                            .get(&(project.workspace_id.clone(), single_line(&project.path)))
                            .map(|identity| ProjectIdentity::Local(Arc::clone(identity)))
                            .unwrap_or_else(|| ProjectIdentity::Other(project.path.clone()))
                    }
                }
            };
            Some(ResolvedProject {
                workspace_id: project.workspace_id,
                display_name,
                path: single_line(&project.path),
                identity,
            })
        })
        .collect();
    ResolvedContext {
        projects,
        active_workspace_id: context.active_workspace_id,
        file_name: optional_name(context.file_name),
    }
}

fn filter_other_owners(
    context: &mut ResolvedContext,
    registry: &ProjectWindowRegistry,
    label: &str,
) {
    context.projects.retain(|project| match &project.identity {
        ProjectIdentity::Local(identity) => registry.owner(identity).is_none_or(|owner| {
            owner.label == label
                && owner
                    .workspace_id
                    .as_ref()
                    .is_none_or(|id| id == &project.workspace_id)
        }),
        ProjectIdentity::Other(_) => true,
    });
}

fn apply_latest(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<WindowTitles>();
    let mut registry = state
        .0
        .lock()
        .map_err(|_| "Window title state is unavailable")?;
    // 原生调用在 UI 线程读取最新快照；缓存与设置处于同一短临界区，旧回调不能覆盖新标题。
    registry.apply(|label, title| {
        let Some(window) = app.get_webview_window(label) else {
            return Ok(false);
        };
        window
            .set_title(title)
            .map(|()| true)
            .map_err(|error| error.to_string())
    })
}

fn apply_context(app: &AppHandle, label: &str, context: ResolvedContext) -> Result<(), String> {
    {
        let state = app.state::<WindowTitles>();
        let mut registry = state
            .0
            .lock()
            .map_err(|_| "Window title state is unavailable")?;
        // 生存检查与更新一起在 UI 回调内完成，销毁事件不能在二者之间插入。
        registry.update_open_window(label, context, |label| {
            app.get_webview_window(label).is_some()
        })?;
    }
    apply_latest(app)
}

fn schedule_apply(app: &AppHandle) {
    let current_app = app.clone();
    if let Err(error) = app.run_on_main_thread(move || {
        if let Err(error) = apply_latest(&current_app) {
            report_error(&current_app, &error);
        }
    }) {
        report_error(app, &format!("Could not schedule title update: {error}"));
    }
}

#[tauri::command]
pub async fn update_window_title_context(
    app: AppHandle,
    window: WebviewWindow,
    context: WindowTitleContext,
) -> Result<(), String> {
    let previous = {
        let state = app.state::<WindowTitles>();
        let registry = state
            .0
            .lock()
            .map_err(|_| "Window title state is unavailable")?;
        registry.native_identities(window.label())
    };
    let mut context = resolve_context_with_previous(context, &previous);
    {
        let state = app.state::<ProjectWindows>();
        let registry = state.0.lock().await;
        filter_other_owners(&mut context, &registry, window.label());
    }
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let current_app = app.clone();
    let label = window.label().to_owned();
    app.run_on_main_thread(move || {
        let result = apply_context(&current_app, &label, context);
        if let Err(error) = &result {
            report_error(&current_app, error);
        }
        if sender.send(result).is_err() {
            report_error(&current_app, "Title update caller is no longer available");
        }
    })
    .map_err(|error| error.to_string())?;
    tokio::time::timeout(TITLE_APPLY_TIMEOUT, receiver)
        .await
        .map_err(|_| "Window title update timed out".to_owned())?
        .map_err(|_| "Window title update was cancelled".to_owned())?
}

pub fn prepare_initial_title(
    app: &AppHandle,
    label: &str,
    path: Option<&str>,
    is_directory: bool,
) -> String {
    let context = match path.filter(|path| !path.contains("://")) {
        Some(path) => {
            let name = Path::new(path)
                .file_name()
                .unwrap_or_else(|| Path::new(path).as_os_str())
                .to_string_lossy()
                .into_owned();
            if is_directory {
                WindowTitleContext {
                    projects: vec![WindowTitleProject {
                        workspace_id: INITIAL_WORKSPACE_ID.into(),
                        display_name: name,
                        path: path.to_owned(),
                    }],
                    active_workspace_id: Some(INITIAL_WORKSPACE_ID.into()),
                    file_name: None,
                }
            } else {
                WindowTitleContext {
                    file_name: Some(name),
                    ..Default::default()
                }
            }
        }
        None => WindowTitleContext::default(),
    };
    let context = resolve_context(context);
    let state = app.state::<WindowTitles>();
    let Ok(mut registry) = state.0.lock() else {
        report_error(app, "Initial title state is unavailable");
        return APPLICATION_NAME.into();
    };
    registry.update(label, context, true);
    registry
        .titles()
        .remove(label)
        .unwrap_or_else(|| APPLICATION_NAME.into())
}

pub fn window_created(app: &AppHandle) {
    schedule_apply(app);
}

pub fn remove_window(app: &AppHandle, label: &str) {
    if let Some(state) = app.try_state::<WindowTitles>() {
        match state.0.lock() {
            Ok(mut registry) => registry.remove(label),
            Err(_) => report_error(app, "Could not remove closed window title"),
        }
        schedule_apply(app);
    }
}

pub fn release_pending(app: &AppHandle, label: &str) {
    if let Some(state) = app.try_state::<WindowTitles>() {
        match state.0.lock() {
            Ok(mut registry) => registry.release_pending(label),
            Err(_) => report_error(app, "Could not release pending window title"),
        }
        schedule_apply(app);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "lithe-window-title-{}-{}",
                std::process::id(),
                NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> String {
            self.0.to_string_lossy().into_owned()
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            if let Err(error) = fs::remove_dir_all(&self.0) {
                eprintln!("Could not remove window-title test directory: {error}");
            }
        }
    }

    fn project(id: &str, name: &str, path: &str) -> ResolvedProject {
        ResolvedProject {
            workspace_id: id.into(),
            display_name: single_line(name),
            path: single_line(path),
            identity: ProjectIdentity::Other(path.into()),
        }
    }

    fn context(
        projects: Vec<ResolvedProject>,
        active: &str,
        file: Option<&str>,
    ) -> ResolvedContext {
        ResolvedContext {
            projects,
            active_workspace_id: Some(active.into()),
            file_name: optional_name(file.map(str::to_owned)),
        }
    }

    #[test]
    fn formats_welcome_project_file_alias_and_standalone_file() {
        let mut registry = TitleRegistry::default();
        registry.update("main", ResolvedContext::default(), false);
        assert_eq!(registry.titles()["main"], "Lithe");
        registry.update(
            "main",
            context(vec![project("a", "demo", r"D:\work\demo")], "a", None),
            false,
        );
        assert_eq!(registry.titles()["main"], "demo – Lithe");
        registry.update(
            "main",
            context(
                vec![project("a", "demo (后端)", r"D:\work\demo")],
                "a",
                Some("Application.java"),
            ),
            false,
        );
        assert_eq!(
            registry.titles()["main"],
            "demo (后端) – Application.java – Lithe"
        );
        registry.update(
            "main",
            ResolvedContext {
                file_name: Some("notes.md".into()),
                ..Default::default()
            },
            false,
        );
        assert_eq!(registry.titles()["main"], "notes.md – Lithe");
    }

    #[test]
    fn updates_both_duplicate_windows_and_restores_after_close() {
        let mut registry = TitleRegistry::default();
        registry.update(
            "a",
            context(
                vec![project("a", "demo", r"D:\work\demo")],
                "a",
                Some("Main.java"),
            ),
            false,
        );
        registry.update(
            "b",
            context(vec![project("b", "demo", r"E:\sample\demo")], "b", None),
            false,
        );
        assert_eq!(
            registry.titles()["a"],
            r"demo [D:\work\demo] – Main.java – Lithe"
        );
        assert_eq!(registry.titles()["b"], r"demo [E:\sample\demo] – Lithe");
        registry.remove("b");
        assert_eq!(registry.titles()["a"], "demo – Main.java – Lithe");
    }

    #[test]
    fn counts_background_tabs_and_uses_final_display_name() {
        let mut registry = TitleRegistry::default();
        registry.update(
            "main",
            context(
                vec![
                    project("a", "demo", r"D:\demo"),
                    project("b", "demo", r"E:\demo"),
                ],
                "a",
                None,
            ),
            false,
        );
        assert_eq!(registry.titles()["main"], r"demo [D:\demo] – Lithe");
        registry.update(
            "main",
            context(
                vec![
                    project("a", "demo", r"D:\demo"),
                    project("b", "demo (实验)", r"E:\demo"),
                ],
                "a",
                None,
            ),
            false,
        );
        assert_eq!(registry.titles()["main"], "demo – Lithe");
    }

    #[test]
    fn native_directory_aliases_do_not_count_as_distinct_projects() {
        let root = TestDirectory::new();
        let child = root.0.join("child");
        fs::create_dir(&child).unwrap();
        let make_context = |id: &str, path: String| {
            resolve_context(WindowTitleContext {
                projects: vec![WindowTitleProject {
                    workspace_id: id.into(),
                    display_name: "demo".into(),
                    path,
                }],
                active_workspace_id: Some(id.into()),
                file_name: None,
            })
        };
        let mut registry = TitleRegistry::default();
        registry.update("a", make_context("a", root.path()), false);
        registry.update(
            "b",
            make_context("b", child.join("..").to_string_lossy().into_owned()),
            false,
        );
        assert_eq!(registry.titles()["a"], "demo – Lithe");
        assert_eq!(registry.titles()["b"], "demo – Lithe");
        registry.update(
            "b",
            make_context("b", child.to_string_lossy().into_owned()),
            false,
        );
        assert!(registry.titles()["a"].starts_with("demo ["));
    }

    #[test]
    fn deleted_directory_keeps_project_title_and_verified_alias_identity() {
        let root = TestDirectory::new();
        let directory = root.0.join("project");
        fs::create_dir(&directory).unwrap();
        let make_context = |id: &str, path: &Path| WindowTitleContext {
            projects: vec![WindowTitleProject {
                workspace_id: id.into(),
                display_name: "demo".into(),
                path: path.to_string_lossy().into_owned(),
            }],
            active_workspace_id: Some(id.into()),
            file_name: Some("Main.java".into()),
        };
        let alias = directory.join(".");
        let mut registry = TitleRegistry::default();
        registry.update("a", resolve_context(make_context("a", &directory)), false);
        registry.update("b", resolve_context(make_context("b", &alias)), false);
        let known_a = registry.native_identities("a");
        let known_b = registry.native_identities("b");
        fs::remove_dir(&directory).expect("Temporary project directory must support deletion");
        assert!(ProjectWindowRegistry::identity(&directory).is_err());
        registry.update(
            "a",
            resolve_context_with_previous(make_context("a", &directory), &known_a),
            false,
        );
        registry.update(
            "b",
            resolve_context_with_previous(make_context("b", &alias), &known_b),
            false,
        );
        assert_eq!(registry.titles()["a"], "demo – Main.java – Lithe");
        assert_eq!(registry.titles()["b"], "demo – Main.java – Lithe");
    }

    #[test]
    fn unavailable_projects_without_cached_identity_remain_distinct_by_path() {
        let root = TestDirectory::new();
        let make_context = |id: &str, child: &str| WindowTitleContext {
            projects: vec![WindowTitleProject {
                workspace_id: id.into(),
                display_name: "demo".into(),
                path: root.0.join(child).to_string_lossy().into_owned(),
            }],
            active_workspace_id: Some(id.into()),
            file_name: None,
        };
        let mut registry = TitleRegistry::default();
        registry.update("a", resolve_context(make_context("a", "offline-a")), false);
        registry.update("b", resolve_context(make_context("b", "offline-b")), false);
        assert_eq!(
            registry.titles()["a"],
            format!("demo [{}] – Lithe", root.0.join("offline-a").display())
        );
        assert_eq!(
            registry.titles()["b"],
            format!("demo [{}] – Lithe", root.0.join("offline-b").display())
        );
    }

    #[test]
    fn filters_only_local_projects_owned_by_another_workspace() {
        let root = TestDirectory::new();
        let make_context = || {
            resolve_context(WindowTitleContext {
                projects: vec![WindowTitleProject {
                    workspace_id: "a".into(),
                    display_name: "demo".into(),
                    path: root.path(),
                }],
                active_workspace_id: Some("a".into()),
                file_name: None,
            })
        };
        let mut owner_registry = ProjectWindowRegistry::default();
        let mut unresolved = make_context();
        filter_other_owners(&mut unresolved, &owner_registry, "main");
        assert_eq!(unresolved.projects.len(), 1);
        owner_registry.claim(
            ProjectWindowRegistry::identity(&root.0).unwrap(),
            "other",
            Some("a"),
        );
        let mut other_owner = make_context();
        filter_other_owners(&mut other_owner, &owner_registry, "main");
        assert!(other_owner.projects.is_empty());
        let mut matching_owner = make_context();
        filter_other_owners(&mut matching_owner, &owner_registry, "other");
        assert_eq!(matching_owner.projects.len(), 1);
    }

    #[test]
    fn preserves_unicode_and_unc_paths_and_cleans_control_characters() {
        let mut registry = TitleRegistry::default();
        registry.update(
            "a",
            context(
                vec![project(
                    "a",
                    " 项目\n😀\u{2028}后端 ",
                    r"\\server\share\demo",
                )],
                "a",
                Some("中文\0文件.ts"),
            ),
            false,
        );
        registry.update(
            "b",
            context(vec![project("b", "项目 😀 后端", r"D:\demo")], "b", None),
            false,
        );
        assert_eq!(
            registry.titles()["a"],
            r"项目 😀 后端 [\\server\share\demo] – 中文 文件.ts – Lithe"
        );
    }

    #[test]
    fn failed_creation_and_initial_open_release_only_provisional_projection() {
        let mut registry = TitleRegistry::default();
        registry.update(
            "a",
            context(vec![project("a", "demo", r"D:\demo")], "a", None),
            false,
        );
        registry.update(
            "b",
            context(vec![project("b", "demo", r"E:\demo")], "b", None),
            true,
        );
        registry.release_pending("b");
        assert_eq!(registry.titles()["a"], "demo – Lithe");
        assert_eq!(registry.titles()["b"], "Lithe");
        registry.release_pending("a");
        assert_eq!(registry.titles()["a"], "demo – Lithe");
        registry.remove("b");
        assert!(!registry.titles().contains_key("b"));
    }

    #[test]
    fn applies_latest_snapshot_and_caches_only_successful_native_updates() {
        let mut registry = TitleRegistry::default();
        registry.update(
            "main",
            context(vec![project("a", "old", r"D:\demo")], "a", None),
            false,
        );
        registry.update(
            "main",
            context(
                vec![project("a", "latest", r"D:\demo")],
                "a",
                Some("Main.java"),
            ),
            false,
        );
        let mut calls = Vec::new();
        let result = registry.apply(|label, title| {
            calls.push((label.to_owned(), title.to_owned()));
            Err("native failure".into())
        });
        assert!(result.unwrap_err().contains("native failure"));
        assert_eq!(
            calls,
            vec![("main".into(), "latest – Main.java – Lithe".into())]
        );
        registry
            .apply(|_, title| {
                calls.push(("main".into(), title.into()));
                Ok(true)
            })
            .unwrap();
        assert_eq!(calls.len(), 2);
        registry
            .apply(|_, _| panic!("Successful identical title must not be applied again"))
            .unwrap();
    }

    #[test]
    fn pending_native_window_is_not_cached_before_creation() {
        let mut registry = TitleRegistry::default();
        registry.update(
            "pending",
            context(vec![project("a", "demo", r"D:\demo")], "a", None),
            true,
        );
        registry.apply(|_, _| Ok(false)).unwrap();
        let mut calls = Vec::new();
        registry
            .apply(|label, title| {
                calls.push((label.to_owned(), title.to_owned()));
                Ok(true)
            })
            .unwrap();
        assert_eq!(calls, vec![("pending".into(), "demo – Lithe".into())]);
        registry.update(
            "pending",
            context(
                vec![project("actual", "demo (别名)", r"D:\demo")],
                "actual",
                None,
            ),
            false,
        );
        registry.release_pending("pending");
        assert_eq!(registry.titles()["pending"], "demo (别名) – Lithe");
    }

    #[test]
    fn initial_welcome_does_not_clear_provisional_open_target() {
        let mut registry = TitleRegistry::default();
        registry.update(
            "pending",
            context(
                vec![project("initial", "demo", r"D:\demo")],
                "initial",
                None,
            ),
            true,
        );
        registry.update_frontend("pending", ResolvedContext::default());
        assert_eq!(registry.titles()["pending"], "demo – Lithe");
        registry.release_pending("pending");
        assert_eq!(registry.titles()["pending"], "Lithe");
        registry.update(
            "pending",
            ResolvedContext {
                file_name: Some("notes.md".into()),
                ..Default::default()
            },
            true,
        );
        registry.update_frontend("pending", ResolvedContext::default());
        assert_eq!(registry.titles()["pending"], "notes.md – Lithe");
        registry.update_frontend(
            "pending",
            context(
                vec![project("actual", "demo (别名)", r"D:\demo")],
                "actual",
                None,
            ),
        );
        registry.release_pending("pending");
        assert_eq!(registry.titles()["pending"], "demo (别名) – Lithe");
        registry.update_frontend("pending", ResolvedContext::default());
        assert_eq!(registry.titles()["pending"], "Lithe");
    }

    #[test]
    fn queued_update_after_destruction_does_not_restore_a_ghost_project() {
        let mut registry = TitleRegistry::default();
        registry.update(
            "main",
            context(vec![project("a", "demo", r"D:\demo")], "a", None),
            false,
        );
        registry.update(
            "closing",
            context(vec![project("b", "demo", r"E:\demo")], "b", None),
            false,
        );
        let queued_context = context(vec![project("b", "demo", r"E:\demo")], "b", None);
        registry.remove("closing");
        let result = registry.update_open_window("closing", queued_context, |_| false);
        assert_eq!(result.unwrap_err(), "Window is no longer available");
        assert!(!registry.titles().contains_key("closing"));
        assert_eq!(registry.titles()["main"], "demo – Lithe");
        registry
            .update_open_window(
                "main",
                context(
                    vec![project("a", "demo", r"D:\demo")],
                    "a",
                    Some("Main.java"),
                ),
                |_| true,
            )
            .unwrap();
        assert_eq!(registry.titles()["main"], "demo – Main.java – Lithe");
    }

    #[test]
    fn deserializes_the_frontend_camel_case_contract() {
        let context: WindowTitleContext = serde_json::from_value(serde_json::json!({
            "projects": [{ "workspaceId": "a", "displayName": "demo", "path": "D:\\demo" }],
            "activeWorkspaceId": "a", "fileName": null
        }))
        .unwrap();
        assert_eq!(context.projects[0].workspace_id, "a");
        assert_eq!(context.projects[0].display_name, "demo");
        assert_eq!(context.active_workspace_id.as_deref(), Some("a"));
        assert!(context.file_name.is_none());
    }
}
