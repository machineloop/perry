//! Fastify-Compatible Native HTTP Framework for Perry
//!
//! A high-performance HTTP framework with Fastify-like API and Hono-style context methods.
//! Compiles TypeScript to native code while providing familiar patterns.
//!
//! # Example (Fastify style)
//! ```typescript
//! import Fastify from 'fastify';
//!
//! const app = Fastify();
//!
//! app.get('/', async (request, reply) => {
//!   return { hello: 'world' };
//! });
//!
//! app.listen({ port: 3000 });
//! ```
//!
//! # Example (Hono style)
//! ```typescript
//! app.get('/users/:id', async (c) => {
//!   return c.json({ id: c.req.param('id') });
//! });
//! ```

pub mod app;
pub mod context;
pub mod router;
pub mod server;

pub use app::*;
pub use context::*;
pub use router::*;
pub use server::*;

use std::collections::HashMap;

use crate::common::for_each_handle_of;

static FASTIFY_GC_REGISTERED: std::sync::Once = std::sync::Once::new();

/// Register the Fastify GC root scanner exactly once. User closures
/// passed to `app.get/post/put/...`, `app.addHook`, and
/// `app.setErrorHandler` are stored inside the FastifyApp values in
/// the handle registry. Without this scanner, a malloc-triggered GC
/// between route/hook registration and an incoming request would
/// sweep the handler closures — same root cause as issue #35 for
/// net.Socket listeners. Also covers any Arc-clones of the app that
/// tokio worker tasks hold for dispatch: those Arcs point to the same
/// heap allocation as the registry entry, so marking via the registry
/// covers the tokio copies too (routes/hooks are Clone and closures
/// are stored by i64 value — the tokio copy references the same GC
/// tracked ClosureHeader).
pub(crate) fn ensure_gc_scanner_registered() {
    FASTIFY_GC_REGISTERED.call_once(|| {
        perry_runtime::gc::gc_register_root_scanner(scan_fastify_roots);
    });
}

/// GC root scanner for Fastify handler / hook / error-handler closures.
fn scan_fastify_roots(mark: &mut dyn FnMut(f64)) {
    let mark_cb = |cb: ClosurePtr, mark: &mut dyn FnMut(f64)| {
        if cb != 0 {
            let boxed = f64::from_bits(0x7FFD_0000_0000_0000 | (cb as u64 & 0x0000_FFFF_FFFF_FFFF));
            mark(boxed);
        }
    };

    for_each_handle_of::<FastifyApp, _>(|app| {
        for route in app.routes.iter() {
            mark_cb(route.handler, mark);
        }
        for cb in app
            .hooks
            .on_request
            .iter()
            .chain(app.hooks.pre_parsing.iter())
            .chain(app.hooks.pre_validation.iter())
            .chain(app.hooks.pre_handler.iter())
            .chain(app.hooks.pre_serialization.iter())
            .chain(app.hooks.on_send.iter())
            .chain(app.hooks.on_response.iter())
            .chain(app.hooks.on_error.iter())
        {
            mark_cb(*cb, mark);
        }
        if let Some(eh) = app.error_handler {
            mark_cb(eh, mark);
        }
        for plugin in app.plugins.iter() {
            mark_cb(plugin.handler, mark);
        }
        // #1113: upgrade handlers registered via `app.server.on("upgrade", cb)`
        // live in the same handle registry slot — pin them too so a
        // GC cycle between registration and an incoming Upgrade
        // request doesn't sweep them.
        for cb in app.upgrade_handlers.iter() {
            mark_cb(*cb, mark);
        }
    });
}

/// Closure pointer type (matches perry-runtime)
pub type ClosurePtr = i64;

/// Route definition
#[derive(Clone)]
pub struct Route {
    /// HTTP method (GET, POST, etc.)
    pub method: String,
    /// Route pattern with parameter extraction
    pub pattern: RoutePattern,
    /// Handler closure pointer
    pub handler: ClosurePtr,
}

