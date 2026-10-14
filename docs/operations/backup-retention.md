# Backup retention and the redaction runbook

Status: normative for Scion v1 operators (SPEC 13.8, SPEC 20.3; PLAN 6.16). Owner: P22
(`scion-redaction`).

Scion never rewrites history to remove data (SPEC 13.1). A redaction removes the bytes of a
Blob or Meta object and keeps every identifier. A purge is irreversible on the live system, but
it cannot reach copies that already left it: database backups, old object versions in the
bucket, and storage-engine history. Those copies expire only through the retention policy in
this document. **The retention policy therefore bounds how long purged content can still be
recovered by an operator.**

## 1. Retention policy

| Data | Where it can persist after a purge | Maximum retention |
|---|---|---|
| CockroachDB backups (full and incremental, including `revision_history` backups) | Backup collection in the backup bucket or other external storage | **30 days** after the backup was taken |
| CockroachDB MVCC history | Earlier revisions of `objects.inline_data` rows, kept for the zone's `gc.ttlseconds` | `gc.ttlseconds` (default 4 h; never set above 30 days) |
| Object-store versions | Noncurrent versions and delete markers of `<prefix>/r/...` and `<prefix>/q/...` keys when bucket versioning is enabled | **30 days** after the version became noncurrent |
| Object-store backups or replicas (cross-region replication, bucket copies) | The replica bucket | **30 days**, with the same lifecycle rules as the primary |
| Daemon caches | The daemon's verified disk cache | Deleted when the daemon learns of the redaction (SPEC 13.3); at most `redaction_poll_interval_ms` plus one request after `effective_by` while the daemon is reachable |
| Working copies | Materialized files on developer machines | Outside operator control; clean copies are replaced by placeholders on the next mutating command (SPEC 13.7) |
| Logs | Nowhere: Scion never logs file contents, raw paths or secrets (PLAN 6.9) | n/a |

Rules:

