mod document;
mod persistence;
mod renderer;

use std::path::Path;

use anyhow::{Context as _, Result};
use document::PdfDocument;
use file_icons::FileIcons;
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, ListAlignment,
    ListOffset, ListState, Render, SharedString, Subscription, Task, WeakEntity, Window, actions,
    canvas, img, list, px,
};
use persistence::PdfViewerDb;
use project::{Project, ProjectPath};
use renderer::{PageSize, RenderKey};
use ui::{Button, ButtonStyle, ScrollAxes, Scrollbars, WithScrollbar, prelude::*};
use util::ResultExt as _;
use workspace::{
    ItemId, Pane, Workspace, WorkspaceId, delete_unloaded_items,
    invalid_item_view::InvalidItemView,
    item::{Item, ItemBufferKind, ItemEvent, ProjectItem, SerializableItem},
};

actions!(
    pdf_viewer,
    [
        /// Zoom in the PDF.
        ZoomIn,
        /// Zoom out the PDF.
        ZoomOut,
        /// Fit PDF pages to the width of the pane.
        FitWidth,
        /// Show PDF pages at 100% zoom.
        ResetZoom,
        /// Go to the next PDF page.
        NextPage,
        /// Go to the previous PDF page.
        PreviousPage,
        /// Go to the first PDF page.
        FirstPage,
        /// Go to the last PDF page.
        LastPage,
        /// Reload the PDF from disk.
        Reload,
    ]
);

const PAGE_GAP: f32 = 16.0;
const MIN_ZOOM: f32 = 0.1;
const MAX_ZOOM: f32 = 8.0;

pub struct PdfView {
    document: Entity<PdfDocument>,
    path: ProjectPath,
    focus_handle: FocusHandle,
    list: ListState,
    zoom: Option<f32>,
    viewport_width: f32,
    generation: u64,
    sizes: Vec<PageSize>,
    current_page: usize,
    restored_position: Option<(usize, f32)>,
    _subscription: Subscription,
}

pub struct PdfViewEvent;
impl EventEmitter<PdfViewEvent> for PdfView {}

impl PdfView {
    fn new(document: Entity<PdfDocument>, cx: &mut Context<Self>) -> Self {
        let list = ListState::new(document.read(cx).sizes.len(), ListAlignment::Top, px(0.));
        let view = cx.weak_entity();
        list.set_scroll_handler(move |event, _, cx| {
            view.update(cx, |this, cx| {
                this.current_page = event.visible_range.start;
                cx.emit(PdfViewEvent);
                cx.notify();
            })
            .log_err();
        });
        let subscription = cx.observe(&document, |this: &mut Self, document, cx| {
            let generation = document.read(cx).generation;
            let path = document.read(cx).path.clone();
            let state_changed = generation != this.generation || path != this.path;
            if generation != this.generation {
                let position = this
                    .restored_position
                    .take()
                    .unwrap_or_else(|| this.position(cx));
                this.generation = generation;
                this.sizes = document.read(cx).sizes.clone();
                this.list.reset(this.sizes.len());
                this.restore_position(position, cx);
            }
            this.path = path;
            if state_changed {
                cx.emit(PdfViewEvent);
            }
            cx.notify();
        });
        Self {
            generation: document.read(cx).generation,
            sizes: document.read(cx).sizes.clone(),
            path: document.read(cx).path.clone(),
            document,
            focus_handle: cx.focus_handle(),
            list,
            zoom: None,
            viewport_width: 800.,
            current_page: 0,
            restored_position: None,
            _subscription: subscription,
        }
    }

    fn scale(&self, _cx: &App) -> f32 {
        self.zoom.unwrap_or_else(|| {
            let widest = self.sizes.iter().map(|page| page.width).fold(1.0, f32::max);
            ((self.viewport_width - PAGE_GAP * 2.) / widest).clamp(MIN_ZOOM, MAX_ZOOM)
        })
    }

