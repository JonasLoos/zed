use super::*;
use fs::{FakeFs, Fs as _};
use gpui::{BackgroundExecutor, TestAppContext};
use renderer::Renderer;
use std::{path::Path, sync::Arc};
use util::rel_path::rel_path;

fn test_pdf(pages: &[(u32, u32, &str)]) -> Vec<u8> {
    test_pdf_with_attributes(pages, "", false)
}

fn test_pdf_with_attributes(pages: &[(u32, u32, &str)], attributes: &str, text: bool) -> Vec<u8> {
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
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
    let renderer = Renderer::new(
        test_pdf_with_attributes(
            &[(300, 200, "1 1 1")],
            "/CropBox [0 0 250 100] /Rotate 90",
            true,
        ),
        executor,
    )
    .await
    .expect("PDF should load");
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
        assert!(u32::from(key.width) * u32::from(key.height) <= 8_010_000);
    }
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
            view.set_zoom(Some(1.), cx);
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
