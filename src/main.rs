use async_trait::async_trait;
use bytes::Bytes;
use http::Uri;
use log::{debug, error, info, warn};
use memchr::memmem;
use once_cell::sync::Lazy;
use pingora::http::{RequestHeader, ResponseHeader};
use pingora::prelude::*;
use pingora::proxy::http_proxy_service;
use pingora::proxy::{ProxyHttp, Session};
use pingora::server::configuration::Opt;
use pingora::server::Server;
use regex::Regex;
use std::collections::HashMap;
use std::time::Duration;

// ━━━━━━━━━━━━ 常量 & 正则 ━━━━━━━━━━━━━━━━━━━━━━━

static M3U8_NEEDLE: &[u8] = b"http://116.199.";
static PROXY_PATH_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^/proxy/([^:]+)(?::(\d+))?/(.*)$").expect("Invalid proxy regex")
});

const REFERER_VALUE: &str = "https://missav.ws/dm242/cn";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
const MAX_REDIRECT_DEPTH: u8 = 5;

// ━━━━━━━━━━━━ 配置 ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[derive(Clone)]
pub struct ProxyConfig {
    pub local_ip: String,
    pub bind_port: u16,
    pub replacement_pattern: String,
}

impl ProxyConfig {
    pub fn new(local_ip: String, bind_port: u16) -> Self {
        let replacement_pattern = format!("http://{}:{}/proxy/116.199.", local_ip, bind_port);
        Self { local_ip, bind_port, replacement_pattern }
    }
}

// ━━━━━━━━━━━━ 请求上下文 ━━━━━━━━━━━━━━━━━━━━━━━━━

#[derive(Debug, Clone, Copy, PartialEq)]
enum RouteMode {
    IptvDirect,
    ProxyPath,
    AntiLeechQuery,
}

pub struct ProxyContext {
    route_mode: RouteMode,
    target_url: Option<url::Url>,
    is_m3u8: bool,
    needs_referer: bool,
    needs_rewrite_iptv: bool,
    needs_rewrite_surrit: bool,
    base_url: Option<String>,
    needs_jpeg_fix: bool,
    cached_body: Option<Bytes>,
    cached_headers: Option<HashMap<String, String>>,
    cached_status: Option<u16>,
}

impl ProxyContext {
    fn new() -> Self {
        Self {
            route_mode: RouteMode::IptvDirect,
            target_url: None,
            is_m3u8: false,
            needs_referer: false,
            needs_rewrite_iptv: false,
            needs_rewrite_surrit: false,
            base_url: None,
            needs_jpeg_fix: false,
            cached_body: None,
            cached_headers: None,
            cached_status: None,
        }
    }
}

// ━━━━━━━━━━━━ 代理主体 ━━━━━━━━━━━━━━━━━━━━━━━━━━━

pub struct IptvProxy {
    config: ProxyConfig,
    finder: memmem::Finder<'static>,
    client: reqwest::Client,
}

impl IptvProxy {
    pub fn new(config: ProxyConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("Failed to build reqwest client");
        Self {
            config,
            finder: memmem::Finder::new(M3U8_NEEDLE),
            client,
        }
    }

    fn host_needs_referer(host: &str) -> bool {
        host.contains("surrit.com") || host.contains("fourhoi.com")
    }

