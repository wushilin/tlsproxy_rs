use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use crate::http_header::HttpHead;

/// Directory listings are paginated; each page shows at most this many
/// entries and links to the next page via the `?after=<name>` cursor.
const PAGE_SIZE: usize = 100_000;

pub async fn not_found<S: AsyncWrite + Unpin>(mut client: S, head_only: bool) -> Result<()> {
    response(&mut client, 404, "text/plain; charset=utf-8", b"Not Found\n", head_only).await
}

pub async fn serve<S>(mut client: S, head: HttpHead, prefix: &str, document_root: &str, index: Option<&str>, directory_listing: bool) -> Result<()>
where S: AsyncRead + AsyncWrite + Unpin {
    let head_only = head.method == "HEAD";
    if head.method != "GET" && !head_only { return response(&mut client, 405, "text/plain; charset=utf-8", b"Method Not Allowed\n", false).await; }
    let root = match tokio::fs::canonicalize(document_root).await {
        Ok(root) => root,
        Err(cause) => {
            log::warn!("static document root `{document_root}` is unavailable: {cause}");
            return response(&mut client, 500, "text/plain; charset=utf-8", b"Internal Server Error\n", head_only).await;
        }
    };
    if !tokio::fs::metadata(&root).await.map(|metadata| metadata.is_dir()).unwrap_or(false) {
        log::warn!("static document root `{document_root}` is not a directory");
        return response(&mut client, 500, "text/plain; charset=utf-8", b"Internal Server Error\n", head_only).await;
    }
    let (request_path, query) = match head.target.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (head.target.as_str(), None),
    };
    let relative = request_path.strip_prefix(prefix.trim_end_matches('/')).unwrap_or(request_path);
    let decoded = match decode_path(relative.trim_start_matches('/')) {
        Ok(decoded) => decoded,
        Err(_) => return response(&mut client, 400, "text/plain; charset=utf-8", b"Bad Request\n", head_only).await,
    };
    let candidate = root.join(decoded);
    let mut physical = match canonical_beneath(&root, &candidate).await {
        Ok(Some(path)) => path,
        Ok(None) => return response(&mut client, 404, "text/plain; charset=utf-8", b"Not Found\n", head_only).await,
        Err(_) => return response(&mut client, 403, "text/plain; charset=utf-8", b"Forbidden\n", head_only).await,
    };
    let mut metadata = match tokio::fs::metadata(&physical).await {
        Ok(metadata) => metadata,
        Err(cause) if cause.kind() == std::io::ErrorKind::PermissionDenied => {
            return response(&mut client, 403, "text/plain; charset=utf-8", b"Forbidden\n", head_only).await
        }
        Err(_) => return response(&mut client, 404, "text/plain; charset=utf-8", b"Not Found\n", head_only).await,
    };
    if metadata.is_dir() {
        // Relative hrefs in listings and index pages only resolve correctly
        // under a trailing-slash URL.
        if !request_path.ends_with('/') {
            return redirect(&mut client, &format!("{request_path}/")).await;
        }
        if let Some(index) = index {
            if let Ok(canonical) = tokio::fs::canonicalize(physical.join(index)).await {
                if canonical.starts_with(&root) && tokio::fs::metadata(&canonical).await.map(|metadata| metadata.is_file()).unwrap_or(false) {
                    metadata = tokio::fs::metadata(&canonical).await?;
                    physical = canonical;
                }
            }
        }
        if metadata.is_dir() {
            if !directory_listing { return response(&mut client, 403, "text/plain; charset=utf-8", b"Directory listing disabled\n", head_only).await; }
            let after = query.and_then(query_after);
            let listing = {
                let physical = physical.clone();
                tokio::task::spawn_blocking(move || scan_directory(&physical, after.as_deref(), PAGE_SIZE))
                    .await
                    .context("directory listing task failed")??
            };
            let body = directory_page(&listing, request_path);
            return listing_response(&mut client, body.as_bytes(), head_only).await;
        }
    }
    if !metadata.is_file() { return response(&mut client, 404, "text/plain; charset=utf-8", b"Not Found\n", head_only).await; }
    serve_file(&mut client, &head, &physical, &metadata, head_only).await
}

