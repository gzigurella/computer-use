//! MCP transports (stdio and Streamable HTTP) and JSON-RPC request dispatch.

use super::*;

pub(crate) async fn shutdown_signal() {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("failed to install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = terminate.recv() => {},
    }
}

pub(crate) async fn stdio(state: SharedState) -> Result<(), String> {
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut line = Vec::new();
    let stdout = Arc::new(Mutex::new(tokio::io::stdout()));
    let pending = Arc::new(Mutex::new(HashMap::<
        String,
        tokio::sync::watch::Sender<bool>,
    >::new()));
    let mut requests = tokio::task::JoinSet::new();
    loop {
        while let Some(result) = requests.try_join_next() {
            result.map_err(|error| format!("MCP request worker failed: {error}"))?;
        }
        let oversized = read_bounded_line(&mut reader, &mut line, MAX_FRAME)
            .await
            .map_err(|error| format!("reading MCP input: {error}"))?;
        let Some(oversized) = oversized else { break };
        if oversized {
            write_stdout(
                &stdout,
                json_rpc_error(Value::Null, -32600, "message exceeds 1 MiB"),
            )
            .await?;
            continue;
        }
        let request = match serde_json::from_slice::<Value>(&line) {
            Ok(value) => value,
            Err(error) => {
                write_stdout(
                    &stdout,
                    json_rpc_error(Value::Null, -32700, &error.to_string()),
                )
                .await?;
                continue;
            }
        };
        if request["method"] == "notifications/cancelled" && request.get("id").is_none() {
            if let Some(request_id) = request["params"].get("requestId")
                && let Some(cancel) = pending.lock().await.get(&request_id.to_string())
            {
                cancel.send_replace(true);
            }
            continue;
        }
        let Some(id) = request.get("id").cloned() else {
            let (_cancel, receiver) = tokio::sync::watch::channel(false);
            if let Some(response) = handle_request(request, state.clone(), receiver).await {
                write_stdout(&stdout, response).await?;
            }
            continue;
        };
        let key = id.to_string();
        let (cancel, receiver) = tokio::sync::watch::channel(false);
        let rejected = {
            let mut pending = pending.lock().await;
            if pending.contains_key(&key) {
                Some(json_rpc_error(
                    id,
                    -32600,
                    "request ID is already in flight",
                ))
            } else if pending.len() >= MAX_PENDING_STDIO_REQUESTS {
                Some(json_rpc_error(
                    id,
                    -32000,
                    "too many in-flight MCP requests",
                ))
            } else {
                pending.insert(key.clone(), cancel);
                None
            }
        };
        if let Some(response) = rejected {
            write_stdout(&stdout, response).await?;
            continue;
        }
        let state = state.clone();
        let stdout = Arc::clone(&stdout);
        let pending = Arc::clone(&pending);
        let computer_start =
            request["method"] == "tools/call" && request["params"]["name"] == "computer_start";
        requests.spawn(async move {
            if let Some(response) = handle_request(request, state.clone(), receiver).await {
                let started_session = computer_start.then(|| response["result"]["structuredContent"]["session_id"].as_str()).flatten().map(str::to_owned);
                if let Err(error) = write_stdout(&stdout, response).await {
                    eprintln!("computer-use-linux: writing MCP response: {error}");
                    if let Some(session_id) = started_session
                        && let Err(cleanup) = stop_session(&json!({"session_id":session_id}), state).await
                    {
                        eprintln!("computer-use-linux: stdio response failed and desktop session cleanup failed: {cleanup}");
                    }
                }
            }
            pending.lock().await.remove(&key);
        });
    }
    while let Some(result) = requests.join_next().await {
        result.map_err(|error| format!("MCP request worker failed: {error}"))?;
    }
    Ok(())
}

pub(crate) async fn read_bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    line: &mut Vec<u8>,
    limit: usize,
) -> std::io::Result<Option<bool>> {
    line.clear();
    let mut oversized = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(if line.is_empty() && !oversized {
                None
            } else {
                Some(oversized)
            });
        }
        let consumed = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if !oversized {
            if line.len().saturating_add(consumed) <= limit {
                line.extend_from_slice(&available[..consumed]);
            } else {
                oversized = true;
                line.clear();
            }
        }
        let complete = available[consumed - 1] == b'\n';
        reader.consume(consumed);
        if complete {
            return Ok(Some(oversized));
        }
    }
}

