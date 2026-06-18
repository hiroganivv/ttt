use async_trait::async_trait;
use bytes::Bytes;
use http::Uri;
use log::{debug, error, info, warn};
use pingora::http::{RequestHeader, ResponseHeader};
use pingora::prelude::*;
use pingora::proxy::http_proxy_service;
use pingora::proxy::{ProxyHttp, Session};
use pingora::server::configuration::Opt;
use pingora::server::Server;
use std::time::Duration;

// ━━━━━━━━━━━━ 常量 ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

const REFERER_VALUE: &str = "https://missav.ws/dm242/cn";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

// ━━━━━━━━━━━━ 配置 ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[derive(Clone)]
pub struct ProxyConfig {
    pub local_ip: String,
    pub bind_port: u16,
}

impl ProxyConfig {
    pub fn new(local_ip: String, bind_port: u16) -> Self {
        Self { local_ip, bind_port }
    }
}

// ━━━━━━━━━━━━ 请求上下文 ━━━━━━━━━━━━━━━━━━━━━━━━━

pub struct ProxyContext {
    target_url: Option<url::Url>,   // 真实目标 URL
    is_m3u8: bool,                  // 是否需要重写播放列表内容
    base_url: Option<String>,       // m3u8 所在目录（用于拼接相对路径）
    needs_jpeg_fix: bool,           // 是否把 .ts 改为 .jpeg（反盗链特殊要求）
}

impl ProxyContext {
    fn new() -> Self {
        Self {
            target_url: None,
            is_m3u8: false,
            base_url: None,
            needs_jpeg_fix: false,
        }
    }
}

// ━━━━━━━━━━━━ 代理主体 ━━━━━━━━━━━━━━━━━━━━━━━━━━━

pub struct IptvProxy {
    config: ProxyConfig,
}

impl IptvProxy {
    pub fn new(config: ProxyConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl ProxyHttp for IptvProxy {
    type CTX = ProxyContext;

    fn new_ctx(&self) -> Self::CTX {
        ProxyContext::new()
    }

    // ---------- 请求过滤：只保留 /?url= 入口 ----------
    async fn request_filter(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<bool> {
        let req = session.req_header();
        let path = req.uri.path();
        let query = req.uri.query().unwrap_or("");

        // 健康检查（无参数访问 / 时返回 OK）
        if path == "/health" || (path == "/" && query.is_empty()) {
            let resp = ResponseHeader::build(200, None)
                .map_err(|e| Error::explain(ErrorType::InternalError, format!("{e}")))?;
            session.write_response_header(Box::new(resp), false).await?;
            session.write_response_body(Some(Bytes::from("OK")), true).await?;
            return Ok(true);
        }

        // 忽略 favicon 请求
        if path == "/favicon.ico" {
            let resp = ResponseHeader::build(404, None)
                .map_err(|e| Error::explain(ErrorType::InternalError, format!("{e}")))?;
            session.write_response_header(Box::new(resp), true).await?;
            return Ok(true);
        }

        // 唯一业务入口：?url=<原始或编码后的目标 URL>
        if let Some(url_param) = query.split('&').find(|p| p.starts_with("url=")) {
            let encoded = &url_param[4..];
            // 解码（兼容未编码的原始 URL）
            let decoded = urlencoding::decode(encoded)
                .map_err(|e| Error::explain(ErrorType::HTTPStatus(400), format!("decode: {e}")))?;
            let decoded_str = decoded.to_string();
            info!("Decoded URL: {}", decoded_str);

            let mut url = url::Url::parse(&decoded_str)
                .map_err(|e| Error::explain(ErrorType::HTTPStatus(400), format!("invalid URL: {e}")))?;

            // 如果请求带了 real_ext=jpeg，标记并将该参数从请求中移除
            if url.query_pairs().any(|(k, v)| k == "real_ext" && v == "jpeg") {
                ctx.needs_jpeg_fix = true;
                let clean: Vec<_> = url.query_pairs()
                    .filter(|(k, _)| k != "real_ext")
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect();
                url.query_pairs_mut().clear();
                for (k, v) in clean {
                    url.query_pairs_mut().append_pair(&k, &v);
                }
            }

            ctx.target_url = Some(url.clone());
            ctx.is_m3u8 = decoded_str.ends_with(".m3u8");

            // 计算当前 m3u8 所在目录（用于后续拼接相对路径）
            if let Some(last_slash) = decoded_str.rfind('/') {
                ctx.base_url = Some(decoded_str[..=last_slash].to_string());
            } else {
                ctx.base_url = Some(decoded_str.clone());
            }

            // 修改代理请求的路径，使之与上游一致
            let path_bytes = url.path().as_bytes().to_vec();
            session.req_header_mut().set_raw_path(&path_bytes)?;
            session.req_header_mut().insert_header("Host", url.host_str().unwrap_or("localhost"))?;
            return Ok(false);
        }

        // 其他路径一律拒绝
        Err(Error::explain(ErrorType::HTTPStatus(400), "Use /?url=<encoded_target>"))
    }

    // ---------- 选择上游对等节点 ----------
    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let target = ctx.target_url.as_ref()
            .ok_or_else(|| Error::explain(ErrorType::InternalError, "No target URL"))?;

        let host = target.host_str().unwrap_or("localhost").to_string();
        let port = target.port().unwrap_or(if target.scheme() == "https" { 443 } else { 80 });
        let is_https = target.scheme() == "https";

        info!("Upstream: {}:{} (TLS: {})", host, port, is_https);
        Ok(Box::new(HttpPeer::new((host.clone(), port), is_https, host)))
    }

    // ---------- 修改发往上游的请求 ----------
    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream_request: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        let url = ctx.target_url.as_ref()
            .ok_or_else(|| Error::explain(ErrorType::InternalError, "No target"))?;

        // 构建正确的路径与查询参数（需要时把 .ts 改成 .jpeg）
        let mut path_and_query = match url.query() {
            Some(q) => format!("{}?{}", url.path(), q),
            None => url.path().to_string(),
        };
        if ctx.needs_jpeg_fix {
            path_and_query = path_and_query.replace(".ts", ".jpeg");
        }
        upstream_request.set_uri(Uri::try_from(path_and_query)
            .map_err(|e| Error::explain(ErrorType::InternalError, format!("URI: {e}")))?);

        // 设置 Host 头
        let host_value = match url.port() {
            Some(port) => format!("{}:{}", url.host_str().unwrap(), port),
            None => url.host_str().unwrap().to_string(),
        };
        upstream_request.insert_header("Host", &host_value)?;

        // ⭐ 关键：所有请求强制添加反盗链头
        upstream_request.insert_header("Referer", REFERER_VALUE)?;
        upstream_request.insert_header("User-Agent", USER_AGENT)?;
        upstream_request.insert_header("Accept", "*/*")?;

        // 如果要重写 m3u8 内容，先移除压缩，避免缓冲问题
        if ctx.is_m3u8 {
            upstream_request.remove_header("Accept-Encoding");
        }

        Ok(())
    }

    // ---------- 处理上游响应头（重定向改写）----------
    async fn response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        let status = upstream_response.status;
        if status == 301 || status == 302 || status == 307 || status == 308 {
            if let Some(loc) = upstream_response.headers.get("location")
                .and_then(|v| v.to_str().ok())
            {
                let loc_str = loc.to_string();
                // 把所有重定向目标编码回代理地址，客户端会自动跟随
                let new_loc = format!("/?url={}", urlencoding::encode(&loc_str));
                upstream_response.insert_header("Location", &new_loc)?;
                info!("Rewritten redirect: {} -> {}", loc_str, new_loc);
            }
        }

        // 如果是 m3u8 且即将重写内容，去掉 Content-Length，让 pingora 自动计算
        if ctx.is_m3u8 {
            upstream_response.remove_header("Content-Length");
        }

        Ok(())
    }

