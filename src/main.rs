use std::{
    error::Error,
    net::{Ipv4Addr, SocketAddrV4},
    ops::RangeInclusive,
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering::Relaxed},
        Arc,
    },
    time::Duration,
};

use actix_tls::accept::openssl::TlsStream;
use actix_web::{
    dev::Service,
    http::{header, ConnectionType},
    middleware::{DefaultHeaders, Logger},
    rt::net::TcpStream,
    web::{to, Data},
    App, HttpServer,
};
use futures::TryFutureExt;
use log::{error, info};
use openssl::{
    asn1::Asn1Time,
    pkcs12::ParsedPkcs12,
    ssl::{ClientHelloResponse, SslAcceptor, SslAcceptorBuilder, SslMethod, SslOptions},
};
use parking_lot::RwLock;
use tokio::{
    fs::File,
    io::{AsyncBufReadExt, BufReader},
    signal::{self, unix::SignalKind},
    sync::{
        mpsc::{self, Sender, UnboundedReceiver},
        watch,
    },
    time::{sleep_until, Instant}, runtime::Handle,
};

use crate::{
    cache_manager::{CacheFileInfo, CacheManager},
    rpc::RPCClient,
    util::{create_dirs, create_http_client},
};

mod cache_manager;
mod error;
mod logger;
mod route;
mod rpc;
mod util;

#[cfg(not(target_env = "msvc"))]
use tikv_jemallocator::Jemalloc;

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

static CLIENT_VERSION: &str = "1.6.1";
static MAX_KEY_TIME_DRIFT: RangeInclusive<i64> = -300..=300;

#[derive(Clone)]
struct AppState {
    runtime: Handle,
    reqwest: reqwest::Client,
    id: i32,
    key: String,
    rpc: Arc<RPCClient>,
    cache_manager: Arc<CacheManager>,
    command_channel: Sender<COMMAND>,
}

enum COMMAND {
    ReloadCert,
    RefreshSettings,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // TODO read args
    let data_dir = "./data";
    let log_dir = "./log";
    let cache_dir = "./cache";
    let temp_dir = "./tmp";
    let download_dir = "./download";

    create_dirs(vec![data_dir, log_dir, cache_dir, temp_dir, download_dir]).await?;

    init_logger();

    info!("Hentai@Home {} (Rust) starting up", CLIENT_VERSION);

    let (id, key) = match read_credential(data_dir).await {
        Some(i) => i,
        None => todo!("Setup client"),
    };
    let client = Arc::new(RPCClient::new(id, &key));
    client.login().await?;

    // TODO cache clean
    let settings = client.settings();
    let cache_manager = Arc::new(
        CacheManager::new(
            cache_dir,
            temp_dir,
            settings.size_limit(),
            settings.static_range(),
            settings.verify_cache(),
        )
        .await?,
    );

    let (_shutdown_send, shutdown_recv) = mpsc::unbounded_channel::<()>();

    // command channel
    let (tx, mut rx) = mpsc::channel::<COMMAND>(1);
    let cert = client.get_cert().await.unwrap();
    if cert.cert.not_after() < Asn1Time::days_from_now(1).unwrap() {
        error!(
            "The retrieved certificate is expired, or the system time is off by more than a day. Correct the system time and try again."
        );
        return Err(error::Error::CertExpired.into());
    }

    let (server, cert_changer) = create_server(
        client.settings().client_port(),
        cert,
        AppState {
            runtime: Handle::current(),
            reqwest: create_http_client(),
            id,
            key,
            rpc: client.clone(),
            cache_manager: cache_manager.clone(),
            command_channel: tx,
        },
    );
    let server_handle = server.handle();

    // Http server loop
    info!("Starting HTTP server...");
    tokio::spawn(server);

    info!("Notifying the server that we have finished starting up the client...");
    if client.connect_check().await.is_none() {
        error!("Startup notification failed.");
        return Err(error::Error::ConnectTestFail.into());
    }

