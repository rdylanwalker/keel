import React from "react"

import { Checkbox } from "@/components/ui/checkbox"
import { Input } from "@/components/ui/input"
import { NativeSelect } from "@/components/ui/native-select"
import { Textarea } from "@/components/ui/textarea"
import { DEFAULT_STARTER_MANIFEST } from "@/lib/demoFixtures"
import { CheckboxRow, Field } from "./Field"
import { CatalogFormSectionProps } from "./types"

/** Application catalog source: runtime (helm / kustomize / raw manifest) and upstream location. */
export const AppSourceFields: React.FC<
  CatalogFormSectionProps & { onExtractManifestVars: () => void }
> = ({ appForm, setAppForm, onExtractManifestVars }) => {
  const set = (patch: Partial<typeof appForm>) =>
    setAppForm((prev) => ({ ...prev, ...patch }))
  const runtime = appForm.runtime || "helm"
  return (
    <>
      <div className="grid grid-cols-1 sm:grid-cols-2 gap-4">
        <Field id="app-category" label="Category" required>
          <Input
            id="app-category"
            required
            placeholder="Applications"
            value={appForm.category}
            onChange={(e) => set({ category: e.target.value })}
            className="font-sans"
          />
        </Field>
        <Field id="app-runtime" label="Runtime" required>
          <NativeSelect
            id="app-runtime"
            value={runtime}
            onChange={(e) => {
              const val = e.target.value as "helm" | "kustomize" | "manifest"
              setAppForm((prev) => ({
                ...prev,
                runtime: val,
                ...(val === "manifest" && !prev.manifestsYaml
                  ? { manifestsYaml: DEFAULT_STARTER_MANIFEST }
                  : {}),
                ...(val === "manifest" && !prev.defaultEngine
                  ? { defaultEngine: "flux" as const }
                  : {}),
              }))
            }}
          >
            <option value="helm">helm</option>
            <option value="kustomize">kustomize</option>
            <option value="manifest">Raw K8s Manifest / Template</option>
          </NativeSelect>
        </Field>
      </div>

      {runtime === "manifest" ? (
        <div className="space-y-4">
          <Field
            id="app-default-engine"
            label="Action Engine (Default)"
            required
          >
            <NativeSelect
              id="app-default-engine"
              data-testid="catalog-action-engine-select"
              value={appForm.defaultEngine || "flux"}
              onChange={(e) =>
                set({ defaultEngine: e.target.value as "flux" | "pulumi" })
              }
            >
              <option value="flux">
                Flux CD (Kustomization + postBuild substitution)
              </option>
              <option value="pulumi">
                Pulumi (PKO v2 Stack + ConfigGroup preview)
              </option>
            </NativeSelect>
          </Field>
          <Field
            id="app-manifests-yaml"
            label="Kubernetes Manifest YAML"
            required
            action={
              <button
                type="button"
                data-testid="extract-manifest-vars-btn"
                onClick={onExtractManifestVars}
                className="text-[11px] font-medium text-emerald-400 hover:text-emerald-300 transition-colors cursor-pointer"
              >
                Auto-Extract `${`var`}` Fields
              </button>
            }
          >
            <Textarea
              id="app-manifests-yaml"
              data-testid="catalog-manifests-yaml-input"
              rows={10}
              spellCheck={false}
              value={appForm.manifestsYaml ?? ""}
              onChange={(e) => set({ manifestsYaml: e.target.value })}
              placeholder={DEFAULT_STARTER_MANIFEST}
              className="text-[11px] leading-relaxed"
            />
            <p className="text-[11px] text-slate-500">
              Values used inside a larger YAML scalar (e.g.{" "}
              <code className="font-mono">image: repo/${"{tag}"}</code>) may not
              contain quotes, brackets, #, or &quot;: &quot;; use a placeholder
              as the whole value to allow any string.
            </p>
          </Field>
        </div>
      ) : (
        <>
          <Field id="app-repourl" label="Repository URL" required>
            <Input
              id="app-repourl"
              required
              placeholder={
                runtime === "kustomize"
                  ? "https://github.com/stefanprodan/podinfo"
                  : "https://github.com/stefanprodan/podinfo or chart repo URL"
              }
              value={appForm.repoUrl || ""}
              onChange={(e) => set({ repoUrl: e.target.value })}
            />
          </Field>
          <div className="grid grid-cols-1 sm:grid-cols-2 gap-4">
            {runtime === "helm" ? (
              <Field id="app-chart" label="Chart Name" required>
                <Input
                  id="app-chart"
                  placeholder="podinfo"
                  value={appForm.chart || ""}
                  onChange={(e) => set({ chart: e.target.value })}
                />
              </Field>
            ) : (
              <Field id="app-path" label="Path">
                <Input
                  id="app-path"
                  placeholder="./base"
                  value={appForm.path || ""}
                  onChange={(e) => set({ path: e.target.value })}
                />
              </Field>
            )}
            <Field
              id="app-version"
              label={runtime === "helm" ? "Chart Version" : "Version / Git Ref"}
            >
              <Input
                id="app-version"
                placeholder={runtime === "helm" ? "6.7.0" : "v6.7.0"}
                value={appForm.version || ""}
                onChange={(e) => set({ version: e.target.value })}
              />
            </Field>
          </div>
        </>
      )}

      <CheckboxRow id="app-flagger" label="Flagger Canary Support Enabled">
        <Checkbox
          id="app-flagger"
          checked={!!appForm.flaggerSupported}
          onCheckedChange={(v) => set({ flaggerSupported: v === true })}
        />
      </CheckboxRow>

      <Field id="app-description" label="Description">
        <Input
          id="app-description"
          placeholder="Optional summary..."
          value={appForm.description || ""}
          onChange={(e) => set({ description: e.target.value })}
          className="font-sans"
        />
      </Field>
    </>
  )
}
