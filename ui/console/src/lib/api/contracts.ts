import type { Component } from "svelte";

export type ToolClass = "read-like" | "mutating";
export type StatusTone = "good" | "warn" | "muted" | "danger";
export type ConsoleRoutePath =
  | "/"
  | "/records"
  | "/retrieval"
  | "/consolidation"
  | "/audit"
  | "/mcp"
  | "/namespaces"
  | "/embedding"
  | "/security";

export interface ConsoleRoute {
  path: ConsoleRoutePath;
  label: string;
  group: "Operate" | "Configure";
  icon: Component;
}

export interface StatusTileModel {
  label: string;
  value: string;
  detail: string;
  tone: StatusTone;
  testId?: string;
}

export interface McpResourceDescriptor {
  uri: string;
  name: string;
  title?: string;
  description?: string;
  mimeType?: string;
  size?: number;
}

export interface ToolManifestStructuredContent {
  schema: "aionforge.mcp_tools.v1";
  server: {
    name: string;
    version: string;
    transports: string[];
    sampling: boolean;
    prompt_count: number;
    resource_count: number;
    recall_wrapper: string;
  };
  policy: {
    read_like_approval: string;
    mutating_approval: string;
    mutation_rule: string;
  };
  resources: Record<string, string>;
  tools: ToolManifestTool[];
}

export interface ToolManifestTool {
  name: string;
  class: "read_like" | "mutating";
  approval: string;
  mutates: boolean;
  read_only_hint: boolean;
  destructive_hint: boolean;
  idempotent_hint: boolean;
  open_world_hint: boolean;
  default_output: string;
  schema?: StructuredContentSchema;
  verbose: boolean;
  errors: string[];
}

export interface ConsoleSnapshot {
  endpoint: string;
  transport: string;
  auth: string;
  releaseBase: string;
  readLikeTools: number;
  mutatingTools: number;
  structuredContent: "pending" | "partial" | "ready";
}

export type StructuredContentSchema =
  | "aionforge.mcp_tools.v1"
  | "aionforge.server_status.v1"
  | "aionforge.consolidation_status.v1"
  | "aionforge.search_results.v1"
  | "aionforge.read_memory.v1"
  | "aionforge.session_manifest.v1"
  | "aionforge.memory_census.v1"
  | "aionforge.audit_history.v1"
  | "aionforge.work_query.v1"
  | "aionforge.work_tree.v1";

export interface ServerStatusStructuredContent {
  schema: "aionforge.server_status.v1";
  version: string;
  build: { sha: string; build_status: string; built_at: string };
  surface: {
    tools: number;
    resources: number;
    prompts: number;
    read_like_tools: string[];
    mutating_tools: string[];
  };
  transports: string[];
  sampling: boolean;
  recall_wrapper: "recalled-memory-context";
  counts: {
    memories: number;
    work_items: number;
    kinds: Record<string, number>;
    work_statuses: Record<string, number>;
  };
  auth: { enabled: boolean; issuers: string[] };
  telemetry: {
    memory_traffic: {
      bytes_in_total: number;
      bytes_out_total: number;
      estimated_tokens_in_total: number;
      estimated_tokens_out_total: number;
      token_estimate_divisor: number;
      token_estimate_kind: "coarse_bytes_divisor";
    };
  };
  resources: string[];
}

export interface ConsolidationStatusStructuredContent {
  schema: "aionforge.consolidation_status.v1";
  pending: number;
  failed: number;
  oldest_pending_age_s: number;
  generation: number;
  state: "idle" | "backlog_pending" | "attention_required";
}

export interface SearchResultsStructuredContent {
  schema: "aionforge.search_results.v1";
  summary: {
    returned: number;
    candidates_considered: number;
    filtered_or_hidden: number;
    query_class: string;
    embedder_available: boolean;
  };
  explain: {
    route: string;
    signals_run: string[];
    weights: Record<string, number>;
  };
  memories: SearchMemoryRecord[];
}

export interface SearchMemoryRecord {
  id: string;
  serialization_id: string;
  kind: "episode" | "fact" | "core";
  namespace: string;
  role?: string;
  predicate?: string;
  status?: string;
  block_kind?: string;
  score?: number;
  score_band?: "high" | "medium" | "low";
  dense_similarity?: number;
  confidence_band?: "high" | "medium" | "low";
  trust: number;
  signals: Array<{ signal: string; rank: number; weight: number }>;
  supersedes?: string;
  superseded_by?: string;
  always?: boolean;
  snippet: string;
}

export interface ReadMemoryStructuredContent {
  schema: "aionforge.read_memory.v1";
  requested: number;
  found: number;
  missing_or_unauthorized: number;
  memories: MemoryRecord[];
}

