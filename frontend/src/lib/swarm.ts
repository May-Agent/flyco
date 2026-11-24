/**
 * Pure fold from a session's event stream into read-only Antigravity Swarm
 * state.
 *
 * Keeps the newest non-null snapshot (`title`, `goal`, `threads`, `tasks`,
 * `members`) and merges incremental `board_messages` batches by monotonic
 * `seq`, deduplicating catch-up and live overlaps cleanly.
 */
import type { TimedEvent } from "../api/relay";
import type {
  SwarmBoardMessage,
  SwarmGoal,
  SwarmMember,
  SwarmThread,
} from "../api/wire";

export interface SwarmState {
  swarmId: string | null;
  title: string | null;
  goal: SwarmGoal | null;
  threads: SwarmThread[];
  tasks: Record<string, string>;
  members: SwarmMember[];
  messages: SwarmBoardMessage[];
}

export type MemberTone = "active" | "waiting" | "killed" | "neutral";

export type MentionSegment =
  | { kind: "text"; text: string }
  | { kind: "mention"; text: string };

/**
 * Folds all `swarm_updated` events from the session's relay into one
 * deterministic read-only swarm view. Returns `null` when the session has
 * never reported any swarm data.
 */
export function foldSwarmState(events: readonly TimedEvent[]): SwarmState | null {
  let seen = false;
  let swarmId: string | null = null;
  let title: string | null = null;
  let goal: SwarmGoal | null = null;
  let threads: SwarmThread[] = [];
  let tasks: Record<string, string> = {};
  let members: SwarmMember[] = [];
  const bySeq = new Map<number, SwarmBoardMessage>();

  for (const entry of events) {
    if (entry.event.type !== "swarm_updated") {
      continue;
    }
    seen = true;
    const snap = entry.event.snapshot;
    if (snap !== undefined && snap !== null) {
      swarmId = snap.swarm_id ?? null;
      title = snap.title ?? null;
      goal = snap.goal ?? null;
      threads = snap.threads ?? [];
      tasks = snap.tasks ?? {};
      members = snap.members ?? [];
    }
    const batch = entry.event.board_messages;
    if (batch !== undefined) {
      for (const msg of batch) {
        bySeq.set(msg.seq, msg);
      }
    }
  }

  if (!seen) {
    return null;
  }

  const messages = Array.from(bySeq.values()).sort((left, right) => left.seq - right.seq);
  return {
    swarmId,
    title,
    goal,
    threads,
    tasks,
    members,
    messages,
  };
}

/** Semantic status tone for a swarm member's status dot. */
export function memberStatusTone(member: SwarmMember): MemberTone {
  if (member.killed === true) {
    return "killed";
  }
  const phase = (member.phase ?? member.state ?? "").toLowerCase();
  if (phase === "working" || phase === "running" || phase === "active") {
    return "active";
  }
  if (phase === "waiting" || phase === "paused" || phase === "blocked") {
    return "waiting";
  }
  if (phase === "killed" || phase === "failed" || phase === "error") {
    return "killed";
  }
  return "neutral";
}

/** Primary display label for a member's lifecycle state/phase. */
export function memberStatusLabel(member: SwarmMember): string {
  if (member.killed === true) {
    return "killed";
  }
  return member.phase ?? member.state ?? "—";
}

/** Count of non-killed members currently active/working. */
export function activeMemberCount(members: readonly SwarmMember[]): number {
  let count = 0;
  for (const member of members) {
    if (memberStatusTone(member) === "active") {
      count += 1;
    }
  }
  return count;
}

const MENTION_PATTERN = /(^|[\s([{"'])(@[a-zA-Z0-9][a-zA-Z0-9_-]*)/g;

/**
 * Splits a board message into plain text and `@mention` segments so mentions
 * can be highlighted safely in JSX without raw HTML injection.
 */
export function segmentMentions(text: string): MentionSegment[] {
  const segments: MentionSegment[] = [];
  let cursor = 0;
  for (const match of text.matchAll(MENTION_PATTERN)) {
    const fullIndex = match.index ?? 0;
    const prefix = match[1] ?? "";
    const mention = match[2] ?? "";
    const mentionStart = fullIndex + prefix.length;
    if (mentionStart > cursor) {
      segments.push({ kind: "text", text: text.slice(cursor, mentionStart) });
    }
    segments.push({ kind: "mention", text: mention });
    cursor = mentionStart + mention.length;
  }
  if (cursor < text.length) {
    segments.push({ kind: "text", text: text.slice(cursor) });
  }
  return segments.length > 0 ? segments : [{ kind: "text", text }];
}

/** Formats an ISO/RFC 3339 board timestamp into local `HH:MM:SS` when valid. */
export function formatBoardTime(raw: string | null | undefined): string | null {
  if (raw === null || raw === undefined || raw.trim() === "") {
    return null;
  }
  const date = new Date(raw);
  if (Number.isNaN(date.getTime())) {
    return raw;
  }
  const hours = String(date.getHours()).padStart(2, "0");
  const minutes = String(date.getMinutes()).padStart(2, "0");
  const seconds = String(date.getSeconds()).padStart(2, "0");
  return `${hours}:${minutes}:${seconds}`;
}
