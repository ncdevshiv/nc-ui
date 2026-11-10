//! Navigation policy: which origins a page may reach, and which schemes it may use.
//!
//! This is the single most security-relevant decision a `WebView` host makes. The engine will
//! happily navigate anywhere it is told to; the host is what stops `file://`, `javascript:`
//! and an unexpected third-party origin from being reached with the application's privileges.
//!
//! Three rules, applied in order, each of which can refuse on its own:
//!
//! 1. **Scheme.** Anything outside the permitted set is refused. `javascript:` and `data:`
//!    are refused *even when nested*, because a top-level navigation to either runs code in
//!    the current document's origin — the classic way a page escalates itself.
//! 2. **Origin.** The origin must be in the allow-list. Comparison is exact by default,
//!    including case, so a lookalike host cannot slip past.
//! 3. **Subdomains.** Subdomains are permitted only when explicitly opted into, and then only
//!    one label deep, so `evil.example.com` cannot claim `example.com`'s trust.

use core::fmt;

/// The scheme of a URL, reduced to the decision the host actually needs to make.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scheme {
    /// `https:` — the only scheme a production page should be reached over.
    Https,
    /// `http:` — permitted only for local development hosts, and never in production.
    ///
    /// "Local development host" is a concrete, checkable thing rather than a flag an
    /// operator flips: see [`NavigationPolicy::with_loopback_http`]. A loopback origin
    /// never leaves the machine, so the transport it uses cannot be intercepted by anyone
    /// on the network — which is the only thing `https` buys a development server.
    Http,
    /// `file:` — reads the local filesystem. Refused by default in every configuration.
    File,
    /// `data:` — inline content. Refused for top-level navigation; see the module docs.
    Data,
    /// `javascript:` — executes in the current document's origin. Always refused.
    JavaScript,
    /// `blob:` — an object URL, scoped to the creating document.
    Blob,
    /// Anything else, including a scheme this build does not know about.
    Other,
}

impl Scheme {
    /// Parses a scheme from its textual form, case-insensitively as the RFC requires.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        match text.trim().to_ascii_lowercase().as_str() {
            "https" => Self::Https,
            "http" => Self::Http,
            "file" => Self::File,
            "data" => Self::Data,
            "javascript" => Self::JavaScript,
            "blob" => Self::Blob,
            _ => Self::Other,
        }
    }
}

/// A scheme plus host plus port, which together identify a browsing origin.
///
/// Equality is exact. Host comparison is case-insensitive because DNS is, but the *scheme*
/// and *port* are not: `http://example.com` and `https://example.com` are different origins
/// and must not be conflated, because one is a different trust domain from the other.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Origin {
    /// The URL scheme.
    pub scheme: Scheme,
    /// The host, lowercased.
    pub host: String,
    /// The explicit port, if the URL named one.
    pub port: Option<u16>,
}

impl Origin {
    /// Builds an origin, lowercasing the host.
    #[must_use]
    pub fn new(scheme: Scheme, host: &str, port: Option<u16>) -> Self {
        Self {
            scheme,
            host: host.trim().to_ascii_lowercase(),
            port,
        }
    }

    /// The default port for this origin's scheme, which an absent port implies.
    #[must_use]
    pub fn effective_port(&self) -> Option<u16> {
        self.port.or(match self.scheme {
            Scheme::Https => Some(443),
            Scheme::Http => Some(80),
            _ => None,
        })
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scheme = match self.scheme {
            Scheme::Https => "https",
            Scheme::Http => "http",
            Scheme::File => "file",
            Scheme::Data => "data",
            Scheme::JavaScript => "javascript",
            Scheme::Blob => "blob",
            Scheme::Other => "other",
        };
        write!(f, "{scheme}://{}", self.host)?;
        if let Some(port) = self.port {
            write!(f, ":{port}")?;
        }
        Ok(())
    }
}

