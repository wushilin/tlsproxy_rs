//! Static file serving with sensible production defaults: a broad MIME
//! table, negotiated gzip/zstd compression, validator-based caching
//! (`ETag`/`Last-Modified`/304), single-range requests, paginated
//! symlink-aware directory listings, and styled built-in error pages.

use std::io::Write as _;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use crate::http_header::HttpHead;

use super::error_pages;

/// Directory listings are paginated; each page shows at most this many
/// entries and links to the next page via the `?after=<name>` cursor.
const PAGE_SIZE: usize = 100_000;

/// Bodies smaller than this are never compressed; the frames would cost more
/// than they save.
const MIN_COMPRESS_BYTES: u64 = 256;

pub async fn not_found<S: AsyncWrite + Unpin>(mut client: S, head_only: bool) -> Result<()> {
    error_response(&mut client, 404, head_only).await
}

pub async fn serve<S>(mut client: S, head: HttpHead, prefix: &str, document_root: &str, index: Option<&str>, directory_listing: bool) -> Result<()>
where S: AsyncRead + AsyncWrite + Unpin {
    let head_only = head.method == "HEAD";
    if head.method == "OPTIONS" {
        let header = "HTTP/1.1 204 No Content\r\nAllow: GET, HEAD, OPTIONS\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        client.write_all(header.as_bytes()).await?;
        client.shutdown().await?;
        return Ok(());
    }
    if head.method != "GET" && !head_only { return error_response(&mut client, 405, false).await; }
    let root = match tokio::fs::canonicalize(document_root).await {
        Ok(root) => root,
        Err(cause) => {
            log::warn!("static document root `{document_root}` is unavailable: {cause}");
            return error_response(&mut client, 500, head_only).await;
        }
    };
    if !tokio::fs::metadata(&root).await.map(|metadata| metadata.is_dir()).unwrap_or(false) {
        log::warn!("static document root `{document_root}` is not a directory");
        return error_response(&mut client, 500, head_only).await;
    }
    let (request_path, query) = match head.target.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (head.target.as_str(), None),
    };
    let relative = request_path.strip_prefix(prefix.trim_end_matches('/')).unwrap_or(request_path);
    let decoded = match decode_path(relative.trim_start_matches('/')) {
        Ok(decoded) => decoded,
        Err(_) => return error_response(&mut client, 400, head_only).await,
    };
    let candidate = root.join(decoded);
    let mut physical = match canonical_beneath(&root, &candidate).await {
        Ok(Some(path)) => path,
        Ok(None) => return error_response(&mut client, 404, head_only).await,
        Err(_) => return error_response(&mut client, 403, head_only).await,
    };
    let mut metadata = match tokio::fs::metadata(&physical).await {
        Ok(metadata) => metadata,
        Err(cause) if cause.kind() == std::io::ErrorKind::PermissionDenied => {
            return error_response(&mut client, 403, head_only).await
        }
        Err(_) => return error_response(&mut client, 404, head_only).await,
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
            if !directory_listing { return error_response(&mut client, 403, head_only).await; }
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
    if !metadata.is_file() { return error_response(&mut client, 404, head_only).await; }
    serve_file(&mut client, &head, &physical, &metadata, head_only).await
}

/// Response body encoding, negotiated from `Accept-Encoding` for compressible
/// content types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Identity,
    Gzip,
    Zstd,
}

impl Encoding {
    fn token(self) -> &'static str {
        match self {
            Encoding::Identity => "identity",
            Encoding::Gzip => "gzip",
            Encoding::Zstd => "zstd",
        }
    }

    /// Suffix folded into the entity tag so caches key each representation
    /// separately.
    fn etag_suffix(self) -> &'static str {
        match self {
            Encoding::Identity => "",
            Encoding::Gzip => "-gz",
            Encoding::Zstd => "-zst",
        }
    }
}

/// True when the client's `Accept-Encoding` lists the token without `q=0`.
fn accepts_encoding(header: &str, token: &str) -> bool {
    header.split(',').any(|entry| {
        let mut parts = entry.trim().split(';');
        let name = parts.next().unwrap_or("").trim();
        if !name.eq_ignore_ascii_case(token) {
            return false;
        }
        !parts.any(|param| param.trim().replace(' ', "").eq_ignore_ascii_case("q=0"))
    })
}

