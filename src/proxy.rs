use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use http_body_util::combinators::BoxBody;
use hyper::body::Incoming;
use hyper::client::conn::http1 as http1_client;
use hyper::server::conn::http1 as http1_server;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, error, info, warn};
use crate::config::ServiceConfig;
use crate::error::ProxyError;

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type RespBody = BoxBody<Bytes, BoxError>;

fn empty_body() -> RespBody {
    Empty::<Bytes>::new()
        .map_err(|e| -> BoxError { Box::new(e) })
        .boxed()
}

fn error_response(status: StatusCode, msg: &str) -> Response<RespBody> {
    use http_body_util::Full;
    Response::builder()
        .status(status)
        .body(
            Full::new(Bytes::from(msg.to_string()))
                .map_err(|e| -> BoxError { Box::new(e) })
                .boxed()
        )
        .unwrap()
}

/// Parse "host:port" into (host, port).
fn parse_host_port(host_port: &str) -> Result<(String, u16), ProxyError> {
    // Handle IPv6 [::1]:443
    if host_port.starts_with('[') {
        if let Some(bracket_end) = host_port.find(']') {
            let host = host_port[1..bracket_end].to_string();
            let port_str = &host_port[bracket_end + 1..];
            let port_str = port_str.trim_start_matches(':');
            let port = port_str.parse::<u16>().map_err(|_| ProxyError::BadRequest(format!("Invalid port in {}", host_port)))?;
            return Ok((host, port));
        }
    }
    let mut parts = host_port.rsplitn(2, ':');
    let port = parts.next()
        .and_then(|p| p.parse::<u16>().ok())
        .ok_or_else(|| ProxyError::BadRequest(format!("Invalid host:port '{}'", host_port)))?;
    let host = parts.next()
        .ok_or_else(|| ProxyError::BadRequest(format!("Missing host in '{}'", host_port)))?
        .to_string();
    Ok((host, port))
}

/// Parse proxy URL like "http://host:port" into (host, port).
fn parse_proxy_url(url: &str) -> Result<(String, u16), ProxyError> {
    let url = url.trim_start_matches("http://").trim_start_matches("https://");
    parse_host_port(url)
}

/// Resolve a hostname/IP string to a list of IP addresses.
async fn resolve_host(host: &str) -> Vec<IpAddr> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return vec![ip];
    }
    match tokio::net::lookup_host(format!("{}:0", host)).await {
        Ok(addrs) => addrs.map(|a| a.ip()).collect(),
        Err(e) => {
            warn!("DNS resolution failed for {}: {}", host, e);
            vec![]
        }
    }
}

/// Check if a host resolves to an IP in any of the configured local ranges.
async fn is_in_local_range(host: &str, ranges: &[ipnet::IpNet]) -> bool {
    let ips = resolve_host(host).await;
    for ip in &ips {
        for range in ranges {
            if range.contains(ip) {
                return true;
            }
        }
    }
    false
}

pub async fn run_service(config: ServiceConfig) -> Result<(), BoxError> {
    let listener = TcpListener::bind(&config.listen).await
        .map_err(|e| format!("Failed to bind {}: {}", config.listen, e))?;
    info!("Proxy service listening on {} (upstream: {})", config.listen, config.upstream_proxy);

    let config = Arc::new(config);

    loop {
        let (stream, peer_addr) = listener.accept().await?;
        let config = config.clone();

        tokio::spawn(async move {
            let io = TokioIo::new(stream);
            let result = http1_server::Builder::new()
                .preserve_header_case(true)
                .serve_connection(
                    io,
                    service_fn(move |req| handle_request(req, config.clone(), peer_addr)),
                )
                .with_upgrades()
                .await;

            if let Err(e) = result {
                debug!("Connection from {} closed: {}", peer_addr, e);
            }
        });
    }
}

async fn handle_request(
    req: Request<Incoming>,
    config: Arc<ServiceConfig>,
    peer_addr: SocketAddr,
) -> Result<Response<RespBody>, BoxError> {
    let method = req.method().clone();
    let uri = req.uri().clone();
    info!(peer = %peer_addr, method = %method, uri = %uri, "Received request");

    if method == Method::CONNECT {
        handle_connect(req, config, peer_addr).await
    } else {
        handle_http(req, config, peer_addr).await
    }
}

/// Handle HTTP CONNECT tunnel requests.
async fn handle_connect(
    req: Request<Incoming>,
    config: Arc<ServiceConfig>,
    peer_addr: SocketAddr,
) -> Result<Response<RespBody>, BoxError> {
    let host_port = req.uri().authority()
        .map(|a| a.as_str().to_string())
        .ok_or_else(|| -> BoxError { Box::new(ProxyError::BadRequest("CONNECT missing authority".into())) })?;

    let (host, port) = parse_host_port(&host_port)
        .map_err(|e| -> BoxError { Box::new(e) })?;

    let local = is_in_local_range(&host, &config.local_ranges).await;
    let upstream_proxy = config.upstream_proxy.clone();

    tokio::spawn(async move {
        match hyper::upgrade::on(req).await {
            Ok(upgraded) => {
                let mut client_io = TokioIo::new(upgraded);

                if local {
                    info!(peer = %peer_addr, target = %host_port, "CONNECT: routing directly");
                    match TcpStream::connect(format!("{}:{}", host, port)).await {
                        Ok(mut target) => {
                            if let Err(e) = tokio::io::copy_bidirectional(&mut client_io, &mut target).await {
                                debug!("CONNECT tunnel error for {}: {}", host_port, e);
                            }
                        }
                        Err(e) => error!("CONNECT direct connect failed for {}: {}", host_port, e),
                    }
                } else {
                    info!(peer = %peer_addr, target = %host_port, proxy = %upstream_proxy, "CONNECT: routing via upstream proxy");
                    match connect_upstream_tunnel(&host, port, &upstream_proxy).await {
                        Ok(mut upstream_io) => {
                            if let Err(e) = tokio::io::copy_bidirectional(&mut client_io, &mut upstream_io).await {
                                debug!("CONNECT tunnel error for {} via proxy: {}", host_port, e);
                            }
                        }
                        Err(e) => error!("CONNECT via upstream proxy failed for {}: {}", host_port, e),
                    }
                }
            }
            Err(e) => error!("Upgrade error for {}: {}", host_port, e),
        }
    });

    Ok(Response::builder()
        .status(StatusCode::OK)
        .body(empty_body())
        .unwrap())
}

