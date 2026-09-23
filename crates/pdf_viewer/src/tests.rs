use super::*;
use fs::{FakeFs, Fs as _};
use gpui::{
    BackgroundExecutor, Modifiers, PinchEvent, ScrollDelta, ScrollWheelEvent, TestAppContext,
};
use renderer::Renderer;
use std::{path::Path, sync::Arc};
use util::rel_path::rel_path;

fn test_pdf(pages: &[(u32, u32, &str)]) -> Vec<u8> {
    test_pdf_with_attributes(pages, "", false)
}

fn test_pdf_with_attributes(pages: &[(u32, u32, &str)], attributes: &str, text: bool) -> Vec<u8> {
    test_pdf_with_extra_objects(pages, attributes, text, "", &[])
}

fn test_pdf_with_extra_objects(
    pages: &[(u32, u32, &str)],
    attributes: &str,
    text: bool,
    catalog_attributes: &str,
    extra_objects: &[&str],
) -> Vec<u8> {
    let mut objects = vec![
        format!("<< /Type /Catalog /Pages 2 0 R {catalog_attributes} >>"),
        format!(
            "<< /Type /Pages /Count {} /Kids [{}] >>",
            pages.len(),
            (0..pages.len())
                .map(|index| format!("{} 0 R", 3 + index * 2))
                .collect::<Vec<_>>()
                .join(" ")
        ),
    ];
    for (index, (width, height, color)) in pages.iter().enumerate() {
        objects.push(format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {width} {height}] {attributes} /Resources << /Font << /F1 << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >> >> /Contents {} 0 R >>", 4 + index * 2));
        let mut content = format!("{color} rg 0 0 {width} {height} re f");
        if text {
            content.push_str(" 0 0 0 rg BT /F1 20 Tf 10 30 Td (Native PDF preview) Tj ET");
        }
        objects.push(format!(
            "<< /Length {} >>\nstream\n{content}\nendstream",
            content.len()
        ));
    }
    objects.extend(extra_objects.iter().map(|object| (*object).to_owned()));
    let mut pdf = "%PDF-1.7\n".to_owned();
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.push_str(&format!("{} 0 obj\n{object}\nendobj\n", index + 1));
    }
    let xref = pdf.len();
    pdf.push_str(&format!(
        "xref\n0 {}\n0000000000 65535 f \n",
        objects.len() + 1
    ));
    for offset in offsets {
        pdf.push_str(&format!("{offset:010} 00000 n \n"));
    }
    pdf.push_str(&format!(
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        objects.len() + 1
    ));
    pdf.into_bytes()
}

#[test]
fn extracts_text_and_external_and_internal_links() {
    let pdf = hayro::hayro_syntax::Pdf::new(test_pdf_with_extra_objects(
        &[(300, 200, "1 1 1"), (300, 200, "1 1 1")],
        "/Annots [7 0 R 8 0 R]",
        true,
        "",
        &[
            "<< /Type /Annot /Subtype /Link /Rect [10 10 100 40] /A << /S /URI /URI (https://example.com) >> >>",
            "<< /Type /Annot /Subtype /Link /Rect [10 50 100 80] /A << /S /GoTo /D [5 0 R /Fit] >> >>",
        ],
    ))
    .expect("PDF should load");
    let content = interaction::extract_page_content(&pdf, 0);
    let text = content
        .glyphs
        .iter()
        .map(|glyph| glyph.text.as_str())
        .collect::<String>();
    assert!(text.contains("Native PDF preview"), "{text}");
    assert!(content.glyphs.iter().all(|glyph| {
        glyph.bounds.x0 >= 0.
            && glyph.bounds.y0 >= 0.
            && glyph.bounds.x1 <= 300.
            && glyph.bounds.y1 <= 200.
    }));
    assert!(matches!(
        &content.links[0].destination,
        interaction::LinkDestination::Url(url) if url == "https://example.com"
    ));
    assert!(matches!(
        content.links[1].destination,
        interaction::LinkDestination::Page(1)
    ));
}