export interface SessionManifestStructuredContent {
  schema: "aionforge.session_manifest.v1";
  session_id: string;
  count: number;
  total_visible: number;
  limit: number;
  superseded_hidden: number;
  next: Cursor | null;
  episodes: MemoryRecord[];
}

export interface MemoryCensusStructuredContent {
  schema: "aionforge.memory_census.v1";
  mode: "counts" | "list";
  namespaces: MemoryCensusNamespace[];
  totals: {
    memories: number;
    work_items: number;
    kinds: Record<string, number>;
    work_statuses: Record<string, number>;
  };
  list?: {
    count: number;
    total_visible: number;
    limit: number;
    next: Cursor | null;
    memories: MemoryRecord[];
  };
}

export interface MemoryCensusNamespace {
  namespace: string;
  kinds: Record<string, number>;
  work_statuses: Record<string, number>;
  total: number;
}

export interface AuditHistoryStructuredContent {
  schema: "aionforge.audit_history.v1";
  subject: string;
  kind: string;
  count: number;
  next: AuditCursor | null;
  records: AuditRecord[];
}

export interface WorkQueryStructuredContent {
  schema: "aionforge.work_query.v1";
  filter: {
    status: string | null;
    level: string | null;
    parent: string | null;
  };
  found: number;
  items: WorkItemRecord[];
}

export interface WorkTreeStructuredContent {
  schema: "aionforge.work_tree.v1";
  root: string;
  depth: number;
  found: number;
  items: WorkItemRecord[];
}

export type ReadLikeStructuredContent =
  | ServerStatusStructuredContent
  | ConsolidationStatusStructuredContent
  | SearchResultsStructuredContent
  | ReadMemoryStructuredContent
  | SessionManifestStructuredContent
  | MemoryCensusStructuredContent
  | AuditHistoryStructuredContent
  | WorkQueryStructuredContent
  | WorkTreeStructuredContent;

export type Cursor = { ingested_at: string; id: string };
export type AuditCursor = { occurred_at: string; id: string };

export interface AuditRecord {
  id: string;
  subject_id: string;
  kind: string;
  occurred_at: string;
  actor: string;
  namespace: string;
  verification: string;
  payload_preview: string | null;
}

export interface WorkItemRecord {
  id: string;
  namespace: string;
  ingested_at: string;
  level: string;
  status: "todo" | "in_progress" | "blocked" | "done" | "dropped";
  parent: string | null;
  ordinal: number;
  title: string;
  body: string | null;
}

export type MemoryRecord =
  | EpisodeMemoryRecord
  | FactMemoryRecord
  | EntityMemoryRecord
  | NoteMemoryRecord
  | SkillMemoryRecord
  | BadPatternMemoryRecord
  | CoreMemoryRecord
  | WorkItemMemoryRecord
  | TagMemoryRecord;

export interface MemoryRecordBase {
  id: string;
  namespace: string;
  ingested_at: string;
}

export interface EpisodeMemoryRecord extends MemoryRecordBase {
  kind: "episode";
  captured_at: string;
  role: string;
  session_id: string | null;
  supersedes: string | null;
  superseded_by: string | null;
  provenance: {
    writer: string;
    model_family: string;
    model_version: string | null;
    trust_at_write: number;
    written_at: string;
  } | null;
  body: string;
  body_truncated: boolean;
}

export interface FactMemoryRecord extends MemoryRecordBase {
  kind: "fact";
  predicate: string;
  status: string;
  statement: string;
  statement_truncated: boolean;
}

export interface EntityMemoryRecord extends MemoryRecordBase {
  kind: "entity";
  entity_type: string;
  canonical_name: string;
  description: string | null;
  body: string;
  body_truncated: boolean;
}

export interface NoteMemoryRecord extends MemoryRecordBase {
  kind: "note";
  content: string;
  content_truncated: boolean;
}

export interface SkillMemoryRecord extends MemoryRecordBase {
  kind: "skill";
  name: string;
  version: number;
  deprecated: boolean;
  description: string;
  description_truncated: boolean;
}

export interface BadPatternMemoryRecord extends MemoryRecordBase {
  kind: "bad_pattern";
  observed_at: string;
  description: string;
  description_truncated: boolean;
}

export interface CoreMemoryRecord extends MemoryRecordBase {
  kind: "core";
  block_kind: string;
  content: string;
  content_truncated: boolean;
}

export interface WorkItemMemoryRecord extends MemoryRecordBase {
  kind: "work_item";
  level: string;
  work_status: WorkItemRecord["status"];
  parent: string | null;
  ordinal: number;
  title: string;
  body: string | null;
  display: string;
  display_truncated: boolean;
}

export interface TagMemoryRecord extends MemoryRecordBase {
  kind: "tag";
  slug: string;
  display: string;
  display_truncated: boolean;
}
