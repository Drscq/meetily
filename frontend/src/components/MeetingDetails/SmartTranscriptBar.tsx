"use client";

import { useCallback, useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { Folder, Loader2, Sparkles, Undo2, Wand2 } from 'lucide-react';
import { toast } from 'sonner';

interface ProcessingStatus {
  status: 'processing' | 'done' | 'undone' | 'pending';
  error?: string | null;
}

interface MeetingPlacement {
  title: string;
  titleSource: string;
  category: string | null;
  folderPath: string | null;
}

interface TranscriptVersions {
  original: Record<string, string>;
  status: ProcessingStatus | null;
  placement: MeetingPlacement | null;
}

interface SmartTranscriptBarProps {
  meetingId: string;
  showOriginal: boolean;
  onShowOriginalChange: (value: boolean) => void;
  onOriginalChange: (original: Record<string, string>) => void;
  onTranscriptChanged?: () => Promise<void>;
}

/** Status of the post-meeting AI correction, with raw/corrected toggle and undo. */
export function SmartTranscriptBar({
  meetingId,
  showOriginal,
  onShowOriginalChange,
  onOriginalChange,
  onTranscriptChanged,
}: SmartTranscriptBarProps) {
  const [versions, setVersions] = useState<TranscriptVersions | null>(null);
  const [busy, setBusy] = useState(false);

  const refresh = useCallback(async () => {
    try {
      const v = await invoke<TranscriptVersions>('api_smart_get_transcript_versions', { meetingId });
      setVersions(v);
      onOriginalChange(v.original);
    } catch (error) {
      console.error('Failed to load transcript versions:', error);
    }
  }, [meetingId, onOriginalChange]);

  useEffect(() => {
    refresh();
    const unlisten = listen<{ meetingId: string; status: string }>('smart-pipeline-status', async (event) => {
      if (event.payload.meetingId !== meetingId) return;
      await refresh();
      if (event.payload.status === 'done') {
        await onTranscriptChanged?.();
      }
    });
    return () => {
      unlisten.then((fn) => fn());
    };
  }, [meetingId, refresh, onTranscriptChanged]);

  const processNow = async () => {
    setBusy(true);
    try {
      const corrected = await invoke<number>('api_smart_process_meeting', { meetingId });
      toast.success(corrected > 0 ? `Corrected ${corrected} segments and filed the meeting` : 'Meeting processed');
    } catch (error) {
      toast.error('AI correction failed', { description: String(error) });
    } finally {
      setBusy(false);
      await refresh();
    }
  };

  const undo = async () => {
    try {
      await invoke('api_smart_undo_corrections', { meetingId });
      onShowOriginalChange(false);
      await refresh();
      await onTranscriptChanged?.();
      toast.success('Restored the original transcript');
    } catch (error) {
      toast.error('Undo failed', { description: String(error) });
    }
  };

  const status = versions?.status?.status;
  const correctedCount = Object.keys(versions?.original ?? {}).length;
  const processing = busy || status === 'processing';

  return (
    <div className="flex flex-wrap items-center justify-center gap-x-3 gap-y-1 px-4 py-2 border-b border-gray-100 text-xs text-gray-600">
      {processing ? (
        <span className="flex items-center gap-1">
          <Loader2 className="w-3 h-3 animate-spin" /> Correcting, filing and summarizing with AI…
        </span>
      ) : status === 'pending' ? (
        <span className="text-amber-700" title={versions?.status?.error ?? ''}>
          AI processing pending (retries automatically)
        </span>
      ) : correctedCount > 0 ? (
        <span className="flex items-center gap-1">
          <Sparkles className="w-3 h-3 text-purple-500" /> {correctedCount} segments corrected
        </span>
      ) : status === 'undone' ? (
        <span>Corrections undone</span>
      ) : status === 'done' ? (
        <span>No corrections needed</span>
      ) : null}

      {!processing && correctedCount > 0 && (
        <>
          <button className="underline hover:text-gray-900" onClick={() => onShowOriginalChange(!showOriginal)}>
            {showOriginal ? 'Show corrected' : 'Show original'}
          </button>
          <button className="flex items-center gap-1 hover:text-red-600" onClick={undo}>
            <Undo2 className="w-3 h-3" /> Undo corrections
          </button>
        </>
      )}

      {!processing && versions?.placement?.category && (
        <span className="flex items-center gap-1" title={versions.placement.folderPath ?? ''}>
          <Folder className="w-3 h-3" /> {versions.placement.category}
        </span>
      )}

      {!processing && (
        <button className="flex items-center gap-1 hover:text-gray-900" onClick={processNow}>
          <Wand2 className="w-3 h-3" /> {status === 'pending' ? 'Process now' : status ? 'Re-run AI' : 'Process with AI'}
        </button>
      )}
    </div>
  );
}
