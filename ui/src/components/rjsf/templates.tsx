import React from "react"
import {
  DescriptionFieldProps,
  ErrorListProps,
  errorId,
  FieldErrorProps,
  FieldTemplateProps,
  FormContextType,
  getUiOptions,
  IconButtonProps,
  RJSFSchema,
  StrictRJSFSchema,
} from "@rjsf/utils"
import {
  AlertCircle,
  AlertTriangle,
  ArrowDown,
  ArrowUp,
  Copy,
  Plus,
  Trash2,
} from "lucide-react"

import { Button } from "@/components/ui/button"
import { Checkbox } from "@/components/ui/checkbox"
import { Label } from "@/components/ui/label"
import {
  HEURISTIC_SECRET_UI_OPTION,
  type HeuristicSecretField,
} from "@/lib/secretFields"

type Values = Record<string, unknown>

/** Field wrapper: label (bound to the input), description, control, inline errors, help. */
export function ShadcnFieldTemplate<
  T = Values,
  S extends StrictRJSFSchema = RJSFSchema,
  F extends FormContextType = Record<string, unknown>,
>(props: FieldTemplateProps<T, S, F>) {
  const {
    id,
    label,
    children,
    errors,
    help,
    description,
    hidden,
    required,
    displayLabel,
    registry,
    uiSchema,
    classNames,
    style,
  } = props
  const formContext = registry.formContext
  const uiOptions = getUiOptions<T, S, F>(uiSchema)
  const heuristic = uiOptions[HEURISTIC_SECRET_UI_OPTION] as
    HeuristicSecretField | undefined
  const WrapIfAdditionalTemplate = registry.templates.WrapIfAdditionalTemplate
  if (hidden) {
    return <div className="hidden">{children}</div>
  }
  const isCheckbox = uiOptions.widget === "checkbox"
  return (
    <WrapIfAdditionalTemplate {...props}>
      <div
        className={`form-group flex flex-col gap-1.5 ${classNames ?? ""}`}
        style={style}
      >
        {displayLabel && !isCheckbox && label && (
          <Label htmlFor={id} className="control-label">
            {label}
            {required && (
              <span className="required ml-1 text-red-400" aria-hidden="true">
                *
              </span>
            )}
          </Label>
        )}
        {displayLabel && description ? description : null}
        {children}
        {heuristic && (
          <HeuristicSecretNotice
            id={id}
            field={heuristic}
            onToggle={
              (
                formContext as
                  { onToggleNotSecret?: ToggleNotSecret } | undefined
              )?.onToggleNotSecret
            }
          />
        )}
        {errors}
        {help}
      </div>
    </WrapIfAdditionalTemplate>
  )
}

type ToggleNotSecret = (path: string, notSecret: boolean) => void

/**
 * Inline notice under a field the name heuristic treats as secret (AST-2): it is stored
 * encrypted with SOPS unless the user ticks "Not a secret" (then stored in plaintext in Git).
 */
function HeuristicSecretNotice({
  id,
  field,
  onToggle,
}: {
  id: string
  field: HeuristicSecretField
  onToggle?: ToggleNotSecret
}) {
  const toggleId = `${id}__not-secret`
  return (
    <div
      role="note"
      data-testid="heuristic-secret-notice"
      data-path={field.path}
      className={`flex flex-col sm:flex-row sm:items-center sm:justify-between gap-2 rounded-lg border px-2.5 py-2 text-[11px] ${
        field.notSecret && !field.reenter
          ? "border-slate-700 bg-slate-900/60 text-slate-300"
          : "border-amber-500/30 bg-amber-950/30 text-amber-200"
      }`}
    >
      <span className="flex items-start gap-1.5">
        <AlertTriangle
          className={`h-3.5 w-3.5 shrink-0 mt-px ${field.notSecret && !field.reenter ? "text-slate-400" : "text-amber-400"}`}
          aria-hidden="true"
        />
        {field.notSecret && field.reenter ? (
          <span data-testid="heuristic-secret-reenter">
            Marked not secret: re-enter the value. The stored secret is not
            shown and is not kept once this is stored in plaintext.
          </span>
        ) : field.notSecret ? (
          <span>
            Marked not secret: stored in plaintext in Git (
            <code className="font-mono">values.yaml</code>).
          </span>
        ) : (
          <span>
            Looks like a secret: stored encrypted with SOPS, never in plaintext
            Git.
          </span>
        )}
      </span>
      {onToggle && (
        <span className="flex items-center gap-1.5 shrink-0">
          <Checkbox
            id={toggleId}
            checked={field.notSecret}
            onCheckedChange={(checked) =>
              onToggle(field.path, checked === true)
            }
            aria-label={`Not a secret: ${field.path}`}
          />
          <Label htmlFor={toggleId} className="text-[11px] cursor-pointer">
            Not a secret
          </Label>
        </span>
      )}
    </div>
  )
}

