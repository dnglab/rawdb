//! Per-pod SQLite cache mirroring three S3-derived datasets:
//! approved sets (`samples/`), pending uploads (`pending/`), and the
//! users file (`_system/users.toml`).
//!
//! The cache lives at `<RAWDB_CACHE_DIR>/rawdb.sqlite`. It is treated
//! as ephemeral — rebuilt on each pod startup — so we use WAL +
//! NORMAL sync (the durability we lose is irrelevant because the
//! authoritative state is in S3 anyway).
//!
//! All write paths go through a thread on the blocking pool because
//! rusqlite is sync. Reads use the same r2d2 pool.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, OptionalExtension};

use crate::meta::{FileMeta, RawdbMeta};

pub type DbPool = Pool<SqliteConnectionManager>;

#[derive(Clone)]
pub struct Db {
    pool: DbPool,
}

#[derive(Debug, Clone)]
pub struct SetRow {
    pub maker: String,
    pub model: String,
    pub license: String,
    pub notes: Option<String>,
    pub uploaded_at: Option<DateTime<Utc>>,
    pub uploaded_by: Option<String>,
    pub meta_etag: Option<String>,
    pub special: bool,
}

#[derive(Debug, Clone)]
pub struct FileRow {
    pub path: String,
    pub category: String,
    pub extension: Option<String>,
    pub size: u64,
    pub license: Option<String>,
    pub notes: Option<String>,
    pub etag: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PendingSetRow {
    pub maker: String,
    pub model: String,
    pub upload_id: String,
    pub license: String,
    pub notes: Option<String>,
    pub uploaded_at: Option<DateTime<Utc>>,
    pub uploaded_by: Option<String>,
    pub meta_etag: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UserRow {
    pub sub: String,
    pub display_name: Option<String>,
    pub blocked: bool,
    pub added_at: Option<DateTime<Utc>>,
    pub added_by: Option<String>,
    pub roles: Vec<String>,
    pub api_key_hash: Option<String>,
}

impl Db {
    pub fn open(cache_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(cache_dir)
            .with_context(|| format!("create cache dir {}", cache_dir.display()))?;
        let path = cache_dir.join("rawdb.sqlite");
        let manager = SqliteConnectionManager::file(&path)
            .with_init(|c| {
                c.execute_batch(
                    "PRAGMA journal_mode = WAL;
                     PRAGMA synchronous = NORMAL;
                     PRAGMA foreign_keys = ON;
                     PRAGMA temp_store = MEMORY;",
                )
            });
        let pool = Pool::builder()
            .max_size(8)
            .build(manager)
            .context("build sqlite pool")?;

        let db = Self { pool };
        db.migrate()?;
        Ok(db)
    }

    pub fn pool(&self) -> &DbPool {
        &self.pool
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute_batch(SCHEMA_SQL).context("apply schema")?;
        Ok(())
    }

    // ---- approved sets ------------------------------------------------------

    /// Insert or replace a set + its files + tags. Atomically rebuilds the
    /// per-set rows in `files`, `tags`, and `sets_fts`.
    pub fn upsert_set(
        &self,
        meta: &RawdbMeta,
        meta_etag: Option<&str>,
        files_with_sizes: &[(FileMeta, FileSizeInfo)],
    ) -> Result<()> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction()?;

        let maker = &meta.set.maker;
        let model = &meta.set.model;
        let now = Utc::now();

        tx.execute(
            "INSERT INTO sets(maker, model, license, notes, uploaded_at, uploaded_by, meta_etag, special, last_indexed_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(maker, model) DO UPDATE SET
               license = excluded.license,
               notes = excluded.notes,
               uploaded_at = excluded.uploaded_at,
               uploaded_by = excluded.uploaded_by,
               meta_etag = excluded.meta_etag,
               special = excluded.special,
               last_indexed_at = excluded.last_indexed_at",
            params![
                maker,
                model,
                &meta.set.license,
                meta.set.notes.as_deref(),
                meta.set.uploaded_at.map(|d| d.to_rfc3339()),
                meta.set.uploaded_by.as_deref(),
                meta_etag,
                meta.set.special as i64,
                now.to_rfc3339(),
            ],
        )?;

        tx.execute("DELETE FROM files WHERE maker = ? AND model = ?", params![maker, model])?;
        tx.execute("DELETE FROM tags WHERE maker = ? AND model = ?", params![maker, model])?;

        for (f, size_info) in files_with_sizes {
            tx.execute(
                "INSERT INTO files(maker, model, path, category, extension, size, license, notes, etag)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    maker,
                    model,
                    &f.path,
                    f.category(),
                    f.extension(),
                    size_info.size as i64,
                    f.license.as_deref(),
                    f.notes.as_deref(),
                    size_info.etag.as_deref(),
                ],
            )?;
            for tag in &f.tags {
                tx.execute(
                    "INSERT INTO tags(maker, model, file_path, tag) VALUES (?, ?, ?, ?)",
                    params![maker, model, &f.path, tag],
                )?;
            }
        }

        // Rebuild FTS row for this set. Sets have no tags of their own;
        // searchable tags come from the files.
        tx.execute(
            "DELETE FROM sets_fts WHERE maker = ? AND model = ?",
            params![maker, model],
        )?;
        let joined_tags = meta
            .files
            .iter()
            .flat_map(|f| f.tags.iter())
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        tx.execute(
            "INSERT INTO sets_fts(maker, model, notes, tags) VALUES (?, ?, ?, ?)",
            params![maker, model, meta.set.notes.as_deref(), joined_tags],
        )?;

        tx.commit()?;
        Ok(())
    }

