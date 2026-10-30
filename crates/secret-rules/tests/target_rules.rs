//! M1, M2, M3 and M4: host, port and path matching, redirects and authority.

use secret_rules::{
    authority_matches_connect, canonical_host, normalize_request_target, path_covers,
    redirect::resolve_location, ConnectAuthority, RequestTarget, RuleTarget, TargetRefusal,
};

fn rules<'a>(host: &'a str, port: u16, prefix: &'a str) -> RuleTarget<'a> {
    RuleTarget {
        host,
        port,
        path_prefix: prefix,
    }
}

fn request<'a>(host: &'a str, port: u16, scheme: &'a str, target: &'a str) -> RequestTarget<'a> {
    RequestTarget {
        host: host.to_string(),
        port,
        scheme,
        target,
    }
}

#[test]
fn host_match_is_exact_after_lowercasing_and_idna() {
    let target = request("api.example.com", 443, "https", "/v1/x");
    assert!(target
        .matches(&rules("API.Example.COM", 443, "/v1"))
        .is_ok());
    assert_eq!(
        target.matches(&rules("api.example.com.evil", 443, "/v1")),
        Err(TargetRefusal::HostMismatch)
    );
    assert_eq!(
        target.matches(&rules("other.example.com", 443, "/v1")),
        Err(TargetRefusal::HostMismatch)
    );
    // IDNA: punycode and its Unicode spelling are the same host.
    assert_eq!(
        canonical_host("BÜCHER.example").unwrap(),
        "xn--bcher-kva.example"
    );
    let unicode = request("bücher.example", 443, "https", "/v1");
    assert!(unicode
        .matches(&rules("xn--bcher-kva.example", 443, "/v1"))
        .is_ok());
    assert_eq!(canonical_host("[2001:DB8::1]").unwrap(), "[2001:db8::1]");
}

#[test]
fn scheme_and_port_must_match() {
    let target = request("api.example.com", 443, "https", "/v1/x");
    assert!(target
        .matches(&rules("api.example.com", 443, "/v1"))
        .is_ok());
    assert_eq!(
        target.matches(&rules("api.example.com", 8443, "/v1")),
        Err(TargetRefusal::PortMismatch)
    );
    let http = request("api.example.com", 443, "http", "/v1/x");
    assert_eq!(
        http.matches(&rules("api.example.com", 443, "/v1")),
        Err(TargetRefusal::SchemeNotHttps)
    );
    let default_port = request("api.example.com", 443, "https", "/v1/x");
    assert!(default_port
        .matches(&rules("api.example.com", 443, "/v1"))
        .is_ok());
}

#[test]
fn path_prefix_matches_whole_segments() {
    assert!(path_covers("/repos/a", "/repos/a"));
    assert!(path_covers("/repos/a/files", "/repos/a"));
    assert!(!path_covers("/repos/ab", "/repos/a"));
    assert!(!path_covers("/repos/a2", "/repos/a"));
    assert!(path_covers("/anything", "/"));
    assert!(!path_covers("/v1", "/v1/x"));
}

#[test]
fn path_normalization_removes_dot_segments_and_decodes_unreserved() {
    let cases = [
        ("/repos/a/./b", "/repos/a/b"),
        ("/repos/a/../b", "/repos/b"),
        ("/repos/a/b/..", "/repos/a"),
        ("/repos/%61/b", "/repos/a/b"),
    ];
    for (input, expected) in cases {
        match normalize_request_target(input) {
            Ok(path) => assert_eq!(path.value, expected, "{input}"),
            Err(err) => panic!("{input} should normalize, got {err:?}"),
        }
    }
    assert_eq!(
        normalize_request_target("/repos/a/..;/admin"),
        Err(TargetRefusal::AmbiguousPathSeparator)
    );
    // Escaped dots are a second spelling of a dot segment: AFT and the server
    // could each normalise a different one, and a server that decodes twice
    // would resolve the segment away, so they refuse (M2).
    assert_eq!(
        normalize_request_target("/repos/a/%2E%2E/b"),
        Err(TargetRefusal::AmbiguousPathSeparator)
    );
    assert_eq!(
        normalize_request_target("/repos/a/%5C../b"),
        Err(TargetRefusal::AmbiguousPathSeparator)
    );
    assert_eq!(
        normalize_request_target("/repos/a/%2F/b"),
        Err(TargetRefusal::AmbiguousPathSeparator)
    );
    assert_eq!(
        normalize_request_target("/repos/a\\..\\b"),
        Err(TargetRefusal::AmbiguousPathSeparator)
    );
    // M2: an empty segment never matches, in any spelling.
    for input in ["/repos/a//../b", "/repos//a", "/repos/a//b", "//repos/a"] {
        assert_eq!(
            normalize_request_target(input),
            Err(TargetRefusal::EmptySegment),
            "{input}"
        );
    }
    assert_eq!(
        normalize_request_target("repos/a"),
        Err(TargetRefusal::NotOriginForm)
    );
}

