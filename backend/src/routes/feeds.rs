//! Public Atom feeds: recently published sets and recently uploaded
//! (pending) sets.
//!
//! The XML is written by hand, like `meta::to_toml`. Every user-supplied
//! value goes through [`escape`]; entry bodies are HTML that is escaped a
//! second time inside `<content type="html">`.

use std::collections::HashMap;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use chrono::{DateTime, SecondsFormat, Utc};

use crate::cache::db::{Domain, FileRow, PendingSetRow, SetRow};
use crate::cache::responses::respond;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/feeds/published.atom", get(published))
        .route("/feeds/pending.atom", get(pending))
}

const ATOM_CONTENT_TYPE: &str = "application/atom+xml; charset=utf-8";
const FEED_CACHE_CONTROL: &str = "public, max-age=300";

/// Recently published sets, newest first. A set appears once, dated by its
/// first approval; later merges and edits don't add or move entries. Sets
/// approved before the approval record existed are not listed.
#[utoipa::path(
    get,
    path = "/api/feeds/published.atom",
    tag = "feeds",
    responses(
        (status = 200, description = "Atom feed", content_type = "application/atom+xml"),
        (status = 304, description = "Not modified (matching If-None-Match)"),
    ),
)]
pub async fn published(State(state): State<AppState>, headers: HeaderMap) -> AppResult<Response> {
    let base = base_url(state.config.public_url.as_deref(), &headers);
    let cached = state.responses.get_or_build(
        "feed_published",
        &base,
        state.db.stamp(&[Domain::Sets]),
        ATOM_CONTENT_TYPE,
        || {
            let mut entries = Vec::new();
            for set in state
                .db
                .recent_sets(state.config.feed_limit)
                .map_err(AppError::Other)?
            {
                let files = state
                    .db
                    .list_files(&set.maker, &set.model)
                    .map_err(AppError::Other)?;
                let tags = state
                    .db
                    .file_tags(&set.maker, &set.model)
                    .map_err(AppError::Other)?;
                entries.extend(published_entry(&base, &set, &files, &tags));
            }
            let self_url = format!("{base}/api/feeds/published.atom");
            Ok(render_feed(&state.config.feed_published_title, &self_url, &base, &entries)
                .into_bytes())
        },
    )?;
    Ok(respond(&cached, &headers, FEED_CACHE_CONTROL))
}

/// Recently uploaded sets waiting for review, newest first. Entries link
/// to the reviewer page; the uploader is never shown.
#[utoipa::path(
    get,
    path = "/api/feeds/pending.atom",
    tag = "feeds",
    responses(
        (status = 200, description = "Atom feed", content_type = "application/atom+xml"),
        (status = 304, description = "Not modified (matching If-None-Match)"),
    ),
)]
pub async fn pending(State(state): State<AppState>, headers: HeaderMap) -> AppResult<Response> {
    let base = base_url(state.config.public_url.as_deref(), &headers);
    let cached = state.responses.get_or_build(
        "feed_pending",
        &base,
        state.db.stamp(&[Domain::Pending]),
        ATOM_CONTENT_TYPE,
        || {
            let mut entries = Vec::new();
            for row in state
                .db
                .recent_pending(state.config.feed_limit)
                .map_err(AppError::Other)?
            {
                let files = state
                    .db
                    .list_pending_files(&row.upload_id)
                    .map_err(AppError::Other)?;
                entries.push(pending_entry(&base, &row, &files));
            }
            let self_url = format!("{base}/api/feeds/pending.atom");
            Ok(render_feed(&state.config.feed_pending_title, &self_url, &base, &entries)
                .into_bytes())
        },
    )?;
    Ok(respond(&cached, &headers, FEED_CACHE_CONTROL))
}

// ---- entries ----------------------------------------------------------------

struct Entry {
    id: String,
    title: String,
    link: String,
    published: Option<DateTime<Utc>>,
    updated: DateTime<Utc>,
    author: Option<String>,
    /// Unescaped HTML body; escaped once more when rendered.
    html: String,
}

