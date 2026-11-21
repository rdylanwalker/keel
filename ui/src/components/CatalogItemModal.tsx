import React, { useState } from "react"
import {
  AlertCircle,
  Box,
  Loader2,
  Maximize2,
  Minimize2,
  Plus,
  Trash2,
  X,
} from "lucide-react"
import {
  CatalogBundleItem,
  CatalogItem,
  CreateCatalogItemPayload,
} from "../types/api"
import { DEFAULT_STARTER_MANIFEST } from "../lib/demoFixtures"
import {
  PROVIDER_CREDENTIAL_PRESETS,
  SchemaFieldDraft,
  deriveFieldsFromPulumiYaml,
  fieldsToSchemaJson,
  schemaJsonToFields,
} from "../lib/schemaBuilder"
import { SchemaFieldsEditor } from "./SchemaFieldsEditor"

export interface CatalogItemModalProps {
  isOpen: boolean
  editingCatalogItem: CatalogItem | null
  catalog: CatalogItem[]
  appForm: CreateCatalogItemPayload
  setAppForm: React.Dispatch<React.SetStateAction<CreateCatalogItemPayload>>
  schemaJsonText: string
  setSchemaJsonText: React.Dispatch<React.SetStateAction<string>>
  schemaFields: SchemaFieldDraft[]
  setSchemaFields: React.Dispatch<React.SetStateAction<SchemaFieldDraft[]>>
  showSchemaEditor: boolean
  setShowSchemaEditor: React.Dispatch<React.SetStateAction<boolean>>
  appLoading: boolean
  appError: string | null
  tenants: import("../types/api").TenantSummary[]
  onSubmit: (e: React.FormEvent) => void
  onExtractManifestVars: () => void
  onClose: () => void
  onCancel: () => void
}