/// A navigation the engine proposes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavigationRequest {
    /// Where the page wants to go.
    pub target: Origin,
    /// Whether this is a top-level navigation rather than a subframe or a fetch.
    ///
    /// The distinction matters: a page legitimately embeds third-party images from origins it
    /// could never navigate to, and refusing those would break ordinary pages. Only a
    /// top-level navigation can change the document the user is looking at.
    pub top_level: bool,
}

impl NavigationRequest {
    /// Builds a request from a URL string.
    ///
    /// Returns `None` for a URL this module cannot read, which the caller must treat as a
    /// refusal. The parser is deliberately minimal and total in the same way the seam's
    /// probe is: it reads the scheme, the authority and the port, and nothing else. A URL
    /// it cannot read is never guessed at, because a guessed origin is a guess about what
    /// the page is allowed to reach.
    #[must_use]
    pub fn from_url(url: &str, top_level: bool) -> Option<Self> {
        let (scheme, rest) = url.split_once("://")?;
        if rest.is_empty() {
            return None;
        }
        // The authority ends at the first `/`, `?` or `#`. Without this, `https://app/x`
        // yields a host of `app/x`, which matches no allow-list entry and refuses a
        // navigation the policy was written to permit — a real URL would never load.
        let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
        if authority.is_empty() {
            return None;
        }

        // A bare IPv6 literal is bracketed, so the *last* colon is the port separator rather
        // than the first — `rsplit_once` handles both, and a non-numeric tail falls back to
        // being part of the host.
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => match port.parse::<u16>() {
                Ok(port) => (host.to_owned(), Some(port)),
                Err(_) => (authority.to_owned(), None),
            },
            None => (authority.to_owned(), None),
        };

        Some(Self {
            target: Origin::new(Scheme::parse(scheme), &host, port),
            top_level,
        })
    }
}

/// The host's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Allowed.
    Allow,
    /// Refused, with the reason. The reason is part of the API because it is what a test
    /// asserts on and what an operator needs in a log; a bare `Deny` is not diagnosable.
    Deny {
        /// Why the navigation was refused.
        reason: String,
    },
}

impl Verdict {
    /// Whether this verdict permits the navigation.
    #[must_use]
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// The host's navigation rules.
#[derive(Debug, Clone)]
pub struct NavigationPolicy {
    /// Origins a top-level navigation may reach.
    allowed_origins: Vec<Origin>,
    /// Whether a subdomain of an allowed origin is also permitted.
    allow_subdomains: bool,
    /// Whether plain `http:` is permitted at all.
    allow_insecure_http: bool,
    /// Whether `http:` is permitted for loopback hosts.
    allow_loopback_http: bool,
}

impl NavigationPolicy {
    /// A policy permitting exactly the origins given, over `https` only.
    #[must_use]
    pub fn new(origins: &[Origin]) -> Self {
        Self {
            allowed_origins: origins.to_vec(),
            allow_subdomains: false,
            allow_insecure_http: false,
            allow_loopback_http: false,
        }
    }

    /// Permits one label of subdomain beneath each allowed origin.
    ///
    /// One label only: a wildcard for arbitrary depth would let `a.b.c.evil.example.com`
    /// satisfy a rule written for `example.com`.
    #[must_use]
    pub const fn with_subdomains(mut self, allow: bool) -> Self {
        self.allow_subdomains = allow;
        self
    }

    /// Permits plain `http:`, which exists for local development hosts.
    #[must_use]
    pub const fn with_insecure_http(mut self, allow: bool) -> Self {
        self.allow_insecure_http = allow;
        self
    }

    /// Permits plain `http:` for loopback hosts only.
    ///
    /// Off by default, because the general rule is that insecure transport is refused
    /// everywhere; this is the narrow exception that a local development host needs. The
    /// origin allow-list still applies, so a loopback origin has to be *listed* before a
    /// page can reach it — this relaxes the scheme, not the destination.
    ///
    /// A production deployment leaves it off, and then a page served over `http:` is refused
    /// the same way any other insecure origin is.
    #[must_use]
    pub const fn with_loopback_http(mut self, allow: bool) -> Self {
        self.allow_loopback_http = allow;
        self
    }

