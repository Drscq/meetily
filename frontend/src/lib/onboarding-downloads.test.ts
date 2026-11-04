import { test } from 'node:test';
import assert from 'node:assert/strict';
import { canContinue, completionPlan, DEFAULT_DOWNLOAD_SELECTION } from './onboarding-downloads.ts';

const nothing = { parakeetDownloaded: false, summaryDownloaded: false };

test('defaults to speech recognition only', () => {
  assert.deepEqual(DEFAULT_DOWNLOAD_SELECTION, { parakeet: true, summary: false });
});

test('skipping every download lets the user continue at once', () => {
  assert.equal(canContinue({ parakeet: false, summary: false }, nothing), true);
});

test('a selected speech model must finish before continuing; summary never blocks', () => {
  assert.equal(canContinue({ parakeet: true, summary: false }, nothing), false);
  assert.equal(canContinue({ parakeet: false, summary: true }, nothing), true);
  assert.equal(
    canContinue({ parakeet: true, summary: true }, { parakeetDownloaded: true, summaryDownloaded: false }),
    true,
  );
});

test('completion never claims a skipped or unfinished model is downloaded', () => {
  assert.deepEqual(completionPlan({ parakeet: true, summary: false }, { parakeetDownloaded: true, summaryDownloaded: false }), {
    parakeet: 'downloaded',
    summary: 'skipped',
    summaryProvider: 'cli-agent',
  });
  assert.deepEqual(completionPlan({ parakeet: false, summary: true }, nothing), {
    parakeet: 'skipped',
    summary: 'downloading',
    summaryProvider: 'builtin-ai',
  });
  assert.deepEqual(completionPlan({ parakeet: true, summary: true }, { parakeetDownloaded: true, summaryDownloaded: true }), {
    parakeet: 'downloaded',
    summary: 'downloaded',
    summaryProvider: 'builtin-ai',
  });
});
