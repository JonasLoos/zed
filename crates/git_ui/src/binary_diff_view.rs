use std::{
    hash::{DefaultHasher, Hash, Hasher},
    sync::Arc,
    time::Duration,
};

use git::repository::RepoPath;
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    ObjectFit, Render, SharedString, StyledImage, Subscription, Task, Window, checkerboard, img,
};
use project::{
    ProjectPath,
    git_store::{CommitFile, Repository, RepositoryEvent, diff_buffer_list::DiffBase},
    worktree_store::WorktreeStoreEvent,
};
use ui::{Divider, prelude::*};
use util::ResultExt as _;
use workspace::{Item, Workspace, item::ItemEvent};

#[derive(Clone, Copy, PartialEq, Eq)]
enum BinaryFileKind {
    Image,
    Pdf,
}

impl BinaryFileKind {
    fn for_path(path: &RepoPath) -> Option<Self> {
        match path.extension()?.to_ascii_lowercase().as_str() {
            "pdf" => Some(Self::Pdf),
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff" | "ico" | "pnm"
            | "ppm" | "pgm" | "pbm" => Some(Self::Image),
            _ => None,
        }
    }
}

pub(crate) struct BinaryDiffView {
    path: RepoPath,
    pub(crate) before: BinaryPreview,
    pub(crate) after: BinaryPreview,
    pub(crate) actual_size: bool,
    focus_handle: FocusHandle,
    source: Option<(Entity<Repository>, DiffBase)>,
    content_hashes: Option<[u64; 2]>,
    loading: bool,
    error: Option<SharedString>,
    load_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl BinaryDiffView {
    pub(crate) fn supports(path: &RepoPath) -> bool {
        BinaryFileKind::for_path(path).is_some()
    }

    fn new(path: RepoPath, cx: &mut Context<Self>) -> Self {
        Self {
            path,
            before: BinaryPreview::Missing,
            after: BinaryPreview::Missing,
            actual_size: false,
            focus_handle: cx.focus_handle(),
            source: None,
            content_hashes: None,
            loading: false,
            error: None,
            load_task: Task::ready(()),
            _subscriptions: Vec::new(),
        }
    }

    pub(crate) fn load(file: CommitFile, cx: &mut App) -> Task<Entity<Self>> {
        cx.spawn(async move |cx| {
            let kind = BinaryFileKind::for_path(&file.path);
            let (before, after) = if let Some(kind) = kind {
                let before_present = file.old_text.is_some();
                let after_present = file.new_text.is_some();
                let before_bytes = file.old_binary.or_else(|| {
                    (!file.is_binary)
                        .then(|| file.old_text.map(String::into_bytes))
                        .flatten()
                });
                let after_bytes = file.new_binary.or_else(|| {
                    (!file.is_binary)
                        .then(|| file.new_text.map(String::into_bytes))
                        .flatten()
                });
                let before = BinaryPreview::load(before_bytes, before_present, kind, cx).await;
                let after = BinaryPreview::load(after_bytes, after_present, kind, cx).await;
                (before, after)
            } else {
                (
                    BinaryPreview::Error("No preview available for this file type".into()),
                    BinaryPreview::Missing,
                )
            };
            cx.new(|cx| {
                let mut view = Self::new(file.path, cx);
                view.before = before;
                view.after = after;
                view
            })
        })
    }

    pub(crate) fn clone_view(&self, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let mut view = Self::new(self.path.clone(), cx);
            view.before = self.before.clone_preview(cx);
            view.after = self.after.clone_preview(cx);
            view.actual_size = self.actual_size;
            view
        })
    }

    pub(crate) fn try_deploy(
        workspace: &mut Workspace,
        path: &RepoPath,
        repository: Entity<Repository>,
        base: DiffBase,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> bool {
        if !Self::supports(path) {
            return false;
        }
        let existing = workspace.items_of_type::<Self>(cx).find(|view| {
            let view = view.read(cx);
            view.path == *path
                && view
                    .source
                    .as_ref()
                    .is_some_and(|(source_repository, source_base)| {
                        source_repository == &repository && source_base == &base
                    })
        });
        if let Some(existing) = existing {
            existing.update(cx, |view, cx| view.reload(cx));
            workspace.activate_item(&existing, true, true, window, cx);
        } else {
            let project = workspace.project().clone();
            let worktree_store = project.read(cx).worktree_store();
            let project_path = repository.read(cx).repo_path_to_project_path(path, cx);
            let view = cx.new(|cx| {
                let mut view = Self::new(path.clone(), cx);
                // A binary file can change without changing its status or line counts.
                view._subscriptions = vec![
                    cx.subscribe(&repository, |this, _, event, cx| {
                        if matches!(
                            event,
                            RepositoryEvent::StatusesChanged | RepositoryEvent::HeadChanged
                        ) {
                            this.reload(cx);
                        }
                    }),
                    cx.subscribe(&worktree_store, move |this, _, event, cx| {
                        let Some(path) = &project_path else { return };
                        let changed = match event {
                            WorktreeStoreEvent::WorktreeUpdatedEntries(worktree_id, changes) => {
                                *worktree_id == path.worktree_id
                                    && changes
                                        .iter()
                                        .any(|(changed_path, _, _)| *changed_path == path.path)
                                    && this
                                        .source
                                        .as_ref()
                                        .is_some_and(|(_, base)| *base != DiffBase::Staged)
                            }
                            WorktreeStoreEvent::WorktreeUpdatedGitRepositories(worktree_id, _) => {
                                *worktree_id == path.worktree_id
                            }
                            _ => false,
                        };
                        if changed {
                            this.reload(cx);
                        }
                    }),
                ];
                view.source = Some((repository, base));
                view.reload(cx);
                view
            });
            workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
        }
        true
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let Some((repository, base)) = &self.source else {
            return;
        };
        let Some(kind) = BinaryFileKind::for_path(&self.path) else {
            return;
        };
        let repository = repository.clone();
        let base = base.clone();
        let path = self.path.clone();
        let previous_hashes = self.content_hashes;
        self.loading = true;
        self.error = None;
        self.load_task = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(200))
                .await;
            let content = repository.read_with(cx, |repository, cx| {
                repository.load_binary_diff(path, base, cx)
            });
            let result = match content.await {
                Ok((before, after)) => {
                    let (before, after, hashes) = cx
                        .background_spawn(async move {
                            let hashes = [&before, &after].map(|bytes| {
                                let mut hasher = DefaultHasher::new();
                                bytes.hash(&mut hasher);
                                hasher.finish()
                            });
                            (before, after, hashes)
                        })
                        .await;
                    let before_present = before.is_some();
                    let after_present = after.is_some();
                    let before = if previous_hashes.is_none_or(|previous| previous[0] != hashes[0])
                    {
                        Some(BinaryPreview::load(before, before_present, kind, cx).await)
                    } else {
                        None
                    };
                    let after = if previous_hashes.is_none_or(|previous| previous[1] != hashes[1]) {
                        Some(BinaryPreview::load(after, after_present, kind, cx).await)
                    } else {
                        None
                    };
                    Ok((before, after, hashes))
                }
                Err(error) => Err(error),
            };
            this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Ok((before, after, hashes)) => {
                        if let Some(before) = before {
                            this.before = before;
                        }
                        if let Some(after) = after {
                            this.after = after;
                        }
                        this.content_hashes = Some(hashes);
                    }
                    Err(error) => {
                        this.error = Some(format!("Cannot load binary diff: {error:#}").into())
                    }
                }
                cx.notify();
            })
            .log_err();
        });
        cx.notify();
    }
}

