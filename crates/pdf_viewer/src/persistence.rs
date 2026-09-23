use std::path::PathBuf;

use db::{
    query,
    sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
    sqlez_macros::sql,
};
use workspace::{ItemId, WorkspaceDb, WorkspaceId};

pub struct PdfViewerDb(ThreadSafeConnection);

impl Domain for PdfViewerDb {
    const NAME: &str = stringify!(PdfViewerDb);
    const MIGRATIONS: &[&str] = &[sql!(
        CREATE TABLE pdf_viewers (
            workspace_id INTEGER,
            item_id INTEGER UNIQUE,
            path BLOB NOT NULL,
            page INTEGER NOT NULL,
            fraction REAL NOT NULL,
            zoom REAL,
            PRIMARY KEY(workspace_id, item_id),
            FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id) ON DELETE CASCADE
        ) STRICT;
    )];
}

db::static_connection!(PdfViewerDb, [WorkspaceDb]);

impl PdfViewerDb {
    query! {
        pub async fn save_state(item_id: ItemId, workspace_id: WorkspaceId, path: PathBuf, page: i64, fraction: f64, zoom: Option<f64>) -> Result<()> {
            INSERT OR REPLACE INTO pdf_viewers(item_id, workspace_id, path, page, fraction, zoom)
            VALUES (?, ?, ?, ?, ?, ?)
        }
    }
    query! {
        pub fn get_state(item_id: ItemId, workspace_id: WorkspaceId) -> Result<Option<(PathBuf, i64, f64, Option<f64>)>> {
            SELECT path, page, fraction, zoom FROM pdf_viewers WHERE item_id = ? AND workspace_id = ?
        }
    }
}
