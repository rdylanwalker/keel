//! One JSON Schema walker that decides which catalog values are secret (`writeOnly: true` /
//! `format: "password"`, `NFR-SEC-05`, `SDD.md` §6.3).
//!
//! Partitioning ([`crate::partition_values_by_schema`]), validation-error redaction
//! ([`crate::validate_schema_values`]), strict-mode checks ([`crate::undeclared_value_paths`]),
//! server-side defaults and the stored-secret edit flow all use this [`SchemaResolver`], so they
//! cannot disagree about whether a field is secret.
//!
//! # Supported keywords
//!
//! A field is secret if **any** schema that can apply to it carries a secret marker. The walker
//! follows:
//!
//! * in place (same instance): local `$ref` (`#`, `#/json/pointer` into `$defs`/`definitions`/
//!   anywhere, `#anchor` via `$anchor`, or the root `$id` followed by one of those),
//!   `allOf`/`anyOf`/`oneOf` (every branch), `if`/`then`/`else` (every branch),
//!   `dependentSchemas` and schema-valued `dependencies` (always, whether or not the trigger key
//!   is present);
//! * object members: `properties`, `patternProperties` (ECMA-ish regex via `fancy-regex`; a
//!   pattern that does not compile matches every key) and `additionalProperties` (keys matched by
//!   neither of the former in the same schema);
//! * array items: `prefixItems`, `items` (schema or draft-7 tuple array) and `additionalItems`.
//!
//! `$ref` cycles are guarded. Anything else (`not`, `contains`, `propertyNames`,
//! `unevaluated*`, unknown keywords, remote or relative `$ref`, `$dynamicRef`, `$recursiveRef`)
//! is unsupported: [`validate_secret_schema_support`] rejects a catalog schema with a secret
//! marker the walker cannot reach, or with a reference the walker cannot resolve (fail closed).

use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde_json::{Map, Value};

use crate::CoreError;

/// Combinators whose every branch applies to the same instance.
const COMBINATORS: [&str; 3] = ["allOf", "anyOf", "oneOf"];
/// Conditional keywords whose every branch applies to the same instance.
const CONDITIONALS: [&str; 3] = ["if", "then", "else"];
/// Keywords holding instance data, never schemas: skipped when scanning for secret markers.
const DATA_KEYWORDS: [&str; 5] = ["const", "enum", "default", "examples", "example"];
/// Keywords holding a map of name -> schema.
const SCHEMA_MAP_KEYWORDS: [&str; 6] = [
    "properties",
    "patternProperties",
    "$defs",
    "definitions",
    "dependentSchemas",
    "dependencies",
];
/// Keywords the walker supports, listed in fail-closed errors.
const SUPPORTED_KEYWORDS: &str = "properties, patternProperties, additionalProperties, items, \
    prefixItems, additionalItems, allOf/anyOf/oneOf, if/then/else, dependentSchemas and local $ref";

/// Whether `node` itself (no `$ref` or combinator resolution) carries a secret marker.
fn has_secret_marker(node: &Value) -> bool {
    node.get("writeOnly").and_then(Value::as_bool) == Some(true)
        || node.get("format").and_then(Value::as_str) == Some("password")
}

fn ptr(node: &Value) -> *const Value {
    node as *const Value
}

/// Escapes one JSON Pointer reference token (RFC 6901).
pub(crate) fn escape_pointer_token(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

fn join_dot(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_string()
    } else {
        format!("{prefix}.{key}")
    }
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Whether the `patternProperties` regex `pattern` matches `key`. A pattern that does not compile
/// (or exceeds the backtrack limit) counts as a match: for secrets that over-approximates.
fn pattern_matches(pattern: &str, key: &str) -> bool {
    fancy_regex::Regex::new(pattern)
        .ok()
        .and_then(|re| re.is_match(key).ok())
        .unwrap_or(true)
}

/// A concrete secret value inside submitted values.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SecretValueLocation {
    /// JSON Pointer of the value (`/gitea/admin/password`, `/tokens/0`; `""` for the root).
    pub pointer: String,
    /// Dot path in the [`crate::PartitionedValues::secret_field_paths`] form (`gitea.admin.password`,
    /// list items as `tokens[]`; `""` for the root).
    pub path: String,
}

/// Local-`$ref`-aware view of a catalog JSON Schema (see the module docs for the keywords).
///
/// A "schema set" (`&[&Value]`) is every schema node that applies at one instance location; a
/// location is secret when any of them (after in-place expansion) carries a secret marker.
#[derive(Debug, Clone, Copy)]
pub struct SchemaResolver<'a> {
    root: &'a Value,
}

impl<'a> SchemaResolver<'a> {
    /// A resolver for the schema document `root` (local `$ref`s resolve against it).
    pub fn new(root: &'a Value) -> Self {
        Self { root }
    }

