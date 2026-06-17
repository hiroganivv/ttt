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
use pingora::tls::rustls::TlsConnector;
use regex::Regex;
use std::time::Duration;

// ==================== 编译时常量 ====================

static PROXY_REGEX: Lazy<Regex> = Lazy::new(|| {
    // 端口可选，若无端口默认为80
    Regex::new(r"^/proxy/([^:]+)(?::(\d+))?/(.*)$").expect("Invalid proxy regex")
});

static M3U8_NEEDLE: &[u8] = b"http://116.199.";
const REFERER_VALUE: &str = "https://missav.ws/dm242/cn";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

// ==================== 配置结构 ====================

#[derive(Clone)]
pub struct ProxyConfig {
    pub local_ip: String,
    pub replacement_pattern: String,
}

impl ProxyConfig {
    pub fn new(local_ip: String, bind_port: u16) -> Self {
        let replacement_pattern = format!(
            "http://{}:{}/proxy/116.199.",
            local_ip, bind_port
        );
        Self {
            local_ip,
            replacement_pattern,
        }
    }
}

// ==================== 请求上下文 ====================

pub struct ProxyContext {
    target_url: Option<url::Url>,
    is_m3u8: bool,
    needs_referer: bool,
    needs_rewrite: bool,
}

impl ProxyContext {
    fn new() -> Self {
        Self {
            target_url: None,
            is_m3u8: false,
            needs_referer: false,
            needs_rewrite: false,
        }
    }
}

// ==================== 核心代理逻辑 ====================

pub struct IptvProxy {
    config: ProxyConfig,
    finder: memmem::Finder<'static>,
}

impl IptvProxy {
    pub fn new(config: ProxyConfig) -> Self {
        Self {
            config,
            finder: memmem::Finder::new(M3U8_NEEDLE),
        }
    }

    #[inline]
    fn parse_iptv_url(&self, path: &str, query: &str) -> Result<url::Url> {
        let url_str = path
            .strip_prefix("/iptv/")
            .ok_or_else(|| Error::explain(ErrorType::HTTPStatus(400), "Missing /iptv/ prefix"))?;

        let full_url = if query.is_empty() {
            url_str.to_string()
        } else {
            format!("{}?{}", url_str, query)
        };

        url::Url::parse(&full_url).map_err(|e| {
            error!("URL parse error: {} - {}", full_url, e);
            Error::explain(ErrorType::HTTPStatus(400), format!("Invalid URL: {}", e))
        })
    }

    #[inline]
    fn parse_proxy_path(&self, path: &str, query: &str) -> Result<(String, u16, String)> {
        let captures = PROXY_REGEX
            .captures(path)
            .ok_or_else(|| Error::explain(ErrorType::HTTPStatus(400), "Invalid proxy path"))?;

        let host = captures.get(1).unwrap().as_str().to_string();
        let port = captures
            .get(2)
            .map(|m| m.as_str().parse().unwrap_or(80))
            .unwrap_or(80);

        let uri_path = captures.get(3).unwrap().as_str();
        let full_path = if query.is_empty() {
            format!("/{}", uri_path)
        } else {
            format!("/{}?{}", uri_path, query)
        };

        Ok((host, port, full_path))
    }

    #[inline]
    fn needs_referer(host: &str) -> bool {
        host.contains("surrit.com") || host.contains("fourhoi.com")
    }

    #[inline]
    fn needs_m3u8_rewrite(host: &str) -> bool {
        host.contains("116.199")
    }
}

#[async_trait]
impl ProxyHttp for IptvProxy {
    type CTX = ProxyContext;

    fn new_ctx(&self) -> Self::CTX {
        ProxyContext::new()
    }

