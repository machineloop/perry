//! Route pattern parsing and matching
//!
//! Supports:
//! - Static segments: `/users`, `/api/v1`
//! - Parameters: `/users/:id`, `/posts/:postId/comments/:commentId`
//! - Wildcards: `/static/*` (captures rest of path)

use std::collections::HashMap;

/// A segment in a route pattern
#[derive(Debug, Clone, PartialEq)]
pub enum Segment {
    /// Static path segment (e.g., "users")
    Static(String),
    /// Named parameter (e.g., ":id" captures "id")
    Param(String),
    /// Wildcard captures rest of path
    Wildcard,
}

/// Parsed route pattern for efficient matching
#[derive(Debug, Clone)]
pub struct RoutePattern {
    /// Pattern segments
    pub segments: Vec<Segment>,
    /// Original pattern string
    pub raw: String,
}

impl RoutePattern {
    /// Parse a route pattern string into segments
    ///
    /// # Examples
    /// - `/users` -> [Static("users")]
    /// - `/users/:id` -> [Static("users"), Param("id")]
    /// - `/static/*` -> [Static("static"), Wildcard]
    pub fn parse(path: &str) -> Self {
        let mut segments = Vec::new();
        let path = path.trim_start_matches('/');

        if path.is_empty() {
            return Self {
                segments,
                raw: "/".to_string(),
            };
        }

        for part in path.split('/') {
            if part.is_empty() {
                continue;
            }

            let segment = if part.starts_with(':') {
                // Parameter segment
                Segment::Param(part[1..].to_string())
            } else if part == "*" {
                // Wildcard segment
                Segment::Wildcard
            } else {
                // Static segment
                Segment::Static(part.to_string())
            };

            segments.push(segment);
        }

        Self {
            segments,
            raw: path.to_string(),
        }
    }

