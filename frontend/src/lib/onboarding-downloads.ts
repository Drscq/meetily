// Which models the user chose to download during onboarding, and what that
// means for continuing and for the saved setup. Pure logic, no Tauri calls.

export interface DownloadSelection {
  parakeet: boolean;
  summary: boolean;
}

export const DEFAULT_DOWNLOAD_SELECTION: DownloadSelection = { parakeet: true, summary: false };

export interface DownloadProgress {
  parakeetDownloaded: boolean;
  summaryDownloaded: boolean;
}

// Speech recognition is needed before the first recording, so a selected
// Parakeet download must finish. The summary model downloads in the background.
export function canContinue(selection: DownloadSelection, progress: DownloadProgress): boolean {
  return !selection.parakeet || progress.parakeetDownloaded;
}

export type OnboardingModelStatus = 'downloaded' | 'downloading' | 'skipped';

export interface CompletionPlan {
  parakeet: OnboardingModelStatus;
  summary: OnboardingModelStatus;
  summaryProvider: 'builtin-ai' | 'cli-agent';
}

function statusOf(selected: boolean, downloaded: boolean): OnboardingModelStatus {
  if (!selected) return 'skipped';
  return downloaded ? 'downloaded' : 'downloading';
}

// Without a local summary model, summaries go through the local CLI agent.
export function completionPlan(selection: DownloadSelection, progress: DownloadProgress): CompletionPlan {
  return {
    parakeet: statusOf(selection.parakeet, progress.parakeetDownloaded),
    summary: statusOf(selection.summary, progress.summaryDownloaded),
    summaryProvider: selection.summary ? 'builtin-ai' : 'cli-agent',
  };
}
