//! Request/Reply context objects
//!
//! Provides a unified context for both Fastify and Hono style request handling.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use perry_runtime::{js_string_from_bytes, BufferHeader, JSValue, StringHeader};

extern "C" {
    /// Probe the runtime's BUFFER_REGISTRY for a NaN-boxed POINTER_TAG
    /// pointer. Returns 1 for `Buffer` / `Uint8Array`, 0 otherwise.
    /// Used to distinguish binary payloads from `StringHeader`-shaped
    /// objects when building response bodies (#1120).
    fn js_buffer_is_buffer(ptr: i64) -> i32;
}

/// Body type used by `build_response_body` / `jsvalue_to_response_body`
/// so the caller can default `content-type` to `application/octet-stream`
/// when the handler returned a Buffer and didn't pin a type via
/// `reply.type(...)` (#1120).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BodyKind {
    Binary,
    TextOrJson,
}

/// Probe BUFFER_REGISTRY for a POINTER_TAG'd value; returns the raw
/// payload bytes if it's a `Buffer` / `Uint8Array`. Shared with
/// `server.rs::build_response_body` (#1120).
pub(crate) unsafe fn extract_buffer_bytes(value: f64) -> Option<Vec<u8>> {
    let v = JSValue::from_bits(value.to_bits());
    if !v.is_pointer() {
        return None;
    }
    let raw = (value.to_bits() & 0x0000_FFFF_FFFF_FFFF) as i64;
    if js_buffer_is_buffer(raw) == 0 {
        return None;
    }
    let buf = raw as *const BufferHeader;
    if buf.is_null() {
        return None;
    }
    let len = (*buf).length as usize;
    let data = (buf as *const u8).add(std::mem::size_of::<BufferHeader>());
    Some(std::slice::from_raw_parts(data, len).to_vec())
}

use crate::common::{get_handle, get_handle_mut, Handle};

// Declare perry-runtime's JSON parser (defined in perry-runtime with #[no_mangle])
extern "C" {
    fn js_json_parse(text_ptr: *const StringHeader) -> u64; // returns NaN-boxed JSValue bits
}

/// Context ID counter
static CONTEXT_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Helper to extract string from StringHeader pointer
pub(crate) unsafe fn string_from_header(ptr: *const StringHeader) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    let len = (*ptr).byte_len as usize;
    let data_ptr = (ptr as *const u8).add(std::mem::size_of::<StringHeader>());
    let bytes = std::slice::from_raw_parts(data_ptr, len);
    Some(String::from_utf8_lossy(bytes).to_string())
}

/// Helper to extract string from raw i64 pointer (NaN-boxed or raw)
pub(crate) unsafe fn string_from_nanboxed(value: i64) -> Option<String> {
    let ptr = perry_runtime::js_get_string_pointer_unified(f64::from_bits(value as u64));
    if ptr == 0 {
        return None;
    }
    string_from_header(ptr as *const StringHeader)
}

/// Unified context for both Fastify and Hono styles
pub struct FastifyContext {
    /// Unique context ID
    pub id: u64,
    /// Request ID (from underlying server)
    pub request_id: u64,
    /// HTTP method
    pub method: String,
    /// Request URL path
    pub url: String,
    /// Query string (without leading ?)
    pub query_string: String,
    /// Extracted route parameters
    pub params: HashMap<String, String>,
    /// Request headers
    pub headers: HashMap<String, String>,
    /// Request body (raw bytes)
    pub body: Option<Vec<u8>>,

