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

      <div className="overflow-x-auto">
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
                      className={`py-3 px-4 select-none ${alignRight ? "text-right" : ""}`}
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
                className="hover:bg-slate-800/40 transition-colors"
              >
                {row.getAllCells().map((cell) => {
                  const alignRight =
                    (cell.column.columnDef.meta as { align?: string })
                      ?.align === "right"
                  return (
                    <td
                      key={cell.id}
                      className={`py-3.5 px-4 ${alignRight ? "text-right" : ""}`}
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
