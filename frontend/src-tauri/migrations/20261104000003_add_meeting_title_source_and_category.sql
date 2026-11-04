-- Who set the meeting title: auto (recording default), summary, smart
-- (post-meeting pipeline) or manual (user). Manual titles are never
-- overwritten automatically; summary renames only replace auto/summary titles.
ALTER TABLE meetings ADD COLUMN title_source TEXT NOT NULL DEFAULT 'auto';
-- Category folder the smart pipeline filed the meeting under (NULL = unfiled).
ALTER TABLE meetings ADD COLUMN category TEXT;