    /// The schema document.
    pub fn root(&self) -> &'a Value {
        self.root
    }

    fn root_id(&self) -> Option<&'a str> {
        self.root
            .get("$id")
            .or_else(|| self.root.get("id"))
            .and_then(Value::as_str)
            .map(|id| id.trim_end_matches('#'))
            .filter(|id| !id.is_empty())
    }

    /// The local fragment (`#...`) of `reference`, if it targets this document.
    fn local_fragment<'r>(&self, reference: &'r str) -> Option<&'r str> {
        if reference.starts_with('#') {
            return Some(reference);
        }
        let id = self.root_id()?;
        let rest = reference.strip_prefix(id)?;
        if rest.is_empty() {
            Some("#")
        } else {
            rest.starts_with('#').then_some(rest)
        }
    }

    /// Resolves a local `$ref` (`#`, `#/pointer`, `#anchor`, or the root `$id` plus one of those).
    /// Remote, relative-URL and unresolvable references give `None`.
    pub fn resolve_ref(&self, reference: &str) -> Option<&'a Value> {
        let fragment = self.local_fragment(reference)?;
        let fragment = percent_decode(&fragment[1..])?;
        if fragment.is_empty() {
            return Some(self.root);
        }
        if fragment.starts_with('/') {
            return self.root.pointer(&fragment);
        }
        find_anchor(self.root, &fragment)
    }

    /// Every schema object that applies to the same instance as one of `set`: the nodes
    /// themselves plus, recursively, their `$ref` target, `allOf`/`anyOf`/`oneOf` branches,
    /// `if`/`then`/`else` and `dependentSchemas`/`dependencies` schemas. Deduplicated and
    /// cycle-safe; non-object (boolean) schemas are dropped.
    pub fn expand(&self, set: &[&'a Value]) -> Vec<&'a Value> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut stack: Vec<&'a Value> = set.iter().rev().copied().collect();
        while let Some(node) = stack.pop() {
            let Some(obj) = node.as_object() else {
                continue;
            };
            if !seen.insert(ptr(node)) {
                continue;
            }
            out.push(node);
            let mut next = Vec::new();
            if let Some(target) = obj
                .get("$ref")
                .and_then(Value::as_str)
                .and_then(|r| self.resolve_ref(r))
            {
                next.push(target);
            }
            for combinator in COMBINATORS {
                if let Some(branches) = obj.get(combinator).and_then(Value::as_array) {
                    next.extend(branches.iter());
                }
            }
            for conditional in CONDITIONALS {
                if let Some(branch) = obj.get(conditional) {
                    next.push(branch);
                }
            }
            for keyword in ["dependentSchemas", "dependencies"] {
                if let Some(map) = obj.get(keyword).and_then(Value::as_object) {
                    next.extend(map.values().filter(|v| v.is_object()));
                }
            }
            stack.extend(next.into_iter().rev());
        }
        out
    }

    /// Whether the instance location described by `set` is secret.
    pub fn is_secret(&self, set: &[&'a Value]) -> bool {
        self.expand(set).into_iter().any(has_secret_marker)
    }

    /// Whether `node` is secret, following `$ref` (against this document) and every branch.
    /// `node` may be a copy of a node of the document.
    pub fn is_secret_node<'b>(&self, node: &'b Value) -> bool
    where
        'a: 'b,
    {
        let resolver: SchemaResolver<'b> = *self;
        resolver.is_secret(&[node])
    }

    /// Whether the catalog author explicitly declared the location not secret (`writeOnly:
    /// false` in an applying schema) and no applying schema marks it secret. Used to opt a field
    /// out of name-based secret heuristics; a declared secret always wins.
    pub fn is_declared_not_secret(&self, set: &[&'a Value]) -> bool {
        let expanded = self.expand(set);
        !expanded.iter().copied().any(has_secret_marker)
            && expanded
                .iter()
                .any(|n| n.get("writeOnly").and_then(Value::as_bool) == Some(false))
    }

    /// The schemas that apply to member `key` of an object validated by `set`: `properties[key]`,
    /// every matching `patternProperties` entry and, for a key neither matches in that schema,
    /// an object `additionalProperties`.
    pub fn property_schemas(&self, set: &[&'a Value], key: &str) -> Vec<&'a Value> {
        let mut out = Vec::new();
        for node in self.expand(set) {
            let Some(obj) = node.as_object() else {
                continue;
            };
            let mut matched = false;
            if let Some(prop) = obj
                .get("properties")
                .and_then(Value::as_object)
                .and_then(|p| p.get(key))
            {
                out.push(prop);
                matched = true;
            }
            if let Some(patterns) = obj.get("patternProperties").and_then(Value::as_object) {
                for (pattern, sub) in patterns {
                    if pattern_matches(pattern, key) {
                        out.push(sub);
                        matched = true;
                    }
                }
            }
            if !matched
                && let Some(additional) = obj.get("additionalProperties").filter(|v| v.is_object())
            {
                out.push(additional);
            }
        }
        out
    }

    /// The schemas that apply to item `index` of an array validated by `set` (`None`: any item):
    /// `prefixItems`, `items` (schema, or a draft-7 tuple array) and `additionalItems`.
    pub fn item_schemas(&self, set: &[&'a Value], index: Option<usize>) -> Vec<&'a Value> {
        let mut out = Vec::new();
        for node in self.expand(set) {
            let Some(obj) = node.as_object() else {
                continue;
            };
            let prefix = obj.get("prefixItems").and_then(Value::as_array);
            let items = obj.get("items");
            let tuple = items.and_then(Value::as_array);
            let additional = obj.get("additionalItems").filter(|v| v.is_object());
            match index {
                Some(i) => {
                    if let Some(p) = prefix.filter(|p| i < p.len()) {
                        out.push(&p[i]);
                    } else if let Some(t) = tuple {
                        match t.get(i) {
                            Some(item) => out.push(item),
                            None => out.extend(additional),
                        }
                    } else if let Some(items) = items {
                        out.push(items);
                    }
                }
                None => {
                    out.extend(prefix.into_iter().flatten());
                    match tuple {
                        Some(t) => out.extend(t.iter()),
                        None => out.extend(items),
                    }
                    out.extend(additional);
                }
            }
        }
        out.retain(|v| v.is_object());
        out
    }

    /// Whether the dot path `path` (`a.b`, list items as `list[]`) lies inside a secret field
    /// (the field itself or an enclosing one is secret). Keys containing `.` or `[` cannot be
    /// addressed. Used to keep stored secrets of fields without a static name
    /// (`patternProperties`/`additionalProperties`) on edits.
    pub fn is_secret_path(&self, path: &str) -> bool {
        let mut set = vec![self.root];
        if self.is_secret(&set) {
            return true;
        }
        for segment in path.split('.').filter(|s| !s.is_empty()) {
            let (name, mut brackets) = match segment.find('[') {
                Some(i) => (&segment[..i], &segment[i..]),
                None => (segment, ""),
            };
            if !name.is_empty() {
                set = self.property_schemas(&set, name);
                if self.is_secret(&set) {
                    return true;
                }
            }
            while let Some(rest) = brackets.strip_prefix("[]") {
                set = self.item_schemas(&set, None);
                if self.is_secret(&set) {
                    return true;
                }
                brackets = rest;
            }
            if set.is_empty() {
                return false;
            }
        }
        false
    }

    /// The schemas that apply at the dot path `path` (`a.b`, list items as `list[]`; `""` is the
    /// root). Empty when the path leaves the schema.
    pub fn schemas_at_path(&self, path: &str) -> Vec<&'a Value> {
        let mut set = vec![self.root];
        for segment in path.split('.').filter(|s| !s.is_empty()) {
            let (name, mut brackets) = match segment.find('[') {
                Some(i) => (&segment[..i], &segment[i..]),
                None => (segment, ""),
            };
            if !name.is_empty() {
                set = self.property_schemas(&set, name);
            }
            while let Some(rest) = brackets.strip_prefix("[]") {
                set = self.item_schemas(&set, None);
                brackets = rest;
            }
            if set.is_empty() {
                break;
            }
        }
        set
    }

    /// Sorted dot paths of every secret field the schema names statically (`a.b`, list items as
    /// `list[]`). Fields under `patternProperties`/`additionalProperties` have no static name:
    /// see [`Self::secret_value_locations`]. Recursion through `$ref` cycles stops at the first
    /// repeated schema.
    pub fn declared_secret_paths(&self) -> Vec<String> {
        let mut out = BTreeSet::new();
        let mut stack = Vec::new();
        self.collect_declared(vec![self.root], "", &mut stack, &mut out);
        out.into_iter().collect()
    }

    fn collect_declared(
        &self,
        set: Vec<&'a Value>,
        prefix: &str,
        stack: &mut Vec<*const Value>,
        out: &mut BTreeSet<String>,
    ) {
        let expanded = self.expand(&set);
        if !prefix.is_empty() && expanded.iter().copied().any(has_secret_marker) {
            out.insert(prefix.to_string());
            return;
        }
        let fresh: Vec<&'a Value> = expanded
            .into_iter()
            .filter(|n| !stack.contains(&ptr(n)))
            .collect();
        if fresh.is_empty() {
            return;
        }
        let depth = stack.len();
        stack.extend(fresh.iter().map(|n| ptr(n)));
        let mut props: BTreeMap<&'a str, Vec<&'a Value>> = BTreeMap::new();
        for node in &fresh {
            if let Some(map) = node.get("properties").and_then(Value::as_object) {
                for (key, sub) in map {
                    props.entry(key.as_str()).or_default().push(sub);
                }
            }
        }
        let items = self.item_schemas(&fresh, None);
        for (key, subs) in props {
            self.collect_declared(subs, &join_dot(prefix, key), stack, out);
        }
        if !items.is_empty() {
            self.collect_declared(items, &format!("{prefix}[]"), stack, out);
        }
        stack.truncate(depth);
    }

    /// Every secret value present in `values`, outermost first (a secret object's children are
    /// not listed separately). Includes the root (`""`) when the root schema is secret.
    pub fn secret_value_locations(&self, values: &Value) -> Vec<SecretValueLocation> {
        let mut out = Vec::new();
        self.collect_values(vec![self.root], values, "", "", &mut out);
        out.sort();
        out
    }

    fn collect_values(
        &self,
        set: Vec<&'a Value>,
        value: &Value,
        pointer: &str,
        path: &str,
        out: &mut Vec<SecretValueLocation>,
    ) {
        if self.is_secret(&set) {
            out.push(SecretValueLocation {
                pointer: pointer.to_string(),
                path: path.to_string(),
            });
            return;
        }
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    let child_set = self.property_schemas(&set, key);
                    self.collect_values(
                        child_set,
                        child,
                        &format!("{pointer}/{}", escape_pointer_token(key)),
                        &join_dot(path, key),
                        out,
                    );
                }
            }
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    let child_set = self.item_schemas(&set, Some(i));
                    self.collect_values(
                        child_set,
                        item,
                        &format!("{pointer}/{i}"),
                        &format!("{path}[]"),
                        out,
                    );
                }
            }
            _ => {}
        }
    }

    /// Splits `value` (validated by `set`) into its non-secret and secret parts.
    pub(crate) fn split(&self, set: &[&'a Value], value: &Value) -> (Option<Value>, Option<Value>) {
        if self.is_secret(set) {
            return (None, Some(value.clone()));
        }
        match value {
            Value::Object(map) => {
                if map.is_empty() {
                    return (Some(Value::Object(Map::new())), None);
                }
                let mut normal_map = Map::new();
                let mut secret_map = Map::new();
                for (k, v) in map {
                    let child = self.property_schemas(set, k);
                    let (n_part, s_part) = self.split(&child, v);
                    if let Some(nv) = n_part {
                        normal_map.insert(k.clone(), nv);
                    }
                    if let Some(sv) = s_part {
                        secret_map.insert(k.clone(), sv);
                    }
                }
                (
                    (!normal_map.is_empty()).then_some(Value::Object(normal_map)),
                    (!secret_map.is_empty()).then_some(Value::Object(secret_map)),
                )
            }
            Value::Array(items) => {
                let mut normal_arr = Vec::new();
                let mut secret_arr = Vec::new();
                for (i, item) in items.iter().enumerate() {
                    let child = self.item_schemas(set, Some(i));
                    let (n_part, s_part) = self.split(&child, item);
                    normal_arr.extend(n_part);
                    secret_arr.extend(s_part);
                }
                let any_secret = !secret_arr.is_empty();
                (
                    (!(normal_arr.is_empty() && any_secret)).then_some(Value::Array(normal_arr)),
                    any_secret.then_some(Value::Array(secret_arr)),
                )
            }
            _ => (Some(value.clone()), None),
        }
    }
}