    // Reply state
    /// Response status code
    pub status_code: u16,
    /// Response headers
    pub response_headers: Vec<(String, String)>,
    /// Whether response has been sent
    pub sent: bool,
    /// Response body (if built incrementally)
    pub response_body: Option<Vec<u8>>,
    /// User data attached by auth middleware (NaN-boxed JSValue bits)
    pub user_data: u64,
    /// PR 4 (bottleneck #4): cached `req.params` JS object built on
    /// first access. 0 means uncached. Cached value is a NaN-boxed
    /// pointer (top 16 bits = 0x7FFD), so 0 never collides with a
    /// valid cache entry. AtomicU64 (not Cell) because FastifyContext
    /// must be Send+Sync for the global DashMap handle registry —
    /// Cell isn't Sync. Reset by `FastifyContext::new` (each request
    /// gets a fresh context, no inter-request leak).
    pub params_object_cache: std::sync::atomic::AtomicU64,
    /// PR 4: cached `req.query` JS object. Same encoding as
    /// params_object_cache.
    pub query_object_cache: std::sync::atomic::AtomicU64,
    /// PR 5 (bottleneck #7): cached `req.headers` JS object. Same
    /// encoding as params_object_cache. Headers are read once or
    /// twice per request in typical yammer-web-server code (CSP
    /// nonce path reads content-type, host, x-forwarded-for) so
    /// caching the constructed object after the first read avoids
    /// rebuilding it for each subsequent read.
    pub headers_object_cache: std::sync::atomic::AtomicU64,
}

impl FastifyContext {
    /// Create a new context
    pub fn new(
        request_id: u64,
        method: String,
        url: String,
        headers: HashMap<String, String>,
        body: Option<Vec<u8>>,
        params: HashMap<String, String>,
    ) -> Self {
        // Parse query string from URL
        let (path, query_string) = match url.split_once('?') {
            Some((p, q)) => (p.to_string(), q.to_string()),
            None => (url.clone(), String::new()),
        };

        const TAG_UNDEFINED: u64 = 0x7FFC_0000_0000_0001;
        Self {
            id: CONTEXT_ID_COUNTER.fetch_add(1, Ordering::SeqCst),
            request_id,
            method,
            url: path,
            query_string,
            params,
            headers,
            body,
            status_code: 200,
            response_headers: Vec::new(),
            sent: false,
            response_body: None,
            user_data: TAG_UNDEFINED,
            params_object_cache: std::sync::atomic::AtomicU64::new(0),
            query_object_cache: std::sync::atomic::AtomicU64::new(0),
            headers_object_cache: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Get a request header value
    pub fn get_header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_lowercase()).map(|s| s.as_str())
    }

    /// Get a route parameter value
    pub fn get_param(&self, name: &str) -> Option<&str> {
        self.params.get(name).map(|s| s.as_str())
    }

    /// Get a query parameter value
    pub fn get_query_param(&self, name: &str) -> Option<String> {
        for pair in self.query_string.split('&') {
            if let Some((key, value)) = pair.split_once('=') {
                if key == name {
                    return Some(urlencoding_decode(value));
                }
            }
        }
        None
    }

    /// Get all query parameters as a map
    pub fn get_query_params(&self) -> HashMap<String, String> {
        let mut params = HashMap::new();
        for pair in self.query_string.split('&') {
            if let Some((key, value)) = pair.split_once('=') {
                params.insert(key.to_string(), urlencoding_decode(value));
            }
        }
        params
    }

    /// Get body as string
    pub fn body_string(&self) -> Option<String> {
        self.body
            .as_ref()
            .map(|b| String::from_utf8_lossy(b).to_string())
    }

    /// Set response status code
    pub fn set_status(&mut self, code: u16) {
        self.status_code = code;
    }

    /// Add a response header
    pub fn add_header(&mut self, name: &str, value: &str) {
        self.response_headers
            .push((name.to_string(), value.to_string()));
    }
}

/// Simple URL decoding
fn urlencoding_decode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '%' {
            let hex: String = chars.by_ref().take(2).collect();
            if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                result.push(byte as char);
            } else {
                result.push('%');
                result.push_str(&hex);
            }
        } else if c == '+' {
            result.push(' ');
        } else {
            result.push(c);
        }
    }

    result
}

// ============================================================================
// Request Methods FFI
// ============================================================================

/// Get request method
#[no_mangle]
pub unsafe extern "C" fn js_fastify_req_method(ctx_handle: Handle) -> *mut StringHeader {
    if let Some(ctx) = get_handle::<FastifyContext>(ctx_handle) {
        return js_string_from_bytes(ctx.method.as_ptr(), ctx.method.len() as u32);
    }
    std::ptr::null_mut()
}

