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
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error, info};
use crate::config::ServiceConfig;
use crate::error::ProxyError;
use crate::tls;

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

fn parse_host_port(host_port: &str) -> Result<(String, u16), ProxyError> {
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

fn parse_proxy_url(url: &str) -> Result<(String, u16), ProxyError> {
    let url = url.trim_start_matches("http://").trim_start_matches("https://");
    parse_host_port(url)
}

fn is_ip_in_local_range(host: &str, ranges: &[ipnet::IpNet]) -> bool {
    match host.parse::<IpAddr>() {
        Ok(ip) => ranges.iter().any(|r| r.contains(&ip)),
        Err(_) => false,
    }
}

fn maybe_map_v4(ip: &IpAddr) -> IpAddr {
    if let IpAddr::V6(v6) = ip {
        if let Some(v4) = v6.to_ipv4_mapped() {
            return IpAddr::V4(v4);
        }
    }
    *ip
}

async fn build_acceptor(tls_cfg: &crate::config::TlsConfig) -> std::result::Result<TlsAcceptor, BoxError> {
    let cert_path = tls_cfg.cert_path.as_ref().map(|s| std::path::Path::new(s.as_str()));
    let key_path = tls_cfg.key_path.as_ref().map(|s| std::path::Path::new(s.as_str()));
    if cert_path.is_none() != key_path.is_none() {
        return Err(Box::new(ProxyError::Config(
            "tls.cert_path and tls.key_path must be specified together".into(),
        )));
    }
    let (cert, key) = tls::load_or_generate(cert_path, key_path).await?;
    let acceptor = tls::build_tls_acceptor(&cert, &key)?;
    Ok(acceptor)
}

pub async fn run_service(
    config: ServiceConfig,
    shared_tls: Option<crate::config::TlsConfig>,
) -> Result<(), BoxError> {
    let acceptor = if config.tls {
        match shared_tls {
            Some(ref cfg) => Some(build_acceptor(cfg).await?),
            None => return Err(Box::new(ProxyError::Config(
                "service has tls: true but no top-level tls config block".into(),
            ))),
        }
    } else {
        None
    };

    let listener = TcpListener::bind(&config.listen).await
        .map_err(|e| format!("Failed to bind {}: {}", config.listen, e))?;
    info!(
        "Proxy service listening on {} (upstream: {}, tls: {})",
        config.listen, config.upstream_proxy, config.tls
    );

    let config = Arc::new(config);

    loop {
        let (stream, peer_addr) = listener.accept().await?;
        let config = config.clone();

        let raw_ip = peer_addr.ip();
        let check_ip = maybe_map_v4(&raw_ip);
        let excluded = config.tls_exclude.iter().any(|r| r.contains(&check_ip));
        let use_tls = config.tls && !excluded;

        info!(
            "Accept from {} (raw: {}, check: {}, excluded: {}, tls: {})",
            peer_addr, raw_ip, check_ip, excluded, use_tls
        );

        if use_tls {
            let acceptor = acceptor.clone().unwrap();
            tokio::spawn(async move {
                let tls_stream = match acceptor.accept(stream).await {
                    Ok(s) => s,
                    Err(e) => {
                        debug!("TLS accept error from {}: {}", peer_addr, e);
                        return;
                    }
                };
                let io = TokioIo::new(tls_stream);
                let result = http1_server::Builder::new()
                    .preserve_header_case(true)
                    .serve_connection(
                        io,
                        service_fn(move |req| handle_request(req, config.clone(), peer_addr)),
                    )
                    .with_upgrades()
                    .await;

                if let Err(e) = result {
                    debug!("TLS connection from {} closed: {}", peer_addr, e);
                }
            });
        } else {
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

    let local = is_ip_in_local_range(&host, &config.local_ranges);
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
        return Err(Box::new(ProxyError::UpstreamProxy(
            format!("CONNECT to {}:{} failed with status {}", host, port, resp.status())
        )));
    }

    let upgraded = hyper::upgrade::on(resp).await?;
    Ok(TokioIo::new(upgraded))
}

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

    let local = is_ip_in_local_range(&host, &config.local_ranges);

    if local {
        info!(peer = %peer_addr, target = %uri, "HTTP: routing directly");
        forward_http_direct(req, &host, port).await
    } else {
        info!(peer = %peer_addr, target = %uri, proxy = %config.upstream_proxy, "HTTP: routing via upstream proxy");
        forward_http_via_proxy(req, &config.upstream_proxy).await
    }
}

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
        let lower = name.as_str();
        if lower.eq_ignore_ascii_case("proxy-authorization")
            || lower.eq_ignore_ascii_case("proxy-connection")
            || lower.eq_ignore_ascii_case("proxy-authenticate")
        {
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
