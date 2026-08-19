use super::{full, CaptureTasks, ProxyCtx, DOM_GATE};
use crate::adapter::Identity;
use crate::capture::{self, CapturedRequest, CapturedResponse, PendingCapture};
use futures_util::{SinkExt, StreamExt};
use hyper::{Request, Response, StatusCode};
use serde_json::Value;
use std::sync::Arc;
use std::time::SystemTime;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::server::create_response_with_body;
use tokio_tungstenite::tungstenite::protocol::{Message, Role};
use tokio_tungstenite::WebSocketStream;

struct ActiveTurn {
    pending: PendingCapture,
    response_headers: hyper::HeaderMap,
    sse: String,
}

#[derive(Default)]
struct RelayHistory {
    request: Option<Vec<u8>>,
    response_id: Option<String>,
    response_items: Vec<Vec<u8>>,
    response_turn_id: Option<String>,
}

struct NormalizedRequest {
    body: Value,
    encoded: Vec<u8>,
    body_sha256: String,
    ids: Identity,
    warmup: bool,
}

pub(super) fn is_upgrade(req: &Request<hyper::body::Incoming>) -> bool {
    req.headers()
        .get(hyper::header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
}

pub(super) async fn upgrade(
    req: &mut Request<hyper::body::Incoming>,
    ctx: Arc<ProxyCtx>,
    capture_tasks: CaptureTasks,
) -> Response<super::BoxBody> {
    let downstream_response = match create_response_with_body(&*req, || ()) {
        Ok(response) => response,
        Err(error) => {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(full(format!("invalid WebSocket upgrade: {error}\n")))
                .unwrap();
        }
    };

    let path = req.uri().path().to_string();
    let query = req
        .uri()
        .query()
        .map(|query| format!("?{query}"))
        .unwrap_or_default();
    let url = match upstream_url(
        &ctx.adapter.upstream,
        ctx.adapter.upstream_path(&path),
        &query,
    ) {
        Ok(url) => url,
        Err(error) => {
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(full(format!("vaultr upstream: {error}\n")))
                .unwrap();
        }
    };
    let request_headers = req.headers().clone();
    let upstream_request = match upstream_request(&url, &request_headers) {
        Ok(request) => request,
        Err(error) => {
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(full(format!("vaultr upstream: {error}\n")))
                .unwrap();
        }
    };
    let (upstream, upstream_response) =
        match tokio_tungstenite::connect_async(upstream_request).await {
            Ok(connection) => connection,
            Err(error) => {
                eprintln!(
                    "[{}] WebSocket upstream failed: {error}",
                    ctx.adapter.harness.capture_label()
                );
                return upstream_failure_response(error);
            }
        };

    let response_headers = upstream_response.headers().clone();
    let relay_response_headers = response_headers.clone();
    let on_upgrade = hyper::upgrade::on(req);
    capture_tasks.spawn(async move {
        match on_upgrade.await {
            Ok(upgraded) => {
                let downstream = WebSocketStream::from_raw_socket(
                    hyper_util::rt::TokioIo::new(upgraded),
                    Role::Server,
                    None,
                )
                .await;
                relay(
                    downstream,
                    upstream,
                    ctx,
                    request_headers,
                    relay_response_headers,
                    path,
                )
                .await;
            }
            Err(error) => eprintln!("WebSocket client upgrade failed: {error}"),
        }
    });

    let (parts, ()) = downstream_response.into_parts();
    let mut response = Response::from_parts(parts, full(""));
    let connection_tokens = connection_tokens(&response_headers);
    for (name, value) in &response_headers {
        if response_header_is_forwardable(name, &connection_tokens) {
            response.headers_mut().append(name, value.clone());
        }
    }
    response
}

fn upstream_failure_response(
    error: tokio_tungstenite::tungstenite::Error,
) -> Response<super::BoxBody> {
    match error {
        tokio_tungstenite::tungstenite::Error::Http(response) => {
            let (parts, _) = (*response).into_parts();
            let connection_tokens = connection_tokens(&parts.headers);
            let mut downstream = Response::builder()
                .status(parts.status)
                .version(parts.version)
                .body(full(""))
                .unwrap();
            for (name, value) in &parts.headers {
                if response_header_is_forwardable(name, &connection_tokens) {
                    downstream.headers_mut().append(name, value.clone());
                }
            }
            downstream
        }
        error => Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .body(full(format!("vaultr upstream: {error}\n")))
            .unwrap(),
    }
}

fn upstream_url(base: &str, path: &str, query: &str) -> Result<String, String> {
    let base = base.trim_end_matches('/');
    let base = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if base.starts_with("ws://") || base.starts_with("wss://") {
        base.to_string()
    } else {
        return Err(format!("unsupported upstream URL: {base}"));
    };
    Ok(format!("{base}{path}{query}"))
}

fn upstream_request(
    url: &str,
    downstream_headers: &hyper::HeaderMap,
) -> Result<hyper::Request<()>, tokio_tungstenite::tungstenite::Error> {
    let mut request = url.into_client_request()?;
    let connection_tokens = connection_tokens(downstream_headers);
    for (name, value) in downstream_headers {
        if !request_header_is_forwardable(name, &connection_tokens) {
            continue;
        }
        request.headers_mut().append(name, value.clone());
    }
    Ok(request)
}

fn connection_tokens(headers: &hyper::HeaderMap) -> Vec<String> {
    headers
        .get_all(hyper::header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|token| token.trim().to_ascii_lowercase())
        .filter(|token| !token.is_empty())
        .collect()
}

fn is_hop_by_hop(name: &hyper::header::HeaderName, connection_tokens: &[String]) -> bool {
    let lower = name.as_str();
    connection_tokens.iter().any(|token| token == lower)
        || matches!(
            lower,
            "connection"
                | "keep-alive"
                | "proxy-authenticate"
                | "proxy-authorization"
                | "proxy-connection"
                | "te"
                | "trailer"
                | "transfer-encoding"
                | "upgrade"
        )
}

fn request_header_is_forwardable(
    name: &hyper::header::HeaderName,
    connection_tokens: &[String],
) -> bool {
    !is_hop_by_hop(name, connection_tokens)
        && !matches!(
            name.as_str(),
            "content-length"
                | "host"
                | "sec-websocket-key"
                | "sec-websocket-version"
                | "sec-websocket-accept"
                | "sec-websocket-extensions"
        )
}

fn response_header_is_forwardable(
    name: &hyper::header::HeaderName,
    connection_tokens: &[String],
) -> bool {
    !is_hop_by_hop(name, connection_tokens)
        && !matches!(
            name.as_str(),
            "content-length" | "sec-websocket-accept" | "sec-websocket-extensions"
        )
}

async fn relay<Downstream>(
    mut downstream: WebSocketStream<Downstream>,
    mut upstream: WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    ctx: Arc<ProxyCtx>,
    request_headers: hyper::HeaderMap,
    response_headers: hyper::HeaderMap,
    path: String,
) where
    Downstream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut active = None;
    let mut history = RelayHistory::default();
    let mut awaiting_terminal = false;
    let mut capture_disabled = false;
    loop {
        tokio::select! {
            from_client = downstream.next() => {
                let message = match from_client {
                    Some(Ok(message)) => message,
                    Some(Err(error)) => {
                        eprintln!("WebSocket client relay failed: {error}");
                        let _ = upstream.close(None).await;
                        break;
                    }
                    None => {
                        let _ = upstream.close(None).await;
                        break;
                    }
                };
                match message {
                    Message::Text(text) => {
                        let permit = DOM_GATE.acquire().await;
                        let body = response_create_body(text.as_str());
                        if body.is_none() && !capture_disabled {
                            disable_ambiguous_capture(
                                &ctx,
                                &mut active,
                                &mut history,
                                &mut awaiting_terminal,
                                &mut capture_disabled,
                                "unrecognized client text",
                            )
                            .await;
                        }
                        if let Some(body) = body {
                            if awaiting_terminal {
                                disable_ambiguous_capture(
                                    &ctx,
                                    &mut active,
                                    &mut history,
                                    &mut awaiting_terminal,
                                    &mut capture_disabled,
                                    "overlapping response.create",
                                )
                                .await;
                            } else if !capture_disabled {
                                match normalize_request(
                                    body,
                                    &history,
                                    &request_headers,
                                    &ctx.adapter,
                                ) {
                                    Some(normalized) => {
                                        awaiting_terminal = true;
                                        history.request = Some(normalized.encoded);
                                        history.response_id = None;
                                        history.response_items.clear();
                                        history.response_turn_id = normalized.ids.turn_id.clone();
                                        if !normalized.warmup {
                                            active = prepare_turn(
                                                &ctx,
                                                &response_headers,
                                                &path,
                                                normalized.body,
                                                normalized.ids,
                                                normalized.body_sha256,
                                            )
                                            .await;
                                        }
                                    }
                                    None => {
                                        capture_disabled = true;
                                        history = RelayHistory::default();
                                        eprintln!(
                                            "[{}] WebSocket capture disabled: broken response-id chain",
                                            ctx.adapter.harness.capture_label()
                                        );
                                    }
                                }
                            }
                        }
                        drop(permit);
                        capture::release_memory();
                        if let Err(error) = upstream.send(Message::Text(text)).await {
                            eprintln!("WebSocket upstream relay failed: {error}");
                            break;
                        }
                    }
                    Message::Binary(payload) => {
                        if !capture_disabled {
                            disable_ambiguous_capture(
                                &ctx,
                                &mut active,
                                &mut history,
                                &mut awaiting_terminal,
                                &mut capture_disabled,
                                "binary client data",
                            )
                            .await;
                        }
                        if let Err(error) = upstream.send(Message::Binary(payload)).await {
                            eprintln!("WebSocket upstream relay failed: {error}");
                            break;
                        }
                    }
                    Message::Ping(payload) => {
                        if downstream.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Message::Pong(_) => {}
                    Message::Close(frame) => {
                        let _ = downstream.flush().await;
                        let _ = upstream.send(Message::Close(frame)).await;
                        break;
                    }
                    other => {
                        if let Err(error) = upstream.send(other).await {
                            eprintln!("WebSocket upstream relay failed: {error}");
                            break;
                        }
                    }
                }
            }
            from_upstream = upstream.next() => {
                let message = match from_upstream {
                    Some(Ok(message)) => message,
                    Some(Err(error)) => {
                        eprintln!("WebSocket upstream relay failed: {error}");
                        let _ = downstream.close(None).await;
                        break;
                    }
                    None => {
                        let _ = downstream.close(None).await;
                        break;
                    }
                };
                match message {
                    Message::Text(text) => {
                        let permit = DOM_GATE.acquire().await;
                        let terminal = !capture_disabled
                            && awaiting_terminal
                            && observe_response(&mut active, &mut history, text.as_str());
                        drop(permit);
                        capture::release_memory();
                        let forwarded = downstream.send(Message::Text(text)).await.is_ok();
                        if terminal {
                            awaiting_terminal = false;
                            if let Some(completed) = active.take() {
                                finish_turn(&ctx, completed, forwarded).await;
                            }
                        }
                        if !forwarded {
                            break;
                        }
                    }
                    Message::Ping(payload) => {
                        if upstream.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Message::Pong(_) => {}
                    Message::Close(frame) => {
                        let _ = upstream.flush().await;
                        let _ = downstream.send(Message::Close(frame)).await;
                        break;
                    }
                    other => {
                        if downstream.send(other).await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
    }
    if let Some(interrupted) = active {
        finish_turn(&ctx, interrupted, false).await;
    }
}

async fn disable_ambiguous_capture(
    ctx: &Arc<ProxyCtx>,
    active: &mut Option<ActiveTurn>,
    history: &mut RelayHistory,
    awaiting_terminal: &mut bool,
    capture_disabled: &mut bool,
    reason: &str,
) {
    if let Some(previous) = active.take() {
        finish_turn(ctx, previous, false).await;
    }
    *history = RelayHistory::default();
    *awaiting_terminal = false;
    *capture_disabled = true;
    eprintln!(
        "[{}] WebSocket capture disabled: {reason}",
        ctx.adapter.harness.capture_label()
    );
}

fn response_create_body(text: &str) -> Option<Value> {
    let mut body: Value = serde_json::from_str(text).ok()?;
    let object = body.as_object_mut()?;
    if object.get("type").and_then(Value::as_str) != Some("response.create") {
        return None;
    }
    object.remove("type");
    Some(body)
}

fn normalize_request(
    mut body: Value,
    history: &RelayHistory,
    request_headers: &hyper::HeaderMap,
    adapter: &crate::adapter::Adapter,
) -> Option<NormalizedRequest> {
    let object = body.as_object_mut()?;
    let previous_response_id = match object.remove("previous_response_id") {
        None => None,
        Some(Value::String(response_id)) => Some(response_id),
        Some(_) => return None,
    };
    let warmup = match object.remove("generate") {
        None | Some(Value::Bool(true)) => false,
        Some(Value::Bool(false)) => true,
        Some(_) => return None,
    };

    if let Some(previous_response_id) = previous_response_id {
        if history.response_id.as_deref() != Some(previous_response_id.as_str()) {
            return None;
        }
        let mut previous: Value = serde_json::from_slice(history.request.as_deref()?).ok()?;
        let mut full_input = previous
            .get_mut("input")?
            .as_array_mut()
            .map(std::mem::take)?;
        for encoded_item in &history.response_items {
            let mut item: Value = serde_json::from_slice(encoded_item).ok()?;
            if let (Some(turn_id), Some(item)) =
                (history.response_turn_id.as_deref(), item.as_object_mut())
            {
                item.insert(
                    "internal_chat_message_metadata_passthrough".into(),
                    serde_json::json!({ "turn_id": turn_id }),
                );
            }
            full_input.push(item);
        }
        let mut incremental = object
            .get_mut("input")?
            .as_array_mut()
            .map(std::mem::take)?;
        full_input.append(&mut incremental);
        object.insert("input".into(), Value::Array(full_input));
    }

    let ids = websocket_identity(adapter, request_headers, &body);
    let encoded = serde_json::to_vec(&body).ok()?;
    let body_sha256 = vaultr::digest::sha256_hex(&encoded);
    Some(NormalizedRequest {
        body,
        encoded,
        ids,
        warmup,
        body_sha256,
    })
}

fn websocket_identity(
    adapter: &crate::adapter::Adapter,
    handshake_headers: &hyper::HeaderMap,
    body: &Value,
) -> Identity {
    let frame = adapter.identity(&hyper::HeaderMap::new(), body);
    let handshake = adapter.identity(handshake_headers, body);
    Identity {
        session_id: frame.session_id.or(handshake.session_id),
        thread_id: frame.thread_id.or(handshake.thread_id),
        turn_id: frame.turn_id.or(handshake.turn_id),
    }
}

async fn prepare_turn(
    ctx: &Arc<ProxyCtx>,
    response_headers: &hyper::HeaderMap,
    path: &str,
    body: Value,
    ids: Identity,
    body_sha256: String,
) -> Option<ActiveTurn> {
    let request = CapturedRequest {
        method: "POST".to_string(),
        path: path.to_string(),
        content_encoding: None,
        body_sha256,
        ids,
        started_at: SystemTime::now(),
    };
    let prepared =
        capture::prepare_capture_offloaded(ctx.vault.clone(), ctx.adapter.clone(), request, body)
            .await;
    match prepared {
        Ok(pending) => Some(ActiveTurn {
            pending,
            response_headers: response_headers.clone(),
            sse: String::new(),
        }),
        Err(error) => {
            eprintln!(
                "[{}] WebSocket capture failed: {error}",
                ctx.adapter.harness.capture_label()
            );
            None
        }
    }
}

fn observe_response(
    active: &mut Option<ActiveTurn>,
    history: &mut RelayHistory,
    text: &str,
) -> bool {
    let parsed = serde_json::from_str::<Value>(text).ok();
    if let Some(turn) = active {
        turn.sse.push_str("data: ");
        match &parsed {
            Some(parsed) => turn.sse.push_str(&parsed.to_string()),
            None => turn.sse.push_str(text),
        }
        turn.sse.push_str("\n\n");
    }
    let Some(parsed) = parsed else {
        return false;
    };
    if parsed.get("type").and_then(Value::as_str) == Some("response.output_item.done") {
        if let Some(item) = parsed.get("item") {
            history
                .response_items
                .push(serde_json::to_vec(item).expect("a JSON value always serializes"));
        }
    }
    if parsed.get("type").and_then(Value::as_str) != Some("response.completed") {
        return false;
    }
    history.response_id = parsed
        .pointer("/response/id")
        .and_then(Value::as_str)
        .map(String::from);
    if history.response_items.is_empty() {
        if let Some(items) = parsed.pointer("/response/output").and_then(Value::as_array) {
            history.response_items.extend(
                items
                    .iter()
                    .map(|item| serde_json::to_vec(item).expect("a JSON value always serializes")),
            );
        }
    }
    true
}

async fn finish_turn(ctx: &Arc<ProxyCtx>, active: ActiveTurn, transport_complete: bool) {
    let events = vaultr::recon::parse_sse(&active.sse);
    let response = CapturedResponse {
        status: 200,
        headers: active.response_headers,
        complete: ctx.adapter.response_complete(&events, transport_complete),
        sse: active.sse,
    };
    ctx.otel.record(
        &ctx.adapter,
        active.pending.model.as_deref(),
        &active.pending.req,
        &response,
        &events,
    );
    drop(events);
    if let Err(error) = capture::finish_capture_offloaded(
        ctx.vault.clone(),
        ctx.adapter.clone(),
        active.pending,
        response,
    )
    .await
    {
        eprintln!("WebSocket capture failed: {error}");
    }
    capture::release_memory();
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn websocket_url_preserves_base_path_and_query() {
        let adapter = crate::adapter::adapters().remove(1);
        assert_eq!(
            upstream_url(
                "https://chatgpt.com/backend-api/codex/",
                adapter.upstream_path("/codex/responses"),
                "?feature=1"
            )
            .unwrap(),
            "wss://chatgpt.com/backend-api/codex/responses?feature=1"
        );
        assert_eq!(
            upstream_url(
                "https://chatgpt.com/backend-api/codex/",
                adapter.upstream_path("/responses"),
                ""
            )
            .unwrap(),
            "wss://chatgpt.com/backend-api/codex/responses"
        );
    }

    #[test]
    fn response_create_expands_incremental_history_and_removes_transport_fields() {
        let adapter = crate::adapter::adapters().remove(1);
        let stale_turn = "019f1234-5678-7abc-8def-0123456789b6";
        let frame_turn = "019f1234-5678-7abc-8def-0123456789b7";
        let mut headers = hyper::HeaderMap::new();
        headers.insert(
            "x-codex-turn-metadata",
            serde_json::json!({"turn_id": stale_turn})
                .to_string()
                .parse()
                .unwrap(),
        );
        let warmup = response_create_body(
            &serde_json::json!({
                "type":"response.create",
                "model":"gpt-test",
                "input":[{"role":"user"}],
                "generate":false,
                "client_metadata":{"turn_id":frame_turn}
            })
            .to_string(),
        )
        .unwrap();
        let warmup =
            normalize_request(warmup, &RelayHistory::default(), &headers, &adapter).unwrap();
        assert!(warmup.warmup);
        assert!(warmup.body.get("generate").is_none());
        assert_eq!(warmup.ids.turn_id.as_deref(), Some(frame_turn));

        let history = RelayHistory {
            request: Some(warmup.encoded),
            response_id: Some("resp-1".into()),
            response_items: vec![serde_json::to_vec(&serde_json::json!({
                "type":"message", "role":"assistant"
            }))
            .unwrap()],
            response_turn_id: Some(frame_turn.into()),
        };
        let next = response_create_body(
            r#"{"type":"response.create","model":"gpt-test","previous_response_id":"resp-1","input":[{"role":"user","content":"next"}]}"#,
        )
        .unwrap();
        let next = normalize_request(next, &history, &headers, &adapter).unwrap();
        assert!(!next.warmup);
        assert!(next.body.get("type").is_none());
        assert!(next.body.get("previous_response_id").is_none());
        assert_eq!(next.body["input"].as_array().unwrap().len(), 3);
        assert_eq!(
            next.body["input"][1]["internal_chat_message_metadata_passthrough"]["turn_id"],
            frame_turn
        );
        assert!(normalize_request(
            response_create_body(
                r#"{"type":"response.create","previous_response_id":"wrong","input":[]}"#
            )
            .unwrap(),
            &history,
            &headers,
            &adapter,
        )
        .is_none());
        assert!(normalize_request(
            response_create_body(
                r#"{"type":"response.create","previous_response_id":7,"input":[]}"#
            )
            .unwrap(),
            &history,
            &headers,
            &adapter,
        )
        .is_none());
        assert!(normalize_request(
            response_create_body(r#"{"type":"response.create","generate":"no","input":[]}"#)
                .unwrap(),
            &history,
            &headers,
            &adapter,
        )
        .is_none());
        assert!(response_create_body(r#"{"type":"session.update"}"#).is_none());
    }

    #[test]
    fn upstream_handshake_regenerates_every_hop_by_hop_header() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert("connection", "Upgrade, x-hop".parse().unwrap());
        headers.insert("upgrade", "websocket".parse().unwrap());
        headers.insert("x-hop", "drop".parse().unwrap());
        headers.insert("keep-alive", "drop".parse().unwrap());
        headers.insert("proxy-authorization", "drop".parse().unwrap());
        headers.insert("proxy-connection", "drop".parse().unwrap());
        headers.insert("content-length", "7".parse().unwrap());
        headers.insert("authorization", "Bearer keep".parse().unwrap());
        headers.insert("sec-websocket-protocol", "codex".parse().unwrap());

        let request = upstream_request("ws://127.0.0.1/responses", &headers).unwrap();
        assert_eq!(request.headers()["authorization"], "Bearer keep");
        assert_eq!(request.headers()["sec-websocket-protocol"], "codex");
        assert_eq!(request.headers()["connection"], "Upgrade");
        assert!(request.headers().get("x-hop").is_none());
        assert!(request.headers().get("keep-alive").is_none());
        assert!(request.headers().get("proxy-authorization").is_none());
        assert!(request.headers().get("proxy-connection").is_none());
        assert!(request.headers().get("content-length").is_none());
        assert!(response_header_is_forwardable(
            &hyper::header::HeaderName::from_static("x-codex-turn-state"),
            &[]
        ));
        assert!(!response_header_is_forwardable(
            &hyper::header::SEC_WEBSOCKET_EXTENSIONS,
            &[]
        ));
    }

    #[tokio::test]
    async fn upstream_handshake_rejections_preserve_fallback_status_and_headers() {
        for status in [StatusCode::UPGRADE_REQUIRED, StatusCode::UNAUTHORIZED] {
            let body = status.as_u16().to_string().into_bytes();
            let upstream = Response::builder()
                .status(status)
                .header("content-type", "text/plain")
                .header("www-authenticate", "Bearer test")
                .header("connection", "x-hop")
                .header("x-hop", "drop")
                .header("content-length", "999")
                .body(Some(body.clone()))
                .unwrap();
            let response = upstream_failure_response(tokio_tungstenite::tungstenite::Error::Http(
                upstream.into(),
            ));

            assert_eq!(response.status(), status);
            assert_eq!(response.headers()["content-type"], "text/plain");
            assert_eq!(response.headers()["www-authenticate"], "Bearer test");
            assert!(response.headers().get("connection").is_none());
            assert!(response.headers().get("x-hop").is_none());
            assert!(response.headers().get("content-length").is_none());
            assert!(response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .is_empty());
        }
    }

    #[tokio::test]
    async fn split_chunked_handshake_rejection_never_forwards_a_partial_body() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let send_body = Arc::new(tokio::sync::Notify::new());
        let server_send_body = send_body.clone();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            socket
                .write_all(
                    b"HTTP/1.1 426 Upgrade Required\r\n\
                      Content-Type: text/plain\r\n\
                      Transfer-Encoding: chunked\r\n\
                      X-Fallback: http\r\n\
                      \r\n",
                )
                .await
                .unwrap();
            socket.flush().await.unwrap();
            server_send_body.notified().await;
            let _ = socket.write_all(b"5\r\nlater\r\n0\r\n\r\n").await;
        });

        let error =
            match tokio_tungstenite::connect_async(format!("ws://{address}/responses")).await {
                Err(error) => error,
                Ok(_) => panic!("rejected handshake unexpectedly upgraded"),
            };
        let response = upstream_failure_response(error);
        send_body.notify_one();
        server.await.unwrap();

        assert_eq!(response.status(), StatusCode::UPGRADE_REQUIRED);
        assert_eq!(response.headers()["content-type"], "text/plain");
        assert_eq!(response.headers()["x-fallback"], "http");
        assert!(response.headers().get("transfer-encoding").is_none());
        assert!(response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .is_empty());
    }
}
