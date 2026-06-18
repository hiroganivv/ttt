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
use std::collections::HashMap;

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
    /// 用于存放服务端跟随重定向后得到的完整响应体
    cached_body: Option<Bytes>,
    /// 缓存的上游最终响应头（用于替换 302 返回给客户端的头）
    cached_headers: Option<HashMap<String, String>>,
    /// 缓存的上游最终状态码
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
    /// 用于服务端内部重定向请求的 HTTP 客户端
    client: reqwest::Client,
}

impl IptvProxy {
    pub fn new(config: ProxyConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none()) // 我们自己控制重定向
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

    /// 服务端跟随重定向，返回最终的响应信息
    async fn follow_redirects(
        client: &reqwest::Client,
        mut url: String,
        depth: u8,
    ) -> Result<(u16, HashMap<String, String>, Bytes)> {
        if depth == 0 {
            return Err(Error::explain(
                ErrorType::InternalError,
                "Too many redirects",
            ));
        }

        let mut request = client.get(&url);
        // 如果目标主机需要反盗链，则加上 Referer 和 UA
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
                // 处理相对路径
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

        // 收集响应头
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

        Ok((status, headers, body))
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
            if let Ok(decoded) = urlencoding::decode(encoded) {
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

                // 服务端自动跟随重定向
                match Self::follow_redirects(&self.client, loc_str, MAX_REDIRECT_DEPTH).await {
                    Ok((final_status, headers, body)) => {
                        // 将上游响应修改为最终的响应
                        upstream_response.set_status(final_status);
                        // 清除原有所有头（包括 Location）
                        upstream_response.headers.clear();
                        for (k, v) in &headers {
                            // skip content-length, we will set it later if needed
                            if k.to_lowercase() != "content-length" {
                                if let Err(e) = upstream_response.insert_header(k, v) {
                                    warn!("Failed to set header {}: {}", k, e);
                                }
                            }
                        }
                        // 缓存 body，后续在 body_filter 中替换
                        ctx.cached_body = Some(body);
                        // 记录最终状态，确保 body_filter 知道需要替换
                        ctx.cached_status = Some(final_status);
                        ctx.cached_headers = Some(headers);
                        info!("Internal redirect succeeded, final status: {}", final_status);
                    }
                    Err(e) => {
                        error!("Internal redirect failed: {:?}", e);
                        // 回退到透传 302（但会保持原 Location 或修改后的）
                        // 此处选择修改 Location 返回给客户端
                        let new_loc = match ctx.route_mode {
                            RouteMode::AntiLeechQuery => {
                                let encoded = urlencoding::encode(&loc_str);
                                format!("/?url={}", encoded)
                            }
                            RouteMode::IptvDirect => {
                                format!("/iptv/{}", loc_str)
                            }
                            RouteMode::ProxyPath => {
                                format!("/iptv/{}", loc_str)
                            }
                        };
                        upstream_response.insert_header("Location", &new_loc)?;
                        info!("Fallback to client-side redirect: {} -> {}", loc_str, new_loc);
                    }
                }
            }
        }

        // 对于非重定向最终响应，也需要清理 Content-Length（因为 body 可能被改写）
        if ctx.needs_rewrite_iptv || ctx.needs_rewrite_surrit {
            upstream_response.remove_header("Content-Length");
        }

        Ok(())
    }

    fn response_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> Result<Option<Duration>> {
        // 如果存在内部重定向获得的缓存 body，则用它替换原始 body
        if let Some(cached) = ctx.cached_body.take() {
            *body = Some(cached);
            // 如果还有后续数据（通常不会），忽略掉
            return Ok(None);
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
    info!("  Leech:  http://<ip>:8080/?url=https://surrit.com/.../playlist.m3u8");
    info!("========================================");

    server.run_forever();
}
