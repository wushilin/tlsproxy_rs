//! Built-in styled error pages, served by the static file server and the
//! reverse proxy's locally-generated error responses. Self-contained HTML —
//! inline CSS and an inline SVG mark, light and dark aware, no external
//! resources.

/// Renders the standard error page for a status code.
pub fn render(status: u16, reason: &str, detail: &str) -> String {
    format!(
        r##"<!doctype html><html lang=en><meta charset=utf-8><meta name=viewport content="width=device-width,initial-scale=1"><title>{status} {reason}</title><style>
:root{{--bg:#fafbfd;--panel:#fff;--text:#1c2433;--muted:#6b7686;--line:#e5e9f1;--accent:#2563c9}}
@media(prefers-color-scheme:dark){{:root{{--bg:#12161e;--panel:#1a202b;--text:#e6eaf2;--muted:#8b95a7;--line:#2a3240;--accent:#6ba3f5}}}}
*{{box-sizing:border-box}}body{{margin:0;min-height:100vh;display:flex;align-items:center;justify-content:center;background:var(--bg);color:var(--text);font:16px/1.6 system-ui,-apple-system,"Segoe UI",sans-serif}}
main{{text-align:center;padding:3rem 1.5rem;max-width:36rem}}
.mark{{width:64px;height:64px;margin:0 auto 1.2rem;display:block;color:var(--accent)}}
h1{{font-size:4rem;margin:0;font-weight:700;letter-spacing:-.02em}}
h2{{font-size:1.25rem;margin:.2rem 0 1rem;font-weight:600}}
p{{color:var(--muted);margin:0 0 1.6rem}}
footer{{color:var(--muted);font-size:.8rem;border-top:1px solid var(--line);padding-top:1rem;letter-spacing:.04em}}
</style><body><main>
<svg class=mark viewBox="0 0 48 48" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M24 4 6 11v11c0 10.5 7.7 18.9 18 22 10.3-3.1 18-11.5 18-22V11L24 4z"/><path d="M16 24h12m0 0-4-4m4 4-4 4"/><path d="M32 30H20m0 0 4 4m-4-4 4-4" opacity=".55"/></svg>
<h1>{status}</h1><h2>{reason}</h2><p>{detail}</p>
<footer>tlsproxy</footer>
</main></body></html>"##,
        detail = html_escape(detail),
    )
}

/// Standard detail line for a status when the caller has nothing specific.
pub fn default_detail(status: u16) -> &'static str {
    match status {
        400 => "The request could not be understood.",
        401 => "Authentication is required to access this resource.",
        403 => "You don't have permission to access this resource.",
        404 => "The requested resource was not found on this server.",
        405 => "This method is not allowed for the requested resource.",
        416 => "The requested range cannot be satisfied.",
        500 => "The server encountered an unexpected condition.",
        502 => "The upstream server could not be reached.",
        _ => "The request could not be completed.",
    }
}

pub fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        416 => "Range Not Satisfiable",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        _ => "Error",
    }
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_page_is_self_contained_and_escapes_detail() {
        let page = render(404, "Not Found", "no <script> here");
        assert!(page.contains("<h1>404</h1>"));
        assert!(page.contains("no &lt;script&gt; here"));
        assert!(!page.contains("no <script>"));
        assert!(!page.contains("http://"), "no external resources");
        assert!(page.contains("<svg"));
    }
}
