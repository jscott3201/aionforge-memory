//! MCP server command execution.

use std::convert::Infallible;
use std::io::Write;
use std::sync::Arc;

use aionforge_mcp::{
    AionforgeStreamableHttpService, STREAMABLE_HTTP_ENDPOINT, StreamableHttpOptions, serve_stdio,
    streamable_http_service,
};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as AutoHttpBuilder;
use tokio::net::TcpListener;
use tower_service::Service;

use crate::cli::{ServeArgs, ServeTransport};
use crate::error::CliError;
use crate::host::{
    HostOptions, RuntimeEmbedder, StartupEmbedderStatus, check_startup_embedder, load_config,
    open_memory, render_startup_embedder_status,
};

type HttpResponse = Response<BoxBody<Bytes, Infallible>>;

pub(crate) async fn run(options: &HostOptions, args: ServeArgs) -> Result<(), CliError> {
    let config = load_config(options)?;
    let memory = open_memory(&config)?;
    match args.transport {
        ServeTransport::Stdio => {
            let startup = check_startup_embedder(memory.as_ref()).await?;
            report_startup_embedder(&startup);
            serve_stdio(memory)
                .await
                .map_err(|error| CliError::Serve(error.to_string()))
        }
        ServeTransport::Http => serve_http(memory, args).await,
    }
}

async fn serve_http(
    memory: Arc<aionforge::Memory<RuntimeEmbedder>>,
    args: ServeArgs,
) -> Result<(), CliError> {
    let mut options = StreamableHttpOptions::default()
        .with_stateful_mode(!args.stateless)
        .with_json_response(args.json_response);
    if let Some(max_request_body_bytes) = args.max_request_body_bytes {
        options = options.with_max_request_body_bytes(max_request_body_bytes);
    }
    if !args.allowed_hosts.is_empty() {
        options = options.with_allowed_hosts(args.allowed_hosts);
    }
    if !args.allowed_origins.is_empty() {
        options = options.with_allowed_origins(args.allowed_origins);
    }

    let startup = check_startup_embedder(memory.as_ref()).await?;
    report_startup_embedder(&startup);
    let service = streamable_http_service(memory, options)?;
    let service = HttpMcpRouter { inner: service };

    let listener = TcpListener::bind(args.listen).await?;
    let builder = AutoHttpBuilder::new(TokioExecutor::new());
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let io = TokioIo::new(stream);
                let builder = builder.clone();
                let service = service.clone();
                tokio::spawn(async move {
                    let hyper_service = service_fn(move |request| {
                        let mut service = service.clone();
                        async move { service.call(request).await }
                    });
                    let _ = builder.serve_connection(io, hyper_service).await;
                });
            }
            interrupted = tokio::signal::ctrl_c() => {
                interrupted?;
                break;
            }
        }
    }
    Ok(())
}

fn report_startup_embedder(status: &StartupEmbedderStatus) {
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(
        stderr,
        "aionforge serve: {}",
        render_startup_embedder_status(status),
    );
}

#[derive(Clone)]
struct HttpMcpRouter {
    inner: AionforgeStreamableHttpService<RuntimeEmbedder>,
}

impl HttpMcpRouter {
    async fn call(&mut self, request: Request<Incoming>) -> Result<HttpResponse, Infallible> {
        if request.uri().path() != STREAMABLE_HTTP_ENDPOINT {
            return Ok(not_found_response());
        }
        self.inner.call(request).await
    }
}

fn not_found_response() -> HttpResponse {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Full::new(Bytes::from_static(b"Not Found")).boxed())
        .expect("valid not found response")
}
