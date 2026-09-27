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

## Linux test VM

- Ubuntu 22.04 VM `codex@192.168.189.142` (kernel 6.8, NOPASSWD sudo for testing only), shared with
  `/home/w0w/linuxreflect`; do not touch that project's files there. `tools/linux-vm.sh 'command'` runs a
  command, `tools/linux-vm.sh -sync` copies this repository (without `target/` and `testdata/`) to
  `~/Linux_Storage_Spaces`. Use it for ublk, NBD, dm and mount tests. The corpus lives on the
  dedicated 128 GB disk `/dev/sdb1` mounted at `/srv/spaces` (`~/Linux_Storage_Spaces/testdata` links there);
  the system disk has only ~17 GB free.

## Long-running commands with Longrun MCP

- For a reviewed, trusted, non-interactive command expected to run longer than about 30 seconds, use `longrun.start_job` exactly once when it is available and the exact project root is enrolled.
- After a bridge-enabled `start_job` returns, never call `longrun.get_job`, `collaboration.wait_agent`, a generic wait tool, `write_stdin`, log-tail tools, or any polling loop in that turn. In the automatically resumed turn, call `longrun.get_job` exactly once and continue from the terminal result.
- Use `wake_policy="none"` only with ordinary `codex` or an explicitly requested manual fallback. In that mode automatic wakeup is unavailable: end the turn without waiting, and use one `longrun.get_job` only in a later user-resumed turn. If it is still running, end that turn without polling again.
- Treat `longrun.run_and_wait` as legacy compatibility mode because current Codex runtimes may convert a pending blocking call into model-driven wait cycles. Progress heartbeats are disabled by default and do not fix that outer wait loop.
- Never put secret values in command arguments, MCP fields, prompts, or environment. For one reviewed non-interactive command that accepts one finite secret stdin payload, prefer `project_memory_stage_test_asset_for_longrun` when a suitable encrypted test-only asset exists, then pass only its one-time `stdin_secret_id` to Longrun; do not ask the user to re-enter that stored credential. Use `codex-longrun-secret` only when no suitable asset is enrolled. Output is suppressed for secret-stdin jobs. TTY-dependent or repeated password prompts remain unsupported. Do not use longrun for interactive programs, daemons, or unreviewed commands.
- Keep project roots narrowly enrolled. Never broaden `LONGRUN_ALLOWED_ROOTS` to `/home/w0w`; add each trusted project root explicitly.
- The global MCP configuration is loaded by new Codex processes. After installing, upgrading, or changing longrun configuration, start a new process or resume the session from a new process.
- Whenever creating a project-level `AGENTS.md`, include this section unless that project explicitly opts out or already has stricter equivalent guidance.