fn compressible(content_type: &str) -> bool {
    content_type.starts_with("text/")
        || matches!(
            content_type.split(';').next().unwrap_or(""),
            "application/json"
                | "application/xml"
                | "application/yaml"
                | "application/wasm"
                | "application/rss+xml"
                | "application/atom+xml"
                | "application/xhtml+xml"
                | "application/manifest+json"
                | "image/svg+xml"
        )
}

fn negotiate_encoding(head: &HttpHead, content_type: &str, length: u64) -> Encoding {
    if length < MIN_COMPRESS_BYTES || !compressible(content_type) {
        return Encoding::Identity;
    }
    let Some(accept) = head.header_value("accept-encoding") else { return Encoding::Identity };
    if accepts_encoding(&accept, "zstd") {
        Encoding::Zstd
    } else if accepts_encoding(&accept, "gzip") {
        Encoding::Gzip
    } else {
        Encoding::Identity
    }
}

/// Incremental compressor draining into an in-memory buffer between chunks so
/// large files stream without being held in memory whole.
enum BodyEncoder {
    Gzip(flate2::write::GzEncoder<Vec<u8>>),
    Zstd(zstd::stream::write::Encoder<'static, Vec<u8>>),
}

impl BodyEncoder {
    fn new(encoding: Encoding) -> Option<Self> {
        match encoding {
            Encoding::Identity => None,
            Encoding::Gzip => Some(Self::Gzip(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default()))),
            Encoding::Zstd => zstd::stream::write::Encoder::new(Vec::new(), 0).ok().map(Self::Zstd),
        }
    }

    fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
        match self {
            Self::Gzip(encoder) => encoder.write_all(data),
            Self::Zstd(encoder) => encoder.write_all(data),
        }
    }

    fn take_output(&mut self) -> Vec<u8> {
        match self {
            Self::Gzip(encoder) => std::mem::take(encoder.get_mut()),
            Self::Zstd(encoder) => std::mem::take(encoder.get_mut()),
        }
    }

    fn finish(self) -> std::io::Result<Vec<u8>> {
        match self {
            Self::Gzip(encoder) => encoder.finish(),
            Self::Zstd(encoder) => encoder.finish(),
        }
    }
}

/// Serves one regular file: negotiated compression, `ETag`/`Last-Modified`
/// revalidation, and single `Range: bytes=` requests guarded by `If-Range`.
/// Ranges always apply to the identity representation, so a range request
/// disables compression.
async fn serve_file<S: AsyncWrite + Unpin>(stream: &mut S, head: &HttpHead, physical: &Path, metadata: &std::fs::Metadata, head_only: bool) -> Result<()> {
    let length = metadata.len();
    let modified = metadata.modified().ok();
    let last_modified = modified.map(http_date);
    let content_type = content_type(physical);

    // A stale If-Range validator means the stored partial body no longer
    // matches this file; ignore the Range and send the full entity instead.
    let identity_etag = entity_tag(length, modified, Encoding::Identity);
    let range_applicable = match head.header_value("if-range") {
        Some(validator) => validator == identity_etag || last_modified.as_deref() == Some(validator.as_str()),
        None => true,
    };
    let range = if head_only || !range_applicable { None } else { head.header_value("range").and_then(|value| parse_range(&value, length)) };

    let encoding = if range.is_some() { Encoding::Identity } else { negotiate_encoding(head, content_type, length) };
    let etag = entity_tag(length, modified, encoding);

    let revalidated = match head.header_value("if-none-match") {
        Some(value) => value.split(',').any(|tag| {
            let tag = tag.trim();
            tag == "*" || tag == etag || tag == identity_etag
        }),
        None => matches!((head.header_value("if-modified-since"), &last_modified), (Some(since), Some(current)) if since == *current),
    };
    if revalidated {
        let header = format!(
            "HTTP/1.1 304 Not Modified\r\nETag: {etag}\r\n{}Vary: Accept-Encoding\r\nConnection: close\r\n\r\n",
            last_modified.as_deref().map(|value| format!("Last-Modified: {value}\r\n")).unwrap_or_default()
        );
        stream.write_all(header.as_bytes()).await?;
        stream.shutdown().await?;
        return Ok(());
    }

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
            return error_response(stream, 403, head_only).await
        }
        Err(_) => return error_response(stream, 404, head_only).await,
    };

    let validators = format!(
        "ETag: {etag}\r\n{}Accept-Ranges: bytes\r\nCache-Control: public, no-cache\r\nVary: Accept-Encoding\r\nX-Content-Type-Options: nosniff\r\n",
        last_modified.as_deref().map(|value| format!("Last-Modified: {value}\r\n")).unwrap_or_default()
    );

    if encoding != Encoding::Identity {
        // Compressed length is unknown up front; the body is delimited by the
        // connection close the proxy already applies to every response.
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Encoding: {}\r\n{validators}Connection: close\r\n\r\n",
            encoding.token()
        );
        stream.write_all(header.as_bytes()).await?;
        if !head_only {
            stream_compressed(stream, &mut file, length, encoding).await?;
        }
        stream.shutdown().await?;
        return Ok(());
    }

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
        "{status_line}\r\nContent-Type: {content_type}\r\nContent-Length: {body_length}\r\n{content_range}{validators}Connection: close\r\n\r\n"
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
    let mut chunk = vec![0u8; 4096];
    while remaining > 0 {
        let want = remaining.min(chunk.len() as u64) as usize;
        let count = tokio::io::AsyncReadExt::read(file, &mut chunk[..want]).await?;
        if count == 0 { bail!("static file truncated while streaming"); }
        stream.write_all(&chunk[..count]).await?;
        remaining -= count as u64;
    }
    Ok(())
}

