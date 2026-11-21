import React, { useCallback } from "react"
import {
  ariaDescribedByIds,
  BaseInputTemplateProps,
  descriptionId,
  enumOptionSelectedValue,
  enumOptionValueDecoder,
  enumOptionValueEncoder,
  examplesId,
  getInputProps,
  getOptionValueFormat,
  labelValue,
  RJSFSchema,
  schemaRequiresTrueValue,
  StrictRJSFSchema,
  WidgetProps,
  FormContextType,
} from "@rjsf/utils"

import { Input } from "@/components/ui/input"
import { Textarea } from "@/components/ui/textarea"
import { NativeSelect } from "@/components/ui/native-select"
import { Checkbox } from "@/components/ui/checkbox"
import { cn } from "@/lib/utils"

type Values = Record<string, unknown>

/** Shared `<input>` template (text, password, email, url, number/updown, range …) on Shadcn `Input`. */
export function ShadcnBaseInputTemplate<
  T = Values,
  S extends StrictRJSFSchema = RJSFSchema,
  F extends FormContextType = Record<string, unknown>,
>(props: BaseInputTemplateProps<T, S, F>) {
  const {
    id,
    htmlName,
    value,
    readonly,
    disabled,
    autofocus,
    onBlur,
    onFocus,
    onChange,
    onChangeOverride,
    options,
    schema,
    rawErrors,
    type,
    placeholder,
    required,
  } = props
  const inputProps = getInputProps<T, S, F>(schema, type, options)
  // Range sliders are rendered as plain numeric inputs (keyboard friendly, exact values).
  if (inputProps.type === "range") inputProps.type = "number"
  const inputValue =
    inputProps.type === "number" || inputProps.type === "integer"
      ? value || value === 0
        ? value
        : ""
      : value == null
        ? ""
        : value
  const handleChange = useCallback(
    ({ target: { value: next } }: React.ChangeEvent<HTMLInputElement>) =>
      onChange(next === "" ? options.emptyValue : next),
    [onChange, options]
  )
  const hasErrors = Boolean(rawErrors && rawErrors.length > 0)
  const examples = Array.isArray(schema.examples)
    ? (schema.examples as unknown[])
    : undefined
  return (
    <>
      <Input
        id={id}
        name={htmlName || id}
        readOnly={readonly}
        disabled={disabled}
        autoFocus={autofocus}
        required={required}
        placeholder={placeholder}
        value={inputValue as string | number}
        {...inputProps}
        list={examples ? examplesId(id) : undefined}
        onChange={onChangeOverride || handleChange}
        onBlur={({ target }) => onBlur(id, target?.value)}
        onFocus={({ target }) => onFocus(id, target?.value)}
        aria-invalid={hasErrors || undefined}
        aria-describedby={ariaDescribedByIds(id, Boolean(examples))}
      />
      {examples && (
        <datalist id={examplesId(id)}>
          {examples.map((ex) => (
            <option key={String(ex)} value={String(ex)} />
          ))}
        </datalist>
      )}
    </>
  )
}

export function ShadcnTextareaWidget<
  T = Values,
  S extends StrictRJSFSchema = RJSFSchema,
  F extends FormContextType = Record<string, unknown>,
>({
  id,
  htmlName,
  options,
  placeholder,
  value,
  required,
  disabled,
  readonly,
  autofocus = false,
  onChange,
  onBlur,
  onFocus,
  rawErrors,
}: WidgetProps<T, S, F>) {
  return (
    <Textarea
      id={id}
      name={htmlName || id}
      value={(value as string) || ""}
      placeholder={placeholder}
      required={required}
      disabled={disabled}
      readOnly={readonly}
      autoFocus={autofocus}
      rows={(options.rows as number | undefined) ?? 4}
      aria-invalid={Boolean(rawErrors?.length) || undefined}
      onBlur={({ target }) => onBlur(id, target?.value)}
      onFocus={({ target }) => onFocus(id, target?.value)}
      onChange={({ target: { value: next } }) =>
        onChange(next === "" ? options.emptyValue : next)
      }
      aria-describedby={ariaDescribedByIds(id)}
    />
  )
}

