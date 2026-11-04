import React, { useState, useEffect } from 'react';
import { Switch } from '@/components/ui/switch';
import { Input } from '@/components/ui/input';
import { FolderOpen, FolderSearch, RotateCcw } from 'lucide-react';
import { invoke } from '@tauri-apps/api/core';
import { DeviceSelection, SelectedDevices } from '@/components/DeviceSelection';
import Analytics from '@/lib/analytics';
import { toast } from 'sonner';
import { useRecordingState } from '@/contexts/RecordingStateContext';
import { useConfig } from '@/contexts/ConfigContext';

export const DEFAULT_FOLDER_NAME_TEMPLATE = '{meeting}_{date}_{time}';
export const DEFAULT_AUDIO_FILE_NAME = 'audio';

export interface RecordingPreferences {
  save_folder: string;
  auto_save: boolean;
  file_format: string;
  preferred_mic_device: string | null;
  preferred_system_device: string | null;
  /** Template for each meeting's folder name, e.g. `{meeting}_{date}_{time}`. */
  folder_name_template: string;
  /** Base name (no extension) of the merged audio file inside the meeting folder. */
  audio_file_name: string;
}

const NAME_PLACEHOLDERS: Array<{ token: string; meaning: string }> = [
  { token: '{meeting}', meaning: 'meeting title' },
  { token: '{date}', meaning: 'YYYY-MM-DD' },
  { token: '{time}', meaning: 'HH-MM' },
  { token: '{datetime}', meaning: 'YYYY-MM-DD_HH-MM-SS' },
  { token: '{year}', meaning: 'YYYY' },
  { token: '{month}', meaning: 'MM' },
  { token: '{day}', meaning: 'DD' },
  { token: '{hour}', meaning: 'HH' },
  { token: '{minute}', meaning: 'MM' },
  { token: '{second}', meaning: 'SS' },
];

interface RecordingSettingsProps {
  onSave?: (preferences: RecordingPreferences) => void;
}

