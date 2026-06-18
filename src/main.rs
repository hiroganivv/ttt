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

// ==================== 常量 ====================

static PROXY_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^/proxy/([^:]+)(?::(\d+))?/(.*)$").expect("Invalid proxy regex")
});

const REFERER_VALUE: &str = "https://missav.ws/dm242/cn";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36";

// ==================== 配置 ====================

#[derive(Clone)]
pub struct ProxyConfig {
    pub local_ip: String,
    pub bind_port: u16,
}

// ==================== 请求上下文 ====================

enum ProxyMode {
    IptvDirect,      // /iptv/URL - 用于116.199直播源
    IptvProxy,       // /proxy/host:port/path - TS片段透明代理
    AntiLeech,       // ?url=encoded - 用于surrit/fourhoi反盗链
}

pub struct ProxyContext {
    mode: ProxyMode,
    target_url: Option<url::Url>,
    is_m3u8: bool,
    base_url: Option<String>,
    needs_jpeg_fix: bool,
}

impl ProxyContext {
    fn new() -> Self {
        Self {
            mode: ProxyMode::IptvDirect,
            target_url: None,
            is_m3u8: false,
            base_url: None,
            needs_jpeg_fix: false,
        }
    }
    
    fn is_iptv_source(&self) -> bool {
        self.target_url.as_ref()
            .and_then(|u| u.host_str())
            .map(|h| h.contains("116.199"))
            .unwrap_or(false)
    }
    
    fn needs_antileech_headers(&self) -> bool {
        self.target_url.as_ref()
            .and_then(|u| u.host_str())
            .map(|h| h.contains("surrit.com") || h.contains("fourhoi.com"))
            .unwrap_or(false)
    }
}

// ==================== 代理主体 ====================

pub struct IptvProxy {
    config: ProxyConfig,
    iptv_finder: memmem::Finder<'static>,
}

impl IptvProxy {
    pub fn new(config: ProxyConfig) -> Self {
        Self { 
            config, 
            iptv_finder: memmem::Finder::new(b"http://116.199."),
        }
    }
    
    fn get_proxy_url(&self, target: &str) -> String {
        format!("http://{}:{}/proxy/116.199.", self.config.local_ip, self.config.bind_port)
    }
    
    fn get_antileech_url(&self, target: &str) -> String {
        let encoded = urlencoding::encode(target);
        format!("http://{}:{}/?url={}", self.config.local_ip, self.config.bind_port, encoded)
    }
}

#[async_trait]
impl ProxyHttp for IptvProxy {
    type CTX = ProxyContext;

    fn new_ctx(&self) -> Self::CTX { ProxyContext::new() }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        let req = session.req_header();
        let path = req.uri.path();
        let query = req.uri.query().unwrap_or("");

        // 健康检查
        if path == "/health" || (path == "/" && !query.contains("url=")) {
            session.write_response_header(Box::new(ResponseHeader::build(200, None)?), false).await?;
            session.write_response_body(Some(Bytes::from("OK")), true).await?;
            return Ok(true);
        }

        // favicon
        if path == "/favicon.ico" {
            session.write_response_header(Box::new(ResponseHeader::build(404, None)?), true).await?;
            return Ok(true);
        }

        // 模式1: /iptv/URL - 直接代理(用于116.199和surrit/fourhoi)
        if path.starts_with("/iptv/") {
            let url_str = &path[6..]; // 去掉 "/iptv/"
            let full = if query.is_empty() { url_str.to_string() } else { format!("{}?{}", url_str, query) };
            let url = url::Url::parse(&full)
                .map_err(|e| Error::explain(ErrorType::HTTPStatus(400), format!("Invalid URL: {}", e)))?;
            
            ctx.mode = ProxyMode::IptvDirect;
            ctx.target_url = Some(url.clone());
            ctx.is_m3u8 = path.contains(".m3u8");
            
            // 检查是否有real_ext参数(surrit的jpeg->ts转换标记)
            if url.query_pairs().any(|(k, v)| k == "real_ext" && v == "jpeg") {
                ctx.needs_jpeg_fix = true;
            }
            
            info!("[IptvDirect] {}", full);
            return Ok(false);
        }

        // 模式2: /proxy/host:port/path - TS片段透明代理
        if path.starts_with("/proxy/") {
            ctx.mode = ProxyMode::IptvProxy;
            info!("[IptvProxy] {}", path);
            return Ok(false);
        }

