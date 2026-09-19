-- album_verifications にディスク番号と、記録したジョブの id を足す（P2-9、SPEC §7.3、D-63）。
--
-- 遡及照合の単位は「アルバムの 1 ディスク」で、複数ディスクのアルバムはディスクごとに
-- TOC を再構成して別々に照会する。1 ディスクのアルバムでも tracks.disc_no に合わせて 1 を
-- 入れる（disc_no が無いトラック群は 1 とみなす）。NULL は 0013 より前の行。
--
-- job_id はジョブの冪等性のため。記録（DB の commit）の後、ログの確定やジョブの done の前に
-- 落ちて起動時リカバリで同じジョブが再実行されても、同じ (job_id, disc_no, method) の行が
-- あれば再利用し、履歴を重複させない。履歴として積む表なので既存行は触らない

ALTER TABLE album_verifications ADD COLUMN disc_no INTEGER;
ALTER TABLE album_verifications ADD COLUMN job_id INTEGER REFERENCES jobs(id) ON DELETE SET NULL;

CREATE UNIQUE INDEX idx_alb_verif_job ON album_verifications(job_id, disc_no, method)
  WHERE job_id IS NOT NULL;