fn find_anchor<'a>(node: &'a Value, anchor: &str) -> Option<&'a Value> {
    match node {
        Value::Object(obj) => {
            if obj.get("$anchor").and_then(Value::as_str) == Some(anchor) {
                return Some(node);
            }
            obj.iter()
                .filter(|(k, _)| !DATA_KEYWORDS.contains(&k.as_str()))
                .find_map(|(_, v)| find_anchor(v, anchor))
        }
        Value::Array(items) => items.iter().find_map(|v| find_anchor(v, anchor)),
        _ => None,
    }
}

/// Schema keywords the walker follows from a schema to its subschemas (the edges of
/// [`SchemaResolver::expand`], [`SchemaResolver::property_schemas`] and
/// [`SchemaResolver::item_schemas`]); `$ref` is handled on its own.
const FOLLOWED_KEYWORDS: [&str; 15] = [
    "properties",
    "patternProperties",
    "additionalProperties",
    "items",
    "prefixItems",
    "additionalItems",
    "allOf",
    "anyOf",
    "oneOf",
    "if",
    "then",
    "else",
    "dependentSchemas",
    "dependencies",
    "$ref",
];

/// Schema-aware scan of a document: secret markers and references, visiting `$defs`/
/// `definitions` only through `$ref`s (an unused definition applies to nothing).
///
/// The scan is edge-aware: it remembers whether the path from the root to a schema went only
/// through keywords the walker follows ([`FOLLOWED_KEYWORDS`]). A secret marker reached through
/// any other keyword (`not`, `contains`, `propertyNames`, `unevaluated*`, unknown keywords) is a
/// problem even when the same schema is also reachable through a followed keyword: a `$defs`
/// entry `$ref`'d from both `properties.a` and `unevaluatedProperties` applies its secret to
/// values the walker never classifies.
struct MarkerScan<'a> {
    resolver: SchemaResolver<'a>,
    /// `(schema, reached through an unfollowed keyword, inside an embedded resource)`.
    visited: HashSet<(*const Value, bool, bool)>,
    problems: BTreeSet<String>,
}

