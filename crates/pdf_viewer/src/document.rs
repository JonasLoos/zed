use std::{collections::VecDeque, sync::Arc, time::Duration};

use anyhow::{Context as _, Result, anyhow};
use gpui::{
    App, AppContext, Context, Entity, Global, RenderImage, SharedString, Subscription, Task,
    WeakEntity,
};
use project::{Project, ProjectEntryId, ProjectPath};
use util::ResultExt as _;
use worktree::Worktree;

use crate::renderer::{PageSize, RenderKey, RenderedPage, Renderer};

const CACHE_BYTES: usize = 96 * 1024 * 1024;
const RELOAD_DELAY: Duration = Duration::from_millis(200);

#[derive(Default)]
struct Documents(Vec<(WeakEntity<Project>, WeakEntity<PdfDocument>)>);
impl Global for Documents {}

struct CachedPage {
    key: RenderKey,
    result: Result<Arc<RenderImage>, SharedString>,
}

impl CachedPage {
    fn byte_size(&self) -> usize {
        if self.result.is_ok() {
            usize::from(self.key.width) * usize::from(self.key.height) * 4
        } else {
            0
        }
    }
}

pub struct PdfDocument {
    pub path: ProjectPath,
    pub sizes: Vec<PageSize>,
    pub generation: u64,
    pub loading: bool,
    pub error: Option<SharedString>,
    pub deleted: bool,
    entry_id: Option<ProjectEntryId>,
    worktree: Entity<Worktree>,
    renderer: Option<Renderer>,
    cache: VecDeque<CachedPage>,
    cache_bytes: usize,
    _subscription: Subscription,
    reload_task: Task<()>,
    render_task: Task<()>,
}

impl gpui::EventEmitter<()> for PdfDocument {}

