# Agent goals

Task texts for a coding agent's `/goal`, one per block, in the order they
are meant to run. Each builds on [the plan](plan.md) and ends by asking
for approval before anything is pushed, tagged or released.

## Goal A: Stage 1 release blocker and M5

```text
Settle the Stage 1 release blocker and do M5 of docs/plan.md in /home/w0w/Linux_Storage_Spaces (Windows 11 pools only). Work through this TODO in order; record the evidence for each item in docs/plan.md and mark M5 "exit criteria met" only when everything in it is done.

1. Stage 1 release blocker: mirror dirty region tracking
   - On the Windows VM create small mirror pools and end them in different ways:
     - Disconnect-VirtualDisk before dismounting;
     - Set-StoragePool -IsReadOnly before dismounting;
     - a Windows restart and a shutdown with the pool attached;
     - dismount while online (what New-TestPool.ps1 does now);
     - 0, 1, 5 and 15 min idle after the last write.
     Record the SPACEDRT headers in each case.
   - Adjust the spaces info wording and the mirror read path (whether listed runs must compare every copy) to the result. Update docs/storage-spaces-format.md and back it with fixtures and tests.
   - If the generator should end pools cleanly, change New-TestPool.ps1 but keep the existing corpus.
   - Prepare v0.1.0 as decided: GitHub release only (tag plus a release with the static x86_64 musl build), no crates.io. Stop and ask before pushing, tagging or releasing.

2. Round-trip harness (needed from M5 on)
   - Pools changed outside Windows must go back to Windows: raw images -> VHDX (qemu-img, or a raw2vhdx next to tools/vhdx2raw.py where qemu-img cannot write 4Kn), upload, attach.
   - Add a scripted Windows check:
     - Get-StoragePool / Get-VirtualDisk health;
     - Get-PhysicalExtent;
     - Repair-VirtualDisk has nothing to do;
     - chkdsk on NTFS;
     - pattern and file hash check.
   - Add a spaces command that dumps and diffs pool metadata (database copies, SDBB entries, decoded record fields, cache/journal/DRT headers and slots) between two pool states.

3. M5: what Windows writes
   Method: VM experiments with snapshots of the VHDX files before and after each operation (several points in between where order matters) and metadata diffs.
   - Dirty region tracking: when runs are added and removed, generations, both header copies, timing.
   - Parity journal and write-back cache write path: how a write is logged, committed and destaged; slot reuse, sequence numbers, the type 1 slot, why the log restarts mid-area.
   - Database update protocol: which copies are written in which order, sequence numbers and checksums, how Windows chooses between diverging copies after a crash (disks cut at chosen points).
   - Extent and space health fields: how a copy is marked stale when a write misses a disk, and how repair clears it.
   - Slab allocation of thin spaces and cache destaging: which slabs on which disks, in which order.
   - For every item write the specification into docs/storage-spaces-format.md and add a predictor test: from a pool state and a scripted operation the library predicts the metadata Windows writes, and the test compares it with the pool Windows produced. Encoders stay in memory; nothing writes to member disks yet.

Rules:
- Follow AGENTS.md and docs/plan.md.
- Never write to pool member disks, the test_ubuntu pool or the pre-existing Windows "Storage pool". Fetched corpus pools stay unchanged; work on copies.
- Keep docs/storage-spaces-format.md current and back every new format claim with a test.
- Windows VM (tools/vm.sh):
  - generators pace their writes;
  - never run heavy I/O on the Windows and Linux VMs at the same time;
  - restarting the test VM for item 1 is allowed; if it crashes twice, stop and ask.
- Kernel-level tests run on the Linux VM (tools/linux-vm.sh, corpus in /srv/spaces, delete copies after use). The VM is shared with linuxreflect: detach only what you attached.
- cargo fmt, clippy and all tests must be green; add fuzz targets for every new parser or encoder (decode(encode(x)) == x).
- Commit after each completed step with English messages.
- Do not push, publish or tag without asking.
- Stop and ask when a step needs a destructive action, a VM or hardware reconfiguration, or a decision the plan does not cover.

Done when the dirty region question is settled, M5 exit criteria are met, and only the approval for v0.1.0 remains.
```

## Goal B: Stage 2, write support (after Goal A)

