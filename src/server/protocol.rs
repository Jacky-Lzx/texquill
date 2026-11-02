//! LSP stdio framing, lifecycle and ordered, cancellable dispatch.
use super::{
    client::{Client, Id},
    dispatch::RequestQueue,
    Backend,
};
use crate::rpc::{Error, ErrorCode, Result};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io,
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        Arc, Mutex,
    },
};
use tokio::{
    io::{
        AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt,
        BufReader,
    },
    sync::{mpsc, oneshot, Semaphore},
    task::JoinSet,
};

const MAX_HEADERS: usize = 8192;
const MAX_MESSAGE: usize = 64 * 1024 * 1024;
const READY: u8 = 2;
const SHUTTING_DOWN: u8 = 3;

fn invalid_data(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

async fn read_frame<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut total = 0;
    let mut length = None;
    loop {
        let mut line = Vec::new();
        let count = (&mut *reader)
            .take((MAX_HEADERS - total + 1) as u64)
            .read_until(b'\n', &mut line)
            .await?;
        if count == 0 {
            return if total == 0 {
                Ok(None)
            } else {
                Err(invalid_data("Incomplete LSP header"))
            };
        }
        total += count;
        if total > MAX_HEADERS {
            return Err(invalid_data("LSP headers exceed the size limit"));
        }
        let line = std::str::from_utf8(&line)
            .map_err(|_| invalid_data("Invalid LSP header"))?
            .trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| invalid_data("Invalid LSP header"))?;
        if name.eq_ignore_ascii_case("Content-Length") {
            if length.is_some() {
                return Err(invalid_data("Duplicate Content-Length"));
            }
            length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| invalid_data("Invalid Content-Length"))?,
            );
        }
    }
    let length = length
        .filter(|length| *length <= MAX_MESSAGE)
        .ok_or_else(|| invalid_data("Missing or excessive Content-Length"))?;
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await?;
    Ok(Some(body))
}

async fn write_message<W: AsyncWrite + Unpin>(writer: &mut W, message: Value) -> io::Result<()> {
    let bytes = serde_json::to_vec(&message)?;
    writer
        .write_all(format!("Content-Length: {}\r\n\r\n", bytes.len()).as_bytes())
        .await?;
    writer.write_all(&bytes).await?;
    writer.flush().await
}

fn response(id: Id, result: Result<Value>) -> Value {
    match result {
        Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
        Err(error) => json!({"jsonrpc":"2.0","id":id,"error":error}),
    }
}

struct PhaseReset(Arc<AtomicU8>);
impl Drop for PhaseReset {
    fn drop(&mut self) {
        let _ = self
            .0
            .compare_exchange(1, 0, Ordering::SeqCst, Ordering::SeqCst);
    }
}