    /// 内部重定向跟随，返回 (最终URL, 状态码, 响应头, 响应体)
    async fn follow_redirects(
        client: &reqwest::Client,
        url: String,
        depth: u8,
    ) -> Result<(String, u16, HashMap<String, String>, Bytes)> {
        if depth == 0 {
            return Err(Error::explain(
                ErrorType::InternalError,
                "Too many redirects",
            ));
        }

        let mut request = client.get(&url);
        if let Ok(parsed) = url::Url::parse(&url) {
            if Self::host_needs_referer(parsed.host_str().unwrap_or("")) {
                request = request
                    .header("Referer", REFERER_VALUE)
                    .header("User-Agent", USER_AGENT);
            }
        }

        let resp = request
            .send()
            .await
            .map_err(|e| Error::explain(ErrorType::InternalError, format!("Redirect request failed: {e}")))?;

        let status = resp.status().as_u16();
        if status == 301 || status == 302 || status == 307 || status == 308 {
            if let Some(location) = resp.headers().get("location") {
                let loc = location.to_str().map_err(|_| {
                    Error::explain(ErrorType::InternalError, "Invalid Location header")
                })?;
                let new_url = if loc.starts_with("http://") || loc.starts_with("https://") {
                    loc.to_string()
                } else {
                    let base = url::Url::parse(&url).map_err(|_| {
                        Error::explain(ErrorType::InternalError, "Invalid base URL")
                    })?;
                    base.join(loc)
                        .map_err(|_| {
                            Error::explain(ErrorType::InternalError, "Failed to resolve relative URL")
                        })?
                        .to_string()
                };
                return Box::pin(Self::follow_redirects(client, new_url, depth - 1)).await;
            }
        }

        let final_url = url;
        let headers: HashMap<String, String> = resp
            .headers()
            .iter()
            .filter_map(|(k, v)| {
                v.to_str().ok().map(|v| (k.as_str().to_string(), v.to_string()))
            })
            .collect();

        let body = resp
            .bytes()
            .await
            .map_err(|e| Error::explain(ErrorType::InternalError, format!("Read body failed: {e}")))?;

        Ok((final_url, status, headers, body))
    }
}

#[async_trait]
impl ProxyHttp for IptvProxy {
    type CTX = ProxyContext;

    fn new_ctx(&self) -> Self::CTX {
        ProxyContext::new()
    }

