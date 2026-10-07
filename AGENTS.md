# Project notes for agents

- Rust workspace: `crates/storage-spaces` (library), `crates/spaces-cli` (`spaces` binary).
  Later ReFS crates go next to them. License GPL-2.0-or-later.
- Repository text (code, comments, docs, commits) is English only.
- The format knowledge lives in `docs/storage-spaces-format.md`; update it together with the parser.
- Never write to pool members: all tools are read-only until write support is designed explicitly.

## Windows test VM

- `tools/vm.sh` runs PowerShell on the test VM (DESKTOP-BQ2J4NS, `sshuser@10.0.77.97`; the older VM
  192.168.189.129 via `WIN_VM_HOST`, see the script); `-f script.ps1 args` uploads and runs a
  script, `-get` copies a file back.
- `tools/vm/New-TestPool.ps1` builds a pool on VHDX files in `C:\sstest\<name>`, fills the space with the
  verification pattern, writes `manifest.json` (Windows' view incl. `Get-PhysicalExtent`) and detaches it.
  `tools/vm/Remove-TestPool.ps1` deletes one. Never touch the pre-existing "Storage pool" on the VM.
- `tools/gen-corpus.sh` lists the corpus configurations; `tools/fetch-corpus.sh NAME...` copies pools to
  `testdata/pools/NAME/` as sparse raw images (git-ignored). Corpus tests skip missing pools.
- Older Windows for ReFS samples of other versions: Windows Server 2019 Evaluation (WIN-R326LQ0OIA6,
  `WIN_VM_HOST=Administrator@10.0.77.11`; ReFS 3.4, volumes `r34*`) and Server 2022 Evaluation
  (WIN-4OUB3OQJKV6, `WIN_VM_HOST=Administrator@10.0.77.10`; ReFS 3.7, volumes `r37*`) and Server 2012 R2
  Evaluation (WIN-2P4MLM1IG2G, `WIN_VM_HOST=Administrator@10.0.77.12`; ReFS 1.2, 64 KiB clusters only,
  Windows PowerShell 4.0, Win32-OpenSSH; volumes `r12*`) and Server 2016 Evaluation (WIN-LA8L93TB06T,
  `WIN_VM_HOST=Administrator@10.0.77.20`; ReFS 3.1, Win32-OpenSSH; volumes `r31*`), key login. The user runs
  one such VM at a time; every Windows that the evaluation center offers has been sampled.
  `tools/vm/New-RefsVolume.ps1` formats plain ReFS where there are no Dev Drives;
  `tools/vm/Install-Updates.ps1` installs Windows updates there (as SYSTEM, since Windows Update refuses
  network logons).

## Linux test VM

- Ubuntu 22.04 VM `codex@192.168.189.142` (kernel 6.8, NOPASSWD sudo for testing only), shared with
  `/home/w0w/linuxreflect`; do not touch that project's files there. `tools/linux-vm.sh 'command'` runs a
  command, `tools/linux-vm.sh -sync` copies this repository (without `target/` and `testdata/`) to
  `~/Linux_Storage_Spaces`. Use it for ublk, NBD, dm and mount tests. The corpus lives on the
  dedicated 128 GB disk `/dev/sdb1` mounted at `/srv/spaces` (`~/Linux_Storage_Spaces/testdata` links there);
  the system disk has only ~17 GB free.
- Ubuntu 26.10 (development) VM `claude@192.168.189.143` (kernel 7.3, systemd 261, AppArmor with Ubuntu's
  `fusermount3` profile, udisks2 mounting under `/run/media/<user>`; NOPASSWD sudo for testing only), used by
  this project alone: `LINUX_VM_HOST=claude@192.168.189.143 tools/linux-vm.sh ...`. It has what the 22.04 VM
  lacks (systemd >= 254 makes `PrivateNetwork` imply `PrivateMounts`; the AppArmor profile confines
  `fusermount3`), so test `mount.ReFS`, udisks2 and fuseblk mounts on both. ZFS root with ~180 GB free;
  `~/sstest` holds the `r314small` ReFS sample and test pools (`~/sstest/pools`; on the 22.04 VM they live
  on `/srv/spaces/pools`, linked from there). `tools/mount-vm-check.sh`, `tools/package-vm-check.sh` and
  `tools/guard-check.sh` (the health guard: refusals, reports, `--force`, udev hints, ReFS mounts) check a deb
  on either VM (`LINUX_VM_HOST=...`); on the 22.04 VM the package's scripts leave linuxreflect's pool (served
  by `contrib/install.sh`'s `/usr/local/sbin/spaces`) alone, so it may be installed and removed there.

## Long-running commands with Longrun MCP

- For a reviewed, trusted, non-interactive command expected to run longer than about 30 seconds, use `longrun.start_job` exactly once when it is available and the exact project root is enrolled.
- After a bridge-enabled `start_job` returns, never call `longrun.get_job`, `collaboration.wait_agent`, a generic wait tool, `write_stdin`, log-tail tools, or any polling loop in that turn. In the automatically resumed turn, call `longrun.get_job` exactly once and continue from the terminal result.
- Use `wake_policy="none"` only with ordinary `codex` or an explicitly requested manual fallback. In that mode automatic wakeup is unavailable: end the turn without waiting, and use one `longrun.get_job` only in a later user-resumed turn. If it is still running, end that turn without polling again.
- Treat `longrun.run_and_wait` as legacy compatibility mode because current Codex runtimes may convert a pending blocking call into model-driven wait cycles. Progress heartbeats are disabled by default and do not fix that outer wait loop.
- Never put secret values in command arguments, MCP fields, prompts, or environment. For one reviewed non-interactive command that accepts one finite secret stdin payload, prefer `project_memory_stage_test_asset_for_longrun` when a suitable encrypted test-only asset exists, then pass only its one-time `stdin_secret_id` to Longrun; do not ask the user to re-enter that stored credential. Use `codex-longrun-secret` only when no suitable asset is enrolled. Output is suppressed for secret-stdin jobs. TTY-dependent or repeated password prompts remain unsupported. Do not use longrun for interactive programs, daemons, or unreviewed commands.
- Keep project roots narrowly enrolled. Never broaden `LONGRUN_ALLOWED_ROOTS` to `/home/w0w`; add each trusted project root explicitly.
- The global MCP configuration is loaded by new Codex processes. After installing, upgrading, or changing longrun configuration, start a new process or resume the session from a new process.
- Whenever creating a project-level `AGENTS.md`, include this section unless that project explicitly opts out or already has stricter equivalent guidance.