#[test]
fn resolves_named_pdf_destinations() {
    let pdf = hayro::hayro_syntax::Pdf::new(test_pdf_with_extra_objects(
        &[(300, 200, "1 1 1"), (300, 200, "1 1 1")],
        "/Annots [7 0 R 8 0 R]",
        false,
        "/Dests << /section [5 0 R /Fit] >> /Names << /Dests << /Names [(named) [5 0 R /Fit]] >> >>",
        &[
            "<< /Type /Annot /Subtype /Link /Rect [10 10 100 40] /A << /S /GoTo /D /section >> >>",
            "<< /Type /Annot /Subtype /Link /Rect [10 50 100 80] /Dest (named) >>",
        ],
    ))
    .expect("PDF should load");
    let content = interaction::extract_page_content(&pdf, 0);
    assert_eq!(content.links.len(), 2);
    assert!(
        content
            .links
            .iter()
            .all(|link| { matches!(link.destination, interaction::LinkDestination::Page(1)) })
    );
}

#[gpui::test]
async fn pinch_zooms_around_pointer(cx: &mut TestAppContext) {
    let (fs, _project, document) = open_document(cx).await;
    let generation = cx.read(|cx| document.read(cx).generation);
    fs.insert_file(
        "/root/document.pdf",
        test_pdf(&[(800, 600, "1 0 0"), (800, 600, "0 0 1")]),
    )
    .await;
    document
        .condition(cx, move |document, _| document.generation > generation)
        .await;
    let (view, visual) = cx.add_window_view(|_, cx| PdfView::new(document.clone(), cx));
    visual.run_until_parked();
    let initial_scroll_range = visual.read(|cx| view.read(cx).list.max_offset_for_scrollbar().y);
    assert!(initial_scroll_range > px(100.));
    let (pointer, page_position) = visual.read(|cx| {
        let view = view.read(cx);
        let bounds = view.page_bounds[0].expect("first page bounds");
        let pointer = point(
            bounds.left() + bounds.size.width * 0.6,
            bounds.top() + bounds.size.height * 0.3,
        );
        let position = point(
            f32::from(pointer.x - bounds.left()) / view.scale(cx),
            f32::from(pointer.y - bounds.top()) / view.scale(cx),
        );
        (pointer, position)
    });
    visual.simulate_event(PinchEvent {
        position: pointer,
        delta: 0.5,
        ..Default::default()
    });
    visual.run_until_parked();
    visual.read(|cx| {
        let view = view.read(cx);
        let bounds = view.page_bounds[0].expect("zoomed page bounds");
        let mapped = point(
            bounds.left() + px(page_position.x * view.scale(cx)),
            bounds.top() + px(page_position.y * view.scale(cx)),
        );
        assert!((f32::from(mapped.x - pointer.x)).abs() < 2.);
        assert!((f32::from(mapped.y - pointer.y)).abs() < 2.);
    });
}