/// Get request URL path (includes query string from ctx.url)
#[no_mangle]
pub unsafe extern "C" fn js_fastify_req_url(ctx_handle: Handle) -> *mut StringHeader {
    if let Some(ctx) = get_handle::<FastifyContext>(ctx_handle) {
        return js_string_from_bytes(ctx.url.as_ptr(), ctx.url.len() as u32);
    }
    std::ptr::null_mut()
}

/// Get all route params as JSON object
#[no_mangle]
pub unsafe extern "C" fn js_fastify_req_params(ctx_handle: Handle) -> *mut StringHeader {
    if let Some(ctx) = get_handle::<FastifyContext>(ctx_handle) {
        if let Ok(json) = serde_json::to_string(&ctx.params) {
            return js_string_from_bytes(json.as_ptr(), json.len() as u32);
        }
    }
    std::ptr::null_mut()
}

/// Get all route params as a JavaScript object (NaN-boxed pointer).
///
/// PR 4 (bottleneck #4): caches the constructed JS object on the
/// FastifyContext on first access — subsequent reads return the
/// cached pointer with no allocation. Per-request invariant: each
/// new FastifyContext starts with `params_object_cache = 0`, so the
/// cache is naturally scoped to one request (and dropped along with
/// the context by PR 1.5's drop_handle at dispatch tail).
#[no_mangle]
pub unsafe extern "C" fn js_fastify_req_params_object(ctx_handle: Handle) -> f64 {
    use perry_runtime::{
        js_array_alloc, js_array_push_f64, js_nanbox_string, js_object_alloc,
        js_object_set_field_f64, js_object_set_keys,
    };
    use std::sync::atomic::Ordering;

    if let Some(ctx) = get_handle::<FastifyContext>(ctx_handle) {
        // Fast path: cached pointer from a prior call in the same
        // request. 0 means uncached; valid cache entries are NaN-boxed
        // pointers (top 16 bits = 0x7FFD) so 0 never collides.
        let cached = ctx.params_object_cache.load(Ordering::Acquire);
        if cached != 0 {
            return f64::from_bits(cached);
        }

        let field_count = ctx.params.len() as u32;
        let obj = js_object_alloc(0, field_count);
        if obj.is_null() {
            return f64::from_bits(0x7FFC_0000_0000_0001);
        }
        let keys_arr = js_array_alloc(field_count);
        if keys_arr.is_null() {
            return f64::from_bits(0x7FFC_0000_0000_0001);
        }
        for (i, (key, value)) in ctx.params.iter().enumerate() {
            let key_ptr = js_string_from_bytes(key.as_ptr(), key.len() as u32);
            let value_ptr = js_string_from_bytes(value.as_ptr(), value.len() as u32);
            let key_nanboxed = js_nanbox_string(key_ptr as i64);
            js_array_push_f64(keys_arr, key_nanboxed);
            let value_nanboxed = js_nanbox_string(value_ptr as i64);
            js_object_set_field_f64(obj, i as u32, value_nanboxed);
        }
        js_object_set_keys(obj, keys_arr);
        let ptr = obj as u64;
        let nan_boxed = 0x7FFD_0000_0000_0000u64 | (ptr & 0x0000_FFFF_FFFF_FFFF);
        // Cache for the rest of the request lifetime. Release so any
        // subsequent Acquire load on this same context sees the
        // populated bits.
        ctx.params_object_cache.store(nan_boxed, Ordering::Release);
        return f64::from_bits(nan_boxed);
    }
    f64::from_bits(0x7FFC_0000_0000_0001)
}

/// Get a single route param (Hono style)
#[no_mangle]
pub unsafe extern "C" fn js_fastify_req_param(ctx_handle: Handle, name: i64) -> *mut StringHeader {
    let name = match string_from_nanboxed(name) {
        Some(n) => n,
        None => return std::ptr::null_mut(),
    };

    if let Some(ctx) = get_handle::<FastifyContext>(ctx_handle) {
        if let Some(value) = ctx.params.get(&name) {
            return js_string_from_bytes(value.as_ptr(), value.len() as u32);
        }
    }
    std::ptr::null_mut()
}

