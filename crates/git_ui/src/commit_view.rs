use anyhow::{Context as _, Result};
use buffer_diff::BufferDiff;
use collections::HashMap;
use editor::scroll::Autoscroll;
use editor::{
    Addon, Editor, EditorEvent, EditorSettings, HiddenDiffHunkRenderer, MultiBuffer,
    SelectionEffects, SplittableEditor, hover_markdown_style, multibuffer_context_lines,
};
use futures_lite::future::yield_now;
use git::repository::{CommitDetails, RepoPath};
use git::status::{FileStatus, StatusCode, TrackedStatus};
use git::{
    BuildCommitPermalinkParams, GitHostingProviderRegistry, GitRemote, ParsedGitRemote,
    parse_git_remote_url,
};
use gpui::{
    AnyElement, App, AppContext as _, AsyncWindowContext, ClipboardItem, Context, Entity,
    EventEmitter, FocusHandle, Focusable, InteractiveElement, IntoElement, ObjectFit,
    ParentElement, PromptLevel, Render, ScrollHandle, StatefulInteractiveElement as _, Styled,
    StyledImage, Subscription, Task, WeakEntity, Window, actions, checkerboard, img,
};
use language::{
    Buffer, BufferEvent, Capability, DiskState, File, LanguageRegistry, LineEnding,
    OffsetRangeExt as _, ReplicaId, Rope, TextBuffer,
};
use markdown::{Markdown, MarkdownElement};
use multi_buffer::PathKey;
use project::{
    Project, ProjectPath, WorktreeId,
    git_store::{CommitDiff, Repository, UnshallowState},
};
use settings::{DiffViewStyle, Settings};
use std::{
    any::{Any, TypeId},
    collections::HashSet,
    path::PathBuf,
    sync::Arc,
};
use theme::ActiveTheme;
use ui::{ContextMenu, DiffStat, Disclosure, Divider, Tooltip, WithScrollbar, prelude::*};
use util::{ResultExt, paths::PathStyle, rel_path::RelPath, truncate_and_trailoff};
use workspace::item::PreviewTabsSettings;
use workspace::item::TabTooltipContent;
use workspace::{
    Item, ItemHandle, ItemNavHistory, ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView,
    Workspace,
    item::ItemEvent,
    item::SaveOptions,
    notifications::{NotifyResultExt, NotifyTaskExt},
    pane::SaveIntent,
    searchable::SearchableItemHandle,
};

use crate::commit_tooltip::CommitAvatar;
use crate::git_panel::GitPanel;