#[gpui::test]
async fn zoomed_view_scrolls_through_all_pages(cx: &mut TestAppContext) {
    let (fs, _project, document) = open_document(cx).await;
    let generation = cx.read(|cx| document.read(cx).generation);
    fs.insert_file(
        "/root/document.pdf",
        test_pdf(&[
            (4000, 1600, "1 0 0"),
            (4000, 1600, "0 1 0"),
            (4000, 1600, "0 0 1"),
        ]),
    )
    .await;
    document
        .condition(cx, move |document, _| document.generation > generation)
        .await;
    let (view, visual) = cx.add_window_view(|_, cx| PdfView::new(document, cx));
    visual.run_until_parked();
    let bounds = visual.read(|cx| view.read(cx).horizontal_scroll.bounds());
    let pointer = bounds.center();
    view.update_in(visual, |view, _, cx| {
        view.set_zoom(Some(view.scale(cx) * 1.5), pointer, cx)
    });
    visual.update(|window, cx| {
        let _ = window.draw(cx);
    });
    let horizontal_offset = visual.read(|cx| {
        let view = view.read(cx);
        assert!(
            view.list.max_offset_for_scrollbar().y > px(view.sizes[0].height * view.scale(cx) * 2.),
            "scrollbar should cover all pages"
        );
        view.horizontal_scroll.offset().x
    });
    visual.simulate_event(ScrollWheelEvent {
        position: pointer,
        delta: ScrollDelta::Pixels(point(px(0.), px(-3000.))),
        ..Default::default()
    });
    visual.update(|window, cx| {
        let _ = window.draw(cx);
    });
    visual.read(|cx| {
        let view = view.read(cx);
        assert!(view.list.logical_scroll_top().item_ix >= 1);
        assert_eq!(view.horizontal_scroll.offset().x, horizontal_offset);
    });
}

#[gpui::test]
async fn drag_selects_pdf_text_and_copies_it(cx: &mut TestAppContext) {
    let (fs, _project, document) = open_document(cx).await;
    let generation = cx.read(|cx| document.read(cx).generation);
    fs.insert_file(
        "/root/document.pdf",
        test_pdf_with_attributes(&[(300, 200, "1 1 1")], "", true),
    )
    .await;
    document
        .condition(cx, move |document, _| document.generation > generation)
        .await;
    let (view, visual) = cx.add_window_view(|_, cx| PdfView::new(document.clone(), cx));
    visual.run_until_parked();
    let (start, end) = visual.read(|cx| {
        let view = view.read(cx);
        let bounds = view.page_bounds[0].expect("page bounds");
        let content = document.read(cx).content(0).expect("text content");
        let scale = view.scale(cx);
        let point_for = |index: usize| {
            let glyph = &content.glyphs[index];
            point(
                bounds.left() + px((glyph.bounds.x0 + glyph.bounds.x1) * scale / 2.),
                bounds.top() + px((glyph.bounds.y0 + glyph.bounds.y1) * scale / 2.),
            )
        };
        (point_for(0), point_for(5))
    });
    visual.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
    visual.simulate_mouse_move(end, MouseButton::Left, Modifiers::none());
    visual.simulate_mouse_up(end, MouseButton::Left, Modifiers::none());
    visual.dispatch_action(CopySelection);
    assert_eq!(
        visual.read_from_clipboard().and_then(|item| item.text()),
        Some("Native".to_owned())
    );
}

#[gpui::test]
async fn clicks_pdf_links(cx: &mut TestAppContext) {
    let (fs, _project, document) = open_document(cx).await;
    let generation = cx.read(|cx| document.read(cx).generation);
    fs.insert_file(
        "/root/document.pdf",
        test_pdf_with_extra_objects(
            &[(300, 200, "1 1 1"), (300, 200, "1 1 1")],
            "/Annots [7 0 R 8 0 R]",
            false,
            "",
            &[
                "<< /Type /Annot /Subtype /Link /Rect [10 100 100 130] /A << /S /URI /URI (https://example.com) >> >>",
                "<< /Type /Annot /Subtype /Link /Rect [10 140 100 170] /A << /S /GoTo /D [5 0 R /Fit] >> >>",
            ],
        ),
    )
    .await;
    document
        .condition(cx, move |document, _| document.generation > generation)
        .await;
    let (view, visual) = cx.add_window_view(|_, cx| PdfView::new(document.clone(), cx));
    visual.run_until_parked();
    visual.update(|window, cx| {
        let _ = window.draw(cx);
    });
    let links = visual.read(|cx| {
        let view = view.read(cx);
        let bounds = view.page_bounds[0].expect("page bounds");
        let scale = view.scale(cx);
        document
            .read(cx)
            .content(0)
            .expect("link content")
            .links
            .iter()
            .map(|link| {
                point(
                    bounds.left() + px((link.bounds.x0 + link.bounds.x1) * scale / 2.),
                    bounds.top() + px((link.bounds.y0 + link.bounds.y1) * scale / 2.),
                )
            })
            .collect::<Vec<_>>()
    });
    visual.simulate_click(links[0], Modifiers::none());
    assert_eq!(visual.opened_url(), Some("https://example.com".to_owned()));
    visual.simulate_click(links[1], Modifiers::none());
    visual.read(|cx| assert_eq!(view.read(cx).current_page, 1));
}