/// Get all query params as JSON string (for backwards compatibility)
#[no_mangle]
pub unsafe extern "C" fn js_fastify_req_query(ctx_handle: Handle) -> *mut StringHeader {
    if let Some(ctx) = get_handle::<FastifyContext>(ctx_handle) {
        let params = ctx.get_query_params();
        if let Ok(json) = serde_json::to_string(&params) {
            return js_string_from_bytes(json.as_ptr(), json.len() as u32);
        }
    }
    std::ptr::null_mut()
}

/// Get all query params as a JavaScript object (NaN-boxed pointer).
///
/// PR 4 (bottleneck #4): caches on first access via
/// `query_object_cache`. Same encoding as
/// `js_fastify_req_params_object` — 0 means uncached, non-zero is
/// the NaN-boxed pointer bits (top 16 = 0x7FFD).
#[no_mangle]
pub unsafe extern "C" fn js_fastify_req_query_object(ctx_handle: Handle) -> f64 {
    use perry_runtime::{
        js_array_alloc, js_array_push_f64, js_nanbox_string, js_object_alloc,
        js_object_set_field_f64, js_object_set_keys,
    };
    use std::sync::atomic::Ordering;

    if let Some(ctx) = get_handle::<FastifyContext>(ctx_handle) {
        let cached = ctx.query_object_cache.load(Ordering::Acquire);
        if cached != 0 {
            return f64::from_bits(cached);
        }

        let params = ctx.get_query_params();
        let field_count = params.len() as u32;

        // Allocate object with enough fields
        let obj = js_object_alloc(0, field_count);
        if obj.is_null() {
            return f64::from_bits(0x7FFC_0000_0000_0001); // undefined
        }

        // Allocate keys array
        let keys_arr = js_array_alloc(field_count);
        if keys_arr.is_null() {
            return f64::from_bits(0x7FFC_0000_0000_0001);
        }

        // Set each field and add key to keys array
        for (i, (key, value)) in params.iter().enumerate() {
            // Create key string
            let key_ptr = js_string_from_bytes(key.as_ptr(), key.len() as u32);
            // Create value string
            let value_ptr = js_string_from_bytes(value.as_ptr(), value.len() as u32);

            // Add key to keys array (NaN-boxed)
            let key_nanboxed = js_nanbox_string(key_ptr as i64);
            js_array_push_f64(keys_arr, key_nanboxed);

            // Set field on object by index (NaN-boxed string value)
            let value_nanboxed = js_nanbox_string(value_ptr as i64);
            js_object_set_field_f64(obj, i as u32, value_nanboxed);
        }

        // Set keys array on object
        js_object_set_keys(obj, keys_arr);

        // Return NaN-boxed pointer
        let ptr = obj as u64;
        let nan_boxed = 0x7FFD_0000_0000_0000u64 | (ptr & 0x0000_FFFF_FFFF_FFFF);
        // PR 4 cache write — see params accessor above for invariant.
        ctx.query_object_cache.store(nan_boxed, Ordering::Release);
        return f64::from_bits(nan_boxed);
    }

    f64::from_bits(0x7FFC_0000_0000_0001) // undefined
}

/// Get raw request body as string
#[no_mangle]
pub unsafe extern "C" fn js_fastify_req_body(ctx_handle: Handle) -> *mut StringHeader {
    if let Some(ctx) = get_handle::<FastifyContext>(ctx_handle) {
        if let Some(body) = ctx.body_string() {
            return js_string_from_bytes(body.as_ptr(), body.len() as u32);
        }
    }
    std::ptr::null_mut()
}

