# Test cartridge

A cartridge that exercises the whole cartridge contract and plays no game. It is the Cabinet's example cartridge and the program behind its loopback integration test.

| Part | What it exercises |
|---|---|
| `lifecycle.setup` | runs once at install; writes `touch-test.txt` into the scratch folder |
| `[[touches]]` | that file is snapshotted before `setup` and restored (removed) on uninstall |
| `lifecycle.service` | `echo-cart --service`, started on the room host before anyone's session |
| `lifecycle.session` | `echo-cart` on every member: publishes `state` with every widget kind and reacts to every event |
| `lifecycle.uninstall` | runs before the footprint restore |
| `capabilities.hosts` | a session-scoped hosts entry on `cabinet-test.invalid`, pointed at the service |
| `capabilities.low_ports` | the service binds port 987 on the wildcard address (on macOS this needs no privileged change) |
| `capabilities.firewall` | the cartridge's program is allowed through the firewall for the session (on macOS the allowance is per program; the port is informational) |
| `[[settings]]` | one required `dir` setting, `scratch_dir` |

## echo-cart

`echo-cart` (Rust, `src/`) speaks the JSON-lines protocol through the `cabinet-cartridge` crate.

| Mode | Behaviour |
|---|---|
| `echo-cart` | session: `ready`, then `state` after every event; `public` carries `hello` (own id), `role`, `service`, `members`, `seen` (other members' `hello`) and `actions` |
| `echo-cart --service` | `ready`, then waits for `stop` |
| `echo-cart --service --bind <port>` | also binds `<port>` on the wildcard address (without listening) until it stops, and logs whether that worked |
| `echo-cart --fail` | reports a fatal `error` and exits 3 |
| `echo-cart setup <file>` | writes `<file>` |
| `echo-cart uninstall` | exits 0 |

| Button | Effect |
|---|---|
| `bump` | recorded in `actions` |
| `exit` | the session exits 0 |
| `crash` | the session exits 4 |
| `stubborn` | the session ignores `stop` (the Cabinet kills it after the grace period) |

A `join.request` is allowed unless the joiner's name contains `deny`.

## Packing

Build `echo-cart` for each platform, lay the binaries out as the manifest lists them, and pack:

```sh
mkdir -p stage/files/macos-aarch64 stage/files/linux-x86_64 stage/files/windows-x86_64
cp cartridges/test/cartridge.toml stage/
# copy each platform's build to stage/files/<platform>/echo-cart (echo-cart.exe on Windows)
cabinet-pack pack stage --out test-0.1.0.cart --key <secret.key>
cabinet-pack inspect test-0.1.0.cart
```

CI packs it this way on every run, signed with a throwaway key.
