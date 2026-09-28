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
   - Prepare v0.2.0 (GitHub release only) and stop to ask before pushing, tagging or releasing.

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

Stage 3 (M9-M11) and the ReFS track get their goals once Stage 2 shows
what the metadata engine can do; the ReFS track does not depend on it and
can start earlier.