pub(crate) async fn write_stdout(
    stdout: &Mutex<tokio::io::Stdout>,
    value: Value,
) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(&value).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    let mut stdout = stdout.lock().await;
    stdout
        .write_all(&bytes)
        .await
        .map_err(|error| format!("writing MCP response: {error}"))?;
    stdout
        .flush()
        .await
        .map_err(|error| format!("flushing MCP response: {error}"))
}

pub(crate) async fn http(state: SharedState, port: u16) -> Result<(), String> {
    let token = Uuid::new_v4().simple().to_string();
    let pending = Arc::new(Mutex::new(HashMap::<
        String,
        tokio::sync::watch::Sender<bool>,
    >::new()));
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|error| format!("binding loopback HTTP server: {error}"))?;
    let address = listener.local_addr().map_err(|error| error.to_string())?;
    eprintln!("MCP HTTP listening on http://{address}/mcp");
    eprintln!("Bearer token: {token}");
    let mut requests = tokio::task::JoinSet::new();
    let connections = Arc::new(tokio::sync::Semaphore::new(MAX_HTTP_CONNECTIONS));
    loop {
        while let Some(result) = requests.try_join_next() {
            if let Err(error) = result {
                eprintln!("computer-use-linux: HTTP request worker failed: {error}");
            }
        }
        let (stream, peer) = listener.accept().await.map_err(|error| error.to_string())?;
        let Ok(permit) = Arc::clone(&connections).try_acquire_owned() else {
            let (_, mut write) = stream.into_split();
            let _ = write_http(
                &mut write,
                503,
                "text/plain",
                b"too many HTTP connections\n",
            )
            .await;
            continue;
        };
        let state = state.clone();
        let token = token.clone();
        let pending = Arc::clone(&pending);
        requests.spawn(async move {
            let _permit = permit;
            if let Err(error) =
                serve_http(stream, peer, state, pending, &token, address.port()).await
            {
                eprintln!("HTTP request failed: {error}");
            }
        });
    }
}

