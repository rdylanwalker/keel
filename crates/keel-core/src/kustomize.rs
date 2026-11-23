//! Three-way merge of the Keel-generated kustomize resource indexes (`tenants/kustomization.yaml`,
//! `tenants/<id>/kustomization.yaml`).
//!
//! Every proposal rewrites its tenant's index, so two proposals opened from the same `main`
//! usually touch neighbouring lines of the same file and Git reports a textual conflict once the
//! first one merges. The index is a plain list, though: a proposal's intent is "these entries
//! added, those removed", which [`merge_kustomization_index`] re-applies to the current `main`.

use std::collections::BTreeSet;

use serde_json::Value;

const KUSTOMIZE_API_VERSION: &str = "kustomize.config.k8s.io/v1beta1";

/// Resource entries of a Keel-generated index: a `kustomize.config.k8s.io/v1beta1`
/// `Kustomization` with no keys other than `apiVersion`, `kind` and a list of string
/// `resources` (missing or `null` = empty). `None` for any other document, which a caller must
/// then treat as user content.
pub fn parse_kustomization_index(yaml: &str) -> Option<Vec<String>> {
    let doc: Value = noyalib::from_str(yaml).ok()?;
    let map = doc.as_object()?;
    if map.get("apiVersion").and_then(Value::as_str) != Some(KUSTOMIZE_API_VERSION)
        || map.get("kind").and_then(Value::as_str) != Some("Kustomization")
        || map
            .keys()
            .any(|k| !matches!(k.as_str(), "apiVersion" | "kind" | "resources"))
    {
        return None;
    }
    match map.get("resources") {
        None | Some(Value::Null) => Some(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| v.as_str().map(str::to_string))
            .collect(),
        Some(_) => None,
    }
}

/// Re-applies the change from `base` to `ours` onto `theirs`: entries `ours` removed are dropped
/// from `theirs`, entries `ours` added are appended (in `ours` order) unless `theirs` already
/// lists them. Everything `theirs` changed is kept.
pub fn merge_kustomization_resources(
    base: &[String],
    ours: &[String],
    theirs: &[String],
) -> Vec<String> {
    let base_set: BTreeSet<&str> = base.iter().map(String::as_str).collect();
    let ours_set: BTreeSet<&str> = ours.iter().map(String::as_str).collect();
    let mut merged: Vec<String> = theirs
        .iter()
        .filter(|e| !(base_set.contains(e.as_str()) && !ours_set.contains(e.as_str())))
        .cloned()
        .collect();
    for entry in ours {
        if !base_set.contains(entry.as_str()) && !merged.contains(entry) {
            merged.push(entry.clone());
        }
    }
    merged
}

/// Three-way merge of a Keel-generated index file: the merged resource list, or `None` when any
/// of the versions is not a plain index ([`parse_kustomization_index`]). `base` is `None` when
/// both sides created the file (an empty index).
pub fn merge_kustomization_index(
    base: Option<&str>,
    ours: &str,
    theirs: &str,
) -> Option<Vec<String>> {
    let base = match base {
        Some(base) => parse_kustomization_index(base)?,
        None => Vec::new(),
    };
    Some(merge_kustomization_resources(
        &base,
        &parse_kustomization_index(ours)?,
        &parse_kustomization_index(theirs)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn index(items: &[&str]) -> String {
        crate::yaml::to_string(&serde_json::json!({
            "apiVersion": KUSTOMIZE_API_VERSION,
            "kind": "Kustomization",
            "resources": items,
        }))
        .unwrap()
    }

    #[test]
    fn parses_only_plain_generated_indexes() {
        assert_eq!(
            parse_kustomization_index(&index(&["a.yaml", "b/"])),
            Some(v(&["a.yaml", "b/"]))
        );
        assert_eq!(
            parse_kustomization_index(&index(&[])),
            Some(Vec::new()),
            "`resources: []`"
        );
        assert_eq!(
            parse_kustomization_index(
                "apiVersion: kustomize.config.k8s.io/v1beta1\nkind: Kustomization\n"
            ),
            Some(Vec::new())
        );
        for other in [
            "apiVersion: kustomize.config.k8s.io/v1beta1\nkind: Kustomization\nresources: [a]\npatches: []\n",
            "apiVersion: kustomize.config.k8s.io/v1beta1\nkind: Component\nresources: [a]\n",
            "apiVersion: v1\nkind: Kustomization\nresources: [a]\n",
            "apiVersion: kustomize.config.k8s.io/v1beta1\nkind: Kustomization\nresources: [1]\n",
            "apiVersion: kustomize.config.k8s.io/v1beta1\nkind: Kustomization\nresources: a\n",
            "<<<<<<< HEAD\n",
            "- a\n",
        ] {
            assert_eq!(parse_kustomization_index(other), None, "{other}");
        }
    }

    #[test]
    fn reapplies_the_proposal_onto_the_new_main() {
        // Snag #9 of the v0.12.0 dry run: two deletions opened from the same `main`.
        let base = v(&[
            "infra/kind-cluster/stack.yaml",
            "apps/podinfo/helm-repository.yaml",
            "apps/podinfo/helmrelease.yaml",
        ]);
        let first_merged = v(&["infra/kind-cluster/stack.yaml"]);
        let second = v(&[
            "apps/podinfo/helm-repository.yaml",
            "apps/podinfo/helmrelease.yaml",
        ]);
        assert_eq!(
            merge_kustomization_resources(&base, &second, &first_merged),
            Vec::<String>::new()
        );

        // Two additions: main's entry stays first, the proposal's is appended once.
        let base = v(&["a.yaml"]);
        assert_eq!(
            merge_kustomization_resources(
                &base,
                &v(&["a.yaml", "c.yaml"]),
                &v(&["a.yaml", "b.yaml"])
            ),
            v(&["a.yaml", "b.yaml", "c.yaml"])
        );
        // Both sides added the same entry: listed once.
        assert_eq!(
            merge_kustomization_resources(
                &base,
                &v(&["a.yaml", "b.yaml"]),
                &v(&["a.yaml", "b.yaml"])
            ),
            v(&["a.yaml", "b.yaml"])
        );
        // An entry main removed stays removed even though the proposal kept it unchanged.
        assert_eq!(
            merge_kustomization_resources(&v(&["a", "b"]), &v(&["a", "b", "c"]), &v(&["b"])),
            v(&["b", "c"])
        );
    }

    #[test]
    fn merges_index_files_or_refuses_foreign_documents() {
        assert_eq!(
            merge_kustomization_index(
                Some(&index(&["a"])),
                &index(&["a", "c"]),
                &index(&["a", "b"])
            ),
            Some(v(&["a", "b", "c"]))
        );
        // Both sides created the index.
        assert_eq!(
            merge_kustomization_index(None, &index(&["c"]), &index(&["b"])),
            Some(v(&["b", "c"]))
        );
        assert_eq!(
            merge_kustomization_index(
                Some(&index(&["a"])),
                &index(&["a", "c"]),
                "apiVersion: kustomize.config.k8s.io/v1beta1\nkind: Kustomization\nresources: [a]\npatches: []\n"
            ),
            None
        );
    }
}