/// Lifecycle hooks for request processing
#[derive(Default, Clone)]
pub struct Hooks {
    /// Called when a request is received
    pub on_request: Vec<ClosurePtr>,
    /// Called before body parsing
    pub pre_parsing: Vec<ClosurePtr>,
    /// Called before validation
    pub pre_validation: Vec<ClosurePtr>,
    /// Called before the route handler
    pub pre_handler: Vec<ClosurePtr>,
    /// Called before serialization
    pub pre_serialization: Vec<ClosurePtr>,
    /// Called before sending response
    pub on_send: Vec<ClosurePtr>,
    /// Called after response is sent
    pub on_response: Vec<ClosurePtr>,
    /// Called when an error occurs
    pub on_error: Vec<ClosurePtr>,
}

/// Plugin registration
#[derive(Clone)]
pub struct Plugin {
    /// Plugin handler closure
    pub handler: ClosurePtr,
    /// URL prefix for all routes in this plugin
    pub prefix: String,
}

/// Main Fastify application instance
pub struct FastifyApp {
    /// Registered routes
    pub routes: Vec<Route>,
    /// O(1) index over the subset of `routes` whose pattern has no
    /// `Param`/`Wildcard` segments. Key format: `"{METHOD} {full_path}"`
    /// (matching what `match_route` builds at lookup time). Value: the
    /// index into `self.routes`. Built incrementally in `add_route`;
    /// parametric routes fall through to the existing linear scan over
    /// `self.routes`. This is the radix-routing micro-fix from bottleneck
    /// #3 in .claude/plans/look-at-the-benchmarks-jazzy-llama.md — keeps
    /// the existing data model (and GC scanner walk over `routes`) intact
    /// while turning the dominant static-route lookup cost from O(N) per
    /// request into one HashMap hash + comparison.
    pub static_index: HashMap<String, usize>,
    /// Lifecycle hooks
    pub hooks: Hooks,
    /// Custom error handler
    pub error_handler: Option<ClosurePtr>,
    /// Registered plugins
    pub plugins: Vec<Plugin>,
    /// Route prefix (for plugins)
    pub prefix: String,
    /// Server configuration
    pub config: FastifyConfig,
    /// #1113: callbacks registered through `app.server.on("upgrade", cb)`.
    /// Storage lives on the app (not on `FastifyServerHandle`) because
    /// the user pattern is `app.server.on(...)` — accessing the
    /// `server` getter after `await app.listen(...)` returns the
    /// **app** handle (object-tagged) and `.on()` dispatches against
    /// that same handle. Mirrors Node `http.Server extends
    /// EventEmitter` semantics with a tiny stub: storing the callback
    /// makes the user's boot-time `app.server.on("upgrade", …)` line
    /// stop throwing `(number).on is not a function`.
    ///
    /// **Today only stores the callbacks** — the hyper accept-loop in
    /// `server.rs` doesn't yet route `Upgrade:` requests through
    /// `hyper::upgrade::on(req)` and hand the raw socket + head bytes
    /// back to TypeScript, so registered upgrade handlers never fire.
    /// A diagnostic line at request-time tells the user when an upgrade
    /// arrived (`PERRY_DEBUG=1` or always when a handler is registered).
    /// Full bidirectional upgrade dispatch through `perry-ext-ws`'s
    /// `noServer` mode is tracked as the #1113 follow-up.
    pub upgrade_handlers: Vec<ClosurePtr>,
}

/// Server configuration options
#[derive(Clone, Default)]
pub struct FastifyConfig {
    /// Enable request logging
    pub logger: bool,
    /// Trust proxy headers
    pub trust_proxy: bool,
    /// Maximum body size in bytes
    pub body_limit: Option<usize>,
}

impl FastifyApp {
    /// Create a new Fastify application
    pub fn new() -> Self {
        Self {
            routes: Vec::new(),
            static_index: HashMap::new(),
            hooks: Hooks::default(),
            error_handler: None,
            plugins: Vec::new(),
            prefix: String::new(),
            config: FastifyConfig::default(),
            upgrade_handlers: Vec::new(),
        }
    }

    /// Create a new Fastify application with a prefix (for plugins)
    pub fn with_prefix(prefix: String) -> Self {
        Self {
            routes: Vec::new(),
            static_index: HashMap::new(),
            hooks: Hooks::default(),
            error_handler: None,
            plugins: Vec::new(),
            prefix,
            config: FastifyConfig::default(),
            upgrade_handlers: Vec::new(),
        }
    }

