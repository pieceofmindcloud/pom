# Plugin sandbox - Linux phase 0 spike

Date: 2026-11-27. Spec: `docs/superpowers/specs/2026-11-27-plugin-sandbox-design.md`,
plan phase 0. Describes the code and the machines at the time of writing.

## What was run

A throwaway Rust program (`landlock` 0.4.8, `seccompiler` 0.4, not in the
repository) that sandboxes itself the way the spec proposes, without
namespaces: `PR_SET_NO_NEW_PRIVS`, a Landlock ruleset (filesystem, TCP bind
and connect, signal and abstract socket scopes), then a seccomp filter that
returns `EPERM` for `ptrace`, `process_vm_readv/writev`, `kcmp`, `bpf`,
`perf_event_open`, `unshare`, `setns`, keyring, module, `kexec`,
`pidfd_getfd` and `userfaultfd`. It then probes what it can reach, or
`exec`s a command inside the sandbox.

Allowed in the probe: read-only `/usr`, `/lib*`, `/bin`, `/sbin`, `/etc`,
`/opt`, `/sys`, `/app` (and `/proc` in one variant); read-write the
workspace, the plugin home, `/dev/null|zero|random|urandom|shm|pts|tty` and,
with `--gpu`, `/dev/nvidia*`, `/dev/dri`, `/dev/kfd`. The target process is
one of the same user (on the host) or the POM node itself (in the container),
which is the realistic attacker position.

Two environments:

- **Host**: Ubuntu 26.04, kernel 7.0, Landlock ABI 8, Yama `ptrace_scope=1`,
  `apparmor_restrict_unprivileged_userns=1`.
- **Container**: the cluster node `salem-gpus` (POM dev image, default Docker
  seccomp profile), same kernel, Landlock ABI 8, two NVIDIA GPUs.

## Results

| Probe | Host | Container |
|---|---|---|
| Landlock ruleset | fully enforced | fully enforced |
| `unshare -Urpf` (user + PID namespace) | refused: AppArmor denies the `uid_map` write | refused: Docker seccomp denies `unshare` |
| signal another process (`kill(pid, 0)`) | blocked (Landlock signal scope) | blocked |
| `ptrace` attach, `process_vm_readv` | blocked (seccomp) | blocked |
| `/proc/<pid>/environ` of another process | blocked (ptrace access mode) | blocked |
| list `/proc` pids, read `/proc/<pid>/cmdline` | **visible** when `/proc` is allowed | **visible** (the node's command line) |
| `/proc/self/*` when `/proc` is not allowed | blocked, so programs break | - |
| read `pom.json`, another plugin's home, the user's home | blocked | blocked |
| write workspace and own home | allowed | allowed |
| connect `127.0.0.1:<node port>` (allow-listed) | allowed | allowed |
| connect another loopback port, connect the internet | blocked | blocked |
| bind `127.0.0.1:0` (own server) | allowed only with a port-0 bind rule | allowed with that rule |
| `fork` | allowed | allowed |
| `nvidia-smi` without the GPU grant | - | `Insufficient Permissions` |
| `nvidia-smi` and a torch CUDA matmul with it | - | works (RTX 3060 and 5060 Ti listed; matmul on the 5060 Ti with ComfyUI's runtime) |

## Findings

1. **Landlock + seccomp works unprivileged, on the host and inside the
   published style of Docker image**, with no change to the image or to the
   Docker flags. It covers the spec's core: no signalling, debugging or
   memory reading of other processes, no access to other plugins' files or
   the node's secrets, the workspace shared, the GPU usable when granted.
2. **Process visibility is not covered without a PID namespace.** Python,
   CUDA and most runtimes need `/proc/self`, and Landlock cannot allow
   "self" only, so `/proc` must be readable; then the process list and every
   process's command line are readable. A PID namespace (and its own `/proc`)
   is the only fix, and both environments refuse it: on the host Ubuntu's
   AppArmor userns restriction (lifted per program by an AppArmor profile
   with `userns,`, installed as root, the way browsers do), in Docker the
   default seccomp profile (lifted by a custom profile or
   `--security-opt seccomp=...`).
3. **Landlock's network rules are per port, never per address.** "Loopback
   only" cannot be expressed: `network: loopback` becomes "these TCP ports
   only" (the node's gateway port plus the plugin's own ports), and
   `network: internet` must leave connect unrestricted, which also opens
   every loopback port - the node's inference server (`8181`) and the other
   plugins' loopback servers. Those servers then need their own credential,
   or a network namespace isolates loopback, with the node reached through
   the host's IPC instead of TCP.
4. **A plugin's own child servers need known ports in `loopback` mode.** A
   bind on port 0 works with a port-0 rule, but connecting to that random
   port afterwards does not; vLLM workers, ComfyUI and Strata talk to their
   own children over loopback. The node would have to assign each plugin a
   port range and grant it.
5. Command lines are visible to every plugin (finding 2): POM and its plugins
   must not pass secrets as arguments.

## Recommendation for the spec

- Baseline, required on every Linux machine: Landlock ABI >= 6 + seccomp +
  `no_new_privs`, applied by `pom-plugin-host` before `dlopen`. Measured
  here; refuse plugins when Landlock is missing or below ABI 6.
- PID namespace as an extra layer where the system allows it (an AppArmor
  profile shipped by the Linux packages, a seccomp profile for the Docker
  image), so the process list is hidden; reported in the panel as the
  sandbox level, not required, otherwise decision 4 of the spec would keep
  POM's own Docker image and every current Ubuntu from running plugins.
- The manifest's `network` becomes a port policy: the node assigns each
  plugin a loopback port range for its servers and grants it plus the
  gateway port; `internet` is an explicit, louder permission because it also
  reaches the other loopback services.
- Next Linux step: the same probe with a PID namespace allowed (an AppArmor
  profile on the host, a seccomp profile in a test container) and the vLLM
  plugin end to end under the sandbox.