    // Check download jobs
    client.refresh_settings().await;

    // Check purge list
    if let Some(list) = client.get_purgelist(259200).await {
        for info in list.iter().filter_map(CacheFileInfo::from_file_id) {
            cache_manager.remove_cache(&info).await;
        }
    }

    info!("H@H initialization completed successfully. Starting normal operation");

    // Command listener
    let client2 = client.clone();
    tokio::spawn(async move {
        while let Some(command) = rx.recv().await {
            match command {
                COMMAND::ReloadCert => {
                    if let Some(cert) = client2.get_cert().await {
                        if cert_changer.send(cert).is_err() {
                            error!("Update SSL Cert fail");
                        }
                    }
                }
                COMMAND::RefreshSettings => {
                    client2.refresh_settings().await;
                }
            }
        }
    });

    // Schedule task
    let client3 = client.clone();
    let keepalive = tokio::spawn(async move {
        let mut counter: u32 = 0;
        let mut next_run = Instant::now() + Duration::from_secs(10);
        loop {
            sleep_until(next_run).await;

            if !client3.is_running() {
                break;
            }

            if counter % 11 == 0 {
                client3.still_alive(false).await;
            }

            // Check purge list every 7hr
            if counter % 2160 == 2159 {
                if let Some(list) = client3.get_purgelist(43200).await {
                    for info in list.iter().filter_map(CacheFileInfo::from_file_id) {
                        cache_manager.remove_cache(&info).await;
                    }
                }
            }

            counter = counter.wrapping_add(1);
            next_run = Instant::now() + Duration::from_secs(10);
        }
    });

    // Shutdown handle
    wait_shutdown_signal(shutdown_recv).await; // TODO force shutdown
    info!("Shutting down...");
    keepalive.abort();
    client.shutdown().await;
    info!("Shutdown in progress - please wait");
    server_handle.stop(true).await;
    Ok(())
}

/**
 * main helper
*/
fn init_logger() {
    logger::init().unwrap();
}

async fn read_credential(data_path: &str) -> Option<(i32, String)> {
    let path = Path::new(data_path).join("client_login");
    let mut file = File::open(path.clone()).map_ok(|f| BufReader::new(f).lines()).await.ok()?; // TODO better error handle
    let data = file.next_line().await.ok().flatten()?;
    let mut credential = data.split('-');

    let id: i32 = credential.next()?.parse().ok()?;
    let key = credential.next()?.to_owned();

    info!("Loaded login settings from {}", path.display());
    Some((id, key))
}

fn create_server(port: u16, cert: ParsedPkcs12, data: AppState) -> (actix_web::dev::Server, watch::Sender<ParsedPkcs12>) {
    let app_data = Data::new(data);
    let (tx, mut rx) = watch::channel(cert);
    let ssl_context = Arc::new(RwLock::new(create_ssl_acceptor(&rx.borrow_and_update()).build()));
    let ssl_context_write = ssl_context.clone();

    let mut ssl_acceptor = create_ssl_acceptor(&rx.clone().borrow_and_update());
    ssl_acceptor.set_client_hello_callback(move |ssl, _alert| {
        ssl.set_ssl_context(ssl_context.read().context())?;
        Ok(ClientHelloResponse::SUCCESS)
    });

    // Cert changer
    tokio::spawn(async move {
        while rx.changed().await.is_ok() {
            *ssl_context_write.write() = create_ssl_acceptor(&rx.borrow()).build();
        }
    });

    (
        HttpServer::new(move || {
            App::new()
                .app_data(app_data.clone())
                .wrap(logger_format())
                .wrap(DefaultHeaders::new().add((
                    header::SERVER,
                    format!("Genetic Lifeform and Distributed Open Server {}", CLIENT_VERSION),
                )))
                .wrap_fn(|req, next| {
                    next.call(req).map_ok(|mut res| {
                        let head = res.response_mut().head_mut();
                        head.set_connection_type(ConnectionType::Close);
                        head.set_camel_case_headers(true);
                        res
                    })
                })
                .default_service(to(route::default))
                .configure(route::configure)
        })
        .disable_signals()
        .client_request_timeout(Duration::from_secs(15))
        .on_connect(|conn, _ext| {
            if let Some(tcp) = conn.downcast_ref::<TlsStream<TcpStream>>() {
                tcp.get_ref().set_nodelay(true).unwrap();
            }
        })
        .bind_openssl(SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), port), ssl_acceptor)
        .unwrap()
        .run(),
        tx,
    )
}