/// Set-level facts shown at the top of both feeds' entry bodies.
struct Summary<'a> {
    maker: &'a str,
    model: &'a str,
    license: &'a str,
    uploaded_by: Option<&'a str>,
    notes: Option<&'a str>,
    special: bool,
}

/// `None` when the set has no `approved_initial_at` (never returned by
/// `recent_sets`, but the type allows it).
fn published_entry(
    base: &str,
    set: &SetRow,
    files: &[FileRow],
    tags: &HashMap<String, Vec<String>>,
) -> Option<Entry> {
    let at = set.approved_initial_at?;
    let set_url = format!("{base}/sets/{}/{}", enc(&set.maker), enc(&set.model));
    let summary = Summary {
        maker: &set.maker,
        model: &set.model,
        license: &set.license,
        uploaded_by: set.uploaded_by.as_deref(),
        notes: set.notes.as_deref(),
        special: set.special,
    };
    Some(Entry {
        id: format!("{set_url}#published-{}", ts(at)),
        title: format!("Samples published for {} - {}", set.maker, set.model),
        link: set_url,
        published: Some(at),
        updated: at,
        author: set.uploaded_by.clone(),
        html: published_html(base, &summary, files, tags),
    })
}

fn pending_entry(base: &str, row: &PendingSetRow, files: &[FileRow]) -> Entry {
    let url = format!("{base}/admin/pending/{}", enc(&row.upload_id));
    let summary = Summary {
        maker: &row.maker,
        model: &row.model,
        license: &row.license,
        // Unreviewed: the uploader is not published.
        uploaded_by: None,
        notes: row.notes.as_deref(),
        special: false,
    };
    Entry {
        id: url.clone(),
        title: format!("New pending upload for {} - {}", row.maker, row.model),
        link: url,
        published: None,
        updated: row.uploaded_at.unwrap_or_else(Utc::now),
        author: None,
        html: pending_html(&summary, files),
    }
}

fn pending_html(s: &Summary, files: &[FileRow]) -> String {
    format!(
        "<p>A new set was uploaded and is waiting for review. Its files are not \
         publicly available until it has been reviewed and approved.</p>{}",
        summary_html(s, files)
    )
}

/// Summary followed by every file, grouped by category, with its size,
/// note and tags. File names link to the public download endpoint.
fn published_html(
    base: &str,
    s: &Summary,
    files: &[FileRow],
    tags: &HashMap<String, Vec<String>>,
) -> String {
    let mut out = summary_html(s, files);
    // `files` arrives ordered by category, then path.
    let mut category: Option<&str> = None;
    for f in files {
        if category != Some(f.category.as_str()) {
            if category.is_some() {
                out.push_str("</ul>");
            }
            out.push_str(&format!("<h3>{}</h3><ul>", escape(&f.category)));
            category = Some(&f.category);
        }
        let name = f.path.split_once('/').map_or(f.path.as_str(), |(_, rest)| rest);
        let href = format!(
            "{base}/api/download/{}/{}/{}",
            enc(s.maker),
            enc(s.model),
            f.path.split('/').map(enc).collect::<Vec<_>>().join("/")
        );
        out.push_str(&format!(
            "<li><a href=\"{}\">{}</a> ({})",
            escape(&href),
            escape(name),
            human_size(f.size)
        ));
        if let Some(n) = f.notes.as_deref().filter(|n| !n.trim().is_empty()) {
            out.push_str(&format!(" — {}", multiline(n)));
        }
        if let Some(t) = tags.get(&f.path).filter(|t| !t.is_empty()) {
            out.push_str(&format!("<br/>Tags: {}", escape(&t.join(", "))));
        }
        out.push_str("</li>");
    }
    if category.is_some() {
        out.push_str("</ul>");
    }
    out
}