    /// Decides a navigation.
    #[must_use]
    pub fn evaluate(&self, request: &NavigationRequest) -> Verdict {
        let target = &request.target;

        // Scheme first. `javascript:` executes in the current document's origin, so allowing
        // it would hand any page the ability to act as the application — and it is refused
        // for subframes too, because a subframe running `javascript:` is the same escalation.
        match target.scheme {
            Scheme::JavaScript => {
                return Verdict::Deny {
                    reason: "javascript: executes in the current document's origin".to_owned(),
                };
            }
            Scheme::File => {
                return Verdict::Deny {
                    reason: "file: reads the local filesystem".to_owned(),
                };
            }
            Scheme::Data => {
                return Verdict::Deny {
                    reason: "data: runs inline content with no origin to attribute it to"
                        .to_owned(),
                };
            }
            Scheme::Other => {
                return Verdict::Deny {
                    reason: "unrecognised scheme".to_owned(),
                };
            }
            Scheme::Http if !self.http_scheme_is_permitted(target) => {
                return Verdict::Deny {
                    reason: "http: is not permitted; use https".to_owned(),
                };
            }
            Scheme::Https | Scheme::Http | Scheme::Blob => {}
        }

        // `blob:` inherits the origin of whatever created it, so the engine — not the host —
        // is what resolves it. Reaching here without a real host means the URL was
        // unparseable rather than merely unusual.
        if target.host.is_empty() {
            return Verdict::Deny {
                reason: "navigation target has no host".to_owned(),
            };
        }

        if self.origin_is_allowed(target) {
            return Verdict::Allow;
        }

        // A subframe is permitted to load from an origin the page cannot itself navigate to:
        // that is how every ordinary page embeds third-party images and fonts. The *document*
        // is still protected, because only a top-level navigation changes what the user sees
        // and what the application's privileges apply to.
        if !request.top_level {
            return Verdict::Allow;
        }

        Verdict::Deny {
            reason: format!("{target} is not an allowed origin"),
        }
    }

    /// Whether `target` is in the allow-list, exactly or as one label of subdomain.
    fn origin_is_allowed(&self, target: &Origin) -> bool {
        self.allowed_origins.iter().any(|allowed| {
            if allowed.scheme != target.scheme
                || allowed.effective_port() != target.effective_port()
            {
                return false;
            }
            if allowed.host == target.host {
                return true;
            }
            self.allow_subdomains && is_one_label_subdomain(&allowed.host, &target.host)
        })
    }

    /// Whether the `http:` scheme is permitted for this particular target.
    fn http_scheme_is_permitted(&self, target: &Origin) -> bool {
        self.allow_insecure_http || (self.allow_loopback_http && is_loopback_host(&target.host))
    }
}

/// Whether a host names this machine's loopback interface.
///
/// The set is the one RFC 6761 and the IPv4/IPv6 specifications actually define:
///
/// - `localhost` and any name under `.localhost`, which RFC 6761 reserves to the loopback.
/// - `127.0.0.0/8`, the whole IPv4 loopback block rather than only `127.0.0.1`, because a
///   listener bound to `127.0.0.2` is exactly as local as one bound to `127.0.0.1`.
/// - `::1`, the IPv6 loopback, bracketed or bare.
///
/// A name that merely *looks* like one of these — `localhost.evil.test`, `127.0.0.1.evil.test`,
/// `notlocalhost` — is not one, and a DNS-resolvable name is never treated as loopback even
/// when it is spelled `localhost`: if it resolves at all, it was resolved by someone else.
#[must_use]
pub fn is_loopback_host(host: &str) -> bool {
    let host = host.trim();
    if host.is_empty() {
        return false;
    }
    // A bracketed IPv6 literal carries brackets in the authority; the bare form appears when
    // the URL omitted a port and the parser split on the wrong colon.
    let host = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host);

    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if host.len() > "localhost".len() && host.to_ascii_lowercase().ends_with(".localhost") {
        return true;
    }
    if host.eq_ignore_ascii_case("::1") {
        return true;
    }
    // The IPv4 loopback block: four dot-separated decimal octets whose first is 127.
    let mut octets = host.split('.');
    let mut count = 0;
    let mut first_is_loopback = false;
    for octet in &mut octets {
        count += 1;
        if count > 4 {
            return false;
        }
        if octet.is_empty() || !octet.bytes().all(|byte| byte.is_ascii_digit()) {
            return false;
        }
        if octet.len() > 3 || octet.parse::<u16>().map_or(true, |value| value > 255) {
            return false;
        }
        if count == 1 {
            first_is_loopback = octet.parse::<u16>() == Ok(127);
        }
    }
    count == 4 && first_is_loopback
}

