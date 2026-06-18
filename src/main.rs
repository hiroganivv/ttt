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

// ━━━━━━━━━━━━ 常量 & 正则 ━━━━━━━━━━━━━━━━━━━━━━━

static M3U8_NEEDLE: &[u8] = b"http://116.199.";
static PROXY_PATH_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^/proxy/([^:]+)(?::(\d+))?/(.*)$").expect("Invalid proxy regex")
});

const REFERER_VALUE: &str = "https://missav.ws/dm242/cn";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

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

/// 记录用户请求的入口形式，用于重定向时生成正确的 Location
#[derive(Debug, Clone, Copy, PartialEq)]
enum RouteMode {
    IptvDirect,    // /iptv/<URL>
    ProxyPath,     // /proxy/<host>:<port>/<path>
    AntiLeechQuery,// /?url=<encoded>
}

pub struct ProxyContext {
    route_mode: RouteMode,
    target_url: Option<url::Url>,
    is_m3u8: bool,
    needs_referer: bool,
    /// 是否需替换 m3u8 中 116.199 开头的链接（仅 IPTV 模式）
    needs_rewrite_iptv: bool,
    /// 是否需重写反盗链 m3u8 中的片段链接
    needs_rewrite_surrit: bool,
    /// 用于拼接相对路径的 base URL（仅 AntiLeech 重写时使用）
    base_url: Option<String>,
    /// 是否将请求路径中的 .ts 改回 .jpeg 以获取实际文件（用于 surrit 反盗链）
    needs_jpeg_fix: bool,
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
        }
    }
}

// ━━━━━━━━━━━━ 代理主体 ━━━━━━━━━━━━━━━━━━━━━━━━━━━

pub struct IptvProxy {
    config: ProxyConfig,
    finder: memmem::Finder<'static>,
}

impl IptvProxy {
    pub fn new(config: ProxyConfig) -> Self {
        Self { config, finder: memmem::Finder::new(M3U8_NEEDLE) }
    }

    /// 判断目标主机是否需要反盗链头
    fn host_needs_referer(host: &str) -> bool {
        host.contains("surrit.com") || host.contains("fourhoi.com")
    }
}

#[async_trait]
impl ProxyHttp for IptvProxy {
    type CTX = ProxyContext;

    fn new_ctx(&self) -> Self::CTX {
        ProxyContext::new()
    }

    // ─── 请求入口解析 ─────────────────────────────

