import { fireEvent, render, screen, waitFor } from "@testing-library/react"
import { describe, expect, it, vi } from "vitest"

import { SchemaForm } from "./SchemaForm"

const schema = {
  type: "object" as const,
  required: ["name", "replicas", "mode"],
  properties: {
    name: { type: "string" as const, title: "Service Name", minLength: 3 },
    token: { type: "string" as const, title: "API Token", writeOnly: true },
    email: { type: "string" as const, title: "Email", format: "email" },
    url: { type: "string" as const, title: "Homepage", format: "uri" },
    notes: { type: "string" as const, title: "Notes" },
    replicas: { type: "integer" as const, title: "Replicas", minimum: 1 },
    ratio: { type: "number" as const, title: "Ratio" },
    mode: {
      type: "string" as const,
      title: "Mode",
      enum: ["fast", "safe"],
    },
    enabled: {
      type: "boolean" as const,
      title: "Enabled",
      description: "Turn it on",
    },
    tags: {
      type: "array" as const,
      title: "Tags",
      items: { type: "string" as const },
    },
  },
}

const uiSchema = {
  token: { "ui:widget": "password" },
  notes: { "ui:widget": "textarea" },
  ratio: { "ui:widget": "range" },
}

describe("SchemaForm (@rjsf/core + Shadcn widgets)", () => {
  it("renders Shadcn widgets bound to accessible labels", () => {
    render(
      <SchemaForm
        schema={schema}
        uiSchema={uiSchema}
        formData={{ tags: ["a"] }}
        onChange={vi.fn()}
        onSubmit={vi.fn()}
      />
    )
    expect(screen.getByLabelText(/Service Name/)).toHaveAttribute(
      "data-slot",
      "input"
    )
    expect(screen.getByLabelText(/API Token/)).toHaveAttribute(
      "type",
      "password"
    )
    expect(screen.getByLabelText(/Email/)).toHaveAttribute("type", "email")
    expect(screen.getByLabelText(/Homepage/)).toHaveAttribute("type", "url")
    expect(screen.getByLabelText(/Notes/)).toHaveAttribute(
      "data-slot",
      "textarea"
    )
    expect(screen.getByLabelText(/Replicas/)).toHaveAttribute("type", "number")
    expect(screen.getByLabelText(/Ratio/)).toHaveAttribute("type", "number")
    expect(screen.getByLabelText(/Mode/)).toHaveAttribute(
      "data-slot",
      "native-select"
    )
    expect(screen.getByRole("checkbox", { name: /Enabled/ })).toHaveAttribute(
      "data-slot",
      "checkbox"
    )
    expect(screen.getByText("Turn it on")).toBeInTheDocument()
    expect(screen.getByRole("button", { name: /Add/ })).toBeInTheDocument()
    expect(
      screen.getByRole("button", { name: "Remove item" })
    ).toBeInTheDocument()
  })

  it("emits changes from text, select, checkbox and textarea widgets", () => {
    const onChange = vi.fn()
    render(
      <SchemaForm
        schema={schema}
        uiSchema={uiSchema}
        formData={{}}
        onChange={onChange}
        onSubmit={vi.fn()}
      />
    )
    fireEvent.change(screen.getByLabelText(/Service Name/), {
      target: { value: "web" },
    })
    expect(onChange).toHaveBeenLastCalledWith(
      expect.objectContaining({ name: "web" })
    )
    fireEvent.change(screen.getByLabelText(/Mode/), {
      target: { value: "1" },
    })
    expect(onChange).toHaveBeenLastCalledWith(
      expect.objectContaining({ mode: "safe" })
    )
    fireEvent.click(screen.getByRole("checkbox", { name: /Enabled/ }))
    expect(onChange).toHaveBeenLastCalledWith(
      expect.objectContaining({ enabled: true })
    )
    fireEvent.change(screen.getByLabelText(/Notes/), {
      target: { value: "hello" },
    })
    expect(onChange).toHaveBeenLastCalledWith(
      expect.objectContaining({ notes: "hello" })
    )
    fireEvent.blur(screen.getByLabelText(/Notes/))
    fireEvent.focus(screen.getByLabelText(/Mode/))
    fireEvent.blur(screen.getByLabelText(/Mode/))
  })

  it("shows friendly errors next to fields and blocks submit", async () => {
    const onSubmit = vi.fn()
    render(
      <SchemaForm
        schema={schema}
        formData={{ name: "ab" }}
        onChange={vi.fn()}
        onSubmit={onSubmit}
      >
        <button type="submit">Go</button>
      </SchemaForm>
    )
    fireEvent.click(screen.getByRole("button", { name: "Go" }))
    expect(
      await screen.findByText("Please fix the following:")
    ).toBeInTheDocument()
    expect(
      screen.getAllByText("must be at least 3 characters").length
    ).toBeGreaterThan(0)
    expect(screen.getByText("Replicas is required")).toBeInTheDocument()
    expect(screen.getByLabelText(/Service Name/)).toHaveAttribute(
      "aria-invalid",
      "true"
    )
    expect(onSubmit).not.toHaveBeenCalled()
  })

  it("renders server extraErrors and runs customValidate", async () => {
    const onSubmit = vi.fn()
    render(
      <SchemaForm
        schema={{ type: "object", properties: { tag: { type: "string" } } }}
        formData={{ tag: "a#b" }}
        onChange={vi.fn()}
        onSubmit={onSubmit}
        extraErrors={{ tag: { __errors: ["rejected by server"] } }}
        customValidate={(data, errors) => {
          if (String(data?.tag).includes("#")) {
            ;(
              errors as unknown as { tag: { addError: (m: string) => void } }
            ).tag.addError("no hashes")
          }
          return errors
        }}
      >
        <button type="submit">Go</button>
      </SchemaForm>
    )
    expect(screen.getByText("rejected by server")).toBeInTheDocument()
    fireEvent.click(screen.getByRole("button", { name: "Go" }))
    await waitFor(() =>
      expect(screen.getAllByText(/no hashes/).length).toBeGreaterThan(0)
    )
    expect(onSubmit).not.toHaveBeenCalled()
  })

  it("submits valid data", async () => {
    const onSubmit = vi.fn()
    render(
      <SchemaForm
        schema={{
          type: "object",
          properties: { a: { type: "string", examples: ["x", "y"] } },
        }}
        formData={{ a: "x" }}
        onChange={vi.fn()}
        onSubmit={onSubmit}
      >
        <button type="submit">Go</button>
      </SchemaForm>
    )
    fireEvent.click(screen.getByRole("button", { name: "Go" }))
    await waitFor(() => expect(onSubmit).toHaveBeenCalledWith({ a: "x" }))
  })
})
