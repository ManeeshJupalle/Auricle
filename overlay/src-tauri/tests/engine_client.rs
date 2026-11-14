//! The overlay reaches its engine even when the machine has a proxy set.
//! Its own test binary: `HTTP_PROXY` is process-wide.

use std::io::{Read, Write};

#[tokio::test]
async fn the_engine_is_reached_behind_a_proxy() {
    // A proxy nothing listens on, so anything sent through it fails.
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy = format!("http://{}", dead.local_addr().unwrap());
    drop(dead);
    std::env::set_var("HTTP_PROXY", &proxy);
    std::env::set_var("http_proxy", &proxy);
    std::env::remove_var("NO_PROXY");
    std::env::remove_var("no_proxy");

    let engine = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", engine.local_addr().unwrap());
    std::thread::spawn(move || {
        let (mut socket, _) = engine.accept().unwrap();
        let mut request = [0u8; 1024];
        let _ = socket.read(&mut request);
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .unwrap();
    });

    let resp = auricle_overlay_lib::engine_client()
        .get(format!("{base}/api/v1/health"))
        .send()
        .await
        .expect("went to the proxy instead of the engine");
    assert_eq!(resp.status(), 200);
}