pub(crate) async fn serve_http(
    stream: TcpStream,
    peer: std::net::SocketAddr,
    state: SharedState,
    pending: Arc<Mutex<HashMap<String, tokio::sync::watch::Sender<bool>>>>,
    token: &str,
    port: u16,
) -> Result<(), String> {
    if !peer.ip().is_loopback() {
        return Err("non-loopback peer rejected".into());
    }
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let headers_deadline = Instant::now() + Duration::from_secs(10);
    let mut first = Vec::new();
    match timeout(
        headers_deadline.saturating_duration_since(Instant::now()),
        read_bounded_line(&mut reader, &mut first, 8192),
    )
    .await
    {
        Err(_) => return write_http(&mut write, 408, "text/plain", b"request timeout\n").await,
        Ok(Err(error)) => return Err(error.to_string()),
        Ok(Ok(None)) => return Err("empty HTTP request".into()),
        Ok(Ok(Some(true))) => {
            return write_http(&mut write, 431, "text/plain", b"request line too long\n").await;
        }
        Ok(Ok(Some(false))) => {}
    }
    let first = String::from_utf8_lossy(&first);
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let path = parts.next().unwrap_or_default();
    let version = parts.next().unwrap_or_default();
    if method.is_empty()
        || path.is_empty()
        || !matches!(version, "HTTP/1.0" | "HTTP/1.1")
        || parts.next().is_some()
    {
        return write_http(&mut write, 400, "text/plain", b"invalid request line\n").await;
    }
    let mut headers = HashMap::new();
    let mut header_line = Vec::new();
    let mut header_bytes = first.len();
    loop {
        let remaining = headers_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return write_http(
                &mut write,
                408,
                "text/plain",
                b"request headers timed out\n",
            )
            .await;
        }
        let line = match timeout(
            remaining,
            read_bounded_line(&mut reader, &mut header_line, 8192),
        )
        .await
        {
            Err(_) => return write_http(&mut write, 408, "text/plain", b"request timeout\n").await,
            Ok(Err(error)) => return Err(error.to_string()),
            Ok(Ok(None)) => return Err("incomplete HTTP headers".into()),
            Ok(Ok(Some(true))) => {
                return write_http(&mut write, 431, "text/plain", b"header line too long\n").await;
            }
            Ok(Ok(Some(false))) => header_line.len(),
        };
        header_bytes += line;
        if header_bytes > 16 * 1024 {
            return write_http(
                &mut write,
                431,
                "text/plain",
                b"HTTP headers exceed 16 KiB\n",
            )
            .await;
        }
        if header_line == b"\r\n" || header_line == b"\n" {
            break;
        }
        let header_line = String::from_utf8_lossy(&header_line);
        let Some((name, value)) = header_line.split_once(':') else {
            return write_http(&mut write, 400, "text/plain", b"invalid HTTP header\n").await;
        };
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty() || headers.insert(name, value.trim().to_owned()).is_some() {
            return write_http(
                &mut write,
                400,
                "text/plain",
                b"empty or duplicate HTTP header\n",
            )
            .await;
        }
    }
    if headers.contains_key("transfer-encoding") {
        return write_http(
            &mut write,
            400,
            "text/plain",
            b"Transfer-Encoding is unsupported\n",
        )
        .await;
    }
    if !host_is_loopback(
        headers.get("host").map(String::as_str).unwrap_or_default(),
        port,
    ) {
        return write_http(&mut write, 403, "text/plain", b"invalid host\n").await;
    }
    if let Some(origin) = headers.get("origin")
        && !origin_is_loopback(origin, port)
    {
        return write_http(&mut write, 403, "text/plain", b"invalid origin\n").await;
    }
    if !headers
        .get("authorization")
        .is_some_and(|value| bearer_authorization_matches(value, token))
    {
        return write_http(&mut write, 401, "text/plain", b"unauthorized\n").await;
    }
    if path != "/mcp" {
        return write_http(&mut write, 404, "text/plain", b"not found\n").await;
    }
    if method != "POST" {
        return write_http(&mut write, 405, "text/plain", b"POST required\n").await;
    }
    if headers.get("content-type").is_none_or(|value| {
        !value
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .eq_ignore_ascii_case("application/json")
    }) {
        return write_http(
            &mut write,
            415,
            "text/plain",
            b"application/json required\n",
        )
        .await;
    }
    if !accepts_mcp_response(
        headers
            .get("accept")
            .map(String::as_str)
            .unwrap_or_default(),
    ) {
        return write_http(
            &mut write,
            406,
            "text/plain",
            b"Accept must include application/json and text/event-stream\n",
        )
        .await;
    }
    let Some(length) = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
    else {
        return write_http(
            &mut write,
            400,
            "text/plain",
            b"valid Content-Length required\n",
        )
        .await;
    };
    if length > MAX_FRAME {
        return write_http(&mut write, 413, "text/plain", b"request too large\n").await;
    }
    let mut body = vec![0; length];
    match timeout(Duration::from_secs(15), reader.read_exact(&mut body)).await {
        Err(_) => return write_http(&mut write, 408, "text/plain", b"request timeout\n").await,
        Ok(Err(_)) => {
            return write_http(&mut write, 400, "text/plain", b"incomplete HTTP body\n").await;
        }
        Ok(Ok(_)) => {}
    }
    let request = match serde_json::from_slice::<Value>(&body) {
        Ok(value) => value,
        Err(error) => {
            return write_http(
                &mut write,
                400,
                "application/json",
                &serde_json::to_vec(&json_rpc_error(Value::Null, -32700, &error.to_string()))
                    .unwrap_or_default(),
            )
            .await;
        }
    };
    let request_method = request["method"].as_str().unwrap_or_default();
    let request_id = request.get("id").cloned().unwrap_or(Value::Null);
    let request_version =
        request["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"].as_str();
    if let Some(version) = request_version.filter(|version| !MCP_VERSIONS.contains(version)) {
        let response = json!({"jsonrpc":"2.0","id":request_id,"error":{"code":-32022,"message":"Unsupported protocol version","data":{"supported":MCP_VERSIONS,"requested":version}}});
        return write_http_rpc(&mut write, 400, response).await;
    }
    let modern = request_version == Some(MODERN_MCP_VERSION)
        || headers
            .get("mcp-protocol-version")
            .is_some_and(|version| version == MODERN_MCP_VERSION);
    if request_version == Some(MODERN_MCP_VERSION)
        || headers
            .get("mcp-protocol-version")
            .is_some_and(|version| version == MODERN_MCP_VERSION)
    {
        let name = match request_method {
            "tools/call" => request["params"]["name"].as_str(),
            "resources/read" => request["params"]["uri"].as_str(),
            "prompts/get" => request["params"]["name"].as_str(),
            _ => None,
        };
        let mismatch = headers.get("mcp-protocol-version").map(String::as_str)
            != Some(MODERN_MCP_VERSION)
            || request_version != Some(MODERN_MCP_VERSION)
            || headers.get("mcp-method").map(String::as_str) != Some(request_method)
            || name.is_some_and(|name| headers.get("mcp-name").map(String::as_str) != Some(name))
            || matches!(
                request_method,
                "tools/call" | "resources/read" | "prompts/get"
            ) && name.is_none();
        if mismatch {
            let response = json_rpc_error(
                request_id,
                -32020,
                "HTTP MCP headers do not match request body",
            );
            return write_http_rpc(&mut write, 400, response).await;
        }
    }
    if modern
        && request["params"]["_meta"]["io.modelcontextprotocol/clientCapabilities"]
            .as_object()
            .is_none()
    {
        return write_http_rpc(
            &mut write,
            400,
            json_rpc_error(request_id, -32602, "invalid request metadata"),
        )
        .await;
    }
    if !modern && request_method != "initialize" {
        let version = headers
            .get("mcp-protocol-version")
            .map(String::as_str)
            .unwrap_or(HEADERLESS_HTTP_MCP_VERSION);
        if !LEGACY_MCP_VERSIONS.contains(&version) {
            return write_http_rpc(
                &mut write,
                400,
                json_rpc_error(
                    request_id,
                    -32602,
                    "unsupported MCP-Protocol-Version header",
                ),
            )
            .await;
        }
    }
    if modern
        && !matches!(
            request_method,
            "server/discover" | "ping" | "tools/list" | "tools/call"
        )
    {
        return write_http_rpc(
            &mut write,
            404,
            json_rpc_error(request_id, -32601, "method not found"),
        )
        .await;
    }
    if request_method == "notifications/cancelled" && request.get("id").is_none() {
        if let Some(request_id) = request["params"].get("requestId")
            && let Some(cancel) = pending.lock().await.get(&request_id.to_string())
        {
            cancel.send_replace(true);
        }
        return write_http(&mut write, 202, "text/plain", b"").await;
    }
    let notification = request.get("id").is_none();
    let request_key = request.get("id").map(Value::to_string);
    let (cancel, receiver) = tokio::sync::watch::channel(false);
    if let Some(key) = &request_key {
        let rejected = {
            let mut pending = pending.lock().await;
            if pending.contains_key(key) {
                Some(json_rpc_error(
                    request_id.clone(),
                    -32600,
                    "request ID is already in flight",
                ))
            } else if pending.len() >= MAX_PENDING_STDIO_REQUESTS {
                Some(json_rpc_error(
                    request_id.clone(),
                    -32000,
                    "too many in-flight MCP requests",
                ))
            } else {
                pending.insert(key.clone(), cancel.clone());
                None
            }
        };
        if let Some(response) = rejected {
            return write_http_rpc(&mut write, 200, response).await;
        }
    }
    if notification {
        write_http(&mut write, 202, "text/plain", b"").await
    } else {
        if let Err(error) = write_sse_head(&mut write).await {
            if let Some(key) = request_key {
                pending.lock().await.remove(&key);
            }
            return Err(error);
        }
        let computer_start =
            request_method == "tools/call" && request["params"]["name"] == "computer_start";
        let operation = handle_request(request, state.clone(), receiver);
        tokio::pin!(operation);
        let mut heartbeat = tokio::time::interval(Duration::from_secs(1));
        let mut peer_byte = [0u8; 1];
        let result = loop {
            tokio::select! {
                response = &mut operation => break match response {
                    Some(response) => {
                        let started_session = computer_start.then(|| response["result"]["structuredContent"]["session_id"].as_str()).flatten().map(str::to_owned);
                        match write_sse_event(&mut write, response).await {
                            Ok(()) => Ok(()),
                            Err(error) => {
                                if let Some(session_id) = started_session
                                    && let Err(cleanup) = stop_session(&json!({"session_id":session_id}), state.clone()).await
                                {
                                    eprintln!("computer-use-linux: HTTP response failed and desktop session cleanup failed: {cleanup}");
                                }
                                Err(error)
                            }
                        }
                    }
                    None => Ok(()),
                },
                _ = reader.read(&mut peer_byte) => {
                    // The request body is complete and this server closes each
                    // HTTP connection after one response. EOF means the SSE
                    // client has gone away; cancel now instead of waiting for
                    // the next keepalive write.
                    cancel.send_replace(true);
                    let _ = operation.as_mut().await;
                    break Ok(());
                },
                _ = heartbeat.tick() => if let Err(error) = write_sse_keepalive(&mut write).await {
                    cancel.send_replace(true);
                    let _ = operation.as_mut().await;
                    break Err(error);
                },
            }
        };
        if let Some(key) = request_key {
            pending.lock().await.remove(&key);
        }
        result?;
        write.shutdown().await.map_err(|error| error.to_string())
    }
}