export const CatalogItemModal: React.FC<CatalogItemModalProps> = ({
  isOpen,
  editingCatalogItem,
  catalog,
  appForm,
  setAppForm,
  schemaJsonText,
  setSchemaJsonText,
  schemaFields,
  setSchemaFields,
  showSchemaEditor,
  setShowSchemaEditor,
  appLoading,
  appError,
  tenants,
  onSubmit,
  onExtractManifestVars,
  onClose,
  onCancel,
}) => {
  const [isAddModalFullscreen, setIsAddModalFullscreen] = useState(false)

  if (!isOpen) return null

  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label={editingCatalogItem ? "Edit Catalog Item" : "Add Catalog Item"}
      className={`fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm ${
        isAddModalFullscreen ? "p-0" : "p-4"
      }`}
    >
      <div
        data-testid="catalog-item-modal"
        className={`bg-slate-900 border-slate-800 flex flex-col shadow-2xl ${
          isAddModalFullscreen
            ? "w-full h-full rounded-none border-0 p-6"
            : "border rounded-2xl max-w-4xl w-full max-h-[90vh] p-6"
        }`}
      >
        <div className="flex items-center justify-between border-b border-slate-800 pb-3 shrink-0">
          <h3 className="text-base font-bold text-white flex items-center gap-2">
            <Box className="h-4 w-4 text-indigo-400" />
            {editingCatalogItem ? "Edit Catalog Item" : "Add Catalog Item"}
          </h3>
          <div className="flex items-center gap-1.5">
            <button
              type="button"
              onClick={() => setIsAddModalFullscreen((prev) => !prev)}
              aria-label={
                isAddModalFullscreen ? "Exit full screen" : "Enter full screen"
              }
              title={isAddModalFullscreen ? "Exit full screen" : "Full screen"}
              className="p-1 rounded-lg text-slate-400 hover:text-white hover:bg-slate-800 transition-colors cursor-pointer"
            >
              {isAddModalFullscreen ? (
                <Minimize2 className="h-4 w-4" />
              ) : (
                <Maximize2 className="h-4 w-4" />
              )}
            </button>
            <button
              type="button"
              onClick={() => {
                setIsAddModalFullscreen(false)
                onClose()
              }}
              aria-label="Close modal"
              className="p-1 rounded-lg text-slate-400 hover:text-white hover:bg-slate-800 transition-colors cursor-pointer"
            >
              <X className="h-5 w-5" />
            </button>
          </div>
        </div>

        <form onSubmit={onSubmit} className="flex-1 min-h-0 flex flex-col mt-4">
          <div className="flex-1 min-h-0 overflow-y-auto space-y-4 pr-1">
            <div>
              <label
                htmlFor="catalog-type"
                className="block text-xs text-slate-300 font-medium mb-1"
              >
                Catalog Type *
              </label>
              <select
                id="catalog-type"
                disabled={Boolean(editingCatalogItem)}
                value={appForm.kind || "app"}
                onChange={(e) => {
                  const newKind = e.target.value as "app" | "infra" | "bundle"
                  setAppForm((prev) => ({
                    ...prev,
                    kind: newKind,
                    category:
                      newKind === "infra"
                        ? "Infrastructure"
                        : newKind === "bundle"
                          ? "Platform"
                          : "Applications",
                    items:
                      newKind === "bundle" &&
                      (!prev.items || prev.items.length === 0)
                        ? [
                            {
                              id: "step-1",
                              kind: catalog.some(
                                (c) =>
                                  c.type === "infra" ||
                                  c.type === "infrastructure"
                              )
                                ? "infra"
                                : "app",
                              catalogRef:
                                catalog.find(
                                  (c) =>
                                    c.type === "infra" ||
                                    c.type === "infrastructure"
                                )?.id ??
                                catalog.find((c) => c.type === "app")?.id ??
                                "",
                              dependsOn: [],
                            },
                          ]
                        : prev.items,
                  }))
                }}
                className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 focus:outline-none focus:ring-1 focus:ring-indigo-500"
              >
                <option value="app">Application (Helm / Kustomize)</option>
                <option value="infra">Infrastructure Blueprint (PKO v2)</option>
                <option value="bundle">Multi-Item Bundle (Infra + Apps)</option>
              </select>
            </div>

            <div className="grid grid-cols-1 sm:grid-cols-2 gap-4">
              <div>
                <label
                  htmlFor="app-id"
                  className="block text-xs text-slate-300 font-medium mb-1"
                >
                  Item ID *
                </label>
                <input
                  id="app-id"
                  type="text"
                  required
                  disabled={Boolean(editingCatalogItem)}
                  placeholder="e.g. podinfo"
                  value={appForm.id}
                  onChange={(e) =>
                    setAppForm((prev) => ({ ...prev, id: e.target.value }))
                  }
                  className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 placeholder-slate-500 focus:outline-none focus:ring-1 focus:ring-indigo-500 font-mono disabled:opacity-60"
                />
              </div>

              <div>
                <label
                  htmlFor="app-name"
                  className="block text-xs text-slate-300 font-medium mb-1"
                >
                  Item Name *
                </label>
                <input
                  id="app-name"
                  type="text"
                  required
                  placeholder={
                    appForm.kind === "infra"
                      ? "e.g. Proxmox Cluster Blueprint"
                      : appForm.kind === "bundle"
                        ? "e.g. kind Cluster + Podinfo Stack"
                        : "e.g. Podinfo Microservice"
                  }
                  value={appForm.name}
                  onChange={(e) =>
                    setAppForm((prev) => ({
                      ...prev,
                      name: e.target.value,
                    }))
                  }
                  className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 placeholder-slate-500 focus:outline-none focus:ring-1 focus:ring-indigo-500"
                />
              </div>
            </div>

            <div className="grid grid-cols-1 sm:grid-cols-2 gap-4">
              <div>
                <label
                  htmlFor="catalog-tenant-scope"
                  className="block text-xs text-slate-300 font-medium mb-1"
                >
                  Tenant Visibility
                </label>
                <select
                  id="catalog-tenant-scope"
                  value={appForm.tenant || ""}
                  onChange={(e) =>
                    setAppForm((prev) => ({
                      ...prev,
                      tenant: e.target.value,
                    }))
                  }
                  className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 focus:outline-none focus:ring-1 focus:ring-indigo-500"
                >
                  <option value="">All Tenants (Global)</option>
                  {tenants.map((t) => (
                    <option key={t.id} value={t.id}>
                      {t.id}
                    </option>
                  ))}
                </select>
              </div>

              <div className="flex flex-col justify-end pb-2">
                <label className="flex items-center gap-2 cursor-pointer">
                  <input
                    type="checkbox"
                    id="catalog-protected"
                    data-testid="catalog-protected-checkbox"
                    checked={Boolean(appForm.protected)}
                    onChange={(e) =>
                      setAppForm((prev) => ({
                        ...prev,
                        protected: e.target.checked,
                      }))
                    }
                    className="w-4 h-4 bg-slate-950 border border-slate-800 rounded text-indigo-600 focus:ring-indigo-500 focus:ring-offset-slate-900 cursor-pointer"
                  />
                  <span className="text-xs text-slate-300 font-medium select-none">
                    Protect Catalog Entry (Block Deletion)
                  </span>
                </label>
              </div>
            </div>

            {(appForm.kind || "app") === "app" && (
              <>
                <div className="grid grid-cols-1 sm:grid-cols-2 gap-4">
                  <div>
                    <label
                      htmlFor="app-category"
                      className="block text-xs text-slate-300 font-medium mb-1"
                    >
                      Category *
                    </label>
                    <input
                      id="app-category"
                      type="text"
                      required
                      placeholder="Applications"
                      value={appForm.category}
                      onChange={(e) =>
                        setAppForm((prev) => ({
                          ...prev,
                          category: e.target.value,
                        }))
                      }
                      className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 placeholder-slate-500 focus:outline-none focus:ring-1 focus:ring-indigo-500"
                    />
                  </div>

                  <div>
                    <label
                      htmlFor="app-runtime"
                      className="block text-xs text-slate-300 font-medium mb-1"
                    >
                      Runtime *
                    </label>
                    <select
                      id="app-runtime"
                      value={appForm.runtime || "helm"}
                      onChange={(e) => {
                        const val = e.target.value as
                          "helm" | "kustomize" | "manifest"
                        setAppForm((prev) => ({
                          ...prev,
                          runtime: val,
                          ...(val === "manifest" && !prev.manifestsYaml
                            ? { manifestsYaml: DEFAULT_STARTER_MANIFEST }
                            : {}),
                          ...(val === "manifest" && !prev.defaultEngine
                            ? { defaultEngine: "flux" }
                            : {}),
                        }))
                      }}
                      className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 focus:outline-none focus:ring-1 focus:ring-indigo-500"
                    >
                      <option value="helm">helm</option>
                      <option value="kustomize">kustomize</option>
                      <option value="manifest">
                        Raw K8s Manifest / Template
                      </option>
                    </select>
                  </div>
                </div>

                {appForm.runtime === "manifest" && (
                  <div className="space-y-4">
                    <div>
                      <label
                        htmlFor="app-default-engine"
                        className="block text-xs text-slate-300 font-medium mb-1"
                      >
                        Action Engine (Default) *
                      </label>
                      <select
                        id="app-default-engine"
                        data-testid="catalog-action-engine-select"
                        value={appForm.defaultEngine || "flux"}
                        onChange={(e) =>
                          setAppForm((prev) => ({
                            ...prev,
                            defaultEngine: e.target.value as "flux" | "pulumi",
                          }))
                        }
                        className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 focus:outline-none focus:ring-1 focus:ring-indigo-500"
                      >
                        <option value="flux">
                          Flux CD (Kustomization + postBuild substitution)
                        </option>
                        <option value="pulumi">
                          Pulumi (PKO v2 Stack + ConfigGroup preview)
                        </option>
                      </select>
                    </div>

                    <div>
                      <div className="flex items-center justify-between mb-1">
                        <label
                          htmlFor="app-manifests-yaml"
                          className="block text-xs text-slate-300 font-medium"
                        >
                          Kubernetes Manifest YAML *
                        </label>
                        <button
                          type="button"
                          data-testid="extract-manifest-vars-btn"
                          onClick={onExtractManifestVars}
                          className="text-[11px] font-medium text-emerald-400 hover:text-emerald-300 transition-colors cursor-pointer flex items-center gap-1"
                        >
                          Auto-Extract `${`var`}` Fields
                        </button>
                      </div>
                      <textarea
                        id="app-manifests-yaml"
                        data-testid="catalog-manifests-yaml-input"
                        rows={10}
                        value={appForm.manifestsYaml ?? ""}
                        onChange={(e) =>
                          setAppForm((prev) => ({
                            ...prev,
                            manifestsYaml: e.target.value,
                          }))
                        }
                        placeholder={DEFAULT_STARTER_MANIFEST}
                        className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 placeholder-slate-500 focus:outline-none focus:ring-1 focus:ring-indigo-500 font-mono text-[11px] leading-relaxed"
                      />
                    </div>
                  </div>
                )}

                {appForm.runtime !== "manifest" && (
                  <>
                    <div>
                      <label
                        htmlFor="app-repourl"
                        className="block text-xs text-slate-300 font-medium mb-1"
                      >
                        Repository URL *
                      </label>
                      <input
                        id="app-repourl"
                        type="text"
                        required
                        placeholder={
                          appForm.runtime === "kustomize"
                            ? "https://github.com/stefanprodan/podinfo"
                            : "https://github.com/stefanprodan/podinfo or chart repo URL"
                        }
                        value={appForm.repoUrl || ""}
                        onChange={(e) =>
                          setAppForm((prev) => ({
                            ...prev,
                            repoUrl: e.target.value,
                          }))
                        }
                        className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 placeholder-slate-500 focus:outline-none focus:ring-1 focus:ring-indigo-500 font-mono"
                      />
                    </div>

                    {appForm.runtime === "helm" ? (
                      <div className="grid grid-cols-1 sm:grid-cols-2 gap-4">
                        <div>
                          <label
                            htmlFor="app-chart"
                            className="block text-xs text-slate-300 font-medium mb-1"
                          >
                            Chart Name *
                          </label>
                          <input
                            id="app-chart"
                            type="text"
                            placeholder="podinfo"
                            value={appForm.chart || ""}
                            onChange={(e) =>
                              setAppForm((prev) => ({
                                ...prev,
                                chart: e.target.value,
                              }))
                            }
                            className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 placeholder-slate-500 focus:outline-none focus:ring-1 focus:ring-indigo-500 font-mono"
                          />
                        </div>

                        <div>
                          <label
                            htmlFor="app-version"
                            className="block text-xs text-slate-300 font-medium mb-1"
                          >
                            Chart Version
                          </label>
                          <input
                            id="app-version"
                            type="text"
                            placeholder="6.7.0"
                            value={appForm.version || ""}
                            onChange={(e) =>
                              setAppForm((prev) => ({
                                ...prev,
                                version: e.target.value,
                              }))
                            }
                            className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 placeholder-slate-500 focus:outline-none focus:ring-1 focus:ring-indigo-500 font-mono"
                          />
                        </div>
                      </div>
                    ) : (
                      <div className="grid grid-cols-1 sm:grid-cols-2 gap-4">
                        <div>
                          <label
                            htmlFor="app-path"
                            className="block text-xs text-slate-300 font-medium mb-1"
                          >
                            Path
                          </label>
                          <input
                            id="app-path"
                            type="text"
                            placeholder="./base"
                            value={appForm.path || ""}
                            onChange={(e) =>
                              setAppForm((prev) => ({
                                ...prev,
                                path: e.target.value,
                              }))
                            }
                            className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 placeholder-slate-500 focus:outline-none focus:ring-1 focus:ring-indigo-500 font-mono"
                          />
                        </div>

                        <div>
                          <label
                            htmlFor="app-version"
                            className="block text-xs text-slate-300 font-medium mb-1"
                          >
                            Version / Git Ref
                          </label>
                          <input
                            id="app-version"
                            type="text"
                            placeholder="v6.7.0"
                            value={appForm.version || ""}
                            onChange={(e) =>
                              setAppForm((prev) => ({
                                ...prev,
                                version: e.target.value,
                              }))
                            }
                            className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 placeholder-slate-500 focus:outline-none focus:ring-1 focus:ring-indigo-500 font-mono"
                          />
                        </div>
                      </div>
                    )}
                  </>
                )}

                <div className="flex items-center space-x-2 pt-1">
                  <input
                    id="app-flagger"
                    type="checkbox"
                    checked={!!appForm.flaggerSupported}
                    onChange={(e) =>
                      setAppForm((prev) => ({
                        ...prev,
                        flaggerSupported: e.target.checked,
                      }))
                    }
                    className="h-4 w-4 rounded border-slate-800 bg-slate-950 text-indigo-600 focus:ring-indigo-500"
                  />
                  <label
                    htmlFor="app-flagger"
                    className="text-xs text-slate-300 font-medium cursor-pointer"
                  >
                    Flagger Canary Support Enabled
                  </label>
                </div>

                <div>
                  <label
                    htmlFor="app-description"
                    className="block text-xs text-slate-300 font-medium mb-1"
                  >
                    Description
                  </label>
                  <input
                    id="app-description"
                    type="text"
                    placeholder="Optional summary..."
                    value={appForm.description || ""}
                    onChange={(e) =>
                      setAppForm((prev) => ({
                        ...prev,
                        description: e.target.value,
                      }))
                    }
                    className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 placeholder-slate-500 focus:outline-none focus:ring-1 focus:ring-indigo-500"
                  />
                </div>
              </>
            )}

            {appForm.kind === "infra" && (
              <>
                <div className="grid grid-cols-1 sm:grid-cols-2 gap-4">
                  <div>
                    <label
                      htmlFor="infra-resource-kind"
                      className="block text-xs text-slate-300 font-medium mb-1"
                    >
                      Infrastructure Resource Kind *
                    </label>
                    <select
                      id="infra-resource-kind"
                      data-testid="infra-resource-kind-select"
                      value={appForm.resourceKind || "cluster"}
                      onChange={(e) =>
                        setAppForm((prev) => ({
                          ...prev,
                          resourceKind: e.target
                            .value as import("../types/api").InfraResourceKind,
                        }))
                      }
                      className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 focus:outline-none focus:ring-1 focus:ring-indigo-500"
                    >
                      <option value="vm">Virtual Machine (SSH Enabled)</option>
                      <option value="cluster">
                        Kubernetes Cluster (App Deployment Target)
                      </option>
                      <option value="service">Managed Service / Config</option>
                    </select>
                  </div>

                  <div>
                    <label
                      htmlFor="app-category"
                      className="block text-xs text-slate-300 font-medium mb-1"
                    >
                      Category *
                    </label>
                    <input
                      id="app-category"
                      type="text"
                      required
                      placeholder="Infrastructure"
                      value={appForm.category}
                      onChange={(e) =>
                        setAppForm((prev) => ({
                          ...prev,
                          category: e.target.value,
                        }))
                      }
                      className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 placeholder-slate-500 focus:outline-none focus:ring-1 focus:ring-indigo-500"
                    />
                  </div>

                  <div>
                    <label
                      htmlFor="infra-provider"
                      className="block text-xs text-slate-300 font-medium mb-1"
                    >
                      Provider *
                    </label>
                    <select
                      id="infra-provider"
                      value={appForm.provider || "kind"}
                      onChange={(e) =>
                        setAppForm((prev) => ({
                          ...prev,
                          provider: e.target.value,
                        }))
                      }
                      className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 focus:outline-none focus:ring-1 focus:ring-indigo-500"
                    >
                      <option value="kind">kind</option>
                      <option value="aws">aws</option>
                      <option value="proxmox">proxmox</option>
                      <option value="gcp">gcp</option>
                      <option value="azure">azure</option>
                      <option value="hetzner">hetzner</option>
                      <option value="cloudflare">cloudflare</option>
                      <option value="custom">custom</option>
                    </select>
                  </div>
                </div>

                <div className="grid grid-cols-1 sm:grid-cols-2 gap-4">
                  <div>
                    <label
                      htmlFor="infra-pulumi-runtime"
                      className="block text-xs text-slate-300 font-medium mb-1"
                    >
                      Pulumi Runtime *
                    </label>
                    <select
                      id="infra-pulumi-runtime"
                      value={appForm.pulumiRuntime || "yaml"}
                      onChange={(e) =>
                        setAppForm((prev) => ({
                          ...prev,
                          pulumiRuntime: e.target.value as
                            "yaml" | "hcl" | "opentofu-bridge",
                        }))
                      }
                      className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 focus:outline-none focus:ring-1 focus:ring-indigo-500"
                    >
                      <option value="yaml">yaml</option>
                      <option value="hcl">hcl</option>
                      <option value="opentofu-bridge">opentofu-bridge</option>
                    </select>
                  </div>

                  <div>
                    <label
                      htmlFor="infra-entrypoint"
                      className="block text-xs text-slate-300 font-medium mb-1"
                    >
                      Entrypoint
                    </label>
                    <input
                      id="infra-entrypoint"
                      type="text"
                      placeholder="Pulumi.yaml"
                      value={appForm.entrypoint ?? "Pulumi.yaml"}
                      onChange={(e) =>
                        setAppForm((prev) => ({
                          ...prev,
                          entrypoint: e.target.value,
                        }))
                      }
                      className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 placeholder-slate-500 focus:outline-none focus:ring-1 focus:ring-indigo-500 font-mono"
                    />
                  </div>
                </div>

                <div>
                  <label
                    htmlFor="infra-tf-provider"
                    className="block text-xs text-slate-300 font-medium mb-1"
                  >
                    OpenTofu / Terraform Provider
                  </label>
                  <input
                    id="infra-tf-provider"
                    type="text"
                    placeholder="e.g. tehcyx/kind, hashicorp/aws, bpg/proxmox"
                    value={appForm.terraformProvider || ""}
                    onChange={(e) =>
                      setAppForm((prev) => ({
                        ...prev,
                        terraformProvider: e.target.value,
                      }))
                    }
                    className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 placeholder-slate-500 focus:outline-none focus:ring-1 focus:ring-indigo-500 font-mono"
                  />
                </div>

                <div className="flex items-center space-x-2 pt-1">
                  <input
                    id="infra-protect"
                    type="checkbox"
                    checked={!!appForm.protectByDefault}
                    onChange={(e) =>
                      setAppForm((prev) => ({
                        ...prev,
                        protectByDefault: e.target.checked,
                      }))
                    }
                    className="h-4 w-4 rounded border-slate-800 bg-slate-950 text-indigo-600 focus:ring-indigo-500"
                  />
                  <label
                    htmlFor="infra-protect"
                    className="text-xs text-slate-300 font-medium cursor-pointer"
                  >
                    Protect by Default
                  </label>
                </div>

                <div>
                  <label
                    htmlFor="infra-description"
                    className="block text-xs text-slate-300 font-medium mb-1"
                  >
                    Description
                  </label>
                  <input
                    id="infra-description"
                    type="text"
                    placeholder="Optional summary..."
                    value={appForm.description || ""}
                    onChange={(e) =>
                      setAppForm((prev) => ({
                        ...prev,
                        description: e.target.value,
                      }))
                    }
                    className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 placeholder-slate-500 focus:outline-none focus:ring-1 focus:ring-indigo-500"
                  />
                </div>

                <div>
                  <div className="flex items-center justify-between mb-1">
                    <label
                      htmlFor="infra-pulumi-yaml"
                      className="block text-xs text-slate-300 font-medium"
                    >
                      Pulumi.yaml Program
                    </label>
                    <button
                      type="button"
                      onClick={() => {
                        const bpId = appForm.id.trim() || "my-blueprint"
                        const bpName = appForm.name.trim() || bpId
                        const runtime = appForm.pulumiRuntime || "yaml"
                        const provider = appForm.provider || "kind"
                        const tf = appForm.terraformProvider?.trim()
                        const pkgBlock = tf
                          ? `packages:\n  ${provider}:\n    source: terraform-provider\n    parameters:\n      - ${tf}\n`
                          : ""
                        const starter = `name: ${bpId}\nruntime: ${runtime}\ndescription: ${bpName} (${provider})\n${pkgBlock}config:\n  name:\n    type: string\n    default: workload-1\nresources: {}\noutputs: {}\n`
                        setAppForm((prev) => ({
                          ...prev,
                          pulumiYaml: starter,
                        }))
                      }}
                      className="text-[11px] text-indigo-400 hover:text-indigo-300 transition-colors cursor-pointer"
                    >
                      Insert Starter Template
                    </button>
                  </div>
                  <textarea
                    id="infra-pulumi-yaml"
                    rows={10}
                    spellCheck={false}
                    placeholder={
                      editingCatalogItem
                        ? "Leave blank to preserve existing Pulumi.yaml, or paste/edit YAML..."
                        : "Optional — leave blank to auto-generate a starter Pulumi.yaml"
                    }
                    value={appForm.pulumiYaml || ""}
                    onChange={(e) =>
                      setAppForm((prev) => ({
                        ...prev,
                        pulumiYaml: e.target.value,
                      }))
                    }
                    className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 placeholder-slate-500 focus:outline-none focus:ring-1 focus:ring-indigo-500 font-mono leading-relaxed"
                  />
                </div>
              </>
            )}

            {appForm.kind === "bundle" && (
              <div className="space-y-3">
                <div className="grid grid-cols-1 sm:grid-cols-2 gap-4">
                  <div>
                    <label
                      htmlFor="bundle-category"
                      className="block text-xs text-slate-300 font-medium mb-1"
                    >
                      Category *
                    </label>
                    <input
                      id="bundle-category"
                      type="text"
                      required
                      value={appForm.category}
                      onChange={(e) =>
                        setAppForm((prev) => ({
                          ...prev,
                          category: e.target.value,
                        }))
                      }
                      className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 focus:outline-none focus:ring-1 focus:ring-indigo-500"
                    />
                  </div>
                  <div>
                    <label
                      htmlFor="bundle-description"
                      className="block text-xs text-slate-300 font-medium mb-1"
                    >
                      Description
                    </label>
                    <input
                      id="bundle-description"
                      type="text"
                      placeholder="Optional summary..."
                      value={appForm.description || ""}
                      onChange={(e) =>
                        setAppForm((prev) => ({
                          ...prev,
                          description: e.target.value,
                        }))
                      }
                      className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-100 placeholder-slate-500 focus:outline-none focus:ring-1 focus:ring-indigo-500"
                    />
                  </div>
                </div>

                <div className="space-y-2">
                  <div className="flex items-center justify-between">
                    <span className="text-xs font-medium text-slate-300">
                      Bundle Items &amp; Dependency Chain
                    </span>
                    <button
                      type="button"
                      onClick={() =>
                        setAppForm((prev) => {
                          const existing = prev.items ?? []
                          const nextIdx = existing.length + 1
                          return {
                            ...prev,
                            items: [
                              ...existing,
                              {
                                id: `step-${nextIdx}`,
                                kind: "app",
                                catalogRef:
                                  catalog.find((c) => c.type === "app")?.id ??
                                  "",
                                dependsOn: existing[0] ? [existing[0].id] : [],
                              },
                            ],
                          }
                        })
                      }
                      className="px-2.5 py-1 bg-slate-800 hover:bg-slate-700 text-indigo-300 rounded-lg text-[11px] font-medium flex items-center gap-1"
                    >
                      <Plus className="h-3 w-3" />
                      <span>Add Step</span>
                    </button>
                  </div>

                  <div className="space-y-2.5 max-h-60 overflow-y-auto pr-1">
                    {(appForm.items ?? []).map((item, idx) => {
                      const availableRefs = catalog.filter((c) =>
                        item.kind === "infra"
                          ? c.type === "infra" || c.type === "infrastructure"
                          : c.type === "app"
                      )
                      const selectedApp =
                        item.kind === "app"
                          ? catalog.find(
                              (c) =>
                                c.type === "app" && c.id === item.catalogRef
                            )
                          : undefined
                      const priorItems = (appForm.items ?? []).filter(
                        (_, i) => i !== idx
                      )
                      const infraDeps = priorItems.filter(
                        (p) =>
                          p.kind === "infra" &&
                          (item.dependsOn ?? []).includes(p.id)
                      )
                      const updateItem = (
                        patch: Partial<CatalogBundleItem>
                      ) => {
                        setAppForm((prev) => ({
                          ...prev,
                          items: (prev.items ?? []).map((it, i) =>
                            i === idx ? { ...it, ...patch } : it
                          ),
                        }))
                      }
                      return (
                        <div
                          key={idx}
                          className="p-3 bg-slate-950 border border-slate-800 rounded-xl space-y-2"
                        >
                          <div className="grid grid-cols-12 gap-2 items-center">
                            <input
                              type="text"
                              aria-label={`Step ${idx + 1} ID`}
                              placeholder="step id"
                              value={item.id}
                              onChange={(e) =>
                                updateItem({ id: e.target.value })
                              }
                              className="col-span-3 px-2 py-1.5 bg-slate-900 border border-slate-800 rounded-lg text-[11px] font-mono text-slate-100"
                            />
                            <select
                              aria-label={`Step ${idx + 1} Kind`}
                              value={item.kind}
                              onChange={(e) => {
                                const nextKind = e.target.value as
                                  "infra" | "app"
                                const firstRef = catalog.find((c) =>
                                  nextKind === "infra"
                                    ? c.type === "infra" ||
                                      c.type === "infrastructure"
                                    : c.type === "app"
                                )?.id
                                updateItem({
                                  kind: nextKind,
                                  catalogRef: firstRef ?? "",
                                  targetClusterFrom: undefined,
                                  targetNamespace: undefined,
                                  canary: undefined,
                                })
                              }}
                              className="col-span-3 px-2 py-1.5 bg-slate-900 border border-slate-800 rounded-lg text-[11px] text-slate-100"
                            >
                              <option value="infra">infra</option>
                              <option value="app">app</option>
                            </select>
                            <select
                              aria-label={`Step ${idx + 1} Catalog Ref`}
                              value={item.catalogRef}
                              onChange={(e) => {
                                const nextRef = e.target.value
                                const nextApp = catalog.find(
                                  (c) => c.type === "app" && c.id === nextRef
                                )
                                updateItem({
                                  catalogRef: nextRef,
                                  canary: nextApp?.flaggerSupported
                                    ? item.canary
                                    : undefined,
                                })
                              }}
                              className="col-span-5 px-2 py-1.5 bg-slate-900 border border-slate-800 rounded-lg text-[11px] font-mono text-slate-100"
                            >
                              <option value="">Select item...</option>
                              {availableRefs.map((c) => (
                                <option key={c.id} value={c.id}>
                                  {c.id} ({c.name})
                                </option>
                              ))}
                            </select>
                            <button
                              type="button"
                              aria-label={`Remove step ${idx + 1}`}
                              onClick={() =>
                                setAppForm((prev) => ({
                                  ...prev,
                                  items: (prev.items ?? []).filter(
                                    (_, i) => i !== idx
                                  ),
                                }))
                              }
                              className="col-span-1 p-1 text-slate-400 hover:text-red-400 flex justify-center"
                            >
                              <Trash2 className="h-3.5 w-3.5" />
                            </button>
                          </div>

                          {priorItems.length > 0 && (
                            <div className="flex flex-wrap items-center gap-2 text-[11px] text-slate-400">
                              <span>Depends on:</span>
                              {priorItems.map((p) => {
                                const checked = (item.dependsOn ?? []).includes(
                                  p.id
                                )
                                return (
                                  <label
                                    key={p.id}
                                    className="inline-flex items-center gap-1 cursor-pointer font-mono text-slate-300"
                                  >
                                    <input
                                      type="checkbox"
                                      checked={checked}
                                      onChange={(e) => {
                                        const current = item.dependsOn ?? []
                                        const next = e.target.checked
                                          ? [...current, p.id]
                                          : current.filter((d) => d !== p.id)
                                        updateItem({
                                          dependsOn: next,
                                          targetClusterFrom:
                                            item.targetClusterFrom &&
                                            !next.includes(
                                              item.targetClusterFrom
                                            )
                                              ? undefined
                                              : item.targetClusterFrom,
                                        })
                                      }}
                                      className="rounded border-slate-700 bg-slate-900 text-indigo-500"
                                    />
                                    <span>{p.id}</span>
                                  </label>
                                )
                              })}
                            </div>
                          )}

                          {item.kind === "app" && (
                            <div className="space-y-2">
                              <div className="grid grid-cols-2 gap-2">
                                <select
                                  aria-label={`Step ${idx + 1} Target Cluster From`}
                                  value={item.targetClusterFrom || ""}
                                  onChange={(e) =>
                                    updateItem({
                                      targetClusterFrom:
                                        e.target.value || undefined,
                                    })
                                  }
                                  className="px-2 py-1 bg-slate-900 border border-slate-800 rounded-lg text-[11px] font-mono text-slate-200"
                                >
                                  <option value="">
                                    Local / Tenant Cluster
                                  </option>
                                  {infraDeps.map((inf) => (
                                    <option key={inf.id} value={inf.id}>
                                      From infra: {inf.id}
                                    </option>
                                  ))}
                                </select>
                                <input
                                  type="text"
                                  aria-label={`Step ${idx + 1} Target Namespace`}
                                  placeholder="Target namespace (optional)"
                                  value={item.targetNamespace || ""}
                                  onChange={(e) =>
                                    updateItem({
                                      targetNamespace:
                                        e.target.value || undefined,
                                    })
                                  }
                                  className="px-2 py-1 bg-slate-900 border border-slate-800 rounded-lg text-[11px] font-mono text-slate-200"
                                />
                              </div>
                              {selectedApp?.flaggerSupported && (
                                <label className="inline-flex items-center gap-2 text-[11px] text-slate-300 cursor-pointer select-none">
                                  <input
                                    type="checkbox"
                                    aria-label={`Step ${idx + 1} Enable Flagger Canary`}
                                    checked={!!item.canary}
                                    onChange={(e) =>
                                      updateItem({
                                        canary: e.target.checked || undefined,
                                      })
                                    }
                                    className="h-3.5 w-3.5 rounded border-slate-700 bg-slate-900 text-indigo-500 focus:ring-indigo-500 cursor-pointer"
                                  />
                                  <span>Enable Flagger Canary</span>
                                </label>
                              )}
                            </div>
                          )}
                        </div>
                      )
                    })}
                  </div>
                </div>
              </div>
            )}

            <div className="pt-1 border-t border-slate-800/80">
              <div className="flex flex-wrap items-center justify-between gap-2">
                <button
                  type="button"
                  onClick={() => {
                    if (!showSchemaEditor && !schemaJsonText) {
                      const empty = JSON.stringify(
                        { type: "object", properties: {} },
                        null,
                        2
                      )
                      setSchemaJsonText(empty)
                      setSchemaFields([])
                    }
                    setShowSchemaEditor((prev) => !prev)
                  }}
                  className="text-[11px] font-medium text-indigo-400 hover:text-indigo-300 transition-colors cursor-pointer"
                >
                  {showSchemaEditor
                    ? "Hide JSON Schema Editor"
                    : "Edit JSON Schema (Optional)"}
                </button>
                {appForm.kind === "infra" && (
                  <div className="flex flex-wrap items-center gap-2">
                    {appForm.pulumiYaml?.trim() && (
                      <button
                        type="button"
                        onClick={() => {
                          const derived = deriveFieldsFromPulumiYaml(
                            appForm.pulumiYaml || ""
                          )
                          const existingSecrets = schemaFields.filter(
                            (f) =>
                              f.secret &&
                              !derived.some((d) => d.name === f.name)
                          )
                          const next = [...derived, ...existingSecrets]
                          setSchemaFields(next)
                          setSchemaJsonText(
                            fieldsToSchemaJson(next, schemaJsonText)
                          )
                          setShowSchemaEditor(true)
                        }}
                        className="text-[11px] font-medium text-emerald-400 hover:text-emerald-300 transition-colors cursor-pointer"
                      >
                        Derive from Pulumi.yaml
                      </button>
                    )}
                    {PROVIDER_CREDENTIAL_PRESETS[appForm.provider || ""] && (
                      <button
                        type="button"
                        onClick={() => {
                          const presets =
                            PROVIDER_CREDENTIAL_PRESETS[
                              appForm.provider || ""
                            ] ?? []
                          const existingNames = new Set(
                            schemaFields.map((f) => f.name)
                          )
                          const toAdd = presets.filter(
                            (p) => !existingNames.has(p.name)
                          )
                          const next = [...schemaFields, ...toAdd]
                          setSchemaFields(next)
                          setSchemaJsonText(
                            fieldsToSchemaJson(next, schemaJsonText)
                          )
                          setShowSchemaEditor(true)
                        }}
                        className="text-[11px] font-medium text-amber-400 hover:text-amber-300 transition-colors cursor-pointer"
                      >
                        + Add {appForm.provider} Credentials
                      </button>
                    )}
                  </div>
                )}
                {appForm.kind === "app" && appForm.runtime === "manifest" && (
                  <div className="flex flex-wrap items-center gap-2">
                    <button
                      type="button"
                      onClick={onExtractManifestVars}
                      className="text-[11px] font-medium text-emerald-400 hover:text-emerald-300 transition-colors cursor-pointer"
                    >
                      Auto-Extract `${`var`}` Fields
                    </button>
                  </div>
                )}
              </div>
              {showSchemaEditor && (
                <div className="mt-2.5 space-y-3">
                  <div className="space-y-2">
                    <div className="flex items-center justify-between">
                      <span className="text-xs text-slate-300 font-medium">
                        Form Fields
                      </span>
                      <button
                        type="button"
                        onClick={() => {
                          const next: SchemaFieldDraft[] = [
                            ...schemaFields,
                            {
                              name: "",
                              title: "",
                              type: "string",
                              defaultValue: "",
                              required: false,
                              secret: false,
                            },
                          ]
                          setSchemaFields(next)
                          setSchemaJsonText(
                            fieldsToSchemaJson(next, schemaJsonText)
                          )
                        }}
                        className="text-[11px] font-medium text-indigo-400 hover:text-indigo-300 cursor-pointer"
                      >
                        + Add Field
                      </button>
                    </div>
                    {schemaFields.length > 0 && (
                      <div className="max-h-80 overflow-y-auto pr-1">
                        <SchemaFieldsEditor
                          fields={schemaFields}
                          onChange={(next) => {
                            setSchemaFields(next)
                            setSchemaJsonText(
                              fieldsToSchemaJson(next, schemaJsonText)
                            )
                          }}
                        />
                      </div>
                    )}
                  </div>
                  <div>
                    <label
                      htmlFor="catalog-schema-json"
                      className="block text-xs text-slate-300 font-medium mb-1"
                    >
                      JSON Schema
                    </label>
                    <textarea
                      id="catalog-schema-json"
                      rows={6}
                      value={schemaJsonText}
                      onChange={(e) => {
                        const text = e.target.value
                        setSchemaJsonText(text)
                        const parsed = schemaJsonToFields(text)
                        if (parsed) setSchemaFields(parsed)
                      }}
                      className="w-full px-3 py-2 bg-slate-950 border border-slate-800 rounded-lg text-[11px] text-slate-100 font-mono focus:outline-none focus:ring-1 focus:ring-indigo-500"
                    />
                  </div>
                </div>
              )}
            </div>

            {appError && (
              <div className="p-3 bg-red-950/40 border border-red-500/30 rounded-lg text-red-300 text-xs flex items-center gap-2">
                <AlertCircle className="h-4 w-4 text-red-400 shrink-0" />
                <span>{appError}</span>
              </div>
            )}
          </div>

          <div className="flex items-center justify-end gap-3 pt-4 border-t border-slate-800 shrink-0 mt-4">
            <button
              type="button"
              onClick={onCancel}
              className="px-4 py-2 bg-slate-800 hover:bg-slate-700 text-slate-300 text-xs font-medium rounded-lg transition-colors"
            >
              Cancel
            </button>
            <button
              type="submit"
              disabled={
                appLoading ||
                !appForm.id.trim() ||
                !appForm.name.trim() ||
                (appForm.kind !== "infra" &&
                  appForm.kind !== "bundle" &&
                  appForm.runtime !== "manifest" &&
                  !appForm.repoUrl?.trim()) ||
                (appForm.kind === "bundle" &&
                  (!appForm.items ||
                    appForm.items.length === 0 ||
                    appForm.items.some(
                      (it) => !it.id.trim() || !it.catalogRef.trim()
                    )))
              }
              className="px-4 py-2 bg-indigo-600 hover:bg-indigo-500 text-white text-xs font-semibold rounded-lg transition-colors disabled:opacity-50 flex items-center gap-1.5"
            >
              {appLoading ? (
                <>
                  <Loader2 className="h-3.5 w-3.5 animate-spin" />
                  <span>Opening PR...</span>
                </>
              ) : (
                <span>
                  {editingCatalogItem
                    ? "Submit Update PR"
                    : appForm.kind === "infra"
                      ? "Submit Infra PR Proposal"
                      : appForm.kind === "bundle"
                        ? "Submit Bundle PR Proposal"
                        : "Submit App PR Proposal"}
                </span>
              )}
            </button>
          </div>
        </form>
      </div>
    </div>
  )
}
