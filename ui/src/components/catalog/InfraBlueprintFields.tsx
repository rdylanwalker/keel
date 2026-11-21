import React from "react"

import { Checkbox } from "@/components/ui/checkbox"
import { Input } from "@/components/ui/input"
import { NativeSelect } from "@/components/ui/native-select"
import { Textarea } from "@/components/ui/textarea"
import { InfraResourceKind } from "@/types/api"
import { CheckboxRow, Field } from "./Field"
import { CatalogFormSectionProps } from "./types"
import { starterPulumiYaml } from "./catalogForm"

const PROVIDERS = [
  "kind",
  "aws",
  "proxmox",
  "gcp",
  "azure",
  "hetzner",
  "cloudflare",
  "custom",
]

/** PKO v2 infrastructure blueprint fields (provider, runtime, Pulumi program). */
export const InfraBlueprintFields: React.FC<
  CatalogFormSectionProps & { isEditing: boolean }
> = ({ appForm, setAppForm, isEditing }) => {
  const set = (patch: Partial<typeof appForm>) =>
    setAppForm((prev) => ({ ...prev, ...patch }))
  return (
    <>
      <div className="grid grid-cols-1 sm:grid-cols-2 gap-4">
        <Field
          id="infra-resource-kind"
          label="Infrastructure Resource Kind"
          required
        >
          <NativeSelect
            id="infra-resource-kind"
            data-testid="infra-resource-kind-select"
            value={appForm.resourceKind || "cluster"}
            onChange={(e) =>
              set({ resourceKind: e.target.value as InfraResourceKind })
            }
          >
            <option value="vm">Virtual Machine (SSH Enabled)</option>
            <option value="cluster">
              Kubernetes Cluster (App Deployment Target)
            </option>
            <option value="service">Managed Service / Config</option>
          </NativeSelect>
        </Field>
        <Field id="app-category" label="Category" required>
          <Input
            id="app-category"
            required
            placeholder="Infrastructure"
            value={appForm.category}
            onChange={(e) => set({ category: e.target.value })}
            className="font-sans"
          />
        </Field>
        <Field id="infra-provider" label="Provider" required>
          <NativeSelect
            id="infra-provider"
            value={appForm.provider || "kind"}
            onChange={(e) => set({ provider: e.target.value })}
          >
            {PROVIDERS.map((p) => (
              <option key={p} value={p}>
                {p}
              </option>
            ))}
          </NativeSelect>
        </Field>
        <Field id="infra-pulumi-runtime" label="Pulumi Runtime" required>
          <NativeSelect
            id="infra-pulumi-runtime"
            value={appForm.pulumiRuntime || "yaml"}
            onChange={(e) =>
              set({
                pulumiRuntime: e.target.value as
                  "yaml" | "hcl" | "opentofu-bridge",
              })
            }
          >
            <option value="yaml">yaml</option>
            <option value="hcl">hcl</option>
            <option value="opentofu-bridge">opentofu-bridge</option>
          </NativeSelect>
        </Field>
        <Field id="infra-entrypoint" label="Entrypoint">
          <Input
            id="infra-entrypoint"
            placeholder="Pulumi.yaml"
            value={appForm.entrypoint ?? "Pulumi.yaml"}
            onChange={(e) => set({ entrypoint: e.target.value })}
          />
        </Field>
        <Field id="infra-tf-provider" label="OpenTofu / Terraform Provider">
          <Input
            id="infra-tf-provider"
            placeholder="e.g. tehcyx/kind, hashicorp/aws, bpg/proxmox"
            value={appForm.terraformProvider || ""}
            onChange={(e) => set({ terraformProvider: e.target.value })}
          />
        </Field>
      </div>

      <CheckboxRow id="infra-protect" label="Protect by Default">
        <Checkbox
          id="infra-protect"
          checked={!!appForm.protectByDefault}
          onCheckedChange={(v) => set({ protectByDefault: v === true })}
        />
      </CheckboxRow>

      <Field id="infra-description" label="Description">
        <Input
          id="infra-description"
          placeholder="Optional summary..."
          value={appForm.description || ""}
          onChange={(e) => set({ description: e.target.value })}
          className="font-sans"
        />
      </Field>

      <Field
        id="infra-pulumi-yaml"
        label="Pulumi.yaml Program"
        action={
          <button
            type="button"
            onClick={() => set({ pulumiYaml: starterPulumiYaml(appForm) })}
            className="text-[11px] text-indigo-400 hover:text-indigo-300 transition-colors cursor-pointer"
          >
            Insert Starter Template
          </button>
        }
      >
        <Textarea
          id="infra-pulumi-yaml"
          rows={10}
          spellCheck={false}
          placeholder={
            isEditing
              ? "Leave blank to preserve existing Pulumi.yaml, or paste/edit YAML..."
              : "Optional — leave blank to auto-generate a starter Pulumi.yaml"
          }
          value={appForm.pulumiYaml || ""}
          onChange={(e) => set({ pulumiYaml: e.target.value })}
          className="leading-relaxed"
        />
      </Field>
    </>
  )
}