/// Whether `child` is exactly one label beneath `parent`.
///
/// `app.example.com` is one label beneath `example.com`: the prefix is `"app."`, and stripping
/// the trailing dot leaves a single label with no dot in it. `a.b.example.com` is *not* — its
/// prefix is `"a.b."`, which still contains a dot — and `notexample.com` is not, because the
/// suffix has to land on a label boundary.
///
/// The earlier version tested `!prefix.ends_with('.')`, which rejects every well-formed
/// subdomain including the one-label case: the prefix of a valid subdomain always ends with the
/// dot that separates it from the parent. That made the feature a no-op rather than a
/// permissive one, which is the safer direction but is not what the option says it does.
fn is_one_label_subdomain(parent: &str, child: &str) -> bool {
    let Some(prefix) = child.strip_suffix(parent) else {
        return false;
    };
    // The prefix must be exactly `"<label>."`. Requiring the trailing dot is what separates a
    // real subdomain from a lookalike: `notexample.com` also *ends with* `example.com`, with
    // `not` as the prefix, and accepting a prefix without the separating dot would treat that
    // lookalike as a subdomain of the parent — the exact confusion the rule exists to prevent.
    let Some(label) = prefix.strip_suffix('.') else {
        return false;
    };
    !label.is_empty() && !label.contains('.')
}

#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]
#[cfg(test)]
mod tests {
    use super::*;

    fn https(host: &str) -> Origin {
        Origin::new(Scheme::Https, host, None)
    }

    fn policy() -> NavigationPolicy {
        NavigationPolicy::new(&[https("app.example.com"), https("example.com")])
    }

    fn top_level(target: Origin) -> NavigationRequest {
        NavigationRequest {
            target,
            top_level: true,
        }
    }

    #[test]
    fn an_allowed_origin_is_permitted() {
        assert!(
            policy()
                .evaluate(&top_level(https("app.example.com")))
                .is_allowed()
        );
        assert!(
            policy()
                .evaluate(&top_level(https("example.com")))
                .is_allowed()
        );
    }

    #[test]
    fn javascript_is_refused_even_as_a_subframe() {
        // The case that matters most: a subframe running `javascript:` executes in the
        // *parent's* origin, so refusing it only for top-level navigations would leave the
        // escalation wide open.
        for top_level in [true, false] {
            let request = NavigationRequest {
                target: Origin::new(Scheme::JavaScript, "app.example.com", None),
                top_level,
            };
            let verdict = policy().evaluate(&request);
            assert!(
                !verdict.is_allowed(),
                "javascript: was allowed (top_level={top_level})"
            );
        }
    }

    #[test]
    fn file_and_data_are_refused_even_though_they_carry_no_remote_origin() {
        for scheme in [Scheme::File, Scheme::Data] {
            let request = NavigationRequest {
                target: Origin::new(scheme, "app.example.com", None),
                top_level: true,
            };
            assert!(
                !policy().evaluate(&request).is_allowed(),
                "{scheme:?} was allowed"
            );
        }
    }