#[test]
fn the_emitted_path_is_the_normalized_path_not_the_client_bytes() {
    let target = request("api.example.com", 443, "https", "/repos/%61/../b/./c?x=1");
    let matched = target
        .matches(&rules("api.example.com", 443, "/repos"))
        .unwrap();
    assert_eq!(matched.value, "/repos/b/c");
    assert_eq!(matched.wire, b"/repos/b/c?x=1");
    let awkward = request("api.example.com", 443, "https", "/repos/a b");
    let matched = awkward
        .matches(&rules("api.example.com", 443, "/repos"))
        .unwrap();
    assert_eq!(matched.wire, b"/repos/a%20b");
}

#[test]
fn http2_path_forms_follow_the_same_rules() {
    // An HTTP/2 `:path` pseudo-header carries the same origin-form value.
    let h2_path = "/repos/a?per_page=10";
    let target = request("api.example.com", 443, "https", h2_path);
    assert!(target
        .matches(&rules("api.example.com", 443, "/repos"))
        .is_ok());
    let h2_outside = request("api.example.com", 443, "https", "/repos/ab");
    assert_eq!(
        h2_outside.matches(&rules("api.example.com", 443, "/repos/a")),
        Err(TargetRefusal::PathOutsidePrefix)
    );
    let h2_absolute = request(
        "api.example.com",
        443,
        "https",
        "https://api.example.com/repos/a",
    );
    let matched = h2_absolute
        .matches(&rules("api.example.com", 443, "/repos"))
        .unwrap();
    assert_eq!(matched.wire, b"/repos/a");
}

#[test]
fn connect_authority_is_canonical_and_requests_must_match_it() {
    let connect = ConnectAuthority::parse("API.Example.com:443").unwrap();
    assert_eq!(connect.authority(), "api.example.com:443");
    assert!(authority_matches_connect(&connect, Some("api.example.com"), None, "/v1").is_ok());
    assert!(authority_matches_connect(
        &connect,
        Some("api.example.com:443"),
        Some("api.example.com:443"),
        "/v1"
    )
    .is_ok());
    assert_eq!(
        authority_matches_connect(&connect, Some("other.example.com"), None, "/v1"),
        Err(TargetRefusal::AuthorityMismatch)
    );
    assert_eq!(
        authority_matches_connect(&connect, None, Some("api.example.com:444"), "/v1"),
        Err(TargetRefusal::AuthorityMismatch)
    );
    assert_eq!(
        authority_matches_connect(&connect, None, None, "https://other.example.com/v1"),
        Err(TargetRefusal::AuthorityMismatch)
    );
    assert!(authority_matches_connect(&connect, None, None, "https://api.example.com/v1").is_ok());
}

#[test]
fn redirects_are_resolved_against_the_current_request() {
    let current = request("api.example.com", 443, "https", "/v1/start");
    assert_eq!(
        resolve_location("/v1/next", &current).unwrap(),
        "https://api.example.com/v1/next"
    );
    assert_eq!(
        resolve_location("next?x=1", &current).unwrap(),
        "https://api.example.com/v1/next?x=1"
    );
    assert_eq!(
        resolve_location("https://api.example.com/v1/x", &current).unwrap(),
        "https://api.example.com/v1/x"
    );
    assert_eq!(
        resolve_location("//other.example.com/v1/x", &current).unwrap(),
        "https://other.example.com/v1/x"
    );
    let non_default = request("api.example.com", 8443, "https", "/v1/start");
    assert_eq!(
        resolve_location("/v1/next", &non_default).unwrap(),
        "https://api.example.com:8443/v1/next"
    );
}

#[test]
fn a_redirect_hop_to_another_host_scheme_or_port_is_not_authorized() {
    use secret_rules::{evaluate_redirect, LabelRule, Location, RedirectOutcome};
    let rule = LabelRule {
        label: "repo_key".to_string(),
        host: "api.example.com".to_string(),
        port: 443,
        path_prefix: "/v1".to_string(),
        location: Location::Query("key".to_string()),
        grant_id: "g".to_string(),
    };
    let rules = [rule];
    let same = request("api.example.com", 443, "https", "/v1/next");
    assert_eq!(
        evaluate_redirect(&rules, &same)[0].outcome,
        RedirectOutcome::Authorized
    );
    let other_host = request("evil.example.com", 443, "https", "/v1/next");
    assert_eq!(
        evaluate_redirect(&rules, &other_host)[0].outcome,
        RedirectOutcome::NotAuthorized
    );
    let http = request("api.example.com", 443, "http", "/v1/next");
    assert_eq!(
        evaluate_redirect(&rules, &http)[0].outcome,
        RedirectOutcome::NotAuthorized
    );
    let other_port = request("api.example.com", 8443, "https", "/v1/next");
    assert_eq!(
        evaluate_redirect(&rules, &other_port)[0].outcome,
        RedirectOutcome::NotAuthorized
    );
    let outside = request("api.example.com", 443, "https", "/v2/next");
    assert_eq!(
        evaluate_redirect(&rules, &outside)[0].outcome,
        RedirectOutcome::NotAuthorized
    );
}
