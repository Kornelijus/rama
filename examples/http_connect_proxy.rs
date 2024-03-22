//! An example to showcase how one can build an unauthenticated http proxy server.
//!
//! # Run the example
//!
//! ```sh
//! cargo run --example http_connect_proxy
//! ```
//!
//! # Expected output
//!
//! The server will start and listen on `:8080`. You can use `curl` to interact with the service:
//!
//! ```sh
//! curl -v -x http://127.0.0.1:8080 --proxy-user 'john:secret' http://www.example.com/
//! curl -v -x http://127.0.0.1:8080 --proxy-user 'john-cc-us:secret' http://www.example.com/
//! curl -v -x http://127.0.0.1:8080 --proxy-user 'john:secret' https://www.example.com/
//! curl -v -x http://127.0.0.1:8080 --proxy-user 'john:secret' http://echo.example/foo/bar
//! curl -v -x http://127.0.0.1:8080 --proxy-user 'john:secret' -XPOST http://echo.example/lucky/7
//! ```
//! The psuedo API can be used as follows:
//!
//! ```sh
//! curl -v -x http://127.0.0.1:8080 --proxy-user 'john:secret' http://echo.example/foo/bar
//! curl -v -x http://127.0.0.1:8080 --proxy-user 'john-cc-us:secret' http://echo.example/foo/bar
//! ```
//!
//! You should see in all the above examples the responses from the server.
//!
//! If you want to see the HTTP traffic in action you can of course also use telnet instead:
//!
//! ```sh
//! telnet 127.0.0.1:8080
//! ```
//!
//! and then type:
//!
//! ```
//! CONNECT example.com:80 HTTP/1.1
//! Host: example.com:80
//! Proxy-Authorization: basic am9objpzZWNyZXQ=
//!
//!
//! GET / HTTP/1.1
//! HOST: example.com:80
//! Connection: close
//!
//!
//! ```
//!
//! You should see the same response as when running:
//!
//! ```sh
//! curl -v -x http://127.0.0.1:8080 --proxy-user 'john:secret' http://www.example.com/
//! ```

use rama::{
    http::{
        client::HttpClient,
        layer::{
            proxy_auth::ProxyAuthLayer,
            trace::TraceLayer,
            upgrade::{UpgradeLayer, Upgraded},
        },
        matcher::{DomainMatcher, HttpMatcher, MethodMatcher},
        response::Json,
        server::HttpServer,
        service::web::{
            extract::{FromRequestParts, Host, Path},
            match_service,
        },
        Body, IntoResponse, Request, Response, StatusCode,
    },
    proxy::ProxyFilter,
    rt::Executor,
    service::{layer::HijackLayer, service_fn, Context, Service, ServiceBuilder},
    tcp::utils::is_connection_error,
};
use serde::Deserialize;
use serde_json::json;
use std::{convert::Infallible, sync::Arc, time::Duration};
use tracing::level_filters::LevelFilter;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::DEBUG.into())
                .from_env_lossy(),
        )
        .init();

    let graceful = rama::graceful::Shutdown::default();

    #[derive(Deserialize)]
    /// API parameters for the lucky number endpoint
    struct APILuckyParams {
        number: u32,
    }

    // TODO: what about the hop headers?!

    graceful.spawn_task_fn(|guard| async move {
        let exec = Executor::graceful(guard.clone());
        HttpServer::auto(exec)
            .listen_graceful(
                guard,
                "127.0.0.1:8080",
                ServiceBuilder::new()
                    .layer(TraceLayer::new_for_http())
                    // - specify it as `with_proxy_filter_labels::<'_'>()`
                    //   in case you want to define a different separator, such as '_'.
                    // - specify `.with_labels::<T>()` in case you want to use a custom labels extractor.
                    // - `ProxyAuthLayer::new` can be used for a custom Credentials type.
                    .layer(ProxyAuthLayer::basic(("john", "secret")).with_proxy_filter_labels_default())
                    // example of how one might insert an API layer into their proxy
                    .layer(HijackLayer::new(
                        DomainMatcher::new("echo.example"),
                        Arc::new(match_service!{
                            HttpMatcher::post("/lucky/:number") => |path: Path<APILuckyParams>| async move {
                                Json(json!({
                                    "lucky_number": path.number,
                                }))
                            },
                            HttpMatcher::get("/*") => |ctx: Context<()>, req: Request| async move {
                                Json(json!({
                                    "method": req.method().as_str(),
                                    "path": req.uri().path(),
                                    "filter": ctx.get::<ProxyFilter>().map(|f| format!("{:?}", f)),
                                }))
                            },
                            _ => StatusCode::NOT_FOUND,
                        })
                    ))
                    .layer(UpgradeLayer::new(
                        MethodMatcher::CONNECT,
                        service_fn(http_connect_accept),
                        service_fn(http_connect_proxy),
                    ))
                    .service_fn(http_plain_proxy),
            )
            .await
            .unwrap();
    });

    graceful
        .shutdown_with_limit(Duration::from_secs(30))
        .await
        .expect("graceful shutdown");
}

async fn http_connect_accept<S>(
    mut ctx: Context<S>,
    req: Request,
) -> Result<(Response, Context<S>, Request), Response>
where
    S: Send + Sync + 'static,
{
    // TODO: should we support http connect better?
    // e.g. by always adding the host

    let (parts, body) = req.into_parts();
    let host = match Host::from_request_parts(&ctx, &parts).await {
        Ok(host) => host,
        Err(err) => {
            tracing::error!(error = %err, "error extracting host");
            return Err(err.into_response());
        }
    };
    let req = Request::from_parts(parts, body);

    tracing::info!("accept CONNECT to {}", host.0);
    ctx.insert(host);

    Ok((StatusCode::OK.into_response(), ctx, req))
}

async fn http_connect_proxy<S>(ctx: Context<S>, mut upgraded: Upgraded) -> Result<(), Infallible>
where
    S: Send + Sync + 'static,
{
    let Host(host) = ctx.get().unwrap();
    tracing::info!("CONNECT to {}", host);
    let mut stream = match tokio::net::TcpStream::connect(&host).await {
        Ok(stream) => stream,
        Err(err) => {
            tracing::error!(error = %err, "error connecting to host");
            return Ok(());
        }
    };
    if let Err(err) = tokio::io::copy_bidirectional(&mut upgraded, &mut stream).await {
        if !is_connection_error(&err) {
            tracing::error!(error = %err, "error copying data");
        }
    }
    Ok(())
}

async fn http_plain_proxy<S>(ctx: Context<S>, req: Request) -> Result<Response, Infallible>
where
    S: Send + Sync + 'static,
{
    let client = HttpClient::new();
    match client.serve(ctx, req).await {
        Ok(resp) => Ok(resp),
        Err(err) => {
            tracing::error!(error = %err, "error in client request");
            Ok(Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::empty())
                .unwrap())
        }
    }
}