1. **No backup or object version may be retained for more than 30 days** (SPEC 13.8 "SHOULD
   NOT exceed 30 days"; this deployment treats it as a requirement). Pick a shorter period if
   you can; the purge guarantee strengthens as retention shrinks.
2. Purged content persists in backups until they expire. Restoring a backup taken before a
   purge brings the content back: see section 4.
3. Retention is enforced by the storage systems themselves (lifecycle rules, backup schedule
   options). It must not depend on a human remembering to delete old backups.
4. Every change to retention settings is reviewed against this document and recorded in the
   deployment's change log.

## 2. Configuring retention

### 2.1 CockroachDB

Backups are taken with a schedule. CockroachDB does not delete old backups by itself, so the
backup destination must expire them:

```sql
-- Daily full backup, hourly incrementals, into a dedicated bucket path.
CREATE SCHEDULE scion_backup
  FOR BACKUP INTO 's3://scion-crdb-backups/scion?AUTH=implicit'
  RECURRING '@hourly'
  FULL BACKUP '@daily'
  WITH SCHEDULE OPTIONS first_run = 'now';
```

- Put a lifecycle rule on the backup bucket (or prefix) that **expires objects 30 days after
  creation** (fewer is better). A full backup and the incrementals that chain on it then
  expire together within one day of each other; keep at least one full backup inside the
  window so restores stay possible.
- If the backup bucket is itself versioned, also expire noncurrent versions after at most
  30 days (section 2.2).
- MVCC garbage collection: keep `gc.ttlseconds` for the Scion database at or below the
  default (`ALTER DATABASE scion CONFIGURE ZONE USING gc.ttlseconds = 14400;`). It must never
  exceed 30 days. Backups with `revision_history` capture revisions within that window, so
  their own 30-day expiry still applies.
- Protected timestamps held by changefeeds or paused backup jobs delay MVCC GC. Do not leave
  such jobs paused for more than a few hours after a purge (section 3, step 6).

### 2.2 Object storage (S3 / MinIO)

Scion stores blob bytes and large tree/meta frames under `<prefix>/r/<repo_id>/...` (live) and
`<prefix>/q/<repo_id>/...` (quarantine) (SPEC 5.1). Purge deletes both keys. On a versioned
bucket a delete only adds a delete marker, so:

- Either leave versioning **disabled** on the Scion bucket. Then a purge removes the bytes
  immediately (subject to the provider's own deletion SLA).
- Or, if versioning is required (accidental-deletion protection, replication), add lifecycle
  rules that apply to the whole Scion prefix:
  - `NoncurrentVersionExpiration` with `NoncurrentDays <= 30`;
  - `ExpiredObjectDeleteMarker = true`, to clean up the delete markers left behind;
  - `AbortIncompleteMultipartUpload` (e.g. 7 days).

Example (AWS CLI):

```json
{
  "Rules": [
    {
      "ID": "scion-noncurrent-30d",
      "Filter": { "Prefix": "scion/" },
      "Status": "Enabled",
      "NoncurrentVersionExpiration": { "NoncurrentDays": 30 },
      "Expiration": { "ExpiredObjectDeleteMarker": true },
      "AbortIncompleteMultipartUpload": { "DaysAfterInitiation": 7 }
    }
  ]
}
```

```sh
aws s3api put-bucket-lifecycle-configuration --bucket scion-objects \
  --lifecycle-configuration file://scion-lifecycle.json
```

MinIO: `mc ilm rule add --noncurrent-expire-days 30 --expire-delete-marker myminio/scion-objects`.

- **Object Lock / WORM retention** (governance or compliance mode) on the Scion bucket must be
  disabled or set to at most 30 days. Compliance-mode locks cannot be shortened later and would
  defeat purges.
- Replica buckets (cross-region replication) need the same rules. Replication of delete
  markers must be enabled so a purge reaches the replica.

## 3. Redaction runbook

Roles: the requester (security or legal), an operator with `admin.redact`, and an operator with
`admin.purge` (SPEC 6.2 `[admin]`). Every step is recorded in the audit log with principal and
reason (SPEC 17; PLAN 6.8).

**Redaction is not credential revocation. Rotate any leaked secret first** (SPEC 13.1).

1. **Identify the objects.** Resolve the ids to redact:
   - one file at a revision: `scion redact --path <p> --at <rev>`;
   - every version the path ever held: `scion redact --path <p> --at <rev> --all-versions`
     (walks the path's history nodes, SPEC 13.2);
   - a known object: `scion redact --object <id>`;
   - a changeset's author/message: `scion redact --meta <rev>`.
   Review the resolved id list. Only Blob and Meta objects can be redacted; trees and changesets
   are rejected with `INVALID_ARGUMENT/not_redactable`, and unknown ids with `NOT_FOUND`.
2. **Redact.** Issue `Redact(ids, reason, purge_after?)` with a meaningful reason (ticket id).
   The default quarantine is `redaction.default_quarantine` (7 days). The response carries the
   new `epoch` and `effective_by = commit time + redaction.propagation_bound_ms`. Record both
   in the ticket. In one transaction the server bumps `redaction_epoch`, stamps the
   `redactions` rows, sets the objects to `quarantined` and appends one audit row per id. It
   then moves the bytes from `<prefix>/r/...` to `<prefix>/q/...`. If that move is interrupted
   (crash, object-store outage), the background mover (every purge interval) finishes it; the
   objects are served to no one either way.
3. **Verify propagation** (after `effective_by`):
   - `GetObjects` for each id returns status `redacted`; `ReadPath` returns `REDACTED`; `Log`
     and `show` render a redacted Meta as `<redacted>` (SPEC 13.4).
   - Every server reports the new epoch: the `scion-redaction-epoch` response trailer and
     `GetServerInfo.redaction_epoch` are at least `epoch`.
   - Daemons: within `redaction_poll_interval_ms` plus one request, a read through each daemon
     returns `redacted` and the cached copy is gone. A daemon that cannot refresh for longer
     than `redaction_max_staleness_ms` fails closed and forwards reads (SPEC 13.3).
   - `ListRedactions(since_epoch = epoch - 1)` lists the ids.
   - In the object store, the live keys of the redacted blobs are absent and the quarantine keys
     exist.
4. **Decide during quarantine.** If the redaction was a mistake, `Unredact(ids)` (permission
   `admin.redact`) restores the bytes byte-identically and bumps the epoch. It works only while
   the objects are quarantined; after a purge it fails with `INVALID_ARGUMENT/not_quarantined`.
   The same holds once a purge has started deleting bytes but was interrupted before marking
   the object purged: unredact refuses it and a re-run of the purge completes it.
5. **Purge.** Either let the scheduled job purge the objects when `purge_after` passes, or
   issue `PurgeNow(ids, reason)` (permission `admin.purge`). Purge deletes the live and
   quarantine keys, the inline data in the `objects` rows, and content-bearing derived records
   keyed by the ids (SPEC 13.5), then marks the objects `purged` with an audit row. Purge is
   irreversible. If it is interrupted, the objects stay quarantined and a re-run (or the next
   scheduled run) completes it. Verify:
   - `ListRedactions(0)` shows `state = purged` for every id;
   - neither `<prefix>/r/...` nor `<prefix>/q/...` exists for any id (current versions);
   - the audit log has a `purge` row per id with actor and reason.
6. **Verify backups expire.** Record the purge time `T` in the ticket. Purged content now
   persists only in:
   - CockroachDB backups taken before `T` (inline Meta/tree data, redaction rows);
   - noncurrent object versions created before `T`;
   - CockroachDB MVCC history until `T + gc.ttlseconds`.
   On `T + 30 days` (or your shorter retention), confirm section 5's checks: no backup older
   than the retention exists, and no noncurrent version of the purged keys remains. Close the
   ticket only then.
7. **Working copies.** Tell affected users that their clean copies will be replaced by
   placeholders on the next mutating command and that dirty copies are reported, never touched
   (SPEC 13.7). Content already copied elsewhere (forks, laptops, CI caches) is outside Scion.

## 4. Restoring from backup after a purge

Restoring a CockroachDB backup taken before a purge resurrects `redactions` rows in the older
state (or none at all) and possibly inline data. Object-store bytes stay deleted unless old
versions are restored too. After any restore:

1. Re-apply every redaction and purge performed after the backup's timestamp. Take the list
   from the audit log of the pre-restore cluster or from the redaction tickets, never from the
   restored database alone.
2. Run `Redact` for the affected ids, then `PurgeNow`. The redaction epoch only grows, so
   daemons pick the change up through `ListRedactions`.
3. Verify with the section 3 checks before re-opening the service to readers.

## 5. Verification steps (SPEC 20.3)

Run these before production and then at least quarterly. Record the results.

1. **Backup retention.**
   - `SHOW SCHEDULES;` shows the backup schedule as active.
   - The backup bucket has a lifecycle rule expiring objects at or below 30 days:
     `aws s3api get-bucket-lifecycle-configuration --bucket scion-crdb-backups`.
   - `SHOW BACKUPS IN 's3://scion-crdb-backups/scion?AUTH=implicit';` lists no backup older than
     30 days.
2. **Object-store retention.**
   - `aws s3api get-bucket-versioning --bucket scion-objects` is `Suspended`/absent, or the
     lifecycle configuration contains `NoncurrentVersionExpiration <= 30` and
     `ExpiredObjectDeleteMarker` for the Scion prefix.
   - `aws s3api get-object-lock-configuration --bucket scion-objects` shows no lock, or a
     default retention of at most 30 days in governance mode.
   - `aws s3api list-object-versions --bucket scion-objects --prefix scion/` shows no
     noncurrent version older than 30 days.
3. **MVCC GC.** `SHOW ZONE CONFIGURATION FROM DATABASE scion;` shows `gc.ttlseconds` at or below
   30 days (default 14400), and no long-paused jobs hold protected timestamps
   (`SHOW JOBS` without old `paused` backup or changefeed jobs).
4. **End-to-end runbook drill** (staging, with a synthetic secret): run section 3 steps 1-6 with
   a short quarantine. Confirm the trailers and `ListRedactions`, the daemon cache deletion,
   the purge of both object keys and the inline data, the audit rows, and then, after the
   retention window, the absence of the content from every backup and object version
   (restore the oldest remaining backup into a scratch cluster and check that the purged ids
   are absent or `purged`).
5. **Audit.** `audit_log` contains `redact`, `unredact` and `purge` rows with actor and reason
   for the drill.

## 6. Implementation notes

- The quarantine move and the purge are idempotent: every step tolerates a re-run after a
  crash. Fault checkpoints `redact.after_commit`, `quarantine.after_copy` and
  `purge.after_bytes` exercise them in tests (`crates/scion-redaction/tests/lifecycle.rs`).
- The background task (`Redactor::spawn_background`) runs the quarantine mover and the purge
  scheduler every purge interval (default 60 s). Scheduled purges are audited with actor
  `service:scion` and the reason `scheduled purge: quarantine period elapsed`.
- Purge does not change the redaction epoch: a purged object stays redacted and blocklisted
  (SPEC 13.6), so re-uploading the same content keeps failing with `REDACTED`.