impl<'a> MarkerScan<'a> {
    /// Scans `node` at `pointer`. `unfollowed` is the location of the first keyword on the path
    /// from the root the walker does not follow (`None`: every edge is followed).
    fn scan_schema(
        &mut self,
        node: &'a Value,
        pointer: &str,
        unfollowed: Option<&str>,
        inside_resource: bool,
    ) {
        let Some(obj) = node.as_object() else {
            return;
        };
        if !self
            .visited
            .insert((ptr(node), unfollowed.is_some(), inside_resource))
        {
            return;
        }
        if has_secret_marker(node) {
            let marker = display_pointer(pointer);
            match unfollowed {
                Some(at) if at == marker => {
                    self.problems.insert(format!(
                        "secret marker (writeOnly/format: password) at '{marker}' sits under a \
                         keyword Keel does not follow"
                    ));
                }
                Some(at) => {
                    self.problems.insert(format!(
                        "secret marker (writeOnly/format: password) at '{marker}' applies \
                         through '{at}', a keyword Keel does not follow"
                    ));
                }
                None => {}
            }
        }
        let is_root = ptr(node) == ptr(self.resolver.root());
        let embedded_resource = !is_root && obj.get("$id").is_some_and(Value::is_string);
        let inside_resource = inside_resource || embedded_resource;
        for (keyword, child) in obj {
            let child_pointer = format!("{pointer}/{}", escape_pointer_token(keyword));
            let followed = FOLLOWED_KEYWORDS.contains(&keyword.as_str());
            let unfollowed_owned;
            let child_unfollowed = match unfollowed {
                Some(at) => Some(at),
                None if followed => None,
                None => {
                    unfollowed_owned = display_pointer(&child_pointer);
                    Some(unfollowed_owned.as_str())
                }
            };
            match keyword.as_str() {
                "$ref" => {
                    let Some(reference) = child.as_str() else {
                        continue;
                    };
                    if inside_resource {
                        self.problems.insert(format!(
                            "'$ref' at '{}' is inside an embedded schema resource ('$id'), \
                             which Keel cannot resolve",
                            display_pointer(pointer)
                        ));
                    } else if self.resolver.local_fragment(reference).is_none() {
                        self.problems.insert(format!(
                            "'$ref' '{reference}' at '{}' is not a local reference (remote and \
                             relative-URL references are not supported)",
                            display_pointer(pointer)
                        ));
                    } else {
                        match self.resolver.resolve_ref(reference) {
                            Some(target) => {
                                let target_pointer = reference
                                    .find('#')
                                    .map(|i| &reference[i + 1..])
                                    .and_then(percent_decode)
                                    .filter(|f| f.is_empty() || f.starts_with('/'))
                                    .unwrap_or_else(|| reference.to_string());
                                // The target applies where the reference does: it inherits
                                // whether the path so far was followed.
                                self.scan_schema(target, &target_pointer, unfollowed, false);
                            }
                            None => {
                                self.problems.insert(format!(
                                    "'$ref' '{reference}' at '{}' does not resolve inside the \
                                     schema",
                                    display_pointer(pointer)
                                ));
                            }
                        }
                    }
                }
                "$dynamicRef" | "$recursiveRef" => {
                    self.problems.insert(format!(
                        "'{keyword}' at '{}' is not supported",
                        display_pointer(pointer)
                    ));
                }
                "$defs" | "definitions" => {
                    // Only through a `$ref`, except for references the walker cannot follow.
                    self.scan_unreachable_refs(child, &child_pointer);
                }
                k if DATA_KEYWORDS.contains(&k) => {}
                k if SCHEMA_MAP_KEYWORDS.contains(&k) => {
                    if let Some(map) = child.as_object() {
                        for (name, sub) in map {
                            let sub_pointer =
                                format!("{child_pointer}/{}", escape_pointer_token(name));
                            self.scan_schema(sub, &sub_pointer, child_unfollowed, inside_resource);
                        }
                    }
                }
                _ => match child {
                    Value::Object(_) => {
                        self.scan_schema(child, &child_pointer, child_unfollowed, inside_resource)
                    }
                    Value::Array(items) => {
                        for (i, item) in items.iter().enumerate() {
                            self.scan_schema(
                                item,
                                &format!("{child_pointer}/{i}"),
                                child_unfollowed,
                                inside_resource,
                            );
                        }
                    }
                    _ => {}
                },
            }
        }
    }