    fn position(&self, cx: &App) -> (usize, f32) {
        let position = self.list.logical_scroll_top();
        let height = self
            .sizes
            .get(position.item_ix)
            .map(|page| page.height * self.scale(cx) + PAGE_GAP)
            .unwrap_or(1.);
        (
            position.item_ix,
            (f32::from(position.offset_in_item) / height).clamp(0., 1.),
        )
    }

    fn restore_position(&mut self, (page, fraction): (usize, f32), cx: &App) {
        let page = page.min(self.sizes.len().saturating_sub(1));
        let height = self
            .sizes
            .get(page)
            .map(|page| page.height * self.scale(cx) + PAGE_GAP)
            .unwrap_or(1.);
        self.current_page = page;
        self.list.scroll_to(ListOffset {
            item_ix: page,
            offset_in_item: px(height * fraction.clamp(0., 1.)),
        });
    }

    fn set_zoom(&mut self, zoom: Option<f32>, cx: &mut Context<Self>) {
        self.zoom = zoom.map(|zoom| zoom.clamp(MIN_ZOOM, MAX_ZOOM));
        self.list.remeasure();
        cx.emit(PdfViewEvent);
        cx.notify();
    }

    fn zoom_in(&mut self, _: &ZoomIn, _: &mut Window, cx: &mut Context<Self>) {
        self.set_zoom(Some(self.scale(cx) * 1.2), cx);
    }
    fn zoom_out(&mut self, _: &ZoomOut, _: &mut Window, cx: &mut Context<Self>) {
        self.set_zoom(Some(self.scale(cx) / 1.2), cx);
    }
    fn fit_width(&mut self, _: &FitWidth, _: &mut Window, cx: &mut Context<Self>) {
        self.set_zoom(None, cx);
    }
    fn reset_zoom(&mut self, _: &ResetZoom, _: &mut Window, cx: &mut Context<Self>) {
        self.set_zoom(Some(1.), cx);
    }
    fn go_to_page(&mut self, page: usize, cx: &mut Context<Self>) {
        self.restore_position((page, 0.), cx);
        cx.emit(PdfViewEvent);
        cx.notify();
    }
    fn next_page(&mut self, _: &NextPage, _: &mut Window, cx: &mut Context<Self>) {
        self.go_to_page(self.current_page.saturating_add(1), cx);
    }
    fn previous_page(&mut self, _: &PreviousPage, _: &mut Window, cx: &mut Context<Self>) {
        self.go_to_page(self.current_page.saturating_sub(1), cx);
    }
    fn first_page(&mut self, _: &FirstPage, _: &mut Window, cx: &mut Context<Self>) {
        self.go_to_page(0, cx);
    }
    fn last_page(&mut self, _: &LastPage, _: &mut Window, cx: &mut Context<Self>) {
        self.go_to_page(self.sizes.len().saturating_sub(1), cx);
    }
    fn reload(&mut self, _: &Reload, _: &mut Window, cx: &mut Context<Self>) {
        self.document
            .update(cx, |document, cx| document.reload(false, cx));
    }

    fn render_page(
        &mut self,
        page: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(size) = self.sizes.get(page).copied() else {
            return div().into_any_element();
        };
        let scale = self.scale(cx);
        let key = RenderKey::new(page, size, scale * window.scale_factor());
        let image = self.document.update(cx, |document, _| document.page(key));
        let page = div()
            .w(px(size.width * scale))
            .h(px(size.height * scale))
            .flex_none()
            .bg(gpui::white())
            .flex()
            .items_center()
            .justify_center();
        let page = match image {
            Ok(Some(image)) => page.child(img(image).size_full()),
            Ok(None) => page.child(Label::new("Rendering page…").color(Color::Muted)),
            Err(error) => page.child(Label::new(error).color(Color::Error)),
        };
        div()
            .w_full()
            .h(px(size.height * scale + PAGE_GAP))
            .flex_none()
            .flex()
            .justify_center()
            .child(page)
            .into_any_element()
    }
}