```text
Do Stage 2 of docs/plan.md (M6-M8, write support) in /home/w0w/Linux_Storage_Spaces on top of the M5 specification (Windows 11 pools only). Work through this TODO in order; record the evidence for each item in docs/plan.md and mark M6-M8 "exit criteria met" only when everything in them is done.

1. Write infrastructure
   - `--rw` for attach and the serve commands, off by default.
   - Writes only to spaces whose whole state is understood (known record versions; no unknown cache, journal or DRT state; Healthy unless an item explicitly allows degraded writes). Refuse everything else with a clear message.
   - Members are opened read-write (O_EXCL) only in rw mode; read paths stay read-only.
   - Crash testing without power cuts of the shared Linux VM: record member writes (dm-log-writes or an in-process write log) and replay every flush point plus random prefixes. A real power-off of a VM needs approval.

2. M6: in-place writes to allocated regions
   - Simple spaces: dm read-write tables and ublk.
   - Mirror: write all copies, mark and clear DRT as Windows does; degraded writes mark missing copies stale.
   - Single parity (dual parity if M5 covered it): full-stripe and read-modify-write under the journal, with the write hole closed.
   - Regions held in the write-back cache: write through the cache log or destage first; never leave a stale entry over newer data.
   - Flush/FUA mapped to member flushes in the right order.
   - Exit:
     - fio verify workloads and NTFS stress (ntfs3 rw, fsstress/fsx) on copies of corpus pools, then the Windows round trip: Healthy, Repair-VirtualDisk has nothing to do, chkdsk clean, data verified;
     - replays at every flush point: Windows repairs or accepts the pool, and no acknowledged write is lost.

3. M7: writes that change metadata
   - Thin provisioning: allocate slabs on the first write like Windows; update the database on all members with correct sequence numbers and checksums.
   - TRIM/discard with slab reclamation, matching Windows.
   - Cache destaging on Linux.
   - Crash consistency of the metadata engine: write-ahead ordering, recovery on the next assembly, a replay after every metadata write.
   - Exit: thin spaces filled from Linux beyond their initial allocation pass the M6 checks in Windows, including the crash replays.

4. Hardening of the write code
   - Fuzz targets for the encoders and for assembly after torn writes.
   - Security review of the write paths.
   - 24 h of fuzzing without findings.

5. M8: release
   - Document `spaces attach --rw`: guarantees, risks, what is refused.
   - Update the README, user guide, man page and CHANGELOG.
   - Record the state as v0.2.0-ready. Nothing is pushed to GitHub, tagged or released before Stage 3 is complete (decision 2026-09-29).

Rules:
- Follow AGENTS.md and docs/plan.md.
- All writes go to sparse copies of corpus pools (on the Linux VM under /srv/spaces/work, locally under testdata/work). Keep the fetched originals read-only. Never write to real disks, the test_ubuntu pool or the pre-existing Windows "Storage pool".
- Keep docs/storage-spaces-format.md current and back every new format claim with a test.
- Windows VM (tools/vm.sh):
  - generators pace their writes;
  - never run heavy I/O on the Windows and Linux VMs at the same time;
  - if the VM crashes twice, stop and ask.
- Kernel-level tests run on the Linux VM (tools/linux-vm.sh). Detach only what you attached; delete work copies after use (/srv/spaces has 128 GB).
- cargo fmt, clippy and all tests must be green.
- Commit after each completed step with English messages.
- Do not push, publish or tag without asking.
- Stop and ask when a step needs a destructive action, a VM or hardware reconfiguration, or a decision the plan does not cover.

Done when M6-M8 exit criteria are met and only the approval for v0.2.0 remains.
```

## Goal C: Stage 3, pool management (after Goal B)