fn summary_html(s: &Summary, files: &[FileRow]) -> String {
    let total: u64 = files.iter().map(|f| f.size).sum();
    let mut files_line = format!("{} ({})", files.len(), human_size(total));
    let by_ext = count_by(
        files
            .iter()
            .filter_map(|f| f.extension.as_deref().map(str::to_ascii_uppercase)),
    );
    if !by_ext.is_empty() {
        let parts: Vec<String> = by_ext.iter().map(|(e, n)| format!("{n} × {e}")).collect();
        files_line.push_str(&format!(" — {}", parts.join(", ")));
    }
    let by_cat = count_by(files.iter().map(|f| f.category.clone()));
    let cats: Vec<String> = by_cat.iter().map(|(c, n)| format!("{c} ({n})")).collect();

    let mut items = vec![("Maker", escape(s.maker)), ("Model", escape(s.model))];
    if s.special {
        items.push(("Type", "Non-camera set".to_string()));
    }
    items.push(("Files", escape(&files_line)));
    if !cats.is_empty() {
        items.push(("Categories", escape(&cats.join(", "))));
    }
    items.push(("License", escape(s.license)));
    if let Some(by) = s.uploaded_by.filter(|b| !b.trim().is_empty()) {
        items.push(("Uploaded by", escape(by)));
    }
    if let Some(n) = s.notes.filter(|n| !n.trim().is_empty()) {
        items.push(("Note", multiline(n)));
    }

    let mut out = String::from("<ul>");
    for (label, value) in items {
        out.push_str(&format!("<li><strong>{label}:</strong> {value}</li>"));
    }
    out.push_str("</ul>");
    out
}

/// Occurrence counts, most frequent first, ties alphabetically.
fn count_by(items: impl Iterator<Item = String>) -> Vec<(String, usize)> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for i in items {
        *counts.entry(i).or_default() += 1;
    }
    let mut out: Vec<_> = counts.into_iter().collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

// ---- feed envelope ----------------------------------------------------------

fn render_feed(title: &str, self_url: &str, site_url: &str, entries: &[Entry]) -> String {
    let updated = entries
        .iter()
        .map(|e| e.updated)
        .max()
        .unwrap_or_else(Utc::now);
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    out.push_str("<feed xmlns=\"http://www.w3.org/2005/Atom\">\n");
    out.push_str(&format!("  <title>{}</title>\n", escape(title)));
    out.push_str(&format!("  <id>{}</id>\n", escape(self_url)));
    out.push_str(&format!(
        "  <link rel=\"self\" type=\"application/atom+xml\" href=\"{}\"/>\n",
        escape(self_url)
    ));
    out.push_str(&format!(
        "  <link rel=\"alternate\" type=\"text/html\" href=\"{}/\"/>\n",
        escape(site_url)
    ));
    out.push_str(&format!("  <updated>{}</updated>\n", ts(updated)));
    // Feed-level author satisfies Atom for entries without their own.
    out.push_str("  <author><name>RawDB</name></author>\n");
    for e in entries {
        out.push_str("  <entry>\n");
        out.push_str(&format!("    <id>{}</id>\n", escape(&e.id)));
        out.push_str(&format!("    <title>{}</title>\n", escape(&e.title)));
        out.push_str(&format!(
            "    <link rel=\"alternate\" type=\"text/html\" href=\"{}\"/>\n",
            escape(&e.link)
        ));
        if let Some(p) = e.published {
            out.push_str(&format!("    <published>{}</published>\n", ts(p)));
        }
        out.push_str(&format!("    <updated>{}</updated>\n", ts(e.updated)));
        if let Some(a) = e.author.as_deref().filter(|a| !a.trim().is_empty()) {
            out.push_str(&format!("    <author><name>{}</name></author>\n", escape(a)));
        }
        out.push_str(&format!(
            "    <content type=\"html\">{}</content>\n",
            escape(&e.html)
        ));
        out.push_str("  </entry>\n");
    }
    out.push_str("</feed>\n");
    out
}