fn create_ssl_acceptor(cert: &ParsedPkcs12) -> SslAcceptorBuilder {
    let mut builder = SslAcceptor::mozilla_intermediate(SslMethod::tls_server()).unwrap();
    builder.clear_options(SslOptions::NO_TLSV1_3);
    builder.set_options(SslOptions::NO_RENEGOTIATION | SslOptions::ENABLE_MIDDLEBOX_COMPAT);

    cpufeatures::new!(cpuid_aes, "aes");
    if !cpuid_aes::get() {
        builder
            .set_cipher_list(
                "ECDHE-ECDSA-CHACHA20-POLY1305:ECDHE-RSA-CHACHA20-POLY1305:\
            ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES128-GCM-SHA256:\
            ECDHE-ECDSA-AES256-GCM-SHA384:ECDHE-RSA-AES256-GCM-SHA384:\
            DHE-RSA-CHACHA20-POLY1305:DHE-RSA-AES128-GCM-SHA256:DHE-RSA-AES256-GCM-SHA384:\
            ECDHE-ECDSA-AES128-SHA256:ECDHE-RSA-AES128-SHA256:ECDHE-ECDSA-AES128-SHA:ECDHE-RSA-AES128-SHA:\
            ECDHE-ECDSA-AES256-SHA384:ECDHE-RSA-AES256-SHA384:ECDHE-ECDSA-AES256-SHA:ECDHE-RSA-AES256-SHA:\
            DHE-RSA-AES128-SHA256:DHE-RSA-AES256-SHA256:\
            AES128-GCM-SHA256:AES256-GCM-SHA384:AES128-SHA256:AES256-SHA256:AES128-SHA:AES256-SHA:\
            DES-CBC3-SHA",
            )
            .unwrap();
        builder
            .set_ciphersuites("TLS_CHACHA20_POLY1305_SHA256:TLS_AES_128_GCM_SHA256:TLS_AES_256_GCM_SHA384")
            .unwrap();
    }
    builder.set_private_key(&cert.pkey).unwrap();
    builder.set_certificate(&cert.cert).unwrap();
    if let Some(i) = &cert.chain {
        i.iter().rev().for_each(|j| builder.add_extra_chain_cert(j.to_owned()).unwrap());
    }
    builder
}

// TODO custom impl logger
fn logger_format() -> Logger {
    static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);
    Logger::new("%{CONNECTION}xi Code=%s Bytes=%b %r").custom_request_replace("CONNECTION", |req| {
        format!(
            "{{{}/{:16}",
            REQUEST_COUNTER.fetch_add(1, Relaxed),
            format!("{}}}", &req.connection_info().peer_addr().unwrap_or("-"))
        )
    })
}

async fn wait_shutdown_signal(mut shutdown_channel: UnboundedReceiver<()>) {
    let mut sigint = signal::unix::signal(SignalKind::interrupt()).unwrap();
    let mut sigterm = signal::unix::signal(SignalKind::terminate()).unwrap();
    tokio::select! {
        _ = signal::ctrl_c() => (),
        _ = sigint.recv() => (),
        _ = sigterm.recv() => (),
        _ = shutdown_channel.recv() => (),
        else => ()
    };
}