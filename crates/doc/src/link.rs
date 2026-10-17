//! Which hyperlink targets may leave the document: followed by the OS, or written into an
//! exported HTML/Markdown file where a browser would run them.

/// The longest link handed to the system, in bytes: far beyond any real URL, short of a document
/// making the opener copy megabytes.
const MAX_FOLLOWABLE_LEN: usize = 8192;

/// The `mailto:` header fields a followed link keeps (RFC 6068). Everything else is dropped:
/// some mail clients honour `attach=`/`attachment=`, so a document could attach a local file.
const MAILTO_FIELDS: [&str; 5] = ["to", "cc", "bcc", "subject", "body"];

/// Whether Ctrl/⌘+click may hand a document's hyperlink to the system: see [`followable_link`].
pub fn is_web_or_mail_link(url: &str) -> bool {
    followable_link(url).is_some()
}

/// The link to hand to the system (browser or mail client) when a document's hyperlink is
/// Ctrl/⌘+clicked, or `None` when it must not be followed, so a document can't launch `file:`
/// paths, programs or custom-scheme handlers. Only two shapes are followed, parsed strictly
/// rather than by their prefix:
/// - `http://` or `https://` with a valid host (a domain name, a dotted-quad IPv4 address or a
///   bracketed IPv6 address) and an optional port in `1..=65535`. User info (`user@host`, which
///   disguises the real host) and backslashes (read as `/` by browsers) are refused.
/// - `mailto:` keeping only the `to`, `cc`, `bcc`, `subject` and `body` fields (so no `attach=`),
///   without a fragment.
///
/// The result is normalised: the scheme and an ASCII host are lower-cased, and spaces, non-ASCII
/// characters and stray `%` signs in the path, query and fragment are percent-encoded. Exact
/// otherwise: a leading or trailing space or any control character makes it not a link. A link
/// the opener could not parse must never get this far: `webbrowser` would retry it as a local
/// file path.
pub fn followable_link(url: &str) -> Option<String> {
    if url.len() > MAX_FOLLOWABLE_LEN || url.chars().any(char::is_control) || url.trim() != url {
        return None;
    }
    let (scheme, rest) = url.split_once(':')?;
    let scheme = scheme.to_ascii_lowercase();
    match scheme.as_str() {
        "http" | "https" => web_link(&scheme, rest),
        "mailto" => mail_link(rest),
        _ => None,
    }
}

/// `http(s)://host[:port][/path][?query][#fragment]`, from after the scheme's `:`.
fn web_link(scheme: &str, rest: &str) -> Option<String> {
    let rest = rest.strip_prefix("//")?;
    if rest.contains('\\') {
        return None;
    }
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at_checked(end)?;
    if authority.contains('@') {
        return None;
    }
    let (host, port) = host_and_port(authority)?;
    let mut out = format!("{scheme}://{host}");
    if let Some(port) = port {
        out.push_str(&format!(":{port}"));
    }
    let (before_fragment, fragment) = match tail.split_once('#') {
        Some((b, f)) => (b, Some(f)),
        None => (tail, None),
    };
    let (path, query) = match before_fragment.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (before_fragment, None),
    };
    push_encoded(&mut out, path);
    if let Some(q) = query {
        out.push('?');
        push_encoded(&mut out, q);
    }
    if let Some(f) = fragment {
        out.push('#');
        push_encoded(&mut out, f);
    }
    Some(out)
}

/// The normalised host and the port (`None` when absent or empty) of an authority without user
/// info.
fn host_and_port(authority: &str) -> Option<(String, Option<u16>)> {
    let (host, port) = if let Some(inner) = authority.strip_prefix('[') {
        let (v6, after) = inner.split_once(']')?;
        if !is_ipv6(v6) {
            return None;
        }
        let port = if after.is_empty() { None } else { Some(after.strip_prefix(':')?) };
        (format!("[{}]", v6.to_ascii_lowercase()), port)
    } else {
        let (host, port) = match authority.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        };
        (domain_or_ipv4(host)?, port)
    };
    let port = match port {
        None | Some("") => None,
        Some(p) if p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit()) => Some(p.parse::<u16>().ok().filter(|n| *n > 0)?),
        Some(_) => return None,
    };
    Some((host, port))
}

