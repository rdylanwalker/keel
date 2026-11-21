import { useState } from "react"
import {
  ColumnDef,
  columnFilteringFeature,
  createFilteredRowModel,
  createSortedRowModel,
  filterFns,
  flexRender,
  globalFilteringFeature,
  RowData,
  rowSortingFeature,
  sortFns,
  SortingState,
  tableFeatures,
  useTable,
} from "@tanstack/react-table"
import { ArrowDown, ArrowUp, ArrowUpDown, Search, X } from "lucide-react"

/**
 * Column `meta` understood by {@link DataTable}:
 * - `align: "right"` right-aligns header and cells
 * - `sticky: true` pins the column to the right edge (actions) so it stays visible when the
 *   table scrolls horizontally (UI-10)
 * - `grow: true` makes the column absorb all leftover width and truncate its content only when
 *   the table is genuinely out of room (`w-full max-w-0` table-cell truncation); use it for the
 *   primary name column so status/actions stay visible without horizontal scrolling
 * - `hideBelow` hides a low-priority column when the *table container* is narrower than the
 *   given size (container queries, so the sidebar/card width is accounted for — not the viewport):
 *   `md` < 42rem, `lg` < 48rem, `xl` < 56rem, `2xl` < 64rem, `3xl` < 72rem
 */
export interface DataTableColumnMeta {
  align?: "right"
  sticky?: boolean
  hideBelow?: "md" | "lg" | "xl" | "2xl" | "3xl"
  grow?: boolean
}

const HIDE_BELOW_CLASS: Record<
  NonNullable<DataTableColumnMeta["hideBelow"]>,
  string
> = {
  md: "hidden @2xl:table-cell",
  lg: "hidden @3xl:table-cell",
  xl: "hidden @4xl:table-cell",
  "2xl": "hidden @5xl:table-cell",
  "3xl": "hidden @6xl:table-cell",
}

function columnClasses(meta: unknown, kind: "th" | "td"): string {
  const m = (meta ?? {}) as DataTableColumnMeta
  const classes: string[] = []
  if (m.align === "right") classes.push("text-right")
  if (m.hideBelow) classes.push(HIDE_BELOW_CLASS[m.hideBelow])
  if (m.grow)
    classes.push(
      kind === "th"
        ? "w-full min-w-[5.5rem] @md:min-w-[7rem] @xl:min-w-[11rem] @5xl:min-w-[12rem]"
        : "w-full max-w-0 min-w-[5.5rem] @md:min-w-[7rem] @xl:min-w-[11rem] @5xl:min-w-[12rem]"
    )
  if (m.sticky) {
    classes.push(
      "sticky right-0 z-10 shadow-[-8px_0_8px_-8px_rgba(0,0,0,0.6)]",
      kind === "th" ? "bg-slate-950" : "bg-slate-900"
    )
  }
  return classes.join(" ")
}

const dataTableFeatures = tableFeatures({
  rowSortingFeature,
  columnFilteringFeature,
  globalFilteringFeature,
  filteredRowModel: createFilteredRowModel(),
  sortedRowModel: createSortedRowModel(),
  filterFns,
  sortFns,
})

export type DataTableFeatures = typeof dataTableFeatures

export type DataTableColumnDef<TData extends RowData> = ColumnDef<
  DataTableFeatures,
  TData,
  unknown
>

interface DataTableProps<TData extends RowData> {
  data: TData[]
  columns: DataTableColumnDef<TData>[]
  filterPlaceholder?: string
  emptyMessage?: string
  noResultsMessage?: string
  getRowId?: (row: TData, index: number) => string
}

