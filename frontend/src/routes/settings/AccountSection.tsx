/**
 * Settings → Account (docs/ux.md §10).
 *
 * Who flyco acts as, and the way out. The credentials that call the API
 * without a browser are their own section, and so is everything about how
 * the app looks and when it interrupts: neither is a fact about the
 * identity, and both were making this page a drawer of leftovers.
 */
import { Show } from "solid-js";
import { createQuery } from "../../lib/query";
import { useNavigate } from "@solidjs/router";
import { LogOut } from "lucide-solid";
import Logomark, { GITHUB_MARK } from "../../components/Logomark";
import { initialsOf } from "../../components/AppShell";
import ProblemNotice from "../../components/ProblemNotice";
import RatioBar, { type MeterTier } from "../../components/RatioBar";
import { getMe, getStorageUsage } from "../../api/client";
import { signOut } from "../../lib/signOut";
import styles from "./Settings.module.css";

export default function AccountSection() {
  const navigate = useNavigate();

  return (
    <section class={styles.section}>
      <header class={styles.sectionHead}>
        <h2>Account</h2>
      </header>

      <Identity />
      <StorageQuota />

      <div>
        <button type="button" class={styles.pill} onClick={() => signOut(navigate)}>
          <LogOut size={14} aria-hidden="true" />
          Sign out
        </button>
      </div>
    </section>
  );
}

/* ── Identity ─────────────────────────────────────────────────────────── */

function Identity() {
  const [me] = createQuery(getMe);

  return (
    <div class={styles.group}>
      <ProblemNotice error={me.error} />
      <Show when={me()}>
        {(user) => (
          <article class={styles.card}>
            <div class={styles.cardTop}>
              <span class={styles.mark} aria-hidden="true">
                {initialsOf(user().login)}
              </span>
              <div class={styles.identity}>
                <span class={styles.cardTitle}>{user().login}</span>
                <span class={styles.metaWithMark}>
                  <Logomark mark={GITHUB_MARK} size={12} />
                  Signed in with GitHub · {user().session_cap} sessions at once
                </span>
              </div>
            </div>
          </article>
        )}
      </Show>
    </div>
  );
}

/* ── Storage ──────────────────────────────────────────────────────────── */

function formatStorageBytes(bytes: number): string {
  const GB = 1_000_000_000;
  const MB = 1_000_000;
  const KB = 1_000;
  if (bytes >= GB) return `${(bytes / GB).toFixed(2)} GB`;
  if (bytes >= MB) return `${(bytes / MB).toFixed(2)} MB`;
  if (bytes >= KB) return `${(bytes / KB).toFixed(1)} KB`;
  return `${bytes} B`;
}

function storageTier(ratio: number): MeterTier {
  if (ratio >= 0.95) return "paused";
  if (ratio >= 0.85) return "warn";
  if (ratio >= 0.7) return "notice";
  return "ok";
}

function StorageQuota() {
  const [storage] = createQuery(getStorageUsage);

  return (
    <div class={styles.group}>
      <ProblemNotice error={storage.error} />
      <Show when={storage()}>
        {(usage) => {
          const ratio = () =>
            usage().quota_bytes > 0
              ? usage().used_bytes / usage().quota_bytes
              : usage().used_bytes > 0
                ? 1
                : 0;

          return (
            <article class={styles.card}>
              <RatioBar
                label="Storage"
                ratio={ratio()}
                value={`${formatStorageBytes(usage().used_bytes)} / ${formatStorageBytes(usage().quota_bytes)}`}
                tier={storageTier(ratio())}
              />
              <dl class={styles.breakdownList}>
                <div class={styles.breakdownItem}>
                  <dt class={styles.breakdownLabel}>Transcripts</dt>
                  <dd class={styles.breakdownValue}>
                    {formatStorageBytes(usage().breakdown.transcripts_bytes)}
                  </dd>
                </div>
                <div class={styles.breakdownItem}>
                  <dt class={styles.breakdownLabel}>Handoffs</dt>
                  <dd class={styles.breakdownValue}>
                    {formatStorageBytes(usage().breakdown.handoffs_bytes)}
                  </dd>
                </div>
                <div class={styles.breakdownItem}>
                  <dt class={styles.breakdownLabel}>Patches</dt>
                  <dd class={styles.breakdownValue}>
                    {formatStorageBytes(usage().breakdown.workdir_patches_bytes)}
                  </dd>
                </div>
                <div class={styles.breakdownItem}>
                  <dt class={styles.breakdownLabel}>Skills</dt>
                  <dd class={styles.breakdownValue}>
                    {formatStorageBytes(usage().breakdown.skills_bytes)}
                  </dd>
                </div>
              </dl>
            </article>
          );
        }}
      </Show>
    </div>
  );
}