#[gpui::test]
async fn renders_multiple_pages_in_bgra(executor: BackgroundExecutor) {
    let renderer = Renderer::new(
        test_pdf(&[(100, 200, "1 0 0"), (200, 100, "0 0 1")]),
        executor,
    )
    .await
    .expect("PDF should load");
    assert_eq!(renderer.sizes.len(), 2);
    for (page, expected) in [(0, [0, 0, 255, 255]), (1, [255, 0, 0, 255])] {
        let key = RenderKey::new(page, renderer.sizes[page], 2.);
        renderer.request(key);
        let result = renderer
            .results
            .recv()
            .await
            .expect("renderer should respond");
        assert_eq!(result.key, key);
        let image = result.result.expect("page should render");
        let bytes = image.as_bytes(0).expect("image should have a frame");
        assert_eq!(
            bytes.len(),
            usize::from(key.width) * usize::from(key.height) * 4
        );
        assert!(bytes.chunks_exact(4).all(|pixel| pixel == expected));
    }
}

#[gpui::test]
async fn invalid_pdf_reports_an_error(executor: BackgroundExecutor) {
    let result = Renderer::new(b"not a PDF".to_vec(), executor).await;
    assert!(
        result
            .err()
            .expect("invalid PDF should fail")
            .to_string()
            .contains("not a valid PDF")
    );
}

#[gpui::test]
async fn renders_standard_fonts_and_rotated_crop_boxes(executor: BackgroundExecutor) {
    let pdf = test_pdf_with_attributes(
        &[(300, 200, "1 1 1")],
        "/CropBox [0 0 250 100] /Rotate 90",
        true,
    );
    let metadata = interaction::extract_page_content(
        &hayro::hayro_syntax::Pdf::new(pdf.clone()).expect("PDF should parse"),
        0,
    );
    assert!(!metadata.glyphs.is_empty());
    assert!(metadata.glyphs.iter().all(|glyph| {
        glyph.bounds.x0 >= 0.
            && glyph.bounds.y0 >= 0.
            && glyph.bounds.x1 <= 100.
            && glyph.bounds.y1 <= 250.
    }));
    let renderer = Renderer::new(pdf, executor).await.expect("PDF should load");
    assert_eq!(renderer.sizes[0].width, 100.);
    assert_eq!(renderer.sizes[0].height, 250.);
    renderer.request(RenderKey::new(0, renderer.sizes[0], 1.));
    let image = renderer
        .results
        .recv()
        .await
        .expect("renderer should respond")
        .result
        .expect("page should render");
    let bytes = image.as_bytes(0).expect("image frame");
    assert!(bytes.chunks_exact(4).all(|pixel| pixel[3] == 255));
    assert!(
        bytes.chunks_exact(4).filter(|pixel| pixel[0] < 128).count() > 100,
        "the standard font should produce visible text on the white page"
    );
}

#[test]
fn raster_dimensions_are_bounded() {
    for size in [
        PageSize {
            width: 612.,
            height: 792.,
        },
        PageSize {
            width: 100_000.,
            height: 50.,
        },
    ] {
        let key = RenderKey::new(0, size, 1000.);
        assert!(key.width <= 8192 && key.height <= 8192);
        assert!(u32::from(key.width) * u32::from(key.height) <= 24_010_000);
    }
    let high_zoom = RenderKey::new(
        0,
        PageSize {
            width: 612.,
            height: 792.,
        },
        16.,
    );
    assert!(high_zoom.width > 4_000 && high_zoom.height > 5_000);
}

