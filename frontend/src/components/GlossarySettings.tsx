'use client';

import React, { useCallback, useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { BookOpen, Loader2, Plus, Trash2 } from 'lucide-react';
import { toast } from 'sonner';
import { Input } from '@/components/ui/input';
import { Button } from '@/components/ui/button';

interface GlossaryTerm {
  id: number;
  term: string;
  source: string;
}

const SOURCE_LABELS: Record<string, string> = {
  default: 'Default',
  manual: 'Added by you',
  obsidian: 'Obsidian',
  learned: 'Learned from meetings',
};

export function GlossarySettings() {
  const [terms, setTerms] = useState<GlossaryTerm[]>([]);
  const [newTerm, setNewTerm] = useState('');
  const [filter, setFilter] = useState('');
  const [importing, setImporting] = useState(false);

  const refresh = useCallback(async () => {
    try {
      setTerms(await invoke<GlossaryTerm[]>('api_glossary_list'));
    } catch (error) {
      toast.error('Failed to load glossary', { description: String(error) });
    }
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  const addTerm = async () => {
    const term = newTerm.trim();
    if (!term) return;
    try {
      await invoke('api_glossary_add', { term });
      setNewTerm('');
      await refresh();
    } catch (error) {
      toast.error('Failed to add term', { description: String(error) });
    }
  };

  const deleteTerm = async (id: number) => {
    try {
      await invoke('api_glossary_delete', { id });
      await refresh();
    } catch (error) {
      toast.error('Failed to delete term', { description: String(error) });
    }
  };

  const importFromObsidian = async () => {
    const vaultPath = await invoke<string | null>('api_glossary_pick_vault').catch(() => null);
    if (!vaultPath) return;
    setImporting(true);
    try {
      const count = await invoke<number>('api_glossary_seed_from_obsidian', { vaultPath });
      toast.success(`Imported ${count} terms from Obsidian`);
      await refresh();
    } catch (error) {
      toast.error('Obsidian import failed', { description: String(error) });
    } finally {
      setImporting(false);
    }
  };

  const visible = terms.filter((t) => t.term.toLowerCase().includes(filter.trim().toLowerCase()));

  return (
    <div className="space-y-6 mt-6">
      <div className="bg-white rounded-lg border border-gray-200 p-6 space-y-4">
        <div>
          <h3 className="text-lg font-semibold text-gray-900">Glossary</h3>
          <p className="text-sm text-gray-600 mt-1">
            Names and terms used to correct transcripts after each meeting. New terms are learned
            automatically once they appear in two meetings or the model is confident. Deleted terms
            are never re-learned.
          </p>
        </div>

        <div className="flex gap-2">
          <Input
            placeholder="Add a name or term, e.g. Thang Hoang"
            value={newTerm}
            onChange={(e) => setNewTerm(e.target.value)}
            onKeyDown={(e) => e.key === 'Enter' && addTerm()}
          />
          <Button onClick={addTerm} disabled={!newTerm.trim()}>
            <Plus className="w-4 h-4 mr-1" /> Add
          </Button>
          <Button variant="outline" onClick={importFromObsidian} disabled={importing}>
            {importing ? <Loader2 className="w-4 h-4 mr-1 animate-spin" /> : <BookOpen className="w-4 h-4 mr-1" />}
            Import from Obsidian
          </Button>
        </div>

        <Input placeholder={`Filter ${terms.length} terms`} value={filter} onChange={(e) => setFilter(e.target.value)} />

        <ul className="divide-y divide-gray-100 border border-gray-100 rounded-md max-h-[28rem] overflow-y-auto">
          {visible.map((t) => (
            <li key={t.id} className="flex items-center justify-between px-3 py-2">
              <div>
                <span className="text-sm text-gray-900">{t.term}</span>
                <span className="ml-2 text-xs text-gray-500">{SOURCE_LABELS[t.source] ?? t.source}</span>
              </div>
              <button
                aria-label={`Delete ${t.term}`}
                onClick={() => deleteTerm(t.id)}
                className="text-gray-400 hover:text-red-600"
              >
                <Trash2 className="w-4 h-4" />
              </button>
            </li>
          ))}
          {visible.length === 0 && <li className="px-3 py-4 text-sm text-gray-500">No terms</li>}
        </ul>
      </div>
    </div>
  );
}