    #[test]
    fn plain_http_is_refused_unless_the_policy_opts_in() {
        // The allow-list is written over https, so an http origin is absent from it and is
        // refused twice over: once for the scheme and once for not being listed.
        let insecure = Origin::new(Scheme::Http, "app.example.com", None);
        assert!(!policy().evaluate(&top_level(insecure.clone())).is_allowed());

        // Enabling insecure http relaxes the *scheme*, not the allow-list: a host still has to
        // be listed, and it has to be listed under the scheme it will actually be reached at.
        let dev = NavigationPolicy::new(&[Origin::new(Scheme::Http, "localhost", Some(8080))])
            .with_insecure_http(true);
        assert!(
            dev.evaluate(&top_level(Origin::new(
                Scheme::Http,
                "localhost",
                Some(8080)
            )))
            .is_allowed()
        );
        // A different port is a different origin, even when the scheme is permitted.
        assert!(
            !dev.evaluate(&top_level(Origin::new(
                Scheme::Http,
                "localhost",
                Some(9999)
            )))
            .is_allowed()
        );
        // Still refused without the opt-in.
        assert!(
            !NavigationPolicy::new(&[Origin::new(Scheme::Http, "localhost", Some(8080))])
                .evaluate(&top_level(Origin::new(
                    Scheme::Http,
                    "localhost",
                    Some(8080)
                )))
                .is_allowed()
        );
    }

    #[test]
    fn a_scheme_mismatch_is_a_different_origin() {
        // `https://example.com` being in the list must not permit `http://example.com`,
        // even with insecure http enabled — the port defaults differ, so they differ twice.
        let policy = policy().with_insecure_http(true);
        assert!(
            !policy
                .evaluate(&top_level(Origin::new(Scheme::Http, "example.com", None)))
                .is_allowed()
        );
    }

    #[test]
    fn a_third_party_top_level_navigation_is_refused() {
        let verdict = policy().evaluate(&top_level(https("evil.test")));
        assert!(!verdict.is_allowed());
        assert!(
            matches!(&verdict, Verdict::Deny { reason } if reason.contains("evil.test")),
            "the refusal must name the origin it refused; got {verdict:?}"
        );
    }

    #[test]
    fn a_subframe_may_embed_an_origin_the_page_cannot_navigate_to() {
        // Every ordinary page loads third-party images and fonts. Refusing those would break
        // the web rather than secure it, and only a top-level navigation changes the document
        // the application's privileges apply to.
        let request = NavigationRequest {
            target: https("cdn.thirdparty.test"),
            top_level: false,
        };
        assert!(policy().evaluate(&request).is_allowed());
        assert!(
            !policy()
                .evaluate(&top_level(https("cdn.thirdparty.test")))
                .is_allowed()
        );
    }

    #[test]
    fn subdomains_are_refused_until_the_policy_opts_in() {
        let strict = policy();
        assert!(
            !strict
                .evaluate(&top_level(https("staging.example.com")))
                .is_allowed()
        );

        let relaxed = policy().with_subdomains(true);
        assert!(
            relaxed
                .evaluate(&top_level(https("staging.example.com")))
                .is_allowed()
        );
    }

    #[test]
    fn subdomain_permission_is_exactly_one_label_deep() {
        let policy = policy().with_subdomains(true);
        assert!(
            policy
                .evaluate(&top_level(https("staging.example.com")))
                .is_allowed()
        );
        // Two labels is not covered: a rule written for `example.com` should not extend to
        // `a.b.example.com`, which is an attacker-controlled subdomain of an attacker-
        // controlled subdomain.
        assert!(
            !policy
                .evaluate(&top_level(https("a.b.example.com")))
                .is_allowed()
        );
        // And a lookalike host whose name merely ends with the parent is not a subdomain.
        assert!(
            !policy
                .evaluate(&top_level(https("notexample.com")))
                .is_allowed()
        );
        assert!(
            !policy
                .evaluate(&top_level(https("evilexample.com")))
                .is_allowed()
        );
    }