export function RecordingSettings({ onSave }: RecordingSettingsProps) {
  const [preferences, setPreferences] = useState<RecordingPreferences>({
    save_folder: '',
    auto_save: true,
    file_format: 'mp4',
    preferred_mic_device: null,
    preferred_system_device: null,
    folder_name_template: DEFAULT_FOLDER_NAME_TEMPLATE,
    audio_file_name: DEFAULT_AUDIO_FILE_NAME,
  });
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [showRecordingNotification, setShowRecordingNotification] = useState(true);
  const [defaultFolder, setDefaultFolder] = useState<string>('');
  // Draft values for the naming inputs; committed on blur / Enter so every keystroke
  // doesn't round-trip through the store.
  const [folderTemplateDraft, setFolderTemplateDraft] = useState<string>(DEFAULT_FOLDER_NAME_TEMPLATE);
  const [audioNameDraft, setAudioNameDraft] = useState<string>(DEFAULT_AUDIO_FILE_NAME);
  const [folderPreview, setFolderPreview] = useState<string>('');
  const [audioPreview, setAudioPreview] = useState<string>('');
  const { isRecording } = useRecordingState();
  const { setSelectedDevices } = useConfig();

  // Load recording preferences on component mount
  useEffect(() => {
    const loadPreferences = async () => {
      try {
        const prefs = await invoke<RecordingPreferences>('get_recording_preferences');
        const normalized: RecordingPreferences = {
          ...prefs,
          folder_name_template: prefs.folder_name_template || DEFAULT_FOLDER_NAME_TEMPLATE,
          audio_file_name: prefs.audio_file_name || DEFAULT_AUDIO_FILE_NAME,
        };
        setPreferences(normalized);
        setFolderTemplateDraft(normalized.folder_name_template);
        setAudioNameDraft(normalized.audio_file_name);
      } catch (error) {
        console.error('Failed to load recording preferences:', error);
        // If loading fails, get default folder path
        try {
          const defaultPath = await invoke<string>('get_default_recordings_folder_path');
          setPreferences(prev => ({ ...prev, save_folder: defaultPath }));
        } catch (defaultError) {
          console.error('Failed to get default folder path:', defaultError);
        }
      } finally {
        setLoading(false);
      }
    };

    loadPreferences();
  }, []);

  // Default folder, used for the "Reset to default" action and to label the current choice
  useEffect(() => {
    invoke<string>('get_default_recordings_folder_path')
      .then(setDefaultFolder)
      .catch((error) => console.error('Failed to get default folder path:', error));
  }, []);

  // Live preview of what the folder / audio file will be called
  useEffect(() => {
    let cancelled = false;
    const timer = setTimeout(() => {
      Promise.all([
        invoke<string>('preview_recording_name', { template: folderTemplateDraft, meetingName: null }),
        invoke<string>('preview_recording_name', { template: audioNameDraft || DEFAULT_AUDIO_FILE_NAME, meetingName: null }),
      ])
        .then(([folder, audio]) => {
          if (!cancelled) {
            setFolderPreview(folder);
            setAudioPreview(`${audio}.mp4`);
          }
        })
        .catch((error) => console.error('Failed to preview recording name:', error));
    }, 250);
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, [folderTemplateDraft, audioNameDraft]);

  // Load recording notification preference
  useEffect(() => {
    const loadNotificationPref = async () => {
      try {
        const { Store } = await import('@tauri-apps/plugin-store');
        const store = await Store.load('preferences.json');
        const show = await store.get<boolean>('show_recording_notification') ?? true;
        setShowRecordingNotification(show);
      } catch (error) {
        console.error('Failed to load notification preference:', error);
      }
    };
    loadNotificationPref();
  }, []);

  const handleAutoSaveToggle = async (enabled: boolean) => {
    const newPreferences = { ...preferences, auto_save: enabled };
    setPreferences(newPreferences);
    await savePreferences(newPreferences);

    // Track auto-save setting change
    await Analytics.track('auto_save_recording_toggled', {
      enabled: enabled.toString()
    });
  };

  const handleDeviceChange = async (devices: SelectedDevices) => {
    const newPreferences = {
      ...preferences,
      preferred_mic_device: devices.micDevice,
      preferred_system_device: devices.systemDevice
    };
    setPreferences(newPreferences);
    // Sync the in-memory selection ConfigContext exposes to the start path.
    // Without this, the picker only writes to disk (loaded into context once at
    // app mount), so a newly-chosen mic isn't honored until the next launch —
    // start keeps sending the stale launch-time device.
    setSelectedDevices(devices);
    await savePreferences(newPreferences);

    // Track default device preference changes
    // Note: Individual device selection analytics are tracked in DeviceSelection component
    await Analytics.track('default_devices_changed', {
      has_preferred_microphone: (!!devices.micDevice).toString(),
      has_preferred_system_audio: (!!devices.systemDevice).toString()
    });
  };

  const handleOpenFolder = async () => {
    try {
      await invoke('open_recordings_folder');
    } catch (error) {
      console.error('Failed to open recordings folder:', error);
      toast.error('Failed to open recordings folder', {
        description: error instanceof Error ? error.message : String(error),
      });
    }
  };

  const applySaveFolder = async (folder: string) => {
    const newPreferences = { ...preferences, save_folder: folder };
    setPreferences(newPreferences);
    await savePreferences(newPreferences, {
      successTitle: 'Save location updated',
      successDescription: folder,
    });
    await Analytics.track('recording_save_folder_changed', {
      is_default: (folder === defaultFolder).toString(),
    });
  };

  const handleChooseFolder = async () => {
    try {
      const picked = await invoke<string | null>('select_recording_folder');
      if (picked) {
        await applySaveFolder(picked);
      }
    } catch (error) {
      console.error('Failed to choose recordings folder:', error);
      toast.error('Failed to choose folder', {
        description: error instanceof Error ? error.message : String(error),
      });
    }
  };

  const handleResetFolder = async () => {
    if (!defaultFolder) return;
    await applySaveFolder(defaultFolder);
  };

  const commitFolderTemplate = async () => {
    const template = folderTemplateDraft.trim() || DEFAULT_FOLDER_NAME_TEMPLATE;
    setFolderTemplateDraft(template);
    if (template === preferences.folder_name_template) return;
    const newPreferences = { ...preferences, folder_name_template: template };
    setPreferences(newPreferences);
    await savePreferences(newPreferences, {
      successTitle: 'Folder naming updated',
      successDescription: folderPreview ? `Example: ${folderPreview}` : undefined,
    });
  };

  const commitAudioName = async () => {
    const name = audioNameDraft.trim() || DEFAULT_AUDIO_FILE_NAME;
    setAudioNameDraft(name);
    if (name === preferences.audio_file_name) return;
    const newPreferences = { ...preferences, audio_file_name: name };
    setPreferences(newPreferences);
    await savePreferences(newPreferences, {
      successTitle: 'Audio file naming updated',
      successDescription: audioPreview ? `Example: ${audioPreview}` : undefined,
    });
  };

  const resetNaming = async () => {
    setFolderTemplateDraft(DEFAULT_FOLDER_NAME_TEMPLATE);
    setAudioNameDraft(DEFAULT_AUDIO_FILE_NAME);
    if (
      preferences.folder_name_template === DEFAULT_FOLDER_NAME_TEMPLATE &&
      preferences.audio_file_name === DEFAULT_AUDIO_FILE_NAME
    ) {
      return;
    }
    const newPreferences = {
      ...preferences,
      folder_name_template: DEFAULT_FOLDER_NAME_TEMPLATE,
      audio_file_name: DEFAULT_AUDIO_FILE_NAME,
    };
    setPreferences(newPreferences);
    await savePreferences(newPreferences, { successTitle: 'Naming reset to defaults' });
  };

  const handleNotificationToggle = async (enabled: boolean) => {
    try {
      setShowRecordingNotification(enabled);
      const { Store } = await import('@tauri-apps/plugin-store');
      const store = await Store.load('preferences.json');
      await store.set('show_recording_notification', enabled);
      await store.save();
      toast.success('Preference saved');
      await Analytics.track('recording_notification_preference_changed', {
        enabled: enabled.toString()
      });
    } catch (error) {
      console.error('Failed to save notification preference:', error);
      toast.error('Failed to save preference');
    }
  };

  const savePreferences = async (
    prefs: RecordingPreferences,
    feedback?: { successTitle: string; successDescription?: string }
  ) => {
    setSaving(true);
    try {
      await invoke('set_recording_preferences', { preferences: prefs });
      onSave?.(prefs);

      if (feedback) {
        toast.success(feedback.successTitle, { description: feedback.successDescription });
      } else {
        // Show success toast with device details
        const micDevice = prefs.preferred_mic_device || 'Default';
        const systemDevice = prefs.preferred_system_device || 'Default';
        toast.success("Device preferences saved", {
          description: `Microphone: ${micDevice}, System Audio: ${systemDevice}`
        });
      }
    } catch (error) {
      console.error('Failed to save recording preferences:', error);
      toast.error(feedback ? 'Failed to save recording preferences' : "Failed to save device preferences", {
        description: error instanceof Error ? error.message : String(error)
      });
      // Re-sync the UI with what is actually persisted
      try {
        const persisted = await invoke<RecordingPreferences>('get_recording_preferences');
        setPreferences(persisted);
        setFolderTemplateDraft(persisted.folder_name_template || DEFAULT_FOLDER_NAME_TEMPLATE);
        setAudioNameDraft(persisted.audio_file_name || DEFAULT_AUDIO_FILE_NAME);
      } catch {
        // ignore - keep current UI state
      }
    } finally {
      setSaving(false);
    }
  };

  if (loading) {
    return (
      <div className="animate-pulse">
        <div className="h-4 bg-gray-200 rounded w-1/4 mb-4"></div>
        <div className="h-8 bg-gray-200 rounded mb-4"></div>
      </div>
    );
  }

  return (
    <div className="space-y-6">
      <div>
        <h3 className="text-lg font-semibold mb-4">Recording Settings</h3>
        <p className="text-sm text-gray-600 mb-6">
          Configure how your audio recordings are saved during meetings.
        </p>
      </div>

      {/* Auto Save Toggle */}
      <div className="flex items-center justify-between p-4 border rounded-lg">
        <div className="flex-1">
          <div className="font-medium">Save Audio Recordings</div>
          <div className="text-sm text-gray-600">
            Automatically save audio files when recording stops
          </div>
        </div>
        <Switch
          checked={preferences.auto_save}
          onCheckedChange={handleAutoSaveToggle}
          disabled={saving}
        />
      </div>

      {/* Save location + naming. Transcripts/metadata are written here even when audio saving is off. */}
      <div className="space-y-4">
        <div className="p-4 border rounded-lg bg-gray-50">
          <div className="flex items-center justify-between mb-2">
            <div className="font-medium">Save Location</div>
            {defaultFolder && preferences.save_folder === defaultFolder && (
              <span className="text-xs text-gray-500">Default</span>
            )}
          </div>
          <div className="text-sm text-gray-700 mb-3 break-all font-mono" data-testid="save-folder-path">
            {preferences.save_folder || 'Default folder'}
          </div>
          <div className="flex flex-wrap gap-2">
            <button
              onClick={handleChooseFolder}
              disabled={saving || isRecording}
              className="flex items-center gap-2 px-3 py-2 text-sm border border-gray-300 rounded-md bg-white hover:bg-gray-100 transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
            >
              <FolderSearch className="w-4 h-4" />
              Choose Folder…
            </button>
            <button
              onClick={handleOpenFolder}
              className="flex items-center gap-2 px-3 py-2 text-sm border border-gray-300 rounded-md bg-white hover:bg-gray-100 transition-colors"
            >
              <FolderOpen className="w-4 h-4" />
              Open Folder
            </button>
            {defaultFolder && preferences.save_folder !== defaultFolder && (
              <button
                onClick={handleResetFolder}
                disabled={saving || isRecording}
                className="flex items-center gap-2 px-3 py-2 text-sm text-gray-600 hover:text-gray-900 transition-colors disabled:opacity-50"
                title={defaultFolder}
              >
                <RotateCcw className="w-4 h-4" />
                Reset to default
              </button>
            )}
          </div>
          {isRecording && (
            <p className="text-xs text-amber-700 mt-2">
              The save location can be changed once the current recording has stopped.
            </p>
          )}
        </div>

        <div className="p-4 border rounded-lg bg-gray-50 space-y-4">
          <div className="flex items-center justify-between">
            <div>
              <div className="font-medium">File Naming</div>
              <div className="text-sm text-gray-600">
                Each meeting gets its own folder inside the save location.
              </div>
            </div>
            <button
              onClick={resetNaming}
              disabled={saving}
              className="flex items-center gap-1 text-xs text-gray-600 hover:text-gray-900 disabled:opacity-50"
            >
              <RotateCcw className="w-3 h-3" />
              Reset
            </button>
          </div>

          <div>
            <label htmlFor="folder-name-template" className="block text-sm font-medium text-gray-800 mb-1">
              Meeting folder name
            </label>
            <Input
              id="folder-name-template"
              value={folderTemplateDraft}
              onChange={(e) => setFolderTemplateDraft(e.target.value)}
              onBlur={commitFolderTemplate}
              onKeyDown={(e) => {
                if (e.key === 'Enter') {
                  (e.target as HTMLInputElement).blur();
                }
              }}
              placeholder={DEFAULT_FOLDER_NAME_TEMPLATE}
              disabled={saving}
              className="font-mono bg-white"
            />
            <div className="text-xs text-gray-600 mt-1 break-all">
              Example: <span className="font-mono text-gray-800">{folderPreview || '…'}</span>
            </div>
          </div>

          <div>
            <label htmlFor="audio-file-name" className="block text-sm font-medium text-gray-800 mb-1">
              Audio file name {!preferences.auto_save && <span className="text-gray-500 font-normal">(audio saving is off)</span>}
            </label>
            <div className="flex items-center gap-2">
              <Input
                id="audio-file-name"
                value={audioNameDraft}
                onChange={(e) => setAudioNameDraft(e.target.value)}
                onBlur={commitAudioName}
                onKeyDown={(e) => {
                  if (e.key === 'Enter') {
                    (e.target as HTMLInputElement).blur();
                  }
                }}
                placeholder={DEFAULT_AUDIO_FILE_NAME}
                disabled={saving}
                className="font-mono bg-white"
              />
              <span className="text-sm text-gray-600 font-mono">.{preferences.file_format}</span>
            </div>
            <div className="text-xs text-gray-600 mt-1 break-all">
              Example: <span className="font-mono text-gray-800">{audioPreview || '…'}</span>
            </div>
          </div>

          <div className="text-xs text-gray-600">
            <div className="mb-1">Placeholders (local time):</div>
            <div className="flex flex-wrap gap-x-3 gap-y-1">
              {NAME_PLACEHOLDERS.map(({ token, meaning }) => (
                <span key={token}>
                  <code className="px-1 py-0.5 bg-white border rounded text-[11px]">{token}</code>
                  <span className="ml-1 text-gray-500">{meaning}</span>
                </span>
              ))}
            </div>
            <div className="mt-2 text-gray-500">
              Characters that are not allowed in file names are replaced with “_”. If a folder with
              the same name already exists, “(2)”, “(3)”… is appended. The transcript is always saved
              as <code className="font-mono">transcripts.json</code> next to the audio file.
            </div>
          </div>
        </div>
      </div>

      {/* Info when auto_save is disabled */}
      {!preferences.auto_save && (
        <div className="p-4 border rounded-lg bg-yellow-50">
          <div className="text-sm text-yellow-800">
            Audio recording is disabled. Enable "Save Audio Recordings" to automatically save your meeting audio.
          </div>
        </div>
      )}

      {/* Recording Notification Toggle */}
      <div className="flex items-center justify-between p-4 border rounded-lg">
        <div className="flex-1">
          <div className="font-medium">Recording Start Notification</div>
          <div className="text-sm text-gray-600">
            Show reminder to inform participants when recording starts
          </div>
        </div>
        <Switch
          checked={showRecordingNotification}
          onCheckedChange={handleNotificationToggle}
        />
      </div>

      {/* Device Preferences */}
      <div className="space-y-4">
        <div className="border-t pt-6">
          <h4 className="text-base font-medium text-gray-900 mb-4">Default Audio Devices</h4>
          <p className="text-sm text-gray-600 mb-4">
            Set your preferred microphone and system audio devices for recording. These will be automatically selected when starting new recordings.
          </p>

          {isRecording && (
            <p
              role="status"
              aria-live="polite"
              className="text-sm text-amber-700 bg-amber-50 border border-amber-200 rounded-md p-2 mb-4"
            >
              Device selection is locked while a recording is in progress. Connecting a new device mid-recording will not switch to it. Stop the current meeting to change devices.
            </p>
          )}

          <div className="border rounded-lg p-4 bg-gray-50">
            <DeviceSelection
              selectedDevices={{
                micDevice: preferences.preferred_mic_device,
                systemDevice: preferences.preferred_system_device
              }}
              onDeviceChange={handleDeviceChange}
              disabled={saving || isRecording}
            />
          </div>
        </div>
      </div>
    </div>
  );
}