// ---- helpers ----------------------------------------------------------------

/// Absolute site URL without a trailing slash: `RAWDB_PUBLIC_URL` when
/// set, else derived from the (proxy) request headers.
fn base_url(configured: Option<&str>, headers: &HeaderMap) -> String {
    if let Some(u) = configured.map(str::trim).filter(|u| !u.is_empty()) {
        return u.trim_end_matches('/').to_string();
    }
    // Proxies may append comma-separated values; the first is the client's.
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    let proto = header("x-forwarded-proto")
        .filter(|p| p == "http" || p == "https")
        .unwrap_or_else(|| "http".to_string());
    let host = header("x-forwarded-host")
        .or_else(|| header("host"))
        .unwrap_or_else(|| "localhost".to_string());
    format!("{proto}://{host}")
}

/// Escape for XML/HTML text and attributes. Drops control characters
/// that XML 1.0 forbids (notes are free text).
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            '\t' | '\n' | '\r' => out.push(c),
            c if (c as u32) < 0x20 || c == '\u{FFFE}' || c == '\u{FFFF}' => {}
            c => out.push(c),
        }
    }
    out
}

/// Escaped text with line breaks kept.
fn multiline(s: &str) -> String {
    escape(s.trim()).replace('\n', "<br/>")
}

/// Percent-encode one URL path segment.
fn enc(s: &str) -> String {
    urlencoding::encode(s).into_owned()
}