actions!(
    git,
    [
        ApplyCurrentStash,
        PopCurrentStash,
        DropCurrentStash,
        OpenFileAtHead,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &ApplyCurrentStash, window, cx| {
            CommitView::apply_stash(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &DropCurrentStash, window, cx| {
            CommitView::remove_stash(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &PopCurrentStash, window, cx| {
            CommitView::pop_stash(workspace, window, cx);
        });
    })
    .detach();
}

pub struct CommitView {
    commit: CommitDetails,
    editor: Entity<SplittableEditor>,
    message: Entity<Markdown>,
    message_expanded: bool,
    message_scroll_handle: ScrollHandle,
    stash: Option<usize>,
    multibuffer: Entity<MultiBuffer>,
    repository: Entity<Repository>,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    remote: Option<GitRemote>,
    is_shallow_boundary: bool,
    file_filter: Option<RepoPath>,
    editing: Option<Entity<HistoricalEditing>>,
    image_diff: Option<Arc<HistoricalImageDiff>>,
    image_actual_size: bool,
    _subscriptions: Vec<Subscription>,
    _load_diff_task: Task<Result<()>>,
}

struct HistoricalEditing {
    buffer: Entity<Buffer>,
    diff: Entity<BufferDiff>,
    edits_diff: Entity<BufferDiff>,
    parent_text: Option<Arc<str>>,
    commit_text: Arc<str>,
    update_task: Task<()>,
    _subscription: Subscription,
}

impl HistoricalEditing {
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let snapshot = self.buffer.read(cx).snapshot().text;
        let diff = self.diff.clone();
        let edits_diff = self.edits_diff.clone();
        let parent_text = self.parent_text.clone();
        let commit_text = self.commit_text.clone();
        self.update_task = cx.spawn(async move |this, cx| {
            let update = diff.update(cx, |diff, cx| {
                diff.set_base_text(parent_text, snapshot.clone(), cx)
            });
            update.await;
            let update = edits_diff.update(cx, |diff, cx| {
                diff.set_base_text(Some(commit_text), snapshot, cx)
            });
            update.await;
            this.update(cx, |_, cx| cx.notify()).log_err();
        });
    }
}

struct HistoricalEditHighlight;

struct HistoricalImageDiff {
    before: HistoricalImage,
    after: HistoricalImage,
}

enum HistoricalImage {
    Missing,
    Loaded {
        image: Arc<gpui::Image>,
        metadata: project::image_store::ImageMetadata,
    },
    Error(SharedString),
}

impl HistoricalImage {
    fn load(bytes: Option<Vec<u8>>, present: bool) -> Self {
        let Some(bytes) = bytes else {
            return if present {
                Self::Error("Image data is unavailable from this repository".into())
            } else {
                Self::Missing
            };
        };
        match project::ImageItem::compute_metadata_from_bytes(&bytes).and_then(|metadata| {
            project::image_store::create_gpui_image(bytes).map(|image| (image, metadata))
        }) {
            Ok((image, metadata)) => Self::Loaded { image, metadata },
            Err(error) => Self::Error(format!("Cannot preview image: {error}").into()),
        }
    }

    fn render(&self, label: &'static str, actual_size: bool, cx: &App) -> impl IntoElement {
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_hidden()
            .child(
                h_flex()
                    .flex_none()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .child(Label::new(label))
                    .when_some(
                        match self {
                            Self::Loaded { metadata, .. } => Some(format!(
                                "{} × {} · {} bytes",
                                metadata.width, metadata.height, metadata.file_size
                            )),
                            _ => None,
                        },
                        |this, dimensions| {
                            this.child(
                                Label::new(dimensions)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                        },
                    ),
            )
            .child(
                div()
                    .id(label)
                    .flex_1()
                    .min_h_0()
                    .overflow_scroll()
                    .child(match self {
                        Self::Missing => div()
                            .size_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(Label::new("Not present in this revision").color(Color::Muted))
                            .into_any_element(),
                        Self::Error(message) => div()
                            .p_3()
                            .child(Label::new(message.clone()).color(Color::Warning))
                            .into_any_element(),
                        Self::Loaded { image, metadata } => div()
                            .debug_selector(move || format!("{label}-image"))
                            .map(|this| {
                                if actual_size {
                                    this.w(px(metadata.width as f32))
                                        .h(px(metadata.height as f32))
                                } else {
                                    this.size_full()
                                }
                            })
                            .bg(checkerboard(
                                cx.theme().colors().text_muted.opacity(0.15),
                                8.,
                            ))
                            .child(
                                img(image.clone())
                                    .size_full()
                                    .object_fit(ObjectFit::Contain)
                                    .with_fallback(|| {
                                        div()
                                            .p_3()
                                            .child(Label::new("Cannot display this image"))
                                            .into_any_element()
                                    }),
                            )
                            .into_any_element(),
                    }),
            )
    }
}

fn can_edit_historical_buffer(buffer: &Buffer, text: &str, cx: &App) -> bool {
    let mut normalized = text.to_string();
    let line_ending = LineEnding::detect(text);
    LineEnding::normalize(&mut normalized);
    buffer.capability() == Capability::ReadWrite
        && !buffer.is_dirty()
        && !buffer.has_conflict()
        && buffer
            .file()
            .is_some_and(|file| file.is_local() && file.disk_state().exists())
        && project::File::from_dyn(buffer.file()).is_some_and(|file| {
            file.worktree
                .read(cx)
                .entry_for_path(&file.path)
                .is_some_and(|entry| entry.canonical_path.is_none())
        })
        && buffer.line_ending() == line_ending
        && buffer.text() == normalized
}

pub(crate) struct GitBlob {
    pub(crate) path: RepoPath,
    pub(crate) worktree_id: WorktreeId,
    pub(crate) is_deleted: bool,
    pub(crate) is_binary: bool,
    pub(crate) display_name: String,
}

pub(crate) fn worktree_id_for_repo_path(
    repository: &Repository,
    project: &Project,
    path: &RepoPath,
    cx: &App,
) -> Option<WorktreeId> {
    repository
        .repo_path_to_project_path(path, cx)
        .map(|project_path| project_path.worktree_id)
        .or_else(|| {
            let (worktree, _) = project.find_worktree(&repository.work_directory_abs_path, cx)?;
            Some(worktree.read(cx).id())
        })
        .or_else(|| {
            project
                .worktrees(cx)
                .next()
                .map(|worktree| worktree.read(cx).id())
        })
}

struct CommitDiffAddon {
    file_statuses: HashMap<language::BufferId, FileStatus>,
    commit_view: WeakEntity<CommitView>,
}

impl Addon for CommitDiffAddon {
    fn to_any(&self) -> &dyn std::any::Any {
        self
    }

    fn override_status_for_buffer_id(
        &self,
        buffer_id: language::BufferId,
        _cx: &App,
    ) -> Option<FileStatus> {
        self.file_statuses.get(&buffer_id).copied()
    }

    fn extend_buffer_header_context_menu(
        &self,
        menu: ContextMenu,
        buffer: &language::BufferSnapshot,
        _window: &mut Window,
        cx: &mut App,
    ) -> ContextMenu {
        let file_to_open = buffer.file().and_then(|file| {
            let commit_view = self.commit_view.upgrade()?;
            let commit_view = commit_view.read(cx);
            let project_path = commit_view
                .repository
                .read(cx)
                .repo_path_to_project_path(&RepoPath::from_rel_path(file.path()), cx)?;
            let exists_at_head = commit_view
                .workspace
                .upgrade()?
                .read(cx)
                .project()
                .read(cx)
                .entry_for_path(&project_path, cx)
                .is_some();
            exists_at_head.then(|| file.clone())
        });

        menu.when_some(file_to_open, |menu, file| {
            let commit_view = self.commit_view.clone();
            menu.entry(
                "Open File in Project",
                Some(Box::new(OpenFileAtHead)),
                move |window, cx| {
                    commit_view
                        .update(cx, |view, cx| view.open_file_at_head(&file, window, cx))
                        .log_err();
                },
            )
        })
    }
}

const FILE_NAMESPACE_SORT_PREFIX: u64 = 1;

impl CommitView {
    pub fn open(
        commit_sha: String,
        repo: WeakEntity<Repository>,
        workspace: WeakEntity<Workspace>,
        stash: Option<usize>,
        file_filter: Option<RepoPath>,
        window: &mut Window,
        cx: &mut App,
    ) {
        Self::open_with_options(
            commit_sha,
            repo,
            workspace,
            stash,
            file_filter,
            false,
            window,
            cx,
        )
    }

    pub fn open_preview(
        commit_sha: String,
        repo: WeakEntity<Repository>,
        workspace: WeakEntity<Workspace>,
        file_filter: Option<RepoPath>,
        preview: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Option<()>> {
        Self::open_task(
            commit_sha,
            repo,
            workspace,
            None,
            file_filter,
            false,
            preview,
            window,
            cx,
        )
    }

    fn open_with_options(
        commit_sha: String,
        repo: WeakEntity<Repository>,
        workspace: WeakEntity<Workspace>,
        stash: Option<usize>,
        file_filter: Option<RepoPath>,
        ignore_shallow_boundary: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        Self::open_task(
            commit_sha,
            repo,
            workspace,
            stash,
            file_filter,
            ignore_shallow_boundary,
            false,
            window,
            cx,
        )
        .detach();
    }

    fn open_task(
        commit_sha: String,
        repo: WeakEntity<Repository>,
        workspace: WeakEntity<Workspace>,
        stash: Option<usize>,
        file_filter: Option<RepoPath>,
        ignore_shallow_boundary: bool,
        preview: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Option<()>> {
        let commit_diff = repo
            .update(cx, |repo, _| {
                repo.load_commit_diff(
                    commit_sha.clone(),
                    ignore_shallow_boundary,
                    file_filter.clone(),
                )
            })
            .ok();
        let commit_details = repo
            .update(cx, |repo, _| repo.show(commit_sha.clone()))
            .ok();

        window.spawn(cx, async move |cx| {
            let commit_diff = commit_diff?;
            let commit_details = commit_details?;
            let (commit_diff, commit_details) = futures::join!(commit_diff, commit_details);
            let mut commit_diff = commit_diff
                .context("Commit diff request was cancelled")
                .and_then(|result| result)
                .notify_workspace_async_err(workspace.clone(), cx)?;
            let commit_details = commit_details
                .context("Commit details request was cancelled")
                .and_then(|result| result)
                .notify_workspace_async_err(workspace.clone(), cx)?;

            // Filter to specific file if requested
            if let Some(ref filter_path) = file_filter {
                commit_diff.files.retain(|f| &f.path == filter_path);
            }

            let repo = repo.upgrade()?;

            workspace
                .update_in(cx, |workspace, window, cx| {
                    let project = workspace.project();
                    let workspace_entity = cx.entity();
                    let workspace_handle = cx.weak_entity();
                    let commit_view = cx.new(|cx| {
                        CommitView::new(
                            commit_details,
                            commit_diff,
                            repo,
                            project.clone(),
                            workspace_entity,
                            workspace_handle,
                            stash,
                            file_filter,
                            window,
                            cx,
                        )
                    });

                    let pane = workspace.active_pane();
                    pane.update(cx, |pane, cx| {
                        let existing = pane.items().enumerate().find_map(|(index, item)| {
                            let view = item.downcast::<CommitView>()?;
                            let existing = view.read(cx);
                            let new = commit_view.read(cx);
                            (existing.commit.sha == new.commit.sha
                                && existing.repository == new.repository
                                && existing.stash == new.stash
                                && existing.file_filter == new.file_filter)
                                .then_some((index, view.clone()))
                        });
                        if let Some((index, existing)) = existing {
                            if existing.read(cx).is_shallow_boundary
                                == commit_view.read(cx).is_shallow_boundary
                            {
                                if !preview {
                                    pane.unpreview_item_if_preview(existing.item_id());
                                }
                                pane.activate_item(index, true, !preview, window, cx);
                                return;
                            }
                            pane.remove_item(existing.item_id(), false, false, window, cx);
                        }
                        let destination = if preview && PreviewTabsSettings::get_global(cx).enabled
                        {
                            pane.replace_preview_item_id(commit_view.item_id(), window, cx)
                        } else {
                            None
                        };
                        pane.add_item(
                            Box::new(commit_view),
                            true,
                            !preview,
                            destination,
                            window,
                            cx,
                        );
                    })
                })
                .log_err()
        })
    }

    fn new(
        commit: CommitDetails,
        commit_diff: CommitDiff,
        repository: Entity<Repository>,
        project: Entity<Project>,
        workspace_entity: Entity<Workspace>,
        workspace: WeakEntity<Workspace>,
        stash: Option<usize>,
        file_filter: Option<RepoPath>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let language_registry = project.read(cx).languages().clone();
        let is_shallow_boundary = commit_diff.is_shallow_boundary;
        let showing_full_file = file_filter.is_some();
        let multibuffer = cx.new(|cx| {
            let mut multibuffer = if showing_full_file {
                MultiBuffer::without_headers(Capability::ReadWrite)
            } else {
                MultiBuffer::new(Capability::ReadOnly)
            };
            multibuffer.set_all_diff_hunks_expanded(cx);
            multibuffer
        });

        let message = cx.new(|cx| {
            Markdown::new(
                commit.message.clone(),
                Some(language_registry.clone()),
                None,
                cx,
            )
        });

        let editor = cx.new(|cx| {
            let editor = SplittableEditor::new(
                EditorSettings::get_global(cx).diff_view_style,
                multibuffer.clone(),
                project.clone(),
                workspace_entity.clone(),
                window,
                cx,
            );
            editor.set_diff_hunk_renderer(Some(Arc::new(HiddenDiffHunkRenderer)), cx);

            editor.rhs_editor().update(cx, |editor, cx| {
                editor.set_read_only(true);
                editor.set_allow_git_diff_scrollbar_markers(showing_full_file, cx);
                editor.set_show_bookmarks(false, cx);
                editor.set_show_breakpoints(false, cx);
                editor.set_show_diff_review_button(true, cx);
            });

            editor
        });
        let subscriptions =
            vec![
                cx.subscribe(&editor, |this: &mut Self, _, event: &EditorEvent, cx| {
                    this.on_editor_event(event, cx);
                }),
            ];
        let commit_sha = Arc::<str>::from(commit.sha.as_ref());

        let repository_clone = repository.clone();
        let project_clone = project.clone();

        let load_diff_task = cx.spawn_in(window, async move |this, cx| {
            let mut binary_buffer_ids: HashSet<language::BufferId> = HashSet::default();
            let mut file_statuses: HashMap<language::BufferId, FileStatus> = HashMap::default();

            for file in commit_diff.files {
                if showing_full_file
                    && file.is_binary
                    && matches!(
                        file.path
                            .as_unix_str()
                            .rsplit('.')
                            .next()
                            .map(str::to_ascii_lowercase)
                            .as_deref(),
                        Some(
                            "png"
                                | "jpg"
                                | "jpeg"
                                | "gif"
                                | "webp"
                                | "bmp"
                                | "tif"
                                | "tiff"
                                | "ico"
                                | "pnm"
                                | "ppm"
                                | "pgm"
                                | "pbm"
                        )
                    )
                {
                    let images = cx
                        .background_spawn(async move {
                            HistoricalImageDiff {
                                before: HistoricalImage::load(
                                    file.old_binary,
                                    file.old_text.is_some(),
                                ),
                                after: HistoricalImage::load(
                                    file.new_binary,
                                    file.new_text.is_some(),
                                ),
                            }
                        })
                        .await;
                    this.update(cx, |this, cx| {
                        this.image_diff = Some(Arc::new(images));
                        cx.notify();
                    })?;
                    continue;
                }
                let repo_path = file.path.clone();
                let is_created = file.old_text.is_none();
                let is_deleted = file.new_text.is_none();
                let raw_new_text = file.new_text.unwrap_or_default();
                let raw_old_text = file.old_text;

                let is_binary = file.is_binary;

                let new_text = if is_binary {
                    "(binary file not shown)".to_string()
                } else {
                    raw_new_text
                };
                let old_text = if is_binary { None } else { raw_old_text };
                let worktree_id = repository_clone
                    .read_with(cx, |repository, cx| {
                        worktree_id_for_repo_path(
                            repository,
                            project_clone.read(cx),
                            &file.path,
                            cx,
                        )
                    })
                    .context("project has no worktrees")?;
                let short_sha = commit_sha
                    .get(0..git::SHORT_SHA_LENGTH)
                    .unwrap_or(&commit_sha);
                let file_name = file
                    .path
                    .file_name()
                    .map(|name| name.to_string())
                    .unwrap_or_else(|| file.path.display(PathStyle::local()).to_string());
                let display_name = format!("{short_sha} - {file_name}");

                let file = Arc::new(GitBlob {
                    path: file.path.clone(),
                    is_deleted,
                    is_binary,
                    worktree_id,
                    display_name,
                }) as Arc<dyn language::File>;

                let historical_text = new_text.clone();
                let parent_text = old_text.clone();
                let mut buffer = build_buffer(new_text, file, &language_registry, cx).await?;

                let status_code = if is_created {
                    StatusCode::Added
                } else if is_deleted {
                    StatusCode::Deleted
                } else {
                    StatusCode::Modified
                };
                let mut buffer_diff = if is_binary {
                    cx.update(|_, cx| {
                        let snapshot = buffer.read(cx).snapshot();
                        cx.new(|cx| {
                            BufferDiff::new_unchanged(
                                &snapshot,
                                snapshot.language().cloned(),
                                Some(language_registry.clone()),
                                cx,
                            )
                        })
                    })?
                } else {
                    build_buffer_diff(old_text, &buffer, &language_registry, cx).await?
                };

                if showing_full_file
                    && stash.is_none()
                    && !is_binary
                    && !is_deleted
                    && !is_shallow_boundary
                {
                    let project_path = repository_clone.read_with(cx, |repo, cx| {
                        repo.repo_path_to_project_path(&repo_path, cx)
                    });
                    if let Some(project_path) = project_path {
                        let candidate = project_clone
                            .update(cx, |project, cx| project.open_buffer(project_path, cx))
                            .await;
                        match candidate {
                            Ok(candidate) => {
                                if candidate.read_with(cx, |buffer, cx| {
                                    can_edit_historical_buffer(buffer, &historical_text, cx)
                                }) {
                                    let candidate_diff = build_buffer_diff(
                                        parent_text.clone(),
                                        &candidate,
                                        &language_registry,
                                        cx,
                                    )
                                    .await?;
                                    let edits_diff = build_buffer_diff(
                                        Some(historical_text.clone()),
                                        &candidate,
                                        &language_registry,
                                        cx,
                                    )
                                    .await?;
                                    let attached = this.update(cx, |this, cx| {
                                        if !can_edit_historical_buffer(
                                            candidate.read(cx),
                                            &historical_text,
                                            cx,
                                        ) {
                                            return false;
                                        }
                                        let editing = cx.new(|cx| HistoricalEditing {
                                            buffer: candidate.clone(),
                                            diff: candidate_diff.clone(),
                                            edits_diff,
                                            parent_text: parent_text.clone().map(Arc::from),
                                            commit_text: historical_text.clone().into(),
                                            update_task: Task::ready(()),
                                            _subscription: cx.subscribe(
                                                &candidate,
                                                |this: &mut HistoricalEditing, _, event, cx| {
                                                    if matches!(
                                                        event,
                                                        BufferEvent::Edited { .. }
                                                            | BufferEvent::Reloaded
                                                    ) {
                                                        this.refresh(cx);
                                                    }
                                                },
                                            ),
                                        });
                                        this.attach_editing(editing, cx);
                                        true
                                    })?;
                                    if attached {
                                        buffer = candidate;
                                        buffer_diff = candidate_diff;
                                    }
                                }
                            }
                            Err(error) => log::debug!(
                                "Cannot open working file for historical editing: {error:#}"
                            ),
                        }
                    }
                }
                let buffer_id = cx.update(|_, cx| buffer.read(cx).remote_id())?;
                file_statuses.insert(
                    buffer_id,
                    FileStatus::Tracked(TrackedStatus {
                        index_status: status_code,
                        worktree_status: StatusCode::Unmodified,
                    }),
                );

                if is_binary {
                    binary_buffer_ids.insert(buffer_id);
                }

                let (excerpt_ranges, path) = cx.update(|_, cx| {
                    let snapshot = buffer.read(cx).snapshot();
                    let path = PathKey::with_sort_prefix(
                        FILE_NAMESPACE_SORT_PREFIX,
                        snapshot.file().unwrap().path().clone(),
                    );
                    let ranges = if is_binary || showing_full_file {
                        vec![language::Point::zero()..snapshot.max_point()]
                    } else {
                        let diff_snapshot = buffer_diff.read(cx).snapshot(cx);
                        let mut hunks = diff_snapshot.hunks(&snapshot).peekable();
                        if hunks.peek().is_none() {
                            vec![language::Point::zero()..snapshot.max_point()]
                        } else {
                            hunks
                                .map(|hunk| hunk.buffer_range.to_point(&snapshot))
                                .collect::<Vec<_>>()
                        }
                    };
                    (ranges, path)
                })?;

                // Batch the insertion of excerpts and yield between batches, to avoid blocking the main thread when a single file has many hunks.
                const EXCERPT_BATCH_SIZE: usize = 10;
                let total = excerpt_ranges.len();
                let mut batch_end = 0;
                while batch_end < total {
                    let is_first_batch = batch_end == 0;
                    batch_end = (batch_end + EXCERPT_BATCH_SIZE).min(total);
                    let ranges = excerpt_ranges[..batch_end].to_vec();
                    this.update_in(cx, |this, window, cx| {
                        this.editor.update(cx, |editor, cx| {
                            editor.update_excerpts_for_path(
                                path.clone(),
                                buffer.clone(),
                                ranges,
                                multibuffer_context_lines(cx),
                                buffer_diff.clone(),
                                cx,
                            );
                            if is_first_batch && editor.diff_view_style() == DiffViewStyle::Split {
                                editor.split(window, cx);
                            }
                            if showing_full_file && is_first_batch {
                                editor.rhs_editor().update(cx, |editor, cx| {
                                    let snapshot = editor.snapshot(window, cx);
                                    let buffer = snapshot.buffer_snapshot();
                                    if let Some(hunk) = buffer
                                        .diff_hunks_in_range(
                                            language::Point::zero()..buffer.max_point(),
                                        )
                                        .next()
                                    {
                                        let position =
                                            language::Point::new(hunk.row_range.start.0, 0);
                                        editor.change_selections(
                                            SelectionEffects::scroll(Autoscroll::center()),
                                            window,
                                            cx,
                                            |selections| {
                                                selections.select_ranges([position..position]);
                                            },
                                        );
                                    }
                                });
                            }
                        });
                    })?;
                    if batch_end < total {
                        yield_now().await;
                    }
                }
            }

            this.update(cx, |this, cx| {
                let commit_view = cx.weak_entity();
                this.editor.update(cx, |editor, cx| {
                    editor.rhs_editor().update(cx, |editor, _cx| {
                        editor.register_addon(CommitDiffAddon {
                            file_statuses,
                            commit_view,
                        });
                    });
                });
                if !showing_full_file && !binary_buffer_ids.is_empty() {
                    this.editor.update(cx, |editor, cx| {
                        editor.rhs_editor().update(cx, |editor, cx| {
                            editor.fold_buffers(binary_buffer_ids, cx);
                        });
                    });
                }
            })?;

            anyhow::Ok(())
        });

        let snapshot = repository.read(cx).snapshot();
        let remote_url = snapshot
            .remote_upstream_url
            .as_ref()
            .or(snapshot.remote_origin_url.as_ref());

        let remote = remote_url.and_then(|url| {
            let provider_registry = GitHostingProviderRegistry::default_global(cx);
            parse_git_remote_url(provider_registry, url).map(|(host, parsed)| GitRemote {
                host,
                owner: parsed.owner.into(),
                repo: parsed.repo.into(),
            })
        });

        Self {
            commit,
            editor,
            message,
            message_expanded: false,
            message_scroll_handle: ScrollHandle::new(),
            multibuffer,
            stash,
            repository,
            project,
            workspace,
            remote,
            is_shallow_boundary,
            file_filter,
            editing: None,
            image_diff: None,
            image_actual_size: false,
            _subscriptions: subscriptions,
            _load_diff_task: load_diff_task,
        }
    }

    fn on_editor_event(&mut self, event: &EditorEvent, cx: &mut Context<Self>) {
        if matches!(event, EditorEvent::BufferEdited)
            && self
                .editing
                .as_ref()
                .is_none_or(|editing| !editing.read(cx).buffer.read(cx).is_dirty())
        {
            return;
        }
        cx.emit(event.clone());
    }

    fn attach_editing(&mut self, editing: Entity<HistoricalEditing>, cx: &mut Context<Self>) {
        self.editor
            .read(cx)
            .rhs_editor()
            .clone()
            .update(cx, |editor, _| editor.set_read_only(false));
        self._subscriptions
            .push(cx.observe(&editing, |this, editing, cx| {
                let editing = editing.read(cx);
                let buffer = editing.buffer.read(cx).snapshot();
                let snapshot = this.multibuffer.read(cx).snapshot(cx);
                let ranges = editing
                    .edits_diff
                    .read(cx)
                    .snapshot(cx)
                    .hunks(&buffer)
                    .filter_map(|hunk| {
                        let offsets = hunk.buffer_range.to_offset(&buffer);
                        let last_offset = if offsets.is_empty() {
                            offsets.end
                        } else {
                            offsets.end - 1
                        };
                        Some(
                            snapshot.anchor_in_excerpt(buffer.anchor_before(offsets.start))?
                                ..snapshot.anchor_in_excerpt(buffer.anchor_after(last_offset))?,
                        )
                    })
                    .collect::<Vec<_>>();
                this.editor
                    .read(cx)
                    .rhs_editor()
                    .clone()
                    .update(cx, |editor, cx| {
                        editor.clear_gutter_highlights::<HistoricalEditHighlight>(cx);
                        editor.highlight_gutter::<HistoricalEditHighlight>(
                            ranges,
                            |cx| cx.theme().status().warning,
                            cx,
                        );
                    });
                cx.notify();
            }));
        self.editing = Some(editing.clone());
        editing.update(cx, |_, cx| cx.notify());
        cx.notify();
    }

    fn render_shallow_boundary_notice(&self, cx: &App) -> impl IntoElement {
        let commit_sha = self.commit.sha.to_string();
        let repository = self.repository.clone();
        let workspace = self.workspace.clone();
        let stash = self.stash;
        let file_filter = self.file_filter.clone();
        let unshallow_state = self.repository.read(cx).unshallow_state();
        let can_fetch = !self.project.read(cx).is_via_collab()
            && unshallow_state != UnshallowState::Unshallowed;
        let fetch_in_flight = unshallow_state == UnshallowState::InProgress;
        v_flex()
            .flex_grow(1.)
            .items_center()
            .justify_center()
            .gap_2()
            .child(
                Label::new("This commit is at the boundary of a shallow clone.")
                    .color(Color::Muted),
            )
            .child(
                Label::new(
                    "Its parent history was not fetched, so the changes it introduced cannot be shown.",
                )
                .color(Color::Muted),
            )
            .child(
                h_flex()
                    .gap_2()
                    .when(can_fetch, |this| {
                        let commit_sha = commit_sha.clone();
                        let repository = repository.clone();
                        let workspace = workspace.clone();
                        let file_filter = file_filter.clone();
                        this.child(
                            Button::new(
                                "fetch-unshallow",
                                if fetch_in_flight {
                                    "Fetching…"
                                } else {
                                    "Fetch Missing History"
                                },
                            )
                                .style(ButtonStyle::Filled)
                                .disabled(fetch_in_flight)
                                .tooltip(Tooltip::text(
                                    "Run `git fetch --unshallow` to download the full history, then show this commit's changes.",
                                ))
                                .on_click(move |_, window, cx| {
                                    let fetch = crate::commit_tooltip::fetch_unshallow(
                                        repository.clone(),
                                        workspace.clone(),
                                        window,
                                        cx,
                                    );
                                    let commit_sha = commit_sha.clone();
                                    let repository = repository.downgrade();
                                    let workspace = workspace.clone();
                                    let file_filter = file_filter.clone();
                                    window
                                        .spawn(cx, async move |cx| {
                                            fetch.await?;
                                            cx.update(|window, cx| {
                                                Self::open_with_options(
                                                    commit_sha,
                                                    repository,
                                                    workspace,
                                                    stash,
                                                    file_filter,
                                                    false,
                                                    window,
                                                    cx,
                                                )
                                            })
                                        })
                                        .detach_and_log_err(cx);
                                }),
                        )
                    })
                    .child(
                        Button::new(
                            "load-shallow-snapshot",
                            if file_filter.is_some() {
                                "Load File Snapshot"
                            } else {
                                "Load Full Snapshot"
                            },
                        )
                            .style(ButtonStyle::Outlined)
                            .tooltip(Tooltip::text(if file_filter.is_some() {
                                "Show this file's full contents at this commit as added."
                            } else {
                                "Show every file at this commit as added. This can be slow in large repositories."
                            }))
                            .on_click(move |_, window, cx| {
                                Self::open_with_options(
                                    commit_sha.clone(),
                                    repository.downgrade(),
                                    workspace.clone(),
                                    stash,
                                    file_filter.clone(),
                                    true,
                                    window,
                                    cx,
                                );
                            }),
                    ),
            )
    }

    fn render_commit_avatar(
        &self,
        sha: &SharedString,
        size: impl Into<gpui::AbsoluteLength>,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        CommitAvatar::new(
            sha,
            Some(self.commit.author_email.clone()),
            self.remote.as_ref(),
        )
        .size(size)
        .render(window, cx)
    }

    fn calculate_changed_lines(&self, cx: &App) -> (u32, u32) {
        self.multibuffer.read(cx).snapshot(cx).total_changed_lines()
    }

    fn open_file_at_head(
        &mut self,
        file: &Arc<dyn language::File>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let rel_path = file.path().clone();
        let worktree_id = file.worktree_id(cx);
        let repo_path = RepoPath::from_rel_path(&rel_path);
        let project_path = self
            .repository
            .read(cx)
            .repo_path_to_project_path(&repo_path, cx)
            .unwrap_or(project::ProjectPath {
                worktree_id,
                path: rel_path,
            });

        self.workspace
            .update(cx, |workspace, cx| {
                workspace
                    .open_path_preview(project_path, None, false, false, true, window, cx)
                    .detach_and_log_err(cx);
            })
            .log_err();
    }

    fn working_file_path(&self, cx: &App) -> Option<ProjectPath> {
        let path = self.file_filter.clone().or_else(|| {
            let buffer = self
                .editor
                .read(cx)
                .focused_editor()
                .read(cx)
                .active_buffer(cx)?;
            Some(RepoPath::from_rel_path(buffer.read(cx).file()?.path()))
        })?;
        let project_path = self
            .repository
            .read(cx)
            .repo_path_to_project_path(&path, cx)?;
        self.project.read(cx).entry_for_path(&project_path, cx)?;
        Some(project_path)
    }

    fn open_file_at_head_action(
        &mut self,
        _: &OpenFileAtHead,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = self.working_file_path(cx) else {
            return;
        };
        self.workspace
            .update(cx, |workspace, cx| {
                workspace
                    .open_path_preview(path, None, false, false, true, window, cx)
                    .detach_and_notify_err(self.workspace.clone(), window, cx);
            })
            .log_err();
    }

    fn render_header(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let commit = &self.commit;
        let author_name = commit.author_name.clone();
        let author_email = commit.author_email.clone();
        let commit_sha = commit.sha.clone();
        let commit_date = time::OffsetDateTime::from_unix_timestamp(commit.commit_timestamp)
            .unwrap_or_else(|_| time::OffsetDateTime::now_utc());
        let local_offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
        let date_string = time_format::format_localized_timestamp(
            commit_date,
            time::OffsetDateTime::now_utc(),
            local_offset,
            time_format::TimestampFormat::MediumAbsolute,
        );

        let avatar_size = rems_from_px(40_f32);
        let avatar_size_px = avatar_size.to_pixels(window.rem_size());
        let gutter_width = self.editor.update(cx, |editor, cx| {
            let editor = editor.rhs_editor().clone();
            editor.update(cx, |editor, cx| {
                let snapshot = editor.snapshot(window, cx);
                let style = editor.style(cx);
                let font_id = window.text_system().resolve_font(&style.text.font());
                let font_size = style.text.font_size.to_pixels(window.rem_size());
                snapshot
                    .gutter_dimensions(font_id, font_size, style, window, cx)
                    .full_width()
            })
        });
        let avatar_min_side_padding = rems_from_px(6_f32).to_pixels(window.rem_size());
        let avatar_container_min = avatar_size_px + avatar_min_side_padding;
        let avatar_container_width = gutter_width.max(avatar_container_min);

        let clipboard_has_sha = cx
            .read_from_clipboard()
            .and_then(|entry| entry.text())
            .map_or(false, |clipboard_text| {
                clipboard_text.trim() == commit_sha.as_ref()
            });

        let (copy_icon, copy_icon_color) = if clipboard_has_sha {
            (IconName::Check, Color::Success)
        } else {
            (IconName::Copy, Color::Muted)
        };

        let has_more = self.commit.message.trim().contains('\n');
        let is_expanded = self.message_expanded;
        let expand_tooltip = if is_expanded {
            "Fold Commit Description"
        } else {
            "Expand Commit Description"
        };

        v_flex()
            .w_full()
            .py_2p5()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                h_flex()
                    .pr_2p5()
                    .w_full()
                    .flex_wrap()
                    .justify_between()
                    .child(
                        h_flex()
                            .child(
                                h_flex()
                                    .flex_none()
                                    .w(avatar_container_width)
                                    .justify_center()
                                    .child(self.render_commit_avatar(
                                        &commit.sha,
                                        avatar_size,
                                        window,
                                        cx,
                                    )),
                            )
                            .child(
                                v_flex()
                                    .child(h_flex().gap_1().child(Label::new(author_name)).when(
                                        has_more,
                                        |this| {
                                            this.child(
                                                Disclosure::new(
                                                    "commit-message-disclosure",
                                                    is_expanded,
                                                )
                                                .closed_icon(IconName::ExpandVertical)
                                                .opened_icon(IconName::FoldVertical)
                                                .tooltip(Tooltip::text(expand_tooltip))
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.message_expanded = !this.message_expanded;
                                                    cx.notify();
                                                })),
                                            )
                                        },
                                    ))
                                    .child(
                                        h_flex()
                                            .gap_1p5()
                                            .child(
                                                Label::new(date_string)
                                                    .color(Color::Muted)
                                                    .size(LabelSize::Small),
                                            )
                                            .child(
                                                Label::new("•")
                                                    .size(LabelSize::Small)
                                                    .color(Color::Muted)
                                                    .alpha(0.5),
                                            )
                                            .child(
                                                Label::new(author_email)
                                                    .color(Color::Muted)
                                                    .size(LabelSize::Small),
                                            ),
                                    ),
                            ),
                    )
                    .when(self.stash.is_none(), |this| {
                        this.child(
                            Button::new("sha", "Commit SHA")
                                .start_icon(
                                    Icon::new(copy_icon)
                                        .size(IconSize::Small)
                                        .color(copy_icon_color),
                                )
                                .tooltip({
                                    let commit_sha = commit_sha.clone();
                                    move |_, cx| {
                                        Tooltip::with_meta(
                                            "Copy Commit SHA",
                                            None,
                                            commit_sha.clone(),
                                            cx,
                                        )
                                    }
                                })
                                .on_click(move |_, _, cx| {
                                    cx.stop_propagation();
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        commit_sha.to_string(),
                                    ));
                                }),
                        )
                    }),
            )
            .children(self.render_commit_message(avatar_container_width, window, cx))
    }

    fn render_commit_message(
        &self,
        avatar_spacer: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let message = self.commit.message.trim();
        if message.is_empty() {
            return None;
        }

        let markdown_style = hover_markdown_style(window, cx);

        let is_expanded = self.message_expanded;

        let has_more = message.contains('\n');
        let collapsed = has_more && !is_expanded;
        let collapsed_height = window.line_height();
        let max_expanded_height = window.line_height() * 12.;

        Some(
            h_flex()
                .w_full()
                .pr_2p5()
                .child(h_flex().flex_none().w(avatar_spacer))
                .child(
                    div()
                        .relative()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .id("commit-message")
                                .size_full()
                                .text_sm()
                                .when(collapsed, |this| this.h(collapsed_height).overflow_hidden())
                                .when(!collapsed, |this| {
                                    this.max_h(max_expanded_height)
                                        .overflow_y_scroll()
                                        .track_scroll(&self.message_scroll_handle)
                                })
                                .child(MarkdownElement::new(self.message.clone(), markdown_style)),
                        )
                        .vertical_scrollbar_for(&self.message_scroll_handle, window, cx),
                ),
        )
    }

    fn apply_stash(workspace: &mut Workspace, window: &mut Window, cx: &mut App) {
        Self::stash_action(
            workspace,
            "Apply",
            window,
            cx,
            async move |repository, sha, stash, commit_view, workspace, cx| {
                let result = repository.update(cx, |repo, cx| {
                    if !stash_matches_index(&sha, stash, repo) {
                        return Err(anyhow::anyhow!("Stash has changed, not applying"));
                    }
                    Ok(repo.stash_apply(Some(stash), cx))
                });

                match result {
                    Ok(task) => task.await?,
                    Err(err) => {
                        Self::close_commit_view(commit_view, workspace, cx).await?;
                        return Err(err);
                    }
                };
                Self::close_commit_view(commit_view, workspace, cx).await?;
                anyhow::Ok(())
            },
        );
    }

    fn pop_stash(workspace: &mut Workspace, window: &mut Window, cx: &mut App) {
        Self::stash_action(
            workspace,
            "Pop",
            window,
            cx,
            async move |repository, sha, stash, commit_view, workspace, cx| {
                let result = repository.update(cx, |repo, cx| {
                    if !stash_matches_index(&sha, stash, repo) {
                        return Err(anyhow::anyhow!("Stash has changed, pop aborted"));
                    }
                    Ok(repo.stash_pop(Some(stash), cx))
                });

                match result {
                    Ok(task) => task.await?,
                    Err(err) => {
                        Self::close_commit_view(commit_view, workspace, cx).await?;
                        return Err(err);
                    }
                };
                Self::close_commit_view(commit_view, workspace, cx).await?;
                anyhow::Ok(())
            },
        );
    }

    fn remove_stash(workspace: &mut Workspace, window: &mut Window, cx: &mut App) {
        Self::stash_action(
            workspace,
            "Drop",
            window,
            cx,
            async move |repository, sha, stash, commit_view, workspace, cx| {
                let result = repository.update(cx, |repo, cx| {
                    if !stash_matches_index(&sha, stash, repo) {
                        return Err(anyhow::anyhow!("Stash has changed, drop aborted"));
                    }
                    Ok(repo.stash_drop(Some(stash), cx))
                });

                match result {
                    Ok(task) => task.await??,
                    Err(err) => {
                        Self::close_commit_view(commit_view, workspace, cx).await?;
                        return Err(err);
                    }
                };
                Self::close_commit_view(commit_view, workspace, cx).await?;
                anyhow::Ok(())
            },
        );
    }

    fn stash_action<AsyncFn>(
        workspace: &mut Workspace,
        str_action: &str,
        window: &mut Window,
        cx: &mut App,
        callback: AsyncFn,
    ) where
        AsyncFn: AsyncFnOnce(
                Entity<Repository>,
                &SharedString,
                usize,
                Entity<CommitView>,
                WeakEntity<Workspace>,
                &mut AsyncWindowContext,
            ) -> anyhow::Result<()>
            + 'static,
    {
        let Some(commit_view) = workspace.active_item_as::<CommitView>(cx) else {
            return;
        };
        let Some(stash) = commit_view.read(cx).stash else {
            return;
        };
        let sha = commit_view.read(cx).commit.sha.clone();
        let answer = window.prompt(
            PromptLevel::Info,
            &format!("{} stash@{{{}}}?", str_action, stash),
            None,
            &[str_action, "Cancel"],
            cx,
        );

        let workspace_weak = workspace.weak_handle();
        let commit_view_entity = commit_view;

        window
            .spawn(cx, async move |cx| {
                if answer.await != Ok(0) {
                    return anyhow::Ok(());
                }

                let Some(workspace) = workspace_weak.upgrade() else {
                    return Ok(());
                };

                let repo = workspace.update(cx, |workspace, cx| {
                    workspace
                        .panel::<GitPanel>(cx)
                        .and_then(|p| p.read(cx).active_repository.clone())
                });

                let Some(repo) = repo else {
                    return Ok(());
                };

                callback(repo, &sha, stash, commit_view_entity, workspace_weak, cx).await?;
                anyhow::Ok(())
            })
            .detach_and_notify_err(workspace.weak_handle(), window, cx);
    }

    async fn close_commit_view(
        commit_view: Entity<CommitView>,
        workspace: WeakEntity<Workspace>,
        cx: &mut AsyncWindowContext,
    ) -> anyhow::Result<()> {
        workspace
            .update_in(cx, |workspace, window, cx| {
                let active_pane = workspace.active_pane();
                let commit_view_id = commit_view.entity_id();
                active_pane.update(cx, |pane, cx| {
                    pane.close_item_by_id(commit_view_id, SaveIntent::Skip, window, cx)
                })
            })?
            .await?;
        anyhow::Ok(())
    }
}