impl PdfDocument {
    pub fn open(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Result<Entity<Self>> {
        let existing = cx.default_global::<Documents>().0.clone();
        for (owner, document) in existing {
            if owner == project.downgrade()
                && let Some(document) = document.upgrade()
                && document.read(cx).path == *path
            {
                return Ok(document);
            }
        }
        let worktree = project
            .read(cx)
            .worktree_for_id(path.worktree_id, cx)
            .context("PDF worktree no longer exists")?;
        let entry_id = project
            .read(cx)
            .entry_for_path(path, cx)
            .map(|entry| entry.id);
        let document = cx.new(|cx| {
            let subscription = cx.subscribe(&worktree, |this: &mut Self, worktree, event, cx| {
                if let worktree::Event::UpdatedEntries(changes) = event {
                    if !changes.iter().any(|(path, entry_id, _)| {
                        *path == this.path.path || Some(*entry_id) == this.entry_id
                    }) {
                        return;
                    }
                    let snapshot = worktree.read(cx).snapshot();
                    let entry = this
                        .entry_id
                        .and_then(|id| snapshot.entry_for_id(id))
                        .or_else(|| snapshot.entry_for_path(&this.path.path));
                    this.deleted = entry.is_none();
                    if let Some(entry) = entry {
                        this.path.path = entry.path.clone();
                        this.entry_id = Some(entry.id);
                    }
                    this.reload(true, cx);
                }
            });
            cx.on_release(|this, cx| this.clear_cache(cx)).detach();
            let mut document = Self {
                path: path.clone(),
                sizes: Vec::new(),
                generation: 0,
                loading: true,
                error: None,
                deleted: false,
                entry_id,
                worktree,
                renderer: None,
                cache: VecDeque::new(),
                cache_bytes: 0,
                _subscription: subscription,
                reload_task: Task::ready(()),
                render_task: Task::ready(()),
            };
            document.reload(false, cx);
            document
        });
        let registry = cx.default_global::<Documents>();
        registry
            .0
            .retain(|(project, document)| project.is_upgradable() && document.is_upgradable());
        registry.0.push((project.downgrade(), document.downgrade()));
        Ok(document)
    }

    pub fn abs_path(&self, cx: &App) -> std::path::PathBuf {
        self.worktree
            .read(cx)
            .abs_path()
            .join(self.path.path.as_std_path())
    }

    pub fn reload(&mut self, debounce: bool, cx: &mut Context<Self>) {
        self.loading = true;
        let executor = cx.background_executor().clone();
        self.reload_task = cx.spawn(async move |this, cx| {
            if debounce {
                executor.timer(RELOAD_DELAY).await;
            }
            for attempt in 0..3 {
                let load = this.update(cx, |this, cx| {
                    this.worktree.update(cx, |worktree, cx| {
                        worktree.load_binary_file(&this.path.path, cx)
                    })
                });
                let Ok(load) = load else { return };
                let result = match load.await {
                    Ok(file) => Renderer::new(file.content, executor.clone())
                        .await
                        .map(|renderer| (file.file, renderer)),
                    Err(error) => Err(error),
                };
                match result {
                    Ok((file, renderer)) => {
                        this.update(cx, |this, cx| {
                            this.entry_id = file.entry_id;
                            this.deleted = false;
                            this.install_renderer(renderer, cx);
                        })
                        .log_err();
                        return;
                    }
                    Err(error) if attempt == 2 => {
                        this.update(cx, |this, cx| {
                            this.loading = false;
                            this.error = Some(format!("{error:#}").into());
                            cx.notify();
                        })
                        .log_err();
                    }
                    Err(_) => executor.timer(RELOAD_DELAY).await,
                }
            }
        });
        cx.notify();
    }

    fn install_renderer(&mut self, renderer: Renderer, cx: &mut Context<Self>) {
        self.clear_cache(cx);
        self.generation += 1;
        self.sizes = renderer.sizes.clone();
        self.loading = false;
        self.error = None;
        let results = renderer.results.clone();
        self.renderer = Some(renderer);
        self.render_task = cx.spawn(async move |this, cx| {
            while let Ok(page) = results.recv().await {
                if this
                    .update(cx, |this, cx| this.cache_page(page, cx))
                    .is_err()
                {
                    return;
                }
            }
            this.update(cx, |this, cx| {
                this.error = Some("The PDF renderer stopped. Reload to try again.".into());
                cx.notify();
            })
            .log_err();
        });
        cx.notify();
    }

    pub fn page(&mut self, key: RenderKey) -> Result<Option<Arc<RenderImage>>, SharedString> {
        if let Some(index) = self.cache.iter().position(|page| page.key == key)
            && let Some(page) = self.cache.remove(index)
        {
            let result = page.result.clone().map(Some);
            self.cache.push_back(page);
            return result;
        }
        if let Some(renderer) = &self.renderer {
            renderer.request(key);
        }
        Ok(self
            .cache
            .iter()
            .rev()
            .find(|page| page.key.page == key.page)
            .and_then(|page| page.result.as_ref().ok())
            .cloned())
    }

    fn cache_page(&mut self, page: RenderedPage, cx: &mut Context<Self>) {
        if let Some(index) = self.cache.iter().position(|cached| cached.key == page.key)
            && let Some(previous) = self.cache.remove(index)
        {
            self.cache_bytes -= previous.byte_size();
            if let Ok(image) = previous.result {
                cx.drop_image(image, None);
            }
        }
        let page = CachedPage {
            key: page.key,
            result: page.result.map_err(|error| format!("{error:#}").into()),
        };
        self.cache_bytes += page.byte_size();
        self.cache.push_back(page);
        while self.cache_bytes > CACHE_BYTES || self.cache.len() > 64 {
            if let Some(page) = self.cache.pop_front() {
                self.cache_bytes -= page.byte_size();
                if let Ok(image) = page.result {
                    cx.drop_image(image, None);
                }
            }
        }
        cx.notify();
    }

    fn clear_cache(&mut self, cx: &mut App) {
        for page in self.cache.drain(..) {
            if let Ok(image) = page.result {
                // On release, the closing window may still be borrowed by the caller.
                cx.defer(move |cx| cx.drop_image(image, None));
            }
        }
        self.cache_bytes = 0;
    }
}

impl project::ProjectItem for PdfDocument {
    fn try_open(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Option<Task<Result<Entity<Self>>>> {
        // A worktree rooted at a single file has an empty relative path.
        let absolute_path = project.read(cx).absolute_path(path, cx)?;
        if !absolute_path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
        {
            return None;
        }
        Some(Task::ready(if project.read(cx).is_local() {
            Self::open(project, path, cx)
        } else {
            Err(anyhow!(
                "PDF viewing is currently available only for local projects"
            ))
        }))
    }

    fn entry_id(&self, _: &App) -> Option<ProjectEntryId> {
        self.entry_id
    }
    fn project_path(&self, _: &App) -> Option<ProjectPath> {
        Some(self.path.clone())
    }
    fn is_dirty(&self) -> bool {
        false
    }
}
