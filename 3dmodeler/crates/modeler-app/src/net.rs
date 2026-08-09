//! Cross-platform HTTP for the AI providers: fire a request, poll for the
//! result from the render loop (the same "start now, poll later" pattern as
//! the io.rs file dialogs — the render loop must never block).
//!
//! Native: a background thread + ureq. Browser: `fetch` via
//! wasm-bindgen-futures. Either way the outcome lands in an mpsc channel the
//! frame loop drains.

use modeler_ai::HttpRequest;
use std::sync::mpsc::{channel, Receiver, TryRecvError};

/// Response body, or a transport/HTTP error message. Non-2xx responses with a
/// readable body come back as `Ok(body)` — provider parsers turn API error
/// JSON into friendly messages (they carry more detail than status codes).
pub type HttpOutcome = Result<String, String>;

/// Binary response body (images, zip packs, …).
pub type BytesOutcome = Result<Vec<u8>, String>;

pub struct HttpTask {
    receiver: Receiver<HttpOutcome>,
    finished: bool,
}

impl HttpTask {
    /// The result, once. Returns None while in flight; a dropped worker
    /// surfaces as an error instead of hanging forever.
    pub fn poll(&mut self) -> Option<HttpOutcome> {
        if self.finished {
            return None;
        }
        match self.receiver.try_recv() {
            Ok(outcome) => {
                self.finished = true;
                Some(outcome)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.finished = true;
                Some(Err("request worker vanished".into()))
            }
        }
    }
}

pub struct BytesTask {
    receiver: Receiver<BytesOutcome>,
    finished: bool,
}

impl BytesTask {
    pub fn poll(&mut self) -> Option<BytesOutcome> {
        if self.finished {
            return None;
        }
        match self.receiver.try_recv() {
            Ok(outcome) => {
                self.finished = true;
                Some(outcome)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.finished = true;
                Some(Err("request worker vanished".into()))
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub fn fetch(request: HttpRequest) -> HttpTask {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(180))
            .build();
        let mut call = match request.method {
            "GET" => agent.get(&request.url),
            _ => agent.post(&request.url),
        };
        for (name, value) in &request.headers {
            call = call.set(name, value);
        }
        let response = match request.body {
            Some(body) => call.send_string(&body),
            None => call.call(),
        };
        let outcome = match response {
            Ok(response) => response
                .into_string()
                .map_err(|e| format!("response read failed: {e}")),
            // HTTP errors still carry an API error body worth parsing
            Err(ureq::Error::Status(code, response)) => match response.into_string() {
                Ok(body) if !body.trim().is_empty() => Ok(body),
                _ => Err(format!("HTTP {code}")),
            },
            Err(e) => Err(format!("request failed: {e}")),
        };
        let _ = tx.send(outcome);
    });
    HttpTask { receiver: rx, finished: false }
}

#[cfg(target_arch = "wasm32")]
pub fn fetch(request: HttpRequest) -> HttpTask {
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;

    let (tx, rx) = channel();
    wasm_bindgen_futures::spawn_local(async move {
        let outcome = fetch_async(request).await;
        let _ = tx.send(outcome);
    });
    return HttpTask { receiver: rx, finished: false };

    async fn fetch_async(request: HttpRequest) -> HttpOutcome {
        use wasm_bindgen::JsValue;

        let init = web_sys::RequestInit::new();
        init.set_method(request.method);
        init.set_mode(web_sys::RequestMode::Cors);
        if let Some(body) = &request.body {
            init.set_body(&JsValue::from_str(body));
        }
        let js_request = web_sys::Request::new_with_str_and_init(&request.url, &init)
            .map_err(|_| "bad request URL".to_string())?;
        for (name, value) in &request.headers {
            let _ = js_request.headers().set(name, value);
        }
        let window = web_sys::window().ok_or("no window")?;
        let response = JsFuture::from(window.fetch_with_request(&js_request))
            .await
            .map_err(|_| {
                "fetch failed (network or CORS — some providers block browser calls; \
                 the native app has no such limits)"
                    .to_string()
            })?;
        let response: web_sys::Response =
            response.dyn_into().map_err(|_| "bad fetch response".to_string())?;
        let status = response.status();
        let text = JsFuture::from(
            response.text().map_err(|_| "response read failed".to_string())?,
        )
        .await
        .map_err(|_| "response read failed".to_string())?;
        let body = text.as_string().unwrap_or_default();
        if body.trim().is_empty() && !(200..300).contains(&status) {
            return Err(format!("HTTP {status}"));
        }
        Ok(body)
    }
}

/// Download raw bytes (GET). Used for PBR map images and thumbnails.
#[cfg(not(target_arch = "wasm32"))]
pub fn fetch_bytes(url: &str, user_agent: &str) -> BytesTask {
    use std::io::Read;
    let url = url.to_string();
    let user_agent = user_agent.to_string();
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(180))
            .build();
        let response = agent
            .get(&url)
            .set("User-Agent", &user_agent)
            .call();
        let outcome = match response {
            Ok(response) => {
                let mut buf = Vec::new();
                match response.into_reader().read_to_end(&mut buf) {
                    Ok(_) => Ok(buf),
                    Err(e) => Err(format!("response read failed: {e}")),
                }
            }
            Err(ureq::Error::Status(code, _)) => Err(format!("HTTP {code}")),
            Err(e) => Err(format!("request failed: {e}")),
        };
        let _ = tx.send(outcome);
    });
    BytesTask {
        receiver: rx,
        finished: false,
    }
}