    // ---------- 处理响应体（m3u8 播放列表重写）----------
    fn response_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        _end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> Result<Option<Duration>> {
        if let Some(bytes) = body.as_mut() {
            if ctx.is_m3u8 {
                if let Ok(content) = std::str::from_utf8(bytes) {
                    let base = ctx.base_url.as_ref()
                        .expect("base_url missing for m3u8 rewrite");
                    let mut new_content = String::with_capacity(content.len());

                    for line in content.lines() {
                        if line.starts_with('#') || line.trim().is_empty() {
                            // 注释或空行保留原样
                            new_content.push_str(line);
                            new_content.push('\n');
                        } else {
                            // 资源行：先拼成完整 URL
                            let full_url = if line.starts_with("http://") || line.starts_with("https://") {
                                line.to_string()
                            } else {
                                format!("{}{}", base, line)
                            };

                            // JPEG 特殊处理：补上 real_ext=jpeg 参数（如果原始是 .jpeg）
                            if full_url.ends_with(".jpeg") {
                                let ts_url = full_url.replace(".jpeg", ".ts");
                                let sep = if ts_url.contains('?') { "&" } else { "?" };
                                let fixed = format!("{}{}real_ext=jpeg", ts_url, sep);
                                let encoded = urlencoding::encode(&fixed);
                                new_content.push_str(&format!(
                                    "http://{}:{}/?url={}\n",
                                    self.config.local_ip, self.config.bind_port, encoded
                                ));
                            } else {
                                // 普通资源：编码后指向代理
                                let encoded = urlencoding::encode(&full_url);
                                new_content.push_str(&format!(
                                    "http://{}:{}/?url={}\n",
                                    self.config.local_ip, self.config.bind_port, encoded
                                ));
                            }
                        }
                    }

                    *bytes = Bytes::from(new_content);
                    info!("M3U8 rewritten ({} lines)", content.lines().count());
                }
            }
        }
        Ok(None)
    }

    // ---------- 访问日志 ----------
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

// ━━━━━━━━━━━━ 主函数 ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    info!("========================================");
    info!("IPTV Proxy (unified ?url= mode)");
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
    info!("Usage: http://<ip>:8080/?url=<target URL>");
    info!("========================================");

    server.run_forever();
}