/// Serves one regular file with validator and range support: `ETag` +
/// `Last-Modified` with `If-None-Match`/`If-Modified-Since` revalidation, and
/// single `Range: bytes=` requests guarded by `If-Range`.
async fn serve_file<S: AsyncWrite + Unpin>(stream: &mut S, head: &HttpHead, physical: &Path, metadata: &std::fs::Metadata, head_only: bool) -> Result<()> {
    let length = metadata.len();
    let modified = metadata.modified().ok();
    let last_modified = modified.map(http_date);
    let etag = entity_tag(length, modified);

    let revalidated = match head.header_value("if-none-match") {
        Some(value) => value.split(',').any(|tag| { let tag = tag.trim(); tag == "*" || tag == etag }),
        None => matches!((head.header_value("if-modified-since"), &last_modified), (Some(since), Some(current)) if since == *current),
    };
    if revalidated {
        let header = format!(
            "HTTP/1.1 304 Not Modified\r\nETag: {etag}\r\n{}Connection: close\r\n\r\n",
            last_modified.as_deref().map(|value| format!("Last-Modified: {value}\r\n")).unwrap_or_default()
        );
        stream.write_all(header.as_bytes()).await?;
        stream.shutdown().await?;
        return Ok(());
    }

    // A stale If-Range validator means the stored partial body no longer
    // matches this file; ignore the Range and send the full entity instead.
    let range_applicable = match head.header_value("if-range") {
        Some(validator) => validator == etag || last_modified.as_deref() == Some(validator.as_str()),
        None => true,
    };
    let range = if head_only || !range_applicable { None } else { head.header_value("range").and_then(|value| parse_range(&value, length)) };
    if let Some(RangeSpec::Unsatisfiable) = range {
        let header = format!(
            "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{length}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(header.as_bytes()).await?;
        stream.shutdown().await?;
        return Ok(());
    }

    let mut file = match tokio::fs::File::open(physical).await {
        Ok(file) => file,
        Err(cause) if cause.kind() == std::io::ErrorKind::PermissionDenied => {
            return response(stream, 403, "text/plain; charset=utf-8", b"Forbidden\n", head_only).await
        }
        Err(_) => return response(stream, 404, "text/plain; charset=utf-8", b"Not Found\n", head_only).await,
    };

    let validators = format!(
        "ETag: {etag}\r\n{}Accept-Ranges: bytes\r\nCache-Control: public, no-cache\r\nX-Content-Type-Options: nosniff\r\n",
        last_modified.as_deref().map(|value| format!("Last-Modified: {value}\r\n")).unwrap_or_default()
    );
    let (status_line, offset, body_length, content_range) = match range {
        Some(RangeSpec::Bounded { start, end }) => (
            "HTTP/1.1 206 Partial Content",
            start,
            end - start + 1,
            format!("Content-Range: bytes {start}-{end}/{length}\r\n"),
        ),
        _ => ("HTTP/1.1 200 OK", 0, length, String::new()),
    };
    let header = format!(
        "{status_line}\r\nContent-Type: {}\r\nContent-Length: {body_length}\r\n{content_range}{validators}Connection: close\r\n\r\n",
        content_type(physical)
    );
    stream.write_all(header.as_bytes()).await?;
    if !head_only {
        if offset > 0 {
            tokio::io::AsyncSeekExt::seek(&mut file, std::io::SeekFrom::Start(offset)).await?;
        }
        stream_body(stream, &mut file, body_length).await?;
    }
    stream.shutdown().await?;
    Ok(())
}

/// Streams the body in fixed-size chunks so concurrent large downloads do not
/// buffer whole files in memory.
async fn stream_body<S: AsyncWrite + Unpin>(stream: &mut S, file: &mut tokio::fs::File, length: u64) -> Result<()> {
    let mut remaining = length;
    let mut chunk = vec![0u8; 64 * 1024];
    while remaining > 0 {
        let want = remaining.min(chunk.len() as u64) as usize;
        let count = tokio::io::AsyncReadExt::read(file, &mut chunk[..want]).await?;
        if count == 0 { bail!("static file truncated while streaming"); }
        stream.write_all(&chunk[..count]).await?;
        remaining -= count as u64;
    }
    Ok(())
}

enum RangeSpec {
    Bounded { start: u64, end: u64 },
    Unsatisfiable,
}

/// Parses a single-range `bytes=` header. Multi-range requests and malformed
/// values return `None`, which serves the full entity (permitted by RFC 9110).
fn parse_range(value: &str, length: u64) -> Option<RangeSpec> {
    let spec = value.trim().strip_prefix("bytes=")?.trim();
    if spec.contains(',') { return None; }
    let (start, end) = spec.split_once('-')?;
    let (start, end) = (start.trim(), end.trim());
    if start.is_empty() {
        // Suffix form `-N`: the final N bytes.
        let suffix: u64 = end.parse().ok()?;
        if suffix == 0 || length == 0 { return Some(RangeSpec::Unsatisfiable); }
        let start = length.saturating_sub(suffix);
        return Some(RangeSpec::Bounded { start, end: length - 1 });
    }
    let start: u64 = start.parse().ok()?;
    if start >= length { return Some(RangeSpec::Unsatisfiable); }
    let end = if end.is_empty() { length - 1 } else { end.parse().ok()? };
    if end < start { return None; }
    Some(RangeSpec::Bounded { start, end: end.min(length - 1) })
}

fn entity_tag(length: u64, modified: Option<SystemTime>) -> String {
    let stamp = modified
        .and_then(|value| value.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|duration| (duration.as_secs(), duration.subsec_nanos()))
        .unwrap_or((0, 0));
    format!("\"{length:x}-{:x}.{:x}\"", stamp.0, stamp.1)
}

fn http_date(value: SystemTime) -> String {
    let time = time::OffsetDateTime::from(value);
    const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        DAYS[time.weekday().number_days_from_monday() as usize],
        time.day(),
        MONTHS[time.month() as usize - 1],
        time.year(),
        time.hour(),
        time.minute(),
        time.second()
    )
}