/// Streams the file through the negotiated compressor, flushing the encoder's
/// output buffer to the client between chunks.
async fn stream_compressed<S: AsyncWrite + Unpin>(stream: &mut S, file: &mut tokio::fs::File, length: u64, encoding: Encoding) -> Result<()> {
    let mut encoder = BodyEncoder::new(encoding).context("failed to create compressor")?;
    let mut remaining = length;
    let mut chunk = vec![0u8; 4096];
    while remaining > 0 {
        let want = remaining.min(chunk.len() as u64) as usize;
        let count = tokio::io::AsyncReadExt::read(file, &mut chunk[..want]).await?;
        if count == 0 { bail!("static file truncated while streaming"); }
        encoder.write(&chunk[..count])?;
        let output = encoder.take_output();
        if !output.is_empty() {
            stream.write_all(&output).await?;
        }
        remaining -= count as u64;
    }
    let tail = encoder.finish()?;
    if !tail.is_empty() {
        stream.write_all(&tail).await?;
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

fn entity_tag(length: u64, modified: Option<SystemTime>, encoding: Encoding) -> String {
    let stamp = modified
        .and_then(|value| value.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|duration| (duration.as_secs(), duration.subsec_nanos()))
        .unwrap_or((0, 0));
    format!("\"{length:x}-{:x}.{:x}{}\"", stamp.0, stamp.1, encoding.etag_suffix())
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
    /// The target a symlink points at, for display only; serving still
    /// enforces the canonical-path root check.
    link_target: Option<String>,
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
    let mut names: Vec<(String, bool, bool)> = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let Ok(entry) = entry else { continue };
        let (is_dir, is_symlink) = match entry.file_type() {
            Ok(kind) if kind.is_symlink() => (entry.path().metadata().map(|m| m.is_dir()).unwrap_or(false), true),
            Ok(kind) => (kind.is_dir(), false),
            Err(_) => (false, false),
        };
        names.push((entry.file_name().to_string_lossy().into_owned(), is_dir, is_symlink));
    }
    let directories = names.iter().filter(|(_, is_dir, _)| *is_dir).count();
    let files = names.len() - directories;
    names.sort_by(|a, b| a.0.cmp(&b.0));
    let start = match after {
        Some(cursor) => names.partition_point(|(name, _, _)| name.as_str() <= cursor),
        None => 0,
    };
    let page_end = (start + page_size).min(names.len());
    let next_after = (page_end < names.len()).then(|| names[page_end - 1].0.clone());
    let entries = names[start..page_end]
        .iter()
        .map(|(name, is_dir, is_symlink)| {
            let metadata = std::fs::metadata(path.join(name)).ok();
            let link_target = is_symlink
                .then(|| std::fs::read_link(path.join(name)).ok())
                .flatten()
                .map(|target| target.to_string_lossy().into_owned());
            ListedEntry {
                name: name.clone(),
                is_dir: *is_dir,
                size: metadata.as_ref().map(|m| m.len()).unwrap_or(0),
                modified: metadata.and_then(|m| m.modified().ok()),
                link_target,
            }
        })
        .collect();
    Ok(Listing { directories, files, start_ordinal: start + 1, entries, next_after })
}

/// Icon category for a file name, keyed to the bundled SVG sprite.
fn icon_for(name: &str, is_dir: bool) -> &'static str {
    if is_dir { return "folder"; }
    match name.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" | "svg" | "ico" | "bmp" | "tif" | "tiff" => "image",
        "mp4" | "m4v" | "webm" | "mkv" | "avi" | "mov" => "video",
        "mp3" | "m4a" | "aac" | "ogg" | "oga" | "opus" | "flac" | "wav" => "audio",
        "zip" | "gz" | "zst" | "tar" | "7z" | "rar" | "bz2" | "xz" | "deb" | "rpm" | "apk" | "iso" => "archive",
        "rs" | "c" | "h" | "cpp" | "hpp" | "go" | "java" | "ts" | "js" | "mjs" | "py" | "sh" | "css" | "json" | "yaml" | "yml" | "toml" | "xml" | "html" | "htm" => "code",
        "txt" | "md" | "log" | "csv" | "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" => "doc",
        "woff" | "woff2" | "ttf" | "otf" | "eot" => "font",
        _ => "file",
    }
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
        rows.push_str("<tr class=d><td><a href=\"../\"><svg class=ic><use href=\"#i-up\"/></svg>..</a></td><td class=s>—</td><td class=m>—</td></tr>");
    }
    for entry in &listing.entries {
        let class = if entry.is_dir { "d" } else { "f" };
        let suffix = if entry.is_dir { "/" } else { "" };
        let size = if entry.is_dir { "—".to_string() } else { human_size(entry.size) };
        let link_note = entry
            .link_target
            .as_deref()
            .map(|target| format!("<span class=lt><svg viewBox=\"0 0 24 24\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"2\"><path d=\"M10 13a5 5 0 0 0 7.5.5l3-3a5 5 0 0 0-7-7l-1.7 1.7M14 11a5 5 0 0 0-7.5-.5l-3 3a5 5 0 0 0 7 7l1.7-1.7\"/></svg>{}</span>", html_escape(target)))
            .unwrap_or_default();
        rows.push_str(&format!(
            "<tr class={class} data-n=\"{filter}\"><td><a href=\"{href}{suffix}\"><svg class=ic><use href=\"#i-{icon}\"/></svg>{name}{suffix}</a>{link_note}</td><td class=s>{size}</td><td class=m>{date}</td></tr>",
            filter = html_escape(&entry.name.to_lowercase()),
            icon = icon_for(&entry.name, entry.is_dir),
            href = url_escape(&entry.name),
            name = html_escape(&entry.name),
            date = listing_date(entry.modified),
        ));
    }
    let total = listing.directories + listing.files;
    let shown = listing.entries.len();
    let range_note = if total > shown {
        format!(
            "Showing {}&#8202;–&#8202;{} of {}",
            listing.start_ordinal,
            listing.start_ordinal + shown.saturating_sub(1),
            total
        )
    } else {
        String::new()
    };
    let next = listing
        .next_after
        .as_deref()
        .map(|cursor| format!("<a class=\"btn next\" href=\"?after={}\">Next page &rarr;</a>", url_escape(cursor)))
        .unwrap_or_default();
    format!(
        r##"<!doctype html><html lang=en><meta charset=utf-8><meta name=viewport content="width=device-width,initial-scale=1"><title>Index of {title}</title><style>
:root{{--bg:#f4f6fa;--panel:#fff;--text:#1c2433;--muted:#6b7686;--line:#e5e9f1;--accent:#2563c9;--accent-soft:#e8f0fd;--hover:#f4f7fc;--shadow:0 1px 3px rgba(16,24,40,.08),0 1px 2px rgba(16,24,40,.04)}}
@media(prefers-color-scheme:dark){{:root{{--bg:#0f131a;--panel:#181e29;--text:#e6eaf2;--muted:#8b95a7;--line:#293142;--accent:#6ba3f5;--accent-soft:#1d2b45;--hover:#1f2734;--shadow:0 1px 3px rgba(0,0,0,.4)}}}}
*{{box-sizing:border-box}}body{{margin:0;background:var(--bg);color:var(--text);font:14.5px/1.55 system-ui,-apple-system,"Segoe UI",sans-serif}}
header.top{{background:var(--panel);border-bottom:1px solid var(--line);padding:.8rem 1.25rem;display:flex;align-items:center;gap:.7rem}}
header.top svg{{width:26px;height:26px;color:var(--accent);flex:none}}
nav.crumbs{{font-size:1.02rem;font-weight:600;word-break:break-all;min-width:0}}nav.crumbs a{{color:var(--accent);text-decoration:none}}nav.crumbs a:hover{{text-decoration:underline}}nav.crumbs i{{font-style:normal;color:var(--muted);margin:0 .12em}}nav.crumbs span{{color:var(--text)}}
main{{max-width:68rem;margin:1.5rem auto 4rem;padding:0 1.25rem}}
.card{{background:var(--panel);border:1px solid var(--line);border-radius:12px;box-shadow:var(--shadow);overflow:hidden}}
.toolbar{{display:flex;align-items:center;gap:.8rem;padding:.75rem 1rem;border-bottom:1px solid var(--line);flex-wrap:wrap}}
.toolbar input{{flex:1;min-width:10rem;background:var(--bg);color:var(--text);border:1px solid var(--line);border-radius:8px;padding:.45rem .75rem;font:inherit;outline:none}}
.toolbar input:focus{{border-color:var(--accent);box-shadow:0 0 0 3px var(--accent-soft)}}
.badge{{background:var(--accent-soft);color:var(--accent);font-size:.78rem;font-weight:600;border-radius:999px;padding:.2rem .65rem;white-space:nowrap}}
table{{width:100%;border-collapse:collapse}}
thead th{{position:sticky;top:0;background:var(--panel);text-align:left;font-size:.72rem;letter-spacing:.07em;text-transform:uppercase;color:var(--muted);font-weight:600;padding:.6rem 1rem;border-bottom:1px solid var(--line)}}
tbody td{{padding:0;border-bottom:1px solid var(--line)}}tbody tr:last-child td{{border-bottom:0}}tbody tr:hover{{background:var(--hover)}}
td a{{display:inline-block;padding:.5rem 0 .5rem 1rem;color:var(--text);text-decoration:none;word-break:break-all}}
td a:hover{{color:var(--accent)}}
tr.d td a{{font-weight:600}}svg.ic{{width:18px;height:18px;vertical-align:-3.5px;margin-right:.55em;color:var(--muted);flex:none}}tr.d svg.ic{{color:var(--accent)}}
span.lt{{color:var(--muted);font-size:.85em;margin-left:.5em;word-break:break-all}}span.lt svg{{width:12px;height:12px;vertical-align:-1.5px;margin-right:.25em}}
td.s,td.m{{padding:.5rem 1rem;color:var(--muted);white-space:nowrap;font-variant-numeric:tabular-nums}}td.s{{text-align:right}}
th.s{{text-align:right}}th.s,td.s{{width:7.5rem}}th.m,td.m{{width:10rem}}
footer{{display:flex;justify-content:space-between;align-items:center;color:var(--muted);font-size:.83rem;margin-top:1rem;padding:0 .25rem}}
a.btn{{color:var(--accent);text-decoration:none;font-weight:600;border:1px solid var(--line);background:var(--panel);border-radius:8px;padding:.4rem .9rem;box-shadow:var(--shadow)}}a.btn:hover{{border-color:var(--accent)}}
@media(max-width:640px){{th.m,td.m{{display:none}}}}
</style><body>
<svg style=display:none xmlns="http://www.w3.org/2000/svg"><defs>
<symbol id=i-up viewBox="0 0 24 24" fill=none stroke=currentColor stroke-width=1.8 stroke-linecap=round stroke-linejoin=round><path d="M3 8V6a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v10a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8z"/><path d="M12 16v-5m0 0-2.5 2.5M12 11l2.5 2.5"/></symbol>
<symbol id=i-folder viewBox="0 0 24 24" fill=none stroke=currentColor stroke-width=1.8 stroke-linecap=round stroke-linejoin=round><path d="M3 8V6a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v10a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8z"/></symbol>
<symbol id=i-file viewBox="0 0 24 24" fill=none stroke=currentColor stroke-width=1.8 stroke-linecap=round stroke-linejoin=round><path d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8l-5-5z"/><path d="M14 3v5h5"/></symbol>
<symbol id=i-doc viewBox="0 0 24 24" fill=none stroke=currentColor stroke-width=1.8 stroke-linecap=round stroke-linejoin=round><path d="M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8l-5-5z"/><path d="M14 3v5h5M9 13h6M9 17h6"/></symbol>
<symbol id=i-image viewBox="0 0 24 24" fill=none stroke=currentColor stroke-width=1.8 stroke-linecap=round stroke-linejoin=round><rect x=4 y=4 width=16 height=16 rx=2/><circle cx=9.5 cy=9.5 r=1.6/><path d="m5 18 5-5 3 3 2.5-2.5L20 18"/></symbol>
<symbol id=i-video viewBox="0 0 24 24" fill=none stroke=currentColor stroke-width=1.8 stroke-linecap=round stroke-linejoin=round><rect x=3.5 y=5 width=17 height=14 rx=2.5/><path d="m10.5 9.5 4.5 2.5-4.5 2.5v-5z"/></symbol>
<symbol id=i-audio viewBox="0 0 24 24" fill=none stroke=currentColor stroke-width=1.8 stroke-linecap=round stroke-linejoin=round><path d="M9 18V6l10-2v12"/><circle cx=6.6 cy=18 r=2.4/><circle cx=16.6 cy=16 r=2.4/></symbol>
<symbol id=i-archive viewBox="0 0 24 24" fill=none stroke=currentColor stroke-width=1.8 stroke-linecap=round stroke-linejoin=round><path d="M4 8V6a1.5 1.5 0 0 1 1.5-1.5h13A1.5 1.5 0 0 1 20 6v2M4 8h16v10a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V8z"/><path d="M10 12h4"/></symbol>
<symbol id=i-code viewBox="0 0 24 24" fill=none stroke=currentColor stroke-width=1.8 stroke-linecap=round stroke-linejoin=round><path d="m9 8-5 4 5 4M15 8l5 4-5 4"/></symbol>
<symbol id=i-font viewBox="0 0 24 24" fill=none stroke=currentColor stroke-width=1.8 stroke-linecap=round stroke-linejoin=round><path d="M6 19 12 5l6 14M8.4 14h7.2"/></symbol>
</defs></svg>
<header class=top><svg viewBox="0 0 48 48" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M24 4 6 11v11c0 10.5 7.7 18.9 18 22 10.3-3.1 18-11.5 18-22V11L24 4z"/><path d="M16 24h12m0 0-4-4m4 4-4 4"/></svg><nav class=crumbs>{crumbs}</nav></header>
<main><div class=card>
<div class=toolbar><input id=q type=search placeholder="Filter this page&#8230;" autocomplete=off><span class=badge>{directories} folders</span><span class=badge>{files} files</span></div>
<table><thead><tr><th>Name</th><th class=s>Size</th><th class=m>Modified</th></tr></thead><tbody id=rows>{rows}</tbody></table>
</div><footer><span>{range_note}</span>{next}</footer></main>
<script>const q=document.getElementById('q');q.addEventListener('input',()=>{{const v=q.value.toLowerCase();for(const r of document.querySelectorAll('#rows tr'))r.style.display=(r.dataset.n===undefined||r.dataset.n.includes(v))?'':'none'}});</script>
</body></html>"##,
        title = html_escape(request_path),
        crumbs = breadcrumbs(request_path),
        directories = listing.directories,
        files = listing.files,
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

/// Writes a styled built-in error page for the status.
pub(crate) async fn error_response<S: AsyncWrite + Unpin>(stream: &mut S, status: u16, head_only: bool) -> Result<()> {
    let reason = error_pages::reason(status);
    let body = error_pages::render(status, reason, error_pages::default_detail(status));
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nX-Content-Type-Options: nosniff\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    if !head_only { stream.write_all(body.as_bytes()).await?; }
    stream.shutdown().await?;
    Ok(())
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|v| v.to_str()).unwrap_or("").to_ascii_lowercase().as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "jsonl" | "ndjson" => "application/x-ndjson",
        "xml" => "application/xml",
        "rss" => "application/rss+xml",
        "atom" => "application/atom+xml",
        "xhtml" => "application/xhtml+xml",
        "yaml" | "yml" => "application/yaml",
        "toml" => "text/plain; charset=utf-8",
        "wasm" => "application/wasm",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "avi" => "video/x-msvideo",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "ogg" | "oga" => "audio/ogg",
        "opus" => "audio/opus",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "txt" | "md" | "text" | "log" => "text/plain; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "zst" => "application/zstd",
        "tar" => "application/x-tar",
        "7z" => "application/x-7z-compressed",
        "rar" => "application/vnd.rar",
        "bz2" => "application/x-bzip2",
        "xz" => "application/x-xz",
        "apk" => "application/vnd.android.package-archive",
        "iso" => "application/x-iso9660-image",
        "deb" => "application/vnd.debian.binary-package",
        "rpm" => "application/x-rpm",
        "sh" => "text/x-shellscript; charset=utf-8",
        "py" => "text/x-python; charset=utf-8",
        "rs" | "c" | "h" | "cpp" | "hpp" | "go" | "java" | "ts" => "text/plain; charset=utf-8",
        "eot" => "application/vnd.ms-fontobject",
        "manifest" | "webmanifest" => "application/manifest+json",
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

    #[tokio::test]
    async fn encoding_negotiation_honors_type_size_and_preference() {
        async fn head_with(accept: Option<&str>) -> HttpHead {
            let raw = match accept {
                Some(value) => format!("GET /a.txt HTTP/1.1\r\nHost: h\r\nAccept-Encoding: {value}\r\n\r\n"),
                None => "GET /a.txt HTTP/1.1\r\nHost: h\r\n\r\n".to_string(),
            };
            crate::http_header::read_http_head(
                &mut std::io::Cursor::new(raw.into_bytes()),
                std::time::Duration::from_secs(1),
                65536,
            )
            .await
            .unwrap()
        }
        let text = "text/plain; charset=utf-8";
        assert_eq!(negotiate_encoding(&head_with(Some("gzip, deflate")).await, text, 4096), Encoding::Gzip);
        assert_eq!(negotiate_encoding(&head_with(Some("zstd, gzip")).await, text, 4096), Encoding::Zstd);
        assert_eq!(negotiate_encoding(&head_with(Some("gzip;q=0")).await, text, 4096), Encoding::Identity);
        assert_eq!(negotiate_encoding(&head_with(Some("gzip")).await, text, 64), Encoding::Identity, "tiny bodies stay identity");
        assert_eq!(negotiate_encoding(&head_with(Some("gzip")).await, "image/png", 1 << 20), Encoding::Identity, "already-compressed types stay identity");
        assert_eq!(negotiate_encoding(&head_with(None).await, text, 4096), Encoding::Identity);
        assert_ne!(entity_tag(10, None, Encoding::Gzip), entity_tag(10, None, Encoding::Identity));
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

    #[cfg(unix)]
    #[test]
    fn listing_shows_symlink_targets() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("real.txt"), b"data").unwrap();
        std::os::unix::fs::symlink("real.txt", directory.path().join("alias")).unwrap();
        let listing = scan_directory(directory.path(), None, 100).unwrap();
        let alias = listing.entries.iter().find(|e| e.name == "alias").unwrap();
        assert_eq!(alias.link_target.as_deref(), Some("real.txt"));
        let page = directory_page(&listing, "/files/");
        assert!(page.contains("real.txt</span>"), "symlink target shown in the link note");
    }

    #[test]
    fn listing_html_escapes_names_and_links_next_page() {
        let listing = Listing {
            directories: 0,
            files: 2,
            start_ordinal: 1,
            entries: vec![ListedEntry { name: "<script>.txt".into(), is_dir: false, size: 10, modified: None, link_target: None }],
            next_after: Some("a&b.txt".into()),
        };
        let page = directory_page(&listing, "/downloads/");
        assert!(page.contains("&lt;script&gt;.txt"));
        assert!(!page.contains("<script>.txt"));
        assert!(page.contains("?after=a%26b.txt"));
    }

    async fn request(root: &str, request_line: &str, extra_headers: &str) -> Vec<u8> {
        use tokio::io::AsyncReadExt;
        let (mut client, server) = tokio::io::duplex(1 << 22);
        let raw = format!("{request_line}\r\nHost: files.example\r\n{extra_headers}\r\n");
        let head = {
            let mut reader = std::io::Cursor::new(raw.into_bytes());
            crate::http_header::read_http_head(&mut reader, std::time::Duration::from_secs(1), 65536).await.unwrap()
        };
        let root = root.to_string();
        let task = tokio::spawn(async move { serve(server, head, "/", &root, None, true).await });
        let mut output = Vec::new();
        client.read_to_end(&mut output).await.unwrap();
        task.await.unwrap().unwrap();
        output
    }

    fn text_of(raw: &[u8]) -> String { String::from_utf8_lossy(raw).into_owned() }

    #[tokio::test]
    async fn conditional_and_range_requests_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("data.bin"), b"0123456789").unwrap();
        let root = directory.path().to_str().unwrap();

        let full = text_of(&request(root, "GET /data.bin HTTP/1.1", "").await);
        assert!(full.starts_with("HTTP/1.1 200 OK"));
        assert!(full.contains("Accept-Ranges: bytes"));
        assert!(full.contains("Vary: Accept-Encoding"));
        assert!(full.ends_with("0123456789"));
        let etag = full.lines().find(|l| l.starts_with("ETag:")).unwrap().trim_start_matches("ETag:").trim().to_string();

        let revalidated = text_of(&request(root, "GET /data.bin HTTP/1.1", &format!("If-None-Match: {etag}\r\n")).await);
        assert!(revalidated.starts_with("HTTP/1.1 304 Not Modified"));

        let partial = text_of(&request(root, "GET /data.bin HTTP/1.1", "Range: bytes=2-5\r\n").await);
        assert!(partial.starts_with("HTTP/1.1 206 Partial Content"));
        assert!(partial.contains("Content-Range: bytes 2-5/10"));
        assert!(partial.ends_with("2345"));

        let stale = text_of(&request(root, "GET /data.bin HTTP/1.1", "Range: bytes=2-5\r\nIf-Range: \"different\"\r\n").await);
        assert!(stale.starts_with("HTTP/1.1 200 OK"));
        assert!(stale.ends_with("0123456789"));

        let beyond = text_of(&request(root, "GET /data.bin HTTP/1.1", "Range: bytes=99-\r\n").await);
        assert!(beyond.starts_with("HTTP/1.1 416 Range Not Satisfiable"));

        let options = text_of(&request(root, "OPTIONS /data.bin HTTP/1.1", "").await);
        assert!(options.starts_with("HTTP/1.1 204 No Content"));
        assert!(options.contains("Allow: GET, HEAD, OPTIONS"));
    }

    #[tokio::test]
    async fn compression_round_trips_and_keeps_ranges_identity() {
        use std::io::Read as _;
        let directory = tempfile::tempdir().unwrap();
        let content = "tlsproxy ".repeat(500);
        std::fs::write(directory.path().join("page.txt"), &content).unwrap();
        let root = directory.path().to_str().unwrap();

        let gz = request(root, "GET /page.txt HTTP/1.1", "Accept-Encoding: gzip, deflate\r\n").await;
        let text = text_of(&gz);
        assert!(text.starts_with("HTTP/1.1 200 OK"));
        assert!(text.contains("Content-Encoding: gzip"));
        assert!(!text.contains("Content-Length:"), "compressed bodies are close-delimited");
        let body_start = gz.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let mut decoded = String::new();
        flate2::read::GzDecoder::new(&gz[body_start..]).read_to_string(&mut decoded).unwrap();
        assert_eq!(decoded, content);
        let gz_etag = text.lines().find(|l| l.starts_with("ETag:")).unwrap().trim_start_matches("ETag:").trim().to_string();
        assert!(gz_etag.contains("-gz"));

        // The encoded ETag revalidates for a client sending the same encoding.
        let revalidated = text_of(&request(root, "GET /page.txt HTTP/1.1", &format!("Accept-Encoding: gzip\r\nIf-None-Match: {gz_etag}\r\n")).await);
        assert!(revalidated.starts_with("HTTP/1.1 304 Not Modified"));

        let zst = text_of(&request(root, "GET /page.txt HTTP/1.1", "Accept-Encoding: zstd, gzip\r\n").await);
        assert!(zst.contains("Content-Encoding: zstd"), "zstd preferred over gzip");

        // Range requests always serve the identity representation.
        let ranged = text_of(&request(root, "GET /page.txt HTTP/1.1", "Accept-Encoding: gzip\r\nRange: bytes=0-8\r\n").await);
        assert!(ranged.starts_with("HTTP/1.1 206 Partial Content"));
        assert!(!ranged.contains("Content-Encoding:"));
        assert!(ranged.ends_with("tlsproxy "));
    }

    #[tokio::test]
    async fn missing_files_get_the_styled_error_page() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_str().unwrap();
        let missing = text_of(&request(root, "GET /nope.txt HTTP/1.1", "").await);
        assert!(missing.starts_with("HTTP/1.1 404 Not Found"));
        assert!(missing.contains("Content-Type: text/html"));
        assert!(missing.contains("<h1>404</h1>"));
        assert!(missing.contains("tlsproxy"));
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

        // An escaping symlink is visible in the listing but not servable.
        let listing = scan_directory(&canonical_root, None, 100).unwrap();
        assert!(listing.entries.iter().any(|e| e.name == "outside-link" && e.link_target.is_some()));
    }
}
