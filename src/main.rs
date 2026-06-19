use async_trait::async_trait;
use bytes::Bytes;
use http::Uri;
use log::{error, info};
use pingora::http::{RequestHeader, ResponseHeader};
use pingora::prelude::*;
use pingora::proxy::http_proxy_service;
use pingora::proxy::{ProxyHttp, Session};
use pingora::server::configuration::Opt;
use pingora::server::Server;
use std::time::Duration;

const REFERER_VALUE: &str = "https://missav.ws/dm242/cn";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

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

pub struct ProxyContext {
    target_url: Option<url::Url>,
    is_m3u8: bool,
    base_url: Option<String>,      // 目录，如 http://example.com:8080/path/to/
    origin_base: Option<String>,   // scheme://host:port，如 http://example.com:8080
    needs_jpeg_fix: bool,
}

impl ProxyContext {
    fn new() -> Self {
        Self {
            target_url: None,
            is_m3u8: false,
            base_url: None,
            origin_base: None,
            needs_jpeg_fix: false,
        }
    }
}

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

    async fn request_filter(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<bool> {
        let req = session.req_header();
        let path = req.uri.path();
        let query = req.uri.query().unwrap_or("");

        // 健康检查
        if path == "/health" || (path == "/" && query.is_empty()) {
            let resp = ResponseHeader::build(200, None)?;
            session.write_response_header(Box::new(resp), false).await?;
            session.write_response_body(Some(Bytes::from("OK")), true).await?;
            return Ok(true);
        }

        if path == "/favicon.ico" {
            let resp = ResponseHeader::build(404, None)?;
            session.write_response_header(Box::new(resp), true).await?;
            return Ok(true);
        }

        // 解析 ?url=
        if let Some(url_param) = query.split('&').find(|p| p.starts_with("url=")) {
            let encoded = &url_param[4..];
            let decoded = urlencoding::decode(encoded)
                .map_err(|e| Error::explain(ErrorType::HTTPStatus(400), format!("decode: {e}")))?;
            let decoded_str = decoded.to_string();
            info!("Decoded URL: {}", decoded_str);

            let mut url = url::Url::parse(&decoded_str)
                .map_err(|e| Error::explain(ErrorType::HTTPStatus(400), format!("invalid URL: {e}")))?;

            // 处理 jpeg 伪装
            if url.query_pairs().any(|(k, v)| k == "real_ext" && v == "jpeg") {
                ctx.needs_jpeg_fix = true;
                let clean: Vec<_> = url
                    .query_pairs()
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

            // 保存 scheme + authority (host:port)
            let authority = url.authority().to_string();
            ctx.origin_base = Some(format!("{}://{}", url.scheme(), authority));

            // 正确截取目录前缀（基于 URL path，避免 :// 干扰）
            let path = url.path();
            let base_path = match path.rfind('/') {
                Some(pos) => &path[..=pos],
                None => "/",
            };
            ctx.base_url = Some(format!("{}://{}{}", url.scheme(), authority, base_path));

            // 修改请求路径和 Host（为上游连接做准备）
            let path_bytes = url.path().as_bytes().to_vec();
            session.req_header_mut().set_raw_path(&path_bytes)?;
            session.req_header_mut().insert_header("Host", &authority)?;
            return Ok(false);
        }

        Err(Error::explain(ErrorType::HTTPStatus(400), "Use /?url=<encoded_target>"))
    }

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

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream_request: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        let url = ctx.target_url.as_ref()
            .ok_or_else(|| Error::explain(ErrorType::InternalError, "No target"))?;

        let mut path_and_query = match url.query() {
            Some(q) => format!("{}?{}", url.path(), q),
            None => url.path().to_string(),
        };
        if ctx.needs_jpeg_fix {
            path_and_query = path_and_query.replace(".ts", ".jpeg");
        }
        let uri = Uri::try_from(path_and_query)
            .map_err(|e| Error::explain(ErrorType::InternalError, format!("URI: {e}")))?;
        upstream_request.set_uri(uri);

        // 使用 authority 设置 Host（含端口）
        upstream_request.insert_header("Host", url.authority())?;

        upstream_request.insert_header("Referer", REFERER_VALUE)?;
        upstream_request.insert_header("User-Agent", USER_AGENT)?;
        upstream_request.insert_header("Accept", "*/*")?;

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

        // 重定向改写
        if status == 301 || status == 302 || status == 307 || status == 308 {
            if let Some(loc) = upstream_response.headers.get("location")
                .and_then(|v| v.to_str().ok())
            {
                let loc_str = loc.to_string();
                let resolved = if let Ok(abs_url) = url::Url::parse(&loc_str) {
                    abs_url.to_string()
                } else if let Some(base) = &ctx.target_url {
                    base.join(&loc_str).map(|u| u.to_string()).unwrap_or_else(|_| loc_str.clone())
                } else {
                    loc_str.clone()
                };
                let new_loc = format!("/?url={}", urlencoding::encode(&resolved));
                upstream_response.insert_header("Location", &new_loc)?;
                info!("Rewritten redirect: {} -> {}", loc_str, new_loc);
            }
        }

        // 如果是 jpeg 伪装资源，修正 Content-Type 为 TS 视频流
        if ctx.needs_jpeg_fix {
            upstream_response.remove_header("Content-Type");
            upstream_response.insert_header("Content-Type", "video/mp2t")?;
            upstream_response.remove_header("Content-Disposition");
        }

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
        if let Some(bytes) = body.as_mut() {
            if ctx.is_m3u8 {
                let content = match std::str::from_utf8(bytes) {
                    Ok(s) => s.to_string(),
                    Err(_) => return Ok(None),
                };
                let base = ctx.base_url.as_ref()
                    .expect("base_url missing for m3u8 rewrite");
                let origin_base = ctx.origin_base.as_ref()
                    .expect("origin_base missing for m3u8 rewrite");
                let mut new_content = String::with_capacity(content.len());

                for line in content.lines() {
                    if line.starts_with('#') || line.trim().is_empty() {
                        new_content.push_str(line);
                        new_content.push('\n');
                    } else {
                        let full_url = if line.starts_with("http://") || line.starts_with("https://") {
                            line.to_string()
                        } else if line.starts_with('/') {
                            format!("{}{}", origin_base, line)
                        } else {
                            format!("{}{}", base, line)
                        };

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

    // 解析命令行参数：-Li <local_ip>
    let mut args = std::env::args().skip(1);
    let mut local_ip = None;
    while let Some(arg) = args.next() {
        if arg == "-Li" {
            local_ip = args.next();
            break;
        }
    }
    // 优先级：命令行 > 环境变量 LOCAL_IP > 默认值
    let local_ip = local_ip
        .or_else(|| std::env::var("LOCAL_IP").ok())
        .unwrap_or_else(|| "192.168.1.3".to_string());

    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let bind_port: u16 = bind_addr
        .split(':')
        .nth(1)
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);

    info!("========================================");
    info!("IPTV Proxy (unified ?url= mode)");
    info!("========================================");
    info!("Local IP: {}", local_ip);
    info!("Bind: {}", bind_addr);

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