export function ShadcnFieldErrorTemplate<
  T = Values,
  S extends StrictRJSFSchema = RJSFSchema,
  F extends FormContextType = Record<string, unknown>,
>({ errors = [], fieldPathId }: FieldErrorProps<T, S, F>) {
  const visible = errors.filter(Boolean)
  if (visible.length === 0) return null
  return (
    <ul
      id={errorId(fieldPathId)}
      className="error-detail space-y-0.5 text-[11px] text-red-400"
      aria-live="polite"
    >
      {visible.map((err, i) => (
        <li key={i} className="flex items-start gap-1">
          <AlertCircle className="h-3 w-3 mt-0.5 shrink-0" aria-hidden="true" />
          <span>{err}</span>
        </li>
      ))}
    </ul>
  )
}

export function ShadcnDescriptionField<
  T = Values,
  S extends StrictRJSFSchema = RJSFSchema,
  F extends FormContextType = Record<string, unknown>,
>({ id, description }: DescriptionFieldProps<T, S, F>) {
  if (!description) return null
  return (
    <div id={id} className="field-description text-[11px] text-slate-400">
      {description}
    </div>
  )
}

/** Top-of-form summary of validation problems (each entry names the field). */
export function ShadcnErrorList<
  T = Values,
  S extends StrictRJSFSchema = RJSFSchema,
  F extends FormContextType = Record<string, unknown>,
>({ errors }: ErrorListProps<T, S, F>) {
  return (
    <div
      role="alert"
      className="panel-danger mb-4 rounded-xl border border-red-500/35 bg-red-950/25 px-4 py-3 text-xs text-red-300"
    >
      <p className="mb-1 font-semibold">Please fix the following:</p>
      <ul className="list-disc space-y-0.5 pl-4">
        {errors.map((err, i) => (
          <li key={i}>{err.stack}</li>
        ))}
      </ul>
    </div>
  )
}

function IconButton({
  title,
  glyph: Glyph,
  variant = "secondary",
  onClick,
  disabled,
  className,
}: Omit<IconButtonProps, "icon"> & {
  title: string
  glyph: React.ComponentType<{ className?: string }>
  variant?: "secondary" | "destructive-subtle"
}) {
  return (
    <Button
      variant={variant}
      size="icon-sm"
      onClick={onClick}
      disabled={disabled}
      className={className}
      title={title}
      aria-label={title}
    >
      <Glyph className="h-3.5 w-3.5" />
    </Button>
  )
}

export const RjsfAddButton = ({
  onClick,
  disabled,
  className,
}: IconButtonProps) => (
  <Button
    variant="subtle"
    onClick={onClick}
    disabled={disabled}
    className={className}
    title="Add item"
  >
    <Plus className="h-3.5 w-3.5" />
    <span>Add</span>
  </Button>
)

export const RjsfRemoveButton = (p: IconButtonProps) => (
  <IconButton
    {...p}
    title="Remove item"
    glyph={Trash2}
    variant="destructive-subtle"
  />
)
export const RjsfMoveUpButton = (p: IconButtonProps) => (
  <IconButton {...p} title="Move up" glyph={ArrowUp} />
)
export const RjsfMoveDownButton = (p: IconButtonProps) => (
  <IconButton {...p} title="Move down" glyph={ArrowDown} />
)
export const RjsfCopyButton = (p: IconButtonProps) => (
  <IconButton {...p} title="Duplicate item" glyph={Copy} />
)