export function ShadcnSelectWidget<
  T = Values,
  S extends StrictRJSFSchema = RJSFSchema,
  F extends FormContextType = Record<string, unknown>,
>({
  schema,
  id,
  htmlName,
  options,
  value,
  required,
  disabled,
  readonly,
  multiple = false,
  autofocus = false,
  onChange,
  onBlur,
  onFocus,
  placeholder,
  rawErrors,
}: WidgetProps<T, S, F>) {
  const { enumOptions, enumDisabled, emptyValue: optEmptyVal } = options
  const optionValueFormat = getOptionValueFormat(options)
  const read = (e: React.SyntheticEvent<HTMLSelectElement>) => {
    const target = e.target as HTMLSelectElement
    const raw = multiple
      ? Array.from(target.options)
          .filter((o) => o.selected)
          .map((o) => o.value)
      : target.value
    return enumOptionValueDecoder<S>(
      raw,
      enumOptions,
      optionValueFormat,
      optEmptyVal
    )
  }
  const selectValue = enumOptionSelectedValue<S>(
    value,
    enumOptions,
    multiple,
    optionValueFormat,
    multiple ? [] : ""
  )
  const showPlaceholderOption = !multiple && schema.default === undefined
  return (
    <NativeSelect
      id={id}
      name={htmlName || id}
      multiple={multiple}
      value={selectValue as string | string[]}
      required={required}
      disabled={disabled || readonly}
      autoFocus={autofocus}
      aria-invalid={Boolean(rawErrors?.length) || undefined}
      onBlur={(e) => onBlur(id, read(e))}
      onFocus={(e) => onFocus(id, read(e))}
      onChange={(e) => onChange(read(e))}
      aria-describedby={ariaDescribedByIds(id)}
    >
      {showPlaceholderOption && (
        <option value="">{placeholder || "Select…"}</option>
      )}
      {Array.isArray(enumOptions) &&
        enumOptions.map(({ value: optValue, label }, i) => (
          <option
            key={String(optValue)}
            value={enumOptionValueEncoder(optValue, i, optionValueFormat)}
            disabled={
              Array.isArray(enumDisabled) && enumDisabled.includes(optValue)
            }
          >
            {label}
          </option>
        ))}
    </NativeSelect>
  )
}

export function ShadcnCheckboxWidget<
  T = Values,
  S extends StrictRJSFSchema = RJSFSchema,
  F extends FormContextType = Record<string, unknown>,
>({
  schema,
  options,
  id,
  value,
  disabled,
  readonly,
  label,
  hideLabel,
  autofocus = false,
  onBlur,
  onFocus,
  onChange,
  required,
}: WidgetProps<T, S, F>) {
  const trueValueRequired = schemaRequiresTrueValue<S>(schema) && required
  const description = options.description ?? schema.description
  return (
    <div
      className={cn(
        "flex flex-col gap-1",
        (disabled || readonly) && "opacity-60"
      )}
    >
      {!hideLabel && description && (
        <div
          id={descriptionId(id)}
          className="field-description text-[11px] text-slate-400"
        >
          {description}
        </div>
      )}
      <div className="flex items-center gap-2">
        <Checkbox
          id={id}
          checked={value === true}
          disabled={disabled || readonly}
          autoFocus={autofocus}
          aria-required={trueValueRequired || undefined}
          onCheckedChange={(checked) => onChange(checked === true)}
          onBlur={() => onBlur(id, value)}
          onFocus={() => onFocus(id, value)}
          aria-describedby={ariaDescribedByIds(id)}
        />
        {labelValue(
          <label
            htmlFor={id}
            className="text-xs text-slate-200 cursor-pointer select-none"
          >
            {label}
            {trueValueRequired && (
              <span className="ml-1 text-red-400" aria-hidden="true">
                *
              </span>
            )}
          </label>,
          hideLabel
        )}
      </div>
    </div>
  )
}