#[cfg(target_arch = "wasm32")]
pub fn fetch_bytes(url: &str, _user_agent: &str) -> BytesTask {
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;

    let url = url.to_string();
    let (tx, rx) = channel();
    wasm_bindgen_futures::spawn_local(async move {
        let outcome = fetch_bytes_async(&url).await;
        let _ = tx.send(outcome);
    });
    return BytesTask {
        receiver: rx,
        finished: false,
    };

    async fn fetch_bytes_async(url: &str) -> BytesOutcome {
        use js_sys::Uint8Array;
        use wasm_bindgen::JsValue;

        let init = web_sys::RequestInit::new();
        init.set_method("GET");
        init.set_mode(web_sys::RequestMode::Cors);
        let js_request = web_sys::Request::new_with_str_and_init(url, &init)
            .map_err(|_| "bad request URL".to_string())?;
        let window = web_sys::window().ok_or("no window")?;
        let response = JsFuture::from(window.fetch_with_request(&js_request))
            .await
            .map_err(|_| {
                "fetch failed (network or CORS — some texture hosts block browser calls; \
                 the native app has no such limits)"
                    .to_string()
            })?;
        let response: web_sys::Response =
            response.dyn_into().map_err(|_| "bad fetch response".to_string())?;
        if !(200..300).contains(&response.status()) {
            return Err(format!("HTTP {}", response.status()));
        }
        let buf = JsFuture::from(
            response
                .array_buffer()
                .map_err(|_| "response read failed".to_string())?,
        )
        .await
        .map_err(|_| "response read failed".to_string())?;
        let arr = Uint8Array::new(&buf);
        let mut bytes = vec![0u8; arr.length() as usize];
        arr.copy_to(&mut bytes);
        let _ = JsValue::NULL; // silence unused in some builds
        Ok(bytes)
    }
}

/// Fire a GET that returns a UTF-8 body (API JSON). Sets User-Agent for
/// APIs that require one (e.g. Poly Haven).
pub fn fetch_get(url: &str, user_agent: &str) -> HttpTask {
    fetch(HttpRequest {
        method: "GET",
        url: url.to_string(),
        headers: vec![("User-Agent".into(), user_agent.into())],
        body: None,
    })
}