    /// Add a route
    pub fn add_route(&mut self, method: &str, path: &str, handler: ClosurePtr) {
        let full_path = if self.prefix.is_empty() {
            path.to_string()
        } else {
            format!("{}{}", self.prefix, path)
        };

        let method_upper = method.to_uppercase();
        let pattern = RoutePattern::parse(&full_path);

        // If the pattern is fully static (no Param/Wildcard segments),
        // index it for O(1) lookup. Parametric and wildcard routes still
        // go through the linear scan in `match_route` below. The key
        // shape mirrors what `match_route` builds at request time —
        // `match_route` falls back to linear scan when the static-index
        // miss is a real miss vs a parametric path.
        let is_static = pattern
            .segments
            .iter()
            .all(|s| matches!(s, crate::fastify::router::Segment::Static(_)));

        let new_idx = self.routes.len();
        self.routes.push(Route {
            method: method_upper,
            pattern,
            handler,
        });
        if is_static {
            // Key shape: `"METHOD /normalized/path"` — leading slash
            // canonicalized so we can match the lookup-side
            // normalization in `match_route` below regardless of
            // whether the user registered "/foo" or "foo".
            let route = &self.routes[new_idx];
            let canon_path = if full_path.starts_with('/') {
                full_path.clone()
            } else {
                format!("/{}", full_path)
            };
            let key = format!("{} {}", route.method, canon_path);
            // Last-registration-wins on collisions, matching the
            // existing linear-scan behaviour where the latest route is
            // hit first only if it shadows the earlier one's pattern
            // exactly. Fastify itself errors on exact duplicates but
            // perry-stdlib has never enforced that — preserve today's
            // tolerant behavior rather than tighten it inside a perf PR.
            self.static_index.insert(key, new_idx);
        }
    }

    /// Add a hook
    pub fn add_hook(&mut self, hook_name: &str, handler: ClosurePtr) {
        match hook_name {
            "onRequest" => self.hooks.on_request.push(handler),
            "preParsing" => self.hooks.pre_parsing.push(handler),
            "preValidation" => self.hooks.pre_validation.push(handler),
            "preHandler" => self.hooks.pre_handler.push(handler),
            "preSerialization" => self.hooks.pre_serialization.push(handler),
            "onSend" => self.hooks.on_send.push(handler),
            "onResponse" => self.hooks.on_response.push(handler),
            "onError" => self.hooks.on_error.push(handler),
            _ => eprintln!("Unknown hook: {}", hook_name),
        }
    }

    /// Set error handler
    pub fn set_error_handler(&mut self, handler: ClosurePtr) {
        self.error_handler = Some(handler);
    }

    /// Find matching route for a request
    pub fn match_route(
        &self,
        method: &str,
        path: &str,
    ) -> Option<(&Route, HashMap<String, String>)> {
        // Strip an optional query string before looking up the static
        // index — `RoutePattern::match_path` does the same on the linear
        // scan side (router.rs:79). Without this, a request to
        // `/external_ping?foo=bar` would miss the index entry for the
        // static `/external_ping` registration even though the linear
        // scan would correctly match it.
        let lookup_path = path.split('?').next().unwrap_or(path);

        // Fast path: static (no Param/Wildcard) routes are O(1) via
        // self.static_index. Key shape matches what `add_route` writes
        // at registration time: METHOD-uppercased + space + path with
        // leading slash.
        let canon_path: String = if lookup_path.starts_with('/') {
            lookup_path.to_string()
        } else {
            format!("/{}", lookup_path)
        };
        let key = format!("{} {}", method, canon_path);
        if let Some(&idx) = self.static_index.get(&key) {
            let route = &self.routes[idx];
            // Static routes never extract path params. Returning the
            // empty HashMap matches `RoutePattern::match_path`'s
            // behaviour for static patterns (see router.rs:88-92).
            return Some((route, HashMap::new()));
        }

        // Slow path: parametric / wildcard routes still go through the
        // linear scan, preserving the existing first-registered-wins
        // semantic.
        for route in &self.routes {
            if route.method == method {
                if let Some(params) = route.pattern.match_path(path) {
                    return Some((route, params));
                }
            }
        }
        None
    }
}

impl Default for FastifyApp {
    fn default() -> Self {
        Self::new()
    }
}