/// Get parsed JSON body (returns NaN-boxed object or undefined)
#[no_mangle]
pub unsafe extern "C" fn js_fastify_req_json(ctx_handle: Handle) -> f64 {
    if let Some(ctx) = get_handle::<FastifyContext>(ctx_handle) {
        if let Some(body) = ctx.body_string() {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&body) {
                return json_value_to_jsvalue(&value);
            }
        }
    }
    f64::from_bits(JSValue::undefined().bits())
}

/// Get all request headers as a JS object.
///
/// PR 5 (bottleneck #7): builds the JS object directly via the
/// runtime's object-creation FFI (`js_object_alloc` +
/// `js_object_set_field_f64` + `js_object_set_keys`) instead of
/// going through the previous `serde_json::to_string → js_string_
/// from_bytes → js_json_parse` round-trip. Removes one full JSON
/// encode + one full JSON decode per `req.headers` access — the
/// pattern is identical to PR 4's direct params/query object
/// construction, just keyed on `ctx.headers` instead of
/// `ctx.params`.
///
/// PR 5 also caches the constructed object in
/// `headers_object_cache` (same encoding as PR 4's caches: 0 =
/// uncached, non-zero = NaN-boxed pointer bits). yammer-web-server
/// CSP nonce path reads several headers per request; the cache
/// makes the second+ accesses O(1).
#[no_mangle]
pub unsafe extern "C" fn js_fastify_req_headers(ctx_handle: Handle) -> i64 {
    use perry_runtime::{
        js_array_alloc, js_array_push_f64, js_nanbox_string, js_object_alloc,
        js_object_set_field_f64, js_object_set_keys,
    };
    use std::sync::atomic::Ordering;

    const TAG_UNDEFINED: i64 = 0x7FFC_0000_0000_0001u64 as i64;

    if let Some(ctx) = get_handle::<FastifyContext>(ctx_handle) {
        let cached = ctx.headers_object_cache.load(Ordering::Acquire);
        if cached != 0 {
            return cached as i64;
        }

        let field_count = ctx.headers.len() as u32;
        let obj = js_object_alloc(0, field_count);
        if obj.is_null() {
            return TAG_UNDEFINED;
        }
        let keys_arr = js_array_alloc(field_count);
        if keys_arr.is_null() {
            return TAG_UNDEFINED;
        }
        for (i, (key, value)) in ctx.headers.iter().enumerate() {
            let key_ptr = js_string_from_bytes(key.as_ptr(), key.len() as u32);
            let value_ptr = js_string_from_bytes(value.as_ptr(), value.len() as u32);
            let key_nanboxed = js_nanbox_string(key_ptr as i64);
            js_array_push_f64(keys_arr, key_nanboxed);
            let value_nanboxed = js_nanbox_string(value_ptr as i64);
            js_object_set_field_f64(obj, i as u32, value_nanboxed);
        }
        js_object_set_keys(obj, keys_arr);
        let ptr = obj as u64;
        let nan_boxed = 0x7FFD_0000_0000_0000u64 | (ptr & 0x0000_FFFF_FFFF_FFFF);
        ctx.headers_object_cache.store(nan_boxed, Ordering::Release);
        return nan_boxed as i64;
    }
    TAG_UNDEFINED
}

/// Get a single header value
#[no_mangle]
pub unsafe extern "C" fn js_fastify_req_header(ctx_handle: Handle, name: i64) -> *mut StringHeader {
    let name = match string_from_nanboxed(name) {
        Some(n) => n.to_lowercase(),
        None => return std::ptr::null_mut(),
    };

    if let Some(ctx) = get_handle::<FastifyContext>(ctx_handle) {
        if let Some(value) = ctx.headers.get(&name) {
            return js_string_from_bytes(value.as_ptr(), value.len() as u32);
        }
    }
    std::ptr::null_mut()
}

/// Get user data attached by auth middleware
#[no_mangle]
pub unsafe extern "C" fn js_fastify_req_get_user_data(ctx_handle: Handle) -> f64 {
    if let Some(ctx) = get_handle::<FastifyContext>(ctx_handle) {
        return f64::from_bits(ctx.user_data);
    }
    const TAG_UNDEFINED: u64 = 0x7FFC_0000_0000_0001;
    f64::from_bits(TAG_UNDEFINED)
}