    // ---------- 健康检查 ----------
    async fn request_filter(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<bool> {
        let path = session.req_header().uri.path();
        if path == "/health" || path == "/" {
            let resp = ResponseHeader::build(200, None)
                .map_err(|e| Error::explain(ErrorType::InternalError, format!("build response: {}", e)))?;
            session.write_response_header(Box::new(resp), false).await?;
            session.write_response_body(Some(Bytes::from("OK")), true).await?;
            return Ok(true);
        }
        Ok(false)
    }

    async fn upstream_peer(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let req_header = session.req_header();
        let path = req_header.uri.path();
        let query = req_header.uri.query().unwrap_or("");

        // 处理 /iptv/ 路径
        if path.starts_with("/iptv/") {
            let url = self.parse_iptv_url(path, query)?;
            let host = url.host_str().unwrap_or("localhost").to_string();
            let port = url
                .port()
                .unwrap_or(if url.scheme() == "https" { 443 } else { 80 });
            let is_https = url.scheme() == "https";

            ctx.target_url = Some(url);
            ctx.is_m3u8 = path.contains(".m3u8");
            ctx.needs_referer = Self::needs_referer(&host);
            ctx.needs_rewrite = ctx.is_m3u8 && Self::needs_m3u8_rewrite(&host);

            let mut peer = HttpPeer::new(
                (host.clone(), port),
                is_https,
                host.clone(),
            );

            if is_https {
                peer.options.set_tls(Some(Box::new(TlsConnector::new())));
            }

            debug!(
                "IPTV: {}:{} HTTPS:{} M3U8:{} Rewrite:{}",
                host, port, is_https, ctx.is_m3u8, ctx.needs_rewrite
            );

            return Ok(Box::new(peer));
        }

        // 处理 /proxy/ 路径
        if path.starts_with("/proxy/") {
            let (host, port, full_path) = self.parse_proxy_path(path, query)?;

            ctx.target_url = Some(
                url::Url::parse(&format!("http://{}:{}{}", host, port, full_path))
                    .map_err(|_| Error::explain(ErrorType::InternalError, "Failed to build proxy URL"))?,
            );

            debug!("Proxy TS: {}:{}{}", host, port, full_path);
            return Ok(Box::new(HttpPeer::new(
                (host.clone(), port),
                false,
                host,
            )));
        }

        warn!("Invalid path: {}", path);
        Err(Error::explain(
            ErrorType::HTTPStatus(400),
            "Use /iptv/URL or /proxy/host:port/path",
        ))
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

            if ctx.needs_referer {
                upstream_request.insert_header("Referer", REFERER_VALUE)?;
            }
            upstream_request.insert_header("User-Agent", USER_AGENT)?;
            upstream_request.insert_header("Accept", "*/*")?;

            if ctx.needs_rewrite {
                upstream_request.remove_header("Accept-Encoding");
            }
        }
        Ok(())
    }

    async fn response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        if ctx.needs_rewrite {
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
        if !ctx.needs_rewrite {
            return Ok(None);
        }

        if let Some(bytes) = body.as_ref() {
            if self.finder.find(bytes).is_some() {
                if let Ok(content) = std::str::from_utf8(bytes) {
                    let old_len = content.len();
                    let modified = content.replace(
                        "http://116.199.",
                        &self.config.replacement_pattern,
                    );
                    let new_len = modified.len();
                    *body = Some(Bytes::from(modified));
                    debug!("M3U8 rewritten: {} -> {} bytes", old_len, new_len);
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
    info!("IPTV Proxy Server v1.0.0 (pingora git)");
    info!("Target: IPQ60xx 1GB RAM");
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
    info!("Bind Address: {}", bind_addr);
    info!("Workers: {}", workers);

    let config = ProxyConfig::new(local_ip, bind_port);

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

    info!("Performance tuning: {} workers, max 65535 conns/worker", workers);
    server.add_service(proxy_service);

    info!("========================================");
    info!("Server listening on: {}", bind_addr);
    info!("Usage:");
    info!("  IPTV M3U8:  http://<ip>:8080/iptv/http://116.199.x.x/path/file.m3u8");
    info!("  Surrit M3U8: http://<ip>:8080/iptv/https://surrit.com/.../playlist.m3u8");
    info!("  TS (HTTP):  http://<ip>:8080/proxy/116.199.x.x:port/path/file.ts");
    info!("  Health:     http://<ip>:8080/health");
    info!("========================================");

    server.run_forever();
}