    async fn request_filter(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<bool> {
        let req = session.req_header();
        let path = req.uri.path();
        let query = req.uri.query().unwrap_or("");

        // 健康检查
        if path == "/health" || (path == "/" && !query.contains("url=")) {
            let resp = ResponseHeader::build(200, None)
                .map_err(|e| Error::explain(ErrorType::InternalError, e))?;
            session.write_response_header(Box::new(resp), false).await?;
            session.write_response_body(Some(Bytes::from("OK")), true).await?;
            return Ok(true);
        }

        // favicon
        if path == "/favicon.ico" {
            let resp = ResponseHeader::build(404, None)
                .map_err(|e| Error::explain(ErrorType::InternalError, e))?;
            session.write_response_header(Box::new(resp), true).await?;
            return Ok(true);
        }

        // ── /iptv/<URL> ──
        if path.starts_with("/iptv/") {
            ctx.route_mode = RouteMode::IptvDirect;

            let url_str = path.strip_prefix("/iptv/").unwrap();
            let full = if query.is_empty() { url_str.to_string() } else { format!("{}?{}", url_str, query) };
            let mut url = url::Url::parse(&full)
                .map_err(|e| Error::explain(ErrorType::HTTPStatus(400), format!("Invalid URL: {}", e)))?;

            // 处理 real_ext=jpeg 参数（surrit 反盗链扩展名修正）
            if url.query_pairs().any(|(k, v)| k == "real_ext" && v == "jpeg") {
                ctx.needs_jpeg_fix = true;
                // 从 URL 中移除 real_ext 参数，避免传给上游
                let clean_params: Vec<_> = url.query_pairs()
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

        // ── /proxy/host:port/path ──
        if path.starts_with("/proxy/") {
            ctx.route_mode = RouteMode::ProxyPath;

            let caps = PROXY_PATH_REGEX.captures(path)
                .ok_or_else(|| Error::explain(ErrorType::HTTPStatus(400), "Invalid proxy path"))?;
            let host = caps.get(1).unwrap().as_str().to_string();
            let port: u16 = caps.get(2).map(|m| m.as_str().parse().unwrap_or(80)).unwrap_or(80);
            let uri_path = caps.get(3).unwrap().as_str();
            let full_path = if query.is_empty() { format!("/{}", uri_path) } else { format!("/{}?{}", uri_path, query) };
            let url = url::Url::parse(&format!("http://{}:{}{}", host, port, full_path))
                .map_err(|_| Error::explain(ErrorType::InternalError, "Bad proxy URL"))?;

            ctx.target_url = Some(url);
            ctx.is_m3u8 = false;
            ctx.needs_referer = Self::host_needs_referer(&host);
            ctx.needs_rewrite_iptv = false;

            return Ok(false);
        }

        // ── /?url=<encoded> (反盗链主入口) ──
        if let Some(url_param) = query.split('&').find(|p| p.starts_with("url=")) {
            let encoded = &url_param[4..];
            if let Ok(decoded) = urlencoding::decode(encoded) {
                let decoded_str = decoded.to_string();
                info!("Decoded anti-leech URL: {}", decoded_str);

                if let Ok(url) = url::Url::parse(&decoded_str) {
                    let host = url.host_str().unwrap_or("surrit.com").to_string();
                    ctx.route_mode = RouteMode::AntiLeechQuery;
                    ctx.target_url = Some(url);
                    ctx.is_m3u8 = decoded_str.ends_with(".m3u8");
                    ctx.needs_referer = true;   // 反盗链入口始终需要
                    ctx.needs_rewrite_surrit = ctx.is_m3u8;

                    if let Some(last_slash) = decoded_str.rfind('/') {
                        ctx.base_url = Some(decoded_str[..last_slash + 1].to_string());
                    } else {
                        ctx.base_url = Some(decoded_str);
                    }

                    // 修改请求路径为目标 path，Host 也改为目标主机
                    session.req_header_mut().set_raw_path(url.path().as_bytes())?;
                    session.req_header_mut().insert_header("Host", &host)?;

                    return Ok(false);
                }
            }
        }

        warn!("Invalid request: {}", path);
        Err(Error::explain(ErrorType::HTTPStatus(400), "Use /iptv/URL, /proxy/host:port/path, or ?url=encoded_target"))
    }

    // ─── 选择上游 ─────────────────────────────────

    async fn upstream_peer(
        &self,
        session: &mut Session,
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

    // ─── 修改上游请求 ─────────────────────────────

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream_request: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        // 构造上游 URI（处理路径修正、参数等）
        if let Some(url) = &ctx.target_url {
            let mut path_and_query = if let Some(q) = url.query() {
                format!("{}?{}", url.path(), q)
            } else {
                url.path().to_string()
            };

            // .ts → .jpeg 扩展名修正（用于 surrit 反盗链）
            if ctx.needs_jpeg_fix {
                path_and_query = path_and_query.replace(".ts", ".jpeg");
            }

            let uri = Uri::try_from(path_and_query)
                .map_err(|e| Error::explain(ErrorType::InternalError, format!("Invalid URI: {}", e)))?;
            upstream_request.set_uri(uri);
        }

        // 设置 Host 头（所有模式都需要）
        if let Some(url) = &ctx.target_url {
            let host = url.host_str().unwrap_or("localhost");
            let host_value = if let Some(port) = url.port() {
                format!("{}:{}", host, port)
            } else {
                host.to_string()
            };
            upstream_request.insert_header("Host", &host_value)?;
        }

        // 反盗链头：由 needs_referer 标志统一控制
        if ctx.needs_referer {
            upstream_request.insert_header("Referer", REFERER_VALUE)?;
            upstream_request.insert_header("User-Agent", USER_AGENT)?;
            upstream_request.insert_header("Accept", "*/*")?;
        }

        // 需要修改 body 时禁用压缩
        if ctx.needs_rewrite_iptv || ctx.needs_rewrite_surrit {
            upstream_request.remove_header("Accept-Encoding");
        }

        Ok(())
    }

    // ─── 处理上游响应头 ───────────────────────────

    async fn response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        // 重定向修复：根据原始入口生成正确的 Location
        let status = upstream_response.status;
        if status == 301 || status == 302 || status == 307 || status == 308 {
            if let Some(loc) = upstream_response.headers.get("location") {
                if let Ok(loc_str) = loc.to_str() {
                    let new_loc = match ctx.route_mode {
                        RouteMode::AntiLeechQuery => {
                            let encoded = urlencoding::encode(loc_str);
                            format!("/?url={}", encoded)
                        }
                        RouteMode::IptvDirect => {
                            format!("/iptv/{}", loc_str)
                        }
                        RouteMode::ProxyPath => {
                            // 保守处理，直接用 /iptv/ 代理
                            format!("/iptv/{}", loc_str)
                        }
                    };
                    upstream_response.insert_header("Location", &new_loc)?;
                    info!("Rewrite redirect (mode={:?}): {} -> {}", ctx.route_mode, loc_str, new_loc);
                }
            }
        }

        // 需要修改 body 时移除 Content-Length，后续会重新计算
        if ctx.needs_rewrite_iptv || ctx.needs_rewrite_surrit {
            upstream_response.remove_header("Content-Length");
        }

        Ok(())
    }

    // ─── 修改上游响应体 ───────────────────────────

    fn response_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        _end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> Result<Option<Duration>> {
        // IPTV 模式：替换 116.199 开头的链接为 /proxy/ 形式
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

        // 反盗链模式：重写 m3u8 中的片段链接
        if ctx.needs_rewrite_surrit {
            if let Some(bytes) = body.as_ref() {
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
                                // .jpeg → .ts + real_ext=jpeg，通过 /iptv/ 代理
                                let ts_url = full_url.replace(".jpeg", ".ts");
                                let sep = if ts_url.contains('?') { "&" } else { "?" };
                                let fixed_url = format!("{}{}real_ext=jpeg", ts_url, sep);
                                new_content.push_str(&format!("http://{}:{}/iptv/{}\n",
                                    self.config.local_ip, self.config.bind_port, fixed_url));
                            } else {
                                // 其余文件使用 ?url= 入口保持反盗链能力
                                let encoded = urlencoding::encode(&full_url);
                                new_content.push_str(&format!("http://{}:{}/?url={}\n",
                                    self.config.local_ip, self.config.bind_port, encoded));
                            }
                        }
                    }
                    *body = Some(Bytes::from(new_content));
                    debug!("Anti-leech m3u8 rewritten");
                }
            }
        }

        Ok(None)
    }

    // ─── 日志 ─────────────────────────────────────

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