    #[test]
    fn host_comparison_is_case_insensitive_but_scheme_comparison_is_not() {
        let policy = policy();
        assert!(
            policy
                .evaluate(&top_level(https("APP.EXAMPLE.COM")))
                .is_allowed()
        );
        // A scheme is a trust boundary, not a spelling: `Https` and `Http` never match.
        assert!(
            !policy
                .evaluate(&top_level(Origin::new(
                    Scheme::Http,
                    "app.example.com",
                    Some(443)
                )))
                .is_allowed()
        );
    }

    #[test]
    fn an_explicit_port_must_match_the_effective_one() {
        let policy = NavigationPolicy::new(&[https("example.com")]);
        assert!(
            policy
                .evaluate(&top_level(https("example.com")))
                .is_allowed()
        );
        // 443 is the default for https, so naming it explicitly is the same origin.
        assert!(
            policy
                .evaluate(&top_level(Origin::new(
                    Scheme::Https,
                    "example.com",
                    Some(443)
                )))
                .is_allowed()
        );
        // A different port is a different origin.
        assert!(
            !policy
                .evaluate(&top_level(Origin::new(
                    Scheme::Https,
                    "example.com",
                    Some(8443)
                )))
                .is_allowed()
        );
    }

    #[test]
    fn a_navigation_with_no_host_is_refused() {
        let verdict = policy().evaluate(&top_level(https("")));
        assert!(!verdict.is_allowed());
    }

    #[test]
    fn loopback_http_is_permitted_only_when_the_policy_opts_in() {
        // A development host serves the application over loopback http. The origin is
        // listed, so the only thing standing between the page and its own assets is the
        // scheme — and the scheme is permitted for exactly this case.
        let dev = NavigationPolicy::new(&[Origin::new(Scheme::Http, "127.0.0.1", Some(4321))])
            .with_loopback_http(true);
        assert!(
            dev.evaluate(&top_level(Origin::new(
                Scheme::Http,
                "127.0.0.1",
                Some(4321)
            )))
            .is_allowed()
        );

        // Without the opt-in the same navigation is refused: the default is that insecure
        // transport is refused everywhere.
        assert!(
            !NavigationPolicy::new(&[Origin::new(Scheme::Http, "127.0.0.1", Some(4321))])
                .evaluate(&top_level(Origin::new(
                    Scheme::Http,
                    "127.0.0.1",
                    Some(4321)
                )))
                .is_allowed()
        );

        // The opt-in relaxes the scheme, not the destination: a loopback scheme allowance
        // does not put a remote host on the allow-list.
        assert!(
            !dev.evaluate(&top_level(Origin::new(
                Scheme::Http,
                "example.com",
                Some(4321)
            )))
            .is_allowed()
        );
    }

    #[test]
    fn loopback_host_matching_is_exact() {
        for host in [
            "localhost",
            "LOCALHOST",
            "app.localhost",
            "127.0.0.1",
            "127.0.0.2",
            "127.255.255.254",
            "::1",
            "[::1]",
        ] {
            assert!(is_loopback_host(host), "{host} should be loopback");
        }
        for host in [
            "",
            "localhost.evil.test",
            "127.0.0.1.evil.test",
            "notlocalhost",
            "127.0.0.256",
            "127.0.0",
            "127.0.0.1.2",
            "example.com",
            "128.0.0.1",
            "::2",
        ] {
            assert!(!is_loopback_host(host), "{host} should not be loopback");
        }
    }

    #[test]
    fn a_denial_always_carries_a_reason() {
        // An undiagnosable refusal is not an acceptable API: it is what a test asserts on and
        // what an operator has to act on.
        for target in [
            Origin::new(Scheme::JavaScript, "app.example.com", None),
            Origin::new(Scheme::File, "c:/windows", None),
            https("evil.test"),
            https(""),
        ] {
            match policy().evaluate(&top_level(target)) {
                Verdict::Allow => panic!("expected a denial"),
                Verdict::Deny { reason } => {
                    assert!(!reason.trim().is_empty(), "a denial arrived with no reason");
                }
            }
        }
    }
}