impl language::File for GitBlob {
    fn as_local(&self) -> Option<&dyn language::LocalFile> {
        None
    }

    fn disk_state(&self) -> DiskState {
        DiskState::Historic {
            was_deleted: self.is_deleted,
        }
    }

    fn path_style(&self, _: &App) -> PathStyle {
        PathStyle::local()
    }

    fn path(&self) -> &Arc<RelPath> {
        self.path.as_ref()
    }

    fn full_path(&self, _: &App) -> PathBuf {
        self.path.as_std_path().to_path_buf()
    }

    fn file_name<'a>(&'a self, _: &'a App) -> &'a str {
        self.display_name.as_ref()
    }

    fn worktree_id(&self, _: &App) -> WorktreeId {
        self.worktree_id
    }

    fn to_proto(&self, _cx: &App) -> language::proto::File {
        unimplemented!()
    }

    fn is_private(&self) -> bool {
        false
    }

    fn can_open(&self) -> bool {
        !self.is_binary
    }
}

pub(crate) async fn build_buffer(
    mut text: String,
    blob: Arc<dyn File>,
    language_registry: &Arc<language::LanguageRegistry>,
    cx: &mut AsyncWindowContext,
) -> Result<Entity<Buffer>> {
    let line_ending = LineEnding::detect(&text);
    LineEnding::normalize(&mut text);
    let text = Rope::from(text);
    let language =
        cx.update(|_, cx| language_registry.language_for_file(&blob, Some(&text), cx))?;
    let language = if let Some(language_id) = language {
        language_registry
            .load_language(language_id)
            .await
            .ok()
            .and_then(|e| e.log_err())
    } else {
        None
    };
    let buffer = cx.new(|cx| {
        let buffer = TextBuffer::new_normalized(
            ReplicaId::LOCAL,
            cx.entity_id().as_non_zero_u64().into(),
            line_ending,
            text,
        );
        let mut buffer = Buffer::build(buffer, Some(blob), Capability::ReadWrite, cx);
        buffer.set_language_async(language, cx);
        buffer
    });
    Ok(buffer)
}