async fn redirect<S: AsyncWrite + Unpin>(stream: &mut S, location: &str) -> Result<()> {
    let header = format!("HTTP/1.1 301 Moved Permanently\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    stream.write_all(header.as_bytes()).await?;
    stream.shutdown().await?;
    Ok(())
}

async fn canonical_beneath(root: &Path, candidate: &Path) -> Result<Option<PathBuf>> {
    let physical = match tokio::fs::canonicalize(candidate).await {
        Ok(path) => path,
        Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(cause) => return Err(cause.into()),
    };
    if !physical.starts_with(root) { bail!("static path escapes the physical document root"); }
    Ok(Some(physical))
}

fn percent_decode(value: &str) -> Result<String> {
    let input = value.as_bytes();
    let mut bytes = Vec::with_capacity(input.len());
    let mut offset = 0;
    while offset < input.len() {
        if input[offset] == b'%' {
            if offset + 2 >= input.len() { bail!("invalid percent encoding"); }
            let hex = std::str::from_utf8(&input[offset + 1..offset + 3])?;
            bytes.push(u8::from_str_radix(hex, 16).context("invalid percent encoding")?);
            offset += 3;
        } else {
            bytes.push(input[offset]);
            offset += 1;
        }
    }
    if bytes.contains(&0) { bail!("NUL byte in encoded value"); }
    String::from_utf8(bytes).context("encoded value is not UTF-8")
}

fn decode_path(value: &str) -> Result<PathBuf> {
    let decoded = percent_decode(value).context("request path")?;
    let path = Path::new(&decoded);
    if path.components().any(|part| matches!(part, Component::ParentDir | Component::RootDir | Component::Prefix(_))) { bail!("request path attempts to escape the document root"); }
    Ok(path.to_path_buf())
}

fn query_after(query: &str) -> Option<String> {
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix("after="))
        .and_then(|value| percent_decode(value).ok())
}

