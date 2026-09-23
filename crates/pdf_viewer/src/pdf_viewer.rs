mod document;
mod interaction;
mod persistence;
mod renderer;

use std::path::Path;

use anyhow::{Context as _, Result};
use document::PdfDocument;
use file_icons::FileIcons;
use gpui::{
    AnyElement, App, Bounds, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable,
    ListAlignment, ListOffset, ListState, MouseButton, Pixels, Point, Render, ScrollHandle,
    SharedString, Subscription, Task, WeakEntity, Window, actions, canvas, img, list, point, px,
};
use interaction::LinkDestination;
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
        /// Copy selected PDF text.
        CopySelection,
    ]
);

const PAGE_GAP: f32 = 16.0;
const MIN_ZOOM: f32 = 0.1;
const MAX_ZOOM: f32 = 8.0;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct TextPosition {
    page: usize,
    glyph: usize,
}

#[derive(Clone, Copy)]
struct TextSelection {
    start: TextPosition,
    end: TextPosition,
}

pub struct PdfView {
    document: Entity<PdfDocument>,
    path: ProjectPath,
    focus_handle: FocusHandle,
    list: ListState,
    horizontal_scroll: ScrollHandle,
    zoom: Option<f32>,
    viewport_width: f32,
    generation: u64,
    sizes: Vec<PageSize>,
    page_bounds: Vec<Option<Bounds<Pixels>>>,
    selection: Option<TextSelection>,
    selecting: bool,
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
                this.page_bounds.fill(None);
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
                this.page_bounds = vec![None; this.sizes.len()];
                this.selection = None;
                this.reset_list(cx);
                this.restore_position(position, cx);
            }
            this.path = path;
            if state_changed {
                cx.emit(PdfViewEvent);
            }
            cx.notify();
        });
        let view = Self {
            generation: document.read(cx).generation,
            sizes: document.read(cx).sizes.clone(),
            page_bounds: vec![None; document.read(cx).sizes.len()],
            selection: None,
            selecting: false,
            path: document.read(cx).path.clone(),
            document,
            focus_handle: cx.focus_handle(),
            list,
            horizontal_scroll: ScrollHandle::new(),
            zoom: None,
            viewport_width: 800.,
            current_page: 0,
            restored_position: None,
            _subscription: subscription,
        };
        view.reset_list(cx);
        view
    }

    fn scale(&self, _cx: &App) -> f32 {
        self.zoom.unwrap_or_else(|| {
            let widest = self.sizes.iter().map(|page| page.width).fold(1.0, f32::max);
            ((self.viewport_width - PAGE_GAP * 2.) / widest).clamp(MIN_ZOOM, MAX_ZOOM)
        })
    }

    fn content_width(&self, scale: f32) -> f32 {
        self.sizes
            .iter()
            .map(|page| page.width * scale + PAGE_GAP * 2.)
            .fold(self.viewport_width, f32::max)
    }

    fn reset_list(&self, cx: &App) {
        let scale = self.scale(cx);
        self.list.reset_with_item_heights(
            self.sizes
                .iter()
                .map(|page| px(page.height * scale + PAGE_GAP)),
        );
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
        let position = self.position(cx);
        let old_width = self.content_width(self.scale(cx));
        let old_center = -f32::from(self.horizontal_scroll.offset().x) + self.viewport_width / 2.;
        self.zoom = zoom.map(|zoom| zoom.clamp(MIN_ZOOM, MAX_ZOOM));
        let new_width = self.content_width(self.scale(cx));
        let new_scroll = (old_center / old_width * new_width - self.viewport_width / 2.)
            .clamp(0., (new_width - self.viewport_width).max(0.));
        self.horizontal_scroll
            .set_offset(point(px(-new_scroll), px(0.)));
        self.page_bounds.fill(None);
        self.reset_list(cx);
        self.restore_position(position, cx);
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

    fn nearest_glyph(&self, position: Point<Pixels>, cx: &App) -> Option<(TextPosition, f32)> {
        let scale = self.scale(cx);
        let document = self.document.read(cx);
        self.page_bounds
            .iter()
            .enumerate()
            .filter_map(|(page, bounds)| {
                let bounds = bounds.as_ref()?;
                let content = document.content(page)?;
                Some((page, bounds, content))
            })
            .flat_map(|(page, bounds, content)| {
                content.glyphs.iter().enumerate().map(move |(glyph, item)| {
                    let left = bounds.left() + px(item.bounds.x0 * scale);
                    let right = bounds.left() + px(item.bounds.x1 * scale);
                    let top = bounds.top() + px(item.bounds.y0 * scale);
                    let bottom = bounds.top() + px(item.bounds.y1 * scale);
                    let dx = (left - position.x).max(px(0.)) + (position.x - right).max(px(0.));
                    let dy = (top - position.y).max(px(0.)) + (position.y - bottom).max(px(0.));
                    (TextPosition { page, glyph }, f32::from(dx + dy))
                })
            })
            .min_by(|left, right| left.1.total_cmp(&right.1))
    }

    fn copy_selection(&mut self, _: &CopySelection, _: &mut Window, cx: &mut Context<Self>) {
        let Some(selection) = self.selection else {
            return;
        };
        if selection.start == selection.end {
            return;
        }
        let (start, end) = if selection.start <= selection.end {
            (selection.start, selection.end)
        } else {
            (selection.end, selection.start)
        };
        let document = self.document.read(cx);
        let mut text = String::new();
        for page in start.page..=end.page {
            let Some(content) = document.content(page) else {
                continue;
            };
            if !text.is_empty() {
                text.push('\n');
            }
            let first = if page == start.page { start.glyph } else { 0 };
            let last = if page == end.page {
                end.glyph + 1
            } else {
                content.glyphs.len()
            };
            for glyph in content.glyphs.get(first..last).into_iter().flatten() {
                text.push_str(&glyph.text);
            }
        }
        if !text.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn is_selected(&self, page: usize, glyph: usize) -> bool {
        let Some(selection) = self.selection else {
            return false;
        };
        if selection.start == selection.end {
            return false;
        }
        let (start, end) = if selection.start <= selection.end {
            (selection.start, selection.end)
        } else {
            (selection.end, selection.start)
        };
        (start..=end).contains(&TextPosition { page, glyph })
    }

    fn render_page(
        &mut self,
        page_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(size) = self.sizes.get(page_index).copied() else {
            return div().into_any_element();
        };
        let scale = self.scale(cx);
        let key = RenderKey::new(page_index, size, scale * window.scale_factor());
        let image = self.document.update(cx, |document, _| document.page(key));
        let content = self.document.read(cx).content(page_index).cloned();
        let page = div()
            .relative()
            .w(px(size.width * scale))
            .h(px(size.height * scale))
            .flex_none()
            .bg(gpui::white())
            .flex()
            .items_center()
            .justify_center()
            .cursor_text();
        let mut page = match image {
            Ok(Some(image)) => page.child(img(image).size_full()),
            Ok(None) => page.child(Label::new("Rendering page…").color(Color::Muted)),
            Err(error) => page.child(Label::new(error).color(Color::Error)),
        };
        let measure_view = cx.weak_entity();
        page = page.child(
            canvas(
                move |bounds, _, cx| {
                    cx.defer(move |cx| {
                        measure_view
                            .update(cx, |view, _| {
                                if let Some(slot) = view.page_bounds.get_mut(page_index) {
                                    *slot = Some(bounds);
                                }
                            })
                            .log_err();
                    });
                },
                |_, _, _, _| {},
            )
            .absolute()
            .size_full(),
        );
        if let Some(content) = content {
            for (glyph, text) in content.glyphs.iter().enumerate() {
                if self.is_selected(page_index, glyph) {
                    let bounds = text.bounds;
                    page = page.child(
                        div()
                            .absolute()
                            .left(px(bounds.x0 * scale))
                            .top(px(bounds.y0 * scale))
                            .w(px((bounds.x1 - bounds.x0) * scale))
                            .h(px((bounds.y1 - bounds.y0) * scale))
                            .bg(cx.theme().colors().element_selection_background),
                    );
                }
            }
            for (index, link) in content.links.iter().enumerate() {
                let bounds = link.bounds;
                let destination = link.destination.clone();
                let view = cx.weak_entity();
                page = page.child(
                    div()
                        .id((
                            gpui::ElementId::from(("pdf-link", page_index)),
                            index.to_string(),
                        ))
                        .absolute()
                        .left(px(bounds.x0 * scale))
                        .top(px(bounds.y0 * scale))
                        .w(px((bounds.x1 - bounds.x0) * scale))
                        .h(px((bounds.y1 - bounds.y0) * scale))
                        .cursor_pointer()
                        .on_click(move |_, _, cx| match &destination {
                            LinkDestination::Url(url) => cx.open_url(url),
                            LinkDestination::Page(page) => {
                                view.update(cx, |view, cx| view.go_to_page(*page, cx))
                                    .log_err();
                            }
                        }),
                );
            }
        }
        let view = cx.weak_entity();
        let page = page.on_mouse_down(MouseButton::Left, move |event, window, cx| {
            view.update(cx, |view, cx| {
                window.focus(&view.focus_handle, cx);
                if let Some((position, distance)) = view.nearest_glyph(event.position, cx)
                    && position.page == page_index
                    && distance <= 8.
                {
                    view.selection = Some(TextSelection {
                        start: position,
                        end: position,
                    });
                    view.selecting = true;
                } else {
                    view.selection = None;
                    view.selecting = false;
                }
                cx.notify();
            })
            .log_err();
        });
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
        let width = self.content_width(scale);
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
            .on_action(cx.listener(Self::copy_selection))
            .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, _, cx| {
                if this.selecting && event.pressed_button == Some(MouseButton::Left) {
                    let end = this
                        .nearest_glyph(event.position, cx)
                        .map(|(position, _)| position);
                    if let (Some(selection), Some(end)) = (&mut this.selection, end)
                        && selection.end != end
                    {
                        selection.end = end;
                        cx.notify();
                    }
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.selecting = false;
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.selecting = false;
                }),
            )
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
                    .child(
                        div()
                            .id("pdf-horizontal-scroll")
                            .size_full()
                            .overflow_x_scroll()
                            .track_scroll(&self.horizontal_scroll)
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
                                                        let position = this.position(cx);
                                                        this.viewport_width = width;
                                                        this.page_bounds.fill(None);
                                                        this.reset_list(cx);
                                                        this.restore_position(position, cx);
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
                            ),
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
            view.reset_list(cx);
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
                    view.reset_list(cx);
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