async fn build_buffer_diff(
    mut old_text: Option<String>,
    buffer: &Entity<Buffer>,
    language_registry: &Arc<LanguageRegistry>,
    cx: &mut AsyncWindowContext,
) -> Result<Entity<BufferDiff>> {
    if let Some(old_text) = &mut old_text {
        LineEnding::normalize(old_text);
    }

    let language = cx.update(|_, cx| buffer.read(cx).language().cloned())?;
    let buffer = cx.update(|_, cx| buffer.read(cx).snapshot())?;

    let diff =
        cx.new(|cx| BufferDiff::new(&buffer.text, language, Some(language_registry.clone()), cx));

    diff.update(cx, |diff, cx| {
        diff.set_base_text(
            old_text.map(|old_text| Arc::from(old_text.as_str())),
            buffer.text.clone(),
            cx,
        )
    })
    .await;

    Ok(diff)
}

impl EventEmitter<EditorEvent> for CommitView {}

impl Focusable for CommitView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Item for CommitView {
    type Event = EditorEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::GitCommit).color(Color::Muted))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        let short_sha = self.commit.sha.get(0..7).unwrap_or(&*self.commit.sha);
        if let Some(path) = &self.file_filter {
            let name = path.file_name().unwrap_or_default();
            return format!("{name} — {short_sha}").into();
        }
        let subject = truncate_and_trailoff(self.commit.message.split('\n').next().unwrap(), 20);
        format!("{short_sha} — {subject}").into()
    }

    fn tab_tooltip_content(&self, _: &App) -> Option<TabTooltipContent> {
        let short_sha = self.commit.sha.get(0..16).unwrap_or(&*self.commit.sha);
        let subject = self.commit.message.split('\n').next().unwrap();

        Some(TabTooltipContent::Custom(Box::new(Tooltip::element({
            let subject = self
                .file_filter
                .as_ref()
                .map(|path| path.display(PathStyle::local()).to_string())
                .unwrap_or_else(|| subject.to_string());
            let short_sha = short_sha.to_string();

            move |_, _| {
                v_flex()
                    .child(Label::new(subject.clone()))
                    .child(
                        Label::new(short_sha.clone())
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    )
                    .into_any_element()
            }
        }))))
    }

    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(ItemEvent)) {
        // Loading excerpts changes the view, but must not pin a preview tab as an edit.
        if !matches!(
            event,
            EditorEvent::BufferRangesUpdated { .. } | EditorEvent::BuffersRemoved { .. }
        ) {
            Editor::to_item_events(event, f)
        }
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Commit View Opened")
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.deactivated(window, cx));
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        cx: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else if self.image_diff.is_some() {
            None
        } else if type_id == TypeId::of::<SplittableEditor>() {
            Some(self.editor.clone().into())
        } else if type_id == TypeId::of::<Editor>() {
            Some(self.editor.read(cx).rhs_editor().clone().into())
        } else {
            None
        }
    }

    fn as_searchable(&self, _: &Entity<Self>, _: &App) -> Option<Box<dyn SearchableItemHandle>> {
        self.image_diff
            .is_none()
            .then(|| Box::new(self.editor.clone()) as Box<dyn SearchableItemHandle>)
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        self.editor.read(cx).for_each_project_item(cx, f)
    }

    fn active_project_path(&self, cx: &App) -> Option<ProjectPath> {
        self.editor.read(cx).active_project_path(cx)
    }

    fn set_nav_history(
        &mut self,
        nav_history: ItemNavHistory,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |editor, _| {
                editor.set_nav_history(Some(nav_history));
            });
        });
    }

    fn navigate(
        &mut self,
        data: Arc<dyn Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.editor
            .update(cx, |editor, cx| editor.navigate(data, window, cx))
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.added_to_workspace(workspace, window, cx)
        });
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.editing.is_some() && self.editor.read(cx).is_dirty(cx)
    }

    fn has_conflict(&self, cx: &App) -> bool {
        self.editing.is_some() && self.editor.read(cx).has_conflict(cx)
    }

    fn can_save(&self, cx: &App) -> bool {
        self.editing.is_some() && self.editor.read(cx).can_save(cx)
    }

    fn save(
        &mut self,
        options: SaveOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        if self.editing.is_none() {
            return Task::ready(Ok(()));
        }
        self.editor
            .update(cx, |editor, cx| editor.save(options, project, window, cx))
    }

    fn reload(
        &mut self,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.editor
            .update(cx, |editor, cx| editor.reload(project, window, cx))
    }

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<workspace::WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>>
    where
        Self: Sized,
    {
        let file_statuses = self
            .editor
            .read(cx)
            .rhs_editor()
            .read(cx)
            .addon::<CommitDiffAddon>()
            .map(|addon| addon.file_statuses.clone())
            .unwrap_or_default();
        let Some(workspace_entity) = self.workspace.upgrade() else {
            return Task::ready(None);
        };
        let project = self.project.clone();
        let diff_view_style = self.editor.read(cx).diff_view_style();
        let multibuffer = self.multibuffer.clone();
        Task::ready(Some(cx.new(|cx| {
            let commit_view = cx.weak_entity();
            let editor = cx.new({
                let file_statuses = file_statuses.clone();
                let project = project.clone();
                let workspace_entity = workspace_entity.clone();
                let multibuffer = multibuffer.clone();
                move |cx| {
                    let editor = SplittableEditor::new(
                        diff_view_style,
                        multibuffer.clone(),
                        project.clone(),
                        workspace_entity.clone(),
                        window,
                        cx,
                    );
                    editor.set_diff_hunk_renderer(Some(Arc::new(HiddenDiffHunkRenderer)), cx);
                    editor.rhs_editor().update(cx, |editor, cx| {
                        editor.set_show_bookmarks(false, cx);
                        editor.set_show_breakpoints(false, cx);
                        editor.set_show_diff_review_button(true, cx);
                        editor.register_addon(CommitDiffAddon {
                            file_statuses,
                            commit_view,
                        });
                    });
                    editor
                }
            });
            let language_registry = project.read(cx).languages().clone();
            let message = cx.new(|cx| {
                Markdown::new(
                    self.commit.message.clone(),
                    Some(language_registry),
                    None,
                    cx,
                )
            });
            let subscriptions =
                vec![
                    cx.subscribe(&editor, |this: &mut Self, _, event: &EditorEvent, cx| {
                        this.on_editor_event(event, cx);
                    }),
                ];
            editor
                .read(cx)
                .rhs_editor()
                .clone()
                .update(cx, |editor, _| editor.set_read_only(self.editing.is_none()));
            let mut view = Self {
                editor,
                message,
                message_expanded: self.message_expanded,
                message_scroll_handle: ScrollHandle::new(),
                multibuffer: self.multibuffer.clone(),
                commit: self.commit.clone(),
                stash: self.stash,
                repository: self.repository.clone(),
                project: self.project.clone(),
                workspace: self.workspace.clone(),
                remote: self.remote.clone(),
                is_shallow_boundary: self.is_shallow_boundary,
                file_filter: self.file_filter.clone(),
                editing: None,
                image_diff: self.image_diff.clone(),
                image_actual_size: self.image_actual_size,
                _subscriptions: subscriptions,
                _load_diff_task: Task::ready(Ok(())),
            };
            if let Some(editing) = &self.editing {
                view.attach_editing(editing.clone(), cx);
            }
            view
        })))
    }
}