/// A domain name (labels of letters, digits, `-` and `_`; non-ASCII letters allowed for
/// internationalised names) or, when the last label is a number, a dotted-quad IPv4 address.
/// Lower-cased.
fn domain_or_ipv4(host: &str) -> Option<String> {
    let name = host.strip_suffix('.').unwrap_or(host);
    if name.is_empty() || name.chars().count() > 253 {
        return None;
    }
    let labels: Vec<&str> = name.split('.').collect();
    let label_ok = |l: &&str| {
        !l.is_empty()
            && l.chars().count() <= 63
            && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || (!c.is_ascii() && c.is_alphanumeric()))
    };
    if !labels.iter().all(label_ok) {
        return None;
    }
    // A URL parser reads a host whose last label is a number (`1.2.3.4`, `0x7f`, `1.2.3.256`) as
    // an IPv4 address and fails when it isn't one: only an exact dotted quad is accepted.
    let last = labels.last()?;
    let numeric = last.bytes().all(|b| b.is_ascii_digit()) || last.get(..2).is_some_and(|p| p.eq_ignore_ascii_case("0x"));
    if numeric && !is_ipv4(name) {
        return None;
    }
    Some(host.to_ascii_lowercase())
}

/// Exactly four decimal parts in `0..=255`, without leading zeros (which parsers read as octal).
fn is_ipv4(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 4
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.len() <= 3
                && p.bytes().all(|b| b.is_ascii_digit())
                && (p.len() == 1 || !p.starts_with('0'))
                && p.parse::<u16>().is_ok_and(|n| n <= 255)
        })
}

/// An IPv6 address as written inside `[` `]` (RFC 4291 §2.2): eight groups of one to four hex
/// digits, at most one `::` standing for one or more zero groups, and optionally a dotted-quad
/// IPv4 address in place of the last two groups. No zone id.
fn is_ipv6(s: &str) -> bool {
    if s.len() > 45 {
        return false;
    }
    let (head, tail, compressed) = match s.split_once("::") {
        Some((h, t)) => (h, t, true),
        None => (s, "", false),
    };
    if tail.contains("::") {
        return false;
    }
    let groups_of = |x: &str| -> Vec<String> { if x.is_empty() { Vec::new() } else { x.split(':').map(str::to_string).collect() } };
    let mut parts = groups_of(head);
    parts.extend(groups_of(tail));
    // The IPv4 form may only end the address (`::ffff:1.2.3.4`, not `1.2.3.4::`).
    let v4_allowed = !compressed || !tail.is_empty();
    let n = parts.len();
    let mut groups = 0usize;
    for (i, g) in parts.iter().enumerate() {
        if i + 1 == n && v4_allowed && g.contains('.') {
            if !is_ipv4(g) {
                return false;
            }
            groups += 2;
        } else if !g.is_empty() && g.len() <= 4 && g.bytes().all(|b| b.is_ascii_hexdigit()) {
            groups += 1;
        } else {
            return false;
        }
    }
    if compressed { groups <= 7 } else { groups == 8 }
}

/// `mailto:` from after the scheme's `:`: the addresses, then only the allowed header fields.
fn mail_link(rest: &str) -> Option<String> {
    let rest = rest.split_once('#').map_or(rest, |(r, _)| r);
    let (to, query) = match rest.split_once('?') {
        Some((t, q)) => (t, Some(q)),
        None => (rest, None),
    };
    let mut out = String::from("mailto:");
    push_encoded(&mut out, to);
    let mut fields = Vec::new();
    for pair in query.unwrap_or("").split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        // Compared as written: a percent-encoded name (`%61ttach`) isn't on the list either.
        let Some(key) = MAILTO_FIELDS.iter().find(|f| key.eq_ignore_ascii_case(f)) else { continue };
        let mut field = format!("{key}=");
        push_encoded(&mut field, value);
        fields.push(field);
    }
    if !fields.is_empty() {
        out.push('?');
        out.push_str(&fields.join("&"));
    }
    (out.len() > "mailto:".len()).then_some(out)
}

/// Appends `s`, percent-encoding every byte outside RFC 3986's unreserved and reserved characters
/// (spaces, quotes, `<>`, `{}`, `|`, `^`, `` ` ``, `[]`, `#` and all non-ASCII) and a `%` that
/// doesn't start a `%XX` escape.
fn push_encoded(out: &mut String, s: &str) {
    let bytes = s.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        let escape_follows = || bytes.get(i + 1).is_some_and(u8::is_ascii_hexdigit) && bytes.get(i + 2).is_some_and(u8::is_ascii_hexdigit);
        if b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@/?".contains(&b) || (b == b'%' && escape_follows()) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
}

