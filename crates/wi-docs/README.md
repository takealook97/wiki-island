# wi-docs

Local, model-free import. It extracts PDF, DOCX, PPTX, CSV and PNG, JPEG, TIFF, BMP, GIF and WebP files that are already inside the vault into searchable sidecar notes with page maps and provenance run notes, and converts a Notion export (zip or folder) into ordinary notes. Originals are only read and ingested into history, never rewritten; every derived write goes through `wi-store` with ingress `Import`. OCR uses an installed Tesseract and poppler when present and is otherwise reported as `needs_ocr`; each external tool runs under a timeout. Formats: [FORMAT](../../docs/FORMAT.md#document-sidecars); behavior and limits: [OPERATIONS](../../docs/OPERATIONS.md#document-import).

```rust,ignore
let report = wi_docs::import(&vault, "report.pdf", wi_docs::ImportOptions {
    ocr: wi_docs::Ocr::Off,
    bulk: true,
})?;
vault.flush()?;
```

A Notion export (a `.zip`, or a folder whose names carry 32-hex Notion ids) is converted offline by the same `import` entry point (`notion` module): pages and CSV databases become ordinary notes under `Notion/<export name>/`, links become vault-relative wikilinks, and attachments are copied beside the pages through `Vault::write_attachment`. A hidden record under `.wiki-island/imports/` (written through `Vault::write_import_record`) keeps the mapping and checksums, so a repeated import publishes only what changed and never overwrites a converted note or copy edited in the vault. `notion::is_export(root, path)` lets callers that expand directories pass an export folder through whole. Format: [FORMAT](../../docs/FORMAT.md#notion-exports).

Import a downloaded export directly with `wi import ~/Downloads/Export.zip` or `wi import ~/Downloads/Export`. Absolute export paths outside the vault are read-only sources; only the converted copy is indexed in the vault. Relative export folders inside the vault remain supported. Protected `.wiki-island` and `.git` members at any depth are refused individually and reported while the other pages import.

Multipart exports containing `*-Part-N.zip` members are unpacked within one shared entry and expansion budget. Database columns become top-level note properties; names colliding with lifecycle or import metadata gain a `notion_` prefix. Source metadata stores the export filename, without its absolute path. Existing imports keep their recorded folders on reimport; same-name sources receive a numbered folder. Existing converted notes are still checked for edits before replacement.
