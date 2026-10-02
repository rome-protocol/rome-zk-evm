//! A minimal, hand-rolled HTTP/1.1 responder for exactly one endpoint: `GET /metrics`. Moved out of
//! `rome-zk-sequencer::metrics` into its own reth-free crate, unchanged, so `rome-zk-batcher` — which has no reth
//! dependency and should never grow one just to serve a Prometheus endpoint — can reuse the identical responder
//! instead of hand-rolling a second copy. The sequencer's own `metrics::serve_metrics` is now a thin call into
//! [`serve`]; its behaviour (and its own tests) are unchanged.
//!
//! No HTTP framework: this is the one endpoint any binary in this workspace needs
//! (`Content-Type: text/plain; version=0.0.4`, the Prometheus text-exposition format), and pulling in a
//! full server crate for it would cost more than it saves.

use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serve `GET /metrics` on `addr` until the process exits (or the listener itself errors) — `render` is
/// called fresh on every request, so callers whose metrics change over the process's lifetime (every
/// caller in this workspace) get a live snapshot each time, never a value captured once at startup. Any
/// path other than exactly `/metrics` gets a 404 with an empty body.
/// Binds the responder's listener and nothing else. Callers that must fail loudly on a bad or busy
/// address (a service that would otherwise run with a silently dead `/metrics`) bind here first, on their
/// own task, and only then hand the listener to [`serve_on`] in a spawned task.
pub async fn bind(addr: SocketAddr) -> std::io::Result<TcpListener> {
    TcpListener::bind(addr).await
}

/// [`bind`] + [`serve_on`] in one call, for callers that await the whole server themselves.
pub async fn serve<F>(addr: SocketAddr, render: F) -> std::io::Result<()>
where
    F: Fn() -> Vec<u8> + Send + Sync + 'static,
{
    let listener = bind(addr).await?;
    serve_on(listener, render).await
}

/// Serves `GET /metrics` on an already-bound listener until the accept loop errors.
pub async fn serve_on<F>(listener: TcpListener, render: F) -> std::io::Result<()>
where
    F: Fn() -> Vec<u8> + Send + Sync + 'static,
{
    let render = Arc::new(render);
    loop {
        let (mut socket, _) = listener.accept().await?;
        let render = render.clone();
        tokio::spawn(async move {
            // A fixed-size, single `read` (never a loop to EOF): a client that sends more than this
            // fits is simply truncated for the purposes of matching the one request line this responder
            // ever inspects — it must never panic or hang trying to read the rest.
            let mut buf = [0u8; 512];
            let n = match socket.read(&mut buf).await {
                Ok(n) => n,
                Err(_) => return,
            };
            let request = String::from_utf8_lossy(&buf[..n]);
            let is_metrics = request.starts_with("GET /metrics ");
            let body = if is_metrics { render() } else { Vec::new() };
            let status = if is_metrics {
                "200 OK"
            } else {
                "404 Not Found"
            };
            let header = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(header.as_bytes()).await;
            let _ = socket.write_all(&body).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Binds an ephemeral port, spawns [`serve`] on it, and hands back the address once the listener is
    /// almost certainly ready to accept (a short sleep — the same pattern `rome-zk-sequencer`'s own,
    /// now-removed responder test used).
    async fn spawn_serving(render: impl Fn() -> Vec<u8> + Send + Sync + 'static) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        tokio::spawn(async move { serve(addr, render).await.unwrap() });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        addr
    }

    /// `GET /metrics` -> 200, the exposition-format content type, body == `render()`.
    #[tokio::test]
    async fn get_metrics_returns_200_with_the_rendered_body() {
        let addr = spawn_serving(|| b"rome_zk_test_metric 1\n".to_vec()).await;

        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /metrics HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut resp = Vec::new();
        stream.read_to_end(&mut resp).await.unwrap();
        let text = String::from_utf8_lossy(&resp);

        assert!(text.starts_with("HTTP/1.1 200 OK"), "got: {text}");
        assert!(
            text.contains("Content-Type: text/plain; version=0.0.4"),
            "got: {text}"
        );
        assert!(
            text.ends_with("rome_zk_test_metric 1\n"),
            "body must equal render()'s own output verbatim, got: {text}"
        );
    }

    /// Any path other than `/metrics` gets a 404 with an empty body. Mutating the responder to return 200 for every
    /// path turns this red — `text` would start with "HTTP/1.1 200 OK" and carry the (non-empty) rendered body
    /// instead.
    #[tokio::test]
    async fn any_other_path_returns_404_with_an_empty_body() {
        let addr = spawn_serving(|| b"should never be rendered for a 404".to_vec()).await;

        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /other HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut resp = Vec::new();
        stream.read_to_end(&mut resp).await.unwrap();
        let text = String::from_utf8_lossy(&resp);

        assert!(text.starts_with("HTTP/1.1 404 Not Found"), "got: {text}");
        assert!(text.contains("Content-Length: 0"), "got: {text}");
        assert!(
            text.ends_with("\r\n\r\n"),
            "a 404 must carry no body at all, got: {text}"
        );
    }

    /// A request bigger than the responder's own fixed read buffer (512 B) must not panic or hang the spawned
    /// connection task. Deliberately does not assert on what (if anything) this same connection reads back: on
    /// Linux, closing a socket that still has unread peer bytes buffered in the kernel (guaranteed here — the
    /// request is far larger than the one 512-byte read) sends an RST rather than a clean FIN, which can surface to
    /// the client as `ConnectionReset` even after the server already wrote a complete response — a real TCP/OS
    /// artifact of this test's own oversized write, not a claim this contract makes. The actual proof of "does not
    /// panic or hang" is a **second, fresh connection** answering normally right after: a panic inside one
    /// connection's own spawned task cannot be observed via that connection's socket anyway (tokio isolates a
    /// panicking task from the rest of the process), so the only way to see the accept loop is still alive and well
    /// is to open a new connection and check it still works.
    #[tokio::test]
    async fn a_request_larger_than_the_read_buffer_does_not_panic() {
        let addr = spawn_serving(|| b"rome_zk_test_metric 1\n".to_vec()).await;

        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let oversized_header = format!(
            "GET /metrics HTTP/1.1\r\nX-Padding: {}\r\n\r\n",
            "a".repeat(4_096)
        );
        stream.write_all(oversized_header.as_bytes()).await.unwrap();
        let mut resp = Vec::new();
        // Ignored on purpose — see this test's own doc above.
        let _ = stream.read_to_end(&mut resp).await;

        let mut stream2 = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream2
            .write_all(b"GET /metrics HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut resp2 = Vec::new();
        stream2.read_to_end(&mut resp2).await.unwrap();
        let text2 = String::from_utf8_lossy(&resp2);
        assert!(
            text2.starts_with("HTTP/1.1 200 OK"),
            "the server must still be alive and answering normally after an oversized request on a \
             prior connection, got: {text2}"
        );
    }

    /// A bind failure must reach the caller by name BEFORE anything is spawned — a dropped spawn handle would
    /// otherwise swallow `AddrInUse` and leave a silently dead endpoint.
    #[tokio::test]
    async fn binding_an_occupied_port_is_a_named_error_before_anything_is_served() {
        let holder = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = holder.local_addr().unwrap();
        let err = bind(addr)
            .await
            .expect_err("the port is held; bind must fail");
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse, "got: {err}");
    }
}
