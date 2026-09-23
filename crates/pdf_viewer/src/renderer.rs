use std::{collections::VecDeque, sync::Arc};

use anyhow::{Context as _, Result, anyhow, ensure};
use async_channel::{Receiver, Sender};
use gpui::{BackgroundExecutor, RenderImage, Task};
use hayro::{
    RenderCache, RenderSettings,
    hayro_interpret::InterpreterSettings,
    hayro_syntax::{LoadPdfError, Pdf},
    vello_cpu::color::palette::css::WHITE,
};
use parking_lot::Mutex;

const MAX_PIXELS: f32 = 8_000_000.0;
const MAX_DIMENSION: f32 = 8192.0;
const MAX_PENDING_PAGES: usize = 16;

#[derive(Clone, Copy, Debug)]
pub struct PageSize {
    pub width: f32,
    pub height: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderKey {
    pub page: usize,
    pub width: u16,
    pub height: u16,
}

impl RenderKey {
    pub fn new(page: usize, size: PageSize, scale: f32) -> Self {
        let scale = scale
            .min(MAX_DIMENSION / size.width.max(size.height))
            .min((MAX_PIXELS / (size.width * size.height)).sqrt());
        Self {
            page,
            width: (size.width * scale).ceil().clamp(1.0, MAX_DIMENSION) as u16,
            height: (size.height * scale).ceil().clamp(1.0, MAX_DIMENSION) as u16,
        }
    }
}

#[derive(Default)]
struct RenderQueue {
    pending: VecDeque<RenderKey>,
    active: Option<RenderKey>,
}

pub struct RenderedPage {
    pub key: RenderKey,
    pub result: Result<Arc<RenderImage>>,
}

pub struct Renderer {
    pub sizes: Vec<PageSize>,
    pub results: Receiver<RenderedPage>,
    queue: Arc<Mutex<RenderQueue>>,
    wake: Sender<()>,
    _worker: Task<()>,
}

impl Renderer {
    pub async fn new(content: Vec<u8>, executor: BackgroundExecutor) -> Result<Self> {
        let (ready_sender, ready) = async_channel::bounded(1);
        let (result_sender, results) = async_channel::bounded(1);
        let (wake, wake_receiver) = async_channel::bounded(1);
        let queue = Arc::new(Mutex::new(RenderQueue::default()));
        let worker = executor.spawn_dedicated({
            let queue = queue.clone();
            move |_| async move {
                let document = match load_document(content) {
                    Ok(document) => document,
                    Err(error) => {
                        if ready_sender.send(Err(error)).await.is_err() {
                            return;
                        }
                        return;
                    }
                };
                let sizes = document
                    .pages()
                    .iter()
                    .map(|page| {
                        let (width, height) = page.render_dimensions();
                        PageSize { width, height }
                    })
                    .collect::<Vec<_>>();
                if ready_sender.send(Ok(sizes)).await.is_err() {
                    return;
                }
                let mut cache = RenderCache::new();
                let mut rendered_count = 0;
                while wake_receiver.recv().await.is_ok() {
                    loop {
                        let key = {
                            let mut queue = queue.lock();
                            queue.active = queue.pending.pop_front();
                            queue.active
                        };
                        let Some(key) = key else { break };
                        // Hayro's resource cache has no eviction API. Periodically release it
                        // so browsing a long document does not retain every decoded resource.
                        if rendered_count == 32 {
                            cache = RenderCache::new();
                            rendered_count = 0;
                        }
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            render_page(&document, &cache, key)
                        }))
                        .unwrap_or_else(|_| {
                            Err(anyhow!("The PDF renderer could not render this page"))
                        });
                        if result.is_err() {
                            cache = RenderCache::new();
                        }
                        rendered_count += 1;
                        if result_sender
                            .send(RenderedPage { key, result })
                            .await
                            .is_err()
                        {
                            return;
                        }
                        queue.lock().active = None;
                    }
                }
            }
        });
        let sizes = ready
            .recv()
            .await
            .context("PDF renderer stopped while opening the document")??;
        Ok(Self {
            sizes,
            results,
            queue,
            wake,
            _worker: worker,
        })
    }

    pub fn request(&self, key: RenderKey) {
        let mut queue = self.queue.lock();
        if queue.active == Some(key) {
            return;
        }
        queue.pending.retain(|pending| *pending != key);
        queue.pending.push_front(key);
        queue.pending.truncate(MAX_PENDING_PAGES);
        match self.wake.try_send(()) {
            Ok(()) | Err(async_channel::TrySendError::Full(())) => {}
            Err(async_channel::TrySendError::Closed(())) => {
                queue.pending.clear();
            }
        }
    }
}

fn load_document(content: Vec<u8>) -> Result<Pdf> {
    let document = Pdf::new(content).map_err(|error| match error {
        LoadPdfError::Decryption(_) => anyhow!(
            "This PDF is encrypted or requires a password. Open it in an external PDF viewer."
        ),
        LoadPdfError::Invalid => anyhow!("This file is not a valid PDF or is still being written"),
    })?;
    ensure!(!document.pages().is_empty(), "This PDF has no pages");
    for page in document.pages().iter() {
        let (width, height) = page.render_dimensions();
        ensure!(
            width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0,
            "This PDF contains an invalid page size"
        );
    }
    Ok(document)
}

fn render_page<'a>(
    document: &'a Pdf,
    cache: &RenderCache<'a>,
    key: RenderKey,
) -> Result<Arc<RenderImage>> {
    let page = document
        .pages()
        .get(key.page)
        .context("PDF page no longer exists")?;
    let (width, height) = page.render_dimensions();
    let pixmap = hayro::render(
        page,
        cache,
        &InterpreterSettings::default(),
        &RenderSettings {
            x_scale: f32::from(key.width) / width,
            y_scale: f32::from(key.height) / height,
            width: Some(key.width),
            height: Some(key.height),
            bg_color: WHITE,
        },
    );
    // An opaque paper background also makes premultiplied and straight alpha identical.
    let mut pixels = pixmap.data_as_u8_slice().to_vec();
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    let buffer = image::RgbaImage::from_raw(u32::from(key.width), u32::from(key.height), pixels)
        .context("PDF renderer returned an invalid pixel buffer")?;
    Ok(Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])))
}
