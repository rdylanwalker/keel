//! YAML emission and text-level helpers for multi-document YAML streams: serializing values
//! safely for YAML 1.1 consumers, joining documents with `---` separators, splitting a stream
//! into its document texts, and verbatim per-document edits.

use noyalib::{Mapping, Value};
use serde::Serialize;

/// Serializes `value` to YAML with `noyalib`, additionally double-quoting every string scalar
/// (value or mapping key) that a YAML 1.1 reader would resolve to a non-string.
///
/// `noyalib` quotes strings by YAML 1.2 rules, so it writes e.g. `1_000`, `0x1_F`, `2024-01-01`,
/// as plain scalars. The Go YAML stacks that consume Keel's Git output (go-yaml v2/v3
/// behind Helm, `sigs.k8s.io/yaml`, kustomize, and SOPS) resolve those as integers, floats,
/// or timestamps, which would silently change Helm value types or make the API server
/// reject Secret `stringData`. Every Keel YAML writer goes through this function.
pub fn to_string<T: ?Sized + Serialize>(value: &T) -> Result<String, noyalib::Error> {
    let mut tree = noyalib::to_value(value)?;
    let mut ambiguous = false;
    visit_strings(&mut tree, &mut |s| {
        ambiguous |= is_yaml11_non_string(s);
        None
    });
    if !ambiguous {
        return noyalib::to_string(&tree);
    }

    // Swap each ambiguous string for a unique plain-scalar placeholder, emit, then substitute the
    // placeholders with double-quoted scalars. The nonce keeps placeholders from colliding with
    // any text already present in the document.
    let plain = noyalib::to_string(&tree)?;
    let mut nonce = 0usize;
    while plain.contains(&format!("__keel_yaml11_{nonce}_")) {
        nonce += 1;
    }
    let mut quoted = Vec::new();
    visit_strings(&mut tree, &mut |s| {
        if !is_yaml11_non_string(s) {
            return None;
        }
        let placeholder = format!("__keel_yaml11_{nonce}_{}__", quoted.len());
        // A JSON string literal is a valid YAML double-quoted scalar.
        quoted.push((placeholder.clone(), serde_json::Value::from(s).to_string()));
        Some(placeholder)
    });
    let mut out = noyalib::to_string(&tree)?;
    for (placeholder, literal) in quoted {
        out = out.replace(&placeholder, &literal);
    }
    Ok(out)
}