/// Connect to the upstream proxy and perform CONNECT tunneling; returns the tunneled stream.
async fn connect_upstream_tunnel(host: &str, port: u16, upstream_proxy: &str) -> Result<TokioIo<hyper::upgrade::Upgraded>, BoxError> {
    let (proxy_host, proxy_port) = parse_proxy_url(upstream_proxy)
        .map_err(|e| -> BoxError { Box::new(e) })?;

    let stream = TcpStream::connect(format!("{}:{}", proxy_host, proxy_port)).await?;
    let io = TokioIo::new(stream);

    let (mut sender, conn) = http1_client::Builder::new()
        .handshake(io)
        .await?;

    tokio::spawn(async move {
        if let Err(e) = conn.with_upgrades().await {
            debug!("Upstream proxy connection driver error: {}", e);
        }
    });

    let connect_req = Request::builder()
        .method(Method::CONNECT)
        .uri(format!("{}:{}", host, port))
        .header(hyper::header::HOST, format!("{}:{}", host, port))
        .header("proxy-connection", "keep-alive")
        .body(Empty::<Bytes>::new())
        .map_err(|e| -> BoxError { Box::new(e) })?;

    let resp = sender.send_request(connect_req).await?;

    if resp.status() != StatusCode::OK {
        return Err(format!("Upstream proxy CONNECT failed: {}", resp.status()).into());
    }

    let upgraded = hyper::upgrade::on(resp).await?;
    Ok(TokioIo::new(upgraded))
}

/// Handle plain HTTP proxy requests (non-CONNECT).
async fn handle_http(
    req: Request<Incoming>,
    config: Arc<ServiceConfig>,
    peer_addr: SocketAddr,
) -> Result<Response<RespBody>, BoxError> {
    let uri = req.uri().clone();

    let host = match uri.host() {
        Some(h) => h.to_string(),
        None => return Ok(error_response(StatusCode::BAD_REQUEST, "Missing host in request URI")),
    };
    let port = uri.port_u16().unwrap_or(80);

    let local = is_in_local_range(&host, &config.local_ranges).await;

    if local {
        info!(peer = %peer_addr, target = %uri, "HTTP: routing directly");
        forward_http_direct(req, &host, port).await
    } else {
        info!(peer = %peer_addr, target = %uri, proxy = %config.upstream_proxy, "HTTP: routing via upstream proxy");
        forward_http_via_proxy(req, &config.upstream_proxy).await
    }
}

/// Forward an HTTP request directly to the target server.
async fn forward_http_direct(
    req: Request<Incoming>,
    host: &str,
    port: u16,
) -> Result<Response<RespBody>, BoxError> {
    let stream = TcpStream::connect(format!("{}:{}", host, port)).await?;
    let io = TokioIo::new(stream);

    let (mut sender, conn) = http1_client::Builder::new()
        .preserve_header_case(true)
        .handshake(io)
        .await?;

    tokio::spawn(async move {
        if let Err(e) = conn.await {
            debug!("HTTP client connection error: {}", e);
        }
    });

    let (parts, body) = req.into_parts();
    let path_and_query = parts.uri.path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");

    let mut builder = Request::builder()
        .method(parts.method)
        .uri(path_and_query)
        .version(hyper::Version::HTTP_11);

    let headers = builder.headers_mut().unwrap();
    for (name, value) in &parts.headers {
        let lower = name.as_str().to_ascii_lowercase();
        if lower == "proxy-authorization" || lower == "proxy-connection" || lower == "proxy-authenticate" {
            continue;
        }
        headers.insert(name.clone(), value.clone());
    }

    let new_req = builder.body(body)
        .map_err(|e| -> BoxError { Box::new(e) })?;

    let resp = sender.send_request(new_req).await?;
    let (parts, body) = resp.into_parts();
    Ok(Response::from_parts(
        parts,
        body.map_err(|e| -> BoxError { Box::new(e) }).boxed(),
    ))
}

/// Forward an HTTP proxy request to the upstream proxy as-is.
async fn forward_http_via_proxy(
    req: Request<Incoming>,
    upstream_proxy: &str,
) -> Result<Response<RespBody>, BoxError> {
    let (proxy_host, proxy_port) = parse_proxy_url(upstream_proxy)
        .map_err(|e| -> BoxError { Box::new(e) })?;

    let stream = TcpStream::connect(format!("{}:{}", proxy_host, proxy_port)).await?;
    let io = TokioIo::new(stream);

    let (mut sender, conn) = http1_client::Builder::new()
        .preserve_header_case(true)
        .handshake(io)
        .await?;

    tokio::spawn(async move {
        if let Err(e) = conn.await {
            debug!("Upstream proxy HTTP connection error: {}", e);
        }
    });

    let (parts, body) = req.into_parts();
    let new_req = Request::from_parts(parts, body);

    let resp = sender.send_request(new_req).await?;
    let (parts, body) = resp.into_parts();
    Ok(Response::from_parts(
        parts,
        body.map_err(|e| -> BoxError { Box::new(e) }).boxed(),
    ))
}
