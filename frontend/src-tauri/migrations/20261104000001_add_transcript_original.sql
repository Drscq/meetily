-- Raw ASR text kept when a segment is corrected by the smart pipeline.
-- NULL means the segment has never been corrected.
ALTER TABLE transcripts ADD COLUMN original_transcript TEXT;
