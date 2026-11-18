export type JsonPrimitive = string | number | boolean | null
export type JsonArray = JsonValue[]
export type JsonObject = { [key: string]: JsonValue }
export type JsonValue = JsonPrimitive | JsonObject | JsonArray

export type SqlColumnType = 'INTEGER' | 'REAL' | 'TEXT' | 'BOOLEAN'

export interface ColumnDef {
  /** Flattened SQL column name, e.g. `customer_name` or `error_retryable` */
  name: string
  /** Original nested path segments, e.g. `['customer', 'name']` */
  sourcePath: string[]
  /** Inferred SQLite column affinity */
  type: SqlColumnType
  /** Whether any record omitted or nulled this column */
  nullable: boolean
  /** Whether this column stores a serialized primitive list (e.g. `read|write`) */
  isPrimitiveArray?: boolean
}

export interface ForeignKeyDef {
  column: string
  parentTable: string
  parentColumn: string
}

export interface TableDef {
  /** Normalized SQLite table name, e.g. `orders` or `orders_items` */
  name: string
  /** JSON path from root, e.g. `['orders']` or `['orders', 'items']` */
  jsonPath: string[]
  /** Primary key column name (`id`, `orderId`, `_key`, or `_rowid`) */
  primaryKey: string
  /** Whether this table originated from a keyed object map (`Record<string, obj>`) */
  isKeyedMap: boolean
  /** Whether this table represents a singleton root/nested object */
  isSingleton: boolean
  /** Ordered list of columns */
  columns: ColumnDef[]
  /** Foreign key link to parent table if this is a 1-to-Many child table */
  foreignKey?: ForeignKeyDef
  /** Flattened row objects ready for SQLite insertion */
  rows: Record<string, string | number | null>[]
  /** Original top-level field names in discovery order (for structure queries) */
  topLevelFields: string[]
}

export interface NumericColumnSummary {
  column: string
  count: number
  nullCount: number
  sum: number
  avg: number
  min: number
  max: number
  isFloat: boolean
}

export interface CategoricalColumnSummary {
  column: string
  distinctCount: number
  /** Sorted descending by frequency: `[value, count]` */
  frequencies: [string, number][]
}

export interface TableOlapSummary {
  tableName: string
  rowCount: number
  primaryKey: string
  foreignKey?: ForeignKeyDef
  topLevelFields: string[]
  columns: { name: string; type: SqlColumnType; nullable: boolean }[]
  numericStats: NumericColumnSummary[]
  categoricalStats: CategoricalColumnSummary[]
  firstRow: Record<string, unknown> | undefined
  lastRow: Record<string, unknown> | undefined
}

export interface OlapDataCard {
  tables: TableOlapSummary[]
  /** Compact LLM-ready text representation (~150-280 tokens) */
  text: string
}

export interface SqlSliceResult {
  /** The auto-compiled SQLite query executed against the 3NF tables */
  sql: string
  /** Result rows returned by SQLite */
  rows: Record<string, unknown>[]
  /** Compact, verified LLM prompt context combining the SQL answer + schema card */
  context: string
  /** Execution time in milliseconds */
  latencyMs: number
}
