export interface Email {
  messageId: string;
  threadId: string;
  from: string;
  subject: string;
  body: string;
  date: string;
  accountEntityId?: string;
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
