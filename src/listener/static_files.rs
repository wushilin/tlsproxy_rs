use std::path::{Component, Path, PathBuf};
use anyhow::{bail, Context, Result};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use crate::http_header::HttpHead;

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
    let request_path = head.target.split('?').next().unwrap_or("/");
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
            let body = directory_page(&physical, request_path).await?;
            return response(&mut client, 200, "text/html; charset=utf-8", body.as_bytes(), head_only).await;
        }
    }
    if !metadata.is_file() { return response(&mut client, 404, "text/plain; charset=utf-8", b"Not Found\n", head_only).await; }
    stream_file(&mut client, &physical, metadata.len(), head_only).await
}

/// Streams the file in fixed-size chunks so concurrent large downloads do not
/// buffer whole files in memory.
async fn stream_file<S: AsyncWrite + Unpin>(stream: &mut S, physical: &Path, length: u64, head_only: bool) -> Result<()> {
    let mut file = match tokio::fs::File::open(physical).await {
        Ok(file) => file,
        Err(cause) if cause.kind() == std::io::ErrorKind::PermissionDenied => {
            return response(stream, 403, "text/plain; charset=utf-8", b"Forbidden\n", head_only).await
        }
        Err(_) => return response(stream, 404, "text/plain; charset=utf-8", b"Not Found\n", head_only).await,
    };
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {length}\r\nConnection: close\r\nX-Content-Type-Options: nosniff\r\n\r\n",
        content_type(physical)
    );
    stream.write_all(header.as_bytes()).await?;
    if !head_only {
        let mut remaining = length;
        let mut chunk = vec![0u8; 64 * 1024];
        while remaining > 0 {
            let want = remaining.min(chunk.len() as u64) as usize;
            let count = tokio::io::AsyncReadExt::read(&mut file, &mut chunk[..want]).await?;
            if count == 0 { bail!("static file truncated while streaming"); }
            stream.write_all(&chunk[..count]).await?;
            remaining -= count as u64;
        }
    }
    stream.shutdown().await?;
    Ok(())
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

fn decode_path(value: &str) -> Result<PathBuf> {
    let input = value.as_bytes(); let mut bytes = Vec::with_capacity(input.len()); let mut offset = 0;
    while offset < input.len() { if input[offset] == b'%' { if offset + 2 >= input.len() { bail!("invalid percent encoding in request path"); } let hex = std::str::from_utf8(&input[offset+1..offset+3])?; bytes.push(u8::from_str_radix(hex, 16).context("invalid percent encoding in request path")?); offset += 3; } else { bytes.push(input[offset]); offset += 1; } }
    let decoded = String::from_utf8(bytes).context("request path is not UTF-8")?; let path = Path::new(&decoded);
    if path.components().any(|part| matches!(part, Component::ParentDir | Component::RootDir | Component::Prefix(_))) { bail!("request path attempts to escape the document root"); }
    Ok(path.to_path_buf())
}

async fn directory_page(path: &Path, request_path: &str) -> Result<String> {
    let mut entries = Vec::new(); let mut directory = tokio::fs::read_dir(path).await?;
    while let Some(entry) = directory.next_entry().await? { entries.push((entry.file_name().to_string_lossy().into_owned(), entry.file_type().await?.is_dir())); }
    entries.sort_by(|a,b| b.1.cmp(&a.1).then_with(|| a.0.to_ascii_lowercase().cmp(&b.0.to_ascii_lowercase())));
    let mut rows = String::new(); if request_path != "/" { rows.push_str("<li class=dir><a href=\"../\">../</a></li>"); }
    for (name,is_dir) in entries { let suffix=if is_dir{"/"}else{""}; rows.push_str(&format!("<li class=\"{}\"><a href=\"{}{}\">{}{}</a></li>",if is_dir{"dir"}else{"file"},url_escape(&name),suffix,html_escape(&name),suffix)); }
    Ok(format!("<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width\"><title>Index of {0}</title><style>body{{font:16px system-ui;margin:3rem auto;max-width:60rem;padding:0 1.5rem;color:#172033}}h1{{font-size:1.5rem}}ul{{list-style:none;padding:0;border-top:1px solid #d9dfeb}}li{{border-bottom:1px solid #d9dfeb}}a{{display:block;padding:.7rem;text-decoration:none;color:#165dcc}}a:hover{{background:#f2f6fc}}.dir a{{font-weight:650}}</style><h1>Index of {0}</h1><ul>{1}</ul>",html_escape(request_path),rows))
}

async fn response<S: AsyncWrite + Unpin>(stream:&mut S,status:u16,content_type:&str,body:&[u8],head_only:bool)->Result<()> { let reason=match status{200=>"OK",400=>"Bad Request",403=>"Forbidden",404=>"Not Found",405=>"Method Not Allowed",500=>"Internal Server Error",_=>"Error"}; let header=format!("HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nX-Content-Type-Options: nosniff\r\n\r\n",body.len()); stream.write_all(header.as_bytes()).await?; if !head_only{stream.write_all(body).await?;} stream.shutdown().await?; Ok(()) }
fn content_type(path:&Path)->&'static str { match path.extension().and_then(|v|v.to_str()).unwrap_or("").to_ascii_lowercase().as_str(){"html"|"htm"=>"text/html; charset=utf-8","css"=>"text/css; charset=utf-8","js"=>"text/javascript; charset=utf-8","json"=>"application/json","svg"=>"image/svg+xml","png"=>"image/png","jpg"|"jpeg"=>"image/jpeg","gif"=>"image/gif","webp"=>"image/webp","txt"|"md"=>"text/plain; charset=utf-8","pdf"=>"application/pdf",_=>"application/octet-stream"} }
fn html_escape(value:&str)->String{value.replace('&',"&amp;").replace('<',"&lt;").replace('>',"&gt;").replace('"',"&quot;")}
fn url_escape(value:&str)->String{value.bytes().map(|b|if b.is_ascii_alphanumeric()||b"-._~".contains(&b){(b as char).to_string()}else{format!("%{b:02X}")}).collect()}

#[cfg(test)] mod tests {
    use super::*;
    #[test] fn traversal_is_rejected_after_decoding(){assert!(decode_path("../secret").is_err());assert!(decode_path("%2e%2e/secret").is_err());assert!(decode_path("assets/app.js").is_ok());}

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
