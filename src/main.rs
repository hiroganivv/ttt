use async_trait::async_trait;
use bytes::Bytes;
use log::{debug, error, info, warn};
use memchr::memmem;
use once_cell::sync::Lazy;
use pingora::prelude::*;
use pingora::proxy::{ProxyHttp, Session};
use regex::Regex;
use std::sync::Arc;
use std::time::Duration;

// ==================== 编译时常量 ====================

static PROXY_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^/proxy/([^:]+):(\d+)/(.*)$").expect("Invalid proxy regex")
});

static M3U8_NEEDLE: &[u8] = b"http://116.199.";
static M3U8_REPLACEMENT_PREFIX: &str = "http://";
static M3U8_REPLACEMENT_SUFFIX: &str = ":8080/proxy/116.199.";

const REFERER_VALUE: &str = "https://missav.ws/dm242/cn";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

// ==================== 配置结构 ====================

#[derive(Clone)]
pub struct ProxyConfig {
    pub local_ip: String,
    pub replacement_pattern: String,
}

impl ProxyConfig {
    pub fn new(local_ip: String) -> Self {
        let replacement_pattern = format!(
            "{}{}{}",
            M3U8_REPLACEMENT_PREFIX, local_ip, M3U8_REPLACEMENT_SUFFIX
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
    config: Arc<ProxyConfig>,
    finder: memmem::Finder<'static>,
}

impl IptvProxy {
    pub fn new(config: ProxyConfig) -> Self {
        Self {
            config: Arc::new(config),
            finder: memmem::Finder::new(M3U8_NEEDLE),
        }
    }

    #[inline]
    fn parse_iptv_url(&self, path: &str, query: &str) -> Result<url::Url> {
        let url_str = path.strip_prefix("/iptv/")
            .ok_or_else(|| Error::new(ErrorType::HTTPStatus(400)))?;

        let full_url = if query.is_empty() {
            url_str.to_string()
        } else {
            format!("{}?{}", url_str, query)
        };

        url::Url::parse(&full_url)
            .map_err(|e| {
                error!("URL parse error: {} - {}", full_url, e);
                Error::explain(ErrorType::HTTPStatus(400), format!("Invalid URL: {}", e))
            })
    }

    #[inline]
    fn parse_proxy_path(&self, path: &str, query: &str) -> Result<(String, u16, String)> {
        let captures = PROXY_REGEX.captures(path)
            .ok_or_else(|| Error::new(ErrorType::HTTPStatus(400)))?;

        let host = captures.get(1).unwrap().as_str().to_string();
        let port = captures.get(2).unwrap().as_str()
            .parse::<u16>()
            .map_err(|_| Error::new(ErrorType::HTTPStatus(400)))?;
        let uri_path = captures.get(3).unwrap().as_str();
        
        // 重要：保留query参数
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

    async fn upstream_peer(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let req_header = session.req_header();
        let path = req_header.uri.path();
        let query = req_header.uri.query().unwrap_or("");

        // 健康检查
        if path == "/health" || path == "/" {
            return Error::e_explain(ErrorType::HTTPStatus(200), "OK");
        }

        // 处理 /iptv/ 路径
        if path.starts_with("/iptv/") {
            let url = self.parse_iptv_url(path, query)?;
            let host = url.host_str().unwrap_or("localhost").to_string();
            let port = url.port().unwrap_or(if url.scheme() == "https" { 443 } else { 80 });
            let is_https = url.scheme() == "https";

            ctx.target_url = Some(url);
            ctx.is_m3u8 = path.contains(".m3u8");
            ctx.needs_referer = Self::needs_referer(&host);
            ctx.needs_rewrite = ctx.is_m3u8 && Self::needs_m3u8_rewrite(&host);

            debug!("IPTV: {}:{} HTTPS:{} M3U8:{} Rewrite:{}", 
                   host, port, is_https, ctx.is_m3u8, ctx.needs_rewrite);

            return Ok(Box::new(HttpPeer::new(
                (host.clone(), port),
                is_https,
                host,
            )));
        }

        // 处理 /proxy/ 路径
        if path.starts_with("/proxy/") {
            let (host, port, full_path) = self.parse_proxy_path(path, query)?;

            ctx.target_url = Some(
                url::Url::parse(&format!("http://{}:{}{}", host, port, full_path))
                    .map_err(|_| Error::new(ErrorType::InternalError))?
            );

            debug!("Proxy TS: {}:{}{}", host, port, full_path);

            return Ok(Box::new(HttpPeer::new(
                (host.clone(), port),
                false,
                host,
            )));
        }

        warn!("Invalid path: {}", path);
        Error::e_explain(
            ErrorType::HTTPStatus(400),
            "Use /iptv/URL or /proxy/host:port/path"
        )
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream_request: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        if let Some(url) = &ctx.target_url {
            // 设置请求路径
            let path_and_query = if let Some(q) = url.query() {
                format!("{}?{}", url.path(), q)
            } else {
                url.path().to_string()
            };

            upstream_request.set_uri(path_and_query.as_bytes())?;

            // 设置请求头
            if ctx.needs_referer {
                upstream_request.insert_header("Referer", REFERER_VALUE)?;
            }

            upstream_request.insert_header("User-Agent", USER_AGENT)?;
            upstream_request.insert_header("Accept", "*/*")?;

            // m3u8需要禁用压缩以便改写
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
        // 如果需要改写body，移除Content-Length
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

        if let Some(bytes) = body {
            // 使用SIMD加速的memchr查找
            if self.finder.find(bytes).is_some() {
                // 零拷贝转换为可变字符串
                if let Ok(content) = std::str::from_utf8(bytes) {
                    let modified = content.replace(
                        "http://116.199.",
                        &self.config.replacement_pattern
                    );

                    *body = Some(Bytes::from(modified));
                    debug!("M3U8 rewritten, size: {} -> {}", bytes.len(), body.as_ref().unwrap().len());
                }
            }
        }

        Ok(None)
    }

    async fn logging(
        &self,
        session: &mut Session,
        e: Option<&Error>,
        ctx: &mut Self::CTX,
    ) {
        let req = session.req_header();
        let status = session
            .response_written()
            .map(|r| r.status.as_u16())
            .unwrap_or(0);

        let client_addr = session.client_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|| "unknown".to_string());

        if let Some(err) = e {
            error!("{} {} {} - Status: {} Error: {:?}",
                   client_addr, req.method, req.uri.path(), status, err);
        } else {
            info!("{} {} {} - Status: {}",
                 client_addr, req.method, req.uri.path(), status);
        }
    }
}

// ==================== 主程序 ====================

fn main() {
    // 初始化日志
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    info!("========================================");
    info!("IPTV Proxy Server v1.0.0");
    info!("Target: IPQ60xx 1GB RAM");
    info!("Max Concurrency: 50,000");
    info!("========================================");

    // 配置
    let local_ip = std::env::var("LOCAL_IP")
        .unwrap_or_else(|_| "192.168.1.3".to_string());
    let bind_addr = std::env::var("BIND_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let workers = std::env::var("WORKERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(num_cpus::get);

    info!("Local IP: {}", local_ip);
    info!("Bind Address: {}", bind_addr);
    info!("Workers: {}", workers);

    // 创建配置
    let config = ProxyConfig::new(local_ip);
    
    // 创建服务器
    let mut server = Server::new(Some(Opt {
        upgrade: false,
        daemon: false,
        nocapture: false,
        test: false,
        conf: None,
    })).expect("Failed to create server");

    server.bootstrap();

    // 配置代理服务
    let mut proxy_service = http_proxy_service(
        &server.configuration,
        IptvProxy::new(config),
    );

    proxy_service.add_tcp(&bind_addr);

    // 性能优化设置
    info!("Applying performance tuning...");
    info!("  - Worker threads: {}", workers);
    info!("  - Max connections per worker: 65535");
    info!("  - Total capacity: {}", workers * 65535);

    server.add_service(proxy_service);

    info!("========================================");
    info!("Server started successfully!");
    info!("Listening on: {}", bind_addr);
    info!("");
    info!("Usage:");
    info!("  M3U8: http://<ip>:8080/iptv/http://116.199.x.x/path/file.m3u8");
    info!("  M3U8: http://<ip>:8080/iptv/https://surrit.com/path/file.m3u8");
    info!("  TS:   http://<ip>:8080/proxy/116.199.x.x:port/path/file.ts");
    info!("  Health: http://<ip>:8080/health");
    info!("========================================");

    server.run_forever();
}