        // 模式3: ?url=encoded - surrit/fourhoi反盗链
        if let Some(url_param) = query.split('&').find(|p| p.starts_with("url=")) {
            let encoded = &url_param[4..];
            let decoded = urlencoding::decode(encoded)
                .map_err(|e| Error::explain(ErrorType::HTTPStatus(400), format!("Decode error: {}", e)))?;
            let url = url::Url::parse(&decoded)
                .map_err(|e| Error::explain(ErrorType::HTTPStatus(400), format!("Invalid URL: {}", e)))?;
            
            ctx.mode = ProxyMode::AntiLeech;
            ctx.target_url = Some(url.clone());
            ctx.is_m3u8 = decoded.ends_with(".m3u8");
            ctx.base_url = decoded.rsplit_once('/').map(|(base, _)| format!("{}/", base));
            
            // 修改请求路径和Host头
            session.req_header_mut().set_raw_path(url.path().as_bytes())?;
            session.req_header_mut().insert_header("Host", url.host_str().unwrap_or("localhost"))?;
            
            info!("[AntiLeech] {}", decoded);
            return Ok(false);
        }

        Err(Error::explain(ErrorType::HTTPStatus(400), "Use /iptv/URL or ?url=encoded"))
    }

    async fn upstream_peer(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<Box<HttpPeer>> {
        match ctx.mode {
            ProxyMode::IptvDirect => {
                let url = ctx.target_url.as_ref().unwrap();
                let host = url.host_str().unwrap_or("localhost");
                let port = url.port().unwrap_or(if url.scheme() == "https" { 443 } else { 80 });
                let is_https = url.scheme() == "https";
                
                Ok(Box::new(HttpPeer::new((host.to_string(), port), is_https, host.to_string())))
            }
            
            ProxyMode::IptvProxy => {
                let path = session.req_header().uri.path();
                let query = session.req_header().uri.query().unwrap_or("");
                let captures = PROXY_REGEX.captures(path)
                    .ok_or_else(|| Error::explain(ErrorType::HTTPStatus(400), "Invalid proxy path"))?;
                    
                let host = captures[1].to_string();
                let port = captures.get(2).map(|m| m.as_str().parse().unwrap_or(80)).unwrap_or(80);
                let uri_path = &captures[3];
                let full_path = if query.is_empty() { 
                    format!("/{}", uri_path) 
                } else { 
                    format!("/{}?{}", uri_path, query) 
                };
                
                let url = url::Url::parse(&format!("http://{}:{}{}", host, port, full_path))
                    .map_err(|_| Error::explain(ErrorType::InternalError, "Bad proxy URL"))?;
                ctx.target_url = Some(url);
                
                Ok(Box::new(HttpPeer::new((host.clone(), port), false, host)))
            }
            
            ProxyMode::AntiLeech => {
                let url = ctx.target_url.as_ref().unwrap();
                let host = url.host_str().unwrap_or("localhost");
                let port = url.port().unwrap_or(if url.scheme() == "https" { 443 } else { 80 });
                let is_https = url.scheme() == "https";
                
                Ok(Box::new(HttpPeer::new((host.to_string(), port), is_https, host.to_string())))
            }
        }
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream_request: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        let url = ctx.target_url.as_ref().unwrap();
        
        // 构建正确的路径
        let mut path_and_query = if let Some(q) = url.query() {
            format!("{}?{}", url.path(), q)
        } else {
            url.path().to_string()
        };
        
        // surrit的jpeg->ts修正：请求时改回.jpeg
        if ctx.needs_jpeg_fix {
            path_and_query = path_and_query.replace(".ts", ".jpeg");
            // 移除real_ext参数
            if let Some(pos) = path_and_query.find("real_ext=jpeg") {
                let before = &path_and_query[..pos];
                let after_param = &path_and_query[pos+13..];
                path_and_query = if before.ends_with('?') {
                    format!("{}{}", before.trim_end_matches('?'), after_param.trim_start_matches('&'))
                } else {
                    format!("{}{}", before.trim_end_matches('&'), after_param.trim_start_matches('&'))
                };
            }
        }
        
        upstream_request.set_uri(Uri::try_from(path_and_query)
            .map_err(|e| Error::explain(ErrorType::InternalError, format!("Invalid URI: {}", e)))?)?;
        
        // 设置Host头
        let host_value = if let Some(port) = url.port() {
            format!("{}:{}", url.host_str().unwrap_or("localhost"), port)
        } else {
            url.host_str().unwrap_or("localhost").to_string()
        };
        upstream_request.insert_header("Host", &host_value)?;
        
        // 反盗链头(surrit/fourhoi)
        if ctx.needs_antileech_headers() {
            upstream_request.insert_header("Referer", REFERER_VALUE)?;
            upstream_request.insert_header("User-Agent", USER_AGENT)?;
            upstream_request.insert_header("Accept", "*/*")?;
        }
        
        // m3u8需要禁用压缩以便改写
        if ctx.is_m3u8 {
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
        
        // 只处理AntiLeech模式的重定向
        if matches!(ctx.mode, ProxyMode::AntiLeech) && (status == 301 || status == 302 || status == 307 || status == 308) {
            if let Some(loc) = upstream_response.headers.get("location") {
                if let Ok(loc_str) = loc.to_str() {
                    // 使用?url=模式重定向，保持AntiLeech模式
                    let new_loc = self.get_antileech_url(loc_str);
                    upstream_response.insert_header("Location", &new_loc)?;
                    info!("Redirect (AntiLeech): {} -> {}", loc_str, new_loc);
                }
            }
        }
        
        // m3u8改写需要移除Content-Length
        if ctx.is_m3u8 {
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
        if !ctx.is_m3u8 || body.is_none() {
            return Ok(None);
        }
        
        let bytes = body.as_ref().unwrap();
        let content = match std::str::from_utf8(bytes) {
            Ok(s) => s,
            Err(_) => return Ok(None),
        };
        
        match ctx.mode {
            // 116.199 IPTV: 改写为 /proxy/ 模式
            ProxyMode::IptvDirect if ctx.is_iptv_source() => {
                if self.iptv_finder.find(bytes).is_some() {
                    let modified = content.replace(
                        "http://116.199.", 
                        &self.get_proxy_url("")
                    );
                    *body = Some(Bytes::from(modified));
                    debug!("IPTV m3u8 rewritten");
                }
            }
            
            // surrit/fourhoi: 改写为 /iptv/ 或 ?url= 模式
            ProxyMode::IptvDirect if ctx.needs_antileech_headers() => {
                let mut new_content = String::new();
                let base_url = ctx.base_url.as_ref().unwrap();
                
                for line in content.lines() {
                    if line.starts_with('#') || line.trim().is_empty() {
                        new_content.push_str(line);
                    } else {
                        let full_url = if line.starts_with("http") {
                            line.to_string()
                        } else {
                            format!("{}{}", base_url, line)
                        };
                        
                        // .jpeg片段: 用/iptv/模式，改为.ts并加real_ext参数
                        if full_url.ends_with(".jpeg") {
                            let ts_url = full_url.replace(".jpeg", ".ts");
                            let sep = if ts_url.contains('?') { "&" } else { "?" };
                            new_content.push_str(&format!("http://{}:{}/iptv/{}{}real_ext=jpeg",
                                self.config.local_ip, self.config.bind_port, ts_url, sep));
                        } else {
                            // .m3u8等: 用?url=模式
                            new_content.push_str(&self.get_antileech_url(&full_url));
                        }
                    }
                    new_content.push('\n');
                }
                *body = Some(Bytes::from(new_content));
                debug!("AntiLeech m3u8 rewritten");
            }
            
            // AntiLeech模式的m3u8改写
            ProxyMode::AntiLeech => {
                let mut new_content = String::new();
                let base_url = ctx.base_url.as_ref().unwrap();
                
                for line in content.lines() {
                    if line.starts_with('#') || line.trim().is_empty() {
                        new_content.push_str(line);
                    } else {
                        let full_url = if line.starts_with("http") {
                            line.to_string()
                        } else {
                            format!("{}{}", base_url, line)
                        };
                        
                        if full_url.ends_with(".jpeg") {
                            let ts_url = full_url.replace(".jpeg", ".ts");
                            let sep = if ts_url.contains('?') { "&" } else { "?" };
                            new_content.push_str(&format!("http://{}:{}/iptv/{}{}real_ext=jpeg",
                                self.config.local_ip, self.config.bind_port, ts_url, sep));
                        } else {
                            new_content.push_str(&self.get_antileech_url(&full_url));
                        }
                    }
                    new_content.push('\n');
                }
                *body = Some(Bytes::from(new_content));
                debug!("AntiLeech m3u8 rewritten");
            }
            
            _ => {}
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
    info!("IPTV Proxy Server v2.0.0");
    info!("Target: IPQ60xx 1GB RAM");
    info!("========================================");

    let local_ip = std::env::var("LOCAL_IP").unwrap_or_else(|_| "192.168.1.3".to_string());
    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let workers = std::env::var("WORKERS").ok().and_then(|s| s.parse().ok()).unwrap_or_else(|| num_cpus::get());
    let bind_port: u16 = bind_addr.split(':').nth(1).and_then(|p| p.parse().ok()).unwrap_or(8080);

    info!("Local IP: {}", local_ip);
    info!("Bind Address: {}", bind_addr);
    info!("Workers: {}", workers);

    let config = ProxyConfig { local_ip, bind_port };
    let mut server = Server::new(Some(Opt {
        upgrade: false, daemon: false, nocapture: false, test: false, conf: None,
    })).expect("Failed to create server");
    server.bootstrap();

    let mut proxy_service = http_proxy_service(&server.configuration, IptvProxy::new(config));
    proxy_service.add_tcp(&bind_addr);
    server.add_service(proxy_service);

    info!("========================================");
    info!("Server listening on: {}", bind_addr);
    info!("URL Formats:");
    info!("  IPTV:      /iptv/http://116.199.x.x:port/path");
    info!("  TS Proxy:  /proxy/116.199.x.x:port/path");
    info!("  AntiLeech: /?url=<encoded_url>");
    info!("  Health:    /health");
    info!("========================================");

    server.run_forever();
}