pub(crate) fn accepts_mcp_response(value: &str) -> bool {
    let mut json = false;
    let mut events = false;
    for media_type in value.split(',') {
        let mut parts = media_type.trim().split(';');
        let name = parts.next().unwrap_or_default().trim();
        let quality = parts
            .find_map(|part| {
                let (name, value) = part.trim().split_once('=')?;
                name.trim()
                    .eq_ignore_ascii_case("q")
                    .then(|| valid_qvalue(value.trim()).unwrap_or(0.0))
            })
            .unwrap_or(1.0);
        if quality <= 0.0 {
            continue;
        }
        json |= name.eq_ignore_ascii_case("application/json");
        events |= name.eq_ignore_ascii_case("text/event-stream");
    }
    json && events
}

pub(crate) fn bearer_authorization_matches(value: &str, token: &str) -> bool {
    let Some((scheme, credential)) = value.split_once(' ') else {
        return false;
    };
    scheme.eq_ignore_ascii_case("bearer") && credential.trim_start_matches(' ') == token
}

pub(crate) fn valid_qvalue(value: &str) -> Option<f32> {
    if value == "0" {
        return Some(0.0);
    }
    if value == "1" {
        return Some(1.0);
    }
    let (whole, fraction) = value.split_once('.')?;
    if fraction.len() > 3 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    match whole {
        "0" => value.parse().ok(),
        "1" if fraction.bytes().all(|byte| byte == b'0') => Some(1.0),
        _ => None,
    }
}