/// Set user data from auth middleware
#[no_mangle]
pub unsafe extern "C" fn js_fastify_req_set_user_data(ctx_handle: Handle, data: f64) {
    if let Some(ctx) = get_handle_mut::<FastifyContext>(ctx_handle) {
        ctx.user_data = data.to_bits();
    }
}

// ============================================================================
// Reply Methods FFI (Fastify style)
// ============================================================================

/// Set response status code (chainable, returns handle)
#[no_mangle]
pub unsafe extern "C" fn js_fastify_reply_status(ctx_handle: Handle, code: f64) -> Handle {
    if let Some(ctx) = get_handle_mut::<FastifyContext>(ctx_handle) {
        ctx.status_code = code as u16;
    }
    ctx_handle
}

/// Set a response header (chainable, returns handle)
#[no_mangle]
pub unsafe extern "C" fn js_fastify_reply_header(
    ctx_handle: Handle,
    name: i64,
    value: i64,
) -> Handle {
    let name = match string_from_nanboxed(name) {
        Some(n) => n,
        None => return ctx_handle,
    };
    let value = match string_from_nanboxed(value) {
        Some(v) => v,
        None => return ctx_handle,
    };

    if let Some(ctx) = get_handle_mut::<FastifyContext>(ctx_handle) {
        ctx.response_headers.push((name, value));
    }
    ctx_handle
}

/// `reply.type(value)` — Fastify alias for `reply.header("content-type", value)`.
/// Chainable: returns the reply handle so `.type(...).send(...)` works.
///
/// Previously missing from the stdlib copy of fastify (#1048): user code using
/// the bundled-fastify path failed at link time with
/// `Undefined symbols: _js_fastify_reply_type` because only perry-ext-fastify
/// shipped this symbol.
#[no_mangle]
pub unsafe extern "C" fn js_fastify_reply_type(ctx_handle: Handle, value: i64) -> Handle {
    let value = match string_from_nanboxed(value) {
        Some(v) => v,
        None => return ctx_handle,
    };

    if let Some(ctx) = get_handle_mut::<FastifyContext>(ctx_handle) {
        ctx.response_headers
            .push(("content-type".to_string(), value));
    }
    ctx_handle
}

/// Send response (Fastify style). Returns true if newly sent.
///
/// `Buffer` / `Uint8Array` payloads default `content-type` to
/// `application/octet-stream` when the handler hasn't pinned one
/// via `reply.type(...)` so binary assets don't ship as JSON (#1120).
#[no_mangle]
pub unsafe extern "C" fn js_fastify_reply_send(ctx_handle: Handle, data: f64) -> bool {
    if let Some(ctx) = get_handle_mut::<FastifyContext>(ctx_handle) {
        if ctx.sent {
            return false;
        }

        let (body, kind) = jsvalue_to_response_body(data);
        if kind == BodyKind::Binary
            && !ctx
                .response_headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        {
            ctx.response_headers.push((
                "content-type".to_string(),
                "application/octet-stream".to_string(),
            ));
        }
        ctx.response_body = Some(body);
        ctx.sent = true;
        return true;
    }
    false
}

// ============================================================================
// Context Methods FFI (Hono style)
// ============================================================================

/// Send JSON response (Hono style)
/// Returns a response marker value
#[no_mangle]
pub unsafe extern "C" fn js_fastify_ctx_json(ctx_handle: Handle, data: f64, status: f64) -> f64 {
    // #1240 — `request.json()` (Fetch API) shares the dispatch slot with
    // `reply.json(data)` / `c.json(data, status)` because both receivers are
    // backed by the same FastifyContext handle. A zero-arg call from user code
    // arrives here with `data` padded to NaN-boxed undefined; in that case route
    // to `js_fastify_req_json` so existing Fetch-style codebases keep working
    // without touching `request.body`.
    if data.to_bits() == JSValue::undefined().bits() {
        return js_fastify_req_json(ctx_handle);
    }

    if let Some(ctx) = get_handle_mut::<FastifyContext>(ctx_handle) {
        if status > 0.0 {
            ctx.status_code = status as u16;
        }

        // Add content-type header
        ctx.response_headers.push((
            "content-type".to_string(),
            "application/json; charset=utf-8".to_string(),
        ));

        // Convert data to JSON string
        let body = jsvalue_to_json_string(data);
        ctx.response_body = Some(body.into_bytes());
        ctx.sent = true;
    }

    // Return undefined (response is implicit)
    f64::from_bits(JSValue::undefined().bits())
}