```text
Do Stage 3 of docs/plan.md (M9-M11, pool management) in /home/w0w/Linux_Storage_Spaces on top of the Stage 2 metadata engine (Windows 11 pools only; pool version 28 by default, 29 on request). Work through this TODO in order; record the evidence for each item in docs/plan.md and mark M9-M11 "exit criteria met" only when everything in them is done.

1. What Windows writes for management operations (as M5 did for writes)
   Method: VM experiments on small VHDX pools with snapshots before and after each operation (and in between where order matters), metadata diffs with spaces diff, scenarios in tools/scenarios.sh.
   - New-StoragePool on blank disks (1-8 disks, 512/512e/4Kn): GPT and partition, SPACEDB header, database layout, pool and disk records, metadata space, which values are random (GUIDs) and which follow from the input.
   - Add-PhysicalDisk, Set-PhysicalDisk (-Usage Retired, -MediaType), Remove-PhysicalDisk with evacuation, a replaced disk, Set-StoragePool (-NewFriendlyName, -IsReadOnly), Remove-StoragePool.
   - New-VirtualDisk for every resiliency and provisioning, with and without write-back cache, and tiers: space records, hidden children (cache, DRT, journal), their initial headers and slots, the first slabs. Resize-VirtualDisk, Remove-VirtualDisk, Set-VirtualDisk -NewFriendlyName.
   - Repair-VirtualDisk onto another disk, Optimize-StoragePool (rebalance): which slabs move, the order of copy, metadata switch and free, how progress survives an interruption.
   - For every item write the specification into docs/storage-spaces-format.md with predictor tests (from a pool state and an operation the library predicts the metadata Windows writes). Where Windows' choices do not follow from the metadata (disks, GUIDs, object ids), record the rules they obey and treat the choice as an input.

2. Management infrastructure
   - Commands `spaces pool ...`, `spaces disk ...`, `spaces space ...`. Every operation first prints what it will change (--dry-run shows the metadata diff) and needs an explicit confirmation for anything destructive.
   - `pool create` takes only disks named explicitly and refuses disks that are not blank (partition table, file system signature, pool membership) unless told to wipe them.
   - Operations only on clean, healthy pools that are not attached (or attached read-only) anywhere; refuse everything else with the reason.
   - Crash safety: each operation is a sequence of database updates that are each consistent; data moves copy first, switch the metadata, then free. Replay tests after every metadata write and at points inside data moves.

3. M9: pool and disk operations (plan items M9.1-M9.3).
4. M10: space operations (M10.1-M10.3); new spaces must pass the Stage 2 write checks (fio, NTFS) on Linux.
5. M11: maintenance (M11.1-M11.4): repair of stale or missing copies onto other disks, rebalance, scrub with a report and optional repair, a health report in Windows' terms.

6. Exit checks
   - Pools created and changed only on Linux: Windows 11 imports them without warnings; Get-StoragePool, Get-PhysicalDisk, Get-VirtualDisk and Get-PhysicalExtent show the expected properties; data written on Linux reads back on Windows; Windows' own Repair-VirtualDisk and Optimize-StoragePool succeed on them.
   - The reverse: copies of Windows-created corpus pools changed on Linux (disks added and removed, spaces created, extended, deleted, repaired) pass the same checks.
   - Crash replays of every operation: Windows and Linux open the pool in the old or the new state, never a broken one.
   - Fuzz targets for every new encoder and operation, security review of the management paths, 24 h of fuzzing without findings.

7. v1.0.0
   - Document the management commands (user guide, man page, README, CHANGELOG), version 1.0.0, packages.
   - Prepare the GitHub publication decided on 2026-09-29 (after Stage 3): the local commits, the release notes, the static x86_64 musl build; no crates.io. Stop and ask before pushing, tagging or releasing, including whether 0.1.0 and 0.2.0 get tags of their own.

Rules:
- Follow AGENTS.md and docs/plan.md.
- All writes go to sparse image files or copies of corpus pools (on the Linux VM under /srv/spaces/work, locally under testdata/work, on the Windows VM under C:\sstest). Keep the fetched originals read-only. Never write to real disks, the test_ubuntu pool or the pre-existing Windows "Storage pool".
- Keep docs/storage-spaces-format.md current and back every new format claim with a test.
- Windows VM (tools/vm.sh):
  - generators pace their writes;
  - never run heavy I/O on the Windows and Linux VMs at the same time;
  - if the VM crashes twice, stop and ask.
- Kernel-level tests run on the Linux VM (tools/linux-vm.sh). Detach only what you attached; delete work copies after use (/srv/spaces has 128 GB).
- Keep long runs light on the host: fuzzing and stress runs at nice 19 with one worker per fuzz target (about 8 of 64 cores, a few GB of memory); a run cut short by a host crash resumes from its corpus for the remaining time on unchanged code.
- cargo fmt, clippy and all tests must be green.
- Commit after each completed step with English messages.
- Do not push, publish or tag without asking.
- Stop and ask when a step needs a destructive action, a VM or hardware reconfiguration, or a decision the plan does not cover.

Done when M9-M11 exit criteria are met and only the approval for publishing v1.0.0 remains.
```

The ReFS track (Track B of the plan) gets its own goal; it does not depend
on Stage 3.