pub(crate) async fn write_sse_head(
    stream: &mut tokio::net::tcp::OwnedWriteHalf,
) -> Result<(), String> {
    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache, no-transform\r\nConnection: close\r\nX-Accel-Buffering: no\r\n\r\n").await
        .map_err(|error| format!("starting MCP event stream: {error}"))?;
    stream
        .flush()
        .await
        .map_err(|error| format!("flushing MCP event stream: {error}"))
}

pub(crate) async fn write_sse_event(
    stream: &mut tokio::net::tcp::OwnedWriteHalf,
    value: Value,
) -> Result<(), String> {
    let value = serde_json::to_string(&value).map_err(|error| error.to_string())?;
    let event = format!("event: message\ndata: {value}\n\n");
    timeout(Duration::from_secs(5), stream.write_all(event.as_bytes()))
        .await
        .map_err(|_| "writing MCP event timed out".to_owned())?
        .map_err(|error| format!("writing MCP event: {error}"))?;
    stream
        .flush()
        .await
        .map_err(|error| format!("flushing MCP event: {error}"))
}

pub(crate) async fn write_sse_keepalive(
    stream: &mut tokio::net::tcp::OwnedWriteHalf,
) -> Result<(), String> {
    timeout(
        Duration::from_secs(5),
        stream.write_all(b": keep-alive\n\n"),
    )
    .await
    .map_err(|_| "writing MCP keepalive timed out".to_owned())?
    .map_err(|error| format!("writing MCP keepalive: {error}"))?;
    stream
        .flush()
        .await
        .map_err(|error| format!("flushing MCP keepalive: {error}"))
}

pub(crate) async fn write_http_rpc(
    stream: &mut tokio::net::tcp::OwnedWriteHalf,
    status: u16,
    response: Value,
) -> Result<(), String> {
    let body = serde_json::to_vec(&response).map_err(|error| error.to_string())?;
    write_http(stream, status, "application/json", &body).await
}

pub(crate) fn host_is_loopback(host: &str, port: u16) -> bool {
    host == format!("127.0.0.1:{port}") || host == format!("localhost:{port}")
}

pub(crate) fn origin_is_loopback(origin: &str, port: u16) -> bool {
    origin == format!("http://127.0.0.1:{port}") || origin == format!("http://localhost:{port}")
}

pub(crate) async fn write_http(
    stream: &mut tokio::net::tcp::OwnedWriteHalf,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> Result<(), String> {
    let status_text = match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        406 => "Not Acceptable",
        408 => "Request Timeout",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n",
        body.len()
    );
    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|error| error.to_string())?;
    stream
        .write_all(body)
        .await
        .map_err(|error| error.to_string())?;
    stream.shutdown().await.map_err(|error| error.to_string())
}

