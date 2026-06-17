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
use std::time::Duration;

// ==================== 正则与常量 ====================

static PROXY_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^/proxy/([^:]+)(?::(\d+))?/(.*)$").expect("Invalid proxy regex")
});

static M3U8_NEEDLE: &[u8] = b"http://116.199.";

const REFERER_VALUE: &str = "https://missav.ws/dm242/cn";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

// ==================== 配置 ====================

#[derive(Clone)]
pub struct ProxyConfig {
    pub local_ip: String,
    pub replacement_pattern: String,
}

impl ProxyConfig {
    pub fn new(local_ip: String, bind_port: u16) -> Self {
        let replacement_pattern = format!("http://{}:{}/proxy/116.199.", local_ip, bind_port);
        Self { local_ip, replacement_pattern }
    }
}

// ==================== 请求上下文 ====================

enum ProxyMode {
    Iptv,
    AntiLeech,
}

pub struct ProxyContext {
    mode: ProxyMode,
    target_url: Option<url::Url>,
    is_m3u8: bool,
    needs_referer: bool,
    needs_rewrite_iptv: bool,
    needs_rewrite_surrit: bool,
    base_url: Option<String>,
}

impl ProxyContext {
    fn new() -> Self {
        Self {
            mode: ProxyMode::Iptv,
            target_url: None,
            is_m3u8: false,
            needs_referer: false,
            needs_rewrite_iptv: false,
            needs_rewrite_surrit: false,
            base_url: None,
        }
    }
}

// ==================== 代理主体 ====================

pub struct IptvProxy {
    config: ProxyConfig,
    finder: memmem::Finder<'static>,
}

impl IptvProxy {
    pub fn new(config: ProxyConfig) -> Self {
        Self { config, finder: memmem::Finder::new(M3U8_NEEDLE) }
    }
}

#[async_trait]
impl ProxyHttp for IptvProxy {
    type CTX = ProxyContext;

    fn new_ctx(&self) -> Self::CTX { ProxyContext::new() }

