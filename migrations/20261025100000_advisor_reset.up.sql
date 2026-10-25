-- One-off reset (owner, 2026-10-25): everything the advisor produced before
-- trip revenue was stored correctly was reasoned from wrong figures (Watanya
-- and TAQA revenue were computed at runtime, so trips.revenue read ~2 EGP and
-- 0), and with the earlier model. This ships with the fix, so it clears
-- exactly what came before it.
--
-- * Digests are deleted.
-- * Undecided suggestions are closed as rejected, with the reason, so none can
--   be approved. Applied ones stay: they are real changes FalconGo / maint-rust
--   already made, and their audit logs point at these rows.
-- * Conversations are archived: the model replays a transcript, and old ones
--   would keep repeating the wrong numbers.
DELETE FROM advisor.digests;

UPDATE advisor.suggestions
   SET status = 'rejected',
       decided_at = now(),
       result = jsonb_build_object('reset', 'built on trip revenue before it was stored correctly; ask again')
 WHERE status IN ('pending', 'applying');

UPDATE advisor.conversations
   SET deleted_at = now()
 WHERE deleted_at IS NULL;