struct ListedEntry {
    name: String,
    is_dir: bool,
    size: u64,
    modified: Option<SystemTime>,
}

struct Listing {
    directories: usize,
    files: usize,
    /// 1-based ordinal of the first entry on this page.
    start_ordinal: usize,
    entries: Vec<ListedEntry>,
    next_after: Option<String>,
}

/// Scans a directory in one blocking task: names are collected and sorted
/// byte-lexicographically (S3-style, so the `after` cursor is a plain name
/// comparison), and metadata is fetched only for the returned page.
fn scan_directory(path: &Path, after: Option<&str>, page_size: usize) -> std::io::Result<Listing> {
    let mut names: Vec<(String, bool)> = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let Ok(entry) = entry else { continue };
        let is_dir = entry
            .file_type()
            .map(|kind| if kind.is_symlink() { entry.path().metadata().map(|m| m.is_dir()).unwrap_or(false) } else { kind.is_dir() })
            .unwrap_or(false);
        names.push((entry.file_name().to_string_lossy().into_owned(), is_dir));
    }
    let directories = names.iter().filter(|(_, is_dir)| *is_dir).count();
    let files = names.len() - directories;
    names.sort_by(|a, b| a.0.cmp(&b.0));
    let start = match after {
        Some(cursor) => names.partition_point(|(name, _)| name.as_str() <= cursor),
        None => 0,
    };
    let page_end = (start + page_size).min(names.len());
    let next_after = (page_end < names.len()).then(|| names[page_end - 1].0.clone());
    let entries = names[start..page_end]
        .iter()
        .map(|(name, is_dir)| {
            let metadata = std::fs::metadata(path.join(name)).ok();
            ListedEntry {
                name: name.clone(),
                is_dir: *is_dir,
                size: metadata.as_ref().map(|m| m.len()).unwrap_or(0),
                modified: metadata.and_then(|m| m.modified().ok()),
            }
        })
        .collect();
    Ok(Listing { directories, files, start_ordinal: start + 1, entries, next_after })
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 { return format!("{bytes} B"); }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

fn listing_date(value: Option<SystemTime>) -> String {
    match value {
        Some(value) => {
            let time = time::OffsetDateTime::from(value);
            format!("{:04}-{:02}-{:02} {:02}:{:02}", time.year(), time.month() as u8, time.day(), time.hour(), time.minute())
        }
        None => "—".to_string(),
    }
}

fn breadcrumbs(request_path: &str) -> String {
    let mut html = String::from("<a href=\"/\">/</a>");
    let mut href = String::from("/");
    let segments: Vec<&str> = request_path.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    for (position, segment) in segments.iter().enumerate() {
        href.push_str(segment);
        href.push('/');
        if position + 1 == segments.len() {
            html.push_str(&format!("<span>{}</span>", html_escape(segment)));
        } else {
            html.push_str(&format!("<a href=\"{}\">{}</a><i>/</i>", html_escape(&href), html_escape(segment)));
        }
    }
    html
}