impl Render for CommitView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_stash = self.stash.is_some();

        v_flex()
            .key_context(if is_stash { "StashDiff" } else { "CommitDiff" })
            .on_action(cx.listener(Self::open_file_at_head_action))
            .when(self.image_diff.is_some(), |this| this
                .track_focus(&self.focus_handle(cx))
                .on_mouse_down(gpui::MouseButton::Left, cx.listener(|this, _, window, cx| this.focus_handle(cx).focus(window, cx))))
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .when(self.file_filter.is_none(), |this| {
                this.child(self.render_header(window, cx))
            })
            .when(self.file_filter.is_some() && self.image_diff.is_none() && !self.is_shallow_boundary, |this| {
                this.child(h_flex().px_3().py_1().flex_none().child(
                    Label::new(if self.editing.is_some() {
                        "Working file · changes since this commit are marked in orange"
                    } else {
                        "Read-only snapshot · editing requires a matching, saved working file"
                    }).size(LabelSize::Small).color(if self.editing.is_some() { Color::Warning } else { Color::Muted })
                ))
            })
            .when_some(self.image_diff.clone(), |this, images| {
                this.child(v_flex().flex_1().min_h_0()
                    .child(h_flex().px_3().py_1().child(
                        Button::new("image-size", if self.image_actual_size { "Fit Images" } else { "Actual Size" })
                            .on_click(cx.listener(|this, _, _, cx| { this.image_actual_size = !this.image_actual_size; cx.notify(); }))
                    ))
                    .child(h_flex().flex_1().min_h_0()
                        .child(images.before.render("Before", self.image_actual_size, cx))
                        .child(Divider::vertical())
                        .child(images.after.render("After", self.image_actual_size, cx))))
            })
            .when(
                self.image_diff.is_none() && !self.editor.read(cx).rhs_editor().read(cx).is_empty(cx),
                |this| this.child(div().flex_grow(1.).child(self.editor.clone())),
            )
            .when(self.is_shallow_boundary, |this| {
                this.child(self.render_shallow_boundary_notice(cx))
            })
    }
}