impl Focusable for BinaryDiffView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<()> for BinaryDiffView {}

impl Item for BinaryDiffView {
    type Event = ();
    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        format!(
            "{} ({})",
            self.path.file_name().unwrap_or(self.path.as_unix_str()),
            match self.source.as_ref().map(|(_, base)| base) {
                Some(DiffBase::Staged) => "staged",
                Some(DiffBase::Index) => "unstaged",
                _ => "diff",
            }
        )
        .into()
    }
    fn tab_tooltip_text(&self, _: &App) -> Option<SharedString> {
        Some(self.path.as_unix_str().to_owned().into())
    }
    fn to_item_events(_: &(), emit: &mut dyn FnMut(ItemEvent)) {
        emit(ItemEvent::UpdateTab);
    }
    fn active_project_path(&self, cx: &App) -> Option<ProjectPath> {
        self.source
            .as_ref()?
            .0
            .read(cx)
            .repo_path_to_project_path(&self.path, cx)
    }
}

impl Render for BinaryDiffView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (before_label, after_label) = match self.source.as_ref().map(|(_, base)| base) {
            Some(DiffBase::Head) => ("HEAD", "Working Tree"),
            Some(DiffBase::Index) => ("Index", "Working Tree"),
            Some(DiffBase::Staged) => ("HEAD", "Index"),
            _ => ("Before", "After"),
        };
        v_flex()
            .size_full()
            .min_h_0()
            .overflow_hidden()
            .track_focus(&self.focus_handle)
            .bg(cx.theme().colors().editor_background)
            .when(
                BinaryFileKind::for_path(&self.path) == Some(BinaryFileKind::Image),
                |this| {
                    this.child(
                        h_flex().px_3().py_1().child(
                            Button::new(
                                "image-size",
                                if self.actual_size {
                                    "Fit Images"
                                } else {
                                    "Actual Size"
                                },
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.actual_size = !this.actual_size;
                                cx.notify();
                            })),
                        ),
                    )
                },
            )
            .when(self.loading && self.content_hashes.is_none(), |this| {
                this.child(Label::new("Loading previews…").color(Color::Muted))
            })
            .when_some(self.error.clone(), |this, error| {
                this.child(div().p_3().child(Label::new(error).color(Color::Error)))
            })
            .when(
                (!self.loading || self.content_hashes.is_some()) && self.error.is_none(),
                |this| {
                    this.child(
                        h_flex()
                            .flex_1()
                            .min_h_0()
                            .w_full()
                            .child(self.before.render(before_label, self.actual_size, cx))
                            .child(Divider::vertical())
                            .child(self.after.render(after_label, self.actual_size, cx)),
                    )
                },
            )
    }
}