    async fn request_filter(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<bool> {
        let req = session.req_header();
        let path = req.uri.path();
        let query = req.uri.query().unwrap_or("");

        if path == "/health" || (path == "/" && !query.contains("url=")) {
            let resp = ResponseHeader::build(200, None)
                .map_err(|e| Error::explain(ErrorType::InternalError, format!("{e}")))?;
            session.write_response_header(Box::new(resp), false).await?;
            session.write_response_body(Some(Bytes::from("OK")), true).await?;
            return Ok(true);
        }

        if path == "/favicon.ico" {
            let resp = ResponseHeader::build(404, None)
                .map_err(|e| Error::explain(ErrorType::InternalError, format!("{e}")))?;
            session.write_response_header(Box::new(resp), true).await?;
            return Ok(true);
        }

        if path.starts_with("/iptv/") {
            ctx.route_mode = RouteMode::IptvDirect;
            let url_str = path.strip_prefix("/iptv/").unwrap();
            let full = if query.is_empty() {
                url_str.to_string()
            } else {
                format!("{}?{}", url_str, query)
            };
            let mut url = url::Url::parse(&full)
                .map_err(|e| Error::explain(ErrorType::HTTPStatus(400), format!("Invalid URL: {e}")))?;

            if url.query_pairs().any(|(k, v)| k == "real_ext" && v == "jpeg") {
                ctx.needs_jpeg_fix = true;
                let clean_params: Vec<_> = url
                    .query_pairs()
                    .filter(|(k, _)| k != "real_ext")
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect();
                url.query_pairs_mut().clear();
                for (k, v) in clean_params {
                    url.query_pairs_mut().append_pair(&k, &v);
                }
            }

            let host = url.host_str().unwrap_or("localhost").to_string();
            ctx.target_url = Some(url.clone());
            ctx.is_m3u8 = path.contains(".m3u8");
            ctx.needs_referer = Self::host_needs_referer(&host);
            ctx.needs_rewrite_iptv = ctx.is_m3u8 && host.contains("116.199");
            return Ok(false);
        }

        if path.starts_with("/proxy/") {
            ctx.route_mode = RouteMode::ProxyPath;
            let caps = PROXY_PATH_REGEX.captures(path)
                .ok_or_else(|| Error::explain(ErrorType::HTTPStatus(400), "Invalid proxy path"))?;
            let host = caps.get(1).unwrap().as_str().to_string();
            let port: u16 = caps.get(2).map(|m| m.as_str().parse().unwrap_or(80)).unwrap_or(80);
            let uri_path = caps.get(3).unwrap().as_str();
            let full_path = if query.is_empty() {
                format!("/{}", uri_path)
            } else {
                format!("/{}?{}", uri_path, query)
            };
            let url = url::Url::parse(&format!("http://{}:{}{}", host, port, full_path))
                .map_err(|_| Error::explain(ErrorType::InternalError, "Bad proxy URL"))?;

            ctx.target_url = Some(url);
            ctx.is_m3u8 = false;
            ctx.needs_referer = Self::host_needs_referer(&host);
            ctx.needs_rewrite_iptv = false;
            return Ok(false);
        }

        if let Some(url_param) = query.split('&').find(|p| p.starts_with("url=")) {
            let encoded = &url_param[4..];
            match urlencoding::decode(encoded) {
                Ok(decoded) => {
                    let decoded_str = decoded.to_string();
                    info!("Decoded anti-leech URL: {}", decoded_str);

                    if let Ok(url) = url::Url::parse(&decoded_str) {
                        let host = url.host_str().unwrap_or("surrit.com").to_string();
                        let path_bytes = url.path().as_bytes().to_vec();

                        ctx.route_mode = RouteMode::AntiLeechQuery;
                        ctx.target_url = Some(url);
                        ctx.is_m3u8 = decoded_str.ends_with(".m3u8");
                        ctx.needs_referer = true;
                        ctx.needs_rewrite_surrit = ctx.is_m3u8;

                        if let Some(last_slash) = decoded_str.rfind('/') {
                            ctx.base_url = Some(decoded_str[..last_slash + 1].to_string());
                        } else {
                            ctx.base_url = Some(decoded_str);
                        }

                        session.req_header_mut().set_raw_path(&path_bytes)?;
                        session.req_header_mut().insert_header("Host", &host)?;
                        return Ok(false);
                    } else {
                        warn!("Failed to parse decoded URL: {}", decoded_str);
                        return Err(Error::explain(
                            ErrorType::HTTPStatus(400),
                            "Invalid target URL in url parameter",
                        ));
                    }
                }
                Err(e) => {
                    warn!("Failed to decode url parameter '{}': {}", encoded, e);
                    return Err(Error::explain(
                        ErrorType::HTTPStatus(400),
                        "The 'url' parameter must be URL-encoded. Special characters like ':', '/', '?', '&' need to be percent-encoded (e.g., %3A, %2F, %3F, %26)",
                    ));
                }
            }
        }

        warn!("Invalid request: {}", path);
        Err(Error::explain(
            ErrorType::HTTPStatus(400),
            "Use /iptv/URL, /proxy/host:port/path, or ?url=encoded_target",
        ))
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let target = ctx.target_url.as_ref().ok_or_else(|| {
            Error::explain(ErrorType::InternalError, "No target URL")
        })?;

        let host = target.host_str().unwrap_or("localhost").to_string();
        let port = target.port().unwrap_or(if target.scheme() == "https" { 443 } else { 80 });
        let is_https = target.scheme() == "https";

        info!("Upstream: {}:{} (TLS: {})", host, port, is_https);
        Ok(Box::new(HttpPeer::new((host.clone(), port), is_https, host)))
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream_request: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        if let Some(url) = &ctx.target_url {
            let mut path_and_query = if let Some(q) = url.query() {
                format!("{}?{}", url.path(), q)
            } else {
                url.path().to_string()
            };

            if ctx.needs_jpeg_fix {
                path_and_query = path_and_query.replace(".ts", ".jpeg");
            }

            let uri = Uri::try_from(path_and_query)
                .map_err(|e| Error::explain(ErrorType::InternalError, format!("Invalid URI: {e}")))?;
            upstream_request.set_uri(uri);
        }

        if let Some(url) = &ctx.target_url {
            let host = url.host_str().unwrap_or("localhost");
            let host_value = if let Some(port) = url.port() {
                format!("{}:{}", host, port)
            } else {
                host.to_string()
            };
            upstream_request.insert_header("Host", &host_value)?;
        }

        if ctx.needs_referer {
            upstream_request.insert_header("Referer", REFERER_VALUE)?;
            upstream_request.insert_header("User-Agent", USER_AGENT)?;
            upstream_request.insert_header("Accept", "*/*")?;
        }

        if ctx.needs_rewrite_iptv || ctx.needs_rewrite_surrit {
            upstream_request.remove_header("Accept-Encoding");
        }

        Ok(())
    }

    async fn response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        let status = upstream_response.status;
        if status == 301 || status == 302 || status == 307 || status == 308 {
            let maybe_loc = upstream_response
                .headers
                .get("location")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());

            if let Some(loc_str) = maybe_loc {
                info!("Upstream returned redirect ({}), following internally...", status);

                let loc_for_fallback = loc_str.clone();
                match Self::follow_redirects(&self.client, loc_str, MAX_REDIRECT_DEPTH).await {
                    Ok((final_url, final_status, headers, body)) => {
                        upstream_response.set_status(final_status);
                        upstream_response.headers.clear();
                        for (k, v) in &headers {
                            if k.to_lowercase() != "content-length" {
                                if let Err(e) = upstream_response.insert_header(k.clone(), v) {
                                    warn!("Failed to set header {}: {}", k, e);
                                }
                            }
                        }
                        ctx.cached_body = Some(body);
                        ctx.cached_status = Some(final_status);
                        ctx.cached_headers = Some(headers);

                        // 根据最终URL更新上下文，以便后续重写
                        if let Ok(new_url) = url::Url::parse(&final_url) {
                            ctx.target_url = Some(new_url.clone());
                            let is_m3u8 = final_url.ends_with(".m3u8");
                            match ctx.route_mode {
                                RouteMode::AntiLeechQuery => {
                                    ctx.is_m3u8 = is_m3u8;
                                    ctx.needs_rewrite_surrit = is_m3u8;
                                    if is_m3u8 {
                                        if let Some(last_slash) = final_url.rfind('/') {
                                            ctx.base_url = Some(final_url[..last_slash + 1].to_string());
                                        } else {
                                            // 修复：克隆 final_url 以避免所有权转移后仍被使用
                                            ctx.base_url = Some(final_url.clone());
                                        }
                                    }
                                }
                                RouteMode::IptvDirect => {
                                    ctx.is_m3u8 = is_m3u8;
                                    let host = new_url.host_str().unwrap_or("");
                                    ctx.needs_rewrite_iptv = is_m3u8 && host.contains("116.199");
                                }
                                _ => {}
                            }
                        }

                        info!("Internal redirect succeeded, final URL: {}, status: {}", final_url, final_status);
                    }
                    Err(e) => {
                        error!("Internal redirect failed: {:?}", e);
                        let new_loc = match ctx.route_mode {
                            RouteMode::AntiLeechQuery => {
                                let encoded = urlencoding::encode(&loc_for_fallback);
                                format!("/?url={}", encoded)
                            }
                            RouteMode::IptvDirect => {
                                format!("/iptv/{}", loc_for_fallback)
                            }
                            RouteMode::ProxyPath => {
                                format!("/iptv/{}", loc_for_fallback)
                            }
                        };
                        upstream_response.insert_header("Location", &new_loc)?;
                        info!("Fallback to client-side redirect: {} -> {}", loc_for_fallback, new_loc);
                    }
                }
            }
        }

