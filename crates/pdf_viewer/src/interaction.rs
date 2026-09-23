use hayro::hayro_interpret::{
    BlendMode, ClipPath, Context, Device, GlyphDrawMode, Image, InterpreterCache,
    InterpreterSettings, Paint, PathDrawMode, SoftMask, TransformExt,
    font::Glyph,
    hayro_syntax::{
        Pdf,
        object::{Array, Dict, Name, Object, Rect as PdfRect, String as PdfString},
    },
    interpret_page,
};
use kurbo::{Affine, BezPath, Rect};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PageRect {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

impl From<Rect> for PageRect {
    fn from(rect: Rect) -> Self {
        Self {
            x0: rect.x0 as f32,
            y0: rect.y0 as f32,
            x1: rect.x1 as f32,
            y1: rect.y1 as f32,
        }
    }
}

#[derive(Clone, Debug)]
pub struct TextGlyph {
    pub text: String,
    pub bounds: PageRect,
}

#[derive(Clone, Debug)]
pub enum LinkDestination {
    Url(String),
    Page(usize),
}

#[derive(Clone, Debug)]
pub struct PageLink {
    pub bounds: PageRect,
    pub destination: LinkDestination,
}

#[derive(Default)]
pub struct PageContent {
    pub glyphs: Vec<TextGlyph>,
    pub links: Vec<PageLink>,
}

struct TextDevice {
    glyphs: Vec<TextGlyph>,
}

impl<'a> Device<'a> for TextDevice {
    fn set_soft_mask(&mut self, _: Option<SoftMask<'a>>) {}
    fn set_blend_mode(&mut self, _: BlendMode) {}
    fn draw_path(&mut self, _: &BezPath, _: Affine, _: &Paint<'a>, _: &PathDrawMode) {}
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'a>>, _: BlendMode) {}
    fn draw_image(&mut self, _: Image<'a, '_>, _: Affine) {}
    fn pop_clip_path(&mut self) {}
    fn pop_transparency_group(&mut self) {}

    fn draw_glyph(
        &mut self,
        glyph: &Glyph<'a>,
        transform: Affine,
        glyph_transform: Affine,
        _: &Paint<'a>,
        _: &GlyphDrawMode,
    ) {
        let Some(unicode) = glyph.as_unicode() else {
            return;
        };
        let text = match unicode {
            hayro::hayro_interpret::hayro_cmap::BfString::Char(character) => character.to_string(),
            hayro::hayro_interpret::hayro_cmap::BfString::String(text) => text,
        };
        let advance = match glyph {
            Glyph::Outline(outline) => outline.advance_width().unwrap_or(500.),
            Glyph::Type3(_) => 500.,
        };
        let bounds = (transform * glyph_transform).transform_rect_bbox(Rect::new(
            0.,
            -200.,
            advance.max(1.) as f64,
            900.,
        ));
        if bounds.x0.is_finite()
            && bounds.y0.is_finite()
            && bounds.x1.is_finite()
            && bounds.y1.is_finite()
        {
            let bounds = bounds.into();
            if self
                .glyphs
                .last()
                .is_some_and(|previous| previous.text == text && previous.bounds == bounds)
            {
                return;
            }
            self.glyphs.push(TextGlyph { text, bounds });
        }
    }
}

pub fn extract_page_content(document: &Pdf, page_index: usize) -> PageContent {
    let Some(page) = document.pages().get(page_index) else {
        return PageContent::default();
    };
    let (width, height) = page.render_dimensions();
    let initial_transform = page.initial_transform(true).to_kurbo();
    let cache = InterpreterCache::new();
    let mut context = Context::new(
        initial_transform,
        Rect::new(0., 0., width as f64, height as f64),
        &cache,
        page.xref(),
        InterpreterSettings::default(),
    );
    let mut device = TextDevice { glyphs: Vec::new() };
    interpret_page(page, &mut context, &mut device);

    let links = page
        .raw()
        .get::<Array<'_>>(b"Annots")
        .into_iter()
        .flat_map(|annotations| annotations.iter::<Dict<'_>>())
        .filter_map(|annotation| {
            let subtype = annotation.get::<Name<'_>>(b"Subtype")?;
            if subtype.as_ref() != b"Link" {
                return None;
            }
            if annotation.get::<u32>(b"F").unwrap_or(0) & (2 | 32) != 0 {
                return None;
            }
            let rect = annotation.get::<PdfRect>(b"Rect")?;
            let action = annotation.get::<Dict<'_>>(b"A");
            let destination = action
                .as_ref()
                .and_then(|action| {
                    if action.get::<Name<'_>>(b"S")?.as_ref() == b"URI" {
                        let url = action.get::<PdfString<'_>>(b"URI")?;
                        Some(LinkDestination::Url(
                            String::from_utf8_lossy(url.as_bytes()).into_owned(),
                        ))
                    } else {
                        None
                    }
                })
                .or_else(|| {
                    let destination = action
                        .as_ref()
                        .and_then(|action| action.get::<Array<'_>>(b"D"))
                        .or_else(|| annotation.get::<Array<'_>>(b"Dest"))
                        .or_else(|| {
                            let name = action
                                .as_ref()
                                .and_then(|action| action.get::<Name<'_>>(b"D"))
                                .or_else(|| annotation.get::<Name<'_>>(b"Dest"));
                            let name = name
                                .as_ref()
                                .map(AsRef::as_ref)
                                .map(|name| name.to_vec())
                                .or_else(|| {
                                action
                                    .as_ref()
                                    .and_then(|action| action.get::<PdfString<'_>>(b"D"))
                                    .or_else(|| annotation.get::<PdfString<'_>>(b"Dest"))
                                    .map(|name| name.as_bytes().to_vec())
                            })?;
                            named_destination(document, &name)
                        })?;
                    destination_page(document, destination).map(LinkDestination::Page)
                })?;
            let bounds = initial_transform
                .transform_rect_bbox(Rect::new(rect.x0, rect.y0, rect.x1, rect.y1));
            Some(PageLink {
                bounds: bounds.into(),
                destination,
            })
        })
        .collect();

    PageContent {
        glyphs: device.glyphs,
        links,
    }
}

fn destination_page(document: &Pdf, destination: Array<'_>) -> Option<usize> {
    let page = destination.iter::<Dict<'_>>().next()?;
    let page_id = page.obj_id()?;
    document
        .pages()
        .iter()
        .position(|page| page.raw().obj_id() == Some(page_id))
}

fn named_destination<'a>(document: &'a Pdf, name: &[u8]) -> Option<Array<'a>> {
    let catalog = document.xref().get::<Dict<'_>>(document.xref().root_id())?;
    if let Some(destinations) = catalog.get::<Dict<'_>>(b"Dests") {
        if let Some(destination) = destinations.get::<Array<'_>>(name).or_else(|| {
            destinations
                .get::<Dict<'_>>(name)
                .and_then(|destination| destination.get::<Array<'_>>(b"D"))
        }) {
            return Some(destination);
        }
    }
    let mut nodes = vec![
        catalog
            .get::<Dict<'_>>(b"Names")?
            .get::<Dict<'_>>(b"Dests")?,
    ];
    let mut visited = 0;
    while let Some(node) = nodes.pop() {
        visited += 1;
        if visited > 1024 {
            break;
        }
        if let Some(names) = node.get::<Array<'_>>(b"Names") {
            let mut entries = names.flex_iter();
            while let Some(key) = entries.next::<PdfString<'_>>() {
                let value = entries.next::<Object<'_>>()?;
                if key.as_bytes() == name {
                    return match value {
                        Object::Array(destination) => Some(destination),
                        Object::Dict(destination) => destination.get::<Array<'_>>(b"D"),
                        _ => None,
                    };
                }
            }
        }
        if let Some(children) = node.get::<Array<'_>>(b"Kids") {
            nodes.extend(children.iter::<Dict<'_>>());
        }
    }
    None
}