pub(crate) enum BinaryPreview {
    Missing,
    Pdf(Entity<pdf_viewer::PdfView>),
    Loaded {
        image: Arc<gpui::Image>,
        metadata: project::image_store::ImageMetadata,
    },
    Error(SharedString),
}

impl BinaryPreview {
    fn clone_preview(&self, cx: &mut App) -> Self {
        match self {
            Self::Missing => Self::Missing,
            Self::Error(error) => Self::Error(error.clone()),
            Self::Loaded { image, metadata } => Self::Loaded {
                image: image.clone(),
                metadata: *metadata,
            },
            Self::Pdf(view) => {
                Self::Pdf(view.update(cx, |view, cx| cx.new(|cx| view.clone_view(cx))))
            }
        }
    }

    async fn load(
        bytes: Option<Vec<u8>>,
        present: bool,
        kind: BinaryFileKind,
        cx: &mut gpui::AsyncApp,
    ) -> Self {
        let Some(bytes) = bytes else {
            return if present {
                Self::Error("Preview data is unavailable from this repository".into())
            } else {
                Self::Missing
            };
        };
        if kind == BinaryFileKind::Pdf {
            return match cx
                .update(|cx| pdf_viewer::PdfView::from_bytes(bytes, cx))
                .await
            {
                Ok(view) => Self::Pdf(view),
                Err(error) => Self::Error(format!("Cannot preview PDF: {error:#}").into()),
            };
        }
        cx.background_spawn(async move {
            match project::ImageItem::compute_metadata_from_bytes(&bytes).and_then(|metadata| {
                project::image_store::create_gpui_image(bytes).map(|image| (image, metadata))
            }) {
                Ok((image, metadata)) => Self::Loaded { image, metadata },
                Err(error) => Self::Error(format!("Cannot preview image: {error:#}").into()),
            }
        })
        .await
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
                    .when(!matches!(self, Self::Pdf(_)), |this| this.overflow_scroll())
                    .when(matches!(self, Self::Pdf(_)), |this| this.overflow_hidden())
                    .child(match self {
                        Self::Pdf(view) => div()
                            .size_full()
                            .debug_selector(move || format!("{label}-pdf"))
                            .child(view.clone())
                            .into_any_element(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use git::repository::repo_path;
    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use serde_json::json;
    use std::path::Path;
    use workspace::MultiWorkspace;

    fn init(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            language_model::init(cx);
            editor::init(cx);
            crate::init(cx);
        });
    }

    fn pdf() -> Vec<u8> {
        let mut pdf = "%PDF-1.7\n".to_owned();
        let mut offsets = Vec::new();
        for (index, object) in [
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Count 1 /Kids [3 0 R] >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 400] /Resources << >> >>",
        ]
        .iter()
        .enumerate()
        {
            offsets.push(pdf.len());
            pdf.push_str(&format!("{} 0 obj\n{object}\nendobj\n", index + 1));
        }
        let xref = pdf.len();
        pdf.push_str("xref\n0 4\n0000000000 65535 f \n");
        for offset in offsets {
            pdf.push_str(&format!("{offset:010} 00000 n \n"));
        }
        pdf.push_str(&format!(
            "trailer\n<< /Size 4 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
        ));
        pdf.into_bytes()
    }

    struct PreviewHost(Entity<BinaryDiffView>);
    impl Render for PreviewHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            self.0.clone()
        }
    }

    #[gpui::test]
    async fn pdf_previews_show_both_revisions_and_missing_sides(cx: &mut TestAppContext) {
        init(cx);
        for (before, after) in [(true, true), (false, true), (true, false)] {
            let view = cx
                .update(|cx| {
                    BinaryDiffView::load(
                        CommitFile {
                            path: repo_path("document.PDF"),
                            old_text: before.then(String::new),
                            new_text: after.then(String::new),
                            old_binary: before.then(pdf),
                            new_binary: after.then(pdf),
                            is_binary: true,
                        },
                        cx,
                    )
                })
                .await;
            view.read_with(cx, |view, cx| {
                assert_eq!(matches!(view.before, BinaryPreview::Pdf(_)), before);
                assert_eq!(matches!(view.after, BinaryPreview::Pdf(_)), after);
                assert!(!view.can_save(cx));
            });
            let split = view.update(cx, |view, cx| view.clone_view(cx));
            cx.read(|cx| {
                if let (BinaryPreview::Pdf(original), BinaryPreview::Pdf(clone)) =
                    (&view.read(cx).before, &split.read(cx).before)
                {
                    assert_ne!(original, clone);
                }
            });
            let (_, visual) = cx.add_window_view(|_, _| PreviewHost(view));
            visual.simulate_resize(gpui::size(px(1100.), px(800.)));
            visual.run_until_parked();
            for (label, present) in [("Before-pdf", before), ("After-pdf", after)] {
                let bounds = visual.debug_bounds(label);
                assert_eq!(bounds.is_some(), present);
                if let Some(bounds) = bounds {
                    assert!(bounds.size.width > px(300.) && bounds.size.height > px(300.));
                }
            }
        }
        for (kind, bytes, present) in [
            (BinaryFileKind::Pdf, Some(b"broken PDF".to_vec()), true),
            (BinaryFileKind::Image, Some(b"broken image".to_vec()), true),
            (BinaryFileKind::Pdf, None, true),
        ] {
            assert!(matches!(
                BinaryPreview::load(bytes, present, kind, &mut cx.to_async()).await,
                BinaryPreview::Error(_)
            ));
        }
    }

    #[gpui::test]
    async fn working_previews_refresh_and_keep_staging_views_distinct(cx: &mut TestAppContext) {
        init(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", json!({ ".git": {} })).await;
        fs.insert_file("/project/document.pdf", pdf()).await;
        let project = Project::test(fs.clone(), [Path::new("/project")], cx).await;
        let repository = project.read_with(cx, |project, cx| {
            project.active_repository(cx).expect("repository")
        });
        fs.with_git_state(Path::new("/project/.git"), true, |state| {
            state.head_contents.insert(repo_path("document.pdf"), pdf());
            state
                .index_contents
                .insert(repo_path("document.pdf"), pdf());
        })
        .expect("set PDF revisions");
        project
            .update(cx, |project, cx| project.git_scans_complete(cx))
            .await;
        let (multi_workspace, visual) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(visual, |multi, _| multi.workspace().clone());
        let mut views = Vec::new();
        for base in [DiffBase::Head, DiffBase::Index, DiffBase::Staged] {
            let view = workspace.update_in(visual, |workspace, window, cx| {
                assert!(BinaryDiffView::try_deploy(
                    workspace,
                    &repo_path("document.pdf"),
                    repository.clone(),
                    base,
                    window,
                    cx
                ));
                workspace
                    .active_item_as::<BinaryDiffView>(cx)
                    .expect("binary diff")
            });
            view.condition(visual, |view, _| !view.loading).await;
            view.read_with(visual, |view, _| {
                assert!(view.error.is_none());
                assert!(matches!(view.before, BinaryPreview::Pdf(_)));
                assert!(matches!(view.after, BinaryPreview::Pdf(_)));
            });
            assert!(!views.contains(&view));
            views.push(view);
        }
        fs.insert_file("/project/document.pdf", b"broken PDF".to_vec())
            .await;
        for view in views.iter().take(2) {
            view.condition(visual, |view, _| {
                matches!(view.after, BinaryPreview::Error(_))
            })
            .await;
        }
        views
            .last()
            .expect("staged view")
            .condition(visual, |view, _| !view.loading)
            .await;
        views
            .last()
            .expect("staged view")
            .read_with(visual, |view, _| {
                assert!(matches!(view.after, BinaryPreview::Pdf(_)))
            });

        // Keep the file modified so neither its Git status nor its line counts change.
        let mut changed_pdf = pdf();
        changed_pdf.extend_from_slice(b"\n% modified\n");
        fs.insert_file("/project/document.pdf", changed_pdf.clone())
            .await;
        for view in views.iter().take(2) {
            view.condition(visual, |view, _| {
                matches!(view.after, BinaryPreview::Pdf(_))
            })
            .await;
        }
        let staged = views.last().expect("staged view");
        let previous_after = staged.read_with(visual, |view, _| match &view.after {
            BinaryPreview::Pdf(view) => view.clone(),
            _ => panic!("staged PDF"),
        });
        fs.with_git_state(Path::new("/project/.git"), true, |state| {
            state
                .index_contents
                .insert(repo_path("document.pdf"), changed_pdf);
        })
        .expect("update index");
        staged.condition(visual, |view, _| matches!(&view.after, BinaryPreview::Pdf(view) if view != &previous_after)).await;

        let previous_after = staged.read_with(visual, |view, _| match &view.after {
            BinaryPreview::Pdf(view) => view.clone(),
            _ => panic!("staged PDF"),
        });
        fs.with_git_state(Path::new("/project/.git"), true, |state| {
            state
                .index_contents
                .insert(repo_path("document.pdf"), pdf());
        })
        .expect("change index again");
        staged.condition(visual, |view, _| matches!(&view.after, BinaryPreview::Pdf(view) if view != &previous_after)).await;
    }
}