pub struct CommitViewToolbar {
    commit_view: Option<WeakEntity<CommitView>>,
}

impl CommitViewToolbar {
    pub fn new() -> Self {
        Self { commit_view: None }
    }
}

impl EventEmitter<ToolbarItemEvent> for CommitViewToolbar {}

impl Render for CommitViewToolbar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(commit_view) = self.commit_view.as_ref().and_then(|w| w.upgrade()) else {
            return div();
        };

        let commit_view_ref = commit_view.read(cx);
        let is_stash = commit_view_ref.stash.is_some();
        let can_open_working_file = commit_view_ref.working_file_path(cx).is_some();

        let (additions, deletions) = commit_view_ref.calculate_changed_lines(cx);

        let commit_sha = commit_view_ref.commit.sha.clone();

        let remote_info = commit_view_ref.remote.as_ref().map(|remote| {
            let provider = remote.host.name();
            let parsed_remote = ParsedGitRemote {
                owner: remote.owner.as_ref().into(),
                repo: remote.repo.as_ref().into(),
            };
            let params = BuildCommitPermalinkParams { sha: &commit_sha };
            let url = remote
                .host
                .build_commit_permalink(&parsed_remote, params)
                .to_string();
            (provider, url)
        });

        let sha_for_graph = commit_sha.to_string();

        h_flex()
            .gap_1()
            .when(additions > 0 || deletions > 0, |this| {
                this.child(
                    h_flex()
                        .gap_2()
                        .child(DiffStat::new(
                            "toolbar-diff-stat",
                            additions as usize,
                            deletions as usize,
                        ))
                        .child(Divider::vertical()),
                )
            })
            .child(
                Button::new("open-working-file", "Open Working File")
                    .label_size(LabelSize::Small)
                    .disabled(!can_open_working_file)
                    .tooltip(Tooltip::text(if can_open_working_file {
                        "Open the current file in the project"
                    } else {
                        "This file is not present in the working tree"
                    }))
                    .on_click(move |_, window, cx| {
                        commit_view.update(cx, |view, cx| {
                            view.open_file_at_head_action(&OpenFileAtHead, window, cx);
                        });
                    }),
            )
            .child(
                IconButton::new("buffer-search", IconName::MagnifyingGlass)
                    .icon_size(IconSize::Small)
                    .tooltip(move |_, cx| {
                        Tooltip::for_action(
                            "Buffer Search",
                            &zed_actions::buffer_search::Deploy::find(),
                            cx,
                        )
                    })
                    .on_click(|_, window, cx| {
                        window.dispatch_action(
                            Box::new(zed_actions::buffer_search::Deploy::find()),
                            cx,
                        );
                    }),
            )
            .when(!is_stash, |this| {
                this.child(
                    IconButton::new("show-in-git-graph", IconName::GitGraph)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text("Show in Git Graph"))
                        .on_click(move |_, window, cx| {
                            window.dispatch_action(
                                Box::new(crate::git_graph::OpenAtCommit {
                                    sha: sha_for_graph.clone(),
                                }),
                                cx,
                            );
                        }),
                )
                .children(remote_info.map(|(provider_name, url)| {
                    let icon = ui::git_hosting_provider_icon(provider_name.as_str());

                    IconButton::new("view_on_provider", icon)
                        .icon_size(IconSize::Small)
                        .tooltip(Tooltip::text(format!("View on {}", provider_name)))
                        .on_click(move |_, _, cx| cx.open_url(&url))
                }))
            })
    }
}