async fn request(
    backend: &Backend,
    phase: &Arc<AtomicU8>,
    method: &str,
    params: Value,
) -> Result<Value> {
    if method == "initialize" {
        if phase
            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(Error::invalid_request());
        }
        let _reset = PhaseReset(phase.clone());
        let params = serde_json::from_value(params)
            .map_err(|error| Error::invalid_params(error.to_string()))?;
        let result = backend.initialize(params).await?;
        phase.store(READY, Ordering::SeqCst);
        return serde_json::to_value(result).map_err(|_| Error::internal_error());
    }
    match phase.load(Ordering::SeqCst) {
        0 | 1 => {
            return Err(Error {
                code: ErrorCode::ServerError(-32002),
                message: "Server not initialized".into(),
                data: None,
            })
        }
        SHUTTING_DOWN.. => return Err(Error::invalid_request()),
        _ => {}
    }
    if method == "shutdown" {
        if !params.is_null() && params != json!({}) {
            return Err(Error::invalid_params("shutdown takes no parameters"));
        }
        phase.store(SHUTTING_DOWN, Ordering::SeqCst);
        backend.shutdown().await?;
        return Ok(Value::Null);
    }
    macro_rules! call {
        ($handler:ident) => {{
            let params = serde_json::from_value(params)
                .map_err(|error| Error::invalid_params(error.to_string()))?;
            let value = backend.$handler(params).await?;
            serde_json::to_value(value).map_err(|_| Error::internal_error())
        }};
    }
    match method {
        "workspace/executeCommand" => call!(execute_command),
        "workspace/willRenameFiles" => call!(will_rename_files),
        "textDocument/diagnostic" => call!(diagnostic),
        "workspace/diagnostic" => call!(workspace_diagnostic),
        "textDocument/formatting" => call!(formatting),
        "textDocument/rangeFormatting" => call!(range_formatting),
        "textDocument/onTypeFormatting" => call!(on_type_formatting),
        "textDocument/prepareCallHierarchy" => call!(prepare_call_hierarchy),
        "callHierarchy/incomingCalls" => call!(incoming_calls),
        "callHierarchy/outgoingCalls" => call!(outgoing_calls),
        "textDocument/inlayHint" => call!(inlay_hint),
        "textDocument/completion" => call!(completion),
        "completionItem/resolve" => call!(completion_resolve),
        "textDocument/hover" => call!(hover),
        "textDocument/signatureHelp" => call!(signature_help),
        // Static TeX symbols use their source definition as the declaration.
        "textDocument/declaration" | "textDocument/definition" => call!(goto_definition),
        "textDocument/references" => call!(references),
        "textDocument/prepareRename" => call!(prepare_rename),
        "textDocument/rename" => call!(rename),
        "textDocument/documentHighlight" => call!(document_highlight),
        "textDocument/linkedEditingRange" => call!(linked_editing_range),
        "textDocument/selectionRange" => call!(selection_range),
        "textDocument/codeAction" => call!(code_action),
        "codeAction/resolve" => call!(code_action_resolve),
        "textDocument/codeLens" => call!(code_lens),
        "textDocument/documentSymbol" => call!(document_symbol),
        "workspace/symbol" => call!(symbol),
        "textDocument/foldingRange" => call!(folding_range),
        "textDocument/documentColor" => call!(document_color),
        "textDocument/colorPresentation" => call!(color_presentation),
        "textDocument/semanticTokens/full" => call!(semantic_tokens_full),
        "textDocument/semanticTokens/full/delta" => call!(semantic_tokens_full_delta),
        "textDocument/semanticTokens/range" => call!(semantic_tokens_range),
        "textDocument/documentLink" => call!(document_link),
        _ => Err(Error::method_not_found()),
    }
}

async fn notification(
    backend: &Backend,
    phase: &AtomicU8,
    initialized: &AtomicBool,
    method: &str,
    params: Value,
) {
    if phase.load(Ordering::SeqCst) != READY {
        return;
    }
    if method == "initialized" && initialized.swap(true, Ordering::SeqCst) {
        return;
    }
    macro_rules! notify {
        ($handler:ident) => {
            if let Ok(params) = serde_json::from_value(params) {
                backend.$handler(params).await;
            }
        };
    }
    match method {
        "initialized" => notify!(initialized),
        "textDocument/didOpen" => notify!(did_open),
        "textDocument/didChange" => notify!(did_change),
        "textDocument/didSave" => notify!(did_save),
        "textDocument/didClose" => notify!(did_close),
        "workspace/didChangeWatchedFiles" => notify!(did_change_watched_files),
        "workspace/didChangeConfiguration" => notify!(did_change_configuration),
        "workspace/didCreateFiles" => notify!(did_create_files),
        "workspace/didDeleteFiles" => notify!(did_delete_files),
        "workspace/didRenameFiles" => notify!(did_rename_files),
        "workspace/didChangeWorkspaceFolders" => notify!(did_change_workspace_folders),
        "window/workDoneProgress/cancel" => notify!(cancel_progress),
        _ => {}
    }
}

type Pending = Arc<Mutex<HashMap<Id, Option<oneshot::Sender<()>>>>>;
struct Incoming {
    id: Id,
    pending: Pending,
}
impl Drop for Incoming {
    fn drop(&mut self) {
        self.pending.lock().unwrap().remove(&self.id);
    }
}