fn ts(d: DateTime<Utc>) -> String {
    d.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Mirrors the frontend's `formatBytes`.
fn human_size(n: u64) -> String {
    if n < 1024 {
        return format!("{n} B");
    }
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    let mut v = n as f64 / 1024.0;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if v >= 10.0 {
        format!("{v:.0} {}", UNITS[i])
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, size: u64, notes: Option<&str>) -> FileRow {
        FileRow {
            path: path.into(),
            category: path.split('/').next().unwrap().into(),
            extension: path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()),
            size,
            license: None,
            notes: notes.map(Into::into),
            sha256: None,
        }
    }

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn set_row() -> SetRow {
        SetRow {
            maker: "Canon".into(),
            model: "EOS R5".into(),
            license: "CC0 1.0".into(),
            notes: Some("Base ISO <b>only</b>".into()),
            uploaded_at: Some(at("2026-10-01T00:00:00Z")),
            uploaded_by: Some("Jane Doe".into()),
            special: false,
            approved_at: Some(at("2026-10-23T10:00:00Z")),
            approved_by: Some("admin".into()),
            approved_initial_at: Some(at("2026-10-02T09:41:07Z")),
            approved_initial_by: Some("admin".into()),
        }
    }

    fn pending_row() -> PendingSetRow {
        PendingSetRow {
            maker: "Canon".into(),
            model: "EOS R5".into(),
            upload_id: "20261023T091200Z-3fa9c2d1".into(),
            license: "CC0 1.0".into(),
            notes: Some("Shot at base ISO".into()),
            uploaded_at: Some(at("2026-10-23T09:12:00Z")),
        }
    }

    #[test]
    fn escape_handles_markup_and_control_chars() {
        assert_eq!(
            escape("<script>a&b</script>\u{0}\u{1b}\n"),
            "&lt;script&gt;a&amp;b&lt;/script&gt;\n"
        );
    }

    #[test]
    fn published_entry_title_body_and_stable_id() {
        let files = vec![
            file("crm/CLIP_0001.CRM", 109 * 1024 * 1024, None),
            file("raw_modes/IMG_0001.CR3", 45 * 1024 * 1024, Some("Full RAW")),
            file("raw_modes/IMG_0002.CR3", 28 * 1024 * 1024, None),
        ];
        let tags = HashMap::from([(
            "raw_modes/IMG_0001.CR3".to_string(),
            vec!["lossless".to_string(), "14bit".to_string()],
        )]);
        let e = published_entry("https://rawdb.example", &set_row(), &files, &tags).unwrap();
        assert_eq!(e.title, "Samples published for Canon - EOS R5");
        assert_eq!(e.link, "https://rawdb.example/sets/Canon/EOS%20R5");
        assert_eq!(
            e.id,
            "https://rawdb.example/sets/Canon/EOS%20R5#published-2026-10-02T09:41:07Z"
        );
        assert!(e.html.contains("<li><strong>Files:</strong> 3 (182 MB) — 2 × CR3, 1 × CRM</li>"));
        assert!(e.html.contains("<li><strong>Categories:</strong> raw_modes (2), crm (1)</li>"));
        assert!(e.html.contains("<li><strong>Uploaded by:</strong> Jane Doe</li>"));
        assert!(e.html.contains("Base ISO &lt;b&gt;only&lt;/b&gt;"));
        assert!(e.html.contains(
            "<a href=\"https://rawdb.example/api/download/Canon/EOS%20R5/raw_modes/IMG_0001.CR3\">IMG_0001.CR3</a> (45 MB) — Full RAW<br/>Tags: lossless, 14bit"
        ));
        assert!(e.html.contains("<h3>crm</h3>") && e.html.contains("<h3>raw_modes</h3>"));
    }

    #[test]
    fn published_entry_requires_initial_approval() {
        let mut set = set_row();
        set.approved_initial_at = None;
        assert!(published_entry("http://x", &set, &[], &HashMap::new()).is_none());
    }

    #[test]
    fn pending_entry_never_shows_uploader() {
        let files = vec![file("raw_modes/IMG_0001.CR3", 2048, None)];
        let e = pending_entry("https://rawdb.example", &pending_row(), &files);
        assert_eq!(e.title, "New pending upload for Canon - EOS R5");
        assert_eq!(
            e.link,
            "https://rawdb.example/admin/pending/20261023T091200Z-3fa9c2d1"
        );
        assert!(e.author.is_none());
        assert!(!e.html.contains("Jane Doe"));
        assert!(!e.html.contains("Uploaded"));
        assert!(e.html.contains("not publicly available"));
        assert!(e.html.contains("<li><strong>Note:</strong> Shot at base ISO</li>"));
        let feed = render_feed("Pending", "https://rawdb.example/api/feeds/pending.atom", "https://rawdb.example", &[e]);
        assert!(!feed.contains("Jane Doe"));
    }

    #[test]
    fn feed_double_escapes_html_content() {
        let e = pending_entry("http://x", &pending_row(), &[]);
        let feed = render_feed("T", "http://x/f", "http://x", &[e]);
        assert!(feed.contains("<content type=\"html\">&lt;p&gt;A new set"));
        assert!(feed.contains("<updated>2026-10-23T09:12:00Z</updated>"));
    }

    #[test]
    fn empty_feed_still_has_updated() {
        let feed = render_feed("RawDB & co", "http://x/f", "http://x", &[]);
        assert!(feed.contains("<title>RawDB &amp; co</title>"));
        assert!(feed.contains("<updated>"));
        assert!(!feed.contains("<entry>"));
    }

    #[test]
    fn base_url_prefers_config_then_forwarded_headers() {
        let mut h = HeaderMap::new();
        h.insert("host", "internal:8080".parse().unwrap());
        assert_eq!(base_url(Some("https://rawdb.example/"), &h), "https://rawdb.example");
        assert_eq!(base_url(None, &h), "http://internal:8080");
        h.insert("x-forwarded-proto", "https".parse().unwrap());
        h.insert("x-forwarded-host", "rawdb.example, proxy".parse().unwrap());
        assert_eq!(base_url(None, &h), "https://rawdb.example");
    }

    #[test]
    fn human_size_matches_frontend() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(45 * 1024 * 1024), "45 MB");
    }
}
