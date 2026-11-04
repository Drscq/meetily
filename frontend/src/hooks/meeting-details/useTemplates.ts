import { useState, useEffect, useCallback, useRef } from 'react';
import { invoke as invokeTauri } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { toast } from 'sonner';
import Analytics from '@/lib/analytics';

export function useTemplates(meetingId?: string) {
  const [availableTemplates, setAvailableTemplates] = useState<Array<{
    id: string;
    name: string;
    description: string;
  }>>([]);
  const [selectedTemplate, setSelectedTemplate] = useState<string>('standard_meeting');

  // Fetch available templates on mount
  useEffect(() => {
    const fetchTemplates = async () => {
      try {
        const templates = await invokeTauri('api_list_templates') as Array<{
          id: string;
          name: string;
          description: string;
        }>;
        console.log('Available templates:', templates);
        setAvailableTemplates(templates);
      } catch (error) {
        console.error('Failed to fetch templates:', error);
      }
    };
    fetchTemplates();
  }, []);

  // Default to the template for the meeting's category (set by the smart
  // pipeline) until the user picks one by hand.
  const pickedByUser = useRef(false);
  useEffect(() => {
    if (!meetingId) return;
    pickedByUser.current = false;
    const loadDefault = async () => {
      try {
        const id = await invokeTauri<string>('api_smart_template_for_meeting', { meetingId });
        if (!pickedByUser.current) setSelectedTemplate(id);
      } catch (error) {
        console.error('Failed to load category template:', error);
      }
    };
    void loadDefault();
    const unlisten = listen<{ meetingId: string; status: string }>('smart-pipeline-status', (event) => {
      if (event.payload.meetingId === meetingId && event.payload.status === 'done') void loadDefault();
    });
    return () => { void unlisten.then(fn => fn()); };
  }, [meetingId]);

  // Handle template selection
  const handleTemplateSelection = useCallback((templateId: string, templateName: string) => {
    pickedByUser.current = true;
    setSelectedTemplate(templateId);
    toast.success('Template selected', {
      description: `Using "${templateName}" template for summary generation`,
    });
    Analytics.trackFeatureUsed('template_selected');
  }, []);

  return {
    availableTemplates,
    selectedTemplate,
    handleTemplateSelection,
  };
}