/// Whether a hyperlink may be written as an `href` (or Markdown destination, RTF `HYPERLINK`
/// field, ODT `xlink:href`, LaTeX `\href`) in an exported file: a web or email link, a bookmark
/// (`#name`) or a relative path. Scripts (`javascript:`, `vbscript:`), `data:`, `file:` and every
/// other scheme are refused, read the way a browser reads a URL: ASCII control characters and
/// spaces are dropped first, so `java\nscript:` is a script. A protocol-relative reference
/// (`//host/…`) or a UNC path (`\\host\…`, or `//host` once backslashes are read as separators)
/// names another machine — following it on Windows leaks the user's NTLM credentials — so it is
/// refused too.
pub fn is_safe_href(url: &str) -> bool {
    let cleaned: String = url.chars().filter(|c| !c.is_ascii_control() && *c != ' ').collect();
    // `//host` (protocol-relative) and `\\host` (UNC), with `\` read as `/` the way Windows does.
    if cleaned.replace('\\', "/").starts_with("//") {
        return false;
    }
    match scheme(url) {
        Some(s) => matches!(s.as_str(), "http" | "https" | "mailto"),
        None => !cleaned.is_empty(),
    }
}

/// The scheme a browser would see in `url` (RFC 3986 §3.1: a letter, then letters, digits, `+`,
/// `-` or `.`, up to the first `:`), lower-cased; `None` for a relative reference.
fn scheme(url: &str) -> Option<String> {
    let cleaned: String = url.chars().filter(|c| !c.is_ascii_control() && *c != ' ').collect();
    let (s, _) = cleaned.split_once(':')?;
    let mut chars = s.chars();
    let first = chars.next()?;
    (first.is_ascii_alphabetic() && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))).then(|| s.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WC-28 (#2769): links are parsed, not prefix-checked. Real links keep working (normalised);
    /// a link with a bad host or port, user info or backslashes is refused, so the opener never
    /// gets a string it can't parse (`webbrowser` would retry it as a local file path).
    #[test]
    fn followed_links_are_parsed_and_normalised() {
        for (link, want) in [
            ("https://example.com/a?b#c", "https://example.com/a?b#c"),
            ("HTTPS://EXAMPLE.COM", "https://example.com"),
            ("http://example.com:8080/x/y.html?q=1&r=a%20b#frag", "http://example.com:8080/x/y.html?q=1&r=a%20b#frag"),
            ("https://example.com/my file.pdf", "https://example.com/my%20file.pdf"),
            ("https://example.com/100%", "https://example.com/100%25"),
            ("https://example.com/a\"<b>", "https://example.com/a%22%3Cb%3E"),
            ("https://example.com/caf\u{e9}", "https://example.com/caf%C3%A9"),
            ("https://b\u{fc}cher.example/", "https://b\u{fc}cher.example/"),
            ("https://sub_domain.example.com.", "https://sub_domain.example.com."),
            ("http://127.0.0.1:3000/", "http://127.0.0.1:3000/"),
            ("http://[::1]:8080/", "http://[::1]:8080/"),
            ("http://[2001:DB8::ffff:192.0.2.1]/", "http://[2001:db8::ffff:192.0.2.1]/"),
            ("http://localhost:/", "http://localhost/"),
            ("https://example.com#a#b", "https://example.com#a%23b"),
        ] {
            assert_eq!(followable_link(link).as_deref(), Some(want), "{link}");
        }
        for bad in [
            // The audit's example: an out-of-range port made the URL unparsable.
            "http://x:99999/../../../../home/u/Downloads/evil.html",
            "http://x:0/",
            "http://x:-1/",
            "http://x:8a/",
            "http://x:1:2/",
            "http:example.com",
            "http:/example.com",
            "http://",
            "http:///etc/passwd",
            "http://exa mple.com/",
            "http://exa%6Dple.com/",
            "http://example..com/",
            "http://-\u{202e}gro.example/",
            "http://good.example@evil.example/",
            "http://user:pass@example.com/",
            "http://evil.example\\@good.example/",
            "https://example.com\\..\\x",
            "http://1.2.3.256/",
            "http://0x7f.1/",
            "http://010.0.0.1/",
            "http://1.2.3/",
            "http://[::1/",
            "http://[1:2:3:4:5:6:7:8:9]/",
            "http://[1::2::3]/",
            "http://[1.2.3.4::]/",
            "http://[fe80::1%25eth0]/",
            "http://[::1]x/",
            "https://example.com/\u{7f}",
            "https://example.com/\u{85}",
            "https://example.com/ ",
        ] {
            assert_eq!(followable_link(bad), None, "{bad:?}");
        }
        let long = format!("https://example.com/{}", "a".repeat(MAX_FOLLOWABLE_LEN));
        assert_eq!(followable_link(&long), None);
    }

    /// WC-29 (#2770): a `mailto:` link keeps only the address and the to/cc/bcc/subject/body
    /// fields, so a document can't make the mail client attach a local file.
    #[test]
    fn mailto_links_keep_only_address_and_safe_fields() {
        for (link, want) in [
            ("mailto:someone@example.com", "mailto:someone@example.com"),
            ("MAILTO:a@example.com?subject=Hi%20there&body=Line", "mailto:a@example.com?subject=Hi%20there&body=Line"),
            ("mailto:a@example.com?Subject=Hi there&CC=b@example.com", "mailto:a@example.com?subject=Hi%20there&cc=b@example.com"),
            ("mailto:?to=a@example.com&bcc=b@example.com", "mailto:?to=a@example.com&bcc=b@example.com"),
            ("mailto:a@example.com?attach=~/.ssh/id_rsa", "mailto:a@example.com"),
            ("mailto:a@example.com?subject=x&attachment=/etc/passwd&body=y", "mailto:a@example.com?subject=x&body=y"),
            ("mailto:a@example.com?%61ttach=/etc/passwd", "mailto:a@example.com"),
            ("mailto:a@example.com#frag", "mailto:a@example.com"),
        ] {
            assert_eq!(followable_link(link).as_deref(), Some(want), "{link}");
        }
        for bad in ["mailto:", "mailto:?attach=/etc/passwd", "mailto:a@example.com\n?attach=x"] {
            assert_eq!(followable_link(bad), None, "{bad:?}");
        }
    }

    /// Hostile input never panics, and whatever is followed is web or mail.
    #[test]
    fn followable_link_never_panics_on_junk() {
        let junk = [
            "http://[",
            "http://[]",
            "http://[:]",
            "http://[::]",
            "http://:",
            "http://.",
            "http://a.",
            "mailto:&&&==",
            "https://\u{1F600}/",
            "http://[::ffff:1.2.3.4]",
        ];
        for j in junk {
            if let Some(out) = followable_link(j) {
                assert!(out.starts_with("http://") || out.starts_with("https://") || out.starts_with("mailto:"), "{j:?} -> {out}");
            }
        }
    }

    #[test]
    fn only_web_and_mail_links_are_followable() {
        for ok in ["https://example.com/a?b#c", "http://example.com", "HTTPS://EXAMPLE.COM", "mailto:someone@example.com"] {
            assert!(is_web_or_mail_link(ok), "{ok}");
        }
        for bad in [
            "file:///etc/passwd",
            "C:\\Windows\\System32\\calc.exe",
            "/usr/bin/xterm",
            "javascript:alert(1)",
            "ms-msdt:/id",
            " https://example.com",
            "https\u{0}://example.com",
            "",
            "example.com",
            "#top",
        ] {
            assert!(!is_web_or_mail_link(bad), "{bad:?}");
        }
    }

    /// An exported `href` may be a web/mail link, a bookmark or a relative path: never a script
    /// or another scheme, however a browser would normalise it.
    #[test]
    fn exported_hrefs_allow_bookmarks_and_relative_paths_but_no_scripts() {
        for ok in ["https://example.com/x", "mailto:a@b.c", "#top", "other.html", "docs/guide.html#s2", "../up.html", "?q=1"] {
            assert!(is_safe_href(ok), "{ok}");
        }
        for bad in [
            "javascript:alert(1)",
            "JavaScript:alert(1)",
            "java\nscript:alert(1)",
            "\tjavascript:alert(1)",
            " javascript:alert(1)",
            "java\u{0}script:alert(1)",
            "vbscript:MsgBox(1)",
            "data:text/html,<script>alert(1)</script>",
            "file:///etc/passwd",
            "ms-msdt:/id",
            // Protocol-relative and UNC references name another machine (NTLM leak on Windows).
            "//attacker.example/share/x",
            "\\\\attacker.example\\share\\x",
            "/\\attacker.example\\x",
            " //attacker.example/x",
            "",
        ] {
            assert!(!is_safe_href(bad), "{bad:?}");
        }
    }
}