impl Focusable for PdfView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PdfView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let document = self.document.read(cx);
        let count = self.sizes.len();
        let error = document.error.clone();
        let loading = document.loading;
        let scale = self.scale(cx);
        let width = self
            .sizes
            .iter()
            .map(|page| page.width * scale + PAGE_GAP * 2.)
            .fold(self.viewport_width, f32::max);
        let view = cx.entity();
        let measure_view = cx.weak_entity();
        v_flex()
            .size_full()
            .track_focus(&self.focus_handle)
            .key_context("PdfViewer")
            .bg(cx.theme().colors().editor_background)
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::fit_width))
            .on_action(cx.listener(Self::reset_zoom))
            .on_action(cx.listener(Self::next_page))
            .on_action(cx.listener(Self::previous_page))
            .on_action(cx.listener(Self::first_page))
            .on_action(cx.listener(Self::last_page))
            .on_action(cx.listener(Self::reload))
            .on_pinch(cx.listener(|this, event: &gpui::PinchEvent, _, cx| {
                this.set_zoom(Some(this.scale(cx) * (1. + event.delta)), cx);
            }))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    window.focus(&this.focus_handle, cx);
                }),
            )
            .child(
                h_flex()
                    .w_full()
                    .flex_none()
                    .gap_2()
                    .p_2()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        Button::new("previous-page", "Previous")
                            .style(ButtonStyle::Subtle)
                            .disabled(count == 0 || self.current_page == 0)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.previous_page(&PreviousPage, window, cx)
                            })),
                    )
                    .child(Label::new(format!(
                        "{} / {count}",
                        if count == 0 { 0 } else { self.current_page + 1 }
                    )))
                    .child(
                        Button::new("next-page", "Next")
                            .style(ButtonStyle::Subtle)
                            .disabled(count == 0 || self.current_page + 1 >= count)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.next_page(&NextPage, window, cx)
                            })),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("zoom-out", "−")
                            .style(ButtonStyle::Subtle)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.zoom_out(&ZoomOut, window, cx)
                            })),
                    )
                    .child(
                        Button::new("reset-zoom", format!("{:.0}%", scale * 100.))
                            .style(ButtonStyle::Subtle)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.reset_zoom(&ResetZoom, window, cx)
                            })),
                    )
                    .child(
                        Button::new("zoom-in", "+")
                            .style(ButtonStyle::Subtle)
                            .on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.zoom_in(&ZoomIn, window, cx)
                                }),
                            ),
                    )
                    .child(
                        Button::new("fit-width", "Fit width")
                            .style(ButtonStyle::Subtle)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.fit_width(&FitWidth, window, cx)
                            })),
                    )
                    .child(
                        Button::new("reload", "Reload")
                            .style(ButtonStyle::Subtle)
                            .disabled(loading)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.reload(&Reload, window, cx)),
                            ),
                    ),
            )
            .when(loading, |this| {
                this.child(Label::new("Loading PDF…").color(Color::Muted))
            })
            .when_some(error, |this, error| {
                this.child(div().p_2().child(Label::new(error).color(Color::Error)))
            })
            .child(
                div()
                    .id("pdf-pages")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_x_scroll()
                    .child(
                        canvas(
                            move |bounds, _, cx| {
                                let width = f32::from(bounds.size.width);
                                cx.defer(move |cx| {
                                    measure_view
                                        .update(cx, |this, cx| {
                                            if width > 0.
                                                && (this.viewport_width - width).abs() > 0.5
                                            {
                                                this.viewport_width = width;
                                                this.list.remeasure();
                                                cx.notify();
                                            }
                                        })
                                        .log_err();
                                });
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .child(
                        list(self.list.clone(), move |page, window, cx| {
                            view.update(cx, |this, cx| this.render_page(page, window, cx))
                        })
                        .w(px(width))
                        .h_full(),
                    )
                    .custom_scrollbars(
                        Scrollbars::new(ScrollAxes::Vertical).tracked_scroll_handle(&self.list),
                        window,
                        cx,
                    ),
            )
    }
}

impl Item for PdfView {
    type Event = PdfViewEvent;

    fn tab_content_text(&self, _: usize, cx: &App) -> SharedString {
        self.document
            .read(cx)
            .abs_path(cx)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("PDF")
            .to_owned()
            .into()
    }
    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        Some(
            self.document
                .read(cx)
                .abs_path(cx)
                .to_string_lossy()
                .into_owned()
                .into(),
        )
    }
    fn tab_icon(&self, _: &Window, cx: &App) -> Option<Icon> {
        FileIcons::get_icon(&self.document.read(cx).abs_path(cx), cx).map(Icon::from_path)
    }
    fn to_item_events(_: &Self::Event, emit: &mut dyn FnMut(ItemEvent)) {
        emit(ItemEvent::UpdateTab);
    }
    fn for_each_project_item(
        &self,
        cx: &App,
        callback: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        callback(self.document.entity_id(), self.document.read(cx));
    }
    fn buffer_kind(&self, _: &App) -> ItemBufferKind {
        ItemBufferKind::Singleton
    }
    fn has_deleted_file(&self, cx: &App) -> bool {
        self.document.read(cx).deleted
    }
    fn can_split(&self) -> bool {
        true
    }
    fn clone_on_split(
        &self,
        _: Option<WorkspaceId>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>> {
        let position = self.position(cx);
        Task::ready(Some(cx.new(|cx| {
            let mut view = Self::new(self.document.clone(), cx);
            view.zoom = self.zoom;
            view.viewport_width = self.viewport_width;
            view.restore_position(position, cx);
            view
        })))
    }
}

impl ProjectItem for PdfView {
    type Item = PdfDocument;
    fn for_project_item(
        _: Entity<Project>,
        _: Option<&Pane>,
        item: Entity<Self::Item>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new(item, cx)
    }
    fn for_broken_project_item(
        path: &Path,
        is_local: bool,
        error: &anyhow::Error,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<InvalidItemView> {
        Some(InvalidItemView::new(path, is_local, error, window, cx))
    }
}

impl SerializableItem for PdfView {
    fn serialized_item_kind() -> &'static str {
        "PdfView"
    }

    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<ItemId>,
        _: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        delete_unloaded_items(
            alive_items,
            workspace_id,
            "pdf_viewers",
            &PdfViewerDb::global(cx),
            cx,
        )
    }

    fn deserialize(
        project: Entity<Project>,
        _: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let database = PdfViewerDb::global(cx);
        window.spawn(cx, async move |cx| {
            let (path, page, fraction, zoom) = database
                .get_state(item_id, workspace_id)?
                .context("No saved PDF view")?;
            let (worktree, path) = project
                .update(cx, |project, cx| {
                    project.find_or_create_worktree(path, false, cx)
                })
                .await?;
            cx.update(|_, cx| {
                let path = ProjectPath {
                    worktree_id: worktree.read(cx).id(),
                    path,
                };
                let document = PdfDocument::open(&project, &path, cx)?;
                Ok(cx.new(|cx| {
                    let mut view = Self::new(document, cx);
                    view.zoom = zoom
                        .filter(|zoom| zoom.is_finite())
                        .map(|zoom| (zoom as f32).clamp(MIN_ZOOM, MAX_ZOOM));
                    let position = (
                        page.max(0) as usize,
                        if fraction.is_finite() {
                            fraction as f32
                        } else {
                            0.
                        },
                    );
                    if view.document.read(cx).sizes.is_empty() {
                        view.restored_position = Some(position);
                    } else {
                        view.restore_position(position, cx);
                    }
                    view
                }))
            })?
        })
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: ItemId,
        _: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let path = self.document.read(cx).abs_path(cx);
        let (page, fraction) = self.restored_position.unwrap_or_else(|| self.position(cx));
        let zoom = self.zoom.map(f64::from);
        let database = PdfViewerDb::global(cx);
        Some(cx.background_spawn(async move {
            database
                .save_state(
                    item_id,
                    workspace_id,
                    path,
                    page as i64,
                    f64::from(fraction),
                    zoom,
                )
                .await
        }))
    }
    fn should_serialize(&self, _: &Self::Event) -> bool {
        true
    }
}

pub fn init(cx: &mut App) {
    workspace::register_project_item::<PdfView>(cx);
    workspace::register_serializable_item::<PdfView>(cx);
}

#[cfg(test)]
mod tests;