export function DataTable<TData extends RowData>({
  data,
  columns,
  filterPlaceholder = "Filter...",
  emptyMessage = "No items found.",
  noResultsMessage = "No matching items.",
  getRowId,
}: DataTableProps<TData>) {
  const [sorting, setSorting] = useState<SortingState>([])
  const [globalFilter, setGlobalFilter] = useState("")

  const table = useTable<DataTableFeatures, TData>({
    features: dataTableFeatures,
    data,
    columns,
    state: {
      sorting,
      globalFilter,
    },
    onSortingChange: setSorting,
    onGlobalFilterChange: setGlobalFilter,
    getRowId,
  })

  const rows = table.getRowModel().rows

  return (
    <div className="space-y-3">
      <div className="relative max-w-sm">
        <Search className="absolute left-3 top-2.5 h-3.5 w-3.5 text-slate-500" />
        <input
          type="text"
          value={globalFilter}
          onChange={(e) => setGlobalFilter(e.target.value)}
          placeholder={filterPlaceholder}
          aria-label={filterPlaceholder}
          className="w-full pl-8 pr-8 py-1.5 bg-slate-950 border border-slate-800 rounded-lg text-xs text-slate-200 placeholder-slate-500 focus:outline-none focus:ring-1 focus:ring-indigo-500"
        />
        {globalFilter && (
          <button
            type="button"
            onClick={() => setGlobalFilter("")}
            aria-label="Clear filter"
            className="absolute right-2 top-2 p-0.5 text-slate-500 hover:text-slate-300 cursor-pointer"
          >
            <X className="h-3.5 w-3.5" />
          </button>
        )}
      </div>

      <div className="@container overflow-x-auto overscroll-x-contain">
        <table className="w-full text-left text-xs text-slate-300">
          <thead className="bg-slate-950 text-slate-400 border-b border-slate-800 uppercase font-mono">
            {table.getHeaderGroups().map((headerGroup) => (
              <tr key={headerGroup.id}>
                {headerGroup.headers.map((header) => {
                  const canSort = header.column.getCanSort()
                  const sorted = header.column.getIsSorted()
                  const alignRight =
                    (header.column.columnDef.meta as { align?: string })
                      ?.align === "right"
                  return (
                    <th
                      key={header.id}
                      aria-sort={
                        sorted === "asc"
                          ? "ascending"
                          : sorted === "desc"
                            ? "descending"
                            : canSort
                              ? "none"
                              : undefined
                      }
                      className={`py-3 px-2 @md:px-3 select-none whitespace-nowrap ${columnClasses(header.column.columnDef.meta, "th")}`}
                    >
                      {header.isPlaceholder ? null : canSort ? (
                        <button
                          type="button"
                          onClick={header.column.getToggleSortingHandler()}
                          className={`inline-flex items-center gap-1 hover:text-slate-200 transition-colors cursor-pointer uppercase ${
                            alignRight ? "justify-end w-full" : ""
                          }`}
                        >
                          <span>
                            {flexRender(
                              header.column.columnDef.header,
                              header.getContext()
                            )}
                          </span>
                          {sorted === "asc" ? (
                            <ArrowUp className="h-3 w-3 text-indigo-400" />
                          ) : sorted === "desc" ? (
                            <ArrowDown className="h-3 w-3 text-indigo-400" />
                          ) : (
                            <ArrowUpDown className="h-3 w-3 text-slate-600" />
                          )}
                        </button>
                      ) : (
                        flexRender(
                          header.column.columnDef.header,
                          header.getContext()
                        )
                      )}
                    </th>
                  )
                })}
              </tr>
            ))}
          </thead>
          <tbody className="divide-y divide-slate-800/60 font-mono">
            {rows.map((row) => (
              <tr
                key={row.id}
                className="group hover:bg-slate-800/40 transition-colors"
              >
                {row.getAllCells().map((cell) => {
                  return (
                    <td
                      key={cell.id}
                      className={`py-3 px-2 @md:px-3 whitespace-nowrap align-middle ${columnClasses(cell.column.columnDef.meta, "td")}`}
                    >
                      {flexRender(
                        cell.column.columnDef.cell,
                        cell.getContext()
                      )}
                    </td>
                  )
                })}
              </tr>
            ))}
            {rows.length === 0 && (
              <tr>
                <td
                  colSpan={columns.length}
                  className="py-8 text-center text-slate-500 font-sans"
                >
                  {data.length === 0 ? emptyMessage : noResultsMessage}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
    </div>
  )
}
