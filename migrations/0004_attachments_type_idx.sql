-- The listing by `record_type` alone, with the cursor. The prefix of
-- (record_type, name, id) finds the rows but loses the id order after
-- `name`, so each page would sort every attachment of the type.
CREATE INDEX attachments_type_idx ON attachments (record_type, id);