pub(crate) async fn handle_request(
    request: Value,
    state: SharedState,
    mut cancelled: tokio::sync::watch::Receiver<bool>,
) -> Option<Value> {
    let id = request.get("id").cloned();
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    if request.get("jsonrpc").and_then(Value::as_str) != Some("2.0") || method.is_empty() {
        return id.map(|id| json_rpc_error(id, -32600, "invalid JSON-RPC request"));
    }
    let id = id?;
    let request_version = params["_meta"]["io.modelcontextprotocol/protocolVersion"].as_str();
    if let Some(version) = request_version.filter(|version| !MCP_VERSIONS.contains(version)) {
        return Some(json!({
            "jsonrpc":"2.0",
            "id":id,
            "error":{"code":-32022,"message":"Unsupported protocol version","data":{"supported":MCP_VERSIONS,"requested":version}}
        }));
    }
    if method == "server/discover" || request_version == Some(MODERN_MCP_VERSION) {
        let meta = &params["_meta"];
        if request_version != Some(MODERN_MCP_VERSION)
            || !meta["io.modelcontextprotocol/clientCapabilities"].is_object()
        {
            return Some(json_rpc_error(id, -32602, "invalid request metadata"));
        }
    }
    if method == "initialize" && request_version == Some(MODERN_MCP_VERSION) {
        return Some(json_rpc_error(
            id,
            -32601,
            "initialize is only available to legacy MCP clients; use server/discover",
        ));
    }
    if !matches!(
        method,
        "initialize" | "server/discover" | "ping" | "tools/list" | "tools/call"
    ) {
        return Some(json_rpc_error(id, -32601, "method not found"));
    }
    if *cancelled.borrow() {
        return None;
    }
    let mut result = if method == "tools/call" && params["name"] == "computer_act" {
        let action_params = params.clone();
        let action_state = state.clone();
        let action_cancelled = cancelled.clone();
        let mut action_task = tokio::spawn(async move {
            match call_tool(&action_params, action_state, Some(action_cancelled)).await {
                Ok(value) => value,
                Err(error) => {
                    json!({"resultType":"complete","isError":true,"content":[{"type":"text","text":error}]})
                }
            }
        });
        tokio::select! {
            _ = cancelled.changed() => {
                let _ = action_task.await;
                return None;
            },
            result = &mut action_task => result.unwrap_or_else(|error| json!({"resultType":"complete","isError":true,"content":[{"type":"text","text":format!("computer_act worker failed: {error}")}]})),
        }
    } else {
        tokio::select! {
            _ = cancelled.changed() => return None,
            result = async { match method {
        "initialize" => json!({
            "protocolVersion": params.get("protocolVersion").and_then(Value::as_str)
                .filter(|version| LEGACY_MCP_VERSIONS.contains(version))
                .unwrap_or(LATEST_MCP_VERSION),
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "computer-use-linux", "version": env!("CARGO_PKG_VERSION")},
            "instructions": server_instructions()
        }),
        "server/discover" => json!({
            "resultType":"complete",
            "supportedVersions":MCP_VERSIONS,
            "capabilities":{"tools":{}},
            "_meta":{"io.modelcontextprotocol/serverInfo":{"name":"computer-use-linux","version":env!("CARGO_PKG_VERSION")}},
            "instructions":server_instructions(),
            "ttlMs":300000,
            "cacheScope":"public"
        }),
        "ping" => json!({"resultType":"complete"}),
        "tools/list" => json!({
            "resultType":"complete",
            "tools":tool_definitions(),
            "ttlMs":300000,
            "cacheScope":"public"
        }),
        "tools/call" => match call_tool(&params, state, None).await {
            Ok(value) => value,
            Err(error) => json!({"resultType":"complete","isError":true,"content":[{"type":"text","text":error}]}),
        },
        _ => unreachable!("unknown methods are rejected before dispatch"),
            }} => result,
        }
    };
    if request_version == Some(MODERN_MCP_VERSION)
        && method != "initialize"
        && let Some(object) = result.as_object_mut()
    {
        object
            .entry("resultType")
            .or_insert_with(|| json!("complete"));
        object.insert(
            "_meta".into(),
            json!({
                "io.modelcontextprotocol/serverInfo": {
                    "name":"computer-use-linux",
                    "version":env!("CARGO_PKG_VERSION")
                }
            }),
        );
    }
    Some(json!({"jsonrpc":"2.0", "id":id, "result":result}))
}

pub(crate) fn json_rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc":"2.0", "id":id, "error":{"code":code, "message":message}})
}