async fn open_document(
    cx: &mut TestAppContext,
) -> (Arc<FakeFs>, Entity<Project>, Entity<PdfDocument>) {
    cx.update(|cx| {
        let settings = settings::SettingsStore::test(cx);
        cx.set_global(settings);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
    });
    let fs = FakeFs::new(cx.executor());
    fs.create_dir(Path::new("/root"))
        .await
        .expect("create root");
    fs.insert_file(
        "/root/document.pdf",
        test_pdf(&[(100, 200, "1 0 0"), (200, 100, "0 0 1")]),
    )
    .await;
    let project = Project::test(fs.clone(), [Path::new("/root")], cx).await;
    let document = cx.update(|cx| {
        let worktree_id = project
            .read(cx)
            .worktrees(cx)
            .next()
            .expect("test worktree")
            .read(cx)
            .id();
        PdfDocument::open(
            &project,
            &ProjectPath {
                worktree_id,
                path: rel_path("document.pdf").into(),
            },
            cx,
        )
        .expect("open PDF")
    });
    document
        .condition(cx, |document, _| !document.loading)
        .await;
    cx.read(|cx| assert!(document.read(cx).error.is_none()));
    (fs, project, document)
}

#[gpui::test(iterations = 5)]
async fn reload_preserves_document_on_failure_and_recovers(cx: &mut TestAppContext) {
    let (fs, _project, document) = open_document(cx).await;
    let generation = cx.read(|cx| document.read(cx).generation);
    fs.insert_file("/root/document.pdf", b"partial write".to_vec())
        .await;
    document
        .condition(cx, |document, _| {
            document.error.is_some() && !document.loading
        })
        .await;
    cx.read(|cx| {
        let document = document.read(cx);
        assert_eq!(document.generation, generation);
        assert_eq!(document.sizes.len(), 2);
    });
    fs.insert_file("/root/document.pdf", test_pdf(&[(300, 400, "0 1 0")]))
        .await;
    document
        .condition(cx, move |document, _| document.generation > generation)
        .await;
    cx.read(|cx| {
        let document = document.read(cx);
        assert!(document.error.is_none());
        assert_eq!(document.sizes.len(), 1);
        assert_eq!(document.sizes[0].width, 300.);
    });
}

#[gpui::test]
async fn split_shares_document_and_reload_preserves_fraction(cx: &mut TestAppContext) {
    let (fs, project, document) = open_document(cx).await;
    let generation = cx.read(|cx| document.read(cx).generation);
    fs.insert_file(
        "/root/document.pdf",
        test_pdf(&[(100, 1000, "1 0 0"), (200, 4000, "0 0 1")]),
    )
    .await;
    document
        .condition(cx, move |document, _| document.generation > generation)
        .await;
    let same_document = cx.update(|cx| {
        PdfDocument::open(&project, &document.read(cx).path.clone(), cx).expect("reopen PDF")
    });
    assert_eq!(document, same_document);
    drop(same_document);
    let (view, visual) = cx.add_window_view(|_, cx| PdfView::new(document.clone(), cx));
    let split = view
        .update_in(visual, |view, window, cx| {
            view.set_zoom(Some(1.), window.mouse_position(), cx);
            view.restore_position((1, 0.5), cx);
            view.clone_on_split(None, window, cx)
        })
        .await
        .expect("split view");
    visual.read(|cx| {
        assert_eq!(split.read(cx).document, view.read(cx).document);
        assert_eq!(split.read(cx).position(cx), (1, 0.5));
    });
    cx.run_until_parked();
    let generation = cx.read(|cx| document.read(cx).generation);
    fs.insert_file(
        "/root/document.pdf",
        test_pdf(&[(100, 1000, "1 0 0"), (200, 6000, "0 1 0")]),
    )
    .await;
    document
        .condition(cx, move |document, _| document.generation > generation)
        .await;
    cx.run_until_parked();
    cx.read(|cx| {
        assert_eq!(view.read(cx).position(cx), (1, 0.5));
        assert_eq!(split.read(cx).position(cx), (1, 0.5));
    });
}

