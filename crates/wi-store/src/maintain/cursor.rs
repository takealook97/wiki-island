use super::Cursor;
use crate::vault::Vault;

/// Where a budget-stopped pass resumes (device-local).
/// Inside the per-device local directory ([`Vault::local_dir`]).
const CURSOR_FILE: &str = "maintain-cursor.json";

impl Vault {
    pub(super) fn load_cursor(&self) -> Option<Cursor> {
        let bytes = std::fs::read(self.local_dir().join(CURSOR_FILE)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Record (or, with `None`, forget) where the next pass resumes.
    /// Best effort: a lost cursor only makes the next pass start over.
    pub(super) fn save_cursor(&self, cursor: Option<&Cursor>, exists: bool) {
        let file = self.local_dir().join(CURSOR_FILE);
        match cursor {
            Some(cursor) => {
                let Ok(text) = serde_json::to_vec(cursor) else {
                    return;
                };
                if let Some(dir) = file.parent()
                    && std::fs::create_dir_all(dir).is_ok()
                {
                    let _ =
                        wi_history::durable::write_atomic(&file, &text, self.history.durability());
                }
            }
            None if exists => {
                let _ = std::fs::remove_file(&file);
            }
            None => {}
        }
    }
}