// ━━━━━━━━━━━━ 入口 ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis().init();

    info!("========================================");
    info!("IPTV Proxy Server v1.0.0 (pingora git)");
    info!("========================================");

    let local_ip = std::env::var("LOCAL_IP").unwrap_or_else(|_| "192.168.1.3".to_string());
    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let workers = std::env::var("WORKERS").ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| num_cpus::get());
    let bind_port: u16 = bind_addr.split(':').nth(1).and_then(|p| p.parse().ok()).unwrap_or(8080);

    info!("Local IP: {}", local_ip);
    info!("Bind: {}", bind_addr);
    info!("Workers: {}", workers);

    let config = ProxyConfig::new(local_ip.clone(), bind_port);
    let mut server = Server::new(Some(Opt {
        upgrade: false, daemon: false, nocapture: false, test: false, conf: None,
    })).expect("Failed to create server");
    server.bootstrap();

    let mut proxy_service = http_proxy_service(&server.configuration, IptvProxy::new(config));
    proxy_service.add_tcp(&bind_addr);
    server.add_service(proxy_service);

    info!("========================================");
    info!("Server listening on: {}", bind_addr);
    info!("Usage:");
    info!("  IPTV:   http://<ip>:8080/iptv/http://116.199.x.x/path/file.m3u8");
    info!("  TS:     http://<ip>:8080/proxy/116.199.x.x:port/path/file.ts");
    info!("  Leech:  http://<ip>:8080/?url=https://surrit.com/.../playlist.m3u8");
    info!("========================================");

    server.run_forever();
}