impl ToolbarItemView for CommitViewToolbar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        if let Some(entity) = active_pane_item.and_then(|i| i.act_as::<CommitView>(cx)) {
            self.commit_view = Some(entity.downgrade());
            return ToolbarItemLocation::PrimaryRight;
        }
        self.commit_view = None;
        ToolbarItemLocation::Hidden
    }

    fn pane_focus_update(
        &mut self,
        _pane_focused: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }
}

fn stash_matches_index(sha: &str, stash_index: usize, repo: &Repository) -> bool {
    repo.stash_entries
        .entries
        .get(stash_index)
        .map(|entry| entry.oid.to_string() == sha)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor::ToPoint;
    use git::repository::repo_path;
    use gpui::TestAppContext;
    use project::{FakeFs, git_store::CommitFile};
    use serde_json::json;
    use settings::SettingsStore;
    use std::path::Path;
    use workspace::MultiWorkspace;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings = SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            language_model::init(cx);
            editor::init(cx);
            crate::init(cx);
        });
    }

    #[gpui::test]
    async fn test_conservative_historical_editing(cx: &mut TestAppContext) {
        use fs::Fs;
        init_test(cx);
        for (working_text, commit_text, unsaved, editable) in [
            (Some("committed\n"), Some("committed\n"), false, true),
            (Some("newer\n"), Some("committed\n"), false, false),
            (Some("disk\n"), Some("committed\n"), true, false),
            (None, Some("committed\n"), false, false),
            (Some("committed\n"), None, false, false),
            (Some("committed\r\n"), Some("committed\n"), false, false),
        ] {
            let fs = FakeFs::new(cx.executor());
            let mut tree = json!({ ".git": {} });
            if let Some(text) = working_text {
                tree["file.txt"] = json!(text);
            }
            fs.insert_tree("/project", tree).await;
            let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
            let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
                MultiWorkspace::test_new(project.clone(), window, cx)
            });
            let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
            let repository = project.read_with(cx, |project, cx| {
                project.active_repository(cx).expect("repository")
            });
            let working_buffer = if working_text.is_some() {
                Some(
                    project
                        .update(cx, |project, cx| {
                            project.open_local_buffer(Path::new("/project/file.txt"), cx)
                        })
                        .await
                        .expect("working buffer"),
                )
            } else {
                None
            };
            if unsaved {
                working_buffer
                    .as_ref()
                    .expect("working buffer")
                    .update(cx, |buffer, cx| {
                        buffer.edit(
                            [(0..buffer.len(), commit_text.expect("commit text"))],
                            None,
                            cx,
                        );
                    });
            }
            let view = cx.new_window_entity(|window, cx| {
                CommitView::new(
                    CommitDetails {
                        sha: "1111111111111111111111111111111111111111".into(),
                        message: "Change text".into(),
                        ..Default::default()
                    },
                    CommitDiff {
                        files: vec![CommitFile {
                            path: repo_path("file.txt"),
                            old_text: Some("parent\n".into()),
                            new_text: commit_text.map(str::to_owned),
                            is_binary: false,
                            old_binary: None,
                            new_binary: None,
                        }],
                        is_shallow_boundary: false,
                    },
                    repository,
                    project.clone(),
                    workspace.clone(),
                    workspace.downgrade(),
                    None,
                    Some(repo_path("file.txt")),
                    window,
                    cx,
                )
            });
            view.update(cx, |view, _| {
                std::mem::replace(&mut view._load_diff_task, Task::ready(Ok(())))
            })
            .await
            .expect("load history");
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx)
            });
            cx.run_until_parked();
            let editor = view.read_with(cx, |view, cx| {
                assert_eq!(
                    view.editing.is_some(),
                    editable,
                    "working={working_text:?}, unsaved={unsaved}"
                );
                let editor = view.editor.read(cx).rhs_editor().clone();
                assert_eq!(!editor.read(cx).read_only(cx), editable);
                editor
            });
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.active_pane().update(cx, |pane, cx| {
                    pane.replace_preview_item_id(view.entity_id(), window, cx);
                });
            });
            editor.update_in(cx, |editor, window, cx| {
                editor.handle_input("local\n", window, cx)
            });
            cx.run_until_parked();
            workspace.read_with(cx, |workspace, cx| {
                assert_eq!(
                    workspace
                        .active_pane()
                        .read(cx)
                        .is_active_preview_item(view.entity_id()),
                    !editable
                );
            });
            if editable {
                let working_buffer = working_buffer.expect("live buffer");
                assert_eq!(
                    working_buffer.read_with(cx, |buffer, _| buffer.text()),
                    "local\ncommitted\n"
                );
                view.read_with(cx, |view, cx| {
                    assert!(view.is_dirty(cx));
                    assert!(view.can_save(cx));
                    let editing = view.editing.as_ref().expect("editable").read(cx);
                    assert_eq!(editing.buffer, working_buffer);
                    assert_eq!(
                        editing.diff.read(cx).base_text_string(cx).as_deref(),
                        Some("parent\n")
                    );
                    assert_eq!(
                        editing.edits_diff.read(cx).base_text_string(cx).as_deref(),
                        Some("committed\n")
                    );
                    assert_eq!(
                        editing
                            .edits_diff
                            .read(cx)
                            .snapshot(cx)
                            .hunks(&working_buffer.read(cx).snapshot())
                            .count(),
                        1
                    );
                    assert_eq!(
                        view.editor
                            .read(cx)
                            .lhs_editor()
                            .expect("left pane")
                            .read(cx)
                            .text(cx),
                        "parent\n"
                    );
                });
                editor.update_in(cx, |editor, window, cx| {
                    let snapshot = editor.snapshot(window, cx);
                    let buffer = snapshot.buffer_snapshot();
                    let highlights = editor.gutter_highlights_in_range(
                        buffer.anchor_before(language::Point::zero())
                            ..buffer.anchor_after(buffer.max_point()),
                        &snapshot,
                        cx,
                    );
                    assert_eq!(highlights.len(), 1);
                    let (range, color) = highlights.first().expect("orange marker");
                    assert_eq!(range.start.row(), range.end.row());
                    assert_eq!(*color, cx.theme().status().warning);
                });
                editor.update_in(cx, |editor, window, cx| {
                    editor.undo(&editor::actions::Undo, window, cx)
                });
                cx.run_until_parked();
                assert_eq!(
                    working_buffer.read_with(cx, |buffer, _| buffer.text()),
                    "committed\n"
                );
                view.read_with(cx, |view, cx| {
                    let editing = view.editing.as_ref().expect("editable").read(cx);
                    assert_eq!(
                        editing
                            .edits_diff
                            .read(cx)
                            .snapshot(cx)
                            .hunks(&working_buffer.read(cx).snapshot())
                            .count(),
                        0
                    );
                });
                editor.update_in(cx, |editor, window, cx| {
                    editor.handle_input("saved\n", window, cx)
                });
                cx.run_until_parked();
                view.update_in(cx, |view, window, cx| {
                    view.save(
                        SaveOptions {
                            format: false,
                            ..Default::default()
                        },
                        project.clone(),
                        window,
                        cx,
                    )
                })
                .await
                .expect("save working file");
                assert_eq!(
                    fs.load(Path::new("/project/file.txt"))
                        .await
                        .expect("saved file"),
                    "saved\ncommitted\n"
                );
                assert!(!view.read_with(cx, |view, cx| view.is_dirty(cx)));
            } else {
                assert_eq!(
                    editor.read_with(cx, |editor, cx| editor.text(cx)),
                    commit_text.unwrap_or_default()
                );
                if let Some(working_text) = working_text {
                    assert_eq!(
                        fs.load(Path::new("/project/file.txt"))
                            .await
                            .expect("unchanged disk"),
                        working_text
                    );
                }
            }
        }
    }

    #[gpui::test]
    async fn test_historical_image_diff(cx: &mut TestAppContext) {
        init_test(cx);
        let red = b"P6\n1 1\n255\n\xff\0\0".to_vec();
        let blue = b"P6\n1 1\n255\n\0\0\xff".to_vec();
        for (before, after) in [
            (Some(red.clone()), Some(blue.clone())),
            (None, Some(red.clone())),
            (Some(blue), None),
        ] {
            let fs = FakeFs::new(cx.executor());
            fs.insert_tree("/project", json!({ ".git": {} })).await;
            let project = Project::test(fs, [Path::new("/project")], cx).await;
            let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
                MultiWorkspace::test_new(project.clone(), window, cx)
            });
            let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
            let repository = project.read_with(cx, |project, cx| {
                project.active_repository(cx).expect("repository")
            });
            let view = cx.new_window_entity(|window, cx| {
                CommitView::new(
                    CommitDetails {
                        sha: "1111111111111111111111111111111111111111".into(),
                        message: "Change image".into(),
                        ..Default::default()
                    },
                    CommitDiff {
                        files: vec![CommitFile {
                            path: repo_path("image.ppm"),
                            old_text: before.as_ref().map(|_| String::new()),
                            new_text: after.as_ref().map(|_| String::new()),
                            is_binary: true,
                            old_binary: before.clone(),
                            new_binary: after.clone(),
                        }],
                        is_shallow_boundary: false,
                    },
                    repository,
                    project.clone(),
                    workspace.clone(),
                    workspace.downgrade(),
                    None,
                    Some(repo_path("image.ppm")),
                    window,
                    cx,
                )
            });
            view.update(cx, |view, _| {
                std::mem::replace(&mut view._load_diff_task, Task::ready(Ok(())))
            })
            .await
            .expect("load images");
            view.read_with(cx, |view, cx| {
                assert!(view.editing.is_none());
                assert!(!view.can_save(cx));
                let images = view.image_diff.as_ref().expect("image diff");
                for (image, exists) in [
                    (&images.before, before.is_some()),
                    (&images.after, after.is_some()),
                ] {
                    match image {
                        HistoricalImage::Loaded { metadata, .. } => {
                            assert!(exists);
                            assert_eq!((metadata.width, metadata.height), (1, 1));
                        }
                        HistoricalImage::Missing => assert!(!exists),
                        HistoricalImage::Error(error) => panic!("unexpected image error: {error}"),
                    }
                }
            });
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx)
            });
            cx.run_until_parked();
            cx.update(|window, cx| window.draw(cx).clear(cx));
            cx.simulate_resize(gpui::size(px(1000.), px(800.)));
            cx.run_until_parked();
            for (selector, exists) in [
                ("Before-image", before.is_some()),
                ("After-image", after.is_some()),
            ] {
                let bounds = cx.debug_bounds(selector);
                assert_eq!(bounds.is_some(), exists);
                if let Some(bounds) = bounds {
                    assert!(bounds.size.width > px(100.) && bounds.size.height > px(100.));
                }
            }
            view.update(cx, |view, cx| {
                view.image_actual_size = true;
                cx.notify();
            });
            cx.run_until_parked();
            cx.update(|window, cx| window.draw(cx).clear(cx));
            cx.simulate_resize(gpui::size(px(1001.), px(800.)));
            cx.run_until_parked();
            let selector = if before.is_some() {
                "Before-image"
            } else {
                "After-image"
            };
            let bounds = cx.debug_bounds(selector).expect("actual-size image");
            assert_eq!(bounds.size, gpui::size(px(1.), px(1.)));
        }
        assert!(matches!(
            HistoricalImage::load(Some(b"broken image".to_vec()), true),
            HistoricalImage::Error(_)
        ));
        assert!(matches!(
            HistoricalImage::load(None, true),
            HistoricalImage::Error(_)
        ));
    }

    #[gpui::test]
    async fn test_history_file_full_context_and_working_file(cx: &mut TestAppContext) {
        init_test(cx);
        for first_change_row in [0, 100] {
            let fs = FakeFs::new(cx.executor());
            fs.insert_tree(
                "/project",
                json!({ ".git": {}, "file.txt": "working file\n" }),
            )
            .await;
            let project = Project::test(fs, [Path::new("/project")], cx).await;
            let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
                MultiWorkspace::test_new(project.clone(), window, cx)
            });
            let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
            let repository = project.read_with(cx, |project, cx| {
                project.active_repository(cx).expect("repository")
            });
            let old_text: String = (0..200).map(|line| format!("line {line}\n")).collect();
            let new_text = old_text
                .replace(&format!("line {first_change_row}\n"), "modified line\n")
                .replace("line 170\n", "another change\n");
            let view = cx.new_window_entity(|window, cx| {
                CommitView::new(
                    CommitDetails {
                        sha: "1111111111111111111111111111111111111111".into(),
                        message: "Change a line".into(),
                        ..Default::default()
                    },
                    CommitDiff {
                        files: vec![CommitFile {
                            path: repo_path("file.txt"),
                            old_text: Some(old_text.clone()),
                            new_text: Some(new_text.clone()),
                            is_binary: false,
                            old_binary: None,
                            new_binary: None,
                        }],
                        is_shallow_boundary: false,
                    },
                    repository,
                    project.clone(),
                    workspace.clone(),
                    workspace.downgrade(),
                    None,
                    Some(repo_path("file.txt")),
                    window,
                    cx,
                )
            });
            let load = view.update(cx, |view, _| {
                std::mem::replace(&mut view._load_diff_task, Task::ready(Ok(())))
            });
            load.await.expect("load historical file");
            workspace.update_in(cx, |workspace, window, cx| {
                workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx);
            });
            cx.run_until_parked();
            cx.update(|window, cx| window.draw(cx).clear(cx));
            view.update_in(cx, |view, window, cx| {
                view.editor
                    .read(cx)
                    .rhs_editor()
                    .clone()
                    .update(cx, |editor, cx| {
                        let scroll = editor.snapshot(window, cx).scroll_position().y;
                        if first_change_row > 0 {
                            assert!(
                                scroll > 0.,
                                "initial diff scroll should reveal the first change"
                            );
                        } else {
                            assert_eq!(scroll, 0.);
                        }
                    });
            });
            view.read_with(cx, |view, cx| {
                let editor = view.editor.read(cx);
                assert_eq!(editor.rhs_editor().read(cx).text(cx), new_text);
                assert_eq!(
                    editor.lhs_editor().expect("split diff").read(cx).text(cx),
                    old_text
                );
                assert!(editor.rhs_editor().read(cx).read_only(cx));
                assert!(!view.multibuffer.read(cx).snapshot(cx).show_headers());
                assert_eq!(
                    editor
                        .rhs_editor()
                        .read(cx)
                        .selections
                        .newest_anchor()
                        .head()
                        .to_point(&view.multibuffer.read(cx).snapshot(cx))
                        .row,
                    first_change_row
                );
                assert!(view.working_file_path(cx).is_some());
                assert_eq!(view.tab_content_text(0, cx), "file.txt — 1111111");
            });
            let working_buffer = project
                .update(cx, |project, cx| {
                    project.open_local_buffer(Path::new("/project/file.txt"), cx)
                })
                .await
                .expect("working buffer");
            working_buffer.update(cx, |buffer, cx| {
                buffer.edit([(0..0, "unsaved edit\n")], None, cx);
            });
            view.update_in(cx, |view, window, cx| {
                view.open_file_at_head_action(&OpenFileAtHead, window, cx);
            });
            cx.run_until_parked();
            workspace.read_with(cx, |workspace, cx| {
                let editor = workspace
                    .active_item_as::<Editor>(cx)
                    .expect("working file editor");
                assert_eq!(editor.read(cx).text(cx), "unsaved edit\nworking file\n");
            });
            view.update(cx, |view, cx| {
                view.file_filter = Some(repo_path("deleted.txt"));
                assert!(view.working_file_path(cx).is_none());
            });
        }
    }

    #[gpui::test]
    async fn test_history_file_preview_identity_and_pinning(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/project",
            json!({ ".git": {}, "a.txt": "a", "b.txt": "b" }),
        )
        .await;
        let project = Project::test(fs, [Path::new("/project")], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
        let repository = project.read_with(cx, |project, cx| {
            project.active_repository(cx).expect("repository")
        });
        let sha = "1111111111111111111111111111111111111111";
        for (path, preview, expected_count) in [
            ("a.txt", true, 1),
            ("b.txt", true, 1),
            ("b.txt", false, 1),
            ("a.txt", true, 2),
        ] {
            cx.update(|window, cx| {
                CommitView::open_preview(
                    sha.to_string(),
                    repository.downgrade(),
                    workspace.downgrade(),
                    Some(repo_path(path)),
                    preview,
                    window,
                    cx,
                )
            })
            .await
            .expect("open file diff");
            workspace.read_with(cx, |workspace, cx| {
                let pane = workspace.active_pane().read(cx);
                assert_eq!(pane.items().count(), expected_count);
                let view = pane
                    .active_item()
                    .and_then(|item| item.downcast::<CommitView>())
                    .expect("commit view");
                assert_eq!(view.read(cx).file_filter, Some(repo_path(path)));
                assert_eq!(pane.is_active_preview_item(view.item_id()), preview);
            });
        }
        cx.update(|window, cx| {
            CommitView::open(
                sha.to_string(),
                repository.downgrade(),
                workspace.downgrade(),
                None,
                None,
                window,
                cx,
            );
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.active_pane().read(cx).items().count(), 3);
            assert!(
                workspace
                    .active_item_as::<CommitView>(cx)
                    .expect("whole commit")
                    .read(cx)
                    .file_filter
                    .is_none()
            );
        });
    }
}
