use std::{
    cmp::max,
    time::{Duration, Instant},
};

use actix_web::{route, web::Data, HttpRequest, HttpResponse, Responder};
use actix_web_lab::extract::Path;
use futures::TryStreamExt;
use log::debug;
use rand::{prelude::SmallRng, Rng, SeedableRng};
use reqwest::{
    header::{HeaderValue, CONNECTION},
    Url,
};

use crate::{
    route::{forbidden, parse_additional, speed_test::random_response},
    util::{create_http_client, string_to_hash},
    AppState, Command, MAX_KEY_TIME_DRIFT,
};

#[route("/servercmd/{command}/{additional:[^/]*}/{time}/{key}", method = "GET", method = "HEAD")]
async fn servercmd(
    req: HttpRequest,
    Path((command, additional, time, hash)): Path<(String, String, i64, String)>,
    data: Data<AppState>,
) -> impl Responder {
    // Server IP check
    if !req
        .connection_info()
        .peer_addr()
        .map(|ip| data.rpc.is_vaild_rpc_server(ip))
        .unwrap_or(false)
    {
        debug!("Got a servercmd from an unauthorized IP address");
        return forbidden();
    }

    // Hash check
    let id = data.rpc.id();
    let key = data.rpc.key();
    let hash_string = format!("hentai@home-servercmd-{command}-{additional}-{id}-{time}-{key}");
    if !MAX_KEY_TIME_DRIFT.contains(&(data.rpc.get_timestemp() - time)) || string_to_hash(hash_string) != hash {
        debug!("{} Got a servercmd with expired or incorrect key", "<SESSION>");
        return forbidden();
    }

    match command.to_lowercase().as_str() {
        "still_alive" => HttpResponse::Ok().body("I feel FANTASTIC and I'm still alive"),
        "threaded_proxy_test" => {
            let additional = parse_additional(&additional);

            let host = additional.get("hostname").map(|s| s.as_str()).unwrap_or("");
            let protocol = additional.get("protocol").map(|s| s.as_str()).unwrap_or("http");
            let port = additional.get("port").and_then(|s| s.parse::<u16>().ok()).unwrap_or(0);
            let size = additional.get("testsize").and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
            let count = additional.get("testcount").and_then(|s| s.parse::<u8>().ok()).unwrap_or(0);
            let timestamp = additional.get("testtime").and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            let token = additional.get("testkey").map(|s| s.as_str()).unwrap_or("");

            debug!(
                "Running threaded proxy test against hostname={} protocol={} port={} testsize={} testcount={} testtime={} testkey={}",
                host, protocol, port, size, count, timestamp, token
            );

            if host.is_empty() || port == 0 || size == 0 || count == 0 || timestamp == 0 || token.is_empty() {
                return HttpResponse::BadRequest().finish();
            }

            // Switch to MT tokio runtime
            let runtime = data.runtime.enter();

            let mut rand = SmallRng::from_entropy();
            let mut requests = Vec::new();
            for _ in 1..=count {
                let url = Url::parse(
                    format!(
                        "{}://{}:{}/t/{}/{}/{}/{}",
                        protocol,
                        host,
                        port,
                        size,
                        time,
                        token,
                        rand.gen::<u32>()
                    )
                    .as_str(),
                )
                .unwrap();
                debug!("Speedtest thread start: {}", url);
                let reqwest = create_http_client(Duration::from_secs(60), None); // No proxy http client
                requests.push(tokio::spawn(async move {
                    for retry in 0..3 {
                        if retry > 0 {
                            debug!("Retrying.. ({} tries left)", 3 - retry);
                        }
                        let request = reqwest.get(url.clone()).header(CONNECTION, HeaderValue::from_static("Close"));
                        match request.send().await.and_then(|r| r.error_for_status()) {
                            Ok(res) => {
                                let start = Instant::now();

                                // Read & count response size
                                let response_size = res.bytes_stream().try_fold(0, |size, b| async move { Ok(size + b.len()) }).await;

                                // Check response size as excepted
                                if response_size.is_ok() && response_size.unwrap() == size as usize {
                                    let time = start.elapsed();
                                    let ms = time.as_millis();
                                    debug!("Speedtest thread done: {}ms ({:.2} KB/s)", ms, size as f64 / max(ms, 1) as f64);
                                    return Some(time);
                                }
                            }
                            Err(err) => {
                                debug!("Connection error: {}", err);
                            }
                        }
                    }
                    debug!("Exhaused retries or aborted getting {}", url);
                    None
                }));
            }

            drop(runtime);

            let mut success = 0;
            let mut total_time = Duration::new(0, 0);
            for request in requests {
                if let Some(time) = request.await.ok().flatten() {
                    success += 1;
                    total_time += time;
                };
            }

            let ms = total_time.as_millis();
            let speed = (size * success) as f64 / ms.checked_div(success as u128).unwrap_or(1) as f64;
            debug!("Speedtest result: success {}/{}, speed {:.2} KB/s", success, count, speed);
            HttpResponse::Ok().body(format!("OK:{}-{}", success, ms))
        }
        "speed_test" => random_response(
            parse_additional(&additional)
                .get("testsize")
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(1000000),
        ),
        "refresh_settings" => {
            let _ = data.command_channel.send(Command::RefreshSettings).await; // Ignore error
            HttpResponse::Ok().finish()
        }
        "start_downloader" => {
            let _ = data.command_channel.send(Command::StartDownloader).await; // Ignore error
            HttpResponse::Ok().finish()
        }
        "refresh_certs" => {
            let _ = data.command_channel.send(Command::ReloadCert).await; // Ignore error
            HttpResponse::Ok().finish()
        }
        _ => HttpResponse::Ok().body("INVALID_COMMAND"),
    }
}
