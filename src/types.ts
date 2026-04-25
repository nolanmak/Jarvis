export interface Email {
  messageId: string;
  threadId: string;
  from: string;
  subject: string;
  body: string;
  date: string;
  accountEntityId?: string;
  platform?: string;
  kind?: string;
}

export type ActionStatus =
  | "pending"
  | "approved"
  | "rejected"
  | "sent"
  | "error"
  | "timed_out"
  | "skipped"
  | "flagged";

export interface ActionRecord {
  id: string;
  messageId: string;
  threadId?: string;
  fromEmail: string;
  subject: string;
  originalBody?: string;
  draftBody?: string;
  status: ActionStatus;
  errorMessage?: string;
  createdAt: number;
  updatedAt: number;
  /// Populated via LEFT JOIN on emails.messageId. Absent for orphan actions (should be rare).
  platform?: string;
  kind?: string;
}

export interface Sender {
  id: string;
  email: string;
  label?: string;
  active: boolean;
  createdAt: number;
}

export interface DashboardStats {
  total: number;
  approved: number;
  rejected: number;
  sent: number;
  errored: number;
  todayCount: number;
  approvalRate: number;
}

/// Routing mode for a channel subscription. Matches Rust
/// `augmentagent_store::SubscriptionMode` serde representation.
export type SubscriptionMode = "priority" | "digest" | "store_only";

export interface ChannelSubscription {
  id: string;
  platform: string;
  channel_id: string;
  display_name: string;
  mode: SubscriptionMode;
  active: boolean;
  /// For multi-account platforms, identifies which connected account this
  /// subscription lives under (Slack team_id). NULL for single-account
  /// platforms (Discord user-token).
  account_id?: string | null;
  last_seen_message_id?: string | null;
  last_digest_at_ms?: number | null;
  created_at_ms: number;
  updated_at_ms: number;
}