/// Send text response (Hono style)
#[no_mangle]
pub unsafe extern "C" fn js_fastify_ctx_text(ctx_handle: Handle, text: i64, status: f64) -> f64 {
    let text = string_from_nanboxed(text).unwrap_or_default();

    if let Some(ctx) = get_handle_mut::<FastifyContext>(ctx_handle) {
        if status > 0.0 {
            ctx.status_code = status as u16;
        }

        ctx.response_headers.push((
            "content-type".to_string(),
            "text/plain; charset=utf-8".to_string(),
        ));
        ctx.response_body = Some(text.into_bytes());
        ctx.sent = true;
    }

    f64::from_bits(JSValue::undefined().bits())
}

/// Send HTML response (Hono style)
#[no_mangle]
pub unsafe extern "C" fn js_fastify_ctx_html(ctx_handle: Handle, html: i64, status: f64) -> f64 {
    let html = string_from_nanboxed(html).unwrap_or_default();

    if let Some(ctx) = get_handle_mut::<FastifyContext>(ctx_handle) {
        if status > 0.0 {
            ctx.status_code = status as u16;
        }

        ctx.response_headers.push((
            "content-type".to_string(),
            "text/html; charset=utf-8".to_string(),
        ));
        ctx.response_body = Some(html.into_bytes());
        ctx.sent = true;
    }

    f64::from_bits(JSValue::undefined().bits())
}

/// Send redirect response (Hono style)
#[no_mangle]
pub unsafe extern "C" fn js_fastify_ctx_redirect(ctx_handle: Handle, url: i64, status: f64) -> f64 {
    let url = string_from_nanboxed(url).unwrap_or_default();

    if let Some(ctx) = get_handle_mut::<FastifyContext>(ctx_handle) {
        ctx.status_code = if status > 0.0 { status as u16 } else { 302 };
        ctx.response_headers.push(("location".to_string(), url));
        ctx.response_body = Some(Vec::new());
        ctx.sent = true;
    }

    f64::from_bits(JSValue::undefined().bits())
}

// ============================================================================
// Helper functions
// ============================================================================

/// Convert a JSValue to a response body. Strings pass through as
/// raw UTF-8; `Buffer` / `Uint8Array` ships the raw payload (no
/// UTF-8 round-trip — pre-fix the bytes went through Buffer.toJSON
/// per #1120); everything else gets JSON-stringified. The returned
/// `BodyKind` tells the caller whether to default `content-type` to
/// `application/octet-stream` (binary) or `application/json`.
pub(crate) unsafe fn jsvalue_to_response_body(value: f64) -> (Vec<u8>, BodyKind) {
    let jsv = JSValue::from_bits(value.to_bits());

    if jsv.is_string() {
        // PR 6 fast path: read the StringHeader's bytes directly into
        // Vec<u8> instead of going through
        //   extract_jsvalue_string → String::from_utf8_lossy → .to_string()
        //   → .into_bytes()
        // The intermediate String allocation + UTF-8 validity scan are
        // unnecessary — hyper's body just wants bytes, and the response
        // writer streams them out unchanged. Eliminates one allocation
        // per text-response request, which on yammer-web-server's
        // /external_ping hot path is one of the few remaining per-request
        // allocs after PR 2 mem::take. Bottleneck #1 in
        // .claude/plans/look-at-the-benchmarks-jazzy-llama.md.
        let ptr = perry_runtime::js_get_string_pointer_unified(value);
        if ptr != 0 {
            let header = ptr as *const StringHeader;
            let len = (*header).byte_len as usize;
            let data_ptr = (header as *const u8).add(std::mem::size_of::<StringHeader>());
            let mut bytes = Vec::with_capacity(len);
            bytes.extend_from_slice(std::slice::from_raw_parts(data_ptr, len));
            return (bytes, BodyKind::TextOrJson);
        }
        // Fallback to the original path if the unified pointer accessor
        // returns 0 (defensive — should not happen for is_string()).
        if let Some(s) = extract_jsvalue_string(value) {
            return (s.into_bytes(), BodyKind::TextOrJson);
        }
    }

    if let Some(bytes) = extract_buffer_bytes(value) {
        return (bytes, BodyKind::Binary);
    }

    (
        jsvalue_to_json_string(value).into_bytes(),
        BodyKind::TextOrJson,
    )
}