        if ctx.needs_rewrite_iptv || ctx.needs_rewrite_surrit {
            upstream_response.remove_header("Content-Length");
        }

        Ok(())
    }

    fn response_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        _end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> Result<Option<Duration>> {
        // 如果有缓存的 body（来自内部重定向），先设置为当前 body
        if let Some(cached) = ctx.cached_body.take() {
            *body = Some(cached);
            // 如果需要重写，不要提前返回；如果不需要，可以直接返回
            if !ctx.needs_rewrite_iptv && !ctx.needs_rewrite_surrit {
                return Ok(None);
            }
        }

        if let Some(bytes) = body.as_mut() {
            if ctx.needs_rewrite_iptv && self.finder.find(bytes).is_some() {
                if let Ok(content) = std::str::from_utf8(bytes) {
                    let modified = content.replace(
                        "http://116.199.",
                        &self.config.replacement_pattern,
                    );
                    *bytes = Bytes::from(modified);
                    debug!("IPTV m3u8 rewritten");
                }
            }

            if ctx.needs_rewrite_surrit {
                if let Ok(content) = std::str::from_utf8(bytes) {
                    let base = ctx.base_url.as_ref().expect("base_url missing for surrit rewrite");
                    let mut new_content = String::new();
                    for line in content.lines() {
                        if line.starts_with('#') || line.trim().is_empty() {
                            new_content.push_str(line);
                        } else {
                            let full_url = if line.starts_with("http://") || line.starts_with("https://") {
                                line.to_string()
                            } else {
                                format!("{}{}", base, line)
                            };

                            if full_url.ends_with(".jpeg") {
                                let ts_url = full_url.replace(".jpeg", ".ts");
                                let sep = if ts_url.contains('?') { "&" } else { "?" };
                                let fixed_url = format!("{}{}real_ext=jpeg", ts_url, sep);
                                new_content.push_str(&format!(
                                    "http://{}:{}/iptv/{}\n",
                                    self.config.local_ip, self.config.bind_port, fixed_url
                                ));
                            } else {
                                let encoded = urlencoding::encode(&full_url);
                                new_content.push_str(&format!(
                                    "http://{}:{}/?url={}\n",
                                    self.config.local_ip, self.config.bind_port, encoded
                                ));
                            }
                        }
                    }
                    *bytes = Bytes::from(new_content);
                    debug!("Anti-leech m3u8 rewritten");
                }
            }
        }
        Ok(None)
    }

    async fn logging(
        &self,
        session: &mut Session,
        e: Option<&Error>,
        _ctx: &mut Self::CTX,
    ) {
        let req = session.req_header();
        let status = session
            .response_written()
            .map(|r| r.status.as_u16())
            .unwrap_or(0);
        let client = session
            .client_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|| "unknown".into());
        if let Some(err) = e {
            error!(
                "{} {} {} - Status:{} Error:{:?}",
                client,
                req.method,
                req.uri.path(),
                status,
                err
            );
        } else {
            info!(
                "{} {} {} - Status:{}",
                client,
                req.method,
                req.uri.path(),
                status
            );
        }
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    info!("========================================");
    info!("IPTV Proxy Server v2.0.0 (follow redirect)");
    info!("========================================");

    let local_ip = std::env::var("LOCAL_IP").unwrap_or_else(|_| "192.168.1.3".to_string());
    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let workers = std::env::var("WORKERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| num_cpus::get());
    let bind_port: u16 = bind_addr
        .split(':')
        .nth(1)
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);

    info!("Local IP: {}", local_ip);
    info!("Bind: {}", bind_addr);
    info!("Workers: {}", workers);

    let config = ProxyConfig::new(local_ip.clone(), bind_port);
    let mut server = Server::new(Some(Opt {
        upgrade: false,
        daemon: false,
        nocapture: false,
        test: false,
        conf: None,
    }))
    .expect("Failed to create server");
    server.bootstrap();

    let mut proxy_service = http_proxy_service(&server.configuration, IptvProxy::new(config));
    proxy_service.add_tcp(&bind_addr);
    server.add_service(proxy_service);

    info!("========================================");
    info!("Server listening on: {}", bind_addr);
    info!("Usage:");
    info!("  IPTV:   http://<ip>:8080/iptv/http://116.199.x.x/path/file.m3u8");
    info!("  TS:     http://<ip>:8080/proxy/116.199.x.x:port/path/file.ts");
    info!("  Leech:  http://<ip>:8080/?url=<URL-encoded playlist>");
    info!("========================================");

    server.run_forever();
}