fn directory_page(listing: &Listing, request_path: &str) -> String {
    let mut rows = String::new();
    if request_path != "/" {
        rows.push_str("<tr class=d><td><a href=\"../\">..</a></td><td class=s>—</td><td class=m>—</td></tr>");
    }
    for entry in &listing.entries {
        let class = if entry.is_dir { "d" } else { "f" };
        let suffix = if entry.is_dir { "/" } else { "" };
        let size = if entry.is_dir { "—".to_string() } else { human_size(entry.size) };
        rows.push_str(&format!(
            "<tr class={class}><td><a href=\"{href}{suffix}\">{name}{suffix}</a></td><td class=s>{size}</td><td class=m>{date}</td></tr>",
            href = url_escape(&entry.name),
            name = html_escape(&entry.name),
            date = listing_date(entry.modified),
        ));
    }
    let total = listing.directories + listing.files;
    let shown = listing.entries.len();
    let range_note = if total > shown {
        format!(
            "Showing {}&#8202;–&#8202;{} of {} entries",
            listing.start_ordinal,
            listing.start_ordinal + shown.saturating_sub(1),
            total
        )
    } else {
        format!("{} {} · {} {}", listing.directories, if listing.directories == 1 { "folder" } else { "folders" }, listing.files, if listing.files == 1 { "file" } else { "files" })
    };
    let next = listing
        .next_after
        .as_deref()
        .map(|cursor| format!("<a class=next href=\"?after={}\">Next page &rarr;</a>", url_escape(cursor)))
        .unwrap_or_default();
    format!(
        r##"<!doctype html><html lang=en><meta charset=utf-8><meta name=viewport content="width=device-width,initial-scale=1"><title>Index of {title}</title><style>
:root{{--bg:#fafbfd;--panel:#fff;--text:#1c2433;--muted:#6b7686;--line:#e5e9f1;--accent:#2563c9;--hover:#f0f4fb}}
@media(prefers-color-scheme:dark){{:root{{--bg:#12161e;--panel:#1a202b;--text:#e6eaf2;--muted:#8b95a7;--line:#2a3240;--accent:#6ba3f5;--hover:#212936}}}}
*{{box-sizing:border-box}}body{{margin:0;background:var(--bg);color:var(--text);font:15px/1.5 system-ui,-apple-system,"Segoe UI",sans-serif}}
main{{max-width:64rem;margin:0 auto;padding:2.5rem 1.25rem 4rem}}
nav.crumbs{{font-size:1.35rem;font-weight:600;word-break:break-all}}nav.crumbs a{{color:var(--accent);text-decoration:none}}nav.crumbs a:hover{{text-decoration:underline}}nav.crumbs i{{font-style:normal;color:var(--muted);margin:0 .15em}}nav.crumbs span{{color:var(--text)}}
p.meta{{color:var(--muted);margin:.4rem 0 1.4rem;font-size:.9rem}}
table{{width:100%;border-collapse:collapse;background:var(--panel);border:1px solid var(--line);border-radius:10px;overflow:hidden;box-shadow:0 1px 2px rgba(16,24,40,.05)}}
thead th{{text-align:left;font-size:.75rem;letter-spacing:.06em;text-transform:uppercase;color:var(--muted);font-weight:600;padding:.65rem .9rem;border-bottom:1px solid var(--line)}}
tbody td{{padding:0;border-bottom:1px solid var(--line)}}tbody tr:last-child td{{border-bottom:0}}tbody tr:hover{{background:var(--hover)}}
td a{{display:block;padding:.55rem .9rem;color:var(--text);text-decoration:none;word-break:break-all}}
tr.d td a{{font-weight:600}}tr.d td a::before{{content:"📁";margin-right:.55em}}tr.f td a::before{{content:"📄";margin-right:.55em}}
td.s,td.m{{padding:.55rem .9rem;color:var(--muted);white-space:nowrap;font-variant-numeric:tabular-nums}}td.s{{text-align:right}}
th.s{{text-align:right}}th.s,td.s{{width:7.5rem}}th.m,td.m{{width:10.5rem}}
footer{{display:flex;justify-content:space-between;align-items:center;color:var(--muted);font-size:.85rem;margin-top:1rem}}
a.next{{color:var(--accent);text-decoration:none;font-weight:600}}a.next:hover{{text-decoration:underline}}
@media(max-width:640px){{th.m,td.m{{display:none}}}}
</style><body><main><nav class=crumbs>{crumbs}</nav><p class=meta>{range_note}</p><table><thead><tr><th>Name</th><th class=s>Size</th><th class=m>Modified</th></tr></thead><tbody>{rows}</tbody></table><footer><span>tlsproxy</span>{next}</footer></main></body></html>"##,
        title = html_escape(request_path),
        crumbs = breadcrumbs(request_path),
    )
}

async fn listing_response<S: AsyncWrite + Unpin>(stream: &mut S, body: &[u8], head_only: bool) -> Result<()> {
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nConnection: close\r\nX-Content-Type-Options: nosniff\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    if !head_only { stream.write_all(body).await?; }
    stream.shutdown().await?;
    Ok(())
}

async fn response<S: AsyncWrite + Unpin>(
    stream: &mut S,
    status: u16,
    content_type: &str,
    body: &[u8],
    head_only: bool,
) -> Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        _ => "Error",
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nX-Content-Type-Options: nosniff\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    if !head_only {
        stream.write_all(body).await?;
    }
    stream.shutdown().await?;
    Ok(())
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|v| v.to_str()).unwrap_or("").to_ascii_lowercase().as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "txt" | "md" => "text/plain; charset=utf-8",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn url_escape(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn traversal_is_rejected_after_decoding() {
        assert!(decode_path("../secret").is_err());
        assert!(decode_path("%2e%2e/secret").is_err());
        assert!(decode_path("assets/app.js").is_ok());
        assert!(decode_path("a%00b").is_err());
    }

    #[test]
    fn range_parsing_covers_bounded_open_and_suffix_forms() {
        assert!(matches!(parse_range("bytes=0-99", 1000), Some(RangeSpec::Bounded { start: 0, end: 99 })));
        assert!(matches!(parse_range("bytes=500-", 1000), Some(RangeSpec::Bounded { start: 500, end: 999 })));
        assert!(matches!(parse_range("bytes=-100", 1000), Some(RangeSpec::Bounded { start: 900, end: 999 })));
        assert!(matches!(parse_range("bytes=0-5000", 1000), Some(RangeSpec::Bounded { start: 0, end: 999 })));
        assert!(matches!(parse_range("bytes=1000-", 1000), Some(RangeSpec::Unsatisfiable)));
        assert!(parse_range("bytes=5-2", 1000).is_none());
        assert!(parse_range("bytes=0-1,5-9", 1000).is_none());
        assert!(parse_range("items=0-1", 1000).is_none());
    }

    #[test]
    fn listing_pages_are_sorted_and_cursor_resumes_strictly_after() {
        let directory = tempfile::tempdir().unwrap();
        for name in ["banana.txt", "apple.txt", "cherry.txt", "date.txt"] {
            std::fs::write(directory.path().join(name), b"x").unwrap();
        }
        std::fs::create_dir(directory.path().join("zoo")).unwrap();

        let first = scan_directory(directory.path(), None, 2).unwrap();
        assert_eq!(first.entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), vec!["apple.txt", "banana.txt"]);
        assert_eq!(first.next_after.as_deref(), Some("banana.txt"));
        assert_eq!(first.start_ordinal, 1);
        assert_eq!((first.directories, first.files), (1, 4));

        let second = scan_directory(directory.path(), first.next_after.as_deref(), 2).unwrap();
        assert_eq!(second.entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), vec!["cherry.txt", "date.txt"]);
        assert_eq!(second.start_ordinal, 3);

        let third = scan_directory(directory.path(), second.next_after.as_deref(), 2).unwrap();
        assert_eq!(third.entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), vec!["zoo"]);
        assert!(third.entries[0].is_dir);
        assert!(third.next_after.is_none());

        // A cursor naming a deleted entry still resumes at the right position.
        let resumed = scan_directory(directory.path(), Some("b-missing"), 100).unwrap();
        assert_eq!(resumed.entries[0].name, "banana.txt");
    }

    #[test]
    fn listing_html_escapes_names_and_links_next_page() {
        let listing = Listing {
            directories: 0,
            files: 2,
            start_ordinal: 1,
            entries: vec![ListedEntry { name: "<script>.txt".into(), is_dir: false, size: 10, modified: None }],
            next_after: Some("a&b.txt".into()),
        };
        let page = directory_page(&listing, "/downloads/");
        assert!(page.contains("&lt;script&gt;.txt"));
        assert!(!page.contains("<script>.txt"));
        assert!(page.contains("?after=a%26b.txt"));
    }

    #[tokio::test]
    async fn conditional_and_range_requests_round_trip() {
        use tokio::io::AsyncReadExt;
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("data.bin"), b"0123456789").unwrap();

        async fn request(root: &str, extra_headers: &str) -> String {
            let (mut client, server) = tokio::io::duplex(1 << 20);
            let raw = format!("GET /data.bin HTTP/1.1\r\nHost: files.example\r\n{extra_headers}\r\n");
            let head = {
                let mut reader = std::io::Cursor::new(raw.into_bytes());
                crate::http_header::read_http_head(&mut reader, std::time::Duration::from_secs(1), 65536).await.unwrap()
            };
            let root = root.to_string();
            let task = tokio::spawn(async move { serve(server, head, "/", &root, None, true).await });
            let mut output = Vec::new();
            client.read_to_end(&mut output).await.unwrap();
            task.await.unwrap().unwrap();
            String::from_utf8_lossy(&output).into_owned()
        }

        let root = directory.path().to_str().unwrap();
        let full = request(root, "").await;
        assert!(full.starts_with("HTTP/1.1 200 OK"));
        assert!(full.contains("Accept-Ranges: bytes"));
        assert!(full.ends_with("0123456789"));
        let etag = full.lines().find(|l| l.starts_with("ETag:")).unwrap().trim_start_matches("ETag:").trim().to_string();

        let revalidated = request(root, &format!("If-None-Match: {etag}\r\n")).await;
        assert!(revalidated.starts_with("HTTP/1.1 304 Not Modified"));

        let partial = request(root, "Range: bytes=2-5\r\n").await;
        assert!(partial.starts_with("HTTP/1.1 206 Partial Content"));
        assert!(partial.contains("Content-Range: bytes 2-5/10"));
        assert!(partial.ends_with("2345"));

        let stale = request(root, "Range: bytes=2-5\r\nIf-Range: \"different\"\r\n").await;
        assert!(stale.starts_with("HTTP/1.1 200 OK"));
        assert!(stale.ends_with("0123456789"));

        let beyond = request(root, "Range: bytes=99-\r\n").await;
        assert!(beyond.starts_with("HTTP/1.1 416 Range Not Satisfiable"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_may_be_followed_but_not_outside_physical_root() {
        use std::os::unix::fs::symlink;
        let root_parent = tempfile::tempdir().unwrap();
        let outside_parent = tempfile::tempdir().unwrap();
        let root = root_parent.path().join("www");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("inside.txt"), "inside").unwrap();
        std::fs::write(outside_parent.path().join("secret.txt"), "secret").unwrap();
        symlink(root.join("inside.txt"), root.join("inside-link")).unwrap();
        symlink(outside_parent.path().join("secret.txt"), root.join("outside-link")).unwrap();
        let canonical_root = tokio::fs::canonicalize(&root).await.unwrap();
        assert!(canonical_beneath(&canonical_root, &root.join("inside-link")).await.unwrap().is_some());
        assert!(canonical_beneath(&canonical_root, &root.join("outside-link")).await.is_err());
    }
}