    pub fn delete_set(&self, maker: &str, model: &str) -> Result<()> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM tags WHERE maker = ? AND model = ?", params![maker, model])?;
        tx.execute("DELETE FROM files WHERE maker = ? AND model = ?", params![maker, model])?;
        tx.execute("DELETE FROM sets_fts WHERE maker = ? AND model = ?", params![maker, model])?;
        tx.execute("DELETE FROM sets WHERE maker = ? AND model = ?", params![maker, model])?;
        tx.commit()?;
        Ok(())
    }

    pub fn get_set_meta_etag(&self, maker: &str, model: &str) -> Result<Option<String>> {
        let conn = self.pool.get()?;
        let etag: Option<String> = conn
            .query_row(
                "SELECT meta_etag FROM sets WHERE maker = ? AND model = ?",
                params![maker, model],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(etag)
    }

    pub fn list_set_keys(&self) -> Result<Vec<(String, String)>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare("SELECT maker, model FROM sets")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn get_set(&self, maker: &str, model: &str) -> Result<Option<SetRow>> {
        let conn = self.pool.get()?;
        let row = conn
            .query_row(
                "SELECT maker, model, license, notes, uploaded_at, uploaded_by, meta_etag, special
                 FROM sets WHERE maker = ? AND model = ?",
                params![maker, model],
                |r| {
                    Ok(SetRow {
                        maker: r.get(0)?,
                        model: r.get(1)?,
                        license: r.get(2)?,
                        notes: r.get(3)?,
                        uploaded_at: r
                            .get::<_, Option<String>>(4)?
                            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                            .map(|d| d.with_timezone(&Utc)),
                        uploaded_by: r.get(5)?,
                        meta_etag: r.get(6)?,
                        special: r.get::<_, i64>(7)? != 0,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn list_files(&self, maker: &str, model: &str) -> Result<Vec<FileRow>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT path, category, extension, size, license, notes, etag
             FROM files WHERE maker = ? AND model = ? ORDER BY category, path",
        )?;
        let rows = stmt
            .query_map(params![maker, model], |r| {
                Ok(FileRow {
                    path: r.get(0)?,
                    category: r.get(1)?,
                    extension: r.get(2)?,
                    size: r.get::<_, i64>(3)?.max(0) as u64,
                    license: r.get(4)?,
                    notes: r.get(5)?,
                    etag: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---- search / list -----------------------------------------------------

    /// Set-level search with optional filters. Returns set rows + their file
    /// counts and total byte size. Filters that operate on per-file attributes
    /// (extension) are applied via EXISTS — a set is included
    /// if at least one of its files matches. `tags` matches against the
    /// `tags` table, which holds both set-level rows (`file_path IS NULL`)
    /// and per-file rows, so set tags are effectively inherited by files.
    ///
    /// `fts` is matched against `sets_fts` (FTS5 over maker/model/notes/tags).
    pub fn search_sets(&self, q: &SetQuery) -> Result<SetSearchPage> {
        let conn = self.pool.get()?;

        // Build the WHERE clauses dynamically. We keep this readable rather
        // than clever — every clause appends a bound parameter.
        let mut where_clauses = Vec::<String>::new();
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(maker) = &q.maker {
            where_clauses.push(r"s.maker LIKE '%' || ? || '%' ESCAPE '\'".to_string());
            params.push(Box::new(like_escape(maker)));
        }
        if let Some(model) = &q.model {
            where_clauses.push(r"s.model LIKE '%' || ? || '%' ESCAPE '\'".to_string());
            params.push(Box::new(like_escape(model)));
        }
        if let Some(license) = &q.license {
            where_clauses.push("s.license = ?".to_string());
            params.push(Box::new(license.clone()));
        }
        // Non-camera ("special") sets are hidden unless explicitly requested.
        if !q.include_special {
            where_clauses.push("s.special = 0".to_string());
        }

        // Per-file existence filters.
        let push_exists = |where_clauses: &mut Vec<String>,
                           params: &mut Vec<Box<dyn rusqlite::ToSql>>,
                           col: &str,
                           val: Box<dyn rusqlite::ToSql>| {
            where_clauses.push(format!(
                "EXISTS(SELECT 1 FROM files f WHERE f.maker = s.maker AND f.model = s.model AND f.{col} = ?)"
            ));
            params.push(val);
        };
        if let Some(ext) = &q.extension {
            push_exists(&mut where_clauses, &mut params, "extension", Box::new(ext.clone()));
        }
        // Each requested tag must exist on the set (set-level or any file).
        for tag in &q.tags {
            where_clauses.push(
                "EXISTS(SELECT 1 FROM tags t WHERE t.maker = s.maker AND t.model = s.model AND t.tag = ?)"
                    .to_string(),
            );
            params.push(Box::new(tag.clone()));
        }

        // FTS join.
        let (fts_join, fts_where) = if let Some(text) = q.fts.as_deref().filter(|s| !s.trim().is_empty()) {
            params.push(Box::new(text.to_string()));
            (
                "JOIN sets_fts ft ON ft.maker = s.maker AND ft.model = s.model",
                Some("sets_fts MATCH ?"),
            )
        } else {
            ("", None)
        };
        if let Some(w) = fts_where {
            where_clauses.push(w.to_string());
        }

        let where_sql = if where_clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", where_clauses.join(" AND "))
        };

        let limit = q.limit.unwrap_or(50).clamp(1, 500) as i64;
        let offset = q.offset.unwrap_or(0).max(0) as i64;

        // Tie-break on (maker, model) so a page boundary is deterministic
        // even when the requested sort key has duplicates.
        let order_by = match q.sort_field {
            None => "s.maker COLLATE NOCASE ASC, s.model COLLATE NOCASE ASC".to_string(),
            Some(field) => format!(
                "{} {}, s.maker COLLATE NOCASE ASC, s.model COLLATE NOCASE ASC",
                field.sql_expr(),
                q.sort_order.sql_keyword(),
            ),
        };

        let sql = format!(
            "SELECT s.maker, s.model, s.license, s.notes, s.uploaded_at, s.uploaded_by,
                    COUNT(DISTINCT f.path) AS file_count,
                    COALESCE(SUM(f.size), 0) AS total_size,
                    s.special,
                    (SELECT IFNULL(GROUP_CONCAT(DISTINCT tag), '')
                       FROM tags t
                       WHERE t.maker = s.maker AND t.model = s.model) AS sort_tags
             FROM sets s
             LEFT JOIN files f ON s.maker = f.maker AND s.model = f.model
             {fts_join}
             {where_sql}
             GROUP BY s.maker, s.model
             ORDER BY {order_by}
             LIMIT ? OFFSET ?"
        );
        let total_sql = format!(
            "SELECT COUNT(*) FROM sets s {fts_join} {where_sql}"
        );

        // Total count (without limit/offset binds).
        let total: i64 = {
            let mut stmt = conn.prepare(&total_sql)?;
            let bound: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
            stmt.query_row(rusqlite::params_from_iter(bound), |r| r.get::<_, i64>(0))?
        };

        // Page.
        let mut sets: Vec<SetSummary> = {
            let mut stmt = conn.prepare(&sql)?;
            // Bind both filter params + limit/offset.
            let mut bound: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
            bound.push(&limit);
            bound.push(&offset);
            let rows = stmt
                .query_map(rusqlite::params_from_iter(bound), |r| {
                    Ok(SetSummary {
                        maker: r.get(0)?,
                        model: r.get(1)?,
                        license: r.get(2)?,
                        notes: r.get(3)?,
                        uploaded_at: r
                            .get::<_, Option<String>>(4)?
                            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                            .map(|d| d.with_timezone(&Utc)),
                        uploaded_by: r.get(5)?,
                        file_count: r.get::<_, i64>(6)?.max(0) as u64,
                        total_size: r.get::<_, i64>(7)?.max(0) as u64,
                        special: r.get::<_, i64>(8)? != 0,
                        tags: Vec::new(),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };

        // Distinct file tags per set — fetched only for the page. Sets have
        // no tags of their own; this is the union of their files' tags.
        for s in &mut sets {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT tag FROM tags WHERE maker = ? AND model = ? ORDER BY tag",
            )?;
            s.tags = stmt
                .query_map(params![&s.maker, &s.model], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
        }

        Ok(SetSearchPage {
            sets,
            total: total.max(0) as u64,
            limit: limit as u32,
            offset: offset as u32,
        })
    }

    #[allow(dead_code)] // retained for diagnostics/tests; not in the stats payload
    pub fn count_sets(&self) -> Result<u64> {
        let conn = self.pool.get()?;
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM sets", [], |r| r.get(0))?;
        Ok(n.max(0) as u64)
    }

    /// Distinct camera models with samples — camera sets only (non-camera
    /// "special" sets are excluded; they're counted separately).
    pub fn count_models(&self) -> Result<u64> {
        let conn = self.pool.get()?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM (SELECT DISTINCT maker, model FROM sets WHERE special = 0)",
            [],
            |r| r.get(0),
        )?;
        Ok(n.max(0) as u64)
    }

    /// Number of non-camera ("special") sets.
    pub fn count_special(&self) -> Result<u64> {
        let conn = self.pool.get()?;
        let n: i64 =
            conn.query_row("SELECT COUNT(*) FROM sets WHERE special = 1", [], |r| r.get(0))?;
        Ok(n.max(0) as u64)
    }

    /// Distinct camera makers across approved sets, alphabetically. When
    /// `include_special` is false (the default for public callers),
    /// non-camera "special" sets are excluded so the maker picker stays a
    /// list of real cameras.
    pub fn distinct_makers(&self, include_special: bool) -> Result<Vec<String>> {
        let conn = self.pool.get()?;
        let sql = if include_special {
            "SELECT DISTINCT maker FROM sets ORDER BY maker COLLATE NOCASE"
        } else {
            "SELECT DISTINCT maker FROM sets WHERE special = 0 ORDER BY maker COLLATE NOCASE"
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Distinct `(maker, model)` pairs across approved sets, alphabetically.
    /// `include_special` toggles inclusion of non-camera sets; defaults to
    /// false for public callers.
    pub fn distinct_maker_models(
        &self,
        include_special: bool,
    ) -> Result<Vec<(String, String)>> {
        let conn = self.pool.get()?;
        let sql = if include_special {
            "SELECT DISTINCT maker, model FROM sets
             ORDER BY maker COLLATE NOCASE, model COLLATE NOCASE"
        } else {
            "SELECT DISTINCT maker, model FROM sets WHERE special = 0
             ORDER BY maker COLLATE NOCASE, model COLLATE NOCASE"
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// All tags with their usage counts, ordered most-used first then
    /// alphabetically. The frontend slices the head of this list for tag
    /// suggestions; admin UIs can render the full distribution.
    pub fn tag_counts(&self) -> Result<Vec<(String, u64)>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT tag, COUNT(*) AS n FROM tags
             GROUP BY tag
             ORDER BY n DESC, tag COLLATE NOCASE ASC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn count_pending(&self) -> Result<u64> {
        let conn = self.pool.get()?;
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM pending_sets", [], |r| r.get(0))?;
        Ok(n.max(0) as u64)
    }

    #[allow(dead_code)] // retained for diagnostics; no longer in the stats payload
    pub fn count_users(&self) -> Result<u64> {
        let conn = self.pool.get()?;
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?;
        Ok(n.max(0) as u64)
    }

    /// Tags attached to specific files in a set.
    pub fn file_tags(&self, maker: &str, model: &str) -> Result<HashMap<String, Vec<String>>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT file_path, tag FROM tags
             WHERE maker = ? AND model = ? AND file_path IS NOT NULL
             ORDER BY file_path, tag",
        )?;
        let mut out: HashMap<String, Vec<String>> = HashMap::new();
        let mut rows = stmt.query(params![maker, model])?;
        while let Some(r) = rows.next()? {
            let path: String = r.get(0)?;
            let tag: String = r.get(1)?;
            out.entry(path).or_default().push(tag);
        }
        Ok(out)
    }

    // ---- pending uploads ----------------------------------------------------

    pub fn upsert_pending(
        &self,
        upload_id: &str,
        meta: &RawdbMeta,
        meta_etag: Option<&str>,
        files_with_sizes: &[(FileMeta, FileSizeInfo)],
    ) -> Result<()> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction()?;
        let maker = &meta.set.maker;
        let model = &meta.set.model;
        let now = Utc::now();

        // Pending uploads are keyed by upload_id alone (the S3 path is
        // `pending/<upload_id>/`); maker/model are stored for display and
        // come from the meta, so a maker/model edit must not leave a stale
        // row behind. Replace any prior rows for this upload_id outright
        // (pending_files cascades via FK).
        tx.execute(
            "DELETE FROM pending_sets WHERE upload_id = ?",
            params![upload_id],
        )?;
        tx.execute(
            "INSERT INTO pending_sets(maker, model, upload_id, license, notes, uploaded_at, uploaded_by, meta_etag, last_indexed_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                maker,
                model,
                upload_id,
                &meta.set.license,
                meta.set.notes.as_deref(),
                meta.set.uploaded_at.map(|d| d.to_rfc3339()),
                meta.set.uploaded_by.as_deref(),
                meta_etag,
                now.to_rfc3339(),
            ],
        )?;
        for (f, size_info) in files_with_sizes {
            tx.execute(
                "INSERT INTO pending_files(maker, model, upload_id, path, category, extension, size, license, notes, etag)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    maker,
                    model,
                    upload_id,
                    &f.path,
                    f.category(),
                    f.extension(),
                    size_info.size as i64,
                    f.license.as_deref(),
                    f.notes.as_deref(),
                    size_info.etag.as_deref(),
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn delete_pending_by_upload_id(&self, upload_id: &str) -> Result<()> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction()?;
        // pending_files cascades from pending_sets via FK.
        tx.execute(
            "DELETE FROM pending_sets WHERE upload_id = ?",
            params![upload_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn get_pending_meta_etag(&self, upload_id: &str) -> Result<Option<String>> {
        let conn = self.pool.get()?;
        let etag: Option<String> = conn
            .query_row(
                "SELECT meta_etag FROM pending_sets WHERE upload_id = ?",
                params![upload_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(etag)
    }

    pub fn list_pending_upload_ids(&self) -> Result<Vec<String>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare("SELECT upload_id FROM pending_sets")?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---- users --------------------------------------------------------------

    pub fn replace_users(&self, etag: Option<&str>, users: &[UserRow]) -> Result<()> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM user_roles", [])?;
        tx.execute("DELETE FROM users", [])?;
        for u in users {
            tx.execute(
                "INSERT INTO users(sub, display_name, blocked, added_at, added_by, api_key_hash)
                 VALUES (?, ?, ?, ?, ?, ?)",
                params![
                    &u.sub,
                    u.display_name.as_deref(),
                    u.blocked as i64,
                    u.added_at.map(|d| d.to_rfc3339()),
                    u.added_by.as_deref(),
                    u.api_key_hash.as_deref(),
                ],
            )?;
            for role in &u.roles {
                tx.execute(
                    "INSERT INTO user_roles(sub, role) VALUES (?, ?)",
                    params![&u.sub, role],
                )?;
            }
        }
        tx.execute(
            "INSERT OR REPLACE INTO users_etag(id, etag) VALUES (1, ?)",
            params![etag],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn get_users_etag(&self) -> Result<Option<String>> {
        let conn = self.pool.get()?;
        let etag = conn
            .query_row(
                "SELECT etag FROM users_etag WHERE id = 1",
                [],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(etag)
    }

    // ---- scan state ---------------------------------------------------------

    pub fn set_last_full_scan_at(&self, when: DateTime<Utc>) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT OR REPLACE INTO scan_state(id, last_full_scan_at) VALUES (1, ?)",
            params![when.to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn last_full_scan_at(&self) -> Result<Option<DateTime<Utc>>> {
        let conn = self.pool.get()?;
        let raw: Option<String> = conn
            .query_row(
                "SELECT last_full_scan_at FROM scan_state WHERE id = 1",
                [],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(raw
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|d| d.with_timezone(&Utc)))
    }

    pub fn get_user(&self, sub: &str) -> Result<Option<UserRow>> {
        let conn = self.pool.get()?;
        let user = conn
            .query_row(
                "SELECT sub, display_name, blocked, added_at, added_by, api_key_hash
                 FROM users WHERE sub = ?",
                params![sub],
                row_to_user,
            )
            .optional()?;
        let Some(mut user) = user else { return Ok(None) };
        user.roles = self.load_roles(&conn, sub)?;
        Ok(Some(user))
    }

    /// Look up the user owning a given API-key hash. Powers both the
    /// download rate-limit bypass and the key-protected export endpoint.
    pub fn find_user_by_api_key_hash(&self, hash: &str) -> Result<Option<UserRow>> {
        let conn = self.pool.get()?;
        let user = conn
            .query_row(
                "SELECT sub, display_name, blocked, added_at, added_by, api_key_hash
                 FROM users WHERE api_key_hash = ?",
                params![hash],
                row_to_user,
            )
            .optional()?;
        let Some(mut user) = user else { return Ok(None) };
        let sub = user.sub.clone();
        user.roles = self.load_roles(&conn, &sub)?;
        Ok(Some(user))
    }

    fn load_roles(
        &self,
        conn: &rusqlite::Connection,
        sub: &str,
    ) -> Result<Vec<String>> {
        let mut stmt = conn.prepare("SELECT role FROM user_roles WHERE sub = ?")?;
        let roles = stmt
            .query_map(params![sub], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(roles)
    }

    // ---- full export --------------------------------------------------------

    /// Every approved set with all of its files — backs the key-protected
    /// `/api/export` endpoint. Built from three bulk queries (sets, files,
    /// tags) assembled in memory to avoid an N+1 over >10k sets.
    pub fn export_all(&self) -> Result<Vec<ExportSet>> {
        let conn = self.pool.get()?;

        // Tags: (maker, model, file_path|NULL, tag). Set-level tags have a
        // NULL file_path. Per the public model, a set's tag list is the
        // union of all its tags; a file's list is its own per-file rows.
        let mut tag_stmt = conn.prepare(
            "SELECT maker, model, file_path, tag FROM tags",
        )?;
        let mut set_tags: HashMap<(String, String), Vec<String>> = HashMap::new();
        let mut file_tags: HashMap<(String, String, String), Vec<String>> =
            HashMap::new();
        let tag_rows = tag_stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        for row in tag_rows {
            let (maker, model, file_path, tag) = row?;
            let sk = (maker.clone(), model.clone());
            let st = set_tags.entry(sk).or_default();
            if !st.contains(&tag) {
                st.push(tag.clone());
            }
            if let Some(fp) = file_path {
                file_tags.entry((maker, model, fp)).or_default().push(tag);
            }
        }

        // Files, grouped by (maker, model).
        let mut file_stmt = conn.prepare(
            "SELECT maker, model, path, category, extension, size, license, notes
             FROM files ORDER BY maker, model, category, path",
        )?;
        let mut files_by_set: HashMap<(String, String), Vec<ExportFile>> =
            HashMap::new();
        let file_rows = file_stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<String>>(7)?,
            ))
        })?;
        for row in file_rows {
            let (maker, model, path, category, extension, size, license, notes) =
                row?;
            let tags = file_tags
                .get(&(maker.clone(), model.clone(), path.clone()))
                .cloned()
                .unwrap_or_default();
            files_by_set
                .entry((maker, model))
                .or_default()
                .push(ExportFile {
                    path,
                    category,
                    extension,
                    size: size.max(0) as u64,
                    license,
                    notes,
                    tags,
                });
        }

        // Sets.
        let mut set_stmt = conn.prepare(
            "SELECT maker, model, license, notes, uploaded_at, uploaded_by, special
             FROM sets ORDER BY maker, model",
        )?;
        let set_rows = set_stmt.query_map([], |r| {
            Ok(ExportSet {
                maker: r.get(0)?,
                model: r.get(1)?,
                license: r.get(2)?,
                notes: r.get(3)?,
                uploaded_at: r
                    .get::<_, Option<String>>(4)?
                    .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                    .map(|d| d.with_timezone(&Utc)),
                uploaded_by: r.get(5)?,
                special: r.get::<_, i64>(6)? != 0,
                tags: Vec::new(),
                files: Vec::new(),
            })
        })?;
        let mut out = Vec::new();
        for row in set_rows {
            let mut s = row?;
            let key = (s.maker.clone(), s.model.clone());
            s.tags = set_tags.remove(&key).unwrap_or_default();
            s.files = files_by_set.remove(&key).unwrap_or_default();
            out.push(s);
        }
        Ok(out)
    }
}

fn row_to_user(r: &rusqlite::Row) -> rusqlite::Result<UserRow> {
    Ok(UserRow {
        sub: r.get(0)?,
        display_name: r.get(1)?,
        blocked: r.get::<_, i64>(2)? != 0,
        added_at: r
            .get::<_, Option<String>>(3)?
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|d| d.with_timezone(&Utc)),
        added_by: r.get(4)?,
        roles: vec![],
        api_key_hash: r.get(5)?,
    })
}

/// One file in the `/api/export` payload.
#[derive(Debug, Clone)]
pub struct ExportFile {
    pub path: String,
    pub category: String,
    pub extension: Option<String>,
    pub size: u64,
    pub license: Option<String>,
    pub notes: Option<String>,
    pub tags: Vec<String>,
}

/// One set in the `/api/export` payload, with all its files.
#[derive(Debug, Clone)]
pub struct ExportSet {
    pub maker: String,
    pub model: String,
    pub license: String,
    pub notes: Option<String>,
    pub uploaded_at: Option<DateTime<Utc>>,
    pub uploaded_by: Option<String>,
    pub special: bool,
    pub tags: Vec<String>,
    pub files: Vec<ExportFile>,
}

#[derive(Debug, Clone)]
pub struct FileSizeInfo {
    pub size: u64,
    pub etag: Option<String>,
}

/// Escape `\`, `%`, `_` so user-typed wildcards are treated literally in a
/// `LIKE ... ESCAPE '\'` substring match.
fn like_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

#[derive(Debug, Clone, Default)]
pub struct SetQuery {
    pub maker: Option<String>,
    pub model: Option<String>,
    pub license: Option<String>,
    pub extension: Option<String>,
    /// Each tag must be present on at least one of the set's files.
    pub tags: Vec<String>,
    pub fts: Option<String>,
    /// Include non-camera ("special") sets. Default `false` hides them.
    pub include_special: bool,
    /// Whitelisted sort field. Unknown / `None` falls back to `(maker, model)`.
    pub sort_field: Option<SortField>,
    /// Sort direction; ignored when `sort_field` is `None`.
    pub sort_order: SortOrder,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

/// Columns the browse table can sort by. Hard-coded whitelist so the
/// HTTP layer can't inject arbitrary SQL via the `sort` query param.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortField {
    Maker,
    Model,
    License,
    FileCount,
    TotalSize,
    Tags,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortOrder {
    #[default]
    Asc,
    Desc,
}

impl SortField {
    /// Map to the SQL ORDER BY expression. Aggregates use the column alias
    /// from the SELECT; text columns use COLLATE NOCASE so casing doesn't
    /// dominate sort order. The `Tags` arm references a correlated
    /// subquery alias added to the SELECT list below.
    fn sql_expr(self) -> &'static str {
        match self {
            SortField::Maker => "s.maker COLLATE NOCASE",
            SortField::Model => "s.model COLLATE NOCASE",
            SortField::License => "s.license COLLATE NOCASE",
            SortField::FileCount => "file_count",
            SortField::TotalSize => "total_size",
            SortField::Tags => "sort_tags COLLATE NOCASE",
        }
    }
}

impl SortOrder {
    fn sql_keyword(self) -> &'static str {
        match self {
            SortOrder::Asc => "ASC",
            SortOrder::Desc => "DESC",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SetSummary {
    pub maker: String,
    pub model: String,
    pub license: String,
    pub notes: Option<String>,
    pub uploaded_at: Option<DateTime<Utc>>,
    pub uploaded_by: Option<String>,
    pub file_count: u64,
    pub total_size: u64,
    pub special: bool,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SetSearchPage {
    pub sets: Vec<SetSummary>,
    pub total: u64,
    pub limit: u32,
    pub offset: u32,
}


// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS sets (
    maker            TEXT NOT NULL,
    model            TEXT NOT NULL,
    license          TEXT NOT NULL,
    notes            TEXT,
    uploaded_at      TEXT,
    uploaded_by      TEXT,
    meta_etag        TEXT,
    special          INTEGER NOT NULL DEFAULT 0,
    last_indexed_at  TEXT NOT NULL,
    PRIMARY KEY (maker, model)
);

CREATE TABLE IF NOT EXISTS files (
    maker         TEXT NOT NULL,
    model         TEXT NOT NULL,
    path          TEXT NOT NULL,
    category      TEXT NOT NULL,
    extension     TEXT,
    size          INTEGER NOT NULL,
    license       TEXT,
    notes         TEXT,
    etag          TEXT,
    PRIMARY KEY (maker, model, path),
    FOREIGN KEY (maker, model) REFERENCES sets(maker, model) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS files_maker      ON files(maker);
CREATE INDEX IF NOT EXISTS files_model      ON files(model);
CREATE INDEX IF NOT EXISTS files_extension  ON files(extension);
CREATE INDEX IF NOT EXISTS files_category   ON files(category);
CREATE INDEX IF NOT EXISTS files_license    ON files(license);

CREATE TABLE IF NOT EXISTS tags (
    maker      TEXT NOT NULL,
    model      TEXT NOT NULL,
    file_path  TEXT,
    tag        TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS tags_lookup ON tags(maker, model, file_path);
CREATE INDEX IF NOT EXISTS tags_tag    ON tags(tag);

CREATE VIRTUAL TABLE IF NOT EXISTS sets_fts USING fts5(
    maker, model, notes, tags,
    tokenize = 'unicode61'
);

CREATE TABLE IF NOT EXISTS pending_sets (
    maker            TEXT NOT NULL,
    model            TEXT NOT NULL,
    upload_id        TEXT NOT NULL,
    license          TEXT NOT NULL,
    notes            TEXT,
    uploaded_at      TEXT,
    uploaded_by      TEXT,
    meta_etag        TEXT,
    last_indexed_at  TEXT NOT NULL,
    PRIMARY KEY (maker, model, upload_id)
);

CREATE TABLE IF NOT EXISTS pending_files (
    maker         TEXT NOT NULL,
    model         TEXT NOT NULL,
    upload_id     TEXT NOT NULL,
    path          TEXT NOT NULL,
    category      TEXT NOT NULL,
    extension     TEXT,
    size          INTEGER NOT NULL,
    license       TEXT,
    notes         TEXT,
    etag          TEXT,
    PRIMARY KEY (maker, model, upload_id, path),
    FOREIGN KEY (maker, model, upload_id) REFERENCES pending_sets(maker, model, upload_id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS users (
    sub           TEXT PRIMARY KEY,
    display_name  TEXT,
    blocked       INTEGER NOT NULL DEFAULT 0,
    added_at      TEXT,
    added_by      TEXT,
    api_key_hash  TEXT
);
CREATE INDEX IF NOT EXISTS idx_users_api_key_hash ON users(api_key_hash);
CREATE TABLE IF NOT EXISTS user_roles (
    sub  TEXT NOT NULL,
    role TEXT NOT NULL,
    PRIMARY KEY (sub, role),
    FOREIGN KEY (sub) REFERENCES users(sub) ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS users_etag (
    id   INTEGER PRIMARY KEY CHECK (id = 1),
    etag TEXT
);

CREATE TABLE IF NOT EXISTS scan_state (
    id                INTEGER PRIMARY KEY CHECK (id = 1),
    last_full_scan_at TEXT
);
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::{FileMeta, RawdbMeta, SetMeta};
    use tempfile::TempDir;

    fn sample_meta() -> RawdbMeta {
        RawdbMeta {
            set: SetMeta {
                maker: "Canon".into(),
                model: "EOS R5".into(),
                license: "CC0-1.0".into(),
                uploaded_by: Some("github:cytrinox".into()),
                uploaded_at: None,
                notes: Some("test set".into()),
                special: false,
            },
            files: vec![FileMeta {
                path: "raw_modes/IMG_0001.cr3".into(),
                license: None,
                notes: None,
                tags: vec!["high-iso".into()],
            }],
        }
    }

    #[test]
    fn search_matches_maker_and_model_substring() {
        let dir = TempDir::new().unwrap();
        let db = Db::open(dir.path()).unwrap();
        let meta = sample_meta(); // Canon / EOS R5
        db.upsert_set(&meta, Some("e1"), &[]).unwrap();

        let by_maker = db
            .search_sets(&SetQuery {
                maker: Some("ano".into()), // substring of "Canon"
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_maker.total, 1);

        let by_model = db
            .search_sets(&SetQuery {
                model: Some("R5".into()), // not a prefix of "EOS R5"
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_model.total, 1);

        // A literal '%' must not behave as a wildcard.
        let literal_pct = db
            .search_sets(&SetQuery {
                model: Some("%".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(literal_pct.total, 0);

        let no_match = db
            .search_sets(&SetQuery {
                maker: Some("Nikon".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(no_match.total, 0);
    }

    #[test]
    fn count_models_matches_distinct_sets() {
        let dir = TempDir::new().unwrap();
        let db = Db::open(dir.path()).unwrap();
        db.upsert_set(&sample_meta(), Some("e1"), &[]).unwrap();
        assert_eq!(db.count_models().unwrap(), 1);
        assert_eq!(db.count_models().unwrap(), db.count_sets().unwrap());
    }

    #[test]
    fn open_and_migrate() {
        let dir = TempDir::new().unwrap();
        let db = Db::open(dir.path()).unwrap();
        // re-open should be idempotent
        let _db2 = Db::open(dir.path()).unwrap();
        let _ = db;
    }

    #[test]
    fn upsert_and_readback_set() {
        let dir = TempDir::new().unwrap();
        let db = Db::open(dir.path()).unwrap();
        let meta = sample_meta();
        let files = vec![(
            meta.files[0].clone(),
            FileSizeInfo {
                size: 1234,
                etag: Some("abc".into()),
            },
        )];
        db.upsert_set(&meta, Some("etag-1"), &files).unwrap();

        let row = db.get_set("Canon", "EOS R5").unwrap().expect("set present");
        assert_eq!(row.license, "CC0-1.0");
        assert_eq!(row.meta_etag.as_deref(), Some("etag-1"));

        let frows = db.list_files("Canon", "EOS R5").unwrap();
        assert_eq!(frows.len(), 1);
        assert_eq!(frows[0].path, "raw_modes/IMG_0001.cr3");
        assert_eq!(frows[0].category, "raw_modes");
        assert_eq!(frows[0].size, 1234);

        // delete is idempotent
        db.delete_set("Canon", "EOS R5").unwrap();
        assert!(db.get_set("Canon", "EOS R5").unwrap().is_none());
        assert!(db.list_files("Canon", "EOS R5").unwrap().is_empty());
    }

    #[test]
    fn etag_fast_path_lookup() {
        let dir = TempDir::new().unwrap();
        let db = Db::open(dir.path()).unwrap();
        let meta = sample_meta();
        db.upsert_set(&meta, Some("etag-1"), &[]).unwrap();
        assert_eq!(
            db.get_set_meta_etag("Canon", "EOS R5").unwrap().as_deref(),
            Some("etag-1")
        );
        assert!(db.get_set_meta_etag("Canon", "missing").unwrap().is_none());
    }

    #[test]
    fn pending_upload_isolation_per_upload_id() {
        let dir = TempDir::new().unwrap();
        let db = Db::open(dir.path()).unwrap();
        let mut meta = sample_meta();
        meta.set.maker = "Nikon".into();
        meta.set.model = "Z 9".into();
        db.upsert_pending("20260514T180000Z-a1", &meta, Some("e1"), &[])
            .unwrap();
        db.upsert_pending("20260514T191230Z-9f", &meta, Some("e2"), &[])
            .unwrap();
        let ids = db.list_pending_upload_ids().unwrap();
        assert_eq!(ids.len(), 2);
        assert_eq!(
            db.get_pending_meta_etag("20260514T191230Z-9f").unwrap().as_deref(),
            Some("e2")
        );
        db.delete_pending_by_upload_id("20260514T180000Z-a1").unwrap();
        assert_eq!(db.list_pending_upload_ids().unwrap().len(), 1);
    }

    #[test]
    fn users_replace_and_lookup() {
        let dir = TempDir::new().unwrap();
        let db = Db::open(dir.path()).unwrap();
        let users = vec![UserRow {
            sub: "github:cytrinox".into(),
            display_name: Some("Daniel".into()),
            blocked: false,
            added_at: None,
            added_by: Some("bootstrap:admin".into()),
            roles: vec!["admin".into()],
            api_key_hash: None,
        }];
        db.replace_users(Some("etag-u1"), &users).unwrap();
        assert_eq!(db.get_users_etag().unwrap().as_deref(), Some("etag-u1"));
        let u = db.get_user("github:cytrinox").unwrap().unwrap();
        assert_eq!(u.roles, vec!["admin".to_string()]);
        assert!(!u.blocked);

        // Replace wipes previous users.
        db.replace_users(Some("etag-u2"), &[]).unwrap();
        assert!(db.get_user("github:cytrinox").unwrap().is_none());
    }
}