/// Calls `f` on every string scalar and mapping key in `value`, replacing it when `f` returns
/// `Some`.
fn visit_strings(value: &mut Value, f: &mut impl FnMut(&str) -> Option<String>) {
    match value {
        Value::String(s) => {
            if let Some(replacement) = f(s) {
                *s = replacement;
            }
        }
        Value::Sequence(items) => items.iter_mut().for_each(|item| visit_strings(item, f)),
        Value::Mapping(map) => {
            let entries = std::mem::take(map);
            *map = entries
                .into_iter()
                .map(|(key, mut item)| {
                    let key = f(&key).unwrap_or(key);
                    visit_strings(&mut item, f);
                    (key, item)
                })
                .collect::<Mapping>();
        }
        Value::Tagged(tagged) => visit_strings(tagged.value_mut(), f),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// Whether go-yaml (v2/v3) would resolve the plain scalar `s` to something other than a string
/// while `noyalib` keeps it plain: YAML 1.1 booleans, integers and floats written with `_` digit
/// separators, and YAML 1.1 timestamps.
/// Forms `noyalib` already quotes (YAML 1.2 numbers, `true`, `null`, ...) may also match; quoting
/// them again is harmless.
fn is_yaml11_non_string(s: &str) -> bool {
    if is_yaml11_bool(s) {
        return true;
    }
    if s.contains('_') {
        // go-yaml strips every `_` before trying integer and float parsing.
        let stripped = s.replace('_', "");
        if is_go_int(&stripped) || is_go_yaml_float(&stripped) {
            return true;
        }
    }
    is_yaml11_timestamp(s)
}

/// The exact YAML 1.1 boolean plain scalars (go-yaml v2's resolve table, which
/// `sigs.k8s.io/yaml` and therefore Helm, kustomize and kubectl use): `y`/`yes`/`true`/`on` and
/// `n`/`no`/`false`/`off`, each only in lower, Capitalized or UPPER case. Mixed case (`yEs`) and
/// longer words (`onward`) are strings in YAML 1.1 too. This applies to mapping keys as much as to
/// values: go-yaml v2 reads the key `on:` as the boolean `true`, which `sigs.k8s.io/yaml` then
/// turns into the JSON key `"true"`.
fn is_yaml11_bool(s: &str) -> bool {
    const WORDS: [&str; 8] = ["y", "yes", "true", "on", "n", "no", "false", "off"];
    let lower = s.to_ascii_lowercase();
    WORDS.contains(&lower.as_str()) && {
        let mut capitalized = lower.clone();
        capitalized[..1].make_ascii_uppercase();
        s == lower || s == capitalized || s == s.to_ascii_uppercase()
    }
}

/// `strconv.ParseInt(s, 0, 64)` syntax: optional sign, then decimal digits or a
/// `0x`/`0o`/`0b`-prefixed (any case) or legacy `0`-prefixed octal number.
fn is_go_int(s: &str) -> bool {
    let unsigned = s.strip_prefix(['+', '-']).unwrap_or(s);
    let lower = unsigned.to_ascii_lowercase();
    let (digits, radix) = if let Some(d) = lower.strip_prefix("0x") {
        (d, 16)
    } else if let Some(d) = lower.strip_prefix("0o") {
        (d, 8)
    } else if let Some(d) = lower.strip_prefix("0b") {
        (d, 2)
    } else {
        (lower.as_str(), 10)
    };
    !digits.is_empty() && digits.chars().all(|c| c.is_digit(radix))
}

/// go-yaml's float pattern: `^[-+]?(\.[0-9]+|[0-9]+(\.[0-9]*)?)([eE][-+]?[0-9]+)?$`.
fn is_go_yaml_float(s: &str) -> bool {
    let rest = s.strip_prefix(['+', '-']).unwrap_or(s);
    let (mantissa, exponent) = match rest.find(['e', 'E']) {
        Some(i) => (&rest[..i], Some(&rest[i + 1..])),
        None => (rest, None),
    };
    let all_digits = |t: &str| t.bytes().all(|b| b.is_ascii_digit());
    let mantissa_ok = match mantissa.split_once('.') {
        Some(("", frac)) => !frac.is_empty() && all_digits(frac),
        Some((int, frac)) => !int.is_empty() && all_digits(int) && all_digits(frac),
        None => !mantissa.is_empty() && all_digits(mantissa),
    };
    let exponent_ok = exponent.is_none_or(|e| {
        let e = e.strip_prefix(['+', '-']).unwrap_or(e);
        !e.is_empty() && all_digits(e)
    });
    mantissa_ok && exponent_ok
}

/// A YAML 1.1 timestamp prefix: `YYYY-M-D`, alone or followed by a `T`/`t`/space time part.
fn is_yaml11_timestamp(s: &str) -> bool {
    let bytes = s.as_bytes();
    let digits = |from: usize, min: usize, max: usize| -> Option<usize> {
        let n = bytes[from.min(bytes.len())..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count();
        (min..=max).contains(&n).then_some(from + n)
    };
    let Some(i) = digits(0, 4, 4) else {
        return false;
    };
    if bytes.get(i) != Some(&b'-') {
        return false;
    }
    let Some(i) = digits(i + 1, 1, 2) else {
        return false;
    };
    if bytes.get(i) != Some(&b'-') {
        return false;
    }
    let Some(i) = digits(i + 1, 1, 2) else {
        return false;
    };
    matches!(bytes.get(i), None | Some(b'T' | b't' | b' ' | b'\t'))
}

/// Serializes `docs` into one multi-document YAML stream: every document is newline-terminated
/// and documents are joined by `---` separator lines (no leading or trailing separator).
pub fn to_yaml_documents<T: Serialize>(
    docs: impl IntoIterator<Item = T>,
) -> Result<String, noyalib::Error> {
    let mut out = String::new();
    for (i, doc) in docs.into_iter().enumerate() {
        if i > 0 {
            out.push_str("---\n");
        }
        out.push_str(&to_string(&doc)?);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    Ok(out)
}

/// Splits single or multi-document YAML on lines matching `---` or `...`,
/// returning all non-empty document strings.
pub fn split_yaml_documents(raw_yaml: &str) -> Vec<String> {
    let mut raw_docs = Vec::new();
    let mut current = String::new();
    for line in raw_yaml.lines() {
        if is_yaml_document_separator(line) {
            if !current.trim().is_empty() {
                raw_docs.push(std::mem::take(&mut current));
            }
        } else {
            current.push_str(line);
            current.push('\n');
        }
    }
    if !current.trim().is_empty() {
        raw_docs.push(current);
    }
    raw_docs
}

fn is_yaml_document_separator(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed == "---" || trimmed == "..."
}

/// Replaces the `index`-th non-empty document (same numbering as [`split_yaml_documents`]) with
/// `replacement`, preserving every other document, separator line, and line ending verbatim.
/// Returns `None` when `index` is out of range.
pub fn replace_yaml_document(raw_yaml: &str, index: usize, replacement: &str) -> Option<String> {
    let mut out = String::with_capacity(raw_yaml.len() + replacement.len());
    let mut current = String::new();
    let mut doc_index = 0usize;
    let mut replaced = false;

    let mut flush = |current: &mut String, out: &mut String| {
        if current.trim().is_empty() {
            out.push_str(current);
        } else {
            if doc_index == index {
                out.push_str(replacement);
                if !replacement.is_empty() && !replacement.ends_with('\n') {
                    out.push('\n');
                }
                replaced = true;
            } else {
                out.push_str(current);
            }
            doc_index += 1;
        }
        current.clear();
    };

    for line in raw_yaml.split_inclusive('\n') {
        if is_yaml_document_separator(line) {
            flush(&mut current, &mut out);
            out.push_str(line);
        } else {
            current.push_str(line);
        }
    }
    flush(&mut current, &mut out);
    replaced.then_some(out)
}

/// Returns the leading block of blank and `#` comment lines of a YAML document (verbatim), so
/// callers that re-serialize a document can keep its header comments.
pub fn yaml_leading_comments(doc: &str) -> &str {
    let mut end = 0usize;
    for line in doc.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            end += line.len();
        } else {
            break;
        }
    }
    &doc[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn to_yaml_documents_joins_newline_terminated_documents_with_separators() {
        let out = to_yaml_documents([json!({"kind": "A"}), json!({"kind": "B", "m": 1})]).unwrap();
        assert_eq!(out, "kind: A\n---\nkind: B\nm: 1\n");
        assert_eq!(split_yaml_documents(&out).len(), 2);
        assert_eq!(
            to_yaml_documents(Vec::<serde_json::Value>::new()).unwrap(),
            ""
        );
        assert_eq!(to_yaml_documents([json!({"a": 1})]).unwrap(), "a: 1\n");
    }

    #[test]
    fn to_string_quotes_strings_yaml11_readers_resolve_as_non_strings() {
        for s in [
            "1_000",
            "0_17",
            "+1_0",
            "-1_0",
            "0x1_F",
            "0b1_0",
            "1_0.5",
            "1_0e3",
            "2024-01-01",
            "2024-1-1",
            "2001-12-14t21:59:43.10-05:00",
            "2001-12-14T21:59:43Z",
            "2001-12-14 21:59:43",
            // YAML 1.1 booleans (go-yaml v2 / sigs.k8s.io/yaml), every accepted casing.
            "y",
            "Y",
            "yes",
            "Yes",
            "YES",
            "n",
            "N",
            "no",
            "No",
            "NO",
            "on",
            "On",
            "ON",
            "off",
            "Off",
            "OFF",
            "True",
            "FALSE",
        ] {
            let out = to_string(&json!({ "k": s, s: "v", "list": [s] })).unwrap();
            let quoted = serde_json::Value::from(s).to_string();
            let mut lines: Vec<&str> = out.lines().collect();
            lines.sort_unstable();
            let mut expected = [
                format!("{quoted}: v"),
                format!("k: {quoted}"),
                "list:".to_string(),
                format!("  - {quoted}"),
            ];
            expected.sort_unstable();
            assert_eq!(lines, expected, "{s}");
            let back: serde_json::Value = noyalib::from_str(&out).unwrap();
            assert_eq!(back, json!({ "k": s, s: "v", "list": [s] }), "{s}");
        }
        // Strings no YAML reader takes as a non-string stay plain.
        for s in [
            "yEs",
            "oN",
            "nO",
            "oFF",
            "yes!",
            "nope",
            "ok",
            "1.2.3",
            "v1_000",
            "a_b",
            "2024-01",
            "20240101x",
            "onward",
            "_",
        ] {
            assert_eq!(to_string(&json!({ "k": s })).unwrap(), format!("k: {s}"));
        }
        // Existing placeholder-like text cannot collide with the substitution.
        let out = to_string(&json!({"a": "__keel_yaml11_0_0__", "b": "1_000"})).unwrap();
        assert_eq!(out, "a: __keel_yaml11_0_0__\nb: \"1_000\"");
    }

    /// Real booleans stay YAML booleans; only strings that read as booleans are quoted, whether
    /// they are values or keys (a Helm values key `on` would otherwise become `"true"`).
    #[test]
    fn to_string_quotes_yaml11_boolean_lookalike_strings_only() {
        let value = json!({"enabled": true, "country": "NO", "toggle": "off", "on": {"n": false}});
        let out = to_string(&value).unwrap();
        assert_eq!(
            out,
            "country: \"NO\"\nenabled: true\n\"on\":\n  \"n\": false\ntoggle: \"off\""
        );
        let back: serde_json::Value = noyalib::from_str(&out).unwrap();
        assert_eq!(back, value);
    }

    #[test]
    fn to_string_serializes_integers_above_i64_max() {
        let out = to_string(&json!({ "max": u64::MAX })).unwrap();
        assert_eq!(out, format!("max: {}", u64::MAX));
        assert_eq!(
            to_yaml_documents([json!({ "max": u64::MAX })]).unwrap(),
            format!("max: {}\n", u64::MAX)
        );
    }

    #[test]
    fn replace_yaml_document_preserves_other_documents_verbatim() {
        let raw =
            "# header\napiVersion: v1\nkind: A # keep\n---\n\n# b comment\nkind: B\n---\nkind: C\n";
        assert_eq!(split_yaml_documents(raw).len(), 3);
        let out = replace_yaml_document(raw, 1, "kind: B2").unwrap();
        assert_eq!(
            out,
            "# header\napiVersion: v1\nkind: A # keep\n---\nkind: B2\n---\nkind: C\n"
        );
        let out = replace_yaml_document(raw, 2, "kind: C2\n").unwrap();
        assert!(out.starts_with(
            "# header\napiVersion: v1\nkind: A # keep\n---\n\n# b comment\nkind: B\n---\n"
        ));
        assert!(out.ends_with("kind: C2\n"));
        assert!(replace_yaml_document(raw, 3, "x").is_none());
        // Leading separator and CRLF line endings are kept.
        let crlf = "---\r\nkind: A\r\n---\r\nkind: B\r\n";
        assert_eq!(
            replace_yaml_document(crlf, 0, "kind: Z\n").unwrap(),
            "---\r\nkind: Z\n---\r\nkind: B\r\n"
        );
    }

    #[test]
    fn yaml_leading_comments_returns_header_block() {
        assert_eq!(
            yaml_leading_comments("\n# a\n  # b\napiVersion: v1\n# c\n"),
            "\n# a\n  # b\n"
        );
        assert_eq!(yaml_leading_comments("kind: A\n"), "");
    }
}