    /// Unsupported reference keywords inside definitions are rejected even when unused: they
    /// would otherwise only surface once referenced.
    fn scan_unreachable_refs(&mut self, node: &'a Value, pointer: &str) {
        match node {
            Value::Object(obj) => {
                for (keyword, child) in obj {
                    let child_pointer = format!("{pointer}/{}", escape_pointer_token(keyword));
                    match keyword.as_str() {
                        "$ref" => {
                            if let Some(reference) = child.as_str()
                                && self.resolver.local_fragment(reference).is_none()
                            {
                                self.problems.insert(format!(
                                    "'$ref' '{reference}' at '{}' is not a local reference \
                                     (remote and relative-URL references are not supported)",
                                    display_pointer(pointer)
                                ));
                            }
                        }
                        "$dynamicRef" | "$recursiveRef" => {
                            self.problems.insert(format!(
                                "'{keyword}' at '{}' is not supported",
                                display_pointer(pointer)
                            ));
                        }
                        k if DATA_KEYWORDS.contains(&k) => {}
                        _ => self.scan_unreachable_refs(child, &child_pointer),
                    }
                }
            }
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    self.scan_unreachable_refs(item, &format!("{pointer}/{i}"));
                }
            }
            _ => {}
        }
    }
}

/// `#/json/pointer` for a scan location (anchor targets are already `#name`).
fn display_pointer(pointer: &str) -> String {
    if pointer.starts_with('#') {
        pointer.to_string()
    } else {
        format!("#{pointer}")
    }
}

/// Fails closed on a catalog `spec.schema` whose secrets Keel could not find
/// (`NFR-SEC-05`): every `writeOnly: true` / `format: "password"` marker that applies to a value
/// must be reachable by [`SchemaResolver`], and every `$ref` must resolve inside the document.
///
/// Rejects remote and relative-URL `$ref`s, `$ref`s inside embedded `$id` resources,
/// `$dynamicRef`/`$recursiveRef`, unresolvable local `$ref`s, and secret markers under keywords
/// the walker does not follow (`not`, `contains`, `propertyNames`, `unevaluated*`, unknown
/// keywords). Markers inside unused `$defs`/`definitions` apply to nothing and are ignored.
pub fn validate_secret_schema_support(schema: &Value) -> Result<(), CoreError> {
    let mut scan = MarkerScan {
        resolver: SchemaResolver::new(schema),
        visited: HashSet::new(),
        problems: BTreeSet::new(),
    };
    scan.scan_schema(schema, "", None, false);
    let problems: Vec<String> = scan.problems.into_iter().collect();
    if problems.is_empty() {
        Ok(())
    } else {
        Err(CoreError::Validation(format!(
            "Catalog schema cannot be checked for secret fields: {}. Keel supports \
             {SUPPORTED_KEYWORDS}; restructure the schema so every secret field is reachable \
             through them",
            problems.join("; ")
        )))
    }
}

/// Keywords of a schema document that [`standalone_subschema`] copies into a subschema.
const STANDALONE_ROOT_KEYWORDS: [&str; 3] = ["$defs", "definitions", "$id"];