/// Serve one editor connection. Responses and cancellation notifications bypass
/// source ordering, so a handler waiting for the editor cannot block its reply.
pub async fn serve<R, W>(input: R, output: W) -> io::Result<i32>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (client, mut outgoing) = Client::new();
    let backend = Arc::new(Backend::new(client.clone()));
    let (stop_writer, mut writer_stopped) = oneshot::channel();
    let mut writer = tokio::spawn(async move {
        let mut output = output;
        loop {
            tokio::select! {
                message = outgoing.recv() => match message {
                    Some(message) => write_message(&mut output, message).await?,
                    None => break,
                },
                _ = &mut writer_stopped => {
                    outgoing.close();
                    while let Some(message) = outgoing.recv().await { write_message(&mut output, message).await?; }
                    break;
                }
            }
        }
        output.shutdown().await
    });
    let mut reader = BufReader::new(input);
    let mut queue = RequestQueue::default();
    let pending = Pending::default();
    let phase = Arc::new(AtomicU8::new(0));
    let initialized = Arc::new(AtomicBool::new(false));
    let slots = Arc::new(Semaphore::new(32));
    let mut handlers = JoinSet::new();
    let (exit, mut exited) = mpsc::channel(1);
    let mut writer_finished = false;
    let mut exit_status = None;
    let result = 'connection: loop {
        let body = {
            // A framing read can consume a partial header/body before yielding.
            // Keep it alive while completed handlers are reaped; restarting it
            // after another select branch would corrupt a fragmented message.
            let frame = read_frame(&mut reader);
            tokio::pin!(frame);
            loop {
                tokio::select! {
                    body = &mut frame => match body {
                        Ok(Some(body)) => break body,
                        Ok(None) => break 'connection Ok(exit_status.unwrap_or(0)),
                        Err(error) => break 'connection Err(error),
                    },
                    status = exited.recv() => break 'connection Ok(status.unwrap_or(1)),
                    result = &mut writer => {
                        writer_finished = true;
                        break 'connection result.map_err(io::Error::other).and_then(|result| result).map(|()| 0);
                    },
                    Some(result) = handlers.join_next(), if !handlers.is_empty() => {
                        if let Err(error) = result { break 'connection Err(io::Error::other(error)); }
                    }
                }
            }
        };
        let message: Value = match serde_json::from_slice(&body) {
            Ok(value) => value,
            Err(_) => {
                let error = Error {
                    code: ErrorCode::ParseError,
                    message: "Parse error".into(),
                    data: None,
                };
                let _ = client.send(response(Id::Null, Err(error))).await;
                continue;
            }
        };
        if !message.is_object() || message["jsonrpc"] != "2.0" {
            let _ = client
                .send(response(Id::Null, Err(Error::invalid_request())))
                .await;
            continue;
        }
        if !message.as_object().unwrap().contains_key("method") {
            if message.get("result").is_none() && message.get("error").is_none() {
                let id = serde_json::from_value(message["id"].clone()).unwrap_or(Id::Null);
                let _ = client
                    .send(response(id, Err(Error::invalid_request())))
                    .await;
                continue;
            }
            // Late responses have no waiter after timeout/cancellation. Safely
            // ignore them, including a response arriving during shutdown.
            if let Ok(id) = serde_json::from_value::<Id>(message["id"].clone()) {
                if message.get("error").is_some() && message.get("result").is_none() {
                    if let Ok(error) = serde_json::from_value(message["error"].clone()) {
                        client.respond(&id, Err(error));
                    }
                } else if let Some(result) = message
                    .get("result")
                    .filter(|_| message.get("error").is_none())
                {
                    client.respond(&id, Ok(result.clone()));
                }
            }
            continue;
        }
        let id = match message.get("id") {
            Some(value) => match serde_json::from_value::<Id>(value.clone()) {
                Ok(id) => Some(id),
                Err(_) => {
                    let _ = client
                        .send(response(Id::Null, Err(Error::invalid_request())))
                        .await;
                    continue;
                }
            },
            None => None,
        };
        let Some(method) = message["method"].as_str() else {
            let _ = client
                .send(response(
                    id.unwrap_or(Id::Null),
                    Err(Error::invalid_request()),
                ))
                .await;
            continue;
        };
        // EOF can follow this frame before its queued handler is polled.
        // Preserve the explicit exit status even in that scheduling race.
        if method == "exit" && id.is_none() {
            exit_status = Some(i32::from(!backend.is_shutdown()));
        }
        if method == "$/cancelRequest" && id.is_none() {
            if let Ok(id) = serde_json::from_value::<Id>(message["params"]["id"].clone()) {
                if let Some(sender) = pending.lock().unwrap().get_mut(&id).and_then(Option::take) {
                    let _ = sender.send(());
                }
            }
            continue;
        }
        if handlers.len() >= 1024 {
            break Err(invalid_data("LSP request backlog exceeded"));
        }
        let params = message.get("params").cloned().unwrap_or_else(|| {
            if method == "shutdown" {
                Value::Null
            } else {
                json!({})
            }
        });
        let method = method.to_owned();
        let backend = backend.clone();
        let client = client.clone();
        let phase = phase.clone();
        let initialized = initialized.clone();
        let exit = exit.clone();
        let slots = slots.clone();
        if let Some(id) = id {
            let (cancel, mut cancelled) = oneshot::channel();
            let accepted = {
                let mut pending = pending.lock().unwrap();
                // The handler backlog and worker slots already bound queued
                // and running work. A valid editing burst is not an invalid
                // JSON-RPC request just because 128 requests are outstanding.
                if pending.contains_key(&id) {
                    false
                } else {
                    pending.insert(id.clone(), Some(cancel));
                    true
                }
            };
            if !accepted {
                let _ = client
                    .send(response(id, Err(Error::invalid_request())))
                    .await;
                continue;
            }
            let incoming = Incoming {
                id: id.clone(),
                pending: pending.clone(),
            };
            let turn = queue.turn();
            handlers.spawn(async move {
                tokio::pin!(turn);
                let turn = tokio::select! {
                    biased;
                    _ = &mut cancelled, if method != "shutdown" => {
                        // Cancel before waiting for earlier snapshots or a worker
                        // slot. Retain an empty turn so later document updates
                        // cannot overtake the still-running predecessor.
                        let _ = client.send(response(id, Err(Error::request_cancelled()))).await;
                        drop(incoming);
                        drop(turn.await);
                        return;
                    }
                    turn = &mut turn => turn,
                };
                let work = async {
                    // Pull reports already serialize expensive work through
                    // their analysis cache. Waiting reports must not occupy
                    // every interactive slot and starve completion or saves.
                    let permit = if matches!(
                        method.as_str(),
                        "textDocument/diagnostic" | "workspace/diagnostic"
                    ) {
                        None
                    } else {
                        Some(slots.acquire_owned().await.unwrap())
                    };
                    super::dispatch::with_capacity(
                        permit,
                        request(&backend, &phase, &method, params),
                    )
                    .await
                };
                let result = turn
                    .run(async {
                        if method == "shutdown" {
                            work.await
                        } else {
                            tokio::select! {
                                biased;
                                _ = cancelled => Err(Error::request_cancelled()),
                                result = work => result,
                            }
                        }
                    })
                    .await;
                let _ = client.send(response(id, result)).await;
                drop(incoming);
            });
        } else {
            let change = (method == "textDocument/didChange").then(|| backend.queue_change());
            handlers.spawn(queue.ordered(async move {
                if method == "exit" {
                    let _ = exit.send(i32::from(!backend.is_shutdown())).await;
                    return;
                }
                let permit = slots.acquire_owned().await.unwrap();
                super::dispatch::with_capacity(
                    Some(permit),
                    notification(&backend, &phase, &initialized, &method, params),
                )
                .await;
                if change.is_some() {
                    drop(change);
                    // Flush once for the newest received source revision.
                    // An intervening non-cancelled request can still force a
                    // fresh ordered snapshot through WorkspaceStore::read.
                    backend.finish_changes().await;
                }
            }));
        }
    };
    // Signal disconnection before dropping handlers so subprocess guards cancel
    // promptly. Client close also wakes requests awaiting an editor response.
    let cleanup = backend.disconnect();
    client.close();
    handlers.abort_all();
    while handlers.join_next().await.is_some() {}
    cleanup.await;
    let _ = stop_writer.send(());
    if !writer_finished {
        match tokio::time::timeout(std::time::Duration::from_secs(1), &mut writer).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(error))) => return Err(error),
            Ok(Err(error)) => return Err(io::Error::other(error)),
            Err(_) => writer.abort(),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{DuplexStream, ReadHalf, WriteHalf};

    type Reader = BufReader<ReadHalf<DuplexStream>>;

    fn connection() -> (
        Reader,
        WriteHalf<DuplexStream>,
        tokio::task::JoinHandle<io::Result<i32>>,
    ) {
        let (client, server) = tokio::io::duplex(16 * 1024);
        let (input, output) = tokio::io::split(server);
        let task = tokio::spawn(serve(input, output));
        let (input, output) = tokio::io::split(client);
        (BufReader::new(input), output, task)
    }

    async fn reply(reader: &mut Reader, id: i64) -> Value {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let bytes = read_frame(reader).await.unwrap().unwrap();
                let value: Value = serde_json::from_slice(&bytes).unwrap();
                if value["id"] == id {
                    return value;
                }
            }
        })
        .await
        .unwrap()
    }

    fn initialize(id: i64) -> Value {
        json!({"jsonrpc":"2.0","id":id,"method":"initialize","params":{
            "capabilities":{},"initializationOptions":{"distribution":{"enabled":false}}
        }})
    }

    #[tokio::test]
    async fn fragmented_frames_survive_handler_completion_and_count_utf8_bytes() {
        let (mut reader, mut writer, task) = connection();
        write_message(&mut writer, initialize(1)).await.unwrap();
        let message =
            json!({"jsonrpc":"2.0","id":2,"method":"unknown","params":{"text":"章节 😀"}});
        let bytes = serde_json::to_vec(&message).unwrap();
        let mut packet = format!("Content-Length: {}\r\n\r\n", bytes.len()).into_bytes();
        packet.extend(bytes);
        writer.write_all(&packet[..10]).await.unwrap();
        assert!(reply(&mut reader, 1).await["result"]["capabilities"].is_object());
        // Let the initialize task finish while the next header is incomplete.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        for chunk in packet[10..].chunks(3) {
            writer.write_all(chunk).await.unwrap();
            tokio::task::yield_now().await;
        }
        assert_eq!(reply(&mut reader, 2).await["error"]["code"], -32601);
        write_message(&mut writer, json!({"jsonrpc":"2.0","method":"exit"}))
            .await
            .unwrap();
        assert_eq!(task.await.unwrap().unwrap(), 1);
    }

    #[tokio::test]
    async fn lifecycle_errors_are_recoverable_and_exit_status_survives_immediate_eof() {
        for shutdown in [false, true] {
            let (mut reader, mut writer, task) = connection();
            write_message(
                &mut writer,
                json!({"jsonrpc":"2.0","id":1,"method":"workspace/symbol","params":{"query":""}}),
            )
            .await
            .unwrap();
            assert_eq!(reply(&mut reader, 1).await["error"]["code"], -32002);
            write_message(
                &mut writer,
                json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":{}}),
            )
            .await
            .unwrap();
            assert_eq!(reply(&mut reader, 2).await["error"]["code"], -32602);
            write_message(&mut writer, initialize(3)).await.unwrap();
            assert!(reply(&mut reader, 3).await["result"]["capabilities"].is_object());
            write_message(&mut writer, initialize(4)).await.unwrap();
            assert_eq!(reply(&mut reader, 4).await["error"]["code"], -32600);
            if shutdown {
                write_message(
                    &mut writer,
                    json!({"jsonrpc":"2.0","id":5,"method":"shutdown"}),
                )
                .await
                .unwrap();
                assert_eq!(reply(&mut reader, 5).await["result"], Value::Null);
                write_message(&mut writer,json!({"jsonrpc":"2.0","id":6,"method":"workspace/symbol","params":{"query":""}})).await.unwrap();
                assert_eq!(reply(&mut reader, 6).await["error"]["code"], -32600);
            }
            write_message(&mut writer, json!({"jsonrpc":"2.0","method":"exit"}))
                .await
                .unwrap();
            writer.shutdown().await.unwrap();
            assert_eq!(task.await.unwrap().unwrap(), i32::from(!shutdown));
        }
    }

    #[tokio::test]
    async fn invalid_json_and_requests_do_not_poison_the_next_frame() {
        let (mut reader, mut writer, task) = connection();
        writer
            .write_all(b"Content-Length: 1\r\n\r\n{")
            .await
            .unwrap();
        let error: Value =
            serde_json::from_slice(&read_frame(&mut reader).await.unwrap().unwrap()).unwrap();
        assert_eq!(error["error"]["code"], -32700);
        write_message(&mut writer, json!({"jsonrpc":"2.0","id":9}))
            .await
            .unwrap();
        assert_eq!(reply(&mut reader, 9).await["error"]["code"], -32600);
        write_message(&mut writer, initialize(10)).await.unwrap();
        assert!(reply(&mut reader, 10).await["result"]["capabilities"].is_object());
        write_message(
            &mut writer,
            json!({"jsonrpc":"2.0","id":"late","result":null}),
        )
        .await
        .unwrap();
        write_message(
            &mut writer,
            json!({"jsonrpc":"2.0","method":"$/cancelRequest","params":{"id":999}}),
        )
        .await
        .unwrap();
        write_message(&mut writer, json!({"jsonrpc":"2.0","method":"exit"}))
            .await
            .unwrap();
        assert_eq!(task.await.unwrap().unwrap(), 1);
    }

    #[tokio::test]
    async fn framing_rejects_ambiguous_excessive_and_truncated_messages() {
        for frame in [
            "Content-Length: 0\r\nContent-Length: 0\r\n\r\n".to_owned(),
            "Content-Length: -1\r\n\r\n".to_owned(),
            format!("Content-Length: {}\r\n\r\n", MAX_MESSAGE + 1),
            "Content-Type: application/vscode-jsonrpc\r\n\r\n".to_owned(),
            "Content-Length: 4\r\n\r\n{}".to_owned(),
            format!("X-Header: {}\r\n\r\n", "a".repeat(MAX_HEADERS)),
        ] {
            let mut reader = BufReader::new(frame.as_bytes());
            assert!(read_frame(&mut reader).await.is_err());
        }
    }
}