    /// Match a request path against this pattern
    ///
    /// Returns `Some(params)` if the path matches, with extracted parameters.
    /// Returns `None` if the path doesn't match.
    pub fn match_path(&self, path: &str) -> Option<HashMap<String, String>> {
        let path = path.trim_start_matches('/');
        let path = path.split('?').next().unwrap_or(path); // Remove query string
        let path_parts: Vec<&str> = if path.is_empty() {
            Vec::new()
        } else {
            path.split('/').filter(|s| !s.is_empty()).collect()
        };

        // Handle root path
        if self.segments.is_empty() {
            return if path_parts.is_empty() {
                Some(HashMap::new())
            } else {
                None
            };
        }

        let mut params = HashMap::new();
        let mut path_idx = 0;

        for (_seg_idx, segment) in self.segments.iter().enumerate() {
            match segment {
                Segment::Static(expected) => {
                    if path_idx >= path_parts.len() || path_parts[path_idx] != expected {
                        return None;
                    }
                    path_idx += 1;
                }
                Segment::Param(name) => {
                    if path_idx >= path_parts.len() {
                        return None;
                    }
                    params.insert(name.clone(), path_parts[path_idx].to_string());
                    path_idx += 1;
                }
                Segment::Wildcard => {
                    // Wildcard captures the rest of the path
                    let rest: String = path_parts[path_idx..].join("/");
                    params.insert("*".to_string(), rest);
                    return Some(params); // Wildcard always matches rest
                }
            }
        }

        // All segments matched, check if we consumed all path parts
        if path_idx == path_parts.len() {
            Some(params)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_static_route() {
        let pattern = RoutePattern::parse("/users");
        assert!(pattern.match_path("/users").is_some());
        assert!(pattern.match_path("/users/").is_some());
        assert!(pattern.match_path("/posts").is_none());
        assert!(pattern.match_path("/users/123").is_none());
    }

    #[test]
    fn test_param_route() {
        let pattern = RoutePattern::parse("/users/:id");

        let params = pattern.match_path("/users/123").unwrap();
        assert_eq!(params.get("id"), Some(&"123".to_string()));

        let params = pattern.match_path("/users/abc").unwrap();
        assert_eq!(params.get("id"), Some(&"abc".to_string()));

        assert!(pattern.match_path("/users").is_none());
        assert!(pattern.match_path("/users/123/posts").is_none());
    }

    #[test]
    fn test_multiple_params() {
        let pattern = RoutePattern::parse("/posts/:postId/comments/:commentId");

        let params = pattern.match_path("/posts/1/comments/2").unwrap();
        assert_eq!(params.get("postId"), Some(&"1".to_string()));
        assert_eq!(params.get("commentId"), Some(&"2".to_string()));
    }

    #[test]
    fn test_wildcard() {
        let pattern = RoutePattern::parse("/static/*");

        let params = pattern.match_path("/static/css/style.css").unwrap();
        assert_eq!(params.get("*"), Some(&"css/style.css".to_string()));

        let params = pattern.match_path("/static/").unwrap();
        assert_eq!(params.get("*"), Some(&"".to_string()));
    }

    #[test]
    fn test_root_route() {
        let pattern = RoutePattern::parse("/");
        assert!(pattern.match_path("/").is_some());
        assert!(pattern.match_path("").is_some());
        assert!(pattern.match_path("/users").is_none());
    }

    #[test]
    fn test_query_string_ignored() {
        let pattern = RoutePattern::parse("/users/:id");
        let params = pattern.match_path("/users/123?foo=bar").unwrap();
        assert_eq!(params.get("id"), Some(&"123".to_string()));
    }

    #[test]
    fn test_nested_params() {
        let pattern = RoutePattern::parse("/api/v1/users/:userId/posts/:postId");
        let params = pattern.match_path("/api/v1/users/42/posts/99").unwrap();
        assert_eq!(params.get("userId"), Some(&"42".to_string()));
        assert_eq!(params.get("postId"), Some(&"99".to_string()));
    }

    #[test]
    fn test_mixed_static_and_params() {
        let pattern = RoutePattern::parse("/users/:id/profile");
        assert!(pattern.match_path("/users/123/profile").is_some());
        assert!(pattern.match_path("/users/123/settings").is_none());
        assert!(pattern.match_path("/users/123").is_none());
    }
}

#[cfg(test)]
mod app_tests {
    use crate::fastify::FastifyApp;

    #[test]
    fn test_add_routes() {
        let mut app = FastifyApp::new();

        // Add some routes
        app.add_route("GET", "/", 0);
        app.add_route("GET", "/users", 1);
        app.add_route("GET", "/users/:id", 2);
        app.add_route("POST", "/users", 3);

        assert_eq!(app.routes.len(), 4);
    }

    #[test]
    fn test_route_matching() {
        let mut app = FastifyApp::new();

        app.add_route("GET", "/", 100);
        app.add_route("GET", "/users", 101);
        app.add_route("GET", "/users/:id", 102);
        app.add_route("POST", "/users", 103);
        app.add_route("PUT", "/users/:id", 104);

        // Test root
        let (route, params) = app.match_route("GET", "/").unwrap();
        assert_eq!(route.handler, 100);
        assert!(params.is_empty());

        // Test static
        let (route, params) = app.match_route("GET", "/users").unwrap();
        assert_eq!(route.handler, 101);
        assert!(params.is_empty());

        // Test param
        let (route, params) = app.match_route("GET", "/users/42").unwrap();
        assert_eq!(route.handler, 102);
        assert_eq!(params.get("id"), Some(&"42".to_string()));

        // Test POST
        let (route, _) = app.match_route("POST", "/users").unwrap();
        assert_eq!(route.handler, 103);

        // Test PUT with param
        let (route, params) = app.match_route("PUT", "/users/99").unwrap();
        assert_eq!(route.handler, 104);
        assert_eq!(params.get("id"), Some(&"99".to_string()));

        // Test 404 cases
        assert!(app.match_route("DELETE", "/users").is_none());
        assert!(app.match_route("GET", "/posts").is_none());
    }

    #[test]
    fn test_hooks() {
        let mut app = FastifyApp::new();

        app.add_hook("onRequest", 1);
        app.add_hook("preHandler", 2);
        app.add_hook("preHandler", 3);

        assert_eq!(app.hooks.on_request.len(), 1);
        assert_eq!(app.hooks.pre_handler.len(), 2);
    }

    #[test]
    fn test_error_handler() {
        let mut app = FastifyApp::new();
        assert!(app.error_handler.is_none());

        app.set_error_handler(42);
        assert_eq!(app.error_handler, Some(42));
    }

    #[test]
    fn test_plugin_prefix() {
        let scoped = FastifyApp::with_prefix("/api/v1".to_string());
        assert_eq!(scoped.prefix, "/api/v1");
    }

    /// Regression test for PR 3 (bottleneck #3 — radix-router micro-fix).
    /// `add_route` should index static (no Param/Wildcard) routes into
    /// `static_index` for O(1) lookup in `match_route`, while routes
    /// with Param/Wildcard segments stay out of the index so they only
    /// match via the linear scan that preserves first-registration-wins
    /// semantics for parametric overlap.
    ///
    /// Failure modes this catches:
    ///   * static_index missing entries → linear scan still hit, no
    ///     measurable perf regression but the index becomes dead code
    ///   * parametric routes leaking into static_index → wrong handler
    ///     dispatched for parametric requests, since the static index
    ///     short-circuits before the linear scan's pattern.match_path
    ///     would have extracted the params
    ///   * key normalization drift (e.g. add_route uses "users" while
    ///     match_route looks up "/users") → static_index lookup misses
    ///     and falls through to linear scan; correct behaviour but the
    ///     perf gain disappears
    #[test]
    fn test_static_index_population_and_lookup() {
        use crate::fastify::FastifyApp;

        let mut app = FastifyApp::new();
        app.add_route("GET", "/external_ping", 1); // static
        app.add_route("GET", "/users/:id", 2);     // parametric
        app.add_route("POST", "/users", 3);        // static
        app.add_route("GET", "/static/*", 4);      // wildcard
        app.add_route("GET", "/", 5);              // static (root)

        // Static routes are indexed; parametric/wildcard ones are NOT.
        assert!(app.static_index.contains_key("GET /external_ping"));
        assert!(app.static_index.contains_key("POST /users"));
        assert!(app.static_index.contains_key("GET /"));
        assert!(!app.static_index.contains_key("GET /users/:id"));
        assert!(!app.static_index.contains_key("GET /static/*"));
        assert_eq!(app.static_index.len(), 3);

        // Fast-path hits return the right handler with empty params.
        let (route, params) = app.match_route("GET", "/external_ping").unwrap();
        assert_eq!(route.handler, 1);
        assert!(params.is_empty());

        // Query string on the request path doesn't break the index hit.
        let (route, _) = app.match_route("GET", "/external_ping?foo=bar").unwrap();
        assert_eq!(route.handler, 1);

        // Parametric routes still match through the linear-scan path
        // and extract their params correctly.
        let (route, params) = app.match_route("GET", "/users/42").unwrap();
        assert_eq!(route.handler, 2);
        assert_eq!(params.get("id"), Some(&"42".to_string()));

        // Wildcard routes still match through the linear-scan path.
        let (route, params) = app.match_route("GET", "/static/css/x.css").unwrap();
        assert_eq!(route.handler, 4);
        assert_eq!(params.get("*"), Some(&"css/x.css".to_string()));

        // Method mismatches don't short-circuit through the static index.
        assert!(app.match_route("DELETE", "/external_ping").is_none());
    }

    /// Regression test for PR 3 prefix interaction: when a FastifyApp
    /// is constructed with a prefix (plugin-style), the static-index
    /// key should be the FULL prefixed path, not the user-passed
    /// suffix. Otherwise routes registered via `app.add_route("GET",
    /// "/users", ...)` on a `with_prefix("/api")` app would miss the
    /// index lookup for the actual request URL `/api/users`.
    /// Regression test for PR 7 (bottleneck #5 — single-slab GC root).
    /// The GC root scanner used to reconstruct an 11-way iterator
    /// chain (routes + 8 hook vecs + plugins + upgrade_handlers) per
    /// scan tick. PR 7 precomputes a `gc_pinned_roots: Vec<u64>` slab
    /// on `FastifyApp` that the scanner walks as one linear slice —
    /// rebuilt by `add_route`/`add_hook`/`set_error_handler` at
    /// registration time. The invariant this test guards:
    ///   * the slab contains exactly one NaN-boxed-pointer entry per
    ///     non-zero ClosurePtr across routes/hooks/error_handler/
    ///     plugins/upgrade_handlers
    ///   * every entry has the 0x7FFD POINTER_TAG nibble that
    ///     scan_fastify_roots passes to `mark()`
    /// A regression that forgets to rebuild the slab after a
    /// registration (or that mismatches the tag) would let GC sweep
    /// handlers between registration and dispatch — the same root
    /// cause issue #35 fixed at the iter-chain level.
    #[test]
    fn test_gc_pinned_roots_slab_population() {
        use crate::fastify::FastifyApp;

        let mut app = FastifyApp::new();
        assert!(app.gc_pinned_roots.is_empty(), "fresh app has empty slab");

        app.add_route("GET", "/a", 0x111);
        app.add_route("POST", "/b", 0x222);
        app.add_hook("onRequest", 0x333);
        app.add_hook("preHandler", 0x444);
        app.set_error_handler(0x555);

        // 2 routes + 2 hooks + 1 error_handler = 5 entries.
        assert_eq!(
            app.gc_pinned_roots.len(),
            5,
            "slab should have one entry per non-zero ClosurePtr"
        );

        // Each entry must carry the POINTER_TAG nibble in the top 16
        // bits so scan_fastify_roots' `mark(f64::from_bits(...))`
        // dispatches the right scanner code path.
        for &bits in &app.gc_pinned_roots {
            assert_eq!(
                bits & 0xFFFF_0000_0000_0000,
                0x7FFD_0000_0000_0000,
                "every slab entry must have NaN-boxed POINTER_TAG; got {:016x}",
                bits
            );
        }

        // The original ClosurePtrs round-trip through the encoding.
        let extracted: std::collections::HashSet<i64> = app
            .gc_pinned_roots
            .iter()
            .map(|b| (b & 0x0000_FFFF_FFFF_FFFF) as i64)
            .collect();
        for ptr in [0x111, 0x222, 0x333, 0x444, 0x555] {
            assert!(
                extracted.contains(&ptr),
                "expected ClosurePtr {:#x} in slab",
                ptr
            );
        }

        // Adding another route incrementally rebuilds the slab.
        app.add_route("DELETE", "/c", 0x666);
        assert_eq!(app.gc_pinned_roots.len(), 6);

        // Zero ClosurePtrs are filtered (avoids marking a bogus
        // 0x7FFD_0000_0000_0000 root).
        let mut zero_app = FastifyApp::new();
        zero_app.add_route("GET", "/", 0);
        assert!(
            zero_app.gc_pinned_roots.is_empty(),
            "zero ClosurePtr must not enter the slab"
        );
    }

    #[test]
    fn test_static_index_respects_prefix() {
        use crate::fastify::FastifyApp;

        let mut app = FastifyApp::with_prefix("/api".to_string());
        app.add_route("GET", "/users", 10);

        assert!(app.static_index.contains_key("GET /api/users"));
        assert!(!app.static_index.contains_key("GET /users"));

        // Lookup against the prefixed path hits the fast path.
        let (route, _) = app.match_route("GET", "/api/users").unwrap();
        assert_eq!(route.handler, 10);

        // Unprefixed lookup correctly misses (neither index nor scan).
        assert!(app.match_route("GET", "/users").is_none());
    }

    #[test]
    fn test_route_with_prefix() {
        let mut app = FastifyApp::with_prefix("/api".to_string());
        app.add_route("GET", "/users", 1);

        // Route should be /api/users
        let (route, _) = app.match_route("GET", "/api/users").unwrap();
        assert_eq!(route.handler, 1);

        // Plain /users should not match
        assert!(app.match_route("GET", "/users").is_none());
    }
}