    async fn request_filter(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<bool> {
        let req = session.req_header();
        let path = req.uri.path();
        let query = req.uri.query().unwrap_or("");

        // 健康检查：仅当路径为 /health 或 / 且不含 url= 参数时
        if path == "/health" || (path == "/" && !query.contains("url=")) {
            let resp = ResponseHeader::build(200, None)
                .map_err(|e| Error::explain(ErrorType::InternalError, format!("build response: {}", e)))?;
            session.write_response_header(Box::new(resp), false).await?;
            session.write_response_body(Some(Bytes::from("OK")), true).await?;
            return Ok(true);
        }

        // favicon.ico 返回 404，避免日志污染
        if path == "/favicon.ico" {
            let resp = ResponseHeader::build(404, None)
                .map_err(|e| Error::explain(ErrorType::InternalError, format!("build response: {}", e)))?;
            session.write_response_header(Box::new(resp), true).await?;
            return Ok(true);
        }

        if path.starts_with("/iptv/") || path.starts_with("/proxy/") {
            ctx.mode = ProxyMode::Iptv;
            return Ok(false);
        }

        if let Some(url_param) = query.split('&').find(|p| p.starts_with("url=")) {
            let encoded = &url_param[4..];
            if let Ok(decoded) = urlencoding::decode(encoded) {
                info!("Decoded anti-leech URL: {}", decoded);
                let decoded_str = decoded.to_string();

                if let Ok(url) = url::Url::parse(&decoded_str) {
                    let host = url.host_str().unwrap_or("surrit.com").to_string();
                    let new_path = url.path().to_string();
                    let is_m3u8 = decoded_str.ends_with(".m3u8");

                    ctx.mode = ProxyMode::AntiLeech;
                    ctx.target_url = Some(url);
                    ctx.is_m3u8 = is_m3u8;
                    ctx.needs_referer = host.contains("surrit.com") || host.contains("fourhoi.com");
                    ctx.needs_rewrite_surrit = is_m3u8;

                    if let Some(last_slash) = decoded_str.rfind('/') {
                        ctx.base_url = Some(decoded_str[..last_slash + 1].to_string());
                    } else {
                        ctx.base_url = Some(decoded_str);
                    }

                    session.req_header_mut().set_raw_path(new_path.as_bytes())?;
                    session.req_header_mut().insert_header("Host", &host)?;

                    return Ok(false);
                }
            }
        }

        warn!("Invalid path: {}", path);
        Err(Error::explain(ErrorType::HTTPStatus(400), "Use /iptv/URL, /proxy/host:port/path, or ?url=encoded_target"))
    }

    async fn upstream_peer(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        match ctx.mode {
            ProxyMode::Iptv => {
                let req = session.req_header();
                let path = req.uri.path();
                let query = req.uri.query().unwrap_or("");

                if path.starts_with("/iptv/") {
                    let url_str = path.strip_prefix("/iptv/")
                        .ok_or_else(|| Error::explain(ErrorType::HTTPStatus(400), "Bad /iptv/ URL"))?;
                    let full = if query.is_empty() { url_str.to_string() } else { format!("{}?{}", url_str, query) };
                    let url = url::Url::parse(&full)
                        .map_err(|e| Error::explain(ErrorType::HTTPStatus(400), format!("Invalid URL: {}", e)))?;

                    let host = url.host_str().unwrap_or("localhost").to_string();
                    let port = url.port().unwrap_or(if url.scheme() == "https" { 443 } else { 80 });
                    let is_https = url.scheme() == "https";

                    ctx.target_url = Some(url);
                    ctx.is_m3u8 = path.contains(".m3u8");
                    ctx.needs_referer = false;
                    ctx.needs_rewrite_iptv = ctx.is_m3u8 && host.contains("116.199");

                    let peer = HttpPeer::new((host.clone(), port), is_https, host.clone());
                    return Ok(Box::new(peer));
                }

                if path.starts_with("/proxy/") {
                    let captures = PROXY_REGEX.captures(path)
                        .ok_or_else(|| Error::explain(ErrorType::HTTPStatus(400), "Invalid proxy path"))?;
                    let host = captures.get(1).unwrap().as_str().to_string();
                    let port = captures.get(2).map(|m| m.as_str().parse().unwrap_or(80)).unwrap_or(80);
                    let uri_path = captures.get(3).unwrap().as_str();
                    let full_path = if query.is_empty() { format!("/{}", uri_path) } else { format!("/{}?{}", uri_path, query) };
                    let url = url::Url::parse(&format!("http://{}:{}{}", host, port, full_path))
                        .map_err(|_| Error::explain(ErrorType::InternalError, "Bad proxy URL"))?;

                    ctx.target_url = Some(url);
                    ctx.is_m3u8 = false;
                    ctx.needs_referer = false;
                    ctx.needs_rewrite_iptv = false;

                    return Ok(Box::new(HttpPeer::new((host.clone(), port), false, host)));
                }

                unreachable!()
            }

            ProxyMode::AntiLeech => {
                let target = ctx.target_url.as_ref().unwrap();
                let host = target.host_str().unwrap_or("surrit.com").to_string();
                let port = target.port().unwrap_or(if target.scheme() == "https" { 443 } else { 80 });
                let is_https = target.scheme() == "https";

                info!("AntiLeech upstream: {}:{} (TLS: {})", host, port, is_https);
                Ok(Box::new(HttpPeer::new((host.clone(), port), is_https, host)))
            }
        }
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream_request: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        if let Some(url) = &ctx.target_url {
            let path_and_query = if let Some(q) = url.query() {
                format!("{}?{}", url.path(), q)
            } else {
                url.path().to_string()
            };
            let uri = Uri::try_from(path_and_query)
                .map_err(|e| Error::explain(ErrorType::InternalError, format!("Invalid URI: {}", e)))?;
            upstream_request.set_uri(uri);
        }

        // 为 IPTV 模式设置 Host 头，防止源站因缺少 Host 而拒绝服务
        if let ProxyMode::Iptv = ctx.mode {
            if let Some(url) = &ctx.target_url {
                let host = url.host_str().unwrap_or("localhost");
                let port = url.port();
                let host_value = if let Some(port) = port {
                    format!("{}:{}", host, port)
                } else {
                    host.to_string()
                };
                upstream_request.insert_header("Host", &host_value)?;
            }
        }

        if let ProxyMode::AntiLeech = ctx.mode {
            if ctx.needs_referer {
                upstream_request.insert_header("Referer", REFERER_VALUE)?;
                info!("Added Referer header for anti-leech");
            }
            upstream_request.insert_header("User-Agent", USER_AGENT)?;
            upstream_request.insert_header("Accept", "*/*")?;
            if ctx.needs_rewrite_surrit {
                upstream_request.remove_header("Accept-Encoding");
            }
        }

        if ctx.needs_rewrite_iptv {
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
        // 处理上游返回的重定向，将 Location 改写为代理路径
        let status = upstream_response.status;
        if status == 301 || status == 302 || status == 307 || status == 308 {
            if let Some(loc) = upstream_response.headers.get("location") {
                if let Ok(loc_str) = loc.to_str() {
                    let new_loc = format!("/iptv/{}", loc_str);
                    upstream_response.insert_header("Location", &new_loc)?;
                    info!("Rewrite redirect location: {} -> {}", loc_str, new_loc);
                } else {
                    warn!("Invalid Location header encoding");
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
        if ctx.needs_rewrite_iptv {
            if let Some(bytes) = body.as_ref() {
                if self.finder.find(bytes).is_some() {
                    if let Ok(content) = std::str::from_utf8(bytes) {
                        let modified = content.replace(
                            "http://116.199.",
                            &self.config.replacement_pattern,
                        );
                        *body = Some(Bytes::from(modified));
                        debug!("IPTV m3u8 rewritten");
                    }
                }
            }
        }

        if ctx.needs_rewrite_surrit {
            if let Some(bytes) = body.as_ref() {
                if let Ok(content) = std::str::from_utf8(bytes) {
                    let mut new_content = String::new();
                    for line in content.lines() {
                        if line.starts_with('#') || line.trim().is_empty() {
                            new_content.push_str(line);
                        } else {
                            let full_url = if line.starts_with("http://") || line.starts_with("https://") {
                                line.to_string()
                            } else {
                                format!("{}{}", ctx.base_url.as_ref().unwrap(), line)
                            };
                            let encoded = urlencoding::encode(&full_url);
                            new_content.push_str(&format!("http://{}:{}/?url={}",
                                self.config.local_ip,
                                std::env::var("BIND_ADDR")
                                    .unwrap_or_else(|_| "0.0.0.0:8080".to_string())
                                    .split(':').nth(1).unwrap_or("8080"),
                                encoded));
                        }
                        new_content.push('\n');
                    }
                    *body = Some(Bytes::from(new_content));
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
        let status = session.response_written().map(|r| r.status.as_u16()).unwrap_or(0);
        let client = session.client_addr().map(|a| a.to_string()).unwrap_or_else(|| "unknown".into());
        if let Some(err) = e {
            error!("{} {} {} - Status:{} Error:{:?}", client, req.method, req.uri.path(), status, err);
        } else {
            info!("{} {} {} - Status:{}", client, req.method, req.uri.path(), status);
        }
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis().init();

    info!("========================================");
    info!("IPTV Proxy Server v1.0.0 (pingora git)");
    info!("Target: IPQ60xx 1GB RAM");
    info!("========================================");

    let local_ip = std::env::var("LOCAL_IP").unwrap_or_else(|_| "192.168.1.3".to_string());
    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let workers = std::env::var("WORKERS").ok().and_then(|s| s.parse().ok()).unwrap_or_else(|| num_cpus::get());
    let bind_port: u16 = bind_addr.split(':').nth(1).and_then(|p| p.parse().ok()).unwrap_or(8080);

    info!("Local IP: {}", local_ip);
    info!("Bind Address: {}", bind_addr);
    info!("Workers: {}", workers);

    let config = ProxyConfig::new(local_ip, bind_port);
    let mut server = Server::new(Some(Opt {
        upgrade: false, daemon: false, nocapture: false, test: false, conf: None,
    })).expect("Failed to create server");
    server.bootstrap();

    let mut proxy_service = http_proxy_service(&server.configuration, IptvProxy::new(config));
    proxy_service.add_tcp(&bind_addr);
    info!("Performance tuning: {} workers, max 65535 conns/worker", workers);
    server.add_service(proxy_service);

    info!("========================================");
    info!("Server listening on: {}", bind_addr);
    info!("Usage:");
    info!("  IPTV M3U8:  http://<ip>:8080/iptv/http://116.199.x.x/path/file.m3u8");
    info!("  TS (HTTP):  http://<ip>:8080/proxy/116.199.x.x:port/path/file.ts");
    info!("  Surrit/Fourhoi: http://<ip>:8080/?url=https%3A%2F%2Fsurrit.com%2F...%2Fplaylist.m3u8");
    info!("  Health:     http://<ip>:8080/health");
    info!("========================================");

    server.run_forever();
}
