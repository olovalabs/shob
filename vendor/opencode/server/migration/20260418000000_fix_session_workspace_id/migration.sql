-- Fix any sessions where workspace_id contains a session_id instead of a workspace_id
-- Session IDs start with 'ses_' while Workspace IDs start with 'wrk_'
UPDATE `session` SET `workspace_id` = NULL WHERE `workspace_id` LIKE 'ses_%';