#[gpui::test]
async fn closing_releases_document(cx: &mut TestAppContext) {
    let (_fs, _project, document) = open_document(cx).await;
    let weak = document.downgrade();
    drop(document);
    cx.run_until_parked();
    assert!(!weak.is_upgradable());
}

#[gpui::test]
async fn opens_a_pdf_rooted_at_a_single_file(cx: &mut TestAppContext) {
    let (fs, _, _) = open_document(cx).await;
    cx.update(init);
    let project = Project::test(fs, [Path::new("/root/document.pdf")], cx).await;
    let path = cx.read(|cx| ProjectPath {
        worktree_id: project
            .read(cx)
            .worktrees(cx)
            .next()
            .expect("file worktree")
            .read(cx)
            .id(),
        path: rel_path("").into(),
    });
    let (workspace, visual) =
        cx.add_window_view(|window, cx| Workspace::test_new(project, window, cx));
    let item = workspace
        .update_in(visual, |workspace, window, cx| {
            workspace.open_path(path, None, true, window, cx)
        })
        .await
        .expect("open PDF through workspace");
    let view = item
        .to_any_view()
        .downcast::<PdfView>()
        .expect("PDF viewer");
    visual.read(|cx| assert_eq!(view.read(cx).tab_content_text(0, cx), "document.pdf"));
}

#[gpui::test(iterations = 5)]
async fn atomic_replacement_and_rename_keep_the_same_document(cx: &mut TestAppContext) {
    let (fs, project, document) = open_document(cx).await;
    let generation = cx.read(|cx| document.read(cx).generation);
    fs.insert_file("/root/replacement.pdf", test_pdf(&[(300, 400, "0 1 0")]))
        .await;
    fs.rename(
        Path::new("/root/replacement.pdf"),
        Path::new("/root/document.pdf"),
        fs::RenameOptions {
            overwrite: true,
            ..Default::default()
        },
    )
    .await
    .expect("replace PDF atomically");
    document
        .condition(cx, move |document, _| document.generation > generation)
        .await;
    cx.read(|cx| assert_eq!(document.read(cx).sizes.len(), 1));
    fs.rename(
        Path::new("/root/document.pdf"),
        Path::new("/root/renamed.PDF"),
        Default::default(),
    )
    .await
    .expect("rename PDF");
    document
        .condition(cx, |document, _| {
            document.path.path.as_ref() == rel_path("renamed.PDF") && !document.loading
        })
        .await;
    let reopened = cx
        .update(|cx| {
            let path = document.read(cx).path.clone();
            <PdfDocument as project::ProjectItem>::try_open(&project, &path, cx)
                .expect("PDF handler")
        })
        .await
        .expect("reopen renamed PDF");
    assert_eq!(reopened, document);
}

#[gpui::test]
async fn saved_state_round_trips(cx: &mut TestAppContext) {
    let (database, workspace_database) =
        cx.update(|cx| (PdfViewerDb::global(cx), workspace::WorkspaceDb::global(cx)));
    let workspace_id = workspace_database.next_id().await.expect("workspace ID");
    let path = Path::new("/root/document.pdf").to_path_buf();
    database
        .save_state(123, workspace_id, path.clone(), 7, 0.4, Some(1.5))
        .await
        .expect("save PDF state");
    assert_eq!(
        database
            .get_state(123, workspace_id)
            .expect("load PDF state"),
        Some((path, 7, 0.4, Some(1.5)))
    );
}