/// A self-contained copy of `sub`, a subschema of the document `root` (a bundle item's
/// `properties.<item>`): local `$ref`s inside `sub` resolve against `root`, so the copy gets the
/// root's `$defs`/`definitions` (merged over its own; the root's win, as `#/$defs/x` always
/// meant the root's) and, when `sub` has none, the root's `$id`. A `$ref` into any other part of
/// `root` (`#/properties/other/...`, an anchor outside `sub`) does not resolve in the copy:
/// [`validate_standalone_subschema`] rejects such a document so the secret walker never misses
/// a secret behind it.
pub fn standalone_subschema(root: &Value, sub: &Value) -> Value {
    let mut out = sub.clone();
    let Some(obj) = out.as_object_mut() else {
        return out;
    };
    for keyword in ["$defs", "definitions"] {
        if let Some(defs) = root.get(keyword).and_then(Value::as_object) {
            let merged = obj
                .entry(keyword.to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            if !merged.is_object() {
                *merged = Value::Object(Map::new());
            }
            if let Some(merged) = merged.as_object_mut() {
                for (name, def) in defs {
                    merged.insert(name.clone(), def.clone());
                }
            }
        }
    }
    if !obj.contains_key("$id")
        && let Some(id) = root.get("$id").filter(|v| v.is_string())
    {
        obj.insert("$id".to_string(), id.clone());
    }
    out
}

/// Restores in `standalone` (a [`standalone_subschema`] of `original`, possibly transformed
/// along `properties`) the keywords the copy took from the document root, so it can be put back
/// into the document without shadowing the root's definitions or becoming an embedded
/// resource (`$id`).
pub fn restore_standalone_keywords(standalone: &mut Value, original: &Value) {
    let Some(obj) = standalone.as_object_mut() else {
        return;
    };
    for keyword in STANDALONE_ROOT_KEYWORDS {
        match original.get(keyword) {
            Some(value) => {
                obj.insert(keyword.to_string(), value.clone());
            }
            None => {
                obj.remove(keyword);
            }
        }
    }
}

/// [`validate_secret_schema_support`] on the [`standalone_subschema`] of `sub` in `root`: the
/// copy Keel partitions and masks a bundle item's values with must find every secret the
/// document declares for it (`NFR-SEC-05`).
pub fn validate_standalone_subschema(root: &Value, sub: &Value) -> Result<(), CoreError> {
    validate_secret_schema_support(&standalone_subschema(root, sub))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn paths(schema: &Value) -> Vec<String> {
        SchemaResolver::new(schema).declared_secret_paths()
    }

    fn secret_paths_in(schema: &Value, values: &Value) -> Vec<String> {
        SchemaResolver::new(schema)
            .secret_value_locations(values)
            .into_iter()
            .map(|l| l.path)
            .collect()
    }

    fn split(schema: &Value, values: &Value) -> (Option<Value>, Option<Value>) {
        let r = SchemaResolver::new(schema);
        r.split(&[schema], values)
    }

    fn assert_supported(schema: &Value) {
        validate_secret_schema_support(schema)
            .unwrap_or_else(|e| panic!("schema should be supported: {e}"));
    }

    fn assert_rejected(schema: &Value, needle: &str) {
        let err = validate_secret_schema_support(schema)
            .expect_err("schema should be rejected")
            .to_string();
        assert!(
            err.contains(needle),
            "error '{err}' should mention '{needle}'"
        );
    }

    #[test]
    fn ref_into_defs_and_definitions_is_followed() {
        for defs in ["$defs", "definitions"] {
            let schema = json!({
                defs: { "pw": { "type": "string", "writeOnly": true } },
                "properties": { "adminPassword": { "$ref": format!("#/{defs}/pw") } }
            });
            assert_eq!(paths(&schema), vec!["adminPassword"]);
            let (normal, secret) = split(&schema, &json!({"adminPassword": "hunter2", "x": 1}));
            assert_eq!(normal, Some(json!({"x": 1})));
            assert_eq!(secret, Some(json!({"adminPassword": "hunter2"})));
            assert_supported(&schema);
        }
    }

    #[test]
    fn ref_forms_root_id_anchor_and_percent_encoding() {
        let schema = json!({
            "$id": "https://example.com/values.json",
            "$defs": {
                "a b": { "format": "password" },
                "anchored": { "$anchor": "tok", "writeOnly": true },
                "obj": { "properties": { "inner": { "$ref": "https://example.com/values.json#/$defs/a%20b" } } }
            },
            "properties": {
                "spaced": { "$ref": "#/$defs/a%20b" },
                "anchored": { "$ref": "#tok" },
                "nested": { "$ref": "#/$defs/obj" }
            }
        });
        assert_eq!(paths(&schema), vec!["anchored", "nested.inner", "spaced"]);
        assert_supported(&schema);
    }

    #[test]
    fn ref_cycles_terminate() {
        let schema = json!({
            "$defs": {
                "node": {
                    "type": "object",
                    "properties": {
                        "child": { "$ref": "#/$defs/node" },
                        "token": { "writeOnly": true }
                    }
                },
                "loopA": { "$ref": "#/$defs/loopB" },
                "loopB": { "allOf": [{ "$ref": "#/$defs/loopA" }] }
            },
            "properties": {
                "tree": { "$ref": "#/$defs/node" },
                "loop": { "$ref": "#/$defs/loopA" },
                "self": { "$ref": "#" }
            }
        });
        assert_eq!(paths(&schema), vec!["tree.token"]);
        // Values are finite: concrete paths follow the recursion as deep as the data goes.
        let values = json!({
            "tree": { "child": { "child": { "token": "deep", "other": 1 } }, "token": "top" },
            "loop": { "x": 1 },
            "self": { "tree": { "token": "via-root" } }
        });
        assert_eq!(
            secret_paths_in(&schema, &values),
            vec!["self.tree.token", "tree.child.child.token", "tree.token"]
        );
        let (normal, secret) = split(&schema, &values);
        let normal = normal.unwrap().to_string();
        assert!(
            !normal.contains("deep") && !normal.contains("top") && !normal.contains("via-root")
        );
        assert!(secret.unwrap().to_string().contains("deep"));
        assert!(SchemaResolver::new(&schema).is_secret_path("tree.child.child.token"));
        assert_supported(&schema);
    }

    #[test]
    fn pattern_properties_match_by_regex() {
        let schema = json!({
            "properties": {
                "env": {
                    "type": "object",
                    "properties": { "LOG_LEVEL": { "type": "string" } },
                    "patternProperties": {
                        "^SECRET_": { "type": "string", "format": "password" },
                        "(?<!_)URL$": { "type": "string" }
                    }
                }
            }
        });
        // No static name for a pattern-matched field.
        assert!(paths(&schema).is_empty());
        let values = json!({"env": {"SECRET_KEY": "s3cr3t", "LOG_LEVEL": "info", "DB_URL": "x"}});
        assert_eq!(secret_paths_in(&schema, &values), vec!["env.SECRET_KEY"]);
        let (normal, secret) = split(&schema, &values);
        assert_eq!(
            normal,
            Some(json!({"env": {"LOG_LEVEL": "info", "DB_URL": "x"}}))
        );
        assert_eq!(secret, Some(json!({"env": {"SECRET_KEY": "s3cr3t"}})));
        let r = SchemaResolver::new(&schema);
        assert!(r.is_secret_path("env.SECRET_TOKEN"));
        assert!(!r.is_secret_path("env.LOG_LEVEL"));
        assert_supported(&schema);
    }

    #[test]
    fn invalid_pattern_counts_as_a_match() {
        let schema = json!({ "patternProperties": { "(": { "writeOnly": true } } });
        assert_eq!(
            secret_paths_in(&schema, &json!({"anything": "v"})),
            vec!["anything"]
        );
    }

    #[test]
    fn additional_properties_only_apply_to_unmatched_keys() {
        let schema = json!({
            "properties": { "name": { "type": "string" } },
            "patternProperties": { "^x-": { "type": "string" } },
            "additionalProperties": { "writeOnly": true }
        });
        let values = json!({"name": "n", "x-tag": "t", "other": "secret"});
        assert_eq!(secret_paths_in(&schema, &values), vec!["other"]);
    }

    #[test]
    fn if_then_else_all_branches_count() {
        let schema = json!({
            "properties": { "mode": { "enum": ["a", "b"] } },
            "if": { "properties": { "mode": { "const": "a" } } },
            "then": { "properties": { "apiKey": { "writeOnly": true } } },
            "else": { "properties": { "password": { "format": "password" } } }
        });
        assert_eq!(paths(&schema), vec!["apiKey", "password"]);
        // Whatever `mode` says, both branches' secrets stay secret.
        let values = json!({"mode": "b", "apiKey": "k", "password": "p"});
        let (normal, secret) = split(&schema, &values);
        assert_eq!(normal, Some(json!({"mode": "b"})));
        assert_eq!(secret, Some(json!({"apiKey": "k", "password": "p"})));
        assert_supported(&schema);
    }

    #[test]
    fn dependent_schemas_and_dependencies_count() {
        let schema = json!({
            "properties": { "auth": { "type": "boolean" } },
            "dependentSchemas": { "auth": { "properties": { "token": { "writeOnly": true } } } },
            "dependencies": {
                "ldap": { "properties": { "bindPassword": { "format": "password" } } },
                "user": ["password"]
            }
        });
        assert_eq!(paths(&schema), vec!["bindPassword", "token"]);
        assert_eq!(
            secret_paths_in(&schema, &json!({"token": "t", "bindPassword": "b"})),
            vec!["bindPassword", "token"]
        );
        assert_supported(&schema);
    }

    #[test]
    fn prefix_items_tuple_items_and_additional_items() {
        let schema = json!({
            "properties": {
                "pair": {
                    "type": "array",
                    "prefixItems": [{ "type": "string" }, { "writeOnly": true }],
                    "items": { "type": "string" }
                },
                "legacy": {
                    "type": "array",
                    "items": [{ "type": "string" }],
                    "additionalItems": { "format": "password" }
                }
            }
        });
        assert_eq!(paths(&schema), vec!["legacy[]", "pair[]"]);
        let values = json!({"pair": ["user", "pw", "extra"], "legacy": ["name", "secret"]});
        assert_eq!(
            secret_paths_in(&schema, &values),
            vec!["legacy[]", "pair[]"]
        );
        let locations = SchemaResolver::new(&schema).secret_value_locations(&values);
        let pointers: Vec<&str> = locations.iter().map(|l| l.pointer.as_str()).collect();
        assert_eq!(pointers, vec!["/legacy/1", "/pair/1"]);
        let (normal, secret) = split(&schema, &values);
        assert_eq!(
            normal,
            Some(json!({"pair": ["user", "extra"], "legacy": ["name"]}))
        );
        assert_eq!(secret, Some(json!({"pair": ["pw"], "legacy": ["secret"]})));
        assert_supported(&schema);
    }

    #[test]
    fn combinators_union_every_branch() {
        // The old walker took the first branch declaring `token` and stored it in plaintext.
        let schema = json!({
            "anyOf": [
                { "properties": { "token": { "type": "string" } } },
                { "properties": { "token": { "type": "string", "writeOnly": true } } }
            ],
            "oneOf": [
                { "properties": { "cfg": { "properties": { "key": { "type": "string" } } } } },
                { "properties": { "cfg": { "properties": { "key": { "format": "password" } } } } }
            ]
        });
        assert_eq!(paths(&schema), vec!["cfg.key", "token"]);
        let (normal, secret) = split(&schema, &json!({"token": "t", "cfg": {"key": "k"}}));
        assert_eq!(normal, None);
        assert_eq!(secret, Some(json!({"token": "t", "cfg": {"key": "k"}})));
        assert_supported(&schema);
    }

    #[test]
    fn declared_not_secret_needs_write_only_false_and_no_secret_branch() {
        let schema = json!({
            "properties": {
                "tokenName": { "type": "string", "writeOnly": false },
                "both": { "allOf": [{ "writeOnly": false }, { "format": "password" }] },
                "plain": { "type": "string" }
            }
        });
        let r = SchemaResolver::new(&schema);
        let at = |k: &str| r.property_schemas(&[&schema], k);
        assert!(r.is_declared_not_secret(&at("tokenName")));
        assert!(!r.is_declared_not_secret(&at("both")));
        assert!(r.is_secret(&at("both")));
        assert!(!r.is_declared_not_secret(&at("plain")));
    }

    #[test]
    fn fail_closed_on_unreachable_markers() {
        for (schema, needle) in [
            (
                json!({"not": {"properties": {"pw": {"writeOnly": true}}}}),
                "#/not/properties/pw",
            ),
            (
                json!({"properties": {"l": {"contains": {"format": "password"}}}}),
                "#/properties/l/contains",
            ),
            (
                json!({"propertyNames": {"format": "password"}}),
                "#/propertyNames",
            ),
            (
                json!({"unevaluatedProperties": {"writeOnly": true}}),
                "#/unevaluatedProperties",
            ),
            (
                json!({"x-vendor": {"secret": {"writeOnly": true}}}),
                "#/x-vendor/secret",
            ),
            (
                json!({"$defs": {"pw": {"writeOnly": true}}, "not": {"$ref": "#/$defs/pw"}}),
                "#/$defs/pw",
            ),
        ] {
            assert_rejected(&schema, needle);
        }
    }

    #[test]
    fn fail_closed_on_references_the_walker_cannot_follow() {
        assert_rejected(
            &json!({"properties": {"a": {"$ref": "https://example.com/schema.json#/pw"}}}),
            "not a local reference",
        );
        assert_rejected(
            &json!({"properties": {"a": {"$ref": "other.json"}}}),
            "not a local reference",
        );
        assert_rejected(
            &json!({"properties": {"a": {"$ref": "#/$defs/missing"}}}),
            "does not resolve",
        );
        assert_rejected(
            &json!({"properties": {"a": {"$dynamicRef": "#meta"}}}),
            "$dynamicRef",
        );
        assert_rejected(
            &json!({"properties": {"a": {"$recursiveRef": "#"}}}),
            "$recursiveRef",
        );
        // Even inside an unused definition: it would surface the moment it is referenced.
        assert_rejected(
            &json!({"$defs": {"x": {"$ref": "https://evil.example/x"}}}),
            "not a local",
        );
        assert_rejected(
            &json!({"$defs": {"x": {"$id": "https://x/y.json", "properties": {"a": {"$ref": "#/z"}}}},
                    "properties": {"x": {"$ref": "#/$defs/x"}}}),
            "embedded schema resource",
        );
    }

    #[test]
    fn markers_in_data_and_unused_definitions_are_ignored() {
        assert_supported(&json!({
            "$defs": { "unused": { "writeOnly": true } },
            "properties": {
                "writeOnly": true,
                "format": { "type": "string", "default": "password", "enum": ["password"] },
                "cfg": { "default": { "writeOnly": true }, "examples": [{ "format": "password" }] }
            }
        }));
        assert_supported(&json!({"type": "object", "properties": {"name": {"type": "string"}}}));
    }

    #[test]
    fn fail_closed_when_a_shared_secret_definition_is_also_used_by_an_unfollowed_keyword() {
        // The definition is reachable through `properties.adminPassword`, but it also applies
        // through keywords the walker never classifies values with.
        for keyword in ["unevaluatedProperties", "propertyNames", "not", "x-vendor"] {
            let schema = json!({
                "$defs": { "pw": { "type": "string", "writeOnly": true } },
                "properties": { "adminPassword": { "$ref": "#/$defs/pw" } },
                keyword: { "$ref": "#/$defs/pw" }
            });
            assert_rejected(&schema, &format!("applies through '#/{keyword}'"));
        }
        let schema = json!({
            "$defs": { "pw": { "type": "string", "writeOnly": true } },
            "properties": {
                "adminPassword": { "$ref": "#/$defs/pw" },
                "list": { "type": "array", "contains": { "$ref": "#/$defs/pw" } },
                "obj": { "$ref": "#/$defs/wrap" }
            }
        });
        assert_rejected(&schema, "applies through '#/properties/list/contains'");
        // A definition wrapping the shared one behind an unfollowed keyword.
        let schema = json!({
            "$defs": {
                "pw": { "type": "string", "writeOnly": true },
                "wrap": { "unevaluatedProperties": { "$ref": "#/$defs/pw" } }
            },
            "properties": { "a": { "$ref": "#/$defs/pw" }, "b": { "$ref": "#/$defs/wrap" } }
        });
        assert_rejected(
            &schema,
            "applies through '#/$defs/wrap/unevaluatedProperties'",
        );
        // Shared through followed keywords only: supported.
        assert_supported(&json!({
            "$defs": { "pw": { "type": "string", "writeOnly": true } },
            "properties": {
                "a": { "$ref": "#/$defs/pw" },
                "b": { "type": "array", "items": { "$ref": "#/$defs/pw" } }
            },
            "additionalProperties": { "$ref": "#/$defs/pw" }
        }));
    }

    #[test]
    fn standalone_subschema_resolves_root_definitions() {
        let root = json!({
            "$id": "https://example.com/bundle.json",
            "$defs": { "pw": { "type": "string", "writeOnly": true } },
            "properties": {
                "db": {
                    "$defs": { "own": { "type": "string" }, "pw": { "type": "string" } },
                    "properties": {
                        "dsn": { "$ref": "#/$defs/pw" },
                        "viaId": { "$ref": "https://example.com/bundle.json#/$defs/pw" },
                        "host": { "type": "string" }
                    }
                },
                "other": { "properties": { "x": { "$ref": "#/properties/db/properties/dsn" } } }
            }
        });
        let db = &root["properties"]["db"];
        let standalone = standalone_subschema(&root, db);
        assert_eq!(paths(&standalone), vec!["dsn", "viaId"]);
        let (normal, secret) = split(&standalone, &json!({"dsn": "pg://u:p@h", "host": "h"}));
        assert_eq!(normal, Some(json!({"host": "h"})));
        assert_eq!(secret, Some(json!({"dsn": "pg://u:p@h"})));
        validate_standalone_subschema(&root, db).unwrap();
        // The subschema copied on its own misses the secret: the reason for the helper.
        assert!(paths(db).is_empty());
        // A reference into another part of the document cannot be resolved in the copy.
        let err = validate_standalone_subschema(&root, &root["properties"]["other"])
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not resolve"), "{err}");
        // Putting a transformed copy back restores the subschema's own keywords.
        let mut back = standalone.clone();
        restore_standalone_keywords(&mut back, db);
        assert_eq!(back, *db);
        let mut bare = standalone_subschema(&root, &json!({"type": "object"}));
        restore_standalone_keywords(&mut bare, &json!({"type": "object"}));
        assert_eq!(bare, json!({"type": "object"}));
    }
}