/// Convert a JSValue to a JSON string
unsafe fn jsvalue_to_json_string(value: f64) -> String {
    let jsv = JSValue::from_bits(value.to_bits());

    if jsv.is_undefined() {
        return "null".to_string();
    }
    if jsv.is_null() {
        return "null".to_string();
    }
    if jsv.is_bool() {
        return if jsv.as_bool() {
            "true".to_string()
        } else {
            "false".to_string()
        };
    }
    if jsv.is_number() {
        return format!("{}", value);
    }
    if jsv.is_string() {
        if let Some(s) = extract_jsvalue_string(value) {
            // Escape string for JSON
            return serde_json::to_string(&s).unwrap_or_else(|_| format!("\"{}\"", s));
        }
    }

    // For objects/arrays, use JSON.stringify
    if jsv.is_pointer() {
        extern "C" {
            fn js_json_stringify(value: f64, type_hint: u32) -> *mut perry_runtime::StringHeader;
        }
        let str_ptr = js_json_stringify(value, 0);
        if !str_ptr.is_null() {
            if let Some(s) = string_from_header(str_ptr) {
                return s;
            }
        }
    }

    // Fallback: use runtime's toString
    let str_ptr = perry_runtime::js_jsvalue_to_string(value);
    if !str_ptr.is_null() {
        if let Some(s) = string_from_header(str_ptr) {
            return s;
        }
    }

    "null".to_string()
}

/// Extract string from JSValue
unsafe fn extract_jsvalue_string(value: f64) -> Option<String> {
    let ptr = perry_runtime::js_get_string_pointer_unified(value);
    if ptr == 0 {
        return None;
    }
    string_from_header(ptr as *const StringHeader)
}

/// Convert serde_json::Value to JSValue (as f64)
unsafe fn json_value_to_jsvalue(value: &serde_json::Value) -> f64 {
    match value {
        serde_json::Value::Null => f64::from_bits(JSValue::null().bits()),
        serde_json::Value::Bool(b) => f64::from_bits(JSValue::bool(*b).bits()),
        serde_json::Value::Number(n) => n.as_f64().unwrap_or(0.0),
        serde_json::Value::String(s) => {
            let ptr = js_string_from_bytes(s.as_ptr(), s.len() as u32);
            f64::from_bits(JSValue::string_ptr(ptr).bits())
        }
        serde_json::Value::Array(arr) => {
            let js_arr = perry_runtime::js_array_alloc(arr.len() as u32);
            for item in arr {
                let js_item = json_value_to_jsvalue(item);
                perry_runtime::js_array_push_f64(js_arr, js_item);
            }
            f64::from_bits(JSValue::pointer(js_arr as *const u8).bits())
        }
        serde_json::Value::Object(obj) => {
            let field_count = obj.len();
            let js_obj = perry_runtime::js_object_alloc(0, field_count as u32);
            for (key, val) in obj {
                let js_key = js_string_from_bytes(key.as_ptr(), key.len() as u32);
                let js_val = json_value_to_jsvalue(val);
                perry_runtime::js_object_set_field_by_name(js_obj, js_key, js_val);
            }
            f64::from_bits(JSValue::pointer(js_obj as *const u8).bits())
        }
    }
}
